//! Turning a local file selection into one sendable blob (ADR 0015,
//! "Sender side").
//!
//! This is the boundary the file half of rich clipboard *starts* at, and
//! it is deliberately the mirror image of [`virtual_file`](crate::virtual_file):
//! there a verified blob becomes something the user can paste, here a
//! selection the user copied becomes a blob with an exact length and an
//! exact hash. Everything between the two is transport that already
//! exists.
//!
//! The split follows the one the receiving half settled on (ADR 0015,
//! "The engine decides, the driver touches the disk"): the engine says
//! *build a blob from these paths* and knows nothing else; an
//! implementation walks the selection, packs it, and reports back either
//! a blob or a **typed refusal**. Nothing here is a `Result<_, io::Error>`
//! by accident — FR-3.6 requires a refusal the user can be told about,
//! never a truncated item and never a silent nothing.
//!
//! Four properties belong to the contract rather than to any one
//! implementation, because the sender-side guarantees in
//! [ADR 0015](../../../docs/adr/0015-spooled-virtual-file-paste.md) rest
//! on them:
//!
//! - **One clipboard item is one blob.** A single file travels verbatim;
//!   a folder or a multi-entry selection is packed into one archive
//!   *before* the offer, because the offer must carry the finished
//!   length and hash and the receiver must be able to bound its
//!   commitment before a byte arrives (NFR-1).
//! - **A reparse point refuses the whole transfer.** A symlink, junction,
//!   or any other reparse point is never followed and never silently
//!   skipped: following one would let a copied shortcut pack arbitrary
//!   out-of-tree content, or cycle, and skipping one would send something
//!   that is not what the user selected.
//! - **A partial selection is never sent as if it were the selection.**
//!   An entry that cannot be read — locked, denied, vanished mid-walk —
//!   refuses the transfer rather than producing a smaller archive that
//!   looks complete.
//! - **Every bound bites during the walk.** Entry count, depth, and
//!   cumulative bytes are checked as the selection is traversed, so an
//!   oversized item is refused before it is built rather than measured
//!   afterwards, and the temporary artifact of a refused build does not
//!   outlive the refusal.

use std::fs::File;
use std::path::PathBuf;

use thiserror::Error;

/// Maximum bytes one file item may become — the blob on the wire and in
/// the receiver's spool (ADR 0015).
///
/// A deliberate mirror of
/// `crossover_protocol::clipboard::MAX_CLIPBOARD_FILE_BYTES`: this crate
/// has no dependencies by design (docs/ARCHITECTURE.md §4), so the
/// platform boundary cannot name a protocol constant. `crossover-core`
/// holds a test that the two are equal, so they cannot drift.
///
/// It is stated here because the bound has to bite **at the source**. The
/// sender is the only party that can refuse an oversized selection before
/// the bytes exist at all; a builder that packed first and measured
/// afterwards would have spent the disk and the time to learn what it
/// could have answered from the walk (FR-3.6, NFR-1).
pub const MAX_CLIPBOARD_FILE_BYTES: usize = 256 * 1024 * 1024;

/// Maximum directory nesting a selection may have before it is refused.
///
/// A deliberate mirror of
/// `crossover_protocol::clipboard::MAX_ARCHIVE_DEPTH`, for the same
/// no-dependencies reason as [`MAX_CLIPBOARD_FILE_BYTES`], and held to it
/// by the same `crossover-core` test.
///
/// Depth is counted in archive path components: a selected file or folder
/// is at depth 1 and its children at depth 2.
pub const MAX_ARCHIVE_DEPTH: u32 = 32;

/// Where a blob's proposed name came from, which decides what happens if
/// it turns out not to be a conforming wire name.
///
/// The distinction exists because ADR 0015 gives two different answers,
/// and an implementation of this trait cannot give either one itself: the
/// conformance rules live in `crossover-protocol` (they are the rules a
/// *peer's* name is judged by, and a name that reaches a shell is judged
/// by exactly one validator or by none), and this crate may carry no
/// dependencies. So the builder reports where the name came from and the
/// layer above — which can name the protocol — applies the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobNaming {
    /// The name is the selected item's own: a single file's file name, or
    /// `<folder>.zip` for a single folder. It carries meaning the user
    /// chose, so a name that does not conform is a **refusal** — reject,
    /// never repair (ADR 0015: we do not guess what the user meant, and a
    /// substituted name is a name they never picked).
    Own,
    /// The name was derived from something the user did not name for this
    /// purpose — the parent folder of a multi-entry selection — and may
    /// be empty when there was nothing usable to derive it from. A name
    /// that does not conform falls back to the generic archive name,
    /// because there is nothing to preserve and nothing to guess.
    Derived,
}

/// A blob built from a local selection, ready to be offered.
///
/// Deliberately carries an **open handle and no path**. The temporary
/// artifact is the implementation's business: it is created in the
/// sender's own temp directory, it is the only copy of the item the
/// transfer will read, and it is removed when this value is dropped —
/// including on every refusal path, where no value is produced at all.
/// A caller that could name the file could also keep it, re-resolve it,
/// or hand it to something else, and none of those are things the sender
/// side does.
#[derive(Debug)]
pub struct FileBlob {
    /// The bare name the item should travel under, as derived from the
    /// selection — **not yet validated as a wire name**.
    ///
    /// Never a path: only the final component, never a directory, never a
    /// prefix. It becomes `FileDescriptor::file_name` only once the layer
    /// above has put it through `crossover_protocol::validate_file_name`,
    /// which is the same check the receiving side applies to a peer's
    /// name. May be empty when [`FileBlob::naming`] is
    /// [`BlobNaming::Derived`].
    pub proposed_name: String,
    /// Where [`FileBlob::proposed_name`] came from, and therefore what a
    /// failed validation means.
    pub naming: BlobNaming,
    /// Whether the blob is an archive the builder packed, rather than one
    /// file's bytes verbatim.
    pub archived: bool,
    /// Filesystem entries packed, at most
    /// [`MAX_CLIPBOARD_FILE_ENTRIES`](crate::MAX_CLIPBOARD_FILE_ENTRIES).
    /// Exactly 1 when not archived.
    pub entry_count: u32,
    /// Total bytes of those entries before packing — the user-facing
    /// figure `FileDescriptor::original_bytes` reports. Never used to
    /// size anything.
    pub original_bytes: u64,
    /// Exact length of the blob itself: what the offer declares and what
    /// the receiver bounds its commitment against.
    pub content_length: u64,
    /// SHA-256 of the blob, computed the same way the receiving side
    /// verifies it (`crossover_protocol::clipboard::content_hash`, and
    /// incrementally over the same bytes by `ChunkStream`). An
    /// implementation computes it while it has the bytes, so nothing
    /// re-reads the item to learn its own identity.
    pub content_hash: [u8; 32],
    /// The blob's bytes, open and positioned at the start.
    ///
    /// The send path reads from here and nowhere else; the file is
    /// removed from the filesystem when this value is dropped.
    pub content: File,
}

/// Why a selection was refused, before any of it could travel (FR-3.6).
///
/// Every variant is a *typed* answer with a diagnostic that names the
/// fault and never the data: no path, no file name, no content. A file
/// name is user data (docs/SECURITY.md invariant 6) and a path additionally
/// discloses the sender's directory layout, which ADR 0015 keeps off the
/// wire on purpose — so it does not travel through a refusal either.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum FileBlobRefusal {
    /// This build cannot pack a selection on this platform.
    ///
    /// Permanent and distinct from a fault: the sending half is simply
    /// not a capability here, so the caller reports the feature absent
    /// rather than reporting an error (ADR 0015 leaves the non-Windows
    /// sender side to the platform ports).
    #[error("building a file blob is not supported on this platform")]
    Unsupported,

    /// Nothing was selected. A programming error rather than a user one:
    /// an empty file list is not something a clipboard produces.
    #[error("the selection is empty")]
    EmptySelection,

    /// The selection packs more entries than one item may carry.
    #[error("the selection packs more than {maximum} entries")]
    TooManyEntries {
        /// The cap that was crossed.
        maximum: u32,
    },

    /// The selection nests deeper than [`MAX_ARCHIVE_DEPTH`].
    #[error("the selection nests deeper than {maximum} levels")]
    TooDeep {
        /// The cap that was crossed.
        maximum: u32,
    },

    /// The selection's content, or the finished blob, is over
    /// [`MAX_CLIPBOARD_FILE_BYTES`].
    ///
    /// Carries the size that crossed the line, because "how much too
    /// big" is the one number a user needs to act on and it is not user
    /// data. `bytes` is what had been counted when the walk stopped, so
    /// it is a floor on the true total rather than the total: the walk
    /// refuses the moment the bound is crossed instead of continuing to
    /// measure something it has already decided against.
    #[error("the selection is at least {bytes} bytes, over the {maximum}-byte maximum")]
    TooLarge {
        /// Bytes counted before the walk stopped.
        bytes: u64,
        /// The cap that was crossed.
        maximum: u64,
    },

    /// The selection contains a symlink, junction, or other reparse
    /// point.
    ///
    /// The **whole** transfer is refused. Following it would let a copied
    /// shortcut pack arbitrary out-of-tree content or cycle; omitting it
    /// would send something that is not what the user selected. There is
    /// no third answer that is both safe and honest (ADR 0015).
    #[error("the selection contains a symlink, junction, or other reparse point")]
    ReparsePoint,

    /// An entry could not be read — locked, permission denied, vanished
    /// mid-walk, or not something that can be packed at all.
    ///
    /// A partial archive is never sent as if it were the selection, so
    /// one unreadable entry refuses the item.
    #[error("an entry of the selection could not be read: {reason}")]
    Unreadable {
        /// Diagnostic detail (FR-7.3): the fault, never the path or the
        /// name (FR-7.4).
        reason: String,
    },

    /// Two entries of the selection would be packed under the same name.
    ///
    /// Refused rather than suffixed, for the same reason a name is never
    /// repaired: the second entry would travel under a name the user
    /// never gave it, inside an item they cannot inspect before it
    /// arrives. Ordinary shell selections come from one folder, where the
    /// filesystem has already made names unique, so this is a
    /// pathological clipboard rather than a case worth accommodating.
    #[error("two entries of the selection would be packed under the same name")]
    DuplicateName,

    /// The temporary artifact could not be created, written, or read
    /// back — a fault in our own workspace rather than in the selection.
    #[error("the file blob could not be built: {reason}")]
    Backend {
        /// Diagnostic detail (FR-7.3); never file contents (FR-7.4).
        reason: String,
    },
}

/// Somewhere to turn a local file selection into one offerable blob.
///
/// Separate from [`ClipboardProvider`](crate::ClipboardProvider) because
/// it is not a clipboard operation: the clipboard's part ended when it
/// reported a [`ClipboardContent::FileList`](crate::ClipboardContent), and
/// what follows is a filesystem walk that may take seconds and touch
/// gigabytes. Keeping it off the clipboard trait keeps it off the
/// clipboard listener's thread, which the receiving half already had to
/// learn the hard way (ADR 0015, "Threading").
///
/// Implementations must uphold what the module doc states: a reparse
/// point or an unreadable entry refuses the whole item, every bound is
/// checked during the walk, and no temporary artifact outlives a refusal.
pub trait FileBlobBuilder: Send + Sync {
    /// Pack `selection` into one blob, or refuse it.
    ///
    /// `selection` is the raw, unvalidated set of local paths the OS
    /// clipboard reported, in the order it reported them. A single
    /// regular file travels verbatim; anything else — a folder, or more
    /// than one entry — is packed into a single archive.
    ///
    /// # Errors
    ///
    /// A [`FileBlobRefusal`] naming which rule the selection broke. Every
    /// refusal happens before any of the selection could travel, and
    /// leaves nothing behind on disk.
    fn build(&self, selection: &[PathBuf]) -> Result<FileBlob, FileBlobRefusal>;
}

/// A [`FileBlobBuilder`] for platforms with no sending half: every
/// selection is refused [`FileBlobRefusal::Unsupported`].
///
/// Deliberately not a portable `std::fs` walk. A portable walk cannot see
/// a junction — `FILE_ATTRIBUTE_REPARSE_POINT` covers mount points that
/// `std::fs::symlink_metadata` reports as an ordinary directory — so it
/// would satisfy the *signature* while voiding the refusal the security
/// argument rests on, and a caller could not tell from the return type.
/// Reporting the capability absent is the honest option, and matches
/// [`UnsupportedSpoolStorage`](crate::UnsupportedSpoolStorage).
#[derive(Debug, Default, Clone, Copy)]
pub struct UnsupportedFileBlobBuilder;

impl FileBlobBuilder for UnsupportedFileBlobBuilder {
    fn build(&self, _selection: &[PathBuf]) -> Result<FileBlob, FileBlobRefusal> {
        Err(FileBlobRefusal::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{FileBlobBuilder, FileBlobRefusal, UnsupportedFileBlobBuilder};

    #[test]
    fn the_unsupported_builder_refuses_every_selection() {
        let builder = UnsupportedFileBlobBuilder;
        assert!(matches!(
            builder.build(&[PathBuf::from("C:\\work\\report.pdf")]),
            Err(FileBlobRefusal::Unsupported)
        ));
        // Not "nothing to send": no builder. A caller must not read a
        // refusal to answer as an empty answer, which would look like a
        // successful transfer of nothing.
        assert!(matches!(
            builder.build(&[]),
            Err(FileBlobRefusal::Unsupported)
        ));
    }

    #[test]
    fn a_refusal_never_carries_the_data_it_refused() {
        // The diagnostics are logged (NFR-3), so they are held to the
        // same rule every other user-data-adjacent message is: name the
        // fault, never the name, the path, or the bytes.
        let refusals = [
            FileBlobRefusal::EmptySelection,
            FileBlobRefusal::TooManyEntries { maximum: 256 },
            FileBlobRefusal::TooDeep { maximum: 32 },
            FileBlobRefusal::TooLarge {
                bytes: 1,
                maximum: 2,
            },
            FileBlobRefusal::ReparsePoint,
            FileBlobRefusal::Unreadable {
                reason: "access is denied".to_owned(),
            },
        ];
        for refusal in refusals {
            let rendered = refusal.to_string();
            assert!(!rendered.is_empty());
            assert!(
                !rendered.contains('\\') && !rendered.contains('/'),
                "refusal {rendered:?} looks like it carries a path"
            );
        }
    }
}
