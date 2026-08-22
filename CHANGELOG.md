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

### Added

- **The layout editor names monitors the way you do.** A rectangle is now
  captioned with the monitor's own name — `DELL U2720Q`, the string
  Windows Settings shows for it — instead of its device string
  (`\\.\DISPLAY1`), for both machines' screens. A laptop's built-in panel,
  which has no such name to read, reads `Internal Display`. Anything else
  the OS will not name still shows its device string, and two identical
  screens on one machine are numbered `(1)` and `(2)` so the pair stays
  tellable apart. The name is a caption only: arrangements, saved layouts,
  and where the cursor crosses all still address a monitor by its device
  string, unchanged.

- **The layout editor draws your screens in their real proportions.** A
  rectangle is now seeded from the panel's actual size in millimetres, so a
  13" laptop screen beside a 27" monitor is drawn about half its height
  instead of the same size — and the arrangement you draw is the
  arrangement you experience, since a cursor crosses at the height the
  drawing says it does. Screens whose size cannot be read (a virtual or
  remote display, a panel with no readable EDID) — or whose reported size
  is not one a real panel could have — are drawn from their pixel counts,
  scaled to sit believably beside the screens that could be measured, and
  captioned `(size estimated)` so it is clear which rectangles are guesses.
  Where *nothing* on either machine can be measured, every rectangle is
  drawn from pixels exactly as before and nothing is captioned, since there
  is no difference to point out. Only *new* rectangles are sized this way: an
  arrangement you have already drawn and saved is never rescaled behind
  your back, and a rectangle you are in the middle of dragging is never
  resized under the pointer.

- **You can correct a screen's size in the layout editor.** Click any
  screen and a panel on the right names it, shows its resolution, and gives
  its drawn width and height in millimetres to edit. Type the real size of
  a screen your machine measured wrongly — or could not measure at all —
  and the rectangle is redrawn at it, with the screens beside it in that row
  shuffling along so the seams stay closed — screens elsewhere on the
  machine, such as one plugged in since the arrangement was saved, stay
  exactly where they are. Editing one dimension
  fills the other in the screen's current proportions unless you untick the
  lock. A size no panel could be (under 50 mm or over 3000 mm on a side) is
  refused with a note rather than quietly rounded into range, and **Use
  detected size** puts a rectangle back on the size your machine reported —
  greyed out for a screen captioned `(size estimated)`, since there is no
  measurement to go back to. A size you have stated stops being captioned
  as a guess, is not undone by the editor's once-a-second re-read of the
  worker, and is kept by saving the arrangement like any other change: the
  correction *is* the rectangle, so there is nothing else to store and
  nothing new on the wire.

- **Monitors report how big they physically are.** Each machine now reads
  the real width and height of every attached panel — in millimetres, off
  the monitor's own EDID — and reports it to the other machine and to the
  layout editor's state file, which is what the editor's proportional
  drawing (above) is built on. A screen whose size cannot be read or does
  not look believable (a projector, a virtual display, a remote session)
  simply reports none, and is drawn from its pixels as described above. The
  measurement is proportion only — arrangements, saved layouts, and where
  the cursor crosses all still address a monitor by its device string.

### Changed

- **Wire protocol moves to version 6, and accepts nothing older**
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md), amended 2026-08-21
  and 2026-08-22). Each monitor a machine reports now carries two further
  optional facts about itself, added since version 4: its **product name**,
  so the *other* machine's editor can caption the rectangle, and its
  **physical size**, so that editor can draw it in proportion to the real
  screen. Each adds a byte to every monitor of every report, and no feature
  bit can hide either. **Both machines must be upgraded together**; a mixed
  pair refuses cleanly at the handshake naming both version ranges, rather
  than establishing a session that dies on the first report. Nothing about
  crossing changed — both fields are display-only — but the bytes are on
  the wire regardless, and it is the bytes that force the bump.
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
