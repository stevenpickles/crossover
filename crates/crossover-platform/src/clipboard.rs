//! The clipboard boundary (FR-3.1, platform risks R-4/R-5 in
//! docs/SPECIFICATION.md §6).
//!
//! Crossover observes the real OS clipboard — never keyboard shortcuts —
//! through this trait. The boundary is **typed** since ADR 0014: a
//! provider reads and writes a [`ClipboardContent`], of which UTF-8 text
//! is one variant and a raster image is another. Everything raster —
//! `CF_DIB` and friends — lives *behind* this trait in the platform
//! crates; nothing above it names an OS clipboard format (NFR-4).
//!
//! Image bytes are **opaque here and everywhere above**: no component
//! transcodes, compresses, or parses them (ADR 0014). The format tag is
//! descriptive metadata that travels with the bytes, not a promise the
//! bytes were inspected.

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

    /// The backend does not handle this content *type* at all.
    ///
    /// Split from [`ClipboardError::Unavailable`] because the two mean
    /// different things to the peer that sent the item: a clipboard that
    /// is unavailable might work in a second, while a type this build
    /// cannot represent will never work, and the origin deserves to be
    /// told which it met (NFR-3). It is what a backend returns for a
    /// raster image before ADR 0014's platform slice lands.
    #[error("clipboard content type unsupported: {reason}")]
    Unsupported { reason: String },
}

/// The largest image a provider may hand across this boundary, in bytes.
///
/// A deliberate mirror of `crossover_protocol::clipboard::MAX_CLIPBOARD_IMAGE_BYTES`
/// — this crate has no dependencies by design (docs/ARCHITECTURE.md §4), so
/// the boundary cannot name the protocol constant. `crossover-core` holds a
/// test that the two are equal, so they cannot drift.
///
/// It lives here because the bound has to bite **at the source**, not only
/// where the engine checks it: a provider learns an item's size before it
/// copies the bytes (`GlobalSize` on Windows), and an item past this
/// ceiling can never be synchronized (FR-3.6). Copying 512 MiB out of the
/// OS clipboard so the layer above can discard it would be a self-inflicted
/// allocation spike with a machine-global lock held. A provider that meets
/// an oversized item therefore reports **absent** — the trait's meaning for
/// "nothing this backend represents" — after logging the size (never the
/// content, FR-7.4).
pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Raster formats a [`ClipboardContent::Image`] may carry (ADR 0014).
///
/// A deliberate mirror of `crossover_protocol::clipboard::ImageFormat`:
/// this crate is trait definitions with **no dependencies** by design
/// (docs/ARCHITECTURE.md §4), so the platform boundary cannot name a
/// protocol type. The two are kept in step by an exhaustive, wildcard-free
/// mapping in `crossover-core` and a test that walks every variant, so a
/// new format fails to compile rather than silently losing its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardImageFormat {
    /// Windows device-independent bitmap (`CF_DIB`), the default.
    Dib,
    /// PNG, verbatim.
    Png,
    /// JPEG, verbatim.
    Jpeg,
}

/// What the OS clipboard holds, typed (ADR 0014).
///
/// `Text` keeps a `String` rather than bytes so the UTF-8 guarantee is
/// carried by the type across the boundary — every provider would
/// otherwise have to re-establish it, and one that forgot would put
/// invalid UTF-8 into a channel whose consumers assume otherwise. Image
/// bytes are a plain `Vec<u8>` precisely because nothing may assume
/// anything about them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    /// UTF-8 text (`CF_UNICODETEXT` on Windows).
    Text(String),
    /// A raster image in the source clipboard's own format, verbatim.
    Image {
        /// What the bytes are said to be; never verified by parsing them.
        format: ClipboardImageFormat,
        /// The image bytes, opaque.
        bytes: Vec<u8>,
    },
}

impl ClipboardContent {
    /// The text, if this is text — the shape most callers of the old
    /// text-only boundary want.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Image { .. } => None,
        }
    }

    /// Content byte length, for bounds checks and diagnostics. Never the
    /// content itself — contents are never logged (FR-7.4).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Image { bytes, .. } => bytes.len(),
        }
    }
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
/// - `read` returns `Ok(None)` when the clipboard is empty or holds no
///   representation this build handles — absence is not an error, and a
///   format the backend cannot yet read is absence, not failure.
/// - `write` replaces the clipboard contents. A content *type* the
///   backend cannot install is [`ClipboardError::Unavailable`] (a
///   permanent, non-retryable refusal), never a silent success.
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
    /// Read the current content, or `Ok(None)` if the clipboard is empty
    /// or holds nothing this backend represents.
    ///
    /// "Nothing this backend represents" includes an image larger than
    /// [`MAX_CLIPBOARD_IMAGE_BYTES`]: it is refused *before* its bytes are
    /// copied, logged by size alone, and reported absent rather than
    /// truncated (FR-3.6).
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Busy`] under contention (retryable);
    /// [`ClipboardError::Unavailable`] on real failure.
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError>;

    /// Replace the clipboard contents with `content`.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Busy`] under contention (retryable);
    /// [`ClipboardError::Unavailable`] on real failure, **including a
    /// content type this backend cannot install** — the caller must be
    /// able to tell the origin the truth (FR-3.2, NFR-3).
    fn write(&self, content: &ClipboardContent) -> Result<(), ClipboardError>;

    /// Read the current text content, or `Ok(None)` if the clipboard is
    /// empty or holds no text.
    ///
    /// A convenience over [`ClipboardProvider::read`], not a second
    /// contract: implementations override [`ClipboardProvider::read`].
    ///
    /// # Errors
    ///
    /// As [`ClipboardProvider::read`].
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        Ok(match self.read()? {
            Some(ClipboardContent::Text(text)) => Some(text),
            Some(ClipboardContent::Image { .. }) | None => None,
        })
    }

    /// Replace the clipboard contents with `text`.
    ///
    /// A convenience over [`ClipboardProvider::write`].
    ///
    /// # Errors
    ///
    /// As [`ClipboardProvider::write`].
    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        self.write(&ClipboardContent::Text(text.to_owned()))
    }

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
