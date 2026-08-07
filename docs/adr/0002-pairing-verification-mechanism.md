# 0002. Pairing verification: PAKE (SPAKE2) with a typed short code

Status: Accepted
Date: 2026-08-07

## Context

FR-1.2 requires the pairing ceremony to defeat an active man-in-the-middle
during first contact (threat T2). [SECURITY.md](../SECURITY.md) §3 deferred
the human-verification mechanism: short authentication string (SAS)
comparison vs. a PAKE over a short code.

The security of any ceremony rests on what the human must actually do. A
mechanism that lets an inattentive user approve a MITM is a design defect,
not a user error — security is priority #1 and the ceremony happens exactly
once per peer, so friction is cheap here.

## Decision

Pairing uses a **PAKE — SPAKE2 — keyed by a short one-time code that must
be typed**, not merely compared:

1. `crossover pair --listen` generates and displays a short one-time code.
2. The user types that code into `crossover pair <address>` on the other
   machine.
3. Both sides run SPAKE2 over the as-yet-untrusted connection with the code
   as the low-entropy input, deriving a strong shared key only if both used
   the same code.
4. Each side sends its identity public key, authenticated by a MAC (keyed
   by the SPAKE2 output) over the pairing transcript including both
   identities; on mutual verification, each side persists the other into
   its trust store.
5. Any mismatch or failure aborts the entire ceremony (fail closed). Codes
   are single-use and expire after a short window.

An active MITM gets exactly one online guess at the code per ceremony;
failure is loud and terminal, never retried silently.

## Alternatives Considered

- **SAS comparison** (both machines display a fingerprint; the user
  confirms they match): simpler and dependency-free, and its cryptography
  is sound. Rejected as primary because its security collapses exactly when
  users behave as users do — confirming without carefully comparing. Blind
  confirmation is the expected lazy path, and under it an active MITM
  succeeds silently. Typing a code makes the verification act mandatory:
  the honest path and the secure path are the same path.
- **Out-of-band public-key exchange** (user manually transfers
  fingerprints/files): highest assurance, worst ceremony. Rejected as
  default; may be added later for high-assurance setups without protocol
  changes.

## Consequences

- Easier: MITM resistance no longer depends on user diligence; the
  ceremony's UX naturally enforces its security assumption.
- Harder: adds a cryptographic dependency (the `spake2` crate) that must
  pass the dependency policy ([ARCHITECTURE.md](../ARCHITECTURE.md) §7 —
  maintenance, security history, unsafe usage) during implementation. If it
  fails that evaluation, the fallback is SAS with forced interactive
  comparison, recorded in a superseding ADR.
- The pairing exchange needs its own small message set in
  `crossover-protocol` (pairing init/exchange/confirm), subject to the same
  framing bounds and fuzzing as all other messages.
- Code entropy, format, and expiry window are implementation-phase
  parameters; the requirement fixed here is single-use, short-lived, and
  one online guess per ceremony.
