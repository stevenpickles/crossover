# 0004. Default TCP port: 27677

Status: Accepted
Date: 2026-08-07

## Context

[PROTOCOL.md](../PROTOCOL.md) §8 requires a default TCP port chosen once
and documented centrally. Constraints: avoid well-known service
collisions, remain configurable (FR-7.2's deterministic defaults still
allow overrides), and avoid colliding with the tools Crossover resembles —
Synergy, Barrier, and Deskflow all listen on 24800, and users are likely
to run one of them side by side with Crossover during migration.

## Decision

The default port is **27677** — "CROSS" spelled on a telephone keypad,
which makes it easy to remember and obviously Crossover's own.

- It has no IANA assignment and no notable de-facto usage.
- It is deliberately distinct from 24800 so a Synergy-family listener and
  Crossover can coexist on one machine without either tool connecting to
  the other's port and logging confusing handshake failures.
- The value is defined exactly once in code as a named constant (with the
  other protocol constants in `crossover-protocol`) and referenced
  everywhere else — configuration defaults, documentation, and diagnostics
  all cite that constant or this ADR.

## Alternatives Considered

- **24800 (the Synergy-family port)**: familiar to migrating users.
  Rejected: guarantees collision and mutual confusion when both tools are
  present, and squatting another project's conventional port invites
  misdirected bug reports in both directions.
- **A randomized per-install port**: sidesteps collisions entirely.
  Rejected as the default: contradicts deterministic defaults (FR-7.2) and
  complicates the two-machine setup story; users who want it can configure
  any port.

## Consequences

- Easier: out-of-the-box config is deterministic; documentation and error
  messages can name one number; clean coexistence with Synergy-family
  tools.
- Risk accepted: an unregistered port can always collide with some obscure
  third-party software; mitigation is the existing `[network] listen`
  configuration.
- Nearby 27015–27050 is Steam-server territory; 27677 sits outside that
  block, and the neighborhood overlap is cosmetic only.
