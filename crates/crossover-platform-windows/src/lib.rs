//! Windows implementations of the `crossover-platform` traits.
//!
//! Win32 implementations live behind `#[cfg(windows)]`; on other targets
//! the crate compiles as an empty shell so tri-OS CI can build the whole
//! workspace (docs/ARCHITECTURE.md §2, §4; platform risks in
//! docs/SPECIFICATION.md §6).
//!
//! This is the workspace's designated home for `unsafe` (Win32 FFI): every
//! unsafe block carries a SAFETY comment and is exercised by platform tests
//! on Windows CI (NFR-6, docs/TESTING.md §1.6).

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod cursor;
#[cfg(windows)]
pub mod display;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod keymap;
#[cfg(windows)]
pub mod secure_storage;
#[cfg(windows)]
pub mod service;
// Pure watchdog logic (ADR 0011), deliberately not Windows-gated so it is
// compiled and unit-tested on every CI OS even though only the Windows daemon
// drives it.
pub mod worker_supervisor;

#[cfg(windows)]
pub use capture::WindowsInputCapture;
#[cfg(windows)]
pub use clipboard::WindowsClipboard;
#[cfg(windows)]
pub use cursor::{WindowsCursorMask, restore_system_cursors};
#[cfg(windows)]
pub use display::WindowsDisplayInfo;

/// Restore the default system cursors — a no-op off Windows, where there is
/// no cursor masking. Called on shutdown so a quit never leaves the cursor
/// blanked (ADR 0009).
#[cfg(not(windows))]
pub fn restore_system_cursors() {}
#[cfg(windows)]
pub use input::WindowsInputInjector;
#[cfg(windows)]
pub use secure_storage::DpapiSecureStorage;
#[cfg(windows)]
pub use service::{WindowsServiceManager, run_service_daemon};
pub use worker_supervisor::{SessionId, WorkerAction, WorkerSupervisor, WorkerSupervisorConfig};

/// Make this process **per-monitor DPI aware** (R-3), so display geometry
/// and cursor coordinates are real pixels across mixed-DPI monitors rather
/// than the OS's scaled, virtualized values (ADR 0007, ADR 0009). Call
/// once at startup, before any window, hook, or metric read — after that
/// the context is fixed for the process.
///
/// A failure (already set, or an OS too old for the V2 context) is
/// non-fatal: coordinates stay virtualized, which still works, only less
/// precisely on mixed-DPI setups.
#[cfg(windows)]
pub fn set_process_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // SAFETY: SetProcessDpiAwarenessContext has no preconditions; it is
    // safe to call once at process start and returns an error rather than
    // faulting if the context cannot be applied.
    let result =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if let Err(error) = result {
        tracing::warn!(%error, "could not set per-monitor DPI awareness; coordinates may be scaled");
    }
}

/// No-op off Windows: DPI awareness is a Win32 concept.
#[cfg(not(windows))]
pub fn set_process_dpi_awareness() {}

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str =
    "Win32 implementations of the crossover-platform traits (docs/ARCHITECTURE.md §4)";

#[cfg(test)]
mod tests {
    use super::CRATE_PURPOSE;

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }
}
