//! Screen topology for seamless control transfer (ADR 0009).
//!
//! Pure geometry: the platform supplies the monitor layout and the cursor
//! position, and this module decides whether the cursor is crossing the
//! linked edge and translates the crossing position between two machines.
//! Nothing here is OS-specific.
//!
//! A crossing maps against the specific monitor on the linked edge — the
//! outermost one in that direction — not the whole virtual-desktop bounding
//! box. Monitors of different resolution leave dead space in the bounding
//! box (a shorter monitor beside a taller one), and mapping the fraction
//! against the box rather than the crossing monitor lands the peer's cursor
//! at the wrong height. Against the edge monitor it is exact (ADR 0009).
//!
//! The crossing position never travels as pixels. It is a fraction of the
//! edge monitor's height, so two machines of different resolution and
//! per-monitor DPI need no shared coordinate space — each maps the fraction
//! through its own geometry (ADR 0009). The side model here models
//! exactly one linked edge pair, left–right: the left member's right edge
//! links to the right member's left edge. The drawn arrangement that
//! supersedes it (ADR 0018) is derived in [`crate::crossing`], which speaks
//! all four [`Edge`]s.
//!
//! # The side model's geometry type is gone
//!
//! **Detection moved off it** (ADR 0018): [`crate::edge_driver`] measures
//! against a [`crate::crossing::CrossingMap`], derived from the drawn
//! arrangement — which for a `--left`/`--right` run is the *implicit*
//! layout [`crate::crossing::from_link_side`] builds, so the behaviour is
//! unchanged and the model is not.
//!
//! **Placement moved off it too** (this branch): `control_driver`'s
//! `SeamlessInputs` carries the run's [`crate::edge_driver::CrossingSource`]
//! and places an arriving `EntryPoint` through
//! [`crate::crossing::CrossingMap::enter`], falling back to
//! [`crate::crossing::CrossingMap::outer_entry`] for the degraded case ADR
//! 0018 specifies. Nothing in the running worker holds a `Topology` any
//! more, so the type survives only under `#[cfg(test)]`, as the oracle the
//! span model is required to agree with.
//!
//! [`Edge`], [`EdgeFraction`], [`last_index`], [`outer_monitor_index`] and
//! [`edge_monitor_index`] outlive the side model regardless: the drawn
//! arrangement is built on all of them, and they live here so there is
//! exactly one implementation of per-edge pixel geometry in the crate.

/// Which member of the left–right pair this machine is — the `--left` /
/// `--right` configuration (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSide {
    /// The left machine; its **right** edge links to the peer.
    Left,
    /// The right machine; its **left** edge links to the peer.
    Right,
}

impl LinkSide {
    /// The edge on this machine that links to the peer — the only edge
    /// that triggers a transfer.
    ///
    /// Always a **vertical** edge: the side model names one left–right
    /// pair, so it can never produce `Top` or `Bottom`. Everything
    /// geometric the side model needs is reached *through* this — the
    /// per-edge arithmetic lives on [`Edge`], in one copy shared with the
    /// drawn layout ([`crate::crossing`]), so there is no second
    /// implementation of "which column does the cursor ride" to drift.
    #[must_use]
    pub fn linked_edge(self) -> Edge {
        match self {
            Self::Left => Edge::Right,
            Self::Right => Edge::Left,
        }
    }
}

/// A monitor edge — all four sides, as the drawn layout addresses them
/// (ADR 0018).
///
/// The side model produces only `Left` and `Right` ([`LinkSide`]); `Top`
/// and `Bottom` exist for the drawn arrangement, where an over/under seam
/// is an ordinary crossing rather than an inexpressible one. The wire
/// enum has carried all four since protocol v4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The left edge of a monitor.
    Left,
    /// The right edge of a monitor.
    Right,
    /// The top edge of a monitor.
    Top,
    /// The bottom edge of a monitor.
    Bottom,
}

impl Edge {
    /// Every edge, in a fixed order. Derivation walks this, so the order a
    /// monitor's spans come out in is a property of the code rather than
    /// of a hash or an enumeration accident (NFR-2).
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Top, Self::Bottom];

    /// The edge that **faces** this one across a seam: a cursor leaving a
    /// monitor's `Right` edge arrives on the `Left` edge of whatever abuts
    /// it, and a cursor leaving a `Bottom` arrives on a `Top`.
    ///
    /// This is what turns a sender's own crossing edge into the wire
    /// `EntryPoint.edge`, which docs/PROTOCOL.md §6.1 specifies in the
    /// **receiver's** terms, and it is equally what
    /// [`crate::crossing::derive`] tests abutment against.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    /// The wire encoding of this edge.
    #[must_use]
    pub fn to_wire(self) -> crossover_protocol::control::Edge {
        match self {
            Self::Left => crossover_protocol::control::Edge::Left,
            Self::Right => crossover_protocol::control::Edge::Right,
            Self::Top => crossover_protocol::control::Edge::Top,
            Self::Bottom => crossover_protocol::control::Edge::Bottom,
        }
    }

    /// [`to_wire`](Self::to_wire)'s inverse — how a receiver reads an
    /// arriving `EntryPoint.edge`, which docs/PROTOCOL.md §6.1 states in
    /// the **receiver's own terms**: this is one of *its* monitors' edges,
    /// so no mirroring happens here or anywhere on the way in.
    #[must_use]
    pub fn from_wire(edge: crossover_protocol::control::Edge) -> Self {
        match edge {
            crossover_protocol::control::Edge::Left => Self::Left,
            crossover_protocol::control::Edge::Right => Self::Right,
            crossover_protocol::control::Edge::Top => Self::Top,
            crossover_protocol::control::Edge::Bottom => Self::Bottom,
        }
    }

    // The three functions below are the *only* implementation of per-edge
    // pixel geometry in the crate. Both models reach them: the side model
    // through [`LinkSide::linked_edge`], the drawn layout through each
    // span's own edge. The Schmitt-trigger distance and the last-pixel
    // column arithmetic therefore have exactly one home, which is what
    // stops the two models drifting a pixel apart at the seam.

    /// How far *inside* `monitor` the cursor sits, measured perpendicular
    /// to this edge: `0` on the edge's own outermost column or row, positive
    /// toward the interior, negative for a coordinate beyond it.
    ///
    /// The OS pins the cursor at the last pixel, so a `Right` edge's own
    /// column is `left + width − 1`, not `left + width`.
    ///
    /// Saturating throughout, so a nonsense coordinate or extent from the
    /// platform is a number rather than an overflow (NFR-1: input never
    /// panics).
    pub(crate) fn inset_of(self, monitor: MonitorRect, cursor: CursorPoint) -> i32 {
        match self {
            Self::Left => cursor.x.saturating_sub(monitor.left),
            Self::Right => monitor
                .left
                .saturating_add(last_index(monitor.width))
                .saturating_sub(cursor.x),
            Self::Top => cursor.y.saturating_sub(monitor.top),
            Self::Bottom => monitor
                .top
                .saturating_add(last_index(monitor.height))
                .saturating_sub(cursor.y),
        }
    }

    /// How far *along* this edge of `monitor` the cursor sits, in pixels
    /// from the edge's start, together with the edge's own length.
    ///
    /// The lateral counterpart to [`inset_of`](Self::inset_of): that one
    /// measures perpendicular to the edge, this one along it. Unlike
    /// [`fraction_along`](Self::fraction_along) it does **not** refuse a
    /// cursor outside the monitor's extent — it reports *how far* outside,
    /// which is what a lateral margin test needs
    /// ([`crate::crossing::CrossingMap::clear_of`]).
    ///
    /// Saturating, for the same reason `inset_of` is.
    pub(crate) fn offset_along(self, monitor: MonitorRect, cursor: CursorPoint) -> (i32, u32) {
        match self {
            Self::Left | Self::Right => (cursor.y.saturating_sub(monitor.top), monitor.height),
            Self::Top | Self::Bottom => (cursor.x.saturating_sub(monitor.left), monitor.width),
        }
    }

    /// How far *along* this edge of `monitor` the cursor sits, as a
    /// fraction of the edge — or `None` if the cursor is outside the
    /// monitor's extent along it.
    ///
    /// That guard is what keeps a cursor sharing a seam column with a
    /// taller neighbour, at a row only the neighbour covers, from being
    /// mapped against this monitor's height.
    pub(crate) fn fraction_along(
        self,
        monitor: MonitorRect,
        cursor: CursorPoint,
    ) -> Option<EdgeFraction> {
        let (offset, extent) = self.offset_along(monitor, cursor);
        (offset >= 0 && offset <= last_index(extent))
            .then(|| EdgeFraction::from_pixel(offset, extent))
    }

    /// The corner of `monitor` that sits on this edge, as a position.
    ///
    /// Only the coordinate **perpendicular** to the edge is meaningful —
    /// the column a `Left`/`Right` edge occupies, the row a `Top`/`Bottom`
    /// one does — which is exactly the coordinate [`Edge::inset_of`] reads.
    /// That is what lets one monitor's edge be measured against another's
    /// with the same arithmetic that measures a cursor against an edge.
    pub(crate) fn outer_point(self, monitor: MonitorRect) -> CursorPoint {
        self.entry_point(monitor, EdgeFraction::new(0.0))
    }

    /// The pixel `fraction` of the way along this edge of `monitor` — where
    /// a cursor arriving across this edge is placed.
    pub(crate) fn entry_point(self, monitor: MonitorRect, fraction: EdgeFraction) -> CursorPoint {
        match self {
            Self::Left => CursorPoint {
                x: monitor.left,
                y: monitor
                    .top
                    .saturating_add(fraction.to_pixel(monitor.height)),
            },
            Self::Right => CursorPoint {
                x: monitor.left.saturating_add(last_index(monitor.width)),
                y: monitor
                    .top
                    .saturating_add(fraction.to_pixel(monitor.height)),
            },
            Self::Top => CursorPoint {
                x: monitor
                    .left
                    .saturating_add(fraction.to_pixel(monitor.width)),
                y: monitor.top,
            },
            Self::Bottom => CursorPoint {
                x: monitor
                    .left
                    .saturating_add(fraction.to_pixel(monitor.width)),
                y: monitor.top.saturating_add(last_index(monitor.height)),
            },
        }
    }
}

// The display geometry vocabulary — the monitor layout and a cursor
// position — lives in `crossover-platform`, because the display HAL trait
// must speak it and core cannot be its dependency (docs/ARCHITECTURE.md
// §2), exactly as with the input vocabulary. Re-exported so the topology
// model reads as one module.
pub use crossover_platform::{CursorPoint, MonitorRect, Screen};

/// A normalized position along an edge, clamped to `[0, 1]`: `0.0` is the
/// top of a vertical edge, `1.0` the bottom. This is what crosses the
/// wire, so differing resolutions and DPI map through a ratio rather than
/// a shared pixel space (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeFraction(f64);

impl EdgeFraction {
    /// Clamp an arbitrary value into a valid fraction. A `NaN` input
    /// becomes `0.0` (the `clamp` contract with a non-`NaN` range).
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(if value.is_nan() {
            0.0
        } else {
            value.clamp(0.0, 1.0)
        })
    }

    /// The underlying value, always in `[0, 1]`.
    #[must_use]
    pub fn value(self) -> f64 {
        self.0
    }

    /// The wire encoding of the position (ADR 0009): the fraction scaled
    /// onto `0..=u16::MAX`, so it travels as a compact, always-valid
    /// integer rather than a float. `0` is the top of the edge, `u16::MAX`
    /// the bottom.
    #[must_use]
    pub fn to_wire(self) -> u16 {
        // self.0 ∈ [0, 1], so the product rounds into [0, u16::MAX]: it
        // cannot truncate or lose sign.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled = (self.0 * f64::from(u16::MAX)).round() as u16;
        scaled
    }

    /// Decode a wire position ([`to_wire`](Self::to_wire)'s inverse).
    #[must_use]
    pub fn from_wire(raw: u16) -> Self {
        Self(f64::from(raw) / f64::from(u16::MAX))
    }

    /// The fraction of an `extent`-long edge at offset `offset` along it —
    /// a pixel row on a vertical edge, a column on a horizontal one.
    ///
    /// Offsets map to the full `[0, 1]` range — offset `0` is `0.0`, the
    /// last one is `1.0` — so a round trip through
    /// [`to_pixel`](Self::to_pixel) on the same extent recovers the offset
    /// exactly. An offset outside the edge clamps in.
    #[must_use]
    pub(crate) fn from_pixel(offset: i32, extent: u32) -> Self {
        let last = last_index(extent);
        if last <= 0 {
            return Self(0.0); // a zero- or one-pixel edge has no span
        }
        let offset = offset.clamp(0, last);
        Self(f64::from(offset) / f64::from(last))
    }

    /// The offset this fraction lands on along an `extent`-long edge, the
    /// inverse of [`from_pixel`](Self::from_pixel) against that extent.
    #[must_use]
    pub(crate) fn to_pixel(self, extent: u32) -> i32 {
        let last = last_index(extent);
        if last <= 0 {
            return 0;
        }
        // self.0 ∈ [0, 1] and `last` fits i32, so the product rounds into
        // [0, last]: well within i32, no truncation or sign loss.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let offset = (self.0 * f64::from(last)).round() as i32;
        offset.clamp(0, last)
    }
}

/// The last valid pixel index on a `size`-long axis (`size − 1`), or `0`
/// for a degenerate zero-length axis. Never panics.
pub(crate) fn last_index(size: u32) -> i32 {
    i32::try_from(size.saturating_sub(1)).unwrap_or(i32::MAX)
}

/// Which monitor's `edge` lies on the **outer boundary of the desktop** in
/// that direction — the leftmost screen for [`Edge::Left`], the bottommost
/// for [`Edge::Bottom`], and so on — as an index into `monitors`. `None`
/// only for an empty list, which a real display never reports.
///
/// **The one selector**, and it answers two questions that have to give
/// the same screen:
///
/// - which monitor the side model's linked edge belongs to
///   ([`edge_monitor_index`], and through it the implicit-layout bridge
///   `crate::crossing::from_link_side`) — two implementations that
///   disagreed would attach a crossing span to a different physical screen
///   than the detector measures against;
/// - where ADR 0018's **degraded** placement lands an entry point this
///   machine cannot honour (`crate::crossing::CrossingMap::outer_entry`) —
///   the "desktop-bounds edge matching `EntryPoint.edge`" of
///   docs/PROTOCOL.md §6.1.
///
/// The fraction is taken against *that monitor's* edge rather than against
/// the desktop bounding box, which is ADR 0009's 2026-08-09 refinement: a
/// shorter monitor beside a taller one leaves dead space in the box, and
/// mapping through the box lands the cursor at the wrong height. The
/// monitor's edge and the box's edge are the same line; only the extent
/// differs, and the monitor's is the correct one.
///
/// Generic over an iterator of rectangles so a caller holding richer
/// monitor records need not copy them into a scratch vector to ask.
///
/// **Ties are inherited, not invented.** Two monitors ending on the same
/// column — a stacked pair — tie on the key, and `max_by_key` keeps the
/// *last* such element while `min_by_key` keeps the *first*. That asymmetry
/// is ADR 0009's original behaviour and is preserved deliberately rather
/// than tidied, so this generalization changes no crossing that works
/// today; it is pinned by
/// `every_edge_monitor_selection_agrees_on_the_same_screen`.
///
/// The key saturates: a hostile or nonsense rectangle must not overflow
/// (NFR-1).
pub(crate) fn outer_monitor_index<I>(edge: Edge, monitors: I) -> Option<usize>
where
    I: IntoIterator<Item = MonitorRect>,
{
    let indexed = monitors.into_iter().enumerate();
    match edge {
        Edge::Left => indexed.min_by_key(|(_, m)| m.left).map(|(index, _)| index),
        Edge::Right => indexed
            .max_by_key(|(_, m)| m.left.saturating_add(last_index(m.width)))
            .map(|(index, _)| index),
        Edge::Top => indexed.min_by_key(|(_, m)| m.top).map(|(index, _)| index),
        Edge::Bottom => indexed
            .max_by_key(|(_, m)| m.top.saturating_add(last_index(m.height)))
            .map(|(index, _)| index),
    }
}

/// Which monitor sits on `side`'s linked edge — the outermost one in the
/// linked direction (the rightmost for a left member, the leftmost for a
/// right member).
///
/// A thin naming of [`outer_monitor_index`] in the side model's vocabulary;
/// the side model has exactly two members and neither links vertically, so
/// [`LinkSide::linked_edge`] can only ever produce `Left` or `Right`.
pub(crate) fn edge_monitor_index<I>(side: LinkSide, monitors: I) -> Option<usize>
where
    I: IntoIterator<Item = MonitorRect>,
{
    outer_monitor_index(side.linked_edge(), monitors)
}

/// The two-machine left–right topology (ADR 0009): one linked edge pair,
/// this machine being the left or the right member.
///
/// **Test-only since ADR 0018.** Nothing in the running worker holds one:
/// detection measures against a [`crate::crossing::CrossingMap`] and
/// placement resolves an `EntryPoint` through it. It survives as the
/// **equivalence oracle** the span model is run against
/// (`edge_driver`'s `the_span_detector_reproduces_the_side_model_across_a_cursor_script`
/// and `crossing`'s placement comparisons).
///
/// # What kind of oracle this is, precisely
///
/// An independent **composition**, not an independent implementation. The
/// pixel arithmetic underneath — [`Edge::inset_of`],
/// [`Edge::offset_along`], [`Edge::entry_point`], [`EdgeFraction`] — is
/// shared with the span model deliberately, because two implementations of
/// "which column does the cursor ride" that drifted a pixel apart would
/// drift at exactly the seam where a wrong answer hands control away. A
/// bug *inside* those primitives is therefore invisible to this oracle:
/// both sides would be wrong together. That is a trade taken knowingly,
/// and the primitives carry their own tests.
///
/// What it pins independently is everything **above** the primitives —
/// which is where ADR 0018 changed the model, and so where a regression
/// would come from:
///
/// - **which screen** a crossing attaches to, and which one an arrival is
///   placed against (the edge-monitor selection, ties included);
/// - **which edge** of that screen is the crossing edge, and which edge an
///   arrival lands on;
/// - **when** a touch counts — the threshold, the re-arm margin, and the
///   direction the margin is measured in — and **what fraction** results.
///
/// The side model answers those with one flag and a bounding box; the span
/// model answers them by deriving spans from a drawn arrangement. That two
/// such different compositions agree pixel-for-pixel across a nasty cursor
/// script is the claim worth keeping. Deleting this type would not remove
/// production code; it would remove the only statement of that claim.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Topology {
    side: LinkSide,
}

#[cfg(test)]
impl Topology {
    /// A topology for a machine on `side` of the pair.
    #[must_use]
    pub(crate) fn new(side: LinkSide) -> Self {
        Self { side }
    }

    /// The edge that links to the peer.
    #[must_use]
    pub(crate) fn linked_edge(self) -> Edge {
        self.side.linked_edge()
    }

    /// The monitor on the linked edge — the outermost one in the linked
    /// direction (the rightmost for a left member, the leftmost for a
    /// right member). `None` only for an empty layout, which a real
    /// display never reports. Crossings map against *this* monitor's
    /// height, so mismatched-resolution monitors and the dead space
    /// between them place the peer's cursor correctly (ADR 0009).
    #[must_use]
    fn edge_monitor(self, monitors: &[MonitorRect]) -> Option<MonitorRect> {
        edge_monitor_index(self.side, monitors.iter().copied()).map(|index| monitors[index])
    }

    /// If `cursor` is against the linked edge of the edge monitor, the
    /// normalized crossing position to hand the peer; otherwise `None`
    /// (the cursor is not leaving). The position is a fraction of that
    /// monitor's height, so the peer places its cursor through its own
    /// geometry.
    #[must_use]
    pub(crate) fn leaving(
        self,
        cursor: CursorPoint,
        monitors: &[MonitorRect],
    ) -> Option<EdgeFraction> {
        let monitor = self.edge_monitor(monitors)?;
        let edge = self.linked_edge();
        // Touching means reaching the extreme column — or, defensively,
        // any coordinate at or beyond it, since the side model's linked
        // edge is the outer edge of the whole desktop and nothing of this
        // machine's lies past it.
        if edge.inset_of(monitor, cursor) <= 0 {
            edge.fraction_along(monitor, cursor)
        } else {
            None
        }
    }

    /// Is `cursor` clear of the linked edge by more than `margin` pixels —
    /// far enough inside the screen that a fresh approach to the edge is a
    /// deliberate one, not a wobble at the seam?
    ///
    /// This is the *release* half of a Schmitt trigger around
    /// [`leaving`](Self::leaving)'s bare threshold. Detection alone cannot
    /// distinguish a cursor resting on the entry column (where a transfer
    /// puts it) from a cursor arriving at it, so a one-pixel excursion and
    /// return read as a genuine crossing; requiring real travel away from
    /// the column before the next crossing counts breaks that loop.
    ///
    /// Judged on the horizontal distance from the linked column only: the
    /// column is what a crossing is measured against, and a layout with no
    /// monitors (never reported by a real display) is never "clear", so the
    /// degenerate case can only ever suppress a crossing, not invent one.
    #[must_use]
    pub(crate) fn clear_of_edge(
        self,
        cursor: CursorPoint,
        monitors: &[MonitorRect],
        margin: u32,
    ) -> bool {
        let Some(monitor) = self.edge_monitor(monitors) else {
            return false;
        };
        let margin = i32::try_from(margin).unwrap_or(i32::MAX);
        self.linked_edge().inset_of(monitor, cursor) > margin
    }

    /// Where the cursor should appear when control arrives here for a peer
    /// that crossed at `fraction`: on this machine's edge monitor, at that
    /// fraction of the monitor's height. The inverse direction of
    /// [`leaving`](Self::leaving), and the same edge in reverse.
    #[must_use]
    pub(crate) fn entering(self, fraction: EdgeFraction, monitors: &[MonitorRect]) -> CursorPoint {
        let Some(monitor) = self.edge_monitor(monitors) else {
            return CursorPoint { x: 0, y: 0 };
        };
        self.linked_edge().entry_point(monitor, fraction)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{CursorPoint, Edge, EdgeFraction, LinkSide, MonitorRect, Topology};
    use crossover_protocol::control::Edge as WireEdge;

    /// A single monitor `width`×`height` at the desktop origin.
    fn one(width: u32, height: u32) -> Vec<MonitorRect> {
        vec![MonitorRect {
            left: 0,
            top: 0,
            width,
            height,
        }]
    }

    /// One 1920×1080 monitor — the common single-display case.
    fn hd() -> Vec<MonitorRect> {
        one(1920, 1080)
    }

    #[test]
    fn each_side_links_on_the_edge_facing_the_peer() {
        assert_eq!(LinkSide::Left.linked_edge(), Edge::Right);
        assert_eq!(LinkSide::Right.linked_edge(), Edge::Left);
    }

    /// `opposite` is its own inverse, and `to_wire` maps onto the
    /// like-named protocol variant — pinned directly, since
    /// `wire_entry_point`'s tests rely on both holding (docs/PROTOCOL.md
    /// §6.1: `EntryPoint.edge` is the receiver's arrival edge).
    #[test]
    fn opposite_and_to_wire_are_pinned() {
        assert_eq!(Edge::Left.opposite(), Edge::Right);
        assert_eq!(Edge::Right.opposite(), Edge::Left);
        assert_eq!(Edge::Top.opposite(), Edge::Bottom);
        assert_eq!(Edge::Bottom.opposite(), Edge::Top);
        for edge in Edge::ALL {
            assert_eq!(edge.opposite().opposite(), edge);
        }

        assert_eq!(Edge::Left.to_wire(), WireEdge::Left);
        assert_eq!(Edge::Right.to_wire(), WireEdge::Right);
        assert_eq!(Edge::Top.to_wire(), WireEdge::Top);
        assert_eq!(Edge::Bottom.to_wire(), WireEdge::Bottom);

        // `ALL` is every variant exactly once — the property derivation
        // relies on to walk a monitor's four sides.
        assert_eq!(Edge::ALL.len(), 4);
        for edge in Edge::ALL {
            assert_eq!(Edge::ALL.iter().filter(|&&e| e == edge).count(), 1);
        }
    }

    /// The side model links horizontally and only horizontally, so
    /// `linked_edge` is a vertical edge for both members. The drawn layout
    /// is where a `Top`/`Bottom` crossing becomes expressible.
    #[test]
    fn the_side_model_never_names_a_horizontal_edge() {
        for side in [LinkSide::Left, LinkSide::Right] {
            assert!(matches!(side.linked_edge(), Edge::Left | Edge::Right));
        }
    }

    #[test]
    fn leaving_fires_only_at_the_linked_edge() {
        let left = Topology::new(LinkSide::Left); // links on its right edge
        // Somewhere in the middle: not leaving.
        assert_eq!(left.leaving(CursorPoint { x: 960, y: 540 }, &hd()), None);
        // The opposite (left) edge is inert for the left member.
        assert_eq!(left.leaving(CursorPoint { x: 0, y: 540 }, &hd()), None);
        // The right edge (x == width − 1): leaving.
        assert!(
            left.leaving(CursorPoint { x: 1919, y: 540 }, &hd())
                .is_some()
        );

        let right = Topology::new(LinkSide::Right); // links on its left edge
        assert!(right.leaving(CursorPoint { x: 0, y: 540 }, &hd()).is_some());
        assert_eq!(right.leaving(CursorPoint { x: 1919, y: 540 }, &hd()), None);
    }

    #[test]
    fn a_coordinate_past_the_edge_still_counts_as_touching() {
        let left = Topology::new(LinkSide::Left);
        assert!(
            left.leaving(CursorPoint { x: 5000, y: 10 }, &hd())
                .is_some()
        );
        let right = Topology::new(LinkSide::Right);
        assert!(right.leaving(CursorPoint { x: -5, y: 10 }, &hd()).is_some());
    }

    #[test]
    fn clear_of_edge_needs_real_travel_away_from_the_linked_column() {
        let left = Topology::new(LinkSide::Left); // links on x == 1919
        // On the column, and just off it: not clear — this is the wobble a
        // transfer's entry placement leaves the cursor in.
        assert!(!left.clear_of_edge(CursorPoint { x: 1919, y: 540 }, &hd(), 24));
        assert!(!left.clear_of_edge(CursorPoint { x: 1918, y: 540 }, &hd(), 24));
        // Exactly the margin is still not clear; one pixel further is.
        assert!(!left.clear_of_edge(CursorPoint { x: 1895, y: 540 }, &hd(), 24));
        assert!(left.clear_of_edge(CursorPoint { x: 1894, y: 540 }, &hd(), 24));
        // Past the column (the OS can report beyond it) is never clear.
        assert!(!left.clear_of_edge(CursorPoint { x: 5000, y: 540 }, &hd(), 24));

        let right = Topology::new(LinkSide::Right); // links on x == 0
        assert!(!right.clear_of_edge(CursorPoint { x: 0, y: 540 }, &hd(), 24));
        assert!(!right.clear_of_edge(CursorPoint { x: 24, y: 540 }, &hd(), 24));
        assert!(right.clear_of_edge(CursorPoint { x: 25, y: 540 }, &hd(), 24));
        assert!(!right.clear_of_edge(CursorPoint { x: -50, y: 540 }, &hd(), 24));

        // A zero margin degenerates to "anywhere but the column itself".
        assert!(left.clear_of_edge(CursorPoint { x: 1918, y: 540 }, &hd(), 0));
        assert!(!left.clear_of_edge(CursorPoint { x: 1919, y: 540 }, &hd(), 0));
    }

    #[test]
    fn clear_of_edge_is_measured_against_the_edge_monitor() {
        // Two monitors side by side: the linked (right) edge is the outer
        // one's far column, so the whole first monitor is clear of it.
        let monitors = [
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
        let left = Topology::new(LinkSide::Left);
        assert!(left.clear_of_edge(CursorPoint { x: 1919, y: 540 }, &monitors, 24));
        assert!(!left.clear_of_edge(CursorPoint { x: 3839, y: 540 }, &monitors, 24));
    }

    /// A rectangle whose far column overflows `i32` reaches the selector
    /// from the platform, not from a validated layout, so the arithmetic
    /// saturates rather than panicking in debug or wrapping in release
    /// (NFR-1). A wrap here would be the worst kind: the *rightmost*
    /// monitor would compare as the leftmost.
    #[test]
    fn a_hostile_rectangle_saturates_rather_than_wrapping() {
        let absurd = [
            MonitorRect {
                left: i32::MAX,
                top: i32::MAX,
                width: u32::MAX,
                height: u32::MAX,
            },
            MonitorRect {
                left: i32::MIN,
                top: i32::MIN,
                width: u32::MAX,
                height: u32::MAX,
            },
            MonitorRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
        ];
        for side in [LinkSide::Left, LinkSide::Right] {
            let topology = Topology::new(side);
            for cursor in [
                CursorPoint {
                    x: i32::MIN,
                    y: i32::MIN,
                },
                CursorPoint {
                    x: i32::MAX,
                    y: i32::MAX,
                },
                CursorPoint { x: 0, y: 0 },
            ] {
                let _ = topology.leaving(cursor, &absurd);
                let _ = topology.clear_of_edge(cursor, &absurd, u32::MAX);
                let _ = topology.entering(EdgeFraction::new(0.5), &absurd);
            }
            // The extreme rectangle really is the one selected, which is
            // what makes the saturation load-bearing rather than incidental.
            let chosen =
                super::edge_monitor_index(side, absurd.iter().copied()).expect("a monitor");
            assert_eq!(chosen, usize::from(side == LinkSide::Right));
        }
    }

    #[test]
    fn clear_of_edge_never_panics_on_a_degenerate_layout_or_coordinate() {
        let left = Topology::new(LinkSide::Left);
        // No monitors: nothing to be clear of, so never clear (a crossing
        // can only be suppressed, never invented).
        assert!(!left.clear_of_edge(CursorPoint { x: 0, y: 0 }, &[], 24));
        // Extreme coordinates and margins saturate rather than overflow.
        assert!(!left.clear_of_edge(CursorPoint { x: i32::MAX, y: 0 }, &hd(), u32::MAX));
        assert!(!left.clear_of_edge(CursorPoint { x: i32::MIN, y: 0 }, &hd(), u32::MAX));
        let right = Topology::new(LinkSide::Right);
        assert!(!right.clear_of_edge(CursorPoint { x: i32::MAX, y: 0 }, &hd(), u32::MAX));
        assert!(!right.clear_of_edge(CursorPoint { x: i32::MIN, y: 0 }, &hd(), u32::MAX));
    }

    #[test]
    fn the_crossing_fraction_spans_the_full_edge() {
        let left = Topology::new(LinkSide::Left);
        let top = left.leaving(CursorPoint { x: 1919, y: 0 }, &hd()).unwrap();
        let bottom = left
            .leaving(CursorPoint { x: 1919, y: 1079 }, &hd())
            .unwrap();
        assert!((top.value() - 0.0).abs() < 1e-9);
        assert!((bottom.value() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn entering_lands_on_the_linked_edge_at_the_fraction() {
        // Right member: cursor enters on its left edge (x == 0).
        let right = Topology::new(LinkSide::Right);
        assert_eq!(
            right.entering(EdgeFraction::new(0.0), &hd()),
            CursorPoint { x: 0, y: 0 }
        );
        assert_eq!(
            right.entering(EdgeFraction::new(1.0), &hd()),
            CursorPoint { x: 0, y: 1079 }
        );
        assert_eq!(
            right.entering(EdgeFraction::new(0.5), &hd()),
            CursorPoint { x: 0, y: 540 } // round(0.5 * 1079) = 540
        );

        // Left member: cursor enters on its right edge (x == width − 1).
        let left = Topology::new(LinkSide::Left);
        assert_eq!(
            left.entering(EdgeFraction::new(0.0), &hd()),
            CursorPoint { x: 1919, y: 0 }
        );
    }

    #[test]
    fn a_crossing_maps_across_a_resolution_and_dpi_difference() {
        // A (left, 1920×1080) hands off to B (right, 2560×1440): the
        // vertical position is preserved as a proportion, not pixels.
        let a = Topology::new(LinkSide::Left);
        let b = Topology::new(LinkSide::Right);
        let b_monitors = one(2560, 1440);

        let frac = a.leaving(CursorPoint { x: 1919, y: 540 }, &hd()).unwrap(); // half-ish
        let entry = b.entering(frac, &b_monitors);
        assert_eq!(entry.x, 0); // B's left edge
        // 540/1079 of B's 1439-tall edge ≈ 720; proportional, not equal.
        assert!((entry.y - 720).abs() <= 1, "got {}", entry.y);
    }

    #[test]
    fn the_crossing_maps_against_the_edge_monitor_not_the_bounding_box() {
        // The soak layout: a left machine with a tall laptop panel
        // (3840×2400 at the origin) and a shorter external 4K to its right
        // (3840×2160). The linked (right) edge is the external's, so a
        // crossing must be a fraction of 2160 — not the 2400-tall bounding
        // box, which would shift the peer's cursor upward.
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
        let a = Topology::new(LinkSide::Left);
        let monitors = [laptop, external];

        // Cursor riding the far-right column, halfway down the external.
        let frac = a
            .leaving(CursorPoint { x: 7679, y: 1080 }, &monitors)
            .unwrap();
        // 1080 of the external's 2159-tall edge ≈ 0.5, not 1080/2399.
        assert!((frac.value() - 0.5).abs() < 0.01, "got {}", frac.value());

        // Touching the laptop's right seam (x == 3839) is *not* the edge —
        // that column is interior to the desktop, the monitor boundary.
        assert_eq!(a.leaving(CursorPoint { x: 3839, y: 1080 }, &monitors), None);

        // B is a single 3840×2160 display: the fraction lands mid-screen.
        let b = Topology::new(LinkSide::Right);
        let entry = b.entering(frac, &one(3840, 2160));
        assert_eq!(entry.x, 0);
        assert!((entry.y - 1080).abs() <= 2, "got {}", entry.y);
    }

    #[test]
    fn a_full_round_trip_between_two_machines_returns_home() {
        // A leaves at some height; B receives; B leaves back; A receives.
        // On identical screens the cursor comes home to the same row.
        let a = Topology::new(LinkSide::Left);
        let b = Topology::new(LinkSide::Right);

        let out = a.leaving(CursorPoint { x: 1919, y: 333 }, &hd()).unwrap();
        let b_entry = b.entering(out, &hd());
        assert_eq!(b_entry, CursorPoint { x: 0, y: 333 });

        // B returns from its left edge at the same row.
        let back = b.leaving(CursorPoint { x: 0, y: 333 }, &hd()).unwrap();
        let a_entry = a.entering(back, &hd());
        assert_eq!(a_entry, CursorPoint { x: 1919, y: 333 });
    }

    #[test]
    fn fractions_clamp_and_reject_nonsense() {
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(approx(EdgeFraction::new(-2.0).value(), 0.0));
        assert!(approx(EdgeFraction::new(9.0).value(), 1.0));
        assert!(approx(EdgeFraction::new(f64::NAN).value(), 0.0));
        assert!(approx(EdgeFraction::new(0.25).value(), 0.25));
    }

    #[test]
    fn the_wire_encoding_round_trips_within_one_pixel() {
        // Endpoints are exact; interior values recover within the u16 grid.
        assert_eq!(EdgeFraction::new(0.0).to_wire(), 0);
        assert_eq!(EdgeFraction::new(1.0).to_wire(), u16::MAX);
        for raw in [0u16, 1, 12_345, 32_768, u16::MAX] {
            assert_eq!(EdgeFraction::from_wire(raw).to_wire(), raw);
        }
        // A wire value maps onto the same pixel row it came from.
        let frac = EdgeFraction::from_pixel(720, 1080);
        let recovered = EdgeFraction::from_wire(frac.to_wire());
        assert!((recovered.to_pixel(1080) - 720).abs() <= 1);
    }

    #[test]
    fn degenerate_layouts_never_panic() {
        let t = Topology::new(LinkSide::Left);
        // An empty layout (never from a real display) is inert, not a panic.
        assert_eq!(t.leaving(CursorPoint { x: 0, y: 0 }, &[]), None);
        assert_eq!(
            t.entering(EdgeFraction::new(0.5), &[]),
            CursorPoint { x: 0, y: 0 }
        );
        for monitors in [one(0, 0), one(1, 1)] {
            let _ = t.leaving(CursorPoint { x: 0, y: 0 }, &monitors);
            let entry = t.entering(EdgeFraction::new(0.5), &monitors);
            assert!(entry.x >= 0 && entry.y >= 0);
        }
    }

    proptest! {
        /// Pixel → fraction → pixel on the same height recovers the row
        /// (clamped into range): the mapping is a faithful inverse.
        #[test]
        fn pixel_fraction_round_trip_is_exact(height in 2u32..8000, y in -100i32..8100) {
            let frac = EdgeFraction::from_pixel(y, height);
            let back = frac.to_pixel(height);
            let last = i32::try_from(height - 1).unwrap();
            prop_assert_eq!(back, y.clamp(0, last));
        }

        /// Every fraction the model produces is a valid `[0, 1]` value,
        /// whatever the pixel and height.
        #[test]
        fn produced_fractions_stay_normalized(height in 0u32..8000, y in -8000i32..8000) {
            let v = EdgeFraction::from_pixel(y, height).value();
            prop_assert!((0.0..=1.0).contains(&v));
        }

        /// Entry always lands inside the edge monitor, for any fraction and
        /// monitor placement.
        #[test]
        fn entry_stays_on_the_edge_monitor(
            side in prop::sample::select(vec![LinkSide::Left, LinkSide::Right]),
            left in -4000i32..4000,
            top in -4000i32..4000,
            width in 1u32..8000,
            height in 1u32..8000,
            raw in -3.0f64..3.0,
        ) {
            let monitors = [MonitorRect { left, top, width, height }];
            let entry = Topology::new(side).entering(EdgeFraction::new(raw), &monitors);
            let last_x = left + i32::try_from(width - 1).unwrap();
            let last_y = top + i32::try_from(height - 1).unwrap();
            prop_assert!((left..=last_x).contains(&entry.x));
            prop_assert!((top..=last_y).contains(&entry.y));
        }
    }
}
