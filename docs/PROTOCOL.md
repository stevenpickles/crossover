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
- Capabilities beyond the base protocol (optional clipboard types, multiple
  displays, …) are negotiated via `supported_features`, never assumed.
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

`FeatureFlags::ADVERTISED` is what this build actually sends, and it is
`ALL` — bit 0 set — since ADR 0014's platform slice. Every layer of the
promise is real: the wire carries chunked items, the clipboard engine
offers, streams, reassembles, verifies and installs them, and
`crossover-platform-windows` reads and writes `CF_DIB` on the actual OS
clipboard. Advertising is a promise to **handle**, and the last step that
could not be honoured is implemented, so the promise is honest.

The flip is **wire-visible**: the `Hello` a peer receives now carries
`supported_features = 1` where it carried `0`. That is why the golden
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

## 4. Message classes

Four logical classes, initially multiplexed over the single TLS connection:

| Class | Contents | Delivery requirement |
|-------|----------|----------------------|
| CONTROL | Hello, control-transfer negotiation, keepalive, ReleaseAllInput, session management | Ordered *within the class*, lossless |
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
ride a single frame — `ContentType::Image` today, up to
`MAX_CLIPBOARD_IMAGE_BYTES` = 64 MiB:

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
also triggers them from edge crossings, which carry a normalized `entry`
position so the destination places the cursor where the pointer crossed
(ADR 0009). The `entry` is `Option<u16>` — `0` top, `u16::MAX` bottom, a
fraction of the edge that is resolution- and DPI-independent; `None` for
an explicit (console) transfer, which places no cursor. Carrying it grew
the request and release layouts, which is the v1 → **v2** protocol bump.

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
- Disconnect in any state releases everything: the controlled side executes
  `ReleaseAllInput` locally (FR-4.4), the controller stops capture, and
  both sides are local until a new negotiation.

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
| Chunk payload maximum | 64 KiB, `MAX_CHUNK_BYTES` (ADR 0014) — a *maximum*, not a fixed size; see below |
| Chunk count maximum | 1024, `MAX_CHUNK_COUNT` = `MAX_CLIPBOARD_IMAGE_BYTES` ÷ `MAX_CHUNK_BYTES`. Derived, and compile-time asserted against the two constants it comes from |
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
