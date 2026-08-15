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
          edge crossing detected
 LOCAL ─────────────────────────────► REQUESTING_REMOTE
   ▲                                        │ peer acks ready
   │ reverse edge / peer confirms           ▼
 RETURNING ◄──────────────────────────  REMOTE
```

- Transitions are negotiated: request → acknowledge → switch (FR-5.3).
- Timeout or disconnect in any transitional state falls back to `LOCAL` and
  triggers `ReleaseAllInput` on the remote side (FR-4.4).
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

Chunks are emitted **one at a time**, sliced out of the retained buffer, so
the sender never materializes the whole split; each becomes its own command
and its own frame, which is what makes a chunk the preemption unit §5.4
depends on. On the receiving side the item's `content_hash` is verified over
the complete reassembly before the OS clipboard is touched, so a torn
transfer installs nothing (FR-3.2) — and `ClipboardApplied` is still emitted
only from the write result, never on receipt of the last chunk.

**The memory commitment is deliberate, bounded, and time-bounded — and
larger than "one buffer".** Each individual slot is singular, but the slots
are independent, so the honest worst case is their sum. Three item buffers
of up to `MAX_CLIPBOARD_IMAGE_BYTES` (64 MiB) are simultaneously reachable:

| Slot | Held while |
|------|-----------|
| `pending_write` | an item is being installed, including across the bounded `Busy` retry schedule |
| the inbound reassembly | a *newer* offer, accepted while that write is still retrying, streams in |
| the retained outbound item | a concurrent local copy is offered and awaiting its answer |

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
platform crate keeps its no-dependency rule, so its image-format tag is a
deliberate mirror of the protocol's, reconciled by one wildcard-free
mapping in `crossover-core::clipboard`.

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

Bound 2 catches *continuous* stalling, and a peer that alternates one brisk
write with one slow one evades it, since the brisk write clears the run —
per-frame input delay stays inside bound 1's keepalive timeout, which is what
this section claims, but such a peer is not disconnected; closing that needs
a duty-cycle measure rather than a continuity one, and it matters less once
ADR 0014's chunking caps how large a bulk frame can be.

What these bounds do **not** do is keep the session loop responsive *during*
a write. While one is pending the loop still polls nothing else, so a slow
peer delays input by up to one write — the guarantee is that this ends the
session in bounded time, not that it never happens. Two things shrink it:
ADR 0014's chunking makes the unit smaller, and moving the writer to its own
task would remove the freeze entirely. The latter is deferred, not
forgotten — it would take keepalive off the direct path to the writer that
ADR 0013 specifies, so it needs an ADR of its own.

Session **teardown** has an ordering requirement that falls out of all this.
When a session ends, its send path is retired — receiver dropped, registry
entry removed — *synchronously, before any other teardown step*. Dropping
the receiver is the only thing that closes the session's byte budget, and
closing that budget is the only thing that unparks the mux, the forwarder,
and the driver behind them. Every later step (draining the session's event
task, fanning the loss out to the drivers) pushes into the drivers' bounded
event channels, so doing any of it first waits on a driver that is waiting
on the send path that this drop releases. For the same reason the fanout
delivers to both drivers concurrently rather than in sequence: the clipboard
driver is the one that parks under bulk backpressure, and sequencing the
control driver behind it would gate `ReleaseAllInput` — a stuck key — on a
stalled transfer.

Keepalive never enters the queues at all: `run_session` writes `Ping`
straight to the writer on its idle tick and answers `Pong` from the dispatch
path — the strongest form of High there is.

Preemption granularity is bounded below by one frame: a frame in flight is
unpreemptable. [ADR 0014](adr/0014-chunked-rich-clipboard-transfer.md)'s
chunking is what shrinks that unit, which is why chunk size is a *latency*
knob answering to this section, not just a memory one.

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
- No global mutable state; no sleeps as synchronization; state machines are
  deterministic functions of (state, event) for testability.

## 7. Technology choices

| Concern | Choice | Notes |
|---------|--------|-------|
| Language | Rust (stable, MSRV pinned in `rust-toolchain.toml`) | Memory safety is a security requirement |
| Async runtime | tokio | |
| TLS | rustls + tokio-rustls, TLS 1.3 | See [SECURITY.md](SECURITY.md) |
| Transport | TCP (+ `TCP_NODELAY` for input/control traffic) | QUIC only if measurement demands it (ADR) |
| Serialization | **Deferred to ADR-pending** (postcard / CBOR / MessagePack / protobuf) | Must support explicit size limits, evolution, fuzzing, deterministic encoding; do not architect around serde-specific assumptions |
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
this file (they live in `SecureStorage`). The two-machine model needs a
single peer, so the peer is named inline under `[network]`; a richer
`[peer.<name>]` / named-`[layout]` model can be added under a new
`schema_version` if multi-peer arrangements arrive.

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
in `apps/crossover/src/logging.rs`. Conventions:

- **Metadata only.** Clipboard contents and private key material never
  appear in logs at any level (FR-7.4). Clipboard transactions log
  `clipboard_id`, `content_type`, `byte_count`, `content_hash`,
  `origin_peer`, `attempt_count`, `result`, `latency_ms` — never payloads.
- **Canonical field names**, snake_case, reused verbatim across crates so
  log lines correlate: `peer_id`, `session_id`, `message_id`,
  `clipboard_id`, `protocol_version`, `state`, `latency_ms`, `error`,
  `command`. Values go in fields; the event message is the human summary.
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
