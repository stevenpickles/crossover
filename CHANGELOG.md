# Changelog

Notable changes to Crossover. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) — with
the pre-1.0 caveat that the wire protocol and on-disk formats may still
change between minor versions. Protocol compatibility is negotiated per
session and versioned separately (`PROTOCOL_VERSION` in
`crossover-protocol`); a release note says so whenever that number moves.

Builds that are not tagged releases identify themselves as such —
`0.1.0-dev.7.gabc1234` — and say where they came from. Run
`crossover version` on any binary to see exactly what it is.

## Unreleased

### Changed

- **Wire protocol moves to version 4, and does not accept version 3**
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md)). Crossing control
  now carries an `EntryPoint` — destination monitor, edge, fraction, and
  layout revision — where it used to carry a bare fraction, a structural
  change to messages that already travel between every pair of peers,
  which no feature bit can hide, so **this build cannot connect to a v3
  peer and a v3 peer cannot connect to this build**. Both machines must be
  upgraded together. The failure is a clean refusal at the handshake
  naming both version ranges, not a session that dies later; `crossover
  version` reports the range a build speaks.
- **Wire protocol moves to version 3, and does not accept version 2**
  ([ADR 0017](docs/adr/0017-protocol-version-3.md)). A file descriptor on the
  clipboard offer adds a byte to *every* offer, which no feature bit can
  hide, so **v0.1.0 cannot connect to this build and this build cannot
  connect to v0.1.0**. Both machines must be upgraded together. The failure
  is a clean refusal at the handshake naming both version ranges, not a
  session that dies later; `crossover version` reports the range a build
  speaks.

## [0.1.0] — 2026-08-16

The first release. Two Windows machines share one keyboard, mouse, and
clipboard over a mutually authenticated TLS 1.3 link, unattended.

### Added

**Secure link.** Pairing with a typed one-time code (SPAKE2,
[ADR 0002](docs/adr/0002-pairing-verification-mechanism.md)); device
identity as an Ed25519 key pinned by the SHA-256 of its SPKI
([ADR 0003](docs/adr/0003-device-identity-credential.md)); mutual TLS 1.3
on every session; a trust store that survives restarts, and revocation that
terminates live sessions rather than only refusing new ones
([ADR 0010](docs/adr/0010-active-session-revocation.md)).

**Clipboard.** Text and images synchronize in both directions. Images
travel in the source clipboard's own raster format, verbatim — no
transcode, no re-compression
([ADR 0014](docs/adr/0014-chunked-rich-clipboard-transfer.md)) — chunked so
a large transfer never becomes one unpreemptable frame. Re-offering content
the peer already holds costs one offer and one decline instead of
megabytes. Loop prevention, deterministic conflict resolution, bounded
retries on contention, and success defined as "the destination clipboard
was updated", never "the bytes were sent" (FR-3.2).

**Input.** One keyboard and mouse drive both machines. Crossing a
configured screen edge transfers control and keyboard focus with no manual
switch ([ADR 0009](docs/adr/0009-seamless-edge-transfer.md)); control
returns at the reverse edge. Keys travel by physical USB HID usage so
layouts on the two machines cannot disagree
([ADR 0008](docs/adr/0008-keyboard-key-representation.md)). A disconnect
releases every held key and button — a stuck key is treated as
release-blocking.

**Responsiveness under load.** Interactive input and bulk transfers travel
in separate lanes, and input preempts bulk between chunks
([ADR 0013](docs/adr/0013-interactive-over-bulk-prioritization.md)), so
copying a 4K screenshot does not make the mouse stutter. Every queue on the
path is bounded by messages *and* bytes.

**Unattended operation.** A minimal `LocalSystem` service launches and
supervises the worker in the interactive session
([ADR 0011](docs/adr/0011-background-service-launcher.md)); the worker runs
at high integrity so it can drive elevated windows
([ADR 0012](docs/adr/0012-elevated-worker-integrity.md)). The service
binary links no network, TLS, or protocol code at all — that isolation is
enforced by the dependency graph rather than by discipline. Crash-relaunch
uses a capped backoff, validated over a multi-day soak.

**Packaging and provenance.** `scripts/build.ps1` runs the full gate and
produces every deliverable — portable archive, checksum, Chocolatey
package, and an `artifacts.json` manifest — and CI runs the same script, so
a released artifact and a locally built one come from one code path. Every
binary carries its own identity: `crossover version [--json]` and
`crossover-svc --version` report the build version, source commit and
branch, toolchain, target, and the protocol versions the build speaks; the
same values are stamped into the Windows version resource.

**Diagnostics.** Structured logging to a rolling file under
`~/.crossover/logs`, which is what makes a headless service-launched run
diagnosable at all. Execution statistics — frames and bytes by class,
session lifetimes, clipboard outcomes and latency, input queue-to-wire
latency — are written every fifteen minutes and again at shutdown, so a run
leaves its numbers behind however it ends. Panics reach the log rather than
a `NUL` stderr.

### Known limitations

- **Windows only.** The platform boundary exists and core, protocol, and
  security crates build and test on Windows, Linux, and macOS, but only the
  Windows implementations of the platform traits are written.
- **Two machines.** The protocol treats peers symmetrically and nothing
  assumes a hub, but more than two is untested and unsupported.
- **Files and folders do not travel.** Designed in
  [ADR 0015](docs/adr/0015-spooled-virtual-file-paste.md) and deliberately
  not implemented: it is the first peer-controlled write surface onto disk,
  and the design is still under consideration.
- **Binaries are not code-signed**, so SmartScreen warns on first run.
  Verify the published SHA-256.
- **No automatic updates.** Upgrading means installing the new package.
- **Images are capped at 64 MiB** and oversized captures are skipped with a
  log line rather than transferred.
- **Input latency under a saturating bulk transfer does not meet its own
  criterion.** Measured on hardware over **WiFi**: mean 1.94 ms, worst case
  309.8 ms, against a design expectation of tens of microseconds and
  single-digit milliseconds. Input still preempts bulk *between* frames as
  designed, but a frame already being written blocks the session loop until
  the socket accepts it, and on a contended wireless link that can be a
  third of a second. Wired is untested and the arithmetic in ADR 0013
  assumed 2.5 GbE. Nothing is dropped or stuck; a large transfer can make
  the pointer feel less immediate than it does at rest.
- A worker has been seen not exiting after a clean shutdown; the cause is
  unidentified, but a stuck message pump can no longer wedge the process —
  it is named in a warning and detached.

### Security

The threat model, trust model, and security invariants are documented in
[docs/SECURITY.md](docs/SECURITY.md). A dedicated review against them was
carried out before unattended operation shipped
([docs/security-review-phase6.md](docs/security-review-phase6.md)).
Parsers that touch network input are fuzzed on every change, and every
queue influenced by a peer is bounded before allocation.

[0.1.0]: https://github.com/stevenpickles/crossover/releases/tag/v0.1.0
