# 0009. Seamless control transfer: cross a screen edge, control follows

Status: Accepted — topology superseded by [0018](0018-drawn-display-topology.md) (2026-08-20)
Date: 2026-08-09

What 0018 replaces is the topology only: the "one linked edge pair,
left–right" side model, the 2026-08-09 refinement's "edge monitor =
outermost in the linked direction", and desktop-bounds-decides-the-edge —
all superseded by a drawn layout in one shared coordinate space. The
crossing *mechanism* below is retained: the edge as a trigger on the
negotiated engine, local symmetric detection on the real cursor, the
crossing position as a fraction, reclaim to neutral, the Schmitt-trigger
re-arm (now per-span), the generation-stamped mode, and the cursor mask all
stand as written and are restated in 0018.

## Context

Phase 3 and 4 delivered *explicit* control transfer: a console command
(`c` / `r`) drives a negotiated `LOCAL → REQUESTING → REMOTE` state
machine ([ARCHITECTURE.md](../ARCHITECTURE.md) §5.1) with session-scoped
complete mediation and a timeout/disconnect fallback that returns to
`LOCAL` and issues `ReleaseAllInput`. Phase 5's goal is for the two
machines to feel like neighboring monitors: the cursor crosses a screen
edge and pointer *and* keyboard control follows automatically, then
returns when the cursor crosses back.

The requirements that decide this:

- **Exactly one active input destination at all times** (FR-5.1), and the
  transfer must **converge to exactly one owner under packet delay and
  loss** (Phase 5 exit criterion). This is already the property the
  negotiated engine guarantees; edge crossing must not weaken it.
- **The two machines differ in resolution and, per-monitor, in DPI**
  (R-3). A pixel coordinate on one is meaningless on the other, so a
  crossing position cannot travel as pixels.
- **The local user must never lose their own machine.** A mistaken or
  lost transfer has to fail back to `LOCAL`, and the both-Control escape
  (ADR 0008) and console commands must still work.
- The capture layer (ADR 0007) already **pins the cursor at the screen
  edge** while capturing and forwards **unaccelerated, unclamped** Raw
  Input deltas — ADR 0007 explicitly noted this leaves edge detection to
  Phase 5, "where clamping would be fatal." Injection can place the
  cursor absolutely (`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`,
  DPI-aware) as well as move it relatively.
- **Keyboard already follows the pointer**: one grant covers both
  (Phase 4), so nothing extra is needed to move keyboard ownership.

## Decision

The edge crossing is a **new trigger on the existing engine**, not a new
engine. Everything security-relevant — the negotiated handshake, the
session-scoped mediation, the fail-closed fallback — is reused unchanged.

### Topology: one linked edge pair, left–right (this phase)

Each machine is configured, by a CLI flag, as the **left** or the
**right** member of the pair. The left machine's **right** edge links to
the right machine's **left** edge; that linked edge is the only one that
triggers a transfer, and every other edge is inert. A machine with
several monitors uses its **primary display's** geometry this phase.

The topology model is written so more edges and arrangements (over/under,
multi-monitor) can be added later without a wire change — but they are
out of scope here, matching the `A | B` side-by-side target.

### Trigger: immediate on reaching the linked edge

The instant the local cursor reaches the linked edge, the transfer fires
— no dwell, no push-through. The cursor arriving at the edge *is* the
intent; accidental brushes are covered by the console override and the
escape, and the decision is cheap to revisit if the soak shows it is
annoying.

### Detection is local, symmetric, and on the real cursor

Each machine watches **its own real cursor** against **its own enumerated
monitor geometry** — never a virtual model of the peer's cursor:

- While a machine **controls itself** (`LOCAL`) and its cursor reaches the
  linked edge, it **requests control of the peer** (it is *leaving*).
- While a machine is **controlled by the peer** and its cursor — driven
  by injected motion — reaches the linked edge, it **initiates return**
  (it is *reclaiming*).

Both cases are "my real cursor reached my linked edge"; only the control
state decides what it means. Using the machine whose cursor is actually
moving keeps detection exact.

### Reversing direction is done by reclaiming to neutral, not re-crossing

The linked edge does double duty — *leave* while local, *return* while
controlled — and immediately after a transfer the cursor rests **on** that
edge. Reversing the control direction there (be controlled, then take the
peer instead) means the very next crossing is ambiguous, and a rising-edge
detector that primes on the resting cursor swallows it: the first push does
nothing, the second works. Trying to disambiguate by cursor position alone
failed both ways in soak — placing the entry cursor a few pixels inside the
edge turned into a hair-trigger that bounced the forward crossing, and a
short dwell before firing added palpable latency and a path-repeat glitch.

The resolution keeps the instant, position-only crossing and disambiguates
by a *different* signal: **genuine local input on the controlled machine
reclaims control to neutral.** When the user touches the controlled
machine's own mouse or keyboard, the user is there — so that machine gives
up the peer's grant exactly like the secure-desktop give-up (drains what it
holds, tells the peer to release, which returns the peer to local) and
comes to rest in **neutral**: neither machine controls the other. From
neutral the cursor is free in the machine's interior, so the next edge
crossing is an ordinary, unambiguous rising edge — reversing direction is
just "touch this machine, then cross the edge," with no dwell and no
re-cross.

"Genuine local input" is distinguished from the peer's own injected driving
by the system input tick (`GetLastInputInfo` on Windows), **re-baselined
after every injection the controlled machine makes**, so injected motion
does not read as the user's. The common case — the user walks over while
the peer sits idle — is caught cleanly; simultaneous driving-and-touching
can let the peer's next injection re-baseline past a local event, but that
contention is not the reversal case and resolves the moment the peer
pauses. A platform without the tick query simply does not offer the
reclaim.

### Crossing position travels as a fraction, not pixels

The point along the edge where the cursor crosses is a normalized
fraction in `[0, 1]` of the edge length (the screen height, for a
vertical left/right edge). The destination multiplies that fraction by
**its own** edge length to place the cursor on entry. No pixels cross the
wire and there is no shared coordinate space, so differing resolution and
DPI are handled by construction. The mapping is identical in both
directions.

### Entry places the cursor absolutely, then relative motion resumes

Because streaming injection is relative (ADR 0007), the destination does
a **one-time absolute placement** of the cursor at the mapped entry
fraction the moment control arrives, and relative deltas continue from
there. This adds a "place the cursor at a normalized position" capability
to the injector trait, on top of the relative motion it already injects.

### The transfer reuses the negotiated engine and its messages

- The **forward** crossing drives the existing `ControlRequest →
  ControlResponse(ready) → REMOTE` handshake; the request carries the
  entry fraction so the peer can place the cursor.
- The **reverse** crossing, detected on the controlled side, initiates
  the return, carrying its own exit fraction so the origin's cursor
  reappears at the matching height. The outbound machine gains the
  **`RETURNING`** state between `REMOTE` and `LOCAL` (ARCHITECTURE §5.1).
- The protocol gains a normalized crossing position on the transfer
  messages and a reverse-edge return signal from the controlled side.
  Both are additive and versioned with the existing protocol.

### Exactly one owner, always

The switch to `REMOTE` happens **only after the peer confirms ready**, so
control is never in two places at once. In any transitional state
(`REQUESTING`, `RETURNING`) a timeout or disconnect falls back to `LOCAL`
and issues `ReleaseAllInput` on the remote — a lost acknowledgement
strands neither both nor neither. The console commands and the
both-Control escape remain as manual override and the local user's
guaranteed way back.

### Keyboard follows for free

The grant already covers keyboard and pointer, so an edge transfer moves
both with no separate keyboard-ownership message.

## Alternatives Considered

- **Source-side virtual cursor for return detection** — the controlling
  machine tracks where the peer's cursor "should" be from the deltas it
  forwards, and declares the reverse crossing itself. Rejected: that
  virtual position drifts from the destination's real cursor (edge
  clamping, the destination's own pointer ballistics, dropped or
  coalesced motion), so the return would fire at the wrong moment or not
  at all. Detecting on the machine whose cursor is real is exact.
- **Absolute pixel coordinates on the wire.** Rejected: the machines
  differ in resolution and DPI, so a pixel position means nothing across
  them. A fraction needs no shared coordinate space and is DPI- and
  resolution-independent by construction.
- **A separate seamless path parallel to the explicit engine.** Rejected:
  it would duplicate the negotiated handshake, the session-scoped
  mediation, and the fail-closed fallback — exactly the parts that
  already guarantee one owner. The edge is a trigger, not a machine.
- **Push-through or dwell to cross.** Deferred: requiring sustained travel
  past the pin reduces accidental crossings but adds state and latency to
  every transfer. With the cursor pinned at the edge and the escape and
  console available, immediate crossing is simpler and more seamless;
  revisit if the soak shows accidental crossings are a real annoyance.
- **Full multi-monitor geometry now.** Deferred: pixel-accurate edges
  across arbitrary multi-display desktops are materially more complex and
  unnecessary for the two-machine side-by-side target. The proportional
  single-edge model extends to it later without a protocol change.

## Consequences

- **Easier:** seamless switching inherits the proven, security-reviewed
  control engine, so exactly-one-owner and fail-closed fallback come for
  free; resolution and DPI differences are handled by the fraction; the
  keyboard follows with no extra work.
- **New work:** monitor enumeration behind a platform trait; a local
  cursor-position monitor that runs in *both* control states (leave and
  return detection); an absolute cursor-placement capability on the
  injector; a small protocol extension carrying the crossing fraction and
  the reverse-edge return; and the `RETURNING` state.
- **Risks accepted:** immediate crossing can fire on an accidental brush
  against the linked edge (mitigated by the escape and console override,
  revisited in the soak); the single left–right edge does not yet serve
  over/under arrangements; and a multi-monitor machine uses only its
  primary display this phase.
- **Protocol:** the transfer messages carry a normalized position and a
  reverse-edge return is added. These are additive changes, negotiated
  under the existing version rules; a peer that does not understand them
  simply never triggers a seamless crossing and the explicit path still
  works.

## Refinement (2026-08-09, Phase 5 soak)

The two-machine soak showed the "primary display, this phase" scope above
was not merely a limitation but a defect on a multi-monitor machine: with
geometry taken from the primary monitor, the cursor roaming onto a second
monitor sat permanently past the primary's edge, so the seam *between*
monitors fired constant spurious transfers (a grant/revoke loop). The
geometry now uses the whole **virtual desktop** — every monitor as one
rectangle, the cursor normalized to its top-left — so the crossing edge is
the outer edge of the desktop, not an internal seam. The decision above
(edge as trigger, position as a fraction, immediate crossing) is
unchanged; only "which region" is. The process is also made per-monitor
DPI aware (R-3), which the design had assumed but had never set, so
coordinates are real pixels across mixed-DPI monitors.

A second soak exposed a follow-on defect from the virtual-desktop scope:
the desktop bounding box of two *different-resolution* monitors has dead
space (a 3840×2160 external beside a 3840×2400 laptop panel leaves a
240-pixel gap on one), and mapping the crossing fraction against the box's
full height put the peer's cursor at the wrong row. The fraction is now
taken against the specific monitor on the linked edge — the outermost one
in the linked direction, from an added `DisplayInfo::monitors()`
(`EnumDisplayMonitors` on Windows) — not the bounding box. The desktop
bounds still decide *whether* a monitor seam is the crossing edge (above);
the per-monitor rectangle decides *where along it* the crossing lands, so
mismatched-resolution pairs map exactly. The wire fraction and every
decision above are unchanged.

### One visible cursor, on the active machine

There must be exactly **one** visible cursor — on the machine the user is
working on — never two, and never none. A new `CursorMask` trait hides the
local cursor whenever the user is *not* here and shows it when they are. The
driver derives that from the control-state transition, not a single action,
because "the user is here" is broader than "I am controlling":

- **Driving the peer** (`is_controlling`): the user is on the far machine →
  hide here.
- **Being driven** (`is_controlled`): the user is here → show.
- **Local, but the cursor just returned across this machine's edge** (it was
  controlled and a return revoked that grant): the user has *left* → hide,
  even though control reverted to plain local. This is the case the first
  cut missed — it hid only the controller, so after a return the just-
  controlled machine kept a second cursor visible at its edge.
- **Local otherwise** (returned from controlling, disconnect, startup): the
  user is here → show.

Visibility is **sticky**: it changes on a genuine active-machine transition,
never on the input events between transitions, so a machine hidden by a
return stays hidden until the user returns. A failure to hide or show is
logged, never a reason to disturb control, and the cursor can never be left
hidden — any exit from control that is not a deliberate cross-away shows it
(the mirror of the stuck-key invariant).

The platform mask's Win32 calls (`SetSystemCursor`, `SystemParametersInfo`)
can block, so they are **not** run on the control loop — that stalled event
processing and made the cursor lag reality during quick back-and-forth. The
loop only publishes the latest desired visibility to a `watch` channel; a
separate task coalesces to it and applies it on a blocking thread, so a
burst of crossings converges to the correct final cursor without a backlog.

**Two safety nets guarantee the cursor is never lost.** First, a blanked
system cursor persists past process death, so the binary restores the
defaults *synchronously* on every exit, and again when a mask is created —
so a quit, a lost connection, or even a crash never strands the machine
cursor-less. Second, a **local-input fail-safe**: while the cursor is hidden
and this machine is *not* driving the peer, the driver watches the
last-local-input tick (`GetLastInputInfo`); any change means the user has
touched *this* machine, so the cursor is shown again — recovering from any
state-machine confusion that hid it, without the user needing to know the
escape gesture. It deliberately does not fire while driving the peer, where
local input is *meant* for the far machine and the escape chord is the way
back.

The Windows implementation blanks the **system cursors** (`SetSystemCursor`
with a transparent cursor for each standard type), restoring the defaults
when control ends. A transparent top-most overlay window was tried first and
rejected: it hid the pointer on a single monitor but not on a machine with
**two monitors of different size and DPI** — one window spanning the
mismatched virtual desktop, and a frozen cursor that needs warping to fire
`WM_SETCURSOR`, which then perturbs capture. The Phase 5 soak demonstrated
exactly that asymmetry. `ShowCursor(FALSE)` was never viable: it affects only
the calling thread's own windows.

`SetSystemCursor` is geometry-, monitor-, and DPI-independent — it swaps the
cursor image, touches no window, and never moves the pointer (removing a
source of capture interference the warp introduced). Its cost is that a
blanked system cursor does not revert on process death; that is mitigated by
restoring the defaults on every exit from control and on shutdown, **and** by
restoring them when a mask is created, so the next launch of Crossover
self-heals a crash's blanking. Masking is a display nicety: a failure to hide
or restore is logged and never disturbs control.

## Addendum (2026-08-19): a re-arm margin on the crossing trigger

The risk this ADR accepted — "immediate crossing can fire on an accidental
brush against the linked edge ... revisited in the soak" — materialized on
hardware, and worse than an occasional stray transfer: it *oscillated*.

Crossing onto the controlled machine places its cursor **exactly on** the
linked column (the deliberate entry placement above), and that same column
means *return* while the peer is in control. The detector's re-arm condition
was a single observation one pixel off the column, sampled at 125 Hz — so a
two-pixel wobble at the seam, over 16 ms, read as a fresh arrival and fired a
complete reverse transfer. Each reversal re-parked *both* cursors on their own
trigger columns, leaving both machines primed to do it again: ten take/revoke
cycles in five seconds, with periods down to ~150 ms. It is a self-sustaining
loop, not a brush.

**The trigger is now a Schmitt trigger.** A touch of the linked edge fires
only while the detector is *armed*, and only travel more than
`REARM_MARGIN` pixels (24) back inside the screen re-arms it. Entry
placement, and priming when detection restarts, leave the detector disarmed.

This is deliberately **neither** of the two mitigations this ADR rejected:

- **Not the entry inset.** The cursor still enters *on* the edge column, so
  cursor continuity across the seam is unchanged — placing it a few pixels
  inside was rejected above for making the forward crossing a hair-trigger,
  and it stays rejected. The hysteresis lives in the *detector's* state, not
  in where the pointer is put.
- **Not a dwell.** Nothing waits and nothing is timed, so a deliberate
  crossing gains exactly zero latency: a cursor travelling toward the edge is
  far more than 24 px clear of it on the way, so it is armed, and the first
  observation that reaches the column fires as before. Only a cursor that
  never left the edge's neighbourhood is inert.

The margin also closes a race the immediate trigger left open: the entry
placement runs inline on the control loop while the detector primes on its
own task after a channel hop, so injected motion could land in between and
arm the detector before it was primed. A few pixels of injected motion are
nowhere near the margin, so they no longer can.

**Crossings are also generation-stamped.** A crossing carries a `kind`
(leave/return) frozen at detection time and reaches the control loop through
a bounded queue. If the control state changed on the way, acting on it
applies a decision about a state that no longer exists — a stale `Return` can
revoke a *fresh* grant. Every edge-mode publication carries a generation; the
detector stamps it onto the crossings it emits under that mode, and the
control driver drops any crossing whose stamp is not the one it last
published. That is a correctness fix independent of the margin, and it stays
correct however the polling and queueing are later retimed.

**The mode itself is a level, on a `watch`.** It says what this machine is
watching for *right now*, so latest-wins is the correct semantics and
publishing must never block. It used to ride a bounded `mpsc` that closed a
cycle back onto the control loop — mode → detector → crossings → control
events → the control loop, which is the only thing that drains them — so any
slowness there fed back into itself and cleared only in `MAX_DRAIN_BATCH`
bursts. That is why the generation is *carried inside the published value*
rather than counted at each end: counting was only ever correct over a
lossless FIFO, and a coalescing channel would drift the two counts apart on
the first collapsed burst. Carried, it cannot.

**A cursor placement re-primes the detector, whatever the mode did.** Entry
placement parks the pointer *on* the linked column, which is also the trigger
column; priming there is what stops it firing. A first grant re-primed for
free, because taking it changed the mode — but a **refreshed** grant (below)
does not change `is_controlled`, so nothing was published, and with the
trigger armed the refresh's own placement fired a return that revoked the
grant it had just re-issued. The re-prime is therefore tied to the
`PlaceCursor` that causes it: every placement republishes the mode, unchanged
value and all, under a new generation. The detector primes on the placed
cursor, and the new generation invalidates any crossing detected before the
refresh.

## Addendum (2026-08-19): late answers are self-correcting

"In any transitional state a timeout or disconnect falls back to `LOCAL`"
turned out to be only half a rule. Falling back is what the *requester*
does; it said nothing to the peer, and the peer's slow answer then arrived
into a world that had moved on. Correlated two-machine logs from a burst of
rapid grant/edge-revoke cycles caught the consequence: one answer arrived
4.7 s late and the two state machines locked each other out for seven
seconds.

The sequence, every step matched to code. B's crossing requested control and
timed out, so B reverted to local — silently. B crossed again and requested
a second time. Only then did A work through its inbound backlog: it granted
the *first* request, and answered the second with
`Denied(AlreadyControlled)` — denying the very session that held its grant.
B, meanwhile, received the grant it had stopped waiting for and released it.
The user saw a denial and could not cross until another push.

Three rules now make a late answer converge on its own:

- **A re-request from the grant holder refreshes the grant.** The holder
  only asks again because it believes it holds nothing, so a denial strands
  both machines. The refresh drains everything the old grant left held — a
  refreshed grant must no more inherit a latched key than a hand-back may
  leave one (FR-4.4) — and restarts the applied-input sequence, because the
  controller restarts its send sequence with every grant it is given.
  `AlreadyControlled` still denies a request from any *other* session: one
  peer drives this desktop at a time, and *which* peer is the security
  boundary (FR-2.3). Refreshing is security-neutral — same principal, same
  authenticated session, complete mediation unchanged.
- **A timed-out request cancels on the wire**, not just locally, so a grant
  in flight toward a requester that has given up does not stand with nobody
  believing they hold it.
- **The stray-grant undo yields to our own retry.** A late grant for an
  earlier request, arriving while a newer request to that same session is in
  flight, is left alone: releasing it would tear down the grant the newer
  request is being given. Every other stray grant is still undone, so no
  peer is ever left controlled by a driver that will never drive.

Each rule is idempotent with the others — a release from a session that
holds nothing, and that we do not control, is already a silent no-op — and
together they give the property the incident violated, now pinned by a
deterministic two-engine test that scripts the hardware timeline message by
message: **with no further user input, both engines converge on one belief
about who controls whom, and the requester ends able to cross.** Nothing
here waits on a clock or a heuristic; convergence follows from the message
order alone.

The head-of-line delay on the answering side that opened the window is a
separate matter, settled in
[ADR 0013](0013-interactive-over-bulk-prioritization.md)'s 2026-08-19
addendum: inbound frames are now routed by message type, so a control
request no longer waits on the clipboard driver's queue to discard it.

The `RETURNING` state this ADR promised above (between `REMOTE` and `LOCAL`)
was never built and is not needed: the reverse crossing is detected on the
controlled side, which revokes, so the controller returns to `LOCAL` on the
resulting release with no transitional state of its own. ARCHITECTURE §5.1's
diagram is corrected to the `LOCAL / REQUESTING / REMOTE` the code has always
had.
