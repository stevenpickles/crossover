//! Platform abstraction traits for Crossover: clipboard, input capture and
//! injection, display enumeration, cursor control, and secure storage.
//!
//! Trait definitions only — no OS dependencies. Platform crates such as
//! `crossover-platform-windows` implement these traits; nothing above the
//! platform boundary names an OS API (docs/ARCHITECTURE.md §2, §4).

pub mod clipboard;
pub mod cursor;
pub mod display;
#[cfg(any(test, feature = "fakes"))]
pub mod fakes;
pub mod input;
pub mod secure_storage;
pub mod service;
pub mod spool;
pub mod virtual_file;

pub use clipboard::{
    ClipboardContent, ClipboardError, ClipboardImageFormat, ClipboardListener, ClipboardProvider,
    MAX_CLIPBOARD_IMAGE_BYTES,
};
pub use cursor::{CursorMask, CursorMaskError, NoopCursorMask};
pub use display::{CursorPoint, DisplayError, DisplayInfo, MonitorRect, Screen};
pub use input::{
    InputCapture, InputError, InputEvent, InputInjector, InputSink, KeyEvent, PointerButton,
    PointerEvent, SCROLL_UNITS_PER_DETENT, hid,
};
pub use secure_storage::{SecureStorage, SecureStorageError};
pub use service::{ServiceError, ServiceManager, ServiceStatus, UnsupportedServiceManager};
pub use spool::{
    MAX_SPOOL_ENTRY_NAME_BYTES, MAX_SPOOL_ENUMERATED_OBJECTS, SpoolEntry, SpoolError, SpoolStorage,
    SpoolSweep, UnsupportedSpoolStorage, validate_entry_name,
};
pub use virtual_file::{UnsupportedVirtualFileClipboard, VirtualFile, VirtualFileClipboard};

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str =
    "platform trait definitions with no OS dependencies (docs/ARCHITECTURE.md §4)";

#[cfg(test)]
mod tests {
    use super::CRATE_PURPOSE;

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }
}
