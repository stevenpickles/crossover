//! Shared fixtures for the Windows platform tests.
//!
//! Only one thing lives here so far, and it is the one that must be shared
//! rather than repeated: the clipboard is a **machine-global** lock, so
//! every test that touches it — the `CF_UNICODETEXT`/`CF_DIB` provider and
//! the virtual-file data object alike — has to serialize against the same
//! mutex. A per-module lock would serialize each module against itself and
//! let the two race each other, which is exactly the contention the
//! provider treats as a routine failure and a test would read as a bug.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Serialize a test that touches the machine clipboard, across this whole
/// test binary.
pub(crate) fn clipboard_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// A private directory for a test's fixtures, removed on drop.
pub(crate) struct Sandbox(std::path::PathBuf);

impl Sandbox {
    pub(crate) fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};

        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crossover-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("sandbox");
        Self(dir)
    }

    pub(crate) fn path(&self, leaf: &str) -> std::path::PathBuf {
        self.0.join(leaf)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
