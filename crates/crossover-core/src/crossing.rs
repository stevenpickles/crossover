//! Crossing spans derived from a drawn layout (ADR 0018).
//!
//! The drawn arrangement answers exactly one question: **which peer monitor
//! lies across which of my edges, and where along it.** This module is that
//! answer, computed once per (layout, live geometry) pair and then queried
//! by the detector: for each edge of each local monitor, the intervals of
//! that edge a peer monitor abuts, and where a cursor crossing each interval
//! arrives on the far side.
//!
//! Nothing here does I/O. This *is* what the running detector measures
//! against: [`crate::edge_driver`] holds a [`CrossingMap`] and one armed
//! flag per [`SpanId`]. A `--left`/`--right` run reaches it through
//! [`from_link_side`], which expresses the side model *as* a layout — one
//! span on one edge, going somewhere unaddressed — so the swap onto this
//! model cost the side model no behaviour at all. What has **not** moved
//! yet is the far end: the control wiring still places an arriving cursor
//! through [`crate::topology::Topology`] rather than through
//! [`CrossingMap::arrive`], and still sends a bare fraction rather than an
//! `EntryPoint`.
//!
//! # Two coordinate spaces, and the fraction between them
//!
//! A layout coordinate is **not a pixel**, and this module never treats it
//! as one. Local geometry — where the cursor is, which monitor it is on,
//! how long an edge is — comes from the live [`MonitorInfo`] the platform
//! reports; the layout supplies only adjacency and proportion. The two are
//! joined by monitor id, and everything that crosses between them is a
//! fraction:
//!
//! **Leaving** — a live pixel offset along the local edge → a fraction of
//! that live edge → the matching coordinate on the *drawn* edge → the span
//! containing it → a fraction of the **target monitor's whole drawn facing
//! edge** ([`EdgeFraction`], which is what travels on the wire).
//!
//! **Entering** — an [`EdgeFraction`] against the named local monitor's
//! **live** facing edge → a pixel.
//!
//! Every step is a ratio, so units cancel at the first one and no scale
//! factor ever enters: leaving at 40 % of a 4K edge arrives at 40 % of the
//! adjacent 1080p edge because the arithmetic cannot express anything else.
//! That is ADR 0018's mixed-DPI criterion holding by construction rather
//! than by care. The drawn rectangle decides only *where the seam lies* and
//! *what proportion of the far edge* a crossing lands at — never the
//! arrival pixel, which is the destination's own live geometry's business.
//!
//! All per-edge pixel arithmetic — the distance to an edge, the offset
//! along one, the pixel a fraction lands on — is [`Edge`]'s, shared with
//! the side model rather than reimplemented here. Two implementations of
//! "which column does the cursor ride" could drift a pixel apart at exactly
//! the seam where a wrong answer hands control away.
//!
//! # What the derivation refuses to invent
//!
//! - **Abutment is exact**, with zero tolerance. A one-unit gap is a gap.
//!   Snapping is the editor's job, where the user can see it happen.
//! - **Same-machine seams produce nothing, by construction**: only rects
//!   belonging to the *other* device are searched. A seam between two of
//!   this machine's screens is a crossing exactly when the user drew the
//!   peer across it, and never otherwise.
//! - **Spans are half-open** `[start, end)`, so a corner where three
//!   monitors meet is decided by arithmetic rather than by enumeration
//!   order.
//! - **Touching an edge means being *on* it**, not being anywhere past it.
//!   The side model could read "at or beyond" as touching because its one
//!   edge was the outer edge of the whole desktop, with nothing of this
//!   machine's past it. A drawn arrangement has interior seams, and a
//!   cursor beyond one of those is simply on the screen across it. The one
//!   surviving "beyond" case is the OS clamping a cursor against the
//!   desktop's outer boundary, which both [`CrossingMap::touching`] and
//!   [`CrossingMap::clear_of`] treat as the side model always has.
//! - **A monitor the platform would not name, or named ambiguously, has no
//!   spans and keeps its geometry.** An unknown id degrades placement,
//!   never the rectangle the detector measures against
//!   (`crossover_platform::display`).
//! - **A destination this machine cannot name is `None`, not a fiction.**
//!   [`CrossTarget::monitor`] is an `Option`, and the unaddressed case
//!   is the wire's `EntryPoint::unaddressed`: the receiver places against
//!   its own desktop bounds. Nothing here invents a device string to fill
//!   the hole, because a fabricated id is worse than none — it can match
//!   the wrong screen.
//!
//! # This runs on peer-influenced input
//!
//! The [`Layout`] arrives from the peer, and the live rectangles arrive
//! from the OS. Every derivation and every lookup here is total: all
//! arithmetic is `i64` or saturating, and there is no input — a validated
//! layout with any live rectangles and any cursor at all — that panics
//! (NFR-1).

use crossover_platform::{CursorPoint, MonitorInfo, MonitorRect};
use crossover_topology::{
    DeviceId, DevicePair, Layout, LayoutError, LayoutRect, MonitorId, RawPlacedMonitor,
};

use crate::topology::{Edge, EdgeFraction, LinkSide, edge_monitor_index, last_index};

/// A half-open interval `[start, end)` along one axis of the shared layout
/// space, in layout units (ADR 0018).
///
/// Half-open is the whole reason a three-way corner is deterministic: the
/// coordinate two abutting monitors share belongs to exactly one of them,
/// decided by `<` rather than by which was enumerated first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutSpan {
    /// The first coordinate in the interval.
    pub start: i64,
    /// One past the last coordinate in the interval.
    pub end: i64,
}

impl LayoutSpan {
    /// How many coordinates the interval holds; `0` if it is empty or
    /// inverted (which nothing here constructs, but which costs one `max`
    /// to make unrepresentable in the arithmetic below).
    #[must_use]
    pub fn length(self) -> i64 {
        self.end.saturating_sub(self.start).max(0)
    }

    /// Is `coordinate` in `[start, end)`?
    #[must_use]
    pub fn contains(self, coordinate: i64) -> bool {
        coordinate >= self.start && coordinate < self.end
    }

    /// How far along this interval `coordinate` sits, as a fraction of the
    /// whole of it.
    ///
    /// Delegates to [`EdgeFraction::from_pixel`], which is the definition
    /// of the convention — endpoints at `0.0` and `1.0`, everything outside
    /// clamped in — rather than restating it. The two must agree exactly:
    /// `from_pixel` is what turns a live pixel into a fraction on the way
    /// out, and this is what turns a layout coordinate into one on the way
    /// in, so a difference between them would be a crossing that does not
    /// come home.
    #[must_use]
    pub fn fraction_at(self, coordinate: i64) -> EdgeFraction {
        // Inside a validated layout every coordinate is under 2^25, so both
        // narrowings are exact; the saturating fallbacks only keep the
        // function total for values a `Layout` cannot hold.
        let offset = i32::try_from(coordinate.saturating_sub(self.start)).unwrap_or(i32::MAX);
        let extent = u32::try_from(self.length()).unwrap_or(u32::MAX);
        EdgeFraction::from_pixel(offset, extent)
    }

    /// The overlap of two intervals, or `None` if they merely touch or miss
    /// entirely. Touching is not overlapping — that is what makes two
    /// stacked peer monitors produce two spans rather than one ambiguous
    /// coordinate.
    fn intersect(self, other: Self) -> Option<Self> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start < end).then_some(Self { start, end })
    }
}

/// Where a crossing lands, in the **receiver's** terms (ADR 0018,
/// docs/PROTOCOL.md §6.1): the machine and monitor the cursor arrives on,
/// and which of that monitor's edges it arrives at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossTarget {
    /// The machine the cursor arrives on — the peer, since a span exists
    /// only for a local-to-peer pair — or `None` when the arrangement
    /// names no peer at all.
    ///
    /// `None` is what an **implicit** (side-model) arrangement produces,
    /// and it is an absence rather than a synthesized id on purpose:
    /// `--left` says which end of a pair this machine is and nothing
    /// whatever about the other end, so there is no peer identity to
    /// report. A fabricated one would be indistinguishable, downstream,
    /// from a device a drawn layout actually named — and the first consumer
    /// to treat it as truth would be routing by a fiction.
    pub device: Option<DeviceId>,
    /// The monitor it arrives on, or `None` when the arrangement cannot
    /// name one.
    ///
    /// `None` is the wire's **unaddressed** entry point: the receiver
    /// places the cursor against its own desktop-bounds edge matching
    /// [`edge`](Self::edge), with a diagnostic, and the grant proceeds
    /// normally either way. It is what an *implicit* layout produces
    /// ([`from_link_side`]), because the side model never learned anything
    /// about the peer's screens and a made-up device string would be worse
    /// than an honest absence — it could match a real screen that is not
    /// the one meant.
    pub monitor: Option<MonitorId>,
    /// Which of that monitor's edges it arrives at — the edge facing the
    /// one it left ([`Edge::opposite`]).
    pub edge: Edge,
}

/// One crossing span: an interval of one edge of one local monitor, and
/// where a cursor crossing it goes (ADR 0018).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossSpan {
    /// Which local monitor this edge belongs to, as an index into
    /// [`CrossingMap::monitors`] of the map that produced it.
    monitor: usize,
    /// Which of that monitor's edges the span lies on.
    edge: Edge,
    /// The interval of that edge the span covers, in layout units.
    span: LayoutSpan,
    /// Where a cursor crossing it arrives.
    target: CrossTarget,
    /// The target monitor's **whole** drawn facing edge, in layout units.
    ///
    /// The denominator of the arrival fraction, and deliberately the whole
    /// edge rather than [`span`](Self::span): a crossing two thirds of the
    /// way down a partially-overlapping neighbour must arrive two thirds of
    /// the way down *that neighbour*, not two thirds of the way down the
    /// shared sliver.
    target_edge: LayoutSpan,
}

impl CrossSpan {
    /// Which local monitor the span sits on — an index into
    /// [`CrossingMap::monitors`].
    #[must_use]
    pub fn monitor(&self) -> usize {
        self.monitor
    }

    /// Which of that monitor's edges it lies on.
    #[must_use]
    pub fn edge(&self) -> Edge {
        self.edge
    }

    /// The interval of that edge it covers, in layout units, half-open.
    #[must_use]
    pub fn span(&self) -> LayoutSpan {
        self.span
    }

    /// Where a cursor crossing it arrives.
    #[must_use]
    pub fn target(&self) -> &CrossTarget {
        &self.target
    }

    /// The target monitor's whole drawn facing edge — see the field's own
    /// note for why the whole edge rather than the overlap.
    #[must_use]
    pub fn target_edge(&self) -> LayoutSpan {
        self.target_edge
    }
}

/// One live monitor as the map sees it: the rectangle the detector
/// measures against, the drawn rectangle it was matched to, and its spans.
///
/// **Every** live monitor gets one of these, in the order the platform
/// reported them, whether or not the layout knows it. An entry with no
/// `drawn` rectangle — the platform would not name it, it named it
/// ambiguously, its id is not one the layout can address, or the user
/// simply never drew it — is a real screen with no crossings on it, which
/// is a legal arrangement rather than an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedMonitor {
    id: Option<MonitorId>,
    live: MonitorRect,
    drawn: Option<LayoutRect>,
}

impl MappedMonitor {
    /// The device string a layout and an arriving entry point address this
    /// monitor by.
    ///
    /// `None` when the platform would not name it, named it something
    /// unusable, or named it the same as another live monitor — all three
    /// are "this rectangle cannot be addressed", and all three cost only
    /// placement.
    #[must_use]
    pub fn id(&self) -> Option<&MonitorId> {
        self.id.as_ref()
    }

    /// Its live pixel bounds, exactly as the platform reported them.
    #[must_use]
    pub fn live(&self) -> MonitorRect {
        self.live
    }

    /// The rectangle the user drew for it, if the layout places one.
    #[must_use]
    pub fn drawn(&self) -> Option<LayoutRect> {
        self.drawn
    }

    /// Is `cursor` on this monitor — inside it, or on its boundary?
    ///
    /// Expressed through [`Edge::inset_of`] rather than through a fresh
    /// comparison, so "inside" and "how far from the edge" cannot disagree
    /// about the last pixel.
    fn holds(&self, cursor: CursorPoint) -> bool {
        Edge::ALL
            .into_iter()
            .all(|edge| edge.inset_of(self.live, cursor) >= 0)
    }
}

/// Identifies one span within the map that produced it.
///
/// Valid only for that map. A map is rebuilt whenever the layout or the
/// live geometry changes, and the detector re-primes across such a change
/// anyway — the same discipline `EdgeDetector` already applies to a layout
/// change — so an id never outlives the geometry it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpanId(usize);

impl SpanId {
    /// The id of the span at `index` — the inverse of
    /// [`SpanId::index`], for a caller walking `0..span_count()` to build
    /// its per-span state.
    ///
    /// An index past the end simply names no span, and every lookup that
    /// takes an id answers that case rather than panicking on it.
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        Self(index)
    }

    /// The span's position in [`CrossingMap::spans`] — a dense `0..n`
    /// index, so per-span state (the armed flag ADR 0018 makes per-span)
    /// can be a plain vector rather than a map.
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

/// A crossing about to be handed to the peer: which span fired, and how far
/// along the destination edge.
///
/// Deliberately all-`Copy`: this is produced on the detector's poll path,
/// and the destination it names — a device string — is fetched by reference
/// from the map at the moment a message is actually built
/// ([`CrossingMap::span`] then [`CrossSpan::target`]), rather than cloned
/// on every poll that happens to touch an edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Departure {
    /// The span the cursor crossed.
    pub span: SpanId,
    /// How far along the target's facing edge, `[0, 1]`.
    pub fraction: EdgeFraction,
}

/// Every crossing this machine can make, given an arrangement and the
/// monitors it currently has (ADR 0018).
///
/// Holds live geometry and derived spans together, because a crossing is
/// only answerable with both: the layout says *where* the seams are, the
/// live rectangles say *which pixels* those seams are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossingMap {
    local: DeviceId,
    monitors: Vec<MappedMonitor>,
    spans: Vec<CrossSpan>,
}

impl CrossingMap {
    /// This machine's identity — the device whose monitors the map is
    /// written from.
    #[must_use]
    pub fn local(&self) -> DeviceId {
        self.local
    }

    /// A map of these live monitors with **no crossings at all**: the
    /// geometry intact, seamless transfer off.
    ///
    /// ADR 0018's degradation is "log and cross nowhere", never a guess and
    /// never a panic, and a caller that has a detector already running
    /// needs something to hand it: an inert map stops every crossing while
    /// leaving the rectangles the detector measures against exactly as the
    /// platform reported them. Identity is still resolved, on the same
    /// rules [`derive`] applies, so an arriving entry point can still be
    /// placed against a named screen even where nothing can leave.
    ///
    /// # The contract a caller must honour before using this
    ///
    /// **A machine that is currently *being controlled* must never be left
    /// inert.** The crossing spans do double duty (ADR 0009): while the
    /// peer drives this machine, reaching one is how the user *reclaims*
    /// control. A map with no spans therefore removes the reclaim path, and
    /// a machine that goes inert mid-grant is a machine whose user cannot
    /// get their cursor back by moving it — the release-blocking shape of
    /// defect this project treats a stuck key as.
    ///
    /// So a caller that may substitute this map while a grant is live owes
    /// one of two things: keep the previous map's spans alive for the
    /// `Returning` direction, or force a release (`ReleaseAllInput` and
    /// back to `LOCAL`) as it substitutes. The implicit side-model source
    /// sidesteps the question by construction — it derives from geometry
    /// alone, so no plug event can make it refuse — and the explicit
    /// layout source that the control wiring will grow must answer it
    /// explicitly.
    ///
    /// The remaining reachable route here is geometry no layout model can
    /// express at all (a monitor past `MAX_MONITOR_EXTENT`, or no monitors
    /// whatsoever), which no display reports.
    #[must_use]
    pub fn inert(local: DeviceId, live: &[MonitorInfo]) -> Self {
        let identities = usable_identities(live);
        Self {
            local,
            monitors: live
                .iter()
                .zip(identities)
                .map(|(info, id)| MappedMonitor {
                    id,
                    live: info.rect,
                    drawn: None,
                })
                .collect(),
            spans: Vec::new(),
        }
    }

    /// Every live monitor, in the order the platform reported them.
    #[must_use]
    pub fn monitors(&self) -> &[MappedMonitor] {
        &self.monitors
    }

    /// Every span, in derivation order: monitors as the platform reported
    /// them, each monitor's edges in [`Edge::ALL`] order, and within an
    /// edge the peer monitors in layout order. Deterministic (NFR-2).
    #[must_use]
    pub fn spans(&self) -> &[CrossSpan] {
        &self.spans
    }

    /// How many spans there are — the size of the per-span armed state a
    /// detector needs to hold.
    #[must_use]
    pub fn span_count(&self) -> usize {
        self.spans.len()
    }

    /// One span, or `None` for an id from a different map.
    #[must_use]
    pub fn span(&self, span: SpanId) -> Option<&CrossSpan> {
        self.spans.get(span.0)
    }

    /// Is there nowhere to cross? True for an arrangement that places no
    /// peer monitor against any edge this machine actually has — a legal
    /// drawing, and the state seamless transfer is simply off in.
    #[must_use]
    pub fn is_inert(&self) -> bool {
        self.spans.is_empty()
    }

    /// Every span the cursor is **crossing**, in derivation order.
    ///
    /// At most one span per (monitor, edge) can match, because spans on one
    /// edge are disjoint by construction. Two can match together only at a
    /// literal corner pixel, where the cursor touches two edges of the same
    /// monitor at once; they come out in derivation order, so a caller
    /// taking the first gets the same answer every time.
    pub fn crossings_at(&self, cursor: CursorPoint) -> impl Iterator<Item = SpanId> + '_ {
        self.crossings(cursor).map(|(span, _)| span)
    }

    /// Where along the target's facing edge a cursor at this position
    /// crosses `span` — `None` if it is not on that span at all (a
    /// different monitor, past the edge's extent, or in a neighbouring
    /// span's share of the same edge).
    ///
    /// Deliberately **not** a touch test: this is the *mapping*, and the
    /// caller's arming state decides whether a touch is a crossing. See
    /// [`CrossingMap::leave`] for the two together.
    #[must_use]
    pub fn fraction_at(&self, span: SpanId, cursor: CursorPoint) -> Option<EdgeFraction> {
        let span = self.spans.get(span.0)?;
        let coordinate = self.crossed_coordinate(span, cursor)?;
        Some(span.target_edge.fraction_at(coordinate))
    }

    /// The crossing a cursor at this position makes, if it is on one: the
    /// first span in derivation order that it both touches and falls
    /// inside, with how far along the destination it lands.
    ///
    /// Hysteresis is **not** applied here — a bare position has no history.
    /// The detector holds the per-span armed flags (ADR 0018) and consults
    /// [`CrossingMap::clear_of`] and [`CrossingMap::spans_near`] for them.
    #[must_use]
    pub fn leave(&self, cursor: CursorPoint) -> Option<Departure> {
        let (span, coordinate) = self.crossings(cursor).next()?;
        let fraction = self.spans.get(span.0)?.target_edge.fraction_at(coordinate);
        Some(Departure { span, fraction })
    }

    /// Where the cursor should appear when a crossing arrives here naming
    /// `monitor`, `edge` and `fraction` — that monitor's live facing edge,
    /// `fraction` of the way along it.
    ///
    /// `None` if no live monitor carries that id, which is ADR 0018's
    /// degraded case: the caller falls back to desktop-bounds placement
    /// with a diagnostic, and the grant proceeds regardless. Cursor
    /// placement is a nicety; control correctness never depends on it.
    ///
    /// A [`CrossTarget`] whose `monitor` is `None` is the *same* degraded
    /// case, decided one step earlier — see [`CrossingMap::arrive`], which
    /// is how a caller should follow a target rather than by unwrapping the
    /// option itself.
    ///
    /// The fraction is taken against the **whole** live edge, mirroring
    /// [`CrossSpan::target_edge`] on the sending side. Neither end consults
    /// the other's pixels, which is why a 4K sender and a 1080p receiver
    /// need no shared unit.
    #[must_use]
    pub fn enter(
        &self,
        monitor: &MonitorId,
        edge: Edge,
        fraction: EdgeFraction,
    ) -> Option<CursorPoint> {
        let live = self
            .monitors
            .iter()
            .find(|candidate| candidate.id.as_ref() == Some(monitor))?
            .live;
        Some(edge.entry_point(live, fraction))
    }

    /// Where a cursor following `target` should appear on this machine, or
    /// `None` when the target names no monitor this map holds — because it
    /// named none at all (an unaddressed crossing) or named one that is not
    /// live here. Both are the one degraded case, and collapsing them here
    /// is what keeps a caller from having to unwrap
    /// [`CrossTarget::monitor`] and invent a story for `None`.
    #[must_use]
    pub fn arrive(&self, target: &CrossTarget, fraction: EdgeFraction) -> Option<CursorPoint> {
        self.enter(target.monitor.as_ref()?, target.edge, fraction)
    }

    /// Is the cursor clear of `span`'s edge by more than `margin` pixels —
    /// far enough away that a fresh approach is a deliberate one rather
    /// than a wobble at the seam?
    ///
    /// The release half of the per-span Schmitt trigger (ADR 0018, ADR
    /// 0009's addendum before it).
    ///
    /// A cursor is **not** clear only when it is close to the span's edge
    /// in *both* directions — within `margin` perpendicular of it, **and**
    /// within the span's monitor's own extent along it, likewise widened by
    /// `margin`. Two different mistakes are ruled out by the two halves:
    ///
    /// - **Perpendicular alone would let a distant monitor suppress
    ///   arming.** Two of this machine's screens stacked, a seam on the
    ///   upper one's right edge: a cursor on the *lower* screen, near its
    ///   own right edge, sits within a few pixels of the upper screen's
    ///   seam column while being hundreds of rows away from the seam. It
    ///   would hold that span disarmed for as long as the user worked
    ///   there, and the seam would then not fire on arrival.
    /// - **Narrowing the lateral test to the span's own interval, rather
    ///   than its monitor's edge, would undo ADR 0018's lateral rule.** On
    ///   an edge shared by two spans, a cursor hugging one span's share is
    ///   laterally outside the other's — so the neighbour would arm, and
    ///   sliding into it would fire, which is exactly what "sliding
    ///   laterally along a hugged edge does not fire the neighbour" (and
    ///   the addendum's oscillation) forbids. The monitor's edge is the
    ///   unit the cursor actually hugs, so it is the unit the margin is
    ///   measured in.
    ///
    /// Perpendicular distance is measured **unsigned**, with one exception:
    /// a cursor clamped past the outer boundary of the desktop is never
    /// clear. Those two halves are the exact dual of
    /// [`CrossingMap::touching`]'s.
    ///
    /// A cursor that is past a seam because it is on the screen *across*
    /// that seam has genuinely travelled away, and must be able to re-arm,
    /// or an interior seam would fire once and then be dead for good. A
    /// cursor past the desktop's outer edge, on the other hand, has nowhere
    /// to have travelled to: that is where the OS pins it and where a
    /// transfer parks it, so treating it as travel would re-create exactly
    /// the bounce the margin exists to prevent.
    ///
    /// An id from another map is never clear, so a stale id can only ever
    /// suppress a crossing, never invent one.
    #[must_use]
    pub fn clear_of(&self, span: SpanId, cursor: CursorPoint, margin: u32) -> bool {
        let Some(span) = self.spans.get(span.0) else {
            return false;
        };
        let Some(monitor) = self.monitors.get(span.monitor) else {
            return false;
        };
        let margin = i32::try_from(margin).unwrap_or(i32::MAX);
        if Self::laterally_beyond(span, monitor, cursor, margin) {
            return true;
        }
        let inset = span.edge.inset_of(monitor.live, cursor);
        if inset > 0 {
            return inset > margin;
        }
        if self.clamped_against(span, cursor) {
            return false;
        }
        inset.saturating_neg() > margin
    }

    /// Is the cursor past either end of `span`'s edge — along the edge, not
    /// across it — by more than `margin`?
    ///
    /// The lateral half of [`CrossingMap::clear_of`]; see its note for why
    /// the bound is the monitor's whole edge rather than the span's share
    /// of it. Saturating throughout, so a nonsense rectangle or coordinate
    /// is an answer rather than an overflow (NFR-1).
    fn laterally_beyond(
        span: &CrossSpan,
        monitor: &MappedMonitor,
        cursor: CursorPoint,
        margin: i32,
    ) -> bool {
        let (offset, extent) = span.edge.offset_along(monitor.live, cursor);
        let last = last_index(extent);
        offset < margin.saturating_neg() || offset > last.saturating_add(margin)
    }

    /// Every span the cursor is **not** clear of — the exact complement of
    /// [`CrossingMap::clear_of`] over the whole map.
    ///
    /// This is the set a prime disarms: at a corner it correctly names both
    /// adjacent spans, and on a multi-span edge it names every span on that
    /// edge, because a cursor hugging an edge has cleared none of them.
    pub fn spans_near(
        &self,
        cursor: CursorPoint,
        margin: u32,
    ) -> impl Iterator<Item = SpanId> + '_ {
        (0..self.spans.len())
            .map(SpanId)
            .filter(move |&id| !self.clear_of(id, cursor, margin))
    }

    /// The spans a cursor is crossing, each with the layout coordinate it
    /// crossed at.
    ///
    /// The single implementation behind [`CrossingMap::crossings_at`] and
    /// [`CrossingMap::leave`], so a poll that both detects and maps does the
    /// work once.
    fn crossings(&self, cursor: CursorPoint) -> impl Iterator<Item = (SpanId, i64)> + '_ {
        self.spans
            .iter()
            .enumerate()
            .filter(move |(_, span)| self.touching(span, cursor))
            .filter_map(move |(index, span)| {
                self.crossed_coordinate(span, cursor)
                    .map(|coordinate| (SpanId(index), coordinate))
            })
    }

    /// Is `cursor` on one of this machine's screens?
    fn on_a_monitor(&self, cursor: CursorPoint) -> bool {
        self.monitors.iter().any(|monitor| monitor.holds(cursor))
    }

    /// Is the cursor **on** `span`'s edge?
    ///
    /// On means the edge's own outermost column or row — the pixel the OS
    /// pins the cursor at — and nothing else, with one deliberate
    /// exception: a coordinate *past* the edge counts when the cursor is
    /// clamped there, which is [`CrossingMap::clamped_against`]'s question.
    ///
    /// The exception is narrow on purpose. "At or beyond" alone would make
    /// a cursor sitting anywhere on the screen *across* an interior seam
    /// count as touching that seam from the wrong side — an arrangement
    /// drawn `[A][peer][B]` with A and B physically adjacent would hand
    /// control away from the middle of B. Beyond an edge with a screen
    /// there means the cursor is on *that* screen, not on this edge.
    fn touching(&self, span: &CrossSpan, cursor: CursorPoint) -> bool {
        let Some(monitor) = self.monitors.get(span.monitor) else {
            return false;
        };
        match span.edge.inset_of(monitor.live, cursor) {
            0 => true,
            inset if inset < 0 => self.clamped_against(span, cursor),
            _ => false,
        }
    }

    /// Is the cursor past `span`'s edge because the OS pinned it there —
    /// off every screen, against an edge that is the outer boundary of the
    /// desktop in that direction?
    ///
    /// Both halves are needed, and each rules out a different mistake. Off
    /// every screen alone would let a cursor clamped at the desktop's *left*
    /// boundary count as touching the far-right monitor's left edge, which
    /// it is beyond by a whole desktop's width. Outer-boundary alone would
    /// let a cursor legitimately sitting on a neighbouring screen count as
    /// touching, which is the half-plane the interior-seam case turns on.
    ///
    /// "Outer boundary" is measured with the same [`Edge::inset_of`] the
    /// cursor is measured with, against each other screen's own corner on
    /// that edge ([`Edge::outer_point`]) — so an edge and a cursor are never
    /// judged by two different notions of "further out".
    fn clamped_against(&self, span: &CrossSpan, cursor: CursorPoint) -> bool {
        let Some(monitor) = self.monitors.get(span.monitor) else {
            return false;
        };
        !self.on_a_monitor(cursor)
            && self.monitors.iter().all(|other| {
                span.edge
                    .inset_of(monitor.live, span.edge.outer_point(other.live))
                    >= 0
            })
    }

    /// The layout coordinate a cursor on `span`'s edge maps to, if it falls
    /// inside the span: live pixel → fraction of the live edge → the same
    /// fraction of the drawn edge.
    ///
    /// `None` when the cursor is outside the monitor's extent along that
    /// edge (a taller neighbour sharing the seam column must not be mapped
    /// against this monitor's height) or outside the span's half-open
    /// interval (a neighbouring span's share of the same edge).
    fn crossed_coordinate(&self, span: &CrossSpan, cursor: CursorPoint) -> Option<i64> {
        let monitor = self.monitors.get(span.monitor)?;
        let drawn = monitor.drawn?;
        let along = span.edge.fraction_along(monitor.live, cursor)?;
        let drawn_edge = edge_span(span.edge, drawn);
        // Bounded by MAX_MONITOR_EXTENT inside a validated layout, so the
        // conversion is exact there; the saturating fallback exists only so
        // the function stays total for a value that cannot occur.
        let drawn_extent = u32::try_from(drawn_edge.length()).unwrap_or(u32::MAX);
        let coordinate = drawn_edge
            .start
            .saturating_add(i64::from(along.to_pixel(drawn_extent)));
        span.span.contains(coordinate).then_some(coordinate)
    }
}

/// Derive every crossing span for `local`, given the arrangement `layout`
/// and the monitors the platform currently reports (ADR 0018).
///
/// Each live monitor is matched to a drawn rectangle **by id**; a match
/// gives it spans, and everything else about it — the rectangle the
/// detector measures against — is the platform's regardless. Live monitors
/// with no usable id, ids the layout does not place, and drawn monitors
/// with no live twin all contribute no spans and no geometry.
///
/// A device string the platform reports **twice** makes identity unusable
/// for *both* screens: neither is matched, neither gets spans, neither can
/// be addressed by an arriving entry point, and the collision is logged.
/// Picking one would be picking a physical screen at random, and the screen
/// a crossing attaches to is exactly the thing that must not be a guess.
///
/// Only rectangles belonging to the *other* device are searched, so a
/// same-machine seam produces nothing by construction rather than by a
/// filter that could be forgotten.
///
/// Total: any validated [`Layout`] with any live rectangles produces a map,
/// never a panic. Cost is `live × 4 × layout monitors` integer comparisons,
/// which at ADR 0018's caps is a few thousand — done once per geometry
/// change, never per poll.
#[must_use]
pub fn derive(layout: &Layout, local: DeviceId, live: &[MonitorInfo]) -> CrossingMap {
    derive_with(layout, local, live, Addressing::Drawn)
}

/// Whether the identities in a [`Layout`] describe real screens on real
/// machines, or are scaffolding an [`ImplicitLayout`] erected to express
/// the side model in the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Addressing {
    /// The user drew this: every device and monitor id names something.
    Drawn,
    /// The side model, expressed as a layout. Its ids exist only so
    /// [`derive_with`] can match rectangle to rectangle; **none of them
    /// leaves this module**, because every target comes out unaddressed
    /// (`device` and `monitor` both `None`) and every mapped monitor comes
    /// out unnamed. The side model never learned a single identity, and the
    /// map it produces says exactly that.
    Implicit,
}

/// [`derive`], plus the one thing a [`Layout`] cannot say about itself:
/// whether its identities are real. See [`Addressing`].
fn derive_with(
    layout: &Layout,
    local: DeviceId,
    live: &[MonitorInfo],
    addressing: Addressing,
) -> CrossingMap {
    let identities = usable_identities(live);
    let mut monitors: Vec<MappedMonitor> = Vec::with_capacity(live.len());
    let mut spans: Vec<CrossSpan> = Vec::new();

    for (info, id) in live.iter().zip(identities) {
        let drawn = id
            .as_ref()
            .and_then(|id| layout.find(local, id))
            .map(|placed| placed.rect);
        let index = monitors.len();

        if let Some(drawn) = drawn {
            for edge in Edge::ALL {
                let facing = edge.opposite();
                let mine = edge_span(edge, drawn);
                for peer in layout.monitors().iter().filter(|m| m.device != local) {
                    if edge_line(edge, drawn) != edge_line(facing, peer.rect) {
                        continue; // exact abutment only: a gap is a gap
                    }
                    let theirs = edge_span(facing, peer.rect);
                    let Some(overlap) = mine.intersect(theirs) else {
                        continue; // the edges are collinear but miss
                    };
                    spans.push(CrossSpan {
                        monitor: index,
                        edge,
                        span: overlap,
                        target: match addressing {
                            Addressing::Drawn => CrossTarget {
                                device: Some(peer.device),
                                monitor: Some(peer.id.clone()),
                                edge: facing,
                            },
                            Addressing::Implicit => CrossTarget {
                                device: None,
                                monitor: None,
                                edge: facing,
                            },
                        },
                        target_edge: theirs,
                    });
                }
            }
        }

        monitors.push(MappedMonitor {
            id: match addressing {
                Addressing::Drawn => id,
                Addressing::Implicit => None,
            },
            live: info.rect,
            drawn,
        });
    }

    CrossingMap {
        local,
        monitors,
        spans,
    }
}

/// The identity of each live monitor, in the order given, with the
/// unusable ones blanked: not supplied, not a valid [`MonitorId`], or
/// reported for more than one screen.
///
/// The duplicate rule blanks **every** claimant, not all-but-one. Keeping
/// the first would attach a seam to whichever screen the platform happened
/// to enumerate first, which is precisely the positional identity ADR 0018
/// rejected device-string matching in order to avoid.
fn usable_identities(live: &[MonitorInfo]) -> Vec<Option<MonitorId>> {
    let claimed: Vec<Option<MonitorId>> = live
        .iter()
        .map(|info| {
            info.id
                .as_deref()
                .and_then(|text| MonitorId::new(text).ok())
        })
        .collect();

    claimed
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let name = id.as_ref()?;
            let repeated = claimed
                .iter()
                .enumerate()
                .any(|(other, candidate)| other != index && candidate.as_ref() == Some(name));
            if repeated {
                // A device string is not user content and ADR 0018 asks
                // diagnostics to name monitor ids; this one passed its own
                // validation, so it is printable ASCII.
                tracing::warn!(
                    monitor = %name,
                    "crossing: the platform reported this monitor id for more than one screen; \
                     neither can be addressed by a layout"
                );
                return None;
            }
            Some(name.clone())
        })
        .collect()
}

/// The monitor id given to the peer rectangle [`from_link_side`] invents.
///
/// Private, and it never leaves this module: the arrangement is derived
/// under [`Addressing::Implicit`], which reports every target unaddressed,
/// so no fabricated device string reaches the wire, a log line, or a
/// caller. It exists only because [`Layout`] requires every placed
/// rectangle to carry an id, and the implicit arrangement's peer rectangle
/// is a rectangle.
const IMPLICIT_PEER_MONITOR_ID: &str = "<implicit-peer>";

/// The id given to the local rectangle at position `index` in the live
/// enumeration, for the same reason and with the same guarantee.
///
/// **Positional, and that is safe here precisely because it is
/// throw-away.** ADR 0018 rejected positional identity for *drawn* layouts
/// because an index outlives a re-enumeration and would silently rename a
/// screen; this id is minted and consumed inside one derivation, from one
/// live list, and never persisted, synced, or compared against anything
/// from another enumeration. It exists so [`derive_with`] can match
/// rectangle to rectangle without the platform having named anything —
/// which is what makes the implicit path work on **geometry alone**.
fn implicit_monitor_id(index: usize) -> String {
    format!("<implicit-{index}>")
}

/// The device the peer rectangle is attributed to.
///
/// Same guarantee as [`IMPLICIT_PEER_MONITOR_ID`], for the same reason
/// ([`Layout`] places every rectangle on *some* device): the derivation
/// reports [`CrossTarget::device`] as `None`, so this never escapes.
/// Derived from `local` rather than fixed so it is distinct from it
/// whatever `local` is — [`DevicePair`] requires two distinct devices, and
/// a fixed constant could in principle collide with a real device id and
/// turn a perfectly ordinary configuration into a refusal.
fn implicit_peer_device(local: DeviceId) -> DeviceId {
    let mut bytes = local.to_bytes();
    bytes[0] ^= 0xFF;
    DeviceId::from_bytes(bytes)
}

/// Why a side-model configuration could not be expressed as a layout.
///
/// Both variants are about **geometry**, and deliberately so: the implicit
/// path reads no monitor identity at all, so nothing the platform does or
/// does not know about a screen's name can refuse it. See
/// [`from_link_side`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImplicitLayoutError {
    /// The platform reported no monitors. Never true of a real display.
    #[error("there are no monitors to derive an arrangement from")]
    NoMonitors,
    /// The live geometry does not fit the layout model's bounds — a monitor
    /// past `MAX_MONITOR_EXTENT`, or a desktop past `MAX_LAYOUT_COORDINATE`.
    /// Unreachable on real hardware, and a refusal rather than a truncation
    /// if it ever is not.
    #[error("the live geometry cannot be expressed as an arrangement")]
    Unrepresentable {
        /// Which rule the derived arrangement broke.
        #[source]
        source: LayoutError,
    },
}

/// The side model expressed as a layout (ADR 0018's *implicit layout*:
/// revision 0, never synced, never written back).
///
/// Its identities are scaffolding — see [`Addressing::Implicit`] — so
/// deriving through [`ImplicitLayout::crossings`] rather than through the
/// free [`derive`] is what turns every destination into an honest `None`
/// instead of a device string and a monitor string no peer has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplicitLayout {
    layout: Layout,
}

impl ImplicitLayout {
    /// The arrangement itself, for a caller that has to publish or inspect
    /// it. Note that it is *implicit*: ADR 0018 forbids syncing it to the
    /// peer or writing it back to the config file, and its ids are the
    /// throw-away ones above rather than anything a peer could match.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The arrangement, consumed.
    #[must_use]
    pub fn into_layout(self) -> Layout {
        self.layout
    }

    /// The crossing map this arrangement gives `local`, from the same bare
    /// rectangles it was derived from.
    ///
    /// Every destination comes out unaddressed and every mapped monitor
    /// unnamed, because that is the whole truth the side model holds.
    #[must_use]
    pub fn crossings(&self, local: DeviceId, live: &[MonitorRect]) -> CrossingMap {
        derive_with(&self.layout, local, &scaffold(live), Addressing::Implicit)
    }
}

/// The live rectangles, labelled with the throw-away positional ids the
/// implicit arrangement places them under. One function, called by both
/// [`from_link_side`] and [`ImplicitLayout::crossings`], so the label a
/// rectangle is drawn under and the label it is matched by cannot drift.
fn scaffold(live: &[MonitorRect]) -> Vec<MonitorInfo> {
    live.iter()
        .enumerate()
        .map(|(index, rect)| MonitorInfo {
            id: Some(implicit_monitor_id(index)),
            rect: *rect,
        })
        .collect()
}

/// Express today's side model as a layout, so the same derivation drives
/// both.
///
/// # Geometry in, geometry only
///
/// This takes bare [`MonitorRect`]s, and that is the point rather than a
/// convenience. The side model never knew a monitor's name and never
/// needed one: it names one edge of one screen chosen by *position*, and
/// the destination it produces is unaddressed, so identity has nothing to
/// contribute at either end. Requiring one would import every way a
/// platform can fail to name a screen — a USB display adapter the OS
/// declines to name, two panels reporting one device string, an
/// enumeration caught mid-hotplug — into a path that worked without
/// identity for the whole of Phase 5, and each of those would turn
/// seamless transfer off for a configuration that has nothing wrong with
/// it. The ids the [`Layout`] carries are minted here, positionally, and
/// consumed by the derivation ([`implicit_monitor_id`]).
///
/// # What it draws, and why that shape
///
/// One local rectangle — the **monitor on the linked edge**, meaning the
/// outermost one in the linked direction, chosen by the same selector
/// [`crate::topology::Topology`] uses ([`edge_monitor_index`]) — and one
/// peer rectangle mirroring it across that edge. The result is a single
/// span covering the whole of that monitor's linked edge, whose target is
/// the mirror's opposite edge: precisely the side model's one linked edge
/// pair.
///
/// Two choices in that sentence are worth stating rather than discovering:
///
/// - **The edge monitor, not the desktop bounding box.** The bounding box
///   is what the side model uses to decide *whether* a seam is a crossing
///   edge, but ADR 0009's 2026-08-09 refinement maps the crossing fraction
///   against the edge monitor — because a shorter monitor beside a taller
///   one leaves dead space in the box, and mapping against the box lands
///   the peer's cursor at the wrong height. Drawing the box here would
///   reintroduce exactly that defect under a new name.
/// - **The peer rectangle is a fiction, and is one on purpose.** The side
///   model never learned anything about the peer's screens, so there is
///   nothing truthful to draw. Mirroring the local edge monitor makes the
///   crossing fraction pass through unchanged — a fraction of an edge, then
///   the same fraction of an identically-sized edge — which is what makes
///   the swap a no-op. It is reported as an **unaddressed** destination
///   ([`ImplicitLayout`]), so the receiver falls back to desktop-bounds
///   placement: the pre-0018 behaviour, which is what the side model had
///   all along.
///
/// # Errors
///
/// [`ImplicitLayoutError`] when there are no monitors at all, or when the
/// live geometry is outside what a layout can hold. Neither is reachable
/// from a real display, which is the improvement over an identity-matched
/// implicit path: a `--left` run that worked before this branch works
/// after it, on any hardware.
pub fn from_link_side(
    side: LinkSide,
    local: DeviceId,
    live: &[MonitorRect],
) -> Result<ImplicitLayout, ImplicitLayoutError> {
    let peer = implicit_peer_device(local);
    let pair = DevicePair::new(local, peer)
        .map_err(|source| ImplicitLayoutError::Unrepresentable { source })?;
    let index =
        edge_monitor_index(side, live.iter().copied()).ok_or(ImplicitLayoutError::NoMonitors)?;
    let edge_monitor = live[index];

    let mine = LayoutRect {
        x: edge_monitor.left,
        y: edge_monitor.top,
        width: edge_monitor.width,
        height: edge_monitor.height,
    };
    // Mirrored across the linked edge: the same size, one width away in the
    // linked direction. Matched on the side rather than on its edge, for
    // the reason `edge_monitor_index` gives — neither member links
    // vertically, so there is no third case. `i64` then a checked
    // narrowing, so an absurd live rectangle is a refusal, not a wrap.
    let offset = i64::from(mine.width);
    let mirrored = match side {
        LinkSide::Left => i64::from(mine.x) + offset,
        LinkSide::Right => i64::from(mine.x) - offset,
    };
    let theirs = LayoutRect {
        x: i32::try_from(mirrored).unwrap_or(i32::MAX),
        ..mine
    };

    let layout = Layout::from_raw(
        0,
        local,
        vec![
            RawPlacedMonitor {
                device: local,
                id: implicit_monitor_id(index),
                rect: mine,
            },
            RawPlacedMonitor {
                device: peer,
                id: IMPLICIT_PEER_MONITOR_ID.to_owned(),
                rect: theirs,
            },
        ],
        &pair,
    )
    .map_err(|source| ImplicitLayoutError::Unrepresentable { source })?;

    Ok(ImplicitLayout { layout })
}

/// The coordinate of `edge` on a drawn rectangle: the line the edge sits
/// on, in the shared space. Right and Bottom are the *exclusive* far
/// coordinates, which is what makes abutment a plain equality — a
/// rectangle's `right()` is its neighbour's `left()` when they touch.
fn edge_line(edge: Edge, rect: LayoutRect) -> i64 {
    match edge {
        Edge::Left => rect.left(),
        Edge::Right => rect.right(),
        Edge::Top => rect.top(),
        Edge::Bottom => rect.bottom(),
    }
}

/// The interval a drawn rectangle's `edge` runs over, perpendicular to the
/// edge's own axis: the vertical extent for a Left/Right edge, the
/// horizontal extent for a Top/Bottom one.
fn edge_span(edge: Edge, rect: LayoutRect) -> LayoutSpan {
    match edge {
        Edge::Left | Edge::Right => LayoutSpan {
            start: rect.top(),
            end: rect.bottom(),
        },
        Edge::Top | Edge::Bottom => LayoutSpan {
            start: rect.left(),
            end: rect.right(),
        },
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crossover_platform::{CursorPoint, MonitorInfo, MonitorRect};
    use crossover_topology::{
        DEVICE_ID_BYTES, DeviceId, DevicePair, Layout, LayoutRect, MonitorId, PlacedMonitor,
        RawPlacedMonitor,
    };

    use super::{
        CrossTarget, CrossingMap, Departure, Edge, EdgeFraction, IMPLICIT_PEER_MONITOR_ID,
        ImplicitLayoutError, LayoutSpan, SpanId, derive, from_link_side,
    };
    use crate::topology::{LinkSide, Topology};

    const LOCAL: DeviceId = DeviceId::from_bytes([0x11; DEVICE_ID_BYTES]);
    const PEER: DeviceId = DeviceId::from_bytes([0x22; DEVICE_ID_BYTES]);

    fn pair() -> DevicePair {
        DevicePair::new(LOCAL, PEER).unwrap()
    }

    /// One drawn rectangle in the shared layout space.
    fn drawn(device: DeviceId, id: &str, x: i32, y: i32, width: u32, height: u32) -> PlacedMonitor {
        PlacedMonitor {
            device,
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x,
                y,
                width,
                height,
            },
        }
    }

    /// A validated arrangement of those rectangles.
    fn layout(monitors: Vec<PlacedMonitor>) -> Layout {
        Layout::new(1, LOCAL, monitors, &pair()).unwrap()
    }

    /// One live monitor, named.
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

    /// One live monitor the platform would not name.
    fn unnamed(left: i32, top: i32, width: u32, height: u32) -> MonitorInfo {
        MonitorInfo {
            id: None,
            rect: MonitorRect {
                left,
                top,
                width,
                height,
            },
        }
    }

    fn at(x: i32, y: i32) -> CursorPoint {
        CursorPoint { x, y }
    }

    fn id(text: &str) -> MonitorId {
        MonitorId::new(text).unwrap()
    }

    /// The pixel `fraction` of the way along an edge whose last index is
    /// `last` — where a user aiming at "40 % down the screen" lands.
    fn pixel_at(fraction: f64, last: i32) -> i32 {
        // The products here are all small, deliberate constants.
        #[allow(clippy::cast_possible_truncation)]
        let pixel = (fraction * f64::from(last)).round() as i32;
        pixel
    }

    /// The destination a departure names, fetched from the map that
    /// produced it — the way a caller does it, now that `Departure` is
    /// `Copy` and carries no cloned device string.
    fn target_of(map: &CrossingMap, departure: Departure) -> CrossTarget {
        map.span(departure.span).unwrap().target().clone()
    }

    /// The monitor a departure names, which must be an addressed one.
    fn target_monitor(map: &CrossingMap, departure: Departure) -> MonitorId {
        target_of(map, departure)
            .monitor
            .expect("this arrangement addresses its destinations")
    }

    /// Every crossing at this cursor, as `(local monitor index, local edge,
    /// target monitor id)` — the shape the assertions below read best in.
    fn crossings(map: &CrossingMap, cursor: CursorPoint) -> Vec<(usize, Edge, Option<String>)> {
        map.crossings_at(cursor)
            .map(|span| {
                let span = map.span(span).unwrap();
                (
                    span.monitor(),
                    span.edge(),
                    span.target()
                        .monitor
                        .as_ref()
                        .map(|id| id.as_str().to_owned()),
                )
            })
            .collect()
    }

    // ---- what adjacency does and does not produce ----

    /// The property the desktop-bounding-box rule bought crudely: a seam
    /// between two of this machine's own screens is inert. Here it is
    /// obtained exactly — the *identical* interval fires the moment the
    /// neighbour across it belongs to the peer instead.
    #[test]
    fn a_same_machine_seam_is_inert_where_the_identical_peer_seam_fires() {
        let mine = live("B", 0, 0, 1000, 1000);
        let column = at(0, 500); // B's left column, the seam in question

        // Neighbour on this machine: nothing, on an edge that abuts exactly.
        let same_machine = layout(vec![
            drawn(LOCAL, "A", -1000, 0, 1000, 1000),
            drawn(LOCAL, "B", 0, 0, 1000, 1000),
            drawn(PEER, "FAR", 9000, 9000, 100, 100),
        ]);
        let map = derive(&same_machine, LOCAL, std::slice::from_ref(&mine));
        assert!(map.is_inert(), "a same-machine seam produced a crossing");
        assert!(crossings(&map, column).is_empty());

        // The same rectangle, the same seam, drawn as the peer's.
        let across = layout(vec![
            drawn(PEER, "A", -1000, 0, 1000, 1000),
            drawn(LOCAL, "B", 0, 0, 1000, 1000),
        ]);
        let map = derive(&across, LOCAL, std::slice::from_ref(&mine));
        assert_eq!(map.span_count(), 1);
        assert_eq!(
            crossings(&map, column),
            vec![(0, Edge::Left, Some("A".to_owned()))]
        );
        let span = map.span(map.crossings_at(column).next().unwrap()).unwrap();
        assert_eq!(
            span.span(),
            LayoutSpan {
                start: 0,
                end: 1000
            }
        );
        assert_eq!(span.target().device, Some(PEER));
        assert_eq!(span.target().edge, Edge::Right);
    }

    /// A gap of one unit is a gap. Snapping is the editor's job; a
    /// tolerance here would make "is this an edge" fuzzy at exactly the
    /// place where a wrong answer hands control away.
    #[test]
    fn abutment_is_exact_with_no_tolerance() {
        // One unit short of A's right edge, and one unit short of its left
        // — the smallest miss on either side. (Overlapping rather than
        // missing is not expressible: the layout model refuses it.)
        for x in [1001, -1001] {
            let arrangement = layout(vec![
                drawn(LOCAL, "A", 0, 0, 1000, 1000),
                drawn(PEER, "C", x, 0, 1000, 1000),
            ]);
            let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
            assert!(
                map.is_inert(),
                "a one-unit gap at {x} was treated as a seam"
            );
        }
        // Exactly abutting is the seam.
        let flush = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        assert_eq!(
            derive(&flush, LOCAL, &[live("A", 0, 0, 1000, 1000)]).span_count(),
            1
        );
    }

    /// Collinear edges that do not overlap on the perpendicular axis are
    /// not adjacent either — two monitors on the same line, past each
    /// other's ends.
    #[test]
    fn collinear_edges_that_miss_are_not_a_seam() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 1000, 1000, 1000), // starts where A ends
        ]);
        let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        assert!(map.is_inert());
    }

    // ---- the three-monitor corner, both shapes ----

    /// Two of this machine's screens stacked, one peer screen spanning
    /// both. The corner row belongs to exactly one of them, and it is the
    /// half-open rule — not enumeration order — that says which.
    #[test]
    fn a_corner_of_two_local_monitors_and_one_peer_is_deterministic() {
        let arrangement = layout(vec![
            drawn(LOCAL, "TOP", 0, 0, 1000, 600),
            drawn(LOCAL, "BOTTOM", 0, 600, 1000, 400),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        let map = derive(
            &arrangement,
            LOCAL,
            &[
                live("TOP", 0, 0, 1000, 600),
                live("BOTTOM", 0, 600, 1000, 400),
            ],
        );
        assert_eq!(map.span_count(), 2);
        assert_eq!(map.spans()[0].span(), LayoutSpan { start: 0, end: 600 });
        assert_eq!(
            map.spans()[1].span(),
            LayoutSpan {
                start: 600,
                end: 1000
            }
        );

        // The last row of TOP and the first row of BOTTOM: one crossing
        // each, on different monitors, at adjacent points of one peer edge.
        assert_eq!(
            crossings(&map, at(999, 599)),
            vec![(0, Edge::Right, Some("C".to_owned()))]
        );
        assert_eq!(
            crossings(&map, at(999, 600)),
            vec![(1, Edge::Right, Some("C".to_owned()))]
        );

        let upper = map.leave(at(999, 599)).unwrap().fraction.value();
        let lower = map.leave(at(999, 600)).unwrap().fraction.value();
        assert!(upper < lower, "{upper} !< {lower}");
        // Both land against C's whole 1000-unit edge, one unit apart.
        assert!((upper - 599.0 / 999.0).abs() < 1e-9, "{upper}");
        assert!((lower - 600.0 / 999.0).abs() < 1e-9, "{lower}");
    }

    /// The mirror shape: one local screen, two peer screens stacked across
    /// its edge. The shared coordinate goes to the lower span, and each
    /// crossing is a fraction of *its own* destination's whole edge — so
    /// the boundary is the bottom of one and the top of the other.
    #[test]
    fn a_corner_of_one_local_monitor_and_two_peers_is_deterministic() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "UPPER", 1000, 0, 1000, 600),
            drawn(PEER, "LOWER", 1000, 600, 1000, 400),
        ]);
        let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        assert_eq!(map.span_count(), 2);

        assert_eq!(
            crossings(&map, at(999, 599)),
            vec![(0, Edge::Right, Some("UPPER".to_owned()))]
        );
        assert_eq!(
            crossings(&map, at(999, 600)),
            vec![(0, Edge::Right, Some("LOWER".to_owned()))]
        );

        let last_of_upper = map.leave(at(999, 599)).unwrap();
        let first_of_lower = map.leave(at(999, 600)).unwrap();
        assert!((last_of_upper.fraction.value() - 1.0).abs() < 1e-9);
        assert!((first_of_lower.fraction.value() - 0.0).abs() < 1e-9);
        assert_eq!(target_of(&map, last_of_upper).edge, Edge::Left);
        assert_eq!(target_of(&map, first_of_lower).edge, Edge::Left);
    }

    /// A literal corner pixel touches two edges of one monitor. Both are
    /// genuine crossings; what matters is that the answer is the same
    /// every time, which derivation order (edges in `Edge::ALL` order)
    /// gives (NFR-2).
    #[test]
    fn a_cursor_on_two_edges_at_once_resolves_the_same_way_every_time() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "EAST", 1000, 0, 1000, 1000),
            drawn(PEER, "SOUTH", 0, 1000, 1000, 1000),
        ]);
        let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        let corner = at(999, 999);
        assert_eq!(
            crossings(&map, corner),
            vec![
                (0, Edge::Right, Some("EAST".to_owned())),
                (0, Edge::Bottom, Some("SOUTH".to_owned())),
            ]
        );
        // `leave` takes the first, and takes it consistently.
        let chosen = map.leave(corner).unwrap();
        assert_eq!(target_monitor(&map, chosen), id("EAST"));
        for _ in 0..5 {
            assert_eq!(map.leave(corner).unwrap(), chosen);
        }
    }

    // ---- the fraction chain across mismatched pixels ----

    /// The phase's exit criterion: leaving a 4K panel at 40 % of its edge
    /// arrives at 40 % of the adjacent 1080p edge. Neither machine consults
    /// the other's pixels; the drawn edges are equal because the editor
    /// seeds them in DIPs, so the fraction passes straight through.
    #[test]
    fn forty_percent_of_a_4k_edge_arrives_at_forty_percent_of_a_1080p_edge() {
        // Drawn in DIPs: both monitors 1920x1080 units, abutting exactly.
        let arrangement = layout(vec![
            drawn(LOCAL, "UHD", 0, 0, 1920, 1080),
            drawn(PEER, "HD", 1920, 0, 1920, 1080),
        ]);
        // Live: a 4K panel here, a 1080p panel there.
        let mine = derive(&arrangement, LOCAL, &[live("UHD", 0, 0, 3840, 2160)]);
        let theirs = derive(&arrangement, PEER, &[live("HD", 0, 0, 1920, 1080)]);

        let row = pixel_at(0.4, 2159); // 40 % of the live 4K edge
        let departure = mine.leave(at(3839, row)).unwrap();
        let target = target_of(&mine, departure);
        assert_eq!(target.monitor, Some(id("HD")));
        assert_eq!(target.edge, Edge::Left);
        assert!(
            (departure.fraction.value() - 0.4).abs() < 0.002,
            "{}",
            departure.fraction.value()
        );

        let arrival = theirs.arrive(&target, departure.fraction).unwrap();
        assert_eq!(arrival.x, 0);
        let expected = pixel_at(0.4, 1079);
        assert!(
            (arrival.y - expected).abs() <= 1,
            "{} vs {expected}",
            arrival.y
        );
    }

    /// The same proportional rule across a *partial* overlap: the fraction
    /// is of the destination's whole edge, not of the shared sliver, so a
    /// crossing 40 % down the neighbour lands 40 % down the neighbour.
    #[test]
    fn a_partial_overlap_maps_against_the_whole_destination_edge() {
        // C is drawn 400 units lower than A, so only [400, 1000) of A's
        // right edge is a seam at all.
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 400, 1000, 1000),
        ]);
        let mine = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        let theirs = derive(&arrangement, PEER, &[live("C", 0, 0, 1920, 1080)]);
        assert_eq!(
            mine.spans()[0].span(),
            LayoutSpan {
                start: 400,
                end: 1000
            }
        );
        assert_eq!(
            mine.spans()[0].target_edge(),
            LayoutSpan {
                start: 400,
                end: 1400
            }
        );

        // Above the span there is no seam; the first row of it is C's top.
        assert!(mine.leave(at(999, 399)).is_none());
        let top_of_c = mine.leave(at(999, 400)).unwrap();
        assert!((top_of_c.fraction.value() - 0.0).abs() < 1e-9);

        // 40 % of C's whole 1000-unit edge is layout row 400 + 0.4*999.
        let row = 400 + pixel_at(0.4, 999);
        let departure = mine.leave(at(999, row)).unwrap();
        assert!(
            (departure.fraction.value() - 0.4).abs() < 0.002,
            "{}",
            departure.fraction.value()
        );
        let arrival = theirs
            .arrive(&target_of(&mine, departure), departure.fraction)
            .unwrap();
        let expected = pixel_at(0.4, 1079);
        assert!(
            (arrival.y - expected).abs() <= 1,
            "{} vs {expected}",
            arrival.y
        );

        // The last row of A's edge is the point 600 units into C's edge,
        // not the end of it: A stops before C does.
        let bottom = mine.leave(at(999, 999)).unwrap();
        assert!((bottom.fraction.value() - 599.0 / 999.0).abs() < 1e-3);
    }

    /// A cursor sharing the seam column with a taller neighbour, at a row
    /// only the neighbour covers, must not be mapped against this
    /// monitor's height.
    #[test]
    fn a_cursor_outside_a_monitors_extent_does_not_cross_its_edge() {
        let arrangement = layout(vec![
            drawn(LOCAL, "SHORT", 0, 0, 1000, 500),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        let map = derive(&arrangement, LOCAL, &[live("SHORT", 0, 0, 1000, 500)]);
        assert!(map.leave(at(999, 499)).is_some());
        assert!(map.leave(at(999, 500)).is_none());
        assert!(map.leave(at(999, -1)).is_none());
    }

    // ---- what the two lists disagreeing costs ----

    #[test]
    fn a_drawn_monitor_with_no_live_twin_contributes_nothing() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(LOCAL, "B", 1000, 0, 1000, 1000),
            drawn(PEER, "C", 2000, 0, 1000, 1000),
        ]);
        // B — the only monitor with a seam — is unplugged.
        let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        assert_eq!(map.monitors().len(), 1);
        assert!(map.is_inert());
        assert!(
            map.enter(&id("B"), Edge::Left, EdgeFraction::new(0.5))
                .is_none()
        );
    }

    /// An unknown id degrades placement, never geometry: the rectangle the
    /// detector measures against is reported exactly as the platform gave
    /// it, whatever the layout does or does not know about it.
    #[test]
    fn an_unidentified_live_monitor_keeps_its_geometry_and_gets_no_spans() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        let anonymous = unnamed(0, 0, 1000, 1000);
        let unusable = MonitorInfo {
            id: Some("bad\u{0}id".to_owned()),
            rect: MonitorRect {
                left: 0,
                top: 0,
                width: 1000,
                height: 1000,
            },
        };
        for info in [anonymous, unusable] {
            let expected = info.rect;
            let map = derive(&arrangement, LOCAL, std::slice::from_ref(&info));
            assert_eq!(map.monitors().len(), 1);
            assert_eq!(map.monitors()[0].live(), expected);
            assert!(map.monitors()[0].id().is_none());
            assert!(map.monitors()[0].drawn().is_none());
            assert!(map.is_inert());
            assert!(crossings(&map, at(999, 500)).is_empty());
        }

        // Alongside an identified one, the anonymous monitor costs the
        // identified one nothing.
        let map = derive(
            &arrangement,
            LOCAL,
            &[unnamed(-1000, 0, 1000, 1000), live("A", 0, 0, 1000, 1000)],
        );
        assert_eq!(map.monitors().len(), 2);
        assert_eq!(map.span_count(), 1);
        assert_eq!(map.spans()[0].monitor(), 1);
    }

    /// A device string reported twice — which no platform should do —
    /// makes identity unusable for **both** claimants. Keeping the first
    /// would attach a seam to whichever screen the platform happened to
    /// enumerate first, which is the positional identity ADR 0018 rejected
    /// indices to avoid.
    #[test]
    fn a_repeated_device_string_makes_both_screens_unaddressable() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        let map = derive(
            &arrangement,
            LOCAL,
            &[live("A", 0, 0, 1000, 1000), live("A", 1000, 0, 1000, 1000)],
        );
        // Geometry survives — the detector must never lose a rectangle...
        assert_eq!(map.monitors().len(), 2);
        assert_eq!(map.monitors()[0].live().left, 0);
        assert_eq!(map.monitors()[1].live().left, 1000);
        // ...but neither screen can be addressed, so neither carries a seam
        // and no arriving entry point can be placed against either.
        assert!(map.monitors().iter().all(|m| m.id().is_none()));
        assert!(map.is_inert());
        assert!(
            map.enter(&id("A"), Edge::Right, EdgeFraction::new(0.5))
                .is_none()
        );

        // A collision elsewhere costs an unambiguous screen nothing.
        let map = derive(
            &arrangement,
            LOCAL,
            &[
                live("A", 0, 0, 1000, 1000),
                live("D", 1000, 0, 10, 10),
                live("D", 2000, 0, 10, 10),
            ],
        );
        assert_eq!(map.span_count(), 1);
        assert_eq!(map.spans()[0].monitor(), 0);
    }

    /// The two selectors must land on the same physical screen: this one
    /// chooses the edge monitor by geometry, and the derivation matches it
    /// by the throw-away id minted for it — so the two agree by
    /// construction, and what the *platform* calls a screen never enters.
    ///
    /// That is what makes the implicit path immune to every identity
    /// failure a real display can produce: no name, an unusable name, or
    /// one name on two screens are all just rectangles here.
    #[test]
    fn the_implicit_path_ignores_platform_identity_entirely() {
        // Two screens the platform names identically — which for a *drawn*
        // arrangement makes both unaddressable — still produce exactly the
        // side model's one span, on the outer edge.
        let screens = [
            live("DUP", 0, 0, 1000, 1000),
            live("DUP", 1000, 0, 1000, 1000),
        ];
        let rects: Vec<MonitorRect> = screens.iter().map(|m| m.rect).collect();
        for side in [LinkSide::Left, LinkSide::Right] {
            let implicit = from_link_side(side, LOCAL, &rects).unwrap();
            let map = implicit.crossings(LOCAL, &rects);
            assert_eq!(map.span_count(), 1, "{side:?}");
            assert_eq!(map.spans()[0].edge(), side.linked_edge(), "{side:?}");
        }

        // Identity is not merely tolerated, it is *absent from the input*:
        // the same rectangles named, unnamed, or unusably named give the
        // identical map, and none of the names survives into it.
        let unnamed_map = from_link_side(LinkSide::Left, LOCAL, &rects)
            .unwrap()
            .crossings(LOCAL, &rects);
        assert!(unnamed_map.monitors().iter().all(|m| m.id().is_none()));
        assert_eq!(unnamed_map.spans()[0].target().device, None);
        assert_eq!(unnamed_map.spans()[0].target().monitor, None);
    }

    /// Both selectors break a tie the same way — the property that keeps a
    /// crossing span attached to the screen the side model measures
    /// against. Two screens stacked in the same columns tie on the sort key
    /// and differ in height, so a disagreement is observable.
    #[test]
    fn both_edge_monitor_selectors_break_a_tie_the_same_way() {
        let screens = [
            live("UPPER", 0, 0, 1000, 600),
            live("LOWER", 0, 600, 1000, 400),
        ];
        let rects: Vec<MonitorRect> = screens.iter().map(|m| m.rect).collect();

        for side in [LinkSide::Left, LinkSide::Right] {
            let implicit = from_link_side(side, LOCAL, &rects).unwrap();
            let map = implicit.crossings(LOCAL, &rects);
            let chosen = map.monitors()[map.spans()[0].monitor()].live();
            // The side model's own choice, observed where it places an
            // arriving cursor: fraction 0.0 is the chosen screen's corner.
            let corner = Topology::new(side).entering(EdgeFraction::new(0.0), &rects);
            assert_eq!(
                corner.y, chosen.top,
                "{side:?}: the two selectors chose different screens"
            );
        }
    }

    /// The half-plane trap, pinned. With the peer drawn *between* two of
    /// this machine's screens, the screens themselves are physically
    /// adjacent — so "at or beyond the edge" would make a cursor sitting in
    /// the middle of the right-hand screen count as touching the left-hand
    /// screen's seam, and hand control away from nowhere near it. Beyond an
    /// edge with a screen there means the cursor is on *that* screen.
    #[test]
    fn a_cursor_across_an_interior_seam_is_not_touching_the_seam_from_behind() {
        let arrangement = layout(vec![
            drawn(PEER, "WEST", -1000, 0, 1000, 1000),
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "MIDDLE", 1000, 0, 1000, 1000),
            drawn(LOCAL, "B", 2000, 0, 1000, 1000),
        ]);
        // Live: A and B sit side by side, with no gap between them.
        let screens = [live("A", 0, 0, 1000, 1000), live("B", 1000, 0, 1000, 1000)];
        let map = derive(&arrangement, LOCAL, &screens);
        // A's left (to WEST) and right (to MIDDLE), B's left (to MIDDLE).
        assert_eq!(map.span_count(), 3);
        let span_on = |monitor: usize, edge: Edge| {
            map.spans()
                .iter()
                .position(|s| s.monitor() == monitor && s.edge() == edge)
                .map(SpanId::from_index)
                .unwrap()
        };

        // Deep inside B: A's seam is 501 pixels behind the cursor, on the
        // far side of it. Not a crossing — and clearance must be able to
        // re-arm there, or the seam would be dead for good.
        let inside_b = at(1500, 500);
        assert!(crossings(&map, inside_b).is_empty());
        assert!(map.clear_of(span_on(0, Edge::Right), inside_b, 24));

        // One pixel past A's column is B's first column: that is B's seam
        // firing, never A's.
        assert_eq!(
            crossings(&map, at(1000, 500)),
            vec![(1, Edge::Left, Some("MIDDLE".to_owned()))]
        );
        // And A's own column is A's seam, exactly once.
        assert_eq!(
            crossings(&map, at(999, 500)),
            vec![(0, Edge::Right, Some("MIDDLE".to_owned()))]
        );

        // The one place "beyond" still counts: past the outer edge of the
        // desktop, where the OS clamps a cursor and no screen lies. That is
        // where a transfer parks it, so it must never read as clearance.
        let clamped = at(-5, 500);
        assert_eq!(
            crossings(&map, clamped),
            vec![(0, Edge::Left, Some("WEST".to_owned()))]
        );
        assert!(!map.clear_of(span_on(0, Edge::Left), clamped, 24));
    }

    /// A layout describing a machine this map is not written from places no
    /// local monitor at all, so nothing can cross. Fail closed.
    #[test]
    fn a_layout_that_does_not_name_this_machine_is_inert() {
        let stranger = DeviceId::from_bytes([0x33; DEVICE_ID_BYTES]);
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        let map = derive(&arrangement, stranger, &[live("A", 0, 0, 1000, 1000)]);
        assert!(map.is_inert());
        assert_eq!(map.monitors().len(), 1);
    }

    // ---- placement, on every edge ----

    #[test]
    fn entry_lands_on_the_named_edge_of_the_named_live_monitor() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        // Live geometry deliberately unlike the drawn geometry, and not at
        // the desktop origin: entry is the platform's pixels, not the
        // layout's units.
        let map = derive(&arrangement, LOCAL, &[live("A", 10, 20, 1920, 1080)]);
        let half = EdgeFraction::new(0.5);
        assert_eq!(map.enter(&id("A"), Edge::Left, half).unwrap(), at(10, 560));
        assert_eq!(
            map.enter(&id("A"), Edge::Right, half).unwrap(),
            at(1929, 560)
        );
        assert_eq!(map.enter(&id("A"), Edge::Top, half).unwrap(), at(970, 20));
        assert_eq!(
            map.enter(&id("A"), Edge::Bottom, half).unwrap(),
            at(970, 1099)
        );

        // The extremes sit on the monitor's own corners, never past them.
        let zero = EdgeFraction::new(0.0);
        let one = EdgeFraction::new(1.0);
        assert_eq!(map.enter(&id("A"), Edge::Top, zero).unwrap(), at(10, 20));
        assert_eq!(
            map.enter(&id("A"), Edge::Bottom, one).unwrap(),
            at(1929, 1099)
        );

        // An id this machine does not have is the degraded case, reported
        // rather than guessed.
        assert!(map.enter(&id("NOPE"), Edge::Left, half).is_none());
    }

    // ---- per-span hysteresis ----

    /// Priming disarms every span on an edge the cursor hugs — both of
    /// them at a corner — and a span the cursor is genuinely clear of is
    /// left alone.
    #[test]
    fn the_margin_test_names_every_span_on_a_hugged_edge() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "EAST", 1000, 0, 1000, 1000),
            drawn(PEER, "SOUTH", 0, 1000, 1000, 1000),
        ]);
        let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        let near = |cursor| map.spans_near(cursor, 24).count();

        assert_eq!(near(at(999, 999)), 2, "a corner must disarm both spans");
        assert_eq!(near(at(999, 500)), 1, "only the edge being hugged");
        assert_eq!(near(at(500, 500)), 0, "clear of everything");
        // Exactly the margin is still hugging; one pixel further is clear.
        assert_eq!(near(at(1000 - 1 - 24, 500)), 1);
        assert_eq!(near(at(1000 - 1 - 25, 500)), 0);

        // `spans_near` is exactly the complement of `clear_of`.
        for cursor in [at(999, 999), at(999, 500), at(500, 500), at(0, 0)] {
            let near: Vec<SpanId> = map.spans_near(cursor, 24).collect();
            for span in (0..map.span_count()).map(SpanId::from_index) {
                assert_eq!(near.contains(&span), !map.clear_of(span, cursor, 24));
            }
        }
    }

    /// Sliding along a hugged edge from one span into its neighbour must
    /// not arm the neighbour — lateral motion clears nothing, which is the
    /// property that keeps ADR 0009's oscillation from returning per span.
    #[test]
    fn sliding_along_a_hugged_edge_clears_neither_span() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "UPPER", 1000, 0, 1000, 500),
            drawn(PEER, "LOWER", 1000, 500, 1000, 500),
        ]);
        let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        assert_eq!(map.span_count(), 2);
        // Anywhere along the hugged column, both spans stay disarmed.
        for y in [0, 100, 499, 500, 900, 999] {
            assert_eq!(map.spans_near(at(999, y), 24).count(), 2, "at row {y}");
        }
        // Travelling inward clears both at once, as it should: the cursor
        // has left that edge, not merely that span.
        assert_eq!(map.spans_near(at(900, 500), 24).count(), 0);
    }

    /// The shape a machine falls back to when its arrangement cannot be
    /// derived at all: every rectangle the platform reported, every
    /// identity it could resolve, and nowhere to cross. Geometry and
    /// placement survive; only leaving stops.
    #[test]
    fn an_inert_map_keeps_the_geometry_and_removes_every_crossing() {
        let screens = [
            live("A", 0, 0, 1000, 1000),
            unnamed(1000, 0, 1000, 1000),
            live("DUP", 2000, 0, 10, 10),
            live("DUP", 2010, 0, 10, 10),
        ];
        let map = CrossingMap::inert(LOCAL, &screens);
        assert_eq!(map.local(), LOCAL);
        assert!(map.is_inert());
        assert_eq!(map.span_count(), 0);
        assert_eq!(map.monitors().len(), screens.len());
        for (mapped, original) in map.monitors().iter().zip(&screens) {
            assert_eq!(mapped.live(), original.rect);
            assert!(mapped.drawn().is_none());
        }
        // Identity follows `derive`'s rules: named once is addressable,
        // unnamed and duplicated are not.
        assert_eq!(map.monitors()[0].id(), Some(&id("A")));
        assert!(map.monitors()[1].id().is_none());
        assert!(map.monitors()[2].id().is_none());
        assert!(map.monitors()[3].id().is_none());
        // Nothing leaves, but an arriving entry point still places.
        assert!(map.leave(at(999, 500)).is_none());
        assert!(map.crossings_at(at(999, 500)).next().is_none());
        assert_eq!(
            map.enter(&id("A"), Edge::Right, EdgeFraction::new(0.0)),
            Some(at(999, 0))
        );
    }

    /// A span id from another map is never "clear", so a stale id can only
    /// suppress a crossing, never invent one.
    #[test]
    fn a_span_id_from_another_map_is_never_clear() {
        let arrangement = layout(vec![
            drawn(LOCAL, "A", 0, 0, 1000, 1000),
            drawn(PEER, "C", 1000, 0, 1000, 1000),
        ]);
        let map = derive(&arrangement, LOCAL, &[live("A", 0, 0, 1000, 1000)]);
        let span = map.crossings_at(at(999, 500)).next().unwrap();
        let empty = derive(&arrangement, LOCAL, &[]);
        assert!(!empty.clear_of(span, at(0, 0), 24));
        assert!(empty.span(span).is_none());
        assert!(empty.fraction_at(span, at(999, 500)).is_none());
    }

    // ---- the side model, expressed as a layout ----

    /// The compatibility claim, stated as an equality: for every cursor and
    /// every fraction, the derived map answers exactly what the side model
    /// answers. This is what lets the detector be swapped without a
    /// behaviour change.
    #[test]
    fn from_link_side_reproduces_the_side_models_crossings() {
        // The soak layout: a tall laptop panel with a shorter external 4K
        // beside it, so the bounding box and the edge monitor disagree.
        let laptop = live(r"\\.\DISPLAY1", 0, 0, 3840, 2400);
        let external = live(r"\\.\DISPLAY2", 3840, 0, 3840, 2160);

        for side in [LinkSide::Left, LinkSide::Right] {
            for screens in [
                vec![laptop.clone()],
                vec![laptop.clone(), external.clone()],
                vec![external.clone(), laptop.clone()],
            ] {
                let rects: Vec<MonitorRect> = screens.iter().map(|m| m.rect).collect();
                let topology = Topology::new(side);
                let arrangement = from_link_side(side, LOCAL, &rects).unwrap();
                let map = arrangement.crossings(LOCAL, &rects);

                // One span, on the outer edge, covering the whole of it,
                // and going somewhere this machine cannot name — the side
                // model never knew anything about the peer's screens.
                assert_eq!(map.span_count(), 1);
                assert_eq!(map.spans()[0].edge(), side.linked_edge());
                assert_eq!(map.spans()[0].target().device, None);
                assert_eq!(map.spans()[0].target().monitor, None);
                assert_eq!(map.spans()[0].target().edge, side.linked_edge().opposite());
                assert_eq!(
                    map.arrive(map.spans()[0].target(), EdgeFraction::new(0.5)),
                    None,
                    "an unaddressed target must fall back, not place"
                );

                // Leaving: the same verdict and the same fraction, at every
                // interesting column and a sweep of rows.
                for x in [-5, 0, 1, 3839, 3840, 7679, 7680, 9000] {
                    for y in [0, 1, 539, 1080, 2159, 2160, 2399, 2400, 5000, -1] {
                        let cursor = at(x, y);
                        assert_eq!(
                            map.leave(cursor).map(|d| d.fraction),
                            topology.leaving(cursor, &rects),
                            "{side:?} disagreed at {cursor:?} over {rects:?}"
                        );
                    }
                }

                // Entering: the same pixel, for every fraction. The
                // implicit map names no screen — that is the point — so
                // this goes through the rectangle the span sits on, which
                // is the same rectangle the side model measures against.
                let edge_monitor = map.monitors()[map.spans()[0].monitor()].live();
                assert!(
                    map.monitors().iter().all(|m| m.id().is_none()),
                    "an implicit map reported an identity"
                );
                for raw in [0.0, 0.001, 0.25, 0.5, 0.75, 0.999, 1.0] {
                    let fraction = EdgeFraction::new(raw);
                    assert_eq!(
                        side.linked_edge().entry_point(edge_monitor, fraction),
                        topology.entering(fraction, &rects),
                        "{side:?} placed differently at {raw}"
                    );
                }

                // And the margin test agrees with the side model's, which is
                // what keeps the anti-bounce behaviour identical.
                let span = SpanId::from_index(0);
                for x in [-50, 0, 24, 25, 1000, 3815, 3816, 7654, 7655, 7679, 9000] {
                    assert_eq!(
                        map.clear_of(span, at(x, 1000), 24),
                        topology.clear_of_edge(at(x, 1000), &rects, 24),
                        "{side:?} disagreed about clearance at x = {x}"
                    );
                }
            }
        }
    }

    /// The implicit layout is exactly two rectangles — the edge monitor and
    /// its mirror — at revision 0, both under throw-away ids no platform
    /// reports and no peer could match.
    #[test]
    fn the_implicit_layout_is_one_real_rectangle_and_one_fiction() {
        let screens = [
            MonitorRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
            MonitorRect {
                left: 1920,
                top: 0,
                width: 2560,
                height: 1440,
            },
        ];
        let implicit = from_link_side(LinkSide::Left, LOCAL, &screens).unwrap();
        let arrangement = implicit.layout();
        assert_eq!(arrangement.revision(), 0);
        assert_eq!(arrangement.origin(), LOCAL);
        assert_eq!(arrangement.monitors().len(), 2);

        // The edge monitor is index 1 — the rightmost — and is placed under
        // that position's scaffolding id.
        let mine = arrangement
            .find(LOCAL, &id(&super::implicit_monitor_id(1)))
            .unwrap();
        assert_eq!(
            mine.rect,
            LayoutRect {
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440
            }
        );
        let peer_device = super::implicit_peer_device(LOCAL);
        assert_ne!(peer_device, LOCAL, "the scaffolding peer must be distinct");
        let theirs = arrangement
            .find(peer_device, &id(IMPLICIT_PEER_MONITOR_ID))
            .unwrap();
        assert_eq!(
            theirs.rect,
            LayoutRect {
                x: 4480,
                y: 0,
                width: 2560,
                height: 1440
            }
        );

        // A right member mirrors the other way, off the left of its own
        // leftmost screen.
        let implicit = from_link_side(LinkSide::Right, LOCAL, &screens).unwrap();
        let theirs = implicit
            .layout()
            .find(peer_device, &id(IMPLICIT_PEER_MONITOR_ID))
            .unwrap();
        assert_eq!(
            theirs.rect,
            LayoutRect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080
            }
        );

        // None of that scaffolding reaches the map: no identity in, none out.
        let map = implicit.crossings(LOCAL, &screens);
        assert!(map.monitors().iter().all(|m| m.id().is_none()));
        assert_eq!(map.spans()[0].target().device, None);
        assert_eq!(map.spans()[0].target().monitor, None);
    }

    /// The implicit path refuses only on **geometry**, never on identity —
    /// and both refusals are things no display reports. That is the whole
    /// improvement: a `--left` run that worked before ADR 0018 works after
    /// it, on any hardware, whatever the OS calls the screens.
    #[test]
    fn the_implicit_path_refuses_only_geometry_no_display_reports() {
        assert_eq!(
            from_link_side(LinkSide::Left, LOCAL, &[]),
            Err(ImplicitLayoutError::NoMonitors)
        );
        // Geometry no layout can hold is refused, rather than truncated.
        assert!(matches!(
            from_link_side(
                LinkSide::Left,
                LOCAL,
                &[MonitorRect {
                    left: 0,
                    top: 0,
                    width: u32::MAX,
                    height: 1080
                }]
            ),
            Err(ImplicitLayoutError::Unrepresentable { .. })
        ));
        // Anything a real display reports is accepted, whatever its name.
        for width in [1u32, 1920, 65_535] {
            assert!(
                from_link_side(
                    LinkSide::Left,
                    LOCAL,
                    &[MonitorRect {
                        left: -4000,
                        top: -4000,
                        width,
                        height: 1080
                    }]
                )
                .is_ok(),
                "a {width}-wide screen was refused"
            );
        }
    }

    /// The whole point of a round trip: on a drawn seam, a cursor that
    /// leaves and comes back comes back where it started.
    #[test]
    fn a_full_round_trip_between_two_machines_returns_home() {
        let arrangement = layout(vec![
            drawn(LOCAL, "L", 0, 0, 1920, 1080),
            drawn(PEER, "R", 1920, 0, 1920, 1080),
        ]);
        let mine = derive(&arrangement, LOCAL, &[live("L", 0, 0, 1920, 1080)]);
        let theirs = derive(&arrangement, PEER, &[live("R", 0, 0, 1920, 1080)]);

        let out = mine.leave(at(1919, 333)).unwrap();
        let arrival = theirs.arrive(&target_of(&mine, out), out.fraction).unwrap();
        assert_eq!(arrival, at(0, 333));

        let back = theirs.leave(arrival).unwrap();
        let home = mine
            .arrive(&target_of(&theirs, back), back.fraction)
            .unwrap();
        assert_eq!(home, at(1919, 333));
    }

    // ---- properties ----

    /// A believable arrangement plus the live geometry to go with it: a row
    /// of local monitors, and a column of peer monitors placed against the
    /// row's right edge (sometimes flush, sometimes with a gap that must
    /// produce nothing).
    ///
    /// The third element says whether the seam is flush, so a property can
    /// assert that the flush case really did produce spans — otherwise a
    /// derivation that silently produced none would satisfy every
    /// invariant below vacuously.
    fn drawn_and_live() -> impl Strategy<Value = (Layout, Vec<MonitorInfo>, bool)> {
        (
            prop::collection::vec((1u32..900, 1u32..900), 1..4),
            prop::collection::vec((1u32..900, 1u32..900), 1..4),
            prop::sample::select(vec![0i32, 0, 0, 1, 40]),
            prop::collection::vec(50u32..4000, 1..4),
        )
            .prop_map(|(locals, peers, gap, live_scales)| {
                let mut monitors = Vec::new();
                let mut x = 0i32;
                for (index, (width, height)) in locals.iter().enumerate() {
                    monitors.push(drawn(LOCAL, &format!("L{index}"), x, 0, *width, *height));
                    x += i32::try_from(*width).unwrap();
                }
                let seam = x + gap;
                let mut y = 0i32;
                for (index, (width, height)) in peers.iter().enumerate() {
                    monitors.push(drawn(PEER, &format!("P{index}"), seam, y, *width, *height));
                    y += i32::try_from(*height).unwrap();
                }
                let arrangement = Layout::new(1, LOCAL, monitors, &pair()).unwrap();

                // Live: the same monitors, at unrelated pixel sizes, laid
                // out left to right from the desktop origin.
                let mut live_monitors = Vec::new();
                let mut left = 0i32;
                for index in 0..locals.len() {
                    let extent = live_scales[index % live_scales.len()];
                    live_monitors.push(live(&format!("L{index}"), left, 0, extent, extent));
                    left += i32::try_from(extent).unwrap();
                }
                (arrangement, live_monitors, gap == 0)
            })
    }

    proptest! {
        /// Spans on one edge of one monitor never overlap, and no cursor
        /// crosses two of them: the half-open rule is a partition, not a
        /// preference. Every fraction the map produces is a valid `[0, 1]`
        /// value.
        #[test]
        fn spans_partition_each_edge((arrangement, screens, flush) in drawn_and_live()) {
            let map = derive(&arrangement, LOCAL, &screens);
            prop_assert_eq!(map.monitors().len(), screens.len());
            // A flush seam always abuts the last monitor of the row, so
            // the invariants below are never asserted about nothing.
            prop_assert_eq!(flush, map.span_count() > 0);

            for (index, first) in map.spans().iter().enumerate() {
                prop_assert!(first.span().length() > 0);
                for second in &map.spans()[index + 1..] {
                    if first.monitor() == second.monitor() && first.edge() == second.edge() {
                        prop_assert!(
                            first.span().end <= second.span().start
                                || second.span().end <= first.span().start,
                            "{:?} overlaps {:?}", first.span(), second.span()
                        );
                    }
                }
            }

            // Sweep the desktop's interesting columns and rows: at most one
            // crossing per (monitor, edge), and every fraction normalized.
            let width: i32 = screens.iter().map(|m| i32::try_from(m.rect.width).unwrap()).sum();
            let height = screens
                .iter()
                .map(|m| i32::try_from(m.rect.height).unwrap())
                .max()
                .unwrap();
            for x in [-1, 0, 1, width / 2, width - 1, width] {
                for y in [-1, 0, 1, height / 2, height - 1, height] {
                    let cursor = at(x, y);
                    let mut seen: Vec<(usize, Edge)> = Vec::new();
                    for id in map.crossings_at(cursor) {
                        let span = map.span(id).unwrap();
                        let key = (span.monitor(), span.edge());
                        prop_assert!(
                            !seen.contains(&key),
                            "two crossings on one edge at {:?}", cursor
                        );
                        seen.push(key);
                        let fraction = map.fraction_at(id, cursor).unwrap().value();
                        prop_assert!((0.0..=1.0).contains(&fraction));
                    }
                    if let Some(departure) = map.leave(cursor) {
                        prop_assert!((0.0..=1.0).contains(&departure.fraction.value()));
                        prop_assert_eq!(map.crossings_at(cursor).next(), Some(departure.span));
                    }
                }
            }
        }

        /// Derivation and both mappings are total: an arrangement the model
        /// accepted, *any* live rectangles the platform could report, and
        /// any cursor at all produce values rather than panics (NFR-1). The
        /// live geometry is left exactly as it arrived, whatever the layout
        /// made of it.
        #[test]
        fn any_live_geometry_and_any_cursor_are_survived(
            (arrangement, _, _) in drawn_and_live(),
            rows in prop::collection::vec(
                (
                    prop_oneof![Just(None), "[ -~]{0,70}".prop_map(Some)],
                    prop_oneof![Just(i32::MIN), Just(i32::MAX), Just(0), any::<i32>()],
                    prop_oneof![Just(i32::MIN), Just(i32::MAX), Just(0), any::<i32>()],
                    prop_oneof![Just(0u32), Just(1), Just(u32::MAX), any::<u32>()],
                    prop_oneof![Just(0u32), Just(1), Just(u32::MAX), any::<u32>()],
                ),
                0..5,
            ),
            cursor in (
                prop_oneof![Just(i32::MIN), Just(i32::MAX), any::<i32>()],
                prop_oneof![Just(i32::MIN), Just(i32::MAX), any::<i32>()],
            ),
            raw in -3.0f64..3.0,
        ) {
            let screens: Vec<MonitorInfo> = rows
                .into_iter()
                .map(|(id, left, top, width, height)| MonitorInfo {
                    id,
                    rect: MonitorRect { left, top, width, height },
                })
                .collect();
            let map = derive(&arrangement, LOCAL, &screens);
            prop_assert_eq!(map.monitors().len(), screens.len());
            for (mapped, original) in map.monitors().iter().zip(&screens) {
                prop_assert_eq!(mapped.live(), original.rect);
            }

            let cursor = at(cursor.0, cursor.1);
            let fraction = EdgeFraction::new(raw);
            for id in map.crossings_at(cursor).collect::<Vec<_>>() {
                let value = map.fraction_at(id, cursor).unwrap().value();
                prop_assert!((0.0..=1.0).contains(&value));
            }
            if let Some(departure) = map.leave(cursor) {
                prop_assert!((0.0..=1.0).contains(&departure.fraction.value()));
            }
            for id in map.spans_near(cursor, 24).collect::<Vec<_>>() {
                prop_assert!(!map.clear_of(id, cursor, 24));
            }
            for mapped in map.monitors() {
                if let Some(name) = mapped.id() {
                    for edge in Edge::ALL {
                        prop_assert!(map.enter(name, edge, fraction).is_some());
                    }
                }
            }
        }

        /// Leave, arrive, leave again, arrive home: the cursor comes back
        /// to the row it started on, within what the drawn grid can
        /// express.
        ///
        /// The tolerance is the quantization, stated rather than guessed:
        /// the trip rounds through a drawn edge at each of the four steps,
        /// so it can drift by at most a couple of layout units — each worth
        /// `live / drawn` pixels — plus the pixel roundings at the ends.
        #[test]
        fn a_round_trip_over_a_drawn_seam_returns_home_within_quantization(
            drawn_height in 200u32..2000,
            local_width in 200u32..2000,
            peer_width in 200u32..2000,
            local_scale in prop::sample::select(vec![100u32, 125, 150, 200]),
            peer_scale in prop::sample::select(vec![100u32, 125, 150, 200]),
            position in 0.0f64..1.0,
        ) {
            // Drawn in DIPs, snapped: two rectangles sharing a whole edge.
            let arrangement = layout(vec![
                drawn(LOCAL, "L", 0, 0, local_width, drawn_height),
                drawn(
                    PEER,
                    "R",
                    i32::try_from(local_width).unwrap(),
                    0,
                    peer_width,
                    drawn_height,
                ),
            ]);
            // Live: each machine's own pixels, at its own scale.
            let my_height = drawn_height * local_scale / 100;
            let my_width = local_width * local_scale / 100;
            let mine = derive(&arrangement, LOCAL, &[live("L", 0, 0, my_width, my_height)]);
            let theirs = derive(
                &arrangement,
                PEER,
                &[live(
                    "R",
                    0,
                    0,
                    peer_width * peer_scale / 100,
                    drawn_height * peer_scale / 100,
                )],
            );

            let last_row = i32::try_from(my_height - 1).unwrap();
            #[allow(clippy::cast_possible_truncation)]
            let row = (position * f64::from(last_row)).round() as i32;
            let column = i32::try_from(my_width - 1).unwrap();

            let out = mine
                .leave(at(column, row))
                .expect("a whole-edge seam always crosses");
            let arrival = theirs
                .arrive(&target_of(&mine, out), out.fraction)
                .expect("the target monitor is live on the far side");
            prop_assert_eq!(arrival.x, 0);

            let back = theirs
                .leave(arrival)
                .expect("the return seam is the same seam");
            let home = mine
                .arrive(&target_of(&theirs, back), back.fraction)
                .expect("the origin monitor is still live");
            prop_assert_eq!(home.x, column);

            let tolerance = 2 + 2 * i32::try_from(my_height.div_ceil(drawn_height)).unwrap();
            prop_assert!(
                (home.y - row).abs() <= tolerance,
                "left {}, came home to {} (tolerance {})", row, home.y, tolerance
            );
        }
    }

    /// A rectangle the layout model would refuse never reaches derivation,
    /// so the properties above may assume validated coordinates. Pinned
    /// here so a later relaxation of `Layout::new` is noticed.
    #[test]
    fn derivation_only_ever_sees_a_validated_arrangement() {
        let refused = Layout::from_raw(
            0,
            LOCAL,
            vec![RawPlacedMonitor {
                device: LOCAL,
                id: "A".to_owned(),
                rect: LayoutRect {
                    x: i32::MAX,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            }],
            &pair(),
        );
        assert!(refused.is_err());
    }
}
