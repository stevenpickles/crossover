//! The input boundary and the vocabulary it speaks (FR-4.x, ADR 0007).
//!
//! The event types live here rather than in `crossover-core` because
//! this is the layer both sides must agree on: platform crates produce
//! and consume them, and core cannot be a dependency of the trait that
//! describes them (docs/ARCHITECTURE.md §2). Policy — what is held down,
//! what may be coalesced — stays in core, which owns the state machines.

use thiserror::Error;

/// A pointer button, named by role rather than by any OS's numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PointerButton {
    /// Primary button (the OS has already applied any left-handed swap;
    /// Crossover sees the logical button).
    Left,
    /// Secondary button.
    Right,
    /// Wheel button.
    Middle,
    /// First extended button (typically "back").
    X1,
    /// Second extended button (typically "forward").
    X2,
}

impl PointerButton {
    /// Every button, in a stable order — the order releases are emitted
    /// in, so `ReleaseAllInput` is deterministic (NFR-2).
    pub const ALL: [Self; 5] = [Self::Left, Self::Right, Self::Middle, Self::X1, Self::X2];

    /// Dense index, for array-backed state.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
            Self::X1 => 3,
            Self::X2 => 4,
        }
    }
}

/// One unit of scroll: 120 per traditional detent.
///
/// Windows' `WHEEL_DELTA` convention, adopted because high-resolution
/// wheels report fractions of a detent and a coarser unit would discard
/// them. Platforms whose native unit differs scale at their own
/// boundary, not here.
pub const SCROLL_UNITS_PER_DETENT: i32 = 120;

/// A platform-neutral pointer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEvent {
    /// Relative movement in device units (ADR 0007: unaccelerated,
    /// unclamped — the desktop's pointer ballistics are not applied).
    Motion {
        /// Rightward delta.
        dx: i32,
        /// Downward delta.
        dy: i32,
    },
    /// A button transition. Ordering matters and is never coalesced.
    Button {
        /// Which button.
        button: PointerButton,
        /// True for press, false for release.
        pressed: bool,
    },
    /// Wheel movement in [`SCROLL_UNITS_PER_DETENT`] units.
    Scroll {
        /// Horizontal (positive: right).
        dx: i32,
        /// Vertical (positive: away from the user).
        dy: i32,
    },
}

/// Failures from input capture or injection.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum InputError {
    /// Capture could not be established or was lost unrecoverably.
    ///
    /// Losing observation while the peer holds control is a fail-closed
    /// event (ADR 0007): the caller releases control and issues
    /// `ReleaseAllInput` rather than leaving a dead pointer.
    #[error("input capture unavailable: {reason}")]
    CaptureUnavailable { reason: String },

    /// Injection failed outright.
    ///
    /// Note what this does *not* cover: on Windows, injection into a
    /// higher-integrity window is silently discarded by UIPI and reports
    /// success (R-1). A caller cannot rely on `Ok` meaning the
    /// destination saw the event.
    #[error("input injection failed: {reason}")]
    InjectionFailed { reason: String },
}

/// Receives captured events. Invoked on the platform's input thread, so
/// it must return promptly and never block — on Windows it descends from
/// a low-level hook callback whose overrun causes Windows to silently
/// remove the hook (R-2).
pub type InputSink = Box<dyn Fn(PointerEvent) + Send + Sync>;

/// Captures local pointer input and suppresses its local effect.
///
/// Suppression is the whole point: while the peer holds control, moving
/// the mouse must drive the far machine *without* also acting here.
/// Implementations that can observe but not suppress do not satisfy this
/// trait (ADR 0007).
pub trait InputCapture: Send + Sync {
    /// Begin capturing and suppressing local pointer input, delivering
    /// events to `sink`.
    ///
    /// Idempotent: starting while already capturing replaces the sink.
    ///
    /// # Errors
    ///
    /// [`InputError::CaptureUnavailable`] if capture cannot be
    /// established — fatal to the control transfer that requested it,
    /// never a silent partial success.
    fn start_capture(&self, sink: InputSink) -> Result<(), InputError>;

    /// Stop capturing; local input resumes acting locally.
    ///
    /// Idempotent, and safe to call when not capturing — callers reach
    /// for it on error paths where the state is uncertain, which is
    /// exactly when it must not fail.
    ///
    /// # Errors
    ///
    /// [`InputError::CaptureUnavailable`] if teardown fails.
    fn stop_capture(&self) -> Result<(), InputError>;

    /// Whether capture is currently active *and observed to be healthy*.
    ///
    /// Implementations detect their own loss where the platform allows
    /// it (Windows removes an overrunning hook without telling anyone),
    /// so a caller polling this can fail closed rather than believe it
    /// still holds input it stopped receiving.
    fn is_capturing(&self) -> bool;
}

/// Injects pointer input on this machine.
pub trait InputInjector: Send + Sync {
    /// Replay `events` in order.
    ///
    /// Implementations must mark their own injections so that a
    /// concurrently active [`InputCapture`] does not capture them back
    /// (ADR 0007) — the same mark-what-you-emit discipline clipboard
    /// loop prevention uses.
    ///
    /// # Errors
    ///
    /// [`InputError::InjectionFailed`] if the platform rejects the
    /// events. Success does **not** guarantee the destination window
    /// received them (UIPI, R-1).
    fn inject(&self, events: &[PointerEvent]) -> Result<(), InputError>;
}
