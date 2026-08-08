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
pub mod input;
#[cfg(windows)]
pub mod keymap;
#[cfg(windows)]
pub mod secure_storage;

#[cfg(windows)]
pub use capture::WindowsInputCapture;
#[cfg(windows)]
pub use clipboard::WindowsClipboard;
#[cfg(windows)]
pub use input::WindowsInputInjector;
#[cfg(windows)]
pub use secure_storage::DpapiSecureStorage;

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
