//! The editor's screen, as a pure state machine driven by periodic reads of
//! the worker's state file (ADR 0018/0019).
//!
//! In the `worker_supervisor.rs` style
//! (`crates/crossover-platform-windows/src/worker_supervisor.rs`): every
//! transition is `(state, event) -> state`, touching no OS API and no
//! clock of its own — the app layer supplies the freshly read
//! [`StateFileStatus`], this module decides what the editor shows next and,
//! via [`SessionTracker`], *when* to believe a bad read.
//!
//! # The four screens
//!
//! - [`EditorSession::Loading`] — before the first read has completed.
//! - [`EditorSession::NoWorker`] — the state file is absent, or unusable
//!   and naming why (ADR 0018's diagnostics rule: a rejection says which).
//! - [`EditorSession::WaitingForPeer`] — the worker is reporting, this
//!   machine's own monitors are known, but no peer has ever been seen
//!   (`peer: None` in the document). There is nothing to draw a crossing
//!   against yet, so the editor shows this machine's screens with a banner
//!   rather than pretending it can draw an arrangement.
//! - [`EditorSession::Editing`] — a peer has been seen at least once. Its
//!   monitors are drawn whether or not it is connected *right now*
//!   (`peer_connected`) — ADR 0018's point of keeping a disconnected peer's
//!   last-known monitors in the document is exactly so the editor stays
//!   usable while the link is down.
//!
//! Every screen but `Loading` carries a [`Freshness`], because a state
//! document with a stale heartbeat is still worth drawing — it is the last
//! thing the worker said — but the editor must say so rather than presenting
//! it as current (ADR 0018's whole reason for the heartbeat).
//!
//! # Last-good retention across a transient read (`SessionTracker`)
//!
//! [`EditorSession`] alone answers "what does *this* read imply"; it does
//! not answer "should a single bad read actually take a drawn arrangement
//! off the screen". [`SessionTracker`] adds exactly that, mirroring ADR
//! 0018's rule for the worker's own config poll — "a file that fails to
//! parse keeps the last good configuration... an editor caught mid-save
//! must never kill the run" — applied here to the editor's read of the
//! state file instead: a drawn arrangement survives [`FAILURE_GRACE`]
//! consecutive bad reads before the editor demotes to `NoWorker`, so a
//! rename race or a momentary permissions blip never flashes the empty
//! state over a screen the user is looking at.

use crate::model::Model;
use crate::state_file::StateFileStatus;

/// How many consecutive absent/unreadable polls a drawn arrangement
/// (`WaitingForPeer` or `Editing`) survives before [`SessionTracker`]
/// demotes to [`EditorSession::NoWorker`].
///
/// At the ~1 s poll cadence (`app.rs::POLL_INTERVAL`) this is a few
/// seconds of grace: long enough to absorb one transient read without
/// ever hiding a drawn arrangement, short enough that a genuinely stopped
/// worker is still reported promptly. `Loading` and `NoWorker` have
/// nothing drawn to protect, so a bad read from either applies at once.
const FAILURE_GRACE: u32 = 3;

/// Whether the state document's heartbeat is within the worker's expected
/// cadence, mirroring [`crossover_topology::TopologyState::is_stale`]'s
/// verdict at the moment it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The heartbeat is recent; the worker is presumed running.
    Fresh,
    /// The heartbeat has gone quiet past
    /// [`crossover_topology::HEARTBEAT_STALE_AFTER_MS`]; the worker is
    /// presumed stopped, but its last report is still shown.
    Stale,
}

/// The editor's current screen (ADR 0018/0019).
#[derive(Debug, Clone, PartialEq)]
pub enum EditorSession {
    /// No read has completed yet.
    Loading,
    /// The state file is absent (`reason: None`) or present but unusable
    /// (`reason: Some`, naming why — a version mismatch, a torn or
    /// hand-edited document, an oversized file). `None` also covers the
    /// grace-expired case where the *most recent* read was absent, even if
    /// an earlier one in the same run was a named failure.
    NoWorker { reason: Option<String> },
    /// A worker is reporting, but no peer has ever been seen.
    WaitingForPeer { model: Model, staleness: Freshness },
    /// A worker is reporting and a peer has been seen at least once.
    Editing {
        model: Model,
        staleness: Freshness,
        peer_connected: bool,
    },
}

impl EditorSession {
    fn from_document(state: &crossover_topology::TopologyState, staleness: Freshness) -> Self {
        let model = Model::from_state(state);
        match &state.peer {
            Some(peer) => Self::Editing {
                model,
                staleness,
                peer_connected: peer.connected,
            },
            None => Self::WaitingForPeer { model, staleness },
        }
    }

    /// Whether this screen has a drawn arrangement worth protecting with
    /// [`FAILURE_GRACE`] — `Loading` and `NoWorker` have nothing to lose.
    fn is_drawn(&self) -> bool {
        matches!(self, Self::WaitingForPeer { .. } | Self::Editing { .. })
    }
}

/// What one poll changed, for the app layer to log — a diagnostic per
/// *transition*, never per poll (NFR-3's discipline applied to a read that
/// would otherwise print the same line once a second forever).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Nothing worth telling apart from the last poll: still good, or
    /// still bad for the same already-reported reason, or absorbed by the
    /// grace period.
    Unchanged,
    /// The state file just became unusable (or its reason changed while
    /// still unusable) but a drawn arrangement is still on screen, inside
    /// its grace period.
    Unreadable(String),
    /// A grace-period demotion just happened: [`FAILURE_GRACE`] consecutive
    /// bad reads, not individually reported, finally took the drawn
    /// arrangement down.
    Demoted(String),
    /// The state file is usable again after `NoWorker` was on screen.
    Recovered,
}

/// [`EditorSession`] plus the grace-period bookkeeping that decides *when*
/// a bad read is believed (see the module doc's ADR 0018 mirror). Owns the
/// one piece of state that decision needs — a consecutive-failure count,
/// not wall time — so it stays exactly as pure and testable as
/// `EditorSession` alone was: every transition is still `(tracker, event)
/// -> (tracker, SessionEvent)`, deterministic and clock-free.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTracker {
    current: EditorSession,
    consecutive_bad_reads: u32,
}

impl SessionTracker {
    /// A tracker showing [`EditorSession::Loading`], as if freshly opened.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: EditorSession::Loading,
            consecutive_bad_reads: 0,
        }
    }

    /// The screen to paint right now.
    #[must_use]
    pub fn session(&self) -> &EditorSession {
        &self.current
    }

    /// Advance by one state-file read.
    #[must_use]
    pub fn on_read(&mut self, status: StateFileStatus) -> SessionEvent {
        match status {
            StateFileStatus::Fresh(state) => self.apply_good(&state, Freshness::Fresh),
            StateFileStatus::Stale(state) => self.apply_good(&state, Freshness::Stale),
            StateFileStatus::Absent => self.apply_bad(None),
            StateFileStatus::Unreadable(reason) => self.apply_bad(Some(reason.to_string())),
        }
    }

    fn apply_good(
        &mut self,
        state: &crossover_topology::TopologyState,
        staleness: Freshness,
    ) -> SessionEvent {
        let recovering = matches!(self.current, EditorSession::NoWorker { .. });
        self.consecutive_bad_reads = 0;
        self.current = EditorSession::from_document(state, staleness);
        if recovering {
            SessionEvent::Recovered
        } else {
            SessionEvent::Unchanged
        }
    }

    fn apply_bad(&mut self, reason: Option<String>) -> SessionEvent {
        if self.current.is_drawn() {
            self.consecutive_bad_reads = self.consecutive_bad_reads.saturating_add(1);
            if self.consecutive_bad_reads < FAILURE_GRACE {
                // Inside the grace period: the drawn arrangement stays
                // exactly as it was, and this poll is not worth a log line
                // on its own — `Demoted` covers the eventual transition.
                return SessionEvent::Unchanged;
            }
            let message = reason
                .clone()
                .unwrap_or_else(|| "the state file is no longer present".to_owned());
            self.current = EditorSession::NoWorker { reason };
            return SessionEvent::Demoted(message);
        }

        self.consecutive_bad_reads = self.consecutive_bad_reads.saturating_add(1);
        let previous_reason = match &self.current {
            EditorSession::NoWorker { reason } => reason.clone(),
            EditorSession::Loading
            | EditorSession::WaitingForPeer { .. }
            | EditorSession::Editing { .. } => None,
        };
        self.current = EditorSession::NoWorker {
            reason: reason.clone(),
        };

        match reason {
            Some(message) if previous_reason.as_deref() != Some(message.as_str()) => {
                SessionEvent::Unreadable(message)
            }
            // A plain absence is not itself diagnostic-worthy (an editor
            // opened before the worker's first run sees this constantly),
            // and a repeat of the same named reason was already reported.
            _ => SessionEvent::Unchanged,
        }
    }
}

impl Default for SessionTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorSession, FAILURE_GRACE, Freshness, SessionEvent, SessionTracker};
    use crate::state_file::{StateFileStatus, UnreadableReason};
    use crossover_topology::{
        DeviceId, LayoutRect, LiveMonitor, MachineState, MonitorId, PeerState,
        TOPOLOGY_STATE_VERSION, TopologyState,
    };

    fn monitor(id: &str) -> LiveMonitor {
        LiveMonitor {
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_percent: 100,
        }
    }

    fn document(peer: Option<PeerState>, written_at: u64) -> TopologyState {
        TopologyState {
            version: TOPOLOGY_STATE_VERSION,
            written_at,
            local: MachineState {
                device: DeviceId::from_bytes([0x11; 16]),
                name: "desk".to_owned(),
                monitors: vec![monitor(r"\\.\DISPLAY1")],
            },
            peer,
            layout: None,
        }
    }

    fn peer(connected: bool) -> PeerState {
        PeerState {
            device: DeviceId::from_bytes([0x22; 16]),
            name: "laptop".to_owned(),
            connected,
            last_seen: 0,
            monitors: vec![monitor(r"\\.\DISPLAY1")],
        }
    }

    #[test]
    fn starts_loading() {
        let tracker = SessionTracker::new();
        assert_eq!(*tracker.session(), EditorSession::Loading);
    }

    #[test]
    fn a_bad_read_from_loading_goes_straight_to_no_worker() {
        for status in [
            StateFileStatus::Absent,
            StateFileStatus::Unreadable(UnreadableReason::Io),
        ] {
            let mut tracker = SessionTracker::new();
            let _ = tracker.on_read(status);
            assert!(matches!(tracker.session(), EditorSession::NoWorker { .. }));
        }
    }

    #[test]
    fn an_unreadable_reason_from_loading_is_reported_once() {
        let mut tracker = SessionTracker::new();
        let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        assert_eq!(
            event,
            SessionEvent::Unreadable("the state file could not be read".to_owned())
        );
        // The same reason again is not re-reported.
        let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        assert_eq!(event, SessionEvent::Unchanged);
    }

    #[test]
    fn a_document_with_no_peer_ever_seen_waits_for_one() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(None, 0)));
        match tracker.session() {
            EditorSession::WaitingForPeer { staleness, .. } => {
                assert_eq!(*staleness, Freshness::Fresh);
            }
            other => panic!("expected WaitingForPeer, got {other:?}"),
        }
    }

    #[test]
    fn a_document_with_a_peer_is_editing_whether_connected_or_not() {
        for connected in [true, false] {
            let mut tracker = SessionTracker::new();
            let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(connected)), 0)));
            match tracker.session() {
                EditorSession::Editing {
                    peer_connected,
                    staleness,
                    ..
                } => {
                    assert_eq!(*peer_connected, connected);
                    assert_eq!(*staleness, Freshness::Fresh);
                }
                other => panic!("expected Editing, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_stale_document_still_carries_its_model() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Stale(document(Some(peer(true)), 0)));
        match tracker.session() {
            EditorSession::Editing { staleness, .. } => assert_eq!(*staleness, Freshness::Stale),
            other => panic!("expected Editing, got {other:?}"),
        }
    }

    /// Issue 4, first half: one bad read must not flash `NoWorker` over a
    /// drawn arrangement.
    #[test]
    fn one_bad_read_does_not_demote_a_drawn_arrangement() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(true)), 0)));
        assert!(matches!(tracker.session(), EditorSession::Editing { .. }));

        let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        assert_eq!(event, SessionEvent::Unchanged);
        assert!(
            matches!(tracker.session(), EditorSession::Editing { .. }),
            "one bad read must not demote a drawn arrangement, got {:?}",
            tracker.session()
        );
    }

    /// Issue 4, second half: sustained errors do demote, once the grace
    /// period is exhausted — and exactly once, not on every poll after.
    #[test]
    fn sustained_bad_reads_demote_to_no_worker_exactly_once() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(true)), 0)));

        for _ in 0..FAILURE_GRACE - 1 {
            let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
            assert_eq!(event, SessionEvent::Unchanged);
            assert!(matches!(tracker.session(), EditorSession::Editing { .. }));
        }

        let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        assert!(matches!(event, SessionEvent::Demoted(_)), "{event:?}");
        assert!(matches!(tracker.session(), EditorSession::NoWorker { .. }));

        // Further bad reads while already down are `Unchanged`, not
        // repeated `Demoted`s.
        let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        assert_eq!(event, SessionEvent::Unchanged);
    }

    /// Recovering from `NoWorker` is reported; recovering *inside* the
    /// grace period (never actually shown as `NoWorker`) is not, because
    /// nothing visible changed.
    #[test]
    fn recovery_is_reported_only_once_no_worker_was_actually_shown() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(true)), 0)));

        // One bad read, still within grace, then a good one: nothing to
        // recover from.
        let _ = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        let event = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(true)), 0)));
        assert_eq!(event, SessionEvent::Unchanged);

        // Demote for real, then recover.
        for _ in 0..FAILURE_GRACE {
            let _ = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        }
        assert!(matches!(tracker.session(), EditorSession::NoWorker { .. }));
        let event = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(true)), 0)));
        assert_eq!(event, SessionEvent::Recovered);
    }

    /// The worker appearing then disappearing mid-run: a scripted sequence
    /// of reads, asserted screen by screen, sustaining failures long
    /// enough to actually cross the grace period where a demotion is
    /// expected.
    #[test]
    fn the_worker_appearing_and_disappearing_walks_through_every_screen() {
        let mut tracker = SessionTracker::new();
        assert_eq!(*tracker.session(), EditorSession::Loading);

        // Not started yet.
        let _ = tracker.on_read(StateFileStatus::Absent);
        assert!(matches!(
            tracker.session(),
            EditorSession::NoWorker { reason: None }
        ));

        // Started, no peer yet.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(None, 0)));
        assert!(matches!(
            tracker.session(),
            EditorSession::WaitingForPeer { .. }
        ));

        // Peer connects.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(true)), 0)));
        assert!(matches!(
            tracker.session(),
            EditorSession::Editing {
                peer_connected: true,
                ..
            }
        ));

        // Peer's link drops, but it is still remembered.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(false)), 0)));
        assert!(matches!(
            tracker.session(),
            EditorSession::Editing {
                peer_connected: false,
                ..
            }
        ));

        // The worker goes stale, then its state file becomes unreadable
        // for long enough to exhaust the grace period.
        let _ = tracker.on_read(StateFileStatus::Stale(document(Some(peer(false)), 0)));
        assert!(matches!(
            tracker.session(),
            EditorSession::Editing {
                staleness: Freshness::Stale,
                ..
            }
        ));
        for _ in 0..FAILURE_GRACE {
            let _ = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        }
        assert!(matches!(tracker.session(), EditorSession::NoWorker { .. }));

        // And it comes back.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer(true)), 0)));
        assert!(matches!(
            tracker.session(),
            EditorSession::Editing {
                peer_connected: true,
                staleness: Freshness::Fresh,
                ..
            }
        ));
    }
}
