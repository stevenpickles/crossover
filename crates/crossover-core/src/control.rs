//! The control-transfer state machine (docs/ARCHITECTURE.md §5.1;
//! FR-4.x, FR-5.1, FR-5.3).
//!
//! Pure: no I/O, no clocks, no channels. Events go in, actions come out,
//! and every transition is a deterministic function of (state, event) —
//! which is what makes the stuck-button invariant *provable* here rather
//! than hoped-for in integration (docs/TESTING.md §1).
//!
//! One engine instance serves both roles at once:
//!
//! - **Controller** (this machine drives the peer): negotiates the grant
//!   (request → acknowledge → switch, FR-5.3), captures only *after* the
//!   peer says yes, coalesces and sequences outbound batches, and tracks
//!   what it believes is held down on the peer (FR-4.3).
//! - **Controlled** (the peer drives this machine): grants or denies
//!   requests deterministically, injects granted input, tracks what it
//!   has applied, and releases all of it on hand-back, revocation, or
//!   disconnect (FR-4.4) — the destination executes its *own* belief,
//!   never a list the departed peer might have gotten wrong.
//!
//! Fail-closed rules (FR-2.3): an `InputBatch` without a grant, or with
//! a non-increasing sequence, terminates the session. Authenticated does
//! not mean entitled to inject.

use std::time::Duration;

use crossover_protocol::hello::MessageType;
use crossover_protocol::input::MAX_INPUT_BATCH_EVENTS;
use crossover_protocol::{
    ControlRelease, ControlRequest, ControlResponse, ControlVerdict, DenyReason, InputBatch,
    ProtocolError, ReleaseAllInput, WireButton, WireInputEvent,
};

use crate::input::{InputState, PointerButton, PointerEvent, coalesce};

/// Tuning for the negotiation.
#[derive(Debug, Clone)]
pub struct ControlConfig {
    /// How long a request may wait for its response before the requester
    /// reverts to local control. Bounded like everything else (NFR-1).
    pub request_timeout: Duration,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(3),
        }
    }
}

/// This machine's control of the *peer* — the outbound axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outbound {
    /// Input acts locally.
    Local,
    /// A request is in flight; nothing is captured yet.
    Requesting {
        /// The id awaiting its response.
        request_id: u64,
    },
    /// The peer granted control: local input is captured and forwarded.
    Remote,
}

/// Everything that can happen to the engine.
#[derive(Debug, Clone)]
pub enum ControlEvent {
    /// The user asked to take control of the peer.
    UserRequestControl,
    /// The user asked to end whichever control relationship exists:
    /// hand back control they hold, cancel a pending request, or revoke
    /// the peer's grant over this machine (the escape hatch).
    UserRelease,
    /// A session to the peer reached `ESTABLISHED`.
    SessionEstablished,
    /// The session ended, for any reason.
    SessionLost,
    /// Locally captured pointer events (already suppressed locally).
    Captured(Vec<PointerEvent>),
    /// Capture reported unhealthy (`is_capturing` false while `REMOTE`) —
    /// the Windows watchdog detected silent hook loss (R-2).
    CaptureLost,
    /// The request timeout scheduled for `request_id` came due.
    RequestTimeout {
        /// Which request the timer belonged to.
        request_id: u64,
    },
    /// A decoded control or input message from the peer.
    Peer(InboundControl),
}

/// Control and input messages from the peer, decoded and validated.
#[derive(Debug, Clone)]
pub enum InboundControl {
    /// Peer asks to control this machine.
    Request(ControlRequest),
    /// Peer answered our request.
    Response(ControlResponse),
    /// Peer ended the control relationship.
    Release,
    /// Input to replay here.
    Batch(InputBatch),
    /// Release everything this machine believes it has applied.
    ReleaseAll(ReleaseAllInput),
}

impl InboundControl {
    /// Decode a frame if it is control or input traffic; `Ok(None)` for
    /// other message types.
    ///
    /// # Errors
    ///
    /// [`ProtocolError`] for a control/input frame whose payload fails
    /// validation — the caller terminates the session (fail closed).
    pub fn decode(message_type: u16, payload: &[u8]) -> Result<Option<Self>, ProtocolError> {
        match MessageType::from_wire(message_type) {
            Some(MessageType::ControlRequest) => Ok(Some(Self::Request(
                ControlRequest::decode_payload(payload)?,
            ))),
            Some(MessageType::ControlResponse) => Ok(Some(Self::Response(
                ControlResponse::decode_payload(payload)?,
            ))),
            Some(MessageType::ControlRelease) => {
                ControlRelease::decode_payload(payload)?;
                Ok(Some(Self::Release))
            }
            Some(MessageType::InputBatch) => {
                Ok(Some(Self::Batch(InputBatch::decode_payload(payload)?)))
            }
            Some(MessageType::ReleaseAllInput) => Ok(Some(Self::ReleaseAll(
                ReleaseAllInput::decode_payload(payload)?,
            ))),
            _ => Ok(None),
        }
    }
}

/// Outbound messages the engine asks the driver to send.
#[derive(Debug, PartialEq, Eq)]
pub enum OutboundControl {
    /// Ask for control.
    Request(ControlRequest),
    /// Answer a request.
    Response(ControlResponse),
    /// End the relationship.
    Release,
    /// A batch of captured input.
    Batch(InputBatch),
    /// Tell the peer to release everything it applied for us.
    ReleaseAll(ReleaseAllInput),
}

impl OutboundControl {
    /// Encode into a frame's (type, payload).
    ///
    /// # Errors
    ///
    /// [`ProtocolError`] if encoding fails (a local fault; engine-built
    /// messages are valid by construction).
    pub fn encode(&self) -> Result<(u16, Vec<u8>), ProtocolError> {
        match self {
            Self::Request(m) => Ok((MessageType::ControlRequest.wire(), m.encode_payload()?)),
            Self::Response(m) => Ok((MessageType::ControlResponse.wire(), m.encode_payload()?)),
            Self::Release => Ok((
                MessageType::ControlRelease.wire(),
                ControlRelease {}.encode_payload()?,
            )),
            Self::Batch(m) => Ok((MessageType::InputBatch.wire(), m.encode_payload()?)),
            Self::ReleaseAll(m) => Ok((MessageType::ReleaseAllInput.wire(), m.encode_payload()?)),
        }
    }
}

/// Why this machine's control of the peer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEndReason {
    /// The user handed control back.
    HandedBack,
    /// The user cancelled a pending request.
    Cancelled,
    /// The peer revoked the grant.
    Revoked,
    /// The session ended.
    Disconnected,
    /// Local capture was lost (R-2) and the transfer failed closed.
    CaptureLost,
}

/// Why a request never went out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestBlocked {
    /// No session exists.
    NoSession,
    /// The peer currently controls this machine; release first.
    PeerHoldsControl,
    /// Control is already held.
    AlreadyControlling,
    /// A request is already in flight.
    RequestPending,
}

/// Human-facing state changes; the application decides presentation.
/// Every failed or ended transfer surfaces here (NFR-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlNotice {
    /// A request went to the peer.
    RequestSent,
    /// A request could not be made.
    RequestBlocked(RequestBlocked),
    /// The peer denied the request.
    RequestDenied(DenyReason),
    /// The peer never answered inside the timeout.
    RequestTimedOut,
    /// This machine now controls the peer.
    ControlGained,
    /// This machine's control of the peer ended.
    ControlEnded(ControlEndReason),
    /// The peer now controls this machine.
    PeerTookControl,
    /// The peer handed control of this machine back.
    PeerReleasedControl,
    /// The user revoked the peer's control of this machine.
    PeerControlRevoked,
    /// The peer's control of this machine ended with the session; its
    /// input was released locally (FR-4.4).
    PeerControlLostOnDisconnect,
}

/// What the engine asks the driver to do. Order within the returned
/// `Vec` is the required execution order.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlAction {
    /// Send this message to the peer.
    Send(OutboundControl),
    /// Begin capturing (and suppressing) local pointer input.
    StartCapture,
    /// Stop capturing; local input acts locally again.
    StopCapture,
    /// Inject these events into this machine.
    Inject(Vec<PointerEvent>),
    /// Arrange a [`ControlEvent::RequestTimeout`] after `delay`.
    ScheduleRequestTimeout {
        /// Which request the timer guards.
        request_id: u64,
        /// The configured wait.
        delay: Duration,
    },
    /// The peer violated the protocol: terminate the session (FR-2.3).
    Terminate {
        /// Diagnostic for logs.
        reason: String,
    },
    /// Tell the user something changed.
    Notify(ControlNotice),
}

/// The control-transfer engine. See the module docs for the two roles.
#[derive(Debug)]
pub struct ControlEngine {
    config: ControlConfig,
    session_up: bool,
    outbound: Outbound,
    /// True while the peer holds a grant over this machine.
    peer_controls: bool,
    next_request_id: u64,
    /// Last sequence sent while controlling (reset per grant).
    send_sequence: u64,
    /// What this machine believes is held down on the peer (FR-4.3).
    sent_state: InputState,
    /// Last sequence applied while controlled (reset per grant).
    applied_sequence: u64,
    /// What this machine has applied for the peer (FR-4.3).
    applied_state: InputState,
}

impl ControlEngine {
    /// A fresh engine: local control, no session.
    #[must_use]
    pub fn new(config: ControlConfig) -> Self {
        Self {
            config,
            session_up: false,
            outbound: Outbound::Local,
            peer_controls: false,
            next_request_id: 0,
            send_sequence: 0,
            sent_state: InputState::new(),
            applied_sequence: 0,
            applied_state: InputState::new(),
        }
    }

    /// Is this machine controlling the peer (capture should be active)?
    #[must_use]
    pub fn is_controlling(&self) -> bool {
        self.outbound == Outbound::Remote
    }

    /// Is the peer controlling this machine?
    #[must_use]
    pub fn is_controlled(&self) -> bool {
        self.peer_controls
    }

    /// Process one event. The returned actions must be executed in
    /// order.
    pub fn handle(&mut self, event: ControlEvent) -> Vec<ControlAction> {
        match event {
            ControlEvent::UserRequestControl => self.on_user_request(),
            ControlEvent::UserRelease => self.on_user_release(),
            ControlEvent::SessionEstablished => {
                self.session_up = true;
                Vec::new()
            }
            ControlEvent::SessionLost => self.on_session_lost(),
            ControlEvent::Captured(events) => self.on_captured(&events),
            ControlEvent::CaptureLost => self.on_capture_lost(),
            ControlEvent::RequestTimeout { request_id } => self.on_request_timeout(request_id),
            ControlEvent::Peer(message) => self.on_peer(message),
        }
    }

    fn on_user_request(&mut self) -> Vec<ControlAction> {
        let blocked = if !self.session_up {
            Some(RequestBlocked::NoSession)
        } else if self.peer_controls {
            Some(RequestBlocked::PeerHoldsControl)
        } else {
            match self.outbound {
                Outbound::Remote => Some(RequestBlocked::AlreadyControlling),
                Outbound::Requesting { .. } => Some(RequestBlocked::RequestPending),
                Outbound::Local => None,
            }
        };
        if let Some(reason) = blocked {
            return vec![ControlAction::Notify(ControlNotice::RequestBlocked(reason))];
        }

        self.next_request_id += 1;
        let request_id = self.next_request_id;
        self.outbound = Outbound::Requesting { request_id };
        vec![
            ControlAction::Send(OutboundControl::Request(ControlRequest { request_id })),
            ControlAction::ScheduleRequestTimeout {
                request_id,
                delay: self.config.request_timeout,
            },
            ControlAction::Notify(ControlNotice::RequestSent),
        ]
    }

    fn on_user_release(&mut self) -> Vec<ControlAction> {
        match self.outbound {
            // Hand control back. StopCapture leads so no freshly captured
            // event can chase the release messages; ReleaseAllInput goes
            // before ControlRelease so TCP delivers them in that order.
            Outbound::Remote => {
                self.outbound = Outbound::Local;
                self.sent_state = InputState::new();
                vec![
                    ControlAction::StopCapture,
                    ControlAction::Send(OutboundControl::ReleaseAll(ReleaseAllInput {
                        after_sequence: self.send_sequence,
                    })),
                    ControlAction::Send(OutboundControl::Release),
                    ControlAction::Notify(ControlNotice::ControlEnded(
                        ControlEndReason::HandedBack,
                    )),
                ]
            }
            // Cancel the pending request. If a grant is already in
            // flight, the response handler will send the undo release.
            Outbound::Requesting { .. } => {
                self.outbound = Outbound::Local;
                vec![ControlAction::Notify(ControlNotice::ControlEnded(
                    ControlEndReason::Cancelled,
                ))]
            }
            Outbound::Local if self.peer_controls => {
                // Revoke the peer's grant: the local user's escape hatch.
                self.peer_controls = false;
                let releases = self.applied_state.release_all();
                let mut actions = Vec::new();
                if !releases.is_empty() {
                    actions.push(ControlAction::Inject(releases));
                }
                actions.push(ControlAction::Send(OutboundControl::Release));
                actions.push(ControlAction::Notify(ControlNotice::PeerControlRevoked));
                actions
            }
            Outbound::Local => Vec::new(), // nothing to release; silent
        }
    }

    fn on_session_lost(&mut self) -> Vec<ControlAction> {
        self.session_up = false;
        let mut actions = Vec::new();

        match self.outbound {
            Outbound::Remote => {
                // The peer releases its own belief on its side of the
                // loss (FR-4.4); nothing can be sent on a dead session.
                self.outbound = Outbound::Local;
                self.sent_state = InputState::new();
                actions.push(ControlAction::StopCapture);
                actions.push(ControlAction::Notify(ControlNotice::ControlEnded(
                    ControlEndReason::Disconnected,
                )));
            }
            Outbound::Requesting { .. } => {
                self.outbound = Outbound::Local;
                actions.push(ControlAction::Notify(ControlNotice::ControlEnded(
                    ControlEndReason::Disconnected,
                )));
            }
            Outbound::Local => {}
        }

        if self.peer_controls {
            // FR-4.4, the path the spec calls release-blocking: the
            // destination synthesizes releases for everything it
            // believes is pressed, from its own records.
            self.peer_controls = false;
            let releases = self.applied_state.release_all();
            if !releases.is_empty() {
                actions.push(ControlAction::Inject(releases));
            }
            actions.push(ControlAction::Notify(
                ControlNotice::PeerControlLostOnDisconnect,
            ));
        }
        actions
    }

    fn on_captured(&mut self, events: &[PointerEvent]) -> Vec<ControlAction> {
        // Anything captured outside REMOTE is a stray tail between the
        // stop decision and the platform actually stopping: drop it, or
        // it would act on a peer that no longer expects input.
        if self.outbound != Outbound::Remote {
            return Vec::new();
        }
        let merged = coalesce(events);
        if merged.is_empty() {
            return Vec::new();
        }
        let mut actions = Vec::new();
        for chunk in merged.chunks(MAX_INPUT_BATCH_EVENTS) {
            self.send_sequence += 1;
            self.sent_state.apply_all(chunk);
            actions.push(ControlAction::Send(OutboundControl::Batch(InputBatch {
                sequence: self.send_sequence,
                events: chunk.iter().copied().map(to_wire).collect(),
            })));
        }
        actions
    }

    fn on_capture_lost(&mut self) -> Vec<ControlAction> {
        if self.outbound != Outbound::Remote {
            return Vec::new();
        }
        // Fail closed (ADR 0007): suppression is gone, so control is
        // over. The peer releases what we made it hold; the local
        // StopCapture is belt-and-braces on an already-dead capture.
        self.outbound = Outbound::Local;
        self.sent_state = InputState::new();
        vec![
            ControlAction::StopCapture,
            ControlAction::Send(OutboundControl::ReleaseAll(ReleaseAllInput {
                after_sequence: self.send_sequence,
            })),
            ControlAction::Send(OutboundControl::Release),
            ControlAction::Notify(ControlNotice::ControlEnded(ControlEndReason::CaptureLost)),
        ]
    }

    fn on_request_timeout(&mut self, request_id: u64) -> Vec<ControlAction> {
        // Only the timer for the request still in flight matters; a
        // stale timer (answered or superseded request) is a no-op.
        if self.outbound != (Outbound::Requesting { request_id }) {
            return Vec::new();
        }
        self.outbound = Outbound::Local;
        vec![ControlAction::Notify(ControlNotice::RequestTimedOut)]
    }

    fn on_peer(&mut self, message: InboundControl) -> Vec<ControlAction> {
        match message {
            InboundControl::Request(request) => self.on_peer_request(request),
            InboundControl::Response(response) => self.on_peer_response(response),
            InboundControl::Release => self.on_peer_release(),
            InboundControl::Batch(batch) => self.on_peer_batch(&batch),
            InboundControl::ReleaseAll(_) => {
                // Release this machine's own belief (FR-4.4): more robust
                // than any list the peer could send. Harmless when
                // nothing is held.
                let releases = self.applied_state.release_all();
                if releases.is_empty() {
                    Vec::new()
                } else {
                    vec![ControlAction::Inject(releases)]
                }
            }
        }
    }

    fn on_peer_request(&mut self, request: ControlRequest) -> Vec<ControlAction> {
        let deny = |reason| {
            vec![ControlAction::Send(OutboundControl::Response(
                ControlResponse {
                    request_id: request.request_id,
                    verdict: ControlVerdict::Denied(reason),
                },
            ))]
        };
        if self.peer_controls {
            return deny(DenyReason::AlreadyControlled);
        }
        match self.outbound {
            // Controlling or requesting: busy. Two simultaneous requests
            // thus produce two denials — deterministic (FR-5.1).
            Outbound::Remote | Outbound::Requesting { .. } => deny(DenyReason::Busy),
            Outbound::Local => {
                self.peer_controls = true;
                self.applied_sequence = 0;
                self.applied_state = InputState::new();
                vec![
                    ControlAction::Send(OutboundControl::Response(ControlResponse {
                        request_id: request.request_id,
                        verdict: ControlVerdict::Granted,
                    })),
                    ControlAction::Notify(ControlNotice::PeerTookControl),
                ]
            }
        }
    }

    fn on_peer_response(&mut self, response: ControlResponse) -> Vec<ControlAction> {
        let Outbound::Requesting { request_id } = self.outbound else {
            // Not waiting: if this is a grant, the peer now believes it
            // is controlled by us (our request timed out or was
            // cancelled meanwhile). Undo it explicitly, or it would sit
            // granted-but-driverless forever.
            if response.verdict == ControlVerdict::Granted {
                return vec![ControlAction::Send(OutboundControl::Release)];
            }
            return Vec::new();
        };
        if response.request_id != request_id {
            // An answer to an older request. A stale grant still needs
            // the undo; a stale denial is just late news.
            if response.verdict == ControlVerdict::Granted {
                return vec![ControlAction::Send(OutboundControl::Release)];
            }
            return Vec::new();
        }
        match response.verdict {
            ControlVerdict::Granted => {
                self.outbound = Outbound::Remote;
                self.send_sequence = 0;
                self.sent_state = InputState::new();
                vec![
                    ControlAction::StartCapture,
                    ControlAction::Notify(ControlNotice::ControlGained),
                ]
            }
            ControlVerdict::Denied(reason) => {
                self.outbound = Outbound::Local;
                vec![ControlAction::Notify(ControlNotice::RequestDenied(reason))]
            }
        }
    }

    fn on_peer_release(&mut self) -> Vec<ControlAction> {
        if self.peer_controls {
            // The controller handed back. Release everything it left
            // held — normally its ReleaseAllInput already did, but this
            // engine's belief is the authority and releasing twice is
            // harmless where a stuck button is not (FR-4.4).
            self.peer_controls = false;
            let releases = self.applied_state.release_all();
            let mut actions = Vec::new();
            if !releases.is_empty() {
                actions.push(ControlAction::Inject(releases));
            }
            actions.push(ControlAction::Notify(ControlNotice::PeerReleasedControl));
            return actions;
        }
        match self.outbound {
            // The controlled machine revoked our grant (its user's
            // escape hatch): stop capturing immediately. It releases its
            // own applied state.
            Outbound::Remote => {
                self.outbound = Outbound::Local;
                self.sent_state = InputState::new();
                vec![
                    ControlAction::StopCapture,
                    ControlAction::Notify(ControlNotice::ControlEnded(ControlEndReason::Revoked)),
                ]
            }
            // A release with no relationship: the cleanup path for a
            // grant we un-did, or a duplicate. Nothing to do.
            Outbound::Requesting { .. } | Outbound::Local => Vec::new(),
        }
    }

    fn on_peer_batch(&mut self, batch: &InputBatch) -> Vec<ControlAction> {
        // Fail closed (FR-2.3): input without a grant is a violation,
        // not a race — grants travel on the same ordered stream as
        // batches, so an honest peer cannot interleave them wrongly.
        if !self.peer_controls {
            return vec![ControlAction::Terminate {
                reason: "input batch received without a control grant".to_owned(),
            }];
        }
        // TCP+TLS delivers what was sent, in order; a regression or
        // duplicate cannot be innocent (docs/PROTOCOL.md §6).
        if batch.sequence <= self.applied_sequence {
            return vec![ControlAction::Terminate {
                reason: format!(
                    "input batch sequence {} not after {}",
                    batch.sequence, self.applied_sequence
                ),
            }];
        }
        self.applied_sequence = batch.sequence;
        let events: Vec<PointerEvent> = batch.events.iter().copied().map(from_wire).collect();
        self.applied_state.apply_all(&events);
        vec![ControlAction::Inject(events)]
    }
}

/// Platform event → wire event. Total: every capturable event travels.
fn to_wire(event: PointerEvent) -> WireInputEvent {
    match event {
        PointerEvent::Motion { dx, dy } => WireInputEvent::Motion { dx, dy },
        PointerEvent::Button { button, pressed } => WireInputEvent::Button {
            button: button_to_wire(button),
            pressed,
        },
        PointerEvent::Scroll { dx, dy } => WireInputEvent::Scroll { dx, dy },
    }
}

/// Wire event → platform event. Total: every valid wire event injects.
fn from_wire(event: WireInputEvent) -> PointerEvent {
    match event {
        WireInputEvent::Motion { dx, dy } => PointerEvent::Motion { dx, dy },
        WireInputEvent::Button { button, pressed } => PointerEvent::Button {
            button: button_from_wire(button),
            pressed,
        },
        WireInputEvent::Scroll { dx, dy } => PointerEvent::Scroll { dx, dy },
    }
}

fn button_to_wire(button: PointerButton) -> WireButton {
    match button {
        PointerButton::Left => WireButton::Left,
        PointerButton::Right => WireButton::Right,
        PointerButton::Middle => WireButton::Middle,
        PointerButton::X1 => WireButton::X1,
        PointerButton::X2 => WireButton::X2,
    }
}

fn button_from_wire(button: WireButton) -> PointerButton {
    match button {
        WireButton::Left => PointerButton::Left,
        WireButton::Right => PointerButton::Right,
        WireButton::Middle => PointerButton::Middle,
        WireButton::X1 => PointerButton::X1,
        WireButton::X2 => PointerButton::X2,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crossover_protocol::{
        ControlRequest, ControlResponse, ControlVerdict, DenyReason, InputBatch, ReleaseAllInput,
        WireButton, WireInputEvent,
    };

    use super::{
        ControlAction, ControlConfig, ControlEndReason, ControlEngine, ControlEvent, ControlNotice,
        InboundControl, OutboundControl, RequestBlocked,
    };
    use crate::input::{InputState, PointerButton, PointerEvent};

    fn engine() -> ControlEngine {
        let mut engine = ControlEngine::new(ControlConfig::default());
        let actions = engine.handle(ControlEvent::SessionEstablished);
        assert!(actions.is_empty());
        engine
    }

    /// Drive the engine to REMOTE: request, then grant.
    fn controlling_engine() -> ControlEngine {
        let mut engine = engine();
        let actions = engine.handle(ControlEvent::UserRequestControl);
        let request_id = sent_request_id(&actions);
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Response(
            ControlResponse {
                request_id,
                verdict: ControlVerdict::Granted,
            },
        )));
        assert!(actions.contains(&ControlAction::StartCapture));
        assert!(engine.is_controlling());
        engine
    }

    /// Drive the engine to being controlled: peer requests, we grant.
    fn controlled_engine() -> ControlEngine {
        let mut engine = engine();
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Request(
            ControlRequest { request_id: 1 },
        )));
        assert!(granted(&actions));
        assert!(engine.is_controlled());
        engine
    }

    fn sent_request_id(actions: &[ControlAction]) -> u64 {
        actions
            .iter()
            .find_map(|action| match action {
                ControlAction::Send(OutboundControl::Request(r)) => Some(r.request_id),
                _ => None,
            })
            .expect("no ControlRequest sent")
    }

    fn granted(actions: &[ControlAction]) -> bool {
        actions.iter().any(|action| {
            matches!(
                action,
                ControlAction::Send(OutboundControl::Response(ControlResponse {
                    verdict: ControlVerdict::Granted,
                    ..
                }))
            )
        })
    }

    fn press(button: PointerButton) -> PointerEvent {
        PointerEvent::Button {
            button,
            pressed: true,
        }
    }

    // ---- negotiation (FR-5.3) ----

    #[test]
    fn request_grant_switch_in_that_order() {
        let mut engine = engine();
        let actions = engine.handle(ControlEvent::UserRequestControl);
        // Request sent and timeout scheduled; nothing captured yet.
        assert!(!actions.contains(&ControlAction::StartCapture));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ControlAction::ScheduleRequestTimeout { .. }))
        );
        let request_id = sent_request_id(&actions);
        assert!(!engine.is_controlling());

        // Capture starts only on the grant — the "switch" step.
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Response(
            ControlResponse {
                request_id,
                verdict: ControlVerdict::Granted,
            },
        )));
        assert_eq!(actions[0], ControlAction::StartCapture);
        assert!(engine.is_controlling());
    }

    #[test]
    fn denial_reverts_to_local_with_the_reason() {
        let mut engine = engine();
        let request_id = sent_request_id(&engine.handle(ControlEvent::UserRequestControl));
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Response(
            ControlResponse {
                request_id,
                verdict: ControlVerdict::Denied(DenyReason::Busy),
            },
        )));
        assert!(
            actions.contains(&ControlAction::Notify(ControlNotice::RequestDenied(
                DenyReason::Busy
            )))
        );
        assert!(!engine.is_controlling());
    }

    #[test]
    fn timeout_reverts_to_local_and_a_late_grant_is_undone() {
        let mut engine = engine();
        let request_id = sent_request_id(&engine.handle(ControlEvent::UserRequestControl));

        let actions = engine.handle(ControlEvent::RequestTimeout { request_id });
        assert!(actions.contains(&ControlAction::Notify(ControlNotice::RequestTimedOut)));
        assert!(!engine.is_controlling());

        // The grant arrives after all. The peer believes it is now
        // controlled; the engine must undo that, not adopt it.
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Response(
            ControlResponse {
                request_id,
                verdict: ControlVerdict::Granted,
            },
        )));
        assert_eq!(
            actions,
            vec![ControlAction::Send(OutboundControl::Release)],
            "a grant nobody is waiting for must be explicitly released"
        );
        assert!(!engine.is_controlling());
    }

    #[test]
    fn stale_timeout_after_grant_changes_nothing() {
        let mut engine = controlling_engine();
        // The timer for the (answered) request fires late.
        let actions = engine.handle(ControlEvent::RequestTimeout { request_id: 1 });
        assert!(actions.is_empty());
        assert!(engine.is_controlling());
    }

    #[test]
    fn requests_are_blocked_with_reasons_not_silently() {
        let mut engine = ControlEngine::new(ControlConfig::default());
        // No session yet.
        assert_eq!(
            engine.handle(ControlEvent::UserRequestControl),
            vec![ControlAction::Notify(ControlNotice::RequestBlocked(
                RequestBlocked::NoSession
            ))]
        );

        let mut engine = controlled_engine();
        assert_eq!(
            engine.handle(ControlEvent::UserRequestControl),
            vec![ControlAction::Notify(ControlNotice::RequestBlocked(
                RequestBlocked::PeerHoldsControl
            ))]
        );

        let mut engine = controlling_engine();
        assert_eq!(
            engine.handle(ControlEvent::UserRequestControl),
            vec![ControlAction::Notify(ControlNotice::RequestBlocked(
                RequestBlocked::AlreadyControlling
            ))]
        );
    }

    #[test]
    fn simultaneous_requests_deny_deterministically() {
        // Our request is in flight when the peer's request arrives.
        let mut engine = engine();
        let _ = engine.handle(ControlEvent::UserRequestControl);
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Request(
            ControlRequest { request_id: 9 },
        )));
        assert_eq!(
            actions,
            vec![ControlAction::Send(OutboundControl::Response(
                ControlResponse {
                    request_id: 9,
                    verdict: ControlVerdict::Denied(DenyReason::Busy),
                }
            ))]
        );
        assert!(!engine.is_controlled());
    }

    #[test]
    fn a_controlled_machine_denies_further_requests() {
        let mut engine = controlled_engine();
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Request(
            ControlRequest { request_id: 2 },
        )));
        assert_eq!(
            actions,
            vec![ControlAction::Send(OutboundControl::Response(
                ControlResponse {
                    request_id: 2,
                    verdict: ControlVerdict::Denied(DenyReason::AlreadyControlled),
                }
            ))]
        );
    }

    // ---- controller: capture, batching, hand-back ----

    #[test]
    fn captured_events_are_coalesced_sequenced_and_tracked() {
        let mut engine = controlling_engine();
        let actions = engine.handle(ControlEvent::Captured(vec![
            PointerEvent::Motion { dx: 3, dy: 0 },
            PointerEvent::Motion { dx: 4, dy: 1 },
            press(PointerButton::Left),
        ]));
        assert_eq!(
            actions,
            vec![ControlAction::Send(OutboundControl::Batch(InputBatch {
                sequence: 1,
                events: vec![
                    WireInputEvent::Motion { dx: 7, dy: 1 },
                    WireInputEvent::Button {
                        button: WireButton::Left,
                        pressed: true,
                    },
                ],
            }))]
        );

        // Sequence advances per batch.
        let actions = engine.handle(ControlEvent::Captured(vec![PointerEvent::Motion {
            dx: 1,
            dy: 1,
        }]));
        assert!(matches!(
            &actions[0],
            ControlAction::Send(OutboundControl::Batch(batch)) if batch.sequence == 2
        ));
    }

    #[test]
    fn captured_events_outside_remote_are_dropped() {
        let mut engine = engine();
        assert!(
            engine
                .handle(ControlEvent::Captured(vec![press(PointerButton::Left)]))
                .is_empty()
        );
    }

    #[test]
    fn oversized_capture_bursts_are_chunked_within_the_wire_bound() {
        use crossover_protocol::input::MAX_INPUT_BATCH_EVENTS;
        let mut engine = controlling_engine();
        // Alternating buttons cannot coalesce, forcing a large batch.
        let events: Vec<PointerEvent> = (0..(MAX_INPUT_BATCH_EVENTS + 3))
            .map(|i| PointerEvent::Button {
                button: PointerButton::Left,
                pressed: i % 2 == 0,
            })
            .collect();
        let actions = engine.handle(ControlEvent::Captured(events));
        assert_eq!(actions.len(), 2, "burst must split into two batches");
        for action in &actions {
            let ControlAction::Send(OutboundControl::Batch(batch)) = action else {
                panic!("expected only batch sends, got {action:?}");
            };
            batch.validate().expect("chunk exceeds the wire bound");
        }
    }

    #[test]
    fn hand_back_stops_capture_then_releases_then_ends() {
        let mut engine = controlling_engine();
        let _ = engine.handle(ControlEvent::Captured(vec![press(PointerButton::Left)]));

        let actions = engine.handle(ControlEvent::UserRelease);
        assert_eq!(
            actions,
            vec![
                ControlAction::StopCapture,
                ControlAction::Send(OutboundControl::ReleaseAll(ReleaseAllInput {
                    after_sequence: 1,
                })),
                ControlAction::Send(OutboundControl::Release),
                ControlAction::Notify(ControlNotice::ControlEnded(ControlEndReason::HandedBack)),
            ]
        );
        assert!(!engine.is_controlling());
    }

    #[test]
    fn capture_loss_fails_closed_and_tells_both_sides() {
        let mut engine = controlling_engine();
        let _ = engine.handle(ControlEvent::Captured(vec![press(PointerButton::Right)]));

        let actions = engine.handle(ControlEvent::CaptureLost);
        assert!(actions.contains(&ControlAction::StopCapture));
        assert!(
            actions.contains(&ControlAction::Send(OutboundControl::ReleaseAll(
                ReleaseAllInput { after_sequence: 1 }
            )))
        );
        assert!(actions.contains(&ControlAction::Send(OutboundControl::Release)));
        assert!(!engine.is_controlling());

        // Not controlling: further loss reports are noise.
        assert!(engine.handle(ControlEvent::CaptureLost).is_empty());
    }

    #[test]
    fn disconnect_while_controlling_stops_capture() {
        let mut engine = controlling_engine();
        let actions = engine.handle(ControlEvent::SessionLost);
        assert!(actions.contains(&ControlAction::StopCapture));
        assert!(
            actions.contains(&ControlAction::Notify(ControlNotice::ControlEnded(
                ControlEndReason::Disconnected
            )))
        );
        assert!(!engine.is_controlling());
    }

    #[test]
    fn revocation_by_the_controlled_machine_stops_capture() {
        let mut engine = controlling_engine();
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Release));
        assert_eq!(actions[0], ControlAction::StopCapture);
        assert!(!engine.is_controlling());
    }

    // ---- controlled: injection, sequencing, release (FR-4.4) ----

    #[test]
    fn granted_input_is_injected_and_tracked() {
        let mut engine = controlled_engine();
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![
                WireInputEvent::Motion { dx: 5, dy: 5 },
                WireInputEvent::Button {
                    button: WireButton::Left,
                    pressed: true,
                },
            ],
        })));
        assert_eq!(
            actions,
            vec![ControlAction::Inject(vec![
                PointerEvent::Motion { dx: 5, dy: 5 },
                press(PointerButton::Left),
            ])]
        );
    }

    #[test]
    fn input_without_a_grant_terminates_the_session() {
        let mut engine = engine();
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Motion { dx: 1, dy: 1 }],
        })));
        assert!(
            matches!(&actions[0], ControlAction::Terminate { .. }),
            "authenticated is not entitled to inject: {actions:?}"
        );
    }

    #[test]
    fn sequence_regression_terminates_the_session() {
        let mut engine = controlled_engine();
        let batch = |sequence| {
            ControlEvent::Peer(InboundControl::Batch(InputBatch {
                sequence,
                events: vec![WireInputEvent::Motion { dx: 1, dy: 1 }],
            }))
        };
        let _ = engine.handle(batch(1));
        let _ = engine.handle(batch(2));
        let actions = engine.handle(batch(2)); // duplicate
        assert!(matches!(&actions[0], ControlAction::Terminate { .. }));
    }

    #[test]
    fn hand_back_releases_everything_the_peer_left_held() {
        let mut engine = controlled_engine();
        let _ = engine.handle(ControlEvent::Peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![
                WireInputEvent::Button {
                    button: WireButton::Left,
                    pressed: true,
                },
                WireInputEvent::Button {
                    button: WireButton::X2,
                    pressed: true,
                },
            ],
        })));

        // The controller's explicit ReleaseAllInput releases the belief…
        let actions = engine.handle(ControlEvent::Peer(InboundControl::ReleaseAll(
            ReleaseAllInput { after_sequence: 1 },
        )));
        assert_eq!(
            actions,
            vec![ControlAction::Inject(vec![
                PointerEvent::Button {
                    button: PointerButton::Left,
                    pressed: false,
                },
                PointerEvent::Button {
                    button: PointerButton::X2,
                    pressed: false,
                },
            ])]
        );

        // …and the following ControlRelease finds nothing left to do.
        let actions = engine.handle(ControlEvent::Peer(InboundControl::Release));
        assert_eq!(
            actions,
            vec![ControlAction::Notify(ControlNotice::PeerReleasedControl)]
        );
        assert!(!engine.is_controlled());
    }

    #[test]
    fn disconnect_mid_drag_releases_locally() {
        // FR-4.4's canonical scenario: the controller vanishes while a
        // button is held. The destination must synthesize the release
        // from its own records.
        let mut engine = controlled_engine();
        let _ = engine.handle(ControlEvent::Peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Left,
                pressed: true,
            }],
        })));

        let actions = engine.handle(ControlEvent::SessionLost);
        assert!(
            actions.contains(&ControlAction::Inject(vec![PointerEvent::Button {
                button: PointerButton::Left,
                pressed: false,
            }]))
        );
        assert!(!engine.is_controlled());
    }

    #[test]
    fn revoking_the_peer_releases_and_notifies_it() {
        let mut engine = controlled_engine();
        let _ = engine.handle(ControlEvent::Peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Middle,
                pressed: true,
            }],
        })));

        let actions = engine.handle(ControlEvent::UserRelease);
        assert_eq!(
            actions,
            vec![
                ControlAction::Inject(vec![PointerEvent::Button {
                    button: PointerButton::Middle,
                    pressed: false,
                }]),
                ControlAction::Send(OutboundControl::Release),
                ControlAction::Notify(ControlNotice::PeerControlRevoked),
            ]
        );
        assert!(!engine.is_controlled());
    }

    // ---- fault injection: the Phase 3 exit criterion ----

    /// Model of the two sides' observable obligations, maintained by
    /// replaying the engine's actions exactly as a driver would.
    #[derive(Default)]
    struct Model {
        capture_active: bool,
        injected: InputState,
    }

    impl Model {
        fn apply(&mut self, actions: &[ControlAction]) {
            for action in actions {
                match action {
                    ControlAction::StartCapture => self.capture_active = true,
                    ControlAction::StopCapture => self.capture_active = false,
                    ControlAction::Inject(events) => self.injected.apply_all(events),
                    _ => {}
                }
            }
        }
    }

    fn arbitrary_event() -> impl Strategy<Value = ControlEvent> {
        prop_oneof![
            Just(ControlEvent::UserRequestControl),
            Just(ControlEvent::UserRelease),
            Just(ControlEvent::SessionEstablished),
            Just(ControlEvent::SessionLost),
            Just(ControlEvent::CaptureLost),
            (1u64..4).prop_map(|request_id| ControlEvent::RequestTimeout { request_id }),
            (1u64..4).prop_map(
                |id| ControlEvent::Peer(InboundControl::Request(ControlRequest { request_id: id }))
            ),
            (1u64..4, any::<bool>()).prop_map(|(id, granted)| {
                ControlEvent::Peer(InboundControl::Response(ControlResponse {
                    request_id: id,
                    verdict: if granted {
                        ControlVerdict::Granted
                    } else {
                        ControlVerdict::Denied(DenyReason::Busy)
                    },
                }))
            }),
            Just(ControlEvent::Peer(InboundControl::Release)),
            (1u64..1000, 0usize..5, any::<bool>()).prop_map(|(sequence, button, pressed)| {
                ControlEvent::Peer(InboundControl::Batch(InputBatch {
                    sequence,
                    events: vec![WireInputEvent::Button {
                        button: [
                            WireButton::Left,
                            WireButton::Right,
                            WireButton::Middle,
                            WireButton::X1,
                            WireButton::X2,
                        ][button],
                        pressed,
                    }],
                }))
            }),
            Just(ControlEvent::Peer(InboundControl::ReleaseAll(
                ReleaseAllInput { after_sequence: 0 }
            ))),
            (0usize..5, any::<bool>()).prop_map(|(button, pressed)| {
                ControlEvent::Captured(vec![PointerEvent::Button {
                    button: PointerButton::ALL[button],
                    pressed,
                }])
            }),
        ]
    }

    proptest! {
        /// The Phase 3 exit criterion, hermetically: whatever interleaving
        /// of user actions, peer messages, grants, input, and faults
        /// occurs, a session loss leaves nothing captured and nothing
        /// held down — no stuck buttons, no dead mouse (FR-4.4, FR-6.1).
        #[test]
        fn any_interleaving_ends_clean_on_disconnect(
            events in proptest::collection::vec(arbitrary_event(), 0..60),
        ) {
            let mut engine = ControlEngine::new(ControlConfig::default());
            let mut model = Model::default();
            let mut terminated = false;
            for event in events {
                let actions = engine.handle(event);
                terminated |= actions
                    .iter()
                    .any(|a| matches!(a, ControlAction::Terminate { .. }));
                model.apply(&actions);
            }
            // The fault: the session dies now. (A Terminate would cause
            // exactly this, so the invariant covers that path too.)
            model.apply(&engine.handle(ControlEvent::SessionLost));

            prop_assert!(!model.capture_active, "capture left running after disconnect");
            prop_assert!(model.injected.is_clear(), "buttons left held after disconnect");
            prop_assert!(!engine.is_controlling());
            prop_assert!(!engine.is_controlled());
            let _ = terminated; // documented: termination is a valid outcome
        }

        /// Capture is active exactly while the engine says REMOTE —
        /// repeated activate/deactivate cycles can never leave a capture
        /// running that the state machine has forgotten about.
        #[test]
        fn capture_activity_always_matches_the_state(
            events in proptest::collection::vec(arbitrary_event(), 0..60),
        ) {
            let mut engine = ControlEngine::new(ControlConfig::default());
            let mut model = Model::default();
            for event in events {
                let actions = engine.handle(event);
                model.apply(&actions);
                prop_assert_eq!(
                    model.capture_active,
                    engine.is_controlling(),
                    "capture activity diverged from engine state"
                );
            }
        }
    }
}
