# Known platform risks — Linux

Written **before** `crossover-platform-linux` exists, as
[ROADMAP.md](ROADMAP.md) Phase 8 requires, in the same spirit as
[SPECIFICATION.md](SPECIFICATION.md) §6 for Windows.

**Every entry here is a hypothesis until it runs on real desktops**, and
Linux makes that caveat heavier than macOS does: behaviour depends on the
display server, the compositor, the desktop environment, and the
distribution's policy, so "it worked on mine" proves less here than
anywhere else. Each risk carries what to **verify**, and on *which*
configuration.

The traits a port must satisfy are in `crossover-platform`:
`ClipboardProvider`, `InputCapture`, `InputInjector`, `DisplayInfo`,
`CursorMask`, `SecureStorage`, `ServiceManager`.

---

## L-1 Wayland may make global input capture impossible, by design

This is the risk that decides the shape of the whole port, so it is first.

Under **X11**, a client can capture and inject globally: `XTEST` for
injection, `XInput2`/`XRecord` for capture. Under **Wayland**, that is
deliberately prohibited — a client cannot see or synthesise input outside
its own surfaces. The sanctioned routes are compositor-mediated
(`xdg-desktop-portal`, the RemoteDesktop and InputCapture interfaces,
`libei`), and support varies by compositor and version.

- **Threatens:** `InputCapture`, `InputInjector` — the entire input half of
  the product.
- **Consequence, stated plainly:** a first Linux port may have to be **X11
  only**, with Wayland behind a documented limitation. That is a legitimate
  outcome, but it should be a decision with a reason attached, not something
  discovered halfway through.
- **Verify first, before any other Linux work:** on current GNOME and KDE
  Wayland sessions, whether the portal route can (a) capture input globally,
  (b) inject it, and (c) do both without a per-session user consent dialog
  that would break unattended operation (the Phase 6 property).
- **Note:** an XWayland fallback does not rescue this. XWayland isolates X
  clients from native Wayland surfaces, so capture there sees a subset of
  the desktop — which would be worse than an honest refusal.

## L-2 The X11 clipboard is an ownership protocol, not a buffer

X11 has no clipboard storage. The **owner** of a selection holds the data
and serves it on request. Several consequences follow that Windows and macOS
do not have:

- **Content dies with the process.** Copy in Crossover, quit Crossover, and
  the clipboard is empty unless a clipboard manager took ownership. The
  Windows port's mental model — write and forget — does not hold.
- **Reads are a round trip** with another process, which can be slow or
  hung. `ClipboardProvider::read` therefore needs a timeout and a `Busy`-
  shaped failure, which the trait already has (FR-3.4's bounded retry).
- **Large content needs the `INCR` protocol**, transferring in chunks
  because a single X request cannot carry an image. ADR 0014's 64 MiB
  ceiling means this is not optional.
- **Two selections exist.** `CLIPBOARD` is explicit copy; `PRIMARY` is
  select-to-copy and changes on *every text selection*, which as a sync
  source would be a firehose and a privacy problem.
- **Decide and record:** `CLIPBOARD` only. Syncing `PRIMARY` would transmit
  text a user merely highlighted, which no other platform does and no user
  expects.
- **Change notification** does exist — `XFixesSelectionNotify` — so
  `set_change_listener` need not poll under X11. Verify it fires for
  ownership changes by other clients, not only our own.

Under Wayland the equivalent is `wl_data_device`, or `zwlr_data_control` on
wlroots compositors for clipboard-manager-style access. GNOME does not
implement the wlroots protocol, so the clipboard has the same
per-compositor fragmentation as input.

## L-3 Injection privileges differ by route

`XTEST` needs no special privilege beyond an X connection. `uinput` — the
route that also works under Wayland — is a device node, typically
`root`-owned, needing a udev rule or group membership.

- **Threatens:** `InputInjector`, and the installation story.
- **Consequence:** "install and it works" may become "install, add a udev
  rule, log out and back in", which is a materially worse first run and must
  be documented rather than discovered.
- **Verify:** whether a `uinput`-based injector can be made to work for an
  ordinary desktop user with a shipped udev rule, and what that rule grants
  in security terms — a writable `uinput` is the ability to synthesise input
  system-wide, which belongs in [SECURITY.md](SECURITY.md)'s threat model
  before it ships.

## L-4 Secret storage assumes a running keyring

`SecureStorage` maps to the Secret Service API over D-Bus, implemented by
`gnome-keyring` or KWallet. Both assume a session bus and an unlocked
keyring, and neither is guaranteed: minimal window managers may have no
keyring at all, and a headless or auto-login machine may have one that is
never unlocked.

- **Threatens:** `SecureStorage`, and therefore device identity and the
  trust store (ADR 0003).
- **Consequence:** Crossover cannot start at all if identity cannot be
  loaded, which turns a missing keyring into a total failure rather than a
  degraded one.
- **Decide:** whether a fallback exists (an encrypted file keyed by what?)
  or whether "no keyring, no Crossover" is the supported answer. A fallback
  weakens the at-rest story and therefore needs an ADR, not an
  implementation decision.
- **Verify:** behaviour under GNOME, KDE, and a bare window manager with no
  keyring daemon, including whether the unlock prompt appears in an
  unattended session.

## L-5 Unattended operation needs lingering, not a launcher

ADR 0011's LocalSystem launcher exists because a Windows service cannot
touch the user's session. systemd's model is different: a **user unit**
(`systemctl --user`) runs in the session already, and unattended start
before login requires `loginctl enable-linger`.

- **Threatens:** `ServiceManager`, and the Phase 6 unattended property.
- **Consequence:** probably simpler than Windows — no launcher, no elevated
  worker (ADR 0012 has no analogue) — but "runs before anyone logs in" and
  "can drive the graphical session" are in tension on Linux exactly as they
  are on Windows, and lingering does not by itself give a unit access to the
  display server.
- **Verify:** whether a lingering user unit can reach the display server and
  input devices before an interactive login, and what happens across
  logout/login, which is the soak's core case.

## L-6 Display geometry is queryable under X11 and awkward under Wayland

`DisplayInfo` needs the desktop bounds, per-monitor rectangles, and the
cursor position. X11 answers all three (RandR, `XQueryPointer`). Wayland
deliberately does not give clients a global coordinate space or the pointer
position outside their surfaces.

- **Threatens:** `DisplayInfo`, and with it ADR 0009's edge crossing, which
  is defined in desktop coordinates.
- **Consequence:** under Wayland, seamless edge transfer may be
  unimplementable by the same mechanism even if input capture is solved.
- **Verify:** what the portal route exposes about geometry, and whether
  fractional scaling reports a coordinate space consistent with the one
  injected events land in.

## L-7 There may be no global cursor hide

ADR 0009 masks the local cursor while driving the peer. Under X11,
`XFixesHideCursor` is documented as hiding the cursor for the display, which
would suit; the widely-used alternative is an invisible-cursor override on
one's own window, which would not.

- **Threatens:** `CursorMask`.
- **Verify:** whether `XFixesHideCursor` genuinely hides the cursor
  system-wide from a client with no visible window, and what Wayland offers
  (likely nothing outside a compositor's own policy).
- **Acceptable fallback:** `CursorMask` has a no-op implementation and
  `--no-cursor-mask` exists, so degrading is survivable if stated.

## L-8 Key codes are neither HID usages nor X keycodes

ADR 0008 puts USB HID usages on the wire. X11 delivers keycodes offset from
the kernel's evdev codes (historically +8), and `uinput` speaks evdev codes
directly — so a port may need **two** mappings depending on the route it
takes for capture and injection.

- **Threatens:** keyboard correctness in both directions.
- **Reuse:** the Windows scan-code mapping's shape and its property tests
  carry over; the contents do not.
- **Verify:** on a non-US layout specifically, which is where "mostly
  correct" mappings reveal themselves.

## L-9 Image formats favour PNG

X11 clipboard images are advertised as MIME targets, conventionally
`image/png`. This is the same question [M-5](platform-risks-macos.md) raises
from the macOS side, and the two agree: **PNG is the plausible
cross-platform interchange format**, and `CF_DIB` is the outlier.

- **Threatens:** cross-platform image interop.
- **Decide once, for both platforms**, and if the answer changes the wire it
  needs an ADR rather than a platform-crate decision.

---

## The order this suggests

L-1 is not one risk among nine; it decides whether the others matter. A
Wayland session that cannot capture input makes L-6 and L-7 moot and turns
the port into "X11 today, Wayland when the portals are ready".

**Verify L-1 before writing any Linux code**, on current GNOME and KDE. The
answer determines whether Phase 8's Linux half is a port or a research
project, and that is worth knowing before a crate exists.
