# Security Policy

## Supported versions

Crossover is pre-1.0 software under active development. Only the most
recent release is supported: fixes land on `main` and ship in the next tag,
and earlier tags are not patched. `0.1.0` is the first release.

Binaries are **not code-signed** yet, so Windows SmartScreen will warn about
them; verify the SHA-256 published with each release rather than trusting
the download alone.

## Reporting a vulnerability

Please report suspected vulnerabilities privately via
[GitHub Security Advisories](../../security/advisories/new) rather than
opening a public issue. Include reproduction steps and impact where
possible.

You can expect an acknowledgement within a reasonable time, and coordinated
disclosure once a fix is available.

## Security design

Crossover's threat model, trust model, pairing design, and security
invariants are documented in [docs/SECURITY.md](docs/SECURITY.md).
