//! Core logic for Crossover: the topology model, control-transfer state
//! machine, clipboard and input engines, and connection lifecycle
//! supervision.
//!
//! Contains no direct OS API calls and compiles on all platforms; platform
//! effects flow through the traits in `crossover-platform`
//! (docs/ARCHITECTURE.md §2, §5).

pub mod clipboard;
pub mod clipboard_driver;
pub mod control;
pub mod control_driver;
pub mod input;
pub mod metrics;
pub mod net;
pub mod pairing;
pub mod supervision;

pub use clipboard::{ClipboardConfig, ClipboardEngine, RetryPolicy as ClipboardRetryPolicy};
pub use clipboard_driver::{
    ClipboardSyncDriver, FrameTarget, SessionCommand, SyncEvent, clipboard_sync,
};
pub use control::{
    ControlAction, ControlConfig, ControlEngine, ControlEvent, ControlNotice, InboundControl,
    OutboundControl,
};
pub use control_driver::{InputControlDriver, InputControlEvent, input_control};
pub use input::{
    InputEvent, InputState, KeyEvent, PointerButton, PointerEvent, coalesce, coalesce_input, hid,
};
pub use metrics::{FrameClass, Metrics, Report};
pub use net::{
    EstablishedSession, LocalNode, SessionError, SessionInfo, SessionListener, SessionOptions,
    connect,
};
pub use pairing::{PairingDriveError, PairingListener, pair_with};
pub use supervision::{
    DisconnectReason, KeepaliveConfig, ReconnectPolicy, SessionEvent, SupervisorConfig,
    SupervisorHandle, run_session, supervise_outbound,
};

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str = "state machines, clipboard and input engines, topology, \
     and connection supervision (docs/ARCHITECTURE.md §5)";

#[cfg(test)]
mod tests {
    use super::CRATE_PURPOSE;

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }
}
