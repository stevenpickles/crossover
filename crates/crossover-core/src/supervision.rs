//! Session supervision: keepalive, disconnect diagnostics, and automatic
//! reconnection (FR-6.1, FR-6.2; docs/ARCHITECTURE.md §5.3).
//!
//! [`supervise_outbound`] owns the connector role for one peer: establish,
//! run with keepalive, and on loss retry forever with bounded exponential
//! backoff — attempts are deliberately unbounded (reconnection *is* the
//! product requirement), the delay between them is what NFR-1 bounds. The
//! backoff resets only after a session that lasted long enough to count as a
//! real recovery; a session that dies fast is treated as a *flap* and the
//! backoff keeps climbing, so a persistently-broken link is retried ever
//! more slowly instead of churning at the floor delay for days.
//! [`run_session`] is the per-session loop, exposed separately so the
//! listener side runs the same keepalive and dispatch logic without the
//! reconnect wrapper.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};
use uuid::Uuid;

use crossover_protocol::RawFrame;
use crossover_protocol::hello::MessageType;
use crossover_security::{CertifiedIdentity, DeviceIdentity, TrustStore};

use crate::link::LinkDiagnostics;
use crate::net::{
    EstablishedSession, LocalNode, SessionError, SessionInfo, SessionOptions, connect,
};
use crate::outbound::{OutboundReceiver, OutboundSender, outbound_channel};

/// Bounded exponential backoff between reconnection attempts.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Delay before the first retry.
    pub initial_delay: Duration,
    /// Ceiling every later delay is clamped to.
    pub max_delay: Duration,
    /// How long a session must last to count as a genuine recovery and reset
    /// the backoff. A session that dies sooner is a *flap*: the backoff keeps
    /// escalating so a persistently-broken link is retried ever more slowly
    /// instead of churning at the floor delay forever (multi-day hardening).
    pub reset_after: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            // Comfortably longer than a keepalive round-trip (5 s interval,
            // 15 s timeout): a session that outlives this really connected.
            reset_after: Duration::from_secs(30),
        }
    }
}

impl ReconnectPolicy {
    /// Delay before retry number `attempt` (0-based): `initial * 2^attempt`,
    /// saturating, clamped to `max_delay`. Deterministic (NFR-2) — with two
    /// machines there is no thundering herd to jitter away.
    #[must_use]
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
        self.initial_delay
            .saturating_mul(factor)
            .min(self.max_delay)
    }

    /// Whether a session that lasted `lifetime` was stable enough to reset
    /// the backoff — a real recovery, not a flap.
    #[must_use]
    pub fn resets_backoff(&self, lifetime: Duration) -> bool {
        lifetime >= self.reset_after
    }
}

/// How long the polite TLS shutdown at the end of a session may take before
/// the socket is simply dropped. Short by design: the session is already
/// over and its diagnostic already decided, so this buys tidiness, not
/// correctness, and must never hold a task open (NFR-1).
const GRACEFUL_CLOSE_BUDGET: Duration = Duration::from_secs(1);

/// `interval` was not shorter than `timeout`, so the configuration is not
/// merely odd — it is inoperative (see [`KeepaliveConfig`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "keepalive interval ({interval:?}) must be shorter than the timeout ({timeout:?}): \
     the peer needs a chance to answer a Ping, and the write-stall bound needs room \
     between the two to detect anything"
)]
pub struct InvalidKeepalive {
    /// The rejected interval.
    pub interval: Duration,
    /// The rejected timeout.
    pub timeout: Duration,
}

/// Keepalive tuning for an established session.
///
/// The fields are private because the relationship between them is load
/// bearing twice over, and `interval >= timeout` breaks both silently rather
/// than loudly:
///
/// - the peer would never get a chance to answer a `Ping` before the idle
///   timeout declared it dead;
/// - the write-stall bound (`WriteHealth`) would be **inert**, because any
///   write slow enough to count as stalling already exceeds the per-write
///   deadline and is killed by that first.
///
/// A silently-disabled safety bound is worse than a rejected config, so the
/// only ways to build one are [`Self::new`], which validates, and
/// [`Default`], which is valid by construction.
#[derive(Debug, Clone)]
pub struct KeepaliveConfig {
    interval: Duration,
    timeout: Duration,
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(15),
        }
    }
}

impl KeepaliveConfig {
    /// Build a keepalive configuration.
    ///
    /// # Errors
    ///
    /// [`InvalidKeepalive`] if `interval` is not strictly shorter than
    /// `timeout`. Returned, never panicked: this is reachable from a config
    /// file, and a bad file must produce a diagnostic (NFR-1, NFR-3).
    pub fn new(interval: Duration, timeout: Duration) -> Result<Self, InvalidKeepalive> {
        if interval >= timeout {
            return Err(InvalidKeepalive { interval, timeout });
        }
        Ok(Self { interval, timeout })
    }

    /// Idle time after which a `Ping` is sent.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Idle time after which the session is declared dead.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Everything a supervisor needs tuned.
#[derive(Debug, Clone, Default)]
pub struct SupervisorConfig {
    /// Backoff between reconnection attempts.
    pub reconnect: ReconnectPolicy,
    /// Keepalive behavior inside each session.
    pub keepalive: KeepaliveConfig,
    /// Establishment options for each attempt.
    pub session: SessionOptions,
}

/// Why a session ended. Every variant is an observable diagnostic
/// (NFR-3): silence is not an option for state this important.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DisconnectReason {
    /// The peer closed the connection.
    #[error("peer closed the connection")]
    PeerClosed,
    /// Nothing (not even a `Pong`) arrived within the keepalive timeout.
    #[error("keepalive timeout: no traffic within the configured window")]
    KeepaliveTimeout,
    /// The peer violated the protocol; the session failed closed.
    #[error("protocol violation: {reason}")]
    ProtocolViolation { reason: String },
    /// Transport-level failure.
    #[error("transport failure: {reason}")]
    Transport { reason: String },
    /// The application stopped taking this session's events, so inbound
    /// frames could no longer be dispatched. Kept distinct from
    /// [`Self::Transport`] because it is a *local* fault: blaming the
    /// network for it would send a soak report hunting the wrong thing
    /// (docs/ARCHITECTURE.md §9).
    #[error("session event consumer stalled: {reason}")]
    EventConsumerStalled { reason: String },
    /// The local side asked the supervisor to stop.
    #[error("shutdown requested locally")]
    ShutdownRequested,
}

impl DisconnectReason {
    /// Whether a dead *local* network interface could produce this ending —
    /// i.e. whether the link state is worth asking about (see [`crate::link`]).
    ///
    /// Two reasons qualify, because a wire that goes down mid-session
    /// surfaces in exactly two shapes:
    ///
    /// - [`Self::Transport`] — the socket errors out. On Windows this is
    ///   usually `WSAECONNRESET`, whose message blames "the remote host"
    ///   for something the remote host did not do. This is the incident
    ///   that motivated the field.
    /// - [`Self::KeepaliveTimeout`] — nothing errors, nothing arrives.
    ///   Silence is the other face of the same fault, and reading it as "the
    ///   peer stopped answering" sends the reader to the wrong machine just
    ///   as effectively.
    ///
    /// The rest do not: a protocol violation and a stalled event consumer
    /// are decided from data already in hand, and [`Self::PeerClosed`] is a
    /// clean shutdown handshake, which a link that has gone down cannot
    /// deliver. Asking anyway would add a field whose value carries no
    /// information — noise that dilutes the one line where it matters.
    #[must_use]
    pub fn may_be_a_local_link_failure(&self) -> bool {
        matches!(self, Self::Transport { .. } | Self::KeepaliveTimeout)
    }
}

/// What a supervisor (or [`run_session`]) reports to the application.
#[derive(Debug)]
pub enum SessionEvent {
    /// A session reached `ESTABLISHED`.
    Established(SessionInfo),
    /// A non-control frame arrived; dispatch is the application's job.
    Frame(RawFrame),
    /// The session ended. `retry_in` is `Some` when the supervisor will
    /// try again after that delay.
    Disconnected {
        /// Which session ended.
        session_id: Uuid,
        /// Why it ended.
        reason: DisconnectReason,
        /// The supervisor's next move, if any.
        retry_in: Option<Duration>,
    },
    /// An establishment attempt failed (no session existed yet); the
    /// supervisor retries after `retry_in`.
    ConnectFailed {
        /// The establishment failure.
        error: String,
        /// Delay before the next attempt.
        retry_in: Duration,
    },
}

/// The application's grip on a supervisor.
pub struct SupervisorHandle {
    outbound: OutboundSender,
    shutdown: watch::Sender<bool>,
}

/// The supervisor has stopped and can accept no more work.
#[derive(Debug, thiserror::Error)]
#[error("supervisor is no longer running")]
pub struct SupervisorGone;

impl SupervisorHandle {
    /// Queue a frame for the current (or, while reconnecting, the next)
    /// session, on the lane its message type belongs to (ADR 0013).
    ///
    /// Frames flush in order *within their class* once a session exists;
    /// interactive frames overtake queued bulk, which is the whole point of
    /// the split. Waiting here for room on the Background lane is expected
    /// backpressure and never delays the High lane.
    ///
    /// # Errors
    ///
    /// [`SupervisorGone`] if the supervisor has stopped.
    pub async fn send(&self, message_type: u16, payload: Vec<u8>) -> Result<(), SupervisorGone> {
        self.outbound
            .send(message_type, payload)
            .await
            .map_err(|_| SupervisorGone)
    }

    /// Ask the supervisor to close the current session and stop retrying.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// Spawn a supervisor that maintains one outbound session to `peer_addr`
/// forever (until [`SupervisorHandle::shutdown`]): establish → run with
/// keepalive → on loss, retry with bounded backoff.
///
/// Trust is read (snapshotted) at every establishment attempt, so pairing
/// changes and revocations apply to the next connection automatically.
#[must_use]
pub fn supervise_outbound(
    peer_addr: String,
    identity: DeviceIdentity,
    certified: CertifiedIdentity,
    trust: Arc<RwLock<TrustStore>>,
    config: SupervisorConfig,
) -> (SupervisorHandle, mpsc::Receiver<SessionEvent>) {
    let (events_tx, events_rx) = mpsc::channel(64);
    let (outbound_tx, outbound_rx) = outbound_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(run_supervisor(
        peer_addr,
        identity,
        certified,
        trust,
        config,
        events_tx,
        outbound_rx,
        shutdown_rx,
    ));

    (
        SupervisorHandle {
            outbound: outbound_tx,
            shutdown: shutdown_tx,
        },
        events_rx,
    )
}

#[allow(clippy::too_many_arguments)] // internal task entry point, built by supervise_outbound
async fn run_supervisor(
    peer_addr: String,
    identity: DeviceIdentity,
    certified: CertifiedIdentity,
    trust: Arc<RwLock<TrustStore>>,
    config: SupervisorConfig,
    events: mpsc::Sender<SessionEvent>,
    mut outbound: OutboundReceiver,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut attempt: u32 = 0;
    // Where to point the link probe when an attempt never reaches a socket.
    // Seeded from the configured address when it is already a literal, and
    // upgraded to the address a session actually used as soon as one
    // establishes — so a peer configured by hostname still gets a real
    // answer for the reconnect attempts after its first session, which is
    // exactly the stretch a NIC outage produces.
    let mut peer_socket: Option<std::net::SocketAddr> = peer_addr.parse().ok();
    loop {
        if *shutdown.borrow() {
            break;
        }

        // Snapshot trust under a short lock; never hold it across awaits.
        let trust_snapshot = trust.read().unwrap_or_else(PoisonError::into_inner).clone();
        let local = LocalNode {
            identity: &identity,
            certified: &certified,
            trust: &trust_snapshot,
        };

        match connect(peer_addr.as_str(), &local, &config.session).await {
            Ok(session) => {
                let info = session.info().clone();
                peer_socket = session.link().peer().or(peer_socket);
                if events
                    .send(SessionEvent::Established(info.clone()))
                    .await
                    .is_err()
                {
                    // Application dropped the event stream: stop.
                    break;
                }
                let established_at = tokio::time::Instant::now();
                let reason = run_session(
                    session,
                    &events,
                    &mut outbound,
                    &mut shutdown,
                    &config.keepalive,
                )
                .await;
                // Reset the backoff only if the session was stable — a real
                // recovery. A session that dies fast is a flap, and letting
                // the backoff keep climbing stops a broken link from churning.
                if config.reconnect.resets_backoff(established_at.elapsed()) {
                    attempt = 0;
                }
                let is_shutdown = matches!(reason, DisconnectReason::ShutdownRequested);
                let retry_in = (!is_shutdown).then(|| config.reconnect.delay_for_attempt(attempt));
                let _ = events
                    .send(SessionEvent::Disconnected {
                        session_id: info.session_id,
                        reason,
                        retry_in,
                    })
                    .await;
                if is_shutdown {
                    break;
                }
            }
            Err(error) => {
                let retry_in = config.reconnect.delay_for_attempt(attempt);
                // "Connection refused" and "the local NIC is down" read
                // identically from here; the probe separates them.
                log_connect_failed(
                    peer_addr.as_str(),
                    &error,
                    retry_in,
                    attempt,
                    &LinkDiagnostics::new(peer_socket, config.session.link_probe.clone()),
                );
                if events
                    .send(SessionEvent::ConnectFailed {
                        error: error.to_string(),
                        retry_in,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }

        // Wait out the backoff, but wake immediately on shutdown.
        let delay = config.reconnect.delay_for_attempt(attempt);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    tracing::info!(peer_addr = %peer_addr, "supervisor stopped");
}

/// Drive one established session until it ends: answer `Ping`s, send
/// `Ping`s when idle, declare death when nothing arrives inside the
/// keepalive timeout, forward non-control frames as events, and flush
/// outbound frames. Returns why the session ended.
///
/// This is the writer end of the prioritized send path (ADR 0013):
/// [`OutboundReceiver::recv`] hands over everything queued High before a
/// single Background frame, and because exactly **one** frame is written per
/// iteration the High lane is re-checked between every pair of frames. That
/// is what keeps the kernel send buffer shallow enough for the app-level
/// priority to survive to the wire — queueing several bulk frames at once
/// would put input bytes behind them where no scheduler can reach.
///
/// Keepalive deliberately bypasses the lanes entirely: the idle-tick `Ping`
/// below and the `Pong` in [`dispatch_frame`] go straight to the writer,
/// which is the strongest form of High there is.
///
/// Exposed so the listener side runs identical session semantics without
/// the reconnect wrapper.
pub async fn run_session(
    session: EstablishedSession,
    events: &mpsc::Sender<SessionEvent>,
    outbound: &mut OutboundReceiver,
    shutdown: &mut watch::Receiver<bool>,
    keepalive: &KeepaliveConfig,
) -> DisconnectReason {
    let session_id = session.info().session_id;
    // Taken before the split consumes the session; asked only if the ending
    // turns out to be one a dead local wire could have caused.
    let link = session.link();
    let (mut reader, mut writer) = session.split();
    let mut last_rx = Instant::now();
    let mut write_health = WriteHealth::default();
    let mut tick = tokio::time::interval(keepalive.interval());
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let reason = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break DisconnectReason::ShutdownRequested;
                }
            }
            maybe = outbound.recv() => {
                match maybe {
                    Some(frame) => {
                        // Split point: everything before here is the frame
                        // waiting for the writer, everything after is the
                        // socket accepting it.
                        let taken_at = std::time::Instant::now();
                        let result = write_bounded(
                            &mut writer,
                            frame.message_type,
                            &frame.payload,
                            keepalive,
                            Some(&mut write_health),
                        )
                        .await;
                        // Measured here rather than at dequeue: the wait an
                        // input frame actually suffers includes the writer
                        // being busy with a bulk frame already in flight,
                        // which is the other half of what ADR 0013 and
                        // 0014's chunking jointly bound.
                        if result.is_ok() {
                            writer.record_input_queue_latency(
                                frame.message_type,
                                frame.queued_at,
                                taken_at,
                            );
                        }
                        // Dropping the frame returns its Background byte
                        // budget, so the lane only refills once the bytes
                        // are actually out of our hands.
                        drop(frame);
                        if let Err(reason) = result {
                            break reason;
                        }
                    }
                    // All senders gone: treat as local shutdown.
                    None => break DisconnectReason::ShutdownRequested,
                }
            }
            received = reader.recv() => {
                match received {
                    Ok(frame) => {
                        last_rx = Instant::now();
                        if let Some(reason) =
                            dispatch_frame(frame, &mut writer, events, keepalive).await
                        {
                            break reason;
                        }
                    }
                    Err(SessionError::PeerClosed) => break DisconnectReason::PeerClosed,
                    Err(SessionError::Protocol(e)) => {
                        break DisconnectReason::ProtocolViolation {
                            reason: e.to_string(),
                        };
                    }
                    Err(e) => break transport_reason(&e),
                }
            }
            _ = tick.tick() => {
                let idle = last_rx.elapsed();
                if idle >= keepalive.timeout() {
                    break DisconnectReason::KeepaliveTimeout;
                }
                if idle >= keepalive.interval()
                    && let Err(reason) =
                        write_bounded(&mut writer, MessageType::Ping.wire(), &[], keepalive, None)
                            .await
                {
                    break reason;
                }
            }
        }
    };

    // Best-effort graceful close — and bounded, because "best effort" has to
    // mean it can fail. A TLS shutdown has to flush, and the commonest reason
    // to be here is a peer that stopped reading, so an unbounded close would
    // hang on exactly the sessions that most need to end. The reason above is
    // already decided; this only tidies the socket.
    let _ = tokio::time::timeout(GRACEFUL_CLOSE_BUDGET, writer.close()).await;
    log_session_end(session_id, &reason, &link);
    reason
}

/// Report a failed establishment attempt, naming the local link.
///
/// Unlike a session ending there is nothing to filter on: every way an
/// attempt can fail — refused, unreachable, timed out — is equally
/// producible by a local interface that is down, so the field is always
/// worth carrying here.
fn log_connect_failed(
    peer_addr: &str,
    error: &SessionError,
    retry_in: Duration,
    attempt: u32,
    link: &LinkDiagnostics,
) {
    let retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX);
    let local_link = link.state();
    if local_link.blames_local_link() {
        tracing::warn!(
            peer_addr = %peer_addr,
            error = %error,
            local_link = local_link.as_field(),
            retry_in_ms,
            attempt,
            "connect failed; local link is down, so the failure is local, not the peer; \
             will retry"
        );
    } else {
        tracing::warn!(
            peer_addr = %peer_addr,
            error = %error,
            local_link = local_link.as_field(),
            retry_in_ms,
            attempt,
            "connect failed; will retry"
        );
    }
}

/// Report the end of a session, naming the local link when the link is what
/// can settle where the fault was (docs/ARCHITECTURE.md §10).
///
/// The probe is consulted only for endings a dead local interface could
/// actually cause (see [`DisconnectReason::may_be_a_local_link_failure`]),
/// and the `local_link` field appears only on those lines — a field whose
/// value is always `unknown` on a protocol violation would be noise diluting
/// the one line where it means something.
fn log_session_end(session_id: Uuid, reason: &DisconnectReason, link: &LinkDiagnostics) {
    if matches!(reason, DisconnectReason::ShutdownRequested) {
        tracing::info!(session_id = %session_id, state = "closed", "session shut down");
        return;
    }
    if !reason.may_be_a_local_link_failure() {
        tracing::warn!(
            session_id = %session_id,
            error = %reason,
            state = "disconnected",
            "session ended"
        );
        return;
    }
    let local_link = link.state();
    if local_link.blames_local_link() {
        // The line the whole slice exists for. The `error` field above it
        // may well say "forcibly closed by the remote host"; this says, on
        // the same record, that it was not.
        tracing::warn!(
            session_id = %session_id,
            error = %reason,
            local_link = local_link.as_field(),
            state = "disconnected",
            "session ended; local link is down, so the disconnect is local, not the peer \
             (disregard any \"closed by the remote host\" in the error above)"
        );
    } else {
        tracing::warn!(
            session_id = %session_id,
            error = %reason,
            local_link = local_link.as_field(),
            state = "disconnected",
            "session ended"
        );
    }
}

/// How the outbound direction has been behaving, across writes.
///
/// A per-write deadline alone is not a liveness bound, because it resets
/// every frame: a peer that accepts one frame just inside the deadline, over
/// and over, passes every individual check forever while making the session
/// useless. This carries the history that catches that.
#[derive(Debug, Default)]
struct WriteHealth {
    /// How much stalling the outbound direction is currently carrying — a
    /// leaky bucket, not a run. Slow writes fill it; time spent not stalling
    /// drains it.
    ///
    /// A *run* was the obvious measure and the wrong one: any single brisk
    /// write cleared it, so a peer alternating one slow write with one fast
    /// one stalled the session indefinitely without ever being disconnected
    /// (docs/ARCHITECTURE.md §5.4 recorded this as an open residual). What
    /// matters is the share of time the link spends unusable, and a bucket
    /// measures that without needing a window to be defined or stored.
    stalled: Duration,
    /// When the last write finished, so the gap to the next one can be
    /// credited as time *not* stalling.
    last_write_end: Option<Instant>,
}

impl WriteHealth {
    /// Fold one completed application write into the record, and say whether
    /// the outbound direction has stopped being usable.
    ///
    /// Pure in the sense that matters for testing: both timestamps are
    /// arguments, so the policy is exercised with no clock and no sleeps
    /// (docs/TESTING.md §1.1).
    fn record(
        &mut self,
        started: Instant,
        finished: Instant,
        keepalive: &KeepaliveConfig,
    ) -> Result<(), DisconnectReason> {
        // A quiet spell is not a stall: with nothing queued there was nothing
        // to be held up. Crediting the gap drains the bucket, so an idle
        // session recovers fully and a token pause between stalls recovers
        // only what it was worth.
        //
        // The gap is measured to this write's *start*, not its end. Measuring
        // to the end would credit the write's own duration as idle time, so a
        // slow write would pay down the very debt it is creating.
        if let Some(end) = self.last_write_end {
            self.stalled = self
                .stalled
                .saturating_sub(started.saturating_duration_since(end));
        }
        self.last_write_end = Some(finished);

        let elapsed = finished.saturating_duration_since(started);
        if elapsed >= keepalive.interval() {
            // Slower than the keepalive interval is not throughput; charge
            // the whole write, since none of that time was usable.
            self.stalled = self.stalled.saturating_add(elapsed);
        } else {
            // A brisk write is evidence of a working link, worth exactly the
            // time it took — not an amnesty for everything before it.
            self.stalled = self.stalled.saturating_sub(elapsed);
        }

        if self.stalled < keepalive.timeout() {
            return Ok(());
        }
        Err(DisconnectReason::Transport {
            reason: format!(
                "outbound writes have spent {}s stalling with too little healthy                  throughput to make it up (a write slower than the {}s keepalive                  interval counts as stalling); the peer is consuming too slowly for                  the session to be usable",
                self.stalled.as_secs_f32(),
                keepalive.interval().as_secs_f32()
            ),
        })
    }
}

/// Write one frame under two bounds, both fail-closed.
///
/// The write runs as the body of a `select!` branch, so while it is pending
/// the session loop polls nothing else — not the reader, not the keepalive
/// tick. A peer that stops reading its socket therefore stalls the write
/// once the kernel buffers fill, and an *unbounded* write would freeze
/// `last_rx` with it: the keepalive timeout could never fire, the session
/// would never disconnect, and the release-all-input that a disconnect
/// triggers would never run. A hostile peer would hold input hostage by
/// doing nothing at all.
///
/// So, exactly:
///
/// 1. **No single write may exceed `keepalive.timeout`.** This catches a
///    peer that has frozen outright. A cancelled write leaves the TLS stream
///    mid-record and unusable, so expiry is necessarily fatal, not a retry.
/// 2. **Stalling may not outweigh healthy throughput by more than
///    `keepalive.timeout`.** A write slower than `keepalive.interval` counts
///    as stalling and charges its whole duration; every other interval —
///    brisk writes and idle gaps alike — pays that debt back. This is what
///    catches the trickle that bound 1 waves through, *and* the peer that
///    alternates one slow write with one brisk one to keep a continuity
///    measure permanently reset.
///
/// `health` is `None` for keepalive frames. A `Ping` is a dozen bytes and
/// fits in any window that is open at all, so it is no evidence of usable
/// throughput: it must neither count as a stall nor clear one. A genuinely
/// idle spell — no write at all — does pay the debt down, because an empty
/// outbound queue is health, not a stalled one.
///
/// What this deliberately does **not** provide is a responsive loop during a
/// write; see docs/ARCHITECTURE.md §5.4.
async fn write_bounded(
    writer: &mut crate::net::SessionWriter,
    message_type: u16,
    payload: &[u8],
    keepalive: &KeepaliveConfig,
    health: Option<&mut WriteHealth>,
) -> Result<(), DisconnectReason> {
    let started = Instant::now();
    match tokio::time::timeout(keepalive.timeout(), writer.send(message_type, payload)).await {
        Ok(Ok(_)) => {}
        // The send-path gate refused this frame *locally* — nothing
        // reached the socket, and the session is healthy. Dropping one
        // frame is the correct outcome and killing the session over it
        // would be a self-inflicted outage; the loud diagnostic is what
        // makes the local bug visible (NFR-3).
        Ok(Err(error @ SessionError::FeatureNotNegotiated { .. })) => {
            tracing::warn!(
                message_type,
                byte_count = payload.len(),
                error = %error,
                "outbound frame refused before the wire"
            );
            return Ok(());
        }
        Ok(Err(error)) => return Err(transport_reason(&error)),
        Err(_) => {
            return Err(DisconnectReason::Transport {
                reason: format!(
                    "a single {}-byte frame did not finish sending within {}s; the peer is \
                     not consuming this connection",
                    payload.len(),
                    keepalive.timeout().as_secs_f32()
                ),
            });
        }
    }

    match health {
        Some(health) => health.record(started, Instant::now(), keepalive),
        None => Ok(()),
    }
}

/// Dispatch one inbound frame: control messages are handled here, app
/// frames become events. `Some(reason)` ends the session.
async fn dispatch_frame(
    frame: crossover_protocol::RawFrame,
    writer: &mut crate::net::SessionWriter,
    events: &mpsc::Sender<SessionEvent>,
    keepalive: &KeepaliveConfig,
) -> Option<DisconnectReason> {
    let violation = |reason: &str| {
        Some(DisconnectReason::ProtocolViolation {
            reason: reason.to_owned(),
        })
    };
    match MessageType::from_wire(frame.message_type) {
        Some(MessageType::Ping) => {
            if !frame.payload.is_empty() {
                return violation("Ping with non-empty payload");
            }
            // Bounded like every other write: answering a Ping must not be
            // the thing that wedges the loop.
            write_bounded(writer, MessageType::Pong.wire(), &[], keepalive, None)
                .await
                .err()
        }
        Some(MessageType::Pong) => {
            if frame.payload.is_empty() {
                None
            } else {
                violation("Pong with non-empty payload")
            }
        }
        Some(MessageType::Hello) => violation("Hello after establishment"),
        // Pairing happens on a plain-TCP ceremony before trust exists; on
        // an established session it is a violation.
        Some(MessageType::PairingStart | MessageType::PairingConfirm) => {
            violation("pairing message on an established session")
        }
        // Clipboard, input, and control-transfer traffic belongs to the
        // engines, which consume it as Frame events.
        Some(
            MessageType::ClipboardOffer
            | MessageType::ClipboardAccept
            | MessageType::ClipboardDecline
            | MessageType::ClipboardData
            | MessageType::ClipboardChunk
            | MessageType::ClipboardApplied
            | MessageType::InputBatch
            | MessageType::ReleaseAllInput
            | MessageType::ControlRequest
            | MessageType::ControlResponse
            | MessageType::ControlRelease,
        )
        // Not a control message: the application owns dispatch (and
        // validity) of everything else.
        | None => deliver_bounded(events, SessionEvent::Frame(frame), keepalive).await,
    }
}

/// Hand one frame to the application, giving up if it cannot be accepted
/// inside the keepalive timeout.
///
/// This is the last unbounded await in [`run_session`], and the one hop
/// where backpressure points *inwards*. Parking here parks the whole
/// session loop — the outbound drain, the `Pong` answer, and the keepalive
/// tick that would otherwise notice — so an application that stops
/// consuming freezes the session with nothing left running to time it out.
///
/// That state is reachable, and not only by local misbehaviour. Under
/// sustained High-lane saturation the Background lane is deliberately
/// starved (ADR 0013), so a clipboard driver parked on it stops draining
/// its own events; the fanout then parks, the session's event drain stops,
/// this channel fills, and the loop stops turning. Every hop is legitimate
/// backpressure; the cycle is not.
///
/// The budget is `keepalive.timeout()`, matching the write bounds: one knob
/// for "this session has stopped moving", whichever direction it stopped
/// in. Expiry is fail-closed, and the disconnect is what breaks the cycle —
/// teardown retires the send path, which unparks everything queued behind
/// it (docs/ARCHITECTURE.md §5.4).
///
/// **A peer cannot induce this on a healthy session.** All a peer controls
/// is how fast frames arrive, and a consumer that is *running* accepts each
/// one in microseconds however hard it is pushed: a flood meets
/// backpressure, which slows the peer down, and never approaches a
/// multi-second wait for a single hand-off. Reaching the deadline requires
/// the consumer chain to have genuinely stopped. A peer *can* stop it, by
/// driving the wedge above — and killing the session is then exactly right,
/// because a session whose frames are neither dispatched nor answered is
/// already doing nothing but holding the chain hostage.
async fn deliver_bounded(
    events: &mpsc::Sender<SessionEvent>,
    event: SessionEvent,
    keepalive: &KeepaliveConfig,
) -> Option<DisconnectReason> {
    match tokio::time::timeout(keepalive.timeout(), events.send(event)).await {
        Ok(Ok(())) => None,
        // The application dropped the stream: it is shutting down.
        Ok(Err(_)) => Some(DisconnectReason::ShutdownRequested),
        Err(_) => Some(DisconnectReason::EventConsumerStalled {
            reason: format!(
                "no inbound frame accepted for {}s; the session's event consumer has \
                 stopped, so nothing here can be dispatched or answered",
                keepalive.timeout().as_secs_f32()
            ),
        }),
    }
}

fn transport_reason(error: &SessionError) -> DisconnectReason {
    DisconnectReason::Transport {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use proptest::prelude::*;
    use tokio::sync::{mpsc, watch};
    use tokio::time::timeout;
    use uuid::Uuid;

    use crossover_platform::LinkState;
    use crossover_platform::fakes::FakeLinkStateProbe;
    use crossover_security::{CertifiedIdentity, DeviceIdentity, TrustStore, TrustedPeer};

    use super::{
        DisconnectReason, KeepaliveConfig, ReconnectPolicy, SessionEvent, SupervisorConfig,
        log_connect_failed, log_session_end, run_session, supervise_outbound,
    };
    use crate::link::LinkDiagnostics;
    use crate::net::{LocalNode, SessionError, SessionListener, SessionOptions};
    use crate::outbound::outbound_channel;

    // --- keepalive configuration ---

    /// `interval >= timeout` does not just read oddly, it disables things:
    /// the peer never gets to answer a `Ping`, and the write-stall bound
    /// becomes inert because any write slow enough to count as stalling has
    /// already blown the per-write deadline. Rejected at construction, with
    /// an error rather than a panic — this is reachable from a config file.
    #[test]
    fn a_keepalive_whose_interval_swallows_its_timeout_is_rejected() {
        use super::{InvalidKeepalive, KeepaliveConfig};

        let equal = KeepaliveConfig::new(Duration::from_secs(5), Duration::from_secs(5));
        assert_eq!(
            equal.unwrap_err(),
            InvalidKeepalive {
                interval: Duration::from_secs(5),
                timeout: Duration::from_secs(5),
            }
        );
        assert!(KeepaliveConfig::new(Duration::from_secs(30), Duration::from_secs(15)).is_err());
        assert!(KeepaliveConfig::new(Duration::ZERO, Duration::ZERO).is_err());

        // The message names both values, so a bad config file is actionable.
        let message = KeepaliveConfig::new(Duration::from_secs(9), Duration::from_secs(3))
            .unwrap_err()
            .to_string();
        assert!(message.contains("9s"), "{message}");
        assert!(message.contains("3s"), "{message}");

        // A sane pair is accepted, and the default is one.
        let ok = KeepaliveConfig::new(Duration::from_secs(1), Duration::from_secs(2)).unwrap();
        assert_eq!(ok.interval(), Duration::from_secs(1));
        assert_eq!(ok.timeout(), Duration::from_secs(2));
        let default = KeepaliveConfig::default();
        assert!(default.interval() < default.timeout());
    }

    // --- disconnect diagnostics: the local link ---

    /// A subscriber writing into a buffer, so a test can read the line a
    /// maintainer would read.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Run `body` with everything it logs captured, and return the text.
    fn captured(body: impl FnOnce()) -> String {
        let sink = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let bytes = sink
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        String::from_utf8(bytes).expect("log output was not UTF-8")
    }

    /// The error the incident produced, verbatim: what Windows says when the
    /// *local* NIC drops its link mid-session, on both machines at once.
    fn wsaeconnreset() -> DisconnectReason {
        DisconnectReason::Transport {
            reason: "transport I/O failed: An existing connection was forcibly closed by the \
                     remote host. (os error 10054)"
                .to_owned(),
        }
    }

    fn diagnostics(state: LinkState) -> (LinkDiagnostics, Arc<FakeLinkStateProbe>) {
        let probe = Arc::new(FakeLinkStateProbe::answering(state));
        let peer = "192.168.1.146:27677".parse().unwrap();
        (LinkDiagnostics::new(Some(peer), Some(probe.clone())), probe)
    }

    /// The incident this slice exists for. Machine A's dock NIC dropped its
    /// link; both peers logged "forcibly closed by the remote host", which
    /// was false on both ends, and disproving it took a manual correlation
    /// of two machines' Windows event logs. The log has to say it itself.
    #[test]
    fn a_transport_failure_over_a_dead_local_wire_is_named_as_local() {
        let (link, probe) = diagnostics(LinkState::Down);
        let output = captured(|| log_session_end(Uuid::nil(), &wsaeconnreset(), &link));

        assert!(output.contains(r#"local_link="down""#), "{output}");
        assert!(
            output.contains("the disconnect is local, not the peer"),
            "the line does not overturn the OS's misattribution: {output}"
        );
        // The original error is still there — the diagnosis corrects it, it
        // does not hide it.
        assert!(output.contains("os error 10054"), "{output}");
        // Asked about this session's peer, not about "any interface".
        assert_eq!(
            probe.asked_about(),
            vec!["192.168.1.146:27677".parse().unwrap()]
        );
    }

    /// Silence is the other face of the same fault: with the wire down
    /// nothing errors and nothing arrives, and "the peer stopped answering"
    /// sends the reader to the wrong machine just as effectively.
    #[test]
    fn a_keepalive_timeout_over_a_dead_local_wire_is_named_as_local() {
        let (link, _probe) = diagnostics(LinkState::Down);
        let output = captured(|| {
            log_session_end(Uuid::nil(), &DisconnectReason::KeepaliveTimeout, &link);
        });
        assert!(output.contains(r#"local_link="down""#), "{output}");
        assert!(
            output.contains("the disconnect is local, not the peer"),
            "{output}"
        );
    }

    /// A healthy local link records the fact and blames nobody: the field
    /// is evidence either way, so it has to be present when it exonerates
    /// this machine too.
    #[test]
    fn a_live_local_link_is_recorded_without_a_verdict() {
        let (link, _probe) = diagnostics(LinkState::Up);
        let output = captured(|| log_session_end(Uuid::nil(), &wsaeconnreset(), &link));

        assert!(output.contains(r#"local_link="up""#), "{output}");
        assert!(output.contains("session ended"), "{output}");
        assert!(
            !output.contains("not the peer"),
            "a healthy link must not claim the disconnect was local: {output}"
        );
    }

    /// A probe that cannot determine the state — no implementation on this
    /// OS, no route, an internal failure it swallowed — says so, and says
    /// nothing more. Not knowing must never read as "the local link was
    /// fine", which is exactly the false confidence the incident created.
    #[test]
    fn a_probe_that_cannot_tell_degrades_to_unknown_and_accuses_nobody() {
        let (link, _probe) = diagnostics(LinkState::Unknown);
        let output = captured(|| log_session_end(Uuid::nil(), &wsaeconnreset(), &link));
        assert!(output.contains(r#"local_link="unknown""#), "{output}");
        assert!(!output.contains("not the peer"), "{output}");

        // Same outcome with no probe wired at all (every non-Windows build
        // today), and the line is otherwise unchanged.
        let output = captured(|| {
            log_session_end(Uuid::nil(), &wsaeconnreset(), &LinkDiagnostics::default());
        });
        assert!(output.contains(r#"local_link="unknown""#), "{output}");
        assert!(output.contains("session ended"), "{output}");
    }

    /// The field is carried only where it could mean something. A protocol
    /// violation is decided from bytes already in hand, and a clean peer
    /// close is a handshake a dead wire cannot deliver — a permanently
    /// uninformative `local_link` on those lines would dilute the one line
    /// where it settles the question.
    #[test]
    fn endings_a_dead_wire_cannot_cause_do_not_carry_the_field() {
        assert!(wsaeconnreset().may_be_a_local_link_failure());
        assert!(DisconnectReason::KeepaliveTimeout.may_be_a_local_link_failure());
        assert!(!DisconnectReason::PeerClosed.may_be_a_local_link_failure());
        assert!(!DisconnectReason::ShutdownRequested.may_be_a_local_link_failure());
        assert!(
            !DisconnectReason::ProtocolViolation {
                reason: "Hello after establishment".to_owned()
            }
            .may_be_a_local_link_failure()
        );

        let (link, probe) = diagnostics(LinkState::Down);
        let output =
            captured(|| log_session_end(Uuid::nil(), &DisconnectReason::PeerClosed, &link));
        assert!(output.contains("session ended"), "{output}");
        assert!(!output.contains("local_link"), "{output}");
        assert!(
            probe.asked_about().is_empty(),
            "the probe was consulted for an ending it cannot explain"
        );

        // A local shutdown is not a failure at all: still info, still silent
        // about the link.
        let output =
            captured(|| log_session_end(Uuid::nil(), &DisconnectReason::ShutdownRequested, &link));
        assert!(output.contains("session shut down"), "{output}");
        assert!(!output.contains("local_link"), "{output}");
    }

    /// The other half of the incident: once the link is down, every
    /// reconnect attempt fails too, and "connection refused" reads exactly
    /// like a peer that is switched off.
    #[test]
    fn a_connect_attempt_over_a_dead_local_wire_is_named_as_local() {
        let error = SessionError::Io {
            source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        };
        let (link, _probe) = diagnostics(LinkState::Down);
        let output = captured(|| {
            log_connect_failed(
                "192.168.1.146:27677",
                &error,
                Duration::from_secs(4),
                3,
                &link,
            );
        });

        assert!(output.contains(r#"local_link="down""#), "{output}");
        assert!(
            output.contains("the failure is local, not the peer"),
            "{output}"
        );
        // The fields the retry story is told with are untouched.
        assert!(output.contains("retry_in_ms=4000"), "{output}");
        assert!(output.contains("attempt=3"), "{output}");

        // With a healthy link the line stays the old one, plus the evidence.
        let (link, _probe) = diagnostics(LinkState::Up);
        let output = captured(|| {
            log_connect_failed(
                "192.168.1.146:27677",
                &error,
                Duration::from_secs(4),
                3,
                &link,
            );
        });
        assert!(output.contains("connect failed; will retry"), "{output}");
        assert!(output.contains(r#"local_link="up""#), "{output}");
        assert!(!output.contains("not the peer"), "{output}");
    }

    // --- write health (pure) ---

    /// 5 s interval / 15 s timeout, the shipped defaults.
    fn keepalive() -> KeepaliveConfig {
        KeepaliveConfig::default()
    }

    /// A healthy link never trips either bound, however long it runs.
    #[test]
    fn brisk_writes_never_look_like_a_stall() {
        let mut health = super::WriteHealth::default();
        let mut at = tokio::time::Instant::now();
        for _ in 0..1000 {
            let finished = at + Duration::from_millis(1);
            assert!(health.record(at, finished, &keepalive()).is_ok());
            at = finished + Duration::from_millis(50);
        }
    }

    /// The trickle a per-write deadline waves through: every frame lands
    /// just inside the 15 s per-write bound, so no single write ever fails —
    /// and the session is useless. Continuous stalling has to end it.
    #[test]
    fn a_peer_that_trickles_just_inside_the_per_write_bound_is_cut_off() {
        let mut health = super::WriteHealth::default();
        let mut at = tokio::time::Instant::now();

        // First slow write: stalling starts, but there is no run yet.
        let finished = at + Duration::from_millis(14_900);
        assert!(health.record(at, finished, &keepalive()).is_ok());

        // Second: the run is now longer than the keepalive timeout.
        at = finished;
        let finished = at + Duration::from_millis(14_900);
        let verdict = health.record(at, finished, &keepalive());
        let Err(DisconnectReason::Transport { reason }) = verdict else {
            panic!("a trickling peer was not cut off: {verdict:?}");
        };
        assert!(reason.contains("stalling"), "{reason}");
    }

    /// The evasion a continuity measure could not see, and the reason this
    /// bound is a duty cycle: a peer alternating one slow write with one
    /// brisk one used to reset the run forever and stall the session
    /// indefinitely without ever being disconnected
    /// (docs/ARCHITECTURE.md §5.4 carried it as an open residual).
    ///
    /// A millisecond of throughput buys a millisecond of forgiveness, not an
    /// amnesty, so the debt still climbs and the session still ends.
    #[test]
    fn alternating_one_slow_write_with_one_brisk_one_does_not_evade_the_bound() {
        let mut health = super::WriteHealth::default();
        let mut at = tokio::time::Instant::now();

        for round in 0..3 {
            let finished = at + Duration::from_secs(6);
            let verdict = health.record(at, finished, &keepalive());
            at = finished;

            if let Err(DisconnectReason::Transport { reason }) = verdict {
                assert!(round >= 2, "cut off too early, in round {round}: {reason}");
                return;
            }

            // The token of "health" that used to wipe the slate.
            let finished = at + Duration::from_millis(1);
            assert!(health.record(at, finished, &keepalive()).is_ok());
            at = finished;
        }
        panic!("a peer alternating stalls with brisk writes was never cut off");
    }

    /// Forgiveness has to be real, or a link that hiccups once and then
    /// works for a minute would be killed by ancient history.
    #[test]
    fn healthy_throughput_pays_off_an_earlier_stall() {
        let mut health = super::WriteHealth::default();
        let at = tokio::time::Instant::now();

        // One bad write, most of the way to the bound.
        let finished = at + Duration::from_secs(14);
        assert!(health.record(at, finished, &keepalive()).is_ok());

        // A minute of nothing to send: the debt is paid.
        let at = finished + Duration::from_mins(1);
        let finished = at + Duration::from_secs(14);
        assert!(
            health.record(at, finished, &keepalive()).is_ok(),
            "a recovered link was still carrying its old debt"
        );
    }

    /// An idle session is not a stalling one: with nothing queued there was
    /// nothing to hold up, so a long gap must not be charged as a stall.
    #[test]
    fn a_quiet_spell_between_writes_is_not_a_stall() {
        let mut health = super::WriteHealth::default();
        let at = tokio::time::Instant::now();
        let finished = at + Duration::from_secs(6);
        assert!(health.record(at, finished, &keepalive()).is_ok());

        // Nothing to send for a minute, then one slow write.
        let at = finished + Duration::from_mins(1);
        let finished = at + Duration::from_secs(6);
        assert!(
            health.record(at, finished, &keepalive()).is_ok(),
            "an idle gap was charged as continuous stalling"
        );
    }

    /// The gap that clears a stall run is measured from one write's end to
    /// the *next one's start*. Folding the write's own duration into the gap
    /// would let a slow write clear the run it belongs to — and then a peer
    /// pausing briefly between stalls resets the bound for ever.
    #[test]
    fn a_token_pause_between_stalls_does_not_clear_the_run() {
        let mut health = super::WriteHealth::default();
        let mut at = tokio::time::Instant::now();

        // 14.9 s writes with a 200 ms breather: each write on its own is
        // inside the per-write bound, and the pause is nowhere near the
        // 15 s idle threshold, so the run must keep accumulating.
        let mut verdicts = Vec::new();
        for _ in 0..4 {
            let finished = at + Duration::from_millis(14_900);
            verdicts.push(health.record(at, finished, &keepalive()));
            at = finished + Duration::from_millis(200);
        }
        assert!(
            verdicts.iter().any(Result::is_err),
            "a token pause between stalling writes cleared the run for ever"
        );

        // The boundary from the other side: a gap measured from the previous
        // end to this start that does reach the timeout clears the run.
        let mut health = super::WriteHealth::default();
        let at = tokio::time::Instant::now();
        let finished = at + Duration::from_millis(14_900);
        assert!(health.record(at, finished, &keepalive()).is_ok());
        let at = finished + KeepaliveConfig::default().timeout;
        let finished = at + Duration::from_millis(14_900);
        assert!(
            health.record(at, finished, &keepalive()).is_ok(),
            "a genuine idle gap of the full timeout failed to clear the run"
        );
    }

    // --- ReconnectPolicy (pure) ---

    #[test]
    fn backoff_doubles_from_initial_and_clamps_at_max() {
        let policy = ReconnectPolicy {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            ..ReconnectPolicy::default()
        };
        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(200));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(400));
        assert_eq!(policy.delay_for_attempt(4), Duration::from_millis(1600));
        assert_eq!(policy.delay_for_attempt(5), Duration::from_secs(2));
        // Far past the clamp — including shift overflow territory.
        assert_eq!(policy.delay_for_attempt(40), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(u32::MAX), Duration::from_secs(2));
    }

    #[test]
    fn only_a_session_that_outlives_the_threshold_resets_the_backoff() {
        let policy = ReconnectPolicy {
            reset_after: Duration::from_secs(30),
            ..ReconnectPolicy::default()
        };
        // A flap (dies before the threshold) keeps the backoff climbing.
        assert!(!policy.resets_backoff(Duration::from_secs(0)));
        assert!(!policy.resets_backoff(Duration::from_secs(29)));
        // A session that reaches the threshold is a real recovery.
        assert!(policy.resets_backoff(Duration::from_secs(30)));
        assert!(policy.resets_backoff(Duration::from_secs(45)));
    }

    proptest! {
        #[test]
        fn backoff_is_monotonic_and_bounded(
            initial_ms in 1u64..1000,
            max_ms in 1u64..60_000,
            a in 0u32..64,
            b in 0u32..64,
        ) {
            let policy = ReconnectPolicy {
                initial_delay: Duration::from_millis(initial_ms),
                max_delay: Duration::from_millis(max_ms),
                ..ReconnectPolicy::default()
            };
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(policy.delay_for_attempt(lo) <= policy.delay_for_attempt(hi));
            prop_assert!(policy.delay_for_attempt(hi) <= policy.max_delay.max(policy.initial_delay.min(policy.max_delay)));
            prop_assert!(policy.delay_for_attempt(hi) <= Duration::from_millis(max_ms).max(Duration::from_millis(initial_ms)));
        }
    }

    // --- integration helpers ---

    struct Node {
        identity: DeviceIdentity,
        certified: CertifiedIdentity,
        trust: TrustStore,
    }

    impl Node {
        fn new(name: &str) -> Self {
            let identity = DeviceIdentity::generate(name).unwrap();
            let certified = CertifiedIdentity::from_identity(&identity).unwrap();
            Self {
                identity,
                certified,
                trust: TrustStore::new(),
            }
        }

        fn trust_peer(&mut self, other: &Node) {
            self.trust
                .add_peer(
                    TrustedPeer::new(
                        other.identity.device_id(),
                        other.identity.device_name(),
                        other.certified.fingerprint(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }

        fn local(&self) -> LocalNode<'_> {
            LocalNode {
                identity: &self.identity,
                certified: &self.certified,
                trust: &self.trust,
            }
        }
    }

    fn fast_config() -> SupervisorConfig {
        SupervisorConfig {
            reconnect: ReconnectPolicy {
                initial_delay: Duration::from_millis(50),
                max_delay: Duration::from_millis(200),
                // Any session in these fast tests counts as stable (resetting
                // the backoff), matching the pre-flap-protection behavior.
                reset_after: Duration::from_millis(1),
            },
            keepalive: KeepaliveConfig::new(Duration::from_millis(500), Duration::from_secs(5))
                .unwrap(),
            session: SessionOptions {
                establish_timeout: Duration::from_secs(5),
                ..SessionOptions::default()
            },
        }
    }

    async fn next_event(rx: &mut mpsc::Receiver<SessionEvent>) -> SessionEvent {
        timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for a session event")
            .expect("event channel closed unexpectedly")
    }

    #[tokio::test]
    async fn supervisor_reestablishes_after_session_loss() {
        let mut a = Node::new("connector");
        let mut b = Node::new("listener");
        a.trust_peer(&b);
        b.trust_peer(&a);

        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (handle, mut events) = supervise_outbound(
            addr,
            a.identity,
            a.certified,
            Arc::new(RwLock::new(a.trust)),
            fast_config(),
        );

        let (b_local, opts) = (b.local(), SessionOptions::default());
        let server_session = listener.accept(&b_local, &opts).await.unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            SessionEvent::Established(_)
        ));

        // Server drops the session: the supervisor must report the loss
        // with a scheduled retry, then re-establish on its own.
        server_session.close().await.unwrap();
        match next_event(&mut events).await {
            SessionEvent::Disconnected {
                reason, retry_in, ..
            } => {
                assert_eq!(reason, DisconnectReason::PeerClosed);
                assert!(retry_in.is_some());
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }

        let _second = listener.accept(&b_local, &opts).await.unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            SessionEvent::Established(_)
        ));

        // Shutdown ends the session and the supervisor, with no retry.
        handle.shutdown();
        match next_event(&mut events).await {
            SessionEvent::Disconnected {
                reason, retry_in, ..
            } => {
                assert_eq!(reason, DisconnectReason::ShutdownRequested);
                assert_eq!(retry_in, None);
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keepalive_declares_a_silent_peer_dead() {
        let mut a = Node::new("connector");
        let mut b = Node::new("silent-listener");
        a.trust_peer(&b);
        b.trust_peer(&a);

        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let mut config = fast_config();
        config.keepalive =
            KeepaliveConfig::new(Duration::from_millis(100), Duration::from_millis(400)).unwrap();
        let (handle, mut events) = supervise_outbound(
            addr,
            a.identity,
            a.certified,
            Arc::new(RwLock::new(a.trust)),
            config,
        );

        // Accept, then never read: pings go unanswered.
        let (b_local, opts) = (b.local(), SessionOptions::default());
        let _held_open = listener.accept(&b_local, &opts).await.unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            SessionEvent::Established(_)
        ));

        match next_event(&mut events).await {
            SessionEvent::Disconnected { reason, .. } => {
                assert_eq!(reason, DisconnectReason::KeepaliveTimeout);
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
        handle.shutdown();
    }

    #[tokio::test]
    async fn answered_pings_keep_an_idle_session_alive_and_frames_flow() {
        let mut a = Node::new("connector");
        let mut b = Node::new("responsive-listener");
        a.trust_peer(&b);
        b.trust_peer(&a);

        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        // Aggressive client keepalive: several ping cycles fit in the
        // observation window below.
        let mut config = fast_config();
        config.keepalive =
            KeepaliveConfig::new(Duration::from_millis(100), Duration::from_millis(600)).unwrap();
        let (handle, mut events) = supervise_outbound(
            addr,
            a.identity,
            a.certified,
            Arc::new(RwLock::new(a.trust)),
            config,
        );

        // Listener side runs the same session loop (answers pings) with a
        // relaxed keepalive of its own.
        let (b_local, opts) = (b.local(), SessionOptions::default());
        let server_session = listener.accept(&b_local, &opts).await.unwrap();
        let (server_events_tx, mut server_events_rx) = mpsc::channel(16);
        let (_server_out_tx, mut server_out_rx) = outbound_channel();
        let (_server_shutdown_tx, mut server_shutdown_rx) = watch::channel(false);
        let server_keepalive =
            KeepaliveConfig::new(Duration::from_secs(2), Duration::from_secs(30)).unwrap();
        let server_task = tokio::spawn(async move {
            run_session(
                server_session,
                &server_events_tx,
                &mut server_out_rx,
                &mut server_shutdown_rx,
                &server_keepalive,
            )
            .await
        });

        assert!(matches!(
            next_event(&mut events).await,
            SessionEvent::Established(_)
        ));

        // An application frame reaches the listener's event stream.
        handle.send(0x0100, b"hello there".to_vec()).await.unwrap();
        match timeout(Duration::from_secs(5), server_events_rx.recv())
            .await
            .expect("timed out waiting for server frame")
            .expect("server event channel closed")
        {
            SessionEvent::Frame(frame) => {
                assert_eq!(frame.message_type, 0x0100);
                assert_eq!(frame.payload, b"hello there");
            }
            other => panic!("expected Frame, got {other:?}"),
        }

        // Idle for several keepalive cycles: pings are being answered, so
        // no Disconnected event may arrive.
        let quiet = timeout(Duration::from_millis(800), events.recv()).await;
        assert!(
            quiet.is_err(),
            "expected silence while pings are answered, got {quiet:?}"
        );

        handle.shutdown();
        let reason = server_task.await.unwrap();
        // From the server's perspective the client simply went away.
        assert!(matches!(
            reason,
            DisconnectReason::PeerClosed | DisconnectReason::Transport { .. }
        ));
    }
}
