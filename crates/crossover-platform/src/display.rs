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
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot enumerate the
    /// monitors.
    fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError>;

    /// The cursor's current position, normalized to the virtual desktop's
    /// top-left origin.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot report the
    /// cursor position.
    fn cursor_position(&self) -> Result<CursorPoint, DisplayError>;
}
