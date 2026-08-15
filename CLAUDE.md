# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Crossover: secure keyboard/mouse/clipboard sharing between computers (a
Synergy-like tool), implemented in Rust. The authoritative record of
project state is the current-phase marker at the top of `docs/ROADMAP.md`
— check it before doing project work.

## Source of truth

The `docs/` specification suite is authoritative. Read the relevant document
before working in its area:

- `docs/SPECIFICATION.md` — requirements (FR-x.y / NFR-x numbering), priorities, scope, Windows platform risks
- `docs/ARCHITECTURE.md` — crate layout, platform-abstraction rules, state machines, tech choices
- `docs/PROTOCOL.md` — wire protocol invariants
- `docs/SECURITY.md` — threat model and security invariants (priority #1)
- `docs/TESTING.md` — test strategy and the Definition of Done
- `docs/ROADMAP.md` — phases with exit criteria; update its current-phase marker at milestones
- `docs/adr/README.md` — ADR process; lists decisions deliberately deferred to ADRs

If implementation reveals a better design, update the docs deliberately —
never let code and specification diverge silently.

## Hard rules

- Never weaken a security requirement or remove validation to make tests
  pass. Security > clipboard reliability > input correctness > everything
  else (`docs/SPECIFICATION.md` §2).
- Never log clipboard contents or private key material.
- Platform-specific code (Win32 etc.) lives only in `crossover-platform-*`
  crates behind the traits in `crossover-platform`; core/protocol crates
  must compile and test on all three OSes.
- Architecturally significant changes (crate boundaries, protocol/trust
  changes, core library swaps) require an ADR — see the "when required"
  list in `docs/adr/README.md`.
- Stay within the current roadmap phase; don't implement future-phase
  functionality except to keep the architecture clean.
- A stuck key/button after disconnect, or a clipboard sync loop, is a
  release-blocking defect.
- Bound everything influenced by network input (frames, fields, queues,
  retries); validate lengths before allocating. Malformed input must never
  panic.

## Development

- No build tooling exists yet. Once the Phase 0 workspace lands, the CI
  gate is: `cargo fmt --check`, `cargo clippy --workspace --all-targets`
  (warnings denied), `cargo build --workspace`, `cargo test --workspace` —
  on Windows, Linux, and macOS.
- Prefer small, independently testable/reviewable/revertible tasks; keep
  the repo buildable after each integrated change.
- "Done" = the Definition of Done in `docs/TESTING.md` §5, not "worked once
  manually".

## Conventions

- Branching model: feature branches merge into `dev` (the integration
  branch); `main` is the stable branch. Branch new features from `dev`.
- `dev` and `main` are protected: nothing lands on either by a local merge
  or a direct push. Every integration goes through a formal GitHub pull
  request (`feature/<n>/...` → `dev`, `dev` → `main`) that CI must pass.
- Feature branches: `feature/<n>/<short-description>`, where `<n>` increments
  sequentially per feature. One focused feature per branch, so the branch
  history alone tells the story of what has been developed.
- Commit granularly, grouped by purpose; messages lead with *why* the change
  is made. No AI attribution trailers/footers in commits or PRs.
- License: MIT
