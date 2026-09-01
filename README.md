<p align="center">
  <img src="assets/branding/crossover-logo.png" alt="Crossover logo" width="120">
</p>

# Crossover

Secure keyboard, mouse, and clipboard sharing between computers over an IP
network — two trusted machines beside one another should feel like one
larger workstation, without weakening the security boundary between them.

**Move. Type. Copy. Paste.**

> **Status: v0.2.0 — text, images, files and folders, and a screen
> arrangement you draw.** Two Windows machines pair with a typed one-time
> code, hold a mutually authenticated TLS 1.3 session with automatic
> reconnection, share one keyboard and mouse across screen edges derived
> from an arrangement drawn in `crossover layout`, run unattended as a
> background service, and synchronize the clipboard in both directions —
> **text, images, and files and folders** — all validated on two machines
> over a wired link. The drawn topology has not yet completed its
> two-machine soak. macOS and Linux come later (Phase 9). The
> [roadmap](docs/ROADMAP.md) carries the authoritative current-phase marker.

## What it does

- One keyboard and mouse drives both computers; arrange both machines'
  monitors in `crossover layout` and the pointer transfers control wherever
  the drawing says two screens touch — including a seam between two of one
  machine's own monitors, and a corner where three meet
- Clipboard contents synchronize in both directions — text, images, and
  files and folders. Images are carried in the source clipboard's own
  format, byte for byte; a copied file or folder is spooled, verified, and
  pasted as an ordinary file, and only for peers you have explicitly
  granted it ([ADR 0015](docs/adr/0015-spooled-virtual-file-paste.md))
- All traffic mutually authenticated and encrypted (TLS 1.3); explicit
  pairing with a typed one-time code; local-first — no cloud, no accounts,
  no external telemetry. Crossover adds no cloud service; what arrives is
  installed on your clipboard, so your own Windows clipboard settings still
  apply to it — if **Clipboard History** (Win+V) or **Cloud Clipboard** is on,
  it is on for synchronized content too. Turn them off in Settings → System →
  Clipboard if you would rather they were not
- Runs unattended as a background service, reconnecting on its own

## What it does not do yet

- Send permission for **text and images** — `clipboard_send` is enforced for
  files only; text and images still travel without consulting it
- macOS and Linux — the platform boundary exists and the core compiles on
  all three, but only the Windows implementations are written (Phase 9)
- Rearranging screens on a machine that holds no drawn arrangement takes one
  restart before an arrangement adopted from the peer drives the cursor
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md))
- More than two machines, a tray application, discovery, or auto-update
  (Phase 10)
- Code-signed binaries, so SmartScreen will warn on first run

Target today: two Windows machines on a LAN. Implementation language: Rust.

## Building

```powershell
.\scripts\build.ps1
```

One command: runs the CI gate (format, lint, tests, dependency audit), builds
all three executables — `crossover`, the background service `crossover-svc`,
and the layout editor `crossover-layout` — and writes every deliverable —
portable archive, checksum, Chocolatey package, and an `artifacts.json`
manifest — into `dist\`. Use
`-SkipChecks` for a fast iteration build and `-SkipChocolatey` to stop at the
archive. Installing and packaging are covered in
[packaging/README.md](packaging/README.md).

Every binary knows exactly what it is:

```powershell
crossover version          # build version, channel, source commit, toolchain
crossover version --json   # the same, for scripts
crossover -V               # just the version string
```

A build that is not a tagged release says so — `0.2.0-dev.7.gabc1234.dirty`
names the commit it came from and admits to uncommitted edits.

## Arranging your screens

```powershell
crossover layout
```

Opens the editor with both machines' monitors drawn to scale. Drag them into
the arrangement they have on your desk and save; both running workers pick the
change up within a couple of seconds. `crossover-layout.exe` must sit beside
`crossover.exe` — the packages install them together. `--left` and `--right`
still work and still warn that they are deprecated; they cannot express a seam
between two monitors of one machine.

## Letting a peer send you files

```powershell
crossover peers                       # lists every peer and its grants
crossover peers allow-files <id>      # deny-files to withdraw
```

File transfer is **off by default** for every peer and is the only way a peer
can cause a write to your disk. Files are capped at 256 MiB.

## Documentation

| Document | Contents |
|----------|----------|
| [docs/SPECIFICATION.md](docs/SPECIFICATION.md) | What Crossover must do: requirements, priorities, scope |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Workspace layout, platform abstraction, state machines |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | Wire protocol |
| [docs/SECURITY.md](docs/SECURITY.md) | Threat model and trust model |
| [docs/TESTING.md](docs/TESTING.md) | Testing strategy and Definition of Done |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Development phases and exit criteria |
| [docs/SOAK.md](docs/SOAK.md) | Two-machine hardware validation runbooks and session records |
| [docs/adr/](docs/adr/README.md) | Architectural decision records |
| [docs/platform-risks-macos.md](docs/platform-risks-macos.md) | What a macOS port will fight, catalogued before it exists |
| [docs/platform-risks-linux.md](docs/platform-risks-linux.md) | The same for Linux, where the X11/Wayland split decides the shape |

Vulnerability reporting: [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
