//! Keychain-backed [`SecureStorage`] (FR-1.1, docs/SECURITY.md §2; ADR 0020).
//!
//! Each secret is one generic-password item in the user's default (login)
//! keychain, addressed by a fixed service name and the storage key as the
//! account. The item's value is the secret's bytes, as given.
//!
//! Protection boundary, stated honestly, as the DPAPI backend states its
//! own: the login keychain is encrypted at rest and unlocked with the
//! user's session, and each item carries an access list naming the
//! application that created it. Another user of the Mac cannot read it;
//! another process running as the same user is asked about — or, for the
//! creating application, simply given — the item. Same-user malware is
//! outside the threat model (docs/SECURITY.md §6), as it is on Windows.
//!
//! **The access list binds to the binary's signed identity** (M-8). An
//! unsigned build is a different application after every rebuild, so
//! macOS may prompt before handing a rebuilt `crossover` the identity its
//! predecessor stored. That is verified on the lab Mac with this slice,
//! and signing is decided with packaging (ADR 0020, Consequences); a denied
//! or unanswerable prompt surfaces here as a backend error naming the
//! Keychain status, never as an absent key — absence would make the device
//! look new to its peer, which is the worst reading of a storage failure.
//!
//! The legacy file-based keychain is used deliberately rather than the
//! data-protection keychain: the latter requires a signed binary with a
//! keychain-access-group entitlement, which an unsigned development build
//! cannot have.

use std::ptr;

use crossover_platform::{SecureStorage, SecureStorageError, validate_storage_key};
use objc2_core_foundation::{CFBoolean, CFData, CFDictionary, CFRetained, CFString, CFType};
use objc2_security::{
    SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate, errSecDuplicateItem,
    errSecInteractionNotAllowed, errSecItemNotFound, errSecSuccess, kSecAttrAccount,
    kSecAttrService, kSecClass, kSecClassGenericPassword, kSecMatchLimit, kSecMatchLimitOne,
    kSecReturnData, kSecValueData,
};

/// The Keychain service every Crossover secret is filed under. Versioned
/// with the storage layout, as the DPAPI backend's entropy is.
pub const KEYCHAIN_SERVICE: &str = "com.crossover.secure-storage.v1";

/// Generic-password Keychain storage under one service name.
#[derive(Debug)]
pub struct KeychainSecureStorage {
    service: String,
}

impl KeychainSecureStorage {
    /// Storage under `service`. Tests use their own service name so they
    /// never touch — or collide with — a real identity.
    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Storage under [`KEYCHAIN_SERVICE`], in the user's login keychain.
    #[must_use]
    pub fn for_current_user() -> Self {
        Self::with_service(KEYCHAIN_SERVICE)
    }

    /// The query that identifies one item — class, service, account —
    /// plus any further `(key, value)` pairs the call needs.
    fn query(
        &self,
        key: &str,
        extra: &[(&CFString, &CFType)],
    ) -> CFRetained<CFDictionary<CFString, CFType>> {
        let service = CFString::from_str(&self.service);
        let account = CFString::from_str(key);
        // SAFETY: the `kSec…` constants are immutable statics exported by
        // Security.framework, valid for the life of the process.
        let (class_key, class, service_key, account_key) = unsafe {
            (
                kSecClass,
                kSecClassGenericPassword,
                kSecAttrService,
                kSecAttrAccount,
            )
        };
        let mut keys: Vec<&CFString> = vec![class_key, service_key, account_key];
        let mut values: Vec<&CFType> = vec![class.as_ref(), service.as_ref(), account.as_ref()];
        for (extra_key, extra_value) in extra {
            keys.push(extra_key);
            values.push(extra_value);
        }
        CFDictionary::from_slices(&keys, &values)
    }
}

impl SecureStorage for KeychainSecureStorage {
    fn store(&self, key: &str, secret: &[u8]) -> Result<(), SecureStorageError> {
        validate_storage_key(key)?;
        let data = CFData::from_bytes(secret);
        // SAFETY: immutable Security.framework constant (see `query`).
        let value_key = unsafe { kSecValueData };
        let update = CFDictionary::<CFString, CFType>::from_slices(&[value_key], &[data.as_ref()]);

        // Update in place first: SecItemUpdate replaces the value
        // atomically, which is the trait's replace contract. Delete-then-add
        // would leave a window in which a concurrent load saw no identity
        // at all.
        let query = self.query(key, &[]);
        // SAFETY: both dictionaries are valid, retained CFDictionaries for
        // the duration of the call; the function only reads them.
        let status = unsafe { SecItemUpdate(query.as_opaque(), update.as_opaque()) };
        if status == errSecSuccess {
            return Ok(());
        }
        if status != errSecItemNotFound {
            return Err(keychain_error("updating a keychain item", status));
        }

        let attributes = self.query(key, &[(value_key, data.as_ref())]);
        // SAFETY: `attributes` is a valid, retained CFDictionary for the
        // duration of the call; a null result pointer asks for no result.
        let status = unsafe { SecItemAdd(attributes.as_opaque(), ptr::null_mut()) };
        match status {
            s if s == errSecSuccess => Ok(()),
            // Another writer added the item between our update and our add.
            // Its value is not ours, so update over it — once; a second
            // collision is not a race any more.
            s if s == errSecDuplicateItem => {
                // SAFETY: as for the update above.
                let status = unsafe { SecItemUpdate(query.as_opaque(), update.as_opaque()) };
                if status == errSecSuccess {
                    Ok(())
                } else {
                    Err(keychain_error(
                        "updating a keychain item after a race",
                        status,
                    ))
                }
            }
            s => Err(keychain_error("adding a keychain item", s)),
        }
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
        validate_storage_key(key)?;
        // SAFETY: immutable Security.framework constants (see `query`).
        let (return_data, match_limit, match_one) =
            unsafe { (kSecReturnData, kSecMatchLimit, kSecMatchLimitOne) };
        let yes = CFBoolean::new(true);
        let query = self.query(
            key,
            &[
                (return_data, yes.as_ref()),
                (match_limit, match_one.as_ref()),
            ],
        );
        let mut result: *const CFType = ptr::null();
        // SAFETY: `query` is a valid, retained CFDictionary for the call,
        // and `result` is a valid out-pointer the function writes a +1
        // retained object into on success.
        let status = unsafe { SecItemCopyMatching(query.as_opaque(), &raw mut result) };
        if status == errSecItemNotFound {
            return Ok(None);
        }
        if status != errSecSuccess {
            return Err(keychain_error("reading a keychain item", status));
        }
        let Some(result) = ptr::NonNull::new(result.cast_mut()) else {
            return Err(SecureStorageError::Backend {
                reason: "reading a keychain item: success with no data".to_owned(),
            });
        };
        // SAFETY: a successful SecItemCopyMatching hands us ownership of one
        // retained reference (the "Copy" rule), which this takes over and
        // releases on drop.
        let object: CFRetained<CFType> = unsafe { CFRetained::from_raw(result) };
        let data = object
            .downcast::<CFData>()
            .map_err(|_| SecureStorageError::Backend {
                reason: "reading a keychain item: the value is not data".to_owned(),
            })?;
        Ok(Some(data.to_vec()))
    }

    fn delete(&self, key: &str) -> Result<(), SecureStorageError> {
        validate_storage_key(key)?;
        let query = self.query(key, &[]);
        // SAFETY: `query` is a valid, retained CFDictionary for the call.
        let status = unsafe { SecItemDelete(query.as_opaque()) };
        if status == errSecSuccess || status == errSecItemNotFound {
            Ok(())
        } else {
            Err(keychain_error("deleting a keychain item", status))
        }
    }
}

/// A backend error naming the Keychain status. Never the secret, and never
/// the key's value beyond what the caller already logs.
fn keychain_error(context: &str, status: i32) -> SecureStorageError {
    let meaning = match status {
        s if s == errSecInteractionNotAllowed => {
            " (the keychain needs user interaction that is not possible here — \
             locked, or asking whether this build may use an item an earlier \
             build stored; docs/platform-risks-macos.md M-8)"
        }
        _ => "",
    };
    SecureStorageError::Backend {
        reason: format!("{context}: Keychain status {status}{meaning}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use crossover_platform::SecureStorage;

    use super::KeychainSecureStorage;

    /// A service name no other test, run, or real install shares, removed
    /// again by [`Scratch`]'s drop.
    fn scratch_service() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            "com.crossover.secure-storage.test.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Storage under a scratch service, cleaning up the keys it was told
    /// about even when an assertion fails first.
    struct Scratch {
        storage: KeychainSecureStorage,
        keys: &'static [&'static str],
    }

    impl Scratch {
        fn new(keys: &'static [&'static str]) -> Self {
            Self {
                storage: KeychainSecureStorage::with_service(scratch_service()),
                keys,
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for key in self.keys {
                let _ = self.storage.delete(key);
            }
        }
    }

    #[test]
    fn round_trip_store_load_delete() {
        let scratch = Scratch::new(&["device-identity"]);
        let storage = &scratch.storage;
        assert_eq!(storage.load("device-identity").unwrap(), None);

        let secret = [0x00, 0xFF, 0x10, 0x80, 0x00];
        storage.store("device-identity", &secret).unwrap();
        assert_eq!(
            storage.load("device-identity").unwrap(),
            Some(secret.to_vec())
        );

        storage.delete("device-identity").unwrap();
        assert_eq!(storage.load("device-identity").unwrap(), None);
        // Idempotent, as the trait requires.
        storage.delete("device-identity").unwrap();
    }

    #[test]
    fn store_replaces_existing_value() {
        let scratch = Scratch::new(&["trusted-peers"]);
        let storage = &scratch.storage;
        storage.store("trusted-peers", b"first").unwrap();
        storage
            .store("trusted-peers", b"second, and longer")
            .unwrap();
        assert_eq!(
            storage.load("trusted-peers").unwrap(),
            Some(b"second, and longer".to_vec())
        );
    }

    #[test]
    fn values_persist_across_instances_and_stay_per_service() {
        let scratch = Scratch::new(&["device-identity"]);
        scratch.storage.store("device-identity", b"kept").unwrap();

        let reopened = KeychainSecureStorage::with_service(scratch.storage.service.clone());
        assert_eq!(
            reopened.load("device-identity").unwrap(),
            Some(b"kept".to_vec())
        );
        // Another service name is another store: nothing leaks across.
        let elsewhere = Scratch::new(&["device-identity"]);
        assert_eq!(elsewhere.storage.load("device-identity").unwrap(), None);
    }

    #[test]
    fn an_empty_secret_round_trips() {
        let scratch = Scratch::new(&["empty"]);
        scratch.storage.store("empty", &[]).unwrap();
        assert_eq!(scratch.storage.load("empty").unwrap(), Some(Vec::new()));
    }

    #[test]
    fn hostile_or_malformed_keys_are_rejected_before_the_keychain_is_touched() {
        let scratch = Scratch::new(&[]);
        for key in ["", "../escape", "has space", ".hidden"] {
            assert!(scratch.storage.store(key, b"x").is_err(), "{key:?} stored");
            assert!(scratch.storage.load(key).is_err(), "{key:?} loaded");
            assert!(scratch.storage.delete(key).is_err(), "{key:?} deleted");
        }
    }
}
