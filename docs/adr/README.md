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
| — | *(none recorded yet)* | — |

## Known decisions awaiting an ADR

These are deliberately **deferred** by the specification suite. Each must be
resolved by ADR before the phase that depends on it (see
[docs/ROADMAP.md](../ROADMAP.md)):

1. **Wire serialization format** — postcard vs. CBOR vs. MessagePack vs.
   Protocol Buffers. Required before Phase 1. See
   [docs/PROTOCOL.md](../PROTOCOL.md) for selection criteria.
2. **Pairing verification mechanism** — short authentication string (SAS)
   comparison vs. a PAKE (e.g., SPAKE2) with a short shared code. Required
   before Phase 1. See [docs/SECURITY.md](../SECURITY.md).
3. **Device identity credential form** — raw public key vs. self-signed
   certificate, and how it binds into TLS 1.3 mutual auth. Required before
   Phase 1.
4. **Clipboard transaction message flow** — 2-message (Data/Applied) for small
   payloads with Offer/Accept reserved for large payloads, vs. uniform
   4-message flow. Required before Phase 2. See
   [docs/PROTOCOL.md](../PROTOCOL.md).
5. **Windows input capture approach** — low-level hooks vs. Raw Input, and the
   injection strategy. Required before Phase 3. See risk notes in
   [docs/SPECIFICATION.md](../SPECIFICATION.md).
6. **Default TCP port** — chosen and documented centrally. Required before
   Phase 1.
