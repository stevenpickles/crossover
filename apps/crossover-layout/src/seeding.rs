//! How big a monitor is **drawn** when the editor has to seed it — pure,
//! and separate from the scene that places the rectangles (ADR 0018,
//! amended 2026-08-22).
//!
//! # Why a size rule exists at all
//!
//! Crossing is proportional through *drawn* geometry: a cursor leaving a
//! monitor 40 % of the way up its edge arrives 40 % of the way up whatever
//! is drawn across that seam. So the proportions in the drawing **are** the
//! crossing mapping, and a seed that gets them wrong is a seed the user has
//! to correct by hand before the cursor lands where they expect.
//!
//! Seeding in DIPs — a monitor's pixel size divided by its own scale
//! factor, which is what this editor did before panels could measure
//! themselves — is right for *legibility* and wrong for *proportion*: a 13"
//! laptop at 1920×1200/200 % and a 27" desktop panel at 2560×1600/100 %
//! both seed 960×600 and 2560×1600 respectively, which says nothing about
//! how tall either screen actually is. Two screens of the same DIP size draw
//! identically whether one is a third the height of the other.
//!
//! # The rule
//!
//! For one machine, given each monitor's live pixel rectangle, its
//! `scale_percent`, and its optional [`PhysicalSizeMm`]:
//!
//! - **A monitor that measured itself believably draws at its millimetres
//!   times [`UNITS_PER_MM`]** — layout units are abstract (ADR 0018: "the
//!   model neither knows nor needs to know what a unit is worth"), so the
//!   constant only has to be *consistent*, and is chosen to keep drawn
//!   magnitudes in the same ballpark as the DIP seeding it replaces.
//!   *Believably* is [`believable`]'s business: a size the wire admits but
//!   no panel could have is treated as no size at all, because a
//!   proportion drawn from a fiction misdraws the whole desk.
//! - **A monitor that did not draws at its DIP size times the machine's
//!   median millimetres-per-DIP**, so it stays in proportion with its
//!   measured siblings instead of being drawn at a magnitude from a
//!   different scale entirely. It is marked [`SeededSize::estimated`], which
//!   is what the editor badges.
//! - **A machine with nothing measured at all** — no EDID anywhere, a desk
//!   of virtual or remote displays — has no ratio to borrow from itself, so
//!   it borrows the *other* machine's ([`MachineScale::of`]'s `fallback`).
//!   Failing that the whole *scene* is unmeasured: every rectangle seeds in
//!   DIPs, exactly as it did before sizes existed, and nothing is badged
//!   because nothing is any less certain than anything beside it. That is a
//!   deliberate literal third arm rather than the second arm with a ratio of
//!   1.0, so "nothing measured anywhere draws what it always drew" is a
//!   property of the code's shape rather than of an argument about rounding.
//!
//! # Sizes do not flicker
//!
//! A monitor's size can arrive later than the monitor — the worker's
//! ~1 s sweep reads geometry and EDID separately — but it does not come and
//! go: the worker keeps a per-monitor record of what it has learned and
//! fills the two descriptive fields independently (ADR 0018's retention
//! paragraph), so a sweep that fails to read one panel's EDID reports the
//! last size it did read rather than none. This module therefore needs no
//! state of its own to keep a rectangle from oscillating between arms, and
//! [`crate::model`]'s transplant covers the one transition that remains: a
//! size learned for the first time while the user is mid-edit.
//!
//! # Rotation
//!
//! EDID measures the *panel*, in the panel's own orientation; the OS reports
//! pixels in the orientation the user rotated it to. A portrait 2160×3840
//! screen therefore reports 597×336 mm, and drawing that literally would
//! seed a landscape rectangle for a portrait screen — a worse picture than
//! the DIP seeding it replaced. [`oriented`] matches the millimetre axes to
//! the pixel rectangle's orientation, which is a proportion decision and so
//! belongs here rather than in the platform backend that reads the EDID.
//!
//! # What this module is not
//!
//! It decides **sizes**, not positions. Where a seeded rectangle *goes* is
//! [`crate::model`]'s packing, which abuts a machine's monitors left to
//! right in their live order: abutment and non-overlap are properties of
//! that construction, and they survive this module changing the widths
//! precisely because the packing derives each x from the widths it is given.

use crossover_topology::{
    LiveMonitor, MAX_MONITOR_EXTENT, PhysicalSizeMm, is_plausible_physical_size,
};

/// Layout units per millimetre — one unit is a quarter of a millimetre.
///
/// Layout coordinates are abstract (ADR 0018), so this constant buys
/// nothing but consistency and magnitude, and magnitude is the whole
/// argument for the value: an ordinary 27" panel is 597 mm wide and seeds
/// 2388 units, sitting in the same range as the 1920–2560 its DIP seeding
/// produced, so an arrangement drawn before this rule and one drawn after
/// zoom and snap identically. It is also small enough that the largest size
/// this module will draw from (`MAX_PLAUSIBLE_PHYSICAL_MM`, 3000 mm) seeds
/// 12 000 units — and even the largest the *wire* admits (10 000 mm, which
/// [`believable`] refuses long before it gets here) would seed 40 000,
/// inside [`MAX_MONITOR_EXTENT`]. So no measurement, believable or not, can
/// reach the clamp in [`bound`].
pub const UNITS_PER_MM: u32 = 4;

/// A drawn size, and whether it is a measurement or an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeededSize {
    /// Drawn width, in layout units. Always `1..=MAX_MONITOR_EXTENT`.
    pub width: u32,
    /// Drawn height, in layout units. Always `1..=MAX_MONITOR_EXTENT`.
    pub height: u32,
    /// `true` when this size came from pixels rather than from the panel's
    /// own millimetres, **and some other screen in the scene did measure
    /// itself** — the editor's cue to badge the rectangle.
    ///
    /// The second half is what makes the badge worth painting. It marks a
    /// *difference* in fidelity between rectangles the user is looking at:
    /// this one's proportions are inferred, that one's were measured. Where
    /// nothing anywhere measured itself there is no difference to mark —
    /// every rectangle is seeded from pixels, exactly as every rectangle
    /// always was — and badging all of them would put a third caption line
    /// on every screen on the desk to say nothing at all.
    pub estimated: bool,
}

/// One machine's rule for turning a live monitor into a drawn size.
///
/// Per *machine*, because the ratio it carries is a fact about a desk: the
/// screens on one machine are measured by one platform backend, and a
/// monitor that could not be measured is far more likely to resemble its
/// own siblings than the other desk's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MachineScale {
    /// Layout units per DIP for the monitors that did **not** measure
    /// themselves. `None` when neither this machine nor the other measured
    /// anything, which is the module doc's literal third arm: DIPs, exactly
    /// as before.
    units_per_dip: Option<f64>,
}

impl MachineScale {
    /// The rule for the machine whose live monitors are `monitors`.
    ///
    /// `fallback` is the *other* machine's [`median_mm_per_dip`], used only
    /// when this machine measured nothing at all. A pair where one desk
    /// reports EDIDs and the other does not is otherwise drawn at two
    /// different scales — the measured group in quarter-millimetres and the
    /// unmeasured one in DIPs — which is exactly the cross-machine
    /// disproportion this branch exists to remove.
    #[must_use]
    pub fn of(monitors: &[LiveMonitor], fallback: Option<f64>) -> Self {
        let ratio = median_mm_per_dip(monitors).or(fallback);
        Self {
            units_per_dip: ratio.map(|mm_per_dip| mm_per_dip * f64::from(UNITS_PER_MM)),
        }
    }

    /// The scale a machine with nothing measured on either desk uses: DIPs,
    /// unchanged from before physical sizes existed.
    ///
    /// Test-only, because production never *asks* for it: it is what
    /// [`MachineScale::of`] arrives at on its own when neither machine
    /// measured anything. Naming it lets a test say which of the rule's
    /// three arms it is exercising.
    #[cfg(test)]
    #[must_use]
    pub const fn dips() -> Self {
        Self {
            units_per_dip: None,
        }
    }

    /// How big `monitor` is drawn under this rule.
    #[must_use]
    pub fn size_of(self, monitor: &LiveMonitor) -> SeededSize {
        let pixels = (monitor.rect.width, monitor.rect.height);
        let dips = dip_pair(monitor);
        match (believable(monitor), self.units_per_dip) {
            // Measured: the panel's own millimetres, in the orientation the
            // OS is presenting it. Integer arithmetic throughout — a
            // believable size is at most 3000 mm, so the product is at most
            // 12 000 and nothing here can overflow or round.
            (Some(size), _) => {
                let (width_mm, height_mm) = oriented(size, pixels);
                SeededSize {
                    width: bound(width_mm * UNITS_PER_MM),
                    height: bound(height_mm * UNITS_PER_MM),
                    estimated: false,
                }
            }
            // Unmeasured, on a machine (or a pair) that measured something:
            // DIPs carried onto the measured monitors' scale, so the
            // rectangle is at least the right size relative to its
            // neighbours even though nothing measured it. Badged, because
            // there are measured rectangles beside it to be less certain
            // than.
            (None, Some(units_per_dip)) => SeededSize {
                width: to_extent(f64::from(dips.0) * units_per_dip),
                height: to_extent(f64::from(dips.1) * units_per_dip),
                estimated: true,
            },
            // Nothing believable measured anywhere: the pre-sizes rule, and
            // no badge, because every rectangle in the scene is in exactly
            // this position and a mark that never varies marks nothing.
            (None, None) => SeededSize {
                width: bound(dips.0),
                height: bound(dips.1),
                estimated: false,
            },
        }
    }
}

/// The panel size to *draw from*, which is not simply the one the monitor
/// reported: a size outside the shared plausible range
/// (`crossover_topology::is_plausible_physical_size`) is treated exactly as
/// a missing one.
///
/// The wire and the state file admit 1..=10 000 mm, because a decoder's job
/// is arithmetic safety and refusing a peer's eccentric measurement is not
/// worth a terminated session. Drawing is a different question with a
/// different asymmetry. A size is a *proportion*, so one screen claiming
/// 1 mm does not draw one wrong rectangle — it seeds a four-unit sliver on
/// a real crossing seam, too small to grab, while every other rectangle
/// keeps its own scale; and one claiming 10 m seeds 40 000 units and dwarfs
/// the scene. Neither is repairable in the editor until the manual override
/// lands (feature/161), and both are reachable from a peer this machine
/// merely trusts to be *itself*, not to be correct. Falling back to the
/// estimate the module already computes costs the improvement for that one
/// screen and nothing else.
///
/// The range is `crossover-topology`'s rather than this module's precisely
/// so that it is the same range the Windows EDID reader applies to local
/// hardware: a size this machine would have refused to claim must not be
/// one it will draw from because a peer sent it instead.
fn believable(monitor: &LiveMonitor) -> Option<PhysicalSizeMm> {
    monitor
        .physical_size
        .filter(|size| is_plausible_physical_size(*size))
}

/// The median millimetres-per-DIP over the monitors of `monitors` that
/// measured themselves, or `None` when none did.
///
/// The **median** rather than the mean because one lying panel — a
/// television reporting a round 1600×900 mm, a partially-cached EDID —
/// should not drag every unmeasured rectangle on the desk with it, and a
/// desk has few enough screens that a single outlier is a large share of a
/// mean. One ratio per monitor rather than one per axis: a monitor's two
/// axes carry the same information about scale (pixels are square in every
/// case this seeds for), and blending them by summing both numerators and
/// both denominators weights the ratio by the monitor's own size, which is
/// the right weighting for a value used to size other rectangles.
#[must_use]
pub fn median_mm_per_dip(monitors: &[LiveMonitor]) -> Option<f64> {
    let mut ratios: Vec<f64> = monitors
        .iter()
        .filter_map(|monitor| {
            // The same [`believable`] filter the sizes themselves go
            // through: an implausible measurement must not steer the
            // rectangles of the screens that *did* measure believably,
            // which is precisely what letting it into this median would do.
            let size = believable(monitor)?;
            let (width_mm, height_mm) = oriented(size, (monitor.rect.width, monitor.rect.height));
            let (width_dip, height_dip) = dip_pair(monitor);
            // Both denominators are at least 1 (`dip_size` floors there), so
            // the sum cannot be zero and this cannot divide by zero.
            Some(f64::from(width_mm + height_mm) / f64::from(width_dip + height_dip))
        })
        .collect();
    if ratios.is_empty() {
        return None;
    }
    // `total_cmp` rather than `partial_cmp`: every ratio here is finite and
    // positive, but sorting must be a total order for the sort to be
    // defined at all, and a total one costs nothing.
    ratios.sort_by(f64::total_cmp);
    let middle = ratios.len() / 2;
    if ratios.len() % 2 == 1 {
        Some(ratios[middle])
    } else {
        // The mean of the two middles, so an even-sized desk's median does
        // not depend on which of the two the sort happened to put first.
        Some(f64::midpoint(ratios[middle - 1], ratios[middle]))
    }
}

/// A monitor's millimetres with its axes matched to the orientation the OS
/// is presenting it in — see the module doc's rotation paragraph.
fn oriented(size: PhysicalSizeMm, pixels: (u32, u32)) -> (u32, u32) {
    let width_mm = u32::from(size.width_mm());
    let height_mm = u32::from(size.height_mm());
    if (width_mm >= height_mm) == (pixels.0 >= pixels.1) {
        (width_mm, height_mm)
    } else {
        (height_mm, width_mm)
    }
}

/// A monitor's DIP size on both axes.
fn dip_pair(monitor: &LiveMonitor) -> (u32, u32) {
    (
        dip_size(monitor.rect.width, monitor.scale_percent),
        dip_size(monitor.rect.height, monitor.scale_percent),
    )
}

/// A monitor's size in DIPs: its live pixel size divided by its own scale
/// factor (ADR 0018). Rounds to the nearest unit rather than truncating, and
/// never to zero — a monitor decoded by [`LiveMonitor`] already has
/// `width, height >= 1` and `scale_percent` inside its bounds, so this is a
/// seed computation over already-validated numbers, not a boundary the way
/// the decoder is.
///
/// Unbounded above *here*, and bounded by [`bound`] at every call site: an
/// input extreme enough to exceed the layout model's ceiling (a
/// maximal-extent monitor at 25 % scale seeds 262 140) is one the pre-sizes
/// editor drew as an oversized rectangle that `Layout::new` then refused,
/// leaving a disabled Save button and an empty offender list — a dead end
/// with nothing to act on. Clamping is the only behavioural difference this
/// module makes to an unmeasured desk, it is confined to arrangements that
/// could not be saved at all, and it is what lets [`SeededSize`] promise a
/// legal extent without an exception.
pub(crate) fn dip_size(pixels: u32, scale_percent: u16) -> u32 {
    let scaled =
        (u64::from(pixels) * 100 + u64::from(scale_percent) / 2) / u64::from(scale_percent);
    u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
}

/// An integer drawn extent, held inside the layout model's bounds.
const fn bound(units: u32) -> u32 {
    if units < 1 {
        1
    } else if units > MAX_MONITOR_EXTENT {
        MAX_MONITOR_EXTENT
    } else {
        units
    }
}

/// A floating-point drawn extent, rounded and held inside the layout
/// model's bounds — total by construction, including for the infinities and
/// NaN no ratio here can actually produce.
fn to_extent(value: f64) -> u32 {
    if !value.is_finite() {
        return 1;
    }
    let clamped = value.round().clamp(1.0, f64::from(MAX_MONITOR_EXTENT));
    // `clamp` has already put this in `1.0..=65535.0`, which `u32`
    // represents exactly, so neither the truncation nor the sign can bite.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let narrowed = clamped as u32;
    narrowed
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{MachineScale, UNITS_PER_MM, dip_size, median_mm_per_dip};
    use crossover_topology::{
        LayoutRect, LiveMonitor, MAX_MONITOR_EXTENT, MAX_PHYSICAL_SIZE_MM, MonitorId,
        PhysicalSizeMm,
    };

    fn monitor(width: u32, height: u32, scale_percent: u16) -> LiveMonitor {
        LiveMonitor {
            id: MonitorId::new(r"\\.\DISPLAY1").unwrap(),
            rect: LayoutRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            scale_percent,
            label: None,
            physical_size: None,
        }
    }

    fn measured(
        width: u32,
        height: u32,
        scale_percent: u16,
        width_mm: u16,
        height_mm: u16,
    ) -> LiveMonitor {
        LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(width_mm, height_mm).unwrap()),
            ..monitor(width, height, scale_percent)
        }
    }

    #[test]
    fn dip_size_divides_by_scale_and_never_rounds_to_zero() {
        assert_eq!(dip_size(1920, 100), 1920);
        assert_eq!(dip_size(3840, 200), 1920);
        assert_eq!(dip_size(1, 500), 1); // rounds up from 0.2, floored at 1
    }

    /// The headline case, and the reason the branch exists: a 27" panel and
    /// a 13" laptop screen that seed the *same* size in DIPs seed in their
    /// real proportion once both have measured themselves.
    #[test]
    fn two_screens_of_equal_dip_size_draw_in_their_physical_proportion() {
        let desktop = measured(2560, 1440, 100, 597, 336);
        let laptop = measured(2560, 1440, 200, 286, 179);
        assert_eq!(dip_size(2560, 100), 2560);

        let scale = MachineScale::of(&[desktop.clone(), laptop.clone()], None);
        let big = scale.size_of(&desktop);
        let small = scale.size_of(&laptop);

        assert_eq!(big.width, 597 * UNITS_PER_MM);
        assert_eq!(big.height, 336 * UNITS_PER_MM);
        assert_eq!(small.width, 286 * UNITS_PER_MM);
        assert_eq!(small.height, 179 * UNITS_PER_MM);
        assert!(!big.estimated && !small.estimated);
        // The picture the user is owed: one is roughly twice the other.
        assert!(big.width > small.width * 2);
    }

    /// The chosen constant keeps a measured rectangle in the same magnitude
    /// range as the DIP seeding it replaces, which is the whole of its
    /// justification — so it is pinned rather than left to drift.
    #[test]
    fn a_measured_monitor_seeds_in_the_same_ballpark_as_its_dip_size() {
        let scale = MachineScale::of(&[measured(2560, 1440, 100, 597, 336)], None);
        let drawn = scale.size_of(&measured(2560, 1440, 100, 597, 336));
        assert_eq!(drawn.width, 2388);
        assert!((1920..=3200).contains(&drawn.width), "{drawn:?}");
    }

    /// An unmeasured monitor beside measured ones is not left at DIP
    /// magnitude — it is carried onto their scale, so the machine's own
    /// internal proportions stay believable and its rectangle is badged.
    #[test]
    fn an_unmeasured_monitor_borrows_its_machines_median_ratio() {
        // Two identical measured screens: 597 mm over 2560 DIP on one axis,
        // 336 over 1440 on the other — a median of (597+336)/(2560+1440).
        let measured_pair = [
            measured(2560, 1440, 100, 597, 336),
            measured(2560, 1440, 100, 597, 336),
        ];
        let unmeasured = monitor(2560, 1440, 100);
        let mut machine = measured_pair.to_vec();
        machine.push(unmeasured.clone());

        let scale = MachineScale::of(&machine, None);
        let drawn = scale.size_of(&unmeasured);
        let reference = scale.size_of(&measured_pair[0]);

        assert!(drawn.estimated);
        // Same pixels and the same scale as its measured twins, so the
        // ratio carries it to (very nearly) the same drawn size.
        assert!(drawn.width.abs_diff(reference.width) <= 2, "{drawn:?}");
        assert!(drawn.height.abs_diff(reference.height) <= 2, "{drawn:?}");
    }

    /// The median is a median: one panel lying *within* the believable
    /// range does not drag the unmeasured rectangles with it. (A panel
    /// lying outside it never reaches the median at all — see
    /// [`an_unbelievable_size_is_drawn_as_no_size_at_all`].)
    #[test]
    fn a_single_absurd_measurement_does_not_move_the_median() {
        let honest = measured(2560, 1440, 100, 597, 336);
        let other = measured(1920, 1080, 100, 447, 252);
        let liar = measured(1920, 1080, 100, 2400, 1600);
        let unmeasured = monitor(1920, 1080, 100);

        let with_liar = MachineScale::of(
            &[honest.clone(), other.clone(), liar, unmeasured.clone()],
            None,
        )
        .size_of(&unmeasured);
        let without =
            MachineScale::of(&[honest, other, unmeasured.clone()], None).size_of(&unmeasured);

        // Not bit-identical — three ratios take the middle one and two take
        // the midpoint of both — but moved by a unit, not by a multiple.
        // The same desk under a *mean* would seed the unmeasured screen
        // around twice too wide, which is the failure this chooses the
        // median to avoid.
        assert!(
            with_liar.width.abs_diff(without.width) <= 2,
            "{with_liar:?} vs {without:?}"
        );
        assert!(with_liar.width * 3 < without.width * 4, "{with_liar:?}");
    }

    /// The editor's own plausibility gate. A size the wire admits but no
    /// panel could have is drawn as *no size*: a 1 mm screen would
    /// otherwise seed a four-unit sliver sitting on a real crossing seam,
    /// too small to take hold of, and a 10 m one would seed 40 000 units
    /// and swallow the scene — neither repairable in the editor.
    #[test]
    fn an_unbelievable_size_is_drawn_as_no_size_at_all() {
        // A believable screen on the same machine, so the estimated arm has
        // a ratio to work from and the badge has something to contrast
        // with.
        let honest = measured(2560, 1440, 100, 597, 336);

        for (width_mm, height_mm) in [(1, 1), (10_000, 10_000), (49, 300), (597, 3_001)] {
            let absurd = measured(1920, 1080, 100, width_mm, height_mm);
            let machine = [honest.clone(), absurd.clone()];
            let scale = MachineScale::of(&machine, None);
            let drawn = scale.size_of(&absurd);

            assert!(
                drawn.estimated,
                "{width_mm}x{height_mm} mm was believed: {drawn:?}"
            );
            assert_ne!(drawn.width, u32::from(width_mm) * UNITS_PER_MM);
            // And it did not reach the median either: the honest screen
            // seeds exactly what it seeds on its own.
            assert_eq!(
                scale.size_of(&honest),
                MachineScale::of(std::slice::from_ref(&honest), None).size_of(&honest)
            );
        }

        // The boundaries themselves are believed, so the gate is a range
        // and not an accident of the numbers above.
        for (width_mm, height_mm) in [(50, 50), (3_000, 3_000)] {
            let edge = measured(1920, 1080, 100, width_mm, height_mm);
            let drawn = MachineScale::of(std::slice::from_ref(&edge), None).size_of(&edge);
            assert!(!drawn.estimated, "{width_mm}x{height_mm} mm was refused");
        }
    }

    /// A machine whose *only* size is unbelievable is a machine with no
    /// sizes: it falls all the way through to the unmeasured scene, badge
    /// included (which is to say, absent).
    #[test]
    fn a_lone_unbelievable_size_leaves_the_scene_wholly_unmeasured() {
        let absurd = measured(3840, 2160, 200, 1, 1);
        let scale = MachineScale::of(std::slice::from_ref(&absurd), None);
        assert_eq!(scale, MachineScale::dips());
        let drawn = scale.size_of(&absurd);
        assert_eq!((drawn.width, drawn.height), (1920, 1080));
        assert!(!drawn.estimated);
    }

    /// A machine that measured nothing borrows the *other* machine's ratio
    /// rather than seeding a whole group at a different magnitude from the
    /// desk it has to be arranged against.
    #[test]
    fn a_wholly_unmeasured_machine_borrows_the_other_machines_ratio() {
        let measured_machine = [measured(2560, 1440, 100, 597, 336)];
        let unmeasured = monitor(2560, 1440, 100);

        let fallback = median_mm_per_dip(&measured_machine);
        assert!(fallback.is_some());
        let borrowed = MachineScale::of(std::slice::from_ref(&unmeasured), fallback);
        let drawn = borrowed.size_of(&unmeasured);

        let reference = MachineScale::of(&measured_machine, None).size_of(&measured_machine[0]);
        assert!(drawn.estimated);
        assert!(drawn.width.abs_diff(reference.width) <= 2, "{drawn:?}");
    }

    /// A rotated panel measures itself in its own orientation; the drawn
    /// rectangle has to follow the pixels, not the panel.
    #[test]
    fn a_rotated_panel_draws_in_the_orientation_the_os_reports() {
        let portrait = measured(1440, 2560, 100, 597, 336);
        let drawn = MachineScale::of(std::slice::from_ref(&portrait), None).size_of(&portrait);
        assert_eq!(drawn.width, 336 * UNITS_PER_MM);
        assert_eq!(drawn.height, 597 * UNITS_PER_MM);
        assert!(drawn.height > drawn.width, "a portrait screen draws tall");
    }

    /// The behaviour-unchanged guarantee, stated as an example beside the
    /// property that proves it in general — including the badge, which a
    /// scene with nothing to contrast against does not paint.
    #[test]
    fn nothing_measured_anywhere_is_the_dip_seeding_exactly() {
        let scale = MachineScale::of(&[monitor(3840, 2160, 200)], None);
        let drawn = scale.size_of(&monitor(3840, 2160, 200));
        assert_eq!((drawn.width, drawn.height), (1920, 1080));
        assert!(
            !drawn.estimated,
            "a badge on every rectangle marks no difference"
        );
        assert_eq!(scale, MachineScale::dips());
    }

    /// The same monitor, once beside a measured screen and once not: the
    /// badge is a statement about the *scene*, not about the monitor.
    #[test]
    fn the_badge_marks_a_difference_in_fidelity_and_nothing_else() {
        let unmeasured = monitor(2560, 1440, 100);
        let alone = MachineScale::of(std::slice::from_ref(&unmeasured), None).size_of(&unmeasured);
        assert!(!alone.estimated);

        let beside_a_measured_screen = MachineScale::of(
            &[unmeasured.clone(), measured(1920, 1080, 100, 527, 296)],
            None,
        )
        .size_of(&unmeasured);
        assert!(beside_a_measured_screen.estimated);

        // Including when the measured screen is on the *other* machine,
        // which is where the borrowed ratio comes from.
        let borrowed = MachineScale::of(
            std::slice::from_ref(&unmeasured),
            median_mm_per_dip(&[measured(1920, 1080, 100, 527, 296)]),
        )
        .size_of(&unmeasured);
        assert!(borrowed.estimated);
    }

    fn any_monitor() -> impl Strategy<Value = LiveMonitor> {
        (
            1u32..=MAX_MONITOR_EXTENT,
            1u32..=MAX_MONITOR_EXTENT,
            25u16..=500,
            proptest::option::of((1u16..=MAX_PHYSICAL_SIZE_MM, 1u16..=MAX_PHYSICAL_SIZE_MM)),
        )
            .prop_map(|(width, height, scale_percent, physical)| LiveMonitor {
                physical_size: physical
                    .map(|(w, h)| PhysicalSizeMm::new(w, h).expect("in bounds by construction")),
                ..monitor(width, height, scale_percent)
            })
    }

    proptest! {
        /// Totality: any live geometry the model can hold produces a size,
        /// never a panic, and never one the layout model would refuse —
        /// uniformly, on every arm, which is what [`SeededSize`]'s field
        /// documentation promises.
        #[test]
        fn every_seeded_size_is_a_legal_extent(
            machine in proptest::collection::vec(any_monitor(), 1..=8),
        ) {
            let scale = MachineScale::of(&machine, None);
            for monitor in &machine {
                let drawn = scale.size_of(monitor);
                prop_assert!((1..=MAX_MONITOR_EXTENT).contains(&drawn.width), "{drawn:?}");
                prop_assert!((1..=MAX_MONITOR_EXTENT).contains(&drawn.height), "{drawn:?}");
            }
        }

        /// Determinism: the same machine seeds the same way every time, and
        /// the ratio does not depend on the order the monitors arrive in.
        #[test]
        fn seeding_is_deterministic_and_order_independent(
            machine in proptest::collection::vec(any_monitor(), 1..=8),
        ) {
            let scale = MachineScale::of(&machine, None);
            let mut reversed = machine.clone();
            reversed.reverse();
            let reversed_scale = MachineScale::of(&reversed, None);
            for monitor in &machine {
                prop_assert_eq!(scale.size_of(monitor), scale.size_of(monitor));
                prop_assert_eq!(scale.size_of(monitor), reversed_scale.size_of(monitor));
            }
        }

        /// The behaviour-unchanged guarantee: with no believable size
        /// anywhere, every seeded size is the DIP size the editor drew
        /// before panels could measure themselves — on inputs generated
        /// rather than chosen, and against the arithmetic **restated here**
        /// rather than borrowed from production, since a test that calls
        /// the function under test agrees with it by construction and
        /// proves nothing.
        #[test]
        fn with_nothing_measured_every_size_is_the_old_dip_size(
            machine in proptest::collection::vec(any_monitor(), 1..=8),
        ) {
            let machine: Vec<LiveMonitor> = machine
                .into_iter()
                .map(|monitor| LiveMonitor { physical_size: None, ..monitor })
                .collect();
            let scale = MachineScale::of(&machine, None);
            for monitor in &machine {
                let drawn = scale.size_of(monitor);
                let scale_percent = u64::from(monitor.scale_percent);
                // Pixels ÷ scale, rounded to nearest, floored at one unit
                // and held inside the layout model's ceiling.
                let expected = |pixels: u32| {
                    ((u64::from(pixels) * 100 + scale_percent / 2) / scale_percent)
                        .clamp(1, u64::from(MAX_MONITOR_EXTENT))
                };
                prop_assert_eq!(u64::from(drawn.width), expected(monitor.rect.width));
                prop_assert_eq!(u64::from(drawn.height), expected(monitor.rect.height));
                prop_assert!(!drawn.estimated, "nothing to be less certain than");
            }
        }

        /// A measured monitor's drawn rectangle is its panel's proportion,
        /// to integer rounding — the property the crossing mapping cares
        /// about, since a seam's fraction is read off the drawn edge.
        ///
        /// Stated without restating `oriented`'s own condition: the drawn
        /// rectangle carries the panel's two dimensions (in some order),
        /// and it is oriented the way the *pixels* are. Together those pin
        /// the rotation rule without borrowing its expression.
        #[test]
        fn a_measured_monitor_draws_at_its_panels_aspect(
            width_mm in 50u16..=3000,
            height_mm in 50u16..=3000,
            width in 640u32..=7680,
            height in 480u32..=4320,
        ) {
            let monitor = measured(width, height, 100, width_mm, height_mm);
            let drawn = MachineScale::of(std::slice::from_ref(&monitor), None).size_of(&monitor);

            let mut panel = [u32::from(width_mm) * UNITS_PER_MM, u32::from(height_mm) * UNITS_PER_MM];
            let mut sides = [drawn.width, drawn.height];
            panel.sort_unstable();
            sides.sort_unstable();
            prop_assert_eq!(sides, panel, "the drawn sides are the panel's");
            prop_assert_eq!(
                drawn.width >= drawn.height,
                width >= height,
                "the drawing follows the pixels, not the panel: {:?}",
                drawn
            );
            prop_assert!(!drawn.estimated);
        }
    }
}
