//! Local link state, carried alongside a session so a disconnect can say
//! *which side* broke (docs/ARCHITECTURE.md §10).
//!
//! The failure this exists for: a NIC that drops its physical link mid
//! session ends the session on **both** machines with
//! `An existing connection was forcibly closed by the remote host`. That
//! sentence is the OS's, it is wrong on both ends, and reading it costs a
//! maintainer a cross-machine event-log correlation to disprove. Asking the
//! platform whether the local interface was up at that moment settles it in
//! the same log line.
//!
//! [`LinkDiagnostics`] is the small bundle that makes the question askable
//! at the failure site: *who the peer is* plus *how to ask*. Both halves are
//! optional and either missing one answers [`LinkState::Unknown`], because a
//! run with no platform probe (or a peer named by a hostname that never
//! resolved) must degrade to silence rather than to a guess.
//!
//! Nothing here influences reconnection. It is read on the way to a log
//! line and nowhere else.

use std::net::SocketAddr;
use std::sync::Arc;

use crossover_platform::{LinkState, LinkStateProbe};

/// What a session (or a pending connect attempt) needs in order to ask
/// whether the local end of its path is up.
#[derive(Clone, Default)]
pub struct LinkDiagnostics {
    peer: Option<SocketAddr>,
    probe: Option<Arc<dyn LinkStateProbe>>,
}

impl LinkDiagnostics {
    /// Bundle a peer address with the probe that can be asked about it.
    #[must_use]
    pub fn new(peer: Option<SocketAddr>, probe: Option<Arc<dyn LinkStateProbe>>) -> Self {
        Self { peer, probe }
    }

    /// The peer socket address these diagnostics are about, if known.
    #[must_use]
    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// Ask the platform, right now.
    ///
    /// Cheap and infallible by the [`LinkStateProbe`] contract; with no
    /// probe or no peer address, [`LinkState::Unknown`].
    #[must_use]
    pub fn state(&self) -> LinkState {
        match (self.peer, self.probe.as_ref()) {
            (Some(peer), Some(probe)) => probe.link_state(peer),
            _ => LinkState::Unknown,
        }
    }
}

/// Deliberately opaque: a probe is a live OS handle with nothing worth
/// printing, and `SessionOptions` derives `Debug`.
impl std::fmt::Debug for LinkDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkDiagnostics")
            .field("peer", &self.peer)
            .field("probe", &self.probe.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossover_platform::LinkState;
    use crossover_platform::fakes::FakeLinkStateProbe;

    use super::LinkDiagnostics;

    fn peer() -> std::net::SocketAddr {
        "192.0.2.7:27677".parse().unwrap()
    }

    #[test]
    fn the_probe_is_asked_about_this_sessions_peer() {
        let probe = Arc::new(FakeLinkStateProbe::answering(LinkState::Down));
        let diagnostics = LinkDiagnostics::new(Some(peer()), Some(probe.clone()));

        assert_eq!(diagnostics.state(), LinkState::Down);
        // Per peer, not "is anything up": a machine with a live Wi-Fi
        // adapter and a dead dock still routes this session over the dock.
        assert_eq!(probe.asked_about(), vec![peer()]);
    }

    #[test]
    fn a_missing_half_answers_unknown_rather_than_guessing() {
        let probe = Arc::new(FakeLinkStateProbe::answering(LinkState::Up));

        // No probe wired (a platform with no implementation).
        assert_eq!(
            LinkDiagnostics::new(Some(peer()), None).state(),
            LinkState::Unknown
        );
        // No peer address (a hostname that never resolved).
        let no_peer = LinkDiagnostics::new(None, Some(probe.clone()));
        assert_eq!(no_peer.state(), LinkState::Unknown);
        assert!(
            probe.asked_about().is_empty(),
            "asked the probe about a peer it was never given"
        );
        // Neither.
        assert_eq!(LinkDiagnostics::default().state(), LinkState::Unknown);
    }

    #[test]
    fn debug_says_whether_a_probe_is_wired_without_printing_one() {
        let probe = Arc::new(FakeLinkStateProbe::answering(LinkState::Up));
        let rendered = format!("{:?}", LinkDiagnostics::new(Some(peer()), Some(probe)));
        assert!(rendered.contains("probe: true"), "{rendered}");
        assert!(rendered.contains("192.0.2.7"), "{rendered}");
    }
}
