//! The display boundary and the geometry it speaks (ADR 0009).
//!
//! Seamless control transfer needs two facts from the OS: the primary
//! display's pixel size, and where the cursor is. Both come through this
//! trait, and the geometry *vocabulary* lives here — not in
//! `crossover-core` — for the same reason the input vocabulary does: the
//! trait must speak it and core cannot be a dependency of the trait that
//! describes it (docs/ARCHITECTURE.md §2). The *policy* — which edge
//! links to the peer, how a crossing maps to a fraction of the edge —
//! stays in core's topology model (ADR 0009).

use thiserror::Error;

/// The primary display's pixel size, origin at its top-left (ADR 0009:
/// the primary display, this phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// A cursor position in the primary display's pixel space, top-left
/// origin. Signed so a coordinate at or just past an edge — or, later, on
/// another monitor — is representable without wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPoint {
    /// Rightward pixels from the left edge.
    pub x: i32,
    /// Downward pixels from the top edge.
    pub y: i32,
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

/// Read-only access to the local display geometry and cursor (ADR 0009).
///
/// Implementations report the **primary** display this phase. Coordinates
/// are pixels in that display's space, consistent with each other: the
/// screen size and the cursor position come from the same process, so
/// edge detection compares like with like regardless of the process's DPI
/// context (R-3). Cross-machine mapping never uses these pixels directly —
/// it goes through the fraction in core's topology model.
pub trait DisplayInfo: Send + Sync {
    /// The primary display's pixel size.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot report the
    /// primary display's geometry.
    fn primary_screen(&self) -> Result<Screen, DisplayError>;

    /// The cursor's current position, in the primary display's space.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot report the
    /// cursor position.
    fn cursor_position(&self) -> Result<CursorPoint, DisplayError>;
}
