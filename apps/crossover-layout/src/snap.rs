//! Snapping a dragged group against the monitors that are standing still
//! (ADR 0018), and the guides that show why it moved where it did.
//!
//! # Why this module exists at all
//!
//! ADR 0018 makes abutment **exact, with zero tolerance**: two rectangles
//! are adjacent only where an edge coordinate is *identical*. A one-unit
//! gap is observably not an edge, and the derivation
//! (`crates/crossover-core/src/crossing.rs`) refuses to guess. The ADR
//! puts the other half of that bargain here — "**snapping is the editor's
//! job**, where the user can see it happen" — so this module is what turns
//! a gap into a seam. Nothing downstream ever rounds; a crossing exists
//! because a drag ended on an exact coordinate, and the guide the user saw
//! is the reason.
//!
//! # What it snaps, and what it does not
//!
//! The moving side is a whole machine's **rigid group**: a machine's own
//! monitors sit where the OS says they sit (ADR 0018, "intra-machine
//! geometry stays the OS's"), so a drag translates every rectangle in the
//! group by one delta and never reshapes it. Every rectangle of the group
//! is offered as a snap source, not merely the group's bounding box — a
//! two-monitor machine dragged so its *second* screen meets the peer's is
//! the ordinary case, and a bounding-box-only rule could not express it.
//!
//! # The candidates
//!
//! Per axis, for every (moving, stationary) pair:
//!
//! - **Abutment** — the dragged edge lands exactly on a stationary edge
//!   facing it (`right → left`, `left → right`; `bottom → top`,
//!   `top → bottom`). This is the candidate that creates a crossing.
//! - **Alignment** — near-to-near and far-to-far (`left → left`,
//!   `right → right`; `top → top`, `bottom → bottom`). It creates no
//!   crossing on its own; it is what makes two stacked screens line up
//!   instead of sitting one unit proud.
//! - **Edge midpoint** — the two rectangles' centres on that axis. Two
//!   centres can be a half unit apart (an odd sum), and a layout
//!   coordinate is an integer, so both bracketing integer deltas are
//!   offered and the ordinary "closest to what the pointer asked for" rule
//!   picks between them. That is what keeps [`snap`] idempotent: re-
//!   snapping an already-snapped arrangement offers delta 0, which is
//!   nearest by definition.
//!
//! # The threshold is screen-space
//!
//! [`threshold_for`] divides [`SNAP_SCREEN_PX`] by the viewport's scale, so
//! the snap radius is a constant number of *pixels under the pointer*
//! whatever the arrangement's zoom. A layout-space constant would snap
//! from half a screen away on a zoomed-out desk and be unusable on a
//! zoomed-in one.
//!
//! # Per axis, independently
//!
//! X and Y are resolved separately and neither consults the other's
//! result. In particular a candidate is **not** filtered by whether the
//! perpendicular extents overlap: the perpendicular axis has its own snap
//! running in the same drag, and making one axis's answer depend on the
//! other's would make the pair order-dependent — the same drag reaching
//! two different arrangements depending on which axis was resolved first.
//! Whether an abutment is a *crossing* is then decided, exactly and after
//! the fact, by the model's own adjacency check and by the derivation.

use crossover_topology::LayoutRect;

/// The snap radius, in screen pixels. Roughly a pointer's width: close
/// enough that a deliberate near-miss survives, far enough that a user
/// aiming at a seam gets one without pixel-hunting.
pub const SNAP_SCREEN_PX: f32 = 12.0;

/// At most this many guides are reported for one drag. A drag that
/// satisfies a dozen candidates at once has nothing more to say than the
/// first few of them, and this is one more bounded thing rather than one
/// less (NFR-1's discipline, applied to a list that feeds a painter).
const MAX_GUIDES: usize = 8;

/// The largest delta this module will ever return, as an `f64` bound for
/// the one narrowing conversion it makes. Twice the layout coordinate
/// ceiling: a rectangle at one extreme cannot be asked to move further
/// than the other extreme.
const DELTA_LIMIT: f64 = 33_554_432.0;

/// The restatement of the bound `DELTA_LIMIT` was computed from, so a later
/// edit to the model's ceiling cannot quietly invalidate it.
const _: () = assert!(crossover_topology::MAX_LAYOUT_COORDINATE == 1 << 24);

/// Which axis a snap constrains. [`Axis::X`] is a horizontal displacement
/// and therefore a *vertical* guide line, and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Left/right.
    X,
    /// Up/down.
    Y,
}

/// Why a snap fired — the ranking between equally close candidates, and
/// what a guide's colour and tooltip can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapKind {
    /// Edges meeting: the candidate that makes a crossing.
    Abut,
    /// Same-side edges lining up.
    Align,
    /// Centres lining up.
    Center,
}

impl SnapKind {
    /// The tie-break order between candidates the pointer is equally far
    /// from: a seam beats a line-up, which beats a centring. An abutment
    /// changes what the arrangement *does*; the other two only change how
    /// it looks, so when the user is exactly between them, the one with
    /// consequences wins.
    const fn rank(self) -> u8 {
        match self {
            Self::Abut => 0,
            Self::Align => 1,
            Self::Center => 2,
        }
    }

    /// A short phrase for a status line or a test assertion.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Abut => "edges meet",
            Self::Align => "edges line up",
            Self::Center => "centres line up",
        }
    }
}

/// One line to draw: the coordinate two rectangles now agree on, and how
/// far along the perpendicular axis to draw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    /// The axis this guide constrains — [`Axis::X`] draws vertically.
    pub axis: Axis,
    /// Why it fired.
    pub kind: SnapKind,
    /// The shared coordinate, in layout space.
    pub position: f64,
    /// The perpendicular extent to draw across: the union of the two
    /// rectangles that agreed, so the line visibly touches both.
    pub span: (f64, f64),
}

/// What a drag resolved to: the translation to apply to the moving group,
/// and the guides that explain it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapped {
    /// The translation, in whole layout units — integers, because a
    /// [`LayoutRect`] is integral and an abutment that rounded afterwards
    /// would not be exact.
    pub delta: (i64, i64),
    /// Every candidate the chosen delta satisfies, at most [`MAX_GUIDES`].
    pub guides: Vec<Guide>,
}

/// The snap radius in layout units for a viewport drawing at `scale`
/// screen pixels per layout unit — see the module doc.
///
/// A non-finite or non-positive scale (which [`crate::viewport::Viewport::fit`]
/// never produces, but which is not assumed away) yields `0.0`: no snap,
/// rather than a threshold of infinity that would drag every rectangle to
/// its nearest neighbour.
#[must_use]
pub fn threshold_for(scale: f32) -> f64 {
    if !scale.is_finite() || scale <= 0.0 {
        return 0.0;
    }
    f64::from(SNAP_SCREEN_PX) / f64::from(scale)
}

/// Snap `raw` — the translation the pointer asked for — against
/// `stationary`, for a rigid group whose rectangles are `moving` *as they
/// were when the drag began*.
///
/// Both axes are resolved independently (module doc). When no candidate on
/// an axis is within `threshold`, that axis takes the pointer's own
/// request, rounded to a whole unit.
#[must_use]
pub fn snap(
    moving: &[LayoutRect],
    stationary: &[LayoutRect],
    raw: (f64, f64),
    threshold: f64,
) -> Snapped {
    let x = resolve(Axis::X, moving, stationary, raw.0, threshold);
    let y = resolve(Axis::Y, moving, stationary, raw.1, threshold);
    let delta = (x.delta, y.delta);

    let mut guides = Vec::new();
    collect_guides(&mut guides, &x, delta);
    collect_guides(&mut guides, &y, delta);
    Snapped { delta, guides }
}

/// One axis's answer: the delta, and the candidates that delta satisfies.
struct Resolution {
    axis: Axis,
    delta: i64,
    winners: Vec<Candidate>,
}

/// One way the moving group could sit on this axis.
struct Candidate {
    /// The whole-unit translation that achieves it.
    delta: i64,
    kind: SnapKind,
    /// The shared coordinate the two edges (or centres) then agree on.
    position: f64,
    /// The moving rectangle's extent on the **other** axis, and the
    /// stationary one's — what a guide is drawn across.
    ///
    /// Carried on the candidate rather than as indices back into the two
    /// slices: the pair is known exactly where the candidate is made, and
    /// an index would have to be re-resolved later against slices this type
    /// does not own, with an unreachable "what if it isn't there" branch to
    /// write and never exercise.
    moving_cross: (i64, i64),
    stationary_cross: (i64, i64),
}

fn resolve(
    axis: Axis,
    moving: &[LayoutRect],
    stationary: &[LayoutRect],
    raw: f64,
    threshold: f64,
) -> Resolution {
    let mut candidates: Vec<Candidate> = Vec::new();
    for mover in moving {
        let (m_near, m_far) = extent(axis, *mover);
        let moving_cross = cross_extent(axis, *mover);
        for still in stationary {
            let (s_near, s_far) = extent(axis, *still);
            let stationary_cross = cross_extent(axis, *still);
            let mut offer = |delta: i64, kind: SnapKind, position: f64| {
                if within(delta, raw, threshold) {
                    candidates.push(Candidate {
                        delta,
                        kind,
                        position,
                        moving_cross,
                        stationary_cross,
                    });
                }
            };

            // Abutment: the moving group's far edge onto the stationary
            // near edge, and the reverse. Exact by construction — every
            // term is an integer coordinate.
            offer(s_near - m_far, SnapKind::Abut, whole(s_near));
            offer(s_far - m_near, SnapKind::Abut, whole(s_far));
            // Alignment: same-side edges.
            offer(s_near - m_near, SnapKind::Align, whole(s_near));
            offer(s_far - m_far, SnapKind::Align, whole(s_far));
            // Midpoint: both bracketing integer deltas when the centres
            // are a half unit apart (module doc).
            let doubled = (s_near + s_far) - (m_near + m_far);
            let center = midpoint(s_near, s_far);
            offer(doubled.div_euclid(2), SnapKind::Center, center);
            if doubled.rem_euclid(2) != 0 {
                offer(doubled.div_euclid(2) + 1, SnapKind::Center, center);
            }
        }
    }

    let Some(best) = candidates.iter().min_by(|a, b| order(a, b, raw)) else {
        return Resolution {
            axis,
            delta: to_delta(raw.round()),
            winners: Vec::new(),
        };
    };
    let delta = best.delta;
    let winners = candidates
        .into_iter()
        .filter(|candidate| candidate.delta == delta)
        .collect();
    Resolution {
        axis,
        delta,
        winners,
    }
}

/// Closest to what the pointer asked for; then [`SnapKind::rank`]; then the
/// smaller delta, so the answer is a total order and the same drag always
/// resolves the same way.
fn order(a: &Candidate, b: &Candidate, raw: f64) -> std::cmp::Ordering {
    distance(a.delta, raw)
        .total_cmp(&distance(b.delta, raw))
        .then(a.kind.rank().cmp(&b.kind.rank()))
        .then(a.delta.cmp(&b.delta))
}

fn collect_guides(guides: &mut Vec<Guide>, resolution: &Resolution, delta: (i64, i64)) {
    // The guide is drawn against where the group *ends up*, so the
    // perpendicular displacement — the other axis's own answer — applies.
    let perpendicular = match resolution.axis {
        Axis::X => delta.1,
        Axis::Y => delta.0,
    };
    for candidate in &resolution.winners {
        let (m_lo, m_hi) = candidate.moving_cross;
        let (s_lo, s_hi) = candidate.stationary_cross;
        let span = (
            whole((m_lo + perpendicular).min(s_lo)),
            whole((m_hi + perpendicular).max(s_hi)),
        );
        // Several rectangle pairs routinely agree on one line; drawing it
        // once is the whole point of a guide. But the second pair is not
        // *nothing* — it is more of the same line — so its span is merged
        // into the one already there rather than dropped, which is what
        // makes the guide reach every rectangle it explains instead of
        // stopping at whichever pair happened to be offered first.
        //
        // Bit-for-bit equality is the right test here and not the usual
        // float-comparison mistake: every position is either an integer
        // coordinate or an exact half, computed by the same expression from
        // the same integers, so two that mean the same line *are* the same
        // `f64`.
        #[allow(clippy::float_cmp)]
        let existing = guides
            .iter_mut()
            .find(|guide| guide.axis == resolution.axis && guide.position == candidate.position);
        if let Some(guide) = existing {
            guide.span.0 = guide.span.0.min(span.0);
            guide.span.1 = guide.span.1.max(span.1);
            continue;
        }
        // Checked here rather than at the top of the loop, so reaching the
        // cap stops *new* lines being added without also stopping the
        // merges above from completing the ones already drawn.
        if guides.len() >= MAX_GUIDES {
            return;
        }
        guides.push(Guide {
            axis: resolution.axis,
            kind: candidate.kind,
            position: candidate.position,
            span,
        });
    }
}

/// The rectangle's near and far coordinate on `axis`.
fn extent(axis: Axis, rect: LayoutRect) -> (i64, i64) {
    match axis {
        Axis::X => (rect.left(), rect.right()),
        Axis::Y => (rect.top(), rect.bottom()),
    }
}

/// The rectangle's extent on the *other* axis — what a guide is drawn
/// across.
fn cross_extent(axis: Axis, rect: LayoutRect) -> (i64, i64) {
    match axis {
        Axis::X => (rect.top(), rect.bottom()),
        Axis::Y => (rect.left(), rect.right()),
    }
}

fn within(delta: i64, raw: f64, threshold: f64) -> bool {
    distance(delta, raw) <= threshold
}

fn distance(delta: i64, raw: f64) -> f64 {
    (whole(delta) - raw).abs()
}

/// A layout coordinate as an `f64`. Every value this module sees is a
/// coordinate or a difference of two, bounded by ±2·2^24 — far short of
/// where `i64 -> f64` could lose a bit.
#[allow(clippy::cast_precision_loss)]
fn whole(value: i64) -> f64 {
    value as f64
}

/// The midpoint of two integer coordinates, which is a half-integer when
/// their sum is odd — exact in `f64` at these magnitudes.
fn midpoint(near: i64, far: i64) -> f64 {
    whole(near + far) / 2.0
}

/// The pointer's own request as a whole-unit delta. Total by construction:
/// a NaN (which no viewport `fit` can produce, but which is not assumed
/// away) is no movement at all, and anything past the coordinate ceiling
/// is clamped rather than wrapped.
fn to_delta(value: f64) -> i64 {
    if value.is_nan() {
        return 0;
    }
    let clamped = value.clamp(-DELTA_LIMIT, DELTA_LIMIT);
    // The clamp puts this inside ±2^25, which `i64` represents exactly.
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = clamped as i64;
    narrowed
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{Axis, Guide, SNAP_SCREEN_PX, SnapKind, snap, threshold_for, whole};
    use crossover_topology::LayoutRect;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> LayoutRect {
        LayoutRect {
            x,
            y,
            width,
            height,
        }
    }

    /// Move `rects` by `delta`, the way a drag commits.
    fn moved(rects: &[LayoutRect], delta: (i64, i64)) -> Vec<LayoutRect> {
        rects
            .iter()
            .map(|rect| LayoutRect {
                x: i32::try_from(i64::from(rect.x) + delta.0).unwrap(),
                y: i32::try_from(i64::from(rect.y) + delta.1).unwrap(),
                ..*rect
            })
            .collect()
    }

    #[test]
    fn the_threshold_is_a_constant_number_of_screen_pixels() {
        // Twice the zoom, half the layout-space radius: the pointer's feel
        // does not change with the arrangement's scale.
        assert!((threshold_for(1.0) - f64::from(SNAP_SCREEN_PX)).abs() < 1e-9);
        assert!((threshold_for(2.0) - f64::from(SNAP_SCREEN_PX) / 2.0).abs() < 1e-9);
        // Degenerate scales snap nothing rather than everything.
        assert!((threshold_for(0.0) - 0.0).abs() < 1e-9);
        assert!((threshold_for(f32::NAN) - 0.0).abs() < 1e-9);
    }

    /// The property the whole module exists for: a gap inside the
    /// threshold becomes a seam, exactly (ADR 0018's zero tolerance).
    #[test]
    fn a_near_miss_becomes_an_exact_abutment() {
        let moving = [rect(0, 0, 100, 100)];
        let stationary = [rect(105, 0, 100, 100)];
        // The pointer asks for +2; the peer's left edge is 5 away.
        let snapped = snap(&moving, &stationary, (2.0, 0.0), 10.0);
        assert_eq!(snapped.delta, (5, 0));
        let placed = moved(&moving, snapped.delta);
        assert_eq!(placed[0].right(), stationary[0].left());
        assert!(
            snapped
                .guides
                .iter()
                .any(|guide| guide.kind == SnapKind::Abut && guide.axis == Axis::X),
            "{:?}",
            snapped.guides
        );
    }

    #[test]
    fn a_gap_outside_the_threshold_is_left_exactly_where_the_pointer_asked() {
        let moving = [rect(0, 0, 100, 100)];
        let stationary = [rect(200, 500, 100, 100)];
        let snapped = snap(&moving, &stationary, (7.0, -3.0), 10.0);
        assert_eq!(snapped.delta, (7, -3));
        assert!(snapped.guides.is_empty(), "{:?}", snapped.guides);
    }

    /// The boundary is inclusive, and one unit past it is not.
    #[test]
    fn the_threshold_boundary_is_inclusive() {
        let moving = [rect(0, 0, 100, 100)];
        let stationary = [rect(110, 0, 100, 100)];
        // Exactly 10 away from the abutment at delta 10.
        assert_eq!(snap(&moving, &stationary, (0.0, 0.0), 10.0).delta.0, 10);
        assert_eq!(snap(&moving, &stationary, (0.0, 0.0), 9.99).delta.0, 0);
    }

    /// Two candidates the pointer is equally far from: the abutment wins,
    /// because it is the one that changes what the arrangement does.
    #[test]
    fn an_abutment_outranks_an_alignment_at_equal_distance() {
        // Stationary at x = 100..200. From x = 0..100 the abutment
        // (right → left) is delta +100; the alignment (left → left) is
        // also +100. Offset the mover so the two differ and sit either
        // side of the request.
        let moving = [rect(0, 0, 100, 100)];
        let stationary = [rect(104, 0, 100, 100)];
        // Abut (right → left) is +4; align (left → left) is +104.
        // Align (right→right) is +104 too. Ask for +2: abut is 2 away.
        let snapped = snap(&moving, &stationary, (2.0, 0.0), 6.0);
        assert_eq!(snapped.delta, (4, 0));

        // Now a genuine tie: a stationary rectangle whose left edge is 5
        // beyond the mover's right edge *and* whose own left edge is 5
        // before the mover's left edge is impossible, so tie on a stack:
        // two stationary rectangles, one offering abut at +5 and one
        // offering align at -5, with the request in the middle.
        let stationary = [rect(105, 0, 10, 100), rect(-5, 200, 10, 100)];
        let snapped = snap(&moving, &stationary, (0.0, 0.0), 6.0);
        assert_eq!(snapped.delta.0, 5, "{snapped:?}");
        assert!(
            snapped
                .guides
                .iter()
                .any(|guide| guide.kind == SnapKind::Abut)
        );
    }

    /// Every rectangle of a rigid group is a snap source, not just the
    /// bounding box: the *second* monitor meets the peer here, and the
    /// group moves as one.
    #[test]
    fn any_monitor_of_the_group_can_be_the_one_that_snaps() {
        let moving = [rect(0, 0, 100, 100), rect(100, 0, 100, 100)];
        let stationary = [rect(206, 0, 100, 100)];
        let snapped = snap(&moving, &stationary, (3.0, 0.0), 10.0);
        assert_eq!(snapped.delta, (6, 0));
        let placed = moved(&moving, snapped.delta);
        assert_eq!(placed[1].right(), stationary[0].left());
        // And the group is still rigid: the internal seam is untouched.
        assert_eq!(placed[0].right(), placed[1].left());
    }

    /// Alignment on one axis and abutment on the other, from one drag —
    /// the axes never consult each other.
    #[test]
    fn the_two_axes_resolve_independently() {
        let moving = [rect(0, 0, 100, 100)];
        let stationary = [rect(104, 3, 100, 100)];
        let snapped = snap(&moving, &stationary, (1.0, 1.0), 8.0);
        assert_eq!(snapped.delta, (4, 3), "{snapped:?}");
        let placed = moved(&moving, snapped.delta);
        assert_eq!(placed[0].right(), stationary[0].left());
        assert_eq!(placed[0].top(), stationary[0].top());
    }

    /// Centres a half unit apart still land on a whole coordinate, and the
    /// one nearer the pointer's request is chosen. Sized so the two
    /// rectangles' edges are nowhere near each other: this isolates the
    /// midpoint candidate from the alignment ones.
    #[test]
    fn a_half_unit_midpoint_takes_the_nearer_whole_delta() {
        // Mover's y is 0..100 (centre 50); the stationary rectangle's is
        // 30..171 (centre 100.5), so the centres meet at delta 50.5.
        let moving = [rect(0, 0, 100, 100)];
        let stationary = [rect(400, 30, 100, 141)];
        let up = snap(&moving, &stationary, (0.0, 50.8), 4.0);
        assert_eq!(up.delta.1, 51, "{up:?}");
        let down = snap(&moving, &stationary, (0.0, 50.2), 4.0);
        assert_eq!(down.delta.1, 50, "{down:?}");
    }

    #[test]
    fn no_stationary_rectangles_means_no_snap_and_no_guides() {
        let moving = [rect(0, 0, 100, 100)];
        let snapped = snap(&moving, &[], (12.4, -7.5), 10.0);
        assert_eq!(snapped.delta, (12, -8));
        assert!(snapped.guides.is_empty());
    }

    /// A guide is drawn across both rectangles that agreed, so the line
    /// visibly touches each of them.
    #[test]
    fn a_guide_spans_both_rectangles_it_explains() {
        let moving = [rect(0, 0, 100, 100)];
        let stationary = [rect(105, 300, 100, 100)];
        let snapped = snap(&moving, &stationary, (5.0, 0.0), 10.0);
        let vertical: Vec<&Guide> = snapped
            .guides
            .iter()
            .filter(|guide| guide.axis == Axis::X)
            .collect();
        assert!(!vertical.is_empty(), "{snapped:?}");
        let guide = vertical[0];
        assert!((guide.position - 105.0).abs() < 1e-9, "{guide:?}");
        assert!(guide.span.0 <= 0.0 && guide.span.1 >= 400.0, "{guide:?}");
    }

    proptest! {
        /// A snap never moves the group further than the threshold from
        /// what the pointer asked for — plus the half unit that rounding
        /// to an integer coordinate costs. A snap the user cannot see is
        /// the thing this bound rules out.
        #[test]
        fn a_snap_never_moves_further_than_the_threshold(
            raw_x in -500.0f64..500.0, raw_y in -500.0f64..500.0,
            sx in -400i32..400, sy in -400i32..400,
            threshold in 0.0f64..40.0,
        ) {
            let moving = [rect(0, 0, 100, 100), rect(100, 20, 60, 60)];
            let stationary = [rect(sx, sy, 120, 90), rect(sx + 200, sy - 50, 80, 80)];
            let snapped = snap(&moving, &stationary, (raw_x, raw_y), threshold);
            let slack = threshold + 0.5;
            prop_assert!(
                (whole(snapped.delta.0) - raw_x).abs() <= slack,
                "{snapped:?} from {raw_x}"
            );
            prop_assert!(
                (whole(snapped.delta.1) - raw_y).abs() <= slack,
                "{snapped:?} from {raw_y}"
            );
        }

        /// Idempotence: an axis that actually snapped asks for no further
        /// movement when it is snapped again. Without it a drag could
        /// creep a unit per frame while the pointer stood still.
        ///
        /// Stated per axis, and conditioned on that axis having fired,
        /// because the unsnapped fallback is honestly not idempotent: it
        /// rounds the pointer's request to a whole unit, which can carry
        /// the group up to half a unit *toward* a candidate that was
        /// fractionally out of reach before. That half unit is the
        /// rounding, not the snap, and it happens once.
        #[test]
        fn an_axis_that_snapped_does_not_snap_again(
            raw_x in -300.0f64..300.0, raw_y in -300.0f64..300.0,
            sx in -300i32..300, sy in -300i32..300,
            threshold in 0.0f64..30.0,
        ) {
            let moving = [rect(0, 0, 100, 100), rect(100, 0, 61, 55)];
            let stationary = [rect(sx, sy, 120, 90), rect(sx - 130, sy + 40, 90, 70)];
            let first = snap(&moving, &stationary, (raw_x, raw_y), threshold);
            let settled = moved(&moving, first.delta);
            let again = snap(&settled, &stationary, (0.0, 0.0), threshold);
            let fired = |axis: Axis| first.guides.iter().any(|guide| guide.axis == axis);
            if fired(Axis::X) {
                prop_assert_eq!(again.delta.0, 0, "{:?} then {:?}", first, again);
            }
            if fired(Axis::Y) {
                prop_assert_eq!(again.delta.1, 0, "{:?} then {:?}", first, again);
            }
        }

        /// Whenever any candidate was in reach, the result *is* a
        /// candidate — exactly, not nearly. This is the property ADR
        /// 0018's zero-tolerance derivation depends on: a gap the user
        /// closed is a seam the detector will find.
        #[test]
        fn a_reachable_candidate_is_achieved_exactly(
            raw_x in -200.0f64..200.0,
            sx in -300i32..300, sy in -60i32..60,
            threshold in 1.0f64..30.0,
        ) {
            let moving = [rect(0, 0, 100, 100)];
            let stationary = [rect(sx, sy, 120, 90)];
            let snapped = snap(&moving, &stationary, (raw_x, 0.0), threshold);
            let placed = moved(&moving, snapped.delta);
            let reachable = |target: i64| (whole(target) - raw_x).abs() <= threshold;
            // The three integer deltas that would make the mover's edges
            // meet or line up with the stationary rectangle's, measured
            // from where the drag started.
            let any_reachable = [
                stationary[0].left() - moving[0].right(),
                stationary[0].right() - moving[0].left(),
                stationary[0].left() - moving[0].left(),
            ]
            .into_iter()
            .any(reachable);
            if any_reachable {
                let exact = placed[0].right() == stationary[0].left()
                    || placed[0].left() == stationary[0].right()
                    || placed[0].left() == stationary[0].left()
                    || placed[0].right() == stationary[0].right()
                    || (placed[0].left() + placed[0].right()
                        - stationary[0].left()
                        - stationary[0].right())
                        .abs()
                        <= 1;
                prop_assert!(exact, "{snapped:?} left {placed:?} against {stationary:?}");
            }
        }
    }

    /// Two moving rectangles that both meet the same stationary edge draw
    /// **one** guide, spanning both of them — the second pair widens the
    /// line rather than being dropped for agreeing with the first.
    #[test]
    fn a_second_pair_on_the_same_line_widens_the_guide_it_shares() {
        // Two movers stacked at y = 0..100 and y = 500..600, both with
        // their right edge at x = 100; one stationary rectangle spanning
        // both, its left edge 5 further right.
        let moving = [rect(0, 0, 100, 100), rect(0, 500, 100, 100)];
        let stationary = [rect(105, 0, 100, 600)];
        let snapped = snap(&moving, &stationary, (5.0, 0.0), 10.0);
        assert_eq!(snapped.delta, (5, 0));

        let vertical: Vec<&Guide> = snapped
            .guides
            .iter()
            .filter(|guide| guide.axis == Axis::X)
            .collect();
        assert_eq!(vertical.len(), 1, "one line, not two: {snapped:?}");
        let guide = vertical[0];
        assert!((guide.position - 105.0).abs() < 1e-9, "{guide:?}");
        assert!(
            guide.span.0 <= 0.0 && guide.span.1 >= 600.0,
            "the merged span must reach both movers: {guide:?}"
        );
    }
}
