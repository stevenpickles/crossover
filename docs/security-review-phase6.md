# Phase 6 Security Review

Dedicated review against [SECURITY.md](SECURITY.md) §6 (threat scenarios)
and §7 (security-sensitive areas), a Phase 6 exit-criteria deliverable
(2026-08-09). Each threat is traced to its implementing defense and
assessed. Result: **defenses are in place and sound for T1–T5 and T7–T10;
one gap was found in T6 (active-session revocation).**

## Threat trace

| # | Threat | Defense — as implemented | Assessment |
|---|--------|--------------------------|------------|
| T1 | Unknown LAN device connects | TLS 1.3 with SPKI-pinned mutual verifiers (`crossover-security/src/tls.rs`); an unknown credential fails the handshake before any application data. Framing bounds buffering pre-auth (`framing.rs` `MAX_BUFFERED_BYTES`). | **Sound.** |
| T2 | Active MITM during first pairing | SPAKE2 PAKE over the short code with confirmation-MAC verification (`crossover-security/src/pairing.rs`); a wrong code or MITM fails the MAC and is fail-closed (ADR 0002). | **Sound.** |
| T3 | Replay of captured messages | TLS 1.3 stream anti-replay, plus session-scoped monotonic `message_id` (`framing.rs`) and per-grant input `sequence` regression checks that terminate the session (`control.rs` `on_peer_batch`). | **Sound.** |
| T4 | Malformed frames / parser exploitation | Fail-closed decoders return `ProtocolError` (session terminates); declared length validated against `MAX_FRAME_BODY_BYTES` the moment the 4-byte prefix arrives, before any large allocation; fuzz targets in CI (`fuzz smoke`). | **Sound.** |
| T5 | Oversized payloads (memory exhaustion) | Length validated before `Vec::with_capacity`; `MAX_FRAME_BODY_BYTES` ceiling and negotiated maxima; buffering bounded independent of the declared length. | **Sound.** |
| T6 | Stale/revoked peer reconnecting | **New connections:** the trust store is the authority and is re-read on *every* accept and every outbound establish attempt, so a revoked peer cannot establish a new session. **Active sessions:** not terminated — see finding below. | **Gap (see finding).** |
| T7 | Stolen trust store (no private key) | `TrustedPeer` holds only the peer's public SPKI fingerprint plus bookkeeping — "No secrets" (`trust.rs`); it cannot be used to impersonate. | **Sound.** |
| T8 | Input injection before authorization | Type-level: the send/receive surface exists only on `EstablishedSession`, which cannot be constructed before `CONNECTING → AUTHENTICATING → NEGOTIATING → ESTABLISHED` completes (`net.rs`). Input/clipboard handlers are unreachable earlier. | **Sound (strong).** |
| T9 | Clipboard exfiltration by unauthorized peer | Same establishment gate as T8; control/input authorization is session-scoped and checked on the grant-holder's identity for every batch (complete mediation, `control.rs`). Per-peer clipboard permissions are modeled but default-allow (as the spec permits). | **Sound; permissions not yet enforced (by design).** |
| T10 | Secrets leaking via logs | Code logs metadata only — session id / role / reason (`net.rs` `log_outcome`), clipboard latency in ms (never content), no key material; repo secret-scanning with push protection is enabled. | **Sound; see minor recommendation.** |

## Invariants (§1)

1. **Fail closed** — decoders, auth, and sequence checks all terminate the
   session on violation. ✔
2. **Reachability ≠ authorization** — pinned verifiers reject unknown SPKI
   before application data. ✔
3. **Mutual authentication** — both client and server present and verify
   pinned credentials. ✔
4. **No silent downgrade** — TLS restricted to `&[&TLS13]` on both configs;
   protocol `negotiate()` returns an error (terminating) when the ranges do
   not overlap at or above the configured minimum. ✔
5. **Network input never trusted** — lengths validated before allocation;
   all decoders bounded. ✔
6. **Secrets stay put** — private keys via `SecureStorage` (DPAPI), never
   logged; clipboard content never logged. ✔ (see minor recommendation)
7. **Local first** — no cloud, telemetry local only (FR-7.5). ✔

## Finding — T6: active-session revocation is not enforced

**Severity: medium.** SECURITY.md §4 and the T6 defense state that removal
"revokes authorization immediately: active sessions from that identity are
terminated and future connections rejected." Only the second half holds.

- *Future connections are rejected.* The listener loads the trust store
  fresh on every `accept`, and the connector snapshots it on every establish
  attempt, so a revoked peer cannot open a **new** session.
- *Active sessions are not terminated.* `crossover peers remove` runs as a
  separate process and writes the store; the running `crossover run` process
  holds trust in memory for the life of a session and neither observes the
  store changing nor re-validates an established peer against it. The
  per-session kill switches exist but are wired to shutdown/disconnect, not
  to revocation. So a currently-connected revoked peer keeps its session
  until it drops for some other reason.

**Impact.** Bounded: the revoked peer cannot *re*-connect once its session
ends, so the exposure is the remaining lifetime of the one active session.
On a two-machine LAN where revocation is a deliberate act, this is a real
but limited window — and it contradicts the "immediately" claim.

**Options (resolution is a decision for the maintainer):**
1. **Fix** — have `crossover run` observe trust-store changes (a file watch,
   or a periodic reload on the existing accept/attempt cadence) and, when a
   live session's peer fingerprint is no longer trusted, fire that session's
   existing kill switch. Cross-cutting enough to warrant an ADR, but the
   termination plumbing already exists, so it is tractable.
2. **Accept + document** — record via ADR that revocation is enforced on new
   connections and that an active session ends on its next drop, and amend
   SECURITY.md §4 / T6 to match, noting the mitigation.

*Recommendation:* implement option 1 — it is a genuine security property and
the kill switches are already in place — but whether it lands in Phase 6 or
is deferred is the maintainer's call.

## Minor recommendation — T10: add an automated log-content assertion

The code logs metadata only and secret-scanning is enabled, but T10 cites
"log-content tests in CI." I did not locate a dedicated automated test that
runs a representative clipboard/input operation and asserts the captured
logs contain no content or key material. Recommend confirming one exists or
adding a small one, so invariant 6 is guarded by a test and not only by
discipline and review.

## Conclusion

The security posture is strong: fail-closed throughout, pinned mutual TLS
1.3 with no downgrade, allocation-bounded parsing with fuzzing, and a
type-level guarantee that no input is reachable before authentication. One
medium finding (T6 active-session revocation) needs a resolve-or-accept
decision, and one minor test-coverage recommendation (T10) is noted.
