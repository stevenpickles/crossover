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

**The split spans every hop, not just the last one.** Each driver's
`SessionCommand` stream is classified where the streams merge; from there
High and Background are separate queues, drained by **separate mux tasks**,
into **separate per-session lanes**, to the writer. This is not decoration:
the mux *awaits* delivery into a session's queue, so one task draining both
classes would let a saturated Background path stall input for every
session — the head-of-line block moved upstream rather than removed. No
Background backpressure at any hop can delay a High frame.

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

**Bounds** (NFR-1) are named constants in `crossover-core::outbound`. The
High lane is bounded by message count (`MAX_HIGH_QUEUE_FRAMES` = 64;
interactive frames are tens of bytes). The Background lane is bounded by
**bytes** as well as messages (`MAX_BACKGROUND_QUEUE_BYTES` = 8 MiB,
`MAX_BACKGROUND_QUEUE_FRAMES` = 64) — sixty-four queued maximum-size
clipboard frames would be a quarter-gigabyte commitment per hop. The byte
budget is held until a frame has been *written*, not merely dequeued, and a
frame larger than the whole budget still passes on an empty lane rather than
deadlocking. Producers block on these bounds; that backpressure is the
design, and it never crosses into the High lane.

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
