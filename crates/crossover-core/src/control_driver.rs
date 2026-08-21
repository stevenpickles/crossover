//! Async driver for the control engine: the thin I/O shell around the
//! pure state machine in [`crate::control`], mirroring the clipboard
//! driver's shape — all policy in the engine, everything here mechanical.
//!
//! The driver bridges three worlds:
//!
//! - **The platform**: it owns the [`InputCapture`] and [`InputInjector`]
//!   handles, starts and stops capture when the engine says so (on a
//!   blocking-tolerant task — the Windows implementation performs a
//!   thread handshake), injects what the engine grants, and polls
//!   capture health so the Windows watchdog's silent hook loss (R-2)
//!   becomes a [`ControlEvent::CaptureLost`] the engine can fail closed
//!   on.
//! - **The session**: decoded peer frames go in; frames to send and
//!   fail-closed terminations come out as [`SessionCommand`]s, exactly
//!   like the clipboard driver's.
//! - **The user**: request/release commands go in; [`ControlNotice`]s
//!   come out for the application to present.
//!
//! Captured events reach the driver through a bounded channel fed by
//! the platform sink with `try_send`. Overflow drops events — for
//! motion that *is* the coalescing policy (newest wins, FR-4.2); for a
//! button it would lose a click, but never create a stuck button: the
//! engine's sent-state tracks only what was actually sent, so a dropped
//! press is a press the peer never saw and never needs releasing.
//!
//! **Session identity is carried, not assumed (FR-5.1, FR-2.3).** The
//! application fans every session's frames into this one driver, and the
//! driver hands each to the engine *tagged with the session it arrived
//! on*. Authorization is the engine's job: it grants to, and injects for,
//! one session at a time and checks every injection against the grant-
//! holder's identity, so a trusted-but-ungranted peer cannot ride another
//! peer's grant. The driver's only session-level policy is choosing which
//! peer a user "take control" command targets — the most recently
//! established session (the sole one, in the two-machine case). Each
//! outbound message the engine emits is routed back to the specific
//! session it names, never broadcast.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crossover_platform::{CursorMask, DisplayInfo, InputCapture, InputInjector};
use crossover_protocol::RawFrame;

use crate::command::{FrameTarget, SessionCommand};
use crate::control::{
    ControlAction, ControlConfig, ControlEngine, ControlEvent, ControlNotice, InboundControl,
    OutboundControl,
};
use crate::edge_driver::{EdgeMode, EdgeModeUpdate};
use crate::input::InputEvent;
use crate::outbound::{CommandReceiver, CommandSender, command_lanes};
use crate::topology::{Edge, EdgeFraction, MonitorRect, Topology};

/// How often, while controlling, the driver polls the platform for a
/// lost capture (R-2) and for the release escape gesture (ADR 0008). One
/// period bounds how long a silently lost capture goes unnoticed here,
/// and how long the escape takes to release — short enough that the way
/// out feels immediate.
const CAPTURE_HEALTH_PERIOD: Duration = Duration::from_millis(200);

/// Upper bound on events drained in one pass, so a flood cannot stall
/// the loop (NFR-1).
const MAX_DRAIN_BATCH: usize = 512;

/// Capacity of the driver's event queue. Sized for bursts: at a 1 kHz
/// mouse a full queue is a quarter second of undelivered motion, well
/// past the point where dropping stale motion beats queueing it.
const EVENT_QUEUE_CAPACITY: usize = 256;

/// Events the application (or the platform sink) feeds in.
///
/// Session-scoped variants carry the locally generated `session` id, so
/// the driver can bind control to one session and reject traffic from
/// any other (see the module docs).
#[derive(Debug)]
pub enum InputControlEvent {
    /// A session to the peer reached `ESTABLISHED`.
    SessionEstablished {
        /// Locally generated id of the session.
        session: Uuid,
    },
    /// The session ended (any reason).
    SessionLost {
        /// Locally generated id of the session.
        session: Uuid,
    },
    /// A frame arrived on a session (any type; non-control frames are
    /// ignored here).
    Frame {
        /// Locally generated id of the session it arrived on.
        session: Uuid,
        /// The frame.
        frame: RawFrame,
    },
    /// The user asked to take control of the peer.
    RequestControl,
    /// The user asked to end whichever control relationship exists.
    ReleaseControl,
    /// The cursor crossed the linked edge while controlling this machine:
    /// take control of the peer, carrying where it crossed (ADR 0009).
    EdgeLeave {
        /// Normalized crossing position along the edge.
        position: EdgeFraction,
        /// The edge-mode generation the crossing was detected under; a
        /// crossing from an earlier generation is stale and dropped.
        generation: u64,
    },
    /// The cursor returned to the linked edge while the peer controls this
    /// machine: reclaim control, carrying where it crossed (ADR 0009).
    EdgeReturn {
        /// Normalized crossing position along the edge.
        position: EdgeFraction,
        /// The edge-mode generation the crossing was detected under; a
        /// crossing from an earlier generation is stale and dropped.
        generation: u64,
    },
    /// One captured input event, pointer or key (platform sink bridge).
    Captured(InputEvent),
    /// A scheduled request timeout came due.
    RequestTimeout {
        /// The session the request went to.
        session: Uuid,
        /// Which request the timer guarded.
        request_id: u64,
    },
}

/// The control driver. Create with [`input_control`], then spawn
/// [`InputControlDriver::run`].
/// The extra wiring a machine configured for seamless transfer needs
/// (ADR 0009). Absent for an explicit-only (console) run, which never
/// places a cursor or drives an edge detector.
pub struct SeamlessInputs {
    /// This machine's screen topology (from `--left`/`--right`), for
    /// mapping a `PlaceCursor` fraction to a pixel on the entry edge.
    pub topology: Topology,
    /// Display geometry for that mapping.
    pub display: Arc<dyn DisplayInfo>,
    /// Where the edge detector's watching mode is published, derived from
    /// this machine's control state so it watches to *leave* while local,
    /// to *return* while controlled, and idles while it drives the peer.
    ///
    /// A `watch`, not a queue: the mode is a level, so latest-wins is the
    /// correct semantics and publishing never blocks. That last part is
    /// load-bearing — the mode used to ride a bounded `mpsc` that closed a
    /// cycle back onto this loop (mode → detector → crossings →
    /// `control_events` → here, and this loop is the only thing draining
    /// them), so any slowness here amplified itself into the stall-then-
    /// burst the 2026-08-19 hardware logs show.
    pub edge_mode: watch::Sender<EdgeModeUpdate>,
}

pub struct InputControlDriver {
    engine: ControlEngine,
    capture: Arc<dyn InputCapture>,
    injector: Arc<dyn InputInjector>,
    /// Desired local-cursor visibility, sent to a separate applier task
    /// (ADR 0009). The cursor must be hidden whenever the user is not
    /// working on this machine — while it drives the peer, or after the
    /// cursor edge-crossed away from it — so there is only ever one visible
    /// cursor, on the active machine. A `watch` because the platform mask's
    /// Win32 calls can block: applying them on this loop would stall event
    /// processing, so the loop only records the latest desired state here
    /// and the applier coalesces to it off-thread.
    cursor_tx: watch::Sender<bool>,
    /// The last visibility this driver sent, so it signals only on a real
    /// change of the active machine.
    cursor_hidden: bool,
    /// The local-input tick observed when the cursor was hidden while not
    /// driving the peer, for the fail-safe (ADR 0009): if a later poll sees
    /// a different tick, the user has touched this machine, so the cursor is
    /// shown again. `None` until the first poll after hiding sets it.
    cursor_wake_baseline: Option<u32>,
    /// The local-input tick attributed to *our own* injection while a peer
    /// controls this machine (ADR 0009). Re-baselined after each injection we
    /// make, so the peer's driving does not read as the user's; if a later
    /// poll sees a different tick, genuine local input arrived — the user is
    /// here — and control is relinquished to neutral. `None` while not
    /// controlled, or on a platform without the tick query (detection off).
    controlled_input_baseline: Option<u32>,
    /// The cursor visibility a transition wants, held until *after* its actions
    /// run (ADR 0009). `update_cursor` records the desired state during the
    /// engine step; `execute` flushes it once `StartCapture`/`StopCapture` and
    /// any `PlaceCursor` have completed, so the visual cue tracks the real
    /// capture state and placement instead of racing ahead of them. `None`
    /// between transitions.
    pending_cursor: Option<bool>,
    /// Seamless wiring, present exactly when the machine runs
    /// `--left`/`--right`. `None` makes placement and edge-mode emission
    /// no-ops (an explicit-only run).
    seamless: Option<SeamlessInputs>,
    /// The last edge mode published, so an unchanged mode is republished
    /// only when something else asks for it (see `edge_reprime_due`).
    last_edge_mode: EdgeMode,
    /// The generation stamped on the last edge mode published. A crossing
    /// carries the generation the detector was watching under when it
    /// fired, and it reaches this loop through a bounded queue with its
    /// `kind` frozen at detection time — so a crossing that queued behind a
    /// mode change describes a control state that no longer exists (a
    /// `Return` detected under a grant that has since ended would revoke a
    /// *fresh* one). Anything that does not match this is dropped.
    edge_mode_generation: u64,
    /// Set when a `PlaceCursor` has just put the pointer *on* the linked
    /// column, meaning the detector must re-prime there or read the
    /// placement itself as an arrival.
    ///
    /// A first grant re-primes for free, because taking it changes the edge
    /// mode. A *refreshed* grant (ADR 0009 addendum, 2026-08-19: a
    /// re-request from the session already holding control) does not change
    /// `is_controlled`, so nothing would be published and the placement
    /// could fire a spurious return — revoking the grant just re-issued.
    /// Republishing under a new generation fixes both halves at once: the
    /// detector re-primes on the placed cursor, and any crossing still in
    /// flight from before the refresh no longer matches and is dropped.
    edge_reprime_due: bool,
    /// The monitor layout last seen on the health tick, for noticing a
    /// display change (dock, undock, a monitor powering off) while running.
    /// The seamless machinery re-reads geometry on every use, so nothing
    /// here needs recomputing — but a change is *logged* (the startup
    /// topology line goes stale the moment the layout moves) and a hidden
    /// cursor mask is re-asserted, because a display change makes Windows
    /// reload the system cursors, which can un-blank a mask applied before
    /// it. The rects are origin-normalized, so a change that only re-anchors
    /// the origin (a primary-monitor swap) or moves no rect (a mode change)
    /// is invisible to this proxy; an un-blanked mask in those cases
    /// self-heals on the next control transition or local-input fail-safe.
    /// `None` until the first successful read, or without seamless.
    seen_monitors: Option<Vec<MonitorRect>>,
    events_rx: mpsc::Receiver<InputControlEvent>,
    events_tx: mpsc::Sender<InputControlEvent>,
    commands_tx: CommandSender,
    notices_tx: mpsc::Sender<ControlNotice>,
    /// Established sessions in the order they arrived. Used only to pick
    /// which peer a user "take control" command targets (the engine
    /// tracks membership itself for authorization).
    sessions: Vec<Uuid>,
}

/// Build a driver, returning the handles the application uses: the
/// event sender (session lifecycle, frames, user commands), the command
/// receiver (frames to send, terminations), and the notice receiver
/// (state changes to present).
#[must_use]
pub fn input_control(
    capture: Arc<dyn InputCapture>,
    injector: Arc<dyn InputInjector>,
    cursor_mask: Arc<dyn CursorMask>,
    seamless: Option<SeamlessInputs>,
    config: ControlConfig,
) -> (
    InputControlDriver,
    mpsc::Sender<InputControlEvent>,
    CommandReceiver,
    mpsc::Receiver<ControlNotice>,
) {
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let (commands_tx, commands_rx) = command_lanes();
    let (notices_tx, notices_rx) = mpsc::channel(64);

    // Cursor visibility is applied off this loop (the mask's Win32 calls can
    // block); the loop only publishes the latest desired state.
    let (cursor_tx, cursor_rx) = watch::channel(false);
    tokio::spawn(cursor_applier(cursor_rx, cursor_mask));

    let driver = InputControlDriver {
        engine: ControlEngine::new(config),
        capture,
        injector,
        cursor_tx,
        cursor_hidden: false,
        cursor_wake_baseline: None,
        controlled_input_baseline: None,
        pending_cursor: None,
        seamless,
        // Idle until a session establishes: emitting the initial mode is
        // the driver's job on the first state change.
        last_edge_mode: EdgeMode::Idle,
        edge_mode_generation: 0,
        edge_reprime_due: false,
        seen_monitors: None,
        events_rx,
        events_tx: events_tx.clone(),
        commands_tx,
        notices_tx,
        sessions: Vec::new(),
    };
    (driver, events_tx, commands_rx, notices_rx)
}

impl InputControlDriver {
    /// Run until every event sender is dropped. Spawn this.
    pub async fn run(mut self) {
        let mut health = tokio::time::interval(CAPTURE_HEALTH_PERIOD);
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                maybe = self.events_rx.recv() => {
                    let Some(event) = maybe else { break };
                    if !self.process(event).await {
                        return;
                    }
                }
                _ = health.tick() => {
                    // Notice a monitor-layout change first, whatever the
                    // control state: the edge detector re-primes itself, but
                    // the topology log line and a hidden cursor mask do not.
                    self.refresh_display_topology();
                    if self.engine.is_controlling() {
                        // The platform watchdog reports loss through
                        // is_capturing (R-2); this poll turns it into the
                        // engine's fail-closed transition.
                        if !self.capture.is_capturing() {
                            let actions = self.dispatch(ControlEvent::CaptureLost);
                            if !self.execute(actions).await {
                                return;
                            }
                        } else if self.capture.escape_requested() {
                            // The user pressed the release escape gesture
                            // (both Control keys); hand control back — the
                            // only way out while the keyboard is captured
                            // and the console is unreachable (ADR 0008).
                            let actions = self.dispatch(ControlEvent::UserRelease);
                            if !self.execute(actions).await {
                                return;
                            }
                        }
                    } else {
                        // Fail-safe: while the cursor is hidden and we are
                        // not driving the peer, fresh local input means the
                        // user is at this machine — show the cursor, whatever
                        // state confusion hid it (ADR 0009).
                        self.wake_cursor_on_local_input();
                    }
                    // While a peer controls this machine, two conditions end
                    // its grant and return both sides to neutral (the
                    // controller un-hides its cursor on the resulting release):
                    //   - the input desktop switched to one we cannot inject
                    //     into (a UAC/secure-desktop prompt) — feature/87; or
                    //   - the user produced genuine local input here, so the
                    //     user is at this machine (ADR 0009).
                    // Polled only while controlled, so cheap; the baseline is
                    // dropped otherwise so detection re-arms on the next grant.
                    if self.engine.is_controlled() {
                        let event = if !self.injector.can_inject() {
                            Some(ControlEvent::InputDesktopUnavailable)
                        } else if self.local_input_reclaim_due() {
                            Some(ControlEvent::LocalInputReclaim)
                        } else {
                            None
                        };
                        if let Some(event) = event {
                            let actions = self.dispatch(event);
                            if !self.execute(actions).await {
                                return;
                            }
                        }
                    } else {
                        self.controlled_input_baseline = None;
                    }
                }
            }
            // Any branch may have changed the control state; keep the edge
            // detector's watching mode in step with it (ADR 0009).
            self.sync_edge_mode();
        }
        // Dropping the sender ends the applier, which restores the cursor —
        // so it is never left hidden when the driver stops mid-control.
        tracing::debug!("input control driver stopped");
    }

    /// Drain what is immediately available, merging runs of captured
    /// events into single engine events (the engine coalesces further),
    /// then execute each in order. Returns `false` when the application
    /// side is gone.
    async fn process(&mut self, first: InputControlEvent) -> bool {
        let mut batch = vec![first];
        while batch.len() < MAX_DRAIN_BATCH {
            match self.events_rx.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break, // empty or closed; closed is handled by run()
            }
        }

        let mut captured_run: Vec<InputEvent> = Vec::new();
        for event in batch {
            // A non-capture event is a barrier: the run before it must
            // reach the engine first so ordering is preserved.
            if !matches!(event, InputControlEvent::Captured(_)) && !captured_run.is_empty() {
                let actions = self
                    .engine
                    .handle(ControlEvent::Captured(std::mem::take(&mut captured_run)));
                if !self.execute(actions).await {
                    return false;
                }
            }
            let engine_event = match event {
                InputControlEvent::Captured(input_event) => {
                    captured_run.push(input_event);
                    continue;
                }
                InputControlEvent::SessionEstablished { session } => {
                    if !self.sessions.contains(&session) {
                        self.sessions.push(session);
                    }
                    ControlEvent::SessionEstablished { session }
                }
                InputControlEvent::SessionLost { session } => {
                    self.sessions.retain(|s| *s != session);
                    ControlEvent::SessionLost { session }
                }
                InputControlEvent::RequestControl => {
                    // The CLI names no peer, so target the most recently
                    // established session — the sole one in the two-machine
                    // case. A nil id when none exist makes the engine
                    // report NoSession rather than act on a phantom peer.
                    let session = self.sessions.last().copied().unwrap_or_else(Uuid::nil);
                    ControlEvent::UserRequestControl { session }
                }
                InputControlEvent::ReleaseControl => ControlEvent::UserRelease,
                InputControlEvent::EdgeLeave {
                    position,
                    generation,
                } => {
                    let Some(event) = self.edge_leave_event(position, generation) else {
                        continue;
                    };
                    event
                }
                InputControlEvent::EdgeReturn {
                    position,
                    generation,
                } => {
                    let Some(event) = self.edge_return_event(position, generation) else {
                        continue;
                    };
                    event
                }
                InputControlEvent::RequestTimeout {
                    session,
                    request_id,
                } => ControlEvent::RequestTimeout {
                    session,
                    request_id,
                },
                InputControlEvent::Frame { session, frame } => {
                    match InboundControl::decode(frame.message_type, &frame.payload) {
                        // The engine authorizes per session; it decides
                        // whether this session's message is entitled to act.
                        Ok(Some(message)) => ControlEvent::Peer { session, message },
                        Ok(None) => continue, // not control traffic
                        Err(error) => {
                            // Peer nonconformance: fail closed (FR-2.3),
                            // terminating the offending session specifically.
                            return self
                                .commands_tx
                                .send(SessionCommand::TerminateSession {
                                    target: FrameTarget::Session(session),
                                    reason: error.to_string(),
                                })
                                .await
                                .is_ok();
                        }
                    }
                }
            };
            let actions = self.dispatch(engine_event);
            if !self.execute(actions).await {
                return false;
            }
        }
        if !captured_run.is_empty() {
            let actions = self.engine.handle(ControlEvent::Captured(captured_run));
            if !self.execute(actions).await {
                return false;
            }
        }
        true
    }

    /// Actuate a `PlaceCursor` intent (ADR 0009): map the edge fraction to
    /// a pixel on this machine's linked (entry) edge and inject an
    /// absolute move, so the pointer appears where it crossed. A no-op
    /// without a configured topology — placement is a seamless nicety, and
    /// losing it never breaks control.
    fn place_cursor(&self, fraction: EdgeFraction) {
        let Some(seamless) = &self.seamless else {
            tracing::debug!("cursor placement requested but no topology configured");
            return;
        };
        match seamless.display.monitors() {
            Ok(monitors) => {
                let point = seamless.topology.entering(fraction, &monitors);
                tracing::debug!(
                    fraction = fraction.value(),
                    x = point.x,
                    y = point.y,
                    "control: placing cursor on entry edge"
                );
                if let Err(error) = self.injector.place_cursor(point) {
                    tracing::warn!(error = %error, "cursor placement failed");
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot place cursor: display unavailable");
            }
        }
    }

    /// Notice a monitor-layout change while running (Phase 6 soak finding:
    /// under a boot-started service, docking and monitor power-off are
    /// everyday events, not corner cases). Geometry is never cached — edge
    /// detection and cursor placement re-read the layout on every use, and
    /// the edge detector re-primes itself across a change — so what remains
    /// is the stateful part: say in the log where the seamless edge now is
    /// (the startup line is stale the moment the layout moves), and
    /// re-assert a hidden cursor mask, because a display change makes
    /// Windows reload the system cursors, which can un-blank a mask applied
    /// before it. A read failure skips the tick; the next one retries.
    fn refresh_display_topology(&mut self) {
        let Some(seamless) = &self.seamless else {
            return; // explicit-only run: no display, no edge
        };
        let Ok(monitors) = seamless.display.monitors() else {
            return;
        };
        match &self.seen_monitors {
            Some(seen) if *seen == monitors => {}
            Some(_) => {
                tracing::info!(
                    ?monitors,
                    side = ?seamless.topology.side(),
                    linked_edge = ?seamless.topology.linked_edge(),
                    "display topology changed; the seamless edge follows the new layout"
                );
                self.seen_monitors = Some(monitors);
                if self.cursor_hidden {
                    // Same desired state, fresh apply: the watch channel
                    // re-notifies the applier even for an equal value.
                    let _ = self.cursor_tx.send(true);
                }
            }
            None => {
                // First successful read is the baseline, not a change: the
                // startup topology was already logged by the launcher.
                self.seen_monitors = Some(monitors);
            }
        }
    }

    /// The edge detector's mode for this machine's current control state
    /// (ADR 0009): watch to *leave* while it controls itself with a peer
    /// present, to *return* while a peer controls it, and idle while it
    /// drives the peer or has no session to cross to.
    fn edge_mode(&self) -> EdgeMode {
        if self.sessions.is_empty() || self.engine.is_controlling() {
            EdgeMode::Idle
        } else if self.engine.is_controlled() {
            EdgeMode::Returning
        } else {
            EdgeMode::Leaving
        }
    }

    /// Publish the current edge mode when it has changed — or when a cursor
    /// placement has asked for a re-prime under an unchanged mode — so
    /// detection tracks the control state.
    ///
    /// Every publication carries a fresh generation, which the detector
    /// stamps onto the crossings it then emits; crossings detected before
    /// this publication no longer match and are dropped by
    /// [`edge_crossing_is_current`](Self::edge_crossing_is_current). The
    /// generation is advanced whether or not a detector is listening: it
    /// travels *inside* the published value, so there are no two counts to
    /// keep in step, and a crossing from a departed detector could not
    /// arrive anyway.
    ///
    /// Non-blocking by construction (`watch`), so this cannot become the
    /// slow step in a loop that is also the only drainer of what the
    /// detector produces.
    fn sync_edge_mode(&mut self) {
        let reprime = std::mem::take(&mut self.edge_reprime_due);
        let Some(seamless) = &self.seamless else {
            return; // explicit-only run: no detector to publish to
        };
        let mode = self.edge_mode();
        if mode == self.last_edge_mode && !reprime {
            return;
        }
        self.last_edge_mode = mode;
        // Saturating so no run length can wrap a stale generation onto a
        // current one.
        self.edge_mode_generation = self.edge_mode_generation.saturating_add(1);
        tracing::debug!(
            ?mode,
            generation = self.edge_mode_generation,
            reprime,
            "control: edge mode -> detector"
        );
        let _ = seamless.edge_mode.send_replace(EdgeModeUpdate {
            mode,
            generation: self.edge_mode_generation,
        });
    }

    /// This machine's own linked edge (ADR 0009), for stamping an
    /// edge-driven crossing's wire `EntryPoint.edge` (v4, ADR 0018,
    /// feature/147) — see [`ControlEvent::EdgeLeave`]'s doc for why the
    /// sender's own edge is what travels rather than the peer's.
    /// `Edge::Left` without a configured topology, which an edge crossing
    /// cannot occur without in practice; defensive only.
    fn linked_edge(&self) -> Edge {
        self.seamless
            .as_ref()
            .map_or(Edge::Left, |seamless| seamless.topology.linked_edge())
    }

    /// The engine event for a leave crossing, or `None` for a stale one
    /// (`process`'s translation for [`InputControlEvent::EdgeLeave`]).
    fn edge_leave_event(&self, position: EdgeFraction, generation: u64) -> Option<ControlEvent> {
        if !self.edge_crossing_is_current(generation, "leave") {
            return None;
        }
        // Same session choice as a console take-control, plus where the
        // cursor crossed (ADR 0009).
        let session = self.sessions.last().copied().unwrap_or_else(Uuid::nil);
        Some(ControlEvent::EdgeLeave {
            session,
            position,
            edge: self.linked_edge(),
        })
    }

    /// The engine event for a return crossing, or `None` for a stale one
    /// (`process`'s translation for [`InputControlEvent::EdgeReturn`]).
    fn edge_return_event(&self, position: EdgeFraction, generation: u64) -> Option<ControlEvent> {
        if !self.edge_crossing_is_current(generation, "return") {
            return None;
        }
        Some(ControlEvent::EdgeReturn {
            position,
            edge: self.linked_edge(),
        })
    }

    /// Whether a crossing detected under `generation` still describes the
    /// control state this machine is in. A crossing carries a `kind` frozen
    /// at detection time and travels through bounded queues to get here; if
    /// the edge mode was republished on the way — a changed mode, or a
    /// re-prime after a cursor placement — acting on it would apply a
    /// decision about the old state to the new one.
    fn edge_crossing_is_current(&self, generation: u64, kind: &'static str) -> bool {
        if generation == self.edge_mode_generation {
            return true;
        }
        tracing::debug!(
            kind,
            generation,
            current = self.edge_mode_generation,
            "edge: stale crossing dropped; the control state changed after it was detected"
        );
        false
    }

    /// Hand `event` to the engine, tracing the transition — unless it is
    /// high-frequency input (local capture or a peer input batch), which
    /// would drown the log. A soak with `RUST_LOG=crossover_core=debug`
    /// then reads as the exact sequence of control transitions on this
    /// machine, so a misbehaving transfer can be diagnosed from the two
    /// machines' logs side by side (ADR 0009 diagnostics).
    fn dispatch(&mut self, event: ControlEvent) -> Vec<ControlAction> {
        let loud = !matches!(
            event,
            ControlEvent::Captured(_)
                | ControlEvent::Peer {
                    message: InboundControl::Batch(_),
                    ..
                }
        );
        // A return crosses the cursor *off* this machine: if it was being
        // controlled and now is not, the user has left, so the cursor must
        // hide here even though this machine reverts to plain local control.
        let is_edge_return = matches!(event, ControlEvent::EdgeReturn { .. });
        let was_controlled = self.engine.is_controlled();
        if loud {
            tracing::debug!(
                event = ?event,
                controlling = self.engine.is_controlling(),
                controlled = self.engine.is_controlled(),
                "control transition: event"
            );
        }
        let was_controlling = self.engine.is_controlling();
        let actions = self.engine.handle(event);
        if loud {
            let labels: Vec<&'static str> = actions.iter().map(action_label).collect();
            tracing::debug!(
                controlling = self.engine.is_controlling(),
                controlled = self.engine.is_controlled(),
                actions = ?labels,
                "control transition: result"
            );
        }
        self.update_cursor(was_controlling, was_controlled, is_edge_return);
        actions
    }

    /// Publish the local cursor's desired visibility when — and only when —
    /// the active machine changes (ADR 0009). Visibility is **sticky**: it
    /// changes on a genuine transition, never on the steady stream of input
    /// events between transitions, so a machine hidden by a return stays
    /// hidden until the user actually comes back. The rules:
    /// - started/stopped driving the peer → hidden = are we now driving;
    /// - became controlled → shown (the user is here);
    /// - stopped being controlled → hidden iff the cursor crossed away (a
    ///   return); otherwise shown (disconnect, escape — the machine is the
    ///   user's again).
    ///
    /// The desired state is not published here but held in `pending_cursor`
    /// and flushed at the end of [`execute`], *after* the transition's
    /// `StartCapture`/`StopCapture` and any `PlaceCursor` have run — so the cursor
    /// hides only once capture is actually suppressing local input, and
    /// reappears (placed) only once capture has stopped, keeping the visual cue
    /// in step with which machine is really live. The actual (blocking) mask
    /// call then happens in [`cursor_applier`].
    fn update_cursor(&mut self, was_controlling: bool, was_controlled: bool, is_edge_return: bool) {
        let is_controlling = self.engine.is_controlling();
        let is_controlled = self.engine.is_controlled();
        let hidden = if is_controlling != was_controlling {
            is_controlling
        } else if is_controlled != was_controlled {
            // Being driven → shown; control taken away → hidden only if the
            // user's cursor left across the edge (a return).
            if is_controlled { false } else { is_edge_return }
        } else {
            return; // no change in which machine is active
        };
        if hidden == self.cursor_hidden {
            return;
        }
        self.cursor_hidden = hidden;
        // Record the input tick when hiding so the fail-safe can notice the
        // user touching this machine afterwards; clear it when showing.
        self.cursor_wake_baseline = if hidden {
            self.capture.last_input_tick()
        } else {
            None
        };
        tracing::debug!(hidden, "cursor: active-machine changed");
        // Held, not sent: `execute` flushes it after this transition's actions
        // run, so the cue never precedes the capture/placement it stands for.
        self.pending_cursor = Some(hidden);
    }

    /// Cursor fail-safe (ADR 0009): while the cursor is hidden and this
    /// machine is not driving the peer, a change in the local-input tick
    /// since the cursor was hidden means the user has touched this machine —
    /// so show the cursor, recovering from whatever state confusion hid it.
    /// A no-op on platforms without the input-tick query.
    fn wake_cursor_on_local_input(&mut self) {
        if !self.cursor_hidden {
            return;
        }
        let (Some(tick), Some(baseline)) =
            (self.capture.last_input_tick(), self.cursor_wake_baseline)
        else {
            return; // no query, or no baseline was recorded
        };
        if tick != baseline {
            tracing::debug!("cursor: local input while hidden — showing");
            self.cursor_hidden = false;
            self.cursor_wake_baseline = None;
            let _ = self.cursor_tx.send(false);
        }
    }

    /// Whether genuine local input — not our own injection — has arrived while
    /// a peer controls this machine, meaning the user is here (ADR 0009).
    ///
    /// The signal is the system input tick, re-baselined after every injection
    /// we make (see `execute`), so the peer's driving does not read as the
    /// user's. The first poll of a fresh grant only arms the baseline; a later
    /// poll seeing a different tick fires. A platform without the tick query
    /// keeps `None` and never fires, so the reclaim is simply unavailable
    /// there. During simultaneous driving-and-touching the peer's next
    /// injection can re-baseline past a local event; that contention is not
    /// the reversal case, and it resolves the moment the peer pauses.
    fn local_input_reclaim_due(&mut self) -> bool {
        let Some(tick) = self.capture.last_input_tick() else {
            return false; // no query on this platform — detection off
        };
        match self.controlled_input_baseline {
            None => {
                self.controlled_input_baseline = Some(tick);
                false
            }
            Some(baseline) => tick != baseline,
        }
    }

    /// Execute engine actions in order. Returns `false` when the
    /// application side is gone.
    async fn execute(&mut self, actions: Vec<ControlAction>) -> bool {
        // A StartCapture failure mid-list generates fail-closed actions;
        // they run after the current list so the user sees events in
        // the order they truly happened (gained, then lost) and no
        // action of the triggering transition is skipped.
        let mut deferred: Vec<ControlAction> = Vec::new();
        for action in actions {
            match action {
                ControlAction::Send { session, message } => match message.encode() {
                    Ok((message_type, payload)) => {
                        // Routed to the one session the engine named, never
                        // broadcast: our input goes only to the peer we
                        // control (FR-5.1).
                        if self
                            .commands_tx
                            .send(SessionCommand::SendFrame {
                                target: FrameTarget::Session(session),
                                message_type,
                                payload,
                            })
                            .await
                            .is_err()
                        {
                            return false;
                        }
                    }
                    Err(error) => {
                        // Engine-built messages are always valid; log the
                        // impossible rather than panic (NFR-1 discipline).
                        tracing::error!(error = %error, "unencodable control message dropped");
                    }
                },
                ControlAction::StartCapture => {
                    if let Err(error) = self.start_capture().await {
                        // The transfer this capture served must not limp
                        // on believing it controls anything: the capture-
                        // lost path releases the peer and reverts to
                        // local (fail closed, NFR-3 diagnostic included).
                        tracing::error!(error = %error, "start_capture failed; failing closed");
                        deferred.extend(self.dispatch(ControlEvent::CaptureLost));
                    }
                    // Cursor visibility follows the control-state transition
                    // (see `update_cursor`), not this action.
                }
                ControlAction::StopCapture => {
                    let capture = Arc::clone(&self.capture);
                    let result = tokio::task::spawn_blocking(move || capture.stop_capture()).await;
                    if let Ok(Err(error)) = result {
                        // Lenient by trait contract: error paths call
                        // stop exactly when it must not matter.
                        tracing::warn!(error = %error, "stop_capture reported failure");
                    }
                    // Cursor visibility follows the control-state transition
                    // (see `update_cursor`), not this action.
                }
                ControlAction::Inject(events) => {
                    if let Err(error) = self.injector.inject(&events) {
                        // Nothing to retry into (UIPI and friends, R-1);
                        // observable, not silent (NFR-3).
                        tracing::warn!(error = %error, "input injection failed");
                    }
                    // Our injection advances the system input tick; re-baseline
                    // so the peer's driving is not mistaken for the user's
                    // local input (ADR 0009). Only meaningful while controlled;
                    // drain-on-release injections run after the grant is gone.
                    if self.engine.is_controlled() {
                        self.controlled_input_baseline = self.capture.last_input_tick();
                    }
                }
                ControlAction::PlaceCursor(fraction) => {
                    self.place_cursor(fraction);
                    // The pointer now sits on the linked column, which is
                    // also the trigger column. Ask for a re-prime there —
                    // needed even when the control state, and so the mode,
                    // did not change at all (a refreshed grant).
                    self.edge_reprime_due = true;
                }
                ControlAction::ScheduleRequestTimeout {
                    session,
                    request_id,
                    delay,
                } => {
                    let notify = self.events_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = notify
                            .send(InputControlEvent::RequestTimeout {
                                session,
                                request_id,
                            })
                            .await;
                    });
                }
                ControlAction::Terminate { session, reason } => {
                    if self
                        .commands_tx
                        .send(SessionCommand::TerminateSession {
                            target: FrameTarget::Session(session),
                            reason,
                        })
                        .await
                        .is_err()
                    {
                        return false;
                    }
                }
                ControlAction::Notify(notice) => {
                    // Notices are advisory; a full queue must not stall
                    // input handling.
                    let _ = self.notices_tx.try_send(notice);
                }
            }
        }
        if !deferred.is_empty() {
            // Depth is bounded: the fail-closed transition never emits
            // another StartCapture. The nested call flushes `pending_cursor`
            // (the fail-closed transition overwrote it), so it is applied once,
            // with the final value.
            return Box::pin(self.execute(deferred)).await;
        }
        // The transition's actions have all run; now let the cursor follow, so
        // the hide lands only after capture suppresses local input and the show
        // only after capture stops and the cursor is placed (ADR 0009).
        if let Some(hidden) = self.pending_cursor.take() {
            // Non-blocking: the applier coalesces to this latest value.
            let _ = self.cursor_tx.send(hidden);
        }
        true
    }

    /// Start capture with a sink that bridges into the event queue.
    /// Runs on a blocking-tolerant task: the Windows implementation
    /// handshakes with its pump thread.
    async fn start_capture(&self) -> Result<(), crossover_platform::InputError> {
        let capture = Arc::clone(&self.capture);
        let bridge = self.events_tx.clone();
        tokio::task::spawn_blocking(move || {
            let sink = Box::new(move |event: InputEvent| {
                // try_send IS the backpressure policy — see module docs.
                let _ = bridge.try_send(InputControlEvent::Captured(event));
            });
            capture.start_capture(sink)
        })
        .await
        .unwrap_or_else(|join_error| {
            Err(crossover_platform::InputError::CaptureUnavailable {
                reason: format!("start_capture task failed: {join_error}"),
            })
        })
    }
}

/// Apply the desired cursor visibility off the control loop (ADR 0009).
/// The platform mask's Win32 calls (`SetSystemCursor`, `SystemParametersInfo`)
/// can block for a noticeable time; running them inline would stall event
/// processing and make the cursor lag reality. The `watch` coalesces rapid
/// crossings to the latest desired state, and each apply runs on a blocking
/// task, so a burst converges to the correct final cursor without a backlog.
/// When the driver drops the sender, the cursor is restored — it can never
/// be left hidden.
async fn cursor_applier(mut desired: watch::Receiver<bool>, mask: Arc<dyn CursorMask>) {
    while desired.changed().await.is_ok() {
        let hidden = *desired.borrow_and_update();
        let mask = Arc::clone(&mask);
        let _ = tokio::task::spawn_blocking(move || {
            let result = if hidden { mask.hide() } else { mask.show() };
            if let Err(error) = result {
                // Masking is a display nicety; a failure never disturbs
                // control (ADR 0009).
                tracing::warn!(error = %error, hidden, "cursor mask apply failed");
            }
        })
        .await;
    }
    // Driver gone: make sure the cursor is visible again.
    let _ = tokio::task::spawn_blocking(move || {
        let _ = mask.show();
    })
    .await;
}

/// A short label for a control action, for the transition trace. Kept
/// coarse (message *kind*, not contents) so the log never carries input.
fn action_label(action: &ControlAction) -> &'static str {
    match action {
        ControlAction::Send { message, .. } => match message {
            OutboundControl::Request(_) => "Send(Request)",
            OutboundControl::Response(_) => "Send(Response)",
            OutboundControl::Release(_) => "Send(Release)",
            OutboundControl::Batch(_) => "Send(Batch)",
            OutboundControl::ReleaseAll(_) => "Send(ReleaseAll)",
        },
        ControlAction::StartCapture => "StartCapture",
        ControlAction::StopCapture => "StopCapture",
        ControlAction::Inject(_) => "Inject",
        ControlAction::PlaceCursor(_) => "PlaceCursor",
        ControlAction::ScheduleRequestTimeout { .. } => "ScheduleRequestTimeout",
        ControlAction::Terminate { .. } => "Terminate",
        ControlAction::Notify(_) => "Notify",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::{mpsc, watch};
    use tokio::time::timeout;
    use uuid::Uuid;

    use crossover_platform::fakes::{
        FakeCursorMask, FakeDisplay, FakeInputCapture, FakeInputInjector,
    };
    use crossover_platform::{CursorMask, DisplayInfo, InputCapture, InputInjector, Screen};
    use crossover_protocol::hello::MessageType;
    use crossover_protocol::{
        ControlRelease, ControlRequest, ControlResponse, ControlVerdict, DenyReason, EntryPoint,
        InputBatch, RawFrame, ReleaseAllInput, WireButton, WireInputEvent,
        control::Edge as WireEdge,
    };

    use super::{InputControlEvent, input_control};
    use crate::command::{FrameTarget, SessionCommand};
    use crate::control::{ControlConfig, ControlNotice};
    use crate::edge_driver::{CrossingKind, EdgeMode, EdgeModeUpdate, REARM_MARGIN};
    use crate::input::{InputEvent, KeyEvent, PointerButton, PointerEvent, hid};
    use crate::topology::{CursorPoint, EdgeFraction, LinkSide, MonitorRect, Topology};

    /// A wire [`EntryPoint`] for a test that only needs a round-trippable,
    /// decodable position — via [`EntryPoint::unaddressed`], the
    /// "unaddressed" reading (feature/147, ADR 0018). `edge` is `Left`:
    /// the rig is a left-member topology, whose linked edge is `Right`
    /// (ADR 0009), so the receiver's-terms wire edge is its mirror
    /// (docs/PROTOCOL.md §6.1) — though placement in these tests is
    /// driven by `fraction` alone, never by `edge`.
    fn entry_point(fraction: EdgeFraction) -> EntryPoint {
        EntryPoint::unaddressed(WireEdge::Left, fraction.to_wire())
    }

    const HD: Screen = Screen {
        width: 1920,
        height: 1080,
    };

    /// The session the single-session tests operate on.
    const SESSION: Uuid = Uuid::from_bytes([0xA1; 16]);
    /// A distinct concurrent session, for the cross-session mediation tests.
    const OTHER_SESSION: Uuid = Uuid::from_bytes([0xB2; 16]);

    struct Rig {
        capture: Arc<FakeInputCapture>,
        injector: Arc<FakeInputInjector>,
        cursor_mask: Arc<FakeCursorMask>,
        display: Arc<FakeDisplay>,
        events: mpsc::Sender<InputControlEvent>,
        commands: crate::outbound::CommandReceiver,
        notices: mpsc::Receiver<ControlNotice>,
        /// A real subscription to what the driver publishes — the detecting
        /// rig included, since a `watch` has as many receivers as it likes.
        edge_modes: watch::Receiver<EdgeModeUpdate>,
    }

    /// The rig's edge-poll period, mirroring the application's ~125 Hz.
    const EDGE_POLL: Duration = Duration::from_millis(8);

    fn rig() -> Rig {
        build_rig(false)
    }

    /// A rig wired the way the application wires a seamless machine: a real
    /// [`EdgeDetectDriver`] watching the same [`FakeDisplay`] the injector
    /// places the cursor on, with its crossings forwarded back in as control
    /// events. That closes the loop placement → detection → transfer, which
    /// a bare edge-mode receiver leaves open.
    fn edge_detecting_rig() -> Rig {
        build_rig(true)
    }

    fn build_rig(detect: bool) -> Rig {
        let capture = Arc::new(FakeInputCapture::new());
        let injector = Arc::new(FakeInputInjector::new());
        let cursor_mask = Arc::new(FakeCursorMask::new());
        let display = Arc::new(FakeDisplay::new(HD));
        // Placements move the display's cursor, as a real absolute move does.
        injector.follow(Arc::clone(&display));
        // A left-member topology (links on the right edge) so PlaceCursor
        // has geometry to map through; most tests never trigger it.
        let topology = Topology::new(LinkSide::Left);
        let (edge_mode_tx, detection) = if detect {
            let (edge_driver, mode_tx, crossings) = crate::edge_driver::edge_detect(
                Arc::clone(&display) as Arc<dyn DisplayInfo>,
                topology,
                EDGE_POLL,
            );
            (mode_tx, Some((edge_driver, crossings)))
        } else {
            (watch::channel(EdgeModeUpdate::initial()).0, None)
        };
        // Subscribed before the driver exists, so no publication is missed.
        let edge_modes = edge_mode_tx.subscribe();
        let seamless = super::SeamlessInputs {
            topology,
            display: Arc::clone(&display) as Arc<dyn DisplayInfo>,
            edge_mode: edge_mode_tx,
        };
        let (driver, events, commands, notices) = input_control(
            Arc::clone(&capture) as Arc<dyn InputCapture>,
            Arc::clone(&injector) as Arc<dyn InputInjector>,
            Arc::clone(&cursor_mask) as Arc<dyn CursorMask>,
            Some(seamless),
            ControlConfig {
                request_timeout: Duration::from_millis(100),
            },
        );
        tokio::spawn(driver.run());
        if let Some((edge_driver, mut crossings)) = detection {
            tokio::spawn(edge_driver.run());
            let control_events = events.clone();
            // The application's `spawn_edge_wiring`, in miniature.
            tokio::spawn(async move {
                while let Some(crossing) = crossings.recv().await {
                    let event = match crossing.kind {
                        CrossingKind::Leave => InputControlEvent::EdgeLeave {
                            position: crossing.position,
                            generation: crossing.generation,
                        },
                        CrossingKind::Return => InputControlEvent::EdgeReturn {
                            position: crossing.position,
                            generation: crossing.generation,
                        },
                    };
                    if control_events.send(event).await.is_err() {
                        break;
                    }
                }
            });
        }
        Rig {
            capture,
            injector,
            cursor_mask,
            display,
            events,
            commands,
            notices,
            edge_modes,
        }
    }

    async fn next_command(rig: &mut Rig) -> SessionCommand {
        timeout(Duration::from_secs(5), rig.commands.recv())
            .await
            .expect("timed out waiting for a session command")
            .expect("command channel closed")
    }

    async fn next_notice(rig: &mut Rig) -> ControlNotice {
        timeout(Duration::from_secs(5), rig.notices.recv())
            .await
            .expect("timed out waiting for a notice")
            .expect("notice channel closed")
    }

    /// The next edge-mode publication. A `watch` coalesces, so this is the
    /// newest value at the moment it is read, not necessarily every value
    /// the driver sent — which is exactly the contract the detector reads
    /// it under.
    async fn next_edge_update(rig: &mut Rig) -> EdgeModeUpdate {
        timeout(Duration::from_secs(5), rig.edge_modes.changed())
            .await
            .expect("timed out waiting for an edge mode")
            .expect("edge-mode channel closed");
        *rig.edge_modes.borrow_and_update()
    }

    async fn next_edge_mode(rig: &mut Rig) -> EdgeMode {
        next_edge_update(rig).await.mode
    }

    /// The edge-mode generation in force once `expected` has been reached.
    /// A crossing injected by hand must carry this to be acted on. Read
    /// from the published value rather than counted: the driver drains
    /// events in batches (two control-state changes in one batch produce a
    /// single publication) and the channel coalesces besides, so the only
    /// authority on the current generation is the sender's own stamp.
    async fn generation_at(rig: &mut Rig, expected: EdgeMode) -> u64 {
        loop {
            let update = next_edge_update(rig).await;
            if update.mode == expected {
                return update.generation;
            }
        }
    }

    /// Wait until the cursor mask reaches `hidden` (it is applied on a
    /// separate task, so visibility settles asynchronously), or fail.
    async fn await_cursor(rig: &Rig, hidden: bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while rig.cursor_mask.is_hidden() != hidden {
            assert!(
                tokio::time::Instant::now() < deadline,
                "cursor never reached hidden={hidden}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn frame(message_type: MessageType, payload: Vec<u8>) -> InputControlEvent {
        frame_on(SESSION, message_type, payload)
    }

    fn frame_on(session: Uuid, message_type: MessageType, payload: Vec<u8>) -> InputControlEvent {
        InputControlEvent::Frame {
            session,
            frame: RawFrame {
                message_type: message_type.wire(),
                message_id: 1,
                payload,
            },
        }
    }

    /// Bring a rig to the controlling state: request, grant, capture on.
    async fn make_controlling(rig: &mut Rig) {
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.events
            .send(InputControlEvent::RequestControl)
            .await
            .unwrap();
        let SessionCommand::SendFrame { message_type, .. } = next_command(rig).await else {
            panic!("expected the control request frame");
        };
        assert_eq!(message_type, MessageType::ControlRequest.wire());
        assert_eq!(next_notice(rig).await, ControlNotice::RequestSent);

        let response = ControlResponse {
            request_id: 1,
            verdict: ControlVerdict::Granted,
        };
        rig.events
            .send(frame(
                MessageType::ControlResponse,
                response.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(next_notice(rig).await, ControlNotice::ControlGained);
        assert!(rig.capture.is_capturing(), "grant must start capture");
    }

    #[tokio::test]
    async fn request_grant_capture_forward_full_path() {
        let mut rig = rig();
        make_controlling(&mut rig).await;

        // The user moves and clicks; the fake delivers through the sink.
        rig.capture.raise(PointerEvent::Motion { dx: 3, dy: 1 });
        rig.capture.raise(PointerEvent::Motion { dx: 2, dy: 2 });
        rig.capture.raise(PointerEvent::Button {
            button: PointerButton::Left,
            pressed: true,
        });

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut rig).await
        else {
            panic!("expected an input batch frame");
        };
        assert_eq!(message_type, MessageType::InputBatch.wire());
        let batch = InputBatch::decode_payload(&payload).unwrap();
        assert_eq!(batch.sequence, 1);
        // Motion coalesced, order preserved, button intact.
        assert_eq!(
            batch.events,
            vec![
                WireInputEvent::Motion { dx: 5, dy: 3 },
                WireInputEvent::Button {
                    button: WireButton::Left,
                    pressed: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn hand_back_stops_capture_and_releases_remotely() {
        let mut rig = rig();
        make_controlling(&mut rig).await;

        rig.events
            .send(InputControlEvent::ReleaseControl)
            .await
            .unwrap();

        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected ReleaseAllInput");
        };
        assert_eq!(message_type, MessageType::ReleaseAllInput.wire());
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected ControlRelease");
        };
        assert_eq!(message_type, MessageType::ControlRelease.wire());
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::ControlEnded(crate::control::ControlEndReason::HandedBack)
        );
        assert!(!rig.capture.is_capturing(), "hand-back must stop capture");
    }

    /// The local cursor is hidden while this machine drives the peer and
    /// restored the moment control ends (ADR 0009): the controller's frozen
    /// cursor must not linger as a second visible pointer, and it must
    /// always come back.
    #[tokio::test]
    async fn controlling_hides_the_local_cursor_and_return_shows_it() {
        let mut rig = rig();
        // Not controlling yet: the cursor is untouched.
        assert!(!rig.cursor_mask.is_hidden());

        make_controlling(&mut rig).await;
        // Gaining control hides the cursor (applied off-thread).
        await_cursor(&rig, true).await;

        // Hand control back: capture stops and the cursor is restored.
        rig.events
            .send(InputControlEvent::ReleaseControl)
            .await
            .unwrap();
        let _release_all = next_command(&mut rig).await;
        let _release = next_command(&mut rig).await;
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::ControlEnded(crate::control::ControlEndReason::HandedBack)
        );
        await_cursor(&rig, false).await;
    }

    /// When the peer returns across the edge (a Release carrying an entry
    /// fraction), the controller stops capturing, places its cursor where
    /// control came back, and only *then* shows it — the cursor never appears
    /// at the stale capture edge and jumps. Guards the deferred cursor emission
    /// (ADR 0009): visibility follows the transition's actions, not races ahead.
    #[tokio::test]
    async fn a_returning_controller_places_the_cursor_before_showing_it() {
        let mut rig = rig();
        make_controlling(&mut rig).await;
        await_cursor(&rig, true).await; // hidden while driving the peer
        assert!(
            rig.injector.placements().is_empty(),
            "no placement yet while driving"
        );

        // The peer hands control back across the edge, carrying where it left.
        let release = ControlRelease {
            entry: Some(entry_point(EdgeFraction::new(0.5))),
        };
        rig.events
            .send(frame(
                MessageType::ControlRelease,
                release.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::ControlEnded(crate::control::ControlEndReason::Revoked)
        );

        // The cursor is shown only after execute has run StopCapture and the
        // placement: by the time it is visible again, the entry placement is
        // already recorded — so it was placed before it was shown.
        await_cursor(&rig, false).await;
        assert_eq!(
            rig.injector.placements().len(),
            1,
            "the returning cursor is placed before it is shown"
        );
    }

    /// A display change (dock, undock, a monitor powering off) can make
    /// Windows reload the system cursors, un-blanking a hidden mask — so a
    /// monitor-layout change observed while the cursor is hidden re-asserts
    /// the mask (Phase 6 soak finding).
    #[tokio::test]
    async fn a_monitor_layout_change_reasserts_a_hidden_cursor_mask() {
        let mut rig = rig();
        make_controlling(&mut rig).await;
        await_cursor(&rig, true).await; // hidden while driving the peer
        // Give a health tick time to record the current layout as the
        // baseline, then move the layout under the driver.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let hides = rig.cursor_mask.hide_calls();
        rig.display.set_monitors(vec![MonitorRect {
            left: 0,
            top: 0,
            width: 800,
            height: 600,
        }]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while rig.cursor_mask.hide_calls() == hides {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the mask was never re-asserted after the layout change"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            rig.cursor_mask.is_hidden(),
            "the re-assert must keep it hidden"
        );
    }

    /// The controlled machine hides its cursor when the user's cursor
    /// edge-crosses *away* from it (a return), and shows it again when the
    /// user arrives back (a fresh grant) — so there is only ever one visible
    /// cursor, on the active machine (ADR 0009).
    #[tokio::test]
    async fn a_return_hides_the_controlled_cursor_and_re_entry_shows_it() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();

        // The peer takes control via an edge crossing: the user is now here,
        // so the cursor stays visible.
        let take = ControlRequest {
            request_id: 1,
            entry: Some(entry_point(EdgeFraction::new(0.5))),
        };
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                take.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);
        await_cursor(&rig, false).await; // being controlled keeps it visible

        // The cursor returns across this machine's edge: the user has left,
        // so the cursor must hide even though control reverts to local.
        let generation = generation_at(&mut rig, EdgeMode::Returning).await;
        rig.events
            .send(InputControlEvent::EdgeReturn {
                position: EdgeFraction::new(0.5),
                generation,
            })
            .await
            .unwrap();
        let _release = next_command(&mut rig).await;
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerControlRevoked
        );
        await_cursor(&rig, true).await;

        // The user comes back — a fresh grant — so the cursor shows again.
        let re_enter = ControlRequest {
            request_id: 2,
            entry: Some(entry_point(EdgeFraction::new(0.5))),
        };
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                re_enter.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);
        await_cursor(&rig, false).await;
    }

    /// The fail-safe (ADR 0009): a cursor hidden while this machine is not
    /// driving the peer is shown again the moment local input is detected —
    /// the user is here, whatever state confusion hid it.
    #[tokio::test]
    async fn local_input_wakes_a_hidden_cursor() {
        let mut rig = rig();
        rig.capture.set_last_input_tick(1000);
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();

        // Be controlled, then return → hidden, and no longer controlled.
        let take = ControlRequest {
            request_id: 1,
            entry: Some(entry_point(EdgeFraction::new(0.5))),
        };
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                take.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);
        let generation = generation_at(&mut rig, EdgeMode::Returning).await;
        rig.events
            .send(InputControlEvent::EdgeReturn {
                position: EdgeFraction::new(0.5),
                generation,
            })
            .await
            .unwrap();
        let _release = next_command(&mut rig).await;
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerControlRevoked
        );
        await_cursor(&rig, true).await;

        // The user touches this machine: the input tick advances, and the
        // health-tick fail-safe brings the cursor back.
        rig.capture.set_last_input_tick(2000);
        await_cursor(&rig, false).await;
    }

    /// ADR 0009: while a peer controls this machine, genuine local input —
    /// distinguished from the peer's own injections by re-baselining the tick
    /// on each injection — reclaims the grant to neutral. The peer is told to
    /// release (returning it to local) and it is reported distinctly.
    #[tokio::test]
    async fn local_input_reclaims_the_peers_grant() {
        let mut rig = rig();
        rig.capture.set_last_input_tick(1000);
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();

        // Peer takes control (no entry → no cursor placement to consume).
        let take = ControlRequest {
            request_id: 1,
            entry: None,
        };
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                take.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);

        // The peer drives (motion only, nothing held): injecting it re-baselines
        // the input tick to 1000, so the peer's own driving does not read as the
        // user's local input.
        let batch = InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Motion { dx: 10, dy: 0 }],
        };
        rig.events
            .send(frame(
                MessageType::InputBatch,
                batch.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        // Wait until that motion is actually injected — the injection is what
        // re-baselines the tick to 1000 — before simulating local input, or the
        // bump below could be re-baselined away.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while rig.injector.injected_pointers().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "peer motion never injected"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The user touches this machine: the tick advances past our injection
        // baseline, so within a health period the grant is reclaimed to neutral.
        rig.capture.set_last_input_tick(2000);
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected the Release frame to the peer");
        };
        assert_eq!(message_type, MessageType::ControlRelease.wire());
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerControlReclaimedLocally
        );
    }

    #[tokio::test]
    async fn granted_peer_input_is_injected_and_released_on_disconnect() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();

        // Peer requests; we grant.
        let request = ControlRequest {
            request_id: 7,
            entry: None,
        };
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                request.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected the grant response");
        };
        assert_eq!(message_type, MessageType::ControlResponse.wire());
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);

        // Peer drags: press arrives, then the session dies mid-drag.
        let batch = InputBatch {
            sequence: 1,
            events: vec![
                WireInputEvent::Motion { dx: 10, dy: 0 },
                WireInputEvent::Button {
                    button: WireButton::Left,
                    pressed: true,
                },
            ],
        };
        rig.events
            .send(frame(
                MessageType::InputBatch,
                batch.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        rig.events
            .send(InputControlEvent::SessionLost { session: SESSION })
            .await
            .unwrap();
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerControlLostOnDisconnect
        );

        // FR-4.4 through the whole driver: the injected stream ends with
        // the synthesized release, so nothing is left held.
        let injected = rig.injector.injected_pointers();
        assert_eq!(
            injected,
            vec![
                PointerEvent::Motion { dx: 10, dy: 0 },
                PointerEvent::Button {
                    button: PointerButton::Left,
                    pressed: true,
                },
                PointerEvent::Button {
                    button: PointerButton::Left,
                    pressed: false,
                },
            ]
        );
    }

    #[tokio::test]
    async fn malformed_control_payload_terminates_the_session() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.events
            .send(frame(MessageType::InputBatch, vec![0xFF; 16]))
            .await
            .unwrap();
        let SessionCommand::TerminateSession { .. } = next_command(&mut rig).await else {
            panic!("malformed input must terminate the session");
        };
    }

    #[tokio::test]
    async fn silent_capture_loss_fails_closed_within_a_health_period() {
        let mut rig = rig();
        make_controlling(&mut rig).await;

        // The platform loses the hook without telling anyone (R-2); only
        // is_capturing tells the truth now.
        rig.capture.lose_capture_silently();

        // Within a health period the driver must notice and fail closed:
        // release the peer, end control, report.
        let SessionCommand::SendFrame { message_type, .. } =
            timeout(Duration::from_secs(5), rig.commands.recv())
                .await
                .expect("driver never noticed silent capture loss")
                .expect("command channel closed")
        else {
            panic!("expected ReleaseAllInput after capture loss");
        };
        assert_eq!(message_type, MessageType::ReleaseAllInput.wire());
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::ControlEnded(crate::control::ControlEndReason::CaptureLost)
        );
    }

    #[tokio::test]
    async fn failed_capture_start_releases_the_grant() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.capture.fail_next_start("no hook for you");

        rig.events
            .send(InputControlEvent::RequestControl)
            .await
            .unwrap();
        let _request_frame = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::RequestSent);

        let response = ControlResponse {
            request_id: 1,
            verdict: ControlVerdict::Granted,
        };
        rig.events
            .send(frame(
                MessageType::ControlResponse,
                response.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        // The grant arrives, capture fails to start, and the driver must
        // fail closed rather than pretend: peer released, control ended.
        assert_eq!(next_notice(&mut rig).await, ControlNotice::ControlGained);
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected ReleaseAllInput after failed capture start");
        };
        assert_eq!(message_type, MessageType::ReleaseAllInput.wire());
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected ControlRelease after failed capture start");
        };
        assert_eq!(message_type, MessageType::ControlRelease.wire());
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::ControlEnded(crate::control::ControlEndReason::CaptureLost)
        );
        assert!(!rig.capture.is_capturing());
    }

    #[tokio::test]
    async fn request_timeout_reverts_and_notifies() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.events
            .send(InputControlEvent::RequestControl)
            .await
            .unwrap();
        let _request_frame = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::RequestSent);

        // No response ever comes; the scheduled timeout must fire.
        assert_eq!(next_notice(&mut rig).await, ControlNotice::RequestTimedOut);
        assert!(!rig.capture.is_capturing());
    }

    #[tokio::test]
    async fn peer_release_after_hand_back_finds_nothing_held() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();

        // Grant, receive a press, then the peer hands back properly.
        let request = ControlRequest {
            request_id: 1,
            entry: None,
        };
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                request.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        let _notice = next_notice(&mut rig).await;

        let batch = InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::X1,
                pressed: true,
            }],
        };
        rig.events
            .send(frame(
                MessageType::InputBatch,
                batch.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let release = ReleaseAllInput { after_sequence: 1 };
        rig.events
            .send(frame(
                MessageType::ReleaseAllInput,
                release.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        rig.events
            .send(frame(
                MessageType::ControlRelease,
                crossover_protocol::ControlRelease { entry: None }
                    .encode_payload()
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerReleasedControl
        );

        // The press was released exactly once (by ReleaseAllInput); the
        // following ControlRelease found a clear state.
        let injected = rig.injector.injected_pointers();
        let releases = injected
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    PointerEvent::Button {
                        button: PointerButton::X1,
                        pressed: false,
                    }
                )
            })
            .count();
        assert_eq!(releases, 1, "exactly one release for one press");
    }

    /// End-to-end through the driver: while SESSION legitimately controls
    /// this machine, an input batch from a *different* trusted session
    /// must never be injected. The engine terminates the intruding
    /// session (routed specifically to it), and the legitimate
    /// controller's input still flows. This is the exact scenario the
    /// security review flagged — a second trusted peer riding another
    /// peer's grant (FR-2.3, FR-5.1).
    #[tokio::test]
    async fn input_from_a_non_controlling_session_is_terminated_not_injected() {
        let mut rig = rig();
        // SESSION establishes and takes control: the machine IS being
        // driven by it.
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 1,
                    entry: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);

        // A second trusted peer connects and, holding no grant of its own,
        // sends input.
        rig.events
            .send(InputControlEvent::SessionEstablished {
                session: OTHER_SESSION,
            })
            .await
            .unwrap();
        let intruder = InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Left,
                pressed: true,
            }],
        };
        rig.events
            .send(frame_on(
                OTHER_SESSION,
                MessageType::InputBatch,
                intruder.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        // The intruder's session is terminated — and only its session.
        let SessionCommand::TerminateSession { target, .. } = next_command(&mut rig).await else {
            panic!("intruder input must terminate its session");
        };
        assert_eq!(
            target,
            FrameTarget::Session(OTHER_SESSION),
            "the termination must target the intruder, not the controller"
        );

        // The legitimate controller's input is injected, and the
        // intruder's never was.
        let legit = InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Right,
                pressed: true,
            }],
        };
        rig.events
            .send(frame(
                MessageType::InputBatch,
                legit.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let injected = rig.injector.injected_pointers();
            if injected.contains(&PointerEvent::Button {
                button: PointerButton::Right,
                pressed: true,
            }) {
                assert!(
                    !injected.contains(&PointerEvent::Button {
                        button: PointerButton::Left,
                        pressed: true,
                    }),
                    "input from a non-controlling session was injected — grant bypass"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the controller's input never arrived"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A second peer requesting control while another already controls
    /// this machine is deterministically *denied* (FR-5.1), and the
    /// denial is routed to that specific session — not granted, and not
    /// disrupting the incumbent.
    #[tokio::test]
    async fn a_second_peers_control_request_is_denied() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        // SESSION takes control legitimately.
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 1,
                    entry: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);

        // A second peer establishes and requests control.
        rig.events
            .send(InputControlEvent::SessionEstablished {
                session: OTHER_SESSION,
            })
            .await
            .unwrap();
        rig.events
            .send(frame_on(
                OTHER_SESSION,
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 5,
                    entry: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();

        // It is denied, addressed to the second peer, with the reason.
        let SessionCommand::SendFrame {
            target,
            message_type,
            payload,
        } = next_command(&mut rig).await
        else {
            panic!("the second request must be answered");
        };
        assert_eq!(target, FrameTarget::Session(OTHER_SESSION));
        assert_eq!(message_type, MessageType::ControlResponse.wire());
        let response = ControlResponse::decode_payload(&payload).unwrap();
        assert_eq!(response.request_id, 5);
        assert_eq!(
            response.verdict,
            ControlVerdict::Denied(DenyReason::AlreadyControlled),
            "a second peer must be denied, never granted a shared desktop"
        );
    }

    /// End to end through the driver: a granted peer's key batch reaches
    /// the injector as key events (ADR 0008), interleaved in order with a
    /// pointer event.
    #[tokio::test]
    async fn granted_keyboard_input_reaches_the_injector() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 1,
                    entry: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);

        // Shift held, click, 'A': a chord whose ordering must survive.
        let batch = InputBatch {
            sequence: 1,
            events: vec![
                WireInputEvent::Key {
                    key: hid::LEFT_SHIFT,
                    pressed: true,
                    repeat: false,
                    text: None,
                },
                WireInputEvent::Button {
                    button: WireButton::Left,
                    pressed: true,
                },
                WireInputEvent::Key {
                    key: hid::A,
                    pressed: true,
                    repeat: false,
                    text: Some("A".to_owned()),
                },
            ],
        };
        rig.events
            .send(frame(
                MessageType::InputBatch,
                batch.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        let want = vec![
            InputEvent::Key(KeyEvent::press(hid::LEFT_SHIFT)),
            InputEvent::Pointer(PointerEvent::Button {
                button: PointerButton::Left,
                pressed: true,
            }),
            InputEvent::Key(KeyEvent {
                key: hid::A,
                pressed: true,
                repeat: false,
                text: Some("A".to_owned()),
            }),
        ];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if rig.injector.injected() == want {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the key chord never reached the injector"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The keyboard escape (both Control keys, ADR 0008): while
    /// controlling, the platform sets the escape flag; the driver polls
    /// it and hands control back — the only way out once every key is
    /// being captured and the console is unreachable.
    #[tokio::test]
    async fn escape_gesture_hands_control_back() {
        let mut rig = rig();
        make_controlling(&mut rig).await;
        assert!(rig.capture.is_capturing());

        // The user presses the release chord.
        rig.capture.request_escape();

        // The driver polls the escape and hands back: ReleaseAllInput,
        // then ControlRelease, and capture stops.
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected ReleaseAllInput after the escape gesture");
        };
        assert_eq!(message_type, MessageType::ReleaseAllInput.wire());
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected ControlRelease after the escape gesture");
        };
        assert_eq!(message_type, MessageType::ControlRelease.wire());
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::ControlEnded(crate::control::ControlEndReason::HandedBack)
        );
        assert!(!rig.capture.is_capturing(), "escape must stop capture");
    }

    /// feature/87: when a peer controls this machine and its input desktop
    /// switches to one that cannot be injected into (a UAC/secure-desktop
    /// prompt), the driver's health poll gives up the grant — the controller
    /// returns to local — rather than leaving the link wedged with a hidden
    /// cursor.
    #[tokio::test]
    async fn a_secure_desktop_releases_the_controlling_peer() {
        let mut rig = rig();
        // A peer takes control of this machine.
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 1,
                    entry: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);

        // The input desktop switches to a secure one: injection is impossible.
        rig.injector.set_can_inject(false);

        // The next health poll gives up the grant: a ControlRelease to the
        // peer and the distinct notice.
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected a ControlRelease after the desktop switched");
        };
        assert_eq!(message_type, MessageType::ControlRelease.wire());
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerControlLostToDesktop
        );
    }

    /// An edge-driven grant places the cursor on the entry edge (ADR
    /// 0009): the rig is a left member, so control enters on its right
    /// edge, at the crossing fraction of the screen height.
    #[tokio::test]
    async fn an_edge_request_places_the_cursor_on_grant() {
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();

        let position = EdgeFraction::new(0.5);
        let request = ControlRequest {
            request_id: 1,
            entry: Some(entry_point(position)),
        };
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                request.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        // Grant out, then PeerTookControl — after which PlaceCursor has run.
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);

        let placements = rig.injector.placements();
        assert_eq!(placements.len(), 1, "exactly one placement on entry");
        assert_eq!(placements[0].x, 1919, "entered on the right (linked) edge");
        assert!(
            (placements[0].y - 540).abs() <= 1,
            "placed at mid-height, got y={}",
            placements[0].y
        );
    }

    /// Let the edge detector run a few polls. Under `tokio::time::pause`
    /// this advances the clock deterministically rather than waiting.
    async fn edge_polls(count: u32) {
        tokio::time::sleep(EDGE_POLL * count).await;
    }

    /// Bring an edge-detecting rig to "a peer controls this machine",
    /// entered at mid-height so the cursor is placed on the linked column —
    /// exactly where a real transfer leaves it.
    async fn peer_takes_control_across_the_edge(rig: &mut Rig) {
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 1,
                    entry: Some(entry_point(EdgeFraction::new(0.5))),
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(rig).await;
        assert_eq!(next_notice(rig).await, ControlNotice::PeerTookControl);
    }

    /// The hardware bounce this margin exists for (ADR 0009 addendum,
    /// 2026-08-19). Entry parks the cursor **on** the linked column, and the
    /// same column means *return* while the peer drives this machine — so
    /// with a bare one-pixel rising edge, a wobble at the seam fired a
    /// complete reverse transfer, which re-parked both cursors on their
    /// trigger columns and repeated (ten take/revoke cycles in five seconds
    /// on hardware). Here the whole loop is real: the injector's placement
    /// moves the display's cursor and a real detector polls it.
    #[tokio::test]
    async fn a_wobble_on_the_entry_column_does_not_bounce_control_back() {
        tokio::time::pause();
        let mut rig = edge_detecting_rig();
        peer_takes_control_across_the_edge(&mut rig).await;
        // The placement really moved the cursor onto the linked column.
        assert_eq!(
            rig.display.cursor_position().unwrap(),
            CursorPoint { x: 1919, y: 540 }
        );
        // Let the detector adopt Returning mode and prime on that cursor.
        edge_polls(4).await;

        // The seam wobble: a pixel off the column and back, twice — what a
        // hand resting against the edge produces at 125 Hz polling.
        for _ in 0..2 {
            rig.display.set_cursor(CursorPoint { x: 1918, y: 540 });
            edge_polls(4).await;
            rig.display.set_cursor(CursorPoint { x: 1919, y: 540 });
            edge_polls(4).await;
        }
        assert!(
            timeout(Duration::from_millis(500), rig.notices.recv())
                .await
                .is_err(),
            "a wobble at the seam revoked the peer's control"
        );

        // Deliberate travel back into the screen re-arms, and the next
        // touch of the column returns control as it should.
        let clear = 1919 - i32::try_from(REARM_MARGIN).unwrap() - 1;
        rig.display.set_cursor(CursorPoint { x: clear, y: 540 });
        edge_polls(4).await;
        rig.display.set_cursor(CursorPoint { x: 1919, y: 540 });
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerControlRevoked
        );
    }

    /// A *refreshed* grant — a re-request from the session that already
    /// holds control, so a late answer converges rather than deadlocking
    /// (ADR 0009 addendum, 2026-08-19) — places the cursor on the linked
    /// column exactly as a first grant does. But it does not change
    /// `is_controlled`, so the edge mode does not change either, and
    /// nothing used to be republished. With the trigger armed (the user had
    /// moved clear of the column in the meantime) the placement itself then
    /// read as an arrival: the refresh revoked the grant it had just
    /// re-issued.
    #[tokio::test]
    async fn a_refreshed_grant_re_primes_the_detector_instead_of_returning() {
        tokio::time::pause();
        let mut rig = edge_detecting_rig();
        peer_takes_control_across_the_edge(&mut rig).await;
        let before = generation_at(&mut rig, EdgeMode::Returning).await;
        edge_polls(4).await; // the detector adopts Returning and primes

        // The user's cursor travels well clear of the linked column, which
        // is what arms the trigger — without this the placement could not
        // fire whatever the mode did, and the test would prove nothing.
        let clear = 1919 - i32::try_from(REARM_MARGIN).unwrap() - 1;
        rig.display.set_cursor(CursorPoint { x: clear, y: 540 });
        edge_polls(4).await;

        // The grant holder asks again (its own answer came too late), so
        // the engine refreshes the grant and places the cursor back on the
        // entry column.
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 2,
                    entry: Some(entry_point(EdgeFraction::new(0.5))),
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected a response to the refreshing request");
        };
        assert_eq!(message_type, MessageType::ControlResponse.wire());
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);
        assert_eq!(
            rig.display.cursor_position().unwrap(),
            CursorPoint { x: 1919, y: 540 },
            "the refresh did not place the cursor on the entry column"
        );

        // The refresh's own placement must fire nothing.
        edge_polls(8).await;
        assert!(
            timeout(Duration::from_millis(500), rig.notices.recv())
                .await
                .is_err(),
            "the refresh's cursor placement revoked the grant it had just re-issued"
        );

        // What makes that true: the (unchanged) mode is republished under a
        // new generation, which re-primes the detector on the placed cursor.
        let after = generation_at(&mut rig, EdgeMode::Returning).await;
        assert!(
            after > before,
            "a refreshed grant left the detector primed for the state before it"
        );

        // And a crossing detected before the refresh — in flight while it
        // happened — is stale, so it cannot revoke the refreshed grant.
        rig.events
            .send(InputControlEvent::EdgeReturn {
                position: EdgeFraction::new(0.5),
                generation: before,
            })
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(500), rig.commands.recv())
                .await
                .is_err(),
            "a crossing from before the refresh revoked the refreshed grant"
        );
    }

    /// A crossing carries a `kind` frozen at detection time through two
    /// bounded queues. If the control state changed on the way — the grant
    /// it was detected under ended, and a new one began — acting on it would
    /// revoke the *fresh* grant. The mode generation makes that impossible.
    #[tokio::test]
    async fn a_crossing_detected_under_a_superseded_mode_is_dropped() {
        tokio::time::pause();
        let mut rig = rig();
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        let before = generation_at(&mut rig, EdgeMode::Leaving).await;
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 1,
                    entry: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_notice(&mut rig).await, ControlNotice::PeerTookControl);
        let current = generation_at(&mut rig, EdgeMode::Returning).await;
        assert!(current > before, "the grant must advance the generation");

        // A Return detected back under the pre-grant generation arrives
        // late: it says nothing about the grant that exists now, so it is
        // dropped.
        rig.events
            .send(InputControlEvent::EdgeReturn {
                position: EdgeFraction::new(0.5),
                generation: before,
            })
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(500), rig.commands.recv())
                .await
                .is_err(),
            "a stale crossing revoked the current grant"
        );

        // The same crossing under the current generation does revoke.
        rig.events
            .send(InputControlEvent::EdgeReturn {
                position: EdgeFraction::new(0.5),
                generation: current,
            })
            .await
            .unwrap();
        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected a ControlRelease for the edge return");
        };
        assert_eq!(message_type, MessageType::ControlRelease.wire());
        assert_eq!(
            next_notice(&mut rig).await,
            ControlNotice::PeerControlRevoked
        );
    }

    /// The edge detector's mode follows the control state (ADR 0009): idle
    /// with no session, watching to leave while local, to return while
    /// controlled, idle again when the session drops.
    #[tokio::test]
    async fn the_edge_mode_follows_the_control_state() {
        let mut rig = rig();
        // A session appears: now there is somewhere to cross to.
        rig.events
            .send(InputControlEvent::SessionEstablished { session: SESSION })
            .await
            .unwrap();
        assert_eq!(next_edge_mode(&mut rig).await, EdgeMode::Leaving);

        // The peer takes control of this machine: watch to return.
        rig.events
            .send(frame(
                MessageType::ControlRequest,
                ControlRequest {
                    request_id: 1,
                    entry: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();
        let _grant = next_command(&mut rig).await;
        assert_eq!(next_edge_mode(&mut rig).await, EdgeMode::Returning);

        // The peer releases: back to watching to leave.
        rig.events
            .send(frame(
                MessageType::ControlRelease,
                ControlRelease { entry: None }.encode_payload().unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(next_edge_mode(&mut rig).await, EdgeMode::Leaving);

        // The session drops: nothing to cross to, so idle.
        rig.events
            .send(InputControlEvent::SessionLost { session: SESSION })
            .await
            .unwrap();
        assert_eq!(next_edge_mode(&mut rig).await, EdgeMode::Idle);
    }
}
