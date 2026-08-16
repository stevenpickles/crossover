//! Local execution metrics, dumped on shutdown (FR-7.3, FR-7.5).
//!
//! One shared [`Metrics`] registry of atomic counters that the whole
//! process increments at the source — the session layer counts frames
//! and bytes, the clipboard engine records latency, the application
//! counts sessions and control transfers — and the application renders on
//! the way out. Everything here is **local**: nothing leaves the machine
//! (FR-7.5), and nothing records clipboard *content* or key material,
//! only counts and byte totals (FR-7.4).
//!
//! Counters are `Relaxed` atomics: each is an independent tally with no
//! ordering relationship to any other, and an approximate count under a
//! torn read at shutdown is worth far more than the cost of stronger
//! ordering on the hot send path. Latency samples need a `Mutex` (a
//! percentile needs the distribution, not a running sum); it is taken
//! only on the low-frequency clipboard-transaction path, never on the
//! input hot path.

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossover_protocol::hello::MessageType;

use crate::supervision::DisconnectReason;

/// Upper bound on retained latency samples (NFR-1). Clipboard
/// transactions are human-paced, so a run realistically produces
/// thousands at most; beyond this the oldest are dropped and the report
/// notes the truncation, so percentiles never cost unbounded memory.
const MAX_LATENCY_SAMPLES: usize = 100_000;

/// Traffic grouped for a readable breakdown. The raw message type is a
/// `u16` with fifteen values; a summary wants classes, not a histogram
/// of every discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// `Ping` / `Pong` keepalive.
    Keepalive,
    /// `Hello` and the pairing ceremony.
    Setup,
    /// Clipboard transaction messages.
    Clipboard,
    /// Input batches and `ReleaseAllInput`.
    Input,
    /// Control-transfer negotiation.
    Control,
    /// An unrecognized message type (a newer or misbehaving peer).
    Other,
}

impl FrameClass {
    /// Every class, in report order.
    const ALL: [Self; 6] = [
        Self::Keepalive,
        Self::Setup,
        Self::Clipboard,
        Self::Input,
        Self::Control,
        Self::Other,
    ];

    fn index(self) -> usize {
        match self {
            Self::Keepalive => 0,
            Self::Setup => 1,
            Self::Clipboard => 2,
            Self::Input => 3,
            Self::Control => 4,
            Self::Other => 5,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Keepalive => "keepalive",
            Self::Setup => "setup",
            Self::Clipboard => "clipboard",
            Self::Input => "input",
            Self::Control => "control",
            Self::Other => "other",
        }
    }

    /// Classify a wire message type. Unknown values are `Other`, never a
    /// panic — a peer we do not fully understand still counts.
    #[must_use]
    pub fn of(message_type: u16) -> Self {
        match MessageType::from_wire(message_type) {
            Some(MessageType::Ping | MessageType::Pong) => Self::Keepalive,
            Some(MessageType::Hello | MessageType::PairingStart | MessageType::PairingConfirm) => {
                Self::Setup
            }
            Some(
                MessageType::ClipboardOffer
                | MessageType::ClipboardAccept
                | MessageType::ClipboardDecline
                | MessageType::ClipboardData
                | MessageType::ClipboardChunk
                | MessageType::ClipboardApplied,
            ) => Self::Clipboard,
            Some(MessageType::InputBatch | MessageType::ReleaseAllInput) => Self::Input,
            Some(
                MessageType::ControlRequest
                | MessageType::ControlResponse
                | MessageType::ControlRelease,
            ) => Self::Control,
            None => Self::Other,
        }
    }
}

/// Which disconnect reasons the report breaks out, mirroring
/// [`DisconnectReason`] but flattened for counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisconnectKind {
    PeerClosed,
    KeepaliveTimeout,
    ProtocolViolation,
    Transport,
    EventConsumerStalled,
    ShutdownRequested,
}

impl DisconnectKind {
    const ALL: [Self; 6] = [
        Self::PeerClosed,
        Self::KeepaliveTimeout,
        Self::ProtocolViolation,
        Self::Transport,
        Self::EventConsumerStalled,
        Self::ShutdownRequested,
    ];

    fn index(self) -> usize {
        match self {
            Self::PeerClosed => 0,
            Self::KeepaliveTimeout => 1,
            Self::ProtocolViolation => 2,
            Self::Transport => 3,
            Self::EventConsumerStalled => 4,
            Self::ShutdownRequested => 5,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::PeerClosed => "peer_closed",
            Self::KeepaliveTimeout => "keepalive_timeout",
            Self::ProtocolViolation => "protocol_violation",
            Self::Transport => "transport",
            Self::EventConsumerStalled => "event_consumer_stalled",
            Self::ShutdownRequested => "shutdown_requested",
        }
    }

    fn of(reason: &DisconnectReason) -> Self {
        // Exhaustive within this crate (DisconnectReason is only
        // `#[non_exhaustive]` for downstream crates): a new reason breaks
        // the build here, so the report can never silently drop one.
        match reason {
            DisconnectReason::PeerClosed => Self::PeerClosed,
            DisconnectReason::KeepaliveTimeout => Self::KeepaliveTimeout,
            DisconnectReason::ProtocolViolation { .. } => Self::ProtocolViolation,
            DisconnectReason::Transport { .. } => Self::Transport,
            DisconnectReason::EventConsumerStalled { .. } => Self::EventConsumerStalled,
            DisconnectReason::ShutdownRequested => Self::ShutdownRequested,
        }
    }
}

/// The process-wide metrics registry. Cheap to clone as `Arc<Metrics>`
/// and share; every field is an independent tally.
#[derive(Debug, Default)]
pub struct Metrics {
    // ---- network ----
    frames_sent: AtomicU64,
    frames_received: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    sent_by_class: [AtomicU64; 6],
    received_by_class: [AtomicU64; 6],

    // ---- sessions ----
    sessions_inbound: AtomicU64,
    sessions_outbound: AtomicU64,
    reconnect_attempts: AtomicU64,
    // Recovery time: from a session dropping to the next one establishing —
    // how long the peer link was actually down (TESTING.md §4).
    reconnect_recoveries: AtomicU64,
    reconnect_recovery_total_ms: AtomicU64,
    reconnect_recovery_max_ms: AtomicU64,
    disconnects: [AtomicU64; 6],
    total_connected_ms: AtomicU64,
    longest_session_ms: AtomicU64,

    // ---- clipboard ----
    clipboard_sent: AtomicU64,
    clipboard_applied: AtomicU64,
    clipboard_superseded: AtomicU64,
    clipboard_retries: AtomicU64,
    clipboard_contention: AtomicU64,
    clipboard_loop_suppressed: AtomicU64,
    clipboard_conflicts: AtomicU64,
    clipboard_abandoned: AtomicU64,
    clipboard_deferred_peak: AtomicU64,
    clipboard_latency_ms: Mutex<Vec<u32>>,
    clipboard_latency_dropped: AtomicU64,
    // Count/total/max rather than a sample vector, because this is the one
    // latency recorded on the input path: the module's rule is that the
    // sample `Mutex` is never taken there, and "bounded" is a question the
    // maximum answers exactly. Microseconds, not milliseconds — a healthy
    // wait is tens of µs, which a millisecond scale would round to a column
    // of zeros.
    input_queue_latency_count: AtomicU64,
    input_queue_latency_total_us: AtomicU64,
    input_queue_latency_max_us: AtomicU64,

    // ---- control & input ----
    control_gained: AtomicU64,
    control_given: AtomicU64,
    control_denied: AtomicU64,
    control_requests: AtomicU64,
    control_timeouts: AtomicU64,
    control_handbacks: AtomicU64,
    control_revocations: AtomicU64,
    capture_losses: AtomicU64,
    input_events_sent: AtomicU64,
    input_events_received: AtomicU64,
    key_events_sent: AtomicU64,
    key_events_received: AtomicU64,
}

impl Metrics {
    /// A registry with everything at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ---- network ----

    /// Record a frame sent: its on-wire byte length and message type.
    pub fn record_sent(&self, message_type: u16, wire_bytes: usize) {
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent
            .fetch_add(wire_bytes as u64, Ordering::Relaxed);
        self.sent_by_class[FrameClass::of(message_type).index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Record a frame received: its on-wire byte length and message type.
    pub fn record_received(&self, message_type: u16, wire_bytes: usize) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received
            .fetch_add(wire_bytes as u64, Ordering::Relaxed);
        self.received_by_class[FrameClass::of(message_type).index()]
            .fetch_add(1, Ordering::Relaxed);
    }

    // ---- sessions ----

    /// Record a session reaching `ESTABLISHED`, by role.
    pub fn record_session_established(&self, inbound: bool) {
        if inbound {
            &self.sessions_inbound
        } else {
            &self.sessions_outbound
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a session's end: its reason and how long it was up.
    pub fn record_session_ended(&self, reason: &DisconnectReason, duration: Duration) {
        self.disconnects[DisconnectKind::of(reason).index()].fetch_add(1, Ordering::Relaxed);
        let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.total_connected_ms.fetch_add(ms, Ordering::Relaxed);
        self.longest_session_ms.fetch_max(ms, Ordering::Relaxed);
    }

    /// Record a reconnection attempt (an establish that failed and will
    /// be retried) — the app-level stand-in for "retransmitted", since
    /// TCP's own retransmits are invisible above the socket.
    pub fn record_reconnect_attempt(&self) {
        self.reconnect_attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// Record how long the peer link was down before a session came back —
    /// from the drop to the next establishment (TESTING.md §4).
    pub fn record_reconnect_recovery(&self, downtime: Duration) {
        let ms = u64::try_from(downtime.as_millis()).unwrap_or(u64::MAX);
        self.reconnect_recoveries.fetch_add(1, Ordering::Relaxed);
        self.reconnect_recovery_total_ms
            .fetch_add(ms, Ordering::Relaxed);
        self.reconnect_recovery_max_ms
            .fetch_max(ms, Ordering::Relaxed);
    }

    // ---- clipboard ----

    /// A clipboard item was sent to the peer.
    pub fn record_clipboard_sent(&self) {
        self.clipboard_sent.fetch_add(1, Ordering::Relaxed);
    }
    /// A remote item was applied to the local clipboard.
    pub fn record_clipboard_applied(&self) {
        self.clipboard_applied.fetch_add(1, Ordering::Relaxed);
    }
    /// An item was superseded by a newer one before it took effect.
    pub fn record_clipboard_superseded(&self) {
        self.clipboard_superseded.fetch_add(1, Ordering::Relaxed);
    }
    /// A clipboard transfer was abandoned unfinished: its deadline
    /// expired before the transaction closed (ADR 0014).
    ///
    /// Counted rather than merely logged because it is the *only* signal
    /// for a class of silent stalls — an offer refused locally by the
    /// send gate, or a peer that accepts and then goes quiet — where
    /// nothing else observable happens for a minute and then a
    /// transaction simply is not there any more (NFR-3, FR-7.3).
    pub fn record_clipboard_abandoned(&self) {
        self.clipboard_abandoned.fetch_add(1, Ordering::Relaxed);
    }

    /// Record how deep the clipboard driver's deferred-event queue got
    /// while it was parked on bulk backpressure.
    ///
    /// A high-water mark rather than a count: the queue is bounded (NFR-1)
    /// and the only interesting question is how close a real run came to
    /// that bound. Zero — the normal case — means the driver never had to
    /// defer at all.
    pub fn record_deferred_depth(&self, depth: usize) {
        self.clipboard_deferred_peak
            .fetch_max(u64::try_from(depth).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
    /// A clipboard write was retried after `Busy` contention.
    pub fn record_clipboard_retry(&self) {
        self.clipboard_retries.fetch_add(1, Ordering::Relaxed);
    }
    /// A clipboard read/write hit `Busy` contention (R-5).
    pub fn record_clipboard_contention(&self) {
        self.clipboard_contention.fetch_add(1, Ordering::Relaxed);
    }
    /// Our own applied item was recognized on read-back and not resent
    /// (loop prevention, FR-3.3).
    pub fn record_clipboard_loop_suppressed(&self) {
        self.clipboard_loop_suppressed
            .fetch_add(1, Ordering::Relaxed);
    }
    /// A near-simultaneous clipboard change was resolved by the
    /// latest-wins policy (FR-3.5).
    pub fn record_clipboard_conflict(&self) {
        self.clipboard_conflicts.fetch_add(1, Ordering::Relaxed);
    }
    /// How long an input frame waited between being handed to the send
    /// path and reaching the wire, in microseconds.
    ///
    /// This is the quantity ADR 0013's guarantee is about: a saturating
    /// bulk transfer must not push interactive frames back. Kept as a
    /// running mean and maximum rather than a distribution — the guarantee
    /// is a bound, which the maximum states directly, and aggregating with
    /// atomics keeps the input path lock-free.
    pub fn record_input_queue_latency(&self, micros: u32) {
        self.input_queue_latency_count
            .fetch_add(1, Ordering::Relaxed);
        self.input_queue_latency_total_us
            .fetch_add(u64::from(micros), Ordering::Relaxed);
        self.input_queue_latency_max_us
            .fetch_max(u64::from(micros), Ordering::Relaxed);
    }

    /// A completed clipboard round-trip latency, on the originating
    /// clock (the number docs/TESTING.md §4 defines).
    pub fn record_clipboard_latency(&self, ms: u32) {
        let mut samples = self
            .clipboard_latency_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if samples.len() < MAX_LATENCY_SAMPLES {
            samples.push(ms);
        } else {
            self.clipboard_latency_dropped
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // ---- control & input ----

    /// This machine gained control of a peer.
    pub fn record_control_gained(&self) {
        self.control_gained.fetch_add(1, Ordering::Relaxed);
    }
    /// A peer took control of this machine.
    pub fn record_control_given(&self) {
        self.control_given.fetch_add(1, Ordering::Relaxed);
    }
    /// A control request (either direction) was denied.
    pub fn record_control_denied(&self) {
        self.control_denied.fetch_add(1, Ordering::Relaxed);
    }
    /// This machine asked a peer for control.
    pub fn record_control_request(&self) {
        self.control_requests.fetch_add(1, Ordering::Relaxed);
    }
    /// A control request timed out with no answer.
    pub fn record_control_timeout(&self) {
        self.control_timeouts.fetch_add(1, Ordering::Relaxed);
    }
    /// A control relationship ended by hand-back (includes the escape
    /// gesture, which hands back the same way).
    pub fn record_control_handback(&self) {
        self.control_handbacks.fetch_add(1, Ordering::Relaxed);
    }
    /// A control grant was revoked by the controlled side.
    pub fn record_control_revocation(&self) {
        self.control_revocations.fetch_add(1, Ordering::Relaxed);
    }
    /// Local input capture was lost while controlling (R-2 fail-closed).
    pub fn record_capture_loss(&self) {
        self.capture_losses.fetch_add(1, Ordering::Relaxed);
    }

    /// Record input events forwarded to a peer: total, and how many were
    /// key events.
    pub fn record_input_sent(&self, total: u64, keys: u64) {
        self.input_events_sent.fetch_add(total, Ordering::Relaxed);
        self.key_events_sent.fetch_add(keys, Ordering::Relaxed);
    }

    /// Record input events injected from a peer: total, and how many were
    /// key events.
    pub fn record_input_received(&self, total: u64, keys: u64) {
        self.input_events_received
            .fetch_add(total, Ordering::Relaxed);
        self.key_events_received.fetch_add(keys, Ordering::Relaxed);
    }

    /// Read every counter into a plain, orderless snapshot for rendering.
    #[must_use]
    pub fn snapshot(&self) -> Report {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let sorted_latency = {
            let samples = self
                .clipboard_latency_ms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut v = samples.clone();
            v.sort_unstable();
            v
        };
        Report {
            frames_sent: load(&self.frames_sent),
            frames_received: load(&self.frames_received),
            bytes_sent: load(&self.bytes_sent),
            bytes_received: load(&self.bytes_received),
            sent_by_class: FrameClass::ALL.map(|c| load(&self.sent_by_class[c.index()])),
            received_by_class: FrameClass::ALL.map(|c| load(&self.received_by_class[c.index()])),
            sessions_inbound: load(&self.sessions_inbound),
            sessions_outbound: load(&self.sessions_outbound),
            reconnect_attempts: load(&self.reconnect_attempts),
            reconnect_recoveries: load(&self.reconnect_recoveries),
            reconnect_recovery_avg_ms: {
                let n = load(&self.reconnect_recoveries);
                (n > 0).then(|| load(&self.reconnect_recovery_total_ms) / n)
            },
            reconnect_recovery_max_ms: (load(&self.reconnect_recoveries) > 0)
                .then(|| load(&self.reconnect_recovery_max_ms)),
            disconnects: DisconnectKind::ALL.map(|k| load(&self.disconnects[k.index()])),
            total_connected_ms: load(&self.total_connected_ms),
            longest_session_ms: load(&self.longest_session_ms),
            clipboard_sent: load(&self.clipboard_sent),
            clipboard_applied: load(&self.clipboard_applied),
            clipboard_superseded: load(&self.clipboard_superseded),
            clipboard_retries: load(&self.clipboard_retries),
            clipboard_contention: load(&self.clipboard_contention),
            clipboard_loop_suppressed: load(&self.clipboard_loop_suppressed),
            clipboard_conflicts: load(&self.clipboard_conflicts),
            clipboard_abandoned: load(&self.clipboard_abandoned),
            clipboard_deferred_peak: load(&self.clipboard_deferred_peak),
            clipboard_latency_dropped: load(&self.clipboard_latency_dropped),
            input_queue_avg_us: {
                let n = load(&self.input_queue_latency_count);
                (n > 0).then(|| {
                    u32::try_from(load(&self.input_queue_latency_total_us) / n).unwrap_or(u32::MAX)
                })
            },
            input_queue_max_us: (load(&self.input_queue_latency_count) > 0)
                .then(|| u32::try_from(load(&self.input_queue_latency_max_us)).unwrap_or(u32::MAX)),
            input_queue_samples: load(&self.input_queue_latency_count),
            latency_p50: percentile(&sorted_latency, 50),
            latency_p95: percentile(&sorted_latency, 95),
            latency_max: sorted_latency.last().copied(),
            latency_samples: sorted_latency.len() as u64,
            control_gained: load(&self.control_gained),
            control_given: load(&self.control_given),
            control_denied: load(&self.control_denied),
            control_requests: load(&self.control_requests),
            control_timeouts: load(&self.control_timeouts),
            control_handbacks: load(&self.control_handbacks),
            control_revocations: load(&self.control_revocations),
            capture_losses: load(&self.capture_losses),
            input_events_sent: load(&self.input_events_sent),
            input_events_received: load(&self.input_events_received),
            key_events_sent: load(&self.key_events_sent),
            key_events_received: load(&self.key_events_received),
        }
    }
}

/// The nearest-rank percentile of a pre-sorted slice, or `None` if empty.
fn percentile(sorted: &[u32], p: u8) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    // Nearest-rank: ceil(p/100 * n), 1-indexed, clamped into range. The
    // sample count is bounded by MAX_LATENCY_SAMPLES, so this cannot
    // overflow usize.
    let rank = (usize::from(p) * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// An orderless snapshot of every counter, for rendering. Plain values,
/// so the human block and the structured record read the same numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Total frames sent.
    pub frames_sent: u64,
    /// Total frames received.
    pub frames_received: u64,
    /// Total on-wire bytes sent.
    pub bytes_sent: u64,
    /// Total on-wire bytes received.
    pub bytes_received: u64,
    /// Frames sent, per [`FrameClass`] in `FrameClass::ALL` order.
    pub sent_by_class: [u64; 6],
    /// Frames received, per class in `FrameClass::ALL` order.
    pub received_by_class: [u64; 6],
    /// Inbound sessions established.
    pub sessions_inbound: u64,
    /// Outbound sessions established.
    pub sessions_outbound: u64,
    /// Reconnection attempts.
    pub reconnect_attempts: u64,
    /// Recoveries: sessions that came back after a drop.
    pub reconnect_recoveries: u64,
    /// Mean downtime per recovery, milliseconds (`None` if none yet).
    pub reconnect_recovery_avg_ms: Option<u64>,
    /// Worst downtime before a recovery, milliseconds (`None` if none yet).
    pub reconnect_recovery_max_ms: Option<u64>,
    /// Disconnects, per reason in `DisconnectKind::ALL` order.
    pub disconnects: [u64; 6],
    /// Total connected time across all sessions, milliseconds.
    pub total_connected_ms: u64,
    /// Longest single session, milliseconds.
    pub longest_session_ms: u64,
    /// Clipboard items sent.
    pub clipboard_sent: u64,
    /// Clipboard items applied locally.
    pub clipboard_applied: u64,
    /// Clipboard items superseded before taking effect.
    pub clipboard_superseded: u64,
    /// Clipboard transfers abandoned on their deadline (ADR 0014).
    pub clipboard_abandoned: u64,
    /// Clipboard write retries (`Busy`).
    pub clipboard_retries: u64,
    /// Clipboard contention events (`Busy`).
    pub clipboard_contention: u64,
    /// Own-write loop suppressions.
    pub clipboard_loop_suppressed: u64,
    /// Clipboard conflicts resolved.
    pub clipboard_conflicts: u64,
    /// Deepest the driver's deferred-event queue got while parked on bulk
    /// backpressure; `0` if it never had to defer.
    pub clipboard_deferred_peak: u64,
    /// Latency samples dropped past the retention cap.
    pub clipboard_latency_dropped: u64,
    /// Mean input queue-to-wire latency (µs), if any input flowed. The
    /// ADR 0013 guarantee, as a number rather than an ordering.
    pub input_queue_avg_us: Option<u32>,
    /// Worst input queue-to-wire latency (µs) — the one a saturating bulk
    /// transfer would inflate if the lane split stopped working.
    pub input_queue_max_us: Option<u32>,
    /// How many input frames were timed.
    pub input_queue_samples: u64,
    /// Clipboard round-trip latency p50 (ms), if any samples.
    pub latency_p50: Option<u32>,
    /// Clipboard round-trip latency p95 (ms).
    pub latency_p95: Option<u32>,
    /// Clipboard round-trip latency max (ms).
    pub latency_max: Option<u32>,
    /// Latency samples counted.
    pub latency_samples: u64,
    /// Times this machine gained control of a peer.
    pub control_gained: u64,
    /// Times a peer took control of this machine.
    pub control_given: u64,
    /// Control requests denied.
    pub control_denied: u64,
    /// Control requests made by this machine.
    pub control_requests: u64,
    /// Control requests that timed out.
    pub control_timeouts: u64,
    /// Control relationships ended by hand-back (incl. escape).
    pub control_handbacks: u64,
    /// Control grants revoked by the controlled side.
    pub control_revocations: u64,
    /// Capture losses (R-2).
    pub capture_losses: u64,
    /// Input events forwarded to peers.
    pub input_events_sent: u64,
    /// Input events injected from peers.
    pub input_events_received: u64,
    /// Of the sent input events, how many were key events.
    pub key_events_sent: u64,
    /// Of the received input events, how many were key events.
    pub key_events_received: u64,
}

impl Report {
    /// Emit the snapshot as one structured `tracing` record (`snake_case`
    /// fields), for log post-processing alongside the human block.
    pub fn log(&self) {
        tracing::info!(
            frames_sent = self.frames_sent,
            frames_received = self.frames_received,
            bytes_sent = self.bytes_sent,
            bytes_received = self.bytes_received,
            reconnect_attempts = self.reconnect_attempts,
            reconnect_recoveries = self.reconnect_recoveries,
            reconnect_recovery_avg_ms = self.reconnect_recovery_avg_ms,
            reconnect_recovery_max_ms = self.reconnect_recovery_max_ms,
            sessions_inbound = self.sessions_inbound,
            sessions_outbound = self.sessions_outbound,
            total_connected_ms = self.total_connected_ms,
            longest_session_ms = self.longest_session_ms,
            clipboard_sent = self.clipboard_sent,
            clipboard_applied = self.clipboard_applied,
            clipboard_superseded = self.clipboard_superseded,
            clipboard_abandoned = self.clipboard_abandoned,
            clipboard_retries = self.clipboard_retries,
            clipboard_contention = self.clipboard_contention,
            clipboard_conflicts = self.clipboard_conflicts,
            clipboard_loop_suppressed = self.clipboard_loop_suppressed,
            clipboard_latency_dropped = self.clipboard_latency_dropped,
            clipboard_deferred_peak = self.clipboard_deferred_peak,
            latency_p50_ms = self.latency_p50,
            latency_p95_ms = self.latency_p95,
            latency_max_ms = self.latency_max,
            control_gained = self.control_gained,
            control_given = self.control_given,
            control_denied = self.control_denied,
            capture_losses = self.capture_losses,
            input_queue_avg_us = self.input_queue_avg_us,
            input_queue_max_us = self.input_queue_max_us,
            input_queue_samples = self.input_queue_samples,
            input_events_sent = self.input_events_sent,
            input_events_received = self.input_events_received,
            "execution metrics"
        );
    }

    fn total_disconnects(&self) -> u64 {
        self.disconnects.iter().sum()
    }

    /// The clipboard block of the shutdown report: one summary line, plus
    /// the sub-lines that only mean something when they happened.
    fn write_clipboard(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "  clipboard:  {} sent, {} applied, {} superseded, {} abandoned",
            self.clipboard_sent,
            self.clipboard_applied,
            self.clipboard_superseded,
            self.clipboard_abandoned,
        )?;
        // Loop suppressions belong next to the failure counts, not hidden
        // behind a threshold: a healthy run suppresses roughly one per
        // applied item, so the *absence* of them is the anomaly worth
        // seeing — that is what a sync loop looks like from here.
        writeln!(
            f,
            "                {} retries, {} contention, {} conflicts, {} own-write loops suppressed",
            self.clipboard_retries,
            self.clipboard_contention,
            self.clipboard_conflicts,
            self.clipboard_loop_suppressed,
        )?;
        // A run that never parked on bulk backpressure has nothing to say
        // here, and the block stays short.
        if self.clipboard_deferred_peak > 0 {
            writeln!(
                f,
                "                deferred peak {} events while parked on bulk backpressure",
                self.clipboard_deferred_peak,
            )?;
        }
        if let (Some(p50), Some(p95), Some(max)) =
            (self.latency_p50, self.latency_p95, self.latency_max)
        {
            writeln!(
                f,
                "                latency p50 {p50}ms, p95 {p95}ms, max {max}ms (over {} samples)",
                self.latency_samples,
            )?;
        }
        // Only if the cap was reached: otherwise the percentiles above are
        // over every sample taken, and saying "0 dropped" invites the
        // reader to wonder what could have been.
        if self.clipboard_latency_dropped > 0 {
            writeln!(
                f,
                "                {} latency samples dropped past the retention cap",
                self.clipboard_latency_dropped,
            )?;
        }
        Ok(())
    }
}

/// Human-readable shutdown block. Grouped, aligned, and only printing
/// the sub-lines that carry a signal, so a quiet run stays short.
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Session statistics")?;

        writeln!(
            f,
            "  network:    {} frames / {} sent,  {} frames / {} received",
            self.frames_sent,
            human_bytes(self.bytes_sent),
            self.frames_received,
            human_bytes(self.bytes_received),
        )?;
        for (i, class) in FrameClass::ALL.iter().enumerate() {
            let sent = self.sent_by_class[i];
            let recv = self.received_by_class[i];
            if sent + recv > 0 {
                writeln!(
                    f,
                    "                {:<10} {sent} sent, {recv} received",
                    class.label()
                )?;
            }
        }

        writeln!(
            f,
            "  sessions:   {} inbound, {} outbound established; {} reconnect attempt(s)",
            self.sessions_inbound, self.sessions_outbound, self.reconnect_attempts,
        )?;
        if self.total_disconnects() > 0 {
            for (i, kind) in DisconnectKind::ALL.iter().enumerate() {
                if self.disconnects[i] > 0 {
                    writeln!(
                        f,
                        "                {:<18} {}",
                        kind.label(),
                        self.disconnects[i]
                    )?;
                }
            }
        }
        writeln!(
            f,
            "                connected {}, longest session {}",
            human_ms(self.total_connected_ms),
            human_ms(self.longest_session_ms),
        )?;
        if let (Some(avg), Some(max)) = (
            self.reconnect_recovery_avg_ms,
            self.reconnect_recovery_max_ms,
        ) {
            writeln!(
                f,
                "                recovered {} time(s): downtime avg {}, max {}",
                self.reconnect_recoveries,
                human_ms(avg),
                human_ms(max),
            )?;
        }

        self.write_clipboard(f)?;

        writeln!(
            f,
            "  control:    gained {}, given {}, denied {}, timed out {}, handed back {}, revoked {}",
            self.control_gained,
            self.control_given,
            self.control_denied,
            self.control_timeouts,
            self.control_handbacks,
            self.control_revocations,
        )?;
        if self.capture_losses > 0 {
            writeln!(
                f,
                "                capture lost {} time(s)",
                self.capture_losses
            )?;
        }
        writeln!(
            f,
            "  input:      {} events sent ({} keys), {} events received ({} keys)",
            self.input_events_sent,
            self.key_events_sent,
            self.input_events_received,
            self.key_events_received,
        )?;
        // The ADR 0013 guarantee as a number. Printed only when input
        // actually flowed, and last, so the line a soak is looking for is
        // the one at the bottom of the block.
        match (self.input_queue_avg_us, self.input_queue_max_us) {
            (Some(avg), Some(max)) => write!(
                f,
                "                queue-to-wire avg {}, max {} (over {} frames)",
                human_micros(avg),
                human_micros(max),
                self.input_queue_samples,
            ),
            _ => Ok(()),
        }
    }
}

/// Microseconds at a readable scale: a healthy queue wait is tens of µs and
/// a degraded one is milliseconds, and neither unit alone reads well for
/// both.
fn human_micros(micros: u32) -> String {
    if micros < 1_000 {
        format!("{micros}us")
    } else {
        format!("{:.1}ms", f64::from(micros) / 1_000.0)
    }
}

/// Bytes with a binary-unit suffix, for the human block.
#[allow(clippy::cast_precision_loss)] // display only; a fractional GiB is fine
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Milliseconds as a compact human duration.
fn human_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FrameClass, Metrics, percentile};
    use crate::supervision::DisconnectReason;

    /// Every counter the registry keeps has to reach a human somewhere.
    /// A tally nothing renders is worse than no tally: docs/SOAK.md sends
    /// readers to check numbers, and one that is only ever incremented
    /// reads as "zero happened" rather than "nobody printed it".
    #[test]
    fn the_report_renders_every_clipboard_counter() {
        let metrics = Metrics::new();
        metrics.record_clipboard_sent();
        metrics.record_clipboard_applied();
        metrics.record_clipboard_superseded();
        metrics.record_clipboard_abandoned();
        metrics.record_clipboard_retry();
        metrics.record_clipboard_contention();
        metrics.record_clipboard_conflict();
        metrics.record_clipboard_loop_suppressed();
        metrics.record_deferred_depth(7);

        let rendered = metrics.snapshot().to_string();
        for expected in [
            "1 sent",
            "1 applied",
            "1 superseded",
            "1 abandoned",
            "1 retries",
            "1 contention",
            "1 conflicts",
            "1 own-write loops suppressed",
            "deferred peak 7",
        ] {
            assert!(
                rendered.contains(expected),
                "the report never mentions {expected}:
{rendered}"
            );
        }
    }

    /// The retention note only earns its line when samples were actually
    /// lost; otherwise the percentiles above it cover everything.
    #[test]
    fn dropped_latency_samples_are_reported_only_when_some_were_dropped() {
        let metrics = Metrics::new();
        metrics.record_clipboard_latency(5);
        assert!(
            !metrics
                .snapshot()
                .to_string()
                .contains("dropped past the retention cap")
        );

        for _ in 0..super::MAX_LATENCY_SAMPLES {
            metrics.record_clipboard_latency(5);
        }
        assert!(
            metrics
                .snapshot()
                .to_string()
                .contains("dropped past the retention cap")
        );
    }

    /// The number the Phase 7 exit criterion asks for has to reach the
    /// block a soak reads, in a unit that shows both a healthy wait and a
    /// degraded one.
    #[test]
    fn input_queue_latency_reaches_the_report_at_a_readable_scale() {
        let metrics = Metrics::new();
        metrics.record_sent(11, 40); // an InputBatch, so the line is earned
        for micros in [40, 60, 2_500] {
            metrics.record_input_queue_latency(micros);
        }

        let rendered = metrics.snapshot().to_string();
        assert!(rendered.contains("queue-to-wire"), "{rendered}");
        assert!(
            rendered.contains("866us"),
            "the mean, in microseconds: {rendered}"
        );
        assert!(rendered.contains("2.5ms"), "a slow wait in ms: {rendered}");
        assert!(rendered.contains("over 3 frames"), "{rendered}");
    }

    /// A run with no input says nothing rather than printing empty columns.
    #[test]
    fn a_run_with_no_input_omits_the_queue_latency_line() {
        assert!(
            !Metrics::new()
                .snapshot()
                .to_string()
                .contains("queue-to-wire")
        );
    }

    #[test]
    fn frame_classes_cover_the_message_types() {
        assert_eq!(FrameClass::of(2), FrameClass::Keepalive); // Ping
        assert_eq!(FrameClass::of(9), FrameClass::Clipboard); // ClipboardData
        assert_eq!(FrameClass::of(11), FrameClass::Input); // InputBatch
        assert_eq!(FrameClass::of(13), FrameClass::Control); // ControlRequest
        assert_eq!(FrameClass::of(1), FrameClass::Setup); // Hello
        assert_eq!(FrameClass::of(9999), FrameClass::Other); // unknown
    }

    #[test]
    fn network_counters_total_and_classify() {
        let m = Metrics::new();
        m.record_sent(11, 40); // InputBatch, 40 bytes
        m.record_sent(2, 14); // Ping
        m.record_received(9, 4096); // ClipboardData

        let r = m.snapshot();
        assert_eq!(r.frames_sent, 2);
        assert_eq!(r.bytes_sent, 54);
        assert_eq!(r.frames_received, 1);
        assert_eq!(r.bytes_received, 4096);
        // Input class (index 3) has one sent; keepalive (0) one sent.
        assert_eq!(r.sent_by_class[FrameClass::Input.index()], 1);
        assert_eq!(r.sent_by_class[FrameClass::Keepalive.index()], 1);
        assert_eq!(r.received_by_class[FrameClass::Clipboard.index()], 1);
    }

    #[test]
    fn reconnect_recovery_reports_count_avg_and_max_downtime() {
        let m = Metrics::new();
        // No recoveries yet: the fields stay absent (nothing to average).
        let r = m.snapshot();
        assert_eq!(r.reconnect_recoveries, 0);
        assert_eq!(r.reconnect_recovery_avg_ms, None);
        assert_eq!(r.reconnect_recovery_max_ms, None);

        m.record_reconnect_recovery(Duration::from_millis(200));
        m.record_reconnect_recovery(Duration::from_millis(800));
        let r = m.snapshot();
        assert_eq!(r.reconnect_recoveries, 2);
        assert_eq!(r.reconnect_recovery_avg_ms, Some(500)); // (200 + 800) / 2
        assert_eq!(r.reconnect_recovery_max_ms, Some(800));
    }

    #[test]
    fn sessions_track_reasons_and_longest() {
        let m = Metrics::new();
        m.record_session_established(true);
        m.record_session_established(false);
        m.record_session_ended(&DisconnectReason::PeerClosed, Duration::from_secs(3));
        m.record_session_ended(&DisconnectReason::KeepaliveTimeout, Duration::from_secs(10));
        m.record_reconnect_attempt();

        let r = m.snapshot();
        assert_eq!(r.sessions_inbound, 1);
        assert_eq!(r.sessions_outbound, 1);
        assert_eq!(r.reconnect_attempts, 1);
        assert_eq!(r.total_connected_ms, 13_000);
        assert_eq!(r.longest_session_ms, 10_000);
        assert_eq!(r.total_disconnects(), 2);
    }

    #[test]
    fn latency_percentiles_are_nearest_rank() {
        let m = Metrics::new();
        for ms in 1..=100 {
            m.record_clipboard_latency(ms);
        }
        let r = m.snapshot();
        assert_eq!(r.latency_samples, 100);
        assert_eq!(r.latency_p50, Some(50));
        assert_eq!(r.latency_p95, Some(95));
        assert_eq!(r.latency_max, Some(100));
    }

    #[test]
    fn percentile_of_empty_is_none_and_single_is_itself() {
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(percentile(&[42], 50), Some(42));
        assert_eq!(percentile(&[42], 95), Some(42));
    }

    #[test]
    fn a_quiet_run_renders_without_optional_lines() {
        // Nothing recorded: the block must still render, with no
        // disconnect breakdown, no latency line, no capture-loss line.
        let text = Metrics::new().snapshot().to_string();
        assert!(text.starts_with("Session statistics"));
        assert!(!text.contains("latency"));
        assert!(!text.contains("capture lost"));
        assert!(text.contains("input:"));
    }

    #[test]
    fn a_busy_run_renders_every_group() {
        let m = Metrics::new();
        m.record_sent(11, 40);
        m.record_received(9, 4096);
        m.record_session_established(false);
        m.record_session_ended(&DisconnectReason::PeerClosed, Duration::from_secs(5));
        m.record_clipboard_sent();
        m.record_clipboard_latency(6);
        m.record_control_gained();
        m.record_capture_loss();
        m.record_input_sent(120, 5);

        let text = m.snapshot().to_string();
        assert!(text.contains("network:"));
        assert!(text.contains("peer_closed"));
        assert!(text.contains("latency p50 6ms"));
        assert!(text.contains("capture lost 1"));
        assert!(text.contains("120 events sent (5 keys)"));
    }
}
