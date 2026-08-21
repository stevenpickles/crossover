//! Atomic temp-file-and-rename writes ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)),
//! shared by every writer in this crate — and, behind the `config`
//! feature, in its consumers — that persists a document a reader must
//! never see half-written: [`crate::config::persist_layout`]'s config-file
//! write and the worker's topology state-file write
//! (`apps/crossover/src/topology_state.rs`) both build on this rather than
//! keeping their own copies of the same three steps.
//!
//! **Not** shared with `crossover-platform-windows`'s `secure_storage`
//! module, which keeps its own, independent atomic-write copy. That one
//! sits at a different crate layer — a platform crate writing a
//! DPAPI-encrypted secret blob, with its own durability requirements and
//! no business depending on this crate — so duplicating a few lines of
//! `rename` logic across that boundary is the smaller risk, not a gap this
//! module is meant to close.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Where a write lands before it becomes `path`: a sibling, tagged with
/// this process's id and a per-process counter, so two writers in one
/// process — an adoption write racing an editor save, or a coalesced
/// background writer racing a shutdown's final write — can never collide
/// on the same temporary name.
#[must_use]
pub fn temp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let name = path.file_name().map_or_else(
        || String::from("crossover.tmp"),
        |name| name.to_string_lossy().into_owned(),
    );
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    directory.join(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Why an atomic write failed — one variant per step, so a diagnostic says
/// which one rather than just "it failed".
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AtomicWriteError {
    /// The containing directory could not be created.
    #[error("creating the containing directory failed")]
    CreateDirectory {
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The temporary file could not be written. The temporary file is
    /// removed (best effort) before this is returned, so a transient
    /// failure here — a full disk, a locked file — never leaves an orphan
    /// beside the target.
    #[error("writing the temporary file failed")]
    Write {
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The temporary file could not replace the target. Also cleaned up
    /// before this is returned; the target is untouched, which is the
    /// point of writing beside it first.
    #[error("replacing the target file failed")]
    Replace {
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

/// Write `contents` to `path` by atomic temp-file-and-rename, so a reader
/// sees a whole document or the previous one, never a half-written one.
/// The containing directory is created on demand.
///
/// # Errors
///
/// [`AtomicWriteError`]. Every failing step cleans up its own temporary
/// file before returning, so a failed write — however it fails — never
/// leaves a stray file beside `path`.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), AtomicWriteError> {
    if let Some(directory) = path.parent()
        && !directory.as_os_str().is_empty()
    {
        std::fs::create_dir_all(directory)
            .map_err(|source| AtomicWriteError::CreateDirectory { source })?;
    }
    let temporary = temp_path(path);
    if let Err(source) = std::fs::write(&temporary, contents) {
        let _ = std::fs::remove_file(&temporary);
        return Err(AtomicWriteError::Write { source });
    }
    if let Err(source) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(AtomicWriteError::Replace { source });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{AtomicWriteError, temp_path, write_atomic};

    /// A private directory removed on drop — the house substitute for a
    /// `tempfile` dependency.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crossover-topology-atomic-write-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("sandbox");
            Self(dir)
        }

        fn path(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stray_files(directory: &Path, keep: &str) -> Vec<String> {
        std::fs::read_dir(directory)
            .expect("read sandbox")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != keep)
            .collect()
    }

    #[test]
    fn a_write_lands_atomically_and_leaves_no_temp_file() {
        let sandbox = Sandbox::new("write");
        let path = sandbox.path("doc.json");
        write_atomic(&path, "{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert!(stray_files(&sandbox.0, "doc.json").is_empty());
    }

    #[test]
    fn a_second_write_replaces_the_first_and_still_leaves_nothing_behind() {
        let sandbox = Sandbox::new("replace");
        let path = sandbox.path("doc.json");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert!(stray_files(&sandbox.0, "doc.json").is_empty());
    }

    #[test]
    fn an_absent_directory_is_created() {
        let sandbox = Sandbox::new("create-dir");
        let path = sandbox.path("nested").join("deeper").join("doc.json");
        write_atomic(&path, "x").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    }

    /// A write that cannot land — the temporary path names a directory
    /// that does not exist and cannot be created because its parent is
    /// itself an ordinary file — cleans up rather than leaving a stray
    /// temporary file. This is the property `write_atomic` exists to
    /// guarantee even on the failure path (`AtomicWriteError::Write` and
    /// `AtomicWriteError::CreateDirectory`, both cleaned up the same way).
    #[test]
    fn a_write_failure_leaves_no_temporary_file_behind() {
        let sandbox = Sandbox::new("write-fails");
        // A regular file standing where a directory is needed: `write`
        // into a path under it must fail, and cleanly.
        let blocker = sandbox.path("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("doc.json");

        let error = write_atomic(&path, "contents").unwrap_err();
        assert!(
            matches!(
                error,
                AtomicWriteError::CreateDirectory { .. } | AtomicWriteError::Write { .. }
            ),
            "{error:?}"
        );
        // Only the blocker itself remains — no `.tmp` sibling.
        assert_eq!(stray_files(&sandbox.0, "blocker"), Vec::<String>::new());
    }

    #[test]
    fn the_temporary_file_is_a_sibling_tagged_with_this_process() {
        let path = Path::new("/some/where/doc.json");
        let temporary = temp_path(path);
        assert_eq!(temporary.parent(), path.parent());
        let name = temporary
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("doc.json."), "{name}");
        assert_eq!(temporary.extension(), Some(std::ffi::OsStr::new("tmp")));
        assert!(name.contains(&std::process::id().to_string()), "{name}");
    }

    #[test]
    fn two_calls_in_the_same_process_never_collide() {
        let path = Path::new("/some/where/doc.json");
        assert_ne!(temp_path(path), temp_path(path));
    }
}
