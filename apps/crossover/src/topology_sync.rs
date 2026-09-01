//! Layout sync: making both machines agree on one arrangement over the
//! life of a session
//! ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md),
//! docs/PROTOCOL.md §6.2).
//!
//! One layout describes both machines and either desk can edit it, so
//! ownership is not modelled — convergence is. This module is the worker's
//! half of that: it sends what this machine knows, receives what the peer
//! sends, and acts on [`crossover_core::layout_sync::resolve`]'s answer.
//! The rule itself is pure and lives in `crossover-core`; everything here
//! is the wiring that has side effects — a config write, a publication to
//! the live crossing source, a state-file update, a log line.
//!
//! # The four things that reach it
//!
//! - **A session comes up.** This machine states its own live monitors
//!   (`MonitorTopology`) and, if it holds a *drawn* arrangement, states it
//!   (`LayoutSync`). An implicit arrangement — the deprecated side model —
//!   is never synced (ADR 0018), so a `--left` run says nothing about
//!   layout at all.
//! - **The local display changes.** The ~1 s own-display poll in
//!   [`crate::topology_state`] pings this hub, which re-states
//!   `MonitorTopology` to every live session.
//! - **The config file changes.** `commands::apply_config_changes` offers
//!   every changed, valid, explicit `[layout]` here; a genuinely newer one
//!   becomes this run's arrangement and is stated to the peer.
//! - **A frame arrives.** `MonitorTopology` updates the state file's peer
//!   half; `LayoutSync` is validated against this session's pair and then
//!   resolved.
//!
//! # Adoption is persist, publish, report — and what "persist first" buys
//!
//! ADR 0018 fixes the order and the reason: a machine that crashed between
//! publishing and persisting would come back crossing by an arrangement its
//! own config does not record, which is the worst of the three outcomes.
//! The state file is last because it is a *report*, not a source of truth.
//!
//! Persistence is also **rate-bounded** ([`LAYOUT_PERSIST_INTERVAL`]), and
//! the two rules interact in a way worth stating exactly rather than
//! implying. The *first* adoption reaches the disk before anything is
//! published, so the ordering is literal there. A further adoption inside
//! the interval is **coalesced**: it publishes immediately and its write
//! lands when the interval lapses, so for that window the config genuinely
//! lags what this machine is crossing by. ADR 0018 accepts precisely that
//! window — a restart re-syncs from the peer on reconnect, so the layout a
//! crash loses is the layout the reconnect restores — and the window is
//! what buys the bound docs/SECURITY.md T23 needs: adoption is a filesystem
//! write driven by network input, and a peer feeding distinct revisions as
//! fast as it can send must not be able to rewrite this machine's config at
//! wire speed.
//!
//! # The state file's layout is this hub's alone
//!
//! One thing writes `layout` into `~/.crossover/state/topology.json`, and
//! that is [`TopologySync::report`]. `commands::apply_config_changes`
//! re-reads the config and *offers* what it finds here; it never writes the
//! layout itself. That is not tidiness. During a coalescing window the
//! config holds an older revision than this run is crossing by, so a
//! config-driven writer would report an arrangement the worker is not
//! using — and the editor, which numbers its next save one past everything
//! **both** files have seen (`crossover-layout`'s `save::next_revision`),
//! would then number a save into a revision this machine had already
//! adopted from the peer. Two different arrangements at one revision is the
//! anomaly the resolver's hash tiebreak exists to *survive*, not a state to
//! walk into on purpose.
//!
//! # Nothing here can start an echo loop
//!
//! Every step is content-equality gated, and the loop that would otherwise
//! close is: adopt → write the config → the config poll re-reads it →
//! offers it back here. That last hop resolves to
//! [`Resolution::Identical`], which sends nothing. The state writer's own
//! setters are gated the same way, so an adoption that changes nothing
//! writes nothing anywhere.
//!
//! # What is rejected, and what that costs the session
//!
//! A **malformed** frame terminates the session, fail closed
//! (docs/PROTOCOL.md §7) — that is the decoder's answer, and this hub
//! forwards it as a `TerminateSession` command exactly as the clipboard and
//! control drivers do for their own payload violations. A **well-formed but
//! semantically impossible** layout — one naming a device that is not this
//! session's pair, overlapping rectangles, only one machine drawn — is
//! rejected, logged, counted as a protocol violation, and never adopted;
//! the session survives, because a peer that disagrees with reality must
//! not be able to cost a healthy session its first frame. §7's rule is
//! *graduated*, though, and so is this: past [`MAX_LAYOUT_VIOLATIONS`] on
//! one session the peer loses it, which is what keeps the leniency from
//! being a free channel for junk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use uuid::Uuid;

use crossover_core::layout_sync::{Resolution, keys_tie, resolve};
use crossover_core::outbound::CommandSender;
use crossover_core::{FrameTarget, LiveLayout, Metrics, SessionCommand};
use crossover_platform::DisplayInfo;
use crossover_protocol::RawFrame;
use crossover_protocol::hello::MessageType;
use crossover_protocol::layout::{LayoutSync, MonitorReport, MonitorTopology};
use crossover_topology::{
    DeviceId, DevicePair, Layout, LayoutError, LayoutState, LiveMonitor, MonitorId, PersistError,
    persist_layout,
};

use crate::topology_state::{
    LiveMonitorsError, TopologyStateWriter, live_monitor_ids, live_monitors,
};

/// How often adoption may rewrite `config.toml` (ADR 0018,
/// docs/SECURITY.md T23).
///
/// The first adoption persists immediately; further adoptions inside this
/// window coalesce to the latest revision and are written once when it
/// lapses. Five seconds is short enough that a normal edit at the other
/// desk is on disk before anyone looks, and long enough that a peer
/// streaming revisions costs one write per interval rather than one per
/// frame.
pub const LAYOUT_PERSIST_INTERVAL: Duration = Duration::from_secs(5);

/// How many events the hub's inbound queue holds.
///
/// Generous next to the traffic — a `MonitorTopology` per display change, a
/// `LayoutSync` per edit — and bounded like every other queue in the
/// system, so a peer that floods type 17/18 frames parks its own session's
/// frame pump rather than growing memory here (NFR-1).
pub const TOPOLOGY_QUEUE_DEPTH: usize = 64;

/// How many semantically impossible layouts a peer may send on one session
/// before it is terminated (docs/PROTOCOL.md §7's graduated rule, which
/// ADR 0018 charges these against explicitly).
///
/// A conforming peer sends zero. A *benign* one can send a few: a layout
/// re-stated across a re-pair names a machine that is no longer at the
/// other end, and killing a healthy session over the first frame of that
/// is exactly what §7's distinction between malformed and impossible
/// exists to prevent. A handful absorbs it; nothing absorbs a peer
/// streaming them, which is the point — without a cap, unadoptable junk is
/// free for the sender and unbounded log volume for us.
///
/// The same figure and the same reasoning as
/// `crossover-core`'s `MAX_CLIPBOARD_VIOLATIONS`, and reset per session
/// for the same reason: the counter bounds one peer's misbehaviour on one
/// connection, not a process-lifetime grudge.
const MAX_LAYOUT_VIOLATIONS: u32 = 8;

/// How many times one session may be answered with this machine's own
/// arrangement because the peer's lost ([`Resolution::KeepLocal`]).
///
/// Answering is the convergence mechanism, and a conforming peer needs one:
/// it adopts and stops disagreeing. A peer that ignores the answer and
/// re-states the same superseded arrangement is not converging, and every
/// re-statement earns it a frame from us — a small, free amplifier. A
/// handful covers a genuine crossing of statements (both ends state at
/// once, a reconnect re-states) and nothing covers a loop, which is the
/// point. Exhausting the budget is not a violation and never ends the
/// session: this machine simply stops answering, and the peer keeps
/// whatever it holds.
const MAX_KEEP_LOCAL_ANSWERS: u32 = 8;

/// What reaches the hub.
#[derive(Debug)]
pub enum TopologyEvent {
    /// A session came up: state our monitors and, if we have one, our
    /// arrangement.
    SessionEstablished {
        /// The session's locally generated id, for addressing frames back.
        session: Uuid,
        /// The peer's self-reported device id (bookkeeping — the layout
        /// names desks, never authorizes anything; ADR 0003).
        peer_device: Uuid,
        /// The peer's self-reported name, for the state file.
        peer_name: String,
    },
    /// A session ended. The peer's last-known monitors stay in the state
    /// file with `connected: false`, so the editor is still usable while
    /// the link is down.
    SessionLost {
        /// Which session.
        session: Uuid,
    },
    /// A `MonitorTopology` or `LayoutSync` frame arrived.
    Frame {
        /// Which session it arrived on.
        session: Uuid,
        /// The undecoded frame — decoding is this hub's job, so a
        /// malformed one is refused at exactly one place.
        frame: RawFrame,
    },
    /// This machine's own display configuration changed (the ~1 s poll in
    /// [`crate::topology_state`]).
    LocalDisplayChanged,
    /// The config file's `[layout]` changed to a valid drawn arrangement.
    LocalLayoutEdited(Box<Layout>),
}

/// The worker's layout-sync engine.
pub struct TopologySync {
    local: DeviceId,
    display: Arc<dyn DisplayInfo>,
    commands: CommandSender,
    metrics: Arc<Metrics>,
    state: Option<Arc<TopologyStateWriter>>,
    /// The arrangement in force for this run, or `None` when there is no
    /// drawn one (an implicit side-model run, or no arrangement at all).
    layout: Option<Layout>,
    /// Where an adopted arrangement is published so the detector and the
    /// cursor-placement path pick it up
    /// ([`crossover_core::live_crossing_source`]). `None` for a run with no
    /// drawn arrangement to replace — see [`Self::publish`].
    publisher: Option<watch::Sender<LiveLayout>>,
    publication: u64,
    sessions: HashMap<Uuid, PeerSession>,
    persist: LayoutPersist,
    /// When the user-facing narration last spoke, so a peer cannot make
    /// this run scroll its console (see [`Self::may_narrate`]).
    last_narrated: Option<Instant>,
    /// The receiving end of the hub's own queue, held here rather than
    /// taken by [`Self::run`] as an argument, so a caller cannot pair one
    /// hub's receiver with another hub's state.
    events: mpsc::Receiver<TopologyEvent>,
    /// Asked to stop: flush what is pending and return, so a clean quit
    /// inside a coalescing window does not lose an adopted arrangement.
    shutdown: watch::Receiver<bool>,
}

/// Who is at the other end of one session, and what that end has done.
///
/// Every counter and latch here is **per session** on purpose: they bound
/// one peer's behaviour on one connection, and a fresh connection starts
/// clean rather than inheriting a grudge.
#[derive(Debug, Clone)]
struct PeerSession {
    device: DeviceId,
    name: String,
    /// The two machines a layout on this session may describe, resolved
    /// once at establishment. `None` for the degenerate case of a peer
    /// reporting this machine's own device id — which no layout can
    /// describe, and which is therefore said once here rather than on
    /// every frame.
    pair: Option<DevicePair>,
    /// Semantically impossible layouts this peer has sent
    /// (docs/PROTOCOL.md §7's graduated rule).
    violations: u32,
    /// Answers already spent telling this peer its arrangement lost
    /// ([`MAX_KEEP_LOCAL_ANSWERS`]).
    answers: u32,
    /// Whether this session has already been told to terminate. Its frames
    /// are dropped after that: the kill is in flight, and re-terminating on
    /// every frame that arrives in the meantime would be a command per
    /// frame for a session that is already going.
    terminated: bool,
    /// What has already been said about this peer.
    said: Said,
}

/// One latch per repeatable condition, so each is stated **once per
/// session** rather than once per frame.
///
/// Every one of them describes a standing state of affairs — the
/// arrangement this run holds is not this pair's, two arrangements tie on
/// the ordering key, the answer budget is spent — so a peer able to make
/// one true can make it true on every frame it sends, and a diagnostic
/// that repeated per frame would bury the log it exists to serve.
#[derive(Debug, Clone, Default)]
struct Said {
    held_stale: bool,
    key_tie: bool,
    answer_budget: bool,
}

impl PeerSession {
    fn new(device: DeviceId, name: String, local: DeviceId) -> Self {
        // Once, at establishment: the pair cannot change while the session
        // lives, and the degenerate case is a property of the two
        // identities rather than of any frame.
        let pair = match DevicePair::new(local, device) {
            Ok(pair) => Some(pair),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    peer = %device,
                    "layout sync: the peer reports this machine's own device id; no \
                     arrangement can describe that pair, so none is sent or adopted on \
                     this session"
                );
                None
            }
        };
        Self {
            device,
            name,
            pair,
            violations: 0,
            answers: 0,
            terminated: false,
            said: Said::default(),
        }
    }
}

/// Everything the composition root has to hand the hub. A struct rather
/// than eight parameters, because every one of them is a distinct
/// collaborator and a positional list of that length is a bug waiting for a
/// refactor.
pub struct TopologyInputs {
    /// This machine's identity, as a layout names it.
    pub local: DeviceId,
    /// The display handle the hub states `MonitorTopology` from — the same
    /// one the state file's own-display poll uses, on its own cadence.
    pub display: Arc<dyn DisplayInfo>,
    /// Where frames go out.
    pub commands: CommandSender,
    /// The run's metrics registry.
    pub metrics: Arc<Metrics>,
    /// The state-file writer, or `None` when no home directory resolved.
    pub state: Option<Arc<TopologyStateWriter>>,
    /// The arrangement this run started with, if it is a drawn one.
    pub layout: Option<Layout>,
    /// The live-arrangement publisher, when this run has a drawn
    /// arrangement driving its crossings.
    pub publisher: Option<watch::Sender<LiveLayout>>,
    /// Where an adopted arrangement is persisted. `None` disables
    /// persistence (no home directory), which is logged once at adoption.
    pub config_path: Option<PathBuf>,
}

/// What the composition root keeps of a started hub, so an orderly
/// shutdown can reach it (ADR 0018: a clean quit inside a coalescing
/// window must not lose an adopted arrangement).
pub struct TopologyHandle {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl TopologyHandle {
    /// The handle for a hub the composition root has just spawned.
    #[must_use]
    pub fn new(shutdown: watch::Sender<bool>, task: tokio::task::JoinHandle<()>) -> Self {
        Self { shutdown, task }
    }

    /// Ask the hub to land whatever it still owes the disk, and wait for
    /// it — the counterpart to `TopologyStateWriter::write_final`, and
    /// called beside it.
    ///
    /// Bounded, because shutdown must be: a hub wedged behind a hung
    /// filesystem must not hold the whole process open. Past the bound the
    /// run exits with the pending write unmade, which is the ordinary
    /// crash window ADR 0018 already accepts.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        if tokio::time::timeout(SHUTDOWN_FLUSH_TIMEOUT, self.task)
            .await
            .is_err()
        {
            tracing::warn!(
                "layout sync: did not finish its final config write in time; an arrangement \
                 adopted in the last few seconds may not have been saved"
            );
        }
    }
}

/// How long shutdown waits for the hub's final write. Generous for a
/// few-kilobyte atomic write, short enough that quitting still feels like
/// quitting.
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Why the hub's loop woke up.
enum Wake {
    /// An event arrived, or the last sender went away (`None`).
    Event(Option<TopologyEvent>),
    /// The coalescing interval lapsed and a write is due.
    Flush,
    /// The run is stopping.
    Shutdown,
}

impl TopologySync {
    /// Build the hub and the channel the rest of the run talks to it on.
    #[must_use]
    pub fn start(
        inputs: TopologyInputs,
    ) -> (Self, mpsc::Sender<TopologyEvent>, watch::Sender<bool>) {
        let (sender, receiver) = mpsc::channel(TOPOLOGY_QUEUE_DEPTH);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let hub = Self {
            local: inputs.local,
            display: inputs.display,
            commands: inputs.commands,
            metrics: inputs.metrics,
            state: inputs.state,
            layout: inputs.layout,
            publisher: inputs.publisher,
            publication: 0,
            sessions: HashMap::new(),
            persist: LayoutPersist::new(inputs.config_path),
            last_narrated: None,
            events: receiver,
            shutdown: shutdown_rx,
        };
        (hub, sender, shutdown_tx)
    }

    /// Serve events until the run stops, flushing a coalesced config write
    /// when its interval lapses.
    ///
    /// Spawned as its own task, the same shape as the clipboard and control
    /// drivers.
    pub async fn run(mut self) {
        loop {
            let deadline = self.persist.deadline();
            // The borrows are confined to this block, so the arms below are
            // free to take `&mut self` again.
            let wake = {
                let events = &mut self.events;
                let shutdown = &mut self.shutdown;
                tokio::select! {
                    event = events.recv() => Wake::Event(event),
                    () = wait_until(deadline) => Wake::Flush,
                    () = shutdown_requested(shutdown) => Wake::Shutdown,
                }
            };
            match wake {
                Wake::Event(Some(event)) => self.handle(event).await,
                Wake::Event(None) | Wake::Shutdown => break,
                Wake::Flush => self.persist.flush(),
            }
        }
        // Asked to stop, or every producer is gone: either way a coalesced
        // write still pending has nothing left to coalesce with, so land it
        // rather than let it expire with the task. A worker *killed* rather
        // than asked to stop never reaches here, and ADR 0018 accepts that
        // window — a restart re-syncs from the peer, so the layout a crash
        // loses is the layout the reconnect restores.
        self.persist.flush();
        tracing::debug!("layout sync: stopped");
    }

    async fn handle(&mut self, event: TopologyEvent) {
        match event {
            TopologyEvent::SessionEstablished {
                session,
                peer_device,
                peer_name,
            } => {
                self.session_established(session, peer_device, peer_name)
                    .await;
            }
            TopologyEvent::SessionLost { session } => self.session_lost(session),
            TopologyEvent::Frame { session, frame } => self.frame(session, frame).await,
            TopologyEvent::LocalDisplayChanged => self.local_display_changed().await,
            TopologyEvent::LocalLayoutEdited(layout) => self.local_layout_edited(*layout).await,
        }
    }

    // ---- outbound -------------------------------------------------------

    async fn session_established(&mut self, session: Uuid, peer_device: Uuid, peer_name: String) {
        let peer = PeerSession::new(
            DeviceId::from_bytes(*peer_device.as_bytes()),
            peer_name,
            self.local,
        );
        if let Some(writer) = &self.state {
            writer.set_peer_session(peer.device, peer.name.clone(), true);
        }
        self.sessions.insert(session, peer.clone());

        self.send_monitor_topology(FrameTarget::Session(session))
            .await;
        // Our own arrangement, if we hold a drawn one that describes this
        // pair. Stating it is what lets the peer discover it is behind and
        // adopt; if the peer is ahead, its own `LayoutSync` (sent for the
        // same reason) settles it the other way.
        self.send_layout_sync(session, &peer).await;
    }

    /// A session ended: the peer it belonged to is offline, unless another
    /// live session still reaches the same machine.
    ///
    /// Both guards matter. A run can hold an inbound and an outbound
    /// session at once, so "a session ended" is not "the peer is gone"; and
    /// the peer this document *names* may already be a different machine
    /// after a re-pair, so the disconnect is reported by device rather than
    /// by implication. The last-known monitors stay either way — an editor
    /// that empties itself the moment the link drops is an editor you
    /// cannot use to fix the link (ADR 0018).
    fn session_lost(&mut self, session: Uuid) {
        let Some(gone) = self.sessions.remove(&session) else {
            return;
        };
        if self
            .sessions
            .values()
            .any(|peer| peer.device == gone.device)
        {
            tracing::debug!(
                session = %session,
                peer = %gone.device,
                "layout sync: one session to this peer ended; another is still up"
            );
            return;
        }
        if let Some(writer) = &self.state {
            writer.set_peer_connected(gone.device, false);
        }
    }

    async fn local_display_changed(&mut self) {
        // Broadcast rather than per-session: every live peer needs the new
        // geometry, and a machine with two sessions would otherwise tell
        // only one of them.
        self.send_monitor_topology(FrameTarget::Broadcast).await;
    }

    /// This machine's own live monitors, stated to `target`.
    ///
    /// A machine that enumerates more monitors than
    /// `MAX_MONITORS_PER_MACHINE` **refuses to send** rather than
    /// truncating — ADR 0018's rule, so seamless transfer degrades
    /// observably rather than describing a desk with screens missing.
    /// [`live_monitors`] is the one implementation of that rule, shared
    /// with the state file's own reporting.
    async fn send_monitor_topology(&self, target: FrameTarget) {
        if self.sessions.is_empty() {
            return;
        }
        let mut monitors = match live_monitors(&*self.display) {
            Ok(monitors) => monitors,
            Err(LiveMonitorsError::Unavailable(error)) => {
                tracing::warn!(
                    error = %error,
                    "layout sync: the display would not enumerate; MonitorTopology not sent"
                );
                return;
            }
            Err(LiveMonitorsError::TooManyMonitors { count }) => {
                // ADR 0018's rule, and the reason it is a rule: truncating
                // would describe this desk to the peer with screens
                // missing, which is worse than saying nothing.
                tracing::error!(
                    count,
                    max = crossover_topology::MAX_MONITORS_PER_MACHINE,
                    "layout sync: more monitors than this build can describe; refusing to \
                     state an incomplete desk (ADR 0018) — seamless transfer degrades"
                );
                return;
            }
        };
        if monitors.is_empty() {
            tracing::debug!(
                "layout sync: no nameable monitors to report; MonitorTopology not sent"
            );
            return;
        }
        // Captions and panel sizes the display would not repeat this
        // instant, filled in from what the state writer last saw. Geometry
        // is always this query's own; only the descriptive fields come from
        // memory, and only where this query had none — see
        // `TopologyStateWriter::set_monitors` for why a description is
        // remembered rather than re-read. Without this the state file and
        // the wire would describe the same desk differently whenever a
        // description sweep failed to coincide with a real display change.
        if let Some(writer) = &self.state {
            writer.fill_remembered(&mut monitors);
        }
        let message = MonitorTopology {
            monitors: monitors.iter().map(report_of).collect(),
        };
        match message.encode_payload() {
            Ok(payload) => {
                self.send(target, MessageType::MonitorTopology, payload)
                    .await;
            }
            Err(error) => {
                // Encode-side validation refusing our own message is a
                // local defect, never a peer's doing — and exactly what
                // validating on encode exists to catch before the peer has
                // to (docs/PROTOCOL.md §6.2).
                tracing::warn!(
                    error = %error,
                    monitors = monitors.len(),
                    "layout sync: this machine's own monitor report is not sendable"
                );
            }
        }
    }

    /// State this run's drawn arrangement to `peer`, if it has one that
    /// describes this session's pair.
    ///
    /// The pair check is not politeness. A layout left over from a previous
    /// pairing names a machine that is not at the other end, and sending it
    /// would earn this machine a protocol violation for a fault that is
    /// entirely local — so what cannot be believed is not sent. The same
    /// check on the receiving side is [`Self::held_contender`]'s, and for
    /// the same reason.
    async fn send_layout_sync(&self, session: Uuid, peer: &PeerSession) {
        let Some(layout) = &self.layout else {
            return; // no drawn arrangement: nothing to sync (an implicit
            // one is never sent — ADR 0018)
        };
        let Some(pair) = peer.pair else {
            return;
        };
        if let Err(error) = describes(layout, &pair) {
            tracing::warn!(
                error = %error,
                revision = layout.revision(),
                origin = %layout.origin(),
                peer = %peer.device,
                "layout sync: the arrangement this run holds does not describe this session's \
                 pair; it is not sent (redraw it with `crossover layout`)"
            );
            return;
        }
        let message = LayoutSync {
            revision: layout.revision(),
            origin: layout.origin(),
            monitors: layout.monitors().to_vec(),
        };
        match message.encode_payload() {
            Ok(payload) => {
                self.send(
                    FrameTarget::Session(session),
                    MessageType::LayoutSync,
                    payload,
                )
                .await;
                self.metrics.record_layout_sent();
                tracing::info!(
                    session = %session,
                    revision = layout.revision(),
                    origin = %layout.origin(),
                    "layout sync: stated this machine's arrangement to the peer"
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    revision = layout.revision(),
                    "layout sync: this run's arrangement is not sendable"
                );
            }
        }
    }

    async fn send(&self, target: FrameTarget, message_type: MessageType, payload: Vec<u8>) {
        let _ = self
            .commands
            .send(SessionCommand::SendFrame {
                target,
                message_type: message_type.wire(),
                payload,
            })
            .await;
    }

    // ---- inbound --------------------------------------------------------

    async fn frame(&mut self, session: Uuid, frame: RawFrame) {
        // A session already told to terminate is going, and the frames it
        // sent before the kill landed are not worth answering: acting on
        // them would re-terminate a session per frame, which is a command
        // per frame for a peer whose whole problem is sending too many.
        if self
            .sessions
            .get(&session)
            .is_some_and(|peer| peer.terminated)
        {
            tracing::debug!(
                session = %session,
                message_type = frame.message_type,
                "layout sync: dropping a frame from a session already being terminated"
            );
            return;
        }
        if frame.message_type == MessageType::MonitorTopology.wire() {
            match MonitorTopology::decode_payload(&frame.payload) {
                Ok(message) => self.peer_monitors(session, message),
                Err(error) => self.terminate_malformed(session, &error).await,
            }
            return;
        }
        if frame.message_type == MessageType::LayoutSync.wire() {
            match LayoutSync::decode_payload(&frame.payload) {
                Ok(message) => self.peer_layout(session, message).await,
                Err(error) => self.terminate_malformed(session, &error).await,
            }
            return;
        }
        // Unreachable: `commands::inbound_route` sends exactly these two
        // types here. Logged rather than ignored, because a silent drop is
        // how a misroute survives a refactor.
        tracing::warn!(
            message_type = frame.message_type,
            "layout sync: a frame that is not display topology reached the topology hub"
        );
    }

    /// The peer stated its own live monitors: record them as the state
    /// file's peer half, for the editor to draw.
    ///
    /// This never changes crossing behaviour on its own (ADR 0018) — it is
    /// a fact about the sender, not an arrangement.
    fn peer_monitors(&mut self, session: Uuid, message: MonitorTopology) {
        let Some(peer) = self.sessions.get(&session) else {
            tracing::debug!(session = %session, "layout sync: monitors for an ended session");
            return;
        };
        let monitors: Vec<LiveMonitor> = message.monitors.into_iter().map(live_of).collect();
        tracing::debug!(
            session = %session,
            peer = %peer.device,
            monitors = monitors.len(),
            "layout sync: the peer stated its monitors"
        );
        if let Some(writer) = &self.state {
            writer.set_peer_monitors(peer.device, monitors);
        }
    }

    /// The peer stated an arrangement: validate it against this session's
    /// pair, then resolve.
    async fn peer_layout(&mut self, session: Uuid, message: LayoutSync) {
        let Some(peer) = self.sessions.get(&session).cloned() else {
            tracing::debug!(session = %session, "layout sync: an arrangement for an ended session");
            return;
        };
        let Some(pair) = peer.pair else {
            return;
        };
        // The semantic half of docs/PROTOCOL.md §6.2, which the wire
        // decoder deliberately leaves to whoever knows the session's pair:
        // is this a believable arrangement of *these two machines*?
        let received = match Layout::new(message.revision, message.origin, message.monitors, &pair)
        {
            Ok(layout) => layout,
            Err(error) => {
                self.reject(session, &peer, message.revision, &error).await;
                return;
            }
        };

        // The arrangement this run holds is only a *contender* if it too
        // describes this pair — see `held_contends`.
        let contends = self.held_contends(session, &pair);
        let held = self.layout.as_ref().filter(|_| contends);
        let tie = held.is_some_and(|held| keys_tie(held, &received));
        let resolution = resolve(held, &received);
        tracing::debug!(
            session = %session,
            resolution = resolution.label(),
            received_revision = received.revision(),
            received_origin = %received.origin(),
            held_revision = ?held.map(Layout::revision),
            "layout sync: resolved the peer's arrangement"
        );
        if tie && resolution != Resolution::Identical {
            self.warn_key_tie(session, &received);
        }
        match resolution {
            Resolution::AdoptReceived => self.adopt(received),
            Resolution::KeepLocal => self.answer(session, &peer).await,
            // Identical: nothing to adopt and nothing to answer. This is
            // the arm that keeps two synced machines silent.
            Resolution::Identical => {}
        }
    }

    /// Can the arrangement this run holds **compete on this session** —
    /// that is, does it describe this session's pair?
    ///
    /// This is the check that keeps a re-pair from deadlocking a desk. A
    /// layout left over from a previous pairing names machines that are not
    /// here; it is never sent (see [`Self::send_layout_sync`]) and, without
    /// this, it would still *win* every resolution against the peer's real
    /// arrangement, on nothing but a high revision. The result is a pair
    /// that never converges and never says why: this machine adopts
    /// nothing, sends nothing, and crosses by an arrangement describing
    /// other desks. Demoting it to "no arrangement" makes the peer's the
    /// only contender, so the pair converges on something true and the
    /// stale one is superseded in the ordinary way — which is exactly the
    /// repair ADR 0018 describes, without waiting for a redraw.
    ///
    /// Said once per session, not once per frame: the condition is a
    /// property of the config and the pairing, and neither changes between
    /// two frames.
    fn held_contends(&mut self, session: Uuid, pair: &DevicePair) -> bool {
        let Some(held) = self.layout.as_ref() else {
            return false;
        };
        let Err(error) = describes(held, pair) else {
            return true;
        };
        let revision = held.revision();
        let origin = held.origin();
        if let Some(peer) = self.sessions.get_mut(&session)
            && !peer.said.held_stale
        {
            peer.said.held_stale = true;
            tracing::warn!(
                session = %session,
                error = %error,
                revision,
                origin = %origin,
                "layout sync: the arrangement this run holds describes a different pairing, \
                 so it does not compete here — whatever this peer draws wins outright \
                 (ADR 0018's re-pair repair)"
            );
        }
        false
    }

    /// Tell the peer its arrangement lost, so it adopts ours — bounded, so
    /// a peer that ignores the answer stops earning frames
    /// ([`MAX_KEEP_LOCAL_ANSWERS`]).
    async fn answer(&mut self, session: Uuid, peer: &PeerSession) {
        let Some(live) = self.sessions.get_mut(&session) else {
            return;
        };
        if live.answers >= MAX_KEEP_LOCAL_ANSWERS {
            if !live.said.answer_budget {
                live.said.answer_budget = true;
                tracing::warn!(
                    session = %session,
                    peer = %peer.device,
                    answers = live.answers,
                    "layout sync: this peer keeps re-stating an arrangement this machine has \
                     already superseded; no longer answering it on this session"
                );
            }
            return;
        }
        live.answers = live.answers.saturating_add(1);
        self.send_layout_sync(session, peer).await;
    }

    /// Two arrangements claiming the same revision *and* the same origin —
    /// something one machine cannot produce, decided by content hash and
    /// worth saying (ADR 0018 calls it the anomaly it is). Latched, because
    /// a peer able to cause it once can cause it on every frame.
    fn warn_key_tie(&mut self, session: Uuid, received: &Layout) {
        let Some(peer) = self.sessions.get_mut(&session) else {
            return;
        };
        if peer.said.key_tie {
            return;
        }
        peer.said.key_tie = true;
        tracing::warn!(
            session = %session,
            revision = received.revision(),
            origin = %received.origin(),
            "layout sync: two different arrangements claim the same revision and origin; \
             resolving by content hash (ADR 0018)"
        );
    }

    /// A well-formed arrangement that cannot be true of this pair
    /// (docs/PROTOCOL.md §6.2, §7): refuse it, say so, charge it as a
    /// violation — and keep the session, because a peer that disagrees
    /// with reality must not cost a healthy session its first frame.
    ///
    /// **Until it keeps doing it.** §7's rule is graduated, and the count
    /// is what makes the leniency safe: without it, unadoptable layouts
    /// would be free for the sender and unbounded log volume here. Past
    /// [`MAX_LAYOUT_VIOLATIONS`] the session goes, through the same
    /// fail-closed path a malformed frame takes.
    async fn reject(
        &mut self,
        session: Uuid,
        peer: &PeerSession,
        revision: u64,
        error: &LayoutError,
    ) {
        self.metrics.record_layout_rejected();
        let violations = match self.sessions.get_mut(&session) {
            Some(peer) => {
                peer.violations = peer.violations.saturating_add(1);
                peer.violations
            }
            None => 1,
        };
        tracing::warn!(
            session = %session,
            peer = %peer.device,
            revision,
            violations,
            error = %error,
            "layout sync: refusing a layout that is not a believable arrangement of this \
             session's pair; it is not adopted (protocol violation, docs/PROTOCOL.md §6.2)"
        );
        if violations >= MAX_LAYOUT_VIOLATIONS {
            tracing::error!(
                session = %session,
                peer = %peer.device,
                violations,
                max = MAX_LAYOUT_VIOLATIONS,
                "layout sync: terminating the session on repeated unbelievable arrangements \
                 (docs/PROTOCOL.md §7)"
            );
            self.terminate(
                session,
                format!("{violations} unbelievable display arrangements on one session"),
            )
            .await;
            return;
        }
        println!("Refused the peer's display arrangement: {error}. Nothing changed here.");
    }

    /// A frame this build cannot decode at all: fail closed
    /// (docs/PROTOCOL.md §7), through the same `TerminateSession` path
    /// every other driver's payload violations take.
    async fn terminate_malformed(
        &mut self,
        session: Uuid,
        error: &crossover_protocol::ProtocolError,
    ) {
        tracing::error!(
            session = %session,
            error = %error,
            "layout sync: malformed display-topology frame; terminating the session \
             (docs/PROTOCOL.md §7)"
        );
        self.terminate(
            session,
            format!("malformed display-topology frame: {error}"),
        )
        .await;
    }

    /// Fire the fail-closed lever once, and mark the session so the frames
    /// already in flight behind it are dropped rather than each firing it
    /// again ([`Self::frame`]).
    async fn terminate(&mut self, session: Uuid, reason: String) {
        if let Some(peer) = self.sessions.get_mut(&session) {
            peer.terminated = true;
        }
        let _ = self
            .commands
            .send(SessionCommand::TerminateSession {
                target: FrameTarget::Session(session),
                reason,
            })
            .await;
    }

    // ---- adoption -------------------------------------------------------

    /// The config file changed to a drawn arrangement. A genuinely newer
    /// one becomes this run's, and is stated to every live session.
    ///
    /// No persistence happens here: the arrangement *came from* the file,
    /// so writing it back would be a write nobody asked for — and the
    /// content-equality no-op that keeps this run's own adoption-writes
    /// from echoing into a sync loop is exactly [`Resolution::Identical`]
    /// below.
    async fn local_layout_edited(&mut self, layout: Layout) {
        if !self.usable_here(&layout) {
            return;
        }
        match resolve(self.layout.as_ref(), &layout) {
            Resolution::AdoptReceived => {
                tracing::info!(
                    revision = layout.revision(),
                    origin = %layout.origin(),
                    "layout sync: the config file now names a newer arrangement"
                );
                self.take(layout);
                self.report();
                // Per session, not broadcast: a `LayoutSync` is only
                // believable to the peer it describes, so each one is
                // re-checked against its own session's pair.
                for (session, peer) in self.live_sessions() {
                    self.send_layout_sync(session, &peer).await;
                }
            }
            Resolution::KeepLocal => self.config_lost(&layout),
            Resolution::Identical => {}
        }
    }

    /// Could this run actually *use* an arrangement read from the config —
    /// does it describe a pair this machine is currently in?
    ///
    /// With no session up there is nothing to judge against, so anything is
    /// taken: that is the ordinary startup and edit-while-disconnected
    /// case. With sessions up, an arrangement describing none of their
    /// pairs is the residue of a re-pair (or a hand-edit naming the wrong
    /// machines). Taking it would mean this run published, and briefly
    /// crossed by, an arrangement of other desks — and it could not be
    /// sent, so nothing would correct it until the peer next spoke. The
    /// mirror of [`Self::held_contends`], on the way in from the file
    /// rather than from the wire.
    fn usable_here(&self, layout: &Layout) -> bool {
        let mut pairs = self
            .sessions
            .values()
            .filter_map(|peer| peer.pair)
            .peekable();
        if pairs.peek().is_none() {
            return true;
        }
        if pairs.any(|pair| describes(layout, &pair).is_ok()) {
            return true;
        }
        tracing::warn!(
            revision = layout.revision(),
            origin = %layout.origin(),
            "layout sync: the config file names an arrangement of machines this run is not \
             connected to; it is not used (redraw it with `crossover layout` after pairing)"
        );
        false
    }

    /// The config file names an arrangement that **lost** to the one this
    /// run holds. Two very different situations wear that shape, and
    /// telling them apart is the difference between noise and NFR-3.
    ///
    /// The ordinary one: the file is simply *behind*, because a
    /// rate-bounded adoption write is still pending, so what the poll read
    /// is this machine's own older write coming back. Nothing has been
    /// lost, the pending write settles it, and saying anything would cry
    /// wolf every few seconds.
    ///
    /// The one that matters: a **different** arrangement at an
    /// equal-or-lower key — which is what an editor save that lost a race
    /// looks like from here. Somebody drew that, at this desk, and it is
    /// not going to be used. They are the loser ADR 0018's supersession
    /// diagnostic is written for, so they get it in full: both revisions,
    /// both origins, on the console and in the log.
    fn config_lost(&mut self, from_file: &Layout) {
        let held_revision = self.layout.as_ref().map(Layout::revision);
        if self
            .persist
            .last_written()
            .is_some_and(|written| resolve(Some(written), from_file) == Resolution::Identical)
        {
            tracing::debug!(
                file_revision = from_file.revision(),
                held_revision = ?held_revision,
                "layout sync: the config file is behind this run's arrangement (a coalesced \
                 write of ours is still pending)"
            );
            return;
        }
        let Some(held) = &self.layout else {
            return;
        };
        tracing::warn!(
            adopted_revision = held.revision(),
            adopted_origin = %held.origin(),
            superseded_revision = from_file.revision(),
            superseded_origin = %from_file.origin(),
            "layout sync: the arrangement just saved to the config file is superseded by a \
             newer one this run already holds; it will not be used"
        );
        println!(
            "The display arrangement just saved (revision {}, origin {}) is superseded by \
             revision {} (origin {}), which this machine already holds. Re-open `crossover \
             layout` to edit the newer one.",
            from_file.revision(),
            from_file.origin(),
            held.revision(),
            held.origin(),
        );
    }

    /// Adopt an arrangement drawn at the other desk: **persist, publish,
    /// report**, in that order (ADR 0018).
    fn adopt(&mut self, received: Layout) {
        let superseded = self
            .layout
            .as_ref()
            .map(|held| (held.revision(), held.origin()));
        // 1. Persist first, because that order is the crash-safe one.
        self.persist.offer(received.clone());
        // 2. Publish to the live topology, immediately — the rate bound is
        //    on the disk, never on where this machine crosses.
        self.take(received);
        // 3. Report.
        self.report();
        self.metrics.record_layout_adopted();

        // Narration is rate-limited, the diagnostic is not: a peer feeding
        // revisions cannot be allowed to scroll a console, but every
        // adoption still leaves a record. Above the limit the same fields
        // go out at `debug`; the *first* of a burst always speaks, which is
        // the one a user would notice anyway.
        let loud = self.may_narrate();
        let Some(adopted) = &self.layout else {
            return;
        };
        // The supersession diagnostic ADR 0018 requires of the loser: both
        // revisions and both origins, so a user whose drawing vanished can
        // learn why rather than concluding the editor is broken (NFR-3).
        match (superseded, loud) {
            (Some((revision, origin)), true) => {
                tracing::warn!(
                    adopted_revision = adopted.revision(),
                    adopted_origin = %adopted.origin(),
                    superseded_revision = revision,
                    superseded_origin = %origin,
                    "layout sync: adopted the peer's arrangement; the one this machine held \
                     is superseded"
                );
                println!(
                    "Adopted the display arrangement drawn on the peer (revision {}, origin \
                     {}); this machine's revision {revision} (origin {origin}) is superseded.",
                    adopted.revision(),
                    adopted.origin(),
                );
            }
            (Some((revision, origin)), false) => tracing::debug!(
                adopted_revision = adopted.revision(),
                adopted_origin = %adopted.origin(),
                superseded_revision = revision,
                superseded_origin = %origin,
                "layout sync: adopted the peer's arrangement (narration rate-limited)"
            ),
            (None, true) => {
                tracing::info!(
                    adopted_revision = adopted.revision(),
                    adopted_origin = %adopted.origin(),
                    "layout sync: adopted the peer's arrangement; this machine held none"
                );
                println!(
                    "Adopted the display arrangement drawn on the peer (revision {}).",
                    adopted.revision(),
                );
            }
            (None, false) => tracing::debug!(
                adopted_revision = adopted.revision(),
                adopted_origin = %adopted.origin(),
                "layout sync: adopted the peer's arrangement (narration rate-limited)"
            ),
        }
        // Only on a loud adoption: this reads the display (the identity
        // query, not the cheap one), and a peer feeding revisions must not
        // be able to make this machine enumerate its monitors per frame.
        // Rate-limiting it costs nothing — the condition it reports is a
        // standing one, so the next narration window says it just as well.
        if loud {
            self.warn_if_inert_here();
        }
    }

    /// May the user-facing narration speak right now?
    ///
    /// One line per [`LAYOUT_PERSIST_INTERVAL`], the same window the config
    /// write is bounded by and for the same reason: adoption is driven by
    /// network input, and a peer streaming revisions would otherwise own
    /// this machine's console and its log volume. The first of a burst
    /// always speaks; the rest are still recorded, at `debug`.
    fn may_narrate(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_narrated
            .is_some_and(|last| now.duration_since(last) < LAYOUT_PERSIST_INTERVAL)
        {
            return false;
        }
        self.last_narrated = Some(now);
        true
    }

    /// Say so when the arrangement just adopted matches **none** of this
    /// machine's live screens.
    ///
    /// Such a layout is legal and is deliberately not rejected: an
    /// arrangement may name a monitor that is unplugged right now, which is
    /// exactly what makes a drawing survive a dock and an undock
    /// (docs/PROTOCOL.md §6.2). What it produces is an *inert* crossing map
    /// — no spans, so no seam crosses anywhere — and the failure mode this
    /// closes is discovering that at the seam, days later, by pushing a
    /// cursor at an edge that does nothing.
    ///
    /// Only when this machine actually has screens to match against: a
    /// display that will not enumerate is a different fault, and one
    /// `live_monitors` has already reported.
    ///
    /// Asks [`live_monitor_ids`] rather than `live_monitors`, because ids
    /// are the entire question here and the fuller query would drag a
    /// `QueryDisplayConfig` sweep along behind a log line — breaking ADR
    /// 0018's promise that only the ~1 s topology cadence pays for reading
    /// product names.
    fn warn_if_inert_here(&self) {
        let Some(adopted) = &self.layout else {
            return;
        };
        let Ok(live) = live_monitor_ids(&*self.display) else {
            return;
        };
        if live.is_empty() {
            return;
        }
        if adopted
            .monitors_for(self.local)
            .any(|drawn| live.contains(&drawn.id))
        {
            return;
        }
        let drawn: Vec<&str> = adopted
            .monitors_for(self.local)
            .map(|monitor| monitor.id.as_str())
            .collect();
        let attached: Vec<&str> = live.iter().map(MonitorId::as_str).collect();
        tracing::warn!(
            revision = adopted.revision(),
            drawn = ?drawn,
            attached = ?attached,
            "layout sync: the adopted arrangement names none of this machine's attached \
             screens, so nothing here crosses anywhere until one of them comes back"
        );
        println!(
            "Note: the adopted arrangement draws this machine's screens as {drawn:?}, but \
             {attached:?} are attached. Seamless transfer is off here until they match — \
             redraw with `crossover layout`."
        );
    }

    /// Make `layout` this run's arrangement and publish it to the live
    /// crossing source.
    ///
    /// The publication counter increases on every take, never derived from
    /// the layout's revision: two arrangements can share a revision and
    /// differ (ADR 0018's origin tiebreak), and a detector that missed that
    /// case would keep crossing by the one that lost.
    fn take(&mut self, layout: Layout) {
        self.layout = Some(layout.clone());
        let Some(publisher) = &self.publisher else {
            // A run with no drawn arrangement of its own has no live
            // crossing source to replace — the deprecated side model, or
            // seamless off. The arrangement is persisted all the same, so
            // the next start crosses by it; saying so is what keeps that
            // from looking like the adoption did nothing.
            tracing::info!(
                revision = layout.revision(),
                "layout sync: this run crosses by no drawn arrangement, so the adopted one \
                 takes effect at the next start (it is saved to the config now)"
            );
            return;
        };
        self.publication = self.publication.saturating_add(1);
        let _ = publisher.send(LiveLayout {
            publication: self.publication,
            layout,
        });
    }

    /// Write this run's arrangement into the state file — the report, and
    /// deliberately the last step.
    fn report(&self) {
        if let Some(writer) = &self.state {
            writer.set_layout(self.layout.as_ref().map(LayoutState::from_layout));
        }
    }

    // ---- helpers --------------------------------------------------------

    /// Every live session with its peer, as owned values — so the caller
    /// can `await` per session without holding a borrow of `self`.
    fn live_sessions(&self) -> Vec<(Uuid, PeerSession)> {
        self.sessions
            .iter()
            .filter(|(_, peer)| !peer.terminated)
            .map(|(session, peer)| (*session, peer.clone()))
            .collect()
    }
}

/// Sleep until `deadline`, or forever when there is nothing pending.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Resolve once the run has asked the hub to stop.
///
/// Checks the level before waiting, so a request that arrived before this
/// future was created is not missed — and pends forever if the sender is
/// dropped without asking, because that is `run`'s other exit and the
/// event channel closing is what signals it.
async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow_and_update() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow_and_update() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

/// Does `layout` describe `pair` — is it a believable arrangement of
/// *these two machines*?
///
/// The verdict only; the rebuilt layout is discarded because it is
/// identical to the input by construction. Re-running the whole of
/// [`Layout::new`] rather than keeping a private copy of its
/// pair-dependent half (origin membership, device membership, both
/// machines present) is deliberate: two copies of a validation rule are
/// exactly the thing that drifts, and the cost is one bounded allocation
/// and at most 496 comparisons on a path that runs per session, not per
/// crossing.
fn describes(layout: &Layout, pair: &DevicePair) -> Result<(), LayoutError> {
    Layout::new(
        layout.revision(),
        layout.origin(),
        layout.monitors().to_vec(),
        pair,
    )
    .map(|_| ())
}

/// One of this machine's live monitors, as `MonitorTopology` states it.
///
/// A free function rather than the `From` impl it wants to be: both types
/// are foreign to this crate ([`LiveMonitor`] is `crossover-topology`'s,
/// [`MonitorReport`] is `crossover-protocol`'s), so the orphan rule
/// forbids it — and neither crate may take a dependency on the other's
/// half to host it.
fn report_of(monitor: &LiveMonitor) -> MonitorReport {
    MonitorReport {
        id: monitor.id.clone(),
        rect: monitor.rect,
        scale_percent: monitor.scale_percent,
        // Field for field, the label and the panel size included: the desk
        // the peer is told about and the desk the local editor draws are
        // the same desk, and the peer's editor captions and proportions
        // rectangles from exactly this.
        label: monitor.label.clone(),
        physical_size: monitor.physical_size,
    }
}

/// One of the peer's monitors, as the state file records it.
fn live_of(report: MonitorReport) -> LiveMonitor {
    LiveMonitor {
        id: report.id,
        rect: report.rect,
        scale_percent: report.scale_percent,
        // Both already validated — a `MonitorLabel` and a `PhysicalSizeMm`
        // cannot exist unvalidated, and the wire decoder refused anything
        // else before this frame got here.
        label: report.label,
        physical_size: report.physical_size,
    }
}

/// The rate-bounded config writer (ADR 0018, docs/SECURITY.md T23).
///
/// The first offer writes immediately; offers inside
/// [`LAYOUT_PERSIST_INTERVAL`] replace the pending one — latest wins — and
/// are written once when the interval lapses. A peer feeding distinct
/// revisions as fast as it can send therefore costs one config write per
/// interval, not one per frame.
struct LayoutPersist {
    path: Option<PathBuf>,
    last_write: Option<Instant>,
    pending: Option<Layout>,
    /// What actually reached the file, so the hub can recognize its own
    /// older write coming back through the config poll and not mistake it
    /// for somebody's save being superseded
    /// ([`TopologySync::config_lost`]).
    last_written: Option<Layout>,
    /// Whether a write failure has already been logged, so a full disk or
    /// a locked file does not turn a repeating adoption into a log line
    /// per adoption (the shape `topology_state`'s writer uses too).
    warned: bool,
}

impl LayoutPersist {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            last_write: None,
            pending: None,
            last_written: None,
            warned: false,
        }
    }

    /// The arrangement this run last wrote to the config file, if any.
    fn last_written(&self) -> Option<&Layout> {
        self.last_written.as_ref()
    }

    /// Take `layout` as the arrangement to persist: now if the interval has
    /// lapsed, otherwise as the pending one.
    fn offer(&mut self, layout: Layout) {
        let ready = self
            .last_write
            .is_none_or(|last| last.elapsed() >= LAYOUT_PERSIST_INTERVAL);
        if ready {
            self.write(&layout);
            self.pending = None;
        } else {
            // Latest wins: an older pending revision has already been
            // superseded by this one, so writing it would only be a write
            // of something nobody is using.
            self.pending = Some(layout);
        }
    }

    /// When the pending write is due, or `None` when nothing is pending.
    fn deadline(&self) -> Option<Instant> {
        let last = self.last_write?;
        self.pending.as_ref()?;
        Some(last + LAYOUT_PERSIST_INTERVAL)
    }

    /// Write whatever is pending, if anything.
    fn flush(&mut self) {
        if let Some(layout) = self.pending.take() {
            self.write(&layout);
        }
    }

    fn write(&mut self, layout: &Layout) {
        // The clock advances whether or not the write succeeds: a failing
        // write must not become an unbounded retry loop against the disk,
        // which is precisely the cost T23's rate bound exists to cap.
        self.last_write = Some(Instant::now());
        let Some(path) = self.path.clone() else {
            if !self.warned {
                tracing::warn!(
                    "layout sync: no config file path resolved, so an adopted arrangement \
                     cannot be saved; it applies to this run only"
                );
                self.warned = true;
            }
            return;
        };
        match persist_layout(&path, layout) {
            Ok(()) => {
                if self.warned {
                    tracing::info!("layout sync: the config file is writable again");
                    self.warned = false;
                }
                // Only on success: a write that did not happen is not what
                // the config poll will read back.
                self.last_written = Some(layout.clone());
                tracing::info!(
                    revision = layout.revision(),
                    origin = %layout.origin(),
                    path = %path.display(),
                    "layout sync: saved the adopted arrangement (config schema 2)"
                );
            }
            Err(error) => self.write_failed(&error, &path),
        }
    }

    /// A refused or failed write. Never fatal: publication to the live
    /// topology already happened, so only persistence degrades — and it
    /// degrades observably (ADR 0018's `RevisionUnrepresentable` note).
    fn write_failed(&mut self, error: &PersistError, path: &std::path::Path) {
        if self.warned {
            return;
        }
        tracing::warn!(
            error = %error,
            path = %path.display(),
            "layout sync: could not save the adopted arrangement; it applies to this run but \
             will not survive a restart"
        );
        self.warned = true;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use tokio::sync::{mpsc, watch};
    use uuid::Uuid;

    use crossover_core::outbound::command_lanes;
    use crossover_core::{FrameTarget, LiveLayout, Metrics, SessionCommand};
    use crossover_platform::fakes::FakeDisplay;
    use crossover_platform::{DisplayInfo, MonitorInfo, MonitorRect, Screen};
    use crossover_protocol::RawFrame;
    use crossover_protocol::hello::MessageType;
    use crossover_protocol::layout::{LayoutSync, MonitorReport, MonitorTopology};
    use crossover_topology::{DeviceId, DevicePair, Layout, LayoutRect, MonitorId, PlacedMonitor};

    use super::{TopologyEvent, TopologyInputs, TopologySync};
    use crate::topology_state::{TopologyStateWriter, initial_state};

    const A: [u8; 16] = [0x11; 16];
    const B: [u8; 16] = [0x22; 16];
    const STRANGER: [u8; 16] = [0x33; 16];

    fn device(bytes: [u8; 16]) -> DeviceId {
        DeviceId::from_bytes(bytes)
    }

    fn placed(owner: DeviceId, id: &str, x: i32) -> PlacedMonitor {
        PlacedMonitor {
            device: owner,
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x,
                y: 0,
                width: 1920,
                height: 1080,
            },
        }
    }

    /// The ordinary two-desk arrangement: A's screen at the origin, B's
    /// beside it, `gap` units further right so two layouts can differ in
    /// content alone.
    fn arrangement(revision: u64, origin: [u8; 16], gap: i32) -> Layout {
        let pair = DevicePair::new(device(A), device(B)).unwrap();
        Layout::new(
            revision,
            device(origin),
            vec![
                placed(device(A), "A-SCREEN", 0),
                placed(device(B), "B-SCREEN", 1920 + gap),
            ],
            &pair,
        )
        .unwrap()
    }

    /// A private directory removed on drop — the house substitute for a
    /// `tempfile` dependency (mirrors `topology_state`'s own `Sandbox`).
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crossover-layout-sync-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("sandbox");
            Self(dir)
        }

        fn path(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A subscriber writing into a buffer, so a test can read the line a
    /// maintainer would read — several of ADR 0018's guarantees are
    /// *diagnostics* (the supersession NFR-3 requires of the loser), and
    /// the honest test of a diagnostic is the emitted line.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl CapturedLog {
        fn text(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
            )
            .expect("log output was not UTF-8")
        }
    }

    /// Capture everything logged on this thread for the life of the guard.
    fn capture() -> (CapturedLog, tracing::subscriber::DefaultGuard) {
        let sink = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (sink, guard)
    }

    /// One engine under test.
    ///
    /// The hub is driven **directly** rather than through a spawned task:
    /// every test below is about what one event causes, and calling
    /// `handle` inline makes that deterministic — no yields to guess at, no
    /// clock to wait on, and a `tracing` capture that actually sees the
    /// lines because everything runs on the test's own thread.
    struct Engine {
        hub: TopologySync,
        high: mpsc::Receiver<SessionCommand>,
        _background: crossover_core::outbound::BudgetedReceiver<SessionCommand>,
        metrics: Arc<Metrics>,
        config: PathBuf,
        state: Arc<TopologyStateWriter>,
        live: watch::Receiver<LiveLayout>,
        display: Arc<FakeDisplay>,
        session: Uuid,
    }

    impl Engine {
        async fn feed(&mut self, event: TopologyEvent) {
            self.hub.handle(event).await;
        }

        /// Bring a session up against `peer`.
        async fn connect(&mut self, peer: [u8; 16]) {
            let session = self.session;
            self.feed(TopologyEvent::SessionEstablished {
                session,
                peer_device: Uuid::from_bytes(peer),
                peer_name: "peer".to_owned(),
            })
            .await;
        }

        /// Everything the hub has queued for the wire since the last drain.
        fn drain(&mut self) -> Vec<SessionCommand> {
            let mut commands = Vec::new();
            while let Ok(command) = self.high.try_recv() {
                commands.push(command);
            }
            commands
        }

        /// Deliver `payload` to this engine as if it arrived on the wire.
        async fn receive(&mut self, message_type: MessageType, payload: Vec<u8>) {
            let session = self.session;
            self.feed(TopologyEvent::Frame {
                session,
                frame: RawFrame {
                    message_type: message_type.wire(),
                    message_id: 1,
                    payload,
                },
            })
            .await;
        }

        /// State a whole arrangement to this engine, as the peer would.
        async fn receive_layout(&mut self, layout: &Layout) {
            let payload = LayoutSync {
                revision: layout.revision(),
                origin: layout.origin(),
                monitors: layout.monitors().to_vec(),
            }
            .encode_payload()
            .expect("a validated layout encodes");
            self.receive(MessageType::LayoutSync, payload).await;
        }

        /// State a well-formed arrangement that names a third machine —
        /// never adoptable on this session, and therefore a violation.
        async fn receive_hostile(&mut self, revision: u64) {
            let payload = LayoutSync {
                revision,
                origin: device(STRANGER),
                monitors: vec![
                    placed(device(A), "A-SCREEN", 0),
                    placed(device(STRANGER), "X", 1920),
                ],
            }
            .encode_payload()
            .expect("well-formed on the wire");
            self.receive(MessageType::LayoutSync, payload).await;
        }

        async fn disconnect(&mut self) {
            let session = self.session;
            self.feed(TopologyEvent::SessionLost { session }).await;
        }

        fn held(&self) -> Option<u64> {
            self.hub.layout.as_ref().map(Layout::revision)
        }
    }

    /// Build an engine for `local`, holding `held`, writing into `sandbox`.
    fn engine(sandbox: &Sandbox, label: &str, local: [u8; 16], held: Option<Layout>) -> Engine {
        let display = Arc::new(FakeDisplay::new(Screen {
            width: 1920,
            height: 1080,
        }));
        display.set_monitor_layout(vec![MonitorInfo {
            id: Some(format!("{label}-SCREEN")),
            rect: MonitorRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
        }]);
        let config = sandbox.path(&format!("{label}-config.toml"));
        let metrics = Arc::new(Metrics::new());
        let state = Arc::new(TopologyStateWriter::start(
            sandbox.path(&format!("{label}-topology.json")),
            initial_state(device(local), label.to_owned(), &*display, None),
        ));
        // Every engine gets a publisher, whether or not it started with a
        // drawn arrangement, so a test can see what this run *would* cross
        // by after an adoption.
        let (publisher, live) = watch::channel(LiveLayout {
            publication: 0,
            layout: held.clone().unwrap_or_else(|| arrangement(0, local, 0)),
        });
        let (sender, receiver) = command_lanes();
        let (hub, events, _shutdown) = TopologySync::start(TopologyInputs {
            local: device(local),
            display: Arc::clone(&display) as Arc<dyn DisplayInfo>,
            commands: sender,
            metrics: Arc::clone(&metrics),
            state: Some(Arc::clone(&state)),
            layout: held,
            publisher: Some(publisher),
            config_path: Some(config.clone()),
        });
        drop(events);
        let (high, background) = receiver.into_lanes();
        Engine {
            hub,
            high,
            _background: background,
            metrics,
            config,
            state,
            live,
            display,
            session: Uuid::from_bytes([0x5A; 16]),
        }
    }

    fn payloads(commands: &[SessionCommand], message_type: MessageType) -> Vec<Vec<u8>> {
        commands
            .iter()
            .filter_map(|command| match command {
                SessionCommand::SendFrame {
                    message_type: kind,
                    payload,
                    ..
                } if *kind == message_type.wire() => Some(payload.clone()),
                _ => None,
            })
            .collect()
    }

    fn terminations(commands: &[SessionCommand]) -> usize {
        commands
            .iter()
            .filter(|command| matches!(command, SessionCommand::TerminateSession { .. }))
            .count()
    }

    /// Hand everything `from` queued to `to`, as the wire would.
    async fn relay(from: &mut Engine, to: &mut Engine) {
        for command in from.drain() {
            if let SessionCommand::SendFrame {
                message_type,
                payload,
                ..
            } = command
            {
                let Some(kind) = MessageType::from_wire(message_type) else {
                    continue;
                };
                to.receive(kind, payload).await;
            }
        }
    }

    // ---- establishment --------------------------------------------------

    /// A session coming up states this machine's monitors, and its drawn
    /// arrangement if it has one — both on the High lane, since a topology
    /// frame is interactive-sized negotiation traffic (ADR 0013).
    #[tokio::test]
    async fn establishing_states_the_monitors_and_the_arrangement() {
        let sandbox = Sandbox::new("establish");
        let mut engine = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));

        engine.connect(B).await;
        let commands = engine.drain();

        let topologies = payloads(&commands, MessageType::MonitorTopology);
        assert_eq!(topologies.len(), 1, "{commands:?}");
        let decoded = MonitorTopology::decode_payload(&topologies[0]).unwrap();
        assert_eq!(decoded.monitors.len(), 1);
        assert_eq!(decoded.monitors[0].id.as_str(), "a-SCREEN");

        let syncs = payloads(&commands, MessageType::LayoutSync);
        assert_eq!(syncs.len(), 1, "{commands:?}");
        let decoded = LayoutSync::decode_payload(&syncs[0]).unwrap();
        assert_eq!(decoded.revision, 3);
        assert_eq!(decoded.origin, device(A));
        assert_eq!(engine.metrics.snapshot().layout_sent, 1);

        // Every topology command rode the interactive lane: nothing at all
        // reached the bulk one.
        assert_eq!(commands.len(), 2);
    }

    /// A run with no drawn arrangement — the deprecated side model, or
    /// seamless off — states its monitors and nothing else. An implicit
    /// arrangement is never synced (ADR 0018).
    #[tokio::test]
    async fn a_run_with_no_drawn_arrangement_syncs_no_layout() {
        let sandbox = Sandbox::new("implicit");
        let mut engine = engine(&sandbox, "a", A, None);

        engine.connect(B).await;
        let commands = engine.drain();
        assert_eq!(payloads(&commands, MessageType::MonitorTopology).len(), 1);
        assert!(payloads(&commands, MessageType::LayoutSync).is_empty());
        assert_eq!(engine.metrics.snapshot().layout_sent, 0);
    }

    /// A layout left over from a previous pairing names a machine that is
    /// not at the other end. Sending it would earn this machine a protocol
    /// violation for an entirely local fault, so it is not sent.
    #[tokio::test]
    async fn an_arrangement_that_does_not_describe_this_pair_is_not_sent() {
        let sandbox = Sandbox::new("stale-pair");
        let mut engine = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));

        // The peer is a third machine; the held layout names A and B.
        engine.connect(STRANGER).await;
        let commands = engine.drain();
        assert_eq!(payloads(&commands, MessageType::MonitorTopology).len(), 1);
        assert!(
            payloads(&commands, MessageType::LayoutSync).is_empty(),
            "a layout describing another pairing went on the wire"
        );
    }

    // ---- convergence ----------------------------------------------------

    /// The headline case: A holds revision 3, B holds revision 5. They
    /// connect, and A adopts — persisting first, publishing to its live
    /// crossing source, and reporting into the state file (ADR 0018's
    /// persist-publish-report).
    #[tokio::test]
    async fn the_newer_arrangement_wins_and_the_loser_adopts_and_persists() {
        let sandbox = Sandbox::new("converge");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        let mut b = engine(&sandbox, "b", B, Some(arrangement(5, B, 40)));

        a.connect(B).await;
        b.connect(A).await;
        // Each states its own. A's own statement is not the subject here —
        // set it aside, so what is left at the end is only what A said
        // *after* learning B is newer.
        let _ = a.drain();
        relay(&mut b, &mut a).await;

        assert_eq!(a.held(), Some(5), "A did not adopt the newer arrangement");
        assert_eq!(a.metrics.snapshot().layout_adopted_from_peer, 1);

        // Persist: the config file on disk now records revision 5, at
        // schema 2.
        let written = std::fs::read_to_string(&a.config).unwrap();
        assert!(written.contains("schema_version = 2"), "{written}");
        assert!(written.contains("revision = 5"), "{written}");

        // Publish: the live crossing source crosses by it now, under a
        // fresh publication so the detector re-derives.
        let live = a.live.borrow_and_update().clone();
        assert_eq!(live.layout.revision(), 5);
        assert_eq!(live.publication, 1);

        // Report: the state file's layout is the adopted one.
        assert_eq!(
            a.state.snapshot().layout.map(|layout| layout.revision),
            Some(5)
        );

        // And A does not answer: it lost, so there is nothing to state.
        assert!(payloads(&a.drain(), MessageType::LayoutSync).is_empty());
    }

    /// The winner answers so the peer adopts — which is what makes a
    /// machine that connects holding an *older* arrangement converge
    /// without anyone editing anything.
    #[tokio::test]
    async fn the_winner_answers_an_older_arrangement_with_its_own() {
        let sandbox = Sandbox::new("answer");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(9, A, 0)));

        a.connect(B).await;
        let _ = a.drain();
        a.receive_layout(&arrangement(2, B, 40)).await;

        assert_eq!(a.held(), Some(9), "the older arrangement was adopted");
        let answered = payloads(&a.drain(), MessageType::LayoutSync);
        assert_eq!(answered.len(), 1, "the winner said nothing back");
        assert_eq!(
            LayoutSync::decode_payload(&answered[0]).unwrap().revision,
            9
        );
    }

    /// The supersession diagnostic ADR 0018 requires of the loser (NFR-3):
    /// both revisions and both origins, so a user whose drawing vanished
    /// can learn why rather than concluding the editor is broken.
    #[tokio::test]
    async fn the_loser_logs_a_supersession_naming_both_revisions_and_origins() {
        let sandbox = Sandbox::new("supersession");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));

        let (log, _guard) = capture();
        a.connect(B).await;
        let _ = a.drain();
        a.receive_layout(&arrangement(5, B, 40)).await;
        let text = log.text();

        assert!(text.contains("superseded"), "{text}");
        assert!(text.contains("adopted_revision=5"), "{text}");
        assert!(text.contains("superseded_revision=3"), "{text}");
        assert!(
            text.contains(&format!("adopted_origin={}", device(B))),
            "{text}"
        );
        assert!(
            text.contains(&format!("superseded_origin={}", device(A))),
            "{text}"
        );
    }

    /// Two machines already agreeing say **nothing**. This is the property
    /// that stops an echo loop: if agreement produced an answer, the answer
    /// would produce an answer.
    #[tokio::test]
    async fn identical_arrangements_produce_no_traffic_at_all() {
        let sandbox = Sandbox::new("idle");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(4, A, 0)));
        let mut b = engine(&sandbox, "b", B, Some(arrangement(4, A, 0)));

        a.connect(B).await;
        b.connect(A).await;

        // Ten rounds of handing each side whatever the other queued. A pair
        // that answers an identical layout never runs out of things to say;
        // this pair must fall silent after the establishment statements.
        for round in 0..10 {
            relay(&mut a, &mut b).await;
            relay(&mut b, &mut a).await;
            if round == 0 {
                continue;
            }
            assert!(
                a.drain().is_empty() && b.drain().is_empty(),
                "round {round}: the pair is still talking about an arrangement they agree on"
            );
        }
        assert_eq!(a.held(), Some(4));
        assert_eq!(b.held(), Some(4));
        assert_eq!(a.metrics.snapshot().layout_adopted_from_peer, 0);
        assert_eq!(b.metrics.snapshot().layout_adopted_from_peer, 0);
        // Nothing was adopted, so nothing was written.
        assert!(!a.config.exists(), "an idle sync rewrote the config");
        assert!(!b.config.exists(), "an idle sync rewrote the config");
    }

    /// An adoption that comes back through the config poll — this worker's
    /// own write, re-read seconds later — resolves as identical and is
    /// silent. Without that, every adoption would state itself back to the
    /// peer, which would state it back, forever.
    #[tokio::test]
    async fn a_re_read_of_this_run_s_own_adoption_write_says_nothing() {
        let sandbox = Sandbox::new("no-echo");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));

        a.connect(B).await;
        let _ = a.drain();
        a.receive_layout(&arrangement(5, B, 40)).await;
        assert_eq!(a.held(), Some(5));
        let _ = a.drain();

        // The config poll re-reads what the adoption just wrote.
        let reloaded = crate::config::load_run_config_at(Some(&a.config))
            .unwrap()
            .layout
            .unwrap()
            .expect("the adoption wrote a [layout]");
        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(reloaded)))
            .await;

        assert!(
            a.drain().is_empty(),
            "the worker stated its own adoption back to the peer"
        );
    }

    // ---- rejection ------------------------------------------------------

    /// A well-formed layout naming a third machine is **rejected**, not
    /// adopted — and the session survives, because a peer disagreeing with
    /// reality must not cost a healthy session its first frame
    /// (docs/PROTOCOL.md §6.2, §7).
    #[tokio::test]
    async fn a_layout_naming_a_third_device_is_rejected_and_the_session_survives() {
        let sandbox = Sandbox::new("hostile-device");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));

        a.connect(B).await;
        let _ = a.drain();

        // Well-formed on the wire — two devices, valid rectangles, a
        // revision far ahead — but one of them is not this session's pair.
        let hostile = LayoutSync {
            revision: 99,
            origin: device(STRANGER),
            monitors: vec![
                placed(device(A), "A-SCREEN", 0),
                placed(device(STRANGER), "X", 1920),
            ],
        };
        let payload = hostile.encode_payload().expect("well-formed on the wire");
        a.receive(MessageType::LayoutSync, payload).await;

        assert_eq!(a.held(), Some(3), "a hostile arrangement was adopted");
        assert_eq!(a.metrics.snapshot().layout_rejected, 1);
        assert_eq!(
            terminations(&a.drain()),
            0,
            "a semantically impossible layout killed the session"
        );
        assert!(!a.config.exists(), "a rejected layout was persisted");
        assert_eq!(a.live.borrow_and_update().publication, 0);
    }

    /// The other semantic refusals, each on its own rule rather than by a
    /// catch-all: overlapping rectangles, and an arrangement that draws
    /// only one of the two machines.
    #[tokio::test]
    async fn semantically_impossible_arrangements_are_all_refused_without_adoption() {
        let sandbox = Sandbox::new("impossible");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        a.connect(B).await;
        let _ = a.drain();

        let cases = [
            // Overlapping rectangles: a cursor in the shared area has no
            // single answer for which monitor it left.
            LayoutSync {
                revision: 50,
                origin: device(B),
                monitors: vec![
                    placed(device(A), "A-SCREEN", 0),
                    placed(device(B), "B-SCREEN", 100),
                ],
            },
            // Only one machine drawn: nothing to cross to.
            LayoutSync {
                revision: 51,
                origin: device(B),
                monitors: vec![placed(device(A), "A-SCREEN", 0)],
            },
        ];
        for (index, case) in cases.into_iter().enumerate() {
            let payload = case.encode_payload().expect("well-formed on the wire");
            a.receive(MessageType::LayoutSync, payload).await;
            assert_eq!(a.held(), Some(3), "case {index} was adopted");
            assert_eq!(
                terminations(&a.drain()),
                0,
                "case {index} killed the session"
            );
        }
        assert_eq!(a.metrics.snapshot().layout_rejected, 2);
    }

    /// §7's rule is graduated, and this is the graduation: a peer that
    /// keeps sending unadoptable arrangements loses the session. Without
    /// it, unbelievable layouts would be free for the sender and unbounded
    /// log volume here.
    #[tokio::test]
    async fn repeated_unbelievable_arrangements_do_cost_the_session() {
        let sandbox = Sandbox::new("graduated");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        a.connect(B).await;
        let _ = a.drain();

        // Each one names a third machine — well-formed, never adoptable.
        // The budget is spent *at* the cap, so everything strictly under
        // it survives.
        for attempt in 1..super::MAX_LAYOUT_VIOLATIONS {
            a.receive_hostile(u64::from(attempt)).await;
            assert_eq!(
                terminations(&a.drain()),
                0,
                "violation {attempt} of {} ended the session early",
                super::MAX_LAYOUT_VIOLATIONS
            );
        }

        // The one that reaches the cap: the session goes, fail closed.
        a.receive_hostile(99).await;
        assert_eq!(terminations(&a.drain()), 1);
        assert_eq!(a.held(), Some(3), "nothing was ever adopted");
        assert_eq!(
            a.metrics.snapshot().layout_rejected,
            u64::from(super::MAX_LAYOUT_VIOLATIONS)
        );

        // And the frames already in flight behind the kill are dropped
        // rather than each firing it again.
        a.receive_hostile(100).await;
        assert_eq!(
            terminations(&a.drain()),
            0,
            "a session already being terminated was terminated again"
        );
        assert_eq!(
            a.metrics.snapshot().layout_rejected,
            u64::from(super::MAX_LAYOUT_VIOLATIONS),
            "a dropped frame was still charged as a violation"
        );
    }

    /// The budget is per session, not a process-lifetime grudge: a fresh
    /// connection starts clean.
    #[tokio::test]
    async fn the_violation_budget_resets_with_the_session() {
        let sandbox = Sandbox::new("budget-reset");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));

        for _ in 0..2 {
            a.connect(B).await;
            let _ = a.drain();
            for _ in 1..super::MAX_LAYOUT_VIOLATIONS {
                a.receive_hostile(7).await;
            }
            assert_eq!(
                terminations(&a.drain()),
                0,
                "a session was terminated on a budget carried over from the last one"
            );
            a.disconnect().await;
        }
    }

    /// Malformed is a different answer entirely: the session terminates,
    /// fail closed (docs/PROTOCOL.md §7) — through the same
    /// `TerminateSession` command every other driver's payload violations
    /// take.
    ///
    /// A fresh engine per message type, because the first kill marks the
    /// session and everything after it is deliberately dropped: proving
    /// both types are fatal needs two sessions, not two frames.
    #[tokio::test]
    async fn a_malformed_topology_frame_terminates_the_session() {
        for message_type in [MessageType::MonitorTopology, MessageType::LayoutSync] {
            let sandbox = Sandbox::new("malformed");
            let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
            a.connect(B).await;
            let _ = a.drain();

            // `0xFF` repeated is not a valid leading varint for the
            // element-count prefix both messages' `Vec` fields start with.
            a.receive(message_type, vec![0xFF; 8]).await;
            let commands = a.drain();
            assert_eq!(
                terminations(&commands),
                1,
                "{message_type:?}: a malformed frame did not terminate the session"
            );
            assert!(matches!(
                commands.first(),
                Some(SessionCommand::TerminateSession {
                    target: FrameTarget::Session(_),
                    ..
                })
            ));

            // A decoder failure is not a semantic rejection, and the two
            // are counted apart.
            assert_eq!(a.metrics.snapshot().layout_rejected, 0);
            assert_eq!(a.held(), Some(3));

            // The kill is in flight; further frames on this session are
            // dropped rather than each firing it again.
            a.receive(message_type, vec![0xFF; 8]).await;
            assert_eq!(
                terminations(&a.drain()),
                0,
                "{message_type:?}: a terminated session was terminated again"
            );
        }
    }

    // ---- the peer half of the state file --------------------------------

    /// `MonitorTopology` fills the state file's peer half, which is what
    /// the editor draws the other desk from — and a disconnect keeps it,
    /// with `connected: false`.
    #[tokio::test]
    async fn the_peer_half_of_the_state_file_is_filled_and_survives_a_disconnect() {
        let sandbox = Sandbox::new("peer-half");
        let mut a = engine(&sandbox, "a", A, None);

        a.connect(B).await;
        let peer = a.state.snapshot().peer.expect("a peer was recorded");
        assert_eq!(peer.device, device(B));
        assert!(peer.connected);
        assert!(peer.monitors.is_empty(), "monitors before any report");

        let report = MonitorTopology {
            monitors: vec![MonitorReport {
                id: MonitorId::new("B-SCREEN").unwrap(),
                rect: LayoutRect {
                    x: 0,
                    y: 0,
                    width: 2560,
                    height: 1440,
                },
                scale_percent: 150,
                label: Some(crossover_topology::MonitorLabel::new("DELL U2720Q").unwrap()),
                physical_size: Some(crossover_topology::PhysicalSizeMm::new(597, 336).unwrap()),
            }],
        };
        a.receive(
            MessageType::MonitorTopology,
            report.encode_payload().unwrap(),
        )
        .await;

        let peer = a.state.snapshot().peer.expect("a peer");
        assert_eq!(peer.monitors.len(), 1);
        assert_eq!(peer.monitors[0].id.as_str(), "B-SCREEN");
        assert_eq!(peer.monitors[0].scale_percent, 150);
        // The peer's caption crosses the wire and lands in the state file,
        // so the local editor draws the other desk with the names its
        // owner reads off their own bezels rather than device strings.
        assert_eq!(
            peer.monitors[0]
                .label
                .as_ref()
                .map(crossover_topology::MonitorLabel::as_str),
            Some("DELL U2720Q")
        );
        // And so does the panel's real size, which is what will let the
        // local editor draw the other desk in proportion rather than by
        // pixel count.
        assert_eq!(
            peer.monitors[0].physical_size,
            Some(crossover_topology::PhysicalSizeMm::new(597, 336).unwrap())
        );
        assert_eq!(peer.monitors[0].rect.width, 2560);

        // The link drops: last-known geometry stays, so the editor is still
        // usable while the peer is down (ADR 0018).
        a.disconnect().await;
        let peer = a.state.snapshot().peer.expect("a peer");
        assert!(!peer.connected);
        assert_eq!(peer.monitors.len(), 1, "a disconnect emptied the editor");
    }

    /// A `MonitorTopology` never changes crossing behaviour on its own: it
    /// is a fact about the sender, not an arrangement.
    #[tokio::test]
    async fn a_peer_monitor_report_never_changes_the_arrangement() {
        let sandbox = Sandbox::new("monitors-inert");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        a.connect(B).await;
        let _ = a.drain();

        a.receive(
            MessageType::MonitorTopology,
            MonitorTopology {
                monitors: vec![MonitorReport {
                    id: MonitorId::new("B-SCREEN").unwrap(),
                    rect: LayoutRect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                    scale_percent: 100,
                    label: None,
                    physical_size: None,
                }],
            }
            .encode_payload()
            .unwrap(),
        )
        .await;

        assert_eq!(a.held(), Some(3));
        assert_eq!(a.live.borrow_and_update().publication, 0);
        assert!(a.drain().is_empty());
    }

    // ---- the other two producers ----------------------------------------

    /// A local display change re-states `MonitorTopology` — ADR 0018's
    /// "sent after `Hello` and again whenever the local display
    /// configuration changes".
    #[tokio::test]
    async fn a_local_display_change_re_states_the_monitors() {
        let sandbox = Sandbox::new("display-change");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        a.display.set_monitor_layout(vec![
            MonitorInfo {
                id: Some("a-SCREEN".to_owned()),
                rect: MonitorRect {
                    left: 0,
                    top: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            MonitorInfo {
                id: Some("a-SECOND".to_owned()),
                rect: MonitorRect {
                    left: 1920,
                    top: 0,
                    width: 1280,
                    height: 1024,
                },
            },
        ]);
        a.feed(TopologyEvent::LocalDisplayChanged).await;

        let commands = a.drain();
        let topologies = payloads(&commands, MessageType::MonitorTopology);
        assert_eq!(topologies.len(), 1, "{commands:?}");
        let decoded = MonitorTopology::decode_payload(&topologies[0]).unwrap();
        assert_eq!(decoded.monitors.len(), 2);

        // With no session, there is nobody to tell.
        a.disconnect().await;
        a.feed(TopologyEvent::LocalDisplayChanged).await;
        assert!(a.drain().is_empty());
    }

    /// This machine's own product names and panel sizes reach the peer, and
    /// a screen the platform would neither name nor measure still travels —
    /// undescribed, never dropped. The peer's editor draws this desk from
    /// exactly this message, so either field stopping at the state file
    /// would describe one desk and not the other.
    #[tokio::test]
    async fn the_monitors_this_machine_states_carry_their_product_names() {
        use crossover_platform::MonitorDescription;

        let sandbox = Sandbox::new("stated-labels");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        a.display.set_monitor_descriptions(vec![
            MonitorDescription {
                info: MonitorInfo {
                    id: Some("a-SCREEN".to_owned()),
                    rect: MonitorRect {
                        left: 0,
                        top: 0,
                        width: 1920,
                        height: 1080,
                    },
                },
                label: Some("DELL U2720Q".to_owned()),
                physical_size: Some(crossover_platform::PhysicalSizeMm {
                    width_mm: 597,
                    height_mm: 336,
                }),
            },
            MonitorDescription {
                info: MonitorInfo {
                    id: Some("a-SECOND".to_owned()),
                    rect: MonitorRect {
                        left: 1920,
                        top: 0,
                        width: 1280,
                        height: 1024,
                    },
                },
                label: None,
                physical_size: None,
            },
        ]);
        a.feed(TopologyEvent::LocalDisplayChanged).await;

        let topologies = payloads(&a.drain(), MessageType::MonitorTopology);
        assert_eq!(topologies.len(), 1);
        let decoded = MonitorTopology::decode_payload(&topologies[0]).unwrap();
        assert_eq!(decoded.monitors.len(), 2, "an unlabelled monitor was lost");
        assert_eq!(
            decoded.monitors[0]
                .label
                .as_ref()
                .map(crossover_topology::MonitorLabel::as_str),
            Some("DELL U2720Q")
        );
        assert_eq!(decoded.monitors[1].label, None);
        assert_eq!(
            decoded.monitors[0].physical_size,
            Some(crossover_topology::PhysicalSizeMm::new(597, 336).unwrap())
        );
        assert_eq!(decoded.monitors[1].physical_size, None);
    }

    /// A panel size the layout model would refuse costs the *size* and not
    /// the monitor, on the same trade the label gets. This is the one that
    /// would hurt most if it went the other way: an implausible EDID is
    /// exactly what a projector or a virtual display reports, and losing a
    /// rectangle at the peer's desk over one would break the arrangement
    /// rather than merely draw it at the wrong scale.
    #[tokio::test]
    async fn an_unusable_panel_size_costs_the_size_and_not_the_screen() {
        use crossover_platform::{MonitorDescription, PhysicalSizeMm as PlatformSize};

        let sandbox = Sandbox::new("unusable-size");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        a.display.set_monitor_descriptions(vec![MonitorDescription {
            info: MonitorInfo {
                id: Some("a-SCREEN".to_owned()),
                rect: MonitorRect {
                    left: 0,
                    top: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            label: Some("DELL U2720Q".to_owned()),
            // Zero on one axis and past the wire cap on the other: both
            // rejection classes at once.
            physical_size: Some(PlatformSize {
                width_mm: 0,
                height_mm: u16::MAX,
            }),
        }]);
        a.feed(TopologyEvent::LocalDisplayChanged).await;

        let topologies = payloads(&a.drain(), MessageType::MonitorTopology);
        assert_eq!(topologies.len(), 1);
        let decoded = MonitorTopology::decode_payload(&topologies[0]).unwrap();
        assert_eq!(
            decoded.monitors.len(),
            1,
            "a screen was dropped over a size"
        );
        assert_eq!(decoded.monitors[0].id.as_str(), "a-SCREEN");
        assert_eq!(decoded.monitors[0].physical_size, None);
        // The caption beside it still travelled.
        assert_eq!(
            decoded.monitors[0]
                .label
                .as_ref()
                .map(crossover_topology::MonitorLabel::as_str),
            Some("DELL U2720Q")
        );
    }

    /// A product name the layout model would refuse costs the *label* and
    /// not the monitor. A caption is display-only, so refusing a screen
    /// over one would trade something that matters for something that does
    /// not — and the peer would lose a rectangle rather than a word.
    #[tokio::test]
    async fn an_unusable_product_name_costs_the_label_and_not_the_screen() {
        use crossover_platform::MonitorDescription;

        let sandbox = Sandbox::new("unusable-label");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        a.display.set_monitor_descriptions(vec![MonitorDescription {
            info: MonitorInfo {
                id: Some("a-SCREEN".to_owned()),
                rect: MonitorRect {
                    left: 0,
                    top: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            // Over the byte bound, and carrying a control character: both
            // rejection classes at once.
            label: Some(format!("DELL\n{}", "x".repeat(80))),
            physical_size: None,
        }]);
        a.feed(TopologyEvent::LocalDisplayChanged).await;

        let topologies = payloads(&a.drain(), MessageType::MonitorTopology);
        assert_eq!(topologies.len(), 1);
        let decoded = MonitorTopology::decode_payload(&topologies[0]).unwrap();
        assert_eq!(
            decoded.monitors.len(),
            1,
            "a screen was dropped over a name"
        );
        assert_eq!(decoded.monitors[0].id.as_str(), "a-SCREEN");
        assert_eq!(decoded.monitors[0].label, None);
    }

    /// An edit made in the editor reaches the peer: the config poll offers
    /// the changed `[layout]`, and a genuinely newer one becomes this run's
    /// arrangement and is stated.
    #[tokio::test]
    async fn a_config_edit_becomes_this_run_s_arrangement_and_is_stated() {
        let sandbox = Sandbox::new("config-edit");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        a.connect(B).await;
        let _ = a.drain();

        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(arrangement(
            4, A, 60,
        ))))
        .await;

        assert_eq!(a.held(), Some(4));
        let syncs = payloads(&a.drain(), MessageType::LayoutSync);
        assert_eq!(syncs.len(), 1);
        assert_eq!(LayoutSync::decode_payload(&syncs[0]).unwrap().revision, 4);
        // Published, so this run's crossings follow the edit without a
        // restart…
        assert_eq!(a.live.borrow_and_update().layout.revision(), 4);
        // …and *not* written back: the arrangement came from the file.
        assert!(
            !a.config.exists(),
            "an edit read from the config was written back to it"
        );
    }

    /// A config file that is *behind* this run — the ordinary shape while a
    /// rate-bounded adoption write is still pending — changes nothing.
    #[tokio::test]
    async fn a_config_behind_this_run_is_ignored_rather_than_re_broadcast() {
        let sandbox = Sandbox::new("config-behind");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(7, A, 0)));
        a.connect(B).await;
        let _ = a.drain();

        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(arrangement(
            2, A, 60,
        ))))
        .await;
        assert_eq!(a.held(), Some(7));
        assert!(a.drain().is_empty());
    }

    /// A config file naming machines this run is not connected to is not
    /// used — the mirror of the re-pair demotion, on the way in from the
    /// file rather than from the wire. Taking it would mean briefly
    /// crossing by an arrangement of other desks, with no way to send it
    /// and so nothing to correct it.
    #[tokio::test]
    async fn a_config_naming_machines_this_run_is_not_connected_to_is_not_used() {
        let sandbox = Sandbox::new("config-other-pair");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        a.connect(B).await;
        let _ = a.drain();

        let elsewhere = Layout::new(
            50,
            device(A),
            vec![
                placed(device(A), "A-SCREEN", 0),
                placed(device(STRANGER), "GONE", 1920),
            ],
            &DevicePair::new(device(A), device(STRANGER)).unwrap(),
        )
        .unwrap();
        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(elsewhere)))
            .await;

        assert_eq!(a.held(), Some(3), "an unusable arrangement was taken");
        assert_eq!(a.live.borrow_and_update().publication, 0);
        assert!(a.drain().is_empty());

        // With no session up there is nothing to judge against, so the
        // same file is taken — that is the ordinary
        // edit-while-disconnected case.
        a.disconnect().await;
        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(arrangement(
            4, A, 60,
        ))))
        .await;
        assert_eq!(a.held(), Some(4));
    }

    // ---- persistence ----------------------------------------------------

    /// Adoption survives a restart: the arrangement A adopted from B is
    /// what A's own config hands the next run, straight through the real
    /// config reader.
    #[tokio::test]
    async fn an_adopted_arrangement_survives_a_restart() {
        let sandbox = Sandbox::new("restart");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        let winner = arrangement(5, B, 40);

        a.connect(B).await;
        let _ = a.drain();
        a.receive_layout(&winner).await;
        assert_eq!(a.held(), Some(5));

        // Restart: rebuild the engine from what the config now says, the
        // same way `crossover run` does at startup.
        let reloaded = crate::config::load_run_config_at(Some(&a.config))
            .unwrap()
            .layout
            .unwrap()
            .expect("the adoption wrote a [layout]");
        assert_eq!(reloaded, winner);
        let restarted = engine(&sandbox, "a", A, Some(reloaded));
        assert_eq!(restarted.held(), Some(5));
    }

    /// The schema 1 → 2 upgrade path, finally executed: a v1 config with a
    /// lingering `[seamless] side` is upgraded by the first write — and ADR
    /// 0018 is explicit that **adoption counts as that first write**,
    /// because on a two-machine pair the peer's edit *is* the user's edit,
    /// drawn at the other desk.
    #[tokio::test]
    async fn adopting_upgrades_a_schema_one_config_in_place() {
        let sandbox = Sandbox::new("upgrade");
        let mut a = engine(&sandbox, "a", A, None);
        std::fs::write(
            &a.config,
            concat!(
                "# hand written, and it shows\n",
                "schema_version = 1\n",
                "\n",
                "[network]\n",
                "connect = \"192.168.1.146:27677\"\n",
                "\n",
                "[seamless]\n",
                "side = \"right\"\n",
            ),
        )
        .unwrap();

        a.connect(B).await;
        let _ = a.drain();
        let drawn = arrangement(1, B, 0);
        a.receive_layout(&drawn).await;

        let written = std::fs::read_to_string(&a.config).unwrap();
        assert!(written.contains("schema_version = 2"), "{written}");
        assert!(!written.contains("side ="), "{written}");
        assert!(!written.contains("[seamless]"), "{written}");
        // Everything the user wrote is still there.
        assert!(
            written.contains("# hand written, and it shows"),
            "{written}"
        );
        assert!(written.contains("192.168.1.146:27677"), "{written}");
        // And the upgraded file loads, as schema 2, with the drawn layout.
        let loaded = crate::config::load_run_config_at(Some(&a.config)).unwrap();
        assert_eq!(loaded.layout.unwrap(), Some(drawn));
    }

    /// The rate bound (docs/SECURITY.md T23): a peer feeding distinct
    /// revisions as fast as it can send cannot make this machine rewrite
    /// its config at wire speed. The first adoption writes immediately, the
    /// rest coalesce, and the one that lands is the **latest**.
    #[tokio::test(start_paused = true)]
    async fn adoption_driven_persistence_is_rate_bounded_and_latest_wins() {
        let sandbox = Sandbox::new("rate-bound");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        for revision in 1..=20u64 {
            let drawn = arrangement(revision, B, i32::try_from(revision).unwrap() * 10);
            a.receive_layout(&drawn).await;
            // Publication is immediate every time: the bound is on the
            // disk, never on where this machine crosses.
            assert_eq!(a.live.borrow_and_update().layout.revision(), revision);
        }

        // Twenty adoptions, one write — the first.
        let written = std::fs::read_to_string(&a.config).unwrap();
        assert!(written.contains("revision = 1"), "{written}");
        assert_eq!(a.metrics.snapshot().layout_adopted_from_peer, 20);

        // When the interval lapses the pending write lands, and it is the
        // latest revision rather than the next one in the queue.
        assert!(a.hub.persist.deadline().is_some());
        tokio::time::advance(super::LAYOUT_PERSIST_INTERVAL + std::time::Duration::from_millis(1))
            .await;
        a.hub.persist.flush();
        let written = std::fs::read_to_string(&a.config).unwrap();
        assert!(written.contains("revision = 20"), "{written}");
        assert!(
            a.hub.persist.deadline().is_none(),
            "still pending after a flush"
        );
    }

    /// A revision TOML cannot represent is refused rather than wrapped
    /// (`crossover_topology::PersistError::RevisionUnrepresentable`), and
    /// the refusal costs the run nothing: publication already happened, so
    /// only persistence degrades — observably.
    #[tokio::test]
    async fn a_write_that_cannot_happen_degrades_persistence_and_nothing_else() {
        let sandbox = Sandbox::new("unwritable");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        let (log, _guard) = capture();
        a.receive_layout(&arrangement(u64::MAX, B, 0)).await;

        assert_eq!(a.held(), Some(u64::MAX), "publication did not happen");
        assert_eq!(a.live.borrow_and_update().layout.revision(), u64::MAX);
        assert!(
            !a.config.exists(),
            "an unrepresentable revision was written"
        );
        let text = log.text();
        assert!(
            text.contains("could not save the adopted arrangement"),
            "{text}"
        );
    }

    // ---- the re-pair residue --------------------------------------------

    /// The deadlock a re-pair used to cause, and the repair.
    ///
    /// A holds a high-revision arrangement left over from a **previous
    /// pairing** — it names A and a machine that is no longer here. It is
    /// never sent (the peer would be right to refuse it), so without this
    /// it would sit there winning every resolution on nothing but its
    /// revision: A adopts nothing, sends nothing, and the pair disagrees
    /// forever with no way for either desk to fix it. Demoting a
    /// non-contender to "no arrangement" is what makes the peer's the only
    /// candidate, so the pair converges on something true.
    #[tokio::test]
    async fn a_layout_from_a_previous_pairing_does_not_block_the_new_one() {
        let sandbox = Sandbox::new("re-pair");
        // A's config still describes A and a stranger, at revision 9.
        let stale = Layout::new(
            9,
            device(A),
            vec![
                placed(device(A), "A-SCREEN", 0),
                placed(device(STRANGER), "GONE", 1920),
            ],
            &DevicePair::new(device(A), device(STRANGER)).unwrap(),
        )
        .unwrap();
        let mut a = engine(&sandbox, "a", A, Some(stale));

        let (log, _guard) = capture();
        a.connect(B).await;
        // Nothing is stated: what cannot be believed is not sent.
        assert!(payloads(&a.drain(), MessageType::LayoutSync).is_empty());

        // B's real arrangement arrives — older by revision, and it still
        // wins, because the stale one is not a candidate here at all.
        a.receive_layout(&arrangement(2, B, 0)).await;
        assert_eq!(a.held(), Some(2), "the re-pair residue blocked convergence");
        assert_eq!(a.metrics.snapshot().layout_adopted_from_peer, 1);
        assert!(
            std::fs::read_to_string(&a.config)
                .unwrap()
                .contains("revision = 2")
        );

        // And the demotion is said, once, naming what was demoted.
        let text = log.text();
        assert!(text.contains("describes a different pairing"), "{text}");
        assert!(text.contains("revision=9"), "{text}");

        // Said once per session, not once per frame.
        a.receive_layout(&arrangement(3, B, 40)).await;
        let after = log.text();
        assert_eq!(
            after.matches("describes a different pairing").count(),
            1,
            "the demotion was restated on a later frame: {after}"
        );
    }

    // ---- the state file's layout is the hub's ----------------------------

    /// Blocker: during a coalescing window the config file is *behind* what
    /// this run is crossing by, and the state file must report what the run
    /// holds — never what the file happens to say.
    ///
    /// It matters beyond tidiness: the editor numbers its next save one
    /// past everything **both** files have seen (`crossover-layout`'s
    /// `save::next_revision`), so a state file rolled back to the config's
    /// older revision would let a save claim a revision this machine had
    /// already adopted from the peer — two different arrangements at one
    /// revision, which is the anomaly the hash tiebreak exists to survive
    /// rather than a state to walk into.
    #[tokio::test(start_paused = true)]
    async fn a_config_poll_inside_a_coalescing_window_cannot_roll_the_state_file_back() {
        let sandbox = Sandbox::new("no-rollback");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        // First adoption: written immediately.
        a.receive_layout(&arrangement(5, B, 0)).await;
        assert!(
            std::fs::read_to_string(&a.config)
                .unwrap()
                .contains("revision = 5")
        );

        // Second, inside the interval: published and reported, but its
        // write is still pending, so the file still says 5.
        a.receive_layout(&arrangement(6, B, 40)).await;
        assert_eq!(a.held(), Some(6));
        assert_eq!(a.live.borrow_and_update().layout.revision(), 6);
        let on_disk = std::fs::read_to_string(&a.config).unwrap();
        assert!(on_disk.contains("revision = 5"), "{on_disk}");

        // The config poll now reads the file — revision 5 — and offers it.
        let behind = crate::config::load_run_config_at(Some(&a.config))
            .unwrap()
            .layout
            .unwrap()
            .expect("a [layout]");
        assert_eq!(behind.revision(), 5);
        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(behind)))
            .await;

        // The state file still reports what the run is crossing by.
        assert_eq!(
            a.state.snapshot().layout.map(|layout| layout.revision),
            Some(6),
            "a lagging config poll rolled the state file back"
        );
        assert_eq!(a.held(), Some(6));
        assert!(a.drain().is_empty(), "the lagging read was re-broadcast");
    }

    /// The other half of the same rule: an arrangement the run *adopts*
    /// reaches the state file, so the editor and the worker never disagree
    /// about what is in force.
    #[tokio::test]
    async fn the_hub_is_what_reports_the_layout() {
        let sandbox = Sandbox::new("hub-reports");
        let mut a = engine(&sandbox, "a", A, None);
        assert!(a.state.snapshot().layout.is_none());

        a.connect(B).await;
        let _ = a.drain();
        a.receive_layout(&arrangement(4, B, 0)).await;

        assert_eq!(
            a.state.snapshot().layout.map(|layout| layout.revision),
            Some(4)
        );
    }

    // ---- bounded narration and a bounded answer --------------------------

    /// A peer feeding revisions cannot own this machine's console. The
    /// first adoption of a burst speaks; the rest are recorded at `debug`
    /// and say nothing to the user — and every one of them is still
    /// counted and still adopted.
    #[tokio::test(start_paused = true)]
    async fn adoption_narration_is_rate_limited_without_losing_the_record() {
        let sandbox = Sandbox::new("narration");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        let (log, _guard) = capture();
        for revision in 1..=6u64 {
            a.receive_layout(&arrangement(
                revision,
                B,
                i32::try_from(revision).unwrap() * 10,
            ))
            .await;
        }
        let text = log.text();
        assert_eq!(
            text.matches("adopted the peer's arrangement; this machine held none")
                .count()
                + text
                    .matches("adopted the peer's arrangement; the one this machine held")
                    .count(),
            1,
            "the narration spoke more than once inside one interval: {text}"
        );
        assert_eq!(
            text.matches("narration rate-limited").count(),
            5,
            "the quiet adoptions left no record: {text}"
        );
        assert_eq!(a.metrics.snapshot().layout_adopted_from_peer, 6);

        // Past the interval it speaks again — the limit is a rate, not a
        // one-shot.
        tokio::time::advance(super::LAYOUT_PERSIST_INTERVAL + std::time::Duration::from_millis(1))
            .await;
        a.receive_layout(&arrangement(7, B, 200)).await;
        assert_eq!(
            log.text()
                .matches("adopted the peer's arrangement; the one this machine held")
                .count(),
            1
        );
    }

    /// Answering is how a peer holding a superseded arrangement learns to
    /// adopt. A peer that ignores the answer and re-states the same thing
    /// is not converging, and every re-statement earns it a frame — so the
    /// answers are budgeted.
    #[tokio::test]
    async fn a_peer_that_ignores_the_answer_stops_earning_frames() {
        let sandbox = Sandbox::new("answer-budget");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(9, A, 0)));
        a.connect(B).await;
        let _ = a.drain();

        let stale = arrangement(2, B, 40);
        for attempt in 1..=super::MAX_KEEP_LOCAL_ANSWERS {
            a.receive_layout(&stale).await;
            assert_eq!(
                payloads(&a.drain(), MessageType::LayoutSync).len(),
                1,
                "answer {attempt} was not sent"
            );
        }

        // Budget spent: the peer keeps talking, this machine stops.
        let (log, _guard) = capture();
        for _ in 0..4 {
            a.receive_layout(&stale).await;
            assert!(
                payloads(&a.drain(), MessageType::LayoutSync).is_empty(),
                "an exhausted answer budget still sent a frame"
            );
        }
        // Said once, and not a violation: the session lives.
        let text = log.text();
        assert_eq!(text.matches("no longer answering it").count(), 1, "{text}");
        assert_eq!(a.metrics.snapshot().layout_rejected, 0);
        assert_eq!(a.held(), Some(9));
    }

    // ---- an arrangement that names no screen this machine has -------------

    /// A layout may legitimately name a monitor that is unplugged right
    /// now — that is what makes a drawing survive a dock and an undock — so
    /// it is adopted rather than refused. What it produces is an inert
    /// crossing map, and the failure mode worth closing is discovering that
    /// at the seam days later. So it is said at the moment it happens.
    #[tokio::test]
    async fn adopting_an_arrangement_that_matches_no_attached_screen_says_so() {
        let sandbox = Sandbox::new("inert-adoption");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        // The engine's fake display reports `a-SCREEN`; this arrangement
        // draws A's screen as `A-SCREEN`, which is not attached.
        let (log, _guard) = capture();
        a.receive_layout(&arrangement(3, B, 0)).await;

        assert_eq!(a.held(), Some(3), "an unmatched arrangement was refused");
        let text = log.text();
        assert!(
            text.contains("names none of this machine's attached screens"),
            "{text}"
        );
        assert!(text.contains("A-SCREEN"), "{text}");
        assert!(text.contains("a-SCREEN"), "{text}");
    }

    /// …and an arrangement that *does* name an attached screen says
    /// nothing, so the warning means something when it appears.
    #[tokio::test]
    async fn adopting_an_arrangement_that_matches_stays_quiet() {
        let sandbox = Sandbox::new("inert-quiet");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        let matching = Layout::new(
            3,
            device(B),
            vec![
                placed(device(A), "a-SCREEN", 0),
                placed(device(B), "B-SCREEN", 1920),
            ],
            &DevicePair::new(device(A), device(B)).unwrap(),
        )
        .unwrap();
        let (log, _guard) = capture();
        a.receive_layout(&matching).await;
        assert!(
            !log.text().contains("names none of this machine's attached"),
            "{}",
            log.text()
        );
    }

    // ---- an editor save that lost ----------------------------------------

    /// A save that loses is a person's work disappearing, and NFR-3 is
    /// about exactly that: they get the supersession in full, both
    /// revisions and both origins.
    #[tokio::test]
    async fn a_config_save_that_lost_to_a_newer_arrangement_is_reported_in_full() {
        let sandbox = Sandbox::new("save-lost");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();
        a.receive_layout(&arrangement(5, B, 0)).await;

        // Someone saves revision 4 at this desk — drawn before the peer's
        // 5 arrived, and now superseded.
        let (log, _guard) = capture();
        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(arrangement(
            4, A, 60,
        ))))
        .await;

        assert_eq!(a.held(), Some(5));
        let text = log.text();
        assert!(text.contains("superseded by a newer one"), "{text}");
        assert!(text.contains("superseded_revision=4"), "{text}");
        assert!(text.contains("adopted_revision=5"), "{text}");
    }

    /// The far more common shape must stay quiet: the file is simply
    /// *behind*, because a coalesced write of ours has not landed yet.
    /// Crying wolf every few seconds is how a real diagnostic gets ignored.
    #[tokio::test(start_paused = true)]
    async fn a_config_merely_behind_a_pending_write_is_not_reported_as_a_loss() {
        let sandbox = Sandbox::new("behind-quiet");
        let mut a = engine(&sandbox, "a", A, None);
        a.connect(B).await;
        let _ = a.drain();

        a.receive_layout(&arrangement(5, B, 0)).await; // written
        a.receive_layout(&arrangement(6, B, 40)).await; // pending

        let behind = crate::config::load_run_config_at(Some(&a.config))
            .unwrap()
            .layout
            .unwrap()
            .expect("a [layout]");
        let (log, _guard) = capture();
        a.feed(TopologyEvent::LocalLayoutEdited(Box::new(behind)))
            .await;

        let text = log.text();
        assert!(!text.contains("superseded by a newer one"), "{text}");
        assert!(
            text.contains("coalesced write of ours is still pending"),
            "{text}"
        );
    }

    // ---- shutdown --------------------------------------------------------

    /// A clean quit inside the coalescing window must not lose an adopted
    /// arrangement: shutdown asks the hub to land what it owes the disk,
    /// and waits for it.
    #[tokio::test]
    async fn a_clean_shutdown_lands_a_coalesced_write() {
        let sandbox = Sandbox::new("shutdown-flush");
        let config = sandbox.path("shutdown-config.toml");
        let display = Arc::new(FakeDisplay::new(Screen {
            width: 1920,
            height: 1080,
        }));
        let (publisher, _live) = watch::channel(LiveLayout {
            publication: 0,
            layout: arrangement(0, A, 0),
        });
        let (sender, _receiver) = command_lanes();
        let (hub, events, shutdown) = TopologySync::start(TopologyInputs {
            local: device(A),
            display: Arc::clone(&display) as Arc<dyn DisplayInfo>,
            commands: sender,
            metrics: Arc::new(Metrics::new()),
            state: None,
            layout: None,
            publisher: Some(publisher),
            config_path: Some(config.clone()),
        });
        let handle = super::TopologyHandle::new(shutdown, tokio::spawn(hub.run()));

        let session = Uuid::from_bytes([0x5A; 16]);
        events
            .send(TopologyEvent::SessionEstablished {
                session,
                peer_device: Uuid::from_bytes(B),
                peer_name: "peer".to_owned(),
            })
            .await
            .unwrap();
        for revision in [5u64, 6] {
            let layout = arrangement(revision, B, i32::try_from(revision).unwrap() * 10);
            events
                .send(TopologyEvent::Frame {
                    session,
                    frame: RawFrame {
                        message_type: MessageType::LayoutSync.wire(),
                        message_id: 1,
                        payload: LayoutSync {
                            revision: layout.revision(),
                            origin: layout.origin(),
                            monitors: layout.monitors().to_vec(),
                        }
                        .encode_payload()
                        .unwrap(),
                    },
                })
                .await
                .unwrap();
        }
        // Let the hub work through both before asking it to stop.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("revision = 5"),
            "the first adoption did not write immediately"
        );

        handle.shutdown().await;

        let written = std::fs::read_to_string(&config).unwrap();
        assert!(
            written.contains("revision = 6"),
            "a clean shutdown lost the coalesced write: {written}"
        );
    }

    // ---- two sessions to one peer ----------------------------------------

    /// A run can hold an inbound and an outbound session at once, so "a
    /// session ended" is not "the peer is gone" — and a disconnect that
    /// marked the peer offline without checking would report a machine as
    /// down while it is still connected.
    #[tokio::test]
    async fn one_of_two_sessions_ending_does_not_report_the_peer_offline() {
        let sandbox = Sandbox::new("two-sessions");
        let mut a = engine(&sandbox, "a", A, None);
        let second = Uuid::from_bytes([0x5B; 16]);

        a.connect(B).await;
        a.feed(TopologyEvent::SessionEstablished {
            session: second,
            peer_device: Uuid::from_bytes(B),
            peer_name: "peer".to_owned(),
        })
        .await;
        assert!(a.state.snapshot().peer.unwrap().connected);

        a.disconnect().await; // the first session only
        assert!(
            a.state.snapshot().peer.unwrap().connected,
            "one session ending reported a still-connected peer as offline"
        );

        a.feed(TopologyEvent::SessionLost { session: second }).await;
        assert!(!a.state.snapshot().peer.unwrap().connected);
    }

    /// And a session to a machine the document does not name cannot mark
    /// the named one offline — the shape a re-pair leaves behind while the
    /// old session tears down.
    #[tokio::test]
    async fn an_unrelated_session_ending_leaves_the_named_peer_alone() {
        let sandbox = Sandbox::new("unrelated-session");
        let mut a = engine(&sandbox, "a", A, None);
        let stranger_session = Uuid::from_bytes([0x5C; 16]);

        a.feed(TopologyEvent::SessionEstablished {
            session: stranger_session,
            peer_device: Uuid::from_bytes(STRANGER),
            peer_name: "stranger".to_owned(),
        })
        .await;
        a.connect(B).await; // B is who the document names now
        assert_eq!(a.state.snapshot().peer.unwrap().device, device(B));

        a.feed(TopologyEvent::SessionLost {
            session: stranger_session,
        })
        .await;
        let peer = a.state.snapshot().peer.unwrap();
        assert_eq!(peer.device, device(B));
        assert!(
            peer.connected,
            "an unrelated session's end marked the named peer offline"
        );
    }

    /// A peer reporting this machine's own device id cannot be paired with
    /// — no layout describes one machine twice — so nothing is sent, and
    /// nothing it sends is adopted. Said once, at establishment.
    #[tokio::test]
    async fn a_peer_claiming_this_machine_s_identity_is_inert_and_said_once() {
        let sandbox = Sandbox::new("degenerate-pair");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));

        let (log, _guard) = capture();
        a.connect(A).await;
        assert!(payloads(&a.drain(), MessageType::LayoutSync).is_empty());

        a.receive_layout(&arrangement(9, B, 0)).await;
        assert_eq!(a.held(), Some(3), "a layout was adopted with no valid pair");
        assert!(a.drain().is_empty());

        let text = log.text();
        assert_eq!(
            text.matches("reports this machine's own device id").count(),
            1,
            "{text}"
        );
    }

    // ---- a frame for a session that is gone ------------------------------

    /// A frame that arrives after its session ended is dropped, not
    /// attributed to whatever session happens to be live.
    #[tokio::test]
    async fn a_frame_for_an_ended_session_changes_nothing() {
        let sandbox = Sandbox::new("ended-session");
        let mut a = engine(&sandbox, "a", A, Some(arrangement(3, A, 0)));
        a.connect(B).await;
        a.disconnect().await;
        let _ = a.drain();

        a.receive_layout(&arrangement(5, B, 40)).await;
        assert_eq!(a.held(), Some(3));
        assert!(a.drain().is_empty());
    }
}
