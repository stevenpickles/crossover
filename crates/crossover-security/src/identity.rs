//! Persistent device identity (FR-1.1, ADR 0003).
//!
//! Each installation owns an Ed25519 keypair; the canonical peer identity
//! is the SHA-256 fingerprint of the key's SPKI (`SubjectPublicKeyInfo`)
//! encoding — a *key* identity, never a certificate identity. The private
//! key is persisted only through the [`SecureStorage`] trait and never
//! appears in logs or `Debug` output (docs/SECURITY.md invariant 6).

use core::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crossover_platform::{SecureStorage, SecureStorageError};
use ed25519_dalek::pkcs8::EncodePublicKey;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

/// Key under which the identity blob lives in [`SecureStorage`].
pub const IDENTITY_STORAGE_KEY: &str = "device-identity";

/// Bound on the human-readable device name (bytes of UTF-8). Bounded like
/// every variable-length value in the system (NFR-1); the wire protocol
/// will enforce its own limit no larger than this.
pub const MAX_DEVICE_NAME_BYTES: usize = 64;

/// Version of the at-rest identity blob. Independent of the wire protocol
/// version; bumped only when the stored layout changes.
const STORED_FORMAT_VERSION: u8 = 1;

/// Failures in identity generation, persistence, or loading.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum IdentityError {
    /// The device name is empty or exceeds [`MAX_DEVICE_NAME_BYTES`].
    #[error("device name must be 1..={MAX_DEVICE_NAME_BYTES} UTF-8 bytes, got {got}")]
    InvalidDeviceName { got: usize },

    /// The platform secure-storage backend failed.
    #[error(transparent)]
    Storage(#[from] SecureStorageError),

    /// A stored blob exists but cannot be decoded. Distinguished from
    /// [`IdentityError::UnsupportedFormatVersion`] so diagnostics separate
    /// "corrupt" from "written by a newer Crossover".
    #[error("stored identity is corrupt: {reason}")]
    Corrupt { reason: String },

    /// The stored blob's format version is newer than this build supports.
    #[error("stored identity format version {found} is unsupported (max {max})")]
    UnsupportedFormatVersion { found: u8, max: u8 },

    /// The OS random source failed — identity generation must not proceed
    /// with weak entropy (fail closed).
    #[error("random generation failed: {reason}")]
    Randomness { reason: String },

    /// SPKI encoding of the public key failed.
    #[error("SPKI encoding failed: {reason}")]
    SpkiEncoding { reason: String },

    /// Encoding the identity blob for storage failed.
    #[error("encoding identity for storage failed: {reason}")]
    Encode { reason: String },
}

/// SHA-256 fingerprint of the identity key's SPKI DER encoding (ADR 0003).
///
/// This is the value the trust store pins and diagnostics display.
/// Serializable for trust-store persistence; contains no secret material.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpkiFingerprint(pub(crate) [u8; 32]);

impl SpkiFingerprint {
    /// Raw fingerprint bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for SpkiFingerprint {
    /// Wrap raw bytes (e.g. a fingerprint received in a pairing
    /// confirmation). Constructing a fingerprint asserts nothing — trust
    /// exists only once the value is pinned in the trust store after
    /// verification.
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for SpkiFingerprint {
    /// Lowercase hex, 64 characters.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SpkiFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpkiFingerprint({self})")
    }
}

/// At-rest layout, format version 1. Encoded with postcard; the leading
/// `format_version` byte is validated before full decoding so a newer
/// layout fails with a version error, not a decode error.
#[derive(Serialize, Deserialize)]
struct StoredIdentityV1 {
    format_version: u8,
    device_id: Uuid,
    device_name: String,
    created_at_unix: u64,
    secret_seed: [u8; 32],
}

impl Drop for StoredIdentityV1 {
    fn drop(&mut self) {
        self.secret_seed.zeroize();
    }
}

/// This installation's persistent cryptographic identity.
///
/// `Clone` exists for the composition root, which hands one identity to
/// concurrent roles (listener and supervisor); clones share nothing
/// mutable, and the signing key clones as key material in process memory
/// either way.
#[derive(Clone)]
pub struct DeviceIdentity {
    device_id: Uuid,
    device_name: String,
    created_at_unix: u64,
    signing_key: SigningKey,
}

impl DeviceIdentity {
    /// Generate a fresh identity with a random device id and keypair.
    ///
    /// Generation is explicit: replacing an existing identity invalidates
    /// every pairing pinned to the old key, so callers use
    /// [`DeviceIdentity::load_or_generate`] for normal startup.
    ///
    /// # Errors
    ///
    /// [`IdentityError::InvalidDeviceName`] for an empty or oversized name;
    /// [`IdentityError::Randomness`] if the OS random source fails.
    pub fn generate(device_name: &str) -> Result<Self, IdentityError> {
        validate_device_name(device_name)?;
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| IdentityError::Randomness {
            reason: e.to_string(),
        })?;
        let signing_key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Ok(Self {
            device_id: Uuid::new_v4(),
            device_name: device_name.to_owned(),
            created_at_unix: unix_now(),
            signing_key,
        })
    }

    /// Load the persisted identity, or generate-and-persist a fresh one if
    /// none exists. Returns the identity and whether it was freshly
    /// generated (callers log that transition — FR-7.3).
    ///
    /// If an identity is already stored, `device_name` is ignored in favor
    /// of the stored name: renaming is a future explicit operation, not a
    /// startup side effect.
    ///
    /// # Errors
    ///
    /// Propagates storage, decoding, validation, and generation failures.
    pub fn load_or_generate(
        storage: &dyn SecureStorage,
        device_name: &str,
    ) -> Result<(Self, bool), IdentityError> {
        if let Some(identity) = Self::load(storage)? {
            return Ok((identity, false));
        }
        let identity = Self::generate(device_name)?;
        identity.save(storage)?;
        Ok((identity, true))
    }

    /// Load the persisted identity, or `Ok(None)` if none is stored.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Storage`] on backend failure;
    /// [`IdentityError::UnsupportedFormatVersion`] for a blob written by a
    /// newer layout; [`IdentityError::Corrupt`] for an undecodable blob.
    pub fn load(storage: &dyn SecureStorage) -> Result<Option<Self>, IdentityError> {
        let Some(bytes) = storage.load(IDENTITY_STORAGE_KEY)? else {
            return Ok(None);
        };
        // postcard encodes the leading u8 as a raw byte: check the format
        // version before attempting to decode the full (newer?) layout.
        match bytes.first() {
            None => {
                return Err(IdentityError::Corrupt {
                    reason: "empty identity blob".to_owned(),
                });
            }
            Some(&version) if version != STORED_FORMAT_VERSION => {
                return Err(IdentityError::UnsupportedFormatVersion {
                    found: version,
                    max: STORED_FORMAT_VERSION,
                });
            }
            Some(_) => {}
        }
        let stored: StoredIdentityV1 =
            postcard::from_bytes(&bytes).map_err(|e| IdentityError::Corrupt {
                reason: e.to_string(),
            })?;
        validate_device_name(&stored.device_name)?;
        Ok(Some(Self {
            device_id: stored.device_id,
            device_name: stored.device_name.clone(),
            created_at_unix: stored.created_at_unix,
            signing_key: SigningKey::from_bytes(&stored.secret_seed),
        }))
    }

    /// Persist this identity through the secure-storage boundary.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Encode`] if blob encoding fails;
    /// [`IdentityError::Storage`] on backend failure.
    pub fn save(&self, storage: &dyn SecureStorage) -> Result<(), IdentityError> {
        let stored = StoredIdentityV1 {
            format_version: STORED_FORMAT_VERSION,
            device_id: self.device_id,
            device_name: self.device_name.clone(),
            created_at_unix: self.created_at_unix,
            secret_seed: self.signing_key.to_bytes(),
        };
        let mut bytes = postcard::to_stdvec(&stored).map_err(|e| IdentityError::Encode {
            reason: e.to_string(),
        })?;
        let result = storage.store(IDENTITY_STORAGE_KEY, &bytes);
        bytes.zeroize();
        result.map_err(IdentityError::from)
    }

    /// Random, stable device UUID (distinct from the key identity; used for
    /// human-facing bookkeeping, never for authentication).
    #[must_use]
    pub fn device_id(&self) -> Uuid {
        self.device_id
    }

    /// Human-readable device name.
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Creation time as Unix seconds.
    #[must_use]
    pub fn created_at_unix(&self) -> u64 {
        self.created_at_unix
    }

    /// The public half of the identity keypair.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Crate-internal access to the private key, for the TLS layer to
    /// derive a PKCS#8 credential (`tls` module). Never exposed publicly.
    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// The canonical identity: SHA-256 over the SPKI DER encoding of the
    /// public key (ADR 0003). Stable across certificate regeneration.
    ///
    /// # Errors
    ///
    /// [`IdentityError::SpkiEncoding`] if DER encoding fails.
    pub fn spki_fingerprint(&self) -> Result<SpkiFingerprint, IdentityError> {
        let spki_der =
            self.verifying_key()
                .to_public_key_der()
                .map_err(|e| IdentityError::SpkiEncoding {
                    reason: e.to_string(),
                })?;
        let digest: [u8; 32] = Sha256::digest(spki_der.as_bytes()).into();
        Ok(SpkiFingerprint(digest))
    }
}

/// Manual `Debug`: never expose key material (docs/SECURITY.md invariant 6).
impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .field("created_at_unix", &self.created_at_unix)
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

pub(crate) fn validate_device_name(name: &str) -> Result<(), IdentityError> {
    if name.is_empty() || name.len() > MAX_DEVICE_NAME_BYTES {
        return Err(IdentityError::InvalidDeviceName { got: name.len() });
    }
    Ok(())
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;

    use crossover_platform::SecureStorage;
    use crossover_platform::fakes::InMemorySecureStorage;

    use super::{DeviceIdentity, IDENTITY_STORAGE_KEY, IdentityError, MAX_DEVICE_NAME_BYTES};

    #[test]
    fn generate_validates_device_name_bounds() {
        assert!(matches!(
            DeviceIdentity::generate(""),
            Err(IdentityError::InvalidDeviceName { got: 0 })
        ));
        let oversized = "x".repeat(MAX_DEVICE_NAME_BYTES + 1);
        assert!(matches!(
            DeviceIdentity::generate(&oversized),
            Err(IdentityError::InvalidDeviceName { .. })
        ));
        assert!(DeviceIdentity::generate("workstation-left").is_ok());
    }

    #[test]
    fn fingerprint_is_stable_lowercase_hex() {
        let identity = DeviceIdentity::generate("machine").unwrap();
        let a = identity.spki_fingerprint().unwrap();
        let b = identity.spki_fingerprint().unwrap();
        assert_eq!(a, b);
        let hex = a.to_string();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    /// The identity is the SPKI fingerprint (ADR 0003), so its bytes are
    /// a compatibility surface, not an implementation detail: if a
    /// dependency upgrade changes how the key is DER-encoded, every
    /// paired device stops recognising this one. A fixed key pinned to
    /// its known fingerprint turns that into a test failure instead of a
    /// field report.
    #[test]
    fn spki_fingerprint_is_stable_for_a_known_key() {
        // A fixed, non-secret seed: this key exists only in this test.
        let signing = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        let identity = DeviceIdentity {
            device_id: uuid::Uuid::from_bytes([0x01; 16]),
            device_name: "fixture".to_owned(),
            created_at_unix: 0,
            signing_key: signing,
        };
        assert_eq!(
            identity.spki_fingerprint().unwrap().to_string(),
            "9a82517f9af19416d98fdbcf193726b3a95c0b6fec1d51884bf3e1b739ba2ef4",
            "SPKI encoding changed: every existing pairing would break"
        );
    }

    #[test]
    fn distinct_identities_have_distinct_fingerprints() {
        let a = DeviceIdentity::generate("a").unwrap();
        let b = DeviceIdentity::generate("b").unwrap();
        assert_ne!(a.spki_fingerprint().unwrap(), b.spki_fingerprint().unwrap());
        assert_ne!(a.device_id(), b.device_id());
    }

    #[test]
    fn save_load_round_trips_every_field() {
        let storage = InMemorySecureStorage::new();
        let original = DeviceIdentity::generate("workstation-left").unwrap();
        original.save(&storage).unwrap();

        let loaded = DeviceIdentity::load(&storage).unwrap().unwrap();
        assert_eq!(loaded.device_id(), original.device_id());
        assert_eq!(loaded.device_name(), original.device_name());
        assert_eq!(loaded.created_at_unix(), original.created_at_unix());
        assert_eq!(
            loaded.spki_fingerprint().unwrap(),
            original.spki_fingerprint().unwrap()
        );
    }

    #[test]
    fn load_on_empty_storage_is_none_not_error() {
        let storage = InMemorySecureStorage::new();
        assert!(DeviceIdentity::load(&storage).unwrap().is_none());
    }

    #[test]
    fn load_or_generate_persists_once_then_reloads() {
        let storage = InMemorySecureStorage::new();
        let (first, generated) = DeviceIdentity::load_or_generate(&storage, "machine").unwrap();
        assert!(generated);

        // Second call loads the same identity — including with a different
        // requested name, which must not silently rename or regenerate.
        let (second, generated) = DeviceIdentity::load_or_generate(&storage, "other-name").unwrap();
        assert!(!generated);
        assert_eq!(second.device_name(), "machine");
        assert_eq!(
            second.spki_fingerprint().unwrap(),
            first.spki_fingerprint().unwrap()
        );
    }

    #[test]
    fn corrupt_blob_is_an_error_not_a_panic() {
        let storage = InMemorySecureStorage::new();
        storage
            .store(IDENTITY_STORAGE_KEY, &[1, 0xFF, 0xFF, 0xFF])
            .unwrap();
        assert!(matches!(
            DeviceIdentity::load(&storage),
            Err(IdentityError::Corrupt { .. })
        ));

        storage.store(IDENTITY_STORAGE_KEY, &[]).unwrap();
        assert!(matches!(
            DeviceIdentity::load(&storage),
            Err(IdentityError::Corrupt { .. })
        ));
    }

    #[test]
    fn newer_format_version_is_rejected_distinctly() {
        let storage = InMemorySecureStorage::new();
        storage.store(IDENTITY_STORAGE_KEY, &[2, 0, 0]).unwrap();
        assert!(matches!(
            DeviceIdentity::load(&storage),
            Err(IdentityError::UnsupportedFormatVersion { found: 2, max: 1 })
        ));
    }

    #[test]
    fn storage_failures_propagate() {
        let storage = InMemorySecureStorage::new();
        storage.fail_next_operation("backend unavailable");
        assert!(matches!(
            DeviceIdentity::load_or_generate(&storage, "machine"),
            Err(IdentityError::Storage(_))
        ));
    }

    #[test]
    fn debug_output_redacts_key_material() {
        let identity = DeviceIdentity::generate("machine").unwrap();
        let debug = format!("{identity:?}");
        assert!(debug.contains("<redacted>"));
        // The seed's hex must not leak through Debug.
        let seed_hex =
            identity
                .signing_key
                .to_bytes()
                .iter()
                .fold(String::new(), |mut hex, byte| {
                    let _ = write!(hex, "{byte:02x}");
                    hex
                });
        assert!(!debug.to_lowercase().contains(&seed_hex));
    }
}
