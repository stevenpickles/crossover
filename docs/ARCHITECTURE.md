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
        crossover/                  # the binary: CLI, config, wiring
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

## 6. Concurrency model

- Async runtime: **tokio** (multi-threaded). Chosen for maturity; revisit
  only with evidence, via ADR.
- Platform event sources (hooks, clipboard listeners) run on dedicated OS
  threads owned by the platform crate — Windows hook callbacks must return
  in microseconds (risk R-2) — and forward events into core over bounded
  channels.
- All queues are bounded (NFR-1). Backpressure policy differs by class:
  pointer motion coalesces (newest wins), keyboard and clipboard messages
  are lossless and ordered, per [PROTOCOL.md](PROTOCOL.md) §6.
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
| Errors | thiserror in library crates, contextual (anyhow-style) at app boundary | Security errors distinguishable from network/protocol/clipboard/input/config errors |

Dependency policy: dependencies are welcome for mature commodity
functionality; each meaningful one is evaluated for maintenance, security
history, unsafe usage, transitive weight, cross-platform behavior, and
license. Critical application semantics (trust decisions, clipboard
transaction logic, control transfer) remain in Crossover code.

## 8. Configuration (initial shape)

```toml
schema_version = 1

[device]
name = "workstation-left"

[network]
listen = "0.0.0.0:<port>"        # default port fixed by ADR

[peer.workstation-right]
address = "192.168.1.25:<port>"

[layout]
right = "workstation-right"
```

Validated on load with actionable errors; deterministic defaults; no private
keys in this file (they live in `SecureStorage`).
