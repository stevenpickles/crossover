//! The drawn scene: both machines' monitors in layout space, built
//! read-only from a [`TopologyState`] (ADR 0018).
//!
//! This branch never writes a layout, so [`Model`] is exactly what the
//! renderer needs and nothing more: two groups of rectangles, in the
//! shared, unit-agnostic layout space, and whether they came from an
//! authoritative saved arrangement or were seeded here because none exists
//! (or none could be trusted).
//!
//! # Where the rectangles come from
//!
//! - **A [`LayoutState`] is present and validates.** [`LayoutState::validate`]
//!   is called against this session's pair before anything is drawn from
//!   it — a report is not a source of truth (ADR 0018) — and only a
//!   validated [`Layout`]'s placed rects are used, exactly as reported,
//!   already in layout space. `seeded` is `false`.
//! - **A `LayoutState` is present but does not validate** (re-pair residue,
//!   a hand-edited file, a document naming a version this build cannot
//!   place) **or names no live monitor at all** (every id a stranger —
//!   driver-renamed devices, most likely). This module never draws an
//!   empty canvas as if it were the saved arrangement: it falls back to
//!   the seed path below and records why in [`Model::rejected_layout`], so
//!   the renderer can say so and the app layer can log it (ADR 0018's
//!   reject-and-log rule for peer-influenced local state, T23).
//! - **No layout exists at all.** Nothing has been drawn, so this module
//!   seeds a starting arrangement rather than showing a blank canvas —
//!   `seeded` is `true`.
//!
//! In both seeded cases the renderer is expected to say so, since dragging
//! and saving a seed is a later branch's job, not this one's.
//!
//! # Live monitors the saved layout does not name
//!
//! A monitor this machine currently reports that the validated layout does
//! not place — plugged in after the layout was saved — is drawn too, not
//! hidden: seeded in below the placed rectangles and marked
//! `authoritative: false`, the mirror of the existing placed-but-unplugged
//! representation (`native_size: None`) for the opposite mismatch.
//!
//! # The seeding rule (ADR 0018)
//!
//! Each seeded monitor's drawn **size** is in DIPs: its live pixel size
//! divided by its own `scale_percent / 100`. That is the rule ADR 0018
//! states explicitly — `scale_percent` "is a seeding input only" — and it
//! is what makes a 4K monitor at 150% scale draw the same size as a 1080p
//! monitor at 100%, since both describe the same physical screen.
//!
//! **Position** is a seed this module has to invent, and it picks the
//! simplest rule that is both sensible and provably non-overlapping: each
//! machine's own monitors are laid out **left to right, abutting, in the
//! order their live geometry places them** (sorted by native `(x, y)`), and
//! the peer's group is placed **to the right of the local group**, with a
//! gap, at the same top edge. Abutting sibling monitors immediately shows
//! the user where their own internal seams are — which the real drag/snap
//! step in a later branch replaces — and the fixed left-to-right order
//! plus the unconditional gap between the two groups is what makes
//! non-overlap a property of the construction rather than something that
//! has to be checked after the fact: a machine's monitors can only abut,
//! never overlap, and the peer's group starts strictly to the right of the
//! local group's rightmost edge. The same packing, offset below a
//! machine's placed rectangles instead of beside another machine's, seeds
//! its unplaced-live supplement.

use std::collections::BTreeSet;

use crossover_topology::{
    DeviceId, DevicePair, Layout, LayoutRect, LayoutState, LiveMonitor, MonitorId, PlacedMonitor,
    TopologyState,
};

use crate::viewport::LayoutBounds;

/// The gap, in layout-space units, this module leaves between: the local
/// group's rightmost edge and the peer group's leftmost one (a whole-scene
/// seed), or a machine's placed rectangles and its unplaced-live
/// supplement (an authoritative scene with a monitor the layout does not
/// name) — enough to read as a deliberate gap rather than a rounding
/// artifact, and unrelated to any real measurement (nothing here is a
/// saved layout).
const SEED_GROUP_GAP: i64 = 96;

/// One monitor as the editor draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawnMonitor {
    /// Its platform-supplied identity — the text a monitor label shows.
    pub id: MonitorId,
    /// 1-based position within its machine's group, in the order drawn —
    /// what a short label ("1", "2") shows when the id itself does not fit.
    pub ordinal: usize,
    /// Where it is drawn, in the shared layout space.
    pub rect: LayoutRect,
    /// Its live pixel size, for the resolution a label shows — `None` when
    /// an authoritative layout names a monitor this machine did not report
    /// as currently live (unplugged, or a stale saved arrangement).
    pub native_size: Option<(u32, u32)>,
    /// `true` when `rect` is the saved arrangement's own position for this
    /// monitor. `false` when it is a seed: either the whole scene is seeded
    /// ([`Model::seeded`]), or this one monitor is live but the saved
    /// arrangement does not name it — a fact the renderer cues as
    /// *unplaced* rather than hides.
    pub authoritative: bool,
}

/// One machine's monitors, drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineGroup {
    pub device: DeviceId,
    pub name: String,
    pub monitors: Vec<DrawnMonitor>,
}

impl MachineGroup {
    /// The union of every monitor's rectangle, or `None` for an empty group
    /// (never produced by [`Model::from_state`], but not assumed away).
    #[must_use]
    pub fn bounds(&self) -> Option<LayoutBounds> {
        self.monitors
            .iter()
            .map(|monitor| rect_bounds(monitor.rect))
            .reduce(LayoutBounds::union)
    }
}

/// Whether at least one of the layout's placed monitors for this machine
/// actually corresponds to a monitor it currently reports live.
///
/// Deliberately *not* "does this group have an authoritative monitor at
/// all" — every placed monitor is `authoritative: true` regardless of
/// whether a live one matches it (that mismatch alone is the ordinary,
/// legal placed-but-unplugged case, `native_size: None`). What this checks
/// is the stronger, whole-machine condition issue 5's driver-renamed-ids
/// case needs: has *anything* in the saved arrangement survived contact
/// with reality, or is every id in it a stranger to what is plugged in
/// now.
fn has_a_live_match(group: &MachineGroup) -> bool {
    group
        .monitors
        .iter()
        .any(|monitor| monitor.authoritative && monitor.native_size.is_some())
}

/// The whole drawn scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub local: MachineGroup,
    /// `None` only when [`TopologyState::peer`] is `None` — no peer has
    /// ever been seen, which is exactly
    /// [`crate::session::EditorSession::WaitingForPeer`]'s trigger.
    pub peer: Option<MachineGroup>,
    /// `true` when these rects are this module's seed rather than a
    /// validated, saved [`crossover_topology::LayoutState`] — the
    /// renderer's cue to mark the arrangement as provisional.
    pub seeded: bool,
    /// `Some(reason)` when a saved layout existed but could not be trusted
    /// as drawn, so this scene is a seed instead of the ordinary "nothing
    /// saved yet" case. `None` covers both "no layout at all" and "a saved
    /// layout was used". See the module doc's reject-and-log paragraph.
    pub rejected_layout: Option<String>,
}

impl Model {
    /// Build the scene [`TopologyState`] describes.
    ///
    /// # Panics
    ///
    /// Never. A saved layout is validated through
    /// [`LayoutState::validate`] before anything is drawn from it, and
    /// every failure — an invalid pair, a rule `Layout::new` refuses, no
    /// live monitor matching it at all — is a typed `Err` this function
    /// turns into [`Model::rejected_layout`] and a seed, not a panic.
    #[must_use]
    pub fn from_state(state: &TopologyState) -> Self {
        if let Some(layout) = &state.layout {
            match Self::authoritative(state, layout) {
                Ok(model) => return model,
                Err(reason) => return Self::seed(state, Some(reason)),
            }
        }
        Self::seed(state, None)
    }

    /// Try to draw `state` from its saved `layout`. `Err` names why it
    /// could not be trusted, for [`Model::rejected_layout`].
    fn authoritative(state: &TopologyState, layout: &LayoutState) -> Result<Self, String> {
        let peer_device = state.peer.as_ref().map(|peer| peer.device);
        let pair = infer_pair(state.local.device, peer_device, layout)
            .ok_or_else(|| "the saved arrangement names only one machine".to_owned())?;
        let validated = layout.validate(&pair).map_err(|error| error.to_string())?;

        let local = authoritative_group(
            state.local.device,
            &state.local.name,
            &state.local.monitors,
            &validated,
        );

        let Some(peer) = state.peer.as_ref() else {
            // No peer has connected this run, but the saved arrangement
            // still names this machine's half of a past pairing — draw it
            // rather than re-seeding a guess that would contradict it.
            // `peer: None` still routes the session to `WaitingForPeer`.
            if !has_a_live_match(&local) {
                return Err("no live monitor matches the saved arrangement".to_owned());
            }
            return Ok(Self {
                local,
                peer: None,
                seeded: false,
                rejected_layout: None,
            });
        };

        let peer_group = authoritative_group(peer.device, &peer.name, &peer.monitors, &validated);
        if !has_a_live_match(&local) && !has_a_live_match(&peer_group) {
            // Every id in the saved arrangement is a stranger to both
            // machines' current live monitors (driver-renamed devices,
            // most likely) — nothing left to call authoritative about it.
            return Err(
                "no live monitor matches the saved arrangement (device ids changed?)".to_owned(),
            );
        }
        Ok(Self {
            local,
            peer: Some(peer_group),
            seeded: false,
            rejected_layout: None,
        })
    }

    fn seed(state: &TopologyState, rejected_layout: Option<String>) -> Self {
        let local = seed_group(
            state.local.device,
            &state.local.name,
            &state.local.monitors,
            0,
        );
        let Some(peer) = state.peer.as_ref() else {
            return Self {
                local,
                peer: None,
                seeded: true,
                rejected_layout,
            };
        };
        // `local`'s bounds come from `rect_bounds`, whose comment already
        // argues every value stays far inside `i64`'s exact `f64` range;
        // the same bound applies to `f64 -> i64` here.
        #[allow(clippy::cast_possible_truncation)]
        let start_x = local
            .bounds()
            .map_or(0, |bounds| bounds.max_x.ceil() as i64 + SEED_GROUP_GAP);
        let peer_group = seed_group(peer.device, &peer.name, &peer.monitors, start_x);
        Self {
            local,
            peer: Some(peer_group),
            seeded: true,
            rejected_layout,
        }
    }

    /// The union of every group's bounds, for [`crate::viewport::Viewport::fit`].
    /// Falls back to a unit square around the origin if there is nothing to
    /// draw at all, so `fit` never receives zero-area bounds.
    #[must_use]
    pub fn bounds(&self) -> LayoutBounds {
        let local = self.local.bounds();
        let peer = self.peer.as_ref().and_then(MachineGroup::bounds);
        match (local, peer) {
            (Some(a), Some(b)) => a.union(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => LayoutBounds::point(0.0, 0.0),
        }
    }
}

/// The [`DevicePair`] a saved layout should be validated against: the
/// known peer if one has connected this run, or — when none has — whichever
/// device besides `local` the layout itself names, so a layout drawn
/// before the peer's very first connection this run (or before a re-pair)
/// can still be validated instead of discarded on sight merely because
/// `state.peer` happens to be `None` right now.
fn infer_pair(local: DeviceId, peer: Option<DeviceId>, layout: &LayoutState) -> Option<DevicePair> {
    let other = match peer {
        Some(device) => device,
        None => layout
            .monitors
            .iter()
            .map(|monitor| monitor.device)
            .find(|&device| device != local)?,
    };
    DevicePair::new(local, other).ok()
}

// `LayoutRect::right`/`bottom` return `i64` so the derivation arithmetic
// they're built for cannot overflow (ADR 0018), but every value a `Model`
// ever produces is bounded well inside an edge coordinate's `2^24` ceiling
// — far short of where `i64 -> f64` could lose precision.
#[allow(clippy::cast_precision_loss)]
fn rect_bounds(rect: LayoutRect) -> LayoutBounds {
    LayoutBounds {
        min_x: f64::from(rect.x),
        min_y: f64::from(rect.y),
        max_x: rect.right() as f64,
        max_y: rect.bottom() as f64,
    }
}

/// A monitor's drawn size in DIPs: its live pixel size divided by its own
/// scale factor (ADR 0018). Rounds to the nearest unit rather than
/// truncating, and never to zero — a monitor decoded by
/// [`crossover_topology::LiveMonitor`] already has `width, height >= 1` and
/// `scale_percent` inside its bounds, so this is a seed computation over
/// already-validated numbers, not a boundary the way the decoder is.
fn dip_size(pixels: u32, scale_percent: u16) -> u32 {
    let scaled =
        (u64::from(pixels) * 100 + u64::from(scale_percent) / 2) / u64::from(scale_percent);
    u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
}

/// Seed a whole machine's group: its monitors, DIP-sized, packed left to
/// right abutting in their live left-to-right order, starting at `start_x`,
/// `y = 0`.
fn seed_group(
    device: DeviceId,
    name: &str,
    monitors: &[LiveMonitor],
    start_x: i64,
) -> MachineGroup {
    let drawn = seed_monitors(monitors, start_x, 0, 0);
    MachineGroup {
        device,
        name: name.to_owned(),
        monitors: drawn,
    }
}

/// Seed a list of live monitors, DIP-sized, packed left to right abutting
/// in their live left-to-right order, starting at `(start_x, y)` and
/// numbered from `starting_ordinal + 1`. The building block both
/// [`seed_group`] (a whole machine, `y = 0`, `starting_ordinal = 0`) and
/// [`authoritative_group`]'s unplaced-monitor supplement (below the placed
/// rectangles, continuing their ordinals) share.
fn seed_monitors(
    monitors: &[LiveMonitor],
    start_x: i64,
    y: i32,
    starting_ordinal: usize,
) -> Vec<DrawnMonitor> {
    let mut ordered: Vec<&LiveMonitor> = monitors.iter().collect();
    ordered.sort_by_key(|m| (m.rect.x, m.rect.y));

    let mut x = start_x;
    let mut drawn = Vec::with_capacity(ordered.len());
    for (index, monitor) in ordered.into_iter().enumerate() {
        let width = dip_size(monitor.rect.width, monitor.scale_percent);
        let height = dip_size(monitor.rect.height, monitor.scale_percent);
        drawn.push(DrawnMonitor {
            id: monitor.id.clone(),
            ordinal: starting_ordinal + index + 1,
            rect: LayoutRect {
                x: clamp_coordinate(x),
                y,
                width,
                height,
            },
            native_size: Some((monitor.rect.width, monitor.rect.height)),
            authoritative: false,
        });
        x += i64::from(width);
    }
    drawn
}

/// The bounding box of some already-drawn monitors, in `i64` — the
/// coordinate space [`clamp_coordinate`] and `LayoutRect::left`/`right`
/// already work in, so seeding the unplaced supplement below an
/// authoritative group needs no detour through `f64`.
fn bbox_i64(monitors: &[DrawnMonitor]) -> Option<(i64, i64, i64, i64)> {
    monitors
        .iter()
        .map(|monitor| {
            (
                monitor.rect.left(),
                monitor.rect.top(),
                monitor.rect.right(),
                monitor.rect.bottom(),
            )
        })
        .reduce(|(al, at, ar, ab), (bl, bt, br, bb)| {
            (al.min(bl), at.min(bt), ar.max(br), ab.max(bb))
        })
}

/// One machine's group from a validated [`Layout`]: its placed rects, used
/// exactly as reported, plus any live monitor the layout does not name
/// (see the module doc's "live monitors the saved layout does not name").
fn authoritative_group(
    device: DeviceId,
    name: &str,
    live: &[LiveMonitor],
    layout: &Layout,
) -> MachineGroup {
    let mut placed: Vec<&PlacedMonitor> = layout
        .monitors()
        .iter()
        .filter(|monitor| monitor.device == device)
        .collect();
    // A stable draw order (rather than the layout's own, arbitrary) keeps
    // `ordinal` meaningful and this function's output deterministic for a
    // given input, independent of how the worker happened to list them.
    placed.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    let mut monitors: Vec<DrawnMonitor> = placed
        .iter()
        .enumerate()
        .map(|(index, monitor)| DrawnMonitor {
            id: monitor.id.clone(),
            ordinal: index + 1,
            rect: monitor.rect,
            native_size: live
                .iter()
                .find(|candidate| candidate.id == monitor.id)
                .map(|candidate| (candidate.rect.width, candidate.rect.height)),
            authoritative: true,
        })
        .collect();

    let placed_ids: BTreeSet<&str> = placed.iter().map(|monitor| monitor.id.as_str()).collect();
    let unplaced: Vec<LiveMonitor> = live
        .iter()
        .filter(|candidate| !placed_ids.contains(candidate.id.as_str()))
        .cloned()
        .collect();
    if !unplaced.is_empty() {
        let (start_x, start_y) = bbox_i64(&monitors).map_or((0, 0), |(min_x, _, _, max_y)| {
            (min_x, max_y + SEED_GROUP_GAP)
        });
        let extra = seed_monitors(
            &unplaced,
            start_x,
            clamp_coordinate(start_y),
            monitors.len(),
        );
        monitors.extend(extra);
    }

    MachineGroup {
        device,
        name: name.to_owned(),
        monitors,
    }
}

/// Clamp a seed coordinate into [`crossover_topology::LayoutRect`]'s legal
/// range, so an adversarially large input (many maximal-extent monitors)
/// degrades to overlapping seed rectangles rather than to a value that
/// could not construct a `LayoutRect` at all. This is a display-only
/// fallback: nothing downstream in this branch validates a *seeded*
/// arrangement as a [`crossover_topology::Layout`], and saving one is a
/// later branch's job.
fn clamp_coordinate(value: i64) -> i32 {
    let clamped = value.clamp(
        i64::from(-crossover_topology::MAX_LAYOUT_COORDINATE),
        i64::from(crossover_topology::MAX_LAYOUT_COORDINATE),
    );
    // The clamp above puts `clamped` inside ±`MAX_LAYOUT_COORDINATE`
    // (`2^24`), which `i32` represents exactly, so this cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = clamped as i32;
    narrowed
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{Model, dip_size};
    use crossover_topology::{
        DeviceId, DevicePair, Layout, LayoutRect, LayoutState, LiveMonitor, MachineState,
        MonitorId, PeerState, PlacedMonitor, TOPOLOGY_STATE_VERSION, TopologyState,
    };

    const LOCAL: DeviceId = DeviceId::from_bytes([0x11; 16]);
    const PEER: DeviceId = DeviceId::from_bytes([0x22; 16]);

    fn live(id: &str, x: i32, width: u32, height: u32, scale_percent: u16) -> LiveMonitor {
        LiveMonitor {
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x,
                y: 0,
                width,
                height,
            },
            scale_percent,
        }
    }

    fn state(
        local: Vec<LiveMonitor>,
        peer: Vec<LiveMonitor>,
        layout: Option<LayoutState>,
    ) -> TopologyState {
        TopologyState {
            version: TOPOLOGY_STATE_VERSION,
            written_at: 0,
            local: MachineState {
                device: LOCAL,
                name: "desk".to_owned(),
                monitors: local,
            },
            peer: Some(PeerState {
                device: PEER,
                name: "laptop".to_owned(),
                connected: true,
                last_seen: 0,
                monitors: peer,
            }),
            layout,
        }
    }

    fn placed(device: DeviceId, id: &str, x: i32, width: u32, height: u32) -> PlacedMonitor {
        PlacedMonitor {
            device,
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x,
                y: 0,
                width,
                height,
            },
        }
    }

    fn side_by_side_layout() -> LayoutState {
        let pair = DevicePair::new(LOCAL, PEER).unwrap();
        let monitors = vec![
            placed(LOCAL, r"\\.\DISPLAY1", 0, 1920, 1080),
            placed(PEER, r"\\.\DISPLAY1", 1920, 1920, 1080),
        ];
        let layout = Layout::new(5, LOCAL, monitors, &pair).unwrap();
        LayoutState::from_layout(&layout)
    }

    #[test]
    fn dip_size_divides_by_scale_and_never_rounds_to_zero() {
        assert_eq!(dip_size(1920, 100), 1920);
        assert_eq!(dip_size(3840, 200), 1920);
        assert_eq!(dip_size(1, 500), 1); // rounds up from 0.2, floored at 1
    }

    #[test]
    fn a_scaled_monitor_draws_the_same_size_as_its_unscaled_physical_twin() {
        let scene = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 3840, 2160, 200)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        let peer = scene.peer.unwrap();
        assert_eq!(scene.local.monitors[0].rect.width, 1920);
        assert_eq!(scene.local.monitors[0].rect.height, 1080);
        assert_eq!(peer.monitors[0].rect.width, 1920);
        assert_eq!(peer.monitors[0].rect.height, 1080);
    }

    #[test]
    fn no_saved_layout_seeds_one_and_says_so() {
        let scene = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        assert!(scene.seeded);
        assert!(scene.rejected_layout.is_none());
    }

    #[test]
    fn a_validated_saved_layout_is_authoritative_and_unseeded() {
        let scene = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            Some(side_by_side_layout()),
        ));
        assert!(!scene.seeded);
        assert!(scene.rejected_layout.is_none());
        assert_eq!(scene.local.monitors.len(), 1);
        assert_eq!(scene.local.monitors[0].rect.x, 0);
        assert!(scene.local.monitors[0].authoritative);
        assert_eq!(scene.peer.unwrap().monitors[0].rect.x, 1920);
    }

    /// Issue 2: a saved layout that fails validation (here, a stranger
    /// device — the re-pair residue case) falls back to a seed, marks
    /// itself as such, and names why — never an empty or half-drawn
    /// canvas presented as authoritative.
    #[test]
    fn an_invalid_saved_layout_falls_back_to_a_seed_with_a_reason() {
        let stranger = DeviceId::from_bytes([0x99; 16]);
        let pair = DevicePair::new(LOCAL, stranger).unwrap();
        let monitors = vec![
            placed(LOCAL, r"\\.\DISPLAY1", 0, 1920, 1080),
            placed(stranger, r"\\.\DISPLAY1", 1920, 1920, 1080),
        ];
        let layout = Layout::new(5, LOCAL, monitors, &pair).unwrap();
        let saved = LayoutState::from_layout(&layout);

        let scene = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            Some(saved),
        ));
        assert!(scene.seeded);
        assert!(scene.rejected_layout.is_some(), "{scene:?}");
    }

    /// Issue 5's driver-renamed-ids case: a validated layout whose ids
    /// match neither machine's current live monitors falls back to a seed
    /// too, rather than drawing an authoritative-looking scene with
    /// nothing actually placed.
    #[test]
    fn a_layout_matching_no_live_monitor_at_all_falls_back_to_a_seed() {
        let scene = Model::from_state(&state(
            vec![live("RENAMED-LOCAL", 0, 1920, 1080, 100)],
            vec![live("RENAMED-PEER", 0, 1920, 1080, 100)],
            Some(side_by_side_layout()),
        ));
        assert!(scene.seeded);
        assert!(scene.rejected_layout.is_some(), "{scene:?}");
    }

    /// Issue 5: a live monitor the saved layout does not name is drawn,
    /// not dropped — marked unplaced, positioned below the placed ones,
    /// and not overlapping them.
    #[test]
    fn a_live_monitor_the_layout_does_not_name_is_drawn_as_unplaced() {
        let scene = Model::from_state(&state(
            vec![
                live(r"\\.\DISPLAY1", 0, 1920, 1080, 100),
                live(r"\\.\DISPLAY2", 1920, 1280, 1024, 100),
            ],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            Some(side_by_side_layout()),
        ));
        assert!(!scene.seeded);
        let local = &scene.local;
        assert_eq!(local.monitors.len(), 2, "{local:?}");
        let placed = local
            .monitors
            .iter()
            .find(|m| m.id.as_str() == r"\\.\DISPLAY1")
            .unwrap();
        assert!(placed.authoritative);
        let unplaced = local
            .monitors
            .iter()
            .find(|m| m.id.as_str() == r"\\.\DISPLAY2")
            .unwrap();
        assert!(!unplaced.authoritative);
        assert!(
            !placed.rect.overlaps(unplaced.rect),
            "{placed:?} overlaps {unplaced:?}"
        );
    }

    /// Issue 6: a peer that has never connected this run, but a saved
    /// layout that still names this machine's half of a past pairing,
    /// draws that half authoritatively rather than re-seeding a guess
    /// that would contradict it — `peer` stays `None`.
    #[test]
    fn a_saved_layout_survives_a_peer_that_has_not_connected_yet() {
        let mut document = state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![],
            Some(side_by_side_layout()),
        );
        document.peer = None;
        let scene = Model::from_state(&document);
        assert!(!scene.seeded, "{scene:?}");
        assert!(scene.peer.is_none());
        assert_eq!(scene.local.monitors.len(), 1);
        assert_eq!(scene.local.monitors[0].rect.x, 0);
        assert!(scene.local.monitors[0].authoritative);
    }

    #[test]
    fn no_peer_ever_seen_and_no_layout_leaves_the_peer_group_absent() {
        let mut document = state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![],
            None,
        );
        document.peer = None;
        let scene = Model::from_state(&document);
        assert!(scene.peer.is_none());
        assert!(scene.seeded);
        assert_eq!(scene.local.monitors.len(), 1);
    }

    #[test]
    fn the_seed_places_the_peer_group_strictly_right_of_the_local_group() {
        let scene = Model::from_state(&state(
            vec![
                live(r"\\.\DISPLAY1", 0, 1920, 1080, 100),
                live(r"\\.\DISPLAY2", 1920, 1280, 1024, 100),
            ],
            vec![live(r"\\.\DISPLAY1", 0, 2560, 1440, 150)],
            None,
        ));
        let local_bounds = scene.local.bounds().unwrap();
        let peer_bounds = scene.peer.unwrap().bounds().unwrap();
        assert!(
            peer_bounds.min_x >= local_bounds.max_x,
            "peer group ({peer_bounds:?}) overlaps the local group ({local_bounds:?})"
        );
    }

    proptest! {
        /// Every seed this module produces is internally non-overlapping —
        /// within each machine's own group (abutting, never overlapping by
        /// construction) and between the two groups (the gap) — for any
        /// combination of monitor counts, sizes, positions and scales the
        /// state-file decoder could have admitted.
        #[test]
        fn seeded_arrangements_never_overlap(
            local_monitors in proptest::collection::vec(
                (0i32..20_000, 1u32..4_000, 1u32..4_000, 25u16..=500),
                1..6,
            ),
            peer_monitors in proptest::collection::vec(
                (0i32..20_000, 1u32..4_000, 1u32..4_000, 25u16..=500),
                1..6,
            ),
        ) {
            let local: Vec<LiveMonitor> = local_monitors
                .into_iter()
                .enumerate()
                .map(|(index, (x, width, height, scale))| {
                    live(&format!("L{index}"), x, width, height, scale)
                })
                .collect();
            let peer: Vec<LiveMonitor> = peer_monitors
                .into_iter()
                .enumerate()
                .map(|(index, (x, width, height, scale))| {
                    live(&format!("P{index}"), x, width, height, scale)
                })
                .collect();

            let scene = Model::from_state(&state(local, peer, None));
            let all: Vec<LayoutRect> = scene
                .local
                .monitors
                .iter()
                .chain(scene.peer.as_ref().unwrap().monitors.iter())
                .map(|m| m.rect)
                .collect();
            for (index, first) in all.iter().enumerate() {
                for second in &all[index + 1..] {
                    prop_assert!(!first.overlaps(*second), "{first:?} overlaps {second:?}");
                }
            }
        }
    }
}
