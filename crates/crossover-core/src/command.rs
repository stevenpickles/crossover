//! What a driver asks the application to do, and which session(s) it means.
//!
//! Plain data with no behaviour, in its own module because three layers
//! share it and none of them should have to depend on another to name it:
//! the clipboard and input-control drivers *emit* these, the prioritized
//! send path ([`crate::outbound`]) *classifies* them, and the application
//! *routes* them. Living in a driver, as it used to, meant the generic send
//! path had to reach into a driver module to know its own currency.

use uuid::Uuid;

/// Which session(s) a [`SessionCommand`] is directed at.
///
/// Clipboard sync is session-agnostic (FR-5.4) and broadcasts; control
/// and input traffic is authority for one authenticated session and is
/// routed to exactly that one (FR-5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameTarget {
    /// Every active session.
    Broadcast,
    /// One session, by its locally generated id.
    Session(Uuid),
}

/// What a driver asks the app to do.
#[derive(Debug)]
pub enum SessionCommand {
    /// Send this frame to the target session(s).
    SendFrame {
        /// Which session(s) to send it to.
        target: FrameTarget,
        /// Frame message type.
        message_type: u16,
        /// Encoded payload.
        payload: Vec<u8>,
    },
    /// The target sent an invalid payload: terminate it (fail closed);
    /// supervision handles the rest.
    TerminateSession {
        /// Which session(s) to terminate.
        target: FrameTarget,
        /// Diagnostic for logs.
        reason: String,
    },
}
