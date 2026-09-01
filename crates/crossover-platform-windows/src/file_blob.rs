//! Packing a Windows file selection into one offerable blob (ADR 0015,
//! "Sender side").
//!
//! What arrives here is a `CF_HDROP` observation: absolute paths on this
//! machine, exactly as the shell reported them. What leaves is one blob
//! with an exact length and an exact SHA-256 — or a typed refusal, which
//! is the answer FR-3.6 requires and the reason nothing in this module
//! returns a bare `io::Error`.
//!
//! Three Win32 decisions carry the security of the walk, and each is a
//! decision rather than a style:
//!
//! 1. **Reparse points are detected by attribute, not by
//!    `Path::is_symlink`.** A directory *junction* is a mount point,
//!    which an unprivileged process can create, and
//!    `std::fs::symlink_metadata` reports one as an ordinary directory —
//!    `FileTypeExt::is_symlink_dir` covers name-surrogate links but the
//!    attribute is the complete answer. So every entry's
//!    `FILE_ATTRIBUTE_REPARSE_POINT` is tested, and any entry that has it
//!    refuses the **whole** transfer: following it would let a copied
//!    shortcut pack arbitrary out-of-tree content or cycle forever, and
//!    skipping it would send something that is not what the user
//!    selected.
//! 2. **Metadata is read with `symlink_metadata`, never `metadata`.** The
//!    latter follows the link before we can ask whether there was one, so
//!    the check above would be asking about the target.
//! 3. **The temporary artifact is opened `FILE_FLAG_DELETE_ON_CLOSE`.**
//!    Cleanup on every refusal path is then the operating system's job
//!    rather than a `Drop` we could fail to reach — including when the
//!    process dies mid-build, which no `Drop` covers. `FILE_SHARE_DELETE`
//!    is what makes that legal while our own handle is open, and the file
//!    is never reopened by name: like the spool (F15), the blob is a
//!    handle, and the path exists only for the moment of creation.
//!
//! The bounds are checked **during** the walk — entry count before an
//! entry is added, depth before a directory is descended into, cumulative
//! bytes as they are copied — so an oversized selection is refused at the
//! point it crosses the line rather than after a 256 MiB archive has been
//! written and measured.
//!
//! Archive entries are **Stored, never deflated.** ADR 0014 settled "no
//! compression: the LAN is faster than any codec would save", ADR 0015
//! inherits it for a single file explicitly, and the same answer for
//! archive entries is what keeps the whole compression half of the zip
//! crate — aes, bzip2, zopfli, lzma, ppmd, xz, zstd — out of the
//! dependency tree. It also keeps the byte bound meaningful: with Stored
//! entries the finished archive is the walked content plus a small
//! per-entry header, so the cumulative check performed *during* the walk
//! genuinely bounds the artifact rather than bounding an input to a
//! compressor whose output size is unknown until it is written.

use std::collections::HashSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crossover_platform::{
    BlobNaming, FileBlob, FileBlobBuilder, FileBlobRefusal, MAX_ARCHIVE_DEPTH,
    MAX_CLIPBOARD_FILE_BYTES, MAX_CLIPBOARD_FILE_ENTRIES,
};
use sha2::{Digest, Sha256};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_DELETE_ON_CLOSE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ,
};
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::{CompressionMethod, result::ZipError};

/// Bytes moved per read/write turn while copying an entry.
///
/// The same order as the wire chunk: large enough that a 256 MiB item is
/// not four thousand syscalls, small enough that the builder's own memory
/// is a constant the item's size cannot influence (NFR-1).
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// The extension a packed selection travels under.
const ARCHIVE_EXTENSION: &str = ".zip";

/// Builds one blob per clipboard item from a Windows file selection.
///
/// Holds no state beyond where temporary artifacts are created, and takes
/// no handle at construction: unlike the spool, whose root must be opened
/// once and verified before anything may be written into it, this builder
/// writes into the *user's own* temp directory under a name it generates,
/// exclusively, and deletes on close. There is nothing here for a
/// same-user process to pre-create and nothing that outlives the build.
#[derive(Debug, Clone)]
pub struct WindowsFileBlobBuilder {
    temp_dir: PathBuf,
    /// The byte ceiling this builder enforces. Always
    /// [`MAX_CLIPBOARD_FILE_BYTES`] in production; a test lowers it so the
    /// cap can be crossed for real rather than asserted about.
    max_bytes: u64,
}

impl Default for WindowsFileBlobBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsFileBlobBuilder {
    /// A builder that writes its temporary artifacts to this process's
    /// temp directory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            temp_dir: std::env::temp_dir(),
            max_bytes: MAX_CLIPBOARD_FILE_BYTES as u64,
        }
    }

    /// A builder that writes its temporary artifacts to `dir`.
    ///
    /// For tests, which need to observe that a refused build leaves the
    /// directory as it found it — an assertion that cannot be made about
    /// the shared machine temp directory.
    #[must_use]
    pub fn with_temp_dir(dir: PathBuf) -> Self {
        Self {
            temp_dir: dir,
            max_bytes: MAX_CLIPBOARD_FILE_BYTES as u64,
        }
    }

    /// Create the blob's backing file: exclusive, delete-on-close, and
    /// never reopened by name.
    fn create_artifact(&self) -> Result<File, FileBlobRefusal> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);

        let name = format!(
            "crossover-blob-{}-{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        OpenOptions::new()
            // read/write are what let std accept `create_new`; the actual
            // rights come from `access_mode`, which has to name DELETE
            // explicitly because FILE_FLAG_DELETE_ON_CLOSE requires it.
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | DELETE.0)
            // Sharing delete is what makes the flag legal while we hold
            // the handle; sharing read keeps a diagnostic tool able to
            // look without being able to replace.
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE.0)
            .open(self.temp_dir.join(name))
            .map_err(|error| FileBlobRefusal::Backend {
                reason: error.to_string(),
            })
    }

    /// One file, its bytes verbatim: no archive, no compression, and its
    /// own name (ADR 0014's verbatim principle, inherited by ADR 0015).
    ///
    /// The bytes are copied into the artifact rather than sent from the
    /// source. The copy is what makes the offer honest: the offer's
    /// length and hash are fixed before a byte travels, and a user who
    /// edits or deletes the file during the transfer then changes nothing
    /// the receiver will verify. It costs one local copy of an item this
    /// design already bounds at 256 MiB, on a path ADR 0015 describes as
    /// rare.
    fn verbatim(&self, path: &Path) -> Result<FileBlob, FileBlobRefusal> {
        let proposed_name = entry_name(path)?.to_owned();
        let mut source = File::open(path).map_err(|error| unreadable(&error))?;
        let mut artifact = self.create_artifact()?;

        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        let mut copied = 0_u64;
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(|error| unreadable(&error))?;
            if read == 0 {
                break;
            }
            copied = copied.saturating_add(read as u64);
            if copied > self.max_bytes {
                return Err(FileBlobRefusal::TooLarge {
                    bytes: copied,
                    maximum: self.max_bytes,
                });
            }
            digest.update(&buffer[..read]);
            artifact
                .write_all(&buffer[..read])
                .map_err(|error| backend(&error))?;
        }
        artifact.flush().map_err(|error| backend(&error))?;
        artifact
            .seek(SeekFrom::Start(0))
            .map_err(|error| backend(&error))?;

        Ok(FileBlob {
            proposed_name,
            naming: BlobNaming::Own,
            archived: false,
            entry_count: 1,
            original_bytes: copied,
            content_length: copied,
            content_hash: digest.finalize().into(),
            content: artifact,
        })
    }

    /// A folder, or several entries, as one archive.
    fn archive(&self, selection: &[PathBuf]) -> Result<FileBlob, FileBlobRefusal> {
        let (proposed_name, naming) = archive_name(selection);

        let artifact = self.create_artifact()?;
        let mut walk = Walk {
            writer: ZipWriter::new(artifact),
            options: SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            entry_count: 0,
            original_bytes: 0,
            max_bytes: self.max_bytes,
        };

        // Top-level names have to be unique for the same reason a
        // directory's do: an archive with two entries under one name is
        // ambiguous, and guessing a suffix for the second would rename
        // something the user did not rename.
        let mut top_level = HashSet::new();
        for path in selection {
            let name = entry_name(path)?;
            if !top_level.insert(name.to_owned()) {
                return Err(FileBlobRefusal::DuplicateName);
            }
            walk.entry(path, name.to_owned(), 1)?;
        }

        let mut artifact = walk
            .writer
            .finish()
            .map_err(|error| archive_failure(&error))?;
        artifact.flush().map_err(|error| backend(&error))?;

        let (content_length, content_hash) = hash_artifact(&mut artifact)?;
        if content_length > self.max_bytes {
            return Err(FileBlobRefusal::TooLarge {
                bytes: content_length,
                maximum: self.max_bytes,
            });
        }

        Ok(FileBlob {
            proposed_name,
            naming,
            archived: true,
            entry_count: walk.entry_count,
            original_bytes: walk.original_bytes,
            content_length,
            content_hash,
            content: artifact,
        })
    }
}

impl FileBlobBuilder for WindowsFileBlobBuilder {
    fn build(&self, selection: &[PathBuf]) -> Result<FileBlob, FileBlobRefusal> {
        if selection.is_empty() {
            return Err(FileBlobRefusal::EmptySelection);
        }
        // The count is judged before anything is opened: a selection that
        // cannot travel should cost no filesystem work at all.
        if selection.len() > MAX_CLIPBOARD_FILE_ENTRIES as usize {
            return Err(FileBlobRefusal::TooManyEntries {
                maximum: MAX_CLIPBOARD_FILE_ENTRIES,
            });
        }

        // A single regular file is the verbatim case; everything else —
        // one folder, or any multiple — is one archive (ADR 0015).
        let single = selection.len() == 1;
        let first = describe(&selection[0])?;
        if single && first.is_file() {
            self.verbatim(&selection[0])
        } else {
            self.archive(selection)
        }
    }
}

/// The state one archive build carries: the writer, and every bound that
/// has to be judged before the next entry is added.
struct Walk {
    writer: ZipWriter<File>,
    options: SimpleFileOptions,
    entry_count: u32,
    original_bytes: u64,
    max_bytes: u64,
}

impl Walk {
    /// Pack `path` — a file or a directory — under `name`, at `depth`.
    fn entry(&mut self, path: &Path, name: String, depth: u32) -> Result<(), FileBlobRefusal> {
        let metadata = describe(path)?;
        self.count_entry()?;

        if metadata.is_dir() {
            // The directory entry itself is written even when it is
            // empty, so a folder's shape survives the round trip.
            self.writer
                .add_directory(name.clone(), self.options)
                .map_err(|error| archive_failure(&error))?;
            return self.children(path, &name, depth);
        }
        if !metadata.is_file() {
            return Err(FileBlobRefusal::Unreadable {
                reason: "the entry is neither a regular file nor a directory".to_owned(),
            });
        }
        self.file(path, name)
    }

    /// Descend into a directory, in a stable order.
    ///
    /// Sorted by name so the same selection produces the same bytes and
    /// therefore the same content hash: directory enumeration order is
    /// not a promise Windows makes, and an item whose identity changed
    /// between two copies of the same folder would be a puzzling thing to
    /// debug on the receiving side.
    fn children(&mut self, path: &Path, name: &str, depth: u32) -> Result<(), FileBlobRefusal> {
        if depth >= MAX_ARCHIVE_DEPTH {
            return Err(FileBlobRefusal::TooDeep {
                maximum: MAX_ARCHIVE_DEPTH,
            });
        }

        let mut children = Vec::new();
        for child in std::fs::read_dir(path).map_err(|error| unreadable(&error))? {
            let child = child.map_err(|error| unreadable(&error))?;
            children.push(child.path());
        }
        children.sort();

        for child in children {
            let child_name = format!("{name}/{}", entry_name(&child)?);
            self.entry(&child, child_name, depth + 1)?;
        }
        Ok(())
    }

    /// Copy one file's bytes into the archive, bounded as they go.
    fn file(&mut self, path: &Path, name: String) -> Result<(), FileBlobRefusal> {
        let mut source = File::open(path).map_err(|error| unreadable(&error))?;
        self.writer
            .start_file(name, self.options)
            .map_err(|error| archive_failure(&error))?;

        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(|error| unreadable(&error))?;
            if read == 0 {
                break;
            }
            // Counted from what was actually read, never from the size
            // the metadata claimed: a file that grows under the walk must
            // not be able to write past the bound.
            self.original_bytes = self.original_bytes.saturating_add(read as u64);
            if self.original_bytes > self.max_bytes {
                return Err(FileBlobRefusal::TooLarge {
                    bytes: self.original_bytes,
                    maximum: self.max_bytes,
                });
            }
            self.writer
                .write_all(&buffer[..read])
                .map_err(|error| backend(&error))?;
        }
        Ok(())
    }

    /// Admit one more entry, or refuse the item.
    fn count_entry(&mut self) -> Result<(), FileBlobRefusal> {
        if self.entry_count >= MAX_CLIPBOARD_FILE_ENTRIES {
            return Err(FileBlobRefusal::TooManyEntries {
                maximum: MAX_CLIPBOARD_FILE_ENTRIES,
            });
        }
        self.entry_count += 1;
        Ok(())
    }
}

/// Metadata for `path` **without following a link**, plus the reparse
/// check every entry is subject to.
fn describe(path: &Path) -> Result<Metadata, FileBlobRefusal> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| unreadable(&error))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(FileBlobRefusal::ReparsePoint);
    }
    Ok(metadata)
}

/// The bare name `path` should carry, as a string an archive entry and a
/// wire field can both hold.
///
/// Non-Unicode names are refused rather than lossily converted: the name
/// is what the receiving shell will show, and a replacement character is
/// a name the user never chose (reject, never repair).
fn entry_name(path: &Path) -> Result<&str, FileBlobRefusal> {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| FileBlobRefusal::Unreadable {
            reason: "the entry has no name, or a name that is not valid Unicode".to_owned(),
        })
}

/// What an archived selection is called, and how strictly (ADR 0015).
///
/// A single folder is named after itself, so a failure to validate that
/// name is a refusal. A multi-entry selection is named after its parent
/// folder — something the user did not choose for this purpose — so a
/// failure there falls back to the generic name, and the empty string
/// this returns when there is no usable parent takes the same path.
fn archive_name(selection: &[PathBuf]) -> (String, BlobNaming) {
    if selection.len() == 1 {
        let own = selection[0]
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default();
        return (format!("{own}{ARCHIVE_EXTENSION}"), BlobNaming::Own);
    }
    let parent = selection[0]
        .parent()
        .and_then(Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    if parent.is_empty() {
        return (String::new(), BlobNaming::Derived);
    }
    (format!("{parent}{ARCHIVE_EXTENSION}"), BlobNaming::Derived)
}

/// The finished artifact's length and SHA-256, leaving it positioned at
/// the start for the send path.
///
/// The hash is taken over the bytes on disk, in one bounded-buffer pass,
/// so it is the identity of exactly what will travel — the same digest
/// `crossover_protocol::clipboard::content_hash` computes over the whole
/// item and the receiver's `ChunkStream` accumulates chunk by chunk.
fn hash_artifact(artifact: &mut File) -> Result<(u64, [u8; 32]), FileBlobRefusal> {
    artifact
        .seek(SeekFrom::Start(0))
        .map_err(|error| backend(&error))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut length = 0_u64;
    loop {
        let read = artifact
            .read(&mut buffer)
            .map_err(|error| backend(&error))?;
        if read == 0 {
            break;
        }
        length = length.saturating_add(read as u64);
        digest.update(&buffer[..read]);
    }
    artifact
        .seek(SeekFrom::Start(0))
        .map_err(|error| backend(&error))?;
    Ok((length, digest.finalize().into()))
}

/// An entry of the selection could not be read. The diagnostic is the
/// operating system's message, which names the fault and not the path —
/// `std::io` does not attach one, and neither do we (FR-7.4).
fn unreadable(error: &std::io::Error) -> FileBlobRefusal {
    FileBlobRefusal::Unreadable {
        reason: error.to_string(),
    }
}

/// Our own workspace failed, rather than the selection being at fault.
fn backend(error: &std::io::Error) -> FileBlobRefusal {
    FileBlobRefusal::Backend {
        reason: error.to_string(),
    }
}

/// The archive writer failed.
///
/// Only the I/O message is carried through: the writer's other variants
/// can quote an entry name, and a file name is user data (SECURITY.md
/// invariant 6).
fn archive_failure(error: &ZipError) -> FileBlobRefusal {
    match error {
        ZipError::Io(io) => FileBlobRefusal::Backend {
            reason: io.to_string(),
        },
        _ => FileBlobRefusal::Backend {
            reason: "the archive writer refused an entry".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use crossover_platform::{
        BlobNaming, FileBlob, FileBlobBuilder, FileBlobRefusal, MAX_ARCHIVE_DEPTH,
        MAX_CLIPBOARD_FILE_ENTRIES,
    };
    use sha2::{Digest, Sha256};

    use super::WindowsFileBlobBuilder;
    use crate::test_support::Sandbox;

    impl WindowsFileBlobBuilder {
        /// A builder whose byte ceiling is low enough for a test to cross
        /// it with real bytes. The production ceiling is 256 MiB, and a
        /// test that wrote that much would be measuring the disk.
        fn with_max_bytes(dir: PathBuf, max_bytes: u64) -> Self {
            Self {
                temp_dir: dir,
                max_bytes,
            }
        }
    }

    /// A builder and the private directory its artifacts live in, so a
    /// test can assert that a build left nothing behind.
    struct Fixture {
        sandbox: Sandbox,
        temp: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let sandbox = Sandbox::new(label);
            let temp = sandbox.path("temp");
            std::fs::create_dir_all(&temp).expect("temp dir");
            Self { sandbox, temp }
        }

        fn builder(&self) -> WindowsFileBlobBuilder {
            WindowsFileBlobBuilder::with_temp_dir(self.temp.clone())
        }

        fn bounded_builder(&self, max_bytes: u64) -> WindowsFileBlobBuilder {
            WindowsFileBlobBuilder::with_max_bytes(self.temp.clone(), max_bytes)
        }

        fn path(&self, leaf: &str) -> PathBuf {
            self.sandbox.path(leaf)
        }

        fn write(&self, leaf: &str, bytes: &[u8]) -> PathBuf {
            let path = self.path(leaf);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture parent");
            }
            std::fs::write(&path, bytes).expect("fixture file");
            path
        }

        fn dir(&self, leaf: &str) -> PathBuf {
            let path = self.path(leaf);
            std::fs::create_dir_all(&path).expect("fixture dir");
            path
        }

        /// What is left in the artifact directory. The whole point of
        /// delete-on-close is that this is empty once a build is over,
        /// whether it succeeded or was refused.
        fn artifacts(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.temp)
                .expect("temp dir")
                .map(|entry| entry.expect("temp entry").path())
                .collect()
        }
    }

    /// Read the blob exactly as the send path will, and hash it exactly
    /// as the receiver will.
    fn read_back(blob: &mut FileBlob) -> Vec<u8> {
        let mut bytes = Vec::new();
        blob.content.read_to_end(&mut bytes).expect("read blob");
        bytes
    }

    #[test]
    fn a_single_file_travels_verbatim_under_its_own_name() {
        let fixture = Fixture::new("blob-verbatim");
        let content = b"quarterly numbers, unmangled".repeat(64);
        let path = fixture.write("report.bin", &content);

        let mut blob = fixture.builder().build(&[path]).expect("blob");

        assert_eq!(blob.proposed_name, "report.bin");
        assert_eq!(blob.naming, BlobNaming::Own);
        assert!(!blob.archived);
        assert_eq!(blob.entry_count, 1);
        assert_eq!(blob.original_bytes, content.len() as u64);
        assert_eq!(blob.content_length, content.len() as u64);
        // Verbatim means verbatim: the same bytes, in the same order, not
        // an archive of one entry (ADR 0014's principle, ADR 0015's rule).
        assert_eq!(read_back(&mut blob), content);
    }

    #[test]
    fn the_hash_is_the_one_the_receiver_will_verify() {
        let fixture = Fixture::new("blob-hash");
        let content = b"the identity the receiver checks".repeat(1024);
        let path = fixture.write("payload.bin", &content);

        let mut blob = fixture.builder().build(&[path]).expect("blob");
        let bytes = read_back(&mut blob);

        // The receiving side accumulates SHA-256 over the chunks it is
        // handed and compares the digest to the offer's content_hash. A
        // blob whose declared hash is not the digest of its own bytes
        // would be refused there, after the whole item had travelled.
        let expected: [u8; 32] = Sha256::digest(&bytes).into();
        assert_eq!(blob.content_hash, expected);
        assert_eq!(blob.content_length, bytes.len() as u64);
    }

    #[test]
    fn a_folder_becomes_one_archive_named_after_itself() {
        let fixture = Fixture::new("blob-folder");
        let folder = fixture.dir("designs");
        fixture.write("designs/a.txt", b"alpha");
        fixture.write("designs/nested/b.txt", b"beta");
        fixture.dir("designs/empty");

        let mut blob = fixture.builder().build(&[folder]).expect("blob");

        assert_eq!(blob.proposed_name, "designs.zip");
        assert_eq!(blob.naming, BlobNaming::Own);
        assert!(blob.archived);
        // designs, designs/a.txt, designs/empty, designs/nested,
        // designs/nested/b.txt — directories are entries too, because
        // they are what an empty folder's shape survives as.
        assert_eq!(blob.entry_count, 5);
        assert_eq!(blob.original_bytes, 9);

        let bytes = read_back(&mut blob);
        assert_eq!(blob.content_length, bytes.len() as u64);
        let expected: [u8; 32] = Sha256::digest(&bytes).into();
        assert_eq!(blob.content_hash, expected);
    }

    /// Reading an archive is banned in production (clippy.toml,
    /// docs/SECURITY.md F9) — this is the deliberate exception, because
    /// "the writer produced a real archive" is not something the writer
    /// can be asked.
    #[test]
    #[expect(
        clippy::disallowed_types,
        reason = "a test may read back what the writer produced; production never does"
    )]
    fn the_archive_is_readable_and_stores_its_entries_uncompressed() {
        let fixture = Fixture::new("blob-archive");
        let folder = fixture.dir("designs");
        fixture.write("designs/a.txt", b"alpha");
        fixture.write("designs/nested/b.txt", b"beta");

        let mut blob = fixture.builder().build(&[folder]).expect("blob");
        let bytes = read_back(&mut blob);

        let mut archive =
            zip::read::ZipArchive::new(std::io::Cursor::new(bytes)).expect("readable archive");
        let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
        assert!(names.contains(&"designs/a.txt".to_owned()), "{names:?}");
        assert!(
            names.contains(&"designs/nested/b.txt".to_owned()),
            "{names:?}"
        );

        let mut entry = archive.by_name("designs/a.txt").expect("entry");
        assert_eq!(
            entry.compression(),
            zip::CompressionMethod::Stored,
            "entries are stored, never deflated (ADR 0014's no-compression rule)"
        );
        let mut content = String::new();
        entry.read_to_string(&mut content).expect("entry bytes");
        assert_eq!(content, "alpha");
    }

    #[test]
    fn a_multi_entry_selection_is_named_after_its_parent_folder() {
        let fixture = Fixture::new("blob-multi");
        let folder = fixture.dir("invoices");
        let first = fixture.write("invoices/one.txt", b"1");
        let second = fixture.write("invoices/two.txt", b"22");

        let blob = fixture.builder().build(&[first, second]).expect("blob");

        assert_eq!(blob.proposed_name, "invoices.zip");
        // Derived: the parent was not named for this purpose, so a name
        // that fails validation upstairs falls back rather than refusing.
        assert_eq!(blob.naming, BlobNaming::Derived);
        assert!(blob.archived);
        assert_eq!(blob.entry_count, 2);
        assert_eq!(blob.original_bytes, 3);
        assert!(folder.is_dir());
    }

    #[test]
    fn an_empty_selection_is_refused_rather_than_built() {
        let fixture = Fixture::new("blob-empty");
        assert!(matches!(
            fixture.builder().build(&[]),
            Err(FileBlobRefusal::EmptySelection)
        ));
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn a_selection_of_too_many_paths_costs_no_filesystem_work() {
        let fixture = Fixture::new("blob-count-roots");
        let selection: Vec<PathBuf> = (0..=MAX_CLIPBOARD_FILE_ENTRIES)
            .map(|index| fixture.path(&format!("absent-{index}.txt")))
            .collect();

        assert!(matches!(
            fixture.builder().build(&selection),
            Err(FileBlobRefusal::TooManyEntries { .. })
        ));
        // Refused on the count alone: nothing was opened, so the absent
        // paths never became an "unreadable" answer.
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn the_entry_cap_bites_during_the_walk() {
        let fixture = Fixture::new("blob-count-walk");
        let folder = fixture.dir("many");
        for index in 0..MAX_CLIPBOARD_FILE_ENTRIES {
            fixture.write(&format!("many/{index}.txt"), b"x");
        }

        // The folder itself is an entry, so its contents cross the cap.
        assert!(matches!(
            fixture.builder().build(&[folder]),
            Err(FileBlobRefusal::TooManyEntries { .. })
        ));
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn a_selection_that_nests_too_deeply_is_refused() {
        let fixture = Fixture::new("blob-depth");
        let mut leaf = String::from("deep");
        for _ in 0..MAX_ARCHIVE_DEPTH {
            leaf.push_str("/down");
        }
        fixture.write(&format!("{leaf}/leaf.txt"), b"too far");
        let root = fixture.path("deep");

        assert!(matches!(
            fixture.builder().build(&[root]),
            Err(FileBlobRefusal::TooDeep { .. })
        ));
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn the_byte_cap_stops_a_copy_in_progress() {
        let fixture = Fixture::new("blob-bytes");
        let path = fixture.write("big.bin", &vec![7_u8; 4096]);

        let refusal = fixture
            .bounded_builder(1024)
            .build(&[path])
            .expect_err("over the cap");

        match refusal {
            FileBlobRefusal::TooLarge { bytes, maximum } => {
                assert_eq!(maximum, 1024);
                // Refused as soon as the bound was crossed, not after the
                // whole 4 KiB had been counted.
                assert!(bytes <= 4096);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn an_oversized_folder_is_refused_before_the_archive_is_finished() {
        let fixture = Fixture::new("blob-bytes-folder");
        let folder = fixture.dir("bulk");
        fixture.write("bulk/a.bin", &vec![1_u8; 2048]);
        fixture.write("bulk/b.bin", &vec![2_u8; 2048]);

        assert!(matches!(
            fixture.bounded_builder(2048).build(&[folder]),
            Err(FileBlobRefusal::TooLarge { .. })
        ));
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn an_entry_that_cannot_be_read_refuses_the_whole_selection() {
        let fixture = Fixture::new("blob-unreadable");
        let folder = fixture.dir("mixed");
        fixture.write("mixed/readable.txt", b"fine");
        let locked = fixture.write("mixed/locked.bin", b"held open exclusively");

        // Opened denying every kind of sharing: the walk's own open then
        // fails the way a file held by another application does.
        let held = File::options()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .expect("exclusive handle");

        let refusal = fixture
            .builder()
            .build(&[folder])
            .expect_err("locked entry");
        assert!(
            matches!(refusal, FileBlobRefusal::Unreadable { .. }),
            "{refusal:?}"
        );
        // A partial archive is never offered as if it were the selection,
        // so the readable sibling does not travel either.
        assert!(fixture.artifacts().is_empty());
        drop(held);
    }

    #[test]
    fn a_vanished_path_refuses_rather_than_shrinking_the_selection() {
        let fixture = Fixture::new("blob-absent");
        let present = fixture.write("here.txt", b"present");
        let absent = fixture.path("gone.txt");

        let refusal = fixture
            .builder()
            .build(&[present, absent])
            .expect_err("absent entry");
        assert!(
            matches!(refusal, FileBlobRefusal::Unreadable { .. }),
            "{refusal:?}"
        );
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn two_entries_under_one_name_are_refused_rather_than_renamed() {
        let fixture = Fixture::new("blob-duplicate");
        let first = fixture.write("left/same.txt", b"1");
        let second = fixture.write("right/same.txt", b"2");

        assert!(matches!(
            fixture.builder().build(&[first, second]),
            Err(FileBlobRefusal::DuplicateName)
        ));
        assert!(fixture.artifacts().is_empty());
    }

    /// Create a directory junction, which needs no privilege — unlike a
    /// symbolic link, which is exactly why the attribute check cannot be
    /// left to `is_symlink`.
    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn a_junction_in_the_selection_refuses_the_whole_transfer() {
        let fixture = Fixture::new("blob-junction");
        let folder = fixture.dir("shared");
        fixture.write("shared/real.txt", b"real");
        let elsewhere = fixture.dir("elsewhere");
        fixture.write("elsewhere/secret.txt", b"out of tree");

        if !junction(&fixture.path("shared/link"), &elsewhere) {
            // Some environments (a filesystem without reparse points, a
            // policy that forbids them) cannot create one. Skipping is
            // honest; asserting a refusal we never provoked is not.
            eprintln!("skipping: this environment cannot create a directory junction");
            return;
        }

        assert!(matches!(
            fixture.builder().build(&[folder]),
            Err(FileBlobRefusal::ReparsePoint)
        ));
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn a_junction_selected_directly_is_refused_too() {
        let fixture = Fixture::new("blob-junction-root");
        let elsewhere = fixture.dir("elsewhere");
        fixture.write("elsewhere/secret.txt", b"out of tree");
        let link = fixture.path("link");

        if !junction(&link, &elsewhere) {
            eprintln!("skipping: this environment cannot create a directory junction");
            return;
        }

        assert!(matches!(
            fixture.builder().build(&[link]),
            Err(FileBlobRefusal::ReparsePoint)
        ));
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn a_finished_blob_leaves_nothing_behind_when_it_is_dropped() {
        let fixture = Fixture::new("blob-cleanup");
        let path = fixture.write("keep.txt", b"transient");

        let blob = fixture.builder().build(&[path]).expect("blob");
        // While the blob is alive its artifact exists — the send path has
        // to be able to read it.
        assert_eq!(fixture.artifacts().len(), 1);

        drop(blob);
        // And it is gone the moment the handle closes, without anyone
        // remembering to delete it (FILE_FLAG_DELETE_ON_CLOSE).
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn a_name_that_is_not_valid_unicode_is_refused_rather_than_mangled() {
        // A name Windows accepts but that is not representable as UTF-8
        // cannot become a wire name, and substituting one would be a
        // repair. Built through the OS rather than assumed: if the
        // filesystem refuses the name, there is nothing to test.
        use std::os::windows::ffi::OsStringExt;

        let fixture = Fixture::new("blob-name");
        let folder = fixture.dir("odd");
        let lone_surrogate = std::ffi::OsString::from_wide(&[0xD800, 0x0061]);
        let path = folder.join(lone_surrogate);
        let Ok(mut file) = File::create(&path) else {
            eprintln!("skipping: this filesystem refuses an unpaired-surrogate name");
            return;
        };
        file.write_all(b"x").expect("write");
        drop(file);

        let refusal = fixture.builder().build(&[folder]).expect_err("odd name");
        assert!(
            matches!(refusal, FileBlobRefusal::Unreadable { .. }),
            "{refusal:?}"
        );
        assert!(fixture.artifacts().is_empty());
    }
}
