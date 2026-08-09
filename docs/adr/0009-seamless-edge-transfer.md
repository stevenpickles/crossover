# 0009. Seamless control transfer: cross a screen edge, control follows

Status: Accepted
Date: 2026-08-09

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
