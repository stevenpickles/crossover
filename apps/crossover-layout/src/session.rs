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
//!
//! # Reconciling a fresh read with work in progress
//!
//! The editor re-reads the worker's facts once a second while the user is
//! drawing on top of them, so every good read has to answer: *whose
//! rectangles are on screen now?* The rule this module implements is
//! **transplant, never clone-wholesale**:
//!
//! 1. Build the scene from the **fresh document**. Every fact the worker
//!    reports therefore lands immediately — a monitor docked while the
//!    editor is open appears, a renamed machine is renamed, the freshness
//!    and the peer's connectedness are this read's.
//! 2. Then move the **user's work** onto it ([`Model::transplant_from`]):
//!    where they dragged each machine, whether the drawing is unsaved, the
//!    gesture still in their hand, and the highest revision seen.
//!
//! Keeping the previous model and patching the new facts into it would
//! invert that and is the bug this replaced: a docked monitor would not
//! appear until the edit was saved, and `seen_revision` could walk
//! backwards — which, under newest-revision-wins (ADR 0018), silently
//! supersedes the very save the user just made.
//!
//! Three conditions decide whether there is work to transplant at all:
//!
//! - **Unsaved work** — dirty *or* a drag in flight
//!   ([`Model::has_unsaved_work`]). A drag is not dirty until it is
//!   dropped, so a predicate that asked about dirtiness alone would let the
//!   1 s poll wipe a gesture out from under the pointer.
//! - **The post-save hold** ([`SessionTracker::note_saved`]). A save writes
//!   `config.toml`; the worker picks it up on its own ~2 s poll and only
//!   then does the state file report the new revision. Between the two the
//!   fresh document still carries the *old* arrangement, and adopting it
//!   would snap the drawing back for a second or three — which reads as a
//!   save that failed. So the local arrangement is held until the state
//!   file reports a layout revision at least as high as the one saved.
//!   Everything else about the read still applies during the hold.
//! - **The same machines** ([`Model::describes_the_same_machines`]). A
//!   *different* peer is a re-pair and discards the drawing, as ADR 0018
//!   requires. A peer that is merely absent is not: that is the window
//!   between a worker restarting and its session coming back.

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
    /// The drawn scene, on the two screens that have one.
    #[must_use]
    pub const fn model(&self) -> Option<&Model> {
        match self {
            Self::Loading | Self::NoWorker { .. } => None,
            Self::WaitingForPeer { model, .. } | Self::Editing { model, .. } => Some(model),
        }
    }

    /// The drawn scene, mutably — what the canvas drags.
    pub const fn model_mut(&mut self) -> Option<&mut Model> {
        match self {
            Self::Loading | Self::NoWorker { .. } => None,
            Self::WaitingForPeer { model, .. } | Self::Editing { model, .. } => Some(model),
        }
    }

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
    /// The revision the last successful save claimed, until the state file
    /// reports the worker has caught up with it — the post-save hold the
    /// module doc describes. `None` when nothing is outstanding.
    awaiting_revision: Option<u64>,
    /// An unsaved edit that was on screen when the worker's state file went
    /// bad for long enough to demote — see [`SessionTracker::apply_bad`].
    /// Held rather than thrown away, and transplanted back onto the facts
    /// the next good read brings.
    retained: Option<Model>,
}

impl SessionTracker {
    /// A tracker showing [`EditorSession::Loading`], as if freshly opened.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: EditorSession::Loading,
            consecutive_bad_reads: 0,
            awaiting_revision: None,
            retained: None,
        }
    }

    /// Whether there is work that exists nowhere but this window: an
    /// unsaved edit or a gesture on the current screen, or an edit being
    /// held while the worker's empty state is showing.
    ///
    /// The app layer's close interception asks exactly this, so the
    /// question a close asks and the question a poll answers are one
    /// predicate rather than two that can disagree.
    #[must_use]
    pub fn has_unsaved_work(&self) -> bool {
        self.current.model().is_some_and(Model::has_unsaved_work) || self.retained.is_some()
    }

    /// The scene a save writes: what is on screen, or — while the worker is
    /// down and its empty state is showing — the edit being held for it.
    ///
    /// Held work is still work, and writing `config.toml` does not need the
    /// worker to be running: it is the file the worker reads when it comes
    /// back. So an outage never makes an edit unsaveable, it only takes the
    /// canvas off the screen.
    pub fn savable_mut(&mut self) -> Option<&mut Model> {
        if self.current.model().is_some() {
            return self.current.model_mut();
        }
        self.retained.as_mut()
    }

    /// Record that the drawn arrangement has been written to the config
    /// file at `revision`, opening the post-save hold (module doc): until a
    /// read reports a layout revision at least this high, the arrangement
    /// on screen is the user's rather than the document's.
    ///
    /// Called by the app layer after a save that actually landed — a failed
    /// one has nothing to wait for, and the scene is still dirty anyway.
    pub const fn note_saved(&mut self, revision: u64) {
        self.awaiting_revision = Some(revision);
    }

    /// The screen to paint right now, mutably — the canvas edits the model
    /// in place as it paints it (ADR 0019's immediate-mode reason).
    pub const fn session_mut(&mut self) -> &mut EditorSession {
        &mut self.current
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
        let mut next = EditorSession::from_document(state, staleness);

        // The worker has caught up with the last save, so the hold that was
        // protecting the drawing from its own stale predecessor is over and
        // the document is authoritative again.
        let reported = state.layout.as_ref().map_or(0, |layout| layout.revision);
        if self
            .awaiting_revision
            .is_some_and(|saved| reported >= saved)
        {
            self.awaiting_revision = None;
        }

        // Transplant, rather than keep-and-patch: the scene `next` already
        // holds is built entirely from the fresh document, and what moves
        // onto it is only what exists nowhere else (module doc). The work
        // may be on the current screen, or — if the worker went away and
        // came back — held from before the demotion.
        {
            let previous = self.current.model().or(self.retained.as_ref());
            if let (Some(previous), Some(fresh)) = (previous, next.model_mut())
                && previous.describes_the_same_machines(fresh)
            {
                // Where the user is *looking* survives every good read, not
                // only the ones with work to move: a scene nobody has
                // edited is rebuilt from the document once a second, and an
                // inspector whose selection was rebuilt away with it would
                // empty itself under the user's hands (`model.rs`).
                fresh.adopt_selection_from(previous);
                if previous.has_unsaved_work() || self.awaiting_revision.is_some() {
                    fresh.transplant_from(previous);
                }
            }
        }
        // Either it was just transplanted or the fresh document describes
        // different machines and it no longer applies. Held work is never
        // held across two good reads.
        self.retained = None;

        self.current = next;
        if recovering {
            SessionEvent::Recovered
        } else {
            SessionEvent::Unchanged
        }
    }

    fn apply_bad(&mut self, reason: Option<String>) -> SessionEvent {
        self.consecutive_bad_reads = self.consecutive_bad_reads.saturating_add(1);
        if self.current.is_drawn() {
            if self.consecutive_bad_reads < FAILURE_GRACE {
                // Inside the grace period: the drawn arrangement stays
                // exactly as it was, and this poll is not worth a log line
                // on its own — `Demoted` covers the eventual transition.
                return SessionEvent::Unchanged;
            }
            let message = reason
                .clone()
                .unwrap_or_else(|| "the state file is no longer present".to_owned());
            // An unsaved drawing is not thrown away by a state-file
            // problem — but it is not left on screen pretending the worker
            // is alive either. A scene that goes on saying "Worker:
            // running · Peer: connected" over a worker that has been gone
            // for seconds is a lie the user acts on, and unsaved work is
            // not a licence to tell it. So the demotion happens as it would
            // for a clean scene, the edit is held ([`Self::retained`]), and
            // it can still be saved (`savable_mut`) or transplanted back
            // onto the next good read.
            self.retain_unsaved_work();
            self.current = EditorSession::NoWorker { reason };
            return SessionEvent::Demoted(message);
        }

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

    /// Take the current screen's unsaved work aside before the screen goes
    /// away, so a demotion costs the user nothing but the canvas.
    ///
    /// A gesture cannot continue against a scene that is no longer drawn,
    /// so it is dropped where it stands — which is what
    /// [`Model::end_drag`] does, marking the scene dirty if it actually
    /// moved. A drag that moved nothing leaves nothing to hold.
    fn retain_unsaved_work(&mut self) {
        let Some(model) = self
            .current
            .model()
            .filter(|model| model.has_unsaved_work())
        else {
            return;
        };
        let mut held = model.clone();
        held.end_drag();
        if held.is_dirty() {
            self.retained = Some(held);
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
    use crate::test_support::{
        ARRANGED_REVISION, LOCAL_DEVICE, PEER_DEVICE, arranged_document, document,
        drag_until_dirty, live_monitor, monitor_key, peer_state, unit_viewport,
    };
    use crossover_topology::{DeviceId, TopologyState};

    /// The scene on screen right now — the tests reach for the field
    /// directly, because production has only ever needed the *mutable*
    /// accessor (the canvas edits the model as it paints it).
    fn screen(tracker: &SessionTracker) -> &EditorSession {
        &tracker.current
    }

    /// Drag the local machine somewhere and let go, so the tracker's
    /// current model has unsaved changes.
    fn edit(tracker: &mut SessionTracker) {
        let model = tracker
            .session_mut()
            .model_mut()
            .expect("a drawn arrangement to edit");
        drag_until_dirty(model);
    }

    /// Take hold of the local machine and *keep holding it* — a gesture in
    /// flight, which is not yet dirty.
    fn begin_dragging(tracker: &mut SessionTracker) {
        let model = tracker
            .session_mut()
            .model_mut()
            .expect("a drawn arrangement to drag");
        let target = monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY1");
        model.begin_drag(&target, (10.0, 10.0), unit_viewport());
        model.drag_to((10.0, 3_010.0));
        assert!(!model.is_dirty(), "a drag in flight is not yet dirty");
        assert!(model.has_unsaved_work(), "but it is still work");
    }

    fn local_top(tracker: &SessionTracker) -> i32 {
        screen(tracker)
            .model()
            .expect("a drawn arrangement")
            .local
            .monitors[0]
            .rect
            .y
    }

    /// The fixture document with its saved layout renumbered, standing in
    /// for the worker having adopted a newer arrangement.
    fn document_at_revision(revision: u64) -> TopologyState {
        let mut state = arranged_document(0);
        if let Some(layout) = state.layout.as_mut() {
            layout.revision = revision;
        }
        state
    }

    /// Record a save the way `app.rs` does: the model takes the revision,
    /// the tracker opens the post-save hold.
    fn record_save(tracker: &mut SessionTracker, revision: u64) {
        tracker
            .savable_mut()
            .expect("a scene to save")
            .mark_saved(revision);
        tracker.note_saved(revision);
    }

    #[test]
    fn starts_loading() {
        let tracker = SessionTracker::new();
        assert_eq!(*screen(&tracker), EditorSession::Loading);
    }

    // ---- Reconciling a fresh read with work in progress -----------------

    /// The poll must not revert a drag that has been dropped. An editor
    /// that re-read its facts once a second and silently put the user's
    /// rectangles back would be unusable, and the loss would be invisible.
    #[test]
    fn an_unsaved_edit_survives_the_polls_that_follow_it() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        edit(&mut tracker);
        let edited = local_top(&tracker);

        // A fresh read of the same facts, then a stale one: the drawing
        // stays, and the *worker's* state still updates.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 1)));
        assert_eq!(local_top(&tracker), edited);
        let _ = tracker.on_read(StateFileStatus::Stale(document(Some(peer_state(false)), 2)));
        assert_eq!(local_top(&tracker), edited);
        match screen(&tracker) {
            EditorSession::Editing {
                staleness,
                peer_connected,
                ..
            } => {
                assert_eq!(*staleness, Freshness::Stale);
                assert!(
                    !peer_connected,
                    "the peer's state is still the fresh read's"
                );
            }
            other => panic!("expected Editing, got {other:?}"),
        }
    }

    /// A stated size is unsaved work, so the poll that lands a moment later
    /// keeps it — the same protection a dropped drag gets, through the same
    /// transplant, because an override exists nowhere but this window until
    /// the arrangement is saved (`model.rs`).
    #[test]
    fn a_stated_size_survives_the_polls_that_follow_it() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        let target = monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY1");
        let model = tracker.session_mut().model_mut().expect("a scene");
        assert!(model.set_size_mm(&target, 597, 336));
        let stated = model.local.monitors[0].rect;

        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 1)));
        assert_eq!(
            screen(&tracker).model().expect("a scene").local.monitors[0].rect,
            stated,
            "the poll redrew a size the user had stated"
        );
        assert!(tracker.has_unsaved_work(), "and it is still unsaved");
    }

    /// A **selection** survives a poll even though it is not unsaved work
    /// and nothing is being transplanted: a clean scene is rebuilt from the
    /// document once a second, and an inspector that emptied itself on that
    /// cadence could not be used to correct anything.
    #[test]
    fn the_selected_screen_survives_a_poll_on_an_unedited_scene() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(arranged_document(0)));
        let target = monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY1");
        tracker
            .session_mut()
            .model_mut()
            .expect("a scene")
            .select(Some(&target));

        let _ = tracker.on_read(StateFileStatus::Fresh(arranged_document(1)));
        let model = screen(&tracker).model().expect("a scene");
        assert_eq!(model.selected(), Some(&target));
        assert!(!model.is_dirty(), "and nothing was transplanted with it");

        // A re-pair discards the drawing, and a selection into it with it.
        let mut stranger = peer_state(true);
        stranger.device = crossover_topology::DeviceId::from_bytes([0x77; 16]);
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(stranger), 2)));
        assert_eq!(screen(&tracker).model().expect("a scene").selected(), None);
    }

    /// **A drag in flight survives the poll too.** `is_dirty` is false for
    /// the whole of a gesture — it is the *drop* that commits — so a
    /// reconciliation that asked about dirtiness alone would wipe the
    /// user's hand mid-drag, once a second, on a scene they had not
    /// finished arranging. This is the predicate `has_unsaved_work` exists
    /// for.
    #[test]
    fn a_drag_still_in_the_users_hand_survives_the_poll() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        begin_dragging(&mut tracker);
        let mid_drag = local_top(&tracker);
        assert_ne!(mid_drag, 0, "the fixture must actually have moved");

        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 1)));
        assert_eq!(local_top(&tracker), mid_drag, "the poll reverted the drag");

        // And the gesture is still live: letting go now commits it.
        let model = tracker.session_mut().model_mut().expect("a scene");
        assert!(model.drag().is_some(), "the drag was dropped by the poll");
        model.drag_to((10.0, 4_010.0));
        model.end_drag();
        assert!(model.is_dirty());
    }

    /// A monitor docked while an edit is unsaved appears **immediately** —
    /// it is a fact the worker reported, and the transplant moves the
    /// user's work onto the fresh scene rather than keeping a scene that
    /// predates the monitor. It arrives beside its siblings, and it is in
    /// the arrangement the next save writes.
    #[test]
    fn a_monitor_docked_while_dirty_appears_at_once_and_rides_the_save() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        edit(&mut tracker);
        let edited = local_top(&tracker);

        let mut docked = document(Some(peer_state(true)), 1);
        docked.local.monitors.push(live_monitor(r"\\.\DISPLAY2"));
        let _ = tracker.on_read(StateFileStatus::Fresh(docked));

        let model = screen(&tracker).model().expect("a drawn arrangement");
        assert_eq!(
            model.local.monitors.len(),
            2,
            "the docked monitor must appear without waiting for a save"
        );
        assert!(model.is_dirty(), "and the edit is still unsaved");
        assert_eq!(
            local_top(&tracker),
            edited,
            "the machine is still where the user dragged it"
        );
        // The new monitor came with the group rather than staying at the
        // seed's origin, so it is not sitting on top of its siblings — and
        // every drawn rectangle is what a save would write.
        let placed = model.placed();
        assert_eq!(placed.len(), 3, "{placed:?}");
        for (index, first) in placed.iter().enumerate() {
            for second in &placed[index + 1..] {
                assert!(
                    !first.rect.overlaps(second.rect),
                    "{first:?} overlaps {second:?}"
                );
            }
        }
    }

    /// A **different** peer is a re-pair: the rectangles describe two
    /// machines and one of them is no longer at the other end. ADR 0018
    /// discards that residue rather than guessing about it, and so does an
    /// unsaved edit.
    #[test]
    fn a_re_pair_discards_an_unsaved_edit_rather_than_re_attributing_it() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        edit(&mut tracker);

        let mut stranger = peer_state(true);
        stranger.device = DeviceId::from_bytes([0x77; 16]);
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(stranger), 1)));
        assert_eq!(
            local_top(&tracker),
            0,
            "the seed's position, not the edit's"
        );
        assert!(
            !screen(&tracker)
                .model()
                .expect("a drawn arrangement")
                .is_dirty()
        );
    }

    /// A peer that is merely *absent* is not a stranger. `peer: None` from
    /// the same local machine is the window between a worker restarting and
    /// its session coming back; treating it as a re-pair would throw the
    /// user's work away every time the worker was restarted under the
    /// editor.
    #[test]
    fn a_worker_restart_window_keeps_the_edit_rather_than_discarding_it() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        edit(&mut tracker);
        let edited = local_top(&tracker);

        // The worker restarts: it is reporting again, but it has not seen a
        // peer yet this run.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(None, 1)));
        assert!(matches!(
            screen(&tracker),
            EditorSession::WaitingForPeer { .. }
        ));
        assert_eq!(local_top(&tracker), edited, "the edit was discarded");
        assert!(
            screen(&tracker)
                .model()
                .expect("a drawn arrangement")
                .is_dirty(),
            "and it is still unsaved, so it can still be written"
        );

        // The peer comes back, and it is the same one.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 2)));
        assert_eq!(local_top(&tracker), edited);
    }

    /// With nothing unsaved, each poll's facts are simply adopted — a
    /// monitor plugged in while the editor is open appears.
    #[test]
    fn a_clean_scene_takes_each_polls_facts() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        assert_eq!(screen(&tracker).model().unwrap().local.monitors.len(), 1);

        let mut grown = document(Some(peer_state(true)), 1);
        grown.local.monitors.push(live_monitor(r"\\.\DISPLAY2"));
        let _ = tracker.on_read(StateFileStatus::Fresh(grown));
        assert_eq!(screen(&tracker).model().unwrap().local.monitors.len(), 2);
    }

    // ---- The post-save hold ---------------------------------------------

    /// The seconds after a save are the ones that look like a failure. The
    /// config file is written; the worker has not re-read it yet; the state
    /// file still reports the *old* arrangement. Adopting it would snap the
    /// drawing back for a second or three, which reads as a save that did
    /// nothing — so the arrangement is held until the worker catches up,
    /// and released the moment it does.
    #[test]
    fn a_saved_arrangement_does_not_revert_while_the_worker_catches_up() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document_at_revision(4)));
        edit(&mut tracker);
        let edited = local_top(&tracker);
        record_save(&mut tracker, 9);
        assert!(
            !screen(&tracker).model().unwrap().is_dirty(),
            "a saved scene is clean, which is exactly why the hold is needed"
        );

        // Two polls while the worker is still on the old revision.
        for written_at in 1..3 {
            let mut behind = document_at_revision(4);
            behind.written_at = written_at;
            let _ = tracker.on_read(StateFileStatus::Fresh(behind));
            assert_eq!(
                local_top(&tracker),
                edited,
                "the save must not appear to have been undone"
            );
        }

        // The worker catches up: the document is authoritative again, and
        // what it now says is what the editor shows.
        let mut caught_up = document_at_revision(9);
        caught_up.local.monitors.push(live_monitor(r"\\.\DISPLAY2"));
        let _ = tracker.on_read(StateFileStatus::Fresh(caught_up));
        assert_eq!(
            screen(&tracker).model().unwrap().local.monitors.len(),
            2,
            "the hold is over, so the document's facts land"
        );
    }

    /// The hold keeps the *rectangles*, not the read. Staleness and the
    /// peer's connectedness are still the fresh document's throughout —
    /// holding an arrangement is not a licence to misreport the worker.
    #[test]
    fn the_post_save_hold_still_reports_the_workers_real_state() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document_at_revision(4)));
        edit(&mut tracker);
        record_save(&mut tracker, 9);

        let mut disconnected = document_at_revision(4);
        disconnected.peer = Some(peer_state(false));
        let _ = tracker.on_read(StateFileStatus::Stale(disconnected));
        match screen(&tracker) {
            EditorSession::Editing {
                staleness,
                peer_connected,
                ..
            } => {
                assert_eq!(*staleness, Freshness::Stale);
                assert!(!peer_connected);
            }
            other => panic!("expected Editing, got {other:?}"),
        }
    }

    /// A save's revision is never walked backwards by a document that has
    /// not caught up. `seen_revision` is the floor the *next* save numbers
    /// itself past, and under newest-revision-wins (ADR 0018) a save
    /// numbered below the one it replaces is silently superseded.
    #[test]
    fn a_document_behind_the_save_cannot_lower_the_revision_it_recorded() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document_at_revision(4)));
        edit(&mut tracker);
        record_save(&mut tracker, 9);

        let _ = tracker.on_read(StateFileStatus::Fresh(document_at_revision(4)));
        assert_eq!(
            screen(&tracker).model().unwrap().seen_revision,
            9,
            "the state file's older revision must not lower it"
        );
    }

    // ---- Bad reads -------------------------------------------------------

    #[test]
    fn a_bad_read_from_loading_goes_straight_to_no_worker() {
        for status in [
            StateFileStatus::Absent,
            StateFileStatus::Unreadable(UnreadableReason::Io),
        ] {
            let mut tracker = SessionTracker::new();
            let _ = tracker.on_read(status);
            assert!(matches!(screen(&tracker), EditorSession::NoWorker { .. }));
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

    /// A dead worker is reported as dead **even when there is unsaved
    /// work**. An editor that went on saying "Worker: running · Peer:
    /// connected" over a worker that had been gone for seconds — because
    /// the user happened to have dragged something — would be lying about
    /// the one thing the status bar is for. Saying so costs the edit
    /// nothing: it is held, it can still be saved, and the next good read
    /// puts it back.
    #[test]
    fn a_dead_worker_is_reported_even_while_an_edit_is_unsaved() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        edit(&mut tracker);
        let edited = local_top(&tracker);

        for _ in 0..FAILURE_GRACE - 1 {
            let event = tracker.on_read(StateFileStatus::Absent);
            assert_eq!(event, SessionEvent::Unchanged, "still inside the grace");
            assert_eq!(local_top(&tracker), edited);
        }
        let event = tracker.on_read(StateFileStatus::Absent);
        assert!(matches!(event, SessionEvent::Demoted(_)), "{event:?}");
        assert!(
            matches!(screen(&tracker), EditorSession::NoWorker { reason: None }),
            "the empty state must be shown: {:?}",
            screen(&tracker)
        );

        // The work is held, not lost: still reported as unsaved, and still
        // writable while the worker is away.
        assert!(tracker.has_unsaved_work());
        let held = tracker.savable_mut().expect("the edit is still savable");
        assert!(held.is_dirty());
        assert_eq!(held.local.monitors[0].rect.y, edited);

        // And the worker's return puts it back on screen, on that read's
        // own facts.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 5)));
        assert_eq!(local_top(&tracker), edited);
        assert!(
            screen(&tracker).model().unwrap().is_dirty(),
            "the held edit came back unsaved, as it was"
        );
    }

    /// Issue 4, first half: one bad read must not flash `NoWorker` over a
    /// drawn arrangement.
    #[test]
    fn one_bad_read_does_not_demote_a_drawn_arrangement() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        assert!(matches!(screen(&tracker), EditorSession::Editing { .. }));

        let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        assert_eq!(event, SessionEvent::Unchanged);
        assert!(
            matches!(screen(&tracker), EditorSession::Editing { .. }),
            "one bad read must not demote a drawn arrangement, got {:?}",
            screen(&tracker)
        );
    }

    /// Issue 4, second half: sustained errors do demote, once the grace
    /// period is exhausted — and exactly once, not on every poll after.
    #[test]
    fn sustained_bad_reads_demote_to_no_worker_exactly_once() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));

        for _ in 0..FAILURE_GRACE - 1 {
            let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
            assert_eq!(event, SessionEvent::Unchanged);
            assert!(matches!(screen(&tracker), EditorSession::Editing { .. }));
        }

        let event = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        assert!(matches!(event, SessionEvent::Demoted(_)), "{event:?}");
        assert!(matches!(screen(&tracker), EditorSession::NoWorker { .. }));

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
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));

        // One bad read, still within grace, then a good one: nothing to
        // recover from.
        let _ = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        let event = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        assert_eq!(event, SessionEvent::Unchanged);

        // Demote for real, then recover.
        for _ in 0..FAILURE_GRACE {
            let _ = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        }
        assert!(matches!(screen(&tracker), EditorSession::NoWorker { .. }));
        let event = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        assert_eq!(event, SessionEvent::Recovered);
    }

    // ---- The four screens ------------------------------------------------

    #[test]
    fn a_document_with_no_peer_ever_seen_waits_for_one() {
        let mut tracker = SessionTracker::new();
        let _ = tracker.on_read(StateFileStatus::Fresh(document(None, 0)));
        match screen(&tracker) {
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
            let _ = tracker.on_read(StateFileStatus::Fresh(document(
                Some(peer_state(connected)),
                0,
            )));
            match screen(&tracker) {
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
        let _ = tracker.on_read(StateFileStatus::Stale(document(Some(peer_state(true)), 0)));
        match screen(&tracker) {
            EditorSession::Editing { staleness, .. } => assert_eq!(*staleness, Freshness::Stale),
            other => panic!("expected Editing, got {other:?}"),
        }
    }

    /// The worker appearing then disappearing mid-run: a scripted sequence
    /// of reads, asserted screen by screen, sustaining failures long
    /// enough to actually cross the grace period where a demotion is
    /// expected.
    #[test]
    fn the_worker_appearing_and_disappearing_walks_through_every_screen() {
        let mut tracker = SessionTracker::new();
        assert_eq!(*screen(&tracker), EditorSession::Loading);

        // Not started yet.
        let _ = tracker.on_read(StateFileStatus::Absent);
        assert!(matches!(
            screen(&tracker),
            EditorSession::NoWorker { reason: None }
        ));

        // Started, no peer yet.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(None, 0)));
        assert!(matches!(
            screen(&tracker),
            EditorSession::WaitingForPeer { .. }
        ));

        // Peer connects.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        assert!(matches!(
            screen(&tracker),
            EditorSession::Editing {
                peer_connected: true,
                ..
            }
        ));

        // Peer's link drops, but it is still remembered.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(false)), 0)));
        assert!(matches!(
            screen(&tracker),
            EditorSession::Editing {
                peer_connected: false,
                ..
            }
        ));

        // The worker goes stale, then its state file becomes unreadable
        // for long enough to exhaust the grace period.
        let _ = tracker.on_read(StateFileStatus::Stale(document(Some(peer_state(false)), 0)));
        assert!(matches!(
            screen(&tracker),
            EditorSession::Editing {
                staleness: Freshness::Stale,
                ..
            }
        ));
        for _ in 0..FAILURE_GRACE {
            let _ = tracker.on_read(StateFileStatus::Unreadable(UnreadableReason::Io));
        }
        assert!(matches!(screen(&tracker), EditorSession::NoWorker { .. }));

        // And it comes back.
        let _ = tracker.on_read(StateFileStatus::Fresh(document(Some(peer_state(true)), 0)));
        assert!(matches!(
            screen(&tracker),
            EditorSession::Editing {
                peer_connected: true,
                staleness: Freshness::Fresh,
                ..
            }
        ));
    }

    /// A guard on the shared fixtures rather than on this module: a re-pair
    /// is measured against the peer's device, and the post-save hold
    /// against the saved revision, so both must be what these tests assume.
    #[test]
    fn the_shared_fixture_names_two_machines_and_a_saved_revision() {
        assert_ne!(LOCAL_DEVICE, PEER_DEVICE);
        assert_eq!(
            arranged_document(0)
                .layout
                .expect("the fixture has a saved layout")
                .revision,
            ARRANGED_REVISION
        );
    }
}
