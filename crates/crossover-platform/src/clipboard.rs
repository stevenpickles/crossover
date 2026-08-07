//! The clipboard boundary (FR-3.1, platform risks R-4/R-5 in
//! docs/SPECIFICATION.md §6).
//!
//! Crossover observes the real OS clipboard — never keyboard shortcuts —
//! through this trait. Phase 2 scope is UTF-8 text; the trait reads and
//! writes text and reports non-text content as absent, leaving richer
//! types to a later revision (FR-3.7 keeps the protocol ready for them).

use thiserror::Error;

/// Failures from a [`ClipboardProvider`] backend.
///
/// The `Busy`/`Unavailable` split is load-bearing: `Busy` is the routine
/// contention of R-5 (another process holds the clipboard) and is what
/// the engine's *bounded* retry (FR-3.4) retries on; `Unavailable` is a
/// real failure and is not retried.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ClipboardError {
    /// Transient contention: the clipboard is held elsewhere right now.
    /// Retryable within the engine's bounded budget.
    #[error("clipboard busy: {reason}")]
    Busy { reason: String },

    /// The backend failed in a way retrying will not fix.
    ///
    /// `reason` is diagnostic text for logs (FR-7.3); it must never
    /// contain clipboard contents (FR-7.4).
    #[error("clipboard unavailable: {reason}")]
    Unavailable { reason: String },
}

/// A change-notification callback.
///
/// Deliberately carries **no data**: it signals "the clipboard changed",
/// and the observer reads the current state via
/// [`ClipboardProvider::read_text`]. A notify-then-read race is inherent
/// to every OS clipboard; the latest-observed-wins policy (FR-3.5)
/// absorbs it.
pub type ClipboardListener = Box<dyn Fn() + Send + Sync>;

/// Access to the platform clipboard.
///
/// Semantics implementations must uphold:
///
/// - `read_text` returns `Ok(None)` when the clipboard is empty or holds
///   no text representation — absence is not an error.
/// - `write_text` replaces the clipboard contents.
/// - The listener is invoked on an arbitrary thread and must return
///   quickly without blocking (on Windows it descends from the clipboard
///   listener message on the message-pump thread).
/// - Notifications may coalesce: several rapid changes may produce fewer
///   calls. The observer must treat a call as "state may have changed",
///   not as a per-change event.
/// - **Writes made through this provider may themselves trigger the
///   listener** (Windows does exactly this). Consumers must recognize
///   their own applied content — this is precisely the loop-prevention
///   obligation of FR-3.3, surfaced as a contract term.
/// - At most one listener is active; setting a new one replaces the old,
///   and `None` unsubscribes.
pub trait ClipboardProvider: Send + Sync {
    /// Read the current text content, or `Ok(None)` if empty/non-text.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Busy`] under contention (retryable);
    /// [`ClipboardError::Unavailable`] on real failure.
    fn read_text(&self) -> Result<Option<String>, ClipboardError>;

    /// Replace the clipboard contents with `text`.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Busy`] under contention (retryable);
    /// [`ClipboardError::Unavailable`] on real failure.
    fn write_text(&self, text: &str) -> Result<(), ClipboardError>;

    /// Install (or with `None`, remove) the change listener.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Unavailable`] if observation cannot be
    /// established at all — the engine treats that as fatal, not
    /// retryable: silent non-observation would be silent sync failure
    /// (NFR-3).
    fn set_change_listener(
        &self,
        listener: Option<ClipboardListener>,
    ) -> Result<(), ClipboardError>;
}
