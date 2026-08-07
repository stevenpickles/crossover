//! DPAPI-backed [`SecureStorage`] (FR-1.1, docs/SECURITY.md §2).
//!
//! Secrets are encrypted with Windows DPAPI under the current user's
//! credentials (`CryptProtectData`) and stored as one file per key.
//!
//! Protection boundary, stated honestly: DPAPI binds ciphertext to this
//! Windows user on this machine — any process running as the same user can
//! decrypt. That satisfies "OS protections where practical" against other
//! users and offline disk access; it does not defend against same-user
//! malware, which is outside the threat model (docs/SECURITY.md §6).

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use crossover_platform::{SecureStorage, SecureStorageError};
use windows::Win32::Foundation::LocalFree;
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;

/// Fixed additional entropy mixed into every blob. Not a secret: it binds
/// ciphertext to Crossover's storage format so unrelated software using
/// DPAPI with no entropy cannot accidentally decrypt our blobs (and vice
/// versa). Versioned with the storage layout.
const APP_ENTROPY: &[u8] = b"crossover-secure-storage-v1";

/// Longest accepted storage key, in bytes.
const MAX_KEY_BYTES: usize = 128;

/// File-per-key DPAPI store rooted at a directory.
#[derive(Debug)]
pub struct DpapiSecureStorage {
    root: PathBuf,
}

impl DpapiSecureStorage {
    /// A store rooted at `root`. The directory is created on first write.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The default per-user location: `%LOCALAPPDATA%\Crossover\secure`.
    ///
    /// # Errors
    ///
    /// [`SecureStorageError::Backend`] if `%LOCALAPPDATA%` is not set —
    /// with no per-user profile there is nowhere safe to default to, so
    /// this fails rather than guessing a shared directory.
    pub fn in_default_location() -> Result<Self, SecureStorageError> {
        let local_app_data =
            std::env::var_os("LOCALAPPDATA").ok_or_else(|| SecureStorageError::Backend {
                reason: "LOCALAPPDATA is not set; cannot locate per-user secure storage".to_owned(),
            })?;
        Ok(Self::new(
            PathBuf::from(local_app_data)
                .join("Crossover")
                .join("secure"),
        ))
    }

    fn path_for(&self, key: &str) -> Result<PathBuf, SecureStorageError> {
        validate_key(key)?;
        Ok(self.root.join(format!("{key}.bin")))
    }
}

impl SecureStorage for DpapiSecureStorage {
    fn store(&self, key: &str, secret: &[u8]) -> Result<(), SecureStorageError> {
        let path = self.path_for(key)?;
        let ciphertext = dpapi_protect(secret)?;

        fs::create_dir_all(&self.root)
            .map_err(|e| backend("creating secure storage directory", &e))?;

        // Write-then-rename gives the trait's atomic-replace contract:
        // `rename` onto an existing file is atomic replacement on Windows
        // (MoveFileEx + MOVEFILE_REPLACE_EXISTING under std).
        let tmp = self.root.join(format!("{key}.{}.tmp", std::process::id()));
        fs::write(&tmp, &ciphertext).map_err(|e| backend("writing secure storage file", &e))?;
        if let Err(e) = fs::rename(&tmp, &path) {
            // Best-effort cleanup; the original error is the diagnostic.
            let _ = fs::remove_file(&tmp);
            return Err(backend("replacing secure storage file", &e));
        }
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
        let path = self.path_for(key)?;
        let ciphertext = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(backend("reading secure storage file", &e)),
        };
        dpapi_unprotect(&ciphertext).map(Some)
    }

    fn delete(&self, key: &str) -> Result<(), SecureStorageError> {
        let path = self.path_for(key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(backend("deleting secure storage file", &e)),
        }
    }
}

/// Keys become file names, so they are validated, never sanitized: a key
/// the contract cannot represent literally is rejected outright (no
/// traversal, no reserved names, no surprise collisions from escaping).
fn validate_key(key: &str) -> Result<(), SecureStorageError> {
    let starts_alphanumeric = key
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let charset_ok = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if key.is_empty() || key.len() > MAX_KEY_BYTES || !starts_alphanumeric || !charset_ok {
        return Err(SecureStorageError::Backend {
            reason: format!(
                "invalid storage key {key:?}: keys are 1..={MAX_KEY_BYTES} bytes of \
                 [A-Za-z0-9._-] starting alphanumeric"
            ),
        });
    }
    Ok(())
}

fn backend(context: &str, error: &dyn std::fmt::Display) -> SecureStorageError {
    SecureStorageError::Backend {
        reason: format!("{context}: {error}"),
    }
}

fn blob(bytes: &[u8]) -> Result<CRYPT_INTEGER_BLOB, SecureStorageError> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(|_| SecureStorageError::Backend {
            reason: "secret exceeds DPAPI size limit".to_owned(),
        })?,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

/// Copy a DPAPI output blob into owned memory, zero the original, and free
/// it with `LocalFree` (the allocator DPAPI uses).
///
/// # Safety
///
/// `out` must be a blob populated by a successful `CryptProtectData` /
/// `CryptUnprotectData` call and not yet freed.
unsafe fn take_and_free_blob(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let len = out.cbData as usize;
    // SAFETY: per the function contract, DPAPI populated `pbData` with
    // `cbData` valid bytes.
    let copy = unsafe { std::slice::from_raw_parts(out.pbData, len) }.to_vec();
    // SAFETY: same provenance as above; zeroing before free keeps secret
    // plaintext out of freed heap memory.
    unsafe { std::ptr::write_bytes(out.pbData, 0, len) };
    // SAFETY: DPAPI allocates output with LocalAlloc; the caller guarantees
    // it has not already been freed.
    unsafe { LocalFree(Some(windows::Win32::Foundation::HLOCAL(out.pbData.cast()))) };
    copy
}

fn dpapi_protect(plaintext: &[u8]) -> Result<Vec<u8>, SecureStorageError> {
    let input = blob(plaintext)?;
    let entropy = blob(APP_ENTROPY)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: all pointers reference locals that outlive the call; DPAPI
    // only reads `input`/`entropy` and writes `output` on success.
    unsafe {
        CryptProtectData(
            &raw const input,
            PCWSTR::null(),
            Some(&raw const entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    }
    .map_err(|e| backend("DPAPI encryption failed", &e))?;
    // SAFETY: `output` was populated by the successful call above.
    Ok(unsafe { take_and_free_blob(output) })
}

fn dpapi_unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, SecureStorageError> {
    let input = blob(ciphertext)?;
    let entropy = blob(APP_ENTROPY)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `dpapi_protect`; DPAPI writes `output` only on success.
    unsafe {
        CryptUnprotectData(
            &raw const input,
            None,
            Some(&raw const entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    }
    .map_err(|e| backend("DPAPI decryption failed (wrong user or tampered blob)", &e))?;
    // SAFETY: `output` was populated by the successful call above.
    Ok(unsafe { take_and_free_blob(output) })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crossover_platform::{SecureStorage, SecureStorageError};

    use super::DpapiSecureStorage;

    /// Unique per-test root under the OS temp dir, removed on drop.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crossover-dpapi-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            Self(dir)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trip_store_load_delete() {
        let root = TempRoot::new();
        let storage = DpapiSecureStorage::new(root.0.clone());

        assert_eq!(storage.load("device-identity").unwrap(), None);
        storage.store("device-identity", b"top secret").unwrap();
        assert_eq!(
            storage.load("device-identity").unwrap().as_deref(),
            Some(&b"top secret"[..])
        );
        storage.delete("device-identity").unwrap();
        assert_eq!(storage.load("device-identity").unwrap(), None);
        // Idempotent delete.
        storage.delete("device-identity").unwrap();
    }

    #[test]
    fn store_replaces_existing_value() {
        let root = TempRoot::new();
        let storage = DpapiSecureStorage::new(root.0.clone());
        storage.store("k", b"old").unwrap();
        storage.store("k", b"new").unwrap();
        assert_eq!(storage.load("k").unwrap().as_deref(), Some(&b"new"[..]));
    }

    #[test]
    fn values_persist_across_instances() {
        let root = TempRoot::new();
        DpapiSecureStorage::new(root.0.clone())
            .store("k", b"durable")
            .unwrap();
        let reopened = DpapiSecureStorage::new(root.0.clone());
        assert_eq!(
            reopened.load("k").unwrap().as_deref(),
            Some(&b"durable"[..])
        );
    }

    #[test]
    fn on_disk_bytes_are_not_plaintext() {
        let root = TempRoot::new();
        let storage = DpapiSecureStorage::new(root.0.clone());
        storage
            .store("k", b"finding this means no encryption")
            .unwrap();
        let on_disk = std::fs::read(root.0.join("k.bin")).unwrap();
        assert!(
            !on_disk
                .windows(b"finding this".len())
                .any(|w| w == b"finding this")
        );
    }

    #[test]
    fn tampered_ciphertext_fails_closed() {
        let root = TempRoot::new();
        let storage = DpapiSecureStorage::new(root.0.clone());
        storage.store("k", b"integrity matters").unwrap();

        let path = root.0.join("k.bin");
        let mut bytes = std::fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(
            storage.load("k"),
            Err(SecureStorageError::Backend { .. })
        ));
    }

    #[test]
    fn hostile_or_malformed_keys_are_rejected() {
        let root = TempRoot::new();
        let storage = DpapiSecureStorage::new(root.0.clone());
        for key in [
            "",
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "..\\escape",
            "nul:",
            &"x".repeat(129),
        ] {
            assert!(
                matches!(
                    storage.store(key, b"x"),
                    Err(SecureStorageError::Backend { .. })
                ),
                "key {key:?} should be rejected"
            );
        }
        // The keys real callers use are accepted.
        storage.store("device-identity", b"x").unwrap();
    }
}
