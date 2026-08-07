# 0001. Wire serialization format: postcard

Status: Accepted
Date: 2026-08-07

## Context

[PROTOCOL.md](../PROTOCOL.md) requires a deterministic, strongly typed
binary format for message payloads, with explicit size limits, support for
protocol evolution, fuzzability, and straightforward validation. The
protocol is explicitly versioned with negotiation (§3), and the frame
header — not the payload encoding — carries `message_type` and length, so
unknown *messages* are skippable at the framing layer without parsing their
payloads.

Security shapes the choice: the payload parser is a large part of the
network-facing attack surface (threat T4), so a small grammar and a small
dependency tree are worth more than wire-format flexibility the design
does not need.

## Decision

Payloads are encoded with **postcard** (serde-based, non-self-describing,
deterministic). Compatibility is handled entirely by explicit protocol
versioning: payload schemas are frozen per protocol version, and any schema
change — however small — requires a version bump negotiated in `Hello`.

Rules:

- Message types live in `crossover-protocol`, with bounds on every
  variable-length field enforced explicitly, not left to the decoder.
- serde/postcard remain an implementation detail of `crossover-protocol`;
  serialization traits do not leak across crate boundaries (the
  specification's warning against serde-shaped architecture).
- Golden-file wire-snapshot tests exist per message per protocol version,
  so accidental schema drift fails at test time, not in the field.

## Alternatives Considered

- **CBOR (ciborium)**: self-describing, IETF-standardized; tolerates
  unknown fields within a version. Rejected: schemas are deliberately
  frozen per version for deterministic compatibility, so self-description
  buys little while costing a larger parser grammar — more T4 attack
  surface and a bigger fuzzing space.
- **MessagePack (rmp-serde)**: same trade as CBOR with weaker
  deterministic-encoding guarantees. Rejected for the same reasons.
- **Protocol Buffers (prost)**: best-in-class field-tag evolution and
  cross-language reach. Rejected: codegen and a `.proto` source of truth
  add build complexity; silent unknown-field tolerance encourages implicit
  compatibility, contradicting the explicit-negotiation model; and
  cross-language interop is not a requirement — both ends are Crossover.

## Consequences

- Easier: tiny dependency surface (postcard + serde); deterministic bytes
  (supports content hashing and snapshot tests); simple fuzz targets.
- Harder: zero in-place schema flexibility — every payload change is a
  protocol version bump with negotiation and compatibility tests. Accepted
  deliberately: compatibility stays explicit and testable rather than
  emergent.
- If a future need for cross-language peers or in-version field evolution
  emerges, this decision is revisited by a superseding ADR; the framing
  layer is format-agnostic, which contains the blast radius of a change.
