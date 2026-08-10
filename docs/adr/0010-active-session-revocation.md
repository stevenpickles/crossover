# 0010. Revocation terminates active sessions, not just new ones

Status: Accepted
Date: 2026-08-09

## Context

[SECURITY.md](../SECURITY.md) §4 and threat T6 state that removing a peer
"revokes authorization **immediately**: active sessions from that identity
are terminated and future connections rejected." The Phase 6 security review
([security-review-phase6.md](../security-review-phase6.md)) found only the
second half implemented:

- **New connections are rejected.** The listener reloads the trust store on
  every `accept`, and the outbound supervisor snapshots it on every
  establish attempt, so a revoked peer cannot open a new session.
- **Active sessions were not terminated.** `crossover peers remove` runs as a
  separate process and writes the store; the running `crossover run` process
  held trust in memory for the life of a session and never observed the
  change. So a currently-connected revoked peer kept its session until it
  dropped for some other reason.

Revocation of a trusted peer is a security-sensitive area (§7), so the fix is
recorded here.

## Decision

The running process **enforces revocation on live sessions**. A revocation
checker periodically reloads the trust store and, for every live session
whose peer SPKI fingerprint is no longer trusted, terminates it through the
plumbing that already exists:

- an **inbound** session — fire its per-session kill switch;
- the **outbound** session — call the supervisor's `shutdown()` (the peer is
  revoked, so stopping reconnection to it is exactly right).

Termination flows through the normal disconnect path, so the control engine
releases any held input on the resulting `SessionLost` (no stuck keys,
FR-4.4). New connections were, and remain, rejected by the trust re-read on
each accept/attempt.

**Poll, not file-watch.** The checker reloads on a short fixed interval (2 s)
rather than watching the store file. Revocation is a deliberate act on a
two-machine LAN; a bounded few-second latency is "immediate" in that context,
and polling avoids a filesystem-notification dependency and its per-platform
edge cases. The reload is the same cheap operation the listener already does
per accept.

## Alternatives considered

- **Filesystem watch** (e.g. `ReadDirectoryChangesW`): lower latency, but adds
  a dependency and platform-specific handling for a property a 2 s poll
  already satisfies.
- **Re-validate every frame** against the trust store: correct but wasteful on
  the input hot path, and no more secure than a short poll given TLS already
  authenticated the peer at establishment.
- **Per-session outbound kill** in the supervisor: unnecessary — the outbound
  side connects to exactly one peer, so `shutdown()` (stop the session and
  stop reconnecting to a now-untrusted peer) is the correct granularity.

## Consequences

- Revocation terminates an active session within the poll interval, making the
  SECURITY.md §4 / T6 "immediately" claim true in practice (bounded by 2 s).
- The `run` process now reloads the trust store on a timer; this is local and
  cheap, and reuses the existing load path.
- Which session (if any) to kill is a pure function of the live fingerprints
  and the current store, so it is unit-tested; the wiring is integration.
