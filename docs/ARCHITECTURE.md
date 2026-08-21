# Crossover Architecture

How Crossover is structured to satisfy [SPECIFICATION.md](SPECIFICATION.md).
Wire-level detail lives in [PROTOCOL.md](PROTOCOL.md); trust and cryptography
in [SECURITY.md](SECURITY.md).

---

## 1. Symmetric peer model

Every computer runs the same Crossover application. There is no distinct
server product and client product.

A given connection has a *listening* peer and a *connecting* peer, but that
is a transport detail. Application protocol semantics treat both devices as
equal authenticated peers, and the protocol encodes no permanent
primary/secondary distinction. A future peer may listen, connect, or both.

## 2. Layering

```
+----------------------------------------------------------+
|                     apps/crossover                       |
|            CLI, configuration, composition root          |
+----------------------------------------------------------+
|                     crossover-core                       |
|   Topology     Control-Transfer    Clipboard    Input    |
|   model        state machine       engine       engine   |
|                Connection lifecycle supervision          |
+----------------------------------------------------------+
|   crossover-protocol        |      crossover-security    |
|   framing, messages,        |      identity, pairing,    |
|   versioning, validation    |      trust store, TLS cfg  |
+----------------------------------------------------------+
|                Secure transport (TLS 1.3 over TCP)       |
+----------------------------------------------------------+

         platform boundary (trait objects, dependency inversion)
   ------------------------------------------------------------
+---------------------------+  +------------------------------+
|   crossover-platform      |  |  crossover-platform-windows  |
|   trait definitions only  |  |  Win32 implementations       |
+---------------------------+  +------------------------------+
```

Rules:

- Core logic (state machines, clipboard/input engines, topology, protocol)
  contains **no** direct OS API calls and compiles on all platforms.
- Platform crates implement traits defined in `crossover-platform`; nothing
  above the boundary names a Win32 API.
- Data flows into core as normalized events; core emits normalized commands
  that platform crates execute.

## 3. Workspace layout

```
crossover/
    Cargo.toml                      # workspace root
    apps/
        build_identity.rs           # shared by both build scripts: resolves the
                                    #   build version/commit/channel and emits it
        build_info.rs               # shared module: the BuildInfo the binaries
                                    #   report (`crossover version`). Source
                                    #   includes, not a crate, so the service
                                    #   binary gains no dependency edge.
        crossover/                  # the binary: CLI, config, wiring, worker
        crossover-svc/              # the service daemon (ADR 0011): a minimal
                                    #   Windows LocalSystem launcher. Depends
                                    #   ONLY on crossover-platform-windows —
                                    #   never on core/protocol/security — so the
                                    #   privileged process links no network code.
    crates/
        crossover-protocol/         # wire messages, framing, validation
        crossover-core/             # state machines, clipboard + input engines,
                                    #   topology, connection supervision
        crossover-security/         # device identity, pairing, trust store,
                                    #   TLS configuration
        crossover-platform/         # platform trait definitions (no OS deps)
        crossover-platform-windows/ # Win32 implementations
    tools/
        test-peer/                  # headless scriptable peer (see TESTING.md)
    tests/                          # cross-crate integration tests
    scripts/
        build.ps1                   # one command: gate, build, package (CI runs
                                    #   this same script)
    packaging/                      # install scripts and the Chocolatey package
    docs/
```

> **Phase 8 adds two members** ([ADR 0018](adr/0018-drawn-display-topology.md),
> recorded ahead of the implementation): `crates/crossover-topology` — the
> drawn-layout model, its validation, the config `[layout]` types and writer,
> and the state-file schema — and a layout-editor binary under `apps/`, the
> project's first GUI (its toolkit is the forthcoming ADR 0019). The crate
> exists so the editor shares the model and writer without linking
> core's protocol/security/platform graph; the tree above describes the
> build until those land.

### 3.1 Deliberately not separate crates (yet)

The baseline design prescribed twelve crates. We start with six and split
only when a boundary proves real:

| Candidate crate | Lives initially in | Split when |
|-----------------|--------------------|------------|
| `crossover-clipboard` | `crossover-core::clipboard` | Clipboard engine grows rich typed payloads or needs independent versioning |
| `crossover-input` | `crossover-core::input` | Input normalization becomes large enough to obscure core |
| `crossover-network` | `crossover-core::net` (+ app wiring) | A second transport (e.g., QUIC) or reuse outside the app appears |
| `crossover-config` | `apps/crossover` | Config is needed by tools/test-peer independently |
| `crossover-telemetry` | `tracing` usage throughout | Local metrics grow beyond counters and spans |
| `crossover-platform-macos` / `-linux` | not created | The corresponding port begins (Phase 7) |

Creating or dissolving a crate is an ADR-level decision. The compile-time
firewall that matters from day one is the **platform boundary** and the
**protocol crate's independence** (testable without sockets) — both exist
from Phase 0.

> **Phase 8's `crossover-topology`** is the first split this table did not
> anticipate: not one of the candidates above, but a new boundary created by
> [ADR 0018](adr/0018-drawn-display-topology.md) so the editor binary and the
> worker share one layout model and one config writer. `crossover-protocol`
> gains a dependency edge on it for the wire shapes, with the TOML writer
> behind a non-default `config` cargo feature so the protocol crate stays as
> dependency-light and socket-free as this section requires.

## 4. Platform abstraction layer

`crossover-platform` defines the traits; exact signatures are settled during
implementation, but the shape is:

```rust
trait ClipboardProvider   // observe changes, read, write, report contention
trait InputCapture        // deliver normalized local input events
trait InputInjector       // synthesize input on this machine
trait DisplayProvider     // enumerate displays, dimensions, positions, DPI
trait CursorController    // query/set pointer position, hide/show
trait SecureStorage       // protect private key material at rest
trait LinkStateProbe      // is the local interface carrying this peer up?
```

Guidelines:

- Traits are Crossover-shaped, not wrappers around any one OS's API surface.
  Third-party crates may be used *inside* a platform implementation, but the
  architecture must not assume any third-party clipboard/input abstraction
  behaves identically across operating systems.
- Every trait has a scriptable in-memory fake in the core test suite; the
  entire core must be exercisable with no real OS interaction
  ([TESTING.md](TESTING.md)).
- Windows implementations prefer the official `windows` bindings where
  practical. Platform quirks (hook timeout budgets, clipboard retry,
  DPI mapping — see SPECIFICATION.md §6) are handled *inside*
  `crossover-platform-windows`, surfacing as normalized events/errors.
- `ClipboardProvider` on Windows handles two content types, each in the
  OS's own representation (ADR 0014, and the rules that fall out of it are
  written up on `crossover-platform-windows::clipboard`):
  - **Read** prefers `CF_UNICODETEXT`, then `CF_DIB`. Windows *synthesizes*
    the DIB family (`CF_BITMAP` ↔ `CF_DIB` ↔ `CF_DIBV5`) on demand and
    reports synthesized formats as available, so one probe plus one
    `GetClipboardData` covers every raster source and Crossover converts no
    pixels itself.
  - **Text wins on a mixed clipboard** (Excel, Word, browsers publish both).
    A transaction carries one type; the image in a mixed item is a
    rendering of its text, text pastes into strictly more places, and the
    case ADR 0014 exists for — a screenshot — carries no text at all, so
    the choice costs that case nothing. Both probes run under **one**
    `OpenClipboard`: precedence decided across two opens could be applied
    to a pair of clipboard states that never coexisted (text absent in the
    first, an image found in the second), which is exactly what copying
    twice in quick succession produces. Non-empty text returns without
    probing `CF_DIB`, so the single open costs no extra lock time in the
    common case.
  - **Only copies happen under the clipboard lock.** It is machine-global,
    so the UTF-16 decode and the DIB canonicalization — both of which
    allocate — are handed back to the caller and run once it is closed.
  - **The ceiling bites at the source.** `GlobalSize` gives the blob's size
    before it is locked or copied, so an image past
    `MAX_CLIPBOARD_IMAGE_BYTES` is reported *absent* rather than copied out
    of the OS clipboard for a layer above to discard (FR-3.6, NFR-1). The
    ceiling is mirrored into `crossover-platform` — which may carry no
    dependencies — and a `crossover-core` test holds the mirror to the
    protocol's value.
  - **Bytes are canonicalized to the DIB's own length**, computed from the
    `BITMAPINFOHEADER` alone and never from pixels. Not cosmetics: loop
    prevention keys on the content hash, so trailing allocator slack that
    varied per hop would make Crossover's own write read back as new
    content — a sync loop, which is release-blocking. Anything the header
    does not describe confidently keeps the blob whole, so the failure mode
    is "a few unused bytes travel", never a truncated image.
  - **Write** installs `CF_DIB` verbatim (Windows synthesizes the rest of
    the family for pasting applications) and PNG verbatim under the
    registered `"PNG"` format, with the known limitation that nothing is
    synthesized from PNG. Nothing is ever transcoded between formats. The
    same ceiling is mirrored onto this path as a backstop — the bound that
    matters for NFR-1 is the one applied before an inbound image's
    reassembly buffer is allocated, so nothing should arrive here
    oversized, and a caller that is not the session fails closed rather
    than reaching Win32. Type is judged before size, so an oversized JPEG
    is refused for being a JPEG: the durable answer, where "too big"
    invites a smaller retry that must also fail.
- `LinkStateProbe` is a **diagnostic** capability, not an operational one: it
  is read on a failure path to label a log line and never gates, delays, or
  reorders anything (see §10). Its contract is therefore unusually strict —
  cheap, non-blocking, no `Result`, and no panic — and it answers three
  values, because "could not tell" must stay distinguishable from "the local
  link was fine". On Windows it is `GetBestInterfaceEx` (which interface
  routes to *this* peer) plus `GetIfEntry2` (that interface's
  `MediaConnectState` and `OperStatus`); platforms without an implementation
  use `UnknownLinkStateProbe`, which answers `Unknown` rather than
  pretending.
- `InputCapture` on Windows is backed by two mechanisms rather than one
  (ADR 0007): low-level hooks, because only they can suppress an event
  locally, and Raw Input, because only it reports unaccelerated,
  unclamped motion. `InputInjector` uses `SendInput`, tagging its events
  so they are never captured back — the same mark-what-you-emit pattern
  clipboard loop prevention uses.

## 5. Core state machines

The three central state machines live in `crossover-core`, are pure
(no I/O), and are unit- and property-tested exhaustively.

### 5.1 Control transfer

Exactly one active input destination at all times (FR-5.1).

```
     edge crossing or console request            peer grants
 LOCAL ──────────────────────────────► REQUESTING ───────────► REMOTE
   ▲                                        │                     │
   │        denied / timed out / cancelled  │                     │
   ├────────────────────────────────────────┘                     │
   │  handed back / peer revokes (reverse edge) / capture lost /   │
   └───────────────────────── disconnect ──────────────────────────┘
```

The three states are exactly `Outbound::{Local, Requesting, Remote}` in
`control.rs`. [ADR 0009](adr/0009-seamless-edge-transfer.md) promised a
fourth, `RETURNING`, between `REMOTE` and `LOCAL`; it was subsumed by this
design — the reverse crossing is detected on the *controlled* side, which
revokes, so the controller returns to `LOCAL` on the resulting release with
no transitional state of its own.

- Transitions are negotiated: request → acknowledge → switch (FR-5.3).
- Timeout or disconnect in any transitional state falls back to `LOCAL` and
  triggers `ReleaseAllInput` on the remote side (FR-4.4).
- **Late answers are self-correcting.** A request that timed out cancels
  itself on the wire, and a re-request from the session that already holds
  the grant refreshes it rather than being denied, so a slow answer cannot
  leave the two machines disagreeing about who controls whom (ADR 0009
  addendum, 2026-08-19). Denial for a request from any *other* session is
  unchanged — that is the security boundary below.
- While `REMOTE`: local input is captured and forwarded, local effects are
  suppressed, pointer position maps through the topology model.
- **Authorization is scoped to the session that holds the grant.** The
  engine is session-aware on both axes: the outbound state remembers which
  session it controls (batches and releases route only there), and the
  inbound grant records which session controls this machine. Every
  injection is checked against that grant-holder's identity, so a
  peer authenticated by TLS but holding no grant — or holding a grant on a
  *different* session — cannot inject; its input terminates its own session
  (FR-2.3). This is complete mediation on the principal, not an assumption
  that only one peer is ever connected: it holds for any number of trusted
  peers. Authentication is not authorization.

### 5.2 Clipboard transaction engine

Owns the invariants of FR-3.x: acknowledged installs, loop prevention via
origin + content-hash tracking of recently applied items, deduplication,
bounded retry with observable failure, deterministic latest-wins conflict
policy. Message flow is specified in [PROTOCOL.md](PROTOCOL.md) §5.

Each observed OS clipboard change becomes an immutable `ClipboardItem`
(id, origin peer, sequence, timestamp, content type, length, hash, content).
Contents are never logged; metadata is (FR-7.4).

**Content is typed and opaque.** Since
[ADR 0014](adr/0014-chunked-rich-clipboard-transfer.md) an item is a
`ContentType` plus bytes — text is one type, a raster image another — and
nothing above the platform boundary transcodes, compresses, or parses image
bytes. The hash and the length are the only things ever computed over them.

**The state machine, both directions.** At most one transaction is in
flight per direction; a newer one supersedes the older, which is the rule
that keeps every buffer singular.

| Direction | States | Retains |
|-----------|--------|---------|
| Outbound | `AwaitingAccept` → (`Streaming` for chunked types) → `AwaitingApplied` | the item buffer, until the last chunk is out |
| Inbound | accepted offer → (`ChunkReassembly` for chunked types) → pending write | the reassembly buffer, until it verifies |
| Inbound, files | `Admitting` → `Streaming` → `Verified` → `Committing` → offering | an open partial on disk, and accounting — never the bytes |

Chunks are emitted **one at a time**, sliced out of the retained buffer, so
the sender never materializes the whole split; each becomes its own command
and its own frame, which is what makes a chunk the preemption unit §5.4
depends on. On the receiving side the item's `content_hash` is verified over
the complete reassembly before the OS clipboard is touched, so a torn
transfer installs nothing (FR-3.2) — and `ClipboardApplied` is still emitted
only from the write result, never on receipt of the last chunk.

**Files are the one inbound type that is written through rather than
held** ([ADR 0015](adr/0015-spooled-virtual-file-paste.md)). The engine
stays sans-io: it holds a `ChunkStream` — the same admission rules as
`ChunkReassembly`, with a running hash and length instead of a buffer —
and emits actions the driver performs against a `SpoolStorage`
(`AdmitFile`, `WriteFileChunk`, `CommitFile`, `AbortFile`,
`EvictSpoolEntry`), each answered back into the engine exactly as a
clipboard write's result is. So the sequencing stays testable without a
filesystem, and three properties hold by construction: a chunk is judged
before it is written, an offer is answered only once the spool has taken
the transfer, and every outcome but a verified completion deletes the
partial and registers nothing. `MAX_CONCURRENT_FILE_TRANSFERS` is the
`Option` holding that state, not a counter.

A file transfer is not finished when its bytes are: the engine offers the
verified entry to the platform's paste mechanism and holds the origin's
verdict until that lands, so `Stored` means *the user can paste this*
rather than *the bytes are somewhere*. That is also what makes ADR 0015's
entry-lifetime rule expressible — an entry lives while the clipboard still
offers what it backs — and the same platform answer serves loop
prevention, since a virtual file list has no bytes to hash and the
applied-hash memory has nothing to match on (F13).

Whether files may be received at all is a **policy input**
(`FileReceive`), supplied by the composition root from the trust store and
refreshed as it changes, because a sans-io engine can see neither the
`file_receive` grant nor whether a protected spool was opened. It defaults
to the closed value, and the driver clamps it closed again unless it has
**both** a spool and somewhere to paste from — either alone can accept a
transfer it cannot deliver.

**The memory commitment is deliberate, bounded, and time-bounded — and
larger than "one buffer".** Each individual slot is singular, but the slots
are independent, so the honest worst case is their sum. Three item buffers
of up to `MAX_CLIPBOARD_IMAGE_BYTES` (64 MiB) are simultaneously reachable:

| Slot | Held while |
|------|-----------|
| `pending_write` | an item is being installed, including across the bounded `Busy` retry schedule |
| the inbound reassembly | a *newer* offer, accepted while that write is still retrying, streams in |
| the retained outbound item | a concurrent local copy is offered and awaiting its answer |

A 256 MiB file adds nothing to that figure: its bytes go to the spool a
chunk at a time and the engine's commitment is O(chunk).

That is **192 MiB**, plus the Background lane's 8 MiB byte budget — about
**200 MiB** steady-state — and transiently ~264 MiB during a supersession,
where the buffer being replaced is alive alongside its replacement. Every
one of those is sized only after the declared length was validated against
its type's maximum (NFR-1), and none is unbounded; the number is simply
larger than the count of slots suggests, which is why it is written down
rather than left to be inferred.

Session-scoped cleanup is not a bound on its own — a session can live for
days — so every transaction carries a deadline
(`ClipboardConfig::transfer_timeout`). Expiry releases the buffers, tells
the origin of an inbound transfer that nothing was installed so its
transaction closes rather than stalling (NFR-3), counts the abandonment
(FR-7.3), and leaves the machine able to start the next transfer
immediately. The deadline covers `AwaitingApplied` too, where almost no
memory is at stake but the single outbound slot is: an unanswered
transaction left there would go on deciding conflict races (FR-3.5).

**The platform boundary is typed with it.** `ClipboardProvider` reads and
writes a `ClipboardContent` (text or image-with-format); every raster
format concern — `CF_DIB` and the rest — lives behind it in
`crossover-platform-*`, and core names no OS clipboard format (NFR-4). The
platform crate keeps its no-dependency rule, so its image-format tag and
its image size ceiling are deliberate mirrors of the protocol's, reconciled
by one wildcard-free mapping and one equality assertion in
`crossover-core::clipboard`. §4 records what the Windows side of that
boundary actually does.

### 5.3 Connection lifecycle

```
IDLE → CONNECTING → AUTHENTICATING → NEGOTIATING → ESTABLISHED
                                          │ any failure
   RECONNECT_WAIT (bounded backoff) ◄─────┴── fail closed
```

- Authentication and version negotiation failures terminate the session with
  a diagnostic; there is no partially-trusted state (FR-2.3).
- `ESTABLISHED` supervises keepalive, reconnect (FR-6.2), and channel
  backpressure. Loss of the session while `REMOTE` triggers the control
  transfer fallback above.
- Every session captures a `LinkDiagnostics` at establishment — the peer
  socket address it actually uses, plus the platform's `LinkStateProbe` —
  so the disconnect record can name the *local* link state (§10). It is read
  only on the way to a log line: `RECONNECT_WAIT` and its backoff are
  identical whatever it says.

### 5.4 Outbound send path: two priority classes

A session is a **single TLS-over-TCP stream**, so every frame the
application sends is serialized onto one ordered byte pipe. Phase 7's rich
clipboard puts multi-megabyte payloads on that pipe, and a plain FIFO would
let one of them head-of-line block the pointer and keyboard — violating
NFR-5 and priority #5. [ADR 0013](adr/0013-interactive-over-bulk-prioritization.md)
splits the path into two classes; `crossover-core::outbound` implements it.

**Classification** is by message type, at the moment a frame enters the
path:

| Class | Messages | Why |
|-------|----------|-----|
| **High** | `InputBatch`, `ReleaseAllInput`, `ControlRequest`/`Response`/`Release`, `Ping`/`Pong`, `Hello`, pairing | the live input path, and the negotiation that decides who owns it |
| **Background** | the whole clipboard transaction — `Offer`, `Accept`, `Decline`, `Data`, `Applied` — and any message type this build does not recognize | bulk, and things whose latency budget we cannot vouch for |

The small clipboard messages ride Background *with* the bulk ones
deliberately. Splitting a transaction across classes would let its
acknowledgement overtake its data, and the transaction state machine
(ADR 0005) depends on those messages arriving in the order they were
produced. One lane per transaction keeps that invariant for free and costs
the clipboard only latency — which SPECIFICATION.md §2 never ranks above
input. `TerminateSession`, the fail-closed kill, is High: it is a security
action and must not queue behind a transfer.

**The split spans every hop, and starts at the driver.** Each driver emits
into its *own* High/Background pair (`command_lanes`); the mux merges those
with one task per source lane; each session has its own pair again, drained
by the writer. Every queue on the path is single-class from end to end.

That is not decoration, and a partial split does not work:

- The mux *awaits* delivery into a session's queue, so one task draining both
  classes would let a saturated Background path stall input for **every**
  session — the head-of-line block moved upstream rather than removed.
- A driver emitting into one mixed queue is worse still. A forwarder reading
  that queue handles the two classes in sequence, so while it is parked
  handing a bulk command downstream it cannot pick up that driver's next High
  command; and the mixed queue itself fills with bulk, burying anything
  behind it. Concretely: a peer stops reading its socket until the lanes
  fill, then sends a malformed clipboard payload — and the fail-closed
  `TerminateSession` that payload must trigger never reaches the session.
  Refusing to read would have bought that peer immunity from PROTOCOL.md §7.

So: **no Background backpressure anywhere on the path can delay a High frame
in transit.** One honest limit remains, upstream of the path: a driver is a
serial event loop, and a driver parked on Background backpressure is not
emitting *anything* until it drains — its own High commands included. That
is correct backpressure — it is how the wire tells the clipboard engine to
stop producing. For an **accepted (listening) session** it is also bounded: a
peer that will not consume ends the session by the write bounds below, and
teardown then retires the path, which is what actually unparks the driver.
Both halves are needed; the ordering that makes the second one work is
spelled out below, because getting it wrong deadlocks the application.

**Known limitation — the outbound (supervised) role.** There the send path
belongs to the supervisor, which holds it across reconnects, so a wedge
clears only when the *next* session establishes. The honest bound:
self-healing on reconnect, but **unbounded if the peer never returns**, and
in that window local input suppression is possible — a control driver whose
own High path is also full stops emitting. Two things limit the damage and
neither removes it: the outbound path removes the session from the registry
before fanning the loss out, and the remote peer releases the input it holds
on its own (FR-4.4). It is deliberate rather than overlooked: clearing the
wedge means discarding queued frames on disconnect, which contradicts
`SupervisorHandle::send`'s documented flush-on-reconnect contract, so
changing it is an ADR-level decision, not a bug fix.

**Drain policy: strict High-first, no aging.** The writer takes everything
queued High before a *single* Background frame, then re-checks High. Because
it writes exactly one frame per iteration, the re-check happens between
every pair of frames — which is what keeps the kernel send buffer shallow
enough for app-level priority to reach the wire; queueing several bulk
frames at once would put input bytes behind them where no scheduler can
reach. Strict priority admits unbounded Background starvation in theory, and
that is the accepted trade: real input is bursty, so bulk progresses in the
gaps, while a clipboard transfer has no deadline and a late `ReleaseAllInput`
is a stuck key. Aging (promoting starved bulk) would buy liveness nobody
needs at the cost of the one guarantee this exists to provide. If sustained
input ever does stall a transfer in practice, that is a measurement to act
on, not a policy to pre-empt.

**Bulk is reordered, never dropped.** Each class keeps its own FIFO order;
cross-class reordering is the only thing prioritization changes
([PROTOCOL.md](PROTOCOL.md) §4). Nothing is discarded to keep up.

**Bounds** (NFR-1) are named constants in `crossover-core::outbound`, and the
same pair applies at every hop. The High lane is bounded by message count
(`MAX_HIGH_QUEUE_FRAMES` = 64; interactive frames are tens of bytes). The
Background lane is bounded by **bytes** as well as messages
(`MAX_BACKGROUND_QUEUE_BYTES` = 8 MiB, `MAX_BACKGROUND_QUEUE_FRAMES` = 64) —
sixty-four queued maximum-size clipboard frames would be a
quarter-gigabyte commitment *per hop*. The byte budget is held until an item
has been passed on, not merely dequeued, and a frame larger than the whole
budget still passes on an empty lane rather than deadlocking on permits that
cannot exist. Producers block on these bounds; that backpressure is the
design, and it never crosses into the High lane. Dropping the receiving end
closes the budget, so a producer parked on it unwinds at teardown instead of
waiting for a permit the departed writer still holds.

**Every write is bounded.** The writer sends inside a `select!` branch body,
so while a write is pending the session loop polls neither the reader nor
the keepalive tick. Against a peer that stops reading its socket, an
unbounded write would park forever and freeze the idle clock with it: the
keepalive timeout could never fire, the session would never disconnect, and
the `ReleaseAllInput` a disconnect triggers would never run — held keys stay
held (FR-4.4). Two bounds, both fail-closed, make that state terminal:

1. **No single write may exceed the keepalive timeout.** A cancelled write
   leaves the TLS stream mid-record and unusable, so expiry is necessarily
   fatal rather than a retry.
2. **Application writes may not stall continuously for longer than the
   keepalive timeout.** A write slower than the keepalive *interval* counts
   as stalling; any faster write clears the run, and a genuinely idle spell
   clears it too (an empty outbound queue is health, not a stall). This is
   what catches a peer that accepts one frame just inside bound 1's deadline
   for ever — bound 1 alone resets every frame and waves that through.
   Keepalive frames are excluded: a `Ping` is a dozen bytes and fits in any
   window that is open at all, so it is no evidence of throughput and must
   neither count as a stall nor clear one.

The polite TLS shutdown on the way out is bounded too, for the same reason:
it has to flush, and the commonest reason to be closing is a peer that
stopped reading.

**And so is the one hop that pushes back the other way.** Everything above
describes backpressure heading *out*; the session's event channel is the
only place it points *in*, because `run_session` hands each inbound frame to
the application and waits for it to be accepted. That wait sits in the same
`select!` as the outbound drain, the `Pong` answer, and the keepalive tick,
so an application that stops consuming freezes all of them — and the write
bounds above cannot help, because they only cover a write already in
progress, and the keepalive check lives in the loop that stopped turning.
The state is reachable without any local bug: sustained High traffic starves
the Background lane by design, which parks the clipboard driver, which stops
it draining its own events, which parks the fanout, which stops the session's
event drain, which fills this channel. Every hop is legitimate backpressure;
the cycle is not. So the hand-off carries the same keepalive-timeout deadline
as the writes, and expiry ends the session as `EventConsumerStalled` — a
*local* fault, kept distinct from a transport one so a soak report does not
go hunting the network. Teardown then retires the send path and unparks the
whole chain: a wedged event chain costs the session, not the process.

A peer cannot use this against a healthy session. All it controls is how
fast frames arrive, and a consumer that is running accepts each one in
microseconds however hard it is pushed — a flood meets backpressure, which
slows the peer down, and never approaches a multi-second wait for a single
hand-off. Reaching the deadline requires the consumer chain to have stopped.
A peer *can* stop it, by driving the cycle above, and killing the session is
then exactly right: one whose frames are neither dispatched nor answered is
doing nothing but holding the chain hostage.

Bound 2 is a **duty cycle, not a run** — and it had to become one. Measured
as a continuous run of stalling writes, any single brisk write cleared it, so
a peer alternating one slow write with one fast one stalled the session
indefinitely and was never disconnected: per-frame delay stayed inside
bound 1, and the run never survived long enough to trip bound 2. This section
carried that as an open residual until it was closed.

What replaced it is a leaky bucket. A write slower than the keepalive
interval charges its whole duration, because none of that time was usable
throughput; every other interval — brisk writes and idle gaps alike — pays
the debt back at the rate it actually earned. The session ends when the
outstanding debt reaches the keepalive timeout. A continuous stall fills the
bucket exactly as fast as the old measure did, so nothing that was caught
before escapes now; alternating brisk writes buy a millisecond of
forgiveness for a millisecond of throughput rather than an amnesty; and a
link that hiccups once and then works is forgiven, because a minute of idle
pays off any debt a single write can create.

What these bounds do **not** do is keep the session loop responsive *during*
a write. While one is pending the loop still polls nothing else, so a slow
peer delays input by up to one write — the guarantee is that this ends the
session in bounded time, not that it never happens.

**Measured, 2026-08-16.** Over WiFi, with a saturating image transfer
running, input frames waited a mean of 2.9 ms and a worst case of **124 ms**
for the wire. The split says where: 124.3 ms of that maximum was the frame
waiting for the writer and 0.18 ms was the socket accepting its own bytes.
So the delay is entirely this paragraph's effect — one in-flight 64 KiB
bulk chunk, on a link slow enough to make that write take a tenth of a
second.

Two things shrink it, and an earlier version of this section named a third
that does not:

- **ADR 0014's chunking makes the unit smaller.** This is the only lever
  that acts on the delay directly, and it is a *sender-side* knob: the
  receiver takes its plan from the first chunk's size
  ([PROTOCOL.md](PROTOCOL.md) §8), so a sender may use anything up to
  `MAX_CHUNK_BYTES` without negotiation or a protocol change. Held at
  64 KiB pending a wired measurement — ADR 0013's arithmetic assumed
  2.5 GbE, where the same chunk is 0.21 ms rather than 124 — and **left
  there once that measurement was taken**, below.
- **A faster link.** The measurement above is wireless; the design target
  is wired.
- **Moving the writer to its own task does _not_ fix this**, though this
  section used to imply it would. A writer task still writes serially into
  one TLS stream, so an input frame cannot overtake a bulk frame already
  being written — the wait is unchanged. What it *would* fix is the loop
  being unable to poll reads and the keepalive tick during a write, which
  is a real but different problem. Genuine preemption mid-frame needs
  separate streams (QUIC) or a second connection, both considered and
  rejected in ADR 0013.

**Re-measured wired, 2026-08-21.** The same instrumentation on a direct
2.5 Gbps link, with one writer carrying continuous input and a bulk file
stream at once (ten 200 MiB transfers back-to-back, 4,558 input frames
timed): the socket took **0.019 ms** on average and **0.147 ms** at worst to
accept an input frame's bytes, with the wait for the writer averaging
0.41 ms and queue-to-wire totalling 0.43 ms average. The worst case is
smaller than the 0.21 ms one 64 KiB chunk costs at this speed, so the
in-flight-frame delay this section describes is real but sub-millisecond on
the design link, and the WiFi figures above were the link rather than the
chunk. **The chunk size therefore stays at 64 KiB** (maintainer, 2026-08-20;
[ADR 0013](adr/0013-interactive-over-bulk-prioritization.md)'s 2026-08-20
addendum), which closes the "pending a wired measurement" hold in the first
bullet, and the writer-task work the third bullet already argued would not
help has no latency case to make for it either. One tail of ~72 ms did occur
in the lane *before* the writer while socket writes stayed at 0.147 ms or
below — one sample in 4,558, tracked as a scheduling question in the
roadmap's Phase 7 follow-ups, and not this section's effect.

Session **teardown** has an ordering requirement that falls out of all this.
When a session ends, its send path is retired — receiver dropped, registry
entry removed — *synchronously, before any other teardown step*. Dropping
the receiver is the only thing that closes the session's byte budget, and
closing that budget is the only thing that unparks the mux, the forwarder,
and the driver behind them. Every later step (draining the session's event
task, fanning the loss out to the drivers) pushes into the drivers' bounded
event channels, so doing any of it first waits on a driver that is waiting
on the send path that this drop releases. For the same reason the fanout
delivers *session lifecycle* events to both drivers concurrently rather than
in sequence: the clipboard driver is the one that parks under bulk
backpressure, and sequencing the control driver behind it would gate
`ReleaseAllInput` — a stuck key — on a stalled transfer. Inbound *frames* are
not fanned out at all any more; they are routed by class (§5.5).

Keepalive never enters the queues at all: `run_session` writes `Ping`
straight to the writer on its idle tick and answers `Pong` from the dispatch
path — the strongest form of High there is.

Preemption granularity is bounded below by one frame: a frame in flight is
unpreemptable. [ADR 0014](adr/0014-chunked-rich-clipboard-transfer.md)'s
chunking is what shrinks that unit, which is why chunk size is a *latency*
knob answering to this section, not just a memory one.

### 5.5 Inbound frame routing

The same two classes, read the other way round. §5.4 splits what this machine
*sends*; this is what it does with what arrives.

`run_session` hands each decoded application frame to the application's
per-session frame pump, which is strictly serial. The pump used to broadcast
every frame to **both** the clipboard driver and the control driver and await
both — each driver discarding whatever was not its own. That made a
`ControlRequest`'s delivery wait on the clipboard driver's queue, and under
bulk backpressure that wait was **4.7 s** on 2026-08-19 hardware, which cost
the requester a control timeout ([ADR
0013](adr/0013-interactive-over-bulk-prioritization.md) addendum, 2026-08-19).

Frames are now **routed by message type to exactly one driver**:

| Route | Messages |
|-------|----------|
| **sync driver** | `ClipboardOffer`/`Accept`/`Decline`/`Data`/`Chunk`/`Applied` |
| **control driver** | `InputBatch`, `ReleaseAllInput`, `ControlRequest`/`Response`/`Release` |
| **both** | a message type this build does not recognize |

The classification is **total**, and the unrecognized type is why the third
row exists: whether to ignore an unknown frame is a driver's decision, not
the classifier's, so it keeps its historical delivery to both rather than
becoming a silent drop. Nothing else reaches here — `dispatch_frame` answers
`Ping`, accepts `Pong`, and fails the session on `Hello` or a pairing
message, so those never become application frames.

Two invariants the routing keeps. **Order within a driver is arrival order**:
one frame is delivered before the next is classified, which ADR 0005's
transaction state machine and the applied-input sequence both require.
**Nothing is buffered**: routing adds no queue, it removes a wait.

It does not give inbound preemption, and cannot. Backpressure from a
saturated clipboard path still reaches the peer, and one ordered TCP stream
then delays whatever the peer sent behind it — priority #2 (clipboard
reliability) outranks #5 (input latency), so bulk is never dropped to clear
the way. What is guaranteed is narrower: an interactive frame is never
delayed by a queue belonging to a driver with no interest in it.

## 6. Concurrency model

- Async runtime: **tokio** (multi-threaded). Chosen for maturity; revisit
  only with evidence, via ADR.
- Platform event sources (hooks, clipboard listeners) run on dedicated OS
  threads owned by the platform crate — Windows hook callbacks must return
  in microseconds (risk R-2) — and forward events into core over bounded
  channels.
- All queues are bounded (NFR-1) — bulk queues by bytes as well as message
  count, since a message count alone is not a memory bound once payloads
  are megabytes (§5.4). Backpressure policy differs by class: pointer motion
  coalesces (newest wins), keyboard and clipboard messages are lossless and
  ordered, per [PROTOCOL.md](PROTOCOL.md) §6. Backpressure on a bulk queue
  must never propagate to an interactive one, which is why the send path
  runs a task per priority class rather than a task per stage (§5.4).
- **A level travels on a `watch`, not a queue.** State that means "what is
  true right now" — desired cursor visibility, the edge detector's watching
  mode — is latest-wins by nature, so queueing it buys nothing and costs a
  blocking send. That matters where the consumer feeds back into the
  producer: the edge mode used to ride a bounded `mpsc` inside a cycle
  (control loop → mode → detector → crossings → control loop, which the same
  loop drains), so the loop's own slowness pushed back on itself. Anything
  that must be *counted* across such a channel has to travel inside the
  value, since a `watch` coalesces — which is how the crossing generation is
  carried ([ADR 0009](adr/0009-seamless-edge-transfer.md)).
- No global mutable state; no sleeps as synchronization; state machines are
  deterministic functions of (state, event) for testability.

## 7. Technology choices

| Concern | Choice | Notes |
|---------|--------|-------|
| Language | Rust (stable, MSRV pinned in `rust-toolchain.toml`) | Memory safety is a security requirement |
| Async runtime | tokio | |
| TLS | rustls + tokio-rustls, TLS 1.3 | See [SECURITY.md](SECURITY.md) |
| Transport | TCP (+ `TCP_NODELAY` for input/control traffic) | QUIC only if measurement demands it (ADR) |
| Serialization | postcard ([ADR 0001](adr/0001-wire-serialization-format.md)) | Explicit size limits, deterministic encoding, fuzzed parsers; `default-features = false` drops the unmaintained `atomic-polyfill` heapless brings in |
| Archives (sender only) | `zip`, `default-features = false` | Write-only, Stored entries: no codec is built, so no decompression backend enters the tree, and nothing in the workspace reads an archive ([SECURITY.md](SECURITY.md) F9). `clippy.toml` disallows the reader type so the ban is mechanical rather than a convention |
| Logging | tracing + tracing-subscriber | Structured from first commit |
| CLI | clap | |
| Config | TOML, versioned schema | |
| Errors | thiserror in library crates, contextual (anyhow-style) at app boundary | Conventions and exemplar: §9 |

Dependency policy: dependencies are welcome for mature commodity
functionality; each meaningful one is evaluated for maintenance, security
history, unsafe usage, transitive weight, cross-platform behavior, and
license. Critical application semantics (trust decisions, clipboard
transaction logic, control transfer) remain in Crossover code.

## 8. Configuration (initial shape)

The startup config file (`~/.crossover/config.toml`) is sectioned and versioned
so it can evolve without breaking a hand-edited file. Every field is optional
and every CLI flag overrides its file counterpart. (Config and logs live under
`~/.crossover`; secrets stay DPAPI-encrypted under `%LOCALAPPDATA%\Crossover`.)

```toml
schema_version = 1

[device]
name = "workstation-left"

[network]
listen = "0.0.0.0:27677"          # present = accept inbound peers (default port, ADR 0004)
connect = "192.168.1.25:27677"    # dial this peer

[seamless]
side = "right"                    # "left" | "right" — this machine's screen side

[cursor]
mask = true                       # hide the local cursor while driving the peer
```

Validated on load with actionable errors (unknown keys and unsupported
`schema_version` are rejected); deterministic defaults; no private keys in
this file (they live in `SecureStorage`).

> **Phase 8 changes two things here** ([ADR 0018](adr/0018-drawn-display-topology.md),
> recorded ahead of the implementation): `schema_version` moves to **2** and a
> `[layout]` section — the drawn arrangement — replaces `[seamless] side`, with
> a v1 file loading as an implicit layout that reproduces the old behaviour.
> And "every CLI flag overrides its file counterpart" gains its one deliberate
> exception: an **explicit** `[layout]` beats `--left` / `--right`, because the
> service's saved command line (ADR 0011) would otherwise flatten a drawn
> arrangement on every launch. The ADR carries the reasoning; the shape above
> describes the build until that lands.

The two-machine model needs a single peer, so the peer is named inline
under `[network]`; a richer `[peer.<name>]` / named-`[layout]` model can be
added under a new `schema_version` if multi-peer arrangements arrive.

## 9. Error-handling conventions

Established while the workspace was empty so later work copies a convention
instead of inventing one. `ProtocolError` in `crossover-protocol` is the
living exemplar.

- **Library crates define typed errors** with `thiserror`: one
  `#[non_exhaustive]` enum per cohesive failure domain. Variants carry the
  data an actionable diagnostic needs (FR-7.1) — numbers and identifiers as
  fields, not pre-formatted strings.
- **Causal chains are preserved.** Underlying failures are wrapped with
  `#[source]`/`#[from]`; a cause is never flattened into a message string.
- **Security failures stay distinguishable.** Authentication, authorization,
  and validation failures get their own types or variants — never collapsed
  into generic network/protocol/clipboard/input/config errors — so
  fail-closed paths and diagnostics can discriminate
  ([SECURITY.md](SECURITY.md) invariant 1).
- **Untrusted input produces values, not panics.** Every path reachable from
  network input returns `Result`; `unwrap`/`expect`/`panic!` on such paths
  is a defect (NFR-1).
- **Errors are comparable values** (`PartialEq` where contents allow) so
  state-machine tests assert exact rejection reasons.
- **The app boundary is contextual.** `apps/crossover` attaches operational
  context (anyhow-style), rendering concise user-facing messages while
  structured logs retain detail (FR-7.3). The `anyhow` dependency arrives
  when `main` first becomes fallible (CLI slice), not before.

## 10. Logging conventions

`tracing` is wired from Phase 0 (FR-7.3); the subscriber is installed once
in `apps/crossover/src/logging.rs`. The service daemon (`crossover-svc`,
ADR 0011) installs its own subscriber the same way in
`apps/crossover-svc/src/logging.rs`, but to a different file location —
`%ProgramData%\Crossover\logs` rather than `~/.crossover/logs` — because it
runs as `LocalSystem`, not the console user, so `~` there is the SYSTEM
profile (ADR 0011 addendum, 2026-08-19). Conventions:

- **Metadata only.** Clipboard contents and private key material never
  appear in logs at any level (FR-7.4). Clipboard transactions log
  `clipboard_id`, `content_type`, `byte_count`, `content_hash`,
  `origin_peer`, `attempt_count`, `result`, `latency_ms` — never payloads.
- **Canonical field names**, snake_case, reused verbatim across crates so
  log lines correlate: `peer_id`, `session_id`, `message_id`,
  `clipboard_id`, `protocol_version`, `state`, `latency_ms`, `error`,
  `command`, `local_link`. Values go in fields; the event message is the
  human summary.
- **A disconnect says which side broke.** `local_link` (`up` / `down` /
  `unknown`, from `LinkStateProbe`) is carried by the two records where the
  OS's own wording misleads: the session-end warning, and the
  connect-attempt failure. It exists because a NIC that drops its physical
  link ends the session on **both** machines with `An existing connection
  was forcibly closed by the remote host` — false on both ends, and
  disproving it once cost a manual correlation of two machines' event logs.
  Rules that keep the field worth reading:
  - It is asked only where a dead local interface is a possible cause — a
    transport failure or a keepalive timeout. A protocol violation and a
    clean peer close are not, so those lines carry no `local_link` at all
    rather than a permanently uninformative one.
  - `down` also changes the *message*, not just a field, so the conclusion
    survives being read at a glance: `session ended; local link is down, so
    the disconnect is local, not the peer`.
  - `unknown` never reads as exoneration. `up` is evidence too, and is
    recorded, but no line claims a local fault without `down`.
- **Spans scope lifecycles**: one span per connection session (carrying
  `session_id` and `peer_id`), per clipboard transaction, per control
  transfer. Events inside inherit those fields — no re-stating.
- **Levels**: `info` — important state transitions (the NFR-3 observability
  floor); `debug` — protocol detail; `trace` — per-event input chatter;
  `warn`/`error` — observable failures, carrying an `error` field.
- **Filtering** via `RUST_LOG` (env-filter), defaulting to `info`. Logging
  must not materially degrade input responsiveness (NFR-5): the hot input
  path logs at `trace`, and tracing's static max-level features may cap
  release builds if measurement warrants.
