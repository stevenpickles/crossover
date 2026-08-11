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
disconnect while keys are held down, rapid clipboard replacement bursts,
and a **saturating background transfer** — every send queue full and the
socket stalled behind a peer that will not read — with live input injected
into the middle of it (ADR 0013; `tools/test-peer/tests/priority.rs`).

Chunked transfers (ADR 0014) add their own faults, because a transfer with
many messages has failure modes a single frame does not: a session **torn
mid-stream** (nothing partially installed, no buffer left pinned, and the
same item transferring cleanly on reconnect as a fresh transaction —
`tools/test-peer/tests/stress.rs`); a transfer **accepted and then
abandoned** by a silent peer, which must expire rather than pin its
reassembly buffer for the life of the session; and **malformed chunk
sequences** — gaps, repeats, wrong lengths, foreign item ids — each
fail-closed, counted once per doomed transfer, and fatal only on
repetition (PROTOCOL.md §7). A streaming image is also run through the
saturation case above, since a chunk being preemptable is the whole reason
ADR 0014 chunks at all.

That last one asserts *structurally*: arrival positions and frame counts,
never elapsed time. A wall-clock latency bound on a loaded CI runner
measures the runner, so the guarantee is stated as "everything still queued
arrives after the input frame, and nothing is dropped" rather than as a
number of milliseconds. Numeric latency belongs in §4's measurement, not in
a gate.

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

**Two exceptions, and they are honest ones.** Clipboard images (ADR 0014)
have a part no automation can reach: what a *third-party* application
publishes, and whether one accepts what Crossover installs. Automation
covers everything either side of that — a fabricated DIB round-trips
through Win32 verbatim, canonicalizes to a stable length, and is refused
above the ceiling — so these two are `#[ignore]`d and run deliberately:

```
cargo test -p crossover-platform-windows -- --ignored manual_a_real_snip
cargo test -p crossover-platform-windows -- --ignored manual_an_installed_image
```

| Test | What the human does |
|------|---------------------|
| `clipboard::tests::manual_a_real_snip_is_read_as_a_stable_image` | Take a snip (`Win+Shift+S`) **before** running; the test asserts the Snipping Tool's own DIB reads as an image, sits inside the ceiling, and yields identical bytes on consecutive reads |
| `clipboard::tests::manual_an_installed_image_pastes_into_other_applications` | Run it, then paste (`Ctrl+V`) into Paint, Word, and a browser compose box, and confirm the gradient appears in each |

Both are also on the two-machine list in [SOAK.md](SOAK.md), where the
interesting version is the same paste after the image crossed the wire.

Note that every test in this file that drives the real clipboard is
serialized behind one process-wide lock and tolerates `Busy`, because the
clipboard is a machine-global lock any application may hold. A failure
that reports `OpenClipboard ... Access is denied` on *every* such test —
including ones this change did not touch — is the desktop, not the code;
reproduce it on a clean checkout before debugging it.

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
