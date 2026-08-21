//! Core logic for Crossover: the topology and crossing models, the
//! control-transfer state machine, clipboard and input engines, and
//! connection lifecycle supervision.
//!
//! Contains no direct OS API calls and compiles on all platforms; platform
//! effects flow through the traits in `crossover-platform`
//! (docs/ARCHITECTURE.md §2, §5).

pub mod clipboard;
pub mod clipboard_driver;
pub mod command;
pub mod control;
pub mod control_driver;
pub mod crossing;
pub mod edge_driver;
pub mod file_blob;
pub mod input;
pub mod link;
pub mod metrics;
pub mod net;
pub mod outbound;
pub mod pairing;
pub mod supervision;
pub mod topology;

pub use clipboard::{
    ClipboardConfig, ClipboardEngine, FileReceive, FileSend, RetryPolicy as ClipboardRetryPolicy,
    SpooledFile, TransferScope, WriteFailure,
};
pub use clipboard_driver::{ClipboardSyncDriver, SyncEvent, clipboard_sync};
pub use command::{FrameTarget, SessionCommand};
pub use control::{
    ControlAction, ControlConfig, ControlEngine, ControlEvent, ControlNotice, InboundControl,
    OutboundControl,
};
pub use control_driver::{InputControlDriver, InputControlEvent, SeamlessInputs, input_control};
pub use crossing::{
    CrossSpan, CrossTarget, CrossingMap, Departure, ImplicitLayout, ImplicitLayoutError,
    LayoutSpan, MappedMonitor, SpanId, derive as derive_crossings, from_link_side,
};
pub use edge_driver::{
    CrossingKind, EdgeCrossing, EdgeDetectDriver, EdgeDetector, EdgeMode, EdgeModeUpdate,
    edge_detect,
};
pub use file_blob::{FALLBACK_ARCHIVE_NAME, wire_file_name};
pub use input::{
    InputEvent, InputState, KeyEvent, PointerButton, PointerEvent, coalesce, coalesce_input, hid,
};
pub use link::LinkDiagnostics;
pub use metrics::{FrameClass, Metrics, Report};
pub use net::{
    EstablishedSession, LocalNode, SessionError, SessionInfo, SessionListener, SessionOptions,
    connect,
};
pub use outbound::{
    MAX_BACKGROUND_QUEUE_BYTES, MAX_BACKGROUND_QUEUE_FRAMES, MAX_HIGH_QUEUE_FRAMES, OutboundClosed,
    OutboundFrame, OutboundReceiver, OutboundSender, SendPriority, budgeted_channel,
    outbound_channel,
};
pub use pairing::{PairingDriveError, PairingListener, pair_with};
pub use supervision::{
    DisconnectReason, KeepaliveConfig, ReconnectPolicy, SessionEvent, SupervisorConfig,
    SupervisorHandle, run_session, supervise_outbound,
};
pub use topology::{CursorPoint, Edge, EdgeFraction, LinkSide, Screen, Topology};

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
