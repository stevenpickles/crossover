# 0018. Display topology is a drawn layout in one shared coordinate space

Status: Accepted
Date: 2026-08-20
Supersedes: [0009](0009-seamless-edge-transfer.md)'s **topology** — the "one
linked edge pair, left–right" side model, its 2026-08-09 refinement ("the edge
monitor is the outermost one in the linked direction"), and the rule that the
desktop bounding box decides *whether* a seam is a crossing edge. Everything
0009 decided about the crossing **mechanism** survives, and is restated below
rather than left to be reconstructed: ADRs are immutable, so a reader of this
one must be able to see which half still stands without diffing two documents.

## Context

Phase 8 replaces "which side of the pair is this machine" with an arrangement
the user draws ([ROADMAP.md](../ROADMAP.md)). The floor it replaces is ADR
0009's: a `--left` / `--right` flag names one linked edge pair, a machine with
several monitors is treated as **one desktop** so the crossing edge is the
outer edge of that desktop, and the crossing fraction maps against the
outermost monitor in the linked direction. [SOAK.md](../SOAK.md) records the
one-desktop treatment as an honest limitation; feature/107 made the topology
re-read at runtime when displays change, which this phase builds on.

The side model cannot express what the phase's exit criteria ask for, and the
gap is structural rather than a matter of adding flags:

- **A seam between two monitors of the same machine** is, under the side
  model, deliberately *inert* — the desktop bounding box exists precisely to
  make internal seams invisible, because the Phase 5 soak showed a
  primary-monitor region turning one into a spurious-transfer loop. A per-
  monitor arrangement needs some internal seams to be crossings and others
  not, which "the outer edge of the desktop" has no way to say.
- **A corner where three monitors meet** has no representation at all: one
  linked edge pair admits one answer per machine.
- **Over/under, offset, and floating monitors** are the same missing
  expressiveness in other directions.

Three constraints shape the answer:

- **Mixed DPI and mixed resolution must behave** — a pointer leaving a 4K
  monitor at 40 % of its edge arrives at 40 % of the adjacent edge, whatever
  the scaling (R-3, and the phase's exit criterion). ADR 0009 solved this with
  a proportional position and no shared pixel space; a *shared* coordinate
  space must not quietly undo that.
- **The editor is a separate process.** The worker is a headless,
  service-launched process ([ADR 0011](0011-background-service-launcher.md)),
  so the editor is a user-session surface of its own — which makes the
  worker↔editor interface, and the crate boundary underneath it, part of this
  decision rather than the editor's.
- **A layout arriving from the peer is peer-influenced local state.** It
  decides where this machine hands control away, so it is network input and
  gets network input's treatment: bounded, validated before use, fail closed
  (NFR-1, [SECURITY.md](../SECURITY.md) invariant 5).

## Decision

### What ADR 0009 decided that this ADR does not touch

The crossing **mechanism** is unchanged, in full:

- **The edge is a trigger on the existing negotiated engine**, not a new
  engine. The `ControlRequest → ControlResponse → REMOTE` handshake, the
  session-scoped mediation, the timeout/disconnect fallback to `LOCAL` with
  `ReleaseAllInput`, and "exactly one owner, always" are reused untouched.
- **Detection is local, symmetric, and on the real cursor** — each machine
  watches its own cursor against its own enumerated geometry, never a virtual
  model of the peer's, and the control state decides whether a touch means
  *leave* or *reclaim*.
- **Position travels as a fraction, never as pixels**, encoded on the wire as
  a `u16` — `0` at the start of the edge, `u16::MAX` at the end, where the
  start is the smaller coordinate on the perpendicular axis: top for a
  Left/Right edge, left for a Top/Bottom edge.
- **Reversing direction is reclaim-to-neutral**, driven by genuine local input
  on the controlled machine, not by re-crossing.
- **The trigger is a Schmitt trigger**: a touch fires only while the detector
  is armed, and only travel more than `REARM_MARGIN` (24) back inside the
  screen re-arms it. The margin is unchanged; what changes is that the armed
  state becomes per-span (below).
- **The edge mode is a level on a `watch`, carrying its own generation**, and
  the control driver drops any crossing stamped with a generation it no longer
  publishes.
- **Every `PlaceCursor` republishes the mode under a new generation**, so a
  placement re-primes the detector even when the mode's value did not change.
- **One visible cursor, on the active machine** — the `CursorMask` rules,
  their stickiness, and both safety nets (restore on exit and on mask
  creation; show again on local input while hidden and not driving).

This ADR changes exactly two things: **where crossing edges come from**, and
**how a crossing position is addressed** now that "the edge" is no longer
unique.

### The layout model

A layout is a set of **placed monitors** in one shared coordinate space. Each
carries the device it belongs to, its monitor identity, and its rectangle.

- **Exactly two device identities appear**, and they are this session's pair.
  A layout naming a third device, or a device that is neither end of this
  session, is rejected (below) — the arrangement describes these two machines
  and nothing else.
- **At most `MAX_MONITORS_PER_MACHINE` (16) monitors per machine and
  `MAX_LAYOUT_MONITORS` (32) in total.** Both are named constants and both are
  checked before anything is allocated — generous over any real desk while
  keeping the O(n²) overlap and adjacency checks trivially cheap. A machine
  that genuinely enumerates more than the per-machine cap does not truncate:
  it refuses to send `MonitorTopology`, logs, and reports a diagnostic, so
  seamless transfer degrades observably rather than describing a desk with
  screens missing.
- **Monitor identity is the platform-supplied device string** — on Windows,
  `GetMonitorInfoW`'s `szDevice` (`\\.\DISPLAY1` and friends) — carried as at
  most `MAX_MONITOR_ID_BYTES` (64) bytes of printable ASCII (twice Windows'
  `CCHDEVICENAME`, headroom for other platforms' device strings), unique
  within a machine. It is chosen over a bare enumeration index because an index is
  positional: unplug a monitor, reboot, or re-enumerate and index 1 silently
  becomes a different screen, which would make a saved layout wrong in the way
  that is hardest to see. A device string is stable across restarts for a
  given display configuration, and where it does change, the layout says so
  observably — the monitor is simply unknown (below) rather than confidently
  mismatched.
- **Rectangles are bounded**: `1 ≤ width, height ≤ MAX_MONITOR_EXTENT`
  (65 535) and `|x|, |y| ≤ MAX_LAYOUT_COORDINATE` (2^24). A zero-sized
  monitor is refused because it has no edge to cross. The coordinate bound is
  unreachable by any legitimate arrangement — even 32 monitors of maximal
  extent laid end to end span under `2^21` — and is chosen to keep the
  overflow argument trivial: all derivation arithmetic runs in `i64`, which
  at these bounds provably cannot overflow, because an edge coordinate is at
  most `2^24 + 2^16 < 2^25`, a span length at most `2^26`, and the widest
  intermediate — a span offset scaled by `u16::MAX` before division — is under
  `2^42`, six orders of magnitude inside `i64`.
- **Monitors must not overlap.** Two rectangles occupying the same space have
  no meaningful adjacency, and a cursor in the shared overlap has no single
  answer for which monitor it left.
- **Connectivity is *not* required.** A monitor parked with nothing abutting
  it is a legal drawing that produces no crossings on its free edges. That is
  an observable property of the arrangement the user drew — the editor can
  show it — and never a validation error. Refusing it would turn a deliberate
  choice ("this screen is not a crossing surface") into a failure.
- **Abutment is exact, with zero tolerance.** Two rectangles are adjacent only
  where an edge coordinate is identical and the perpendicular spans overlap. A
  one-unit gap is observably not an edge. **Snapping is the editor's job**,
  where the user can see it happen; tolerance in the detector would make "is
  this an edge" a fuzzy question at exactly the place where a wrong answer
  hands control away.

### Layout coordinates are abstract units, and never leave the layout

The shared coordinate space is **unit-agnostic integers**. Core never
interprets a layout coordinate as a pixel, and no scale factor enters the
crossing mapping.

Every cross-machine mapping is proportional, through fractions of *drawn*
edges: a crossing is a pixel row on a real local monitor → a fraction of that
monitor's drawn edge → a point on the shared axis → a fraction of the
destination monitor's drawn edge → a pixel row on the real remote monitor.
Units cancel at the first step and DPI never enters, so the mixed-DPI exit
criterion holds **by construction** rather than by care: where the two drawn
edges align — the ordinary case, and what the editor's snap produces —
leaving at 40 % of an edge arrives at 40 % of the adjacent edge, and no pixel
count or scale factor took part in saying so.

The editor seeds a monitor's drawn size in **DIPs** (its pixel size divided by
its scale factor), so two physically-equal monitors draw equal and the picture
matches the desk. It can do that for the peer's monitors as well as its own,
because `MonitorTopology` carries each monitor's `scale_percent` alongside its
rectangle — a **seeding input only**, consumed by the editor and never by the
crossing mapping, which stays proportional through the drawn geometry. That is
a seeding convenience, not a meaning the units carry: a user who drags a
rectangle to a different size has simply drawn a different arrangement, and
the model neither knows nor needs to know what a unit is worth.

### Intra-machine geometry stays the OS's

The drawn layout answers exactly one question: **which peer monitor lies
across which of my edges, and where along it.** Everything local — where the
cursor is, which monitor it is on, how long an edge is in pixels, where a
fraction lands — comes from the live `DisplayInfo` geometry, matched to the
layout by monitor id.

Two things follow, and both are deliberate:

- **A local rearrangement that keeps monitor identities needs no layout
  edit.** Moving a monitor in Windows display settings changes local pixel
  geometry, which the worker already re-reads on display change
  (feature/107); the cross-machine relationship the user drew is untouched.
- **A machine's monitors drag as a rigid group in the editor.** Their relative
  placement is the OS's fact, not the user's to redraw — a layout that
  contradicted the OS about where a machine's own screens sit relative to each
  other would be a second, competing source of truth for something already
  answered locally.

### Crossing edges are derived from adjacency

For each local monitor edge, the derivation walks the **peer's** rectangles
and produces a **crossing span** wherever the edges abut exactly and the
perpendicular extents overlap. A span records the destination monitor, the
destination edge, and the interval of the local edge it covers.

- **Local-to-peer pairs only.** Same-machine abutment produces no span *by
  construction*, so a seam between two of this machine's own monitors stays
  invisible to the detector unless the peer is actually drawn across it. This
  is the property the desktop-bounding-box rule bought crudely, now obtained
  exactly — and it is what lets an internal seam be a real crossing when the
  user draws the peer there, which was deliverable 2.
- **Spans are half-open intervals** `[start, end)` along the shared axis, so a
  corner where three monitors meet is deterministic: the shared coordinate
  belongs to exactly one span, decided by arithmetic rather than by
  enumeration order.
- **Outer edges carry spans like any other edge.** Nothing consults the
  desktop bounding box any more, which is what retires the "one desktop"
  limitation [SOAK.md](../SOAK.md) records.

### Hysteresis becomes per-span

The armed flag ADR 0009's addendum introduced is now **one flag per span**,
with `REARM_MARGIN` unchanged at 24.

A single global flag would break exactly the behaviour it was added for. With
several spans on one edge, or spans on two edges meeting at a corner, a
crossing at one span would disarm the detector for all of them, and travel
away from a *different* span would re-arm it — so the cursor resting on an
entry column could be re-armed by motion that never left that column's
neighbourhood, and the oscillation the addendum diagnosed would return in a
form that is harder to see.

The margin test is what it was in ADR 0009's addendum: **perpendicular
distance, in local pixels, from the local edge**. Priming applies it to every
span — a placement leaves disarmed every span on any edge the cursor
currently hugs within the margin, which at a corner correctly disarms both
adjacent spans at once — and lets the geometry decide the rest. Sliding
laterally along a hugged edge from one span into its neighbour does not fire
the neighbour: a span arms only after the cursor has cleared its edge by the
margin, and lateral motion never clears anything.

### Protocol version 4

**`PROTOCOL_VERSION` and `MIN_SUPPORTED_PROTOCOL_VERSION` both move to 4.**

The reason is [ADR 0017](0017-protocol-version-3.md)'s rule, applied
unchanged: this is a structural change to messages that **already travel
between every pair of peers**, so no feature bit can hide it, and pre-1.0 the
floor tracks the ceiling because there are no deployed peers to be compatible
with. A v3 peer is refused at `Hello` with a version-range mismatch — a clean,
diagnosable refusal naming both ranges — rather than establishing a session
that dies on the first crossing.

**`ControlRequest.entry` and `ControlRelease.entry` become
`Option<EntryPoint>`**, where an `EntryPoint` is:

```
EntryPoint {
    monitor          // the destination monitor's id (≤ 64 bytes)
    edge             // Left | Right | Top | Bottom, of that monitor
    fraction         // u16 along that edge, as ADR 0009 encodes it
    layout_revision  // the revision the sender derived this from
}
```

A bare fraction is no longer sufficient information: with per-monitor seams
there is no unique "the edge" for it to be a fraction *of*. The entry point is
expressed in the **receiver's** terms — the monitor the cursor arrives on,
which of its edges, and how far along — because that is what lets the receiver
detect an entry point it cannot honour instead of placing a cursor somewhere
plausible and wrong.

**Two new base-protocol messages**, both CONTROL class:

- **`MonitorTopology` (message type 17)** — the sender's own live monitors,
  in the sender's own local coordinates, each with its id, its rectangle, and
  its `scale_percent` (`MIN_SCALE_PERCENT` 25 ..= `MAX_SCALE_PERCENT` 500,
  100 = unscaled — a seeding input for the editor's DIP sizing, above, never
  an input to crossing mapping). Sent after `Hello` and again whenever the
  local display configuration changes. It is what lets either machine's
  editor draw the peer's screens to scale, and what lets layout validation
  tell a real device string from a fiction.
- **`LayoutSync` (message type 18)** — `revision` (`u64`), the origin device
  uuid, and the placed monitors. Sent after `Hello` when this machine holds an
  explicit layout, and on every edit.

**Neither gets a feature bit**, and the reason is worth recording because a
bit is the project's usual answer. A bit here could protect nobody: the
`entry` change above already excludes every v3 peer at `Hello`, so the only
peers that can receive these messages are v4 peers, which by definition
understand them. A bit both sides always set would add a gate that never
closes and imply an interop story that does not exist.

**Validation is the same discipline as every other message.** Every bound
above is a named constant; every field is validated on **encode as well as
decode**, so a local defect cannot put a layout on the wire that the peer will
be right to refuse. A malformed message — a count past its cap, a zero or
oversized dimension, a coordinate past `2^24`, a non-ASCII or overlong monitor
id — terminates the session, fail closed, per [PROTOCOL.md](../PROTOCOL.md)
§7. A message that is **well-formed but semantically impossible** — naming a
device that is not this session's pair, a monitor neither peer has reported,
or rectangles that overlap — is rejected, logged, and charged as a protocol
violation on §7's graduated rule, and is **never adopted**. The distinction
matters: the first is a broken decoder or a hostile frame; the second is a
peer that disagrees with reality, which should not be able to cost a healthy
session its first frame but must never be allowed to steer local behaviour.

**The receiver's treatment of an `EntryPoint` is deliberately forgiving, and
only about placement.** If the named monitor id is unknown locally, or the
`layout_revision` is not the revision this machine holds, the receiver places
the cursor on the local desktop-bounds edge matching `EntryPoint.edge`, with
the fraction taken against those bounds — exactly the pre-0018 placement,
retained solely as this degraded mode — with a diagnostic naming the
mismatch, and the grant or release itself **proceeds normally**. Cursor
placement is a nicety; control correctness never depends on it. A revision
mismatch is expected, not exceptional, during an edit's propagation window:
every crossing in that window degrades this way, briefly, and the diagnostic
says which revisions disagreed. This is also why `entry` remains an
`Option`: an explicit (console) transfer places no cursor at all, exactly as
before.

### One arrangement, two machines: sync and conflict

One layout describes both machines, and both machines can edit it. Ownership
is therefore not modelled at all; convergence is:

- **Newest revision wins.** The ordering key is `(revision, origin)`,
  compared lexicographically, where `origin` — the editing device's
  identity — compares as its 16 raw bytes. The origin breaks a tie between
  two edits that independently claimed the same revision, which is what a
  simultaneous edit on both desks produces.
- **Equal key, different content** should be impossible and is still decided
  rather than left open: the layout with the lower **SHA-256 hash** — taken
  over the postcard encoding of the monitor list sorted by `(device, id)`,
  so both machines hash identical bytes — wins, and the collision is logged
  as the anomaly it is.
- **The editor assigns `seen_max.saturating_add(1)`** — one past the highest
  revision it has seen from either side. Saturating, so a peer asserting
  `u64::MAX` cannot wrap the counter and cannot brick editing: the local edit
  ties at the ceiling and the deterministic tiebreak still resolves it.
- **Adoption order is persist, publish, report.** The winning layout is
  written to the **config file first**, then published to the live topology
  `watch`, then written into the state file. Config first because that order
  is the crash-safe one: a machine that crashes between publish and persist
  would come back crossing by an arrangement its own config does not record,
  which is the worst of the three outcomes. The state file is last because it
  is a report, not a source of truth.
- **Adoption-driven persistence is rate-bounded.** The first adoption
  persists immediately; further adoptions inside `LAYOUT_PERSIST_INTERVAL`
  (5 s) coalesce to the latest revision, written once when the interval
  lapses — publication to the live `watch` stays immediate. A peer feeding
  distinct revisions as fast as it can send therefore cannot make this
  machine rewrite its config at wire speed, which matters because adoption
  is a filesystem write driven by network input. The crash window the
  coalescing opens is accepted: a restart re-syncs from the peer on
  reconnect, so the layout a crash lost is the layout the reconnect
  restores.
- **The loser logs a supersession diagnostic** naming both revisions and both
  origins (NFR-3). A user whose drawing silently vanished has no way to learn
  why; an observable supersession is the difference between "the other machine
  had a newer arrangement" and "the editor is broken".

### Persistence: config schema v2

- **`schema_version` moves to 2**, and a `[layout]` section replaces
  `[seamless] side`.
- **A v1 config — or a lingering `side` key — loads as an *implicit*
  layout**: revision 0, derived from the side, driving exactly the old
  left–right behaviour. An implicit layout is **never synced** (no
  `LayoutSync` is sent for it) and **never written back**; it upgrades to an
  explicit `[layout]` at schema 2 on the first write. An existing
  installation therefore keeps working as it did, and the first write — an
  editor save here, or the **adoption of a layout drawn on the peer** —
  upgrades the file and removes `side`. Adoption counting as a first write
  is deliberate: on a two-machine pair the peer's edit *is* the user's
  edit, drawn at the other desk, so persisting it here happens on their
  behalf, not behind their back. The honest cost: once upgraded, an older
  binary pointed at the file refuses it (ARCHITECTURE §8's
  unsupported-`schema_version` rule) — the standing schema-version stance,
  not a new one.
- **A layout that names a device other than the current pairing** — the
  residue of a re-pair — is rejected on load with a diagnostic and treated
  as no layout: seamless off, explicit control intact. The repair path is
  redrawing in the editor after the new pairing, not a guess about which
  rectangles belonged to whom.
- **`--left` / `--right` survive as warn-deprecated shorthands** producing the
  same implicit layout, so the flags in a script or a service registration
  keep meaning what they meant.
- **One deliberate deviation from CLI-wins, recorded here with its reason: an
  explicit config layout beats the flags.** Everywhere else in this codebase a
  command-line flag overrides the file. Here it must not, because the worker
  is launched by the service with a command line fixed at install time
  (ADR 0011): a `--right` saved there would flatten a drawn arrangement back
  to a side on **every** launch, so the user would draw a layout, watch it be
  ignored forever, and have nothing to look at that explains it. The flags
  still win over an implicit layout, where there is nothing to lose, and the
  deprecation warning names the config layout as the winner, so the inversion
  is observable at the moment it applies rather than inferred later.
- **No layout at all means seamless transfer is off.** Explicit control (the
  console commands) still works. A machine with no arrangement drawn should
  not guess one: guessing is how a cursor disappears onto a peer the user did
  not place there.

### The worker↔editor contract: a state file

The worker publishes what the editor needs to draw, at
**`~/.crossover/state/topology.json`** — under the same discoverable
`~/.crossover` root as the config and logs, resolved from the user's home so
the service-launched worker and an interactive editor find the same file.

- **Versioned JSON**, written by **atomic temp-file-and-rename**, so a reader
  sees a whole document or the previous one, never a half-written one.
- **One-way, worker → editor.** The editor never writes it. The reverse
  direction — an edit reaching the worker — travels through the config file,
  which the worker already owns as its startup input.
- **The worker notices the write without restarting**: it re-reads the
  config on a ~2 s modification-time poll, the same shape as the
  trust-store poll (ADR 0010), with two guards. A file that fails to parse
  keeps the last good configuration and logs — an editor caught mid-save
  must never kill the run — and a re-read whose content equals what the
  worker already holds is a no-op, which is what keeps the worker's own
  adoption writes from echoing into a worker↔peer sync loop. An edit
  therefore becomes live, and reaches the peer as a `LayoutSync`, within
  seconds and with no worker restart.
- **Contents**: this device's identity and its live monitors; the
  **last-known** peer device and its monitors, retained across a disconnect
  with `connected: false` so the editor stays usable while the peer is down
  (an editor that empties itself the moment the link drops is an editor you
  cannot use to fix the link); the current layout and its revision; and a
  heartbeat timestamp, so the editor can say "the worker is not running"
  rather than presenting stale facts as current.
- **Nothing secret is in it**: device names and uuids, monitor device strings,
  and rectangles. No key material, no clipboard content, no peer credentials
  (invariant 6), so it needs no protection beyond the profile it sits in.

### A new crate: `crossover-topology`

The layout model, its validation, the config `[layout]` section types, the
`toml_edit` writer for them, and the state-file schema live in a new workspace
crate, `crates/crossover-topology`, whose dependencies are **`serde`,
`toml_edit`, and `thiserror` and nothing else**.

It exists because the **editor binary must share the model and the writer with
the worker**, and linking `crossover-core` to get them would drag the protocol,
security, and platform crates into a GUI process that has no business holding
the TLS stack or the input injector. That is the same reasoning ADR 0011
applied to `crossover-svc`: a dependency boundary a reviewer verifies by
reading one `Cargo.toml` and `cargo tree`, rather than by auditing control
flow. `crossover-protocol` depends on it too, for the wire shapes of
`MonitorTopology`, `LayoutSync`, and `EntryPoint`, so the model and its
validation have exactly one definition instead of a wire copy and a config
copy that can drift apart; the `toml_edit` writer sits behind a non-default
`config` cargo feature so the protocol crate takes the model alone and stays
as dependency-light and socket-free as ARCHITECTURE §3.1 requires.

This ADR is the crate-creation record `adr/README.md` requires.

## Alternatives Considered

- **Keep the side model and add more sides** (`--over` / `--under`, or a
  second flag per edge). Rejected: it enumerates *arrangements* rather than
  describing geometry, and it still has no way to say that one seam between
  two of a machine's own monitors is a crossing and another is not — the
  three-monitor corner in the exit criteria has no expression in it at all.
- **Derive the arrangement automatically** from the two machines' monitor
  reports. Rejected: nothing on the network knows which physical desk the
  monitors sit on. An automatic answer would be confidently wrong some of the
  time, and being wrong here means the cursor leaves for the peer at an edge
  the user did not intend. The user knows the arrangement; the phase exists to
  let them state it.
- **Absolute pixel coordinates in the shared space, with DPI on the wire.**
  Rejected for the reason ADR 0009 gave, which a shared space makes stronger
  rather than weaker: the space is *drawn*, not measured, so pixels in it
  would be a fiction that happens to match one machine, and mapping through
  them would restore exactly the resolution and DPI coupling the fraction was
  introduced to remove.
- **A snap or abutment tolerance in the derivation** (treat a gap of a unit or
  two as an edge). Rejected: it makes "is this an edge" fuzzy at the one place
  where a false positive hands control to the other machine. The editor snaps,
  visibly, before the layout is saved; the model then sees exact numbers.
- **A designated layout owner** — one machine holds the arrangement and the
  other receives it. Rejected: both machines have a config file and an editor,
  and an ownership rule makes the arrangement uneditable from one desk exactly
  when the other machine is down. Newest-revision-wins with a deterministic
  tiebreak converges without introducing an ownership concept to get wrong.
- **A feature bit for the two new messages** instead of a version bump.
  Rejected: it protects nobody, because the `entry` change already excludes v3
  peers at `Hello` — see the decision above.
- **Requiring the layout to be connected** (every monitor touching the graph).
  Rejected: it converts a legal, deliberate drawing — a screen the user does
  not want to be a crossing surface — into a validation error the user cannot
  act on.
- **Keeping the layout out of the protocol** and expecting the user to
  configure both machines by hand. Rejected: two machines that disagree about
  the arrangement is precisely the silent mis-crossing the exit criteria
  forbid, and a hand-copied file has no revision, no conflict rule, and no
  diagnostic when it drifts.

## Consequences

- **Easier:** per-monitor seams, three-way corners, over/under and offset
  arrangements, and floating monitors all fall out of one model rather than
  each needing a mechanism. The mixed-DPI criterion is structural (the
  coordinates section above). A
  local display rearrangement that keeps monitor ids needs no layout edit. The
  "one desktop" limitation retires, and the crossing mechanism — the part with
  the soak history and the security review behind it — is reused unchanged.
- **Protocol v4 excludes v3 peers entirely.** Both machines must be upgraded
  together and a mixed pair does not connect at all. This is the third bump
  and the third to raise the floor; the stance and its acceptance are ADR
  0017's, unchanged — the failure mode is a refusal at `Hello` that names both
  ranges, before any session state exists, and `crossover version` reports the
  range for exactly this reason. It belongs in the release notes as well as
  here.
- **The side model's simplicity is gone.** `--left` was one flag and a
  boolean; a layout is up to 32 rectangles, a revision, a sync rule, a state
  file, and a GUI. Anyone diagnosing a crossing now reads an arrangement
  instead of a flag — which is why the state file exists and why diagnostics
  name monitor ids rather than describing edges in prose.
- **Per-span hysteresis is subtler than one flag.** The anti-bounce property
  ADR 0009's addendum won is now a per-span invariant, and the test suite has
  to state it per span — at a corner and on a multi-span edge — rather than
  once. A regression here reproduces the oscillation that addendum exists to
  prevent, so it is exercised deliberately rather than incidentally.
- **A second process writes the config file.** The editor writes `[layout]`
  and so does the worker when it adopts a peer's arrangement. This is handled
  by atomic writes and a **content-equality no-op on re-read**: an adoption
  that changes nothing rewrites nothing, so the two writers cannot ping-pong,
  and the writer is `toml_edit` rather than serialize-and-truncate so a user's
  comments and formatting survive a write they did not make.
- **The revision ceiling is saturating, and that is visible at the extreme.**
  A peer that asserts `u64::MAX` pins both sides at the ceiling, after which
  the `(revision, origin)` tiebreak is fixed and one machine's edits stop
  winning. It cannot corrupt or wrap anything, it is logged, and it is only
  reachable from a peer already sending nonsense — accepted as the cheaper
  cost than a wrapping counter or a rejection rule that could refuse a
  legitimate edit.
- **Cursor placement can degrade without control degrading.** A layout
  mismatch produces a cursor at the desktop edge and a diagnostic, not a
  failed or divided grant. That asymmetry is deliberate and is the reason the
  entry point may be treated as advisory.
- **(Added 2026-08-21, feature/152.) A run holding no drawn arrangement
  needs one restart before an adopted one drives its cursor.** Adoption on
  such a run persists and reports but cannot publish — there is no live
  crossing source to replace (see the amendment below). So the very first
  sync onto a fresh machine converges the *arrangement* immediately and the
  *behaviour* at the next start. It is the honest cost of not rebuilding
  the detector and the placement path mid-session, it applies once per
  machine rather than per edit, and it is logged at the moment it applies
  so nobody has to deduce it from a cursor that will not cross.
- **The platform display trait grows an identity.** `MonitorRect` today is
  bare geometry; matching live monitors to a layout needs a stable id per
  monitor, which on Windows is `GetMonitorInfoW`'s `szDevice`. A stable
  per-monitor identifier thereby becomes a requirement on the future macOS
  and Linux backends as well — recorded ahead of Phase 9, alongside the
  risks in [platform-risks-linux.md](../platform-risks-linux.md).
- **Security:** the peer-supplied layout is a new peer-influenced local state
  transition, added to [SECURITY.md](../SECURITY.md) §6 as **T23** and
  contained by the bounds, the fail-closed decode, the reject-and-log
  treatment of impossible-but-well-formed layouts, the rate-bounded
  persistence, and observable adoption. A hostile *trusted* peer remains out
  of scope under §6's existing carve-out. Two adjacent facts, stated rather
  than implied: `MonitorTopology` discloses local display geometry, scale,
  and device strings to the trusted peer — facts already visible in this
  machine's own logs; and the config file gains a second, medium-integrity
  writer (the editor) feeding a high-integrity reader (the worker,
  ADR 0012) — the same shape as T21's local-process concern, contained the
  same way everything from this file always was: validated on load, before
  use.

**Amendment (2026-08-21):** the adoption order above says the winning
layout is "published to the live topology `watch`", and implementation
found one case the sentence does not cover: a run that holds **no drawn
arrangement** — an implicit `--left`/`--right` run, or seamless off — has
no live crossing source for a publication to replace. Its detector is
derived from the side model, or does not exist at all, and switching a
*running* worker between the two arrangement models would mean rebuilding
the detector and the control driver's placement path mid-session, which
this decision never asked for and which buys nothing a restart does not.

So on such a run, adoption **persists and reports, and the publication is
a no-op that logs itself**: the arrangement is written to the config (which
is precisely the schema 1 → 2 upgrade the persistence section describes),
the state file records it, and the next start crosses by it. The run in
progress keeps crossing the way it was configured to. The two properties
this decision actually rests on are unaffected — the layout is never lost,
and the change is never silent — and the honest cost is one restart before
a first-ever adopted arrangement drives the cursor. A run that already has
a drawn arrangement publishes immediately, as specified, which is the case
the sentence was written for.

**That such a run adopts and persists at all is deliberate, and is the
primary first-use flow rather than an edge case.** A fresh machine — a v1
config, a `--right` baked into a service registration, or nothing
configured — holds no arrangement, so the first time anyone draws one it is
drawn at the *other* desk. Declining to adopt there would leave the pair
unable to converge until the user visited both machines with the editor,
which is precisely the hand-copied-file failure "Alternatives Considered"
rejects. The persistence section already settles the authority question in
its own words: on a two-machine pair the peer's edit **is** the user's
edit, drawn at the other desk, so persisting it here happens on their
behalf rather than behind their back — the same reasoning that makes
adoption count as the first write for the schema upgrade.

**Amendment (2026-08-21), the rejection list:** "a monitor neither peer has
reported" is removed from the well-formed-but-impossible cases above.
Implementation showed it over-strict rather than protective: an arrangement
legitimately names screens that are not attached right now, which is what
lets a drawing survive an undock or a reboot with a monitor powered off,
and such a layout **derives inert** — no spans — so it cannot invent a
crossing, only fail to produce one. What the receiver owes is
observability, and it now gives it: adopting an arrangement matching none
of this machine's attached screens warns at that moment, naming the drawn
ids and the attached ones. [PROTOCOL.md](../PROTOCOL.md) §6.2 and
[SECURITY.md](../SECURITY.md) T23 carry the same amendment.

**Amendment (2026-08-20):** the `config` feature also carries `serde_json`,
for the state-file schema this ADR places in the same crate — versioned
JSON needs a JSON implementation, which the decision's dependency sentence
did not name. That sentence describes the **default** graph, which remains
`serde` and `thiserror`; `toml_edit` and `serde_json` are both what the
non-default `config` feature adds, and `crossover-protocol` will take the
default graph exactly as the decision intends.
