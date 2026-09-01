//! The spool boundary: a protected directory Crossover owns, opened once
//! and thereafter operated on **only by handle** (ADR 0015, SECURITY.md
//! F15).
//!
//! Received file bytes land here — the first place in the system where
//! peer-controlled content reaches disk, written by a process that runs at
//! high integrity for administrator users (ADR 0012). The spool lives in
//! the *user's* profile, so a same-user, medium-integrity process is an
//! in-scope attacker: it can plant a directory junction where the spool
//! root is and, if the worker then deleted through it by path, obtain an
//! arbitrary-file delete at high integrity.
//!
//! That is why this trait has the shape it does. It does **not** expose
//! paths. An implementation resolves the root exactly once, verifies it,
//! and every later create, open, rename, enumerate, and unlink is
//! expressed as a *bare entry name relative to that already-open root*.
//! There is no method a caller can hand a path to, so there is no call
//! site at which a re-resolution could be reintroduced. Deletion is
//! per-entry and never recursive: a subdirectory found in the root is
//! reported, never descended into (F15).
//!
//! Portability is honest rather than convenient: a platform with no
//! implementation of those guarantees reports
//! [`SpoolError::Unsupported`] instead of falling back to unprotected
//! `std::fs`, because "the spool is protected" is a security claim other
//! invariants (F14's "verified when written, protected since") are built
//! on. [`UnsupportedSpoolStorage`] is that honest nothing.

use std::fs::File;
use std::path::Path;

use thiserror::Error;

/// Longest accepted spool entry name, in bytes.
///
/// Entry names are **ours**, never the peer's: a completed transfer is
/// `<uuid>.bin` and its partial is `<uuid>.part` (ADR 0015). A UUID plus
/// an extension is 41 bytes, so this is generous headroom, not a limit
/// anything real approaches — its job is to bound the buffer an
/// implementation builds before it calls the OS (NFR-1).
pub const MAX_SPOOL_ENTRY_NAME_BYTES: usize = 64;

/// Most objects [`SpoolStorage::entries`] will report before it refuses.
///
/// The root's contents are bounded by `MAX_SPOOL_ENTRIES` when only
/// Crossover writes there — but a same-user process may plant files, and
/// enumeration must not become an unbounded allocation driven by that.
/// Past this count an implementation fails closed
/// ([`SpoolError::Backend`]) rather than truncating the listing: a
/// truncated listing would make the startup sweep silently incomplete,
/// which is exactly the property the sweep exists to guarantee.
pub const MAX_SPOOL_ENUMERATED_OBJECTS: usize = 4096;

/// Failures from a [`SpoolStorage`] backend.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum SpoolError {
    /// This build has no protected spool for the platform.
    ///
    /// Distinct from [`SpoolError::Backend`] because it is permanent and
    /// actionable in a different way: file receive is simply not a
    /// capability here, so the caller reports the feature absent rather
    /// than reporting a fault (ADR 0015 leaves the Linux fallback
    /// undecided).
    #[error("protected spool storage is not supported on this platform")]
    Unsupported,

    /// The root exists but is not something we will use as a spool — not
    /// a directory, or a reparse point (junction/symlink), or its
    /// protection could not be asserted.
    ///
    /// The correct response is to **disable file receive for the run** and
    /// say why. It is never to delete and recreate the root: deleting
    /// whatever is sitting there is precisely the operation F15 defends
    /// against, so an implementation must not do it and a caller must not
    /// ask for it.
    #[error("spool root is not usable, file receive disabled: {reason}")]
    UnsafeRoot {
        /// Diagnostic detail (FR-7.3); never file contents (FR-7.4).
        reason: String,
    },

    /// The entry name is not one this boundary represents.
    ///
    /// Entry names are locally generated, so this is a programming error
    /// rather than hostile input — but it is enforced at the boundary
    /// anyway, because the "bare name relative to the open root" rule is
    /// what stops a name from ever behaving like a path (F15).
    #[error("invalid spool entry name: {reason}")]
    InvalidName {
        /// Why the name was refused.
        reason: String,
    },

    /// [`SpoolStorage::create_entry`] found the name already taken.
    ///
    /// Creation is exclusive so a partial transfer can never adopt an
    /// existing entry's identity (F8 discipline, ADR 0015).
    #[error("spool entry already exists: {name}")]
    AlreadyExists {
        /// The entry name.
        name: String,
    },

    /// No such entry in the root.
    ///
    /// Returned by reads and renames. **Not** by
    /// [`SpoolStorage::unlink_entry`], which is idempotent.
    #[error("spool entry not found: {name}")]
    NotFound {
        /// The entry name.
        name: String,
    },

    /// The OS refused or failed the operation.
    #[error("spool storage failure: {reason}")]
    Backend {
        /// Diagnostic detail (FR-7.3); never file contents (FR-7.4).
        reason: String,
    },
}

/// One object found in the spool root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolEntry {
    /// Its bare name in the root. Never a path.
    pub name: String,
    /// Its length in bytes — what the spool's byte budget is computed
    /// from. Zero for anything that is not a file.
    pub len: u64,
    /// Whether it is a plain file, i.e. something this boundary can open
    /// and unlink.
    ///
    /// `false` for a directory (a junction planted in the root included).
    /// Such objects are *reported* so they can be diagnosed and are never
    /// descended into or followed: F15 forbids a recursive tree delete
    /// precisely because one from a high-integrity process, through a
    /// junction an unprivileged process may create, is an arbitrary-file
    /// delete.
    pub is_file: bool,
}

/// What a [`SpoolStorage::sweep`] did, so the caller can log it.
///
/// NFR-3: content vanishing from the spool is a diagnosable event, not a
/// silent tidy-up.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpoolSweep {
    /// Names unlinked.
    pub removed: Vec<String>,
    /// Total bytes those entries occupied.
    pub removed_bytes: u64,
    /// Names still in the root afterwards: subdirectories the sweep will
    /// not recurse into, and entries whose unlink failed. A non-empty
    /// list after a startup sweep means something else is writing to the
    /// spool root, which is worth a warning.
    pub retained: Vec<String>,
}

/// A Crossover-owned spool directory, already opened and verified.
///
/// Constructing an implementation is what resolves the root; from then on
/// the value *is* the open root, and no method takes a path.
///
/// Semantics implementations must uphold:
///
/// - Every operation is performed relative to the handle established at
///   construction. The configured path is **never** resolved again — not
///   to enumerate, not to open, and above all not to delete (F15).
/// - `name` is validated with [`validate_entry_name`] before it reaches
///   the OS.
/// - `create_entry` is exclusive: an existing name is
///   [`SpoolError::AlreadyExists`], never a truncation of what is there.
/// - `unlink_entry` is idempotent: unlinking an absent entry succeeds.
/// - Nothing outside the root is ever created, opened, renamed, or
///   unlinked, and no reparse point is ever followed.
/// - Unlinking never recurses. A directory in the root is left alone and
///   reported.
pub trait SpoolStorage: Send + Sync {
    /// Where the root is, **for comparison and diagnostics only**.
    ///
    /// The one caller that needs it is ADR 0015's sender-side loop guard:
    /// a `CF_HDROP` naming something inside the spool is a copy of what we
    /// delivered and must never be staged back to the peer (SECURITY.md
    /// F13). Answering that needs the root's *name*, which is why this
    /// exists at all.
    ///
    /// It is emphatically **not** a way to reach the spool. Every
    /// operation on this trait goes through the opened handle, and
    /// re-resolving this path to open, enumerate, or unlink anything is
    /// the precise bug F15 exists to prevent — a handle-relative delete
    /// cannot be redirected by a junction planted after the check, and a
    /// path-resolved one can. `None` where the concept does not apply.
    fn root_path(&self) -> Option<&Path> {
        None
    }

    /// Everything currently in the root, enumerated through the open
    /// handle.
    ///
    /// # Errors
    ///
    /// [`SpoolError::Backend`] if the OS enumeration fails, or if the
    /// root holds more than [`MAX_SPOOL_ENUMERATED_OBJECTS`] objects —
    /// failing closed, because a partial listing would make
    /// [`SpoolStorage::sweep`] quietly incomplete.
    fn entries(&self) -> Result<Vec<SpoolEntry>, SpoolError>;

    /// Create `name` in the root, exclusively, and return it open for
    /// writing.
    ///
    /// # Errors
    ///
    /// [`SpoolError::InvalidName`], [`SpoolError::AlreadyExists`], or
    /// [`SpoolError::Backend`].
    fn create_entry(&self, name: &str) -> Result<File, SpoolError>;

    /// Open `name` in the root for reading — the render path's only way
    /// to reach spooled bytes.
    ///
    /// # Errors
    ///
    /// [`SpoolError::InvalidName`], [`SpoolError::NotFound`], or
    /// [`SpoolError::Backend`] — including when the name resolves to
    /// something that is not a plain file.
    fn open_entry(&self, name: &str) -> Result<File, SpoolError>;

    /// Unlink `name` from the root, relative to the open handle.
    ///
    /// Idempotent: an absent entry succeeds, so abort cleanup and
    /// eviction can both run without racing each other into an error.
    /// Never recursive and never follows a link — it unlinks the name in
    /// this root and nothing else (F15).
    ///
    /// # Errors
    ///
    /// [`SpoolError::InvalidName`], or [`SpoolError::Backend`] if the OS
    /// refuses the unlink (a directory, or an entry held open without
    /// delete sharing).
    fn unlink_entry(&self, name: &str) -> Result<(), SpoolError>;

    /// Rename `from` to `to` within the root — how a verified `.part`
    /// becomes an advertisable `.bin` without either name ever being a
    /// path.
    ///
    /// Never replaces an existing `to`: registration must not be able to
    /// overwrite an entry already advertised.
    ///
    /// # Errors
    ///
    /// [`SpoolError::InvalidName`], [`SpoolError::NotFound`] for an
    /// absent `from`, [`SpoolError::AlreadyExists`] for an occupied `to`,
    /// or [`SpoolError::Backend`].
    fn rename_entry(&self, from: &str, to: &str) -> Result<(), SpoolError>;

    /// Bytes this process may still write to the volume the root lives
    /// on, **as the caller** — quota included where the platform has one.
    ///
    /// Answered from the open root handle like everything else here, never
    /// by re-resolving the configured path (F15).
    ///
    /// It exists because admission has to be decided *before* a transfer
    /// starts rather than discovered partway through it (ADR 0015): a
    /// receiver that accepted first and ran out of room later would have
    /// spent the sender's bytes to learn what it could have answered in one
    /// frame, and would have left a partial behind to prove it. The
    /// caller's rule is free space against `content_length` plus a margin,
    /// so the answer is deliberately the *usable* figure and not the
    /// volume's raw free space.
    ///
    /// # Errors
    ///
    /// [`SpoolError::Backend`] if the volume cannot be queried, or
    /// [`SpoolError::Unsupported`] where there is no spool at all. Either
    /// way the caller declines the transfer: an unknown amount of room is
    /// not a reason to start writing.
    fn free_bytes(&self) -> Result<u64, SpoolError>;

    /// Remove every entry in the root — the startup purge (ADR 0015: a
    /// virtual file list does not survive the process that published it,
    /// so nothing from a previous run is reachable and reconciling an
    /// on-disk index against orphans is a surface worth not having).
    ///
    /// Also the collector the entry-lifetime rule leans on: entries from
    /// a previous run cannot correspond to the current clipboard, so they
    /// go unconditionally.
    ///
    /// The default implementation is deliberately written once, here, in
    /// terms of [`SpoolStorage::entries`] and
    /// [`SpoolStorage::unlink_entry`]: sweeping is policy, and the policy
    /// — one unlink per entry, no recursion, non-files reported rather
    /// than removed — must not be restated (and possibly relaxed) by each
    /// platform.
    ///
    /// # Errors
    ///
    /// [`SpoolError::Backend`] if the root cannot be enumerated at all.
    /// A failure to unlink an individual entry is reported in
    /// [`SpoolSweep::retained`] rather than aborting the sweep — one
    /// stuck entry must not leave the rest of a previous run's bytes on
    /// disk.
    fn sweep(&self) -> Result<SpoolSweep, SpoolError> {
        let mut report = SpoolSweep::default();
        for entry in self.entries()? {
            if !entry.is_file {
                report.retained.push(entry.name);
                continue;
            }
            match self.unlink_entry(&entry.name) {
                Ok(()) => {
                    report.removed_bytes = report.removed_bytes.saturating_add(entry.len);
                    report.removed.push(entry.name);
                }
                Err(_) => report.retained.push(entry.name),
            }
        }
        Ok(report)
    }
}

/// Windows device names, which are reserved as *path components* whatever
/// their extension. Matched case-insensitively, with or without one.
const RESERVED_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate a spool entry name: reject, never repair.
///
/// A conforming name is 1..=[`MAX_SPOOL_ENTRY_NAME_BYTES`] bytes of
/// `[A-Za-z0-9._-]`, starts alphanumeric, contains no `..`, does not end
/// in `.`, and is not a Windows reserved device name.
///
/// The charset is far stricter than any filesystem requires, and that is
/// the point. Names here are locally generated UUIDs — the peer's name
/// never becomes a filesystem name on this machine (ADR 0015) — so
/// nothing legitimate needs a separator, a colon, a wildcard, or a
/// non-ASCII character, and refusing all of them means no name this
/// boundary accepts can behave like a path, an alternate data stream, or
/// a search pattern in *any* implementation, present or future. Device
/// names are inert in a handle-relative NT open but are refused anyway,
/// so that a future path-joining backend (the drop-folder fallback ADR
/// 0015 keeps for Linux) cannot resurrect the problem.
///
/// # Errors
///
/// [`SpoolError::InvalidName`] describing which rule the name broke.
pub fn validate_entry_name(name: &str) -> Result<(), SpoolError> {
    let invalid = |reason: &str| {
        Err(SpoolError::InvalidName {
            reason: format!("{name:?}: {reason}"),
        })
    };

    if name.is_empty() {
        return invalid("empty");
    }
    if name.len() > MAX_SPOOL_ENTRY_NAME_BYTES {
        return invalid("longer than MAX_SPOOL_ENTRY_NAME_BYTES");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return invalid("characters outside [A-Za-z0-9._-]");
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return invalid("does not start with an ASCII alphanumeric");
    }
    if name.contains("..") {
        return invalid("contains \"..\"");
    }
    if name.ends_with('.') {
        return invalid("ends with '.'");
    }
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return invalid("is a Windows reserved device name");
    }
    Ok(())
}

/// A [`SpoolStorage`] for platforms with no protected spool: every
/// operation reports [`SpoolError::Unsupported`].
///
/// Deliberately not a `std::fs` fallback. An unprotected directory would
/// satisfy the *signature* while silently voiding F14's "protected since
/// written" claim, and a caller cannot tell the difference from the
/// return type. Reporting the capability absent is the honest option, and
/// matches [`UnsupportedServiceManager`](crate::UnsupportedServiceManager).
#[derive(Debug, Default, Clone, Copy)]
pub struct UnsupportedSpoolStorage;

impl SpoolStorage for UnsupportedSpoolStorage {
    fn entries(&self) -> Result<Vec<SpoolEntry>, SpoolError> {
        Err(SpoolError::Unsupported)
    }

    fn create_entry(&self, _name: &str) -> Result<File, SpoolError> {
        Err(SpoolError::Unsupported)
    }

    fn open_entry(&self, _name: &str) -> Result<File, SpoolError> {
        Err(SpoolError::Unsupported)
    }

    fn unlink_entry(&self, _name: &str) -> Result<(), SpoolError> {
        Err(SpoolError::Unsupported)
    }

    fn rename_entry(&self, _from: &str, _to: &str) -> Result<(), SpoolError> {
        Err(SpoolError::Unsupported)
    }

    fn free_bytes(&self) -> Result<u64, SpoolError> {
        Err(SpoolError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::sync::Mutex;

    use super::{
        MAX_SPOOL_ENTRY_NAME_BYTES, SpoolEntry, SpoolError, SpoolStorage, UnsupportedSpoolStorage,
        validate_entry_name,
    };

    #[test]
    fn unsupported_reports_unsupported_for_every_operation() {
        let spool = UnsupportedSpoolStorage;
        assert!(matches!(spool.entries(), Err(SpoolError::Unsupported)));
        assert!(matches!(
            spool.create_entry("a.part"),
            Err(SpoolError::Unsupported)
        ));
        assert!(matches!(
            spool.open_entry("a.bin"),
            Err(SpoolError::Unsupported)
        ));
        assert!(matches!(
            spool.unlink_entry("a.bin"),
            Err(SpoolError::Unsupported)
        ));
        assert!(matches!(
            spool.rename_entry("a.part", "a.bin"),
            Err(SpoolError::Unsupported)
        ));
        // Not "no room": no spool. A caller must not read a refusal to
        // answer as an answer of zero, or of anything else.
        assert!(matches!(spool.free_bytes(), Err(SpoolError::Unsupported)));
        // The default `sweep` must inherit the refusal rather than
        // reporting an empty, successful sweep of a spool that does not
        // exist — a caller would read that as "nothing left over".
        assert!(matches!(spool.sweep(), Err(SpoolError::Unsupported)));
    }

    #[test]
    fn the_names_the_spool_actually_uses_are_accepted() {
        for name in [
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301.part",
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301.bin",
            "a",
            &format!("{}x", "a".repeat(MAX_SPOOL_ENTRY_NAME_BYTES - 1)),
        ] {
            assert!(
                validate_entry_name(name).is_ok(),
                "name {name:?} should be accepted"
            );
        }
    }

    #[test]
    fn names_that_could_behave_like_paths_are_rejected() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "..\\escape",
            "a/b",
            "a\\b",
            "C:\\evil",
            "a.bin:stream",
            "\\\\server\\share",
            ".hidden",
            "a*",
            "a?.bin",
            "a b.bin",
            "a\0b",
            "trailing.",
            "trailing ",
            "caf\u{e9}.bin",
            "invoice\u{202e}gnp.exe",
            "con",
            "NUL.bin",
            "lpt9",
            &"x".repeat(MAX_SPOOL_ENTRY_NAME_BYTES + 1),
        ] {
            assert!(
                matches!(
                    validate_entry_name(name),
                    Err(SpoolError::InvalidName { .. })
                ),
                "name {name:?} should be rejected"
            );
        }
    }

    /// Enough of a spool to exercise the trait's own `sweep` policy
    /// without an OS: the policy is what is under test, not the I/O.
    #[derive(Default)]
    struct FakeSpool {
        objects: Mutex<Vec<SpoolEntry>>,
        unlinkable: bool,
    }

    impl SpoolStorage for FakeSpool {
        fn entries(&self) -> Result<Vec<SpoolEntry>, SpoolError> {
            Ok(self.objects.lock().expect("fake spool poisoned").clone())
        }

        fn create_entry(&self, _name: &str) -> Result<File, SpoolError> {
            Err(SpoolError::Unsupported)
        }

        fn open_entry(&self, _name: &str) -> Result<File, SpoolError> {
            Err(SpoolError::Unsupported)
        }

        fn unlink_entry(&self, name: &str) -> Result<(), SpoolError> {
            if !self.unlinkable {
                return Err(SpoolError::Backend {
                    reason: "locked".to_owned(),
                });
            }
            self.objects
                .lock()
                .expect("fake spool poisoned")
                .retain(|e| e.name != name);
            Ok(())
        }

        fn rename_entry(&self, _from: &str, _to: &str) -> Result<(), SpoolError> {
            Err(SpoolError::Unsupported)
        }

        fn free_bytes(&self) -> Result<u64, SpoolError> {
            Err(SpoolError::Unsupported)
        }
    }

    fn object(name: &str, len: u64, is_file: bool) -> SpoolEntry {
        SpoolEntry {
            name: name.to_owned(),
            len,
            is_file,
        }
    }

    #[test]
    fn sweep_removes_files_and_refuses_to_descend_into_directories() {
        let spool = FakeSpool {
            objects: Mutex::new(vec![
                object("a.bin", 10, true),
                object("b.part", 5, true),
                object("planted", 0, false),
            ]),
            unlinkable: true,
        };

        let report = spool.sweep().unwrap();

        assert_eq!(
            report.removed,
            vec!["a.bin".to_owned(), "b.part".to_owned()]
        );
        assert_eq!(report.removed_bytes, 15);
        // The directory survives: nothing recurses, nothing follows.
        assert_eq!(report.retained, vec!["planted".to_owned()]);
        assert_eq!(spool.entries().unwrap(), vec![object("planted", 0, false)]);
    }

    #[test]
    fn sweep_reports_entries_it_could_not_remove_rather_than_aborting() {
        let spool = FakeSpool {
            objects: Mutex::new(vec![object("a.bin", 10, true), object("b.bin", 5, true)]),
            unlinkable: false,
        };

        let report = spool.sweep().unwrap();

        assert!(report.removed.is_empty());
        assert_eq!(report.removed_bytes, 0);
        assert_eq!(
            report.retained,
            vec!["a.bin".to_owned(), "b.bin".to_owned()]
        );
    }
}
