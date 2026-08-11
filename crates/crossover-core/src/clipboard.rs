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
//! - **Bounded transfer lifetime** (ADR 0014, NFR-1): every transaction
//!   that retains content — an offer awaiting an answer, a chunk stream,
//!   an accepted offer awaiting its bytes — carries a deadline, so an
//!   answer that never comes costs a bounded amount of memory for a
//!   bounded time instead of pinning up to `MAX_CLIPBOARD_IMAGE_BYTES`
//!   until the session happens to end.
//!
//! Content is **typed and opaque** since ADR 0014: items carry a
//! [`ContentType`] and bytes, text is one type and a raster image another,
//! and nothing here transcodes, parses or even looks at image bytes — the
//! hash and the length are the only things ever computed over them.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crossover_platform::{ClipboardContent, ClipboardImageFormat};
use crossover_protocol::clipboard::{
    ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ChunkOutcome, ChunkPlan, ChunkReassembly,
    ClipboardAccept, ClipboardApplied, ClipboardChunk, ClipboardData, ClipboardDecline,
    ClipboardMeta, ClipboardOffer, ContentType, DeclineReason, ImageFormat, content_hash,
};
use crossover_protocol::hello::MessageType;

use crate::metrics::Metrics;

/// How many recently-applied content hashes are remembered for loop
/// prevention. Notifications coalesce, so a small window suffices; the
/// bound keeps memory fixed (NFR-1).
const APPLIED_HASH_MEMORY: usize = 8;

/// How many clipboard protocol violations a peer may commit on one
/// session before it is terminated (docs/PROTOCOL.md §7: a violation is
/// rejected and counted; repeated violations end the session).
///
/// Small, but not one. A conforming peer commits zero, yet a *benign*
/// race can produce a few — chunks already in flight for a transfer this
/// side abandoned on supersession or session loss arrive with nothing to
/// belong to, and killing a healthy session over an in-flight tail would
/// be its own defect. A handful absorbs that; nothing absorbs a peer
/// streaming violations, which is the point: without a cap, unanswered
/// junk is free for the sender and unbounded log volume for us.
const MAX_CLIPBOARD_VIOLATIONS: u32 = 8;

/// How many recently finished chunked transfers are remembered by id.
///
/// Small on purpose: it exists only to recognize the *tail* of a transfer
/// this side stopped caring about — chunks already in flight when the
/// transfer was superseded, abandoned, or completed. Recognizing them
/// keeps a benign race off the violation budget, which matters at image
/// scale: a superseded transfer can leave a whole background lane's worth
/// of chunks in flight, far past [`MAX_CLIPBOARD_VIOLATIONS`], and killing
/// a healthy session over that would be its own defect.
const RECENT_TRANSFER_MEMORY: usize = 4;

/// Clipboard engine tuning. Grouped because all three knobs are timing
/// policy, and tests need to shrink them without pretending the
/// production defaults are different.
#[derive(Debug, Clone, Default)]
pub struct ClipboardConfig {
    /// Bounded retry for `Busy` clipboard writes (FR-3.4).
    pub retry: RetryPolicy,
    /// Quiet period before staged content is transmitted (ADR 0006).
    pub transmit_debounce: Duration,
    /// Deadline on a transfer that retains content (ADR 0014).
    pub transfer_timeout: Duration,
}

impl ClipboardConfig {
    /// Production defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            retry: RetryPolicy::default(),
            transmit_debounce: TRANSMIT_DEBOUNCE,
            transfer_timeout: TRANSFER_TIMEOUT,
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

/// How long a content-retaining transfer may stay unfinished before it is
/// abandoned (ADR 0014).
///
/// The bound exists because of what a transfer *holds*. An offered item
/// keeps its content until the answer arrives, and an accepted offer keeps
/// a reassembly buffer sized from the offered length — up to
/// `MAX_CLIPBOARD_IMAGE_BYTES`, 64 MiB. Session-scoped cleanup alone is
/// not a bound: a session can live for days, and a peer that offers and
/// then goes quiet would pin that memory for all of it. Nothing about
/// that needs malice; a peer killed between `Accept` and its first chunk
/// produces it.
///
/// Sixty seconds is chosen to be *far* longer than any honest transfer and
/// still short enough to be a bound. The transfer itself is milliseconds
/// (a 64 MiB image is 0.2 s on 2.5 `GbE`, 0.5 s on 1 `GbE`), and the margin is
/// for the deliberate starvation ADR 0013 allows: clipboard bulk yields to
/// live input with no aging, so a transfer *should* be able to wait out a
/// long burst of typing. A transfer that loses even a minute to that is
/// better abandoned observably than kept forever — the content is still on
/// the origin's clipboard, and re-copying re-sends it.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_mins(1);

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
    /// The item content, whole (text).
    Data(ClipboardData),
    /// One fragment of a chunked item (ADR 0014). Each chunk is its own
    /// frame and its own command, because a chunk is the preemption unit:
    /// the writer takes exactly one between checks of the interactive lane
    /// (ADR 0013), which is what keeps live input ahead of a transfer.
    Chunk(ClipboardChunk),
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
            Self::Chunk(_) => MessageType::ClipboardChunk,
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
            Self::Chunk(m) => m.encode_payload()?,
            Self::Applied(m) => m.encode_payload()?,
        };
        Ok((self.message_type().wire(), payload))
    }
}

/// Which half of the transaction machine a deadline belongs to.
///
/// Two independent timers rather than one: an outbound offer and an
/// inbound reassembly can be in flight at the same moment, and a single
/// shared deadline would let the later one keep resetting the earlier
/// one's clock — which is not a bound at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferScope {
    /// Our own item, offered or streaming (the retained content buffer).
    Outbound,
    /// A peer item we accepted (the reassembly buffer, or an accepted
    /// text offer whose `Data` has not arrived).
    Inbound,
}

/// What the driver must do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Read the current clipboard content and report it back via
    /// [`ClipboardEngine::on_local_read`].
    ReadClipboard,
    /// Write `content` to the local clipboard and report the result via
    /// [`ClipboardEngine::on_write_result`].
    WriteClipboard {
        /// Transaction id the result must reference.
        id: Uuid,
        /// The content to install. Shared rather than owned so a retry
        /// (FR-3.4) re-issues the write without copying what may be a
        /// 64 MiB image each time.
        content: Arc<ClipboardContent>,
    },
    /// Send a message to the peer. After a [`OutboundMessage::Chunk`] has
    /// been handed to the send path, call
    /// [`ClipboardEngine::on_chunk_sent`] for the next one — the engine
    /// emits chunks one at a time, straight out of the retained item
    /// buffer, so neither it nor the driver ever holds a second copy of
    /// the image (ADR 0014, NFR-1).
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
    /// Call [`ClipboardEngine::on_transfer_timeout`] with `scope` and
    /// `generation` after `delay` (ADR 0014). Generation-tagged like
    /// [`Action::ScheduleSettle`]: a timer for a superseded transfer
    /// fires into a no-op, so nothing has to be cancelled.
    ScheduleTransferTimeout {
        /// Which half of the machine the deadline covers.
        scope: TransferScope,
        /// Which transfer the deadline belongs to.
        generation: u64,
        /// How long the transfer may take.
        delay: Duration,
    },
    /// End the session: the peer's clipboard traffic committed repeated
    /// protocol violations (docs/PROTOCOL.md §7 — a single violation is
    /// rejected and counted, repetition is fatal).
    TerminateSession {
        /// Operator-facing diagnostic naming what the peer did.
        reason: String,
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
    /// Peer sends one fragment of a chunked item (ADR 0014).
    Chunk(ClipboardChunk),
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
            Some(MessageType::ClipboardChunk) => {
                Some(Self::Chunk(ClipboardChunk::decode_payload(payload)?))
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
///
/// `content` is **the** outbound memory commitment, and it is deliberate:
/// exactly one item is retained at a time (a newer local copy supersedes
/// and replaces it), for at most [`ClipboardConfig::transfer_timeout`],
/// bounded by the content type's maximum — 64 MiB for an image
/// (ADR 0014). Chunks are sliced out of it on demand rather than
/// pre-rendered, so the peak is that one buffer plus one chunk.
#[derive(Debug)]
enum Outbound {
    /// Offer sent; awaiting Accept/Decline. Holds the content, because
    /// an Accept means "send it now".
    AwaitingAccept {
        meta: ClipboardMeta,
        content: Vec<u8>,
        started: Instant,
    },
    /// Accepted and streaming chunks (ADR 0014). `next_index` is the
    /// chunk to emit when the driver comes back for another.
    Streaming {
        meta: ClipboardMeta,
        content: Vec<u8>,
        plan: ChunkPlan,
        next_index: u32,
        started: Instant,
    },
    /// Everything sent; awaiting Applied. Content released.
    AwaitingApplied {
        meta: ClipboardMeta,
        started: Instant,
    },
}

impl Outbound {
    fn meta(&self) -> ClipboardMeta {
        match self {
            Self::AwaitingAccept { meta, .. }
            | Self::Streaming { meta, .. }
            | Self::AwaitingApplied { meta, .. } => *meta,
        }
    }

    fn started(&self) -> Instant {
        match self {
            Self::AwaitingAccept { started, .. }
            | Self::Streaming { started, .. }
            | Self::AwaitingApplied { started, .. } => *started,
        }
    }

    /// Whether this state retains an item buffer, and so needs a deadline.
    const fn retains_content(&self) -> bool {
        matches!(self, Self::AwaitingAccept { .. } | Self::Streaming { .. })
    }
}

/// Inbound write-with-retry state.
#[derive(Debug)]
struct PendingWrite {
    meta: ClipboardMeta,
    content: Arc<ClipboardContent>,
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
    /// An accepted inbound offer whose Data we await (text).
    expecting_data: Option<ClipboardMeta>,
    /// The accepted inbound *chunked* offer being reassembled (ADR 0014).
    ///
    /// At most one, ever: it is the receiver's whole memory commitment,
    /// and a second concurrent one would double a bound the protocol
    /// states as singular. A newer accepted offer replaces it, which is
    /// the same supersession rule `expecting_data` has always had.
    reassembly: Option<ChunkReassembly>,
    /// Ids of chunked transfers recently finished or abandoned, so their
    /// in-flight tail is recognized as the benign race it is rather than
    /// charged to the violation budget.
    recent_transfers: VecDeque<Uuid>,
    /// Deadline generations (ADR 0014). Bumped when a transfer that
    /// retains content starts; a timeout for an older generation is a
    /// no-op, so superseded timers need no cancellation.
    outbound_generation: u64,
    inbound_generation: u64,
    /// The write (with retries) currently underway.
    pending_write: Option<PendingWrite>,
    /// Clipboard protocol violations this peer has committed since the
    /// session was established (docs/PROTOCOL.md §7). Reset by
    /// [`ClipboardEngine::on_session_established`], so the budget is per
    /// session rather than per process.
    violations: u32,
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
            reassembly: None,
            recent_transfers: VecDeque::new(),
            outbound_generation: 0,
            inbound_generation: 0,
            pending_write: None,
            violations: 0,
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
    ///
    /// Typed since ADR 0014: the same rules apply to every content type —
    /// only the bound and the flow differ, and both come from the type.
    pub fn on_local_read(&mut self, content: Option<ClipboardContent>) -> Vec<Action> {
        let Some(content) = content else {
            return Vec::new(); // empty, or a format this build cannot read
        };
        let (content_type, bytes) = into_wire(content);
        let max = content_type.max_content_bytes();
        if bytes.len() as u64 > max {
            tracing::warn!(
                byte_count = bytes.len(),
                max,
                content_type = ?content_type,
                "local clipboard item exceeds the protocol maximum; not synchronized"
            );
            return Vec::new(); // graceful rejection (FR-3.6)
        }
        if content_type.is_chunked() && bytes.is_empty() {
            // A zero-byte image is not an image, and the chunk arithmetic
            // has nothing to reconcile: refuse locally rather than mint an
            // item the wire would reject (docs/PROTOCOL.md §5).
            tracing::warn!(content_type = ?content_type, "empty local clipboard item; not synchronized");
            return Vec::new();
        }
        let hash = content_hash(&bytes);

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
        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: self.origin,
            sequence,
            content_type,
            content_length: bytes.len() as u64,
            content_hash: hash,
        };
        // The read only happens after the clipboard has settled, so
        // whatever we just read is the content worth sending: transmit
        // it directly.
        self.start_outbound(meta, bytes)
    }

    /// A decoded clipboard message arrived from the peer.
    pub fn on_peer_message(&mut self, message: InboundMessage) -> Vec<Action> {
        match message {
            InboundMessage::Offer(offer) => self.on_peer_offer(offer),
            InboundMessage::Accept(accept) => self.on_peer_accept(accept.id),
            InboundMessage::Decline(decline) => self.on_peer_decline(&decline),
            InboundMessage::Data(data) => self.on_peer_data(data),
            InboundMessage::Chunk(chunk) => self.on_peer_chunk(&chunk),
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
                drop(pending.content); // release the item buffer promptly
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
            content: Arc::clone(&pending.content),
        }]
    }

    /// A session (re)connected: re-announce our current item so the peers
    /// converge after any gap (reconnect-safe behavior; receiver-side
    /// dedup makes re-announcement cheap).
    pub fn on_session_established(&mut self) -> Vec<Action> {
        self.outbound = None;
        self.expecting_data = None;
        self.reassembly = None;
        self.recent_transfers.clear();
        // A fresh session gets a fresh violation budget: the counter
        // bounds one peer's misbehaviour on one connection, not a
        // process-lifetime grudge.
        self.violations = 0;
        // Ask the driver to re-read: the clipboard may have changed while
        // disconnected, and re-reading routes through the normal dedup
        // (and then through the debounce, like any other observation).
        self.current_local_hash = None;
        vec![Action::ReadClipboard]
    }

    /// The session dropped: in-flight transaction state is meaningless
    /// now. Pending local writes finish (the content is already here).
    ///
    /// Every buffer the transaction machine can hold is released here —
    /// the retained outbound item and the inbound reassembly both, either
    /// of which can be `MAX_CLIPBOARD_IMAGE_BYTES` (ADR 0014). Nothing is
    /// sent: the peer is gone, and the deadline that would have answered
    /// it becomes moot with the session.
    pub fn on_session_lost(&mut self) -> Vec<Action> {
        if let Some(outbound) = self.outbound.take() {
            tracing::debug!(
                clipboard_id = %outbound.meta().id,
                "outbound clipboard transaction abandoned: session lost"
            );
        }
        if let Some(meta) = self.expecting_data.take() {
            tracing::debug!(
                clipboard_id = %meta.id,
                "accepted inbound offer abandoned: session lost"
            );
        }
        if let Some(reassembly) = self.reassembly.take() {
            tracing::debug!(
                clipboard_id = %reassembly.meta().id,
                byte_count = reassembly.received_bytes(),
                "inbound chunked transfer abandoned: session lost"
            );
        }
        Vec::new()
    }

    /// A transfer deadline came due (ADR 0014).
    ///
    /// Abandoning is observable and never fatal: the state is released,
    /// the origin of an inbound transfer is told the truth so its
    /// transaction closes instead of stalling (NFR-3), and the machine is
    /// left clean — the very next offer or copy works normally.
    pub fn on_transfer_timeout(&mut self, scope: TransferScope, generation: u64) -> Vec<Action> {
        match scope {
            TransferScope::Outbound => {
                if generation != self.outbound_generation {
                    return Vec::new(); // a newer transfer restarted the clock
                }
                let Some(outbound) = self.outbound.take() else {
                    return Vec::new();
                };
                if !outbound.retains_content() {
                    // Already past the point of holding the item; leave it
                    // to close on its Applied.
                    self.outbound = Some(outbound);
                    return Vec::new();
                }
                tracing::warn!(
                    clipboard_id = %outbound.meta().id,
                    byte_count = outbound.meta().content_length,
                    result = "abandoned",
                    "outbound clipboard transfer abandoned: no answer within the deadline"
                );
                Vec::new()
            }
            TransferScope::Inbound => {
                if generation != self.inbound_generation {
                    return Vec::new();
                }
                let mut actions = Vec::new();
                if let Some(meta) = self.expecting_data.take() {
                    tracing::warn!(
                        clipboard_id = %meta.id,
                        result = "abandoned",
                        "accepted inbound offer abandoned: content never arrived"
                    );
                    actions.push(Action::Send(OutboundMessage::Applied(ClipboardApplied {
                        id: meta.id,
                        // Nothing was installed and nothing will be. The
                        // origin needs *an* answer far more than it needs a
                        // bespoke variant, and a new `ApplyResult` would be
                        // a wire change fatal to peers that predate it
                        // (docs/PROTOCOL.md §2).
                        result: ApplyResult::ContentRejected,
                    })));
                }
                if let Some(reassembly) = self.abandon_reassembly("deadline") {
                    actions.push(Action::Send(OutboundMessage::Applied(ClipboardApplied {
                        id: reassembly,
                        result: ApplyResult::ContentRejected,
                    })));
                }
                actions
            }
        }
    }

    /// One chunk has been handed to the send path; emit the next.
    ///
    /// The stream is driven one chunk at a time, on purpose: a chunk is
    /// ADR 0013's preemption unit, so each one becomes its own command and
    /// its own frame, and the sender never materializes the whole split
    /// (a 64 MiB image would otherwise be 128 MiB in flight).
    pub fn on_chunk_sent(&mut self, id: Uuid) -> Vec<Action> {
        let Some(Outbound::Streaming {
            meta,
            content,
            plan,
            next_index,
            started,
        }) = self.outbound.take()
        else {
            return Vec::new(); // superseded, abandoned, or not streaming
        };
        if meta.id != id {
            // A late confirmation for a transfer that has been replaced.
            self.outbound = Some(Outbound::Streaming {
                meta,
                content,
                plan,
                next_index,
                started,
            });
            return Vec::new();
        }
        if next_index >= plan.chunk_count() {
            // Every chunk is out; the content buffer is released here and
            // only the verdict remains outstanding.
            self.outbound = Some(Outbound::AwaitingApplied { meta, started });
            return Vec::new();
        }
        let Some(chunk) = chunk_at(meta.id, &content, plan, next_index) else {
            // Unreachable: the plan was derived from this buffer's length.
            tracing::error!(
                clipboard_id = %meta.id,
                chunk_index = next_index,
                "clipboard chunk slice out of range; abandoning the transfer"
            );
            return Vec::new();
        };
        self.outbound = Some(Outbound::Streaming {
            meta,
            content,
            plan,
            next_index: next_index.saturating_add(1),
            started,
        });
        vec![Action::Send(OutboundMessage::Chunk(chunk))]
    }

    // --- internals ---

    fn start_outbound(&mut self, meta: ClipboardMeta, content: Vec<u8>) -> Vec<Action> {
        if let Some(previous) = self.outbound.take() {
            tracing::debug!(
                clipboard_id = %previous.meta().id,
                "outbound clipboard transaction superseded by newer local copy"
            );
        }
        self.record(Metrics::record_clipboard_sent);
        let started = Instant::now();
        // Chunked types have no inline flow and are offered at any size:
        // the offer round is where the receiver's already-have decline
        // makes a re-paste move zero bytes, and where it bounds its own
        // memory before megabytes arrive (ADR 0014).
        let offered = meta.content_type.is_chunked()
            || meta.content_length > CLIPBOARD_INLINE_MAX_BYTES as u64;
        if !offered {
            self.outbound = Some(Outbound::AwaitingApplied { meta, started });
            return vec![Action::Send(OutboundMessage::Data(ClipboardData {
                meta,
                content,
            }))];
        }
        self.outbound = Some(Outbound::AwaitingAccept {
            meta,
            content,
            started,
        });
        vec![
            Action::Send(OutboundMessage::Offer(ClipboardOffer { meta })),
            self.arm_timeout(TransferScope::Outbound),
        ]
    }

    /// Start (or restart) a scope's deadline, returning the action that
    /// asks the driver for the timer.
    fn arm_timeout(&mut self, scope: TransferScope) -> Action {
        let generation = match scope {
            TransferScope::Outbound => {
                self.outbound_generation = self.outbound_generation.wrapping_add(1);
                self.outbound_generation
            }
            TransferScope::Inbound => {
                self.inbound_generation = self.inbound_generation.wrapping_add(1);
                self.inbound_generation
            }
        };
        Action::ScheduleTransferTimeout {
            scope,
            generation,
            delay: self.config.transfer_timeout,
        }
    }

    /// Drop any in-flight reassembly, remembering its id so the chunks
    /// still on the wire for it are recognized rather than punished.
    /// Returns the abandoned item id.
    fn abandon_reassembly(&mut self, why: &str) -> Option<Uuid> {
        let reassembly = self.reassembly.take()?;
        let id = reassembly.meta().id;
        tracing::debug!(
            clipboard_id = %id,
            byte_count = reassembly.received_bytes(),
            reason = why,
            "inbound chunked transfer abandoned"
        );
        self.remember_transfer(id);
        Some(id)
    }

    fn remember_transfer(&mut self, id: Uuid) {
        if self.recent_transfers.len() >= RECENT_TRANSFER_MEMORY {
            self.recent_transfers.pop_front();
        }
        self.recent_transfers.push_back(id);
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
            // payload bytes moved (ADR 0005) — and for a chunked item that
            // is the whole point of offering it, since a re-pasted snip
            // then costs one offer and one decline instead of megabytes.
            return vec![Action::Send(OutboundMessage::Decline(ClipboardDecline {
                id: offer.meta.id,
                reason: DeclineReason::AlreadyHave,
            }))];
        }

        // Accepting supersedes whatever inbound transfer was in flight:
        // the peer holds at most one outbound transaction of its own, so a
        // second offer means it already abandoned the first, and there is
        // no answer owed for a transaction its origin has dropped.
        if let Some(previous) = self.expecting_data.take() {
            tracing::debug!(
                clipboard_id = %previous.id,
                "accepted inbound offer superseded by a newer one"
            );
        }
        self.abandon_reassembly("superseded by a newer offer");

        if offer.meta.content_type.is_chunked() {
            // The receiver's memory commitment is decided here and nowhere
            // else: `begin` validates the offered length against the
            // type's maximum *before* sizing the buffer from it (NFR-1),
            // and reports an allocation it cannot make rather than dying.
            match ChunkReassembly::begin(offer.meta) {
                Ok(reassembly) => {
                    self.reassembly = Some(reassembly);
                    return vec![
                        Action::Send(OutboundMessage::Accept(ClipboardAccept {
                            id: offer.meta.id,
                        })),
                        self.arm_timeout(TransferScope::Inbound),
                    ];
                }
                Err(error) => {
                    // Declined, not dropped: a typed answer closes the
                    // origin's transaction (NFR-3). `NotReady` because a
                    // memory refusal is about *now*, unlike a length the
                    // protocol will never admit.
                    tracing::warn!(
                        clipboard_id = %offer.meta.id,
                        byte_count = offer.meta.content_length,
                        error = %error,
                        "declining a chunked offer this side cannot buffer"
                    );
                    return vec![Action::Send(OutboundMessage::Decline(ClipboardDecline {
                        id: offer.meta.id,
                        reason: DeclineReason::NotReady,
                    }))];
                }
            }
        }

        self.expecting_data = Some(offer.meta);
        vec![
            Action::Send(OutboundMessage::Accept(ClipboardAccept {
                id: offer.meta.id,
            })),
            self.arm_timeout(TransferScope::Inbound),
        ]
    }

    fn on_peer_accept(&mut self, id: Uuid) -> Vec<Action> {
        match self.outbound.take() {
            Some(Outbound::AwaitingAccept {
                meta,
                content,
                started,
            }) if meta.id == id => {
                if !meta.content_type.is_chunked() {
                    self.outbound = Some(Outbound::AwaitingApplied { meta, started });
                    return vec![Action::Send(OutboundMessage::Data(ClipboardData {
                        meta,
                        content,
                    }))];
                }
                // The split is the same arithmetic the receiver derives
                // from the offered length and chunk 0 — one implementation,
                // both sides (ADR 0014).
                let Ok(plan) = ChunkPlan::for_length(meta.content_length) else {
                    tracing::error!(
                        clipboard_id = %meta.id,
                        byte_count = meta.content_length,
                        "clipboard item cannot be split into chunks; abandoning"
                    );
                    return Vec::new();
                };
                let Some(first) = chunk_at(meta.id, &content, plan, 0) else {
                    tracing::error!(clipboard_id = %meta.id, "empty clipboard chunk plan; abandoning");
                    return Vec::new();
                };
                tracing::debug!(
                    clipboard_id = %meta.id,
                    byte_count = meta.content_length,
                    chunk_count = plan.chunk_count(),
                    "streaming an accepted chunked clipboard item"
                );
                self.outbound = Some(Outbound::Streaming {
                    meta,
                    content,
                    plan,
                    next_index: 1,
                    started,
                });
                vec![Action::Send(OutboundMessage::Chunk(first))]
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
            Some(Outbound::AwaitingAccept { meta, started, .. }) if meta.id == decline.id => {
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
        // A whole item from the peer supersedes any chunked transfer it
        // was midway through — same rule, other direction.
        if self
            .reassembly
            .as_ref()
            .is_some_and(|r| r.meta().id != data.meta.id)
        {
            self.abandon_reassembly("superseded by a newer inbound item");
        }
        self.install_inbound(data.meta, data.content)
    }

    /// A chunk arrived (ADR 0014).
    ///
    /// Three outcomes, in order of how much the chunk is owed:
    ///
    /// 1. it belongs to the transfer being reassembled — routed there,
    ///    and the reassembly is the only thing that decides whether it is
    ///    admissible;
    /// 2. it belongs to a transfer this side recently finished or
    ///    abandoned — the benign tail of an in-flight stream, ignored
    ///    without penalty (see [`RECENT_TRANSFER_MEMORY`]);
    /// 3. anything else has no accepted offer behind it, which is a
    ///    protocol violation (docs/PROTOCOL.md §5) and takes §7's handling
    ///    exactly: rejected, counted, logged at **debug** — the level
    ///    matters, because the log volume is otherwise the peer's to
    ///    choose, and a saturated 2.5 `GbE` link is thousands of chunks per
    ///    second into an uncapped rolling file — and fatal once the peer
    ///    makes a habit of it.
    fn on_peer_chunk(&mut self, chunk: &ClipboardChunk) -> Vec<Action> {
        if self
            .reassembly
            .as_ref()
            .is_some_and(|r| r.meta().id == chunk.id)
        {
            return self.accept_chunk(chunk);
        }
        if self.recent_transfers.contains(&chunk.id) {
            tracing::debug!(
                clipboard_id = %chunk.id,
                chunk_index = chunk.index,
                "chunk for a finished or abandoned transfer; ignoring"
            );
            return Vec::new();
        }
        tracing::debug!(
            clipboard_id = %chunk.id,
            chunk_index = chunk.index,
            byte_count = chunk.payload.len(),
            "clipboard chunk with no accepted offer; rejecting"
        );
        self.record_violation("clipboard chunk with no accepted offer")
    }

    /// Route a chunk into the live reassembly.
    ///
    /// A rejected chunk ends the transfer: the sequence is strictly
    /// ordered and derived, so a chunk the plan cannot admit means the
    /// stream is no longer the item that was offered, and continuing to
    /// buffer it would be assembling something else. One violation per
    /// *transfer*, not per chunk — the rest of a doomed stream is charged
    /// nothing, which keeps a single bad transfer from spending the whole
    /// session budget in one burst.
    fn accept_chunk(&mut self, chunk: &ClipboardChunk) -> Vec<Action> {
        let Some(reassembly) = self.reassembly.as_mut() else {
            return Vec::new();
        };
        let meta = reassembly.meta();
        match reassembly.accept(chunk) {
            Ok(ChunkOutcome::More) => Vec::new(),
            Ok(ChunkOutcome::Complete(bytes)) => {
                // The reassembly verified the item's hash over these bytes
                // before handing them out: this is the offered item, whole,
                // and nothing partially-verified can reach here.
                self.reassembly = None;
                self.remember_transfer(meta.id);
                tracing::debug!(
                    clipboard_id = %meta.id,
                    byte_count = meta.content_length,
                    "chunked clipboard item reassembled and verified"
                );
                self.install_inbound(meta, bytes)
            }
            Err(error) => {
                tracing::debug!(
                    clipboard_id = %chunk.id,
                    chunk_index = chunk.index,
                    error = %error,
                    "malformed clipboard chunk; abandoning the transfer"
                );
                self.abandon_reassembly("malformed chunk");
                let mut actions = vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                    id: meta.id,
                    result: ApplyResult::ContentRejected,
                }))];
                actions.extend(self.record_violation("malformed clipboard chunk"));
                actions
            }
        }
    }

    /// The shared tail of every inbound item, whole or reassembled: the
    /// conflict rule, the loop guard, then an acknowledged install
    /// (FR-3.2 — `Applied` is sent only by [`Self::on_write_result`],
    /// after the destination clipboard actually took the content).
    fn install_inbound(&mut self, meta: ClipboardMeta, bytes: Vec<u8>) -> Vec<Action> {
        if let Some(reason) = self.conflict_verdict(meta) {
            debug_assert_eq!(reason, DeclineReason::Superseded);
            self.record(Metrics::record_clipboard_superseded);
            return vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: meta.id,
                result: ApplyResult::Superseded,
            }))];
        }

        // Loop/echo guard: identical content is a success without a write.
        if self.current_local_hash == Some(meta.content_hash) {
            return vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: meta.id,
                result: ApplyResult::Applied,
            }))];
        }

        // Wire validation guarantees UTF-8 for Utf8Text; defensive here.
        let Some(content) = from_wire(meta.content_type, bytes) else {
            return vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: meta.id,
                result: ApplyResult::ContentRejected,
            }))];
        };

        if let Some(superseded) = self.pending_write.take() {
            tracing::debug!(
                clipboard_id = %superseded.meta.id,
                "pending write superseded by newer inbound item"
            );
        }
        let content = Arc::new(content);
        self.pending_write = Some(PendingWrite {
            meta,
            content: Arc::clone(&content),
            attempts_made: 1,
        });
        vec![Action::WriteClipboard {
            id: meta.id,
            content,
        }]
    }

    /// Count one clipboard protocol violation, terminating the session
    /// once the peer passes [`MAX_CLIPBOARD_VIOLATIONS`]
    /// (docs/PROTOCOL.md §7).
    fn record_violation(&mut self, what: &str) -> Vec<Action> {
        self.violations = self.violations.saturating_add(1);
        if self.violations < MAX_CLIPBOARD_VIOLATIONS {
            return Vec::new();
        }
        tracing::warn!(
            violation_count = self.violations,
            violation = what,
            "terminating the session: repeated clipboard protocol violations"
        );
        vec![Action::TerminateSession {
            reason: format!(
                "{self_violations} clipboard protocol violations ({what})",
                self_violations = self.violations
            ),
        }]
    }

    /// The destination's verdict closes our transaction.
    ///
    /// A verdict is accepted while we are still *streaming* as well, and
    /// deliberately: a receiver that rejects a chunk answers immediately,
    /// and a sender that kept pushing chunks at it would be spending the
    /// wire on an item nobody is assembling any more.
    fn on_peer_applied(&mut self, applied: &ClipboardApplied) -> Vec<Action> {
        match self.outbound.take() {
            Some(
                Outbound::AwaitingApplied { meta, started }
                | Outbound::Streaming { meta, started, .. },
            ) if meta.id == applied.id => {
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

/// The chunk at `index`, sliced straight out of the retained item buffer.
///
/// `None` when the index is past the transfer or the buffer does not
/// reach — both unreachable for a plan derived from this buffer's own
/// length, and both a returned value rather than a panic (NFR-1).
fn chunk_at(id: Uuid, content: &[u8], plan: ChunkPlan, index: u32) -> Option<ClipboardChunk> {
    let len = usize::try_from(plan.chunk_len(index)?).ok()?;
    let start = usize::try_from(u64::from(index) * u64::from(plan.chunk_bytes())).ok()?;
    let end = start.checked_add(len)?;
    let payload = content.get(start..end)?.to_vec();
    Some(ClipboardChunk { id, index, payload })
}

/// Platform image tag → protocol image tag.
///
/// Wildcard-free on purpose. The two enums are deliberate mirrors — the
/// platform crate carries no dependencies (docs/ARCHITECTURE.md §4) — so
/// this match is the single place they are reconciled, and a new format
/// added to either one fails the build here instead of silently losing
/// its tag somewhere on the way to the wire.
const fn wire_format(format: ClipboardImageFormat) -> ImageFormat {
    match format {
        ClipboardImageFormat::Dib => ImageFormat::Dib,
        ClipboardImageFormat::Png => ImageFormat::Png,
        ClipboardImageFormat::Jpeg => ImageFormat::Jpeg,
    }
}

/// Protocol image tag → platform image tag. See [`wire_format`].
const fn platform_format(format: ImageFormat) -> ClipboardImageFormat {
    match format {
        ImageFormat::Dib => ClipboardImageFormat::Dib,
        ImageFormat::Png => ClipboardImageFormat::Png,
        ImageFormat::Jpeg => ClipboardImageFormat::Jpeg,
    }
}

/// Typed platform content → the wire's `(type, bytes)` pair.
///
/// Image bytes move by value and untouched: no transcode, no compression,
/// no inspection — the hash and the length are all that is ever computed
/// over them (ADR 0014).
fn into_wire(content: ClipboardContent) -> (ContentType, Vec<u8>) {
    match content {
        ClipboardContent::Text(text) => (ContentType::Utf8Text, text.into_bytes()),
        ClipboardContent::Image { format, bytes } => {
            (ContentType::Image(wire_format(format)), bytes)
        }
    }
}

/// Verified wire bytes → typed platform content.
///
/// `None` only for text bytes that are not UTF-8, which the decoder
/// already makes unrepresentable — kept as a value-returning check rather
/// than an assumption, because it is the last gate before content reaches
/// the OS.
fn from_wire(content_type: ContentType, bytes: Vec<u8>) -> Option<ClipboardContent> {
    Some(match content_type {
        ContentType::Utf8Text => ClipboardContent::Text(String::from_utf8(bytes).ok()?),
        ContentType::Image(format) => ClipboardContent::Image {
            format: platform_format(format),
            bytes,
        },
    })
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

    use crossover_platform::{ClipboardContent, ClipboardImageFormat};
    use crossover_protocol::clipboard::{
        ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ClipboardAccept, ClipboardApplied, ClipboardChunk,
        ClipboardData, ClipboardMeta, ClipboardOffer, ContentType, DeclineReason, ImageFormat,
        MAX_CHUNK_BYTES, chunk_content, content_hash,
    };

    use std::time::Duration;

    use super::{
        Action, ClipboardConfig, ClipboardEngine, InboundMessage, OutboundMessage, RetryPolicy,
        TransferScope,
    };

    fn engine(origin_fill: u8) -> ClipboardEngine {
        ClipboardEngine::new(Uuid::from_bytes([origin_fill; 16]), ClipboardConfig::new())
    }

    /// Image bytes that no text path could survive: non-UTF-8 lead bytes,
    /// embedded NULs, and a run of 0xFF. Everything about a chunked
    /// transfer must carry them verbatim (ADR 0014).
    fn image_bytes(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| match i % 4 {
                0 => 0xFF,
                1 => 0x00,
                2 => 0xFE,
                _ => u8::try_from(i % 251).unwrap_or(0),
            })
            .collect()
    }

    fn snip(bytes: Vec<u8>) -> ClipboardContent {
        ClipboardContent::Image {
            format: ClipboardImageFormat::Dib,
            bytes,
        }
    }

    /// Copy an image locally and return the actions.
    fn copy_image(engine: &mut ClipboardEngine, bytes: Vec<u8>) -> Vec<Action> {
        engine.on_local_change();
        engine.on_settle_due();
        engine.on_local_read(Some(snip(bytes)))
    }

    fn offer_of(actions: &[Action]) -> ClipboardOffer {
        match sent(actions).as_slice() {
            [OutboundMessage::Offer(offer)] => *offer,
            other => panic!("expected exactly one offer, got {other:?}"),
        }
    }

    fn chunk_of(actions: &[Action]) -> ClipboardChunk {
        match sent(actions).as_slice() {
            [OutboundMessage::Chunk(chunk)] => (*chunk).clone(),
            other => panic!("expected exactly one chunk, got {other:?}"),
        }
    }

    /// Drive an accepted outbound transfer to completion, collecting every
    /// chunk the engine emits — the driver's loop, in miniature.
    fn drain_chunks(engine: &mut ClipboardEngine, first: ClipboardChunk) -> Vec<ClipboardChunk> {
        let id = first.id;
        let mut chunks = vec![first];
        loop {
            let actions = engine.on_chunk_sent(id);
            if actions.is_empty() {
                return chunks;
            }
            chunks.push(chunk_of(&actions));
            assert!(
                chunks.len() <= 2048,
                "the chunk stream never terminated ({} chunks)",
                chunks.len()
            );
        }
    }

    /// An inbound image transfer, from the peer's offer to the last chunk.
    /// Returns the actions produced by each step, flattened.
    fn inbound_image(
        engine: &mut ClipboardEngine,
        origin: u8,
        sequence: u64,
        bytes: &[u8],
    ) -> (ClipboardMeta, Vec<Action>) {
        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([origin; 16]),
            sequence,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: bytes.len() as u64,
            content_hash: content_hash(bytes),
        };
        let mut actions = engine.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        for chunk in chunk_content(meta.id, bytes).unwrap() {
            actions.extend(engine.on_peer_message(InboundMessage::Chunk(chunk)));
        }
        (meta, actions)
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
        engine.on_local_read(Some(ClipboardContent::Text(text.to_owned())))
    }

    /// The text the engine asked to be written, whatever the action shape.
    fn written_text(actions: &[Action]) -> Option<String> {
        actions.iter().find_map(|a| match a {
            Action::WriteClipboard { content, .. } => {
                content.as_text().map(std::borrow::ToOwned::to_owned)
            }
            _ => None,
        })
    }

    /// The content the engine asked to be written.
    fn written(actions: &[Action]) -> Option<ClipboardContent> {
        actions.iter().find_map(|a| match a {
            Action::WriteClipboard { content, .. } => Some((**content).clone()),
            _ => None,
        })
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
        assert!(
            e.on_local_read(Some(ClipboardContent::Text(huge)))
                .is_empty()
        );
        // Per-type bounds since ADR 0014: an image past its own (much
        // larger) ceiling is refused by the same rule, not by the text one.
        let huge_image = vec![0u8; 64 * 1024 * 1024 + 1];
        assert!(e.on_local_read(Some(snip(huge_image))).is_empty());
        // And an empty image is not an image.
        assert!(e.on_local_read(Some(snip(Vec::new()))).is_empty());
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
        let actions = receiver.on_local_read(Some(ClipboardContent::Text("from peer".to_owned())));
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
                            OutboundMessage::Chunk(x) => InboundMessage::Chunk(x),
                            OutboundMessage::Applied(x) => InboundMessage::Applied(x),
                        }),
                        Action::WriteClipboard { id, content } => {
                            let text = content.as_text().unwrap_or_default().to_owned();
                            self.pending_write = Some((id, text));
                        }
                        Action::ScheduleSettle { .. } => {
                            let read = self.engine.on_settle_due();
                            self.drive(read, outbox);
                        }
                        Action::ReadClipboard
                        | Action::ScheduleRetry { .. }
                        | Action::ScheduleTransferTimeout { .. } => {}
                        Action::TerminateSession { reason } => {
                            panic!("conforming engines must not terminate: {reason}")
                        }
                    }
                }
                // Complete writes instantly (fake clipboard, no
                // contention) and run the own-write notification cycle.
                if let Some((id, text)) = self.pending_write.take() {
                    self.clipboard.clone_from(&text);
                    let more = self.engine.on_write_result(id, Ok(()));
                    self.drive(more, outbox);
                    let mut cycle = self.engine.on_local_change();
                    cycle.extend(
                        self.engine
                            .on_local_read(Some(ClipboardContent::Text(text))),
                    );
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
            sent(&e.on_local_read(Some(ClipboardContent::Text("persistent".to_owned())))).len(),
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
        let actions = e.on_local_read(Some(ClipboardContent::Text("settled content".to_owned())));
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
        assert_eq!(
            sent(&e.on_local_read(Some(ClipboardContent::Text("eager".to_owned())))).len(),
            1
        );
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
        e.on_local_read(Some(ClipboardContent::Text("from peer".to_owned())));
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

    /// A chunk with no accepted offer behind it is a protocol violation
    /// (docs/PROTOCOL.md §5), so it gets §7's handling: rejected and
    /// counted, survivable once, fatal when the peer makes a habit of it.
    /// Without the cap, unanswered junk is free for the sender.
    #[test]
    fn unsolicited_chunks_are_rejected_and_terminate_the_session_when_repeated() {
        use crossover_protocol::clipboard::ClipboardChunk;

        use super::MAX_CLIPBOARD_VIOLATIONS;

        let mut e = engine(0xBB);
        let chunk = |i: u32| {
            InboundMessage::Chunk(ClipboardChunk {
                id: Uuid::new_v4(),
                index: i,
                payload: vec![0xAB; 32],
            })
        };

        // Every violation below the budget is absorbed silently: nothing
        // applied, nothing acknowledged, the session lives.
        for i in 0..MAX_CLIPBOARD_VIOLATIONS - 1 {
            let actions = e.on_peer_message(chunk(i));
            assert!(actions.is_empty(), "violation {i} acted on: {actions:?}");
        }

        // The one that reaches the budget ends the session.
        match e
            .on_peer_message(chunk(MAX_CLIPBOARD_VIOLATIONS - 1))
            .as_slice()
        {
            [Action::TerminateSession { reason }] => {
                assert!(
                    reason.contains("violation"),
                    "the diagnostic must name what the peer did: {reason}"
                );
            }
            other => panic!("repeated violations must terminate, got {other:?}"),
        }

        // A new session starts the peer on a clean budget.
        e.on_session_established();
        assert!(e.on_peer_message(chunk(0)).is_empty());
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

    // --- chunked image transfer (ADR 0014) ---------------------------------

    /// The whole outbound transaction: a local snip is offered (never
    /// inline, whatever its size), accepted, streamed chunk by chunk, and
    /// closed by the destination's verdict. The bytes that come out of the
    /// stream must be the bytes that went in, verbatim.
    #[test]
    fn a_local_image_is_offered_streamed_and_closed_by_the_verdict() {
        let mut e = engine(0xAA);
        let bytes = image_bytes(MAX_CHUNK_BYTES * 2 + 7);

        let actions = copy_image(&mut e, bytes.clone());
        let offer = offer_of(&actions);
        assert_eq!(
            offer.meta.content_type,
            ContentType::Image(ImageFormat::Dib)
        );
        assert_eq!(offer.meta.content_length, bytes.len() as u64);
        assert_eq!(offer.meta.content_hash, content_hash(&bytes));
        // A retained transfer arms a deadline; nothing else does.
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::ScheduleTransferTimeout {
                    scope: TransferScope::Outbound,
                    ..
                }
            )),
            "an offer that retains its content must be bounded in time"
        );

        let accepted = e.on_peer_message(InboundMessage::Accept(ClipboardAccept {
            id: offer.meta.id,
        }));
        let chunks = drain_chunks(&mut e, chunk_of(&accepted));

        // Exactly the split the shared arithmetic produces — the sender
        // slices out of its retained buffer rather than pre-rendering, so
        // this equality is what ties the two paths together.
        assert_eq!(chunks, chunk_content(offer.meta.id, &bytes).unwrap());
        let streamed: Vec<u8> = chunks.iter().flat_map(|c| c.payload.clone()).collect();
        assert_eq!(streamed, bytes, "image bytes were not transferred verbatim");
        assert_eq!(chunks.len(), 3);

        // The verdict closes it; nothing further is emitted.
        let closed = e.on_peer_message(InboundMessage::Applied(ClipboardApplied {
            id: offer.meta.id,
            result: ApplyResult::Applied,
        }));
        assert!(closed.is_empty());
        assert!(e.on_chunk_sent(offer.meta.id).is_empty());
    }

    /// A tiny image is *still* offered: the inline threshold is a text
    /// rule (ADR 0014), and the offer round is what makes a re-paste free.
    #[test]
    fn even_a_tiny_image_is_offered_rather_than_sent_inline() {
        let mut e = engine(0xAA);
        let actions = copy_image(&mut e, image_bytes(64));
        let offer = offer_of(&actions);
        assert_eq!(offer.meta.content_length, 64);

        let accepted = e.on_peer_message(InboundMessage::Accept(ClipboardAccept {
            id: offer.meta.id,
        }));
        let chunks = drain_chunks(&mut e, chunk_of(&accepted));
        assert_eq!(chunks.len(), 1, "one chunk under the chunk size");
        assert_eq!(chunks[0].index, 0);
    }

    /// Re-pasting a snip the peer already holds moves **zero** content
    /// bytes: the offer is declined as already-have and the transaction is
    /// over. This is the payoff the offer round exists for (ADR 0014).
    #[test]
    fn an_already_held_image_is_declined_before_any_bytes_travel() {
        let mut e = engine(0xBB);
        let bytes = image_bytes(MAX_CHUNK_BYTES * 4);
        // This side holds the snip already (it copied it locally).
        copy_image(&mut e, bytes.clone());
        // Its own transfer closes, so nothing is in flight to conflict.
        let mine = e.current_local_hash;
        assert_eq!(mine, Some(content_hash(&bytes)));
        e.on_session_lost();

        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xAA; 16]),
            sequence: 99,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: bytes.len() as u64,
            content_hash: content_hash(&bytes),
        };
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        match sent(&actions).as_slice() {
            [OutboundMessage::Decline(decline)] => {
                assert_eq!(decline.id, meta.id);
                assert_eq!(decline.reason, DeclineReason::AlreadyHave);
            }
            other => panic!("expected an AlreadyHave decline, got {other:?}"),
        }
        // No reassembly was begun, so no buffer was committed either.
        assert!(e.reassembly.is_none());
    }

    /// The whole inbound transaction: accept, reassemble, verify, install,
    /// acknowledge. `Applied` is sent only after the destination clipboard
    /// took the content (FR-3.2), never on receipt of the last chunk.
    #[test]
    fn an_inbound_image_is_reassembled_installed_then_acknowledged() {
        let mut e = engine(0xBB);
        let bytes = image_bytes(MAX_CHUNK_BYTES * 3 + 11);
        let (meta, actions) = inbound_image(&mut e, 0xAA, 1, &bytes);

        // Accept first, deadline armed, and no verdict yet.
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Accept(_)]
        ));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::ScheduleTransferTimeout {
                scope: TransferScope::Inbound,
                ..
            }
        )));

        // The completed transfer asks for an install of the exact bytes.
        assert_eq!(written(&actions), Some(snip(bytes.clone())));

        // Only the successful write produces the verdict.
        let closed = e.on_write_result(meta.id, Ok(()));
        match sent(&closed).as_slice() {
            [OutboundMessage::Applied(applied)] => {
                assert_eq!(applied.id, meta.id);
                assert_eq!(applied.result, ApplyResult::Applied);
            }
            other => panic!("expected an Applied verdict, got {other:?}"),
        }
        assert!(e.reassembly.is_none(), "the buffer must be released");
    }

    /// A destination that cannot install the content — which is exactly
    /// this build's Windows backend for an image — says so, and says it
    /// after the bytes arrived rather than pretending success (FR-3.2).
    #[test]
    fn an_image_the_platform_cannot_install_reports_the_failure() {
        let mut e = engine(0xBB);
        let bytes = image_bytes(4096);
        let (meta, _) = inbound_image(&mut e, 0xAA, 1, &bytes);

        let closed = e.on_write_result(meta.id, Err(false));
        match sent(&closed).as_slice() {
            [OutboundMessage::Applied(applied)] => {
                assert_eq!(applied.result, ApplyResult::ClipboardUnavailable);
            }
            other => panic!("expected a typed failure verdict, got {other:?}"),
        }
    }

    /// A newer local copy supersedes a chunk stream in flight: the old
    /// item's buffer goes with it, and the new item travels normally.
    #[test]
    fn a_newer_local_copy_supersedes_a_stream_in_flight() {
        let mut e = engine(0xAA);
        let first = image_bytes(MAX_CHUNK_BYTES * 3);
        let actions = copy_image(&mut e, first);
        let offer = offer_of(&actions);
        let accepted = e.on_peer_message(InboundMessage::Accept(ClipboardAccept {
            id: offer.meta.id,
        }));
        let chunk0 = chunk_of(&accepted);
        assert_eq!(chunk0.index, 0);

        // A new copy lands mid-stream.
        let actions = copy(&mut e, "text beats a half-sent image");
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Data(_)]
        ));
        // The abandoned stream produces nothing more, ever.
        assert!(e.on_chunk_sent(offer.meta.id).is_empty());
        assert!(
            e.on_peer_message(InboundMessage::Accept(ClipboardAccept {
                id: offer.meta.id
            }))
            .is_empty()
        );
    }

    /// A newer inbound offer supersedes a reassembly in flight, and the
    /// tail of the abandoned stream is recognized as the benign race it is
    /// rather than charged to the violation budget — which matters at
    /// image scale, where a lane's worth of chunks can already be in
    /// flight.
    #[test]
    fn a_newer_inbound_offer_supersedes_a_reassembly_without_punishing_its_tail() {
        let mut e = engine(0xBB);
        let first_bytes = image_bytes(MAX_CHUNK_BYTES * 4);
        let first = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xAA; 16]),
            sequence: 1,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: first_bytes.len() as u64,
            content_hash: content_hash(&first_bytes),
        };
        let first_chunks = chunk_content(first.id, &first_bytes).unwrap();
        e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta: first }));
        e.on_peer_message(InboundMessage::Chunk(first_chunks[0].clone()));

        // The peer changes its mind and offers something newer.
        let second_bytes = image_bytes(MAX_CHUNK_BYTES + 3);
        let (second, actions) = inbound_image(&mut e, 0xAA, 2, &second_bytes);
        assert_eq!(written(&actions), Some(snip(second_bytes)));
        e.on_write_result(second.id, Ok(())).len();

        // The first transfer's remaining chunks arrive late. Ignored, not
        // fatal: the session must survive its own supersession.
        for chunk in &first_chunks[1..] {
            let actions = e.on_peer_message(InboundMessage::Chunk(chunk.clone()));
            assert!(
                actions.is_empty(),
                "the tail of a superseded transfer must be absorbed: {actions:?}"
            );
        }
        assert_eq!(e.violations, 0, "a benign race spent the violation budget");
    }

    /// Session loss releases every buffer the machine can hold — in both
    /// directions — and leaves it able to do the whole thing again.
    #[test]
    fn session_loss_mid_transfer_clears_state_and_a_fresh_transfer_works() {
        let mut e = engine(0xBB);

        // Inbound: an accepted offer, half streamed.
        let bytes = image_bytes(MAX_CHUNK_BYTES * 3);
        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xAA; 16]),
            sequence: 1,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: bytes.len() as u64,
            content_hash: content_hash(&bytes),
        };
        let chunks = chunk_content(meta.id, &bytes).unwrap();
        e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        e.on_peer_message(InboundMessage::Chunk(chunks[0].clone()));
        assert!(e.reassembly.is_some());

        // Outbound: our own image, offered.
        let mine = image_bytes(MAX_CHUNK_BYTES * 2);
        let offer = offer_of(&copy_image(&mut e, mine));

        assert!(e.on_session_lost().is_empty());
        assert!(e.reassembly.is_none(), "a reassembly buffer survived");
        assert!(e.outbound.is_none(), "a retained item survived");
        assert!(e.expecting_data.is_none());
        // The abandoned outbound transfer is inert.
        assert!(
            e.on_peer_message(InboundMessage::Accept(ClipboardAccept {
                id: offer.meta.id
            }))
            .is_empty()
        );

        // A fresh session, and the whole thing works again.
        e.on_session_established();
        let fresh = image_bytes(MAX_CHUNK_BYTES + 5);
        let (fresh_meta, actions) = inbound_image(&mut e, 0xAA, 9, &fresh);
        assert_eq!(written(&actions), Some(snip(fresh)));
        assert!(!sent(&e.on_write_result(fresh_meta.id, Ok(()))).is_empty());
    }

    /// The lifetime bound (ADR 0014): a transfer that stalls is abandoned,
    /// observably and non-fatally, with the origin told so its own
    /// transaction closes (NFR-3) — and the machine still works after.
    #[test]
    fn a_stalled_transfer_is_abandoned_and_a_fresh_one_still_works() {
        let mut e = engine(0xBB);

        // Inbound image: accepted, then the peer goes quiet.
        let bytes = image_bytes(MAX_CHUNK_BYTES * 8);
        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xAA; 16]),
            sequence: 1,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: bytes.len() as u64,
            content_hash: content_hash(&bytes),
        };
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        let Some(Action::ScheduleTransferTimeout { generation, .. }) = actions.iter().find(|a| {
            matches!(
                a,
                Action::ScheduleTransferTimeout {
                    scope: TransferScope::Inbound,
                    ..
                }
            )
        }) else {
            panic!("an accepted offer must be bounded in time: {actions:?}");
        };
        let generation = *generation;
        e.on_peer_message(InboundMessage::Chunk(
            chunk_content(meta.id, &bytes).unwrap()[0].clone(),
        ));

        // A stale deadline is a no-op...
        assert!(
            e.on_transfer_timeout(TransferScope::Inbound, generation - 1)
                .is_empty()
        );
        assert!(e.reassembly.is_some());
        // ...and the live one abandons the transfer and answers the origin.
        let abandoned = e.on_transfer_timeout(TransferScope::Inbound, generation);
        match sent(&abandoned).as_slice() {
            [OutboundMessage::Applied(applied)] => {
                assert_eq!(applied.id, meta.id);
                assert_eq!(applied.result, ApplyResult::ContentRejected);
            }
            other => panic!("an abandoned transfer must answer its origin, got {other:?}"),
        }
        assert!(e.reassembly.is_none(), "the buffer was not released");

        // Stuck nowhere: the next offer is accepted and completes.
        let fresh = image_bytes(MAX_CHUNK_BYTES + 1);
        let (fresh_meta, actions) = inbound_image(&mut e, 0xAA, 2, &fresh);
        assert_eq!(written(&actions), Some(snip(fresh)));
        assert!(!sent(&e.on_write_result(fresh_meta.id, Ok(()))).is_empty());
    }

    /// The pre-existing gap ADR 0014 named: an accepted **text** offer
    /// whose `ClipboardData` never arrives had no timeout at all. It does
    /// now, on the same mechanism.
    #[test]
    fn an_accepted_text_offer_that_is_never_fulfilled_is_abandoned() {
        let mut e = engine(0xBB);
        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xAA; 16]),
            sequence: 1,
            content_type: ContentType::Utf8Text,
            content_length: (CLIPBOARD_INLINE_MAX_BYTES + 1) as u64,
            content_hash: content_hash(b"never sent"),
        };
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Accept(_)]
        ));
        assert!(e.expecting_data.is_some());

        let abandoned = e.on_transfer_timeout(TransferScope::Inbound, e.inbound_generation);
        match sent(&abandoned).as_slice() {
            [OutboundMessage::Applied(applied)] => {
                assert_eq!(applied.id, meta.id);
                assert_eq!(applied.result, ApplyResult::ContentRejected);
            }
            other => panic!("expected the origin to be told, got {other:?}"),
        }
        assert!(e.expecting_data.is_none());
    }

    /// An outbound offer nobody answers releases its item — the retained
    /// buffer is up to 64 MiB, and a session can live for days.
    #[test]
    fn an_unanswered_outbound_offer_releases_its_retained_item() {
        let mut e = engine(0xAA);
        let actions = copy_image(&mut e, image_bytes(MAX_CHUNK_BYTES * 2));
        let offer = offer_of(&actions);

        assert!(
            e.on_transfer_timeout(TransferScope::Outbound, e.outbound_generation - 1)
                .is_empty()
        );
        assert!(
            e.outbound.is_some(),
            "a stale deadline abandoned a transfer"
        );
        assert!(
            e.on_transfer_timeout(TransferScope::Outbound, e.outbound_generation)
                .is_empty()
        );
        assert!(e.outbound.is_none(), "the retained item was not released");
        // A late accept for the abandoned item does nothing at all.
        assert!(
            e.on_peer_message(InboundMessage::Accept(ClipboardAccept {
                id: offer.meta.id
            }))
            .is_empty()
        );
    }

    /// Every way a chunk can be wrong, each one fail-closed: the transfer
    /// ends, the origin is told, and the peer is charged **one** violation
    /// per doomed transfer rather than one per chunk.
    #[test]
    fn malformed_chunk_sequences_end_the_transfer_and_count_once() {
        use super::MAX_CLIPBOARD_VIOLATIONS;

        /// One way to break a chunk, applied to an otherwise valid one.
        type Corruption = fn(&mut ClipboardChunk);

        let bytes = image_bytes(MAX_CHUNK_BYTES * 3);
        let corruptions: [(&str, Corruption); 3] = [
            ("out of sequence", |c| c.index = 2),
            ("wrong length", |c| c.payload.truncate(16)),
            ("foreign item id", |c| c.id = Uuid::from_bytes([0xEE; 16])),
        ];

        for (label, corrupt) in corruptions {
            let mut e = engine(0xBB);
            let meta = ClipboardMeta {
                id: Uuid::new_v4(),
                origin: Uuid::from_bytes([0xAA; 16]),
                sequence: 1,
                content_type: ContentType::Image(ImageFormat::Dib),
                content_length: bytes.len() as u64,
                content_hash: content_hash(&bytes),
            };
            let chunks = chunk_content(meta.id, &bytes).unwrap();
            e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
            e.on_peer_message(InboundMessage::Chunk(chunks[0].clone()));

            let mut bad = chunks[1].clone();
            corrupt(&mut bad);
            let actions = e.on_peer_message(InboundMessage::Chunk(bad));
            // A foreign id is not this transfer's problem: it is an
            // unsolicited chunk, counted as one, and the live reassembly
            // is untouched.
            if label == "foreign item id" {
                assert!(actions.is_empty(), "{label}: {actions:?}");
                assert!(
                    e.reassembly.is_some(),
                    "{label}: a foreign chunk tore down a healthy transfer"
                );
                assert_eq!(e.violations, 1, "{label}");
                continue;
            }
            match sent(&actions).as_slice() {
                [OutboundMessage::Applied(applied)] => {
                    assert_eq!(applied.id, meta.id, "{label}");
                    assert_eq!(applied.result, ApplyResult::ContentRejected, "{label}");
                }
                other => panic!("{label}: expected the origin to be told, got {other:?}"),
            }
            assert!(e.reassembly.is_none(), "{label}: the buffer survived");
            assert_eq!(e.violations, 1, "{label}: violations are per transfer");

            // The rest of the doomed stream costs nothing more, so one bad
            // transfer cannot spend the whole session budget in a burst.
            for chunk in &chunks[2..] {
                assert!(
                    e.on_peer_message(InboundMessage::Chunk(chunk.clone()))
                        .is_empty(),
                    "{label}: the tail of an abandoned transfer was punished"
                );
            }
            assert_eq!(e.violations, 1, "{label}");
            assert!(e.violations < MAX_CLIPBOARD_VIOLATIONS);

            // And the peer can still transfer something correctly after.
            let fresh = image_bytes(64);
            let (fresh_meta, actions) = inbound_image(&mut e, 0xAA, 2, &fresh);
            assert_eq!(written(&actions), Some(snip(fresh)), "{label}");
            assert!(!sent(&e.on_write_result(fresh_meta.id, Ok(()))).is_empty());
        }
    }

    /// A conflict decided *before* the transfer starts: an image offer
    /// that loses the deterministic race is declined, so its megabytes
    /// never travel at all.
    #[test]
    fn an_inbound_image_offer_that_loses_the_conflict_race_is_declined() {
        let mut e = engine(0xFF); // high origin: ours wins ties
        copy(&mut e, "ours, in flight");

        let bytes = image_bytes(MAX_CHUNK_BYTES);
        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0x01; 16]),
            sequence: 0,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: bytes.len() as u64,
            content_hash: content_hash(&bytes),
        };
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        match sent(&actions).as_slice() {
            [OutboundMessage::Decline(decline)] => {
                assert_eq!(decline.reason, DeclineReason::Superseded);
            }
            other => panic!("expected a Superseded decline, got {other:?}"),
        }
        assert!(e.reassembly.is_none(), "a losing offer committed memory");
    }

    /// Text keeps every one of its rules while sharing the machine: a
    /// 4 MiB item is offered (not chunked), sent whole, and installed.
    #[test]
    fn the_text_offered_flow_is_unchanged_by_chunking() {
        let mut e = engine(0xBB);
        let big = "t".repeat(CLIPBOARD_INLINE_MAX_BYTES + 1);
        let data = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            0,
            ContentType::Utf8Text,
            big.clone().into_bytes(),
        );
        let meta = data.meta;
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer { meta }));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Accept(_)]
        ));
        assert!(e.reassembly.is_none(), "text must never build a reassembly");

        let actions = e.on_peer_message(InboundMessage::Data(data));
        assert_eq!(written_text(&actions).as_deref(), Some(big.as_str()));
    }
}
