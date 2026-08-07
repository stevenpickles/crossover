//! In-memory fakes of the platform traits.
//!
//! docs/ARCHITECTURE.md §4: every platform trait has a scriptable in-memory
//! fake so all core logic is exercisable with no OS interaction. Enabled via
//! the `fakes` feature (dev-dependencies of consuming crates) and for this
//! crate's own tests.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::clipboard::{ClipboardError, ClipboardListener, ClipboardProvider};
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

/// Which fake-clipboard operation an injected failure applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOp {
    /// Fail upcoming `read_text` calls.
    Read,
    /// Fail upcoming `write_text` calls.
    Write,
}

/// The kind of failure to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFailure {
    /// Transient contention (the R-5 scenario the engine retries).
    Busy,
    /// Permanent failure (never retried).
    Unavailable,
}

#[derive(Default)]
struct ClipboardState {
    content: Option<String>,
    listener: Option<ClipboardListener>,
    fail_reads: (usize, Option<ClipboardFailure>),
    fail_writes: (usize, Option<ClipboardFailure>),
}

/// In-memory [`ClipboardProvider`] with scriptable contention.
///
/// Mirrors the documented contract, including the part that matters most
/// for loop prevention: `write_text` triggers the change listener, just
/// as the Windows clipboard notifies for programmatic writes.
#[derive(Default)]
pub struct InMemoryClipboard {
    state: Mutex<ClipboardState>,
}

impl InMemoryClipboard {
    /// An empty clipboard.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a local user copy: set content and notify the listener,
    /// as the OS would for a change made by another application.
    pub fn set_text_locally(&self, text: &str) {
        let listener = {
            let mut state = lock(&self.state);
            state.content = Some(text.to_owned());
            state.listener.take()
        };
        self.notify_and_restore(listener);
    }

    /// Make the next `count` operations of `op` fail with `kind`, then
    /// succeed again — the shape of every bounded-retry scenario
    /// (docs/TESTING.md §1.5).
    pub fn fail_next(&self, op: ClipboardOp, kind: ClipboardFailure, count: usize) {
        let mut state = lock(&self.state);
        match op {
            ClipboardOp::Read => state.fail_reads = (count, Some(kind)),
            ClipboardOp::Write => state.fail_writes = (count, Some(kind)),
        }
    }

    /// Current content, bypassing failure injection (test assertions).
    #[must_use]
    pub fn peek(&self) -> Option<String> {
        lock(&self.state).content.clone()
    }

    fn notify_and_restore(&self, listener: Option<ClipboardListener>) {
        // Invoke outside the lock (the real provider notifies from a
        // separate thread with no lock held), then restore.
        if let Some(listener) = listener {
            listener();
            let mut state = lock(&self.state);
            if state.listener.is_none() {
                state.listener = Some(listener);
            }
        }
    }

    fn take_failure(slot: &mut (usize, Option<ClipboardFailure>)) -> Option<ClipboardFailure> {
        if slot.0 > 0 {
            slot.0 -= 1;
            let kind = slot.1;
            if slot.0 == 0 {
                slot.1 = None;
            }
            kind
        } else {
            None
        }
    }

    fn failure_error(kind: ClipboardFailure) -> ClipboardError {
        match kind {
            ClipboardFailure::Busy => ClipboardError::Busy {
                reason: "injected contention".to_owned(),
            },
            ClipboardFailure::Unavailable => ClipboardError::Unavailable {
                reason: "injected failure".to_owned(),
            },
        }
    }
}

impl ClipboardProvider for InMemoryClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        let mut state = lock(&self.state);
        if let Some(kind) = Self::take_failure(&mut state.fail_reads) {
            return Err(Self::failure_error(kind));
        }
        Ok(state.content.clone())
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        let listener = {
            let mut state = lock(&self.state);
            if let Some(kind) = Self::take_failure(&mut state.fail_writes) {
                return Err(Self::failure_error(kind));
            }
            state.content = Some(text.to_owned());
            state.listener.take()
        };
        // Contract term under test everywhere: our own writes notify too.
        self.notify_and_restore(listener);
        Ok(())
    }

    fn set_change_listener(
        &self,
        listener: Option<ClipboardListener>,
    ) -> Result<(), ClipboardError> {
        lock(&self.state).listener = listener;
        Ok(())
    }
}

/// Locks a mutex, recovering from poisoning: a panicked test thread must
/// not cascade opaque failures into unrelated tests.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod clipboard_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ClipboardFailure, ClipboardOp, InMemoryClipboard};
    use crate::clipboard::{ClipboardError, ClipboardProvider};

    fn counting_listener(clipboard: &InMemoryClipboard) -> Arc<AtomicUsize> {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        clipboard
            .set_change_listener(Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap();
        count
    }

    #[test]
    fn local_copies_and_own_writes_both_notify() {
        let clipboard = InMemoryClipboard::new();
        let notifications = counting_listener(&clipboard);

        clipboard.set_text_locally("user copied this");
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("user copied this")
        );

        // The contract term loop prevention exists for: writes through
        // the provider notify as well.
        clipboard.write_text("engine applied this").unwrap();
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
        assert_eq!(clipboard.peek().as_deref(), Some("engine applied this"));
    }

    #[test]
    fn injected_contention_fails_n_times_then_clears() {
        let clipboard = InMemoryClipboard::new();
        clipboard.set_text_locally("content");
        clipboard.fail_next(ClipboardOp::Read, ClipboardFailure::Busy, 2);

        assert!(matches!(
            clipboard.read_text(),
            Err(ClipboardError::Busy { .. })
        ));
        assert!(matches!(
            clipboard.read_text(),
            Err(ClipboardError::Busy { .. })
        ));
        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some("content"));

        clipboard.fail_next(ClipboardOp::Write, ClipboardFailure::Unavailable, 1);
        assert!(matches!(
            clipboard.write_text("x"),
            Err(ClipboardError::Unavailable { .. })
        ));
        clipboard.write_text("y").unwrap();
        assert_eq!(clipboard.peek().as_deref(), Some("y"));
    }

    #[test]
    fn failed_writes_do_not_notify_or_mutate() {
        let clipboard = InMemoryClipboard::new();
        clipboard.set_text_locally("original");
        let notifications = counting_listener(&clipboard);

        clipboard.fail_next(ClipboardOp::Write, ClipboardFailure::Busy, 1);
        assert!(clipboard.write_text("rejected").is_err());
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        assert_eq!(clipboard.peek().as_deref(), Some("original"));
    }

    #[test]
    fn listener_replacement_and_removal() {
        let clipboard = InMemoryClipboard::new();
        let first = counting_listener(&clipboard);
        let second = counting_listener(&clipboard); // replaces the first

        clipboard.set_text_locally("x");
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);

        clipboard.set_change_listener(None).unwrap();
        clipboard.set_text_locally("y");
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_clipboard_reads_none_not_error() {
        let clipboard = InMemoryClipboard::new();
        assert_eq!(clipboard.read_text().unwrap(), None);
    }
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
