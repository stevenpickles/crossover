# Crossover Specification

Status: Authoritative product specification
License: MIT
Primary implementation language: Rust
Initial target: Windows ↔ Windows on a trusted LAN
Long-term targets: Windows, macOS, Linux

This document defines **what Crossover is and what it must do**. Companion
documents define how:

| Document | Contents |
|----------|----------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crate layout, platform abstraction, state machines, technology choices |
| [PROTOCOL.md](PROTOCOL.md) | Wire protocol: framing, versioning, messages |
| [SECURITY.md](SECURITY.md) | Threat model, trust model, pairing, key storage |
| [TESTING.md](TESTING.md) | Testing strategy and requirements |
| [ROADMAP.md](ROADMAP.md) | Development phases and exit criteria |
| [adr/](adr/README.md) | Architectural decision records |

When implementation reveals a better design, update these documents
deliberately — code and specification must not diverge silently.

---

## 1. Purpose

Crossover is an open-source application for securely sharing one keyboard,
mouse, and clipboard between computers connected over an IP network.

The product statement:

> Two trusted computers beside one another should feel like one larger
> workstation, without weakening the security boundary between them.
>
> **Move. Type. Copy. Paste.**

The interaction model resembles multiple monitors attached to one computer:

- Each computer runs its own operating system and applications.
- Moving the pointer across a configured screen edge transfers control to the
  adjacent computer.
- Keyboard input follows the currently controlled computer.
- Clipboard contents synchronize between trusted computers.

Crossover is inspired by Synergy-family tools, but treats **clipboard
reliability** and **peer security** as first-class engineering problems rather
than conveniences layered onto input forwarding.

## 2. Priorities

In strict order. When requirements conflict, the higher priority wins.

1. Security
2. Clipboard reliability
3. Correct input behavior
4. Robust recovery from failures
5. Low input latency
6. Testability
7. Cross-platform portability
8. Ease of configuration
9. User-interface polish

Two consequences worth stating explicitly:

- Correctness and security are never sacrificed for an *unmeasured* latency
  concern (measure first — see §7.5).
- Clipboard synchronization is a first-class subsystem with its own state
  machine, acknowledgements, and test suite — not a side effect of input
  forwarding.

## 3. Scope

### 3.1 Initial scope

The first useful implementation supports exactly:

- two Windows computers on the same LAN
- TCP transport with TLS 1.3 encryption
- persistent cryptographic device identity
- explicit pairing with mutual peer authentication and persistent trust
- automatic reconnection
- bidirectional UTF-8 **text** clipboard synchronization with acknowledgement
- keyboard and mouse forwarding
- seamless screen-edge control transfer
- structured logging and command-line operation (foreground/debug execution)

No GUI is required initially (the one exception arrives in Phase 8 — the
topology editor; see the §3.3 note).

### 3.2 Long-term scope

Eventually: macOS and Linux; more than two peers, including arbitrary
display topology across them; background/tray operation; peer discovery; rich
clipboard types (HTML, images, file lists); drag-and-drop; auto-update;
secure WAN operation; diagnostics UI.

> **Re-scoped:** clipboard **images** left this list on 2026-08-11 and are
> being built in Phase 7 ([ADR 0014](adr/0014-chunked-rich-clipboard-transfer.md));
> file lists are designed for a later, deliberately minimal spool-and-paste
> capability ([ADR 0015](adr/0015-spooled-virtual-file-paste.md)). ADR 0014's
> platform slice has since landed, so §3.1's "UTF-8 **text**" now understates
> what a build synchronizes: images travel too, in the source clipboard's own
> raster format, between peers that both advertise `CHUNKED_CLIPBOARD`
> (PROTOCOL.md §3.1). Text remains the only type every peer can take,
> because the capability is negotiated and a peer without the bit is sent
> nothing new.

> **Re-scoped (2026-08-20):** **multiple monitors per peer with arbitrary
> topology** left this list for the two-peer case, which is why the entry
> above now scopes it to "more than two peers". Phase 8 places both machines'
> monitors in one drawn coordinate space and derives crossing edges from
> adjacency, so a seam between two monitors of the same machine, an
> over/under arrangement, and a three-monitor corner are all expressible
> ([ADR 0018](adr/0018-drawn-display-topology.md)). The same capability
> across *more* than two peers stays long-term: the layout model admits it,
> and this phase does not build it.

Long-term capabilities must not complicate the initial implementation except
where required to preserve a clean architecture (chiefly: the protocol and
platform abstractions must not hard-code two-peer, one-display, Windows-only
assumptions).

### 3.3 Non-goals (initial development)

Graphical configuration, tray application, service installation, peer
discovery, >2 computers, macOS/Linux implementations, non-text clipboard,
drag-and-drop, screen streaming or remote video/audio, NAT traversal, cloud
services, user accounts, centralized authentication, mobile platforms.

> **No longer a non-goal (2026-08-20):** graphical configuration, in one
> specific form. Phase 8 ships a **topology editor** — the project's first
> GUI — because an arrangement the user draws is the deliverable, and a
> drawn arrangement has no command-line form worth having. It is a separate
> user-session surface rather than a mode of the headless, service-launched
> worker ([ADR 0011](adr/0011-background-service-launcher.md),
> [ADR 0018](adr/0018-drawn-display-topology.md); the UI toolkit decision
> follows as ADR 0019). Graphical configuration of everything *else* remains
> out of initial scope and stays in Phase 10.

## 4. Functional requirements

Requirements are numbered for traceability (`FR-x.y`). "Must" requirements
are binding; "should" requirements are strong defaults changeable by ADR.

### 4.1 Identity and pairing

- **FR-1.1** Each installation must generate a persistent cryptographic device
  identity (keypair + device UUID + human-readable name) that survives
  restarts. Private key material must not leave the machine in normal
  operation.
- **FR-1.2** Trust between two computers must be established only through an
  explicit pairing ceremony requiring deliberate user action on **both**
  machines, with human verification that defeats an active
  man-in-the-middle during first contact (see [SECURITY.md](SECURITY.md)).
- **FR-1.3** Once paired, trust persists until deliberately removed. Paired
  peers reconnect and authenticate automatically without re-pairing.
- **FR-1.4** Removing a peer must revoke its authorization; subsequent
  connection attempts from that identity are rejected.

### 4.2 Connection security

- **FR-2.1** All peer communication must use authenticated encryption
  (TLS 1.3) with **mutual** authentication. Encryption without peer
  authentication is insufficient.
- **FR-2.2** Knowledge of a peer's IP address, hostname, and port must not be
  sufficient to control it. An untrusted machine must not be able to inject
  input, read or write the clipboard, or establish a session.
- **FR-2.3** Authentication failures, malformed messages, unexpected state
  transitions, and validation failures must fail **closed**: the affected
  session is rejected or terminated. Crossover never continues in an
  uncertain security state.
- **FR-2.4** A packet capture of a session must reveal no application payload
  in plaintext.

### 4.3 Clipboard synchronization

- **FR-3.1** Clipboard synchronization must observe the actual operating
  system clipboard. It must not depend on intercepting keyboard shortcuts.
- **FR-3.1a** Crossover must be a good neighbour on the clipboard: the OS
  clipboard is a machine-global lock, so Crossover must not hold it
  longer than an operation needs, nor take it more often than the user
  can benefit from. Transmission is therefore trigger-driven
  (ADR 0006), and intermediate states no user can paste are not written
  at all.
- **FR-3.2** A synchronization operation succeeds only when the destination
  operating-system clipboard has been updated — not when bytes were written
  to a socket. Success must be acknowledged end to end.
  **Files read this one step earlier, and deliberately**
  ([ADR 0015](adr/0015-spooled-virtual-file-paste.md)): a file item
  succeeds (`ApplyResult::Stored`) when the receiver has verified it and
  registered it in its spool, because what reaches the destination
  clipboard is a *promise* of the bytes rather than the bytes, and what
  follows — whether the user ever pastes, and where — is the user's
  gesture, not a synchronization outcome. The requirement's substance is
  unchanged: the acknowledgement still states a fact about the
  destination, and still never about a socket write.
- **FR-3.3** Synchronization must not loop: a peer applying a remote clipboard
  item must recognize the resulting local clipboard change as that item and
  must not re-send it.
- **FR-3.4** Transient clipboard access failures (e.g., another application
  holding the Windows clipboard) must be retried with bounded attempts and
  bounded time. Ultimate failure must be observable in diagnostics.
- **FR-3.5** Near-simultaneous clipboard changes on both machines must resolve
  deterministically and the policy must be documented. (Initial policy:
  latest observed item wins; see [PROTOCOL.md](PROTOCOL.md).)
- **FR-3.6** All clipboard payloads are bounded. Oversized contents are
  rejected gracefully on both send and receive.
- **FR-3.7** The initial supported type is UTF-8 text. The data model and
  protocol must accommodate future typed payloads without a rewrite.
  *(Discharged as designed: raster images were added in Phase 7
  ([ADR 0014](adr/0014-chunked-rich-clipboard-transfer.md)) as a new content
  type on the existing model — negotiated, chunked, and carried verbatim —
  with no protocol version bump.)*

### 4.4 Input forwarding

- **FR-4.1** Input events use platform-neutral representations distinguishing
  physical key identity from OS key representation from produced text. The
  protocol must not permanently require identical keyboard layouts or
  Windows keycodes.
- **FR-4.2** Keyboard state transitions must be delivered in order. Pointer
  movement may be coalesced; newer positions supersede older ones.
- **FR-4.3** Each peer must track which keys and buttons it believes are
  pressed on the remote destination.
- **FR-4.4** `ReleaseAllInput`: on disconnect, session termination, fatal
  protocol failure, control reset, or (where practical) abnormal shutdown,
  Crossover must synthesize release events for everything it believes is
  pressed. A stuck modifier or button after any disconnect is a
  release-blocking defect.

### 4.5 Control transfer

- **FR-5.1** Exactly one computer is the active input destination at any
  time. Control ownership is explicit state, never inferred solely from
  pointer coordinates.
- **FR-5.2** Moving the pointer across a configured screen edge transfers
  control to the adjacent computer, mapping the pointer to the corresponding
  position on the destination edge. The reverse edge returns control.
- **FR-5.3** Control transfer is a negotiated state-machine transition
  (request → acknowledge → switch) so both peers agree on ownership even
  under packet delay or loss.
- **FR-5.4** Clipboard synchronization operates regardless of which computer
  currently has input control.

### 4.6 Resilience

- **FR-6.1** Crossover must tolerate and recover from: temporary network
  loss, peer restart, application restart, TLS reconnection, clipboard
  contention, input bursts, disconnect while controlling a remote machine,
  malformed/duplicate/stale messages, and peer crashes — without requiring a
  reboot of either machine.
- **FR-6.2** Reconnection to trusted peers is automatic with bounded backoff.

### 4.7 Operation and diagnostics

- **FR-7.1** The CLI must support at minimum: running in the foreground,
  pairing, listing/removing trusted peers, and reporting connection status.
  Errors for configuration, networking, pairing, TLS, clipboard, and input
  failures must be actionable.
- **FR-7.2** Configuration is file-based (TOML initially) plus command-line
  flags, with schema versioning, validation, and deterministic defaults.
  Private keys are never stored in the ordinary configuration file.
- **FR-7.3** Structured logging exists from the first commit. Diagnostics
  must be sufficient to answer: why a peer failed to connect, why pairing
  failed, why a clipboard sync failed, why control transfer failed, whether
  a peer is trusted, and whether the secure session is active.
- **FR-7.4** Clipboard **contents** and private keys never appear in logs.
  Clipboard transactions are logged by metadata only (id, type, byte count,
  content hash, origin, attempts, result, latency).
- **FR-7.5** All telemetry is local. Nothing leaves the user's machines.

## 5. Non-functional requirements

- **NFR-1 (Bounded resources)** Every buffer, queue, retry loop, and
  allocation influenced by network input is bounded. Frame and field sizes
  are validated before allocation. Malformed input never causes a panic or
  unbounded allocation.
- **NFR-2 (Determinism)** Compatibility behavior, conflict resolution, and
  state machines are deterministic and specified before implementation.
- **NFR-3 (Observability)** Silent failure of an important state transition
  is a defect. Every failed clipboard sync, pairing attempt, and control
  transfer produces a diagnostic.
- **NFR-4 (Portability)** Platform-specific behavior lives only behind the
  platform abstraction layer ([ARCHITECTURE.md](ARCHITECTURE.md)). Protocol,
  state machines, and core logic compile and test on all three desktop
  platforms from Phase 0 onward.
- **NFR-5 (Performance qualities)** On a local LAN: pointer movement feels
  responsive, typing does not feel delayed, and copy-then-immediately-paste
  on the other machine normally succeeds. Exact numeric targets are
  established by measurement, not assumption (see [TESTING.md](TESTING.md)).
- **NFR-6 (Memory safety)** Safe Rust by default. `unsafe` is minimized,
  isolated in platform crates, documented, justified, and tested.

## 6. Known platform risks (Windows)

These Windows realities are called out here because they constrain design and
must be addressed (or explicitly deferred with rationale) during the phases
that touch them:

- **R-1 UIPI / elevation.** A non-elevated process cannot inject input into
  elevated windows, and cannot interact with the secure desktop (UAC
  prompts, lock screen, Ctrl+Alt+Del). Decide and document the supported
  behavior: run elevated, degrade gracefully, or both. Injection silently
  failing against an elevated window must at least be detectable.
- **R-2 Low-level hook timeout.** Windows silently removes low-level
  keyboard/mouse hooks whose callbacks exceed the system timeout. Hook
  callbacks must do near-zero work (enqueue and return); the capture design
  must detect and recover from hook loss.
- **R-3 Per-monitor DPI.** Pointer coordinates, edge detection, and
  cross-machine position mapping must account for per-monitor DPI scaling
  and virtual-desktop coordinate space. The process must be per-monitor DPI
  aware, or coordinates will be wrong on mixed-DPI systems.
- **R-4 Clipboard ecosystem interactions.** Windows clipboard history
  (Win+V), cloud clipboard sync, and clipboard-monitoring utilities interact
  with programmatic clipboard writes. Sequence-number-based change detection
  (`GetClipboardSequenceNumber`) and ownership tracking must be robust to
  other listeners, and Crossover must not fight them in a loop.
- **R-5 Clipboard contention.** `OpenClipboard` fails while another process
  holds the clipboard. This is routine, not exceptional — hence FR-3.4's
  bounded retry requirement.
- **R-6 Input capture vs. exclusive-input applications.** Games and remote
  desktop sessions using raw input or exclusive capture may bypass or
  conflict with hooks. Initial scope may exclude these; the limitation must
  be documented.

macOS and Linux risks are now catalogued, ahead of the ports as Phase 9
requires: [platform-risks-macos.md](platform-risks-macos.md) and
[platform-risks-linux.md](platform-risks-linux.md). They are written from
documented platform behaviour rather than measurement, so each risk carries
what to verify; anything that survives contact with hardware belongs here in
§6 alongside the Windows risks, and anything falsified should be struck with
a note saying so.

## 7. Process requirements

- **P-1** Development follows the phases and exit criteria in
  [ROADMAP.md](ROADMAP.md). Later-phase functionality is not implemented
  early except to preserve architectural cleanliness.
- **P-2** Architecturally significant decisions are recorded as ADRs
  ([adr/README.md](adr/README.md)). Security requirements are never weakened
  silently; validation is never removed to make tests pass.
- **P-3** A feature is complete only per the Definition of Done in
  [TESTING.md](TESTING.md). Working once manually is not done.
- **P-4** The project is one Git repository containing one Cargo workspace.
