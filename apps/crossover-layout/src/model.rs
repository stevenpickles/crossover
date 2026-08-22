//! The drawn scene: both machines' monitors in layout space, built from a
//! [`TopologyState`] and then **edited** (ADR 0018).
//!
//! [`Model`] is two groups of rectangles in the shared, unit-agnostic
//! layout space, whether they came from an authoritative saved arrangement
//! or were seeded here because none exists (or none could be trusted), and
//! — since the editing slice — the drag in progress, whether the drawing
//! has unsaved changes, and what validation makes of it.
//!
//! # Dragging is a whole machine at a time
//!
//! [`Model::begin_drag`] names a monitor and moves that machine's **entire
//! group**, rigidly. ADR 0018: "a machine's monitors drag as a rigid group
//! in the editor... their relative placement is the OS's fact, not the
//! user's to redraw". Every rectangle in the group takes the same integer
//! delta, so the intra-machine geometry that came from `DisplayInfo` is
//! preserved exactly, whatever the user does with the mouse. What the
//! drawing *does* say is where the two machines sit relative to each
//! other, which is the one question the layout answers.
//!
//! # Validation runs on every drop, and blocks or warns
//!
//! [`Model::end_drag`] re-runs the shared validation — the same
//! [`crossover_topology::Layout::new`] the worker and the wire use, so
//! there is no second opinion about what is legal — and sorts the result
//! into two kinds ([`Diagnostics`]):
//!
//! - **Blocking**: an arrangement that is not a layout at all. Overlap is
//!   the one a drag can produce; the rest are the rules a scene assembled
//!   from a state file could break. Save is refused while one stands,
//!   because writing it would hand the worker a config it must reject.
//! - **Warning**: the two machines do not touch. That is a *legal*
//!   drawing — ADR 0018 makes connectivity explicitly not a rule, since "a
//!   monitor parked with nothing abutting it is a legal drawing that
//!   produces no crossings on its free edges" — so it saves. It is worth
//!   saying out loud because an arrangement with no seam is an arrangement
//!   with no seamless transfer, which is rarely what a user meant to draw.
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
//! In both seeded cases the renderer says so — a seed is a starting guess,
//! and a guess the user has not accepted by dragging and saving it is not
//! an arrangement anything acts on.
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
//! Each seeded monitor's drawn **size** comes from [`crate::seeding`],
//! which measures a panel in its own millimetres where the platform could
//! read them and falls back to DIPs — carried onto the machine's measured
//! scale — where it could not. That module owns the rule and the argument
//! for it; what matters *here* is that a size is a value this module is
//! handed rather than one it computes, and that a monitor seeded from the
//! fallback carries [`DrawnMonitor::size_estimated`] so the drawing can say
//! so.
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
//!
//! Both properties survive [`crate::seeding`] changing the *widths*,
//! and that is the reason this packing is worth keeping rather than
//! replacing with one derived from the live pixel positions: each x is
//! derived from the widths actually drawn, so exact abutment and
//! non-overlap hold whatever a monitor measures. Positions taken from
//! pixel geometry would have neither guarantee — cloned displays share a
//! pixel rectangle exactly, which would seed two rectangles on top of each
//! other and block the save with an overlap the user never drew.

use std::collections::BTreeSet;

use crossover_topology::{
    DeviceId, DevicePair, Layout, LayoutError, LayoutRect, LayoutState, LiveMonitor, MonitorId,
    MonitorKey, MonitorLabel, PlacedMonitor, TopologyState,
};

use crate::seeding::{self, MachineScale};
use crate::snap::{self, Guide};
use crate::viewport::{LayoutBounds, Viewport};

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
    /// Its platform-supplied identity — what a caption falls back to when
    /// there is no product name, and what every diagnostic names.
    pub id: MonitorId,
    /// Its owning machine's product name for it, where that machine's
    /// platform advertised one. Display-only and *not unique*: two
    /// identical screens on one desk carry the same string, which
    /// [`crate::caption`] disambiguates rather than the model.
    pub label: Option<MonitorLabel>,
    /// 1-based position within its machine's group, in the order drawn —
    /// what a short label ("1", "2") shows when the id itself does not fit.
    pub ordinal: usize,
    /// Where it is drawn, in the shared layout space.
    pub rect: LayoutRect,
    /// Its live pixel size, for the resolution a label shows — `None` when
    /// an authoritative layout names a monitor this machine did not report
    /// as currently live (unplugged, or a stale saved arrangement).
    pub native_size: Option<(u32, u32)>,
    /// Its live scale factor, from the same live report as
    /// [`Self::native_size`] and `None` on the same absence.
    ///
    /// Kept beside the pixel size because the pair — not the pixels alone —
    /// is what says whether a seeded extent still describes this screen: a
    /// monitor whose DPI changed reports the same pixels at a new scale,
    /// and seeds a different rectangle for it ([`transplant_group`]).
    pub native_scale_percent: Option<u16>,
    /// `true` when `rect` is the saved arrangement's own position for this
    /// monitor. `false` when it is a seed: either the whole scene is seeded
    /// ([`Model::seeded`]), or this one monitor is live but the saved
    /// arrangement does not name it — a fact the renderer cues as
    /// *unplaced* rather than hides.
    pub authoritative: bool,
    /// `true` when this rectangle's **size** is a guess rather than a
    /// measurement: it was seeded, the machine could not believably say how
    /// big the panel physically is, and something else in the scene could
    /// ([`crate::seeding::SeededSize::estimated`] states the whole rule).
    ///
    /// Always `false` for a rectangle an authoritative layout placed — that
    /// size is the saved arrangement's, which is not a guess whatever the
    /// platform knows about the panel today. That is an invariant of every
    /// path that builds or updates a group, [`transplant_group`] included.
    pub size_estimated: bool,
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

/// A drag in progress: which machine is moving, where the pointer took
/// hold of it, where its rectangles were when it did, and what it is being
/// snapped against.
///
/// Holding the group's **original** rectangles rather than accumulating
/// per-frame deltas is what makes a drag exact: every frame recomputes one
/// translation from the pointer's current position, so nothing drifts and
/// releasing at the start point leaves the arrangement untouched.
///
/// Everything a pointer move needs is frozen here at the drag's start — the
/// moving rectangles, the stationary ones, and the transform — so
/// [`Model::drag_to`] allocates nothing per frame. A gesture is seconds
/// long and the scene it is aimed at is the scene the user is looking at;
/// re-deriving the target set sixty times a second would only let it change
/// under the pointer.
#[derive(Debug, Clone, PartialEq)]
pub struct Drag {
    device: DeviceId,
    /// Where the pointer grabbed, in layout space.
    grab: (f64, f64),
    /// The moving group's monitor ids as the drag began, in group order and
    /// in step with [`Drag::origin`]. Kept so a drag can be checked against
    /// a scene the state-file poll rebuilt underneath it (see
    /// [`Model::transplant_from`]) rather than being paired blindly by
    /// position in a list.
    origin_ids: Vec<MonitorId>,
    /// The moving group's rectangles as the drag began, in group order.
    origin: Vec<LayoutRect>,
    /// Every rectangle that is standing still, frozen at the drag's start.
    stationary: Vec<LayoutRect>,
    /// The viewport as the drag began, frozen for its duration.
    ///
    /// The canvas refits the viewport to the scene's bounds every frame,
    /// so a drag that grows the bounds would rescale the picture under the
    /// pointer, which changes where the pointer *is* in layout space,
    /// which moves the group again: a feedback loop, felt as the
    /// arrangement sliding away from the cursor. Freezing the transform
    /// for the drag's duration removes the loop rather than damping it.
    viewport: Viewport,
    /// The guides the last [`Model::drag_to`] produced.
    guides: Vec<Guide>,
}

impl Drag {
    /// The machine being dragged.
    #[must_use]
    pub const fn device(&self) -> DeviceId {
        self.device
    }

    /// The guides to draw for the current position.
    #[must_use]
    pub fn guides(&self) -> &[Guide] {
        &self.guides
    }

    /// The transform frozen at the drag's start — see the field's docs.
    #[must_use]
    pub const fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Whether `group` is still the group this drag took hold of: the same
    /// monitors, in the same order. A machine whose display set changed
    /// mid-gesture is no longer the thing being dragged, and continuing
    /// against it would pair rectangles with origins that do not belong to
    /// them.
    fn still_describes(&self, group: &MachineGroup) -> bool {
        group.monitors.len() == self.origin_ids.len()
            && group
                .monitors
                .iter()
                .zip(&self.origin_ids)
                .all(|(drawn, id)| &drawn.id == id)
    }

    /// Whether `group` currently sits anywhere other than where this drag
    /// found it — the question "has anything actually changed" asks, taken
    /// from the rectangles rather than from a flag that has to be kept in
    /// step with them.
    fn moved(&self, group: &MachineGroup) -> bool {
        !self.still_describes(group)
            || group
                .monitors
                .iter()
                .zip(&self.origin)
                .any(|(drawn, origin)| drawn.rect != *origin)
    }
}

/// The whole drawn scene, and the edit in progress on it.
///
/// No `Eq`: a drag carries the frozen [`Viewport`], whose scale is
/// floating point.
#[derive(Debug, Clone, PartialEq)]
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
    /// The highest layout revision the state file reported, or `0` when it
    /// reported none — half of the `seen_max` a save numbers itself one
    /// past (ADR 0018; the other half is the config file's own revision,
    /// which `save.rs` reads at the moment it writes).
    pub seen_revision: u64,
    /// Whether this scene has been edited since it was built or last
    /// saved. The Save button's first condition, and what an attempt to
    /// close the window asks about.
    dirty: bool,
    /// The drag in progress, if any.
    drag: Option<Drag>,
    /// What the shared validation makes of the scene as it stands,
    /// recomputed on construction and on every drop.
    diagnostics: Diagnostics,
}

/// Why a scene could not be expressed as a [`Layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneError {
    /// No peer has ever been seen, so there is only one machine to draw.
    /// A layout describes two.
    NoPeer,
    /// The arrangement broke one of the shared rules.
    Invalid(LayoutError),
}

impl std::fmt::Display for SceneError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPeer => formatter.write_str(
                "no peer machine has been seen yet, so there is nothing to arrange against",
            ),
            Self::Invalid(error) => write!(formatter, "{error}"),
        }
    }
}

/// Written out rather than derived, because `thiserror` is not one of this
/// crate's direct dependencies and ADR 0019 fixes that list (see
/// `save.rs`'s own error type for the same note).
///
/// No `source`: [`SceneError::Invalid`]'s own `Display` already carries the
/// [`LayoutError`]'s message, and reporting it twice — once here and again
/// down the chain — is how an error line ends up saying the same thing to
/// itself.
impl std::error::Error for SceneError {}

/// How badly a diagnostic wants to be seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The arrangement cannot be saved.
    Blocking,
    /// The arrangement can be saved, and is probably not what was meant.
    Warning,
}

/// One reason a save is refused, and which monitors to outline for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    /// What to tell the user.
    pub message: String,
    /// The monitors at fault — empty when the fault is the whole
    /// arrangement's rather than any one rectangle's.
    pub offenders: Vec<MonitorKey>,
}

/// What validation makes of the scene: what refuses a save, and what is
/// merely worth saying (see the module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// Blocking faults. A save is refused while this is non-empty.
    pub blocking: Vec<Blocker>,
    /// Warnings — saved anyway.
    pub warnings: Vec<String>,
}

impl Diagnostics {
    /// Whether a save must be refused.
    #[must_use]
    pub fn blocks_save(&self) -> bool {
        !self.blocking.is_empty()
    }

    /// Whether this monitor is named by a blocking diagnostic — the
    /// renderer's cue to outline it.
    #[must_use]
    pub fn offends(&self, device: DeviceId, id: &MonitorId) -> bool {
        self.blocking.iter().any(|blocker| {
            blocker
                .offenders
                .iter()
                .any(|key| key.device == device && &key.id == id)
        })
    }

    /// The one line worth showing, worst first, or `None` when the
    /// arrangement is clean.
    #[must_use]
    pub fn headline(&self) -> Option<(Severity, &str)> {
        if let Some(blocker) = self.blocking.first() {
            return Some((Severity::Blocking, blocker.message.as_str()));
        }
        self.warnings
            .first()
            .map(|warning| (Severity::Warning, warning.as_str()))
    }
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

        let (local_scale, peer_scale) = scales(state);
        let local = authoritative_group(
            state.local.device,
            &state.local.name,
            &state.local.monitors,
            &validated,
            local_scale,
        );

        let Some(peer) = state.peer.as_ref() else {
            // No peer has connected this run, but the saved arrangement
            // still names this machine's half of a past pairing — draw it
            // rather than re-seeding a guess that would contradict it.
            // `peer: None` still routes the session to `WaitingForPeer`.
            if !has_a_live_match(&local) {
                return Err("no live monitor matches the saved arrangement".to_owned());
            }
            return Ok(Self::assemble(
                local,
                None,
                false,
                None,
                validated.revision(),
            ));
        };

        let peer_group = authoritative_group(
            peer.device,
            &peer.name,
            &peer.monitors,
            &validated,
            peer_scale,
        );
        if !has_a_live_match(&local) && !has_a_live_match(&peer_group) {
            // Every id in the saved arrangement is a stranger to both
            // machines' current live monitors (driver-renamed devices,
            // most likely) — nothing left to call authoritative about it.
            return Err(
                "no live monitor matches the saved arrangement (device ids changed?)".to_owned(),
            );
        }
        Ok(Self::assemble(
            local,
            Some(peer_group),
            false,
            None,
            validated.revision(),
        ))
    }

    /// The one place a [`Model`] is built: it fills the editing state
    /// (nothing dragged, nothing dirty) and runs validation once, so a
    /// scene is never observed with stale diagnostics.
    fn assemble(
        local: MachineGroup,
        peer: Option<MachineGroup>,
        seeded: bool,
        rejected_layout: Option<String>,
        seen_revision: u64,
    ) -> Self {
        let mut model = Self {
            local,
            peer,
            seeded,
            rejected_layout,
            seen_revision,
            dirty: false,
            drag: None,
            diagnostics: Diagnostics::default(),
        };
        model.diagnostics = model.compute_diagnostics();
        model
    }

    fn seed(state: &TopologyState, rejected_layout: Option<String>) -> Self {
        let seen_revision = state.layout.as_ref().map_or(0, |layout| layout.revision);
        let (local_scale, peer_scale) = scales(state);
        let local = seed_group(
            state.local.device,
            &state.local.name,
            &state.local.monitors,
            local_scale,
            0,
        );
        let Some(peer) = state.peer.as_ref() else {
            return Self::assemble(local, None, true, rejected_layout, seen_revision);
        };
        // `local`'s bounds come from `rect_bounds`, whose comment already
        // argues every value stays far inside `i64`'s exact `f64` range;
        // the same bound applies to `f64 -> i64` here.
        #[allow(clippy::cast_possible_truncation)]
        let start_x = local
            .bounds()
            .map_or(0, |bounds| bounds.max_x.ceil() as i64 + SEED_GROUP_GAP);
        let peer_group = seed_group(peer.device, &peer.name, &peer.monitors, peer_scale, start_x);
        Self::assemble(
            local,
            Some(peer_group),
            true,
            rejected_layout,
            seen_revision,
        )
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

    /// Both machines' groups, local first.
    pub fn groups(&self) -> impl Iterator<Item = &MachineGroup> {
        std::iter::once(&self.local).chain(self.peer.as_ref())
    }

    /// Whether the drawing has unsaved changes — set by the *drop*, so it
    /// is `false` for the whole of a gesture still in the user's hand. See
    /// [`Model::has_unsaved_work`] for the question most callers actually
    /// mean.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether this scene holds work that exists nowhere but this window:
    /// an edit that has not been written, **or** a gesture the user has not
    /// let go of yet.
    ///
    /// The second half matters as much as the first. [`Model::is_dirty`] is
    /// only true after [`Model::end_drag`], so a caller that asked about
    /// dirtiness alone would treat the middle of a drag as a clean scene —
    /// and the two callers that ask (the state-file poll, deciding whether
    /// to overwrite the drawing; the close interception, deciding whether
    /// to ask) would each discard a gesture in progress without a word.
    #[must_use]
    pub const fn has_unsaved_work(&self) -> bool {
        self.dirty || self.drag.is_some()
    }

    /// What validation makes of the scene as it stands.
    #[must_use]
    pub const fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// The drag in progress, if any — the renderer's source for the
    /// guides and the frozen transform.
    #[must_use]
    pub const fn drag(&self) -> Option<&Drag> {
        self.drag.as_ref()
    }

    /// Whether a save is both wanted and allowed: something to write, and
    /// nothing blocking. Exactly the Save button's enabled condition.
    #[must_use]
    pub fn can_save(&self) -> bool {
        self.dirty && !self.diagnostics.blocks_save()
    }

    /// Record that this scene has been written to the config file at
    /// `revision`. The worker's own ~2 s re-read is what makes it live;
    /// this only stops the editor offering to save the same drawing twice.
    pub const fn mark_saved(&mut self, revision: u64) {
        self.dirty = false;
        self.seen_revision = revision;
    }

    /// Whether `other` still describes the machines this scene was drawn
    /// for — the question a poll asks before keeping the user's work over a
    /// freshly read document.
    ///
    /// A **different peer** is a re-pair: the rectangles describe two
    /// machines and one of them is no longer at the other end, which ADR
    /// 0018 discards rather than guesses about. A peer that has simply
    /// *gone* (`peer: None` against the same local machine) is not that: it
    /// is the window between a worker restarting and its session coming
    /// back, and treating an absence as a stranger would throw away an edit
    /// every time the worker was restarted under the editor.
    #[must_use]
    pub fn describes_the_same_machines(&self, other: &Self) -> bool {
        if self.local.device != other.local.device {
            return false;
        }
        // Two peers that are both present and *different* is the only pair
        // that contradicts the drawing. An absence on either side does not:
        // it is a peer not seen yet or not seen any more, and neither is
        // evidence that the rectangles now describe somebody else.
        match (self.peer.as_ref(), other.peer.as_ref()) {
            (Some(was), Some(now)) => was.device == now.device,
            _ => true,
        }
    }

    /// Put `previous`'s **user work** onto this freshly built scene.
    ///
    /// The direction is deliberate and is the whole point: the scene is
    /// built from the *fresh* document and then the user's work is moved
    /// onto it, rather than the previous scene being kept and patched up.
    /// So every fact the worker reports wins — which monitors exist, what
    /// the machines are called, what the saved layout says, whether this
    /// scene is a seed and why — and the only things carried across are the
    /// ones that exist nowhere else:
    ///
    /// - **Where the user dragged each machine** ([`transplant_group`]).
    /// - **Whether the drawing is unsaved**, so a poll cannot quietly clear
    ///   the Save button.
    /// - **The gesture in the user's hand**, when the fresh scene is still
    ///   a scene it can be applied to.
    /// - **The highest revision seen**, as the *maximum* of the two. A
    ///   fresh document that has not caught up with a save just made would
    ///   otherwise walk `seen_revision` backwards, and the next save would
    ///   number itself below the one it replaces — newest-revision-wins
    ///   would then silently supersede the user's own edit (ADR 0018).
    pub fn transplant_from(&mut self, previous: &Self) {
        self.seen_revision = self.seen_revision.max(previous.seen_revision);
        transplant_group(&mut self.local, &previous.local);
        if let (Some(fresh), Some(was)) = (self.peer.as_mut(), previous.peer.as_ref())
            && fresh.device == was.device
        {
            transplant_group(fresh, was);
        }
        self.dirty = previous.dirty;

        self.drag = match &previous.drag {
            Some(drag)
                if self
                    .groups()
                    .find(|group| group.device == drag.device)
                    .is_some_and(|group| drag.still_describes(group)) =>
            {
                Some(drag.clone())
            }
            Some(drag) => {
                // The dragged machine's monitors changed underneath the
                // gesture — a display docked or unplugged mid-drag. The
                // drag cannot continue against a group it no longer
                // describes, so it is committed where it stands rather
                // than silently undone.
                if previous.moved_during(drag) {
                    self.dirty = true;
                }
                None
            }
            None => None,
        };

        self.diagnostics = self.compute_diagnostics();
    }

    /// The monitor drawn at `point` (layout space), or `None` for empty
    /// canvas. Later groups win, matching the paint order — the peer is
    /// drawn over the local machine, so it is the peer the pointer takes
    /// hold of where (illegally) they overlap.
    #[must_use]
    pub fn monitor_at(&self, point: (f64, f64)) -> Option<MonitorKey> {
        let mut hit = None;
        for group in self.groups() {
            for monitor in &group.monitors {
                if contains(monitor.rect, point) {
                    hit = Some(MonitorKey {
                        device: group.device,
                        id: monitor.id.clone(),
                    });
                }
            }
        }
        hit
    }

    /// Take hold of `target`'s **machine** — every monitor of it — with
    /// the pointer at `grab`, drawn through `viewport`.
    ///
    /// A target no group holds is ignored rather than refused: a hit test
    /// and a drag start are two frames apart at worst, and an editor that
    /// panicked because a state-file poll landed between them would be a
    /// poor trade for a click that can simply do nothing.
    pub fn begin_drag(&mut self, target: &MonitorKey, grab: (f64, f64), viewport: Viewport) {
        let Some(group) = self.groups().find(|group| group.device == target.device) else {
            return;
        };
        if !group.monitors.iter().any(|drawn| drawn.id == target.id) {
            return;
        }
        let origin_ids = group
            .monitors
            .iter()
            .map(|drawn| drawn.id.clone())
            .collect();
        let origin = group.monitors.iter().map(|drawn| drawn.rect).collect();
        // The snap targets, frozen once here rather than rebuilt on every
        // pointer move (see the type's docs).
        let stationary = self
            .groups()
            .filter(|other| other.device != target.device)
            .flat_map(|other| other.monitors.iter().map(|drawn| drawn.rect))
            .collect();
        self.drag = Some(Drag {
            device: target.device,
            grab,
            origin_ids,
            origin,
            stationary,
            viewport,
            guides: Vec::new(),
        });
    }

    /// Move the dragged group so the grabbed point follows `pointer`,
    /// snapped against every monitor that is standing still.
    ///
    /// Idempotent in the pointer: the same `pointer` always produces the
    /// same arrangement, because the translation is computed from the
    /// drag's origin rather than accumulated.
    pub fn drag_to(&mut self, pointer: (f64, f64)) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        let raw = (pointer.0 - drag.grab.0, pointer.1 - drag.grab.1);
        let snapped = snap::snap(
            &drag.origin,
            &drag.stationary,
            raw,
            snap::threshold_for(drag.viewport.scale),
        );
        // The coordinate ceiling is enforced against the *group*, not each
        // rectangle, so hitting it slides the whole machine to a stop
        // rather than deforming it — rigidity is not negotiable (ADR
        // 0018), and a clamp per rectangle would break it exactly where
        // the arrangement is already extreme.
        let delta = clamp_delta(&drag.origin, snapped.delta);
        drag.guides = if delta == snapped.delta {
            snapped.guides
        } else {
            Vec::new()
        };

        if let Some(group) = self.group_mut(drag.device) {
            for (monitor, origin) in group.monitors.iter_mut().zip(&drag.origin) {
                monitor.rect = translated(*origin, delta);
            }
        }
        self.drag = Some(drag);
    }

    /// Let go: the arrangement stands, the scene is dirty if it actually
    /// moved, and validation runs (the module doc's blocking-vs-warning).
    pub fn end_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        // Derived from the rectangles, not from a flag maintained during
        // the gesture: "did this change anything" has exactly one honest
        // answer, and it is where the group ended up against where it
        // started.
        if self.moved_during(&drag) {
            self.dirty = true;
        }
        self.diagnostics = self.compute_diagnostics();
    }

    /// Whether `drag`'s machine has ended up anywhere other than where the
    /// drag found it.
    fn moved_during(&self, drag: &Drag) -> bool {
        self.groups()
            .find(|group| group.device == drag.device)
            .is_some_and(|group| drag.moved(group))
    }

    /// The whole scene as placed monitors, both machines together — every
    /// rectangle the user can see, including one that is live but was not
    /// named by the saved arrangement (drawing it and then not saving it
    /// would be the editor lying about what it wrote).
    #[must_use]
    pub fn placed(&self) -> Vec<PlacedMonitor> {
        self.groups()
            .flat_map(|group| {
                group.monitors.iter().map(|drawn| PlacedMonitor {
                    device: group.device,
                    id: drawn.id.clone(),
                    rect: drawn.rect,
                })
            })
            .collect()
    }

    /// The scene as a validated [`Layout`] at `revision`, drawn by this
    /// machine.
    ///
    /// This is the *only* judge of whether the drawing is legal: the same
    /// [`crossover_topology::Layout::new`] the worker runs on load and the
    /// protocol runs on the wire, so the editor cannot hold an opinion
    /// about legality that anything downstream disagrees with.
    ///
    /// # Errors
    ///
    /// [`SceneError`] — no peer to arrange against, or a rule the drawing
    /// breaks.
    pub fn to_layout(&self, revision: u64) -> Result<Layout, SceneError> {
        let Some(peer) = &self.peer else {
            return Err(SceneError::NoPeer);
        };
        let pair = DevicePair::new(self.local.device, peer.device).map_err(SceneError::Invalid)?;
        Layout::new(revision, self.local.device, self.placed(), &pair).map_err(SceneError::Invalid)
    }

    fn group_mut(&mut self, device: DeviceId) -> Option<&mut MachineGroup> {
        if self.local.device == device {
            return Some(&mut self.local);
        }
        self.peer.as_mut().filter(|group| group.device == device)
    }

    /// Validate the scene and sort the verdict into blockers and warnings.
    fn compute_diagnostics(&self) -> Diagnostics {
        match self.to_layout(self.seen_revision) {
            Ok(layout) => {
                if machines_touch(&layout, self.local.device) {
                    Diagnostics::default()
                } else {
                    Diagnostics {
                        blocking: Vec::new(),
                        warnings: vec![
                            "The two machines do not touch, so nothing will cross between \
                             them — drag one until an edge snaps to the other."
                                .to_owned(),
                        ],
                    }
                }
            }
            Err(error) => Diagnostics {
                blocking: vec![blocker(&error)],
                warnings: Vec::new(),
            },
        }
    }
}

/// Move `fresh`'s rectangles to where the user put them.
///
/// A monitor the previous scene also drew always takes that scene's
/// **position**. Whether it also keeps that scene's **extent** is one
/// question with two halves, and both halves have to hold:
///
/// - **Is the fresh rectangle a seed?** Only a seed's extent is this
///   module's to hold on to. A rectangle a validated arrangement placed is
///   the *user's saved size*, freshly read, and it wins outright — over a
///   seed the previous scene had computed, and over that seed's badge,
///   which is why an authoritative rectangle is never marked estimated
///   (see [`DrawnMonitor::size_estimated`]).
/// - **Is it still the same screen?** Pixels *and* scale, together. A seed
///   can change underneath a running editor for reasons that are no news
///   at all to the user — the worker learns a panel's physical size a
///   moment after it learns the panel exists, so a monitor seeded from the
///   DIP fallback on one read is seeded from millimetres on the next — and
///   re-seeding a rectangle the user is in the middle of arranging would
///   resize it under their hand, the same wipe this transplant exists to
///   prevent in a form that is harder to see. But a **resolution or DPI
///   change is news**: both change what the screen is, both change what the
///   seed computes, and the pixel size alone would silently swallow the
///   second (a 4K screen at 100 % and at 200 % reports the same pixels and
///   seeds half the size). Either changing takes the fresh extent, keeping
///   only the position.
///
/// A monitor only the fresh scene has — a display docked while the editor
/// was open — is offered at its seeded place plus the translation the user
/// applied to the rest of its machine, so it arrives beside its siblings
/// rather than back where the seed put them before the drag. When the two
/// scenes share no monitor at all there is no translation to infer and the
/// fresh group is left exactly as built.
fn transplant_group(fresh: &mut MachineGroup, previous: &MachineGroup) {
    let placed = |id: &MonitorId| previous.monitors.iter().find(|was| &was.id == id);
    let Some(delta) = fresh.monitors.iter().find_map(|monitor| {
        placed(&monitor.id).map(|was| {
            (
                was.rect.left() - monitor.rect.left(),
                was.rect.top() - monitor.rect.top(),
            )
        })
    }) else {
        return;
    };
    for monitor in &mut fresh.monitors {
        match previous.monitors.iter().find(|was| was.id == monitor.id) {
            Some(was) if !monitor.authoritative && describes_the_same_screen(was, monitor) => {
                // A seed, for a screen that has not changed: the rectangle
                // the user is arranging stands whole, and the badge travels
                // with the size it describes rather than with a size that
                // is not on screen.
                monitor.rect = was.rect;
                monitor.size_estimated = was.size_estimated;
            }
            Some(was) => {
                monitor.rect = LayoutRect {
                    x: was.rect.x,
                    y: was.rect.y,
                    ..monitor.rect
                };
            }
            None => monitor.rect = translated(monitor.rect, delta),
        }
    }
}

/// Whether two reads of one monitor id describe a screen that has not
/// changed in any way the seed depends on — its pixel size and its scale
/// factor, which are exactly [`crate::seeding`]'s two live inputs.
///
/// Not "are these the same monitor": the id already answered that. This
/// asks the narrower question the transplant needs — *would seeding it
/// again produce a different rectangle, for a reason the user would
/// recognise as news about their hardware?*
fn describes_the_same_screen(was: &DrawnMonitor, fresh: &DrawnMonitor) -> bool {
    was.native_size == fresh.native_size && was.native_scale_percent == fresh.native_scale_percent
}

/// The blocking diagnostic for a scene that is not a layout, naming the
/// monitors at fault where the rule knows which they are.
fn blocker(error: &SceneError) -> Blocker {
    let offenders = match error {
        SceneError::Invalid(LayoutError::Overlap { first, second }) => {
            vec![first.clone(), second.clone()]
        }
        SceneError::Invalid(_) | SceneError::NoPeer => Vec::new(),
    };
    Blocker {
        message: match error {
            SceneError::Invalid(LayoutError::Overlap { .. }) => {
                "Two screens overlap. A cursor in the overlap has no single answer for \
                 which screen it left, so this cannot be saved."
                    .to_owned()
            }
            other => format!("This arrangement cannot be saved: {other}."),
        },
        offenders,
    }
}

/// Whether any local monitor abuts any peer monitor — the question the
/// disconnection warning asks.
///
/// The rule itself is [`LayoutRect::abuts`], in `crossover-topology`
/// beside `overlaps`, so the editor cannot hold an opinion about adjacency
/// that differs from the shared model's. The crossing *derivation*
/// (`crates/crossover-core/src/crossing.rs`) is still the authority on what
/// an adjacency means — the spans, the sides, where a cursor lands — and it
/// currently restates the same predicate inline; a later sweep is expected
/// to have it call this one. A difference between the two would show up as
/// an editor that promises a crossing the worker will not make.
fn machines_touch(layout: &Layout, local: DeviceId) -> bool {
    let mine: Vec<LayoutRect> = layout
        .monitors()
        .iter()
        .filter(|monitor| monitor.device == local)
        .map(|monitor| monitor.rect)
        .collect();
    layout
        .monitors()
        .iter()
        .filter(|monitor| monitor.device != local)
        .any(|theirs| mine.iter().any(|rect| rect.abuts(theirs.rect)))
}

/// Is this layout-space point inside the rectangle? Half-open on the far
/// edges, so two abutting monitors never both claim the shared column.
fn contains(rect: LayoutRect, point: (f64, f64)) -> bool {
    let (x, y) = point;
    #[allow(clippy::cast_precision_loss)]
    let (left, top, right, bottom) = (
        f64::from(rect.x),
        f64::from(rect.y),
        rect.right() as f64,
        rect.bottom() as f64,
    );
    x >= left && x < right && y >= top && y < bottom
}

/// `delta`, reduced until every rectangle of the group stays inside
/// ±[`crossover_topology::MAX_LAYOUT_COORDINATE`] — one clamp for the
/// whole group, so the group stays rigid (see [`Model::drag_to`]).
fn clamp_delta(origin: &[LayoutRect], delta: (i64, i64)) -> (i64, i64) {
    (
        clamp_axis(origin.iter().copied().map(LayoutRect::left), delta.0),
        clamp_axis(origin.iter().copied().map(LayoutRect::top), delta.1),
    )
}

/// One axis of [`clamp_delta`]: `requested`, reduced so that both the
/// lowest and the highest of `coordinates` stay inside
/// ±[`crossover_topology::MAX_LAYOUT_COORDINATE`] after moving.
///
/// Total by construction, for the two degenerate inputs it can be handed:
/// an empty group has nothing to move, and a group already wider than the
/// whole coordinate space cannot move at all — the latter unreachable from
/// any scene this crate builds, and worth the branch anyway because a
/// `clamp` with an inverted range panics, which no drag may ever do.
fn clamp_axis(coordinates: impl Iterator<Item = i64> + Clone, requested: i64) -> i64 {
    let limit = i64::from(crossover_topology::MAX_LAYOUT_COORDINATE);
    let (Some(lowest), Some(highest)) = (coordinates.clone().min(), coordinates.max()) else {
        return 0;
    };
    let (floor, ceiling) = (-limit - lowest, limit - highest);
    if floor > ceiling {
        return 0;
    }
    requested.clamp(floor, ceiling)
}

/// `rect` moved by `delta`, clamped the way a seed is — the clamp is
/// unreachable after [`clamp_delta`] and is kept only so this function is
/// total.
fn translated(rect: LayoutRect, delta: (i64, i64)) -> LayoutRect {
    LayoutRect {
        x: clamp_coordinate(rect.left() + delta.0),
        y: clamp_coordinate(rect.top() + delta.1),
        ..rect
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

/// Both machines' [`MachineScale`]s, each falling back to the other's
/// measurements when its own machine has none.
///
/// Computed here, before either group is built, because the fallback is the
/// one part of the size rule that is a fact about the *pair*: a desk that
/// measured nothing has no ratio of its own to seed by, and borrowing the
/// other desk's is what stops the two groups being drawn at magnitudes that
/// cannot be compared (see [`MachineScale::of`]).
fn scales(state: &TopologyState) -> (MachineScale, MachineScale) {
    let peer_monitors: &[LiveMonitor] = state
        .peer
        .as_ref()
        .map_or(&[], |peer| peer.monitors.as_slice());
    let local_ratio = seeding::median_mm_per_dip(&state.local.monitors);
    let peer_ratio = seeding::median_mm_per_dip(peer_monitors);
    (
        MachineScale::of(&state.local.monitors, peer_ratio),
        MachineScale::of(peer_monitors, local_ratio),
    )
}

/// Seed a whole machine's group: its monitors sized by `scale`, packed left
/// to right abutting in their live left-to-right order, starting at
/// `start_x`, `y = 0`.
fn seed_group(
    device: DeviceId,
    name: &str,
    monitors: &[LiveMonitor],
    scale: MachineScale,
    start_x: i64,
) -> MachineGroup {
    let drawn = seed_monitors(monitors, scale, start_x, 0, 0);
    MachineGroup {
        device,
        name: name.to_owned(),
        monitors: drawn,
    }
}

/// Seed a list of live monitors, sized by `scale`, packed left to right
/// abutting in their live left-to-right order, starting at `(start_x, y)`
/// and numbered from `starting_ordinal + 1`. The building block both
/// [`seed_group`] (a whole machine, `y = 0`, `starting_ordinal = 0`) and
/// [`authoritative_group`]'s unplaced-monitor supplement (below the placed
/// rectangles, continuing their ordinals) share.
///
/// `scale` is the whole machine's, even where the list being seeded is only
/// part of it: a monitor docked into an already-arranged desk has to be
/// drawn on the same scale as the screens it is docking beside, and the
/// ratio those screens establish is the machine's, not the supplement's.
fn seed_monitors(
    monitors: &[LiveMonitor],
    scale: MachineScale,
    start_x: i64,
    y: i32,
    starting_ordinal: usize,
) -> Vec<DrawnMonitor> {
    let mut ordered: Vec<&LiveMonitor> = monitors.iter().collect();
    ordered.sort_by_key(|m| (m.rect.x, m.rect.y));

    let mut x = start_x;
    let mut drawn = Vec::with_capacity(ordered.len());
    for (index, monitor) in ordered.into_iter().enumerate() {
        let size = scale.size_of(monitor);
        drawn.push(DrawnMonitor {
            id: monitor.id.clone(),
            label: monitor.label.clone(),
            ordinal: starting_ordinal + index + 1,
            rect: LayoutRect {
                x: clamp_coordinate(x),
                y,
                width: size.width,
                height: size.height,
            },
            native_size: Some((monitor.rect.width, monitor.rect.height)),
            native_scale_percent: Some(monitor.scale_percent),
            authoritative: false,
            size_estimated: size.estimated,
        });
        // Abutment is a property of this line: the next rectangle starts at
        // exactly the width the last one was drawn at, whatever decided
        // that width. A seam the user can see is a seam the layout model
        // sees too, since its abutment test has zero tolerance (ADR 0018).
        x += i64::from(size.width);
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
    scale: MachineScale,
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
        .map(|(index, monitor)| {
            // The live entry, if this placed monitor is one this machine
            // currently reports: the source of both its native size and
            // its caption. A placed-but-unplugged monitor has neither,
            // which is the ordinary saved-arrangement case, and its
            // caption falls back to the id it was saved under.
            let live = live.iter().find(|candidate| candidate.id == monitor.id);
            DrawnMonitor {
                id: monitor.id.clone(),
                label: live.and_then(|candidate| candidate.label.clone()),
                ordinal: index + 1,
                rect: monitor.rect,
                native_size: live.map(|candidate| (candidate.rect.width, candidate.rect.height)),
                native_scale_percent: live.map(|candidate| candidate.scale_percent),
                authoritative: true,
                // A placed rectangle's size is the saved arrangement's, so
                // nothing about it is estimated — not even when the panel
                // behind it declines to measure itself today.
                size_estimated: false,
            }
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
            scale,
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

    use super::Model;
    use crate::seeding::UNITS_PER_MM;
    use crate::test_support::{
        LOCAL_DEVICE as LOCAL, PEER_DEVICE as PEER, drag_by, monitor_key as key, unit_viewport,
    };
    use crossover_topology::{
        DeviceId, DevicePair, Layout, LayoutRect, LayoutState, LiveMonitor, MachineState,
        MonitorId, MonitorLabel, PeerState, PhysicalSizeMm, PlacedMonitor, TOPOLOGY_STATE_VERSION,
        TopologyState,
    };

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
            label: None,
            physical_size: None,
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

    /// The product name reaches the drawn monitor on **both** paths a
    /// group can be built by — a seeded scene and an authoritative one —
    /// and on both machines.
    #[test]
    fn a_drawn_monitor_carries_the_product_name_of_the_live_screen() {
        let named = |id: &str, label: &str| LiveMonitor {
            label: Some(crossover_topology::MonitorLabel::new(label).unwrap()),
            ..live(id, 0, 1920, 1080, 100)
        };

        // Seeded: no saved arrangement, so every rectangle comes from
        // `seed_monitors`.
        let scene = Model::from_state(&state(
            vec![named(r"\\.\DISPLAY1", "DELL U2720Q")],
            vec![named(r"\\.\DISPLAY1", "LG ULTRAGEAR")],
            None,
        ));
        assert!(scene.seeded);
        assert_eq!(
            scene.local.monitors[0]
                .label
                .as_ref()
                .map(MonitorLabel::as_str),
            Some("DELL U2720Q")
        );
        assert_eq!(
            scene.peer.unwrap().monitors[0]
                .label
                .as_ref()
                .map(MonitorLabel::as_str),
            Some("LG ULTRAGEAR")
        );

        // Authoritative: the same monitors, now placed by a saved layout,
        // so every rectangle comes from `authoritative_group` instead.
        let scene = Model::from_state(&state(
            vec![named(r"\\.\DISPLAY1", "DELL U2720Q")],
            vec![named(r"\\.\DISPLAY1", "LG ULTRAGEAR")],
            Some(side_by_side_layout()),
        ));
        assert!(!scene.seeded);
        assert!(scene.local.monitors[0].authoritative);
        assert_eq!(
            scene.local.monitors[0]
                .label
                .as_ref()
                .map(MonitorLabel::as_str),
            Some("DELL U2720Q")
        );
    }

    /// A monitor the arrangement places but the machine no longer reports
    /// live has no caption to take — the same `None` its `native_size`
    /// takes, and from the same absence. Its caption falls back to the id
    /// it was saved under, which is exactly the id a user needs to see to
    /// understand why it is not there.
    #[test]
    fn a_placed_but_unplugged_monitor_has_no_product_name() {
        let scene = Model::from_state(&state(
            // Nothing live matching the layout's `\\.\DISPLAY1`.
            vec![live(r"\\.\DISPLAY7", 0, 1920, 1080, 100)],
            vec![LiveMonitor {
                label: Some(crossover_topology::MonitorLabel::new("LG ULTRAGEAR").unwrap()),
                ..live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)
            }],
            Some(side_by_side_layout()),
        ));
        let placed = scene
            .local
            .monitors
            .iter()
            .find(|monitor| monitor.id.as_str() == r"\\.\DISPLAY1")
            .expect("the layout's monitor is drawn even though it is unplugged");
        assert!(placed.authoritative);
        assert_eq!(placed.native_size, None);
        assert_eq!(placed.label, None);
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

    /// One 27" panel and one 13" laptop screen, both 2560×1440 in DIPs, in
    /// a whole seeded scene — the picture the branch exists to produce, and
    /// the one the DIP seeding above cannot: the same two monitors draw the
    /// same size until they say how big they are.
    #[test]
    fn measured_screens_seed_in_their_physical_proportion() {
        let sized = |monitor: LiveMonitor, width_mm: u16, height_mm: u16| LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(width_mm, height_mm).unwrap()),
            ..monitor
        };
        let scene = Model::from_state(&state(
            vec![sized(live(r"\\.\DISPLAY1", 0, 2560, 1440, 100), 597, 336)],
            vec![sized(live(r"\\.\DISPLAY1", 0, 2560, 1440, 200), 286, 179)],
            None,
        ));

        let desktop = scene.local.monitors[0].rect;
        let laptop = scene.peer.as_ref().unwrap().monitors[0].rect;
        assert_eq!(desktop.width, 597 * UNITS_PER_MM);
        assert_eq!(laptop.width, 286 * UNITS_PER_MM);
        // 336 mm of panel against 179 mm, drawn as such — where the DIP
        // seeding drew the two rectangles identically.
        assert!(
            desktop.height * 100 / laptop.height >= 187,
            "{desktop:?} against {laptop:?}"
        );
        assert!(
            !scene.local.monitors[0].size_estimated,
            "a measured screen is not a guess"
        );
    }

    /// The badge's decision, at the model layer: a screen that would not
    /// measure itself is drawn on its machine's scale — not at DIP
    /// magnitude beside rectangles four times its size — and is marked as
    /// the estimate it is.
    #[test]
    fn an_unmeasured_screen_is_scaled_to_its_measured_siblings_and_marked() {
        let measured = LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(597, 336).unwrap()),
            ..live(r"\\.\DISPLAY1", 0, 2560, 1440, 100)
        };
        let scene = Model::from_state(&state(
            vec![measured, live(r"\\.\DISPLAY2", 2560, 2560, 1440, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));

        let drawn = &scene.local.monitors;
        assert!(!drawn[0].size_estimated);
        assert!(drawn[1].size_estimated, "nothing measured this one");
        // The same pixels at the same scale as its measured sibling, so the
        // borrowed ratio puts it at (very nearly) the same drawn size —
        // rather than at 2560 units beside its sibling's 2388.
        assert!(
            drawn[1].rect.width.abs_diff(drawn[0].rect.width) <= 2,
            "{drawn:?}"
        );
        // The peer measured nothing at all, so it borrows the local
        // machine's ratio rather than being drawn at DIP magnitude.
        let peer = &scene.peer.as_ref().unwrap().monitors[0];
        assert!(peer.size_estimated);
        assert!(peer.rect.width < 1920, "{peer:?}");
    }

    /// A rectangle an authoritative arrangement placed is never badged:
    /// its size is what the user saved, whatever the panel behind it will
    /// or will not say about itself today.
    #[test]
    fn a_placed_rectangle_is_never_marked_estimated() {
        let scene = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            Some(side_by_side_layout()),
        ));
        assert!(scene.local.monitors[0].authoritative);
        assert!(!scene.local.monitors[0].size_estimated);
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

    /// Two local screens and one peer screen, seeded: local at
    /// `0..1920` and `1920..3200`, the peer's group a gap to the right.
    fn two_and_one() -> Model {
        Model::from_state(&state(
            vec![
                live(r"\\.\DISPLAY1", 0, 1920, 1080, 100),
                live(r"\\.\DISPLAY2", 1920, 1280, 1024, 100),
            ],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ))
    }

    /// Every rectangle of the grabbed machine moves by one delta, and the
    /// machine that was not grabbed does not move at all — ADR 0018's
    /// rigid group, which is the OS's fact about the desk and not the
    /// user's to redraw.
    #[test]
    fn a_drag_moves_the_whole_machine_rigidly_and_nothing_else() {
        let mut model = two_and_one();
        let before: Vec<LayoutRect> = model.local.monitors.iter().map(|m| m.rect).collect();
        let peer_before: Vec<LayoutRect> = model
            .peer
            .as_ref()
            .unwrap()
            .monitors
            .iter()
            .map(|m| m.rect)
            .collect();

        drag_by(&mut model, LOCAL, r"\\.\DISPLAY1", (-500.0, 250.0));

        let after: Vec<LayoutRect> = model.local.monitors.iter().map(|m| m.rect).collect();
        let deltas: Vec<(i64, i64)> = before
            .iter()
            .zip(&after)
            .map(|(was, now)| (now.left() - was.left(), now.top() - was.top()))
            .collect();
        assert_eq!(deltas[0], deltas[1], "the group did not move as one");
        assert_eq!(deltas[0], (-500, 250));
        // Sizes are untouched, so the intra-machine geometry is exactly
        // what the OS reported.
        for (was, now) in before.iter().zip(&after) {
            assert_eq!((was.width, was.height), (now.width, now.height));
        }
        let peer_after: Vec<LayoutRect> = model
            .peer
            .as_ref()
            .unwrap()
            .monitors
            .iter()
            .map(|m| m.rect)
            .collect();
        assert_eq!(peer_before, peer_after, "the peer's group must not move");
    }

    /// Grabbing either machine works — the peer's rectangles are as
    /// draggable as this machine's, because the drawing describes where
    /// the two sit relative to each other and either end can say it.
    #[test]
    fn the_peer_group_can_be_dragged_too() {
        let mut model = two_and_one();
        let before = model.local.monitors[0].rect;
        drag_by(&mut model, PEER, r"\\.\DISPLAY1", (0.0, 600.0));
        assert_eq!(model.local.monitors[0].rect, before);
        assert_eq!(model.peer.as_ref().unwrap().monitors[0].rect.y, 600);
    }

    /// Dirty is about the arrangement changing, not about the mouse
    /// moving: a drag that ends where it started has changed nothing.
    #[test]
    fn a_drag_that_returns_to_where_it_started_leaves_the_scene_clean() {
        let mut model = two_and_one();
        assert!(!model.is_dirty());
        let target = key(LOCAL, r"\\.\DISPLAY1");
        model.begin_drag(&target, (0.0, 0.0), unit_viewport());
        model.drag_to((400.0, 400.0));
        model.drag_to((0.0, 0.0));
        model.end_drag();
        assert!(!model.is_dirty(), "nothing moved, so nothing to save");
        assert!(!model.can_save());
    }

    /// The drop is what commits: until then there is nothing to save.
    #[test]
    fn a_drop_commits_the_arrangement_and_marks_it_dirty() {
        let mut model = two_and_one();
        let target = key(LOCAL, r"\\.\DISPLAY1");
        model.begin_drag(&target, (0.0, 0.0), unit_viewport());
        model.drag_to((0.0, 900.0));
        assert!(!model.is_dirty(), "a drag in flight is not yet a change");
        assert!(model.drag().is_some());
        model.end_drag();
        assert!(model.is_dirty());
        assert!(model.drag().is_none());
        assert!(model.can_save(), "{:?}", model.diagnostics());
    }

    /// A monitor no group holds is a click that does nothing, not a panic.
    #[test]
    fn dragging_a_monitor_that_is_not_there_does_nothing() {
        let mut model = two_and_one();
        let before = model.local.monitors[0].rect;
        drag_by(&mut model, LOCAL, "NOT-A-MONITOR", (300.0, 300.0));
        assert_eq!(model.local.monitors[0].rect, before);
        assert!(!model.is_dirty());
    }

    #[test]
    fn the_pointer_finds_the_monitor_under_it() {
        let model = two_and_one();
        assert_eq!(
            model.monitor_at((10.0, 10.0)),
            Some(key(LOCAL, r"\\.\DISPLAY1"))
        );
        assert_eq!(
            model.monitor_at((2000.0, 10.0)),
            Some(key(LOCAL, r"\\.\DISPLAY2"))
        );
        assert_eq!(model.monitor_at((-5.0, -5.0)), None);
        // The shared column of two abutting monitors belongs to exactly
        // one of them (half-open), never both.
        assert_eq!(
            model.monitor_at((1920.0, 10.0)),
            Some(key(LOCAL, r"\\.\DISPLAY2"))
        );
    }

    /// The round trip the whole branch exists for: drag until the snap
    /// makes a seam, and the drawing is a [`Layout`] that validates clean
    /// — no blocking diagnostic and no disconnection warning.
    #[test]
    fn a_snapped_arrangement_becomes_a_layout_that_validates_clean() {
        let mut model = two_and_one();
        // The seed leaves `SEED_GROUP_GAP` (96) between the two groups, so
        // asking for 90 lands inside the snap threshold of the abutment.
        drag_by(&mut model, LOCAL, r"\\.\DISPLAY1", (90.0, 0.0));

        let layout = model.to_layout(9).expect("a snapped scene is a layout");
        assert_eq!(layout.revision(), 9);
        assert_eq!(layout.origin(), LOCAL);
        assert_eq!(layout.monitors().len(), 3);
        assert!(
            model.diagnostics().blocking.is_empty(),
            "{:?}",
            model.diagnostics()
        );
        assert!(
            model.diagnostics().warnings.is_empty(),
            "a snapped seam is a connected arrangement: {:?}",
            model.diagnostics()
        );
        // And the seam is exact, which is what the derivation requires.
        let local_right = model.local.monitors[1].rect.right();
        let peer_left = model.peer.as_ref().unwrap().monitors[0].rect.left();
        assert_eq!(local_right, peer_left, "the snap must abut exactly");
    }

    /// Overlap blocks the save and names both screens, so the renderer can
    /// outline them.
    #[test]
    fn an_overlapping_arrangement_blocks_the_save_and_names_the_offenders() {
        let mut model = two_and_one();
        drag_by(&mut model, LOCAL, r"\\.\DISPLAY1", (1_000.0, 0.0));

        let diagnostics = model.diagnostics();
        assert!(diagnostics.blocks_save(), "{diagnostics:?}");
        assert!(!model.can_save());
        let blocker = &diagnostics.blocking[0];
        assert!(blocker.message.contains("overlap"), "{blocker:?}");
        assert_eq!(blocker.offenders.len(), 2, "{blocker:?}");
        assert!(
            blocker
                .offenders
                .iter()
                .any(|key| diagnostics.offends(key.device, &key.id))
        );
        assert!(model.to_layout(1).is_err());
    }

    /// A floating machine is a legal drawing (ADR 0018: connectivity is
    /// not a rule), so it warns and still saves.
    #[test]
    fn machines_that_do_not_touch_warn_but_do_not_block() {
        let mut model = two_and_one();
        drag_by(&mut model, LOCAL, r"\\.\DISPLAY1", (0.0, 5_000.0));

        let diagnostics = model.diagnostics();
        assert!(diagnostics.blocking.is_empty(), "{diagnostics:?}");
        assert_eq!(diagnostics.warnings.len(), 1, "{diagnostics:?}");
        assert!(model.can_save(), "a floating machine is still savable");
        assert!(model.to_layout(1).is_ok());
    }

    /// A scene with no peer is not a layout at all, and says so as a
    /// blocking diagnostic rather than by producing an empty one.
    #[test]
    fn a_scene_with_no_peer_blocks_rather_than_saving_half_an_arrangement() {
        let mut document = state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![],
            None,
        );
        document.peer = None;
        let model = Model::from_state(&document);
        assert!(model.diagnostics().blocks_save());
        assert!(!model.can_save());
    }

    // Exact abutment itself — a gap is not an edge, an overlap is not an
    // edge, a corner is not an edge — is `LayoutRect::abuts`'s own test in
    // `crossover-topology`, now that the rule lives there rather than being
    // restated here. What this module tests is what it *does* with the
    // answer: the two diagnostics either side of `machines_touch`, above.

    /// The transplant's direction, stated as a unit: the **fresh** scene is
    /// what everything else comes from, and only the user's work moves onto
    /// it. A monitor whose *resolution* changed takes the fresh extent —
    /// that is news, not an edit to undo — while its position stays the
    /// user's.
    #[test]
    fn a_transplant_takes_positions_from_the_edit_and_everything_else_from_the_fresh_scene() {
        let mut edited = two_and_one();
        drag_by(&mut edited, LOCAL, r"\\.\DISPLAY1", (0.0, 700.0));
        let dragged_to = edited.local.monitors[0].rect.y;

        // The same desk, re-read, with the first screen at a new resolution
        // and the machine renamed.
        let mut document = state(
            vec![
                live(r"\\.\DISPLAY1", 0, 2560, 1440, 100),
                live(r"\\.\DISPLAY2", 2560, 1280, 1024, 100),
            ],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        );
        document.local.name = "renamed".to_owned();
        let mut fresh = Model::from_state(&document);
        fresh.transplant_from(&edited);

        assert_eq!(fresh.local.name, "renamed", "the worker's fact wins");
        assert_eq!(
            (
                fresh.local.monitors[0].rect.width,
                fresh.local.monitors[0].rect.height
            ),
            (2560, 1440),
            "the new resolution is the OS's fact, not an edit"
        );
        assert_eq!(
            fresh.local.monitors[0].rect.y, dragged_to,
            "but the position is still the user's"
        );
        assert!(fresh.is_dirty(), "and it is still unsaved");
    }

    /// A panel that measures itself *while the editor is open* — the
    /// worker learns a size a moment after it learns the monitor — must not
    /// resize the rectangle the user is arranging. Nothing about the screen
    /// changed; only what the worker knows about it did, and the drawing on
    /// screen is the user's work.
    #[test]
    fn a_physical_size_arriving_mid_edit_does_not_resize_the_users_rectangles() {
        // The peer measured itself from the start, so the local machine's
        // unmeasured rectangle is badged and has something to contrast
        // with — the badge is a statement about the scene, not the screen.
        let peer = vec![LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(286, 179).unwrap()),
            ..live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)
        }];
        let unmeasured = live(r"\\.\DISPLAY1", 0, 2560, 1440, 100);
        let mut edited = Model::from_state(&state(vec![unmeasured.clone()], peer.clone(), None));
        drag_by(&mut edited, LOCAL, r"\\.\DISPLAY1", (0.0, 900.0));
        let drawn = edited.local.monitors[0].rect;
        assert!(edited.local.monitors[0].size_estimated);

        // The next read of the state file, with the EDID now read.
        let measured = LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(597, 336).unwrap()),
            ..unmeasured.clone()
        };
        let mut fresh = Model::from_state(&state(vec![measured.clone()], peer.clone(), None));
        assert_ne!(
            fresh.local.monitors[0].rect.width, drawn.width,
            "the fixture must actually re-seed differently"
        );
        fresh.transplant_from(&edited);

        assert_eq!(
            fresh.local.monitors[0].rect, drawn,
            "the rectangle the user is arranging is theirs"
        );
        assert!(
            fresh.local.monitors[0].size_estimated,
            "and the badge describes the size actually drawn"
        );

        // And the same in reverse: a size that goes away — a re-enumeration
        // that failed to read the EDID this time — does not resize it back.
        let mut fresh = Model::from_state(&state(vec![unmeasured], peer.clone(), None));
        let mut measured_edit = Model::from_state(&state(vec![measured], peer, None));
        drag_by(&mut measured_edit, LOCAL, r"\\.\DISPLAY1", (0.0, 900.0));
        let measured_rect = measured_edit.local.monitors[0].rect;
        fresh.transplant_from(&measured_edit);
        assert_eq!(fresh.local.monitors[0].rect, measured_rect);
        assert!(!fresh.local.monitors[0].size_estimated);
    }

    /// A **DPI** change is news exactly as a resolution change is: the same
    /// pixels at a new scale are a differently-sized screen and seed a
    /// differently-sized rectangle, and a predicate that looked only at
    /// pixels would swallow it — leaving the editor showing a rectangle
    /// whose size describes a scale factor the machine no longer uses.
    /// TESTING.md's E-1 is a DPI check for exactly this reason.
    #[test]
    fn a_scale_change_alone_still_resizes_a_rectangle_being_edited() {
        let mut edited = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 3840, 2160, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        drag_by(&mut edited, LOCAL, r"\\.\DISPLAY1", (0.0, 900.0));
        assert_eq!(edited.local.monitors[0].rect.width, 3840);

        // The same screen, the same pixels, now at 200 %.
        let mut fresh = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 3840, 2160, 200)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        fresh.transplant_from(&edited);
        assert_eq!(
            (
                fresh.local.monitors[0].rect.width,
                fresh.local.monitors[0].rect.height
            ),
            (1920, 1080),
            "the new scale is the OS's fact, not an edit to undo"
        );
        assert_eq!(fresh.local.monitors[0].rect.y, 900, "still the user's");
    }

    /// A rectangle the *fresh* document places authoritatively is the
    /// user's own saved size, freshly read — so it wins over a seed the
    /// previous scene had computed, and it is never left carrying that
    /// seed's badge. Only the position transplants.
    #[test]
    fn an_authoritative_rectangle_keeps_its_saved_size_and_is_never_badged() {
        // A seeded scene the user has dragged, with the local rectangle
        // badged (the peer measured itself, so there is a contrast).
        let peer = vec![LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(286, 179).unwrap()),
            ..live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)
        }];
        let mut edited = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 2560, 1440, 100)],
            peer.clone(),
            None,
        ));
        drag_by(&mut edited, LOCAL, r"\\.\DISPLAY1", (0.0, 900.0));
        assert!(edited.local.monitors[0].size_estimated);
        assert_ne!(edited.local.monitors[0].rect.width, 1920);

        // The worker has since caught up with a saved arrangement, which
        // places that monitor at 1920 wide.
        let mut fresh = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 2560, 1440, 100)],
            peer,
            Some(side_by_side_layout()),
        ));
        fresh.transplant_from(&edited);

        let drawn = &fresh.local.monitors[0];
        assert!(drawn.authoritative);
        assert_eq!(drawn.rect.width, 1920, "the saved size is the user's own");
        assert!(
            !drawn.size_estimated,
            "and an authoritative size is no guess"
        );
        assert_eq!(drawn.rect.y, 900, "but where they dragged it stands");
    }

    /// The other half of the same rule, so the hold cannot quietly become
    /// "the size never changes": a screen that really did change
    /// resolution takes the fresh extent, mid-edit or not.
    #[test]
    fn a_resolution_change_still_resizes_a_rectangle_being_edited() {
        let mut edited = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        drag_by(&mut edited, LOCAL, r"\\.\DISPLAY1", (0.0, 900.0));

        let mut fresh = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 2560, 1440, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        fresh.transplant_from(&edited);
        assert_eq!(
            (
                fresh.local.monitors[0].rect.width,
                fresh.local.monitors[0].rect.height
            ),
            (2560, 1440)
        );
        assert_eq!(fresh.local.monitors[0].rect.y, 900, "still the user's");
    }

    /// A monitor the edit never saw arrives with the machine it belongs to,
    /// carrying the same translation as its siblings — so a display docked
    /// mid-edit does not land back at the seed's origin on top of them.
    #[test]
    fn a_transplant_carries_a_newly_docked_monitor_along_with_its_machine() {
        let mut edited = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        drag_by(&mut edited, LOCAL, r"\\.\DISPLAY1", (0.0, 900.0));

        let mut fresh = Model::from_state(&state(
            vec![
                live(r"\\.\DISPLAY1", 0, 1920, 1080, 100),
                live(r"\\.\DISPLAY2", 1920, 1280, 1024, 100),
            ],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        fresh.transplant_from(&edited);

        assert_eq!(fresh.local.monitors.len(), 2);
        let tops: Vec<i32> = fresh.local.monitors.iter().map(|m| m.rect.y).collect();
        assert_eq!(tops, vec![900, 900], "the whole machine moved together");
        // Within the machine, the seed's abutting packing survives the
        // translation, which is what keeps the new screen off its
        // siblings. Whether the now-larger machine runs into the *peer* is
        // a question about the arrangement, and validation answers it out
        // loud — this module does not quietly shift a machine the user
        // placed in order to avoid having to say so.
        assert!(
            !fresh.local.monitors[0]
                .rect
                .overlaps(fresh.local.monitors[1].rect),
            "{:?}",
            fresh.local.monitors
        );
    }

    /// A gesture cannot continue against a machine whose monitors changed
    /// underneath it, so it is **committed where it stands** rather than
    /// being silently undone — the arrangement the user had reached is
    /// still theirs, and still unsaved.
    #[test]
    fn a_drag_whose_machine_changed_underneath_it_is_committed_not_lost() {
        let mut edited = Model::from_state(&state(
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        edited.begin_drag(&key(LOCAL, r"\\.\DISPLAY1"), (10.0, 10.0), unit_viewport());
        edited.drag_to((10.0, 1_210.0));
        assert!(!edited.is_dirty(), "a drag in flight is not yet dirty");

        let mut fresh = Model::from_state(&state(
            vec![
                live(r"\\.\DISPLAY1", 0, 1920, 1080, 100),
                live(r"\\.\DISPLAY2", 1920, 1280, 1024, 100),
            ],
            vec![live(r"\\.\DISPLAY1", 0, 1920, 1080, 100)],
            None,
        ));
        fresh.transplant_from(&edited);

        assert!(fresh.drag().is_none(), "the gesture cannot continue");
        assert!(fresh.is_dirty(), "but what it reached is not thrown away");
        assert_eq!(fresh.local.monitors[0].rect.y, 1_200);
    }

    /// `seen_revision` never goes backwards across a transplant. It is the
    /// floor the next save numbers itself past, and a save numbered below
    /// the one it replaces is silently superseded (ADR 0018).
    #[test]
    fn a_transplant_keeps_the_highest_revision_either_scene_has_seen() {
        let mut edited = two_and_one();
        drag_by(&mut edited, LOCAL, r"\\.\DISPLAY1", (0.0, 700.0));
        edited.mark_saved(11);

        let mut fresh = two_and_one();
        assert_eq!(fresh.seen_revision, 0, "the seed has seen nothing");
        fresh.transplant_from(&edited);
        assert_eq!(fresh.seen_revision, 11);
    }

    /// A drag that reaches the coordinate ceiling slides the whole machine
    /// to a stop rather than deforming it: the clamp is one answer for the
    /// group (ADR 0018's rigidity), so the internal seams are untouched.
    #[test]
    fn a_drag_past_the_coordinate_ceiling_stops_rigidly() {
        let mut model = two_and_one();
        let before: Vec<LayoutRect> = model.local.monitors.iter().map(|m| m.rect).collect();
        let limit = f64::from(crossover_topology::MAX_LAYOUT_COORDINATE);
        drag_by(&mut model, LOCAL, r"\\.\DISPLAY1", (limit * 4.0, 0.0));

        let after: Vec<LayoutRect> = model.local.monitors.iter().map(|m| m.rect).collect();
        let offsets = |rects: &[LayoutRect]| -> Vec<i64> {
            rects.iter().map(|r| r.left() - rects[0].left()).collect()
        };
        assert_eq!(offsets(&before), offsets(&after), "the group deformed");
        for rect in &after {
            assert!(
                rect.left().abs() <= i64::from(crossover_topology::MAX_LAYOUT_COORDINATE),
                "{rect:?} left the coordinate space"
            );
        }
    }

    #[test]
    fn saving_clears_the_dirty_flag_and_records_the_revision() {
        let mut model = two_and_one();
        drag_by(&mut model, LOCAL, r"\\.\DISPLAY1", (90.0, 0.0));
        assert!(model.is_dirty());
        model.mark_saved(12);
        assert!(!model.is_dirty());
        assert!(!model.can_save());
        assert_eq!(model.seen_revision, 12);
    }

    proptest! {
        /// Rigidity under any drag at all: whatever the pointer does, the
        /// dragged machine's rectangles keep exactly the relative
        /// positions and sizes the OS gave them.
        #[test]
        fn a_dragged_group_keeps_its_internal_geometry(
            dx in -20_000.0f64..20_000.0,
            dy in -20_000.0f64..20_000.0,
            steps in proptest::collection::vec(-2_000.0f64..2_000.0, 0..4),
        ) {
            let mut model = two_and_one();
            let before: Vec<LayoutRect> =
                model.local.monitors.iter().map(|m| m.rect).collect();
            let offsets: Vec<(i64, i64)> = before
                .iter()
                .map(|rect| (rect.left() - before[0].left(), rect.top() - before[0].top()))
                .collect();

            let target = key(LOCAL, r"\\.\DISPLAY1");
            model.begin_drag(&target, (0.0, 0.0), unit_viewport());
            for step in steps {
                model.drag_to((step, -step));
            }
            model.drag_to((dx, dy));
            model.end_drag();

            let after: Vec<LayoutRect> = model.local.monitors.iter().map(|m| m.rect).collect();
            for (index, rect) in after.iter().enumerate() {
                prop_assert_eq!(
                    (rect.left() - after[0].left(), rect.top() - after[0].top()),
                    offsets[index],
                    "the group deformed: {:?}",
                    after
                );
                prop_assert_eq!((rect.width, rect.height), (before[index].width, before[index].height));
            }
        }

        /// Every seed this module produces is internally non-overlapping —
        /// within each machine's own group (abutting, never overlapping by
        /// construction) and between the two groups (the gap) — for any
        /// combination of monitor counts, sizes, positions, scales, and
        /// physical measurements the state-file decoder could have
        /// admitted. Measured, unmeasured, and mixed desks all included:
        /// what a rectangle's *width* came from must not be able to make
        /// two of them collide.
        #[test]
        fn seeded_arrangements_never_overlap(
            local in any_machine("L"),
            peer in any_machine("P"),
        ) {
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

        /// The abutment invariant, which is the half of the seed that the
        /// crossing mapping reads: consecutive monitors of one machine
        /// touch **exactly**, whatever decided their widths. The layout
        /// model's abutment test has zero tolerance (ADR 0018), so a seam
        /// that is a unit out is not a seam at all.
        #[test]
        fn a_machines_seeded_monitors_abut_exactly(
            local in any_machine("L"),
            peer in any_machine("P"),
        ) {
            let scene = Model::from_state(&state(local, peer, None));
            for group in scene.groups() {
                for pair in group.monitors.windows(2) {
                    prop_assert_eq!(
                        pair[0].rect.right(),
                        pair[1].rect.left(),
                        "a seam opened between {:?} and {:?}",
                        pair[0].rect,
                        pair[1].rect
                    );
                    prop_assert_eq!(pair[0].rect.top(), pair[1].rect.top());
                }
            }
        }

        /// Determinism: one document seeds one arrangement, always. A seed
        /// that varied between two reads of the same file would move the
        /// drawing under the user on the editor's own one-second poll.
        #[test]
        fn seeding_the_same_document_twice_draws_the_same_thing(
            local in any_machine("L"),
            peer in any_machine("P"),
        ) {
            let document = state(local, peer, None);
            let first = Model::from_state(&document);
            let second = Model::from_state(&document);
            prop_assert_eq!(first.local.monitors, second.local.monitors);
            prop_assert_eq!(
                first.peer.map(|group| group.monitors),
                second.peer.map(|group| group.monitors)
            );
        }

        /// The behaviour-unchanged guarantee, at the level a user sees it:
        /// a desk where nothing measured itself — every monitor before this
        /// branch, and every monitor on a platform that cannot read an EDID
        /// after it — seeds precisely the rectangles it always did.
        #[test]
        fn a_scene_with_nothing_measured_seeds_exactly_as_it_did_before(
            local in any_machine("L"),
            peer in any_machine("P"),
        ) {
            let strip = |monitors: Vec<LiveMonitor>| -> Vec<LiveMonitor> {
                monitors
                    .into_iter()
                    .map(|monitor| LiveMonitor { physical_size: None, ..monitor })
                    .collect()
            };
            let local = strip(local);
            let peer = strip(peer);
            let scene = Model::from_state(&state(local.clone(), peer.clone(), None));

            for (group, live_monitors) in scene.groups().zip([&local, &peer]) {
                for drawn in &group.monitors {
                    let source = live_monitors
                        .iter()
                        .find(|candidate| candidate.id == drawn.id)
                        .expect("every drawn monitor came from a live one");
                    // The pre-sizes rule, restated here rather than
                    // borrowed, so the test would notice the production
                    // arithmetic changing under it.
                    let dip = |pixels: u32| {
                        ((u64::from(pixels) * 100 + u64::from(source.scale_percent) / 2)
                            / u64::from(source.scale_percent))
                            .max(1)
                    };
                    prop_assert_eq!(u64::from(drawn.rect.width), dip(source.rect.width));
                    prop_assert_eq!(u64::from(drawn.rect.height), dip(source.rect.height));
                }
            }
        }
    }

    /// One machine's worth of live monitors, each of which may or may not
    /// have measured itself — the mixed desk every seeding property has to
    /// hold over.
    fn any_machine(prefix: &'static str) -> impl Strategy<Value = Vec<LiveMonitor>> {
        proptest::collection::vec(
            (
                0i32..20_000,
                1u32..4_000,
                1u32..4_000,
                25u16..=500,
                proptest::option::of((50u16..=3_000, 50u16..=3_000)),
            ),
            1..6,
        )
        .prop_map(move |rows| {
            rows.into_iter()
                .enumerate()
                .map(|(index, (x, width, height, scale, physical))| LiveMonitor {
                    physical_size: physical.map(|(width_mm, height_mm)| {
                        PhysicalSizeMm::new(width_mm, height_mm).expect("in bounds by construction")
                    }),
                    ..live(&format!("{prefix}{index}"), x, width, height, scale)
                })
                .collect()
        })
    }
}
