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

use tokio::sync::mpsc;
use uuid::Uuid;

use crossover_platform::{InputCapture, InputInjector};
use crossover_protocol::RawFrame;

use crate::clipboard_driver::{FrameTarget, SessionCommand};
use crate::control::{
    ControlAction, ControlConfig, ControlEngine, ControlEvent, ControlNotice, InboundControl,
};
use crate::input::InputEvent;

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
pub struct InputControlDriver {
    engine: ControlEngine,
    capture: Arc<dyn InputCapture>,
    injector: Arc<dyn InputInjector>,
    events_rx: mpsc::Receiver<InputControlEvent>,
    events_tx: mpsc::Sender<InputControlEvent>,
    commands_tx: mpsc::Sender<SessionCommand>,
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
    config: ControlConfig,
) -> (
    InputControlDriver,
    mpsc::Sender<InputControlEvent>,
    mpsc::Receiver<SessionCommand>,
    mpsc::Receiver<ControlNotice>,
) {
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let (commands_tx, commands_rx) = mpsc::channel(64);
    let (notices_tx, notices_rx) = mpsc::channel(64);

    let driver = InputControlDriver {
        engine: ControlEngine::new(config),
        capture,
        injector,
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
                    if self.engine.is_controlling() {
                        // The platform watchdog reports loss through
                        // is_capturing (R-2); this poll turns it into the
                        // engine's fail-closed transition.
                        if !self.capture.is_capturing() {
                            let actions = self.engine.handle(ControlEvent::CaptureLost);
                            if !self.execute(actions).await {
                                return;
                            }
                        } else if self.capture.escape_requested() {
                            // The user pressed the release escape gesture
                            // (both Control keys); hand control back — the
                            // only way out while the keyboard is captured
                            // and the console is unreachable (ADR 0008).
                            let actions = self.engine.handle(ControlEvent::UserRelease);
                            if !self.execute(actions).await {
                                return;
                            }
                        }
                    }
                }
            }
        }
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
            let actions = self.engine.handle(engine_event);
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
                        deferred.extend(self.engine.handle(ControlEvent::CaptureLost));
                    }
                }
                ControlAction::StopCapture => {
                    let capture = Arc::clone(&self.capture);
                    let result = tokio::task::spawn_blocking(move || capture.stop_capture()).await;
                    if let Ok(Err(error)) = result {
                        // Lenient by trait contract: error paths call
                        // stop exactly when it must not matter.
                        tracing::warn!(error = %error, "stop_capture reported failure");
                    }
                }
                ControlAction::Inject(events) => {
                    if let Err(error) = self.injector.inject(&events) {
                        // Nothing to retry into (UIPI and friends, R-1);
                        // observable, not silent (NFR-3).
                        tracing::warn!(error = %error, "input injection failed");
                    }
                }
                ControlAction::PlaceCursor(position) => {
                    // Cursor placement on seamless entry/return (ADR 0009)
                    // maps the fraction through the local display geometry
                    // and injects an absolute move. The seam is explicit
                    // here; the actuation (topology + display + absolute
                    // injection) is wired in the app layer.
                    tracing::debug!(
                        position = position.value(),
                        "place cursor on the entry edge"
                    );
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
            // another StartCapture.
            return Box::pin(self.execute(deferred)).await;
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crossover_platform::fakes::{FakeInputCapture, FakeInputInjector};
    use crossover_platform::{InputCapture, InputInjector};
    use crossover_protocol::hello::MessageType;
    use crossover_protocol::{
        ControlRequest, ControlResponse, ControlVerdict, DenyReason, InputBatch, RawFrame,
        ReleaseAllInput, WireButton, WireInputEvent,
    };

    use super::{InputControlEvent, input_control};
    use crate::clipboard_driver::{FrameTarget, SessionCommand};
    use crate::control::{ControlConfig, ControlNotice};
    use crate::input::{InputEvent, KeyEvent, PointerButton, PointerEvent, hid};

    /// The session the single-session tests operate on.
    const SESSION: Uuid = Uuid::from_bytes([0xA1; 16]);
    /// A distinct concurrent session, for the cross-session mediation tests.
    const OTHER_SESSION: Uuid = Uuid::from_bytes([0xB2; 16]);

    struct Rig {
        capture: Arc<FakeInputCapture>,
        injector: Arc<FakeInputInjector>,
        events: mpsc::Sender<InputControlEvent>,
        commands: mpsc::Receiver<SessionCommand>,
        notices: mpsc::Receiver<ControlNotice>,
    }

    fn rig() -> Rig {
        let capture = Arc::new(FakeInputCapture::new());
        let injector = Arc::new(FakeInputInjector::new());
        let (driver, events, commands, notices) = input_control(
            Arc::clone(&capture) as Arc<dyn InputCapture>,
            Arc::clone(&injector) as Arc<dyn InputInjector>,
            ControlConfig {
                request_timeout: Duration::from_millis(100),
            },
        );
        tokio::spawn(driver.run());
        Rig {
            capture,
            injector,
            events,
            commands,
            notices,
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
}
