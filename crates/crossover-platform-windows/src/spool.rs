//! The protected Windows spool root (ADR 0015, docs/SECURITY.md F15).
//!
//! Three properties carry the security of this module, and each is a
//! specific Win32 decision rather than a style choice:
//!
//! 1. **An explicit security descriptor at creation, never an inherited
//!    one.** The root is created with a *protected* DACL granting only
//!    this process's user and the local administrators group, plus a
//!    mandatory integrity label with no-write-up, so a same-user process
//!    at a lower integrity can neither replace the directory nor alter an
//!    entry inside it. That is what makes F14's "verified when written,
//!    protected since" true — without it an entry's bytes could be
//!    swapped between completion-verification and a render.
//!
//! 2. **Opened once with reparse-point semantics, then verified.**
//!    `FILE_FLAG_OPEN_REPARSE_POINT` makes the open resolve to the object
//!    *at* the path rather than to whatever a junction points at, and the
//!    handle is rejected unless it is a real directory and not a reparse
//!    point. A root that fails the check disables file receive for the
//!    run; it is never deleted and recreated, because deleting whatever
//!    is sitting there is the operation being defended against.
//!
//! 3. **Every later operation is relative to that handle.** `CreateFileW`
//!    has no relative form, so entry opens go through `NtCreateFile` with
//!    `OBJECT_ATTRIBUTES::RootDirectory` set to the verified handle, and
//!    renames go through `FILE_RENAME_INFORMATION::RootDirectory`.
//!    Enumeration is `GetFileInformationByHandleEx`. Nothing here resolves
//!    the configured path a second time — the stored `PathBuf` is for
//!    diagnostics only and is deliberately never passed to an API.
//!
//! The threat this closes: a directory junction is a mount point, which
//! an unprivileged process can create (unlike a symlink). Planted where
//! the spool root is, and deleted through by a worker running at high
//! integrity (ADR 0012), it is an arbitrary-file-delete elevation of
//! privilege — the confused-deputy abuse of the worker that T11 asserts
//! is contained.

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use crossover_platform::{
    MAX_SPOOL_ENUMERATED_OBJECTS, SpoolEntry, SpoolError, SpoolStorage, validate_entry_name,
};
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
    NTCREATEFILE_CREATE_DISPOSITION, NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
    NtSetInformationFile,
};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_NO_MORE_FILES, HANDLE, HLOCAL, LocalFree, NTSTATUS,
    OBJ_CASE_INSENSITIVE, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    UNICODE_STRING,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorSacl,
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LABEL_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, DELETE, FILE_ACCESS_RIGHTS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE, SetFileInformationByHandle, WRITE_DAC, WRITE_OWNER,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

/// Access the root handle is opened with.
///
/// `FILE_TRAVERSE` is what lets the handle serve as `OBJECT_ATTRIBUTES::
/// RootDirectory` for relative opens; `FILE_LIST_DIRECTORY` is what lets
/// it be enumerated; `WRITE_DAC`/`WRITE_OWNER` are what let the explicit
/// descriptor be re-asserted on a root this process did not create.
const ROOT_ACCESS: u32 = FILE_LIST_DIRECTORY.0
    | FILE_TRAVERSE.0
    | READ_CONTROL.0
    | WRITE_DAC.0
    | WRITE_OWNER.0
    | SYNCHRONIZE.0;

/// Enumeration buffer, in bytes. Fixed and modest: the directory is read
/// in as many passes as it takes rather than sized from its contents,
/// which a local process could otherwise inflate (NFR-1).
const ENUM_BUFFER_BYTES: usize = 8 * 1024;

/// Directory entries every directory has and the spool never owns.
const DOT_ENTRIES: [&str; 2] = [".", ".."];

/// `SECURITY_MANDATORY_*_RID`, the last sub-authority of an integrity SID.
const INTEGRITY_RID_LOW: u32 = 0x1000;
const INTEGRITY_RID_MEDIUM: u32 = 0x2000;
const INTEGRITY_RID_HIGH: u32 = 0x3000;

/// A Crossover-owned spool directory, open and verified.
///
/// The handle *is* the spool: it is established once by
/// [`WindowsSpoolStorage::open_or_create`] and every subsequent operation
/// is expressed relative to it.
#[derive(Debug)]
pub struct WindowsSpoolStorage {
    root: OwnedHandle,
    /// For diagnostics only. Never resolved again — re-resolving it is
    /// precisely the bug F15 exists to prevent, so it is kept as text and
    /// passed to no API.
    display_path: PathBuf,
}

impl WindowsSpoolStorage {
    /// Open the spool root at `root`, creating it protected if it is
    /// absent, and verify it.
    ///
    /// The explicit descriptor is re-asserted on the opened handle even
    /// when the directory already existed. ADR 0015's stated check —
    /// "a real directory and not a reparse point" — passes for a
    /// directory a *lower-integrity same-user process pre-created* with a
    /// permissive DACL and no label, and such a root would satisfy the
    /// check while providing none of F15's protection. Asserting the
    /// descriptor on every open removes that window; a root whose
    /// descriptor cannot be asserted is rejected rather than used
    /// unprotected.
    ///
    /// # Errors
    ///
    /// [`SpoolError::UnsafeRoot`] if the path holds something that is not
    /// a plain directory — a file, or a junction or symlink — or if the
    /// protection cannot be applied. File receive is then disabled for
    /// the run: the root is **never** deleted and recreated.
    /// [`SpoolError::Backend`] if the OS refuses the create or the open.
    pub fn open_or_create(root: &Path) -> Result<Self, SpoolError> {
        let descriptor = ProtectedDescriptor::build()?;

        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| backend("creating the spool's parent directory", &e))?;
        }

        let wide_root = wide_nul(root.as_os_str());
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.as_ptr().0,
            bInheritHandle: false.into(),
        };
        // SAFETY: `wide_root` is NUL-terminated and outlives the call;
        // `attributes` points at a descriptor owned by `descriptor`, which
        // is still alive here.
        match unsafe { CreateDirectoryW(PCWSTR(wide_root.as_ptr()), Some(&raw const attributes)) } {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_ALREADY_EXISTS.to_hresult() => {}
            Err(error) => return Err(backend("creating the spool root", &error)),
        }

        // FILE_FLAG_OPEN_REPARSE_POINT is the load-bearing flag: without
        // it a junction planted here would be followed silently and every
        // handle-relative unlink below would land in the junction's
        // target. FILE_FLAG_BACKUP_SEMANTICS is what makes CreateFileW
        // open a directory at all.
        //
        // SAFETY: `wide_root` is NUL-terminated and outlives the call;
        // the returned handle is taken ownership of immediately.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_root.as_ptr()),
                ROOT_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(|e| backend("opening the spool root", &e))?;
        // SAFETY: CreateFileW succeeded, so `handle` is a live handle this
        // code exclusively owns; OwnedHandle closes it exactly once.
        let root_handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };

        verify_is_plain_directory(&root_handle, root)?;
        descriptor.assert_on(&root_handle, root)?;

        Ok(Self {
            root: root_handle,
            display_path: root.to_path_buf(),
        })
    }

    /// The default per-user location, `%LOCALAPPDATA%\Crossover\spool`
    /// (ARCHITECTURE.md §8 puts machine-local state there).
    ///
    /// Fixed by the build and the platform's app-data resolution — never
    /// wire-influenced, never assembled from peer input, and not a user
    /// setting (ADR 0015).
    ///
    /// # Errors
    ///
    /// As [`WindowsSpoolStorage::open_or_create`], plus
    /// [`SpoolError::Backend`] if `%LOCALAPPDATA%` is not set: with no
    /// per-user profile there is nowhere private to default to, so this
    /// fails rather than guessing a shared directory.
    pub fn in_default_location() -> Result<Self, SpoolError> {
        let local_app_data =
            std::env::var_os("LOCALAPPDATA").ok_or_else(|| SpoolError::Backend {
                reason: "LOCALAPPDATA is not set; cannot locate the per-user spool".to_owned(),
            })?;
        Self::open_or_create(
            &PathBuf::from(local_app_data)
                .join("Crossover")
                .join("spool"),
        )
    }

    /// The configured path, for diagnostics. Never re-resolved.
    #[must_use]
    pub fn display_path(&self) -> &Path {
        &self.display_path
    }

    /// Open `name` **relative to the verified root handle**.
    ///
    /// This is the single choke point through which every entry is
    /// reached. `NtCreateFile` is used because it is the only Win32 entry
    /// point that accepts a root directory handle; `CreateFileW` would
    /// require a path, and a path is what an attacker gets to swap.
    fn open_at(
        &self,
        name: &str,
        access: FILE_ACCESS_RIGHTS,
        share: FILE_SHARE_MODE,
        disposition: NTCREATEFILE_CREATE_DISPOSITION,
        options: NTCREATEFILE_CREATE_OPTIONS,
    ) -> Result<OwnedHandle, NTSTATUS> {
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        let byte_len = u16::try_from(wide.len() * 2).unwrap_or(u16::MAX);
        let object_name = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: PWSTR(wide.as_mut_ptr()),
        };
        let object_attributes = OBJECT_ATTRIBUTES {
            Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(0),
            RootDirectory: HANDLE(self.root.as_raw_handle()),
            ObjectName: &raw const object_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = HANDLE::default();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: every pointer references a local that outlives the call.
        // `object_name` borrows `wide`, which is alive for the whole
        // function; `RootDirectory` borrows `self.root`, which outlives
        // `self`. The call writes `handle` and `status_block` only.
        let status = unsafe {
            NtCreateFile(
                &raw mut handle,
                access,
                &raw const object_attributes,
                &raw mut status_block,
                None,
                FILE_ATTRIBUTE_NORMAL,
                share,
                disposition,
                options,
                None,
                0,
            )
        };
        if status.is_err() {
            return Err(status);
        }
        // SAFETY: NtCreateFile succeeded, so `handle` is a live handle
        // this code exclusively owns; OwnedHandle closes it exactly once.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }
}

impl SpoolStorage for WindowsSpoolStorage {
    fn entries(&self) -> Result<Vec<SpoolEntry>, SpoolError> {
        let mut buffer = aligned_buffer(ENUM_BUFFER_BYTES);
        let buffer_bytes = u32::try_from(ENUM_BUFFER_BYTES).unwrap_or(u32::MAX);
        let mut class = FileIdBothDirectoryRestartInfo;
        let mut entries = Vec::new();

        loop {
            // SAFETY: `buffer` is 8-byte aligned (a Vec<u64>) and
            // `buffer_bytes` is its exact byte length, which is what the
            // API is told; it writes only within that span.
            let result = unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(self.root.as_raw_handle()),
                    class,
                    buffer.as_mut_ptr().cast(),
                    buffer_bytes,
                )
            };
            if let Err(error) = result {
                if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
                    break;
                }
                return Err(backend("enumerating the spool root", &error));
            }
            class = FileIdBothDirectoryInfo;

            let mut offset = 0usize;
            loop {
                // SAFETY: the API filled `buffer` with a chain of
                // FILE_ID_BOTH_DIR_INFO records, each 8-byte aligned and
                // linked by NextEntryOffset; `offset` only ever advances
                // by that field, so it stays within the written span.
                let info = unsafe {
                    &*buffer
                        .as_ptr()
                        .cast::<FILE_ID_BOTH_DIR_INFO>()
                        .byte_add(offset)
                };
                let units = info.FileNameLength as usize / size_of::<u16>();
                // SAFETY: FileName is a trailing variable-length array of
                // `FileNameLength` bytes, reported by the same record.
                let name = String::from_utf16_lossy(unsafe {
                    std::slice::from_raw_parts((&raw const info.FileName).cast::<u16>(), units)
                });

                if !DOT_ENTRIES.contains(&name.as_str()) {
                    if entries.len() >= MAX_SPOOL_ENUMERATED_OBJECTS {
                        return Err(SpoolError::Backend {
                            reason: format!(
                                "spool root holds more than {MAX_SPOOL_ENUMERATED_OBJECTS} \
                                 objects; refusing to enumerate rather than report a partial \
                                 listing a sweep would act on"
                            ),
                        });
                    }
                    let is_file = info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0;
                    entries.push(SpoolEntry {
                        name,
                        len: if is_file {
                            u64::try_from(info.EndOfFile).unwrap_or(0)
                        } else {
                            0
                        },
                        is_file,
                    });
                }

                let next = info.NextEntryOffset as usize;
                if next == 0 {
                    break;
                }
                offset += next;
            }
        }

        Ok(entries)
    }

    fn create_entry(&self, name: &str) -> Result<File, SpoolError> {
        validate_entry_name(name)?;
        // FILE_CREATE is exclusive: an existing name collides rather than
        // being truncated, so a partial transfer can never adopt another
        // entry's identity. FILE_NON_DIRECTORY_FILE refuses a directory
        // planted under the name, and FILE_OPEN_REPARSE_POINT refuses to
        // follow a link out of the spool.
        self.open_at(
            name,
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
        )
        .map(File::from)
        .map_err(|status| status_error("creating a spool entry", name, status))
    }

    fn open_entry(&self, name: &str) -> Result<File, SpoolError> {
        validate_entry_name(name)?;
        let handle = self
            .open_at(
                name,
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            )
            .map_err(|status| status_error("opening a spool entry", name, status))?;

        // FILE_OPEN_REPARSE_POINT opened the link itself rather than its
        // target, which is what keeps the read inside the spool; serving
        // one to a render would still be serving something we did not
        // write, so it is refused outright.
        if file_attributes(&handle)? & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(SpoolError::Backend {
                reason: format!("spool entry {name} is a reparse point, not content we wrote"),
            });
        }
        Ok(File::from(handle))
    }

    fn unlink_entry(&self, name: &str) -> Result<(), SpoolError> {
        validate_entry_name(name)?;
        // FILE_OPEN_REPARSE_POINT means a planted link is unlinked as a
        // link: the name goes, its target is untouched. That, plus the
        // root being a handle rather than a path, is the whole of F15's
        // "never a recursive delete through a junction".
        let handle = match self.open_at(
            name,
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ) {
            Ok(handle) => handle,
            // Idempotent: abort cleanup and eviction must be able to race
            // each other to the same entry without either reporting a
            // failure that is really a success.
            Err(status) if is_not_found(status) => return Ok(()),
            Err(status) => {
                return Err(status_error(
                    "opening a spool entry to unlink",
                    name,
                    status,
                ));
            }
        };

        let disposition =
            windows::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: `disposition` is a live local of exactly the size given,
        // and matches the FileDispositionInfo class.
        unsafe {
            SetFileInformationByHandle(
                HANDLE(handle.as_raw_handle()),
                windows::Win32::Storage::FileSystem::FileDispositionInfo,
                (&raw const disposition).cast(),
                u32::try_from(size_of_val(&disposition)).unwrap_or(0),
            )
        }
        .map_err(|e| backend("unlinking a spool entry", &e))
    }

    fn rename_entry(&self, from: &str, to: &str) -> Result<(), SpoolError> {
        validate_entry_name(from)?;
        validate_entry_name(to)?;

        let handle = self
            .open_at(
                from,
                DELETE | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            )
            .map_err(|status| status_error("opening a spool entry to rename", from, status))?;

        // FILE_RENAME_INFORMATION carries its own RootDirectory, so the
        // destination is resolved relative to the verified root exactly
        // as the source was — the rename never touches a path either.
        // The NT form rather than Win32's `SetFileInformationByHandle`
        // with `FileRenameInfo`, which rejects a non-NULL RootDirectory
        // with ERROR_INVALID_PARAMETER; it is also the same layer the
        // relative opens above use.
        let target: Vec<u16> = to.encode_utf16().collect();
        let name_bytes = target.len() * size_of::<u16>();
        let info_bytes = size_of::<FILE_RENAME_INFORMATION>() + name_bytes;
        let mut info_buffer = aligned_buffer(info_bytes);
        {
            let info = info_buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
            // SAFETY: `info_buffer` is a Vec<u64>, so `info` is 8-byte
            // aligned, and it is at least `info_bytes` long — the struct
            // plus its trailing name array. `target` is a live local.
            unsafe {
                (*info).Anonymous.ReplaceIfExists = false;
                (*info).RootDirectory = HANDLE(self.root.as_raw_handle());
                (*info).FileNameLength = u32::try_from(name_bytes).unwrap_or(0);
                std::ptr::copy_nonoverlapping(
                    target.as_ptr(),
                    (&raw mut (*info).FileName).cast::<u16>(),
                    target.len(),
                );
            }
        }

        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: `info_buffer` is a live local of at least `info_bytes`,
        // populated above in the FileRenameInformation layout; `handle`
        // holds DELETE, which is what the rename is checked against.
        let status = unsafe {
            NtSetInformationFile(
                HANDLE(handle.as_raw_handle()),
                &raw mut status_block,
                info_buffer.as_ptr().cast(),
                u32::try_from(info_bytes).unwrap_or(0),
                FileRenameInformation,
            )
        };
        if status.is_err() {
            return Err(status_error("renaming a spool entry", to, status));
        }
        Ok(())
    }
}

/// Reject the root unless it is a real directory and not a reparse point.
///
/// The reparse-point half is the junction defence: the handle was opened
/// with `FILE_FLAG_OPEN_REPARSE_POINT`, so a junction planted at the path
/// yields a handle to the *junction*, and that is what this rejects. A
/// root that fails is left exactly as it is.
fn verify_is_plain_directory(handle: &OwnedHandle, path: &Path) -> Result<(), SpoolError> {
    let attributes = file_attributes(handle)?;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(SpoolError::UnsafeRoot {
            reason: format!(
                "{} is a reparse point (junction or symlink), not a directory Crossover created; \
                 it is left untouched — remove or rename it yourself and restart",
                path.display()
            ),
        });
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return Err(SpoolError::UnsafeRoot {
            reason: format!("{} exists but is not a directory", path.display()),
        });
    }
    Ok(())
}

fn file_attributes(handle: &OwnedHandle) -> Result<u32, SpoolError> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is live for the call and `info` is a local the API
    // fills in.
    unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle()), &raw mut info) }
        .map_err(|e| backend("querying spool file attributes", &e))?;
    Ok(info.dwFileAttributes)
}

/// The spool root's explicit security descriptor, built from SDDL.
///
/// SDDL rather than hand-assembled ACLs: the descriptor is short, and a
/// text form that can be read in a diff is worth more here than saved
/// allocations. The ACEs are inheritable (`OICI`) so that entries created
/// inside the root are protected too — F14's claim that a completed entry
/// still holds the bytes we verified rests on that, not on re-hashing.
struct ProtectedDescriptor(PSECURITY_DESCRIPTOR);

impl ProtectedDescriptor {
    fn build() -> Result<Self, SpoolError> {
        let token = current_process_token()?;
        let user = token_user_sid_string(&token)?;
        // `D:P` protects the DACL from inheriting anything from the
        // parent; the two ACEs are this user and the local administrators
        // group and nothing else. The mandatory label is `NW` — no
        // write-up — which is what stops a lower-integrity same-user
        // process replacing the directory or altering an entry.
        let sddl = match token_integrity_label(&token)? {
            Some(level) => {
                format!("D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;BA)S:(ML;OICI;NW;;;{level})")
            }
            None => format!("D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;BA)"),
        };

        let wide = wide_nul(OsStr::new(&sddl));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `wide` is NUL-terminated and outlives the call, which
        // writes `descriptor` with a LocalAlloc'd self-relative
        // descriptor on success — freed by this type's Drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .map_err(|e| backend("building the spool security descriptor", &e))?;
        Ok(Self(descriptor))
    }

    fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }

    /// Re-assert this descriptor on an already-open root.
    ///
    /// Needed because ADR 0015's "real directory, not a reparse point"
    /// check passes for a root a lower-integrity same-user process
    /// pre-created with a permissive DACL. Asserting on every open makes
    /// the protection a property of *our* run rather than of whoever
    /// happened to create the directory first.
    fn assert_on(&self, handle: &OwnedHandle, path: &Path) -> Result<(), SpoolError> {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut present = windows::core::BOOL::default();
        let mut defaulted = windows::core::BOOL::default();
        // SAFETY: `self.0` is a valid self-relative descriptor; the out
        // parameters are locals. The returned ACL pointers borrow the
        // descriptor, which outlives the SetSecurityInfo call below.
        unsafe {
            GetSecurityDescriptorDacl(self.0, &raw mut present, &raw mut dacl, &raw mut defaulted)
        }
        .map_err(|e| backend("reading the spool DACL", &e))?;
        // SAFETY: as above.
        unsafe {
            GetSecurityDescriptorSacl(self.0, &raw mut present, &raw mut sacl, &raw mut defaulted)
        }
        .map_err(|e| backend("reading the spool integrity label", &e))?;

        let mut information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        if !sacl.is_null() {
            information |= LABEL_SECURITY_INFORMATION;
        }
        // SAFETY: `handle` is live and holds WRITE_DAC | WRITE_OWNER; the
        // ACL pointers reference `self`'s descriptor, still alive here.
        let result = unsafe {
            SetSecurityInfo(
                HANDLE(handle.as_raw_handle()),
                SE_FILE_OBJECT,
                information,
                None,
                None,
                Some(dacl),
                if sacl.is_null() { None } else { Some(sacl) },
            )
        };
        if result.is_err() {
            return Err(SpoolError::UnsafeRoot {
                reason: format!(
                    "cannot apply Crossover's security descriptor to {} (Win32 error {}); \
                     refusing to use an unprotected spool",
                    path.display(),
                    result.0
                ),
            });
        }
        Ok(())
    }
}

impl Drop for ProtectedDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW, which
        // documents LocalFree as its release, and is freed exactly once.
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

fn current_process_token() -> Result<OwnedHandle, SpoolError> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle needing no
    // release; `token` is a local the call fills in on success.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
        .map_err(|e| backend("opening the process token", &e))?;
    // SAFETY: OpenProcessToken succeeded, so `token` is a live handle this
    // code exclusively owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(token.0) })
}

fn token_information(
    token: &OwnedHandle,
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
    what: &str,
) -> Result<Vec<u64>, SpoolError> {
    let mut needed = 0u32;
    // SAFETY: the probing call is told a zero-length buffer and only
    // writes `needed`; it is expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            class,
            None,
            0,
            &raw mut needed,
        )
    };
    if needed == 0 {
        return Err(SpoolError::Backend {
            reason: format!("could not size {what} from the process token"),
        });
    }
    let mut buffer = aligned_buffer(needed as usize);
    // SAFETY: `buffer` is at least `needed` bytes, which is what the call
    // is told, and it writes only within that span.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            class,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &raw mut needed,
        )
    }
    .map_err(|e| backend(&format!("reading {what} from the process token"), &e))?;
    Ok(buffer)
}

fn token_user_sid_string(token: &OwnedHandle) -> Result<String, SpoolError> {
    let buffer = token_information(token, TokenUser, "the user SID")?;
    // SAFETY: the buffer was filled by GetTokenInformation(TokenUser), so
    // it begins with a TOKEN_USER whose SID points inside it.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut text = PWSTR::null();
    // SAFETY: `sid` points into `buffer`, alive here; the call writes
    // `text` with a LocalAlloc'd string on success.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }
        .map_err(|e| backend("formatting the user SID", &e))?;
    // SAFETY: `text` is a NUL-terminated string the call just allocated.
    let owned =
        unsafe { text.to_string() }.map_err(|e| backend("decoding the formatted user SID", &e))?;
    // SAFETY: allocated by ConvertSidToStringSidW, which documents
    // LocalFree as its release, and freed exactly once.
    unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    Ok(owned)
}

/// The SDDL level string for the label to stamp on the root.
///
/// Deliberately *this process's own* integrity level, capped at High.
/// Windows refuses a label above the caller's own without
/// `SeRelabelPrivilege`, so hard-coding `HI` would make the spool
/// unusable for a non-elevated worker. ADR 0015 anticipates exactly this:
/// where the worker is not elevated there is no integrity boundary to
/// cross and the label is inert, while the DACL still applies.
fn token_integrity_label(token: &OwnedHandle) -> Result<Option<&'static str>, SpoolError> {
    let buffer = token_information(token, TokenIntegrityLevel, "the integrity level")?;
    // SAFETY: the buffer was filled by
    // GetTokenInformation(TokenIntegrityLevel), so it begins with a
    // TOKEN_MANDATORY_LABEL whose SID points inside it.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    // SAFETY: `sid` is a valid SID inside `buffer`; an integrity SID has
    // at least one sub-authority, and the level is the last one.
    let rid = unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return Ok(None);
        }
        *GetSidSubAuthority(sid, u32::from(count) - 1)
    };

    Ok(Some(if rid >= INTEGRITY_RID_HIGH {
        "HI"
    } else if rid >= INTEGRITY_RID_MEDIUM {
        "ME"
    } else if rid >= INTEGRITY_RID_LOW {
        "LW"
    } else {
        return Ok(None);
    }))
}

/// A zeroed buffer of at least `bytes`, aligned for every Win32 record
/// this module casts one into.
///
/// `Vec<u8>` would not do: `FILE_RENAME_INFO`, `TOKEN_USER`,
/// `TOKEN_MANDATORY_LABEL` and `FILE_ID_BOTH_DIR_INFO` all contain
/// pointer- or 64-bit-aligned fields, and a byte vector's alignment is
/// only 1. `Vec<u64>` gives 8, which is the strictest any of them needs.
fn aligned_buffer(bytes: usize) -> Vec<u64> {
    vec![0u64; bytes.div_ceil(size_of::<u64>())]
}

fn wide_nul(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

fn is_not_found(status: NTSTATUS) -> bool {
    // STATUS_NO_SUCH_FILE has no binding in the windows crate's Foundation
    // module; it is what a relative open of an absent name returns
    // alongside STATUS_OBJECT_NAME_NOT_FOUND.
    const STATUS_NO_SUCH_FILE: NTSTATUS = NTSTATUS(0xC000_000Fu32.cast_signed());
    status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_NO_SUCH_FILE
}

fn status_error(context: &str, name: &str, status: NTSTATUS) -> SpoolError {
    if status == STATUS_OBJECT_NAME_COLLISION {
        return SpoolError::AlreadyExists {
            name: name.to_owned(),
        };
    }
    if is_not_found(status) {
        return SpoolError::NotFound {
            name: name.to_owned(),
        };
    }
    SpoolError::Backend {
        reason: format!("{context} {name}: NTSTATUS 0x{:08X}", status.0),
    }
}

fn backend(context: &str, error: &dyn std::fmt::Display) -> SpoolError {
    SpoolError::Backend {
        reason: format!("{context}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crossover_platform::{SpoolError, SpoolStorage};
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, LABEL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };
    use windows::core::PCWSTR;

    use super::{WindowsSpoolStorage, wide_nul};

    /// A private directory to build each test's fixtures in, removed on
    /// drop.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crossover-spool-test-{}-{}",
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

    /// A directory junction — a mount point, which needs no privilege to
    /// create and is therefore the attacker's tool in T21.
    fn plant_junction(link: &Path, target: &Path) {
        let output = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .expect("running mklink");
        assert!(
            output.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::fs::symlink_metadata(link)
                .expect("junction metadata")
                .file_type()
                .is_symlink(),
            "mklink did not produce a reparse point"
        );
    }

    /// `(DACL is protected, a mandatory label is present)` for `path`.
    ///
    /// Read by path deliberately: this is a *test observing the world*,
    /// not the implementation reaching for its root a second time.
    fn protection_of(path: &Path) -> (bool, bool) {
        let wide = wide_nul(path.as_os_str());
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the out
        // parameters are locals, and `descriptor` is freed below.
        let result = unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
                None,
                None,
                Some(&raw mut dacl),
                Some(&raw mut sacl),
                &raw mut descriptor,
            )
        };
        assert!(result.is_ok(), "reading the descriptor: {}", result.0);

        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: `descriptor` was populated by the successful call above.
        unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) }
            .expect("reading descriptor control");
        let protected = control & SE_DACL_PROTECTED.0 != 0;
        let labelled = !sacl.is_null();
        // SAFETY: allocated by GetNamedSecurityInfoW, freed exactly once.
        unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
        (protected, labelled)
    }

    fn read_entry(spool: &WindowsSpoolStorage, name: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        spool
            .open_entry(name)
            .expect("opening entry")
            .read_to_end(&mut bytes)
            .expect("reading entry");
        bytes
    }

    fn names(spool: &WindowsSpoolStorage) -> Vec<String> {
        let mut names: Vec<String> = spool
            .entries()
            .expect("enumerating")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn creates_the_root_and_reopens_it_across_runs() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");

        let spool = WindowsSpoolStorage::open_or_create(&root).expect("first open");
        assert!(names(&spool).is_empty());
        drop(spool);

        // A second run finds the directory already there and must use it,
        // not fail and not recreate it.
        let reopened = WindowsSpoolStorage::open_or_create(&root).expect("second open");
        assert!(names(&reopened).is_empty());
        assert!(root.is_dir());
    }

    #[test]
    fn the_root_is_created_with_an_explicit_protected_descriptor() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");
        let _spool = WindowsSpoolStorage::open_or_create(&root).expect("open");

        let (protected, labelled) = protection_of(&root);
        assert!(
            protected,
            "the spool DACL must be protected — an inherited one is whatever the profile grants"
        );
        assert!(labelled, "the spool must carry a mandatory integrity label");
    }

    #[test]
    fn a_root_someone_else_pre_created_is_taken_over_not_trusted() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");
        // Stand in for a lower-integrity same-user process getting there
        // first: an ordinary directory with an inherited descriptor. It
        // passes ADR 0015's "real directory, not a reparse point" check
        // while providing none of F15's protection.
        std::fs::create_dir_all(&root).expect("pre-creating the root");
        assert!(!protection_of(&root).0);

        let _spool = WindowsSpoolStorage::open_or_create(&root).expect("open");

        let (protected, labelled) = protection_of(&root);
        assert!(protected, "protection must be asserted, not assumed");
        assert!(labelled);
    }

    #[test]
    fn a_junction_at_the_root_path_is_rejected_and_its_target_left_alone() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");
        let target = sandbox.path("target");
        std::fs::create_dir_all(&target).expect("target");
        let bystander = target.join("bystander.txt");
        std::fs::write(&bystander, b"an unrelated file").expect("bystander");
        plant_junction(&root, &target);

        let error = WindowsSpoolStorage::open_or_create(&root).expect_err("must be rejected");

        assert!(
            matches!(&error, SpoolError::UnsafeRoot { reason } if reason.contains("reparse point")),
            "expected UnsafeRoot, got {error:?}"
        );
        // Nothing was deleted through the junction, and the junction
        // itself was not "cleaned up" — that is the operation being
        // defended against.
        assert!(bystander.exists());
        assert!(target.is_dir());

        let _ = std::fs::remove_dir(&root);
    }

    #[test]
    fn a_file_where_the_root_belongs_is_rejected() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");
        std::fs::write(&root, b"not a directory").expect("planting a file");

        let error = WindowsSpoolStorage::open_or_create(&root).expect_err("must be rejected");

        assert!(matches!(error, SpoolError::UnsafeRoot { .. }), "{error:?}");
        assert!(root.is_file(), "the planted file must be left as it is");
    }

    #[test]
    fn entries_are_created_enumerated_read_and_unlinked_by_handle() {
        let sandbox = Sandbox::new();
        let spool = WindowsSpoolStorage::open_or_create(&sandbox.path("spool")).expect("open");

        spool
            .create_entry("aaaa.part")
            .expect("create")
            .write_all(b"spooled bytes")
            .expect("write");

        let listed = spool.entries().expect("enumerate");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "aaaa.part");
        assert_eq!(listed[0].len, "spooled bytes".len() as u64);
        assert!(listed[0].is_file);

        assert_eq!(read_entry(&spool, "aaaa.part"), b"spooled bytes");

        spool.unlink_entry("aaaa.part").expect("unlink");
        assert!(names(&spool).is_empty());
        // Idempotent: abort cleanup and eviction may both reach for it.
        spool.unlink_entry("aaaa.part").expect("second unlink");
    }

    #[test]
    fn creation_is_exclusive_and_absent_entries_report_absence() {
        let sandbox = Sandbox::new();
        let spool = WindowsSpoolStorage::open_or_create(&sandbox.path("spool")).expect("open");

        drop(spool.create_entry("aaaa.part").expect("create"));

        assert!(matches!(
            spool.create_entry("aaaa.part"),
            Err(SpoolError::AlreadyExists { .. })
        ));
        assert!(matches!(
            spool.open_entry("bbbb.bin"),
            Err(SpoolError::NotFound { .. })
        ));
    }

    #[test]
    fn a_verified_part_is_promoted_by_a_handle_relative_rename() {
        let sandbox = Sandbox::new();
        let spool = WindowsSpoolStorage::open_or_create(&sandbox.path("spool")).expect("open");

        spool
            .create_entry("aaaa.part")
            .expect("create")
            .write_all(b"verified")
            .expect("write");
        spool.rename_entry("aaaa.part", "aaaa.bin").expect("rename");

        assert_eq!(names(&spool), vec!["aaaa.bin".to_owned()]);
        assert_eq!(read_entry(&spool, "aaaa.bin"), b"verified");

        // Registration must never overwrite something already advertised.
        drop(spool.create_entry("bbbb.part").expect("create"));
        assert!(spool.rename_entry("bbbb.part", "aaaa.bin").is_err());
        assert_eq!(read_entry(&spool, "aaaa.bin"), b"verified");
    }

    #[test]
    fn names_that_are_not_bare_never_reach_the_operating_system() {
        let sandbox = Sandbox::new();
        let outside = sandbox.path("outside.txt");
        std::fs::write(&outside, b"a file beside the spool").expect("bystander");
        let spool = WindowsSpoolStorage::open_or_create(&sandbox.path("spool")).expect("open");

        for name in ["../outside.txt", "..\\outside.txt", "sub\\entry.bin", ".."] {
            assert!(
                matches!(
                    spool.create_entry(name),
                    Err(SpoolError::InvalidName { .. })
                ),
                "create {name:?}"
            );
            assert!(
                matches!(spool.open_entry(name), Err(SpoolError::InvalidName { .. })),
                "open {name:?}"
            );
            assert!(
                matches!(
                    spool.unlink_entry(name),
                    Err(SpoolError::InvalidName { .. })
                ),
                "unlink {name:?}"
            );
            assert!(
                matches!(
                    spool.rename_entry("aaaa.part", name),
                    Err(SpoolError::InvalidName { .. })
                ),
                "rename to {name:?}"
            );
        }
        assert!(outside.exists());
    }

    #[test]
    fn the_startup_sweep_removes_a_previous_runs_entries() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");

        // A previous run: a completed entry and an orphaned partial.
        let previous = WindowsSpoolStorage::open_or_create(&root).expect("open");
        previous
            .create_entry("aaaa.bin")
            .expect("create")
            .write_all(b"completed")
            .expect("write");
        previous
            .create_entry("bbbb.part")
            .expect("create")
            .write_all(b"orphan")
            .expect("write");
        drop(previous);

        // This run. A virtual file list does not survive the process that
        // published it, so nothing here can correspond to the current
        // clipboard.
        let spool = WindowsSpoolStorage::open_or_create(&root).expect("reopen");
        let report = spool.sweep().expect("sweep");

        assert_eq!(
            report.removed,
            vec!["aaaa.bin".to_owned(), "bbbb.part".to_owned()]
        );
        assert_eq!(
            report.removed_bytes,
            ("completed".len() + "orphan".len()) as u64
        );
        assert!(report.retained.is_empty());
        assert!(names(&spool).is_empty());
    }

    #[test]
    fn the_sweep_reports_a_planted_directory_instead_of_recursing_into_it() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");
        let target = sandbox.path("target");
        std::fs::create_dir_all(&target).expect("target");
        let bystander = target.join("bystander.txt");
        std::fs::write(&bystander, b"an unrelated file").expect("bystander");

        let spool = WindowsSpoolStorage::open_or_create(&root).expect("open");
        plant_junction(&root.join("planted"), &target);

        let report = spool.sweep().expect("sweep");

        assert!(report.removed.is_empty());
        assert_eq!(report.retained, vec!["planted".to_owned()]);
        // The whole point: a recursive delete from a high-integrity
        // process through a planted junction is an arbitrary-file delete.
        assert!(bystander.exists());

        let _ = std::fs::remove_dir(root.join("planted"));
    }

    #[test]
    fn operations_follow_the_open_handle_after_the_path_is_swapped_for_a_junction() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("spool");
        let moved = sandbox.path("moved");
        let target = sandbox.path("target");
        std::fs::create_dir_all(&target).expect("target");
        let decoy = target.join("aaaa.bin");
        std::fs::write(&decoy, b"someone else's file").expect("decoy");

        let spool = WindowsSpoolStorage::open_or_create(&root).expect("open");
        spool
            .create_entry("aaaa.bin")
            .expect("create")
            .write_all(b"ours")
            .expect("write");

        // The attack: move the real root aside and put a junction where
        // the configured path used to be. An implementation that
        // re-resolved `%LOCALAPPDATA%\Crossover\spool` for its next
        // delete would now delete inside `target` instead — at high
        // integrity, under the attacker's choice of target.
        std::fs::rename(&root, &moved).expect("moving the root aside");
        plant_junction(&root, &target);

        spool.unlink_entry("aaaa.bin").expect("unlink by handle");

        assert!(
            decoy.exists(),
            "the unlink followed the swapped path, not the verified handle"
        );
        assert!(!moved.join("aaaa.bin").exists(), "our own entry survived");
        assert!(names(&spool).is_empty());

        let _ = std::fs::remove_dir(&root);
    }
}
