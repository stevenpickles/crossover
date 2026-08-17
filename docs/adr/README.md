# Architectural Decision Records

Architecturally significant decisions are recorded here as numbered ADRs. A
decision is architecturally significant when it is expensive to reverse,
security-relevant, or constrains the wire protocol, trust model, or crate
boundaries.

ADRs are immutable once **Accepted**. To change a decision, write a new ADR
that supersedes the old one and update both statuses.

## When an ADR is required

- Selecting or replacing a core library (TLS stack, async runtime, serialization)
- Any change to the wire protocol architecture or compatibility rules
- Any change to the trust model, pairing semantics, or credential storage
- Creating or dissolving a workspace crate
- Any deliberate weakening or strengthening of a security requirement
- Introducing or widening a filesystem-write surface driven by network input
  (requires SECURITY.md threat-model additions alongside the ADR)
- Meaningful increases in `unsafe` Rust usage

## Format

Files are named `NNNN-short-title.md`, numbered sequentially.

```markdown
# NNNN. Title

Status: Proposed | Accepted | Superseded by NNNN
Date: YYYY-MM-DD

## Context

What problem forces this decision. Constraints that apply.

## Decision

The decision, stated in full sentences, in active voice.

## Alternatives Considered

Each alternative and the concrete reason it was not chosen.

## Consequences

What becomes easier, what becomes harder, what risks are accepted.
```

## Index

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-wire-serialization-format.md) | Wire serialization format: postcard | Accepted |
| [0002](0002-pairing-verification-mechanism.md) | Pairing verification: SPAKE2 with typed code | Accepted |
| [0003](0003-device-identity-credential.md) | Device identity: Ed25519 pinned by SPKI in self-signed cert | Accepted |
| [0004](0004-default-tcp-port.md) | Default TCP port: 27677 | Accepted |
| [0005](0005-clipboard-transaction-flow.md) | Clipboard transactions: 2-message inline, Offer/Accept above 64 KiB | Accepted |
| [0006](0006-clipboard-transmission-triggers.md) | Clipboard transmission is trigger-driven, not change-driven | Accepted |
| [0007](0007-windows-input-capture.md) | Windows input: hooks to suppress, Raw Input for motion, SendInput to inject | Accepted |
| [0008](0008-keyboard-key-representation.md) | Keyboard key representation: physical key by USB HID usage, text carried, inject by scan code | Accepted |
| [0009](0009-seamless-edge-transfer.md) | Seamless control transfer: edge crossing triggers the negotiated engine, position as a fraction | Accepted |
| [0010](0010-active-session-revocation.md) | Revocation terminates active sessions via a trust-store poll, not just new connections | Accepted |
| [0011](0011-background-service-launcher.md) | Background operation: a minimal LocalSystem service launches the worker into the user session, behind a `ServiceManager` boundary | Accepted (amended by 0012) |
| [0012](0012-elevated-worker-integrity.md) | Worker runs at high integrity, launched with the user's elevated linked token, so it can drive elevated windows | Accepted |
| [0013](0013-interactive-over-bulk-prioritization.md) | Interactive input takes wire priority over bulk transfers (Phase 7) | Accepted |
| [0014](0014-chunked-rich-clipboard-transfer.md) | Chunked rich-clipboard transfer: images first, native format verbatim (Phase 7) | Accepted |
| [0015](0015-spooled-virtual-file-paste.md) | Files/folders transfer: eager transfer to a bounded internal spool, pasted as a virtual file list, sender-zipped archives, per-peer permission (Phase 7) | Proposed |
| [0016](0016-image-interchange-format.md) | Image interchange: the receiver names the format, the sender produces it; PNG is the baseline and receivers never decode (Phase 8) | Proposed |

## Known decisions awaiting an ADR

None outstanding. The files/folders **filesystem-write surface** sketched in
0014 is now designed in [0015](0015-spooled-virtual-file-paste.md) (Proposed) —
eager verified transfer into a bounded internal spool, disposed of by pasting a
virtual file list, superseding the drop-folder model 0014 sketched and 0015's
own first draft carried — with the matching threat-model additions in
[SECURITY.md](../SECURITY.md) §7 (invariants F1–F15, threats T12–T21) — the
precondition 0014 set for any implementation.

The deferred *specification* decisions are all recorded — wire format (0001),
pairing mechanism (0002), identity credential (0003), default port (0004),
clipboard transaction flow (0005), Windows input capture (0007), and keyboard
key representation (0008); 0006 was raised by evidence rather than deferred.
ADRs 0013 and 0014 are **Accepted** — the Phase 7 rich-clipboard direction is
ratified after a drift-check against the current code. ADR 0015 is **Proposed**
— files/folders transfer stays gated on its ratification.

New entries belong here when a decision is identified but not yet made,
so that the gap is visible rather than implicit.
