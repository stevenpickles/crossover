//! The clipboard transaction engine (docs/ARCHITECTURE.md §5.2, FR-3.x).
//!
//! Sans-io: the driver feeds observations in (local changes, peer
//! messages, write results, timers) and executes the [`Action`]s that
//! come back. Every invariant the spec demands lives here, pure and
//! provable without I/O:
//!
//! - **Acknowledged installs** (FR-3.2): a transaction closes only on the
//!   destination's `ClipboardApplied` verdict.
//! - **Loop prevention** (FR-3.3): hashes of content we applied locally
//!   are remembered, so the provider's own-write notification never
//!   echoes an item back to its origin.
//! - **Dedup**: unchanged content is never re-sent.
//! - **Bounded retry** (FR-3.4): `Busy` write failures retry on a fixed
//!   schedule with a hard attempt cap, then close as
//!   `ClipboardUnavailable`.
//! - **Deterministic conflict rule** (FR-3.5): items are totally ordered
//!   by `(sequence, origin)` lexicographically. Both sides of a crossing
//!   race compute the same winner; the loser's transaction closes as
//!   `Superseded`. The order is deterministic, not wall-clock-fair — a
//!   freshly restarted peer (sequence reset) loses ties until it catches
//!   up, which only matters during genuinely simultaneous copies.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crossover_protocol::clipboard::{
    ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ClipboardAccept, ClipboardApplied, ClipboardData,
    ClipboardDecline, ClipboardMeta, ClipboardOffer, ContentType, DeclineReason,
    MAX_CLIPBOARD_TEXT_BYTES, content_hash,
};
use crossover_protocol::hello::MessageType;

use crate::metrics::Metrics;

/// How many recently-applied content hashes are remembered for loop
/// prevention. Notifications coalesce, so a small window suffices; the
/// bound keeps memory fixed (NFR-1).
const APPLIED_HASH_MEMORY: usize = 8;

/// Clipboard engine tuning. Grouped because both knobs are timing
/// policy, and tests need to shrink them without pretending the
/// production defaults are different.
#[derive(Debug, Clone, Default)]
pub struct ClipboardConfig {
    /// Bounded retry for `Busy` clipboard writes (FR-3.4).
    pub retry: RetryPolicy,
    /// Quiet period before staged content is transmitted (ADR 0006).
    pub transmit_debounce: Duration,
}

impl ClipboardConfig {
    /// Production defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            retry: RetryPolicy::default(),
            transmit_debounce: TRANSMIT_DEBOUNCE,
        }
    }
}

/// How long the local clipboard must stay unchanged before Crossover
/// reads it and transmits (ADR 0006).
///
/// The window gates the **read**, not merely the send. Reading takes the
/// machine-global clipboard lock exactly as writing does, so reacting to
/// every change notification is itself the contention: the two-machine
/// soak showed hundreds of failed opens per run while another
/// application copied at 3 Hz, and a comparable number of that
/// application's own copies failing in return. Waiting for the clipboard
/// to settle collapses a burst into a single lock acquisition.
///
/// Control transfer becomes the primary trigger in Phase 5; this
/// debounce carries Phase 2 and remains the fallback afterwards.
pub const TRANSMIT_DEBOUNCE: Duration = Duration::from_millis(300);

/// Retry policy for `Busy` clipboard writes (FR-3.4): centrally defined,
/// bounded attempts, bounded total time (ADR 0005 requires exactly this
/// shape).
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total write attempts before giving up (first try included).
    pub max_attempts: u32,
    /// Delay between attempts.
    pub delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            delay: Duration::from_millis(200),
        }
    }
}

/// A wire message the driver should send, paired with its frame type.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboundMessage {
    /// Announce a large item.
    Offer(ClipboardOffer),
    /// Accept an offered item.
    Accept(ClipboardAccept),
    /// Decline an offered item.
    Decline(ClipboardDecline),
    /// The item content.
    Data(ClipboardData),
    /// The destination verdict.
    Applied(ClipboardApplied),
}

impl OutboundMessage {
    /// The frame message type this message travels as.
    #[must_use]
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Offer(_) => MessageType::ClipboardOffer,
            Self::Accept(_) => MessageType::ClipboardAccept,
            Self::Decline(_) => MessageType::ClipboardDecline,
            Self::Data(_) => MessageType::ClipboardData,
            Self::Applied(_) => MessageType::ClipboardApplied,
        }
    }

    /// Encode into `(frame message type, payload)` for the session layer.
    ///
    /// # Errors
    ///
    /// [`crossover_protocol::ProtocolError`] if validation or
    /// serialization fails (engine-built messages are always valid; this
    /// is defensive).
    pub fn encode(&self) -> Result<(u16, Vec<u8>), crossover_protocol::ProtocolError> {
        let payload = match self {
            Self::Offer(m) => m.encode_payload()?,
            Self::Accept(m) => m.encode_payload()?,
            Self::Decline(m) => m.encode_payload()?,
            Self::Data(m) => m.encode_payload()?,
            Self::Applied(m) => m.encode_payload()?,
        };
        Ok((self.message_type().wire(), payload))
    }
}

/// What the driver must do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Read the current clipboard text and report it back via
    /// [`ClipboardEngine::on_local_read`].
    ReadClipboard,
    /// Write `text` to the local clipboard and report the result via
    /// [`ClipboardEngine::on_write_result`].
    WriteClipboard {
        /// Transaction id the result must reference.
        id: Uuid,
        /// The content to install.
        text: String,
    },
    /// Send a message to the peer.
    Send(OutboundMessage),
    /// Call [`ClipboardEngine::on_retry_due`] with `id` after `delay`.
    ScheduleRetry {
        /// Transaction id to retry.
        id: Uuid,
        /// How long to wait.
        delay: Duration,
    },
    /// Call [`ClipboardEngine::on_settle_due`] after `delay` unless a
    /// newer change resets it (ADR 0006). Restarting an existing timer is
    /// expected: the driver keeps only the latest.
    ScheduleSettle {
        /// How long the clipboard must stay quiet.
        delay: Duration,
    },
}

/// An inbound clipboard message, decoded by the driver.
#[derive(Debug, Clone, PartialEq)]
pub enum InboundMessage {
    /// Peer announces a large item.
    Offer(ClipboardOffer),
    /// Peer accepts our offer.
    Accept(ClipboardAccept),
    /// Peer declines our offer.
    Decline(ClipboardDecline),
    /// Peer sends item content.
    Data(ClipboardData),
    /// Peer reports the verdict on our item.
    Applied(ClipboardApplied),
}

impl InboundMessage {
    /// Decode a frame if it carries a clipboard message; `Ok(None)` for
    /// non-clipboard traffic (the caller routes those elsewhere).
    ///
    /// # Errors
    ///
    /// [`crossover_protocol::ProtocolError`] for a clipboard-typed frame
    /// whose payload does not validate — a peer nonconformance the
    /// session must treat as fatal (docs/PROTOCOL.md §7).
    pub fn decode(
        message_type: u16,
        payload: &[u8],
    ) -> Result<Option<Self>, crossover_protocol::ProtocolError> {
        Ok(match MessageType::from_wire(message_type) {
            Some(MessageType::ClipboardOffer) => {
                Some(Self::Offer(ClipboardOffer::decode_payload(payload)?))
            }
            Some(MessageType::ClipboardAccept) => {
                Some(Self::Accept(ClipboardAccept::decode_payload(payload)?))
            }
            Some(MessageType::ClipboardDecline) => {
                Some(Self::Decline(ClipboardDecline::decode_payload(payload)?))
            }
            Some(MessageType::ClipboardData) => {
                Some(Self::Data(ClipboardData::decode_payload(payload)?))
            }
            Some(MessageType::ClipboardApplied) => {
                Some(Self::Applied(ClipboardApplied::decode_payload(payload)?))
            }
            _ => None,
        })
    }
}

/// Outbound transaction state.
///
/// `started` stamps when the local observation entered the pipeline, so
/// transaction latency is computed entirely on the originating machine's
/// clock — no cross-machine skew enters the measurement.
#[derive(Debug)]
enum Outbound {
    /// Offer sent; awaiting Accept/Decline.
    AwaitingAccept {
        data: ClipboardData,
        started: Instant,
    },
    /// Data sent; awaiting Applied.
    AwaitingApplied {
        meta: ClipboardMeta,
        started: Instant,
    },
}

impl Outbound {
    fn meta(&self) -> ClipboardMeta {
        match self {
            Self::AwaitingAccept { data, .. } => data.meta,
            Self::AwaitingApplied { meta, .. } => *meta,
        }
    }

    fn started(&self) -> Instant {
        match self {
            Self::AwaitingAccept { started, .. } | Self::AwaitingApplied { started, .. } => {
                *started
            }
        }
    }
}

/// Inbound write-with-retry state.
#[derive(Debug)]
struct PendingWrite {
    meta: ClipboardMeta,
    text: String,
    attempts_made: u32,
}

/// The sans-io clipboard engine. One instance per peer session scope.
#[derive(Debug)]
pub struct ClipboardEngine {
    /// Our device id — the `origin` stamped on items we mint.
    origin: Uuid,
    config: ClipboardConfig,
    /// Local observation counter (conflict ordering).
    next_sequence: u64,
    /// Hash of the last content this engine knows to be on the local
    /// clipboard (whatever its source) — outbound dedup.
    current_local_hash: Option<[u8; 32]>,
    /// Hashes we wrote locally; the provider's own-write notification
    /// must not echo them back (FR-3.3).
    applied_hashes: VecDeque<[u8; 32]>,
    /// At most one outbound transaction in flight; newer local copies
    /// supersede it.
    outbound: Option<Outbound>,
    /// An accepted inbound offer whose Data we await.
    expecting_data: Option<ClipboardMeta>,
    /// The write (with retries) currently underway.
    pending_write: Option<PendingWrite>,
    /// Optional metrics sink. Recorded alongside the `tracing` side
    /// effects the engine already emits at each decision point, so the
    /// semantic outcomes only this engine can see — sent, applied,
    /// superseded, conflicts, loop suppressions, latency — are counted at
    /// their source. `None` in unit tests and when the app runs without a
    /// registry.
    metrics: Option<Arc<Metrics>>,
}

impl ClipboardEngine {
    /// A fresh engine for `origin` (our device id).
    #[must_use]
    pub fn new(origin: Uuid, config: ClipboardConfig) -> Self {
        Self::with_metrics(origin, config, None)
    }

    /// A fresh engine that records its outcomes into `metrics`.
    #[must_use]
    pub fn with_metrics(
        origin: Uuid,
        config: ClipboardConfig,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self {
            origin,
            config,
            next_sequence: 0,
            current_local_hash: None,
            applied_hashes: VecDeque::new(),
            outbound: None,
            expecting_data: None,
            pending_write: None,
            metrics,
        }
    }

    /// Record into the metrics sink if one is attached; a no-op otherwise.
    fn record(&self, f: impl FnOnce(&Metrics)) {
        if let Some(metrics) = &self.metrics {
            f(metrics);
        }
    }

    /// The provider signaled a change.
    ///
    /// Deliberately does **not** read: reading takes the machine-global
    /// clipboard lock, and a notification only means "something changed",
    /// which during a burst is true many times per second. Wait for quiet
    /// (ADR 0006), then read once.
    pub fn on_local_change(&mut self) -> Vec<Action> {
        if self.config.transmit_debounce.is_zero() {
            return vec![Action::ReadClipboard];
        }
        vec![Action::ScheduleSettle {
            delay: self.config.transmit_debounce,
        }]
    }

    /// The settle window elapsed: now read the clipboard, once.
    pub fn on_settle_due(&mut self) -> Vec<Action> {
        vec![Action::ReadClipboard]
    }

    /// The driver read the clipboard. Decide whether anything travels.
    pub fn on_local_read(&mut self, content: Option<String>) -> Vec<Action> {
        let Some(text) = content else {
            return Vec::new(); // empty or non-text: nothing to sync
        };
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            tracing::warn!(
                byte_count = text.len(),
                max = MAX_CLIPBOARD_TEXT_BYTES,
                "local clipboard item exceeds the protocol maximum; not synchronized"
            );
            return Vec::new(); // graceful rejection (FR-3.6)
        }
        let hash = content_hash(text.as_bytes());

        // Loop prevention: this is content we ourselves applied.
        if self.applied_hashes.contains(&hash) {
            self.current_local_hash = Some(hash);
            self.record(Metrics::record_clipboard_loop_suppressed);
            return Vec::new();
        }
        // Dedup: unchanged content never re-sends.
        if self.current_local_hash == Some(hash) {
            return Vec::new();
        }
        self.current_local_hash = Some(hash);

        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let data = ClipboardData::from_content(
            Uuid::new_v4(),
            self.origin,
            sequence,
            ContentType::Utf8Text,
            text.into_bytes(),
        );
        // The read only happens after the clipboard has settled, so
        // whatever we just read is the content worth sending: transmit
        // it directly.
        self.start_outbound(data)
    }

    /// A decoded clipboard message arrived from the peer.
    pub fn on_peer_message(&mut self, message: InboundMessage) -> Vec<Action> {
        match message {
            InboundMessage::Offer(offer) => self.on_peer_offer(offer),
            InboundMessage::Accept(accept) => self.on_peer_accept(accept.id),
            InboundMessage::Decline(decline) => self.on_peer_decline(&decline),
            InboundMessage::Data(data) => self.on_peer_data(data),
            InboundMessage::Applied(applied) => self.on_peer_applied(&applied),
        }
    }

    /// The driver finished (or failed) a clipboard write.
    ///
    /// `retryable` distinguishes `Busy` (true) from `Unavailable`
    /// (false).
    pub fn on_write_result(&mut self, id: Uuid, result: Result<(), bool>) -> Vec<Action> {
        // Take-then-restore: no panic path exists (NFR-1 discipline).
        let Some(pending) = self.pending_write.take() else {
            tracing::debug!(clipboard_id = %id, "write result for no pending write; ignoring");
            return Vec::new();
        };
        if pending.meta.id != id {
            tracing::debug!(clipboard_id = %id, "write result for a superseded write; ignoring");
            self.pending_write = Some(pending);
            return Vec::new();
        }
        match result {
            Ok(()) => {
                self.remember_applied(pending.meta.content_hash);
                self.current_local_hash = Some(pending.meta.content_hash);
                self.record(Metrics::record_clipboard_applied);
                tracing::info!(
                    clipboard_id = %pending.meta.id,
                    origin_peer = %pending.meta.origin,
                    byte_count = pending.meta.content_length,
                    attempt_count = pending.attempts_made,
                    result = "applied",
                    "clipboard item installed"
                );
                vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                    id,
                    result: ApplyResult::Applied,
                }))]
            }
            Err(retryable) => {
                if retryable && pending.attempts_made < self.config.retry.max_attempts {
                    let delay = self.config.retry.delay;
                    tracing::debug!(
                        clipboard_id = %id,
                        attempt_count = pending.attempts_made,
                        "clipboard busy; retry scheduled"
                    );
                    self.pending_write = Some(pending);
                    return vec![Action::ScheduleRetry { id, delay }];
                }
                tracing::warn!(
                    clipboard_id = %pending.meta.id,
                    origin_peer = %pending.meta.origin,
                    attempt_count = pending.attempts_made,
                    result = "clipboard_unavailable",
                    "clipboard item could not be installed"
                );
                vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                    id,
                    result: ApplyResult::ClipboardUnavailable,
                }))]
            }
        }
    }

    /// A scheduled retry came due.
    pub fn on_retry_due(&mut self, id: Uuid) -> Vec<Action> {
        let Some(pending) = self.pending_write.as_mut() else {
            return Vec::new(); // superseded meanwhile
        };
        if pending.meta.id != id {
            return Vec::new();
        }
        pending.attempts_made += 1;
        vec![Action::WriteClipboard {
            id,
            text: pending.text.clone(),
        }]
    }

    /// A session (re)connected: re-announce our current item so the peers
    /// converge after any gap (reconnect-safe behavior; receiver-side
    /// dedup makes re-announcement cheap).
    pub fn on_session_established(&mut self) -> Vec<Action> {
        self.outbound = None;
        self.expecting_data = None;
        // Ask the driver to re-read: the clipboard may have changed while
        // disconnected, and re-reading routes through the normal dedup
        // (and then through the debounce, like any other observation).
        self.current_local_hash = None;
        vec![Action::ReadClipboard]
    }

    /// The session dropped: in-flight transaction state is meaningless
    /// now. Pending local writes finish (the content is already here).
    pub fn on_session_lost(&mut self) -> Vec<Action> {
        if let Some(outbound) = self.outbound.take() {
            tracing::debug!(
                clipboard_id = %outbound.meta().id,
                "outbound clipboard transaction abandoned: session lost"
            );
        }
        self.expecting_data = None;
        Vec::new()
    }

    // --- internals ---

    fn start_outbound(&mut self, data: ClipboardData) -> Vec<Action> {
        if let Some(previous) = self.outbound.take() {
            tracing::debug!(
                clipboard_id = %previous.meta().id,
                "outbound clipboard transaction superseded by newer local copy"
            );
        }
        self.record(Metrics::record_clipboard_sent);
        let meta = data.meta;
        let started = Instant::now();
        if meta.content_length <= CLIPBOARD_INLINE_MAX_BYTES as u64 {
            self.outbound = Some(Outbound::AwaitingApplied { meta, started });
            vec![Action::Send(OutboundMessage::Data(data))]
        } else {
            self.outbound = Some(Outbound::AwaitingAccept { data, started });
            vec![Action::Send(OutboundMessage::Offer(ClipboardOffer {
                meta,
            }))]
        }
    }

    fn on_peer_offer(&mut self, offer: ClipboardOffer) -> Vec<Action> {
        if let Some(reason) = self.conflict_verdict(offer.meta) {
            return vec![Action::Send(OutboundMessage::Decline(ClipboardDecline {
                id: offer.meta.id,
                reason,
            }))];
        }
        if self.current_local_hash == Some(offer.meta.content_hash) {
            // Already holding identical content: a sync success with zero
            // payload bytes moved (ADR 0005).
            return vec![Action::Send(OutboundMessage::Decline(ClipboardDecline {
                id: offer.meta.id,
                reason: DeclineReason::AlreadyHave,
            }))];
        }
        self.expecting_data = Some(offer.meta);
        vec![Action::Send(OutboundMessage::Accept(ClipboardAccept {
            id: offer.meta.id,
        }))]
    }

    fn on_peer_accept(&mut self, id: Uuid) -> Vec<Action> {
        match self.outbound.take() {
            Some(Outbound::AwaitingAccept { data, started }) if data.meta.id == id => {
                let meta = data.meta;
                self.outbound = Some(Outbound::AwaitingApplied { meta, started });
                vec![Action::Send(OutboundMessage::Data(data))]
            }
            other => {
                self.outbound = other; // restore whatever it was
                tracing::debug!(clipboard_id = %id, "stale or unknown accept; ignoring");
                Vec::new()
            }
        }
    }

    fn on_peer_decline(&mut self, decline: &ClipboardDecline) -> Vec<Action> {
        match self.outbound.take() {
            Some(Outbound::AwaitingAccept { data, started }) if data.meta.id == decline.id => {
                let latency_ms = elapsed_ms(started);
                self.record(|m| m.record_clipboard_latency(clamp_ms(latency_ms)));
                let outcome = match decline.reason {
                    // Success-shaped: the peer already has the content, or
                    // a newer item won the race.
                    DeclineReason::AlreadyHave | DeclineReason::Superseded => "converged",
                    DeclineReason::TooLarge
                    | DeclineReason::NotReady
                    | DeclineReason::UnsupportedType => "declined",
                };
                tracing::info!(
                    clipboard_id = %decline.id,
                    reason = ?decline.reason,
                    result = outcome,
                    latency_ms,
                    "clipboard offer resolved"
                );
                Vec::new()
            }
            other => {
                self.outbound = other;
                tracing::debug!(clipboard_id = %decline.id, "stale or unknown decline; ignoring");
                Vec::new()
            }
        }
    }

    fn on_peer_data(&mut self, data: ClipboardData) -> Vec<Action> {
        // Data must match an accepted offer, or be inline-sized. Anything
        // else is peer nonconformance: refuse, keep the session.
        let expected = self.expecting_data.take();
        let matches_offer = expected.is_some_and(|meta| meta.id == data.meta.id);
        if !matches_offer && data.meta.content_length > CLIPBOARD_INLINE_MAX_BYTES as u64 {
            tracing::warn!(
                clipboard_id = %data.meta.id,
                byte_count = data.meta.content_length,
                "oversized inline clipboard data without an accepted offer; rejecting"
            );
            return vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: data.meta.id,
                result: ApplyResult::ContentRejected,
            }))];
        }

        if let Some(reason) = self.conflict_verdict(data.meta) {
            debug_assert_eq!(reason, DeclineReason::Superseded);
            self.record(Metrics::record_clipboard_superseded);
            return vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: data.meta.id,
                result: ApplyResult::Superseded,
            }))];
        }

        // Loop/echo guard: identical content is a success without a write.
        if self.current_local_hash == Some(data.meta.content_hash) {
            return vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: data.meta.id,
                result: ApplyResult::Applied,
            }))];
        }

        // Wire validation guarantees UTF-8 for Utf8Text; defensive here.
        let Ok(text) = String::from_utf8(data.content) else {
            return vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: data.meta.id,
                result: ApplyResult::ContentRejected,
            }))];
        };

        if let Some(superseded) = self.pending_write.take() {
            tracing::debug!(
                clipboard_id = %superseded.meta.id,
                "pending write superseded by newer inbound item"
            );
        }
        self.pending_write = Some(PendingWrite {
            meta: data.meta,
            text: text.clone(),
            attempts_made: 1,
        });
        vec![Action::WriteClipboard {
            id: data.meta.id,
            text,
        }]
    }

    fn on_peer_applied(&mut self, applied: &ClipboardApplied) -> Vec<Action> {
        match self.outbound.take() {
            Some(Outbound::AwaitingApplied { meta, started }) if meta.id == applied.id => {
                let outcome = match applied.result {
                    ApplyResult::Applied => "applied",
                    ApplyResult::Superseded => "superseded",
                    ApplyResult::ClipboardUnavailable => "clipboard_unavailable",
                    ApplyResult::ContentRejected => "content_rejected",
                };
                // Round trip measured on this machine's clock alone:
                // local observation through the destination's verdict
                // (docs/TESTING.md §4 — the number Phase 6 will want).
                let latency_ms = elapsed_ms(started);
                self.record(|m| m.record_clipboard_latency(clamp_ms(latency_ms)));
                tracing::info!(
                    clipboard_id = %applied.id,
                    result = outcome,
                    byte_count = meta.content_length,
                    latency_ms,
                    "clipboard transaction closed"
                );
                Vec::new()
            }
            other => {
                self.outbound = other;
                tracing::debug!(clipboard_id = %applied.id, "stale or unknown applied; ignoring");
                Vec::new()
            }
        }
    }

    /// The deterministic conflict order (FR-3.5): when an inbound item
    /// races our in-flight outbound one, `(sequence, origin)` decides —
    /// identically on both machines. `Some(Superseded)` means the inbound
    /// item lost and must be refused; `None` means it wins (our outbound
    /// closes locally as superseded).
    fn conflict_verdict(&mut self, inbound: ClipboardMeta) -> Option<DeclineReason> {
        let ours = self.outbound.as_ref()?.meta();
        // Reaching here means an inbound item arrived while our own was in
        // flight: a genuine near-simultaneous race (FR-3.5).
        self.record(Metrics::record_clipboard_conflict);
        let inbound_wins =
            (inbound.sequence, inbound.origin.as_bytes()) > (ours.sequence, ours.origin.as_bytes());
        if inbound_wins {
            let latency_ms = self
                .outbound
                .as_ref()
                .map_or(0, |o| elapsed_ms(o.started()));
            tracing::info!(
                clipboard_id = %ours.id,
                result = "superseded",
                latency_ms,
                "outbound item lost the conflict race; converging on the peer's item"
            );
            self.outbound = None;
            None
        } else {
            tracing::info!(
                clipboard_id = %inbound.id,
                result = "superseded",
                "inbound item lost the conflict race; keeping ours in flight"
            );
            Some(DeclineReason::Superseded)
        }
    }

    fn remember_applied(&mut self, hash: [u8; 32]) {
        if self.applied_hashes.len() >= APPLIED_HASH_MEMORY {
            self.applied_hashes.pop_front();
        }
        self.applied_hashes.push_back(hash);
    }
}

/// Milliseconds since `started`, saturating into `u64` for logging.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Narrow a millisecond duration to the `u32` the latency histogram
/// keeps, saturating rather than wrapping (a clipboard round trip past 49
/// days is a broken clock, not a real sample).
fn clamp_ms(ms: u64) -> u32 {
    u32::try_from(ms).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crossover_protocol::clipboard::{
        ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ClipboardApplied, ClipboardData, ClipboardMeta,
        ClipboardOffer, ContentType, DeclineReason, content_hash,
    };

    use std::time::Duration;

    use super::{
        Action, ClipboardConfig, ClipboardEngine, InboundMessage, OutboundMessage, RetryPolicy,
    };

    fn engine(origin_fill: u8) -> ClipboardEngine {
        ClipboardEngine::new(Uuid::from_bytes([origin_fill; 16]), ClipboardConfig::new())
    }

    /// Copy locally and fire the transmit trigger, since these tests are
    /// about what travels, not about debounce timing (which has its own
    /// tests below).
    /// A change schedules the settle window; only then do we read
    /// (ADR 0006). These tests care what travels, not about timing.
    fn copy(engine: &mut ClipboardEngine, text: &str) -> Vec<Action> {
        let scheduled = engine.on_local_change();
        assert!(
            matches!(scheduled.as_slice(), [Action::ScheduleSettle { .. }]),
            "a change should schedule a settle, not read now: {scheduled:?}"
        );
        assert_eq!(engine.on_settle_due(), vec![Action::ReadClipboard]);
        engine.on_local_read(Some(text.to_owned()))
    }

    fn sent(actions: &[Action]) -> Vec<&OutboundMessage> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn small_copy_goes_inline_large_copy_gets_offered() {
        let mut e = engine(0xAA);
        let actions = copy(&mut e, "small");
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Data(_)]
        ));

        let mut e = engine(0xAA);
        let big = "x".repeat(CLIPBOARD_INLINE_MAX_BYTES + 1);
        let actions = copy(&mut e, &big);
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Offer(_)]
        ));
    }

    #[test]
    fn unchanged_content_is_never_resent() {
        let mut e = engine(0xAA);
        assert_eq!(sent(&copy(&mut e, "same")).len(), 1);
        assert_eq!(sent(&copy(&mut e, "same")).len(), 0);
        assert_eq!(sent(&copy(&mut e, "different")).len(), 1);
    }

    #[test]
    fn oversized_and_empty_local_content_is_ignored_gracefully() {
        let mut e = engine(0xAA);
        assert!(e.on_local_read(None).is_empty());
        let huge = "x".repeat(4 * 1024 * 1024 + 1);
        assert!(e.on_local_read(Some(huge)).is_empty());
    }

    /// The full loop-prevention cycle: receive, write, own-write
    /// notification, silence.
    #[test]
    fn applied_remote_content_is_not_echoed_back() {
        let mut receiver = engine(0xBB);
        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            0,
            ContentType::Utf8Text,
            b"from peer".to_vec(),
        );
        let id = item.meta.id;

        let actions = receiver.on_peer_message(InboundMessage::Data(item));
        assert!(matches!(
            actions.as_slice(),
            [Action::WriteClipboard { .. }]
        ));

        let actions = receiver.on_write_result(id, Ok(()));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Applied(ClipboardApplied {
                result: ApplyResult::Applied,
                ..
            })]
        ));

        // The provider now notifies for our own write (the contract
        // term); the engine must stay silent. The notification schedules
        // a settle, the read happens after it, and the loop guard bites.
        let actions = receiver.on_local_change();
        assert!(matches!(
            actions.as_slice(),
            [Action::ScheduleSettle { .. }]
        ));
        assert_eq!(receiver.on_settle_due(), vec![Action::ReadClipboard]);
        let actions = receiver.on_local_read(Some("from peer".to_owned()));
        assert!(
            actions.is_empty(),
            "echoed an applied item back: {actions:?}"
        );
    }

    #[test]
    fn busy_writes_retry_bounded_then_report_unavailable() {
        let policy = RetryPolicy {
            max_attempts: 3,
            delay: std::time::Duration::from_millis(50),
        };
        let mut e = ClipboardEngine::new(
            Uuid::from_bytes([0xBB; 16]),
            ClipboardConfig {
                retry: policy,
                ..ClipboardConfig::new()
            },
        );
        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            0,
            ContentType::Utf8Text,
            b"contended".to_vec(),
        );
        let id = item.meta.id;
        assert!(matches!(
            e.on_peer_message(InboundMessage::Data(item)).as_slice(),
            [Action::WriteClipboard { .. }]
        ));

        assert!(matches!(
            e.on_write_result(id, Err(true)).as_slice(),
            [Action::ScheduleRetry { .. }]
        ));
        assert!(matches!(
            e.on_retry_due(id).as_slice(),
            [Action::WriteClipboard { .. }]
        ));
        assert!(matches!(
            e.on_write_result(id, Err(true)).as_slice(),
            [Action::ScheduleRetry { .. }]
        ));
        assert!(matches!(
            e.on_retry_due(id).as_slice(),
            [Action::WriteClipboard { .. }]
        ));
        // Third attempt fails: the cap closes the transaction honestly.
        let actions = e.on_write_result(id, Err(true));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Applied(ClipboardApplied {
                result: ApplyResult::ClipboardUnavailable,
                ..
            })]
        ));

        // Unretryable failure reports immediately.
        let item2 = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            1,
            ContentType::Utf8Text,
            b"broken".to_vec(),
        );
        let id2 = item2.meta.id;
        e.on_peer_message(InboundMessage::Data(item2));
        let actions = e.on_write_result(id2, Err(false));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Applied(ClipboardApplied {
                result: ApplyResult::ClipboardUnavailable,
                ..
            })]
        ));
    }

    #[test]
    fn offer_for_content_already_held_gets_a_success_shaped_decline() {
        let mut e = engine(0xBB);
        copy(&mut e, "shared content");

        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xAA; 16]),
            sequence: 100,
            content_type: ContentType::Utf8Text,
            content_length: (CLIPBOARD_INLINE_MAX_BYTES + 5) as u64,
            content_hash: content_hash("shared content".as_bytes()),
        };
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        match sent(&actions).as_slice() {
            [OutboundMessage::Decline(d)] => {
                assert!(matches!(
                    d.reason,
                    DeclineReason::AlreadyHave | DeclineReason::Superseded
                ));
            }
            other => panic!("expected a decline, got {other:?}"),
        }
    }

    /// The crown test: two engines with crossing copies, messages pumped
    /// until quiescent, must converge on the same content on both sides,
    /// regardless of delivery order.
    #[test]
    fn crossing_copies_converge_deterministically() {
        struct Host {
            engine: ClipboardEngine,
            clipboard: String,
            pending_write: Option<(Uuid, String)>,
        }
        impl Host {
            fn drive(&mut self, actions: Vec<Action>, outbox: &mut Vec<InboundMessage>) {
                for action in actions {
                    match action {
                        Action::Send(m) => outbox.push(match m {
                            OutboundMessage::Offer(x) => InboundMessage::Offer(x),
                            OutboundMessage::Accept(x) => InboundMessage::Accept(x),
                            OutboundMessage::Decline(x) => InboundMessage::Decline(x),
                            OutboundMessage::Data(x) => InboundMessage::Data(x),
                            OutboundMessage::Applied(x) => InboundMessage::Applied(x),
                        }),
                        Action::WriteClipboard { id, text } => {
                            self.pending_write = Some((id, text));
                        }
                        Action::ScheduleSettle { .. } => {
                            let read = self.engine.on_settle_due();
                            self.drive(read, outbox);
                        }
                        Action::ReadClipboard | Action::ScheduleRetry { .. } => {}
                    }
                }
                // Complete writes instantly (fake clipboard, no
                // contention) and run the own-write notification cycle.
                if let Some((id, text)) = self.pending_write.take() {
                    self.clipboard.clone_from(&text);
                    let more = self.engine.on_write_result(id, Ok(()));
                    self.drive(more, outbox);
                    let mut cycle = self.engine.on_local_change();
                    cycle.extend(self.engine.on_local_read(Some(text)));
                    self.drive(cycle, outbox);
                }
            }
        }

        for (a_first, label) in [(true, "a-delivered-first"), (false, "b-delivered-first")] {
            let mut a = Host {
                engine: engine(0x01),
                clipboard: String::new(),
                pending_write: None,
            };
            let mut b = Host {
                engine: engine(0x02),
                clipboard: String::new(),
                pending_write: None,
            };

            // Crossing copies: both observe local changes before any
            // message arrives. Equal sequences (0), so origin 0x02 must
            // win on both machines.
            let mut a_out = Vec::new();
            let mut b_out = Vec::new();
            let actions = copy(&mut a.engine, "from A");
            "from A".clone_into(&mut a.clipboard);
            a.drive(actions, &mut a_out);
            let actions = copy(&mut b.engine, "from B");
            "from B".clone_into(&mut b.clipboard);
            b.drive(actions, &mut b_out);

            let mut rounds = 0;
            while !a_out.is_empty() || !b_out.is_empty() {
                rounds += 1;
                assert!(rounds < 32, "no convergence ({label})");
                if a_first {
                    for m in std::mem::take(&mut a_out) {
                        let actions = b.engine.on_peer_message(m);
                        b.drive(actions, &mut b_out);
                    }
                    for m in std::mem::take(&mut b_out) {
                        let actions = a.engine.on_peer_message(m);
                        a.drive(actions, &mut a_out);
                    }
                } else {
                    for m in std::mem::take(&mut b_out) {
                        let actions = a.engine.on_peer_message(m);
                        a.drive(actions, &mut a_out);
                    }
                    for m in std::mem::take(&mut a_out) {
                        let actions = b.engine.on_peer_message(m);
                        b.drive(actions, &mut b_out);
                    }
                }
            }

            assert_eq!(a.clipboard, "from B", "wrong winner on A ({label})");
            assert_eq!(b.clipboard, "from B", "wrong winner on B ({label})");
        }
    }

    #[test]
    fn newer_local_copy_supersedes_the_in_flight_one() {
        let mut e = engine(0xAA);
        copy(&mut e, "first");
        let actions = copy(&mut e, "second");
        let msgs = sent(&actions);
        let OutboundMessage::Data(second) = msgs[0] else {
            panic!("expected data");
        };
        let second_id = second.meta.id;

        // A stale ack for an unknown id is ignored quietly...
        let stale = ClipboardApplied {
            id: Uuid::new_v4(),
            result: ApplyResult::Applied,
        };
        assert!(e.on_peer_message(InboundMessage::Applied(stale)).is_empty());
        // ...and the real ack closes the live transaction.
        let done = ClipboardApplied {
            id: second_id,
            result: ApplyResult::Applied,
        };
        assert!(e.on_peer_message(InboundMessage::Applied(done)).is_empty());
    }

    #[test]
    fn reconnect_re_announces_current_content() {
        let mut e = engine(0xAA);
        copy(&mut e, "persistent");
        e.on_session_lost();
        let actions = e.on_session_established();
        assert_eq!(actions, vec![Action::ReadClipboard]);
        // The established reset cleared the dedup hash, so the same
        // content travels again for post-gap convergence.
        assert_eq!(
            sent(&e.on_local_read(Some("persistent".to_owned()))).len(),
            1
        );
    }

    /// ADR 0006: a burst of notifications costs one clipboard *read*,
    /// not one per notification. Reading takes the machine-global lock,
    /// so reacting to every notification is itself the contention the
    /// two-machine soak exposed.
    #[test]
    fn a_burst_of_changes_reads_the_clipboard_once() {
        let mut e = engine(0xAA);

        for i in 0..10 {
            let actions = e.on_local_change();
            assert!(
                matches!(actions.as_slice(), [Action::ScheduleSettle { .. }]),
                "notification {i} read the clipboard immediately: {actions:?}"
            );
        }

        // The window elapses once: one read, then one send of whatever
        // the clipboard settled on.
        assert_eq!(e.on_settle_due(), vec![Action::ReadClipboard]);
        let actions = e.on_local_read(Some("settled content".to_owned()));
        let msgs = sent(&actions);
        assert_eq!(msgs.len(), 1);
        let OutboundMessage::Data(data) = msgs[0] else {
            panic!("expected inline data");
        };
        assert_eq!(data.content, b"settled content");
    }

    #[test]
    fn zero_debounce_reads_eagerly() {
        let mut e = ClipboardEngine::new(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig {
                transmit_debounce: Duration::ZERO,
                ..ClipboardConfig::new()
            },
        );
        // The escape hatch for callers who want no wait at all.
        assert_eq!(e.on_local_change(), vec![Action::ReadClipboard]);
        assert_eq!(sent(&e.on_local_read(Some("eager".to_owned()))).len(), 1);
    }

    #[test]
    fn metrics_record_the_semantic_clipboard_outcomes() {
        use std::sync::Arc;

        use crate::metrics::Metrics;

        let metrics = Arc::new(Metrics::new());
        let mut e = ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        );

        // A local copy is one item sent.
        let actions = copy(&mut e, "hello");
        let OutboundMessage::Data(data) = sent(&actions)[0] else {
            panic!("expected inline data");
        };
        let sent_id = data.meta.id;
        assert_eq!(metrics.snapshot().clipboard_sent, 1);

        // The peer's verdict closes the round trip: one latency sample, on
        // this machine's clock.
        e.on_peer_message(InboundMessage::Applied(ClipboardApplied {
            id: sent_id,
            result: ApplyResult::Applied,
        }));
        assert_eq!(metrics.snapshot().latency_samples, 1);

        // Receiving and writing a peer item counts one applied.
        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xBB; 16]),
            0,
            ContentType::Utf8Text,
            b"from peer".to_vec(),
        );
        let write_id = item.meta.id;
        e.on_peer_message(InboundMessage::Data(item));
        e.on_write_result(write_id, Ok(()));
        assert_eq!(metrics.snapshot().clipboard_applied, 1);

        // The provider's own-write notification is suppressed, not resent.
        e.on_local_change();
        e.on_settle_due();
        e.on_local_read(Some("from peer".to_owned()));
        let snap = metrics.snapshot();
        assert_eq!(snap.clipboard_loop_suppressed, 1);
        // No race occurred in this sequence.
        assert_eq!(snap.clipboard_conflicts, 0);
    }

    #[test]
    fn metrics_record_a_conflict_when_an_inbound_item_races_ours() {
        use std::sync::Arc;

        use crate::metrics::Metrics;

        let metrics = Arc::new(Metrics::new());
        let mut e = ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        );

        // Our item is in flight when a higher-origin inbound item arrives:
        // the deterministic order makes theirs win, and it counts as one
        // conflict resolved.
        copy(&mut e, "ours");
        let inbound = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xBB; 16]),
            0,
            ContentType::Utf8Text,
            b"theirs".to_vec(),
        );
        e.on_peer_message(InboundMessage::Data(inbound));
        assert_eq!(metrics.snapshot().clipboard_conflicts, 1);
    }

    #[test]
    fn oversized_inline_data_without_offer_is_rejected() {
        let mut e = engine(0xBB);
        let big = "x".repeat(CLIPBOARD_INLINE_MAX_BYTES + 1);
        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            0,
            ContentType::Utf8Text,
            big.into_bytes(),
        );
        let actions = e.on_peer_message(InboundMessage::Data(item));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Applied(ClipboardApplied {
                result: ApplyResult::ContentRejected,
                ..
            })]
        ));
    }
}
