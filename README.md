# Crossover

Secure keyboard, mouse, and clipboard sharing between computers over an IP
network — two trusted machines beside one another should feel like one
larger workstation, without weakening the security boundary between them.

**Move. Type. Copy. Paste.**

> **Status: Phase 0 (Repository Foundation) complete.** The Cargo
> workspace, tri-OS CI gate, and error/logging conventions are in place;
> no user-facing functionality exists yet. Next up: Phase 1 — Secure Peer
> Connection. The [roadmap](docs/ROADMAP.md) carries the authoritative
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
