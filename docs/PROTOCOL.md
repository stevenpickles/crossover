# Crossover Wire Protocol

The application protocol between authenticated peers. This document is a
skeleton: it fixes the invariants and message semantics; exact encodings are
settled when the serialization format ADR is accepted
([adr/README.md](adr/README.md), deferred decision 1).

The protocol crate (`crossover-protocol`) implements everything here and is
testable without sockets ([ARCHITECTURE.md](ARCHITECTURE.md) §3).

---

## 1. Ground rules

- The protocol runs only inside an established, mutually authenticated
  TLS 1.3 session ([SECURITY.md](SECURITY.md)). Nothing here substitutes for
  transport security.
- **Network input is never trusted**, even from an authenticated peer.
  Every frame and field is validated; malformed input must not panic,
  allocate unboundedly, or corrupt state (NFR-1).
- The protocol never depends on TCP packet boundaries.
- Versioned from the first release; compatibility behavior is deterministic.

## 2. Framing

Conceptual frame layout:

```
frame_length     // validated against MAX_FRAME_SIZE *before* allocation
message_type
message_id       // monotonic per session, for logging & duplicate detection
payload          // serialized message body
```

Requirements:

- A `MAX_FRAME_SIZE` and a maximum size for **every** variable-length field
  are defined as named constants in `crossover-protocol`.
- Frames with invalid lengths, unknown critical structure, or oversized
  fields are rejected; per §7, rejection of malformed framing is fatal to
  the session (fail closed).
- Unknown *message types* within a known-valid frame are skippable when the
  version negotiation permits it (§3), enabling forward compatibility.

**Forward compatibility is asymmetric, and the asymmetry is load-bearing:**

| Extension | A peer that predates it |
|-----------|-------------------------|
| A new **message type** | Skips the frame (§7): survivable, though the sender gets no answer |
| A new **feature bit** | Ignores it (§3.1): the feature simply never activates |
| A new **variant of an enum inside a payload** (`ContentType`, `DeclineReason`, `ApplyResult`, …) | **Cannot decode the payload at all.** Its decoder rejects the unknown discriminant, and a malformed payload is fatal (§7) — the session terminates |

The third row is the trap: appending an enum variant looks like the same
kind of additive change as the first two, and is not. A `ClipboardDecline`
carrying a reason an older peer does not know does not degrade to "unknown
reason" — it kills that peer's session.

Therefore: **every payload-enum extension must be feature-gated by the
sender** (§3.1), which is why the gate lives on the send path where no
caller can skip it, and why a bit joins `FeatureFlags::ADVERTISED` only
once a build can genuinely honour the capability end to end. This applies
to every future addition of the kind, `ContentType::Files` among them.

## 3. Session establishment and versioning

After TLS establishment and peer authorization, each side sends `Hello`:

```
Hello
    protocol_version        // highest supported
    min_protocol_version    // lowest supported
    device_id
    device_name
    operating_system
    supported_features      // explicit feature flags
```

- The session runs at the highest mutually supported version; if the ranges
  don't intersect, the session terminates with a diagnostic. No silent
  downgrade below either side's minimum.
- Capabilities beyond the base protocol (optional clipboard types, …) are
  negotiated via `supported_features`, never assumed.
- Behavior for unknown fields and unknown messages at each version is part
  of the version's definition. Breaking changes require an ADR,
  compatibility tests, and documentation updates.

### 3.1 Feature flags

`supported_features` is a bitmask, `FeatureFlags` in `crossover-protocol`.
Rules, all of them deliberate:

- **A feature is active only if both sides advertise it.** The session's
  capability set is the *intersection* of the two `Hello`s
  (`FeatureFlags::negotiate`), carried on `SessionInfo::features`.
- **Unknown bits are ignored, never an error.** A future peer advertising
  bits this build does not know simply never activates them — the
  intersection drops them.
- **Advertising is a promise to handle, not a statement of intent to
  send.** A build sets a bit only when it can receive that traffic and
  complete the transaction.
- **Senders gate on the negotiated set before anything travels**, at the
  send path itself (`gate_outbound` in `crossover-core::net`), so the
  check cannot be forgotten by a caller. This is not an optimization. Per
  §2 an unknown *message type* is skipped, so an un-negotiated chunk would
  be answered by nothing at all — a silently stalled transaction, which
  NFR-3 forbids. Worse, an un-negotiated *content type* is not skipped at
  all: it fails the peer's payload decode and terminates its session. The
  gate is what turns both outcomes into a local refusal.

| Bit | Name | Meaning |
|-----|------|---------|
| 0 | `CHUNKED_CLIPBOARD` | The peer can receive `ContentType::Image` items offered and streamed as `ClipboardChunk` messages, reassemble them, and install the result (ADR 0014) |
| 1 | `FILE_CLIPBOARD` | The peer can receive `ContentType::File` items — an offer carrying a `FileDescriptor`, then the blob as `ClipboardChunk` messages — spool the result, and offer it to its own clipboard as a virtual file (ADR 0015) |

Bit 1 is deliberately **not** a widening of bit 0. A peer that implements
ADR 0014 and not ADR 0015 advertises bit 0 and has no `File` discriminant,
and an un-negotiated content type is fatal to its decode rather than
skippable — so files need a bit of their own or they would kill exactly
the sessions they were supposed to enrich.

`FeatureFlags::ADVERTISED` is what this build actually sends, and it is
now **`ALL`** — both bits. Bit 0 has been advertised since ADR 0014's
platform slice, and every layer of that promise is real: the wire carries
chunked items, the clipboard engine offers, streams, reassembles, verifies
and installs them, and `crossover-platform-windows` reads and writes
`CF_DIB` on the actual OS clipboard. Bit 1 joined it in ADR 0015's final
slice (feature/136): the receiving half — the `file_receive` grant, the
bounded spool, and the virtual file list on the OS clipboard — landed in
feature/126–132, and the sending half — local observation, the blob
builder, and the engine's send transaction — in feature/133–135, so by the
time the bit flips every layer beneath it can honour the promise.
Advertising is a promise to **handle**, not a statement of intent to send,
and it was withheld until both halves were true; the code path itself has
been exercised only by the test suites so far, with two-machine hardware
validation of file transfer still outstanding.

The flip is **wire-visible**: the `Hello` a peer receives now carries
`supported_features = 3` where it carried `1`. That is why the golden
`Hello` snapshot pins the byte — a change to the advertisement has to be a
deliberate edit rather than something a peer discovers first. It is also
safe by the rules above and by nothing else: a feature activates only on
the *intersection*, so a peer that predates the bit negotiates it away and
receives nothing new, and no base-protocol layout changed, so text keeps
synchronizing with such a peer exactly as before. `crossover-core::net`'s
`unnegotiated_content_is_refused_before_it_reaches_the_wire` and the
test-peer suite's
`an_un_negotiated_image_never_reaches_the_wire_and_text_still_flows` are
that compatibility case, run against an advertising build.

Tests that need a *specific* negotiation still set it per session
(`SessionOptions::advertised_features`) rather than inheriting the
constant, so each suite states the negotiation it depends on.

This is the route chosen over another hard version-floor bump (the v1 → v2
option ADR 0014 weighed): the base-protocol wire layouts are unchanged, so
a peer without the bit keeps synchronizing text with one that has it.

A feature bit is not a substitute for a version bump, and ADR 0015 is
where the difference shows. Bit 1 gates a new *content type*, which only
travels after both sides advertise; but the same ADR appends
`Option<FileDescriptor>` to `ClipboardOffer`, and that byte is on **every**
offer of every type, negotiated or not. No bit can hide it from a peer
that predates it, so files are the v2 → **v3** bump, while images needed
none.

The same rule decides the v3 → **v4** bump
([ADR 0018](adr/0018-drawn-display-topology.md), Phase 8): the drawn
topology grows `ControlRequest.entry` and `ControlRelease.entry` from
`Option<u16>` to `Option<EntryPoint>` (§6.1), a layout change to control
messages that already travel between every pair of peers. Both ends of the
range move to 4, as they did for v2 and v3 — pre-1.0 the floor tracks the
ceiling, because there are no deployed peers to be compatible with
([ADR 0017](adr/0017-protocol-version-3.md)). The two new topology messages
(§6.2) deliberately carry **no feature bit**: the `entry` change already
refuses every v3 peer at `Hello`, so the only peers that can receive them
are peers that understand them, and a bit both sides always set would be a
gate that never closes.

## 4. Message classes

Four logical classes, initially multiplexed over the single TLS connection:

| Class | Contents | Delivery requirement |
|-------|----------|----------------------|
| CONTROL | Hello, control-transfer negotiation, display topology (§6.2), keepalive, ReleaseAllInput, session management | Ordered *within the class*, lossless |
| INPUT | Key transitions, pointer motion/buttons/scroll | Keys: ordered within the class, lossless. Pointer motion: coalescable (§6) |
| CLIPBOARD | Clipboard transaction messages | Ordered within the class, lossless, acknowledged |
| TELEMETRY | Latency probes, statistics | Best effort |

**Ordering is per class, not total.** The sender schedules interactive
traffic (CONTROL, INPUT) ahead of bulk (CLIPBOARD) rather than strictly
first-come-first-served, so a large clipboard payload cannot head-of-line
block the pointer and keyboard on the single connection
([ADR 0013](adr/0013-interactive-over-bulk-prioritization.md);
[ARCHITECTURE.md](ARCHITECTURE.md) §5.4). Consequences a receiver may rely
on:

- Messages **within** a class arrive in the order the sender produced them.
  In particular a clipboard transaction — `Offer`, `Accept`, `Data` or the
  `Chunk` sequence, `Applied` — is never reordered against itself, which is
  what §5's state machine depends on, and what lets the receiver treat a
  chunk index that is not the next one as a protocol violation rather than
  a delivery artefact.
- Messages of **different** classes may arrive in a different relative order
  than they were produced. A receiver must not infer causality across
  classes from arrival order.
- Nothing is dropped to achieve this. Deprioritized traffic is deferred,
  never discarded; only pointer motion is ever coalesced, and only under the
  rule in §6.
- `message_id` is assigned at the writer, after scheduling, so it stays
  monotonic in wire order and remains usable for logging and duplicate
  detection (§2).

The architecture permits moving classes onto separate connections later if
app-level prioritization proves insufficient; the class tag exists from v1
so that split requires no message redesign.

## 5. Clipboard transactions

Semantics required by FR-3.x: **a sync succeeds only when the destination
OS clipboard was updated.**

*When* a transaction starts is trigger-driven, not change-driven
(ADR 0006): observation of a local change does not itself transmit.
A settled-change debounce triggers transmission in Phase 2, control
transfer becomes the primary trigger in Phase 5, and session
establishment re-announces after a gap.

Inline flow (content ≤ `CLIPBOARD_INLINE_MAX_BYTES` = 64 KiB — the common
case; ADR 0005):

```
A observes local clipboard change, creates ClipboardItem
A -> B   ClipboardData      { id, origin, sequence, content_type,
                              content_length, content_hash, content }
B        validates length + hash, writes OS clipboard (bounded retries)
B -> A   ClipboardApplied   { id, result }        // success or typed failure
```

Offered flow (text above the inline threshold, up to
`MAX_CLIPBOARD_TEXT_BYTES` = 4 MiB; oversized items are rejected
gracefully — text is never chunked, per ADR 0005, because it always fits
one frame):

```
A -> B   ClipboardOffer     { id, content_type, content_length, content_hash }
B -> A   ClipboardAccept | ClipboardDecline   // decline carries a typed
                                              // reason; already-have-hash
                                              // counts as a sync success
A -> B   ClipboardData
B -> A   ClipboardApplied
```

Offered **and chunked** flow (ADR 0014), for content types that cannot
ride a single frame — `ContentType::Image`, up to
`MAX_CLIPBOARD_IMAGE_BYTES` = 64 MiB, and `ContentType::File` below:

```
A -> B   ClipboardOffer     { id, content_type, content_length, content_hash }
B -> A   ClipboardAccept | ClipboardDecline   // AlreadyHave here is why a
                                              // re-pasted snip moves zero
                                              // payload bytes
A -> B   ClipboardChunk     { id, index: 0, payload }
A -> B   ClipboardChunk     { id, index: 1, payload }   // interleaved with
...                                                     // live input (§4)
A -> B   ClipboardChunk     { id, index: n-1, payload }
B        reassembles, verifies content_hash, writes OS clipboard
B -> A   ClipboardApplied
```

Files (ADR 0015) ride that same flow, up to `MAX_CLIPBOARD_FILE_BYTES` =
256 MiB, with one addition on the offer:

```
A -> B   ClipboardOffer     { id, content_type: File, content_length,
                              content_hash,
                              descriptor: { file_name, archived,
                                            entry_count, original_bytes } }
B -> A   ClipboardAccept | ClipboardDecline   // NotPermitted, InvalidName,
                                              // InsufficientSpace, TooLarge,
                                              // NotReady, UnsupportedType
A -> B   ClipboardChunk × n                   // written through to B's spool
B -> A   ClipboardApplied   { id, result: Stored | StorageFailed }
```

Rules specific to files:

- **One clipboard item is one blob**: a single file verbatim, or one zip
  archive the *sender* built from a folder or a multi-entry selection.
  Nothing in Crossover reads an archive, on either machine.
- **The offer carries a descriptor, and only a file offer does.** A file
  offer without one, a descriptor on any other type, an `entry_count` of
  zero or past `MAX_CLIPBOARD_FILE_ENTRIES`, a multi-entry blob that
  claims not to be an archive, or an unarchived item whose
  `original_bytes` disagree with `content_length` are all malformed.
- **`file_name` is validated at decode**, by the rules in ADR 0015 —
  reject, never repair. It is the one field of a file transfer that
  reaches a shell (`FILEDESCRIPTORW.cFileName`), so a name that does not
  conform makes its offer malformed and no descriptor carrying it ever
  exists. The rejection names the fault and never the name, which is user
  data.
- **File content is chunked but never buffered.** The receiver writes
  chunks through to its spool as they arrive, so its commitment is
  O(chunk) rather than O(item); the in-memory reassembly used for images
  refuses this type outright.
- **No `AlreadyHave` for files**: a spool entry may have been evicted, so
  the receiver cannot honestly claim to already have one.
- **Each refusal means something different**, and a sender may act on the
  difference: `NotPermitted` is a grant the user can give
  (`crossover peers allow-files`); `UnsupportedType` is a receiver with no
  spool at all, which no permission will change; `InsufficientSpace` is
  this machine's free space or spool budget; `TooLarge` is the item's own
  ceiling; `NotReady` is a statement about now.
- **A newer offer supersedes a file transfer in flight**, exactly as it
  does for any other inbound item: the sending peer holds one outbound
  transaction, so a second offer means it has already abandoned the first,
  and the receiver deletes the partial and admits the new item rather than
  declining it. No verdict is owed for the abandoned one.

Rules specific to the chunked flow:

- **A chunk is its own message type, not a `ClipboardData`.**
  `ClipboardData` validates that its declared length equals the bytes it
  carries and that its hash covers all of them — which a fragment cannot
  satisfy. A chunk carries only what a fragment can prove about itself:
  item id, index, payload.
- **Chunked types are always offered, at any size.** The inline threshold
  is a text rule. The offer round is where hash dedup short-circuits a
  re-paste and where the receiver bounds its memory, so it is never
  skipped. Correspondingly, a `ClipboardData` carrying a chunked type is
  malformed, and a `ClipboardChunk` with no accepted offer behind it is a
  protocol violation.
- **The split is derived, not declared.** The receiver computes the chunk
  count and each chunk's exact length from the offered `content_length`
  and the size of chunk 0. There is no second declaration on the wire to
  contradict the first; a split that does not reconcile exactly with the
  offered length — or that needs more than `MAX_CHUNK_COUNT` chunks — is
  rejected before any chunk is kept.
- **Indices are strictly sequential from 0.** A gap, a repeat, an index
  past the end, a chunk for a different item, or a chunk whose length is
  not exactly what its position requires is a protocol violation (§7). The
  running total therefore cannot drift past the declared length.
- **The hash is verified over the reassembled bytes**, before the OS
  clipboard is touched — the bounds invariant below, unchanged, now
  spanning a reassembly rather than a single frame.
- **Sender-gated by negotiation** (§3.1): chunked content is offered only
  to a peer advertising `CHUNKED_CLIPBOARD`.
- Image bytes are the source clipboard's own raster format, verbatim: no
  transcode, no compression (ADR 0014). The format tag travels with the
  item; no component parses the pixels.

Invariants (enforced by the core clipboard engine, wire-visible here):

- **Acknowledgement**: every `ClipboardData` receives a `ClipboardApplied`
  (or typed failure) — no fire-and-forget.
- **Loop prevention**: `origin` + `content_hash` let the receiver recognize
  its own subsequent OS clipboard-change notification as the applied remote
  item and suppress re-sending (FR-3.3).
- **Conflict policy**: latest observed item wins, decided by (sequence,
  origin) deterministically; documented and tested (FR-3.5). Logical clocks
  only if a real defect demands them.
- **Bounds**: `content_length` is validated against the maximum **for its
  content type** before any allocation — including the reassembly buffer,
  which is sized from the offered length only after that check — and
  `content_hash` is verified before the OS clipboard is touched (FR-3.6).

## 6. Input events

Platform-neutral events (see FR-4.1 and ADR 0008 for the key-identity
model):

```
Key { key, pressed, repeat, text? }      // key = USB HID usage (u16); ADR 0008
PointerMove                      { x, y, sequence }        // coalescable
PointerButtonDown / Up           { button, sequence }
PointerScroll                    { dx, dy, sequence }
```

Key identity is the USB HID keyboard/keypad usage ID (Usage Page 0x07),
which is neither a Windows keycode nor layout-dependent (FR-4.1). The
source OS virtual-key is deliberately **not** carried — physical `key`
and produced `text` are the two distinct slots, and the destination
injects by scan code (derived from the HID usage), falling back to the
Unicode `text` for mismatched layouts (ADR 0008).

- Key transitions and button transitions are ordered and lossless; pointer
  motion is transient — under backpressure, intermediate positions are
  dropped in favor of the newest (Priority: input correctness > latency >
  motion durability).
- `ReleaseAllInput` (CONTROL class) instructs the destination to synthesize
  release events for everything the sender believes is pressed; it is also
  executed locally by the destination on session loss (FR-4.4).
- Sockets carrying INPUT/CONTROL traffic set `TCP_NODELAY`; latency is then
  managed by coalescing and by send-side prioritization (§4), not by Nagle
  buffering. Input frames are scheduled ahead of any queued clipboard bulk,
  and only one bulk frame is written between re-checks, so the kernel send
  buffer cannot re-serialize input behind a transfer.

### 6.1 Control transfer (CONTROL class)

Ownership is explicit, negotiated state (FR-5.1) — request → acknowledge →
switch (FR-5.3). Phase 3 triggers requests explicitly (CLI command); Phase 5
also triggers them from edge crossings, which carry an `entry` position so
the destination places the cursor where the pointer crossed (ADR 0009).
Carrying it grew the request and release layouts, which was the v1 → **v2**
protocol bump; changing its shape in Phase 8 is the v3 → **v4** bump.

The `entry` is `Option<EntryPoint>` (ADR 0018). `None` is an explicit
(console) transfer, which places no cursor. Otherwise:

```
EntryPoint
    monitor          // destination monitor id, ≤ MAX_MONITOR_ID_BYTES
    edge             // Left | Right | Top | Bottom, of that monitor
    fraction         // u16 along that edge: 0 at its start, u16::MAX at its end
    layout_revision  // the layout revision the sender derived this from
```

An empty `monitor` is a valid value, not a malformed one: it reads as
"unaddressed" under the same degraded-placement rule below, and is what a
sender still on the pre-layout side model sends deliberately, having no
destination id yet to give. Such a sender derives `edge` as the *opposite*
of the edge it is crossing on its own screen, since `edge` is specified in
the receiver's terms above and a two-machine pair's edges mirror each
other — so the field reads correctly under the degraded rule even before
either side knows the other's real geometry.

`fraction` is ADR 0009's normalized position, unchanged and still
resolution- and DPI-independent; the edge's **start** is the smaller
coordinate on the perpendicular axis — top for a Left/Right edge, left for
a Top/Bottom edge. What v4 adds around the fraction is *which* edge it is
a fraction of. A bare fraction was sufficient only while a machine had
exactly one crossing edge — with per-monitor seams there is no unique "the
edge", so the entry point is stated in the **receiver's** terms: the
monitor the cursor arrives on, which of its edges, and how far along it.
Stating it that way is what lets the receiver recognize an entry it cannot
honour instead of placing a cursor somewhere plausible and wrong.

```
A -> B   ControlRequest   { request_id, entry }        // A asks to control B
B -> A   ControlResponse  { request_id, verdict }      // Granted | Denied(reason)
A        on Granted: starts capture, sends InputBatch frames
...
A -> B   ReleaseAllInput  { after_sequence }           // hand-back begins
A -> B   ControlRelease   { entry }                    // relationship ends
```

Rules, all fail-closed:

- Exactly one control relationship may exist (FR-5.1). A peer that is
  controlling, requesting, or already controlled answers `Denied` with the
  reason — so simultaneous requests from both sides deterministically
  resolve to two denials, and either user simply retries.
- `InputBatch` is valid only while the sender holds a grant; otherwise it
  is a protocol violation and the session terminates (§7).
- `request_id` is requester-monotonic; a response whose id matches no
  in-flight request (timeout, supersession) is ignored — late answers are
  the condition the negotiation exists to survive, not an error.
- A request left unanswered past the requester's timeout reverts the
  requester to local control; nothing was captured in the interim.
- `ControlRelease` from the *controlled* side revokes an active grant (the
  local user's escape hatch) and is also the reverse-edge return; when it
  carries an `entry`, the ex-controller places its cursor there on the way
  back (ADR 0009). The ex-controller stops capturing on receipt.
- **An entry point the receiver cannot honour costs placement, not
  control.** An `EntryPoint` naming a monitor id this machine does not
  have, or a `layout_revision` that is not the one this machine holds, is
  **not** an error: the receiver places the cursor on its desktop-bounds
  edge matching `EntryPoint.edge`, fraction taken against those bounds —
  the pre-v4 placement, retained solely as this degraded mode — logs a
  diagnostic naming the mismatch, and the grant or release proceeds.
  Placement is a nicety; control correctness never depends on it
  (ADR 0018). A revision mismatch is expected during an edit's propagation
  window; crossings in that window degrade this way, briefly.
- Disconnect in any state releases everything: the controlled side executes
  `ReleaseAllInput` locally (FR-4.4), the controller stops capture, and
  both sides are local until a new negotiation.

### 6.2 Display topology (CONTROL class)

Seamless crossing follows from a **drawn layout**: both machines' monitors
placed in one shared, unit-agnostic coordinate space, with crossing edges
derived from exact adjacency between a local rectangle and a *peer*
rectangle ([ADR 0018](adr/0018-drawn-display-topology.md), Phase 8). Two
messages carry it, both added at v4 and both base protocol — no feature bit
(§3.1).

```
MonitorTopology   { monitors: [ { id, x, y, width, height,         // type 17
                                  scale_percent } ] }
LayoutSync        { revision, origin,                              // type 18
                    monitors: [ { device, id, x, y, width, height } ] }
```

- **`MonitorTopology` states a fact about the sender**: its own live
  monitors, in its own local coordinates. `scale_percent`
  (`MIN_SCALE_PERCENT`–`MAX_SCALE_PERCENT`, 100 = unscaled) is a seeding
  input for the editor's to-scale drawing **only**; it never enters
  crossing mapping, which is proportional through the drawn geometry
  (ADR 0018). The message is sent after `Hello` and again whenever the
  local display configuration changes, and it is what lets either
  machine's editor draw the peer's screens and lets layout validation tell
  a real monitor id from a fiction. It is not an arrangement and never
  changes crossing behaviour on its own.
- **`LayoutSync` states the arrangement**, which describes *both* machines:
  a `u64` revision, `origin` (the editing device's identity), and the
  placed monitors. It is sent after `Hello` when the sender holds an
  explicit layout, and on every edit. A layout that exists only
  implicitly — the compatibility layout a v1 config or a `--left` /
  `--right` flag produces — is never sent.

Invariants, all of them checked before anything is adopted:

- **Every bound is a named constant (§8), validated on encode as well as
  decode**, so a local defect cannot put on the wire a layout the peer
  would be right to refuse.
- **Bounds**: at most `MAX_MONITORS_PER_MACHINE` monitors from one machine
  and `MAX_LAYOUT_MONITORS` in a layout; a monitor id of at most
  `MAX_MONITOR_ID_BYTES` printable-ASCII bytes, unique within a machine;
  `1 ≤ width, height ≤ MAX_MONITOR_EXTENT`; `|x|, |y| ≤
  MAX_LAYOUT_COORDINATE`. Every derivation runs in `i64`, where those
  bounds make overflow impossible rather than merely unlikely.
- **Malformed is fatal** (§7): a count past its cap, a zero or oversized
  dimension, an out-of-range coordinate, a non-ASCII or overlong id — the
  session terminates, fail closed.
- **Well-formed but semantically impossible is rejected, never adopted**: a
  layout naming a device that is not this session's pair, a monitor neither
  peer has reported, or rectangles that overlap. It is logged and charged
  as a protocol violation on §7's graduated rule. The distinction is
  deliberate — the first case is a broken decoder or a hostile frame, the
  second is a peer disagreeing with reality, which must never steer local
  behaviour but must not cost a healthy session its first frame either.
- **Adjacency is exact.** A crossing span exists only where an edge
  coordinate matches identically and the perpendicular extents overlap; a
  gap of one unit is not an edge. Spans are half-open intervals, so a
  corner where three monitors meet resolves deterministically. Same-machine
  abutment produces no span by construction, so a machine's internal seams
  stay inert unless the peer is drawn across them.
- **Newest revision wins**, ordered by `(revision, origin)`
  lexicographically — `origin` comparing as its 16 raw bytes — with an
  equal key and differing content resolved by the lower SHA-256 hash of
  the postcard encoding of the monitor list sorted by `(device, id)`, and
  logged. Revisions are assigned as `seen_max.saturating_add(1)`, so a
  peer asserting `u64::MAX` cannot wrap the counter. Adoption is
  observable at both ends: the winner logs the adoption, the loser logs
  the supersession with both revisions and both origins (NFR-3).

## 7. Error handling

- Malformed frame or framing-level violation → terminate session (fail
  closed), log diagnostic with session id and reason.
- Valid frame, unknown non-critical message → skip if negotiation permits,
  count it, log at debug.
- Valid message, semantically impossible state (e.g., `ClipboardApplied`
  for an unknown id, control ack in wrong state) → reject; repeated
  violations terminate the session.
- Every rejection path is reachable in tests via the malformed-input
  suites in [TESTING.md](TESTING.md) (fuzz + protocol tests).

## 8. Constants to be fixed by ADR / early implementation

| Constant | Notes |
|----------|-------|
| Serialization format | postcard (ADR 0001) |
| Default TCP port | 27677 (ADR 0004), `DEFAULT_PORT` in `crossover-protocol` |
| Frame body maximum | 4 MiB + 64 KiB (ADR 0005): one maximum *text* item plus envelope per frame. Unchanged by chunking (ADR 0014) — larger content is split, never carried whole, because one giant frame is exactly what cannot be preempted (ADR 0013) |
| Max clipboard text / inline threshold | 4 MiB / 64 KiB (ADR 0005), named constants in `crossover-protocol` |
| Max clipboard image | 64 MiB, `MAX_CLIPBOARD_IMAGE_BYTES` (ADR 0014) — see below |
| Max clipboard file | 256 MiB, `MAX_CLIPBOARD_FILE_BYTES` (ADR 0015). Bounds the wire and the receiver's spool, **not** its memory: file chunks are written through as they arrive |
| Max archive entries | 256, `MAX_CLIPBOARD_FILE_ENTRIES` (ADR 0015) — entries one archived item may pack |
| Max file name | 255 bytes, `MAX_FILE_NAME_BYTES` (NTFS's per-component limit) and 259 UTF-16 units, `MAX_FILE_NAME_UTF16_UNITS` (`FILEDESCRIPTORW.cFileName` is `WCHAR[260]`). Both checked, so raising either cannot silently overrun a fixed-size Win32 buffer |
| Chunk payload maximum | 64 KiB, `MAX_CHUNK_BYTES` (ADR 0014) — a *maximum*, not a fixed size; see below |
| Chunk count maximum | 4096, `MAX_CHUNK_COUNT` = `MAX_CLIPBOARD_FILE_BYTES` ÷ `MAX_CHUNK_BYTES` — the largest chunked type over the largest chunk. Derived, and compile-time asserted against every chunked type's ceiling. Raising it does not raise what a transfer may cost: a plan must reconcile exactly with the offered length, which is bounded per type |
| Max monitors per machine | 16, `MAX_MONITORS_PER_MACHINE` (ADR 0018) — bounds `MonitorTopology` and one machine's share of a layout |
| Max monitors in a layout | 32, `MAX_LAYOUT_MONITORS` (ADR 0018) — a layout describes exactly two machines |
| Max monitor id | 64 bytes, `MAX_MONITOR_ID_BYTES` (ADR 0018), printable ASCII — the platform's device string (`szDevice` on Windows), which survives a restart where an enumeration index does not |
| Max monitor extent | 65 535, `MAX_MONITOR_EXTENT` (ADR 0018); minimum 1 — a zero-sized monitor has no edge to cross |
| Max layout coordinate | 2^24, `MAX_LAYOUT_COORDINATE` (ADR 0018) — with the extent cap this keeps every derivation under 2^42 in `i64`, so overflow is impossible rather than improbable |
| Monitor scale bounds | 25–500 percent, `MIN_SCALE_PERCENT` / `MAX_SCALE_PERCENT` (ADR 0018) — seeds the editor's to-scale drawing only; never enters crossing mapping |
| Keepalive interval / timeout | 5 s / 15 s defaults in `crossover-core::supervision` |

**Chunk size is the sender's to choose.** `MAX_CHUNK_BYTES` bounds a chunk;
it does not fix one. A receiver takes its plan from the size of **chunk 0**
and holds every later chunk to it — full-sized until the last, which is the
remainder — so a sender may use any size in `1..=MAX_CHUNK_BYTES` without
negotiating anything, and two peers using different sizes interoperate.

That matters because chunk size is the latency knob (ADR 0013): a frame
already being written cannot be preempted, so the worst delay an input
frame can suffer is roughly one chunk's write time. Reducing it is a
sender-side change, not a protocol change — which is what makes revisiting
64 KiB cheap when a measurement calls for it.

**Why 64 MiB for images.** Images travel as the source's native raster
bytes, verbatim and uncompressed (ADR 0014), so the ceiling has to cover a
full-screen grab in that form rather than a codec's idea of one. At 32 bits
per pixel: a 4K screenshot is 3840 × 2160 × 4 = 31.6 MiB, and a dual-4K
span is 7680 × 2160 × 4 = 63.3 MiB. 64 MiB admits the worst realistic
screenshot with margin, while the everyday case — snips of a few MB — sits
two orders of magnitude below it. It is also the receiver's worst-case
memory commitment, since the reassembly buffer is sized from the offered
length after that length is checked against this bound.

**Why 64 KiB chunks.** A chunk is the *preemption unit*: the writer emits
at most one background chunk before re-checking the interactive lane
(ADR 0013), so the worst-case delay a keystroke can suffer behind a bulk
transfer is about one chunk's transmit time.

| Link | Bytes/ms | 64 KiB chunk |
|------|----------|--------------|
| 2.5 GbE | 312 500 | 0.21 ms |
| 1 GbE | 125 000 | 0.52 ms |

Both are sub-millisecond, which is ADR 0013's budget, and the same size
keeps overhead negligible: a maximum image is 1024 chunks whose envelopes
(frame header plus postcard fields, well under 64 bytes each) total under
0.1 % of the payload. Smaller chunks would buy latency nobody can perceive
at the cost of more messages; larger ones eat straight into the
input-latency budget. This settles ADR 0013's open "exact chunk size"
question.
