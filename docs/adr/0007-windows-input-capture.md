# 0007. Windows input: low-level hooks to suppress, Raw Input for motion, SendInput to inject

Status: Accepted
Date: 2026-08-07

## Context

Phase 3 needs a Windows machine to (a) capture local pointer input while
the *peer* holds control, without that input also acting locally, and
(b) inject pointer input arriving from a peer. Phase 4 extends both to
the keyboard. The platform risks are already catalogued
([SPECIFICATION.md](../SPECIFICATION.md) §6): UIPI and the secure
desktop (R-1), the low-level hook timeout (R-2), per-monitor DPI (R-3),
and exclusive-input applications (R-6).

One requirement decides most of this: **while the peer has control,
local input must not act locally.** Moving the mouse to drive the far
machine cannot also drag windows on this one. That is suppression, and
not every capture API can do it.

## Decision

Three mechanisms, each chosen for what only it can do.

### Capture: low-level hooks (`WH_MOUSE_LL`, later `WH_KEYBOARD_LL`)

`SetWindowsHookEx` low-level hooks are the only documented user-mode
mechanism that can **swallow** an input event: returning a non-zero
value from the callback stops delivery to every other application. Raw
Input cannot do this at all — it observes, and the OS still dispatches
the event normally — which disqualifies it as the sole mechanism no
matter how much better its data is.

Consequences that are not optional:

- **The callback does near-zero work** (R-2). Windows silently removes a
  hook whose callback exceeds `LowLevelHooksTimeout` (300 ms by
  default), and the removal is *silent* — input simply stops being
  captured. The callback timestamps, decides suppress-or-pass, pushes to
  a bounded queue, and returns. No allocation beyond the queue, no
  locks held across the decision, no logging.
- **Hook loss must be detected and recovered**, not assumed away. A
  watchdog re-installs the hook if events stop arriving while control is
  held remotely, and the loss is logged (NFR-3).
- Hooks live on a dedicated thread owning a message pump — the same
  shape as the clipboard listener in `crossover-platform-windows`.

### Motion data: Raw Input (`RegisterRawInputDevices`, `WM_INPUT`)

Hook coordinates are the wrong data for driving a remote pointer, for
two reasons that are properties of the API rather than guesses:

1. They are **accelerated** — post-pointer-ballistics screen
   coordinates, so remote motion would be accelerated twice, once here
   and once on the destination.
2. They are **clamped to the local desktop**. Once the local cursor is
   pinned at a screen edge (which is exactly the state while the peer
   has control), further movement in that direction produces no
   coordinate change. The user keeps moving the mouse; the hook reports
   nothing.

Raw Input reports unaccelerated device deltas with no desktop clamping,
which is what a remote pointer actually needs. So the two run together:
**hooks decide what is suppressed, Raw Input supplies what is sent.**

### Injection: `SendInput`

`SendInput` is the standard user-mode injection path and the only one
that composes with the hook pipeline. Specifics:

- Absolute motion uses `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`
  with coordinates normalised across the virtual desktop, so
  multi-monitor destinations work without per-monitor arithmetic. The
  process is per-monitor DPI aware so that normalisation is correct on
  mixed-DPI systems (R-3).
- **Injected events are tagged.** `SendInput` accepts a `dwExtraInfo`
  value that surfaces in the hook callback, so a Crossover-specific
  signature identifies our own injections and prevents capturing them
  back. This is the same defect class as clipboard loop prevention
  (FR-3.3), solved the same way: mark what you emit, then recognise it.
  Relying on the `LLMHF_INJECTED` flag alone would be weaker — it says
  *some* process injected the event, not that we did.

### Failure modes we accept and surface

- **UIPI (R-1).** A non-elevated Crossover cannot inject into elevated
  windows, and `SendInput` reports success regardless. The injector
  therefore treats a target-window integrity check as diagnostic
  information and logs when injection is likely to have been swallowed.
  Running elevated is supported but not required, and is documented
  rather than silently recommended.
- **Secure desktop (R-1).** Hooks do not run on the UAC prompt or lock
  screen, and no user-mode mechanism changes that. Control transfer must
  therefore *fail closed*: if input stops being observed while the peer
  holds control, Crossover releases control and issues `ReleaseAllInput`
  rather than leaving the user with a dead mouse.
- **Exclusive-input applications (R-6).** Games using raw input in
  exclusive mode may not honour suppression. Out of scope for Phase 3;
  the limitation is documented.

## Alternatives Considered

- **Raw Input alone.** Better data, no timeout constraint, no hook to
  lose. Rejected outright: it cannot suppress, so local input would act
  on both machines simultaneously. Not a trade-off — a disqualification.
- **Low-level hooks alone**, warping the cursor to screen centre after
  each event to escape edge clamping. This is the classic Synergy-family
  approach and it works, but it fights pointer acceleration, generates
  synthetic motion the hook must then filter, and degrades on mixed-DPI
  multi-monitor desktops. Adding Raw Input is less code than doing this
  well.
- **A kernel filter driver** (Interception-style). Total control,
  including over exclusive-input applications. Rejected: driver signing,
  installation privilege, and an enormous increase in the trusted
  computing base for a tool whose security case rests on being small.
- **WinRT `InputInjector`.** Injection only, no capture, and narrower
  than `SendInput`. No reason to prefer it.

## Consequences

- Easier: suppression is correct by construction; remote motion is
  unaccelerated and unclamped, so it will not need re-engineering when
  Phase 5 adds edge detection where clamping would be fatal.
- Harder: two capture mechanisms to keep coherent, both inside
  `crossover-platform-windows` behind `InputCapture`. The hook's
  near-zero-work constraint is a real design pressure on everything it
  touches, and the watchdog is code that exists solely because Windows
  fails silently.
- The `unsafe` surface in `crossover-platform-windows` grows
  substantially — hook callbacks, Raw Input buffers, `SendInput` arrays.
  The existing discipline applies unchanged: every block carries a
  SAFETY comment, `undocumented_unsafe_blocks` stays denied, and the
  platform tests exercise the real APIs.
- Hook installation requires a message pump thread per process, already
  precedented by the clipboard listener; the two threads stay separate
  so a stall in one cannot starve the other.
