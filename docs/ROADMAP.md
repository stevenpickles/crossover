# Crossover Roadmap

> **Current phase: 0 — Repository Foundation** (not started)
>
> Update this marker when a phase's exit criteria are verified. Do not begin
> a later phase because time remains — complete and validate exit criteria
> first. Later-phase functionality is implemented early only when required
> to keep the architecture clean.

Phases build the three hardest foundations first — security, networking,
reliable clipboard sync — before any input forwarding. The first meaningful
user-facing milestone is the **Secure Clipboard Prototype** (end of
Phase 2), not remote input.

---

## Phase 0 — Repository Foundation

**Goal:** a professional Rust workspace that builds and tests on all three
desktop OSes, with the documentation and decision framework in place.

Deliverables:

- Cargo workspace with the initial crates from
  [ARCHITECTURE.md](ARCHITECTURE.md) §3 (skeletons with placeholder tests)
- `rust-toolchain.toml` (pinned toolchain + MSRV), rustfmt config, clippy
  policy (warnings denied)
- GitHub Actions: fmt/clippy/build/test on Windows, Linux, macOS
- structured logging foundation (tracing) wired into the app skeleton
- error-handling conventions established (thiserror/app-boundary pattern)
- ADRs recorded for any deferred decisions that Phase 1 needs
  ([adr/README.md](adr/README.md))
- README kept current

Exit criteria:

- `cargo build/test/fmt --check/clippy` pass on all three OSes in CI
- No clipboard/input/network functionality exists yet — that is correct

## Phase 1 — Secure Peer Connection

**Goal:** two Windows computers establish and retain a secure trusted
relationship.

Deliverables: TCP listener/connector; TLS 1.3 with mutual authentication;
persistent device identity in `SecureStorage`; pairing ceremony
([SECURITY.md](SECURITY.md) §3); trusted peer store with revocation; Hello
exchange and version negotiation ([PROTOCOL.md](PROTOCOL.md) §3); automatic
reconnect with bounded backoff; `crossover pair/peers/status`; structured
connection logging; test-peer tool covering connect/auth/negotiate paths.

Prerequisite ADRs: serialization format, pairing mechanism, identity
credential form, default port (deferred decisions 1, 2, 3, 6).

Exit criteria — two fresh Windows machines can:

1. explicitly pair, 2. establish an encrypted connection, 3. restart
Crossover and reconnect **without re-pairing**, 4. reject a third untrusted
machine.

Plus: packet capture reveals no plaintext application payload (automated
where practical); every authentication failure mode produces an actionable
diagnostic; fuzz targets exist for all Phase-1 parse paths.

## Phase 2 — Reliable Text Clipboard

**Goal:** extremely reliable bidirectional UTF-8 text clipboard sync —
before any input forwarding.

Deliverables: Windows clipboard monitoring behind `ClipboardProvider`
(sequence-number change detection, contention handling — risks R-4/R-5);
clipboard engine with ids, hashing, acknowledgement, loop prevention,
bounded retry, latest-wins conflict policy; transaction flow from
[PROTOCOL.md](PROTOCOL.md) §5 (prerequisite: ADR for deferred decision 4);
reconnect-safe behavior; metadata-only transaction logging; clipboard
integration + fault-injection suites.

Exit criteria:

- Automated stress: **≥10,000 bidirectional clipboard updates** with zero
  content corruption, zero sync loops, zero unexplained ordering failures,
  zero silent failures, zero crashes ([TESTING.md](TESTING.md) §3)
- Every ultimately-failed update produces an observable diagnostic
- **Milestone: Secure Clipboard Prototype** — pair, trust persistence,
  encrypted transport, reliable bidirectional clipboard, auto-reconnect

## Phase 3 — Remote Mouse

**Goal:** control the second computer's pointer.

Deliverables: mouse capture and injection behind platform traits
(prerequisite ADR: Windows input capture approach, deferred decision 5;
respect hook-timeout budget R-2); pointer transport with coalescing;
buttons and wheel; coordinate abstraction (DPI-aware, R-3); button state
tracking with `ReleaseAllInput`; **explicit** control switching via CLI
command or hotkey — no edge detection yet.

Exit criteria: second machine controllable with first machine's mouse;
repeated activate/deactivate cycles never leave stuck buttons (fault-
injection verified); pointer response subjectively suitable for desktop use
on LAN; injection failures against elevated windows are detected and
diagnosed (R-1).

## Phase 4 — Remote Keyboard

**Goal:** forward keyboard input safely.

Deliverables: keyboard capture/injection; key representation per FR-4.1;
modifier handling; ordered key transitions; key-state tracking;
`ReleaseAllInput` on every disconnect path.

Exit criteria: normal typing and common shortcuts work remotely; repeated
activation/deactivation correct; disconnect at **arbitrary** moments
(fault-injected) never leaves keys or modifiers logically pressed.

## Phase 5 — Seamless Crossover

**Goal:** two computers behave like neighboring monitors.

Deliverables: monitor enumeration; topology configuration; edge detection;
negotiated control-transfer state machine ([ARCHITECTURE.md](ARCHITECTURE.md)
§5.1); cursor mapping across DPI differences; automatic keyboard ownership
transfer; automatic return; transition diagnostics.

Exit criteria: with `A | B`, the user repeatedly moves A → B → A through
screen edges with no manual switching; keyboard follows the active machine;
clipboard stays synchronized throughout; transfer under induced packet
delay/loss converges to exactly one owner.

## Phase 6 — Windows Prototype Hardening

**Goal:** suitable for continuous daily use.

Deliverables: hardened reconnect; startup configuration; background
operation and lifecycle management; graceful shutdown and crash recovery;
performance instrumentation ([TESTING.md](TESTING.md) §4); installer or
package; configuration validation polish; **dedicated security review**
against [SECURITY.md](SECURITY.md) §6-§7; long-duration soak testing.

Exit criteria: multi-day continuous operation between two Windows
workstations without manual intervention; transient network loss and peer
restarts recover automatically; clipboard reliability requirements still
hold under soak; security review findings resolved or accepted by ADR.

## Phase 7 — Cross-Platform Validation

**Goal:** prove the architecture is genuinely portable.

Deliverables: `crossover-platform-macos` and `crossover-platform-linux`
(created now, not before — [ARCHITECTURE.md](ARCHITECTURE.md) §3.1), with a
risk catalogue per platform written before implementation (macOS:
accessibility permissions, event taps, pasteboard, Keychain; Linux:
X11/Wayland split, clipboard ownership semantics, injection permissions,
secret-service).

Exit criteria: core feature set works Windows↔Windows, Windows↔macOS,
Windows↔Linux, macOS↔macOS, Linux↔Linux — and macOS↔Linux requires no
protocol changes.

## Phase 8 — Productization

Potential work, each item gated on preserving the security and clipboard
reliability requirements: tray application, graphical configuration and
topology editor, peer discovery, >2 peers, rich clipboard formats (HTML,
images, files), drag-and-drop, software updates, diagnostics UI, optional
secure WAN operation.

---

## Working practices

- Decompose phases into tasks small enough to understand, test, review, and
  revert independently ("Define the ClipboardItem type with unit tests",
  not "Implement Phase 2").
- Keep the repository buildable after each integrated change.
- Record deviations from the specification suite deliberately — update the
  docs, don't diverge silently.
