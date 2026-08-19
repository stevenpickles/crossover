# 0011. Background operation: a minimal LocalSystem service launches the real process into the user session

Status: Accepted
Date: 2026-08-09

Amended by [0012](0012-elevated-worker-integrity.md): the worker still runs as
the user (never SYSTEM), but for an administrator it now launches at **high
integrity** (via the user's elevated linked token) so it can capture and inject
over elevated windows. The "no privilege escalation of the input path" clause
below is revised there; the minimal-service invariant is unchanged.

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

Two processes with a strict privilege split — and, deliberately, **two
separate binaries**, so the split is enforced by the build, not just at
runtime:

1. **The launcher service — a separate binary, `crossover-svc.exe`** — a
   **LocalSystem** Windows service whose *only* responsibilities are:
   - detect the active console session (`WTSGetActiveConsoleSessionId`) and
     react to logon/logoff via `SERVICE_CONTROL_SESSIONCHANGE`;
   - obtain that session's user token (`WTSQueryUserToken`), duplicate it
     (`DuplicateTokenEx`), and start `crossover.exe run` in the session as that
     user with `CreateProcessAsUser`;
   - **supervise**: if the child exits while a user is logged on, relaunch it
     after a bounded backoff (crash recovery); on logoff, stop the child (no
     session to inject into) and start a fresh one on the next logon.

   It is a distinct workspace binary (`apps/crossover-svc`) that depends **only**
   on `crossover-platform-windows` — never on `crossover-core`, `-protocol`, or
   `-security`. See the security invariant below for why the separation is a
   binary boundary and not merely a `crossover.exe` sub-command.

2. **The worker — `crossover.exe run`** — started **as the logged-in user** in
   their session, doing everything it does today: reads `config.toml` for its
   role/peer/side (ADR 0011 relies on the startup-config file), pairs are
   already established, and it forwards input/clipboard at *user* privilege.
   This is the full Crossover binary; the CLI's `crossover service
   install/uninstall/status` subcommands manage `crossover-svc.exe`'s SCM
   registration.

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

**The separate binary makes this invariant structural.** `crossover-svc.exe`
depends only on `crossover-platform-windows`; its dependency graph excludes
`crossover-core`, `-protocol`, and `-security` entirely. The SYSTEM-privileged
process therefore does not merely *avoid running* the network/TLS/clipboard
code — it does not **contain** that code, so it cannot be steered into it by
any bug. A reviewer verifies the invariant by reading one `Cargo.toml` and
`cargo tree`, not by auditing control flow. Were the service instead a mode of
`crossover.exe`, the LocalSystem process would link the whole protocol stack
and the invariant would rest on discipline alone. Keeping the two as distinct
binaries is thus a security decision, not packaging convenience; no change may
add a Crossover network/protocol crate to `crossover-svc`'s dependencies.

The worker runs as the **user**, not SYSTEM — no privilege escalation of the
input path; injection targets the user's own desktop, as now.

### Cross-platform shape: a `ServiceManager` boundary

Auto-start is a **new category** of platform concern. Every existing platform
trait is dependency inversion — core needs a capability, the platform provides
it. Auto-start is the inverse: an OS lifecycle mechanism that *wraps* the app.
And the "launch into the user session" dance is a Windows-specific solution to
a Windows-specific problem (session-0 isolation): macOS **LaunchAgents** and
Linux **systemd `--user`** units already run in the user's GUI session and
need no launcher at all.

So the OS-specific machinery must not leak into the app or core. Auto-start is
abstracted at the **goal** level — install / uninstall / status — behind a
`ServiceManager` trait in `crossover-platform`, with each OS providing its own
implementation (Windows: the SCM service + user-session launcher below;
macOS: a LaunchAgent plist; Linux: a `systemd --user` unit). The app's
`crossover service` command is platform-neutral; the composition root selects
the implementation, exactly as it does for the other platform traits.
Platforms without an implementation yet return `Unsupported`. The Windows
service + `CreateProcessAsUser` launcher is therefore an *internal detail* of
the Windows `ServiceManager`, not app-level code — keeping this feature as
portable as the rest of the boundary (architecture review, 2026-08-09).

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
- The product now ships **two executables**: `crossover.exe` (CLI + worker)
  and `crossover-svc.exe` (the service daemon). Packaging installs both; the
  service is registered to run `crossover-svc.exe`, which launches
  `crossover.exe run`. A second (small) binary to build and sign is the
  accepted cost of making the minimal-service invariant structural.
- The launcher/watchdog is a small, testable state machine (session state +
  child lifecycle); the privileged Win32 (`WTSQueryUserToken`,
  `DuplicateTokenEx`, `CreateProcessAsUser`, SCM/`SERVICE_CONTROL_*`) is
  isolated behind a platform boundary and exercised on Windows.
- The **minimal-service invariant** is a standing security constraint: the
  service must never gain untrusted input. A future review checks it holds.
- Implementation lands behind `crossover service install/uninstall/status`;
  the interactive `crossover run` is unchanged.

## Addendum: durable supervision logging (2026-08-19)

A two-machine soak turned up a diagnosability gap in the design above. The
worker on one machine died abruptly at 02:05:38 UTC (the peer saw a TCP RST)
and `crossover-svc` relaunched it at 02:05:39 — exactly the crash-recovery
behavior this ADR calls for — but nothing durable recorded *why*. The service
initialized `tracing` to stderr only, on the theory that "the SCM/event
pipeline captures it"; that is not true for a `LocalSystem` service with no
console, so the exit code `GetExitCodeProcess` had already read, and every
other supervision event, was written and immediately lost. Diagnosing the
incident took multiple manual forensics passes and remained inconclusive.

Fix (`crossover-svc/src/logging.rs`): a second sink, a daily-rotating file
under **`%ProgramData%\Crossover\logs`**, alongside the unchanged stderr sink.
This is deliberately *not* `~/.crossover/logs` (the worker's log location,
documented in [ARCHITECTURE.md](../ARCHITECTURE.md) §10 and
[SOAK.md](../SOAK.md)): the service runs as `LocalSystem`, so `~` there
resolves to the SYSTEM profile, a location no one checks. `%ProgramData%` is
fixed and machine-local regardless of which account context the service
happens to run under.

Every supervision transition the loop already drove is now a structured log
line: worker launched (session id), worker exited (exit code as decimal *and*
hex, plus whether the supervisor classified it as a crash — the headline gap
the incident exposed), a service-initiated stop and *why* (`StopReason` on
`WorkerAction::StopWorker` in `worker_supervisor.rs`: `ServiceStopping`,
`Logoff`, or `SessionChanged`, versus a real crash — kept in the pure,
unit-tested state machine, not the untestable daemon glue), launch failures,
backoff waits and the delay chosen, SCM stop/shutdown controls (with which
control fired), and session-change notifications with the probed result.
`crossover service install` and `crossover service status` now print the log
path so it is discoverable without reading this ADR.
