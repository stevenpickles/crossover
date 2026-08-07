//! In-memory fakes of the platform traits.
//!
//! docs/ARCHITECTURE.md §4: every platform trait has a scriptable in-memory
//! fake so all core logic is exercisable with no OS interaction. Enabled via
//! the `fakes` feature (dev-dependencies of consuming crates) and for this
//! crate's own tests.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::secure_storage::{SecureStorage, SecureStorageError};

/// In-memory [`SecureStorage`] with scriptable fault injection.
#[derive(Debug, Default)]
pub struct InMemorySecureStorage {
    entries: Mutex<HashMap<String, Vec<u8>>>,
    /// When set, the next operation fails with this reason (then clears).
    fail_next: Mutex<Option<String>>,
}

impl InMemorySecureStorage {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the next `store`/`load`/`delete` call fail with `reason`.
    ///
    /// Supports fault-injection tests (docs/TESTING.md §1.5) without a
    /// bespoke failing mock per test.
    pub fn fail_next_operation(&self, reason: &str) {
        *lock(&self.fail_next) = Some(reason.to_owned());
    }

    fn take_injected_failure(&self) -> Result<(), SecureStorageError> {
        match lock(&self.fail_next).take() {
            Some(reason) => Err(SecureStorageError::Backend { reason }),
            None => Ok(()),
        }
    }
}

impl SecureStorage for InMemorySecureStorage {
    fn store(&self, key: &str, secret: &[u8]) -> Result<(), SecureStorageError> {
        self.take_injected_failure()?;
        lock(&self.entries).insert(key.to_owned(), secret.to_vec());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
        self.take_injected_failure()?;
        Ok(lock(&self.entries).get(key).cloned())
    }

    fn delete(&self, key: &str) -> Result<(), SecureStorageError> {
        self.take_injected_failure()?;
        lock(&self.entries).remove(key);
        Ok(())
    }
}

/// Locks a mutex, recovering from poisoning: a panicked test thread must
/// not cascade opaque failures into unrelated tests.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{InMemorySecureStorage, SecureStorage, SecureStorageError};

    #[test]
    fn store_load_delete_round_trip() {
        let storage = InMemorySecureStorage::new();
        assert_eq!(storage.load("k").unwrap(), None);

        storage.store("k", b"secret").unwrap();
        assert_eq!(storage.load("k").unwrap().as_deref(), Some(&b"secret"[..]));

        storage.store("k", b"replaced").unwrap();
        assert_eq!(
            storage.load("k").unwrap().as_deref(),
            Some(&b"replaced"[..])
        );

        storage.delete("k").unwrap();
        assert_eq!(storage.load("k").unwrap(), None);
        // Idempotent delete.
        storage.delete("k").unwrap();
    }

    #[test]
    fn injected_failure_fires_once_then_clears() {
        let storage = InMemorySecureStorage::new();
        storage.fail_next_operation("disk on fire");

        let err = storage.store("k", b"secret").unwrap_err();
        let SecureStorageError::Backend { reason } = err;
        assert_eq!(reason, "disk on fire");

        // The failure was consumed; the store is usable again.
        storage.store("k", b"secret").unwrap();
        assert!(storage.load("k").unwrap().is_some());
    }
}
