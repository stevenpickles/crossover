<p align="center">
  <img src="assets/branding/crossover-logo.png" alt="Crossover logo" width="120">
</p>

# Crossover

Secure keyboard, mouse, and clipboard sharing between computers over an IP
network — two trusted machines beside one another should feel like one
larger workstation, without weakening the security boundary between them.

**Move. Type. Copy. Paste.**

> **Status: Phase 2 (Reliable Text Clipboard) complete — the Secure
> Clipboard Prototype works.** Two Windows machines pair with a typed
> one-time code, hold a mutually authenticated TLS 1.3 session with
> automatic reconnection, and synchronize the text clipboard in both
> directions: copy on one, paste on the other. Next up: Phase 3 — Remote
> Mouse. The [roadmap](docs/ROADMAP.md) carries the authoritative
> current-phase marker.

## What it will do

- One keyboard and mouse controls multiple computers; moving the pointer
  across a configured screen edge transfers control, like adjacent monitors
- Clipboard contents synchronize reliably between trusted machines
- All traffic mutually authenticated and encrypted (TLS 1.3); explicit
  pairing; local-first — no cloud, no accounts, no external telemetry

Initial target: two Windows machines on a LAN. Long term: Windows, macOS,
Linux. Implementation language: Rust.

## Building

```powershell
.\scripts\build.ps1
```

One command: runs the CI gate (format, lint, tests, dependency audit), builds
both executables, and writes every deliverable — portable archive, checksum,
Chocolatey package, and an `artifacts.json` manifest — into `dist\`. Use
`-SkipChecks` for a fast iteration build and `-SkipChocolatey` to stop at the
archive. Installing and packaging are covered in
[packaging/README.md](packaging/README.md).

Every binary knows exactly what it is:

```powershell
crossover version          # build version, channel, source commit, toolchain
crossover version --json   # the same, for scripts
crossover -V               # just the version string
```

A build that is not a tagged release says so — `0.1.0-dev.7.gabc1234.dirty`
names the commit it came from and admits to uncommitted edits.

## Documentation

| Document | Contents |
|----------|----------|
| [docs/SPECIFICATION.md](docs/SPECIFICATION.md) | What Crossover must do: requirements, priorities, scope |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Workspace layout, platform abstraction, state machines |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | Wire protocol |
| [docs/SECURITY.md](docs/SECURITY.md) | Threat model and trust model |
| [docs/TESTING.md](docs/TESTING.md) | Testing strategy and Definition of Done |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Development phases and exit criteria |
| [docs/adr/](docs/adr/README.md) | Architectural decision records |

Vulnerability reporting: [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
