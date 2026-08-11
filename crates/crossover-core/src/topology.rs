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
//! through its own geometry (ADR 0009). This phase models exactly one
//! linked edge pair, left–right: the left member's right edge links to the
//! right member's left edge. [`Edge`] is the extension point for the other
//! edges, which are out of scope here.

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
    #[must_use]
    pub fn linked_edge(self) -> Edge {
        match self {
            Self::Left => Edge::Right,
            Self::Right => Edge::Left,
        }
    }
}

/// A screen edge. Only the vertical edges are modelled this phase; the
/// enum is where the horizontal edges are added later (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The left edge of a monitor.
    Left,
    /// The right edge of a monitor.
    Right,
}

impl Edge {
    /// The outer `x` of `monitor` for this edge — the extreme column the
    /// cursor reaches when riding this edge. The OS pins the cursor at the
    /// last pixel, so the right edge is the monitor's last column.
    fn outer_x(self, monitor: MonitorRect) -> i32 {
        match self {
            Self::Left => monitor.left,
            Self::Right => monitor.left + last_index(monitor.width),
        }
    }

    /// Does a cursor at horizontal position `x` touch this edge of
    /// `monitor`? Touching means reaching the extreme column (or,
    /// defensively, any coordinate at or beyond it).
    fn touched_by(self, x: i32, monitor: MonitorRect) -> bool {
        match self {
            Self::Left => x <= self.outer_x(monitor),
            Self::Right => x >= self.outer_x(monitor),
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

    /// The fraction of a `height`-tall edge at pixel row `y`. Rows map to
    /// the full `[0, 1]` range — row `0` is `0.0`, the last row is `1.0`
    /// — so a round trip through [`to_pixel`](Self::to_pixel) on the same
    /// height recovers the row exactly. `y` outside the edge clamps in.
    #[must_use]
    fn from_pixel(y: i32, height: u32) -> Self {
        let last = last_index(height);
        if last <= 0 {
            return Self(0.0); // a zero- or one-row edge has no span
        }
        let y = y.clamp(0, last);
        Self(f64::from(y) / f64::from(last))
    }

    /// The pixel row this fraction lands on for a `height`-tall edge, the
    /// inverse of [`from_pixel`](Self::from_pixel) against that height.
    #[must_use]
    fn to_pixel(self, height: u32) -> i32 {
        let last = last_index(height);
        if last <= 0 {
            return 0;
        }
        // self.0 ∈ [0, 1] and `last` fits i32, so the product rounds into
        // [0, last]: well within i32, no truncation or sign loss.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row = (self.0 * f64::from(last)).round() as i32;
        row.clamp(0, last)
    }
}

/// The last valid pixel index on a `size`-long axis (`size − 1`), or `0`
/// for a degenerate zero-length axis. Never panics.
fn last_index(size: u32) -> i32 {
    i32::try_from(size.saturating_sub(1)).unwrap_or(i32::MAX)
}

/// Is `y` within `monitor`'s vertical span (inclusive)? Guards against a
/// cursor sharing the edge column with a taller neighbour being mapped
/// against the wrong monitor's height.
fn within_vertical_span(y: i32, monitor: MonitorRect) -> bool {
    y >= monitor.top && y <= monitor.top + last_index(monitor.height)
}

/// The two-machine left–right topology (ADR 0009): one linked edge pair,
/// this machine being the left or the right member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Topology {
    side: LinkSide,
}

impl Topology {
    /// A topology for a machine on `side` of the pair.
    #[must_use]
    pub fn new(side: LinkSide) -> Self {
        Self { side }
    }

    /// Which member of the pair this machine is.
    #[must_use]
    pub fn side(self) -> LinkSide {
        self.side
    }

    /// The edge that links to the peer.
    #[must_use]
    pub fn linked_edge(self) -> Edge {
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
        match self.linked_edge() {
            Edge::Right => monitors
                .iter()
                .copied()
                .max_by_key(|m| m.left + last_index(m.width)),
            Edge::Left => monitors.iter().copied().min_by_key(|m| m.left),
        }
    }

    /// If `cursor` is against the linked edge of the edge monitor, the
    /// normalized crossing position to hand the peer; otherwise `None`
    /// (the cursor is not leaving). The position is a fraction of that
    /// monitor's height, so the peer places its cursor through its own
    /// geometry.
    #[must_use]
    pub fn leaving(self, cursor: CursorPoint, monitors: &[MonitorRect]) -> Option<EdgeFraction> {
        let monitor = self.edge_monitor(monitors)?;
        if self.linked_edge().touched_by(cursor.x, monitor)
            && within_vertical_span(cursor.y, monitor)
        {
            Some(EdgeFraction::from_pixel(
                cursor.y - monitor.top,
                monitor.height,
            ))
        } else {
            None
        }
    }

    /// Where the cursor should appear when control arrives here for a peer
    /// that crossed at `fraction`: on this machine's edge monitor, at that
    /// fraction of the monitor's height. The inverse direction of
    /// [`leaving`](Self::leaving), and the same edge in reverse.
    #[must_use]
    pub fn entering(self, fraction: EdgeFraction, monitors: &[MonitorRect]) -> CursorPoint {
        let Some(monitor) = self.edge_monitor(monitors) else {
            return CursorPoint { x: 0, y: 0 };
        };
        CursorPoint {
            x: self.linked_edge().outer_x(monitor),
            y: monitor.top + fraction.to_pixel(monitor.height),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{CursorPoint, Edge, EdgeFraction, LinkSide, MonitorRect, Topology};

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
