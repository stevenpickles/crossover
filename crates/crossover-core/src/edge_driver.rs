//! Edge-detection driver for seamless control transfer (ADR 0009, ADR
//! 0018).
//!
//! While this machine controls itself, the cursor reaching a crossing span
//! means *leave* — request control of the peer. While the peer controls
//! this machine, the cursor reaching a span means *return* — reclaim
//! control. Both are the identical geometric test on the real local cursor
//! ([`crate::crossing`]); the **mode**, supplied by the control wiring, says
//! which meaning applies. While this machine is driving the peer, its own
//! cursor is frozen, so detection is idle and the driver does not poll.
//!
//! The pure [`EdgeDetector`] turns a stream of cursor observations into a
//! crossing on the **rising edge only** — the first observation where the
//! cursor reaches a span after having been clear of it — so a cursor pinned
//! at a seam fires the transfer once, not on every poll. "Clear of it" is a
//! Schmitt trigger, not a bare threshold: the cursor must travel
//! [`REARM_MARGIN`] pixels *perpendicularly* clear before the next touch
//! counts (see that constant). The async [`EdgeDetectDriver`] polls the
//! display, applies the mode, and emits the crossings.
//!
//! # What ADR 0018 changed here, and what it deliberately did not
//!
//! The detector used to measure against the side model's one
//! linked edge; it now measures against a [`CrossingMap`], which is every
//! crossing the drawn arrangement gives this machine. Three consequences,
//! all of them narrow:
//!
//! - **The armed flag is per span.** One flag for the whole detector would
//!   let a crossing at one span disarm every other, and travel away from a
//!   *different* span re-arm it — the oscillation ADR 0009's addendum
//!   exists to prevent, in a form that is harder to see. Each span carries
//!   its own flag, indexed by the map's dense [`SpanId`].
//! - **Priming, and a crossing, leave disarmed every span the cursor is
//!   within the margin of** — which at a corner is both adjacent spans, and
//!   on a multi-span edge is every span on it. The two situations are
//!   geometrically identical (a cursor parked on a seam, put there by an
//!   entry placement or by having just crossed), so they leave identical
//!   state. That is what makes sliding laterally along a hugged edge inert:
//!   lateral motion clears nothing, so the neighbour it slides into was
//!   never armed.
//! - **The map is a pure function of the arrangement and the live
//!   geometry**, so a display change invalidates it. The driver re-derives
//!   through its [`CrossingSource`] and re-primes, never fires — the
//!   feature/107 invariant, extended from "the edge moved" to "every edge
//!   moved".
//!
//! Everything else is unchanged on purpose, because it was bought by a
//! soak: the mode is a level on a `watch` carrying a sender-stamped
//! generation, every publication re-primes, and a crossing detected across
//! a mid-poll display change is discarded rather than trusted.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use crossover_platform::{DisplayError, DisplayInfo, MonitorInfo};
use crossover_topology::{DeviceId, Layout};

use crate::crossing::{CrossTarget, CrossingMap, SpanId, from_link_side};
use crate::topology::{CursorPoint, EdgeFraction, LinkSide, MonitorRect};

/// What the detector is currently watching for — driven by the control
/// state (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeMode {
    /// Not watching: this machine is driving the peer, so its own cursor
    /// is frozen and no edge is meaningful.
    Idle,
    /// This machine controls itself; a crossing is a request to control
    /// the peer.
    Leaving,
    /// The peer controls this machine; a crossing reclaims control.
    Returning,
}

impl EdgeMode {
    /// The kind of crossing a span touch means in this mode, or `None` when
    /// idle (no crossing is meaningful).
    fn crossing_kind(self) -> Option<CrossingKind> {
        match self {
            Self::Leaving => Some(CrossingKind::Leave),
            Self::Returning => Some(CrossingKind::Return),
            Self::Idle => None,
        }
    }
}

/// What the control wiring publishes to the detector: the mode to watch
/// in, and the generation that publication is.
///
/// The mode is a **level**, not an event — "what this machine is watching
/// for right now" — so it travels on a [`watch`] channel: latest wins,
/// publishing never blocks, and a detector that fell behind reads the
/// current state rather than a queue of superseded ones. That matters
/// beyond tidiness: the mode used to ride a bounded `mpsc` inside a cycle
/// (control loop → mode → detector → crossings → control loop), so any
/// slowness in the loop fed back into itself.
///
/// The generation is stamped by the **sender** and carried inside the
/// value, because a `watch` coalesces: counting updates at each end — the
/// scheme a lossless FIFO allowed — would drift the moment two
/// publications collapsed into one. Carried, it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeModeUpdate {
    /// What to watch for.
    pub mode: EdgeMode,
    /// Which publication this is. Monotonic; stamped onto every crossing
    /// detected under it (see [`EdgeCrossing::generation`]).
    pub generation: u64,
}

impl EdgeModeUpdate {
    /// The state before the control wiring has published anything: idle,
    /// generation zero.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            mode: EdgeMode::Idle,
            generation: 0,
        }
    }
}

/// Which direction a detected crossing goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossingKind {
    /// The cursor left across a crossing span: request control of the peer.
    Leave,
    /// The cursor returned to a crossing span: reclaim control.
    Return,
}

/// Where a crossing goes: the destination in the **receiver's** terms, how
/// far along that destination's facing edge, and the revision of the
/// arrangement that said so (ADR 0018).
///
/// This is deliberately **everything a wire `EntryPoint` needs**: the
/// monitor the cursor arrives on, which of its edges, the fraction along
/// it, and the layout revision. A consumer builds the message from this
/// without re-deriving anything, and without holding the [`CrossingMap`]
/// the crossing was detected against — which is the whole point, because
/// that map may already have been replaced by the time the message is
/// built.
///
/// **No mirroring happens downstream.** [`CrossTarget::edge`] is already
/// the receiver's arrival edge — the derivation computed it as
/// [`crate::topology::Edge::opposite`] of the local edge the span sits on —
/// so `control`'s `wire_entry_point` copies it through. A second
/// `opposite()` anywhere on the way out would mirror it back onto the
/// sender's own edge and place the peer's cursor on the wrong side of its
/// screen.
///
/// The destination is **carried by value** rather than named by
/// [`SpanId`], and that is the whole point of the type. A span id is only
/// meaningful in the map that produced it, and a crossing outlives that map
/// — it queues on a channel while a display change rebuilds the arrangement
/// underneath. Resolving the destination at detection time costs one
/// device-string clone on the rare tick that actually fires, and makes a
/// stale crossing impossible to misread as a fresh one.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedCrossing {
    /// The machine, monitor and edge the cursor arrives on. A `monitor` of
    /// `None` is ADR 0018's *unaddressed* entry point — what an implicit
    /// (side-model) arrangement always produces — and the receiver places
    /// against its own desktop bounds.
    pub target: CrossTarget,
    /// How far along the target's facing edge the cursor arrives, `[0, 1]`.
    pub position: EdgeFraction,
    /// The revision of the arrangement this crossing was derived from
    /// ([`CrossingMap::revision`]) — `0` for an implicit one, which is
    /// also what an unaddressed `EntryPoint` carries.
    ///
    /// Stamped here, at detection, rather than read from the live
    /// arrangement when the message is built: a crossing queues on a
    /// channel while a display change or (later) a peer's `LayoutSync`
    /// rebuilds the map underneath it, and an entry point must state the
    /// revision the *sender actually used*, not whichever one it happens
    /// to hold a moment later. Getting that wrong would make the receiver
    /// honour an entry point derived from an arrangement neither machine
    /// still has.
    pub layout_revision: u64,
}

/// A detected edge crossing: which way it goes, where it lands, and the
/// control generation it was detected under.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeCrossing {
    /// Leave or return.
    pub kind: CrossingKind,
    /// Where the cursor went and how far along it arrives.
    pub crossing: DetectedCrossing,
    /// The [`EdgeModeUpdate::generation`] the detector was watching under
    /// when this fired. `kind` is frozen at detection time, so a crossing
    /// that queues behind a control-state change would otherwise act on a
    /// state that no longer exists (a stale `Return` revoking a *fresh*
    /// grant). The control driver stamps each mode it publishes and
    /// discards any crossing whose generation is not the current one.
    pub generation: u64,
}

/// How far the cursor must travel back inside the screen, in pixels, before
/// a touch of a crossing span counts as a new crossing (ADR 0009 addendum,
/// 2026-08-19; per span since ADR 0018).
///
/// A transfer leaves the cursor resting *exactly* on the crossing column at
/// both ends — that is deliberate, for cursor continuity — and a span does
/// double duty (leave while local, return while controlled). With a bare
/// one-pixel rising edge, a two-pixel wobble at 125 Hz polling was enough to
/// fire a complete reverse transfer, which re-parked both cursors on their
/// trigger columns and so repeated: ten take/revoke cycles in five seconds
/// on hardware. Requiring real inward travel first makes a wobble inert
/// while costing a deliberate crossing nothing — a real crossing covers far
/// more than this margin on its way to the edge.
///
/// The distance is **perpendicular to the span's own edge**
/// ([`CrossingMap::clear_of`]), which is what keeps lateral travel along a
/// hugged edge from arming anything.
pub const REARM_MARGIN: u32 = 24;

/// The pure crossing detector: rising-edge detection against every span of
/// a [`CrossingMap`], with per-span hysteresis. Holds which spans a touch
/// would currently count on — and the monitor layout that judgment was made
/// against — so a crossing fires once on a genuine arrival, never
/// repeatedly while the cursor sits pinned there or jitters against it, and
/// never because the *geometry* moved under a stationary cursor. No I/O.
#[derive(Debug)]
pub struct EdgeDetector {
    /// Every crossing this machine can make, given the arrangement and the
    /// monitors it had when this map was derived.
    map: Arc<CrossingMap>,
    /// Whether a touch of each span would count as a crossing, indexed by
    /// [`SpanId::index`] — the reason that index is dense. Set for a span
    /// only by an observation [`REARM_MARGIN`] pixels perpendicularly clear
    /// of *that span's* edge; cleared for every span the cursor is within
    /// the margin of, by a crossing and by priming alike.
    armed: Vec<bool>,
    /// The live monitors `armed` — and `map` — were computed against. A
    /// display change (dock, undock, a monitor powering off, a screen
    /// re-enumerated under a new device string) moves seams without the
    /// cursor moving, so neither the flags nor the map still describe the
    /// geometry the cursor is actually in: an interior column can become a
    /// crossing span in one tick. The observation after a change refuses to
    /// fire, so a hotplug never transfers control by itself.
    ///
    /// The whole [`MonitorInfo`] and not just the rectangle, because
    /// identity is an input to a drawn arrangement's derivation: a screen
    /// re-enumerated under a different device string, in the same place,
    /// changes which seams exist without changing a single pixel.
    ///
    /// **Only [`adopt`](Self::adopt) moves this**, together with the map it
    /// belongs to. Nothing else may, and that is load-bearing: a snapshot
    /// that advanced without the map would leave the detector measuring
    /// new geometry against an arrangement derived from the old, with
    /// nothing left to notice the discrepancy — permanently, and silently.
    live: Vec<MonitorInfo>,
}

impl EdgeDetector {
    /// A detector over `map`, with every span disarmed: only an observation
    /// clear of a span arms it.
    ///
    /// The snapshot starts as the geometry `map` was derived from — with
    /// no identities, since the map does not record which ids it matched —
    /// so `layout_changed` is meaningful from the first observation. A
    /// caller whose source reads identity therefore re-derives once at the
    /// start, which is the safe direction to be wrong in.
    #[must_use]
    pub fn new(map: Arc<CrossingMap>) -> Self {
        let live = map
            .monitors()
            .iter()
            .map(|monitor| MonitorInfo {
                id: monitor.id().cloned().map(|id| id.as_str().to_owned()),
                rect: monitor.live(),
            })
            .collect();
        Self {
            armed: vec![false; map.span_count()],
            map,
            live,
        }
    }

    /// Replace the arrangement **and** the live monitors it was derived
    /// from, and re-prime against them, without emitting a crossing.
    ///
    /// The map-change half of the invariant [`observe`](Self::observe)
    /// applies to a geometry change: a swapped arrangement moves every seam
    /// at once, under a cursor that has not moved, so the observation after
    /// it must re-prime rather than read a moved seam as an arrival. Every
    /// per-span flag is discarded with the map that gave it meaning — a
    /// [`SpanId`] is only valid for the map that produced it.
    ///
    /// This is the **only** way the snapshot advances, so a caller that
    /// cannot derive a new map (its display read failed, say) simply does
    /// not call it, and the staleness is still there to be retried on the
    /// next tick.
    pub fn adopt(&mut self, map: Arc<CrossingMap>, live: &[MonitorInfo], cursor: CursorPoint) {
        self.map = map;
        if self.live.as_slice() != live {
            self.live = live.to_vec();
        }
        self.prime(cursor);
    }

    /// Set the armed state from a cursor **without** emitting a crossing.
    /// Used when detection (re)starts, so a cursor already sitting on a
    /// span — where a transfer's entry placement puts it — does not fire an
    /// immediate, unintended transfer; only an arrival from clear of that
    /// span does.
    ///
    /// Priming applies the same [`REARM_MARGIN`] test as
    /// [`observe`](Self::observe), per span: every span the cursor is
    /// within the margin of is left disarmed — which at a corner is both
    /// adjacent spans — and every span it is genuinely clear of is left
    /// armed. That also closes the race between the entry placement and
    /// this prime: they run on different tasks, so a few injected pixels of
    /// motion can land in between, and they are nowhere near enough to arm.
    ///
    /// It judges against the map the detector currently holds and **does
    /// not touch the snapshot**: priming is about the cursor, not about the
    /// geometry. See [`adopt`](Self::adopt) for why that separation matters.
    pub fn prime(&mut self, cursor: CursorPoint) {
        self.armed = (0..self.map.span_count())
            .map(|index| {
                self.map
                    .clear_of(SpanId::from_index(index), cursor, REARM_MARGIN)
            })
            .collect();
    }

    /// Whether `live` differs from the monitors of the last observation —
    /// which are also the ones the current map was derived from, so a
    /// `true` here means the map needs re-deriving, and stays `true` until
    /// [`adopt`](Self::adopt) supplies one.
    #[must_use]
    pub fn layout_changed(&self, live: &[MonitorInfo]) -> bool {
        self.live.as_slice() != live
    }

    /// Observe a cursor position. Returns a crossing only on an armed
    /// rising edge — the cursor reaching a span after having been
    /// [`REARM_MARGIN`] pixels perpendicularly clear of it — and `None`
    /// otherwise (clear of every span, still pinned, jittering against one,
    /// sliding along a hugged edge, or a display change, which cannot fire
    /// at all until the caller has supplied the arrangement that goes with
    /// the new geometry).
    #[must_use]
    pub fn observe(
        &mut self,
        cursor: CursorPoint,
        live: &[MonitorInfo],
    ) -> Option<DetectedCrossing> {
        if self.layout_changed(live) {
            // Deliberately inert rather than re-priming: the map describes
            // geometry that is gone, so every judgment it could make — armed
            // or not, touching or not — is about seams that have moved. The
            // caller re-derives and calls `adopt`; until it does, the
            // staleness stays visible.
            return None;
        }
        self.arm_cleared_spans(cursor);

        // Spans come out in derivation order, so a cursor on two edges of
        // one monitor at once resolves the same way every time (NFR-2).
        let fired = self
            .map
            .crossings_at(cursor)
            .find(|span| self.is_armed(*span))?;
        let position = self.map.fraction_at(fired, cursor)?;
        let target = self.map.span(fired)?.target().clone();
        // One crossing per approach, and not just for the span that fired:
        // the cursor is now parked on a seam, exactly as an entry placement
        // would leave it, so every span it hugs is left disarmed and only
        // real perpendicular travel brings any of them back.
        self.disarm_spans_near(cursor);
        Some(DetectedCrossing {
            target,
            position,
            layout_revision: self.map.revision(),
        })
    }

    /// Is this span's flag set? A span id the map does not hold is never
    /// armed, so a stale id can only ever suppress a crossing.
    fn is_armed(&self, span: SpanId) -> bool {
        self.armed.get(span.index()).copied().unwrap_or(false)
    }

    /// Arm every span the cursor has travelled clear of. Latching: a span
    /// stays armed until something disarms it, so a deliberate crossing
    /// that begins in the middle of the screen fires on its first touch.
    fn arm_cleared_spans(&mut self, cursor: CursorPoint) {
        for index in 0..self.armed.len() {
            if self
                .map
                .clear_of(SpanId::from_index(index), cursor, REARM_MARGIN)
            {
                self.armed[index] = true;
            }
        }
    }

    /// Disarm every span the cursor is *not* clear of — the exact set
    /// [`CrossingMap::spans_near`] names, written as a loop so no
    /// allocation happens on the poll path.
    fn disarm_spans_near(&mut self, cursor: CursorPoint) {
        for index in 0..self.armed.len() {
            if !self
                .map
                .clear_of(SpanId::from_index(index), cursor, REARM_MARGIN)
            {
                self.armed[index] = false;
            }
        }
    }
}

/// The one thing a detector cannot work out for itself: how to rebuild its
/// [`CrossingMap`] when the display configuration changes — and how much of
/// the display it needs read in order to do so.
///
/// A map is a pure function of the arrangement and the live monitors, and
/// the detector only ever sees the latter. Handing the driver a function
/// rather than an arrangement is what lets the *same* driver serve an
/// implicit side-model layout ([`implicit_crossing_source`]) and an
/// explicit drawn one ([`explicit_crossing_source`]) without knowing which
/// it has.
///
/// **One source per run, shared by both consumers.** The detector holds it
/// to re-derive on a display change; `control_driver`'s `SeamlessInputs`
/// holds a clone to derive at cursor-placement time, off its own fresh
/// read. That is one *definition* of the derivation with two call sites at
/// different cadences, rather than a derived map published from one to the
/// other — which would be a second, independently-aged copy of a pure
/// function's result. It is also the shape the layout-sync branch needs:
/// what changes when a peer's arrangement is adopted is the layout, so a
/// source closure reading it from a `watch` reaches both consumers with no
/// further wiring.
///
/// # Why the fidelity is declared rather than assumed
///
/// [`DisplayInfo::monitor_layout`] is the identity query, and the platform
/// trait is explicit that it is not for the hot path: on Windows it adds a
/// `GetMonitorInfoW` and a `String` per monitor, and warns for every screen
/// the OS declines to name — at an 8 ms poll that is a warning every 8 ms.
/// [`DisplayInfo::monitors`] is bare geometry and cheap.
///
/// A source that reads identity needs the expensive query, because a screen
/// re-enumerated under a new device string changes which seams exist
/// without moving a pixel. A source that does not — the side model, which
/// picks its edge by position and reports an unaddressed destination — must
/// not be made to pay for it, so it declares itself
/// [`geometry_only`](Self::geometry_only) and the driver polls the cheap
/// query and hands it monitors with no ids.
#[derive(Clone)]
pub struct CrossingSource {
    derive: Arc<DeriveFn>,
    reads_identity: bool,
}

/// The derivation itself: live monitors in, the crossings they give out.
type DeriveFn = dyn Fn(&[MonitorInfo]) -> CrossingMap + Send + Sync;

impl std::fmt::Debug for CrossingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossingSource")
            .field("reads_identity", &self.reads_identity)
            .finish_non_exhaustive()
    }
}

impl CrossingSource {
    /// A source that derives from **rectangles alone**. The driver will
    /// poll [`DisplayInfo::monitors`] and pass monitors with `id: None`, so
    /// the closure must not depend on identity to succeed.
    #[must_use]
    pub fn geometry_only(
        derive: impl Fn(&[MonitorInfo]) -> CrossingMap + Send + Sync + 'static,
    ) -> Self {
        Self {
            derive: Arc::new(derive),
            reads_identity: false,
        }
    }

    /// A source that matches live screens to drawn rectangles **by device
    /// string**. The driver will poll [`DisplayInfo::monitor_layout`], so an
    /// identity-only change re-derives.
    #[must_use]
    pub fn identified(
        derive: impl Fn(&[MonitorInfo]) -> CrossingMap + Send + Sync + 'static,
    ) -> Self {
        Self {
            derive: Arc::new(derive),
            reads_identity: true,
        }
    }

    /// The map these monitors give.
    #[must_use]
    pub fn derive(&self, live: &[MonitorInfo]) -> CrossingMap {
        (self.derive)(live)
    }

    /// The live monitors this source needs, read at its own fidelity.
    ///
    /// The one place the choice of display query is made, so the list the
    /// driver compares its snapshot against is always exactly the list the
    /// derivation would consume.
    ///
    /// # Errors
    ///
    /// Whatever the platform reports when it cannot enumerate the display.
    pub fn read(&self, display: &dyn DisplayInfo) -> Result<Vec<MonitorInfo>, DisplayError> {
        if self.reads_identity {
            display.monitor_layout()
        } else {
            Ok(display
                .monitors()?
                .into_iter()
                .map(|rect| MonitorInfo { id: None, rect })
                .collect())
        }
    }
}

/// The side model as a crossing source: geometry in, one unaddressed span
/// out (ADR 0009 expressed under ADR 0018).
///
/// **The single construction of the implicit path**, used by the worker and
/// by every test rig, so "the tests exercise the production path" is true
/// by construction rather than by two copies agreeing today.
///
/// It reads no identity, so no plug event, no unnamed screen, and no
/// duplicated device string can make it refuse — the property that keeps a
/// `--left` run that worked before ADR 0018 working after it. The only
/// failures left are geometry no layout model can express at all, which no
/// display reports; they degrade to [`CrossingMap::inert`] with a warning,
/// and that fallback carries a contract worth reading before relying on it.
#[must_use]
pub fn implicit_crossing_source(side: LinkSide, local: DeviceId) -> CrossingSource {
    CrossingSource::geometry_only(move |live| {
        let rects: Vec<MonitorRect> = live.iter().map(|monitor| monitor.rect).collect();
        match from_link_side(side, local, &rects) {
            Ok(implicit) => implicit.crossings(local, &rects),
            Err(error) => {
                tracing::warn!(
                    %error,
                    monitors = rects.len(),
                    "edge: this display geometry cannot be expressed as an arrangement; \
                     seamless transfer is off until it changes"
                );
                CrossingMap::inert(local, live)
            }
        }
    })
}

/// A **drawn** arrangement as a crossing source: the layout the user drew,
/// matched to live screens by device string (ADR 0018).
///
/// The counterpart to [`implicit_crossing_source`], and the reason the
/// driver takes a source rather than an arrangement: the same detector,
/// the same control wiring and the same placement path serve both, and
/// only this constructor knows which it has.
///
/// It reads identity ([`CrossingSource::identified`]), because that is the
/// whole mechanism — a drawn rectangle finds its screen by the platform's
/// device string, and a screen re-enumerated under a different one changes
/// which seams exist without moving a pixel.
///
/// # Degrading, and the contract that survives it
///
/// [`crate::crossing::derive`] is total: it never refuses. What it can
/// produce is a map with **no spans** — every screen the layout placed for
/// this machine is unplugged, renamed, or reported ambiguously — which is
/// the same state [`CrossingMap::inert`] describes, reached by geometry
/// rather than by error. That map's contract says a machine currently
/// *being controlled* must not be left without a reclaim path. What is
/// lost when it goes inert is reclaiming **by crossing a specific seam**;
/// what remains are four routes that never ran through spans, and it is
/// worth being exact about each rather than claiming the first one always
/// works:
///
/// - **Local input on the controlled machine** —
///   `ControlEvent::LocalInputReclaim`, which gives the grant up on any
///   genuine local input (ADR 0009). This is the one a user reaches for by
///   instinct, and it is *usually* enough, but not unconditionally: the
///   detection re-baselines the system input tick after every injection
///   the peer makes, so while the controller is actively driving, a local
///   event can be re-baselined past before a poll sees it. It resolves the
///   moment the controller pauses — see `local_input_reclaim_due`, which
///   states the same caveat — and it is unavailable altogether on a
///   platform with no input-tick query.
/// - **`r` at the controller's console**, which hands control back.
/// - **Both Control keys at the controller** — the escape gesture, and the
///   way out when its keyboard is captured and its console unreachable
///   (ADR 0008).
/// - **Disconnect**, which releases everything on both sides (FR-4.4).
///
/// The starvation window above is why [`CrossingMap::inert`] carries a
/// NOTE naming the stronger fix — keeping the previous map's spans alive
/// for the `Returning` direction — as the thing to reach for if a soak
/// ever shows this mattering in practice.
///
/// The degradation is logged rather than silent, because "seamless quietly
/// stopped working" is the report that cannot be diagnosed afterwards
/// (NFR-3).
#[must_use]
pub fn explicit_crossing_source(layout: Layout, local: DeviceId) -> CrossingSource {
    CrossingSource::identified(move |live| {
        let map = crate::crossing::derive(&layout, local, live);
        if map.is_inert() {
            tracing::warn!(
                revision = layout.revision(),
                monitors = live.len(),
                "edge: none of this machine's live screens matches the drawn arrangement; \
                 crossing by cursor is off until one does (local input, the console, and the \
                 escape gesture still end a grant)"
            );
        }
        map
    })
}

/// Build an edge-detection driver over `map`, polling `display` every
/// `poll_interval` and re-deriving through `source` whenever the display
/// configuration changes. Returns the driver (spawn
/// [`EdgeDetectDriver::run`]), a sender the control wiring uses to publish
/// the [`EdgeModeUpdate`], and a receiver of detected [`EdgeCrossing`]s.
#[must_use]
pub fn edge_detect(
    display: Arc<dyn DisplayInfo>,
    map: Arc<CrossingMap>,
    source: CrossingSource,
    poll_interval: Duration,
) -> (
    EdgeDetectDriver,
    watch::Sender<EdgeModeUpdate>,
    mpsc::Receiver<EdgeCrossing>,
) {
    let (mode_tx, mode_rx) = watch::channel(EdgeModeUpdate::initial());
    let (crossings_tx, crossings_rx) = mpsc::channel(8);
    let driver = EdgeDetectDriver {
        display,
        source,
        detector: EdgeDetector::new(map),
        mode: EdgeMode::Idle,
        generation: 0,
        mode_rx,
        crossings_tx,
        poll_interval,
        display_failing: false,
    };
    (driver, mode_tx, crossings_rx)
}

/// The async shell: polls the display while watching and emits crossings.
pub struct EdgeDetectDriver {
    display: Arc<dyn DisplayInfo>,
    source: CrossingSource,
    detector: EdgeDetector,
    mode: EdgeMode,
    /// The generation of the mode currently applied, stamped on every
    /// crossing so a consumer can tell one detected under the current
    /// control state from one that queued behind a state change (see
    /// [`EdgeCrossing::generation`]). Read from the published value, never
    /// counted here — a `watch` coalesces, so counting would drift.
    generation: u64,
    mode_rx: watch::Receiver<EdgeModeUpdate>,
    crossings_tx: mpsc::Sender<EdgeCrossing>,
    poll_interval: Duration,
    /// Whether the display is currently refusing to answer.
    ///
    /// A poll runs 125 times a second, so a display that has stopped
    /// answering must not log 125 lines a second — and must not be
    /// invisible either, because "seamless quietly stopped working" is
    /// precisely the report that cannot be diagnosed after the fact
    /// (NFR-3). One `warn` on the way into a failing streak, one on the way
    /// out, and nothing in between.
    display_failing: bool,
}

impl EdgeDetectDriver {
    /// Run until the mode sender or the crossings receiver is dropped.
    /// Spawn this. Polls only while watching (not [`EdgeMode::Idle`]), so
    /// an idle machine costs nothing.
    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(self.poll_interval);
        // A skipped poll is fine — the cursor stays pinned at the edge, so
        // the next tick still catches it; never burst-catch-up.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = self.mode_rx.changed() => {
                    if changed.is_err() {
                        break; // wiring gone
                    }
                    if self.apply_mode() {
                        ticker.reset();
                    }
                }
                _ = ticker.tick(), if self.mode != EdgeMode::Idle => {
                    // `select!` picks at random among ready branches, so a
                    // mode already published can lose to this tick and the
                    // poll would observe under a superseded state. The mode
                    // is a level: take the newest before looking at all.
                    if self.mode_rx.has_changed().unwrap_or(false) {
                        if self.apply_mode() {
                            ticker.reset();
                        }
                        continue;
                    }
                    if !self.poll().await {
                        break; // crossings receiver gone
                    }
                }
            }
        }
        tracing::debug!("edge-detection driver stopped");
    }

    /// Adopt the latest published mode. Returns whether the detector was
    /// re-primed (and the poll ticker should restart with it), which is
    /// every update that is not [`EdgeMode::Idle`] — including a *repeat*
    /// of the current mode, which is how the control wiring asks for a
    /// re-prime after placing the cursor (ADR 0009 addendum, 2026-08-19).
    fn apply_mode(&mut self) -> bool {
        let update = *self.mode_rx.borrow_and_update();
        self.mode = update.mode;
        self.generation = update.generation;
        tracing::debug!(
            mode = ?update.mode,
            generation = update.generation,
            "edge: watching mode published"
        );
        if update.mode == EdgeMode::Idle {
            return false;
        }
        // Begin from the current cursor so a position already on a span
        // does not fire immediately.
        self.prime();
        true
    }

    /// Read the display once and prime the detector's per-span state,
    /// re-deriving the map first if the monitors have moved since it was
    /// built.
    ///
    /// A read failure primes nothing and — crucially — **adopts nothing**:
    /// the detector's snapshot still names the geometry the map was derived
    /// from, so the staleness is still there for the next tick to act on.
    /// Advancing the snapshot here without a map to go with it was a real
    /// wedge: the detector would measure new geometry against an
    /// arrangement derived from the old, with nothing left to notice.
    fn prime(&mut self) {
        let Some(live) = self.read_live() else {
            return;
        };
        let Ok(cursor) = self.display.cursor_position() else {
            return;
        };
        if self.detector.layout_changed(&live) {
            self.rederive(&live, cursor);
        } else {
            self.detector.prime(cursor);
        }
    }

    /// The live monitors, at the fidelity this run's source needs, with a
    /// persistent failure logged once per streak rather than per tick.
    fn read_live(&mut self) -> Option<Vec<MonitorInfo>> {
        match self.source.read(&*self.display) {
            Ok(live) => {
                if self.display_failing {
                    self.display_failing = false;
                    tracing::warn!(
                        monitors = live.len(),
                        "edge: the display is answering again; seamless detection resumes"
                    );
                }
                Some(live)
            }
            Err(error) => {
                if !self.display_failing {
                    self.display_failing = true;
                    tracing::warn!(
                        %error,
                        "edge: the display stopped reporting its monitors; seamless \
                         detection is suspended until it answers again"
                    );
                }
                None
            }
        }
    }

    /// Re-derive the crossing map for the monitors the platform now reports
    /// and adopt both together, without emitting anything.
    ///
    /// Infallible by construction: the list it derives from is the one the
    /// poll already read at the source's own fidelity, so there is no
    /// second query to fail and no way to end up holding new geometry with
    /// an old arrangement.
    fn rederive(&mut self, live: &[MonitorInfo], cursor: CursorPoint) {
        let map = Arc::new(self.source.derive(live));
        tracing::debug!(
            monitors = live.len(),
            spans = map.span_count(),
            "edge: crossing map re-derived for the new display configuration"
        );
        self.detector.adopt(map, live, cursor);
    }

    /// One poll: read the display, and emit a crossing if one just began.
    /// Returns `false` only when the crossings receiver is gone.
    async fn poll(&mut self) -> bool {
        let Some(live) = self.read_live() else {
            return true; // logged once per streak; the next tick retries
        };
        let cursor = match self.display.cursor_position() {
            Ok(cursor) => cursor,
            Err(error) => {
                tracing::debug!(%error, "edge poll: cursor position unavailable");
                return true;
            }
        };
        if self.detector.layout_changed(&live) {
            tracing::debug!(
                monitors = live.len(),
                "edge: the display configuration changed; re-deriving and re-priming"
            );
            self.rederive(&live, cursor);
            // Never a crossing on the tick the geometry moved: an interior
            // column can become a seam without the cursor moving at all.
            return true;
        }
        if let Some(detected) = self.detector.observe(cursor, &live) {
            // The monitor and cursor reads race a display change: each is
            // normalized to the virtual origin at its own call time, so a
            // change landing between them pairs the cursor with the wrong
            // origin — which can read as a seam touch from anywhere on
            // screen. A crossing is trusted only if the geometry is
            // unchanged when re-read after the cursor; otherwise it is
            // dropped, and the next tick observes the settled display and
            // re-derives.
            //
            // Re-read as bare rectangles whatever the source's fidelity:
            // the race is about coordinate normalization, which is purely
            // geometric, and this runs on the tick a crossing fires.
            //
            // The map is covered by the same re-read: it is a pure function
            // of the arrangement and these monitors, and only `rederive` —
            // which runs on a display change — ever replaces it.
            let before: Vec<MonitorRect> = live.iter().map(|monitor| monitor.rect).collect();
            match self.display.monitors() {
                Ok(after) if after == before => {}
                _ => {
                    tracing::debug!("edge: display changed mid-poll; crossing discarded");
                    return true;
                }
            }
            let Some(kind) = self.mode.crossing_kind() else {
                return true; // idle: nothing to emit (unreachable while polling)
            };
            tracing::debug!(
                ?kind,
                position = detected.position.value(),
                target_monitor = ?detected.target.monitor,
                target_edge = ?detected.target.edge,
                cursor_x = cursor.x,
                cursor_y = cursor.y,
                generation = self.generation,
                "edge: crossing detected"
            );
            if self
                .crossings_tx
                .send(EdgeCrossing {
                    kind,
                    crossing: detected,
                    generation: self.generation,
                })
                .await
                .is_err()
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::{mpsc, watch};
    use tokio::time::{sleep, timeout};

    use crossover_platform::fakes::FakeDisplay;
    use crossover_platform::{DisplayInfo, MonitorInfo};
    use crossover_topology::{
        DEVICE_ID_BYTES, DeviceId, DevicePair, Layout, LayoutRect, MonitorId, PlacedMonitor,
    };

    use super::{
        CrossingKind, DetectedCrossing, EdgeCrossing, EdgeDetector, EdgeMode, EdgeModeUpdate,
        REARM_MARGIN, edge_detect, implicit_crossing_source,
    };
    use crate::crossing::{CrossingMap, derive};
    use crate::topology::{
        CursorPoint, Edge, EdgeFraction, LinkSide, MonitorRect, Screen, Topology,
    };

    const LOCAL: DeviceId = DeviceId::from_bytes([0x11; DEVICE_ID_BYTES]);
    const PEER: DeviceId = DeviceId::from_bytes([0x22; DEVICE_ID_BYTES]);

    const HD: Screen = Screen {
        width: 1920,
        height: 1080,
    };

    /// The same 1920×1080 as [`HD`], as a one-monitor layout for the pure
    /// detector (which speaks monitors, not the [`FakeDisplay`]'s screen).
    const HD_MON: [MonitorRect; 1] = [MonitorRect {
        left: 0,
        top: 0,
        width: 1920,
        height: 1080,
    }];

    /// The soak layout: a laptop panel with an external monitor to its
    /// right, so the laptop's right column (x == 1919) is *interior* while
    /// the external is present and becomes the crossing seam when it goes.
    const LAPTOP_AND_EXTERNAL: [MonitorRect; 2] = [
        MonitorRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        },
        MonitorRect {
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
        },
    ];

    fn at(x: i32, y: i32) -> CursorPoint {
        CursorPoint { x, y }
    }

    /// Bare rectangles, exactly as a geometry-only source receives them —
    /// what [`CrossingSource::read`](super::CrossingSource::read) hands the
    /// implicit derivation, so a map built here and a map the driver
    /// re-derives from the fake describe the same screens.
    fn bare(rects: &[MonitorRect]) -> Vec<MonitorInfo> {
        rects
            .iter()
            .map(|rect| MonitorInfo {
                id: None,
                rect: *rect,
            })
            .collect()
    }

    /// The side model's arrangement over these screens, through the one
    /// constructor the worker uses — so every test below measures what the
    /// running worker measures, by construction rather than by two copies
    /// happening to agree.
    fn side_map(side: LinkSide, rects: &[MonitorRect]) -> Arc<CrossingMap> {
        Arc::new(implicit_crossing_source(side, LOCAL).derive(&bare(rects)))
    }

    /// A detector over the side model's arrangement for these screens.
    fn detector(side: LinkSide, rects: &[MonitorRect]) -> EdgeDetector {
        EdgeDetector::new(side_map(side, rects))
    }

    /// The revision every drawn test arrangement is built at — a value
    /// that is neither `0` (which would be indistinguishable from an
    /// implicit map's) nor `1` (which a stray increment could produce).
    const DRAWN_REVISION: u64 = 7;

    /// A drawn arrangement of `(device, id, x, y, width, height)`
    /// rectangles, for the multi-span shapes the side model cannot express.
    /// This is the *explicit* path, which does match by identity.
    fn drawn_map(
        placed: &[(DeviceId, &str, i32, i32, u32, u32)],
        live: &[MonitorInfo],
    ) -> Arc<CrossingMap> {
        let monitors = placed
            .iter()
            .map(|(device, id, x, y, width, height)| PlacedMonitor {
                device: *device,
                id: MonitorId::new(id).unwrap(),
                rect: LayoutRect {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                },
            })
            .collect();
        let pair = DevicePair::new(LOCAL, PEER).unwrap();
        let layout = Layout::new(DRAWN_REVISION, LOCAL, monitors, &pair).unwrap();
        Arc::new(derive(&layout, LOCAL, live))
    }

    /// One named live monitor.
    fn live(id: &str, left: i32, top: i32, width: u32, height: u32) -> MonitorInfo {
        MonitorInfo {
            id: Some(id.to_owned()),
            rect: MonitorRect {
                left,
                top,
                width,
                height,
            },
        }
    }

    // ---- the pure detector ----

    #[test]
    fn a_crossing_fires_once_on_arrival_not_while_pinned() {
        // Left member crosses on its right edge (x == 1919).
        let hd = bare(&HD_MON);
        let mut d = detector(LinkSide::Left, &HD_MON);
        // Away from the seam: nothing.
        assert!(d.observe(at(960, 540), &hd).is_none());
        // Arrival at the seam: one crossing.
        let crossing = d.observe(at(1919, 300), &hd);
        assert!(crossing.is_some());
        // Still pinned: no repeat.
        assert!(d.observe(at(1919, 300), &hd).is_none());
        assert!(d.observe(at(1919, 305), &hd).is_none());
        // Leaves and returns: fires again.
        assert!(d.observe(at(900, 305), &hd).is_none());
        assert!(d.observe(at(1919, 305), &hd).is_some());
    }

    /// What a detected crossing carries: everything a wire `EntryPoint`
    /// needs — the destination in the receiver's terms, the position, and
    /// the revision it was derived under — so the wiring builds the
    /// message without going back to the map (ADR 0018).
    ///
    /// An implicit arrangement names neither a peer device nor a peer
    /// monitor and carries revision 0: ADR 0018's *unaddressed* entry
    /// point, an honest absence rather than a fabricated id, and the same
    /// `0` `EntryPoint::unaddressed` puts on the wire.
    #[test]
    fn a_detected_crossing_names_its_destination() {
        let hd = bare(&HD_MON);
        let mut d = detector(LinkSide::Left, &HD_MON);
        assert!(d.observe(at(960, 540), &hd).is_none());
        let DetectedCrossing {
            target,
            position,
            layout_revision,
        } = d.observe(at(1919, 540), &hd).expect("crossing");
        assert_eq!(target.device, None, "the side model names no peer machine");
        assert_eq!(target.monitor, None, "the side model names no peer screen");
        assert_eq!(target.edge, Edge::Left, "the peer's facing edge");
        assert!((position.value() - 0.5).abs() < 0.01);
        assert_eq!(layout_revision, 0, "an implicit arrangement is revision 0");

        // A drawn arrangement names both, and they travel with the crossing
        // — along with the revision `drawn_map` built the layout at.
        let screens = [live("A", 0, 0, 1000, 1000)];
        let map = drawn_map(
            &[
                (LOCAL, "A", 0, 0, 1000, 1000),
                (PEER, "EAST", 1000, 0, 1000, 1000),
            ],
            &screens,
        );
        let mut d = EdgeDetector::new(map);
        assert!(d.observe(at(500, 500), &screens).is_none());
        let crossing = d.observe(at(999, 0), &screens).expect("a crossing");
        assert_eq!(crossing.target.device, Some(PEER));
        assert_eq!(
            crossing.target.monitor.as_ref().map(MonitorId::as_str),
            Some("EAST")
        );
        assert_eq!(crossing.target.edge, Edge::Left);
        assert!((crossing.position.value() - 0.0).abs() < 1e-9);
        assert_eq!(crossing.layout_revision, DRAWN_REVISION);
    }

    #[test]
    fn unplugging_the_edge_monitor_does_not_fire_under_a_stationary_cursor() {
        // Left member: the seam is the rightmost monitor's right column —
        // the external's while it is plugged in.
        let hd = bare(&HD_MON);
        let both = bare(&LAPTOP_AND_EXTERNAL);
        let mut d = detector(LinkSide::Left, &LAPTOP_AND_EXTERNAL);
        // Cursor on the laptop's right column: interior, not a seam.
        assert!(d.observe(at(1919, 540), &both).is_none());
        // The external is unplugged. The cursor has not moved, but the
        // column under it is suddenly the crossing seam — which must read
        // as moved geometry, never as an arrival, or an unplug would
        // transfer control by itself.
        assert!(d.observe(at(1919, 540), &hd).is_none());
        // Pinned there afterwards: still nothing.
        assert!(d.observe(at(1919, 540), &hd).is_none());
        // The driver re-derives the arrangement for the new geometry (the
        // async half of this is `a_mid_watch_unplug_...` below). Adopting it
        // under the same stationary cursor is still not an arrival.
        d.adopt(side_map(LinkSide::Left, &HD_MON), &hd, at(1919, 540));
        assert!(d.observe(at(1919, 540), &hd).is_none());
        // A genuine arrival on the new layout fires as usual.
        assert!(d.observe(at(900, 540), &hd).is_none());
        assert!(d.observe(at(1919, 540), &hd).is_some());
    }

    #[test]
    fn plugging_a_monitor_in_moves_the_edge_off_a_pinned_cursor() {
        // The plug-in direction never had a firing bug — the seam moves
        // *away* from the cursor — so the observable behavior here matches
        // the pre-fix detector. What this pins is layout adoption: the
        // change must be recognized and stored, or a later refactor could
        // lose the plug-in re-prime while the unplug tests stay green.
        let hd = bare(&HD_MON);
        let both = bare(&LAPTOP_AND_EXTERNAL);
        let mut d = detector(LinkSide::Left, &HD_MON);
        // Pinned at the single monitor's seam (fires once on arrival).
        assert!(d.observe(at(960, 540), &hd).is_none());
        assert!(d.observe(at(1919, 540), &hd).is_some());
        // The external arrives: the seam is now its far column, and the
        // pinned cursor is interior. No crossing, and the change stays
        // visible until the arrangement that goes with it is adopted.
        assert!(d.layout_changed(&both));
        assert!(d.observe(at(1919, 540), &both).is_none());
        assert!(d.layout_changed(&both));
        d.adopt(
            side_map(LinkSide::Left, &LAPTOP_AND_EXTERNAL),
            &both,
            at(1919, 540),
        );
        assert!(!d.layout_changed(&both));
        // ...so reaching the *new* seam fires.
        assert!(d.observe(at(3839, 540), &both).is_some());
    }

    /// The confirmed wedge, pinned: a stale snapshot may be cleared **only**
    /// by adopting the arrangement that goes with the new geometry. If
    /// priming (or observing) advanced it on its own, a caller whose
    /// re-derivation failed would be left measuring new geometry against an
    /// old map, with nothing left to notice — for the rest of the run.
    #[test]
    fn only_adopting_a_new_map_clears_a_stale_layout() {
        let both = bare(&LAPTOP_AND_EXTERNAL);
        let mut d = detector(LinkSide::Left, &HD_MON);
        assert!(d.layout_changed(&both));

        // Priming is about the cursor, and must leave the staleness alone —
        // this is the path a mode publication takes.
        d.prime(at(1919, 540));
        assert!(
            d.layout_changed(&both),
            "priming adopted a layout it had no map for"
        );
        // Observing, likewise, however many times it is retried.
        for _ in 0..3 {
            assert!(d.observe(at(1919, 540), &both).is_none());
            assert!(
                d.layout_changed(&both),
                "observing adopted a layout it had no map for"
            );
        }
        // Only the arrangement arriving resolves it.
        d.adopt(
            side_map(LinkSide::Left, &LAPTOP_AND_EXTERNAL),
            &both,
            at(1919, 540),
        );
        assert!(!d.layout_changed(&both));
        assert!(d.observe(at(1000, 540), &both).is_none());
        assert!(d.observe(at(3839, 540), &both).is_some());
    }

    /// A swapped arrangement moves every seam at once under a cursor that
    /// has not moved — the feature/107 invariant, extended from geometry to
    /// the map derived from it. Adopting one must re-prime, never fire.
    #[test]
    fn swapping_the_map_re_primes_and_never_fires() {
        // A right-member arrangement: the seam is x == 0, and the cursor
        // parked there is on it.
        let hd = bare(&HD_MON);
        let mut d = detector(LinkSide::Right, &HD_MON);
        d.prime(at(0, 500));
        assert!(d.observe(at(400, 500), &hd).is_none()); // arms it
        // The arrangement is redrawn while the cursor sits at x == 1919,
        // which the new one makes a seam and the old one did not.
        d.adopt(side_map(LinkSide::Left, &HD_MON), &hd, at(1919, 500));
        assert!(
            d.observe(at(1919, 500), &hd).is_none(),
            "a swapped arrangement fired under a stationary cursor"
        );
        assert!(d.observe(at(1919, 500), &hd).is_none());
        // Real travel away and back still crosses on the new arrangement.
        assert!(d.observe(at(900, 500), &hd).is_none());
        assert!(d.observe(at(1919, 500), &hd).is_some());
    }

    #[test]
    fn priming_suppresses_a_crossing_for_a_cursor_already_at_the_edge() {
        let hd = bare(&HD_MON);
        let mut d = detector(LinkSide::Right, &HD_MON); // crosses left, x == 0
        d.prime(at(0, 500));
        // Already at the seam when detection began: no crossing.
        assert!(d.observe(at(0, 500), &hd).is_none());
        // Only after leaving and returning does it fire.
        assert!(d.observe(at(400, 500), &hd).is_none());
        assert!(d.observe(at(0, 500), &hd).is_some());
    }

    /// The hardware bounce (ADR 0009 addendum, 2026-08-19): a transfer
    /// leaves the cursor resting on the crossing column, so a one- or
    /// two-pixel wobble there used to read as a fresh arrival and fire a
    /// complete reverse transfer. Only travel clear of the column by more
    /// than [`REARM_MARGIN`] re-arms the trigger.
    #[test]
    fn a_wobble_at_the_edge_does_not_cross_but_real_travel_does() {
        let margin = i32::try_from(REARM_MARGIN).unwrap();
        let column = 1919; // the left member's crossing column
        let hd = bare(&HD_MON);
        let mut d = detector(LinkSide::Left, &HD_MON);
        // Detection begins with the cursor parked on the column, exactly
        // where an entry placement leaves it.
        d.prime(at(column, 540));

        // Jitter of a pixel, and of the whole margin, is inert — however
        // often it repeats.
        for _ in 0..3 {
            assert!(d.observe(at(column - 1, 540), &hd).is_none());
            assert!(d.observe(at(column, 540), &hd).is_none());
            assert!(d.observe(at(column - margin, 540), &hd).is_none());
            assert!(d.observe(at(column, 540), &hd).is_none());
        }

        // One pixel past the margin is real travel: the next touch crosses.
        assert!(
            d.observe(at(column - margin - 1, 540), &hd).is_none(),
            "moving clear of the edge is not itself a crossing"
        );
        assert!(d.observe(at(column, 540), &hd).is_some());
        // And only once: the crossing disarms it again.
        assert!(d.observe(at(column, 540), &hd).is_none());
    }

    /// The same hysteresis on the mirrored side, where the crossing column
    /// is `x == 0` and clearing it means moving *right*.
    #[test]
    fn the_rearm_margin_applies_on_the_left_linked_edge_too() {
        let margin = i32::try_from(REARM_MARGIN).unwrap();
        let hd = bare(&HD_MON);
        let mut d = detector(LinkSide::Right, &HD_MON);
        d.prime(at(0, 500));
        assert!(d.observe(at(1, 500), &hd).is_none());
        assert!(d.observe(at(0, 500), &hd).is_none());
        assert!(d.observe(at(margin, 500), &hd).is_none());
        assert!(d.observe(at(0, 500), &hd).is_none());
        assert!(d.observe(at(margin + 1, 500), &hd).is_none());
        assert!(d.observe(at(0, 500), &hd).is_some());
    }

    /// The margin is perpendicular distance from the span's *own* edge, so
    /// it works vertically on a Top/Bottom seam exactly as it does
    /// horizontally — a shape the side model could not express at all.
    #[test]
    fn the_rearm_margin_is_vertical_on_a_top_or_bottom_seam() {
        let margin = i32::try_from(REARM_MARGIN).unwrap();
        let screens = [live("A", 0, 0, 1000, 1000)];
        for (edge, row, inward) in [(Edge::Bottom, 999, -1), (Edge::Top, 0, 1)] {
            let peer_y = if edge == Edge::Bottom { 1000 } else { -1000 };
            let map = drawn_map(
                &[
                    (LOCAL, "A", 0, 0, 1000, 1000),
                    (PEER, "OVER", 0, peer_y, 1000, 1000),
                ],
                &screens,
            );
            let mut d = EdgeDetector::new(map);
            d.prime(at(500, row));
            // Vertical jitter within the margin is inert...
            assert!(d.observe(at(500, row + inward), &screens).is_none());
            assert!(d.observe(at(500, row), &screens).is_none());
            assert!(
                d.observe(at(500, row + inward * margin), &screens)
                    .is_none()
            );
            assert!(d.observe(at(500, row), &screens).is_none());
            // ...and horizontal travel, however far, clears nothing.
            assert!(d.observe(at(0, row), &screens).is_none());
            assert!(d.observe(at(999, row), &screens).is_none());
            assert!(d.observe(at(500, row), &screens).is_none());
            // One row past the margin is real travel.
            assert!(
                d.observe(at(500, row + inward * (margin + 1)), &screens)
                    .is_none()
            );
            let crossing = d.observe(at(500, row), &screens).expect("a crossing");
            assert_eq!(crossing.target.edge, edge.opposite(), "{edge:?}");
        }
    }

    /// A deliberate crossing pays nothing for the hysteresis: a cursor
    /// crossing the screen is clear of the seam the whole way, so the very
    /// first observation that reaches the column fires.
    #[test]
    fn a_deliberate_crossing_still_fires_on_the_first_touch() {
        let hd = bare(&HD_MON);
        let mut d = detector(LinkSide::Left, &HD_MON);
        d.prime(at(100, 400));
        for x in [400, 900, 1400, 1800] {
            assert!(d.observe(at(x, 400), &hd).is_none());
        }
        assert!(d.observe(at(1919, 400), &hd).is_some());
    }

    // ---- per-span hysteresis (ADR 0018) ----

    /// One local screen with the peer drawn across two of its edges. An
    /// entry at the corner hugs both, so both must be left disarmed — the
    /// case a single global flag gets wrong in either direction.
    #[test]
    fn an_entry_at_a_corner_disarms_both_adjacent_spans() {
        let screens = [live("A", 0, 0, 1000, 1000)];
        let map = drawn_map(
            &[
                (LOCAL, "A", 0, 0, 1000, 1000),
                (PEER, "EAST", 1000, 0, 1000, 1000),
                (PEER, "SOUTH", 0, 1000, 1000, 1000),
            ],
            &screens,
        );
        assert_eq!(map.span_count(), 2);
        let mut d = EdgeDetector::new(map);

        // A transfer parks the cursor on the corner pixel: both edges hug.
        d.prime(at(999, 999));
        assert!(d.observe(at(999, 999), &screens).is_none());
        // Sliding up the right edge, and left along the bottom edge, stays
        // within the margin of one span or the other the whole way.
        for cursor in [at(999, 990), at(999, 999), at(990, 999), at(999, 999)] {
            assert!(
                d.observe(cursor, &screens).is_none(),
                "a corner hug fired at {cursor:?}"
            );
        }
        // Real travel inward from the corner arms both, and returning to
        // the right edge fires exactly the east span, once.
        assert!(d.observe(at(500, 500), &screens).is_none());
        let crossing = d.observe(at(999, 500), &screens).expect("a crossing");
        assert_eq!(
            crossing.target.monitor.as_ref().map(MonitorId::as_str),
            Some("EAST")
        );
        assert!(d.observe(at(999, 500), &screens).is_none());
    }

    /// Two peer screens stacked across one local edge: sliding along that
    /// hugged edge from one span into its neighbour must not fire the
    /// neighbour. Lateral motion along a hugged edge clears nothing, so
    /// nothing arms — which is why the lateral margin is measured against
    /// the monitor's whole edge and not against each span's share of it.
    #[test]
    fn sliding_along_a_hugged_edge_never_fires_the_neighbouring_span() {
        let screens = [live("A", 0, 0, 1000, 1000)];
        let map = drawn_map(
            &[
                (LOCAL, "A", 0, 0, 1000, 1000),
                (PEER, "UPPER", 1000, 0, 1000, 500),
                (PEER, "LOWER", 1000, 500, 1000, 500),
            ],
            &screens,
        );
        assert_eq!(map.span_count(), 2);
        let mut d = EdgeDetector::new(map);

        // An entry places the cursor in the upper span's share of the seam.
        d.prime(at(999, 100));
        // Sliding the whole length of the column, across the span boundary
        // and back, fires nothing at all.
        for y in [200, 400, 499, 500, 501, 700, 999, 0, 100] {
            assert!(
                d.observe(at(999, y), &screens).is_none(),
                "a lateral slide fired at row {y}"
            );
        }

        // The same slide *after* a genuine crossing is equally inert: a
        // crossing leaves the cursor parked on the seam exactly as an entry
        // placement does, so it leaves the same state behind.
        assert!(d.observe(at(500, 100), &screens).is_none()); // arms both
        assert!(d.observe(at(999, 100), &screens).is_some()); // UPPER fires
        for y in [300, 499, 500, 600, 999] {
            assert!(
                d.observe(at(999, y), &screens).is_none(),
                "a slide after a crossing fired at row {y}"
            );
        }
    }

    /// Spans are independent: jitter at one seam must not disturb a span
    /// the cursor is nowhere near. A single global flag would have the
    /// jitter re-arm the far span (travel away from *it*) and a crossing
    /// there disarm this one.
    #[test]
    fn jitter_at_one_span_leaves_a_distant_span_armed() {
        // The peer drawn on both sides of one local screen: two seams a
        // whole desktop apart.
        let screens = [live("A", 0, 0, 1000, 1000)];
        let map = drawn_map(
            &[
                (LOCAL, "A", 0, 0, 1000, 1000),
                (PEER, "WEST", -1000, 0, 1000, 1000),
                (PEER, "EAST", 1000, 0, 1000, 1000),
            ],
            &screens,
        );
        assert_eq!(map.span_count(), 2);
        let mut d = EdgeDetector::new(map);

        // Parked on the east seam: east disarmed, west armed (the cursor is
        // a whole screen clear of it).
        d.prime(at(999, 500));
        for x in [998, 999, 999 - i32::try_from(REARM_MARGIN).unwrap(), 999] {
            assert!(
                d.observe(at(x, 500), &screens).is_none(),
                "east jitter fired at x = {x}"
            );
        }
        // The west seam is untouched by any of that: reaching it crosses on
        // the first touch.
        let crossing = d.observe(at(0, 500), &screens).expect("a crossing");
        assert_eq!(
            crossing.target.monitor.as_ref().map(MonitorId::as_str),
            Some("WEST")
        );
        // The reverse, too: crossing at the west seam did not disarm the
        // east one, and the trip across the desktop is real travel clear of
        // it, so returning to the east seam crosses there — once.
        let crossing = d.observe(at(999, 500), &screens).expect("a crossing");
        assert_eq!(
            crossing.target.monitor.as_ref().map(MonitorId::as_str),
            Some("EAST")
        );
        assert!(d.observe(at(999, 500), &screens).is_none());
    }

    /// The lateral half of the margin: a cursor on a *different* screen
    /// that happens to line up with a seam's column must not hold that seam
    /// disarmed. Perpendicular distance alone cannot tell "hugging the
    /// seam" from "working on the screen below it", and getting that wrong
    /// means the seam never arms while the user is down there — so it does
    /// not fire when they finally arrive at it.
    #[test]
    fn a_distant_aligned_monitor_does_not_suppress_arming() {
        // Two of this machine's screens stacked, with the peer drawn
        // against the *upper* one's right edge only.
        let screens = [
            live("UPPER", 0, 0, 1000, 1000),
            live("LOWER", 0, 1000, 1000, 1000),
        ];
        let map = drawn_map(
            &[
                (LOCAL, "UPPER", 0, 0, 1000, 1000),
                (LOCAL, "LOWER", 0, 1000, 1000, 1000),
                (PEER, "EAST", 1000, 0, 1000, 1000),
            ],
            &screens,
        );
        assert_eq!(map.span_count(), 1, "only the upper screen carries a seam");
        let mut d = EdgeDetector::new(map);

        // An entry parks the cursor on the seam, disarming it...
        d.prime(at(999, 500));
        assert!(d.observe(at(999, 500), &screens).is_none());
        // ...then the user works on the lower screen, right up against its
        // own right-hand column — the same x as the seam, a whole screen
        // away from it. That is genuine travel away, so it must arm.
        for y in [1000, 1200, 1500, 1999] {
            assert!(
                d.observe(at(999, y), &screens).is_none(),
                "the lower screen fired a crossing at row {y}"
            );
        }
        // Arriving back at the seam therefore crosses, on the first touch.
        assert!(
            d.observe(at(999, 500), &screens).is_some(),
            "a cursor on the screen below held the seam disarmed"
        );
    }

    // ---- equivalence with the side model ----

    /// The pre-0018 detector, in miniature: one linked edge, one armed
    /// flag, and the same re-prime-on-layout-change rule. This *is* the
    /// code this branch replaces, kept here as the thing the span detector
    /// is required to agree with.
    struct Reference {
        topology: Topology,
        armed: bool,
        layout: Vec<MonitorRect>,
    }

    impl Reference {
        fn new(side: LinkSide) -> Self {
            Self {
                topology: Topology::new(side),
                armed: false,
                layout: Vec::new(),
            }
        }

        fn prime(&mut self, cursor: CursorPoint, monitors: &[MonitorRect]) {
            self.armed = self.topology.clear_of_edge(cursor, monitors, REARM_MARGIN);
            self.layout = monitors.to_vec();
        }

        fn observe(
            &mut self,
            cursor: CursorPoint,
            monitors: &[MonitorRect],
        ) -> Option<EdgeFraction> {
            if self.layout.as_slice() != monitors {
                self.prime(cursor, monitors);
                return None;
            }
            if self.topology.clear_of_edge(cursor, monitors, REARM_MARGIN) {
                self.armed = true;
            }
            let touching = self.topology.leaving(cursor, monitors);
            if touching.is_some() && self.armed {
                self.armed = false;
                touching
            } else {
                None
            }
        }
    }

    /// The compatibility claim as a behavioural equality, not a geometric
    /// one: run the same cursor script through the span detector and
    /// through a detector built on the old [`Topology`] plus a single armed
    /// flag — the code this branch replaces — and require the same verdict
    /// and the same fraction at every step.
    ///
    /// The script is deliberately nasty: entry placements, sub-margin
    /// wobbles, exact-margin travel, one pixel past it, coordinates beyond
    /// the desktop where the OS clamps, and an unplug in the middle.
    #[test]
    fn the_span_detector_reproduces_the_side_model_across_a_cursor_script() {
        // The soak layout: a tall laptop panel and a shorter external 4K,
        // so the bounding box and the edge monitor disagree.
        let laptop = MonitorRect {
            left: 0,
            top: 0,
            width: 3840,
            height: 2400,
        };
        let external = MonitorRect {
            left: 3840,
            top: 0,
            width: 3840,
            height: 2160,
        };
        let both = [laptop, external];
        let alone = [laptop];
        let both_live = bare(&both);
        let alone_live = bare(&alone);

        for side in [LinkSide::Left, LinkSide::Right] {
            let mut spans = detector(side, &both);
            let mut reference = Reference::new(side);
            // Both begin primed on the same cursor, the way the driver
            // primes on the mode publication.
            let start = at(1000, 1000);
            spans.prime(start);
            reference.prime(start, &both);

            let margin = i32::try_from(REARM_MARGIN).unwrap();
            let script = [
                at(1000, 1000),
                at(0, 1000), // the left seam
                at(1, 1000), // a one-pixel wobble
                at(0, 1000),
                at(margin, 1000), // exactly the margin
                at(0, 1000),
                at(margin + 1, 1000), // one past it: real travel
                at(0, 1000),
                at(0, 1000),
                at(3839, 1000), // the interior seam
                at(3840, 1000),
                at(7679, 1000), // the right seam
                at(7678, 1000),
                at(7679, 1000),
                at(7679 - margin - 1, 1000),
                at(7679, 1000),
                at(-5, 1000), // clamped past the desktop
                at(9000, 1000),
                at(3000, 0),
                at(3000, 2399),
                at(0, 2399),
                at(7679, 2159),
            ];

            for (step, cursor) in script.into_iter().enumerate() {
                let mine = spans.observe(cursor, &both_live);
                let theirs = reference.observe(cursor, &both);
                assert_eq!(
                    mine.is_some(),
                    theirs.is_some(),
                    "{side:?} step {step} at {cursor:?}: span detector said \
                     {mine:?}, the side model said {theirs:?}"
                );
                if let (Some(mine), Some(theirs)) = (mine, theirs) {
                    assert!(
                        (mine.position.value() - theirs.value()).abs() < 1e-9,
                        "{side:?} step {step}: {} vs {}",
                        mine.position.value(),
                        theirs.value()
                    );
                    assert_eq!(mine.target.device, None);
                    assert_eq!(mine.target.monitor, None);
                }
            }

            // The unplug, on both, followed by a genuine arrival on the new
            // geometry — the one place the span detector needs the map
            // rebuilt, which the driver does for it.
            assert!(spans.observe(at(3839, 1000), &alone_live).is_none());
            assert!(reference.observe(at(3839, 1000), &alone).is_none());
            spans.adopt(side_map(side, &alone), &alone_live, at(3839, 1000));
            let column = match side {
                LinkSide::Left => 3839,
                LinkSide::Right => 0,
            };
            for cursor in [at(1000, 1000), at(column, 1000)] {
                let mine = spans.observe(cursor, &alone_live);
                let theirs = reference.observe(cursor, &alone);
                assert_eq!(mine.is_some(), theirs.is_some(), "{side:?} after unplug");
            }
        }
    }

    // ---- the async driver ----
    //
    // The driver polls a real clock, so these tests start the cursor in
    // the middle of the screen (away from any seam) and use short sleeps
    // to let a poll observe an intermediate position before the next move
    // — the same way a real cursor passes through the screen before
    // reaching the edge.

    const MIDDLE: CursorPoint = CursorPoint { x: 960, y: 540 };
    /// A few poll intervals — long enough for the driver to prime on the
    /// current cursor and for one poll to land.
    const SETTLE: Duration = Duration::from_millis(40);

    async fn next_crossing(rx: &mut mpsc::Receiver<EdgeCrossing>) -> EdgeCrossing {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a crossing")
            .expect("crossings channel closed")
    }

    /// The control wiring's half of the mode channel: publishes a mode
    /// under a fresh generation, exactly as the control driver does.
    struct Modes {
        tx: watch::Sender<EdgeModeUpdate>,
        generation: u64,
    }

    impl Modes {
        /// Publish `mode` and return the generation it went out under.
        fn set(&mut self, mode: EdgeMode) -> u64 {
            self.generation += 1;
            let _ = self.tx.send_replace(EdgeModeUpdate {
                mode,
                generation: self.generation,
            });
            self.generation
        }
    }

    fn rig(side: LinkSide) -> (Arc<FakeDisplay>, Modes, mpsc::Receiver<EdgeCrossing>) {
        let display = Arc::new(FakeDisplay::new(HD));
        display.set_cursor(MIDDLE); // away from either seam to start
        // The worker's own wiring: one construction of the implicit source,
        // one derivation from it for the initial map.
        let source = implicit_crossing_source(side, LOCAL);
        let map = Arc::new(source.derive(&source.read(&*display).unwrap()));
        let (driver, mode_tx, crossings_rx) = edge_detect(
            Arc::clone(&display) as Arc<dyn DisplayInfo>,
            map,
            source,
            Duration::from_millis(5),
        );
        tokio::spawn(driver.run());
        (
            display,
            Modes {
                tx: mode_tx,
                generation: 0,
            },
            crossings_rx,
        )
    }

    #[tokio::test]
    async fn leaving_mode_emits_a_leave_at_the_linked_edge() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        modes.set(EdgeMode::Leaving);
        sleep(SETTLE).await; // primes on the middle cursor: away from the seam
        display.set_cursor(at(1919, 540)); // right edge, half-way down
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
        assert!((crossing.crossing.position.value() - 0.5).abs() < 0.01);
        assert_eq!(crossing.crossing.target.device, None);
        assert_eq!(crossing.crossing.target.edge, Edge::Left);
    }

    #[tokio::test]
    async fn returning_mode_emits_a_return() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Right); // crosses left
        modes.set(EdgeMode::Returning);
        sleep(SETTLE).await;
        display.set_cursor(at(0, 270)); // left edge
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Return);
    }

    /// Every crossing is stamped with the generation of the mode it was
    /// detected under, so a consumer can tell a crossing detected under the
    /// current control state from one that queued behind a state change.
    #[tokio::test]
    async fn a_crossing_carries_the_mode_generation_it_was_detected_under() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        modes.set(EdgeMode::Leaving);
        sleep(SETTLE).await;
        let generation = modes.set(EdgeMode::Returning);
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Return);
        assert_eq!(crossing.generation, generation);
    }

    /// The mode is a level on a coalescing channel, so a burst may collapse
    /// — but never past its end. The detector must land on the *last* mode
    /// published, primed and stamped with that publication's generation,
    /// whatever the intermediate ones were.
    #[tokio::test]
    async fn a_burst_of_modes_leaves_the_detector_primed_for_the_last_one() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        // Published back to back, with nothing awaited in between: the
        // detector may well see only the final value.
        modes.set(EdgeMode::Leaving);
        modes.set(EdgeMode::Idle);
        modes.set(EdgeMode::Returning);
        let generation = modes.set(EdgeMode::Leaving);
        sleep(SETTLE).await; // primes on the middle cursor

        display.set_cursor(at(1919, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(
            crossing.kind,
            CrossingKind::Leave,
            "the detector kept a superseded mode from the burst"
        );
        assert_eq!(
            crossing.generation, generation,
            "the crossing was stamped with a generation from mid-burst"
        );
    }

    /// The same, ending on `Idle`: coalescing must not resurrect the
    /// watching mode a burst passed through on its way to stopping.
    #[tokio::test]
    async fn a_burst_ending_idle_stops_the_detector() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        modes.set(EdgeMode::Leaving);
        modes.set(EdgeMode::Idle);
        sleep(SETTLE).await;
        display.set_cursor(at(900, 540));
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540));
        let quiet = timeout(Duration::from_millis(200), crossings.recv()).await;
        assert!(quiet.is_err(), "a burst that ended idle still emitted");
    }

    #[tokio::test]
    async fn idle_mode_never_emits_even_at_the_edge() {
        let (display, _modes, mut crossings) = rig(LinkSide::Left);
        // No mode set (defaults to Idle). Park the cursor at the seam.
        display.set_cursor(at(1919, 540));
        let quiet = timeout(Duration::from_millis(200), crossings.recv()).await;
        assert!(quiet.is_err(), "idle driver emitted a crossing");
    }

    #[tokio::test]
    async fn a_cursor_already_at_the_edge_when_watching_begins_does_not_fire() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        // Cursor at the seam *before* the mode turns on (primed there).
        display.set_cursor(at(1919, 540));
        modes.set(EdgeMode::Leaving);
        let quiet = timeout(Duration::from_millis(150), crossings.recv()).await;
        assert!(quiet.is_err(), "fired on a cursor already at the edge");
        // A fresh arrival does fire: leave the edge, let a poll see it,
        // then return.
        display.set_cursor(at(900, 540));
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
    }

    #[tokio::test]
    async fn a_mid_watch_unplug_emits_no_crossing_until_a_fresh_arrival() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        display.set_monitors(LAPTOP_AND_EXTERNAL.to_vec());
        modes.set(EdgeMode::Leaving);
        sleep(SETTLE).await; // primes: middle of the laptop, away from any seam
        // Park the cursor on the laptop's right column — interior while the
        // external monitor is present — and let a poll observe it there.
        display.set_cursor(at(1919, 540));
        sleep(SETTLE).await;
        // Unplug the external: the parked cursor is now on the seam.
        display.set_monitors(vec![HD_MON[0]]);
        let quiet = timeout(Duration::from_millis(200), crossings.recv()).await;
        assert!(quiet.is_err(), "an unplug fired a crossing by itself");
        // The user moving away and back to the (new) seam still crosses,
        // which is only true if the map was re-derived for the new layout.
        display.set_cursor(at(900, 540));
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
    }

    /// A screen the platform declines to name is ordinary hardware — a USB
    /// display adapter, a docking station — and the side model never asked
    /// what anything was called. Seamless transfer must keep working across
    /// a re-enumeration that loses every device string.
    #[tokio::test]
    async fn a_display_that_names_nothing_still_crosses() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        modes.set(EdgeMode::Leaving);
        sleep(SETTLE).await;
        // The screen comes back a different size and with no id at all.
        display.set_monitor_layout(vec![MonitorInfo {
            id: None,
            rect: MonitorRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1200,
            },
        }]);
        sleep(SETTLE).await;
        display.set_cursor(at(900, 700));
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 700));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
    }

    /// A display outage across a configuration change must be *retried*,
    /// not absorbed. The driver primes on every mode publication, and if
    /// priming could advance its snapshot without a map to go with it, a
    /// read that failed at exactly that moment would leave the detector
    /// measuring the new geometry against the old arrangement for the rest
    /// of the run — silently.
    #[tokio::test]
    async fn a_display_outage_across_a_layout_change_is_retried_not_wedged() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        // The geometry moves and the display stops answering, before the
        // mode publication that will try to prime against it.
        display.set_monitors(LAPTOP_AND_EXTERNAL.to_vec());
        display.fail_with("no display");
        modes.set(EdgeMode::Leaving);
        sleep(SETTLE).await;

        // Nothing fires while the display is dark, whatever the cursor does.
        display.set_cursor(at(3839, 540));
        let quiet = timeout(Duration::from_millis(100), crossings.recv()).await;
        assert!(
            quiet.is_err(),
            "emitted a crossing while the display was dark"
        );

        // The display comes back. The driver must notice the geometry it
        // never managed to adopt, re-derive for it, and cross on the new
        // outer edge — the external's far column, not the laptop's.
        display.clear_failure();
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540)); // interior now: no crossing
        sleep(SETTLE).await;
        let quiet = timeout(Duration::from_millis(100), crossings.recv()).await;
        assert!(quiet.is_err(), "the old arrangement's seam still fired");
        display.set_cursor(at(3839, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
    }

    #[tokio::test]
    async fn a_display_failure_is_survived_without_a_crossing() {
        let (display, mut modes, mut crossings) = rig(LinkSide::Left);
        display.fail_with("no display");
        modes.set(EdgeMode::Leaving);
        display.set_cursor(at(1919, 540));
        // The driver keeps polling, gets errors, and emits nothing — no
        // panic, no crossing.
        let quiet = timeout(Duration::from_millis(200), crossings.recv()).await;
        assert!(quiet.is_err(), "emitted a crossing despite display failure");
    }
}
