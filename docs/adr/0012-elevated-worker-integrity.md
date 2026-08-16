# 0012. The user-session worker runs at high integrity, launched with the user's elevated linked token

Status: Accepted
Date: 2026-08-11

Amends: [0011](0011-background-service-launcher.md) (its "runs as the user, not
SYSTEM — no privilege escalation of the input path" clause).

## Context

ADR 0011 has the LocalSystem service launch the worker as the logged-in user
via the token `WTSQueryUserToken` returns. For a UAC administrator that is the
**filtered** token, so the worker runs at **medium integrity**.

Windows **UIPI** makes medium integrity insufficient for Crossover's core job:

- A low-level input hook (`WH_MOUSE_LL` / `WH_KEYBOARD_LL`) owned by a
  medium-integrity process is **silently skipped while a higher-integrity
  (elevated) window holds the foreground**. The worker then captures nothing
  and forwards nothing.
- `SendInput` into a higher-integrity window is silently discarded (R-1), so
  even if control transferred, injection would not reach an elevated target.

A soak finding (2026-08-11) made this concrete and worse than "does not work":
with an elevated PowerShell focused on the controller, crossing to the peer
hid the local cursor (the machine believed it was now driving) while nothing
was captured or forwarded — and because the capture watchdog only fails closed
when raw-input events flow *while the hook is silent*, the same UIPI
restriction starved that evidence and the cursor was left **wedged hidden**
with no recovery. Elevated windows — an admin console, Task Manager, an
installer's window — are common, and "control silently stops working over
them, sometimes stuck" is unacceptable for a Synergy-like tool.

There is no way for a medium-integrity process to capture or inject over
elevated windows; the integrity wall is absolute and fixed at process
creation. So either the worker runs at high integrity, or it cannot drive
elevated windows at all.

## Decision

The service launches the worker with the user's **full (elevated) token** when
one exists, so the worker runs at **high integrity** in the user's session:

- After `WTSQueryUserToken` yields the console user's token, query
  `TokenElevationType`. When it is **`TokenElevationTypeLimited`** — a UAC
  administrator's filtered token — obtain the full token via
  `TokenLinkedToken` and use it as the source for `DuplicateTokenEx` /
  `CreateProcessAsUserW`. The worker's hooks then capture over elevated
  windows and its injections reach them.
- **`TokenElevationTypeDefault`** (standard user, UAC disabled, or the
  built-in Administrator — no split token) and **`TokenElevationTypeFull`**
  (already the full token) use the token as-is. For a standard user the
  feature is a **no-op**: the worker runs at its normal integrity, exactly as
  ADR 0011 described, and simply cannot drive elevated windows (unchanged from
  today).
- This applies **only** to the service-launched path. Interactive
  `crossover run` keeps whatever integrity the user started it with.

The worker still runs as the **user** — the user's own SID, profile, and
per-user DPAPI secrets — never SYSTEM. The **minimal-service invariant of ADR
0011 is untouched**: the service still handles no network, clipboard, or peer
input, and `crossover-svc` still links none of that code. The escalation is
strictly *user-medium → user-high*, never toward SYSTEM.

This deliberately revises ADR 0011's "no privilege escalation of the input
path": for an administrator, the input path now runs at high integrity. It is
recorded here as a change to a security-relevant decision, per the ADR
process.

## Alternatives Considered

- **Stay medium integrity; fail cleanly over elevated windows.** Detect a
  higher-integrity foreground, release control, keep the cursor visible, and
  notify. Rejected as the primary fix: it makes the wedge *safe* but leaves
  the tool unable to work over exactly the windows users most need it for
  (admin consoles). It remains available as a defensive safety net, but it is
  not the answer to "drive elevated windows."
- **Run the worker as SYSTEM.** Rejected: SYSTEM does not cleanly own the
  user's interactive desktop, profile, or DPAPI-scoped secrets, and is a
  larger escalation than the problem needs. High integrity *as the user* is
  the minimum that clears UIPI for elevated windows.
- **A configuration toggle to opt out of elevation.** Deferred: default-on
  matches the maintainer's decision to make control work over elevated
  windows. A toggle for a deployment that wants the medium-integrity posture
  can be added later without changing this decision.

## Consequences

- **Easier:** control works over elevated windows; the hidden-cursor wedge
  over an elevated foreground is gone.
- **Risk accepted:** the network-facing worker now runs with **admin rights**
  for an administrator user, so compromising Crossover's network handling
  yields **high integrity (local admin)**, not medium. The containment is
  unchanged and still load-bearing: bounded, validated parsing (NFR-1), the
  authorization gates that keep input/clipboard handlers unreachable until a
  session is `ESTABLISHED` (T8/T9), and the fact that the peer is already
  trusted through pairing. The escalation is gated behind *install* (which
  already requires admin) and a *trusted-peer session*. Recorded as **T11** in
  [SECURITY.md](../SECURITY.md).
- **Not a breach of the 0011 invariant:** SYSTEM remains unreachable from the
  network; the service is still a pure launcher/watchdog. Only the worker's
  integrity as the user changes.
- **Secure desktops still handled elsewhere:** the UAC prompt itself, the lock
  screen, and Ctrl-Alt-Del run *above* high integrity and remain covered by
  the feature/87 give-up (`can_inject` via `OpenInputDesktop`). Elevation does
  not — and must not — defeat those; the worker gives up control there rather
  than pretending to drive a desktop it cannot reach.
- **Thin, testable addition:** the token selection is a small Win32 addition in
  the daemon launcher. The pure decision — which elevation type has a usable
  full linked token — is unit-tested; the `GetTokenInformation` /
  `TokenLinkedToken` calls stay behind the Windows platform boundary and are
  validated on hardware.
