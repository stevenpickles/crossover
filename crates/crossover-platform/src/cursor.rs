//! Hiding the local cursor while this machine drives the peer (ADR 0009).
//!
//! When a machine controls the peer, the capture layer freezes its own
//! cursor, pinned at the linked edge (ADR 0007) — a second, stationary
//! cursor with nothing to do, while the peer's cursor is the one actually
//! moving. [`CursorMask`] removes it: hide on gaining control, restore on
//! giving it back, so the user sees exactly one cursor.
//!
//! Masking is a **display nicety, never a correctness lever**: a failure to
//! hide or show is logged and ignored, never a reason to drop or refuse
//! control. The one hard requirement is the inverse of a stuck key — the
//! cursor must always come back — so an implementation restores it on every
//! exit from control *and* on process death, not only on a clean return.

use thiserror::Error;

/// Failures from a [`CursorMask`] backend. Advisory only: the driver logs
/// and continues, since a mis-masked cursor never justifies disturbing
/// control (see the module docs).
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum CursorMaskError {
    /// The platform could not change the cursor's visibility.
    ///
    /// `reason` is diagnostic text for logs (FR-7.3).
    #[error("cursor mask failed: {reason}")]
    Failed {
        /// Diagnostic detail.
        reason: String,
    },
}

/// Hides and restores the local cursor around a spell of controlling the
/// peer (ADR 0009). Both calls are **idempotent** — hiding an already
/// hidden cursor, or showing an already visible one, is a no-op — so the
/// driver can bracket every `StartCapture`/`StopCapture` without tracking
/// state itself.
pub trait CursorMask: Send + Sync {
    /// Hide the local cursor. Called when this machine begins driving the
    /// peer.
    ///
    /// # Errors
    ///
    /// [`CursorMaskError::Failed`] if the platform could not hide it; the
    /// caller treats this as non-fatal.
    fn hide(&self) -> Result<(), CursorMaskError>;

    /// Restore the local cursor. Called on every exit from controlling the
    /// peer.
    ///
    /// # Errors
    ///
    /// [`CursorMaskError::Failed`] if the platform could not restore it;
    /// the caller treats this as non-fatal.
    fn show(&self) -> Result<(), CursorMaskError>;
}

/// A [`CursorMask`] that does nothing — for platforms without a masking
/// implementation and for runs that opt out. Control still works; there is
/// simply no cursor hiding.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCursorMask;

impl CursorMask for NoopCursorMask {
    fn hide(&self) -> Result<(), CursorMaskError> {
        Ok(())
    }

    fn show(&self) -> Result<(), CursorMaskError> {
        Ok(())
    }
}
