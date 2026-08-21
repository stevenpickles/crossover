//! The drawn layout: placed monitors in one shared coordinate space, and
//! the validation that decides whether an arrangement is believable
//! (ADR 0018).
//!
//! A layout is a set of rectangles, each carrying the machine it belongs to
//! and the monitor it is, positioned in **unit-agnostic integers**. Nothing
//! here treats a coordinate as a pixel and no scale factor enters: every
//! cross-machine mapping is proportional, through fractions of drawn edges,
//! which is what makes the mixed-DPI requirement (R-3) hold by construction
//! rather than by care. Intra-machine geometry stays the OS's — the layout
//! answers exactly one question, *which peer monitor lies across which of
//! my edges, and where along it*.
//!
//! # This is peer-influenced local state
//!
//! A layout arrives from the peer as a `LayoutSync`, and it decides where
//! this machine hands control away. So it gets network input's treatment
//! (NFR-1, docs/SECURITY.md invariant 5 and T23): every bound is a named
//! constant, the counts are checked **before anything is allocated**, all
//! arithmetic runs in `i64` where the bounds make overflow impossible
//! rather than improbable, and malformed input produces a value — never a
//! panic. A [`Layout`] exists only if it passed [`Layout::new`], which is
//! why its fields are private.
//!
//! # Connectivity is not a rule
//!
//! A monitor parked with nothing abutting it is a legal drawing that
//! produces no crossings on its free edges. That is an observable property
//! of the arrangement the user drew, and never a validation error:
//! refusing it would turn a deliberate choice — "this screen is not a
//! crossing surface" — into a failure the user cannot act on.
//!
//! Abutment, when it is asked about, is **exact**. Snapping is the editor's
//! job, where the user can see it happen; a tolerance here would make "is
//! this an edge" a fuzzy question at exactly the place where a wrong answer
//! hands control to the other machine.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::device::DeviceId;
use crate::monitor::{MAX_MONITOR_ID_BYTES, MonitorId, MonitorIdError};

/// Most monitors one machine may contribute to a layout (ADR 0018).
///
/// Generous over any real desk, and small enough to keep the O(n²) overlap
/// and adjacency work trivially cheap. A machine that genuinely enumerates
/// more does not truncate — truncating would describe a desk with screens
/// missing — it refuses to publish a topology and says so.
pub const MAX_MONITORS_PER_MACHINE: usize = 16;

/// Most monitors a whole layout may hold (ADR 0018). A layout describes
/// exactly two machines, so this is twice [`MAX_MONITORS_PER_MACHINE`].
pub const MAX_LAYOUT_MONITORS: usize = 32;

/// Largest width or height a placed monitor may have (ADR 0018). The
/// minimum is 1: a zero-sized monitor has no edge to cross.
pub const MAX_MONITOR_EXTENT: u32 = 65_535;

/// Largest absolute value of a placed monitor's `x` or `y` (ADR 0018).
///
/// Unreachable by any legitimate arrangement — 32 monitors of maximal
/// extent laid end to end span under `2^21` — and chosen so the overflow
/// argument is trivial rather than delicate: an edge coordinate is at most
/// `2^24 + 2^16 < 2^25`, a span length at most `2^26`, and the widest
/// intermediate (a span offset scaled by `u16::MAX` before division) is
/// under `2^42`, six orders of magnitude inside `i64`.
pub const MAX_LAYOUT_COORDINATE: i32 = 1 << 24;

/// Smallest display scale a monitor may report, in percent (ADR 0018).
///
/// A **seeding input** for the editor's to-scale drawing only: it lets the
/// editor size a rectangle in DIPs so two physically-equal monitors draw
/// equal. It never enters crossing mapping, which stays proportional
/// through the drawn geometry.
pub const MIN_SCALE_PERCENT: u16 = 25;

/// Largest display scale a monitor may report, in percent (ADR 0018).
/// 100 is unscaled. See [`MIN_SCALE_PERCENT`] for what this is *not*.
pub const MAX_SCALE_PERCENT: u16 = 500;

/// A rectangle in the shared, unit-agnostic layout space (ADR 0018).
///
/// `x`/`y` are the top-left corner; `width`/`height` are extents, not far
/// corners, so a rectangle is never accidentally inverted. Bounds are
/// checked when the rectangle joins a [`Layout`], not here, because a bare
/// rectangle is a value and a layout is what has to be believable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutRect {
    /// Rightward offset of the left edge from the shared origin.
    pub x: i32,
    /// Downward offset of the top edge from the shared origin.
    pub y: i32,
    /// Extent along the horizontal axis, `1..=MAX_MONITOR_EXTENT`.
    pub width: u32,
    /// Extent along the vertical axis, `1..=MAX_MONITOR_EXTENT`.
    pub height: u32,
}

impl LayoutRect {
    /// The left edge's coordinate. In `i64`, as every derivation is.
    #[must_use]
    pub fn left(self) -> i64 {
        i64::from(self.x)
    }

    /// The top edge's coordinate.
    #[must_use]
    pub fn top(self) -> i64 {
        i64::from(self.y)
    }

    /// The right edge's coordinate — exclusive, so `left..right` is the
    /// half-open interval the rectangle occupies.
    ///
    /// Ordinary `+`, not saturating: inside a [`Layout`] the bounds make
    /// this at most `2^24 + 2^16`, and `i64` addition at that size cannot
    /// overflow. On a bare rectangle the widths are still `u32` and the
    /// coordinates `i32`, so the sum is inside `i64` for every possible
    /// input — there is no value that could overflow it.
    #[must_use]
    pub fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    /// The bottom edge's coordinate, exclusive.
    #[must_use]
    pub fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    /// Do these two rectangles share any **positive area**?
    ///
    /// Touching is not overlapping: two rectangles that abut exactly share
    /// a zero-width sliver, which is the whole basis of adjacency. The
    /// comparison is strict (`<`) for exactly that reason.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        self.left().max(other.left()) < self.right().min(other.right())
            && self.top().max(other.top()) < self.bottom().min(other.bottom())
    }

    /// Do these two rectangles **share an edge**, with a perpendicular
    /// overlap of positive length?
    ///
    /// The exact complement of [`Self::overlaps`], and the shape of ADR
    /// 0018's zero-tolerance adjacency: an edge coordinate is *identical*
    /// and the two rectangles run alongside each other for a positive
    /// distance. A one-unit gap is not an edge, an overlap is not an edge,
    /// and a corner touch alone is not an edge — a cursor cannot cross a
    /// point.
    ///
    /// This is the *predicate*, kept beside `overlaps` because the two are
    /// one question asked twice and drifting them apart would be a
    /// crossing the editor promises and the worker does not make. It is
    /// **not** the crossing derivation: `crossover-core`'s `crossing.rs`
    /// owns that — the spans, the sides, and which monitor a cursor lands
    /// on — and remains the authority on what an adjacency *means*. A
    /// later sweep is expected to have that derivation compute its
    /// adjacency through here rather than restating it.
    #[must_use]
    pub fn abuts(self, other: Self) -> bool {
        let vertical_overlap = self.top().max(other.top()) < self.bottom().min(other.bottom());
        let horizontal_overlap = self.left().max(other.left()) < self.right().min(other.right());
        let side_by_side = self.right() == other.left() || other.right() == self.left();
        let stacked = self.bottom() == other.top() || other.bottom() == self.top();
        (side_by_side && vertical_overlap) || (stacked && horizontal_overlap)
    }
}

/// Which monitor, on which machine — the pair that identifies a placed
/// rectangle, and what a diagnostic names when it has something to say
/// about one (ADR 0018: diagnostics name monitor ids).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonitorKey {
    /// The machine the monitor is attached to.
    pub device: DeviceId,
    /// The monitor's platform-supplied identity.
    pub id: MonitorId,
}

impl fmt::Display for MonitorKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} on {}", self.id, self.device)
    }
}

/// One monitor, placed (ADR 0018).
///
/// Decoding one checks what a single monitor can be checked against on its
/// own: its id validates as a [`MonitorId`], and its rectangle satisfies
/// [`LayoutRect::check_bounds`]. Everything that is a property of the
/// *set* — uniqueness, the counts, both machines present, no overlap —
/// needs the whole arrangement and is [`Layout::new`]'s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawDecodedMonitor")]
pub struct PlacedMonitor {
    /// The machine this screen is attached to.
    pub device: DeviceId,
    /// Its platform-supplied identity, unique within that machine.
    pub id: MonitorId,
    /// Where the user drew it.
    pub rect: LayoutRect,
}

/// [`PlacedMonitor`] before its rectangle has been checked — the shape
/// serde builds, which [`TryFrom`] then admits or refuses.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDecodedMonitor {
    device: DeviceId,
    id: MonitorId,
    rect: LayoutRect,
}

impl TryFrom<RawDecodedMonitor> for PlacedMonitor {
    type Error = String;

    fn try_from(raw: RawDecodedMonitor) -> Result<Self, Self::Error> {
        raw.rect
            .check_bounds()
            .map_err(|violation| format!("monitor {} on {}: {violation}", raw.id, raw.device))?;
        Ok(Self {
            device: raw.device,
            id: raw.id,
            rect: raw.rect,
        })
    }
}

impl PlacedMonitor {
    /// Which monitor this is, for a diagnostic.
    #[must_use]
    pub fn key(&self) -> MonitorKey {
        MonitorKey {
            device: self.device,
            id: self.id.clone(),
        }
    }
}

/// A placed monitor as it arrives from a file or the wire, before its id
/// has been validated.
///
/// The separate type is what lets an unusable id be reported as a
/// [`LayoutError`] naming the machine it came from, rather than as a
/// format-level parse failure that says only that some string somewhere was
/// wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPlacedMonitor {
    /// The machine this screen is claimed to be attached to.
    pub device: DeviceId,
    /// Its identity, still unvalidated.
    pub id: String,
    /// Where it is claimed to sit.
    pub rect: LayoutRect,
}

/// The two machines a layout is allowed to describe: this session's pair.
///
/// Carried as a value rather than assumed, because "which two machines" is
/// the check that turns a well-formed layout into a believable one — the
/// residue of a re-pair is a layout full of rectangles belonging to a
/// machine that is no longer at the other end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePair([DeviceId; 2]);

impl DevicePair {
    /// The pair `local` and `peer` form.
    ///
    /// # Errors
    ///
    /// [`LayoutError::DegeneratePair`] if the two are the same device — a
    /// session with itself, which no layout can describe.
    pub fn new(local: DeviceId, peer: DeviceId) -> Result<Self, LayoutError> {
        if local == peer {
            return Err(LayoutError::DegeneratePair { device: local });
        }
        Ok(Self([local, peer]))
    }

    /// Both devices, in the order the pair was built.
    #[must_use]
    pub const fn devices(&self) -> &[DeviceId; 2] {
        &self.0
    }

    /// Is `device` one of the two?
    #[must_use]
    pub fn contains(&self, device: DeviceId) -> bool {
        self.0[0] == device || self.0[1] == device
    }

    /// The other end.
    #[must_use]
    pub fn other(&self, device: DeviceId) -> Option<DeviceId> {
        if device == self.0[0] {
            Some(self.0[1])
        } else if device == self.0[1] {
            Some(self.0[0])
        } else {
            None
        }
    }
}

/// Why an arrangement was refused (ADR 0018).
///
/// One variant per rule, so a diagnostic can say which rule and about which
/// monitor, and so a test can assert the exact rejection reason rather than
/// "it failed" (docs/ARCHITECTURE.md §9).
///
/// A monitor id *is* named here — that is deliberate, and the opposite of
/// the file-name rule in `crossover-protocol`. A device string is not user
/// content: ADR 0018 records that anyone diagnosing a crossing now reads an
/// arrangement rather than a flag, "which is why diagnostics name monitor
/// ids rather than describing edges in prose". An id that failed *its own*
/// validation is still never quoted, because an unprintable id in a log
/// line is the thing the validation exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// A pair of one machine. Not an arrangement of anything.
    #[error("a layout describes two machines, and both ends are {device}")]
    DegeneratePair {
        /// The device given as both ends.
        device: DeviceId,
    },
    /// No monitors at all. "No layout" is a state of its own — seamless
    /// transfer off — and is expressed by having no layout, not by an
    /// empty one.
    #[error("a layout has at least one monitor")]
    NoMonitors,
    /// Past [`MAX_LAYOUT_MONITORS`]. Checked before anything is allocated.
    #[error("the layout has {count} monitors, over the {MAX_LAYOUT_MONITORS} maximum")]
    TooManyMonitors {
        /// How many were offered.
        count: usize,
    },
    /// One machine past [`MAX_MONITORS_PER_MACHINE`].
    #[error(
        "machine {device} has {count} monitors, over the {MAX_MONITORS_PER_MACHINE} per-machine maximum"
    )]
    TooManyMonitorsForMachine {
        /// Which machine.
        device: DeviceId,
        /// How many it claimed.
        count: usize,
    },
    /// A monitor id that is not one. The cause names the fault; the id
    /// itself is not quoted, because it failed printability.
    #[error("machine {device} offered an unusable monitor id")]
    InvalidMonitorId {
        /// Which machine offered it.
        device: DeviceId,
        /// Which rule it broke.
        #[source]
        source: MonitorIdError,
    },
    /// The same id twice on one machine. Ids are unique within a machine,
    /// which is what makes matching a live monitor to a drawn one
    /// unambiguous.
    #[error("machine {} lists monitor {} twice", monitor.device, monitor.id)]
    DuplicateMonitorId {
        /// The repeated monitor.
        monitor: MonitorKey,
    },
    /// A width or height of zero: a monitor with no edge to cross.
    #[error("monitor {monitor} has a zero width or height")]
    ZeroExtent {
        /// Which monitor.
        monitor: MonitorKey,
    },
    /// A width or height past [`MAX_MONITOR_EXTENT`].
    #[error("monitor {monitor} has an extent of {extent}, over the {MAX_MONITOR_EXTENT} maximum")]
    ExtentTooLarge {
        /// Which monitor.
        monitor: MonitorKey,
        /// The offending extent.
        extent: u32,
    },
    /// An `x` or `y` past ±[`MAX_LAYOUT_COORDINATE`], which is what keeps
    /// every later derivation provably inside `i64`.
    #[error(
        "monitor {monitor} sits at {coordinate}, outside ±{MAX_LAYOUT_COORDINATE} of the origin"
    )]
    CoordinateOutOfRange {
        /// Which monitor.
        monitor: MonitorKey,
        /// The offending coordinate.
        coordinate: i32,
    },
    /// A device that is neither end of this session — the residue of a
    /// re-pair, or a peer describing somebody else's desk.
    #[error("the layout names machine {device}, which is not this session's pair")]
    UnexpectedDevice {
        /// The unexpected machine.
        device: DeviceId,
    },
    /// The layout's `origin` is not one of the pair. The origin is the
    /// editing device, and on a two-machine pair the editor is at one of
    /// the two desks; anything else is a layout naming a third device,
    /// which ADR 0018 rejects however the name arrives.
    #[error("the layout was edited by {device}, which is not this session's pair")]
    UnexpectedOrigin {
        /// The unexpected origin.
        device: DeviceId,
    },
    /// One of the pair contributed no monitors. A layout describes *both*
    /// machines: an arrangement naming only one has nothing to cross to.
    #[error("the layout places no monitor for machine {device}")]
    MissingMachine {
        /// The machine with nothing drawn.
        device: DeviceId,
    },
    /// Two rectangles sharing positive area. A cursor in the shared
    /// overlap has no single answer for which monitor it left.
    #[error("monitors {first} and {second} overlap")]
    Overlap {
        /// One of the two.
        first: MonitorKey,
        /// The other.
        second: MonitorKey,
    },
}

/// A validated arrangement: revision, the device that drew it, and the
/// placed monitors (ADR 0018).
///
/// Constructible only through [`Layout::new`] or [`Layout::from_raw`], so
/// holding one is proof that every rule above holds. `revision` and
/// `origin` together are the ordering key convergence uses — newest
/// revision wins, `origin` breaking a tie as its 16 raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    revision: u64,
    origin: DeviceId,
    monitors: Vec<PlacedMonitor>,
}

impl Layout {
    /// Validate `monitors` as an arrangement of `pair`, drawn by `origin`
    /// at `revision`.
    ///
    /// Checks run cheapest-and-most-bounding first: [`check_structure`] —
    /// the counts, each monitor's own rectangle, per-machine counts, and id
    /// uniqueness — then the two checks that need to know *this session's*
    /// pair specifically (device/origin membership, both machines present),
    /// then the O(n²) overlap sweep last, over a set already bounded at
    /// [`MAX_LAYOUT_MONITORS`].
    ///
    /// # Errors
    ///
    /// [`LayoutError`], one variant per rule, naming the monitor or the
    /// machine at fault.
    pub fn new(
        revision: u64,
        origin: DeviceId,
        monitors: Vec<PlacedMonitor>,
        pair: &DevicePair,
    ) -> Result<Self, LayoutError> {
        check_structure(&monitors)?;

        if !pair.contains(origin) {
            return Err(LayoutError::UnexpectedOrigin { device: origin });
        }
        for monitor in &monitors {
            if !pair.contains(monitor.device) {
                return Err(LayoutError::UnexpectedDevice {
                    device: monitor.device,
                });
            }
        }
        for device in *pair.devices() {
            if monitors.iter().all(|m| m.device != device) {
                return Err(LayoutError::MissingMachine { device });
            }
        }

        // O(n²) over at most 32 rectangles: 496 comparisons, each a
        // handful of `i64` operations. Same-machine pairs are compared
        // too — ADR 0018 forbids overlap outright, not merely across
        // machines, because an overlap has no meaningful adjacency
        // whoever owns the two screens.
        for (index, first) in monitors.iter().enumerate() {
            for second in &monitors[index + 1..] {
                if first.rect.overlaps(second.rect) {
                    return Err(LayoutError::Overlap {
                        first: first.key(),
                        second: second.key(),
                    });
                }
            }
        }

        Ok(Self {
            revision,
            origin,
            monitors,
        })
    }

    /// Validate an arrangement whose monitor ids are still bare strings —
    /// the shape a config file or a wire message hands over.
    ///
    /// The counts are checked **before** any id is validated or any
    /// [`MonitorId`] allocated, so an oversized set costs a length
    /// comparison rather than a walk.
    ///
    /// # Errors
    ///
    /// [`LayoutError`], including [`LayoutError::InvalidMonitorId`] for an
    /// id that is not one.
    pub fn from_raw(
        revision: u64,
        origin: DeviceId,
        monitors: Vec<RawPlacedMonitor>,
        pair: &DevicePair,
    ) -> Result<Self, LayoutError> {
        if monitors.is_empty() {
            return Err(LayoutError::NoMonitors);
        }
        if monitors.len() > MAX_LAYOUT_MONITORS {
            return Err(LayoutError::TooManyMonitors {
                count: monitors.len(),
            });
        }
        let mut placed = Vec::with_capacity(monitors.len());
        for monitor in monitors {
            let id =
                MonitorId::new(&monitor.id).map_err(|source| LayoutError::InvalidMonitorId {
                    device: monitor.device,
                    source,
                })?;
            placed.push(PlacedMonitor {
                device: monitor.device,
                id,
                rect: monitor.rect,
            });
        }
        Self::new(revision, origin, placed, pair)
    }

    /// The arrangement's revision. Newest wins (ADR 0018).
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The device that drew it — the tiebreak when two edits claim the
    /// same revision.
    #[must_use]
    pub const fn origin(&self) -> DeviceId {
        self.origin
    }

    /// Every placed monitor, in the order the arrangement was given in.
    #[must_use]
    pub fn monitors(&self) -> &[PlacedMonitor] {
        &self.monitors
    }

    /// The monitors one machine contributed.
    pub fn monitors_for(&self, device: DeviceId) -> impl Iterator<Item = &PlacedMonitor> {
        self.monitors.iter().filter(move |m| m.device == device)
    }

    /// The monitor `device` calls `id`, if the arrangement places one.
    #[must_use]
    pub fn find(&self, device: DeviceId, id: &MonitorId) -> Option<&PlacedMonitor> {
        self.monitors
            .iter()
            .find(|m| m.device == device && &m.id == id)
    }
}

/// Which rectangle rule a [`LayoutRect`] broke, before anything has said
/// *whose* rectangle it was.
///
/// Split out from [`LayoutError`] because the same three rules are checked
/// in two places that name the offender differently: inside a layout, where
/// the monitor is known and the diagnostic says so, and at the state file's
/// decoder, where the answer is a serde message. One implementation, so the
/// two cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RectBoundViolation {
    /// A width or height of zero: a monitor with no edge to cross.
    #[error("a monitor has a zero width or height")]
    ZeroExtent,
    /// A width or height past [`MAX_MONITOR_EXTENT`].
    #[error("an extent of {extent} is over the {MAX_MONITOR_EXTENT} maximum")]
    ExtentTooLarge {
        /// The offending extent.
        extent: u32,
    },
    /// An `x` or `y` past ±[`MAX_LAYOUT_COORDINATE`].
    #[error("a coordinate of {coordinate} is outside ±{MAX_LAYOUT_COORDINATE} of the origin")]
    CoordinateOutOfRange {
        /// The offending coordinate.
        coordinate: i32,
    },
}

impl LayoutRect {
    /// The per-rectangle rules, in bound order (ADR 0018).
    ///
    /// # Errors
    ///
    /// [`RectBoundViolation`], naming the rule and the offending number.
    pub fn check_bounds(self) -> Result<(), RectBoundViolation> {
        if self.width == 0 || self.height == 0 {
            return Err(RectBoundViolation::ZeroExtent);
        }
        for extent in [self.width, self.height] {
            if extent > MAX_MONITOR_EXTENT {
                return Err(RectBoundViolation::ExtentTooLarge { extent });
            }
        }
        for coordinate in [self.x, self.y] {
            // `unsigned_abs` rather than `abs`: `i32::MIN` has no positive
            // counterpart, and a negated overflow is exactly the panic an
            // arrangement from the network must not be able to cause.
            if coordinate.unsigned_abs() > MAX_LAYOUT_COORDINATE.unsigned_abs() {
                return Err(RectBoundViolation::CoordinateOutOfRange { coordinate });
            }
        }
        Ok(())
    }
}

/// The per-monitor rectangle rules, attributed to the monitor that broke
/// them.
fn check_rect(monitor: &PlacedMonitor) -> Result<(), LayoutError> {
    monitor
        .rect
        .check_bounds()
        .map_err(|violation| match violation {
            RectBoundViolation::ZeroExtent => LayoutError::ZeroExtent {
                monitor: monitor.key(),
            },
            RectBoundViolation::ExtentTooLarge { extent } => LayoutError::ExtentTooLarge {
                monitor: monitor.key(),
                extent,
            },
            RectBoundViolation::CoordinateOutOfRange { coordinate } => {
                LayoutError::CoordinateOutOfRange {
                    monitor: monitor.key(),
                    coordinate,
                }
            }
        })
}

/// The structural rules a set of placed monitors must satisfy **on their
/// own**, without knowing which two devices a session's pair names (ADR
/// 0018): the count is `1..=`[`MAX_LAYOUT_MONITORS`], every rectangle
/// satisfies [`LayoutRect::check_bounds`], no device contributes more than
/// [`MAX_MONITORS_PER_MACHINE`] monitors, and no device repeats a monitor
/// id.
///
/// This is the one home for that logic, checked in bound order (cheapest
/// and most-bounding first) and shared by two callers that would otherwise
/// each carry their own copy: [`Layout::new`]/[`Layout::from_raw`] here,
/// and `crossover-protocol`'s wire-level validation of `MonitorTopology`
/// and `LayoutSync` (docs/PROTOCOL.md §6.2), which maps each [`LayoutError`]
/// onto its own error type.
///
/// Deliberately **not** checked here, because both need the caller's own
/// context to mean anything: session-pair membership (`UnexpectedDevice`,
/// `UnexpectedOrigin`) — this function does not know which devices *should*
/// appear, only how many may share the list — "both machines present"
/// (`MissingMachine`), which needs the pair's two identities to ask the
/// question of; and overlap, which is a property of the whole arrangement
/// compared against itself, not of one monitor's own structure.
///
/// # Errors
///
/// [`LayoutError`], one variant per rule, naming the monitor or the
/// machine at fault.
pub fn check_structure(monitors: &[PlacedMonitor]) -> Result<(), LayoutError> {
    // Counts first: everything below walks the set, and this is what says
    // how far that walk can go.
    if monitors.is_empty() {
        return Err(LayoutError::NoMonitors);
    }
    if monitors.len() > MAX_LAYOUT_MONITORS {
        return Err(LayoutError::TooManyMonitors {
            count: monitors.len(),
        });
    }

    for monitor in monitors {
        check_rect(monitor)?;
    }

    // Per-device count and id uniqueness, grouped over whichever devices
    // are actually present — not assumed to be any particular pair, since
    // this function does not know one.
    let mut devices: Vec<DeviceId> = Vec::new();
    for monitor in monitors {
        if !devices.contains(&monitor.device) {
            devices.push(monitor.device);
        }
    }
    for device in devices {
        let mut ids: Vec<&MonitorId> = Vec::new();
        for monitor in monitors.iter().filter(|m| m.device == device) {
            if ids.len() == MAX_MONITORS_PER_MACHINE {
                // Report the true count, not the cap: a diagnostic that
                // says "16 of 16" tells the user nothing about how far
                // over the desk actually is.
                return Err(LayoutError::TooManyMonitorsForMachine {
                    device,
                    count: monitors.iter().filter(|m| m.device == device).count(),
                });
            }
            if ids.contains(&&monitor.id) {
                return Err(LayoutError::DuplicateMonitorId {
                    monitor: monitor.key(),
                });
            }
            ids.push(&monitor.id);
        }
    }

    Ok(())
}

/// A compile-time restatement of the bounds the overflow argument rests
/// on, so a later edit to one constant cannot quietly invalidate it.
const _: () = {
    assert!(MAX_LAYOUT_MONITORS == 2 * MAX_MONITORS_PER_MACHINE);
    assert!(MAX_MONITOR_EXTENT < (1u32 << 17));
    assert!(MAX_LAYOUT_COORDINATE > 0 && MAX_LAYOUT_COORDINATE < (1i32 << 25));
    assert!(MAX_MONITOR_ID_BYTES >= 32);
    assert!(MIN_SCALE_PERCENT < MAX_SCALE_PERCENT);
};

#[cfg(test)]
pub(crate) mod tests {
    use proptest::prelude::*;

    use super::{
        DevicePair, Layout, LayoutError, LayoutRect, MAX_LAYOUT_COORDINATE, MAX_LAYOUT_MONITORS,
        MAX_MONITOR_EXTENT, MAX_MONITORS_PER_MACHINE, MonitorKey, PlacedMonitor, RawPlacedMonitor,
        check_structure,
    };
    use crate::device::DeviceId;
    use crate::monitor::{MAX_MONITOR_ID_BYTES, MonitorId, MonitorIdError};

    pub(crate) const LOCAL: DeviceId = DeviceId::from_bytes([0x11; 16]);
    pub(crate) const PEER: DeviceId = DeviceId::from_bytes([0x22; 16]);
    const STRANGER: DeviceId = DeviceId::from_bytes([0x33; 16]);

    pub(crate) fn pair() -> DevicePair {
        DevicePair::new(LOCAL, PEER).unwrap()
    }

    pub(crate) fn monitor(
        device: DeviceId,
        id: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> PlacedMonitor {
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

    /// Two machines side by side: the ordinary desk, and the arrangement
    /// most tests below perturb one rule at a time.
    pub(crate) fn side_by_side() -> Vec<PlacedMonitor> {
        vec![
            monitor(LOCAL, r"\\.\DISPLAY1", 0, 0, 1920, 1080),
            monitor(PEER, r"\\.\DISPLAY1", 1920, 0, 1920, 1080),
        ]
    }

    pub(crate) fn valid_layout() -> Layout {
        Layout::new(7, LOCAL, side_by_side(), &pair()).unwrap()
    }

    fn key(device: DeviceId, id: &str) -> MonitorKey {
        MonitorKey {
            device,
            id: MonitorId::new(id).unwrap(),
        }
    }

    /// [`check_structure`] needs no [`DevicePair`] at all: it is the same
    /// structural rules `Layout::new` runs, usable directly by a caller
    /// (`crossover-protocol`'s wire validation) that has not yet resolved
    /// which two devices a session's pair actually is.
    #[test]
    fn check_structure_needs_no_pair() {
        // A three-device list is fine structurally — "which devices"
        // is not this function's question — but a per-device rule (the
        // cap, a duplicate id) still fires without knowing the pair.
        check_structure(&[
            monitor(LOCAL, "A", 0, 0, 100, 100),
            monitor(PEER, "B", 200, 0, 100, 100),
            monitor(STRANGER, "C", 400, 0, 100, 100),
        ])
        .unwrap();

        assert_eq!(check_structure(&[]).unwrap_err(), LayoutError::NoMonitors);
        assert_eq!(
            check_structure(&[
                monitor(LOCAL, "A", 0, 0, 100, 100),
                monitor(LOCAL, "A", 200, 0, 100, 100),
            ])
            .unwrap_err(),
            LayoutError::DuplicateMonitorId {
                monitor: key(LOCAL, "A")
            }
        );
        assert_eq!(
            check_structure(&[monitor(LOCAL, "A", 0, 0, 0, 100)]).unwrap_err(),
            LayoutError::ZeroExtent {
                monitor: key(LOCAL, "A")
            }
        );
        // It does not check overlap, session-pair membership, or "both
        // machines present" — those are `Layout::new`'s, once it has a
        // `DevicePair` to check them against.
        check_structure(&[
            monitor(LOCAL, "A", 0, 0, 100, 100),
            monitor(LOCAL, "B", 50, 50, 100, 100),
        ])
        .unwrap();
    }

    #[test]
    fn an_ordinary_arrangement_is_accepted_and_readable() {
        let layout = valid_layout();
        assert_eq!(layout.revision(), 7);
        assert_eq!(layout.origin(), LOCAL);
        assert_eq!(layout.monitors().len(), 2);
        assert_eq!(layout.monitors_for(PEER).count(), 1);
        assert_eq!(
            layout
                .find(PEER, &MonitorId::new(r"\\.\DISPLAY1").unwrap())
                .unwrap()
                .rect
                .x,
            1920
        );
        assert!(
            layout
                .find(LOCAL, &MonitorId::new(r"\\.\DISPLAY9").unwrap())
                .is_none()
        );
    }

    /// Touching is not overlapping, over/under and offset arrangements are
    /// ordinary, and a floating monitor with nothing abutting it is a
    /// legal drawing — ADR 0018 refuses to make connectivity a rule.
    #[test]
    fn abutting_offset_and_floating_arrangements_are_all_legal() {
        // Exactly abutting on a vertical seam.
        Layout::new(0, LOCAL, side_by_side(), &pair()).unwrap();

        // Over/under, with a lateral offset.
        Layout::new(
            0,
            LOCAL,
            vec![
                monitor(LOCAL, "A", 0, 0, 1920, 1080),
                monitor(PEER, "B", 640, 1080, 1920, 1080),
            ],
            &pair(),
        )
        .unwrap();

        // A monitor parked with nothing abutting it, plus a three-way
        // corner: both are arrangements, not errors.
        Layout::new(
            0,
            LOCAL,
            vec![
                monitor(LOCAL, "A", 0, 0, 1000, 1000),
                monitor(LOCAL, "B", 0, 1000, 1000, 1000),
                monitor(PEER, "C", 1000, 500, 1000, 1000),
                monitor(PEER, "FLOATING", 50_000, 50_000, 800, 600),
            ],
            &pair(),
        )
        .unwrap();
    }

    #[test]
    fn a_pair_of_one_machine_is_refused() {
        assert_eq!(
            DevicePair::new(LOCAL, LOCAL).unwrap_err(),
            LayoutError::DegeneratePair { device: LOCAL }
        );
        let pair = pair();
        assert!(pair.contains(LOCAL) && pair.contains(PEER));
        assert!(!pair.contains(STRANGER));
        assert_eq!(pair.other(LOCAL), Some(PEER));
        assert_eq!(pair.other(STRANGER), None);
    }

    #[test]
    fn the_counts_are_refused_at_their_own_bounds() {
        assert_eq!(
            Layout::new(0, LOCAL, Vec::new(), &pair()).unwrap_err(),
            LayoutError::NoMonitors
        );

        // One past the whole-layout cap. Built as 17 + 16 so the
        // *layout* cap is what trips, ahead of the per-machine one.
        let mut too_many: Vec<PlacedMonitor> = (0..17)
            .map(|n| monitor(LOCAL, &format!("L{n}"), n * 100, 0, 80, 80))
            .collect();
        too_many.extend((0..MAX_MONITORS_PER_MACHINE).map(|n| {
            monitor(
                PEER,
                &format!("P{n}"),
                i32::try_from(n).unwrap() * 100,
                200,
                80,
                80,
            )
        }));
        assert_eq!(too_many.len(), MAX_LAYOUT_MONITORS + 1);
        assert_eq!(
            Layout::new(0, LOCAL, too_many, &pair()).unwrap_err(),
            LayoutError::TooManyMonitors {
                count: MAX_LAYOUT_MONITORS + 1
            }
        );

        // Exactly at the cap, split evenly, is fine.
        let mut at_the_cap: Vec<PlacedMonitor> = (0..MAX_MONITORS_PER_MACHINE)
            .map(|n| {
                monitor(
                    LOCAL,
                    &format!("L{n}"),
                    i32::try_from(n).unwrap() * 100,
                    0,
                    80,
                    80,
                )
            })
            .collect();
        at_the_cap.extend((0..MAX_MONITORS_PER_MACHINE).map(|n| {
            monitor(
                PEER,
                &format!("P{n}"),
                i32::try_from(n).unwrap() * 100,
                200,
                80,
                80,
            )
        }));
        assert_eq!(at_the_cap.len(), MAX_LAYOUT_MONITORS);
        Layout::new(0, LOCAL, at_the_cap, &pair()).unwrap();
    }

    #[test]
    fn one_machine_past_its_own_cap_is_refused() {
        let mut monitors: Vec<PlacedMonitor> = (0..=MAX_MONITORS_PER_MACHINE)
            .map(|n| {
                monitor(
                    LOCAL,
                    &format!("L{n}"),
                    i32::try_from(n).unwrap() * 100,
                    0,
                    80,
                    80,
                )
            })
            .collect();
        monitors.push(monitor(PEER, "P0", 0, 200, 80, 80));
        assert_eq!(
            Layout::new(0, LOCAL, monitors, &pair()).unwrap_err(),
            LayoutError::TooManyMonitorsForMachine {
                device: LOCAL,
                count: MAX_MONITORS_PER_MACHINE + 1
            }
        );
    }

    #[test]
    fn a_repeated_id_on_one_machine_is_refused_but_the_same_id_on_both_is_not() {
        let mut monitors = side_by_side();
        monitors.push(monitor(LOCAL, r"\\.\DISPLAY1", 0, 1080, 1920, 1080));
        assert_eq!(
            Layout::new(0, LOCAL, monitors, &pair()).unwrap_err(),
            LayoutError::DuplicateMonitorId {
                monitor: key(LOCAL, r"\\.\DISPLAY1")
            }
        );

        // Uniqueness is *within* a machine: both desks calling their first
        // screen `\\.\DISPLAY1` is the overwhelmingly common case.
        Layout::new(0, LOCAL, side_by_side(), &pair()).unwrap();
    }

    #[test]
    fn rectangle_bounds_are_refused_by_class() {
        for (width, height) in [(0, 1080), (1920, 0), (0, 0)] {
            let monitors = vec![
                monitor(LOCAL, "A", 0, 0, width, height),
                monitor(PEER, "B", 100_000, 0, 100, 100),
            ];
            assert_eq!(
                Layout::new(0, LOCAL, monitors, &pair()).unwrap_err(),
                LayoutError::ZeroExtent {
                    monitor: key(LOCAL, "A")
                }
            );
        }

        let over = MAX_MONITOR_EXTENT + 1;
        assert_eq!(
            Layout::new(
                0,
                LOCAL,
                vec![
                    monitor(LOCAL, "A", 0, 0, over, 1080),
                    monitor(PEER, "B", 1_000_000, 0, 100, 100),
                ],
                &pair(),
            )
            .unwrap_err(),
            LayoutError::ExtentTooLarge {
                monitor: key(LOCAL, "A"),
                extent: over
            }
        );
        // The cap itself is legal.
        Layout::new(
            0,
            LOCAL,
            vec![
                monitor(LOCAL, "A", 0, 0, MAX_MONITOR_EXTENT, MAX_MONITOR_EXTENT),
                monitor(
                    PEER,
                    "B",
                    MAX_MONITOR_EXTENT.try_into().unwrap(),
                    0,
                    100,
                    100,
                ),
            ],
            &pair(),
        )
        .unwrap();

        for coordinate in [
            MAX_LAYOUT_COORDINATE + 1,
            -MAX_LAYOUT_COORDINATE - 1,
            i32::MAX,
            i32::MIN,
        ] {
            assert_eq!(
                Layout::new(
                    0,
                    LOCAL,
                    vec![
                        monitor(LOCAL, "A", coordinate, 0, 100, 100),
                        monitor(PEER, "B", 0, 500, 100, 100),
                    ],
                    &pair(),
                )
                .unwrap_err(),
                LayoutError::CoordinateOutOfRange {
                    monitor: key(LOCAL, "A"),
                    coordinate
                },
                "coordinate {coordinate} was admitted"
            );
        }
        // Both signs of the cap itself are legal.
        Layout::new(
            0,
            LOCAL,
            vec![
                monitor(LOCAL, "A", -MAX_LAYOUT_COORDINATE, 0, 100, 100),
                monitor(PEER, "B", MAX_LAYOUT_COORDINATE, 0, 100, 100),
            ],
            &pair(),
        )
        .unwrap();
    }

    #[test]
    fn a_layout_naming_a_third_machine_is_refused_however_the_name_arrives() {
        let monitors = vec![
            monitor(LOCAL, "A", 0, 0, 100, 100),
            monitor(STRANGER, "B", 200, 0, 100, 100),
        ];
        assert_eq!(
            Layout::new(0, LOCAL, monitors, &pair()).unwrap_err(),
            LayoutError::UnexpectedDevice { device: STRANGER }
        );

        // The origin is a name too.
        assert_eq!(
            Layout::new(0, STRANGER, side_by_side(), &pair()).unwrap_err(),
            LayoutError::UnexpectedOrigin { device: STRANGER }
        );
    }

    #[test]
    fn an_arrangement_describing_only_one_machine_is_refused() {
        let monitors = vec![
            monitor(LOCAL, "A", 0, 0, 100, 100),
            monitor(LOCAL, "B", 200, 0, 100, 100),
        ];
        assert_eq!(
            Layout::new(0, LOCAL, monitors, &pair()).unwrap_err(),
            LayoutError::MissingMachine { device: PEER }
        );
    }

    #[test]
    fn overlapping_rectangles_are_refused_and_touching_ones_are_not() {
        assert_eq!(
            Layout::new(
                0,
                LOCAL,
                vec![
                    monitor(LOCAL, "A", 0, 0, 1920, 1080),
                    monitor(PEER, "B", 1919, 0, 1920, 1080),
                ],
                &pair(),
            )
            .unwrap_err(),
            LayoutError::Overlap {
                first: key(LOCAL, "A"),
                second: key(PEER, "B")
            }
        );

        // Overlap is forbidden between two screens of the *same* machine
        // too, not merely across the pair.
        assert!(matches!(
            Layout::new(
                0,
                LOCAL,
                vec![
                    monitor(LOCAL, "A", 0, 0, 100, 100),
                    monitor(LOCAL, "B", 50, 50, 100, 100),
                    monitor(PEER, "C", 500, 0, 100, 100),
                ],
                &pair(),
            )
            .unwrap_err(),
            LayoutError::Overlap { .. }
        ));

        // A one-unit gap is not an overlap and not an edge — just a gap.
        Layout::new(
            0,
            LOCAL,
            vec![
                monitor(LOCAL, "A", 0, 0, 1920, 1080),
                monitor(PEER, "B", 1921, 0, 1920, 1080),
            ],
            &pair(),
        )
        .unwrap();
    }

    #[test]
    fn the_raw_path_reports_an_unusable_id_as_a_layout_error() {
        let raw = |id: &str| {
            vec![
                RawPlacedMonitor {
                    device: LOCAL,
                    id: id.to_owned(),
                    rect: LayoutRect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                },
                RawPlacedMonitor {
                    device: PEER,
                    id: "B".to_owned(),
                    rect: LayoutRect {
                        x: 200,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                },
            ]
        };

        assert_eq!(
            Layout::from_raw(0, LOCAL, raw(""), &pair()).unwrap_err(),
            LayoutError::InvalidMonitorId {
                device: LOCAL,
                source: MonitorIdError::Empty
            }
        );
        assert_eq!(
            Layout::from_raw(
                0,
                LOCAL,
                raw(&"x".repeat(MAX_MONITOR_ID_BYTES + 1)),
                &pair()
            )
            .unwrap_err(),
            LayoutError::InvalidMonitorId {
                device: LOCAL,
                source: MonitorIdError::TooManyBytes {
                    bytes: MAX_MONITOR_ID_BYTES + 1
                }
            }
        );
        assert!(matches!(
            Layout::from_raw(0, LOCAL, raw("bad\u{0}id"), &pair()).unwrap_err(),
            LayoutError::InvalidMonitorId {
                source: MonitorIdError::NotPrintableAscii { .. },
                ..
            }
        ));

        // A good raw set produces the same layout the typed path does.
        let from_raw = Layout::from_raw(0, LOCAL, raw("A"), &pair()).unwrap();
        assert_eq!(from_raw.monitors()[0].id.as_str(), "A");

        // The count bound is checked before any id is validated, so an
        // oversized set with unusable ids is refused for its size.
        let oversized: Vec<RawPlacedMonitor> = (0..=MAX_LAYOUT_MONITORS)
            .map(|_| RawPlacedMonitor {
                device: LOCAL,
                id: String::new(),
                rect: LayoutRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            })
            .collect();
        assert_eq!(
            Layout::from_raw(0, LOCAL, oversized, &pair()).unwrap_err(),
            LayoutError::TooManyMonitors {
                count: MAX_LAYOUT_MONITORS + 1
            }
        );
        assert_eq!(
            Layout::from_raw(0, LOCAL, Vec::new(), &pair()).unwrap_err(),
            LayoutError::NoMonitors
        );
    }

    /// A rejection names the rule and the monitor, which is what makes a
    /// refused arrangement diagnosable (NFR-3, ADR 0018).
    #[test]
    fn a_rejection_reads_as_a_diagnostic() {
        let rendered = Layout::new(
            0,
            LOCAL,
            vec![
                monitor(LOCAL, r"\\.\DISPLAY1", 0, 0, 1920, 1080),
                monitor(PEER, r"\\.\DISPLAY2", 100, 100, 1920, 1080),
            ],
            &pair(),
        )
        .unwrap_err()
        .to_string();
        assert!(rendered.contains(r"\\.\DISPLAY1"), "{rendered}");
        assert!(rendered.contains(r"\\.\DISPLAY2"), "{rendered}");
        assert!(rendered.contains("overlap"), "{rendered}");

        // An id that failed printability is still never quoted.
        let hidden = Layout::from_raw(
            0,
            LOCAL,
            vec![RawPlacedMonitor {
                device: LOCAL,
                id: "screen\u{202E}name".to_owned(),
                rect: LayoutRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            }],
            &pair(),
        )
        .unwrap_err()
        .to_string();
        assert!(!hidden.contains("screen"), "{hidden}");
    }

    proptest! {
        /// The whole point of the bounds: an adversarial arrangement is a
        /// typed rejection or a valid layout, never a panic and never an
        /// overflow (NFR-1). Coordinates and extents range well past
        /// every cap, including the values that make `abs` and
        /// `x + width` misbehave.
        #[test]
        fn adversarial_arrangements_never_panic(
            rows in proptest::collection::vec(
                (
                    0usize..3,
                    prop_oneof![
                        Just(i32::MIN), Just(i32::MAX),
                        Just(MAX_LAYOUT_COORDINATE), Just(-MAX_LAYOUT_COORDINATE),
                        Just(MAX_LAYOUT_COORDINATE + 1),
                        any::<i32>(),
                    ],
                    prop_oneof![
                        Just(i32::MIN), Just(i32::MAX),
                        Just(MAX_LAYOUT_COORDINATE), Just(0),
                        any::<i32>(),
                    ],
                    prop_oneof![
                        Just(0u32), Just(1), Just(MAX_MONITOR_EXTENT),
                        Just(MAX_MONITOR_EXTENT + 1), Just(u32::MAX),
                        any::<u32>(),
                    ],
                    prop_oneof![
                        Just(0u32), Just(1), Just(MAX_MONITOR_EXTENT),
                        Just(u32::MAX), any::<u32>(),
                    ],
                    "[ -~]{0,70}",
                ),
                0..40,
            ),
            revision in any::<u64>(),
            origin_choice in 0usize..3,
        ) {
            let devices = [LOCAL, PEER, STRANGER];
            let monitors: Vec<RawPlacedMonitor> = rows
                .into_iter()
                .map(|(device, x, y, width, height, id)| RawPlacedMonitor {
                    device: devices[device],
                    id,
                    rect: LayoutRect { x, y, width, height },
                })
                .collect();
            let offered = monitors.len();

            match Layout::from_raw(revision, devices[origin_choice], monitors, &pair()) {
                Err(_) => {}
                Ok(layout) => {
                    // Anything accepted satisfies every bound the later
                    // derivation arithmetic relies on.
                    prop_assert!(!layout.monitors().is_empty());
                    prop_assert!(layout.monitors().len() <= MAX_LAYOUT_MONITORS);
                    prop_assert_eq!(layout.monitors().len(), offered);
                    prop_assert!(pair().contains(layout.origin()));
                    prop_assert!(layout.monitors_for(LOCAL).count() > 0);
                    prop_assert!(layout.monitors_for(PEER).count() > 0);
                    for placed in layout.monitors() {
                        prop_assert!(pair().contains(placed.device));
                        prop_assert!(placed.rect.width >= 1);
                        prop_assert!(placed.rect.height >= 1);
                        prop_assert!(placed.rect.width <= MAX_MONITOR_EXTENT);
                        prop_assert!(placed.rect.height <= MAX_MONITOR_EXTENT);
                        prop_assert!(placed.rect.x.unsigned_abs() <= MAX_LAYOUT_COORDINATE.unsigned_abs());
                        prop_assert!(placed.rect.y.unsigned_abs() <= MAX_LAYOUT_COORDINATE.unsigned_abs());
                        // The overflow argument, asserted rather than
                        // reasoned about: every edge coordinate is inside
                        // 2^25, so every later `i64` derivation is safe.
                        prop_assert!(placed.rect.right().abs() < (1i64 << 25));
                        prop_assert!(placed.rect.bottom().abs() < (1i64 << 25));
                        prop_assert!(placed.rect.left() < placed.rect.right());
                        prop_assert!(placed.rect.top() < placed.rect.bottom());
                    }
                    // And no two of them share positive area.
                    for (index, first) in layout.monitors().iter().enumerate() {
                        for second in &layout.monitors()[index + 1..] {
                            prop_assert!(!first.rect.overlaps(second.rect));
                        }
                    }
                }
            }
        }

        /// Overlap is symmetric, never reflexive-false for a real
        /// rectangle, and insensitive to which rectangle is asked first.
        #[test]
        fn overlap_is_symmetric_and_touching_is_not_overlapping(
            x in -1000i32..1000, y in -1000i32..1000,
            width in 1u32..500, height in 1u32..500,
            dx in -600i32..600, dy in -600i32..600,
        ) {
            let a = LayoutRect { x, y, width, height };
            let b = LayoutRect { x: x + dx, y: y + dy, width, height };
            prop_assert_eq!(a.overlaps(b), b.overlaps(a));
            prop_assert!(a.overlaps(a));

            // Placed exactly edge to edge on either axis, they touch and
            // do not overlap — the property adjacency is built on.
            let right_of = LayoutRect { x: x + i32::try_from(width).unwrap(), y, width, height };
            prop_assert!(!a.overlaps(right_of));
            let below = LayoutRect { x, y: y + i32::try_from(height).unwrap(), width, height };
            prop_assert!(!a.overlaps(below));
            // …and the same two placements are exactly what `abuts` says
            // yes to: the two predicates partition the relationship.
            prop_assert!(a.abuts(right_of));
            prop_assert!(a.abuts(below));
        }

        /// Abutment is symmetric, and never true of a pair that overlaps —
        /// the complement `machines_touch` and the crossing derivation
        /// both rely on.
        #[test]
        fn abutment_is_symmetric_and_never_coincides_with_overlap(
            x in -1000i32..1000, y in -1000i32..1000,
            width in 1u32..500, height in 1u32..500,
            dx in -600i32..600, dy in -600i32..600,
        ) {
            let a = LayoutRect { x, y, width, height };
            let b = LayoutRect { x: x + dx, y: y + dy, width, height };
            prop_assert_eq!(a.abuts(b), b.abuts(a));
            prop_assert!(!(a.abuts(b) && a.overlaps(b)));
        }
    }

    /// Exact adjacency, with zero tolerance (ADR 0018) — a gap is not an
    /// edge, an overlap is not an edge, and a shared corner is not an edge.
    #[test]
    fn abutment_is_exact_and_needs_a_perpendicular_overlap() {
        let left = LayoutRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let touching = LayoutRect {
            x: 100,
            y: 50,
            width: 100,
            height: 100,
        };
        assert!(left.abuts(touching));
        assert!(
            !left.abuts(LayoutRect { x: 101, ..touching }),
            "a gap is a gap"
        );
        assert!(
            !left.abuts(LayoutRect { x: 99, ..touching }),
            "an overlap is not an edge"
        );
        assert!(
            !left.abuts(LayoutRect {
                x: 100,
                y: 100,
                width: 100,
                height: 100
            }),
            "touching at a corner alone is not a crossable edge"
        );
        // Stacked, not side by side: the other axis of the same rule.
        assert!(left.abuts(LayoutRect {
            x: 50,
            y: 100,
            width: 100,
            height: 100
        }));
    }
}
