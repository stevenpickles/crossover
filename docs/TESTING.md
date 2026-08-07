# Crossover Testing Strategy

Testing is an architectural requirement, not an afterthought. A feature that
worked once manually is not done. The architecture exists partly *so that*
the hard parts — protocol, state machines, clipboard semantics — run in
tests without hardware, sockets, or a second computer.

---

## 1. Test layers

| Layer | Runs | Scope |
|-------|------|-------|
| Unit | every CI run, all 3 OSes | pure logic in every crate |
| Property-based | every CI run | invariants over generated inputs (proptest) |
| Fuzz | smoke in CI, extended runs scheduled | network-facing parsers |
| Integration (test-peer) | every CI run | real networking + TLS against a scripted peer, single machine |
| Fault injection | every CI run | integration scenarios with induced failures |
| Platform (Windows) | Windows CI + real hardware | real Win32 clipboard/input/display behavior |
| Stress / long-duration | phase gates (see [ROADMAP.md](ROADMAP.md)) | reliability exit criteria |

### 1.1 Unit tests

Strong coverage is required for: protocol framing and validation, version
negotiation, clipboard dedup/ordering/conflict/loop-prevention, retry state
machines, trust-store validation and revocation, input state tracking,
`ReleaseAllInput`, control transfer, topology mapping, configuration
validation.

Core state machines are pure `(state, event) -> (state, commands)` functions
([ARCHITECTURE.md](ARCHITECTURE.md) §5), so these tests are deterministic —
no sleeps, no real time, no I/O.

### 1.2 Property-based tests

Where generated inputs meaningfully exceed hand-written cases:
serialization round-trips, framing (arbitrary split/coalesce of byte
streams must decode identically), clipboard ordering and dedup under
arbitrary interleavings, input state transitions (any event sequence +
disconnect ⇒ `ReleaseAllInput` leaves nothing pressed), control-transfer
state machine (never two owners, never zero owners after convergence).

### 1.3 Fuzzing

Every network-facing parse path in `crossover-protocol` has a fuzz target
(cargo-fuzz, in `fuzz/`). Goals: arbitrary bytes never panic, never
allocate unboundedly, never corrupt state; invalid sequences fail safely
into the fail-closed path ([PROTOCOL.md](PROTOCOL.md) §7). Decode targets
also assert re-decode equality: anything that decodes must survive an
encode → decode round trip unchanged. CI runs a short smoke of every
target on each change (`fuzz-smoke` job); longer runs are scheduled
later.

### 1.4 Integration via the headless test peer

`tools/test-peer` is a scriptable Crossover peer for CI on a single
machine. It can: connect, authenticate (with valid or invalid credentials),
negotiate (any version range), run clipboard transactions (offer/send/ack,
delayed ack, missing ack, duplicate items), disconnect/reconnect, and send
arbitrary malformed or out-of-order messages.

Integration tests drive the real application core + real TLS + real TCP
against the test peer, with platform traits backed by in-memory fakes.

### 1.5 Fault injection

Deliberately induced, asserted-on scenarios: dropped connections and
reconnects, delayed/duplicate/stale messages, clipboard lock contention
(fake `ClipboardProvider` returns transient failures), missing
acknowledgements, peer crash mid-transaction, abrupt TLS termination,
disconnect while keys are held down, rapid clipboard replacement bursts.

Fault injection is the primary evidence for the reliability requirements
(FR-6.x) and clipboard guarantees (FR-3.x).

### 1.6 Platform tests (Windows)

Separated from cross-platform logic tests; exercise real Win32 behavior:
clipboard observation/update/contention, sequence-number change detection,
input injection (including UIPI-blocked targets — detectably failing),
monitor enumeration and DPI mapping, cursor positioning, hook installation
and hook-loss recovery, shutdown cleanup.

Some of these need real interactive sessions and run on dedicated Windows
runners or manually per release, not in every headless CI job — but they
exist as automated tests, not checklists.

## 2. CI

GitHub Actions builds and tests on **Windows, Linux, and macOS from
Phase 0**, before non-Windows platform code exists — this keeps
platform-independent crates honest (NFR-4).

Required checks:

```
cargo fmt --check
cargo clippy --workspace --all-targets   (warnings denied)
cargo build --workspace
cargo test --workspace
```

The `dependencies` job enforces the dependency policy with `cargo-deny`
(`deny.toml`): RustSec advisories, license compatibility against an
explicit permissive allow-list, and crates.io-only provenance. Unlike
coverage it **gates**, because a vulnerable or unmaintained dependency
is a defect with a known fix. It also runs on a daily schedule, since a
new advisory can land against unchanged code. Locally: `cargo deny
check`.

CodeQL static analysis runs in its own workflow (`.github/workflows/
codeql.yml`) over Rust, the workflow files, and the soak script. It uses
advanced setup because GitHub's default setup does not offer Rust. Its
expected yield is modest by design — safe Rust prevents most of what
CodeQL hunts for — so it targets where the safety argument is weakest:
the Win32 FFI, whose correctness rests on hand-written SAFETY comments,
and the workflows, where script injection is a real supply-chain class.
It is kept out of `ci.yml` so an analyzer problem cannot redden the
merge gate.

GitHub's own services cover what a build-time check cannot. Dependabot
alerts and security updates watch the GitHub Advisory Database and open
fix PRs when an advisory lands against unchanged code; `cargo-deny`
blocks the merge, Dependabot proposes the upgrade. Version updates
(`.github/dependabot.yml`) additionally keep SHA-pinned actions current,
which is otherwise invisible until a runner deprecation warning appears
in a log. Secret scanning with push protection is enabled on the
repository — relevant here because Crossover handles key material, and a
committed private key is unrecoverable once pushed.

The `fuzz-smoke` job runs every fuzz target briefly on each change.

The `coverage` job measures line/region/branch coverage with
`cargo-llvm-cov` on Windows (the only leg where the real-OS platform code
runs), publishes the browsable HTML as a build artifact, and prints the
per-file summary to the job page. Locally:

```
cargo llvm-cov --workspace --html      # target/llvm-cov/html/index.html
cargo llvm-cov report --summary-only   # per-file table
```

Coverage is **reported, not gated**. A percentage threshold rewards tests
written to move a number; the useful question is which paths a change
leaves unexercised, which the report answers directly. Low branch
coverage in error-handling code is a standing invitation to add
fault-injection cases (docs/TESTING.md §1.5), not a build failure.
Added as the project matures: documentation build, MSRV check, protocol
compatibility tests across supported versions.

## 3. Phase-gate testing

Exit criteria in [ROADMAP.md](ROADMAP.md) are verified by automation where
stated — notably Phase 2's clipboard stress gate: **≥10,000 automated
bidirectional clipboard updates with zero corruption, zero sync loops, zero
silent failures, zero crashes**, and a diagnostic for every ultimately
failed update.

## 3.1 Two-machine soak

The stress gate is hermetic by design. Real clipboards, a real network,
and two independent machines are covered by the manual soak in
[SOAK.md](SOAK.md), whose output is a report to interpret rather than a
build verdict — a live desktop can always interfere, and a red build
that says nothing about Crossover is worse than no build at all.
`tools/soak-report.py` summarizes the structured logs from both sides.

## 4. Performance measurement

Numeric latency targets are established by measurement (NFR-5).
Instrumentation (tracing spans + local counters) measures: network RTT,
message latency by class, clipboard transaction latency, input queue depth,
reconnect duration, control-transfer latency. Measurements are local only
(FR-7.5).

## 5. Definition of Done

A feature is complete only when every applicable item holds:

- requirements it implements are identified (FR/NFR numbers) and documented
- implementation complete; errors handled; security implications considered
- diagnostics exist where §FR-7.3 demands them; no sensitive data logged
- unit + property tests pass; integration and fault-injection tests updated
  and passing; platform tests where applicable
- CI green on all three OSes
- documentation current; ADRs written where required
- no unrelated architectural changes smuggled in
