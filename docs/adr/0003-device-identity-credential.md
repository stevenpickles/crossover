# 0003. Device identity: Ed25519 key pinned by SPKI, carried in a self-signed certificate

Status: Accepted
Date: 2026-08-07

## Context

FR-1.1 requires a persistent cryptographic device identity, and
[SECURITY.md](../SECURITY.md) §5 requires TLS 1.3 mutual authentication
where each side verifies the presented credential against a pinned identity
from the trust store — there are no CAs in this system. Deferred was the
credential form: raw public key (RFC 7250) vs. self-signed X.509, and how
identity binds into rustls.

## Decision

- The device identity keypair is **Ed25519**.
- The canonical peer identity is the **SPKI (SubjectPublicKeyInfo)
  SHA-256 fingerprint** — a *key* identity, never a certificate identity.
- The key is carried in a **minimal self-signed X.509 certificate**
  (generated with `rcgen`) solely because certificates are TLS's native
  credential container. Certificate fields — subject, validity, extensions,
  signature chain — carry **no trust semantics**.
- Verification is done by custom rustls certificate verifiers that extract
  the presented SPKI, compare it against the expected pinned identity from
  the trust store, and deliberately ignore X.509 chain building, expiry,
  and name checks. Unknown or mismatched keys reject the session
  (invariants 1–3).
- Certificates may be regenerated freely from the same keypair; trust
  survives because the key, not the certificate, is pinned.

## Alternatives Considered

- **Raw public keys (RFC 7250)**: conceptually exact — no X.509 shell at
  all. Rejected for now: rustls RPK support is newer and far less
  exercised than the certificate path, and the surrounding diagnostic
  ecosystem (packet inspectors, TLS tooling) understands certificates.
  Because identity is pinned at the SPKI, migrating to RPK later swaps the
  container without changing the identity model — the option stays open.
- **Pin the certificate fingerprint**: rejected — couples trust to a
  rotatable artifact, so regenerating a certificate would break pairing
  for no security gain.
- **Private CA issuing per-device certs**: rejected — reintroduces a
  central signing authority the local-first model explicitly avoids, and
  the trust store already provides enrollment and revocation.

## Consequences

- Easier: works with mainstream, battle-tested rustls paths today; SPKI
  pinning survives certificate regeneration; RFC 7250 migration remains a
  contained change.
- Harder: the custom verifiers are security-critical code
  ([SECURITY.md](../SECURITY.md) §7) and take heightened review plus
  dedicated tests: wrong key, unknown peer, revoked peer, and
  expired-certificate-with-valid-key (which must still authenticate, since
  expiry carries no semantics here).
- The rustls crypto provider must support Ed25519 certificates (the
  default providers do); the provider choice is pinned during
  implementation.
- `rcgen` joins the dependency set for identity generation, subject to the
  standard dependency evaluation.
