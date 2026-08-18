//! Offering a spooled file to the operating system's paste mechanism
//! (ADR 0015).
//!
//! This is the boundary the file half of rich clipboard ends at. A
//! completed transfer is bytes in the spool and a name beside them; what
//! turns that into something the user can paste is entirely
//! platform-shaped — on Windows a virtual file list served through an
//! `IDataObject` we own — so the engine states its intent here and knows
//! nothing about how it is met.
//!
//! Three properties are part of the contract rather than of any one
//! implementation, because the security argument in
//! [SECURITY.md](../../../docs/SECURITY.md) §7 rests on them:
//!
//! - **Nothing is written where the user can see it.** Placing an offer
//!   advertises a *promise*; the operating system's own paste, driven by
//!   the user's gesture, is what creates a file, and it creates it
//!   wherever the user pressed paste (F3, invariant 9).
//! - **The bytes are produced from the spool entry, on demand, and never
//!   from anything the consumer names** (F14). An implementation resolves
//!   [`VirtualFile::entry`] through the spool it was built with — it takes
//!   no path, and none is expressible here.
//! - **The offer does not outlive what can serve it** (F16). An
//!   implementation excludes its item from clipboard history and cloud
//!   synchronization, because a retained entry pointing at a process or a
//!   spool entry that is gone is a promise that cannot be kept.

use crate::ClipboardError;

/// A verified spool entry, described well enough to be offered.
///
/// The two names are deliberately different things, and conflating them
/// is the bug this type exists to make impossible: `entry` is **ours**, a
/// locally generated identifier the spool resolves, while `file_name` is
/// the **peer's**, validated network input that reaches the shell and
/// becomes the name of a file the user sees (ADR 0015).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualFile {
    /// The spool entry holding the bytes. Never a path, and resolved only
    /// through the spool the implementation already holds.
    pub entry: String,
    /// The name to present at the paste destination — validated by
    /// `crossover_protocol::validate_file_name` before it ever reaches
    /// here, and re-checked by the implementation before it is built into
    /// a descriptor.
    pub file_name: String,
    /// Exact byte length of the entry, as verified against the offer.
    pub byte_len: u64,
}

/// Somewhere to put a virtual file list so the user can paste it.
///
/// Separate from [`ClipboardProvider`](crate::ClipboardProvider), and the
/// separation is a requirement rather than tidiness: on Windows the object
/// must live on a single-threaded apartment with a message pump of its
/// own, distinct from the clipboard-change listener's, because render
/// callbacks are driven by whichever application is pasting and any local
/// process can drive them. Sharing one thread would let a render loop
/// starve clipboard change notifications machine-wide (ADR 0015, F14).
pub trait VirtualFileClipboard: Send + Sync {
    /// Offer `file` to the paste mechanism, replacing whatever the
    /// clipboard held — the same cost any clipboard write imposes.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Busy`] under contention (retryable);
    /// [`ClipboardError::Unsupported`] where there is no such mechanism;
    /// [`ClipboardError::Unavailable`] on real failure.
    fn offer(&self, file: &VirtualFile) -> Result<(), ClipboardError>;

    /// Whether the clipboard still holds the object this provider placed.
    ///
    /// The cheap, exact half of loop prevention (F13): our own placement
    /// raises a change notification like any other write, and staging that
    /// notification would offer the item straight back to the peer that
    /// sent it. `true` means "this change is ours" — nothing is read, and
    /// no content hash is computed, because a virtual file list has no
    /// bytes to hash without invoking our own render.
    ///
    /// Correctly self-limiting: once any other application copies, the
    /// object is no longer the clipboard's and this stops matching, so
    /// ordinary local copies resume synchronizing exactly as before.
    fn is_current(&self) -> bool;

    /// Withdraw our object if it is still current, leaving the clipboard
    /// empty.
    ///
    /// Used when the entry behind the offer is going away, so the
    /// clipboard never advertises a promise nothing can serve.
    ///
    /// # Errors
    ///
    /// As [`VirtualFileClipboard::offer`]. A clipboard that has already
    /// moved on is a success: there is nothing to withdraw.
    fn withdraw(&self) -> Result<(), ClipboardError>;
}

/// A [`VirtualFileClipboard`] for platforms with no virtual-file paste:
/// offering reports [`ClipboardError::Unsupported`], and nothing is ever
/// current.
///
/// Deliberately not a drop-folder fallback. ADR 0015 keeps the drop folder
/// as a documented alternative for a platform that cannot promise files —
/// X11 and Wayland have no equivalent mechanism — but choosing it is a
/// per-platform user-experience decision with its own threat entries, not
/// something this type may make silently on the way past.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnsupportedVirtualFileClipboard;

impl VirtualFileClipboard for UnsupportedVirtualFileClipboard {
    fn offer(&self, _file: &VirtualFile) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unsupported {
            reason: "virtual file paste is not implemented on this platform".to_owned(),
        })
    }

    fn is_current(&self) -> bool {
        false
    }

    fn withdraw(&self) -> Result<(), ClipboardError> {
        // Nothing was ever offered, so there is nothing to withdraw and no
        // failure to report: the postcondition the caller wants — the
        // clipboard does not hold our object — already holds.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{UnsupportedVirtualFileClipboard, VirtualFile, VirtualFileClipboard};
    use crate::ClipboardError;

    #[test]
    fn the_unsupported_clipboard_refuses_to_offer_but_withdraws_cleanly() {
        let clipboard = UnsupportedVirtualFileClipboard;
        let file = VirtualFile {
            entry: "3f2504e0-4f89-41d3-9a0c-0305e82c3301.bin".to_owned(),
            file_name: "quarterly.pdf".to_owned(),
            byte_len: 4096,
        };

        assert!(matches!(
            clipboard.offer(&file),
            Err(ClipboardError::Unsupported { .. })
        ));
        assert!(!clipboard.is_current());
        // Withdrawal is about a postcondition, not about an action, so
        // "there was never anything there" is a success rather than the
        // refusal `offer` gives.
        assert!(clipboard.withdraw().is_ok());
    }
}
