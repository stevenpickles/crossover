//! Local network-link state, asked per peer (docs/ARCHITECTURE.md §4, §10).
//!
//! This trait exists because of a disconnect diagnosis that was wrong on
//! both machines. When a dock-attached 2.5 `GbE` NIC dropped and renegotiated
//! its physical link, *both* peers ended the session with
//!
//! ```text
//! transport failure: transport I/O failed: An existing connection was
//! forcibly closed by the remote host. (os error 10054)
//! ```
//!
//! Nobody closed anything. The local wire went down, and the OS reported the
//! only thing it could see. Reading that log without a second machine's
//! Windows event log beside it sends the reader hunting the peer, which is
//! the one place the fault was not — and NFR-3 asks for diagnostics that
//! *identify* a failure, not ones that misattribute it.
//!
//! So the question is asked of the platform at the moment the session dies:
//! is the local interface that carries traffic to **this peer** up? Per peer
//! rather than "is any interface up", because a machine with a working Wi-Fi
//! adapter and a dead dock still routes this session over the dead one.
//!
//! The answer is advisory. It never gates reconnection, never changes
//! backoff, and never turns into an error — it is a field on a log line.

use std::net::SocketAddr;

/// State of the local interface that carries (or would carry) traffic to a
/// peer.
///
/// Three-valued on purpose: "we could not tell" is a distinct and common
/// answer (no implementation on this OS, no route to the peer, a peer
/// address that is not a literal), and collapsing it into `Up` would invent
/// the very false confidence this type exists to remove.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// The interface is up and its media is connected.
    Up,
    /// The interface is administratively or physically down — so a
    /// disconnect observed at the same moment is *local*, whatever the
    /// socket error says.
    Down,
    /// Could not be determined. Never treat as evidence either way.
    #[default]
    Unknown,
}

impl LinkState {
    /// The canonical `local_link` field value for a log line
    /// (docs/ARCHITECTURE.md §10): `"up"`, `"down"`, or `"unknown"`.
    #[must_use]
    pub fn as_field(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this state licenses blaming the local machine for a
    /// disconnect. Only [`Self::Down`] does; [`Self::Unknown`] is silence,
    /// not a denial.
    #[must_use]
    pub fn blames_local_link(self) -> bool {
        matches!(self, Self::Down)
    }
}

/// Ask the OS about the local end of the path to a peer.
///
/// Implementations run on the **failure path** of a live session, so the
/// contract is narrow and strict:
///
/// - **Cheap and non-blocking.** A local kernel table lookup, not a probe
///   that talks to the network. Nothing here may delay a reconnect (NFR-1).
/// - **Infallible in effect.** There is no `Result`: every internal error,
///   every unsupported case, every unroutable address answers
///   [`LinkState::Unknown`]. A diagnostic that can fail is a diagnostic that
///   has to be handled at the exact moment the caller is already handling a
///   failure.
/// - **Must not panic.** The caller is a supervisor task whose death would
///   stop reconnection altogether — a diagnostic must never cost more than
///   the diagnosis is worth.
pub trait LinkStateProbe: std::fmt::Debug + Send + Sync {
    /// State of the local interface that would carry traffic to `peer`.
    fn link_state(&self, peer: SocketAddr) -> LinkState;
}

/// A [`LinkStateProbe`] for platforms with no implementation yet: always
/// [`LinkState::Unknown`].
///
/// Honest rather than optimistic — see [`LinkState::Unknown`]. Keeping it in
/// this crate means core needs no `cfg` to stay buildable on every OS.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnknownLinkStateProbe;

impl LinkStateProbe for UnknownLinkStateProbe {
    fn link_state(&self, _peer: SocketAddr) -> LinkState {
        LinkState::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::{LinkState, LinkStateProbe, UnknownLinkStateProbe};

    #[test]
    fn field_values_are_the_canonical_lowercase_names() {
        assert_eq!(LinkState::Up.as_field(), "up");
        assert_eq!(LinkState::Down.as_field(), "down");
        assert_eq!(LinkState::Unknown.as_field(), "unknown");
    }

    #[test]
    fn only_a_down_link_blames_the_local_machine() {
        assert!(LinkState::Down.blames_local_link());
        assert!(!LinkState::Up.blames_local_link());
        // The whole point of the third value: not knowing must never read
        // as "the local link was fine".
        assert!(!LinkState::Unknown.blames_local_link());
        assert_eq!(LinkState::default(), LinkState::Unknown);
    }

    #[test]
    fn the_portable_fallback_admits_it_does_not_know() {
        let probe = UnknownLinkStateProbe;
        assert_eq!(
            probe.link_state("192.0.2.1:27677".parse().unwrap()),
            LinkState::Unknown
        );
        assert_eq!(
            probe.link_state("[2001:db8::1]:27677".parse().unwrap()),
            LinkState::Unknown
        );
    }
}
