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

/// Protects small secrets (private key material) at rest.
///
/// Semantics implementations must uphold:
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
