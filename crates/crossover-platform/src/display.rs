//! The display boundary and the geometry it speaks (ADR 0009).
//!
//! Seamless control transfer needs two facts from the OS: the size of the
//! desktop the cursor roams, and where the cursor is within it. Both come
//! through this trait, and the geometry *vocabulary* lives here — not in
//! `crossover-core` — for the same reason the input vocabulary does: the
//! trait must speak it and core cannot be a dependency of the trait that
//! describes it (docs/ARCHITECTURE.md §2). The *policy* — which edge
//! links to the peer, how a crossing maps to a fraction of the edge —
//! stays in core's topology model (ADR 0009).
//!
//! The reported region is the whole **virtual desktop** — every monitor,
//! as one rectangle — so the crossing edge is the outer edge of the
//! desktop, not a seam between two monitors (a primary-only region turns
//! the boundary between monitors into a false edge). Coordinates are
//! normalized to the desktop's top-left, so the cursor is always in
//! `0..width`×`0..height` and the topology model needs no origin.
//!
//! Since ADR 0018 a monitor also carries an **identity**: matching a live
//! screen to one the user drew in a layout needs something stabler than a
//! position in an enumeration ([`MonitorInfo`] says why an index would not
//! do).
//!
//! **Identity and geometry are separate queries, deliberately.**
//! [`DisplayInfo::monitors`] is the geometry enumeration and stays the
//! required method; [`DisplayInfo::monitor_layout`] adds identity on top
//! and is *defaulted* to "every rectangle, none of them named". The
//! separation is not tidiness — it is the safety property ADR 0018 states
//! as **an unknown id degrades placement, never geometry**:
//!
//! - The edge detector polls `monitors()` every few milliseconds and never
//!   looks at an id. A monitor that vanished from that list because the OS
//!   would not name it would move the desktop's outer edge inward, turning
//!   an interior seam into a crossing edge — a false handoff, which is the
//!   release-blocking class of defect.
//! - An unnamed monitor costs only what the ADR says it should: the layout
//!   cannot address it, so a crossing onto it falls back to desktop-bounds
//!   placement with a diagnostic. Control correctness never depended on it.
//!
//! So a backend that can enumerate rectangles but cannot name them is a
//! first-class backend: it implements `monitors()`, inherits the default
//! `monitor_layout()`, and loses the placement nicety and nothing else.

use thiserror::Error;

/// The virtual desktop's pixel size — all monitors as one rectangle, its
/// origin normalized to the top-left (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    /// Width in pixels across every monitor.
    pub width: u32,
    /// Height in pixels across every monitor.
    pub height: u32,
}

/// A cursor position in the virtual desktop's pixel space, normalized to
/// its top-left origin (so it lies within [`Screen`]). Signed so a
/// coordinate at or just past an edge is representable without wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPoint {
    /// Rightward pixels from the desktop's left edge.
    pub x: i32,
    /// Downward pixels from the desktop's top edge.
    pub y: i32,
}

/// One monitor's bounds, normalized to the virtual desktop's top-left
/// origin (like [`CursorPoint`]). Crossing maps the edge fraction against
/// the specific monitor on the crossing edge — not the whole bounding-box
/// desktop — so monitors of different resolution, and the dead space
/// between mismatched ones, map correctly (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    /// Left pixel (desktop-relative).
    pub left: i32,
    /// Top pixel (desktop-relative).
    pub top: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// One monitor's bounds together with the identity a drawn layout
/// addresses it by, when the platform could supply one (ADR 0018).
///
/// The id is the **platform-supplied device string** — on Windows,
/// `GetMonitorInfoW`'s `szDevice` (`\\.\DISPLAY1` and friends) — and it is
/// what makes a saved arrangement survive a reboot. An enumeration index is
/// positional: unplug a monitor and index 1 silently becomes a different
/// screen, so a layout drawn against indices would be wrong in the way that
/// is hardest to see. A device string that *does* change leaves the monitor
/// simply unknown, which is observable.
///
/// `id` is an `Option` for exactly that reason, and the `None` case is a
/// state to represent rather than a failure to hide. It means *this
/// rectangle is real and the platform would not name it* — a monitor the
/// user can see, that edge detection must keep using, and that a layout
/// cannot address. Never a fabricated or positional stand-in: a made-up id
/// is worse than none, because after a re-enumeration it would confidently
/// match the wrong screen.
///
/// The id is a plain `String`, unvalidated, because that is the honest
/// shape of "whatever the OS said". The bound and the charset rule
/// (`MAX_MONITOR_ID_BYTES`, printable ASCII) belong to the layout model in
/// `crossover-topology`, which this crate must not depend on: a platform
/// trait that could not report what the OS actually returned would have no
/// way to say that a machine's display configuration is unusable.
///
/// A stable per-monitor identifier is thereby a requirement on the future
/// macOS and Linux backends too (ADR 0018, recorded ahead of Phase 9) — but
/// a *soft* one: a backend without it still serves geometry (see this
/// module's header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// The platform's device string for this monitor, or `None` if it
    /// could not be read.
    pub id: Option<String>,
    /// Its bounds, normalized to the desktop origin, exactly as
    /// [`MonitorRect`] describes.
    pub rect: MonitorRect,
}

/// Failures from a [`DisplayInfo`] backend.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum DisplayError {
    /// The platform could not report display geometry or the cursor.
    ///
    /// `reason` is diagnostic text for logs (FR-7.3).
    #[error("display query failed: {reason}")]
    Unavailable {
        /// Diagnostic detail.
        reason: String,
    },
}

/// Read-only access to the local virtual-desktop geometry and cursor
/// (ADR 0009).
///
/// The size and the cursor come from the same process and the same
/// (normalized) coordinate space, so edge detection compares like with
/// like. The process is expected to be per-monitor DPI aware (R-3) so the
/// numbers are real pixels; cross-machine mapping never uses these pixels
/// directly — it goes through the fraction in core's topology model.
pub trait DisplayInfo: Send + Sync {
    /// The virtual desktop's pixel size (all monitors as one rectangle).
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot report the
    /// desktop geometry.
    fn desktop_bounds(&self) -> Result<Screen, DisplayError>;

    /// Every monitor's bounds, normalized to the desktop origin, so the
    /// crossing edge can be mapped against the actual monitor on it rather
    /// than the bounding box (ADR 0009). At least one on any real display.
    ///
    /// **Geometry only, and required.** This is the query edge detection
    /// polls, and it must never be able to lose a monitor because the
    /// platform could not *name* one — see this module's header for what a
    /// short list would cost. A backend implements this whether or not it
    /// can identify anything.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot enumerate the
    /// monitors.
    fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError>;

    /// Every monitor, with the device string a drawn layout addresses it
    /// by where the platform supplies one (ADR 0018).
    ///
    /// The list holds **the same rectangles [`DisplayInfo::monitors`]
    /// reports, always** — identity is added per monitor, best effort, and
    /// its absence shows up as `MonitorInfo::id == None` rather than as a
    /// missing entry. Consulted on the rare paths that care about identity
    /// (publishing the local topology, matching a layout), never on the
    /// hot edge-detection path.
    ///
    /// Defaulted to the geometry enumeration with nothing named, so a
    /// backend with no stable per-monitor identifier is still a working
    /// backend: it loses layout-directed cursor placement, which ADR 0018
    /// treats as advisory, and keeps everything else.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot enumerate the
    /// monitors. Failing to *identify* one is not an error.
    fn monitor_layout(&self) -> Result<Vec<MonitorInfo>, DisplayError> {
        Ok(self
            .monitors()?
            .into_iter()
            .map(|rect| MonitorInfo { id: None, rect })
            .collect())
    }

    /// The cursor's current position, normalized to the virtual desktop's
    /// top-left origin.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot report the
    /// cursor position.
    fn cursor_position(&self) -> Result<CursorPoint, DisplayError>;
}
