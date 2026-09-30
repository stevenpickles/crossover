# 0022. macOS unattended operation: a per-user LaunchAgent, no launcher

Status: Proposed
Date: 2026-09-29

## Context

Phase 9.1's seventh slice brings the Phase 6 property to the Mac: after a
reboot and a login, Crossover is running and working with nothing for the
user to do ([ROADMAP.md](../ROADMAP.md) Phase 9 exit criteria).

On Windows that took two ADRs. [ADR 0011](0011-background-service-launcher.md)
puts a `LocalSystem` service in charge of launching a worker into the
console user's session, because a Windows service cannot touch a user's
desktop; [ADR 0012](0012-elevated-worker-integrity.md) launches that worker
with the user's full token so it can drive elevated windows. Both are
answers to Windows' problem. The catalogue said so in advance
(platform-risks-macos.md M-9): read ADR 0011 "as the problem, not the
solution".

macOS's shape is different in the ways that matter:

- A **LaunchAgent** runs *in the user's session* with access to the
  window server — the pasteboard, event taps, the cursor. A
  **LaunchDaemon** runs as root with no session and could drive none of
  them.
- `launchd` already supervises: it starts a job at login, restarts it
  under stated conditions, and throttles restarts.
- There is no integrity split to cross. What gates input on macOS is the
  Accessibility grant (M-1), which is per binary, not per token.

The trait this implements, `ServiceManager`, is deliberately goal-level —
install, uninstall, status — so the CLI's `crossover service` commands stay
the same on every platform.

## Decision

**On macOS, `crossover service install` writes a per-user LaunchAgent that
runs `crossover run` directly. No launcher process, no daemon, and nothing
running as root.**

1. **The job.** `~/Library/LaunchAgents/com.crossover.agent.plist`, label
   `com.crossover.agent`, `ProgramArguments` = the installed `crossover`
   binary's absolute path and `run`, loaded into the user's GUI domain
   (`launchctl bootstrap gui/<uid>`), with `RunAtLoad` so it starts at
   login.
2. **Supervision mirrors ADR 0011's rules through launchd's own keys.**
   `KeepAlive` with `SuccessfulExit = false`: a crash is relaunched, a
   clean exit — the user quit it — is not, which is the Windows
   supervisor's "clean exit is intentional" rule. `ThrottleInterval`
   bounds the relaunch rate, as the Windows backoff does. What launchd
   cannot express — an exponential backoff that resets after a healthy
   run — is accepted as a difference, not re-implemented in a launcher.
3. **Configuration comes from `config.toml`**, as it does under the Windows
   service, so the plist carries no flags beyond `run` and never needs
   rewriting when settings change.
4. **Logs go where they go on Windows**: `StandardOutPath` and
   `StandardErrorPath` point into `~/.crossover/logs`, beside the rolling
   file log, so a worker that dies before its logger starts still leaves
   its panic somewhere (the Windows lesson recorded in ADR 0011's
   addendum).
5. **Uninstall** is `launchctl bootout gui/<uid>/com.crossover.agent` and
   removing the plist; **status** asks launchd whether the job is loaded
   and whether it has a running PID.
6. **Nothing before login.** A LaunchAgent starts when the user logs in,
   which is also when the Windows service starts a worker. Crossover with
   nobody logged in has no desktop to share on either platform.

## Alternatives Considered

**A LaunchDaemon plus a per-session launcher**, mirroring ADR 0011. Rejected:
the daemon could reach no session resource itself, and the launcher it
would need is exactly what a LaunchAgent already is. It would add a root
process to solve a problem macOS does not have.

**A login item** (`SMAppService.mainApp`, or the legacy login-items list).
Starts at login, but supervises nothing: a crash stays crashed until the
next login. Rejected for the Phase 6 property. It may still be the right
*registration* API for a signed, bundled release, which the packaging work
decides; the supervision rules above would carry over.

**Re-implementing the Windows supervisor's backoff inside a launcher.**
Exact parity, at the cost of a second long-lived process whose only job is
arithmetic launchd approximates. Rejected: the approximation is adequate,
and the pure supervisor stays available if hardware shows it is not.

## Consequences

**The macOS `ServiceManager` is mostly a plist writer and `launchctl`
calls**, which keeps it small and its unsafe surface near zero.

**Accessibility and Keychain grants attach to the installed binary** (M-1,
M-8). The agent must run a stable, signed binary at a stable path, or every
upgrade silently loses input. This is where ADR 0020's signing consequence
becomes a requirement rather than a note.

**To verify on the lab Mac before this is accepted:** that an agent started
at login can use the pasteboard and, once granted Accessibility, event
taps; how the M-11 pasteboard prompt presents for an agent with no
terminal; whether `KeepAlive` / `SuccessfulExit` behave as described across
logout and login (the soak's core case); and what `ThrottleInterval` a
crash loop actually sees.
