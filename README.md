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
