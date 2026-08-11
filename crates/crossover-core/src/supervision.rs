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

/// Keepalive tuning for an established session.
#[derive(Debug, Clone)]
pub struct KeepaliveConfig {
    /// Idle time after which a `Ping` is sent.
    pub interval: Duration,
    /// Idle time after which the session is declared dead. Must exceed
    /// `interval` to give the peer a chance to answer.
    pub timeout: Duration,
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(15),
        }
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
    /// The local side asked the supervisor to stop.
    #[error("shutdown requested locally")]
    ShutdownRequested,
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
                tracing::warn!(
                    peer_addr = %peer_addr,
                    error = %error,
                    retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX),
                    attempt,
                    "connect failed; will retry"
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
    let (mut reader, mut writer) = session.split();
    let mut last_rx = Instant::now();
    let mut tick = tokio::time::interval(keepalive.interval);
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
                        let result = write_bounded(
                            &mut writer,
                            frame.message_type,
                            &frame.payload,
                            keepalive.timeout,
                        )
                        .await;
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
                            dispatch_frame(frame, &mut writer, events, keepalive.timeout).await
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
                if idle >= keepalive.timeout {
                    break DisconnectReason::KeepaliveTimeout;
                }
                if idle >= keepalive.interval
                    && let Err(reason) = write_bounded(
                        &mut writer,
                        MessageType::Ping.wire(),
                        &[],
                        keepalive.timeout,
                    )
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
    match &reason {
        DisconnectReason::ShutdownRequested => {
            tracing::info!(session_id = %session_id, state = "closed", "session shut down");
        }
        other => {
            tracing::warn!(
                session_id = %session_id,
                error = %other,
                state = "disconnected",
                "session ended"
            );
        }
    }
    reason
}

/// Write one frame, giving up if it cannot complete inside `budget`.
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
/// Bounding it makes that state terminal instead. The budget is the
/// keepalive timeout, because the two failures are the same one seen from
/// either end — a peer that has not taken a byte in that long is gone
/// whether or not it is still answering — and reusing it means one knob,
/// not two. Expiry is fail-closed: the session ends, supervision reconnects
/// (FR-6.2), and input is released.
///
/// This matters more as bulk grows: ADR 0014's chunking keeps individual
/// writes small, but the *stall* is a property of the peer, not the frame.
async fn write_bounded(
    writer: &mut crate::net::SessionWriter,
    message_type: u16,
    payload: &[u8],
    budget: Duration,
) -> Result<(), DisconnectReason> {
    match tokio::time::timeout(budget, writer.send(message_type, payload)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(transport_reason(&error)),
        Err(_) => Err(DisconnectReason::Transport {
            reason: format!(
                "peer accepted no data for {}s: the write stalled, so the session is \
                 unusable and is failing closed",
                budget.as_secs_f32()
            ),
        }),
    }
}

/// Dispatch one inbound frame: control messages are handled here, app
/// frames become events. `Some(reason)` ends the session.
async fn dispatch_frame(
    frame: crossover_protocol::RawFrame,
    writer: &mut crate::net::SessionWriter,
    events: &mpsc::Sender<SessionEvent>,
    write_budget: Duration,
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
            write_bounded(writer, MessageType::Pong.wire(), &[], write_budget)
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
            | MessageType::ClipboardApplied
            | MessageType::InputBatch
            | MessageType::ReleaseAllInput
            | MessageType::ControlRequest
            | MessageType::ControlResponse
            | MessageType::ControlRelease,
        )
        // Not a control message: the application owns dispatch (and
        // validity) of everything else.
        | None => {
            if events.send(SessionEvent::Frame(frame)).await.is_err() {
                Some(DisconnectReason::ShutdownRequested)
            } else {
                None
            }
        }
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

    use crossover_security::{CertifiedIdentity, DeviceIdentity, TrustStore, TrustedPeer};

    use super::{
        DisconnectReason, KeepaliveConfig, ReconnectPolicy, SessionEvent, SupervisorConfig,
        run_session, supervise_outbound,
    };
    use crate::net::{LocalNode, SessionListener, SessionOptions};
    use crate::outbound::outbound_channel;

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
            keepalive: KeepaliveConfig {
                interval: Duration::from_millis(500),
                timeout: Duration::from_secs(5),
            },
            session: SessionOptions {
                establish_timeout: Duration::from_secs(5),
                metrics: None,
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
        config.keepalive = KeepaliveConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(400),
        };
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
        config.keepalive = KeepaliveConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(600),
        };
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
        let server_keepalive = KeepaliveConfig {
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(30),
        };
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
