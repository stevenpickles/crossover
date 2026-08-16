# Known platform risks — macOS

Written **before** `crossover-platform-macos` exists, as
[ROADMAP.md](ROADMAP.md) Phase 8 requires. The point is to know what the
port will fight before deciding how to build it, in the same spirit as
[SPECIFICATION.md](SPECIFICATION.md) §6 for Windows.

**Every entry here is a hypothesis until it runs on a Mac.** This catalogue
is written from documented platform behaviour, not from measurement, and
several risks below are exactly the kind that behave differently in practice
than in the documentation. Each carries what to **verify** for that reason.
Anything that survives contact with hardware should be promoted into
SPECIFICATION.md; anything falsified should be struck with a note saying so.

The traits a port must satisfy are in `crossover-platform`:
`ClipboardProvider`, `InputCapture`, `InputInjector`, `DisplayInfo`,
`CursorMask`, `SecureStorage`, `ServiceManager`.

---

## M-1 Accessibility permission gates all input, and is tied to the binary

`CGEventTap` (capture) and `CGEventPost` (injection) both require the
process to be trusted for Accessibility. Untrusted, a tap is created
disabled or not at all, and posted events are dropped — **silently**, which
is the dangerous part.

The grant is made by a human in System Settings and cannot be requested
programmatically beyond raising the prompt (`AXIsProcessTrustedWithOptions`).
It is recorded against the binary's identity, so a **rebuilt or re-signed
binary is a different subject** and loses it.

- **Threatens:** `InputCapture`, `InputInjector` — i.e. every input feature.
- **Consequence:** a developer rebuilding after every change may have to
  re-grant each time; a user upgrading may silently lose input while the
  clipboard keeps working, which reads as "half the app broke".
- **Verify:** whether re-granting is needed per build for an unsigned
  binary; whether stable ad-hoc signing avoids it; what `AXIsProcessTrusted`
  reports in each state, and whether it is reliable enough to *detect* the
  condition and tell the user rather than failing mutely.
- **Design implication:** the port needs an explicit "input unavailable
  because permission is missing" state that reaches the user, in the same
  way NFR-3 demands failures be observable.

## M-2 Event taps are disabled on timeout, exactly like Windows hooks

A tap whose callback is too slow is disabled by the system, delivering
`kCGEventTapDisabledByTimeout` and then nothing. This is the macOS twin of
Windows R-2, and the same discipline applies: the callback enqueues and
returns, and the port must detect disablement and re-enable the tap.

- **Threatens:** `InputCapture`, including `is_capturing` honesty.
- **Verify:** the actual timeout, and whether re-enabling in the callback is
  sufficient or the tap must be recreated.
- **Reuse:** the Windows capture already treats hook loss as a first-class
  event; the same shape should carry over rather than being reinvented.

## M-3 Secure input mode blinds keyboard capture

While a password field holds focus, applications may enable secure event
input, and event taps stop receiving keystrokes for its duration. This is
the analogue of Windows R-1's secure desktop: not a bug to fix but a
boundary to document and detect.

- **Threatens:** `InputCapture` for keyboard.
- **Consequence:** typing into a password field on the controlling Mac would
  not reach the peer. Silence here is indistinguishable from a broken
  session unless the port notices.
- **Verify:** whether secure input state is observable
  (`IsSecureEventInputEnabled`) reliably enough to report it, and whether
  mouse events continue while keyboard is suppressed.

## M-4 The pasteboard has no change notification

`NSPasteboard` exposes a monotonically increasing `changeCount` but no
event when it moves. `ClipboardProvider::set_change_listener` therefore has
no native implementation on macOS — the port must **poll**.

- **Threatens:** `ClipboardProvider::set_change_listener`, and through it
  ADR 0006's trigger-driven transmission and the settle window.
- **Consequence:** a poll interval is a latency/CPU trade the Windows port
  never had to make, and it interacts with the settle window rather than
  composing with it.
- **Verify:** whether polling `changeCount` is cheap enough to run at a
  human-scale interval, and how it behaves while another application holds
  the pasteboard mid-write.
- **Note:** loop prevention (FR-3.3) currently leans on recognising our own
  writes by content hash; polling does not change that, but it does mean a
  write and its own observation are separated by up to one interval.

## M-5 Image format: CF_DIB does not travel

ADR 0014 carries images in the **source clipboard's own raster format**,
verbatim. On Windows that is `CF_DIB`. macOS pasteboards deal in
`public.tiff` and `public.png`; nothing there understands a DIB, and
`ContentType::Image(ImageFormat::Dib)` arriving at a Mac is unrenderable.

This is the one risk in this document that may reach **beyond the platform
crate**, which is precisely what Phase 8 says should not happen ("new
implementations of the clipboard trait, not new protocol design").

- **Threatens:** cross-platform image interop; possibly `ContentType`.
- **Options, none yet chosen:** carry PNG as the interchange format when
  either peer is non-Windows (the protocol already has a PNG variant);
  convert at the receiving edge; or negotiate a format per session.
  Converting contradicts "verbatim"; negotiating adds protocol surface.
- **Decide before implementing**, and if the answer changes the wire, it
  needs an ADR — not a quiet change inside a platform crate.

## M-6 Cursor hiding may be application-scoped

ADR 0009 masks the local cursor while the peer is being driven.
`NSCursor.hide()` is documented as affecting the calling application's
cursor while it is active; `CGDisplayHideCursor` is display-wide but has
historically been tied to the calling process's state.

- **Threatens:** `CursorMask`.
- **Consequence:** a background agent with no active window may be unable to
  hide the cursor at all, which weakens the seamless illusion without
  breaking correctness.
- **Verify:** whether either call works from a `LaunchAgent` with no
  foreground window, and whether the hide survives focus changes.
- **Acceptable fallback:** `CursorMask` already has a no-op implementation
  and `--no-cursor-mask` exists as a diagnostic, so degrading here is
  survivable and should be stated rather than hidden.

## M-7 Coordinates: points, backing scale, and a flipped axis

macOS reports display geometry in *points* with a per-display backing scale
factor, and `NSScreen` uses a bottom-left origin while Core Graphics event
coordinates use top-left. Crossover's edge crossing maps a position as a
fraction of an edge (ADR 0009), which helps, but the port still has to get
the axis and the scale right.

- **Threatens:** `DisplayInfo`, edge detection, injected pointer positions.
- **Verify:** on a mixed-scale multi-display Mac specifically — this is the
  macOS equivalent of Windows R-3, and mixed-DPI is where the Windows port
  found its bugs.

## M-8 Keychain access is also tied to the binary

`SecureStorage` maps to a generic-password Keychain item. Access control
lists are bound to the application, so a rebuilt binary can be treated as a
different application and prompt, or be denied.

- **Threatens:** `SecureStorage`, and therefore device identity and the
  trust store (ADR 0003).
- **Consequence:** worst case, an upgraded build cannot read its own
  identity and appears to be a new device to its peer — which would look
  like a trust failure rather than a storage one.
- **Verify:** behaviour across a rebuild, and whether the login keychain is
  reliably unlocked in the context the agent runs in.

## M-9 launchd has no LocalSystem, and no elevated-worker story

ADR 0011's model — a privileged launcher starting a worker in the user's
session — is Windows-shaped. On macOS a **LaunchAgent** runs in the user's
session with GUI access and needs no launcher, while a **LaunchDaemon** runs
as root with no session and could not drive input at all.

- **Threatens:** `ServiceManager`.
- **Consequence:** the port is probably *simpler* — an agent plist, no
  launcher, no elevated worker (ADR 0012 has no analogue) — but ADR 0011's
  reasoning is written in Windows terms and should be read as "the problem",
  not "the solution", when it is ported.
- **Verify:** whether an agent survives logout/login as the soak requires,
  and what happens before first login.

## M-10 Key codes are not HID usages

ADR 0008 puts USB HID usage codes on the wire deliberately, so two machines
cannot disagree about layout. macOS delivers `CGKeyCode` virtual keycodes,
which are neither HID usages nor Windows scan codes.

- **Threatens:** `InputCapture`, `InputInjector`, keyboard correctness.
- **Consequence:** a mapping table is required in both directions, and it is
  the kind of table that is 95% right and wrong in ways only a specific
  keyboard reveals.
- **Reuse:** the Windows port already maps scan codes to HID usages; the
  table's *shape* and its tests carry over even though its contents do not.

---

## What this catalogue does not cover

Screen recording permission (Crossover captures no screen content),
notarization and Gatekeeper (distribution, not function), and anything
specific to Apple Silicon versus Intel — none of the above is expected to
differ by architecture, which is itself worth confirming once.
