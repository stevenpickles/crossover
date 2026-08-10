# 0011. Background operation: a minimal LocalSystem service launches the real process into the user session

Status: Proposed
Date: 2026-08-09

## Context

Phase 6 wants Crossover to run **unattended** — start automatically, survive
crashes, and keep the peer link up across a machine's uptime
([ROADMAP.md](../ROADMAP.md)).

The hard constraint that shapes everything: Crossover **injects input,
installs low-level input hooks, captures the desktop cursor, and reads the
clipboard**, all of which require the **interactive user session**. A Windows
**service runs in session 0** (Session 0 Isolation) with no desktop access,
so a service *cannot itself* perform any Crossover function.

The maintainer chose the most robust auto-start model (over Task Scheduler
at-logon and a Run-key/Startup shortcut): a service that **launches the real
process into the active user session** and supervises it. This starts earlier
and restarts more reliably than user-session-only triggers, at the cost of a
privileged component — recorded here because a LocalSystem service is a
security-sensitive area (§7).

## Decision

Two processes with a strict privilege split:

1. **The launcher service** — a **LocalSystem** Windows service whose *only*
   responsibilities are:
   - detect the active console session (`WTSGetActiveConsoleSessionId`) and
     react to logon/logoff via `SERVICE_CONTROL_SESSIONCHANGE`;
   - obtain that session's user token (`WTSQueryUserToken`), duplicate it
     (`DuplicateTokenEx`), and start `crossover run` in the session as that
     user with `CreateProcessAsUser`;
   - **supervise**: if the child exits while a user is logged on, relaunch it
     after a bounded backoff (crash recovery); on logoff, stop the child (no
     session to inject into) and start a fresh one on the next logon.

2. **The worker** — `crossover run`, started **as the logged-in user** in
   their session, doing everything it does today: reads `config.toml` for its
   role/peer/side (ADR 0011 relies on the startup-config file), pairs are
   already established, and it forwards input/clipboard at *user* privilege.

### The security invariant that makes a SYSTEM service acceptable

**The service is a pure launcher/watchdog. It never touches the network, the
clipboard, peer data, protocol parsing, or any other untrusted input.** Its
inputs are the OS session state and its own child's exit code — nothing an
attacker on the LAN can reach. All attackable surface (TLS, framing, pairing,
injection) stays in the **user-privilege** worker, exactly where it is today.
So the service's LocalSystem context carries **minimal** attack surface, and
compromising Crossover's network handling yields user, not SYSTEM. This
invariant is load-bearing and must not erode: no feature may push
network/peer handling into the service.

The worker runs as the **user**, not SYSTEM — no privilege escalation of the
input path; injection targets the user's own desktop, as now.

### Install / uninstall / status

- `crossover service install` registers the service (LocalSystem, auto-start)
  — requires elevation; it fails with a clear message if not elevated.
- `crossover service uninstall` stops and removes it (also elevated).
- `crossover service status` reports whether it is installed/running (no
  elevation needed).
- The foreground `crossover run` stays exactly as-is for interactive use; the
  service model is opt-in via install.

### Supervision detail

The service relaunches the worker with bounded backoff so a persistently
crashing worker cannot spin (mirrors the reconnect policy's intent, ADR-less
as it is not protocol). A worker that exits cleanly because the user ran
`crossover` interactively, or logged off, is not a crash — session state, not
just exit code, gates a relaunch.

## Alternatives considered

- **Task Scheduler at-logon (user session), restart-on-failure** — simpler, no
  privileged component, gives crash-restart; rejected by the maintainer in
  favor of the service's earlier start and stronger supervision.
- **Run key / Startup shortcut** — simplest, but no crash-restart.
- **Service that does the work itself** — impossible: session 0 cannot inject
  input or capture the desktop.

## Consequences

- A new **LocalSystem service** exists on installed machines; installation
  needs admin. This is the cost of the chosen robustness.
- The launcher/watchdog is a small, testable state machine (session state +
  child lifecycle); the privileged Win32 (`WTSQueryUserToken`,
  `DuplicateTokenEx`, `CreateProcessAsUser`, SCM/`SERVICE_CONTROL_*`) is
  isolated behind a platform boundary and exercised on Windows.
- The **minimal-service invariant** is a standing security constraint: the
  service must never gain untrusted input. A future review checks it holds.
- Implementation lands behind `crossover service install/uninstall/status`;
  the interactive `crossover run` is unchanged.
