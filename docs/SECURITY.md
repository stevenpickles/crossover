# Crossover Security Model

Threat model, trust model, and security design. Vulnerability reporting is in
the root [SECURITY.md](../SECURITY.md). Security is priority #1
([SPECIFICATION.md](SPECIFICATION.md) §2); nothing in any other document
overrides this one.

---

## 1. Security invariants

These hold at all times, in every phase:

1. **Fail closed.** Authentication failures, malformed protocol input,
   unexpected state transitions, and validation failures reject or terminate
   the affected session. There is no partially-trusted or "probably fine"
   state (FR-2.3).
2. **Reachability is not authorization.** Possession of a peer's IP address,
   hostname, and port grants nothing. Unknown identities are rejected before
   any application data is exchanged (FR-2.2).
3. **Mutual authentication.** Both sides cryptographically prove their
   identity on every connection. Encryption without peer authentication is
   insufficient (FR-2.1).
4. **No silent downgrade.** TLS version, protocol version, and feature
   negotiation never fall below either side's configured minimum without
   terminating.
5. **Network input is never trusted**, even post-authentication: all lengths
   validated before allocation, all invariants checked (NFR-1).
6. **Secrets stay put.** Private keys never leave the machine in normal
   operation, never appear in logs, and are stored under OS protection where
   available. Clipboard contents never appear in logs by default (FR-7.4).
7. **Local first.** No cloud dependency, no external telemetry (FR-7.5).
   Cloud infrastructure is never introduced to solve a local problem without
   explicit ADR-level approval.

## 2. Device identity

Each installation generates a persistent identity at first run (FR-1.1):

- device UUID (random)
- human-readable device name
- asymmetric keypair; credential form (raw public key vs. self-signed
  certificate) and its binding into TLS 1.3 mutual authentication are fixed
  by ADR (deferred decision 3)
- creation timestamp

Storage: private key material goes through the `SecureStorage` platform
trait — DPAPI or equivalent on Windows, Keychain/secret-service later.
Identity survives restarts; regeneration is an explicit user action that
invalidates existing pairings.

## 3. Pairing (trust establishment)

Trust is established only by an explicit ceremony on **both** machines
(FR-1.2). Conceptual flow:

```
Computer A:   crossover pair --listen
Computer B:   crossover pair <address-of-A>

Both display a short human-verifiable code, e.g.:

    Pairing fingerprint:  RAVEN-BLUE-47-MANGO

The user verifies the codes match and confirms on both machines.
```

On confirmation, each peer durably records the other's identity; future
connections authenticate automatically; identities not in the trust store
are rejected (FR-1.3).

**MITM requirement:** the ceremony must defeat an active man-in-the-middle
on first contact. The verification mechanism — short authentication string
(SAS) derived from the key exchange vs. a PAKE (e.g., SPAKE2 over a short
code) — is deferred decision 2. Whichever is chosen, the security argument
(what the human comparison/entry actually proves, and the probability an
active attacker survives it) must be written into the ADR.

## 4. Trusted peer store

Each record contains: peer UUID, device name, credential fingerprint,
first-paired and last-connected timestamps, permissions, optional remembered
addresses.

- Permissions model: per-peer flags (`keyboard`, `mouse`, `clipboard_send`,
  `clipboard_receive`). Initial implementation may default paired peers to
  full capability, but the data model supports granular permissions so they
  can be enforced later without migration.
- `crossover peers` / `peer show` / `peer remove` manage the store.
  Removal revokes authorization immediately (FR-1.4): active sessions from
  that identity are terminated and future connections rejected.
- The store contains **no private keys** — only the local device's public
  metadata and peers' public credentials. Theft of the trust store without
  the private key must not enable impersonation.

## 5. Transport security

- TLS 1.3 via rustls/tokio-rustls ([ARCHITECTURE.md](ARCHITECTURE.md) §7).
- Both peers present credentials; each side verifies the presented
  credential against the *expected* pinned identity from the trust store —
  not against a public CA hierarchy. There are no CAs in this system.
- Unsupported TLS versions, unknown identities, invalid credentials, and
  unsupported protocol versions all terminate with a distinguishable,
  logged reason (actionable diagnostics per FR-7.3, without leaking secret
  material).
- A packet capture must reveal no application payload in plaintext (FR-2.4);
  this is verified by test in Phase 1 ([ROADMAP.md](ROADMAP.md)).

## 6. Threat scenarios

The design must address each of these; tests and reviews trace back to this
table:

| # | Threat | Primary defense |
|---|--------|-----------------|
| T1 | Unknown LAN device connects to Crossover's port | Invariant 2: rejected before application data; bounded resource use pre-auth |
| T2 | Active MITM during first pairing | Pairing ceremony human verification (§3, ADR) |
| T3 | Replay of captured application messages | TLS 1.3 anti-replay; session-scoped message ids |
| T4 | Malformed protocol frames / parser exploitation | NFR-1 bounds + fail-closed parsing + fuzzing ([TESTING.md](TESTING.md)) |
| T5 | Oversized payloads (memory exhaustion) | Length validation before allocation; negotiated maxima |
| T6 | Stale/revoked peer credential reconnecting | Trust store is the authority; revocation is immediate (§4) |
| T7 | Stolen trust store (without private key) | Store contains no secrets usable for impersonation (§4) |
| T8 | Input injection before authorization completes | Input/clipboard handlers unreachable until session `ESTABLISHED` (state machine, [ARCHITECTURE.md](ARCHITECTURE.md) §5.3) |
| T9 | Clipboard exfiltration by unauthorized peer | Same as T8, plus per-peer clipboard permissions (§4) |
| T10 | Secrets leaking via logs/diagnostics | Invariant 6; log-content tests in CI |

Out of scope (documented, not defended): a fully compromised trusted peer
machine — a peer you paired with and that is now malicious can do whatever
its granted permissions allow; per-peer permissions are the containment
mechanism. Physical attackers with local admin on either machine are
likewise out of scope.

## 7. Security-sensitive code areas

Changes in these areas require heightened review and, where architectural,
an ADR: identity generation, key storage, credential verification, pairing,
trust store persistence/revocation, TLS configuration, protocol parsing and
length validation, input/clipboard authorization gates, and
logging/diagnostics (for leak risk).

Security review is a standing item in each phase's exit criteria and a
dedicated activity in Phase 6 ([ROADMAP.md](ROADMAP.md)).
