//! Core logic for Crossover: the topology model, control-transfer state
//! machine, clipboard and input engines, and connection lifecycle
//! supervision.
//!
//! Contains no direct OS API calls and compiles on all platforms; platform
//! effects flow through the traits in `crossover-platform`
//! (docs/ARCHITECTURE.md §2, §5).

pub mod net;
pub mod pairing;
pub mod supervision;

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
