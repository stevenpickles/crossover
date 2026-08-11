//! The control-transfer state machine (docs/ARCHITECTURE.md §5.1;
//! FR-4.x, FR-5.1, FR-5.3).
//!
//! Pure: no I/O, no clocks, no channels. Events go in, actions come out,
//! and every transition is a deterministic function of (state, event) —
//! which is what makes the stuck-button invariant *provable* here rather
//! than hoped-for in integration (docs/TESTING.md §1).
//!
//! One engine instance serves both roles at once, and — this is the
//! point of the design — it is **session-aware**. Authentication is not
//! authorization: a peer authenticated by TLS is entitled to inject only
//! while it holds a *grant*, and a grant is authority for the one
//! session it was negotiated on and no other (FR-2.3, FR-5.1). So every
//! event carries the session it belongs to, and every authorization
//! decision is checked against the identity of the session that holds
//! the grant — complete mediation on the principal. A second trusted
//! peer cannot ride the first peer's grant.
//!
//! - **Controller** (this machine drives a peer): negotiates the grant
//!   (request → acknowledge → switch, FR-5.3), captures only *after* the
//!   peer says yes, coalesces and sequences outbound batches to that
//!   session, and tracks what it believes is held down on the peer
//!   (FR-4.3). At most one peer is controlled at a time — the single
//!   local mouse can drive only one destination.
//! - **Controlled** (a peer drives this machine): grants or denies
//!   requests deterministically, injects granted input from the *one*
//!   session that holds the grant, tracks what it has applied, and
//!   releases all of it on hand-back, revocation, or disconnect (FR-4.4)
//!   — the destination executes its *own* belief, never a list the
//!   departed peer might have gotten wrong. At most one peer controls
//!   this machine at a time — the single local desktop.
//!
//! Fail-closed rules (FR-2.3): an `InputBatch` from a session that holds
//! no grant, or with a non-increasing sequence, terminates that session.

use std::collections::BTreeSet;
use std::time::Duration;

use crossover_protocol::hello::MessageType;
use crossover_protocol::input::MAX_INPUT_BATCH_EVENTS;
use crossover_protocol::{
    ControlRelease, ControlRequest, ControlResponse, ControlVerdict, DenyReason, InputBatch,
    ProtocolError, ReleaseAllInput, WireButton, WireInputEvent,
};
use uuid::Uuid;

use crate::input::{InputEvent, InputState, KeyEvent, PointerButton, PointerEvent, coalesce_input};
use crate::topology::EdgeFraction;

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

/// This machine's control of a peer — the outbound axis. A singleton
/// (the local mouse drives one destination), but it remembers *which*
/// session it targets so responses, batches, and releases go to the
/// right peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outbound {
    /// Input acts locally.
    Local,
    /// A request is in flight to `session`; nothing is captured yet.
    Requesting {
        /// The session the request went to.
        session: Uuid,
        /// The id awaiting its response.
        request_id: u64,
    },
    /// `session` granted control: local input is captured and forwarded
    /// to it.
    Remote {
        /// The session being controlled.
        session: Uuid,
    },
}

/// A peer's control of this machine — the inbound axis. Present only
/// while a grant is held, and it names the one session that holds it, so
/// input from any other session is unauthorized.
#[derive(Debug)]
struct Controlled {
    /// The session that holds the grant.
    session: Uuid,
    /// Last sequence applied from that session (rejects regressions).
    applied_sequence: u64,
    /// What this machine has applied for that peer (FR-4.3, FR-4.4).
    applied_state: InputState,
}

/// Everything that can happen to the engine. Session-scoped variants
/// carry the session they belong to.
#[derive(Debug, Clone)]
pub enum ControlEvent {
    /// The user asked to take control of a specific peer.
    UserRequestControl {
        /// The session to request control of.
        session: Uuid,
    },
    /// The user asked to end whichever control relationship exists:
    /// hand back control they hold, cancel a pending request, or revoke
    /// a peer's grant over this machine (the escape hatch).
    UserRelease,
    /// The cursor crossed the linked edge while this machine controls
    /// itself: request control of `session`, carrying where the cursor
    /// left so the peer can place its own cursor (ADR 0009). The
    /// edge-driven twin of [`Self::UserRequestControl`].
    EdgeLeave {
        /// The session to request control of.
        session: Uuid,
        /// Where the cursor crossed, as a fraction of the edge.
        position: EdgeFraction,
    },
    /// The cursor returned to the linked edge while a peer controls this
    /// machine: reclaim control, carrying where the cursor left so the
    /// controller's cursor reappears at the matching height (ADR 0009).
    /// The edge-driven twin of [`Self::UserRelease`] in its revoke role.
    EdgeReturn {
        /// Where the cursor crossed, as a fraction of the edge.
        position: EdgeFraction,
    },
    /// A session reached `ESTABLISHED`.
    SessionEstablished {
        /// The session.
        session: Uuid,
    },
    /// A session ended, for any reason.
    SessionLost {
        /// The session.
        session: Uuid,
    },
    /// Locally captured input events (already suppressed locally),
    /// pointer or keyboard, destined for whichever peer this machine
    /// controls.
    Captured(Vec<InputEvent>),
    /// Capture reported unhealthy (`is_capturing` false while `REMOTE`) —
    /// the Windows watchdog detected silent hook loss (R-2).
    CaptureLost,
    /// The input desktop switched to one this machine cannot inject into —
    /// a UAC/secure-desktop or lock-screen prompt. A peer can no longer
    /// drive this machine, so any grant it holds is given up rather than
    /// left wedged pretending to be driven (feature/87). Controlled side.
    InputDesktopUnavailable,
    /// The request timeout scheduled for a session's request came due.
    RequestTimeout {
        /// The session the request went to.
        session: Uuid,
        /// Which request the timer belonged to.
        request_id: u64,
    },
    /// A decoded control or input message from a specific session.
    Peer {
        /// The session it arrived on.
        session: Uuid,
        /// The message.
        message: InboundControl,
    },
}

/// Control and input messages from a peer, decoded and validated.
#[derive(Debug, Clone)]
pub enum InboundControl {
    /// Peer asks to control this machine.
    Request(ControlRequest),
    /// Peer answered our request.
    Response(ControlResponse),
    /// Peer ended the control relationship, carrying an edge-return
    /// position when the reclaim came from a crossing (ADR 0009).
    Release(Option<u16>),
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
                let release = ControlRelease::decode_payload(payload)?;
                Ok(Some(Self::Release(release.entry)))
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
    /// End the relationship, carrying an edge-return position when the
    /// reclaim came from a crossing (ADR 0009); `None` for an explicit
    /// hand-back or console revoke.
    Release(Option<u16>),
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
            Self::Release(entry) => Ok((
                MessageType::ControlRelease.wire(),
                ControlRelease { entry: *entry }.encode_payload()?,
            )),
            Self::Batch(m) => Ok((MessageType::InputBatch.wire(), m.encode_payload()?)),
            Self::ReleaseAll(m) => Ok((MessageType::ReleaseAllInput.wire(), m.encode_payload()?)),
        }
    }
}

/// Why this machine's control of a peer ended.
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
    /// The named session is not established.
    NoSession,
    /// A peer currently controls this machine; release first.
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
    /// A request went to a peer.
    RequestSent,
    /// A request could not be made.
    RequestBlocked(RequestBlocked),
    /// The peer denied the request.
    RequestDenied(DenyReason),
    /// The peer never answered inside the timeout.
    RequestTimedOut,
    /// This machine now controls a peer.
    ControlGained,
    /// This machine's control of a peer ended.
    ControlEnded(ControlEndReason),
    /// A peer now controls this machine.
    PeerTookControl,
    /// The controlling peer handed control of this machine back.
    PeerReleasedControl,
    /// The user revoked a peer's control of this machine.
    PeerControlRevoked,
    /// A peer's control of this machine ended with the session; its
    /// input was released locally (FR-4.4).
    PeerControlLostOnDisconnect,
    /// A peer's control ended because this machine's input desktop switched
    /// to one it cannot inject into (a UAC/secure-desktop or lock prompt);
    /// its input was released locally (feature/87).
    PeerControlLostToDesktop,
}

/// What the engine asks the driver to do. Order within the returned
/// `Vec` is the required execution order.
///
/// Not `Eq`: [`ControlAction::PlaceCursor`] carries a floating-point
/// fraction. Equality is still available for assertions via `PartialEq`.
#[derive(Debug, PartialEq)]
pub enum ControlAction {
    /// Send this message to a specific session.
    Send {
        /// The session to send it to.
        session: Uuid,
        /// The message.
        message: OutboundControl,
    },
    /// Begin capturing (and suppressing) local pointer input.
    StartCapture,
    /// Stop capturing; local input acts locally again.
    StopCapture,
    /// Inject these events into this machine, pointer and key interleaved.
    Inject(Vec<InputEvent>),
    /// Place this machine's cursor at the given normalized position along
    /// the linked (entry) edge, as control arrives or returns here — the
    /// driver maps the fraction through the local display's geometry and
    /// injects an absolute move (ADR 0009).
    PlaceCursor(EdgeFraction),
    /// Arrange a [`ControlEvent::RequestTimeout`] after `delay`.
    ScheduleRequestTimeout {
        /// The session the request went to.
        session: Uuid,
        /// Which request the timer guards.
        request_id: u64,
        /// The configured wait.
        delay: Duration,
    },
    /// The session violated the protocol: terminate it (FR-2.3).
    Terminate {
        /// The offending session.
        session: Uuid,
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
    /// Sessions currently established. Membership is what makes a user
    /// request valid and what a lost session is removed from.
    established: BTreeSet<Uuid>,
    /// This machine's control of a peer (the singleton outbound axis).
    outbound: Outbound,
    /// A peer's control of this machine (the singleton inbound axis).
    controlled: Option<Controlled>,
    next_request_id: u64,
    /// Last sequence sent while controlling (reset per grant).
    send_sequence: u64,
    /// What this machine believes is held down on the controlled peer
    /// (FR-4.3). Meaningful only while `outbound` is `Remote`.
    sent_state: InputState,
    /// The session whose inbound grant this machine just gave up (by
    /// revoke or edge return). Input still arriving from it is in-flight
    /// from before it saw the release — a transition race, not an attack
    /// — so it is *dropped*, not treated as a violation that terminates
    /// the session (ADR 0009). Cleared when that session re-negotiates,
    /// is lost, or a new grant is taken.
    recently_released: Option<Uuid>,
}

impl ControlEngine {
    /// A fresh engine: local control, no sessions.
    #[must_use]
    pub fn new(config: ControlConfig) -> Self {
        Self {
            config,
            established: BTreeSet::new(),
            outbound: Outbound::Local,
            controlled: None,
            next_request_id: 0,
            send_sequence: 0,
            sent_state: InputState::new(),
            recently_released: None,
        }
    }

    /// Is this machine controlling a peer (capture should be active)?
    #[must_use]
    pub fn is_controlling(&self) -> bool {
        matches!(self.outbound, Outbound::Remote { .. })
    }

    /// Is a peer controlling this machine?
    #[must_use]
    pub fn is_controlled(&self) -> bool {
        self.controlled.is_some()
    }

    /// Process one event. The returned actions must be executed in
    /// order.
    pub fn handle(&mut self, event: ControlEvent) -> Vec<ControlAction> {
        match event {
            ControlEvent::UserRequestControl { session } => self.on_request(session, None),
            ControlEvent::EdgeLeave { session, position } => {
                self.on_request(session, Some(position))
            }
            ControlEvent::UserRelease => self.on_release(None),
            ControlEvent::EdgeReturn { position } => self.on_release(Some(position)),
            ControlEvent::SessionEstablished { session } => {
                self.established.insert(session);
                Vec::new()
            }
            ControlEvent::SessionLost { session } => self.on_session_lost(session),
            ControlEvent::Captured(events) => self.on_captured(&events),
            ControlEvent::CaptureLost => self.on_capture_lost(),
            ControlEvent::InputDesktopUnavailable => self.on_input_desktop_unavailable(),
            ControlEvent::RequestTimeout {
                session,
                request_id,
            } => self.on_request_timeout(session, request_id),
            ControlEvent::Peer { session, message } => self.on_peer(session, message),
        }
    }

    /// Request control of `session`, whether triggered by the console
    /// (`entry` is `None`) or by an edge crossing (`entry` carries where
    /// the cursor left, for the peer to place its cursor).
    fn on_request(&mut self, session: Uuid, entry: Option<EdgeFraction>) -> Vec<ControlAction> {
        let blocked = if !self.established.contains(&session) {
            Some(RequestBlocked::NoSession)
        } else if self.controlled.is_some() {
            Some(RequestBlocked::PeerHoldsControl)
        } else {
            match self.outbound {
                Outbound::Remote { .. } => Some(RequestBlocked::AlreadyControlling),
                Outbound::Requesting { .. } => Some(RequestBlocked::RequestPending),
                Outbound::Local => None,
            }
        };
        if let Some(reason) = blocked {
            return vec![ControlAction::Notify(ControlNotice::RequestBlocked(reason))];
        }

        self.next_request_id += 1;
        let request_id = self.next_request_id;
        self.outbound = Outbound::Requesting {
            session,
            request_id,
        };
        vec![
            ControlAction::Send {
                session,
                message: OutboundControl::Request(ControlRequest {
                    request_id,
                    entry: entry.map(EdgeFraction::to_wire),
                }),
            },
            ControlAction::ScheduleRequestTimeout {
                session,
                request_id,
                delay: self.config.request_timeout,
            },
            ControlAction::Notify(ControlNotice::RequestSent),
        ]
    }

    /// End whichever control relationship exists. `entry` is `Some` only
    /// for an edge-return revoke, carrying where the reclaiming cursor
    /// left so the controller places its own cursor (ADR 0009); the
    /// console paths (hand-back, cancel, plain revoke) pass `None`.
    fn on_release(&mut self, entry: Option<EdgeFraction>) -> Vec<ControlAction> {
        // Ending outbound control takes precedence over revoking inbound:
        // the outbound relationship is the one the user just initiated,
        // and the two axes end independently across successive presses.
        match self.outbound {
            // Hand control back. StopCapture leads so no freshly captured
            // event can chase the release messages; ReleaseAllInput goes
            // before ControlRelease so TCP delivers them in that order. A
            // hand-back never carries a position — the controller's cursor
            // is frozen at the edge, so only the controlled side returns.
            Outbound::Remote { session } => {
                let after_sequence = self.send_sequence;
                self.outbound = Outbound::Local;
                self.sent_state = InputState::new();
                vec![
                    ControlAction::StopCapture,
                    ControlAction::Send {
                        session,
                        message: OutboundControl::ReleaseAll(ReleaseAllInput { after_sequence }),
                    },
                    ControlAction::Send {
                        session,
                        message: OutboundControl::Release(None),
                    },
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
            Outbound::Local => {
                // Revoke a peer's grant: the local user's escape hatch,
                // and the reverse-edge return. On a return, the position
                // rides on the release so the controller's cursor reappears
                // at the matching height (ADR 0009).
                if let Some(controlled) = self.controlled.take() {
                    let mut controlled = controlled;
                    // Input still in flight from this peer (sent before it
                    // sees our release) is an expected transition race, not
                    // a violation: remember the session so those batches
                    // are dropped rather than terminating it.
                    self.recently_released = Some(controlled.session);
                    let releases = drain_releases(&mut controlled.applied_state);
                    let mut actions = Vec::new();
                    if !releases.is_empty() {
                        actions.push(ControlAction::Inject(releases));
                    }
                    actions.push(ControlAction::Send {
                        session: controlled.session,
                        message: OutboundControl::Release(entry.map(EdgeFraction::to_wire)),
                    });
                    actions.push(ControlAction::Notify(ControlNotice::PeerControlRevoked));
                    actions
                } else {
                    Vec::new() // nothing to release; silent
                }
            }
        }
    }

    /// Give up a peer's control of this machine because the input desktop
    /// switched to one we cannot inject into (a UAC/secure-desktop prompt).
    /// Mirrors the revoke path in `on_release` — drain what we hold locally
    /// and tell the peer to release — but it is machine-driven, not the
    /// user's escape hatch, and is reported distinctly so a headless log says
    /// *why* control dropped. The peer, on the `Release`, returns to local
    /// and un-hides its cursor (feature/87). No-op if no peer holds control.
    fn on_input_desktop_unavailable(&mut self) -> Vec<ControlAction> {
        let Some(mut controlled) = self.controlled.take() else {
            return Vec::new();
        };
        // In-flight input from before the peer sees our release is an
        // expected race, not a violation (as in the revoke path).
        self.recently_released = Some(controlled.session);
        let releases = drain_releases(&mut controlled.applied_state);
        let mut actions = Vec::new();
        if !releases.is_empty() {
            actions.push(ControlAction::Inject(releases));
        }
        actions.push(ControlAction::Send {
            session: controlled.session,
            message: OutboundControl::Release(None),
        });
        actions.push(ControlAction::Notify(
            ControlNotice::PeerControlLostToDesktop,
        ));
        actions
    }

    fn on_session_lost(&mut self, session: Uuid) -> Vec<ControlAction> {
        self.established.remove(&session);
        if self.recently_released == Some(session) {
            self.recently_released = None; // dead: no in-flight grace to keep
        }
        let mut actions = Vec::new();

        // Outbound axis, if it targets the lost session.
        match self.outbound {
            Outbound::Remote { session: s } if s == session => {
                // The peer releases its own belief on its side of the
                // loss (FR-4.4); nothing can be sent on a dead session.
                self.outbound = Outbound::Local;
                self.sent_state = InputState::new();
                actions.push(ControlAction::StopCapture);
                actions.push(ControlAction::Notify(ControlNotice::ControlEnded(
                    ControlEndReason::Disconnected,
                )));
            }
            Outbound::Requesting { session: s, .. } if s == session => {
                self.outbound = Outbound::Local;
                actions.push(ControlAction::Notify(ControlNotice::ControlEnded(
                    ControlEndReason::Disconnected,
                )));
            }
            _ => {}
        }

        // Inbound axis, if the lost session was controlling us.
        if self
            .controlled
            .as_ref()
            .is_some_and(|c| c.session == session)
        {
            // FR-4.4, the path the spec calls release-blocking: the
            // destination synthesizes releases for everything it
            // believes is pressed, from its own records.
            let mut controlled = self.controlled.take().expect("checked just above");
            let releases = drain_releases(&mut controlled.applied_state);
            if !releases.is_empty() {
                actions.push(ControlAction::Inject(releases));
            }
            actions.push(ControlAction::Notify(
                ControlNotice::PeerControlLostOnDisconnect,
            ));
        }
        actions
    }

    fn on_captured(&mut self, events: &[InputEvent]) -> Vec<ControlAction> {
        // Anything captured outside REMOTE is a stray tail between the
        // stop decision and the platform actually stopping: drop it, or
        // it would act on a peer that no longer expects input.
        let Outbound::Remote { session } = self.outbound else {
            return Vec::new();
        };
        let merged = coalesce_input(events);
        if merged.is_empty() {
            return Vec::new();
        }
        let mut actions = Vec::new();
        for chunk in merged.chunks(MAX_INPUT_BATCH_EVENTS) {
            self.send_sequence += 1;
            self.sent_state.apply_inputs(chunk);
            actions.push(ControlAction::Send {
                session,
                message: OutboundControl::Batch(InputBatch {
                    sequence: self.send_sequence,
                    events: chunk.iter().map(to_wire).collect(),
                }),
            });
        }
        actions
    }

    fn on_capture_lost(&mut self) -> Vec<ControlAction> {
        let Outbound::Remote { session } = self.outbound else {
            return Vec::new();
        };
        // Fail closed (ADR 0007): suppression is gone, so control is
        // over. The peer releases what we made it hold; the local
        // StopCapture is belt-and-braces on an already-dead capture.
        let after_sequence = self.send_sequence;
        self.outbound = Outbound::Local;
        self.sent_state = InputState::new();
        vec![
            ControlAction::StopCapture,
            ControlAction::Send {
                session,
                message: OutboundControl::ReleaseAll(ReleaseAllInput { after_sequence }),
            },
            ControlAction::Send {
                session,
                message: OutboundControl::Release(None),
            },
            ControlAction::Notify(ControlNotice::ControlEnded(ControlEndReason::CaptureLost)),
        ]
    }

    fn on_request_timeout(&mut self, session: Uuid, request_id: u64) -> Vec<ControlAction> {
        // Only the timer for the request still in flight matters; a
        // stale timer (answered or superseded request) is a no-op.
        if self.outbound
            == (Outbound::Requesting {
                session,
                request_id,
            })
        {
            self.outbound = Outbound::Local;
            vec![ControlAction::Notify(ControlNotice::RequestTimedOut)]
        } else {
            Vec::new()
        }
    }

    fn on_peer(&mut self, session: Uuid, message: InboundControl) -> Vec<ControlAction> {
        match message {
            InboundControl::Request(request) => self.on_peer_request(session, request),
            InboundControl::Response(response) => self.on_peer_response(session, response),
            InboundControl::Release(entry) => self.on_peer_release(session, entry),
            InboundControl::Batch(batch) => self.on_peer_batch(session, &batch),
            InboundControl::ReleaseAll(_) => self.on_peer_release_all(session),
        }
    }

    fn on_peer_request(&mut self, session: Uuid, request: ControlRequest) -> Vec<ControlAction> {
        // A request means this peer knows the prior grant ended and is
        // re-negotiating, so the drop-in-flight grace no longer applies to
        // it — any input after this must again come via a fresh grant.
        if self.recently_released == Some(session) {
            self.recently_released = None;
        }
        let deny = |reason| {
            vec![ControlAction::Send {
                session,
                message: OutboundControl::Response(ControlResponse {
                    request_id: request.request_id,
                    verdict: ControlVerdict::Denied(reason),
                }),
            }]
        };
        // One peer controls this machine at a time (single desktop): a
        // request while already controlled — even by the same session —
        // is denied.
        if self.controlled.is_some() {
            return deny(DenyReason::AlreadyControlled);
        }
        match self.outbound {
            // Controlling or requesting: busy. Two simultaneous requests
            // thus produce two denials — deterministic (FR-5.1).
            Outbound::Remote { .. } | Outbound::Requesting { .. } => deny(DenyReason::Busy),
            Outbound::Local => {
                self.controlled = Some(Controlled {
                    session,
                    applied_sequence: 0,
                    applied_state: InputState::new(),
                });
                let mut actions = vec![ControlAction::Send {
                    session,
                    message: OutboundControl::Response(ControlResponse {
                        request_id: request.request_id,
                        verdict: ControlVerdict::Granted,
                    }),
                }];
                // An edge-driven request carries where the peer's cursor
                // left; place ours at the matching height on the entry
                // edge so the pointer appears to cross seamlessly (ADR
                // 0009). A console request carries none.
                if let Some(raw) = request.entry {
                    actions.push(ControlAction::PlaceCursor(EdgeFraction::from_wire(raw)));
                }
                actions.push(ControlAction::Notify(ControlNotice::PeerTookControl));
                actions
            }
        }
    }

    fn on_peer_response(&mut self, session: Uuid, response: ControlResponse) -> Vec<ControlAction> {
        // Undo a grant we are not (or no longer) waiting for, sent back
        // to the very session that issued it — otherwise that peer sits
        // believing it is controlled by us, driverless, forever.
        let undo_stray = |verdict: ControlVerdict| -> Vec<ControlAction> {
            if verdict == ControlVerdict::Granted {
                vec![ControlAction::Send {
                    session,
                    message: OutboundControl::Release(None),
                }]
            } else {
                Vec::new()
            }
        };

        let Outbound::Requesting {
            session: req_session,
            request_id,
        } = self.outbound
        else {
            return undo_stray(response.verdict);
        };
        // The answer must come from the session we asked, and match the
        // id: a response from any other session is not our grant.
        if session != req_session || response.request_id != request_id {
            return undo_stray(response.verdict);
        }
        match response.verdict {
            ControlVerdict::Granted => {
                self.outbound = Outbound::Remote { session };
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

    fn on_peer_release(&mut self, session: Uuid, entry: Option<u16>) -> Vec<ControlAction> {
        // Is `session` the peer controlling us handing back?
        if self
            .controlled
            .as_ref()
            .is_some_and(|c| c.session == session)
        {
            // Release everything it left held — normally its
            // ReleaseAllInput already did, but this engine's belief is
            // the authority and releasing twice is harmless where a
            // stuck button is not (FR-4.4).
            let mut controlled = self.controlled.take().expect("checked just above");
            let releases = drain_releases(&mut controlled.applied_state);
            let mut actions = Vec::new();
            if !releases.is_empty() {
                actions.push(ControlAction::Inject(releases));
            }
            actions.push(ControlAction::Notify(ControlNotice::PeerReleasedControl));
            return actions;
        }
        // Is `session` the peer we are controlling, revoking our grant
        // (its user's escape hatch, or the reverse-edge return)? Stop
        // capturing immediately. A return carries where the cursor left
        // the peer's edge, so ours reappears at the matching height on the
        // way back (ADR 0009).
        if self.outbound == (Outbound::Remote { session }) {
            self.outbound = Outbound::Local;
            self.sent_state = InputState::new();
            let mut actions = vec![ControlAction::StopCapture];
            if let Some(raw) = entry {
                actions.push(ControlAction::PlaceCursor(EdgeFraction::from_wire(raw)));
            }
            actions.push(ControlAction::Notify(ControlNotice::ControlEnded(
                ControlEndReason::Revoked,
            )));
            return actions;
        }
        // A release from an unrelated session: the cleanup path for a
        // grant we un-did, or a duplicate. Nothing to do.
        Vec::new()
    }

    fn on_peer_batch(&mut self, session: Uuid, batch: &InputBatch) -> Vec<ControlAction> {
        // Complete mediation (FR-2.3, FR-5.1): inject only for the one
        // session that holds the grant. Input from any other session —
        // even while this machine is legitimately controlled by someone
        // else — is a violation, and grants travel on the same ordered
        // stream as batches, so an honest peer cannot interleave them
        // wrongly.
        let Some(controlled) = self.controlled.as_mut().filter(|c| c.session == session) else {
            // A peer whose grant we just gave up may still have input in
            // flight from before it saw the release: drop it (a transition
            // race, ADR 0009), never inject it, and do not tear the session
            // down. A batch from any *other* ungranted session is a real
            // violation and fails closed (FR-2.3).
            if self.recently_released == Some(session) {
                return Vec::new();
            }
            return vec![ControlAction::Terminate {
                session,
                reason: "input batch from a session that holds no control grant".to_owned(),
            }];
        };
        // TCP+TLS delivers what was sent, in order; a regression or
        // duplicate cannot be innocent (docs/PROTOCOL.md §6).
        if batch.sequence <= controlled.applied_sequence {
            return vec![ControlAction::Terminate {
                session,
                reason: format!(
                    "input batch sequence {} not after {}",
                    batch.sequence, controlled.applied_sequence
                ),
            }];
        }
        controlled.applied_sequence = batch.sequence;
        // Pointer and keyboard events replay in one ordered stream so a
        // chord keeps its ordering (ADR 0008). The applied-state belief
        // tracks both, so `ReleaseAllInput` can synthesize releases for a
        // held key or modifier just as it does for a button (FR-4.4).
        let events: Vec<InputEvent> = batch.events.iter().map(from_wire).collect();
        controlled.applied_state.apply_inputs(&events);
        vec![ControlAction::Inject(events)]
    }

    fn on_peer_release_all(&mut self, session: Uuid) -> Vec<ControlAction> {
        // Only the controlling session's release-all releases this
        // machine's belief (FR-4.4). From any other session it is
        // meaningless — ignore rather than inject or terminate, since it
        // can hold nothing to release.
        if let Some(controlled) = self.controlled.as_mut().filter(|c| c.session == session) {
            let releases = drain_releases(&mut controlled.applied_state);
            if releases.is_empty() {
                Vec::new()
            } else {
                vec![ControlAction::Inject(releases)]
            }
        } else {
            Vec::new()
        }
    }
}

/// Every held button and key drained as release events to inject — the
/// local half of `ReleaseAllInput` (FR-4.4). Buttons first, then keys,
/// each in its own deterministic order (NFR-2); afterwards the state
/// holds nothing, so a stuck modifier cannot survive a disconnect any
/// more than a stuck button can (ADR 0008).
fn drain_releases(state: &mut InputState) -> Vec<InputEvent> {
    let mut releases: Vec<InputEvent> = state
        .release_all()
        .into_iter()
        .map(InputEvent::Pointer)
        .collect();
    releases.extend(state.release_all_keys().into_iter().map(InputEvent::Key));
    releases
}

/// Platform event → wire event (ADR 0008). Total: every capturable
/// event travels, pointer or key.
fn to_wire(event: &InputEvent) -> WireInputEvent {
    match event {
        InputEvent::Pointer(PointerEvent::Motion { dx, dy }) => {
            WireInputEvent::Motion { dx: *dx, dy: *dy }
        }
        InputEvent::Pointer(PointerEvent::Button { button, pressed }) => WireInputEvent::Button {
            button: button_to_wire(*button),
            pressed: *pressed,
        },
        InputEvent::Pointer(PointerEvent::Scroll { dx, dy }) => {
            WireInputEvent::Scroll { dx: *dx, dy: *dy }
        }
        InputEvent::Key(key) => WireInputEvent::Key {
            key: key.key,
            pressed: key.pressed,
            repeat: key.repeat,
            text: key.text.clone(),
        },
    }
}

/// Wire event → platform input event (ADR 0008). Total: every valid wire
/// event injects, pointer or key.
fn from_wire(event: &WireInputEvent) -> InputEvent {
    match event {
        WireInputEvent::Motion { dx, dy } => {
            InputEvent::Pointer(PointerEvent::Motion { dx: *dx, dy: *dy })
        }
        WireInputEvent::Button { button, pressed } => InputEvent::Pointer(PointerEvent::Button {
            button: button_from_wire(*button),
            pressed: *pressed,
        }),
        WireInputEvent::Scroll { dx, dy } => {
            InputEvent::Pointer(PointerEvent::Scroll { dx: *dx, dy: *dy })
        }
        WireInputEvent::Key {
            key,
            pressed,
            repeat,
            text,
        } => InputEvent::Key(KeyEvent {
            key: *key,
            pressed: *pressed,
            repeat: *repeat,
            text: text.clone(),
        }),
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
    use uuid::Uuid;

    use crossover_protocol::{
        ControlRequest, ControlResponse, ControlVerdict, DenyReason, InputBatch, ReleaseAllInput,
        WireButton, WireInputEvent,
    };

    use super::{
        ControlAction, ControlConfig, ControlEndReason, ControlEngine, ControlEvent, ControlNotice,
        InboundControl, OutboundControl, RequestBlocked,
    };
    use crate::input::{InputEvent, InputState, KeyEvent, PointerButton, PointerEvent, hid};
    use crate::topology::EdgeFraction;

    /// The session the single-session tests operate on.
    const SESSION: Uuid = Uuid::from_bytes([0xA1; 16]);
    /// A second, distinct session, for the mediation tests.
    const OTHER: Uuid = Uuid::from_bytes([0xB2; 16]);

    /// A `Send` to [`SESSION`].
    fn send(message: OutboundControl) -> ControlAction {
        ControlAction::Send {
            session: SESSION,
            message,
        }
    }

    /// A peer message from [`SESSION`].
    fn peer(message: InboundControl) -> ControlEvent {
        ControlEvent::Peer {
            session: SESSION,
            message,
        }
    }

    /// A peer message from an arbitrary session.
    fn peer_from(session: Uuid, message: InboundControl) -> ControlEvent {
        ControlEvent::Peer { session, message }
    }

    fn established_engine() -> ControlEngine {
        let mut engine = ControlEngine::new(ControlConfig::default());
        let actions = engine.handle(ControlEvent::SessionEstablished { session: SESSION });
        assert!(actions.is_empty());
        engine
    }

    /// Drive the engine to REMOTE over [`SESSION`]: request, then grant.
    fn controlling_engine() -> ControlEngine {
        let mut engine = established_engine();
        let actions = engine.handle(ControlEvent::UserRequestControl { session: SESSION });
        let request_id = sent_request_id(&actions);
        let actions = engine.handle(peer(InboundControl::Response(ControlResponse {
            request_id,
            verdict: ControlVerdict::Granted,
        })));
        assert!(actions.contains(&ControlAction::StartCapture));
        assert!(engine.is_controlling());
        engine
    }

    /// Drive the engine to being controlled by [`SESSION`].
    fn controlled_engine() -> ControlEngine {
        let mut engine = established_engine();
        let actions = engine.handle(peer(InboundControl::Request(ControlRequest {
            request_id: 1,
            entry: None,
        })));
        assert!(granted(&actions));
        assert!(engine.is_controlled());
        engine
    }

    fn sent_request_id(actions: &[ControlAction]) -> u64 {
        actions
            .iter()
            .find_map(|action| match action {
                ControlAction::Send {
                    message: OutboundControl::Request(r),
                    ..
                } => Some(r.request_id),
                _ => None,
            })
            .expect("no ControlRequest sent")
    }

    fn granted(actions: &[ControlAction]) -> bool {
        actions.iter().any(|action| {
            matches!(
                action,
                ControlAction::Send {
                    message: OutboundControl::Response(ControlResponse {
                        verdict: ControlVerdict::Granted,
                        ..
                    }),
                    ..
                }
            )
        })
    }

    fn press(button: PointerButton) -> PointerEvent {
        PointerEvent::Button {
            button,
            pressed: true,
        }
    }

    /// An `Inject` of pointer events — most tests predate the keyboard and
    /// reason in pointer terms; the engine now injects the unified type.
    fn inject(events: Vec<PointerEvent>) -> ControlAction {
        ControlAction::Inject(events.into_iter().map(InputEvent::Pointer).collect())
    }

    /// A `Captured` of pointer events — same convenience for the capture
    /// side, which the engine now takes as the unified type.
    fn captured(events: Vec<PointerEvent>) -> ControlEvent {
        ControlEvent::Captured(events.into_iter().map(InputEvent::Pointer).collect())
    }

    // ---- negotiation (FR-5.3) ----

    #[test]
    fn request_grant_switch_in_that_order() {
        let mut engine = established_engine();
        let actions = engine.handle(ControlEvent::UserRequestControl { session: SESSION });
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
        let actions = engine.handle(peer(InboundControl::Response(ControlResponse {
            request_id,
            verdict: ControlVerdict::Granted,
        })));
        assert_eq!(actions[0], ControlAction::StartCapture);
        assert!(engine.is_controlling());
    }

    #[test]
    fn denial_reverts_to_local_with_the_reason() {
        let mut engine = established_engine();
        let request_id =
            sent_request_id(&engine.handle(ControlEvent::UserRequestControl { session: SESSION }));
        let actions = engine.handle(peer(InboundControl::Response(ControlResponse {
            request_id,
            verdict: ControlVerdict::Denied(DenyReason::Busy),
        })));
        assert!(
            actions.contains(&ControlAction::Notify(ControlNotice::RequestDenied(
                DenyReason::Busy
            )))
        );
        assert!(!engine.is_controlling());
    }

    #[test]
    fn timeout_reverts_to_local_and_a_late_grant_is_undone() {
        let mut engine = established_engine();
        let request_id =
            sent_request_id(&engine.handle(ControlEvent::UserRequestControl { session: SESSION }));

        let actions = engine.handle(ControlEvent::RequestTimeout {
            session: SESSION,
            request_id,
        });
        assert!(actions.contains(&ControlAction::Notify(ControlNotice::RequestTimedOut)));
        assert!(!engine.is_controlling());

        // The grant arrives after all. The peer believes it is now
        // controlled; the engine must undo that, not adopt it.
        let actions = engine.handle(peer(InboundControl::Response(ControlResponse {
            request_id,
            verdict: ControlVerdict::Granted,
        })));
        assert_eq!(
            actions,
            vec![send(OutboundControl::Release(None))],
            "a grant nobody is waiting for must be explicitly released"
        );
        assert!(!engine.is_controlling());
    }

    #[test]
    fn stale_timeout_after_grant_changes_nothing() {
        let mut engine = controlling_engine();
        // The timer for the (answered) request fires late.
        let actions = engine.handle(ControlEvent::RequestTimeout {
            session: SESSION,
            request_id: 1,
        });
        assert!(actions.is_empty());
        assert!(engine.is_controlling());
    }

    #[test]
    fn requests_are_blocked_with_reasons_not_silently() {
        // No such session.
        let mut engine = ControlEngine::new(ControlConfig::default());
        assert_eq!(
            engine.handle(ControlEvent::UserRequestControl { session: SESSION }),
            vec![ControlAction::Notify(ControlNotice::RequestBlocked(
                RequestBlocked::NoSession
            ))]
        );

        let mut engine = controlled_engine();
        assert_eq!(
            engine.handle(ControlEvent::UserRequestControl { session: SESSION }),
            vec![ControlAction::Notify(ControlNotice::RequestBlocked(
                RequestBlocked::PeerHoldsControl
            ))]
        );

        let mut engine = controlling_engine();
        assert_eq!(
            engine.handle(ControlEvent::UserRequestControl { session: SESSION }),
            vec![ControlAction::Notify(ControlNotice::RequestBlocked(
                RequestBlocked::AlreadyControlling
            ))]
        );
    }

    #[test]
    fn simultaneous_requests_deny_deterministically() {
        // Our request is in flight when the peer's request arrives.
        let mut engine = established_engine();
        let _ = engine.handle(ControlEvent::UserRequestControl { session: SESSION });
        let actions = engine.handle(peer(InboundControl::Request(ControlRequest {
            request_id: 9,
            entry: None,
        })));
        assert_eq!(
            actions,
            vec![send(OutboundControl::Response(ControlResponse {
                request_id: 9,
                verdict: ControlVerdict::Denied(DenyReason::Busy),
            }))]
        );
        assert!(!engine.is_controlled());
    }

    #[test]
    fn a_controlled_machine_denies_further_requests() {
        let mut engine = controlled_engine();
        let actions = engine.handle(peer(InboundControl::Request(ControlRequest {
            request_id: 2,
            entry: None,
        })));
        assert_eq!(
            actions,
            vec![send(OutboundControl::Response(ControlResponse {
                request_id: 2,
                verdict: ControlVerdict::Denied(DenyReason::AlreadyControlled),
            }))]
        );
    }

    // ---- controller: capture, batching, hand-back ----

    /// Captured keyboard travels the same path as pointer input (ADR
    /// 0008): motion coalesces around a key, the key is a barrier and is
    /// never merged, and the batch carries both, in order, to the
    /// controlled session.
    #[test]
    fn captured_keys_coalesce_around_but_never_into_pointer_motion() {
        let mut engine = controlling_engine();
        let actions = engine.handle(ControlEvent::Captured(vec![
            InputEvent::Pointer(PointerEvent::Motion { dx: 2, dy: 0 }),
            InputEvent::Pointer(PointerEvent::Motion { dx: 3, dy: 0 }),
            InputEvent::Key(KeyEvent::press(hid::LEFT_SHIFT)),
            InputEvent::Pointer(PointerEvent::Motion { dx: 4, dy: 0 }),
        ]));
        assert_eq!(
            actions,
            vec![send(OutboundControl::Batch(InputBatch {
                sequence: 1,
                events: vec![
                    WireInputEvent::Motion { dx: 5, dy: 0 }, // the two merged
                    WireInputEvent::Key {
                        key: hid::LEFT_SHIFT,
                        pressed: true,
                        repeat: false,
                        text: None,
                    },
                    WireInputEvent::Motion { dx: 4, dy: 0 }, // not merged across the key
                ],
            }))]
        );
    }

    #[test]
    fn captured_events_are_coalesced_sequenced_and_tracked() {
        let mut engine = controlling_engine();
        let actions = engine.handle(captured(vec![
            PointerEvent::Motion { dx: 3, dy: 0 },
            PointerEvent::Motion { dx: 4, dy: 1 },
            press(PointerButton::Left),
        ]));
        assert_eq!(
            actions,
            vec![send(OutboundControl::Batch(InputBatch {
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
        let actions = engine.handle(captured(vec![PointerEvent::Motion { dx: 1, dy: 1 }]));
        assert!(matches!(
            &actions[0],
            ControlAction::Send { message: OutboundControl::Batch(batch), .. } if batch.sequence == 2
        ));
    }

    #[test]
    fn captured_events_outside_remote_are_dropped() {
        let mut engine = established_engine();
        assert!(
            engine
                .handle(captured(vec![press(PointerButton::Left)]))
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
        let actions = engine.handle(captured(events));
        assert_eq!(actions.len(), 2, "burst must split into two batches");
        for action in &actions {
            let ControlAction::Send {
                message: OutboundControl::Batch(batch),
                ..
            } = action
            else {
                panic!("expected only batch sends, got {action:?}");
            };
            batch.validate().expect("chunk exceeds the wire bound");
        }
    }

    #[test]
    fn hand_back_stops_capture_then_releases_then_ends() {
        let mut engine = controlling_engine();
        let _ = engine.handle(captured(vec![press(PointerButton::Left)]));

        let actions = engine.handle(ControlEvent::UserRelease);
        assert_eq!(
            actions,
            vec![
                ControlAction::StopCapture,
                send(OutboundControl::ReleaseAll(ReleaseAllInput {
                    after_sequence: 1,
                })),
                send(OutboundControl::Release(None)),
                ControlAction::Notify(ControlNotice::ControlEnded(ControlEndReason::HandedBack)),
            ]
        );
        assert!(!engine.is_controlling());
    }

    #[test]
    fn capture_loss_fails_closed_and_tells_both_sides() {
        let mut engine = controlling_engine();
        let _ = engine.handle(captured(vec![press(PointerButton::Right)]));

        let actions = engine.handle(ControlEvent::CaptureLost);
        assert!(actions.contains(&ControlAction::StopCapture));
        assert!(
            actions.contains(&send(OutboundControl::ReleaseAll(ReleaseAllInput {
                after_sequence: 1,
            })))
        );
        assert!(actions.contains(&send(OutboundControl::Release(None))));
        assert!(!engine.is_controlling());

        // Not controlling: further loss reports are noise.
        assert!(engine.handle(ControlEvent::CaptureLost).is_empty());
    }

    #[test]
    fn disconnect_while_controlling_stops_capture() {
        let mut engine = controlling_engine();
        let actions = engine.handle(ControlEvent::SessionLost { session: SESSION });
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
        let actions = engine.handle(peer(InboundControl::Release(None)));
        assert_eq!(actions[0], ControlAction::StopCapture);
        assert!(!engine.is_controlling());
    }

    // ---- controlled: injection, sequencing, release (FR-4.4) ----

    #[test]
    fn granted_input_is_injected_and_tracked() {
        let mut engine = controlled_engine();
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
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
            vec![inject(vec![
                PointerEvent::Motion { dx: 5, dy: 5 },
                press(PointerButton::Left),
            ])]
        );
    }

    #[test]
    fn input_without_a_grant_terminates_the_session() {
        let mut engine = established_engine();
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
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
            peer(InboundControl::Batch(InputBatch {
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
        let _ = engine.handle(peer(InboundControl::Batch(InputBatch {
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
        let actions = engine.handle(peer(InboundControl::ReleaseAll(ReleaseAllInput {
            after_sequence: 1,
        })));
        assert_eq!(
            actions,
            vec![inject(vec![
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
        let actions = engine.handle(peer(InboundControl::Release(None)));
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
        let _ = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Left,
                pressed: true,
            }],
        })));

        let actions = engine.handle(ControlEvent::SessionLost { session: SESSION });
        assert!(actions.contains(&inject(vec![PointerEvent::Button {
            button: PointerButton::Left,
            pressed: false,
        }])));
        assert!(!engine.is_controlled());
    }

    #[test]
    fn revoking_the_peer_releases_and_notifies_it() {
        let mut engine = controlled_engine();
        let _ = engine.handle(peer(InboundControl::Batch(InputBatch {
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
                inject(vec![PointerEvent::Button {
                    button: PointerButton::Middle,
                    pressed: false,
                }]),
                send(OutboundControl::Release(None)),
                ControlAction::Notify(ControlNotice::PeerControlRevoked),
            ]
        );
        assert!(!engine.is_controlled());
    }

    #[test]
    fn a_secure_desktop_releases_the_peer_and_reports_distinctly() {
        // feature/87: when the input desktop switches to a secure one (a UAC
        // prompt), the controlled side gives up the grant exactly like a
        // revoke — drains what it holds and tells the peer, so the controller
        // returns to local and un-hides its cursor — but reports it distinctly
        // so a headless log says why rather than looking like a user revoke.
        let mut engine = controlled_engine();
        let _ = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Left,
                pressed: true,
            }],
        })));

        let actions = engine.handle(ControlEvent::InputDesktopUnavailable);
        assert_eq!(
            actions,
            vec![
                inject(vec![PointerEvent::Button {
                    button: PointerButton::Left,
                    pressed: false,
                }]),
                send(OutboundControl::Release(None)),
                ControlAction::Notify(ControlNotice::PeerControlLostToDesktop),
            ]
        );
        assert!(!engine.is_controlled());

        // No peer holds control now, so a second desktop event is a no-op.
        assert!(
            engine
                .handle(ControlEvent::InputDesktopUnavailable)
                .is_empty()
        );
    }

    /// Keyboard through the engine (ADR 0008): a granted key batch injects
    /// as key events, and a disconnect mid-hold synthesizes their releases
    /// from the engine's own belief — a stuck modifier is the same
    /// release-blocking defect class as a stuck button (FR-4.4).
    #[test]
    fn granted_keys_are_injected_and_released_on_disconnect() {
        let mut engine = controlled_engine();
        // The peer presses Left Control and holds it, then 'a' with text.
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![
                WireInputEvent::Key {
                    key: hid::LEFT_CONTROL,
                    pressed: true,
                    repeat: false,
                    text: None,
                },
                WireInputEvent::Key {
                    key: hid::A,
                    pressed: true,
                    repeat: false,
                    text: Some("a".to_owned()),
                },
            ],
        })));
        assert_eq!(
            actions,
            vec![ControlAction::Inject(vec![
                InputEvent::Key(KeyEvent::press(hid::LEFT_CONTROL)),
                InputEvent::Key(KeyEvent {
                    key: hid::A,
                    pressed: true,
                    repeat: false,
                    text: Some("a".to_owned()),
                }),
            ])]
        );

        // The session drops with both keys still held. The engine releases
        // them from its own record, in ascending usage order (NFR-2).
        let actions = engine.handle(ControlEvent::SessionLost { session: SESSION });
        assert!(actions.contains(&ControlAction::Inject(vec![
            InputEvent::Key(KeyEvent::release(hid::A)), // 0x04
            InputEvent::Key(KeyEvent::release(hid::LEFT_CONTROL)), // 0xE0
        ])));
        assert!(!engine.is_controlled());
    }

    // ---- complete mediation: per-session grant identity (FR-2.3) ----

    /// The security property this whole design exists for: while one
    /// session legitimately controls this machine, input from a
    /// *different* trusted session is never injected — it terminates the
    /// intruding session instead. No riding another peer's grant.
    #[test]
    fn input_from_a_non_granting_session_is_rejected() {
        let mut engine = controlled_engine(); // SESSION controls us
        // A batch from a different established session.
        let _ = engine.handle(ControlEvent::SessionEstablished { session: OTHER });
        let actions = engine.handle(peer_from(
            OTHER,
            InboundControl::Batch(InputBatch {
                sequence: 1,
                events: vec![WireInputEvent::Button {
                    button: WireButton::Left,
                    pressed: true,
                }],
            }),
        ));
        match &actions[0] {
            ControlAction::Terminate { session, .. } => assert_eq!(*session, OTHER),
            other => panic!("intruder input must terminate its own session, got {other:?}"),
        }
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ControlAction::Inject(_))),
            "intruder input must never be injected"
        );

        // The legitimate controller's input is still injected.
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Right,
                pressed: true,
            }],
        })));
        assert_eq!(actions, vec![inject(vec![press(PointerButton::Right)])]);
    }

    /// A grant arriving from a session other than the one we requested is
    /// undone against *that* session, and we do not adopt it.
    #[test]
    fn a_grant_from_the_wrong_session_is_undone_not_adopted() {
        let mut engine = established_engine();
        let _ = engine.handle(ControlEvent::SessionEstablished { session: OTHER });
        let request_id =
            sent_request_id(&engine.handle(ControlEvent::UserRequestControl { session: SESSION }));

        // OTHER answers Granted to a request it was never sent.
        let actions = engine.handle(peer_from(
            OTHER,
            InboundControl::Response(ControlResponse {
                request_id,
                verdict: ControlVerdict::Granted,
            }),
        ));
        assert_eq!(
            actions,
            vec![ControlAction::Send {
                session: OTHER,
                message: OutboundControl::Release(None),
            }],
            "a grant from the wrong session must be undone against it"
        );
        assert!(
            !engine.is_controlling(),
            "we must not adopt the stray grant"
        );
    }

    /// Two peers can each hold a grant in opposite directions at once:
    /// this machine controls SESSION while OTHER is denied control of us
    /// only if we are being controlled — but controlling and being
    /// controlled are independent axes, so verify a batch from the peer
    /// we *control* (not one controlling us) is rejected.
    #[test]
    fn a_batch_from_the_peer_we_control_is_rejected() {
        let mut engine = controlling_engine(); // we control SESSION
        // SESSION is the peer we drive; it holds no grant over us. If it
        // sends input, that is a violation.
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Motion { dx: 1, dy: 1 }],
        })));
        assert!(matches!(&actions[0], ControlAction::Terminate { .. }));
        assert!(engine.is_controlling(), "our own control is unaffected");
    }

    // ---- seamless edge transfer (ADR 0009) ----

    fn placed_cursor(actions: &[ControlAction]) -> Option<f64> {
        actions.iter().find_map(|a| match a {
            ControlAction::PlaceCursor(f) => Some(f.value()),
            _ => None,
        })
    }

    fn sent_request(actions: &[ControlAction]) -> ControlRequest {
        actions
            .iter()
            .find_map(|a| match a {
                ControlAction::Send {
                    message: OutboundControl::Request(r),
                    ..
                } => Some(*r),
                _ => None,
            })
            .expect("no request sent")
    }

    #[test]
    fn an_edge_leave_requests_control_carrying_the_crossing_position() {
        let mut engine = established_engine();
        let position = EdgeFraction::new(0.5);
        let actions = engine.handle(ControlEvent::EdgeLeave {
            session: SESSION,
            position,
        });
        assert_eq!(sent_request(&actions).entry, Some(position.to_wire()));
        // A console request carries no position, but is otherwise identical.
        let mut engine = established_engine();
        let actions = engine.handle(ControlEvent::UserRequestControl { session: SESSION });
        assert_eq!(sent_request(&actions).entry, None);
    }

    #[test]
    fn granting_an_edge_request_places_the_cursor_at_the_entry_fraction() {
        let mut engine = established_engine();
        let actions = engine.handle(peer(InboundControl::Request(ControlRequest {
            request_id: 1,
            entry: Some(EdgeFraction::new(0.25).to_wire()),
        })));
        assert!(granted(&actions));
        let placed = placed_cursor(&actions).expect("no cursor placed on an edge grant");
        assert!((placed - 0.25).abs() < 1e-4, "placed at {placed}");
        assert!(engine.is_controlled());

        // A console grant places nothing.
        let mut engine = established_engine();
        let actions = engine.handle(peer(InboundControl::Request(ControlRequest {
            request_id: 1,
            entry: None,
        })));
        assert!(granted(&actions));
        assert!(placed_cursor(&actions).is_none());
    }

    #[test]
    fn an_edge_return_revokes_with_the_position_and_the_controller_places_its_cursor() {
        // Controlled side: reaching the linked edge returns control,
        // carrying where the cursor left on the release.
        let mut engine = controlled_engine();
        let position = EdgeFraction::new(0.75);
        let actions = engine.handle(ControlEvent::EdgeReturn { position });
        let release_entry = actions
            .iter()
            .find_map(|a| match a {
                ControlAction::Send {
                    message: OutboundControl::Release(entry),
                    ..
                } => Some(*entry),
                _ => None,
            })
            .expect("no release sent on return");
        assert_eq!(release_entry, Some(position.to_wire()));
        assert!(!engine.is_controlled());

        // Controller side: receiving that release stops capture and places
        // its own cursor at the matching height (ADR 0009).
        let mut controller = controlling_engine();
        let actions = controller.handle(peer(InboundControl::Release(Some(position.to_wire()))));
        assert!(actions.contains(&ControlAction::StopCapture));
        let placed = placed_cursor(&actions).expect("controller did not place its cursor");
        assert!((placed - 0.75).abs() < 1e-4, "placed at {placed}");
        assert!(!controller.is_controlling());

        // A console revoke/hand-back carries no position, so no placement.
        let mut controller = controlling_engine();
        let actions = controller.handle(peer(InboundControl::Release(None)));
        assert!(actions.contains(&ControlAction::StopCapture));
        assert!(placed_cursor(&actions).is_none());
    }

    // ---- graceful transition: the revoke race (ADR 0009) ----

    #[test]
    fn in_flight_input_after_a_return_is_dropped_not_terminated() {
        // A is controlled by SESSION, then reclaims (edge return / revoke).
        let mut engine = controlled_engine();
        let _ = engine.handle(ControlEvent::UserRelease);
        assert!(!engine.is_controlled());

        // A batch still in flight from SESSION — sent before it saw the
        // release — is dropped: not injected, and not a session kill.
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 9,
            events: vec![WireInputEvent::Motion { dx: 1, dy: 1 }],
        })));
        assert!(
            actions.is_empty(),
            "in-flight input after a return must be silently dropped: {actions:?}"
        );

        // Once SESSION re-requests, the grace ends and the grant flow
        // resumes normally.
        let actions = engine.handle(peer(InboundControl::Request(ControlRequest {
            request_id: 2,
            entry: None,
        })));
        assert!(granted(&actions));
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Button {
                button: WireButton::Left,
                pressed: true,
            }],
        })));
        assert_eq!(actions, vec![inject(vec![press(PointerButton::Left)])]);
    }

    #[test]
    fn the_drop_grace_is_scoped_to_the_released_session() {
        // Releasing SESSION's grant must not excuse a *different* ungranted
        // session: complete mediation still fails closed for it (FR-2.3).
        let mut engine = controlled_engine();
        let _ = engine.handle(ControlEvent::SessionEstablished { session: OTHER });
        let _ = engine.handle(ControlEvent::UserRelease); // release SESSION

        let actions = engine.handle(peer_from(
            OTHER,
            InboundControl::Batch(InputBatch {
                sequence: 1,
                events: vec![WireInputEvent::Motion { dx: 1, dy: 1 }],
            }),
        ));
        assert!(
            matches!(&actions[0], ControlAction::Terminate { session, .. } if *session == OTHER),
            "an unrelated ungranted session must still terminate: {actions:?}"
        );

        // SESSION's own in-flight input, meanwhile, is dropped.
        let actions = engine.handle(peer(InboundControl::Batch(InputBatch {
            sequence: 9,
            events: vec![WireInputEvent::Motion { dx: 1, dy: 1 }],
        })));
        assert!(actions.is_empty());
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
                    ControlAction::Inject(events) => self.injected.apply_inputs(events),
                    _ => {}
                }
            }
        }
    }

    /// Events drawn across two sessions, so the property covers
    /// cross-session interleavings, not just one peer.
    fn arbitrary_event() -> impl Strategy<Value = ControlEvent> {
        let session = prop_oneof![Just(SESSION), Just(OTHER)];
        prop_oneof![
            session
                .clone()
                .prop_map(|s| ControlEvent::UserRequestControl { session: s }),
            Just(ControlEvent::UserRelease),
            session
                .clone()
                .prop_map(|s| ControlEvent::SessionEstablished { session: s }),
            session
                .clone()
                .prop_map(|s| ControlEvent::SessionLost { session: s }),
            Just(ControlEvent::CaptureLost),
            (session.clone(), 1u64..4).prop_map(|(s, request_id)| ControlEvent::RequestTimeout {
                session: s,
                request_id
            }),
            (session.clone(), 1u64..4).prop_map(|(s, id)| ControlEvent::Peer {
                session: s,
                message: InboundControl::Request(ControlRequest {
                    request_id: id,
                    entry: None
                }),
            }),
            (session.clone(), 1u64..4, any::<bool>()).prop_map(|(s, id, granted)| {
                ControlEvent::Peer {
                    session: s,
                    message: InboundControl::Response(ControlResponse {
                        request_id: id,
                        verdict: if granted {
                            ControlVerdict::Granted
                        } else {
                            ControlVerdict::Denied(DenyReason::Busy)
                        },
                    }),
                }
            }),
            session.clone().prop_map(|s| ControlEvent::Peer {
                session: s,
                message: InboundControl::Release(None)
            }),
            (session.clone(), 1u64..1000, 0usize..5, any::<bool>()).prop_map(
                |(s, sequence, button, pressed)| ControlEvent::Peer {
                    session: s,
                    message: InboundControl::Batch(InputBatch {
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
                    }),
                }
            ),
            session.clone().prop_map(|s| ControlEvent::Peer {
                session: s,
                message: InboundControl::ReleaseAll(ReleaseAllInput { after_sequence: 0 }),
            }),
            (0usize..5, any::<bool>()).prop_map(|(button, pressed)| {
                captured(vec![PointerEvent::Button {
                    button: PointerButton::ALL[button],
                    pressed,
                }])
            }),
        ]
    }

    proptest! {
        /// The Phase 3 exit criterion, hermetically, now across two
        /// sessions: whatever interleaving of user actions, peer
        /// messages, grants, input, and faults occurs, losing every
        /// session leaves nothing captured and nothing held down — no
        /// stuck buttons, no dead mouse (FR-4.4, FR-6.1).
        #[test]
        fn any_interleaving_ends_clean_on_disconnect(
            events in proptest::collection::vec(arbitrary_event(), 0..60),
        ) {
            let mut engine = ControlEngine::new(ControlConfig::default());
            let mut model = Model::default();
            for event in events {
                let actions = engine.handle(event);
                model.apply(&actions);
            }
            // The fault: every session dies now.
            model.apply(&engine.handle(ControlEvent::SessionLost { session: SESSION }));
            model.apply(&engine.handle(ControlEvent::SessionLost { session: OTHER }));

            prop_assert!(!model.capture_active, "capture left running after disconnect");
            prop_assert!(model.injected.is_clear(), "buttons left held after disconnect");
            prop_assert!(!engine.is_controlling());
            prop_assert!(!engine.is_controlled());
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

        /// Complete mediation as a property: whatever the interleaving,
        /// the engine only ever emits Inject after a batch from the exact
        /// session it recorded as its controller — never from any other.
        #[test]
        fn injection_only_ever_follows_the_granting_session(
            events in proptest::collection::vec(arbitrary_event(), 0..60),
        ) {
            let mut engine = ControlEngine::new(ControlConfig::default());
            for event in events {
                let is_batch = matches!(
                    &event,
                    ControlEvent::Peer { message: InboundControl::Batch(_), .. }
                );
                let batch_session = match &event {
                    ControlEvent::Peer { session, message: InboundControl::Batch(_) } => Some(*session),
                    _ => None,
                };
                // The controller before handling (injection authority).
                let controller_before = engine.controlled_session_for_test();
                let actions = engine.handle(event);
                let injected = actions.iter().any(|a| matches!(a, ControlAction::Inject(_)));
                if injected && is_batch {
                    // An injection produced by a batch is only legitimate
                    // if that batch came from the session that held the
                    // grant at the time.
                    prop_assert_eq!(
                        batch_session,
                        controller_before,
                        "injected a batch from a non-granting session"
                    );
                }
            }
        }
    }

    impl ControlEngine {
        /// Test-only peek at which session (if any) currently controls
        /// this machine.
        fn controlled_session_for_test(&self) -> Option<Uuid> {
            self.controlled.as_ref().map(|c| c.session)
        }
    }
}
