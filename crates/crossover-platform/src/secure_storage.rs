//! The secret-at-rest storage boundary.
//!
//! Private key material must be stored under OS protection where available
//! (FR-1.1, docs/SECURITY.md §2). Platform crates implement this trait —
//! DPAPI on Windows, Keychain/secret-service later; core and security code
//! only ever see the trait.

use thiserror::Error;

/// Failures from a [`SecureStorage`] backend.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum SecureStorageError {
    /// The platform backend rejected or failed the operation.
    ///
    /// `reason` is diagnostic text for logs (FR-7.3); it must never contain
    /// secret material.
    #[error("secure storage backend failure: {reason}")]
    Backend { reason: String },
}

/// Longest accepted storage key, in bytes — on every backend.
pub const MAX_STORAGE_KEY_BYTES: usize = 128;

/// Check `key` against the one key contract every backend shares: 1 to
/// [`MAX_STORAGE_KEY_BYTES`] bytes of `[A-Za-z0-9._-]`, starting
/// alphanumeric.
///
/// Validated, never sanitized: a key the contract cannot represent
/// literally is rejected outright — no traversal where a key becomes a file
/// name, and no surprise collisions from escaping. One rule for all
/// backends rather than each backend's own, so a key that works on Windows
/// cannot fail on macOS (or the reverse) and surface as an identity that
/// exists on one machine of a pair and not the other.
///
/// # Errors
///
/// [`SecureStorageError::Backend`] naming the rule the key broke.
pub fn validate_storage_key(key: &str) -> Result<(), SecureStorageError> {
    let starts_alphanumeric = key
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let charset_ok = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if key.is_empty() || key.len() > MAX_STORAGE_KEY_BYTES || !starts_alphanumeric || !charset_ok {
        return Err(SecureStorageError::Backend {
            reason: format!(
                "invalid storage key {key:?}: keys are 1..={MAX_STORAGE_KEY_BYTES} bytes of \
                 [A-Za-z0-9._-] starting alphanumeric"
            ),
        });
    }
    Ok(())
}

/// Protects small secrets (private key material) at rest.
///
/// Semantics implementations must uphold:
///
/// - Every key is checked with [`validate_storage_key`] before the backend
///   touches anything.
///
/// - `store` replaces any existing value under `key` atomically enough that
///   a concurrent `load` sees either the old or the new value, never a mix.
/// - `load` returns `Ok(None)` for an absent key — absence is not an error.
/// - `delete` is idempotent: deleting an absent key succeeds.
/// - Secrets are protected from other users of the machine to the degree
///   the platform allows; implementations must not silently fall back to
///   plaintext-on-disk without that being their documented contract.
pub trait SecureStorage: Send + Sync {
    /// Store `secret` under `key`, replacing any existing value.
    ///
    /// # Errors
    ///
    /// [`SecureStorageError::Backend`] if the platform backend fails.
    fn store(&self, key: &str, secret: &[u8]) -> Result<(), SecureStorageError>;

    /// Load the secret stored under `key`, or `Ok(None)` if absent.
    ///
    /// # Errors
    ///
    /// [`SecureStorageError::Backend`] if the platform backend fails —
    /// including when a value exists but cannot be decrypted for the
    /// current user.
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecureStorageError>;

    /// Delete the secret stored under `key`. Deleting an absent key is not
    /// an error.
    ///
    /// # Errors
    ///
    /// [`SecureStorageError::Backend`] if the platform backend fails.
    fn delete(&self, key: &str) -> Result<(), SecureStorageError>;
}

#[cfg(test)]
mod tests {
    use super::{MAX_STORAGE_KEY_BYTES, validate_storage_key};

    /// The keys the application actually uses, and the boundary cases of
    /// the one rule every backend applies.
    #[test]
    fn the_shared_key_rule_accepts_real_keys_and_rejects_everything_else() {
        for key in [
            "device-identity",
            "trusted-peers",
            "a",
            "v1.key_2",
            &"k".repeat(MAX_STORAGE_KEY_BYTES),
        ] {
            assert!(validate_storage_key(key).is_ok(), "{key:?} was rejected");
        }
        for key in [
            "",
            &"k".repeat(MAX_STORAGE_KEY_BYTES + 1),
            ".hidden",
            "-dash-first",
            "../escape",
            "a/b",
            r"a\b",
            "has space",
            "nul\0byte",
            "ünïcode",
        ] {
            assert!(validate_storage_key(key).is_err(), "{key:?} was accepted");
        }
    }
}
