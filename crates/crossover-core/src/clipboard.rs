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
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crossover_platform::{
    BlobNaming, ClipboardContent, ClipboardImageFormat, FileBlob, FileBlobRefusal,
};
use crossover_protocol::clipboard::{
    ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ChunkOutcome, ChunkPlan, ChunkReassembly, ChunkStream,
    ClipboardAccept, ClipboardApplied, ClipboardChunk, ClipboardData, ClipboardDecline,
    ClipboardMeta, ClipboardOffer, ContentType, DeclineReason, FileDescriptor, ImageFormat,
    MAX_CLIPBOARD_FILE_ENTRIES, StreamOutcome, content_hash,
};
use crossover_protocol::hello::MessageType;

use crate::file_blob::wire_file_name;
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
///
/// **What this concedes, stated plainly.** An id in this ring is a
/// permanently free channel for the rest of the session: a peer may send
/// chunks bearing it forever and be charged nothing. That is deliberate,
/// and the reasoning is economic rather than structural. To obtain such an
/// id a peer must first have had a transfer complete or be abandoned —
/// and having got one, the traffic buys it nothing: no state is created,
/// no memory is committed, no answer is sent, and the frames are logged at
/// **debug**, so at default levels the cost to this side is a decode and a
/// comparison per frame. That is strictly less than the peer spends
/// sending them, and it is no better than what any unknown message type
/// already costs (docs/PROTOCOL.md §7 — skipped, counted, debug). The one
/// caveat worth knowing when diagnosing: under `RUST_LOG=debug` the
/// *volume* of that logging is the peer's to choose, so a machine left in
/// debug logging can have its log growth driven from the far end.
const RECENT_TRANSFER_MEMORY: usize = 4;

/// Total bytes the spool may hold (ADR 0015).
///
/// A backstop rather than a working limit: an entry lives only while the
/// clipboard still offers what it backs, so a healthy machine holds one.
/// It bounds what a peer can leave on this machine's disk if that rule
/// ever fails to fire — which is what a bound is for.
///
/// Counted against **at admission**, including the in-flight partial,
/// rather than after completion. Testing feasibility at admission but
/// only evicting on completion would let one more transfer write its
/// partial alongside an already-full spool, so the honest peak would have
/// been `MAX_SPOOL_BYTES + MAX_CLIPBOARD_FILE_BYTES` — reserving up front
/// makes this figure the true ceiling.
pub const MAX_SPOOL_BYTES: u64 = 1024 * 1024 * 1024;

/// How many completed entries the spool retains before the oldest is
/// evicted to admit a new one (ADR 0015). The second backstop, on count
/// rather than bytes: many small files must not become many entries.
pub const MAX_SPOOL_ENTRIES: usize = 16;

/// Headroom required on the spool volume beyond the offered length
/// before a file transfer is accepted (ADR 0015).
///
/// The margin is the point: filling a user's system volume to the last
/// byte is a fault of its own, worse than the refusal that avoids it, and
/// the refusal is one frame the origin can act on (FR-3.6).
pub const MIN_FREE_SPACE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// How long an entry may sit in the spool without the clipboard ever
/// being observed to move on (ADR 0015).
///
/// A backstop behind the real rule, not the rule itself. An entry lives
/// while the clipboard still offers what it backs, which depends on
/// *observing* the clipboard move on; a lost listener or a missed
/// ownership change would otherwise strand peer bytes on disk forever.
/// This is the floor-sweeper for that case, and it is deliberately far
/// longer than any paste a user is still thinking about.
pub const SPOOL_SWEEP_TTL: Duration = Duration::from_hours(24);

/// In-flight file transfers per session (ADR 0015).
///
/// Structural rather than checked: the engine holds `Option<FileTransfer>`,
/// so a second transfer cannot exist to be counted. Stated as a constant
/// because it is a bound the ADR names, and asserted by the test that a
/// superseding offer leaves exactly one partial behind.
pub const MAX_CONCURRENT_FILE_TRANSFERS: usize = 1;

/// Clipboard engine tuning. Grouped because all three knobs are timing
/// policy, and tests need to shrink them without pretending the
/// production defaults are different.
///
/// [`Default`] is [`ClipboardConfig::new`], not a derive, and the
/// difference is not cosmetic: a derived `Default` gives every `Duration`
/// field zero, which here means "abandon each transfer the instant it
/// starts" and "disable the transmit debounce ADR 0006 exists for". A
/// caller writing `..Default::default()` would get that silently, with
/// nothing to review — the same trap `KeepaliveConfig` avoids the same
/// way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardConfig {
    /// Bounded retry for `Busy` clipboard writes (FR-3.4).
    pub retry: RetryPolicy,
    /// Quiet period before staged content is transmitted (ADR 0006).
    pub transmit_debounce: Duration,
    /// Deadline on a transfer that retains content (ADR 0014).
    pub transfer_timeout: Duration,
    /// Age backstop for a spool entry whose clipboard was never observed
    /// to move on (ADR 0015). Configurable for the same reason the others
    /// are: a test must be able to shrink it without the production
    /// default pretending to be something else.
    pub spool_sweep_ttl: Duration,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardConfig {
    /// Production defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            retry: RetryPolicy::default(),
            transmit_debounce: TRANSMIT_DEBOUNCE,
            transfer_timeout: TRANSFER_TIMEOUT,
            spool_sweep_ttl: SPOOL_SWEEP_TTL,
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
///
/// **Known coarseness.** One case reaches this deadline having never had a
/// chance: an offer the session's send gate refuses locally, because the
/// peer never advertised the content type (docs/PROTOCOL.md §3.1). The
/// engine is sans-io and holds no session knowledge — it cannot know a
/// capability was missing — so it waits out the full minute holding the
/// item, then abandons it like any other unanswered offer. The wait is
/// bounded and the outcome is *counted*
/// ([`Metrics::record_clipboard_abandoned`]) rather than merely logged, so
/// the case is diagnosable instead of silent; teaching the engine the
/// negotiated feature set would fix it properly, and would mean handing
/// the state machine session state it otherwise has no reason to know.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_mins(1);

/// How long a parked install waits between attempts (ADR 0005, addendum
/// 2026-09-01).
///
/// A second rather than the fast schedule's 200 ms because the parked
/// phase is answering a different question. The fast phase covers a
/// *blip* — another application between `OpenClipboard` and
/// `CloseClipboard` — and polling hard is right for something that
/// resolves in milliseconds. Past that budget the holder is doing
/// something, and re-taking the machine-global lock five times a second
/// while it does is how Crossover made other applications' clipboard
/// calls fail in the two-machine soak (docs/SOAK.md). Once a second is
/// cheap enough to be a good neighbour and frequent enough that a
/// clipboard freed at any moment is re-tried almost immediately — and the
/// change notification, which is the *primary* revival, usually gets
/// there first.
pub const PARK_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How long an install may stay parked before it is finally reported
/// `ClipboardUnavailable` (ADR 0005, addendum 2026-09-01).
///
/// The arithmetic that fixes it is the origin's, not ours. An outbound
/// transaction is abandoned after [`TRANSFER_TIMEOUT`] — 60 s — so a
/// receiver that keeps trying past that point is answering a transaction
/// nobody is listening to any more, and the origin would count an
/// `abandoned` where a verdict was on its way. The whole install budget
/// must therefore finish comfortably *inside* 60 s: the fast phase is
/// 5 attempts × 200 ms ≈ 0.8 s, this adds 20 s, and the last parked
/// attempt can be scheduled up to [`PARK_RETRY_DELAY`] after the budget
/// is checked, so the worst case is ≈ 22 s — roughly a third of the
/// origin's patience, with the remaining two thirds absorbing the network
/// and any queueing on either side.
///
/// Twenty seconds is also chosen against the observed fault: on machine A
/// (2026-09-01) an external holder kept the clipboard for about a second
/// at five of eight reconnects, which the fast budget missed by a hair.
/// A budget an order of magnitude past the observed hold is what makes
/// the fix about the *class* of fault rather than about one second.
pub const PARK_BUDGET: Duration = Duration::from_secs(20);

/// Retry policy for `Busy` clipboard writes (FR-3.4): centrally defined,
/// bounded attempts, bounded total time (ADR 0005 requires exactly this
/// shape).
///
/// Two phases, not one (ADR 0005, addendum 2026-09-01). The fast phase is
/// the original bounded schedule and covers the common blip. When it is
/// exhausted and the clipboard is still `Busy` the install is **parked**
/// rather than failed: it keeps being retried, on the slower
/// [`Self::park_delay`] cadence and on every local change notification,
/// until [`Self::park_budget`] elapses. Both phases together are still a
/// hard bound, which is what ADR 0005 requires — losing a user's clipboard
/// item to a second of contention was the defect, not the bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Fast-phase write attempts before the install parks (first try
    /// included).
    pub max_attempts: u32,
    /// Delay between fast-phase attempts.
    pub delay: Duration,
    /// Delay between parked attempts.
    pub park_delay: Duration,
    /// How long an install may stay parked before it is reported
    /// `ClipboardUnavailable`. [`Duration::ZERO`] configures the parked
    /// phase off entirely, which only a test that is about the fast cap
    /// itself has any reason to do.
    pub park_budget: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            delay: Duration::from_millis(200),
            park_delay: PARK_RETRY_DELAY,
            park_budget: PARK_BUDGET,
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
    /// A local file selection the driver is packing into a blob (ADR
    /// 0015).
    ///
    /// A third scope rather than a reuse of [`Self::Outbound`], for the
    /// reason that made two out of one: a build runs *while an unrelated
    /// outbound transfer is still in flight* — it does not supersede
    /// anything until it has something to supersede with — so arming it
    /// on the outbound clock would restart that transfer's deadline and
    /// leave it unbounded. A build that never reports back is its own
    /// stall, with its own bound.
    Build,
}

/// Why a clipboard write did not succeed.
///
/// Three outcomes rather than a retryable/not-retryable flag, because the
/// third one is a different *kind* of answer: a content type this
/// destination cannot represent is a permanent statement about the item,
/// where an unavailable clipboard is a transient statement about the
/// machine. Collapsing them would tell the origin "clipboard
/// unavailable" for an image that will never install here, whatever it
/// tries (NFR-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFailure {
    /// Transient contention (FR-3.4): the only failure that is retried.
    Busy,
    /// The clipboard could not be written and retrying will not help.
    Unavailable,
    /// The backend does not handle this content type at all.
    UnsupportedType,
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
    /// Reserve room for an offered file and create the partial it streams
    /// into, then report back via [`ClipboardEngine::on_file_admitted`]
    /// (ADR 0015).
    ///
    /// The offer is answered by *that* reply and not before: a receiver
    /// that accepted first and discovered the volume was full afterwards
    /// would have spent the sender's bytes to learn what one frame could
    /// have said.
    AdmitFile {
        /// Transaction id the reply must reference.
        id: Uuid,
        /// The partial's name in the spool. Ours, never the peer's.
        entry: String,
        /// Offered length: what the free-space check is against, and what
        /// the spool budget reserves.
        byte_len: u64,
    },
    /// Append one verified-in-sequence chunk to the open partial, then
    /// call [`ClipboardEngine::on_file_chunk_written`] — or
    /// [`ClipboardEngine::on_file_write_failed`] if it did not land.
    ///
    /// The payload is judged before it gets here (`ChunkStream`), so what
    /// this writes is always a prefix of a conforming transfer.
    WriteFileChunk {
        /// Which transfer the bytes belong to.
        id: Uuid,
        /// The bytes, moved rather than copied out of the chunk frame.
        payload: Vec<u8>,
    },
    /// Promote the verified partial to a spool entry and report back via
    /// [`ClipboardEngine::on_file_committed`]. The rename is the moment
    /// the bytes become advertisable, and it happens only after the hash
    /// and the length have both verified.
    CommitFile {
        /// Which transfer completed.
        id: Uuid,
        /// The partial's name.
        from: String,
        /// The entry name it takes once it is the offered item.
        to: String,
    },
    /// Close the open partial for `id` and unlink it. Best-effort and
    /// idempotent: no reply, because there is no decision left to make —
    /// **nothing partially received is ever registered** (ADR 0015).
    AbortFile {
        /// Which transfer is being abandoned.
        id: Uuid,
        /// The partial to remove.
        entry: String,
    },
    /// Unlink a completed spool entry the budget has evicted (ADR 0015).
    EvictSpoolEntry {
        /// The entry to remove.
        entry: String,
    },
    /// Offer a verified entry to the OS paste mechanism as a virtual file
    /// list, then report back via [`ClipboardEngine::on_file_offered`]
    /// (ADR 0015).
    ///
    /// This is what makes a delivered file reachable at all: until it
    /// happens the bytes are in the spool and nothing can paste them. It
    /// replaces whatever the clipboard held, exactly as installing any
    /// other item does.
    OfferFile {
        /// Transaction id the reply must reference.
        id: Uuid,
        /// The entry to offer, and what to say about it.
        file: SpooledFile,
    },
    /// Take our virtual file list off the clipboard, because the entry
    /// behind it is going away. Best-effort: a clipboard that has already
    /// moved on needs nothing done to it.
    WithdrawFileOffer,
    /// Call [`ClipboardEngine::on_spool_sweep_due`] after `delay`: the
    /// age backstop behind the clipboard-lifetime rule
    /// ([`SPOOL_SWEEP_TTL`]).
    ScheduleSpoolSweep {
        /// How long to wait.
        delay: Duration,
    },
    /// Pack a local file selection into one offerable blob, then report
    /// back via [`ClipboardEngine::on_file_blob_built`] (ADR 0015,
    /// "Sender side").
    ///
    /// Emitted only after every gate that can be judged without touching
    /// the filesystem has passed, because this is the expensive one: the
    /// walk reads the selection and the archive writes it out again, which
    /// on gigabytes is seconds and a temporary file. Spending that to
    /// learn what a feature bit or a permission flag already said would be
    /// the sender-side version of the mistake the receiver avoids by
    /// answering the offer before the bytes arrive.
    ///
    /// **Blocking, and long.** The driver must not run it on the
    /// clipboard listener's thread or on the loop that has to keep
    /// answering events (ADR 0015, "Threading").
    BuildFileBlob {
        /// Transaction id the reply — and the blob — must reference.
        id: Uuid,
        /// The raw local paths the clipboard reported, unvalidated and in
        /// the order it reported them.
        selection: Vec<PathBuf>,
    },
    /// Read one chunk out of the built blob and send it to the peer, then
    /// call [`ClipboardEngine::on_chunk_sent`] — or
    /// [`ClipboardEngine::on_file_read_failed`] if the bytes could not be
    /// read.
    ///
    /// The engine names the slice rather than carrying it, which is what
    /// makes the sending half O(chunk) instead of O(file): a 256 MiB item
    /// is 4096 of these, and at no point does either the engine or the
    /// driver hold more than one chunk of it (ADR 0015, mirroring the
    /// receiver's write-through).
    SendFileChunk {
        /// Which transfer the chunk belongs to.
        id: Uuid,
        /// Chunk index, as the receiver's plan will expect it.
        index: u32,
        /// Byte offset into the blob.
        offset: u64,
        /// Exactly how many bytes this chunk carries.
        len: u32,
    },
    /// Drop the built blob for `id`, deleting the sender's temporary
    /// artifact. Best-effort and idempotent: no reply, because there is
    /// no decision left to make.
    ///
    /// Emitted on **every** path that ends an outbound file transaction —
    /// delivered, declined, superseded, timed out, session lost — so a
    /// stalled transaction can never pin up to `MAX_CLIPBOARD_FILE_BYTES`
    /// of this machine's own disk (NFR-1).
    ReleaseFileBlob {
        /// Which blob is finished with.
        id: Uuid,
    },
}

/// Whether peer files may be received at all, and if not, why not.
///
/// Three states rather than a boolean, because the two refusals are
/// different answers to the origin and it acts on them differently: a
/// build with no protected spool will never take a file, while a peer
/// without the `file_receive` grant is one `crossover peers allow-files`
/// away (NFR-3). The engine is sans-io and holds no trust store, so the
/// application supplies this and refreshes it as the store changes; the
/// default is the closed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileReceive {
    /// No protected spool on this platform or in this run: files cannot
    /// be received here at all. The default, so a build that never wires
    /// a spool refuses by construction rather than by remembering to.
    #[default]
    Unsupported,
    /// The peer has not been granted `file_receive` (ADR 0015,
    /// SECURITY.md invariant 8). Default-off, and never granted by
    /// pairing.
    Denied,
    /// Granted: offers are judged on their merits.
    Allowed,
}

/// Whether a local file selection may be sent to the peer at all, and if
/// not, why not (ADR 0015, "Sender side").
///
/// The mirror of [`FileReceive`], and four states for the same reason
/// that one has three: the refusals are different answers and a user acts
/// on them differently (NFR-3). The engine is sans-io and knows neither
/// the negotiated feature set nor the trust store, so the application
/// supplies this and refreshes it as either changes; the default is the
/// closed one, so a build that never wires a sender refuses by
/// construction rather than by remembering to.
///
/// **This is a gate, not an optimization.** It is judged *before* a
/// selection is walked, so a peer that cannot take files never costs this
/// machine a filesystem walk and an archive — and, more importantly, an
/// un-negotiated `ContentType::File` is fatal to an older peer's session
/// rather than skippable (docs/PROTOCOL.md §3.1), so the offer must never
/// be built in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileSend {
    /// No blob builder in this build: a selection cannot be packed here,
    /// whatever the peer or the trust store say. The default.
    #[default]
    Unsupported,
    /// The peer never advertised `FILE_CLIPBOARD`, so a file offer would
    /// be a frame its session cannot decode.
    NotNegotiated,
    /// The peer holds no `clipboard_send` grant.
    Denied,
    /// Granted and negotiated: selections are judged on their merits.
    Allowed,
}

/// What the driver built from a local selection, minus the bytes.
///
/// Everything a `crossover_platform::FileBlob` carries except its open
/// handle, which stays with the driver: the engine decides the
/// transaction from the length, the hash and the name, and never sees a
/// byte of the item (ADR 0015). The name is still the *proposed* one —
/// judging it against the wire's rules is the engine's job, because a
/// name that reaches a shell is judged by exactly one validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltBlob {
    /// The bare name the item should travel under, not yet validated.
    pub proposed_name: String,
    /// Where that name came from, and so what a failed validation means.
    pub naming: BlobNaming,
    /// Whether the blob is an archive the builder packed.
    pub archived: bool,
    /// Filesystem entries packed.
    pub entry_count: u32,
    /// Total bytes of those entries before packing.
    pub original_bytes: u64,
    /// Exact length of the blob: what the offer declares.
    pub content_length: u64,
    /// SHA-256 of the blob, as the receiver will verify it.
    pub content_hash: [u8; 32],
}

impl BuiltBlob {
    /// Everything but the bytes, taken from a built blob.
    #[must_use]
    pub fn of(blob: &FileBlob) -> Self {
        Self {
            proposed_name: blob.proposed_name.clone(),
            naming: blob.naming,
            archived: blob.archived,
            entry_count: blob.entry_count,
            original_bytes: blob.original_bytes,
            content_length: blob.content_length,
            content_hash: blob.content_hash,
        }
    }
}

/// Why the spool refused to admit a transfer (the driver's answer to
/// [`Action::AdmitFile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRefusal {
    /// The volume has less than the offered length plus
    /// [`MIN_FREE_SPACE_MARGIN_BYTES`] free.
    InsufficientSpace,
    /// The spool could not be reserved or opened. A statement about now,
    /// not about the item.
    Storage,
}

/// A verified file resting in the spool (ADR 0015).
///
/// The peer's name is here, as metadata, and **not** on the filesystem:
/// the entry is named by a locally generated id, and `descriptor` is what
/// a paste will present to the shell once the platform half exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpooledFile {
    /// The entry's bare name in the spool root.
    pub entry: String,
    /// What the sender said the item is — a validated name, and whether
    /// it is an archive it built.
    pub descriptor: FileDescriptor,
    /// Verified byte length of the entry.
    pub byte_len: u64,
    /// Verified content hash, as offered.
    pub content_hash: [u8; 32],
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
/// `content` is the outbound memory commitment, and it is deliberate:
/// exactly one item is retained *in this slot* at a time (a newer local
/// copy supersedes and replaces it), for at most
/// [`ClipboardConfig::transfer_timeout`], bounded by the content type's
/// maximum — 64 MiB for an image (ADR 0014). Chunks are sliced out of it
/// on demand rather than pre-rendered, so this slot's peak is one buffer
/// plus one chunk.
///
/// It is not the engine's *only* buffer: an inbound reassembly and a
/// `PendingWrite` under retry are independent slots of the same size, so
/// the honest whole-engine worst case is their sum — see
/// docs/ARCHITECTURE.md §5.2, which states it.
#[derive(Debug)]
enum Outbound {
    /// Offer sent; awaiting Accept/Decline. Holds the body, because
    /// an Accept means "send it now".
    AwaitingAccept {
        meta: ClipboardMeta,
        body: OutboundBody,
        started: Instant,
    },
    /// Accepted and streaming chunks (ADR 0014). `next_index` is the
    /// chunk to emit when the driver comes back for another.
    Streaming {
        meta: ClipboardMeta,
        body: OutboundBody,
        plan: ChunkPlan,
        next_index: u32,
        started: Instant,
    },
    /// Everything sent; awaiting Applied. Body released.
    AwaitingApplied {
        meta: ClipboardMeta,
        started: Instant,
    },
}

/// Where an in-flight outbound item's bytes actually live.
///
/// Two answers, and the difference is the whole reason the file half
/// costs the engine no memory (ADR 0015). Text and an image are read
/// into this process and retained here until the transaction closes; a
/// file selection is packed into a blob the *driver* holds open on disk,
/// and the engine never sees a byte of it — it knows the length, the
/// hash and the name, which is everything the transaction is decided
/// from. Chunking is identical either way: the same [`ChunkPlan`], the
/// same one-chunk-at-a-time pacing (ADR 0013), only a different place
/// the bytes are fetched from when the chunk is actually sent.
#[derive(Debug)]
enum OutboundBody {
    /// Bytes retained by the engine (text, images).
    Bytes(Vec<u8>),
    /// A blob the driver holds open (ADR 0015). Carries nothing: the
    /// descriptor went out with the offer, and what remains to be decided
    /// about a file in flight is decided from its `ClipboardMeta` like
    /// any other item's.
    Blob,
}

impl OutboundBody {
    /// Whether these bytes are held in *this* process's memory, and so
    /// are what the transfer deadline exists to bound.
    const fn is_retained(&self) -> bool {
        matches!(self, Self::Bytes(_))
    }
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
        match self {
            Self::AwaitingAccept { body, .. } | Self::Streaming { body, .. } => body.is_retained(),
            Self::AwaitingApplied { .. } => false,
        }
    }

    /// The action that hands the driver-held blob back, if this state
    /// still pins one.
    ///
    /// Called on **every** path that ends an outbound transaction, which
    /// is the whole discipline: dropping the blob deletes the sender's
    /// temporary artifact, and a transaction that ended without doing so
    /// would pin up to `MAX_CLIPBOARD_FILE_BYTES` of the sender's own
    /// disk until the process exited (ADR 0015, NFR-1).
    fn release(&self) -> Option<Action> {
        match self {
            Self::AwaitingAccept {
                meta,
                body: OutboundBody::Blob,
                ..
            }
            | Self::Streaming {
                meta,
                body: OutboundBody::Blob,
                ..
            } => Some(Action::ReleaseFileBlob { id: meta.id }),
            _ => None,
        }
    }
}

/// Where an inbound file transfer has got to (ADR 0015).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    /// The spool has been asked for room and a partial to write into.
    /// The offer is unanswered until it replies.
    Admitting,
    /// Accepted; chunks are being written through.
    Streaming,
    /// Every chunk has been accepted and the item's hash has verified.
    /// The last write is still in flight — the commit waits for it,
    /// because an entry must never be promoted ahead of its own bytes.
    Verified,
    /// The partial is being promoted to an entry.
    Committing,
}

/// A verified entry waiting to reach the clipboard.
///
/// The transfer is over — the bytes are on disk under their final name —
/// but the transaction is not: a file nobody can paste is not a delivery,
/// so the origin's verdict waits for the offer to land (FR-3.2 as ADR
/// 0015 adapts it).
#[derive(Debug)]
struct PendingOffer {
    id: Uuid,
    file: SpooledFile,
    /// Offering takes the machine-global clipboard lock, so it meets the
    /// same contention every write does and gets the same bounded retry
    /// (FR-3.4).
    attempts_made: u32,
    started: Instant,
}

/// An inbound file transfer, written through to the spool rather than
/// held (ADR 0015).
///
/// The memory here is the whole commitment: accounting, a descriptor, and
/// an id. The bytes are on their way to disk and are never in this
/// struct — which is what lets a file be four times the size an image is
/// allowed to be.
#[derive(Debug)]
struct FileTransfer {
    stream: ChunkStream,
    descriptor: FileDescriptor,
    /// Locally generated, and the reason the peer's name never becomes a
    /// filesystem name on this machine (ADR 0015).
    entry_id: Uuid,
    state: FileState,
    started: Instant,
}

impl FileTransfer {
    fn id(&self) -> Uuid {
        self.stream.meta().id
    }

    /// The partial: created exclusively, deleted on any outcome but a
    /// verified completion.
    fn part_name(&self) -> String {
        format!("{}.part", self.entry_id)
    }

    /// The name the partial takes once — and only once — it is the item
    /// that was offered.
    fn entry_name(&self) -> String {
        format!("{}.bin", self.entry_id)
    }
}

/// A local file selection handed to the driver's builder and not yet
/// answered (ADR 0015).
///
/// Holds no paths and no bytes: the selection went out with the action,
/// and what is kept is only what is needed to recognize the answer and to
/// say how long the pack took.
#[derive(Debug)]
struct PendingBuild {
    id: Uuid,
    started: Instant,
}

/// Inbound write-with-retry state.
#[derive(Debug)]
struct PendingWrite {
    meta: ClipboardMeta,
    content: Arc<ClipboardContent>,
    attempts_made: u32,
    /// When the fast retry budget ran out and the install parked, or
    /// `None` while it is still in the fast phase (ADR 0005, addendum
    /// 2026-09-01). The instant, not a flag, because the parked budget is
    /// measured from here.
    parked_since: Option<Instant>,
    /// Whether a retry timer is outstanding and still entitled to fire.
    ///
    /// The parked phase has two revival paths — the slow timer and a local
    /// change notification — and without this they would race into two
    /// concurrent write attempts for one transaction, each scheduling its
    /// own successor. Whichever gets there first consumes the entitlement;
    /// the other fires into a no-op, exactly as a stale generation-tagged
    /// timer does elsewhere, so nothing has to be cancelled.
    retry_armed: bool,
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
    /// The next read should announce whatever it finds even if dedup
    /// would suppress it — set by [`ClipboardEngine::on_session_established`]
    /// so peers converge after a gap (ADR 0006, trigger 3).
    ///
    /// A flag rather than the older trick of clearing
    /// [`Self::current_local_hash`], because that trick threw away the one
    /// fact the read needs: whether the content is actually *new*. With
    /// the hash gone, a reconnect's re-read of unchanged content was
    /// indistinguishable from the user copying something, which made it
    /// supersede a parked install in precisely the hardware scenario the
    /// parked phase exists for (ADR 0005, addendum 2026-09-01).
    reannounce_pending: bool,
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
    /// The inbound *file* transfer being written through to the spool
    /// (ADR 0015). At most one, like the reassembly beside it — and for a
    /// stronger reason: this one holds an open partial on disk, and
    /// `MAX_CONCURRENT_FILE_TRANSFERS` is that `Option`.
    file: Option<FileTransfer>,
    /// Whether peer files may be received here at all. Supplied by the
    /// application from the trust store; closed until it says otherwise.
    file_receive: FileReceive,
    /// Whether local files may be *sent* to the peer (ADR 0015). Supplied
    /// the same way and for the same reason, and closed by default.
    file_send: FileSend,
    /// The spool root, for one purpose only: recognizing a `CF_HDROP`
    /// that points back into it, which must never be staged (ADR 0015
    /// loop prevention, SECURITY.md F13). Held as text and compared, never
    /// opened and never resolved — the spool itself is reached by handle
    /// and nothing here changes that. `None` where this build has no
    /// spool, which is also the only honest answer then: with nothing
    /// writing there, no path can be inside it.
    spool_root: Option<PathBuf>,
    /// A local file selection the driver is packing (ADR 0015). At most
    /// one, and a newer local copy supersedes it — the same rule
    /// `outbound` has, one step earlier in the pipeline.
    building: Option<PendingBuild>,
    /// Verified spool entries, oldest first, each with when it was
    /// registered — the eviction order, the spool's byte budget, and the
    /// age the [`SPOOL_SWEEP_TTL`] backstop measures. Computed from
    /// entries this engine put there rather than from whatever is in the
    /// directory. Bounded by [`MAX_SPOOL_ENTRIES`].
    spooled: VecDeque<(SpooledFile, Instant)>,
    /// A verified entry whose offer to the clipboard has not been
    /// answered yet.
    offering: Option<PendingOffer>,
    /// The entry our virtual file list currently advertises, if any. Kept
    /// so that evicting *that* entry also takes the promise off the
    /// clipboard rather than leaving one nothing can serve.
    offered: Option<String>,
    /// Ids of chunked transfers recently finished or abandoned, so their
    /// in-flight tail is recognized as the benign race it is rather than
    /// charged to the violation budget.
    recent_transfers: VecDeque<Uuid>,
    /// Deadline generations (ADR 0014). Bumped when a transfer that
    /// retains content starts; a timeout for an older generation is a
    /// no-op, so superseded timers need no cancellation.
    outbound_generation: u64,
    inbound_generation: u64,
    build_generation: u64,
    /// The write (with retries) currently underway.
    pending_write: Option<PendingWrite>,
    /// Clipboard protocol violations this peer has committed since the
    /// session was established (docs/PROTOCOL.md §7). Reset by
    /// [`ClipboardEngine::on_session_established`], so the budget is per
    /// session rather than per process.
    violations: u32,
    /// How many sessions are currently live (ADR 0006, addendum
    /// 2026-09-01). Nothing is transmitted while this is zero.
    ///
    /// A **count**, not a flag, because this process can hold more than
    /// one session at once: the inbound listener and the outbound
    /// supervisor run independently, so a machine can be serving one peer
    /// while dialling another — and both fan `SessionEstablished` /
    /// `SessionLost` into this one engine. A flag would be cleared by the
    /// first of two peers to drop, and every copy after that would be
    /// silently held from the peer that was still there: a clipboard that
    /// stops working with no fault visible anywhere, which is the
    /// priority-#2 failure this rule exists to avoid causing.
    ///
    /// The two events are strictly paired at every call site, so the
    /// count tracks reality; `saturating_sub` keeps an unpaired loss from
    /// wrapping, and [`Self::on_session_established`] re-reads the
    /// clipboard, so even a miscount heals at the next connect rather
    /// than persisting.
    live_sessions: u32,
    /// Whether the current offline stretch has already announced itself.
    ///
    /// A pair can be apart for hours (docs/SOAK.md), and one `info` line
    /// per copy for eight hours is noise that buries the lines a soak is
    /// read for. The first copy of a stretch says it at `info`, the rest
    /// at `debug`, and [`Self::on_session_established`] arms it again for
    /// the next stretch.
    offline_announced: bool,
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
            reannounce_pending: false,
            applied_hashes: VecDeque::new(),
            outbound: None,
            expecting_data: None,
            reassembly: None,
            file: None,
            file_receive: FileReceive::default(),
            file_send: FileSend::default(),
            spool_root: None,
            building: None,
            spooled: VecDeque::new(),
            offering: None,
            offered: None,
            recent_transfers: VecDeque::new(),
            outbound_generation: 0,
            inbound_generation: 0,
            build_generation: 0,
            pending_write: None,
            violations: 0,
            live_sessions: 0,
            offline_announced: false,
            metrics,
        }
    }

    /// Record into the metrics sink if one is attached; a no-op otherwise.
    fn record(&self, f: impl FnOnce(&Metrics)) {
        if let Some(metrics) = &self.metrics {
            f(metrics);
        }
    }

    /// Is there a peer for an item to travel to at all?
    ///
    /// The one question that separates "this copy is going nowhere" from
    /// "this copy is going nowhere *yet*", and the whole basis of the
    /// 2026-09-01 addendum to ADR 0006: with no session live, minting a
    /// deadline-bound transaction produces one broadcast frame that the
    /// application drops for want of a sink, and one `abandoned` warning
    /// sixty seconds later for a fault that never happened.
    fn has_live_session(&self) -> bool {
        self.live_sessions > 0
    }

    /// Count a local change that was recorded but not transmitted, and
    /// say so at most once per offline stretch.
    ///
    /// The counter takes every copy, because the run report's job is to
    /// account for all of them; the `info` line takes only the first,
    /// because the log's job is to be readable after eight hours with a
    /// peer that is asleep. Each caller has already emitted its own
    /// `debug` line with the fields particular to its content type.
    fn note_offline_change(&mut self) {
        self.record(Metrics::record_clipboard_offline_change);
        if self.offline_announced {
            return;
        }
        self.offline_announced = true;
        tracing::info!(
            "clipboard: no peer connected; the current item will be offered when one connects"
        );
    }

    /// The provider signaled a change.
    ///
    /// Deliberately does **not** read: reading takes the machine-global
    /// clipboard lock, and a notification only means "something changed",
    /// which during a burst is true many times per second. Wait for quiet
    /// (ADR 0006), then read once.
    /// A parked install is deliberately **not** retried here, and this is
    /// the load-bearing half of the parked design's safety. A change
    /// notification on Windows almost always means new content just
    /// landed, and writing on it would race the user: a peer item parked
    /// under contention would install itself over the copy the user made
    /// one instant earlier, and the settle read would then find our own
    /// content, suppress it as a loop, and report nothing. The user's copy
    /// would be gone with no diagnostic anywhere.
    ///
    /// So the notification only starts the clock. The *read* decides
    /// ([`Self::on_local_read`]): unchanged or our own content means the
    /// clipboard is merely free and the parked install may take it;
    /// genuinely new content means the user outranks it. The cost is one
    /// settle window, with the parked timer as the backstop underneath.
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
    ///
    /// A [`ClipboardContent::FileList`] observation goes down its own
    /// path (ADR 0015, "Sender side"): the bytes do not exist yet, so it
    /// is gated, then handed to the driver's builder, and only the blob
    /// that comes back can be hashed, named and offered. Everything a
    /// clipboard item is judged on here — dedup, loop suppression, the
    /// type's maximum — applies to it too, one step later, where the
    /// numbers it is judged on first exist.
    /// This is also where a **parked install** learns its fate (ADR 0005,
    /// addendum 2026-09-01), because this is the first moment anything
    /// knows what the clipboard actually holds:
    ///
    /// - content we ourselves installed, or content unchanged since we
    ///   last looked — the clipboard is merely free again, so the parked
    ///   install is retried now rather than waiting out its timer;
    /// - genuinely new content — this machine's user copied something, and
    ///   installing a peer item over it would destroy what they just made,
    ///   so the parked install is superseded;
    /// - nothing readable — no evidence either way, so the parked install
    ///   is left to its own timer. **Known residual** (ADR 0005, addendum
    ///   2026-09-01): a copy in a format this build cannot render answers
    ///   `None` too, and is indistinguishable from an empty clipboard
    ///   here, so the parked install can still overwrite it. Fixing it
    ///   means the provider separating `Empty` from `Unreadable`, which is
    ///   a platform-trait change and its own branch.
    pub fn on_local_read(&mut self, content: Option<ClipboardContent>) -> Vec<Action> {
        // Consumed whatever the read shows: the re-announcement had its
        // chance, and a flag left set would make the next ordinary read
        // behave like a reconnect.
        let reannouncing = std::mem::take(&mut self.reannounce_pending);
        let Some(content) = content else {
            return Vec::new(); // empty, or a format this build cannot read
        };
        if let ClipboardContent::FileList(selection) = content {
            return self.on_local_file_list(selection, reannouncing);
        }
        let Some((content_type, bytes)) = into_wire(content) else {
            return Vec::new(); // never reached; see `into_wire`
        };
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

        // Loop prevention: this is content we ourselves applied. Nothing
        // travels — and the clipboard is demonstrably readable and holds
        // our own content rather than the user's, which is exactly when a
        // parked install may take it.
        if self.applied_hashes.contains(&hash) {
            self.current_local_hash = Some(hash);
            self.record(Metrics::record_clipboard_loop_suppressed);
            return self.revive_parked_write();
        }
        // Dedup: unchanged content never re-sends. Same reasoning — the
        // user has not put anything here since we last looked.
        let unchanged = self.current_local_hash == Some(hash);
        if unchanged && !reannouncing {
            return self.revive_parked_write();
        }
        self.current_local_hash = Some(hash);

        // Only content that is genuinely new to this machine outranks a
        // parked install. A reconnect's re-announcement of what was
        // already here is not new, and must not cost the peer's item —
        // that combination is the 2026-09-01 hardware scenario exactly.
        let mut superseded = if unchanged {
            Vec::new()
        } else {
            self.supersede_parked_write("a local copy")
        };

        // Nobody to offer it to (ADR 0006, addendum 2026-09-01). The
        // observation above stands in full — the hash is updated, dedup
        // and loop suppression are untouched, a parked install has had
        // its answer — and nothing is minted: no outbound slot, no
        // deadline, no frame.
        //
        // Placed *after* the supersession and returning what it built,
        // not `Vec::new()`. A `Superseded` verdict is constructed
        // immediately above, and swallowing one here would leave the
        // origin waiting out its own 60 s deadline for an answer this
        // machine had already reached — the exact failure ADR 0005's
        // "every transaction ends in a typed verdict" invariant forbids.
        // On today's code that vector is always empty on this path: a
        // parked install belongs to a session, and `on_session_lost`
        // drops the pending write before the count can reach zero, so
        // there is nothing to supersede once there is nobody to tell.
        // The ordering is written to be safe rather than to happen to
        // be, because what makes it unreachable lives in another method.
        if !self.has_live_session() {
            tracing::debug!(
                byte_count = bytes.len(),
                content_type = ?content_type,
                "no peer connected; the current item is held, not transmitted"
            );
            self.note_offline_change();
            return superseded;
        }

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
        superseded.extend(self.start_outbound(meta, bytes));
        superseded
    }

    /// Set whether peer files may be received (ADR 0015).
    ///
    /// The application owns this: it holds the trust store and knows
    /// whether a protected spool was opened, neither of which a sans-io
    /// engine can see. It is a *policy input*, re-supplied whenever the
    /// answer changes, so withdrawing `file_receive` stops the next
    /// transfer without waiting for a reconnect. In flight transfers are
    /// deliberately left alone: the bytes are already arriving into a
    /// partial that will be deleted or verified either way, and there is
    /// nothing for a half-applied revocation to protect.
    pub fn set_file_receive(&mut self, receive: FileReceive) {
        if self.file_receive != receive {
            tracing::info!(policy = ?receive, "file receive policy changed");
        }
        self.file_receive = receive;
    }

    /// Set whether local files may be sent to the peer (ADR 0015).
    ///
    /// The application owns this for the same reason it owns
    /// [`Self::set_file_receive`]: it holds the trust store and the
    /// negotiated feature set, and a sans-io engine can see neither. It is
    /// re-supplied whenever either answer changes, so a revoked grant or a
    /// reconnection to a peer that cannot take files stops the *next*
    /// selection without waiting for anything.
    pub fn set_file_send(&mut self, send: FileSend) {
        if self.file_send != send {
            tracing::info!(policy = ?send, "file send policy changed");
        }
        self.file_send = send;
    }

    /// Tell the engine where the spool root is, so a copy of something
    /// inside it is never staged back to its sender (ADR 0015 loop
    /// prevention, SECURITY.md F13).
    ///
    /// The path is **compared and never used**: nothing here opens it,
    /// resolves it, or hands it to an API. `None` — no spool in this
    /// build — is not a hole, it is the truthful answer that nothing of
    /// ours is on disk for a selection to point at.
    pub fn set_spool_root(&mut self, root: Option<PathBuf>) {
        self.spool_root = root;
    }

    /// A local file selection was observed (ADR 0015, "Sender side").
    ///
    /// Gates first, cheapest first, and **all of them before the build**,
    /// because the build is the expensive irreversible step: it reads the
    /// selection and writes an archive, which on gigabytes is seconds and
    /// a temporary file the size of the item. Spending that to discover
    /// what a feature bit already said would be the sender's version of
    /// accepting bytes before checking whether there is room for them.
    ///
    /// The order is: is there a selection at all; is it *ours* (a copy of
    /// something in the spool, which must never travel back); may files be
    /// sent to this peer; is the selection within the entry cap that can
    /// be judged without walking anything. Only then does a build start.
    fn on_local_file_list(&mut self, selection: Vec<PathBuf>, reannouncing: bool) -> Vec<Action> {
        if selection.is_empty() {
            tracing::debug!("empty local file selection; nothing to stage");
            return Vec::new();
        }
        // Loop prevention, layer 2 (ADR 0015). Layer 1 is the platform's
        // own-object check, which fires first and without reading
        // anything; this is what holds if that ever misses — a replacement
        // object placed by a shell extension, a provider that lost track
        // across a restart. Judged before the permission gates on purpose:
        // this is not a refusal, it is our own item coming back, and
        // reporting it as a refusal would be a diagnostic that misleads.
        if self.selection_is_ours(&selection) {
            self.record(Metrics::record_clipboard_loop_suppressed);
            tracing::debug!(
                entry_count = selection.len(),
                "local file selection points inside the spool; not staging it"
            );
            return Vec::new();
        }
        // Only now is this known to be the *user's* selection, which is
        // the whole basis for it outranking a parked install (ADR 0005,
        // addendum 2026-09-01). Deciding it above the loop guard meant our
        // own spool selection coming back — the case layer 2 exists for —
        // would kill the peer's parked item and then be discarded as a
        // loop, costing the item to catch a mistake of our own.
        //
        // Below the guard but *above* the permission gates, because a
        // selection this build refuses to send is still on the clipboard
        // and still the user's: a refusal is a statement about us, not
        // about what they copied.
        let mut actions = if reannouncing {
            // A reconnect re-read is not somebody copying (see
            // `on_local_read`); it must not cost the peer's item either.
            Vec::new()
        } else {
            self.supersede_parked_write("a local copy")
        };
        // Whatever the staging decides — including the no-peer case just
        // below, which stages nothing — the verdict built above still
        // travels, because it is already in `actions`.
        actions.extend(self.stage_local_file_list(selection));
        actions
    }

    /// The gates and the build, once the selection is known to be the
    /// user's own (see [`Self::on_local_file_list`]).
    fn stage_local_file_list(&mut self, selection: Vec<PathBuf>) -> Vec<Action> {
        // Before every permission gate, because "no peer" is not a
        // refusal (ADR 0006, addendum 2026-09-01). The application's
        // policy does already close to `Denied` with nothing live, so the
        // expensive build was never at risk here — but reporting an empty
        // desk as "this peer holds no clipboard-send grant" names a peer
        // that does not exist, and charges `files_send_refused` for it.
        //
        // Below the caller's supersession rather than above it, for the
        // reason `on_local_read` returns what it built: a verdict this
        // machine has already decided must reach the origin even on a
        // path that stages nothing. Here the caller's `extend` preserves
        // it by shape, so this can return an honest empty.
        if !self.has_live_session() {
            tracing::debug!(
                entry_count = selection.len(),
                "no peer connected; nothing is packed for a selection that cannot travel"
            );
            self.note_offline_change();
            return Vec::new();
        }
        match self.file_send {
            FileSend::Allowed => {}
            FileSend::Unsupported => {
                return self.refuse_selection(
                    "this build cannot pack a file selection for sending",
                    selection.len(),
                );
            }
            FileSend::NotNegotiated => {
                return self.refuse_selection(
                    "the peer did not negotiate file support, so nothing is built or offered",
                    selection.len(),
                );
            }
            FileSend::Denied => {
                return self
                    .refuse_selection("this peer holds no clipboard-send grant", selection.len());
            }
        }
        // The one bound that can be judged without touching a disk: the
        // top-level selection alone already packs more entries than one
        // item may carry. The builder enforces the rest during the walk,
        // where the subdirectories become visible.
        if selection.len() > MAX_CLIPBOARD_FILE_ENTRIES as usize {
            return self.refuse_selection(
                "the selection packs more entries than one item may carry",
                selection.len(),
            );
        }

        let mut actions = self.abandon_build("superseded by a newer local copy");
        let id = Uuid::new_v4();
        let deadline = self.arm_timeout(TransferScope::Build);
        self.building = Some(PendingBuild {
            id,
            started: Instant::now(),
        });
        tracing::debug!(
            clipboard_id = %id,
            entry_count = selection.len(),
            "packing a local file selection"
        );
        actions.push(Action::BuildFileBlob { id, selection });
        actions.push(deadline);
        actions
    }

    /// Refuse a selection here and now, observably (FR-3.6): a warning
    /// naming the gate that closed and a counter, never a silent nothing
    /// and never the paths themselves.
    fn refuse_selection(&mut self, why: &str, entry_count: usize) -> Vec<Action> {
        self.record(Metrics::record_file_send_refused);
        tracing::warn!(entry_count, reason = why, "local file selection not sent");
        Vec::new()
    }

    /// Whether any path in a selection resolves inside the spool root.
    ///
    /// **Any** is the rule, not *all*: one clipboard item is one blob, so
    /// a selection that is partly ours cannot be sent minus those entries
    /// without sending something the user did not select.
    ///
    /// Deliberately conservative about what it cannot judge. A relative
    /// path, or one containing a `..` component, cannot be compared
    /// against a root without resolving it — which is filesystem work the
    /// engine does not do and, on the spool, work F15 forbids outright —
    /// so such a path is treated as *possibly* ours and the selection is
    /// not staged. A shell `CF_HDROP` carries absolute, normalized paths,
    /// so this costs nothing real, and the direction of the concession is
    /// the safe one: a copy that does not synchronize, rather than a loop
    /// on the largest payload type in the system.
    fn selection_is_ours(&self, selection: &[PathBuf]) -> bool {
        let Some(root) = self.spool_root.as_deref() else {
            return false; // no spool: nothing of ours is on disk to copy
        };
        selection.iter().any(|path| inside_spool(root, path))
    }

    /// Drop a build in flight, returning the action that releases
    /// whatever the driver may already have produced for it.
    ///
    /// The release is emitted even though the blob may not exist yet: the
    /// driver's answer can be in flight at this exact moment, and an
    /// idempotent release is how that race is closed rather than reasoned
    /// about.
    fn abandon_build(&mut self, why: &str) -> Vec<Action> {
        let Some(build) = self.building.take() else {
            return Vec::new();
        };
        self.record(Metrics::record_file_send_refused);
        tracing::debug!(
            clipboard_id = %build.id,
            elapsed_ms = elapsed_ms(build.started),
            reason = why,
            "local file selection abandoned before it was offered"
        );
        vec![Action::ReleaseFileBlob { id: build.id }]
    }

    /// The driver packed the selection — or refused it (ADR 0015).
    ///
    /// This is where a file item finally becomes an ordinary clipboard
    /// item: it has a length, a hash and a name, so it goes through the
    /// same dedup, the same loop guard and the same offered transaction
    /// every other type does. Everything before this point was about
    /// producing those three numbers.
    pub fn on_file_blob_built(
        &mut self,
        id: Uuid,
        outcome: Result<BuiltBlob, FileBlobRefusal>,
    ) -> Vec<Action> {
        let Some(build) = self.building.take_if(|build| build.id == id) else {
            // Superseded, abandoned, or timed out while it was building.
            // The blob is still handed back — this is the race
            // `abandon_build` cannot resolve on its own, and the artifact
            // is this machine's own disk.
            tracing::debug!(clipboard_id = %id, "blob for no pending selection; releasing it");
            return vec![Action::ReleaseFileBlob { id }];
        };
        let built_ms = elapsed_ms(build.started);
        let blob = match outcome {
            Ok(blob) => blob,
            Err(refusal) => {
                // The refusal names the fault and never the data
                // (SECURITY.md invariant 6): no path, no file name, no
                // contents.
                self.record(Metrics::record_file_send_refused);
                tracing::warn!(
                    clipboard_id = %id,
                    elapsed_ms = built_ms,
                    error = %refusal,
                    "local file selection refused; nothing offered"
                );
                return Vec::new();
            }
        };
        // Reject, never repair (ADR 0015): a name the user chose that
        // cannot conform refuses the item rather than travelling as
        // something they did not pick. A derived name falls back instead,
        // which `wire_file_name` decides — one validator for the one
        // string of a file transfer a shell ever sees.
        let file_name = match wire_file_name(&blob.proposed_name, blob.naming) {
            Ok(name) => name,
            Err(error) => {
                self.record(Metrics::record_file_send_refused);
                tracing::warn!(
                    clipboard_id = %id,
                    error = %error,
                    "local file selection has no name that can travel; nothing offered"
                );
                return vec![Action::ReleaseFileBlob { id }];
            }
        };
        // Both bounds re-checked here rather than trusted from the
        // builder: this is the last point before an offer is minted, and
        // the offer's own encoder would otherwise be the first thing to
        // notice (NFR-1 — validate before the wire, not at it).
        let max = ContentType::File.max_content_bytes();
        if blob.content_length == 0 || blob.content_length > max {
            self.record(Metrics::record_file_send_refused);
            tracing::warn!(
                clipboard_id = %id,
                byte_count = blob.content_length,
                max,
                "packed file selection is empty or over the protocol maximum; not synchronized"
            );
            return vec![Action::ReleaseFileBlob { id }];
        }

        // Loop prevention, layer 3 (ADR 0015): close to inert on Windows,
        // where a delivered file is never read back as bytes, and kept
        // because a platform whose fallback delivers real files would put
        // it straight back in the firing line.
        if self.applied_hashes.contains(&blob.content_hash) {
            self.current_local_hash = Some(blob.content_hash);
            self.record(Metrics::record_clipboard_loop_suppressed);
            return vec![Action::ReleaseFileBlob { id }];
        }
        if self.current_local_hash == Some(blob.content_hash) {
            tracing::debug!(clipboard_id = %id, "packed selection is unchanged content; not re-sent");
            return vec![Action::ReleaseFileBlob { id }];
        }
        self.current_local_hash = Some(blob.content_hash);

        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let meta = ClipboardMeta {
            id,
            origin: self.origin,
            sequence,
            content_type: ContentType::File,
            content_length: blob.content_length,
            content_hash: blob.content_hash,
        };
        let descriptor = FileDescriptor {
            file_name,
            archived: blob.archived,
            entry_count: blob.entry_count,
            original_bytes: blob.original_bytes,
        };
        // The name is logged; the contents never are. A file name is user
        // data and the receiving side already logs the offered one, so the
        // sent one is no new disclosure (SECURITY.md invariant 6).
        tracing::info!(
            clipboard_id = %id,
            file_name = %descriptor.file_name,
            byte_count = blob.content_length,
            entry_count = descriptor.entry_count,
            archived = descriptor.archived,
            elapsed_ms = built_ms,
            "offering a packed file selection"
        );
        self.record(|m| m.record_file_sent(blob.content_length));
        self.start_outbound_body(meta, OutboundBody::Blob, Some(descriptor))
    }

    /// A chunk of the blob could not be read back (ADR 0015).
    ///
    /// The transaction ends here: half an item is never sent as if it
    /// were the item, and the peer's own deadline closes its side. The
    /// blob is released, which is also what removes the artifact that
    /// could not be read.
    pub fn on_file_read_failed(&mut self, id: Uuid) -> Vec<Action> {
        let Some(outbound) = self.outbound.take() else {
            return vec![Action::ReleaseFileBlob { id }];
        };
        if outbound.meta().id != id {
            self.outbound = Some(outbound);
            return vec![Action::ReleaseFileBlob { id }];
        }
        self.record(Metrics::record_file_send_failed);
        tracing::warn!(
            clipboard_id = %id,
            "the packed file selection could not be read back; abandoning the transfer"
        );
        outbound.release().into_iter().collect()
    }

    /// Verified files resting in the spool, oldest first.
    ///
    /// The receiving half ends here for now: an entry is spooled and
    /// registered, and the platform half that offers it to the OS
    /// clipboard as a virtual file list is the next slice of ADR 0015.
    #[must_use]
    pub fn spooled_files(&self) -> impl ExactSizeIterator<Item = &SpooledFile> {
        self.spooled.iter().map(|(file, _)| file)
    }

    /// The spool answered [`Action::AdmitFile`]: the offer can now be
    /// accepted or declined with a reason that is actually true.
    pub fn on_file_admitted(&mut self, id: Uuid, outcome: Result<(), FileRefusal>) -> Vec<Action> {
        if !self
            .file
            .as_ref()
            .is_some_and(|transfer| transfer.id() == id && transfer.state == FileState::Admitting)
        {
            tracing::debug!(clipboard_id = %id, "admission result for no pending file transfer; ignoring");
            return Vec::new();
        }
        match outcome {
            Ok(()) => {
                if let Some(transfer) = self.file.as_mut() {
                    transfer.state = FileState::Streaming;
                }
                vec![Action::Send(OutboundMessage::Accept(ClipboardAccept {
                    id,
                }))]
            }
            Err(refusal) => {
                let Some(transfer) = self.file.take() else {
                    return Vec::new();
                };
                self.remember_transfer(id);
                self.record(Metrics::record_file_declined);
                let reason = match refusal {
                    FileRefusal::InsufficientSpace => DeclineReason::InsufficientSpace,
                    // A spool that could not be opened may work for the
                    // next item; it is not a statement about this one.
                    FileRefusal::Storage => DeclineReason::NotReady,
                };
                tracing::warn!(
                    clipboard_id = %id,
                    byte_count = transfer.stream.meta().content_length,
                    reason = ?reason,
                    "declining a file offer the spool would not admit"
                );
                decline(id, reason)
            }
        }
    }

    /// One chunk reached the spool. Only the last one has anything left
    /// to do: promote the partial, now that its final bytes are durable.
    pub fn on_file_chunk_written(&mut self, id: Uuid) -> Vec<Action> {
        let Some(transfer) = self.file.as_mut() else {
            return Vec::new();
        };
        if transfer.id() != id || transfer.state != FileState::Verified {
            return Vec::new();
        }
        transfer.state = FileState::Committing;
        vec![Action::CommitFile {
            id,
            from: transfer.part_name(),
            to: transfer.entry_name(),
        }]
    }

    /// A chunk did not reach the spool. Local failure, not peer
    /// misbehaviour: the partial goes and the origin is told the truth.
    pub fn on_file_write_failed(&mut self, id: Uuid) -> Vec<Action> {
        if self.file.as_ref().is_none_or(|t| t.id() != id) {
            return Vec::new();
        }
        self.abort_file("the spool write failed", true)
    }

    /// The partial was promoted — or was not. This is where a peer file
    /// becomes something this machine holds.
    pub fn on_file_committed(&mut self, id: Uuid, stored: bool) -> Vec<Action> {
        if !self
            .file
            .as_ref()
            .is_some_and(|transfer| transfer.id() == id && transfer.state == FileState::Committing)
        {
            return Vec::new();
        }
        if !stored {
            return self.abort_file("the verified partial could not be registered", true);
        }
        let Some(transfer) = self.file.take() else {
            return Vec::new();
        };
        let meta = transfer.stream.meta();
        self.remember_transfer(id);
        // Layer three of ADR 0015's loop prevention, and close to inert on
        // Windows by design: a virtual file list is never read back as
        // bytes, so no hash is ever computed for it to match. It costs one
        // insert and earns its place for a platform where delivered
        // content *is* re-read as ordinary bytes — a drop-folder fallback
        // would put this guard straight back in the firing line.
        self.remember_applied(meta.content_hash);
        let spooled = SpooledFile {
            entry: transfer.entry_name(),
            descriptor: transfer.descriptor,
            byte_len: meta.content_length,
            content_hash: meta.content_hash,
        };
        tracing::debug!(
            clipboard_id = %id,
            byte_count = spooled.byte_len,
            spool_entry = %spooled.entry,
            elapsed_ms = elapsed_ms(transfer.started),
            "peer file verified and spooled; offering it to the clipboard"
        );
        // No verdict yet. The bytes are on disk under their final name,
        // but a file nobody can paste is not a delivery — the origin hears
        // `Stored` when the offer reaches the clipboard, and
        // `StorageFailed` if it never does.
        self.offering = Some(PendingOffer {
            id,
            file: spooled.clone(),
            attempts_made: 1,
            started: transfer.started,
        });
        vec![Action::OfferFile { id, file: spooled }]
    }

    /// The clipboard took our virtual file list — or would not.
    ///
    /// Success is where a delivery becomes real: the entry is registered
    /// against the spool budget, the origin's transaction closes, and the
    /// item stays pasteable until the clipboard moves on. Failure deletes
    /// the entry, because one nothing advertises is peer bytes resting on
    /// disk for no reason at all.
    pub fn on_file_offered(&mut self, id: Uuid, result: Result<(), WriteFailure>) -> Vec<Action> {
        let Some(pending) = self.offering.take() else {
            tracing::debug!(clipboard_id = %id, "offer result for no pending offer; ignoring");
            return Vec::new();
        };
        if pending.id != id {
            self.offering = Some(pending);
            return Vec::new();
        }
        match result {
            Ok(()) => self.registered(pending),
            // Offering takes the machine-global clipboard lock like any
            // other write, so it meets the same contention and gets the
            // same bounded retry (FR-3.4).
            Err(WriteFailure::Busy) if pending.attempts_made < self.config.retry.max_attempts => {
                let delay = self.config.retry.delay;
                tracing::debug!(
                    clipboard_id = %id,
                    attempt_count = pending.attempts_made,
                    "clipboard busy while offering a file; retry scheduled"
                );
                self.offering = Some(pending);
                vec![Action::ScheduleRetry { id, delay }]
            }
            Err(failure) => {
                self.record(Metrics::record_file_failed);
                tracing::warn!(
                    clipboard_id = %id,
                    spool_entry = %pending.file.entry,
                    attempt_count = pending.attempts_made,
                    failure = ?failure,
                    result = "storage_failed",
                    "a verified file could not be offered for paste; deleting the entry"
                );
                vec![
                    Action::EvictSpoolEntry {
                        entry: pending.file.entry,
                    },
                    Action::Send(OutboundMessage::Applied(ClipboardApplied {
                        id,
                        result: ApplyResult::StorageFailed,
                    })),
                ]
            }
        }
    }

    /// An offered entry becomes a delivery: counted, acknowledged, and
    /// from here on subject to the lifetime rule.
    fn registered(&mut self, pending: PendingOffer) -> Vec<Action> {
        self.record(|m| m.record_file_stored(pending.file.byte_len));
        tracing::info!(
            clipboard_id = %pending.id,
            byte_count = pending.file.byte_len,
            spool_entry = %pending.file.entry,
            archived = pending.file.descriptor.archived,
            entry_count = pending.file.descriptor.entry_count,
            attempt_count = pending.attempts_made,
            latency_ms = elapsed_ms(pending.started),
            result = "stored",
            "peer file spooled and offered for paste"
        );
        // The name is user data (SECURITY.md invariant 6): debug only,
        // never the info line, and never the content at any level.
        tracing::debug!(
            clipboard_id = %pending.id,
            file_name = %pending.file.descriptor.file_name,
            "offered file name"
        );
        self.offered = Some(pending.file.entry.clone());
        self.spooled.push_back((pending.file, Instant::now()));
        vec![
            Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: pending.id,
                result: ApplyResult::Stored,
            })),
            // The age backstop, armed per registration: the real rule is
            // the clipboard moving on, and this is the floor-sweeper for a
            // run that never observes it.
            Action::ScheduleSpoolSweep {
                delay: self.config.spool_sweep_ttl,
            },
        ]
    }

    /// The clipboard moved on to something that is not ours — ADR 0015's
    /// entry-lifetime rule, and the whole of it.
    ///
    /// This is why there is no age-based expiry as the design first
    /// proposed: an entry can only be collected once the thing it backs is
    /// no longer on offer, at which point it could not have been pasted
    /// anyway. A TTL is the only bound that can delete something the user
    /// was still planning to paste.
    pub fn on_clipboard_moved_on(&mut self) -> Vec<Action> {
        if self.spooled.is_empty() {
            return Vec::new();
        }
        self.offered = None;
        self.spooled
            .drain(..)
            .map(|(file, _)| {
                tracing::debug!(
                    spool_entry = %file.entry,
                    byte_count = file.byte_len,
                    "the clipboard moved on; collecting the entry behind it"
                );
                Action::EvictSpoolEntry { entry: file.entry }
            })
            .collect()
    }

    /// The age backstop came due (`SPOOL_SWEEP_TTL`).
    ///
    /// Only entries genuinely past the backstop go, so a timer armed by an
    /// earlier delivery cannot collect a newer one, and an entry still on
    /// the clipboard has its promise withdrawn before its bytes vanish.
    pub fn on_spool_sweep_due(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        while let Some((file, registered)) = self.spooled.front() {
            if registered.elapsed() < self.config.spool_sweep_ttl {
                break; // oldest first, so everything behind it is younger
            }
            tracing::warn!(
                spool_entry = %file.entry,
                byte_count = file.byte_len,
                age_hours = registered.elapsed().as_secs() / 3600,
                "collecting a spool entry on age: the clipboard was never observed to move on"
            );
            if self.offered.as_deref() == Some(file.entry.as_str()) {
                self.offered = None;
                actions.push(Action::WithdrawFileOffer);
            }
            let entry = file.entry.clone();
            self.spooled.pop_front();
            actions.push(Action::EvictSpoolEntry { entry });
        }
        actions
    }

    /// A decoded clipboard message arrived from the peer.
    pub fn on_peer_message(&mut self, message: InboundMessage) -> Vec<Action> {
        match message {
            InboundMessage::Offer(offer) => self.on_peer_offer(&offer),
            InboundMessage::Accept(accept) => self.on_peer_accept(accept.id),
            InboundMessage::Decline(decline) => self.on_peer_decline(&decline),
            InboundMessage::Data(data) => self.on_peer_data(data),
            InboundMessage::Chunk(chunk) => self.on_peer_chunk(&chunk),
            InboundMessage::Applied(applied) => self.on_peer_applied(&applied),
        }
    }

    /// The driver finished (or failed) a clipboard write.
    pub fn on_write_result(&mut self, id: Uuid, result: Result<(), WriteFailure>) -> Vec<Action> {
        // Take-then-restore: no panic path exists (NFR-1 discipline).
        let Some(mut pending) = self.pending_write.take() else {
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
                    // Zero on the overwhelmingly common path. Non-zero is
                    // the parked phase reporting that it earned its keep:
                    // an item this long under contention is one the old
                    // fixed budget would have dropped.
                    parked_ms = pending.parked_since.map_or(0, elapsed_ms),
                    result = "applied",
                    "clipboard item installed"
                );
                vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                    id,
                    result: ApplyResult::Applied,
                }))]
            }
            Err(failure) => {
                if failure == WriteFailure::Busy
                    && let Some(retry) = self.schedule_install_retry(id, &mut pending)
                {
                    self.pending_write = Some(pending);
                    return vec![retry];
                }
                // A type this destination cannot represent is a statement
                // about the *content*, not about the clipboard's
                // availability, and the origin acts on the two
                // differently: one will never work here, the other might
                // work on the next copy (NFR-3).
                let verdict = match failure {
                    WriteFailure::UnsupportedType => ApplyResult::ContentRejected,
                    WriteFailure::Busy | WriteFailure::Unavailable => {
                        self.record(Metrics::record_clipboard_install_failed);
                        ApplyResult::ClipboardUnavailable
                    }
                };
                tracing::warn!(
                    clipboard_id = %pending.meta.id,
                    origin_peer = %pending.meta.origin,
                    content_type = ?pending.meta.content_type,
                    attempt_count = pending.attempts_made,
                    parked_ms = pending.parked_since.map_or(0, elapsed_ms),
                    result = ?verdict,
                    "clipboard item could not be installed"
                );
                vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
                    id,
                    result: verdict,
                }))]
            }
        }
    }

    /// A scheduled retry came due — for a clipboard write, or for the
    /// offer of a verified file, which contends for the same lock.
    pub fn on_retry_due(&mut self, id: Uuid) -> Vec<Action> {
        if let Some(pending) = self.offering.as_mut()
            && pending.id == id
        {
            pending.attempts_made += 1;
            return vec![Action::OfferFile {
                id,
                file: pending.file.clone(),
            }];
        }
        let Some(pending) = self.pending_write.as_mut() else {
            return Vec::new(); // superseded meanwhile
        };
        if pending.meta.id != id {
            return Vec::new();
        }
        if !pending.retry_armed {
            // A local change notification already revived this install; the
            // timer that armed it has nothing left to do (see
            // `PendingWrite::retry_armed`).
            return Vec::new();
        }
        pending.retry_armed = false;
        pending.attempts_made += 1;
        vec![Action::WriteClipboard {
            id,
            content: Arc::clone(&pending.content),
        }]
    }

    /// Decide what a `Busy` install does next: retry fast, park, keep
    /// trying while parked, or finally give up (`None`).
    ///
    /// The whole two-phase policy of ADR 0005's 2026-09-01 addendum is
    /// here, and it is a pure decision — the driver owns every clock.
    fn schedule_install_retry(&self, id: Uuid, pending: &mut PendingWrite) -> Option<Action> {
        let policy = &self.config.retry;
        let Some(parked_since) = pending.parked_since else {
            if pending.attempts_made < policy.max_attempts {
                tracing::debug!(
                    clipboard_id = %id,
                    attempt_count = pending.attempts_made,
                    "clipboard busy; retry scheduled"
                );
                pending.retry_armed = true;
                return Some(Action::ScheduleRetry {
                    id,
                    delay: policy.delay,
                });
            }
            if policy.park_budget.is_zero() {
                return None; // parked phase configured off (tests)
            }
            // The transition, logged exactly once. Everything after this
            // is at debug, because a parked install that logged per
            // attempt would turn one contended second into twenty lines
            // and bury the outcome the operator actually wants.
            pending.parked_since = Some(Instant::now());
            pending.retry_armed = true;
            self.record(Metrics::record_clipboard_install_parked);
            tracing::warn!(
                clipboard_id = %id,
                origin_peer = %pending.meta.origin,
                attempt_count = pending.attempts_made,
                park_budget_ms = duration_ms(policy.park_budget),
                park_delay_ms = duration_ms(policy.park_delay),
                "clipboard still busy after the fast retry budget; parking the install \
                 and retrying on every change notification until the budget runs out"
            );
            return Some(Action::ScheduleRetry {
                id,
                delay: policy.park_delay,
            });
        };
        if parked_since.elapsed() >= policy.park_budget {
            return None; // bounded, as ADR 0005 requires: the verdict travels
        }
        tracing::debug!(
            clipboard_id = %id,
            attempt_count = pending.attempts_made,
            parked_ms = elapsed_ms(parked_since),
            "clipboard still busy; parked install rescheduled"
        );
        pending.retry_armed = true;
        Some(Action::ScheduleRetry {
            id,
            delay: policy.park_delay,
        })
    }

    /// A local change notification is the parked phase's primary revival:
    /// the clipboard demonstrably just moved, which is the best evidence
    /// available that whoever was holding it has let go.
    ///
    /// Only a *parked* install is revived this way. One inside the fast
    /// budget already has a 200 ms timer running, and a notification
    /// arriving in that window would only take a turn the timer was about
    /// to take.
    fn revive_parked_write(&mut self) -> Vec<Action> {
        let Some(pending) = self.pending_write.as_mut() else {
            return Vec::new();
        };
        let Some(parked_since) = pending.parked_since else {
            return Vec::new();
        };
        if !pending.retry_armed {
            return Vec::new(); // an attempt is already outstanding
        }
        pending.retry_armed = false;
        pending.attempts_made += 1;
        tracing::debug!(
            clipboard_id = %pending.meta.id,
            attempt_count = pending.attempts_made,
            parked_ms = elapsed_ms(parked_since),
            "the clipboard changed; retrying the parked install now"
        );
        vec![Action::WriteClipboard {
            id: pending.meta.id,
            content: Arc::clone(&pending.content),
        }]
    }

    /// Close a pending install that something newer has replaced, telling
    /// the origin the truth.
    ///
    /// `Superseded` is the verdict the coalescing driver and the conflict
    /// rule already use for "a newer item won", reused rather than
    /// invented. Sending it at all is new: this path used to drop the
    /// write with a debug line and no verdict, which was survivable while
    /// the write lived for 800 ms and is not once it can live for twenty
    /// seconds — ADR 0005 requires every transaction to end in a typed
    /// verdict within a bounded time.
    /// [`Self::supersede_pending_write`], but only for an install that has
    /// **parked**.
    ///
    /// The distinction is the whole reason the fast phase is unchanged: an
    /// install inside the 800 ms fast budget is not a window a user copies
    /// into and then waits, so nothing that happens there is evidence of a
    /// race with them. Twenty seconds is.
    fn supersede_parked_write(&mut self, by: &str) -> Vec<Action> {
        if self
            .pending_write
            .as_ref()
            .is_some_and(|pending| pending.parked_since.is_some())
        {
            return self.supersede_pending_write(by);
        }
        Vec::new()
    }

    /// [`Self::supersede_pending_write`], but only when the install being
    /// replaced belongs to a *different* transaction than `incoming`.
    ///
    /// A peer that repeats a `ClipboardData` frame — a retransmit, a
    /// duplicate delivery — arrives bearing the id already installing.
    /// That is not a newer item, and answering it `Superseded` would draw
    /// **two verdicts for one transaction**: the supersession now and the
    /// install's own verdict later, for the same id. One transaction, one
    /// verdict is what the origin's state machine is built on (ADR 0005).
    fn supersede_pending_write_for(&mut self, incoming: Uuid, by: &str) -> Vec<Action> {
        if self
            .pending_write
            .as_ref()
            .is_some_and(|pending| pending.meta.id == incoming)
        {
            tracing::debug!(
                clipboard_id = %incoming,
                "a repeat of the item already installing; not superseding it with itself"
            );
            return Vec::new();
        }
        self.supersede_pending_write(by)
    }

    fn supersede_pending_write(&mut self, by: &str) -> Vec<Action> {
        let Some(superseded) = self.pending_write.take() else {
            return Vec::new();
        };
        self.record(Metrics::record_clipboard_superseded);
        tracing::debug!(
            clipboard_id = %superseded.meta.id,
            parked_ms = superseded.parked_since.map_or(0, elapsed_ms),
            superseded_by = by,
            "pending clipboard install superseded before it landed"
        );
        vec![Action::Send(OutboundMessage::Applied(ClipboardApplied {
            id: superseded.meta.id,
            result: ApplyResult::Superseded,
        }))]
    }

    /// A session (re)connected: re-announce our current item so the peers
    /// converge after any gap (reconnect-safe behavior; receiver-side
    /// dedup makes re-announcement cheap).
    ///
    /// Since 2026-09-01 this is also the delivery mechanism for every
    /// copy made while the pair was apart (ADR 0006 trigger 3, and its
    /// addendum): those copies minted no transaction, so the re-read
    /// below is the only thing that offers them — one item, the current
    /// one, however many copies the offline stretch contained.
    pub fn on_session_established(&mut self) -> Vec<Action> {
        self.live_sessions = self.live_sessions.saturating_add(1);
        self.offline_announced = false;
        self.outbound = None;
        self.expecting_data = None;
        self.reassembly = None;
        // A partial from before the gap belongs to a transaction no
        // longer in flight; it is deleted rather than adopted by the new
        // session (ADR 0015: nothing partially received is registered).
        let mut actions = self.abort_file("session re-established", false);
        // A selection being packed for the session that just ended has
        // nobody to be offered to; the artifact goes with it.
        actions.extend(self.abandon_build("session re-established"));
        self.recent_transfers.clear();
        // A fresh session gets a fresh violation budget: the counter
        // bounds one peer's misbehaviour on one connection, not a
        // process-lifetime grudge.
        self.violations = 0;
        // Ask the driver to re-read: the clipboard may have changed while
        // disconnected. The dedup hash is *kept* and a re-announce flag
        // set instead of clearing it, so the read still knows whether what
        // it finds is new — clearing it made every reconnect's re-read
        // look like a fresh local copy, which is a lie the parked install
        // paid for (ADR 0005, addendum 2026-09-01).
        self.reannounce_pending = true;
        actions.push(Action::ReadClipboard);
        actions
    }

    /// The session dropped: in-flight transaction state is meaningless
    /// now, an install that has not landed included.
    ///
    /// A pending install used to be left running on the grounds that its
    /// content was already here and the write would cost nothing. That was
    /// true while it lived for 800 ms and stopped being true when it could
    /// live for twenty seconds (ADR 0005, addendum 2026-09-01): a parked
    /// install can outlive the session, land during the *next* one, and
    /// overwrite whatever the user did in between, answering a peer that
    /// stopped waiting long ago. It is dropped without a verdict — there
    /// is nobody to tell — and without counting a failure, matching the
    /// outbound slot beside it, which is also cleared here uncounted. The
    /// content is not lost: the peer re-announces on reconnect (ADR 0006,
    /// trigger 3) and the read revival makes sure that re-announcement is
    /// heard.
    ///
    /// Every buffer the transaction machine can hold is released here —
    /// the retained outbound item and the inbound reassembly both, either
    /// of which can be `MAX_CLIPBOARD_IMAGE_BYTES` (ADR 0014). Nothing is
    /// sent: the peer is gone, and the deadline that would have answered
    /// it becomes moot with the session.
    pub fn on_session_lost(&mut self) -> Vec<Action> {
        // One session of possibly several. Only the last one out turns
        // transmission off (ADR 0006, addendum 2026-09-01): a machine
        // serving one peer while dialling another must keep offering to
        // the one that stayed.
        self.live_sessions = self.live_sessions.saturating_sub(1);
        let mut released_outbound = None;
        if let Some(outbound) = self.outbound.take() {
            tracing::debug!(
                clipboard_id = %outbound.meta().id,
                "outbound clipboard transaction abandoned: session lost"
            );
            released_outbound = outbound.release();
        }
        if let Some(meta) = self.expecting_data.take() {
            tracing::debug!(
                clipboard_id = %meta.id,
                "accepted inbound offer abandoned: session lost"
            );
        }
        // Info rather than debug: this is the one path that discards
        // content the peer successfully delivered, and an operator
        // reconciling "the item never appeared" against the logs needs to
        // find it at default levels.
        if let Some(dropped) = self.pending_write.take() {
            tracing::info!(
                clipboard_id = %dropped.meta.id,
                origin_peer = %dropped.meta.origin,
                attempt_count = dropped.attempts_made,
                parked_ms = dropped.parked_since.map_or(0, elapsed_ms),
                "inbound install dropped with its session"
            );
        }
        if let Some(reassembly) = self.reassembly.take() {
            tracing::debug!(
                clipboard_id = %reassembly.meta().id,
                byte_count = reassembly.received_bytes(),
                "inbound chunked transfer abandoned: session lost"
            );
        }
        // Not nothing, for a file: the partial is on disk, and the peer
        // being gone is exactly why it must not be left there. No verdict
        // travels — there is nobody to tell. The *sending* side has a
        // temporary artifact of its own, and the same argument applies to
        // it: released here, and released again by the outbound state
        // above if the transaction had got past the build.
        let mut actions = self.abandon_build("session lost");
        if let Some(release) = released_outbound {
            self.record(Metrics::record_file_send_failed);
            actions.push(release);
        }
        actions.extend(self.abort_file("session lost", false));
        actions
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
                // Every outbound state expires, not only the ones holding
                // an item buffer. `AwaitingApplied` costs almost no
                // memory, but a peer that never answers would occupy the
                // single outbound slot forever — and that slot is an input
                // to the conflict rule, so a zombie transaction would keep
                // deciding races against items minted long after it
                // (FR-3.5).
                self.record(Metrics::record_clipboard_abandoned);
                tracing::warn!(
                    clipboard_id = %outbound.meta().id,
                    byte_count = outbound.meta().content_length,
                    retained_content = outbound.retains_content(),
                    result = "abandoned",
                    "outbound clipboard transaction abandoned: no answer within the deadline"
                );
                // A file's bytes are not in this process, so the deadline
                // is not protecting memory here — it is protecting the
                // sender's own disk, which a stalled transaction would pin
                // for as long as the session lived (ADR 0015).
                if let Some(release) = outbound.release() {
                    self.record(Metrics::record_file_send_failed);
                    return vec![release];
                }
                Vec::new()
            }
            TransferScope::Inbound => {
                if generation != self.inbound_generation {
                    return Vec::new();
                }
                let mut actions = Vec::new();
                if let Some(meta) = self.expecting_data.take() {
                    self.record(Metrics::record_clipboard_abandoned);
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
                    self.record(Metrics::record_clipboard_abandoned);
                    actions.push(Action::Send(OutboundMessage::Applied(ClipboardApplied {
                        id: reassembly,
                        result: ApplyResult::ContentRejected,
                    })));
                }
                if self.file.is_some() {
                    self.record(Metrics::record_clipboard_abandoned);
                    actions.extend(self.abort_file("deadline", true));
                }
                actions
            }
            TransferScope::Build => {
                if generation != self.build_generation {
                    return Vec::new(); // a newer selection restarted the clock
                }
                if self.building.is_none() {
                    return Vec::new();
                }
                // A build that has not answered inside the deadline is
                // either wedged or working on something absurd; either way
                // the answer, if it ever comes, is released rather than
                // offered (`on_file_blob_built` sees no pending build).
                self.record(Metrics::record_clipboard_abandoned);
                tracing::warn!(
                    result = "abandoned",
                    "packing a local file selection did not finish within the deadline"
                );
                self.abandon_build("deadline")
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
            body,
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
                body,
                plan,
                next_index,
                started,
            });
            return Vec::new();
        }
        if next_index >= plan.chunk_count() {
            // Every chunk is out; the body is released here — the buffer
            // freed, or the driver's blob handed back so the sender's
            // temporary artifact is deleted — and only the verdict remains
            // outstanding.
            let released = Outbound::Streaming {
                meta,
                body,
                plan,
                next_index,
                started,
            }
            .release();
            self.outbound = Some(Outbound::AwaitingApplied { meta, started });
            return released.into_iter().collect();
        }
        let Some(action) = chunk_action(meta.id, &body, plan, next_index) else {
            // Unreachable: the plan was derived from this item's declared
            // length. Released rather than merely dropped, so an
            // impossible arithmetic fault still cannot pin a blob.
            tracing::error!(
                clipboard_id = %meta.id,
                chunk_index = next_index,
                "clipboard chunk slice out of range; abandoning the transfer"
            );
            self.record(Metrics::record_clipboard_abandoned);
            return release_of(&body, meta.id);
        };
        self.outbound = Some(Outbound::Streaming {
            meta,
            body,
            plan,
            next_index: next_index.saturating_add(1),
            started,
        });
        vec![action]
    }

    // --- internals ---

    fn start_outbound(&mut self, meta: ClipboardMeta, content: Vec<u8>) -> Vec<Action> {
        self.start_outbound_body(meta, OutboundBody::Bytes(content), None)
    }

    /// Start an outbound transaction whose bytes may live here or in the
    /// driver's blob.
    ///
    /// `descriptor` is the file half of the offer (ADR 0015) and is
    /// `Some` exactly when `body` is a blob — the protocol enforces the
    /// same rule in both directions, so a mismatch would not encode.
    fn start_outbound_body(
        &mut self,
        meta: ClipboardMeta,
        body: OutboundBody,
        descriptor: Option<FileDescriptor>,
    ) -> Vec<Action> {
        // Note what is *not* decided here: whether a parked install loses
        // to this item. That question needs to know whether the local
        // content is genuinely new, which only the read that produced it
        // can say (see `on_local_read`) — a re-announcement after a
        // reconnect reaches this function looking identical to a fresh
        // copy, and deciding it here got that case wrong.
        let mut superseded = Vec::new();
        if let Some(previous) = self.outbound.take() {
            tracing::debug!(
                clipboard_id = %previous.meta().id,
                "outbound clipboard transaction superseded by newer local copy"
            );
            if let Some(release) = previous.release() {
                self.record(Metrics::record_file_send_failed);
                superseded.push(release);
            }
        }
        self.record(Metrics::record_clipboard_sent);
        let started = Instant::now();
        // Chunked types have no inline flow and are offered at any size:
        // the offer round is where the receiver's already-have decline
        // makes a re-paste move zero bytes, and where it bounds its own
        // memory before megabytes arrive (ADR 0014).
        let offered = meta.content_type.is_chunked()
            || meta.content_length > CLIPBOARD_INLINE_MAX_BYTES as u64;
        // Armed for *every* outbound transaction, inline text included.
        // The buffer is only half the reason: the other half is that
        // `outbound` is the single slot the conflict rule reads, so a
        // transaction nobody ever answers would go on deciding races it
        // has no business in (FR-3.5). One deadline covers the whole
        // transaction — offer, stream and verdict — so accepting does not
        // restart the clock.
        let deadline = self.arm_timeout(TransferScope::Outbound);
        if !offered {
            let OutboundBody::Bytes(content) = body else {
                // Unreachable: a blob is a file, and a file is chunked.
                tracing::error!(clipboard_id = %meta.id, "inline flow for a blob item; abandoning");
                superseded.push(Action::ReleaseFileBlob { id: meta.id });
                return superseded;
            };
            self.outbound = Some(Outbound::AwaitingApplied { meta, started });
            superseded.push(Action::Send(OutboundMessage::Data(ClipboardData {
                meta,
                content,
            })));
            superseded.push(deadline);
            return superseded;
        }
        self.outbound = Some(Outbound::AwaitingAccept {
            meta,
            body,
            started,
        });
        superseded.push(Action::Send(OutboundMessage::Offer(ClipboardOffer {
            meta,
            // `Some` for a file and nothing else: the protocol rejects a
            // descriptor on any other type, and a file offer without one
            // (ADR 0015).
            descriptor,
        })));
        superseded.push(deadline);
        superseded
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
            TransferScope::Build => {
                self.build_generation = self.build_generation.wrapping_add(1);
                self.build_generation
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

    /// Remember a finished or abandoned transfer, at most once.
    ///
    /// The de-duplication is not tidiness. The same id can arrive here
    /// twice — a transfer abandoned and then re-offered under its original
    /// id, say — and without the check those repeats would evict the ring
    /// with copies of one value. A ring holding four of the same id
    /// remembers exactly one transfer, so the tail of a *different*
    /// superseded transfer would suddenly be chargeable as unsolicited:
    /// the peer's repetition, not its misbehaviour, would decide whether a
    /// benign race costs it violations.
    fn remember_transfer(&mut self, id: Uuid) {
        if self.recent_transfers.contains(&id) {
            return;
        }
        if self.recent_transfers.len() >= RECENT_TRANSFER_MEMORY {
            self.recent_transfers.pop_front();
        }
        self.recent_transfers.push_back(id);
    }

    fn on_peer_offer(&mut self, offer: &ClipboardOffer) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(reason) = self.conflict_verdict(offer.meta, &mut actions) {
            actions.push(Action::Send(OutboundMessage::Decline(ClipboardDecline {
                id: offer.meta.id,
                reason,
            })));
            return actions;
        }
        // No `AlreadyHave` for files (docs/PROTOCOL.md §5). The hash this
        // would compare against describes what is on the *clipboard*, and
        // a spooled entry is not that: claiming to hold a file already
        // would decline an offer this machine may no longer be able to
        // paste, which is a worse answer than moving the bytes again.
        if !offer.meta.content_type.needs_file_descriptor()
            && self.current_local_hash == Some(offer.meta.content_hash)
        {
            // Already holding identical content: a sync success with zero
            // payload bytes moved (ADR 0005) — and for a chunked item that
            // is the whole point of offering it, since a re-pasted snip
            // then costs one offer and one decline instead of megabytes.
            actions.push(Action::Send(OutboundMessage::Decline(ClipboardDecline {
                id: offer.meta.id,
                reason: DeclineReason::AlreadyHave,
            })));
            return actions;
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
        actions.extend(self.abort_file("superseded by a newer offer", false));

        if offer.meta.content_type.needs_file_descriptor() {
            actions.extend(self.on_file_offer(offer));
            return actions;
        }

        if offer.meta.content_type.is_chunked() {
            // The receiver's memory commitment is decided here and nowhere
            // else: `begin` validates the offered length against the
            // type's maximum *before* sizing the buffer from it (NFR-1),
            // and reports an allocation it cannot make rather than dying.
            match ChunkReassembly::begin(offer.meta) {
                Ok(reassembly) => {
                    self.reassembly = Some(reassembly);
                    actions.push(Action::Send(OutboundMessage::Accept(ClipboardAccept {
                        id: offer.meta.id,
                    })));
                    actions.push(self.arm_timeout(TransferScope::Inbound));
                    return actions;
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
                    actions.extend(decline(offer.meta.id, DeclineReason::NotReady));
                    return actions;
                }
            }
        }

        self.expecting_data = Some(offer.meta);
        actions.push(Action::Send(OutboundMessage::Accept(ClipboardAccept {
            id: offer.meta.id,
        })));
        actions.push(self.arm_timeout(TransferScope::Inbound));
        actions
    }

    /// A file offer (ADR 0015): permission, then room, then a partial to
    /// write into — and only then an answer.
    ///
    /// Every refusal is a typed decline naming which gate closed, because
    /// the origin acts on them differently and a silent drop is the
    /// failure NFR-3 forbids.
    fn on_file_offer(&mut self, offer: &ClipboardOffer) -> Vec<Action> {
        let meta = offer.meta;
        let Some(descriptor) = offer.descriptor.clone() else {
            // Unreachable past the parser — a file offer without a
            // descriptor is malformed and never decodes — so this is the
            // defensive half of that rule, answered rather than asserted.
            self.record(Metrics::record_file_declined);
            return decline(meta.id, DeclineReason::InvalidName);
        };
        match self.file_receive {
            FileReceive::Unsupported => {
                tracing::warn!(
                    clipboard_id = %meta.id,
                    "declining a file offer: this build has no spool to receive files into"
                );
                self.record(Metrics::record_file_declined);
                return decline(meta.id, DeclineReason::UnsupportedType);
            }
            FileReceive::Denied => {
                // Operator-visible, not debug: a peer offering files to a
                // machine that has not granted it is exactly the event the
                // permission exists to make visible (SECURITY.md).
                tracing::warn!(
                    clipboard_id = %meta.id,
                    origin_peer = %meta.origin,
                    byte_count = meta.content_length,
                    "declining a file offer: this peer has no file-receive grant \
                     (`crossover peers allow-files`)"
                );
                self.record(Metrics::record_file_declined);
                return decline(meta.id, DeclineReason::NotPermitted);
            }
            FileReceive::Allowed => {}
        }
        // The spool's own ceiling, distinct from the item's: an offer no
        // spool could ever hold is refused before any room is made for it.
        if meta.content_length > MAX_SPOOL_BYTES {
            tracing::warn!(
                clipboard_id = %meta.id,
                byte_count = meta.content_length,
                max = MAX_SPOOL_BYTES,
                "declining a file offer larger than the whole spool budget"
            );
            self.record(Metrics::record_file_declined);
            return decline(meta.id, DeclineReason::TooLarge);
        }
        let stream = match ChunkStream::begin(meta) {
            Ok(stream) => stream,
            Err(error) => {
                // Defensive: the offer decoded, so its meta already
                // validated. Answered rather than dropped all the same.
                tracing::warn!(
                    clipboard_id = %meta.id,
                    error = %error,
                    "declining a file offer that cannot be streamed"
                );
                self.record(Metrics::record_file_declined);
                return decline(meta.id, DeclineReason::NotReady);
            }
        };

        // Room is made *before* the partial is created, and the partial
        // counts against the budget from the moment it exists, so
        // MAX_SPOOL_BYTES is the true ceiling rather than the ceiling plus
        // one transfer (ADR 0015).
        let mut actions = self.make_room_for(meta.content_length);
        let transfer = FileTransfer {
            stream,
            descriptor,
            entry_id: Uuid::new_v4(),
            state: FileState::Admitting,
            started: Instant::now(),
        };
        let entry = transfer.part_name();
        tracing::debug!(
            clipboard_id = %meta.id,
            byte_count = meta.content_length,
            spool_entry = %entry,
            "admitting a file offer to the spool"
        );
        self.file = Some(transfer);
        actions.push(Action::AdmitFile {
            id: meta.id,
            entry,
            byte_len: meta.content_length,
        });
        // Armed now rather than on acceptance: an admission that never
        // comes back must cost a bounded amount of time too.
        actions.push(self.arm_timeout(TransferScope::Inbound));
        actions
    }

    /// Evict completed entries, oldest first, until this transfer fits
    /// inside both spool bounds (ADR 0015).
    ///
    /// Eviction is real rather than hypothetical: entries go before the
    /// transfer is admitted, not after it completes. Every removal is
    /// logged, because content leaving the spool is a diagnosable event
    /// and never a silent tidy-up (NFR-3).
    fn make_room_for(&mut self, needed: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        while !self.spooled.is_empty()
            && (self.spooled.len() >= MAX_SPOOL_ENTRIES
                || self.spooled_bytes().saturating_add(needed) > MAX_SPOOL_BYTES)
        {
            let Some((evicted, _)) = self.spooled.pop_front() else {
                break;
            };
            tracing::info!(
                spool_entry = %evicted.entry,
                byte_count = evicted.byte_len,
                needed,
                "evicting the oldest spool entry to make room for an incoming file"
            );
            // The oldest entry is normally long off the clipboard, but a
            // budget that has to evict the *offered* one must take the
            // promise with it: a virtual file list whose bytes are gone
            // fails at paste time, in the shell, with nothing from us.
            if self.offered.as_deref() == Some(evicted.entry.as_str()) {
                self.offered = None;
                actions.push(Action::WithdrawFileOffer);
            }
            actions.push(Action::EvictSpoolEntry {
                entry: evicted.entry,
            });
        }
        actions
    }

    /// What the registered entries occupy. The in-flight partial is not
    /// counted here because there is never one when this is called: a
    /// transfer is admitted only when no other holds the slot.
    fn spooled_bytes(&self) -> u64 {
        self.spooled
            .iter()
            .fold(0u64, |total, (file, _)| total.saturating_add(file.byte_len))
    }

    /// Abandon the in-flight file transfer: delete the partial, register
    /// nothing, and (when the peer is still owed an answer) say so.
    ///
    /// The one exit every failure takes — a bad chunk, a failed write, a
    /// deadline, a lost session, a superseding offer — so "nothing
    /// partially received is ever registered" is a property of one
    /// function rather than of five call sites remembering to.
    fn abort_file(&mut self, why: &str, answer: bool) -> Vec<Action> {
        let Some(transfer) = self.file.take() else {
            return Vec::new();
        };
        let id = transfer.id();
        self.remember_transfer(id);
        self.record(Metrics::record_file_failed);
        tracing::warn!(
            clipboard_id = %id,
            byte_count = transfer.stream.received_bytes(),
            declared_bytes = transfer.stream.meta().content_length,
            spool_entry = %transfer.part_name(),
            reason = why,
            result = "abandoned",
            "file transfer abandoned; the partial is deleted and nothing is registered"
        );
        let mut actions = vec![Action::AbortFile {
            id,
            entry: transfer.part_name(),
        }];
        if answer {
            actions.push(Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id,
                result: ApplyResult::StorageFailed,
            })));
        }
        actions
    }

    /// Route a chunk into the in-flight file transfer, judging it before
    /// any of it is written.
    fn accept_file_chunk(&mut self, chunk: &ClipboardChunk) -> Vec<Action> {
        let Some(transfer) = self.file.as_mut() else {
            return Vec::new();
        };
        let id = transfer.id();
        if transfer.state != FileState::Streaming {
            // Chunks for a transfer this side has not accepted, or has
            // already finished streaming. Not the benign in-flight tail
            // `recent_transfers` covers: this id is live, and the peer is
            // sending ahead of its own answer.
            let mut actions = self.abort_file("chunks outside the accepted window", true);
            actions
                .extend(self.record_violation("clipboard file chunk outside the accepted window"));
            return actions;
        }
        match transfer.stream.accept(chunk) {
            Ok(StreamOutcome::More) => vec![Action::WriteFileChunk {
                id,
                payload: chunk.payload.clone(),
            }],
            Ok(StreamOutcome::Final) => {
                // Verified, but not yet complete: the entry is promoted
                // when these last bytes are actually in the spool.
                transfer.state = FileState::Verified;
                vec![Action::WriteFileChunk {
                    id,
                    payload: chunk.payload.clone(),
                }]
            }
            Err(error) => {
                tracing::debug!(
                    clipboard_id = %id,
                    chunk_index = chunk.index,
                    error = %error,
                    "malformed file chunk; abandoning the transfer"
                );
                let mut actions = self.abort_file("malformed chunk", true);
                actions.extend(self.record_violation("malformed clipboard file chunk"));
                actions
            }
        }
    }

    fn on_peer_accept(&mut self, id: Uuid) -> Vec<Action> {
        match self.outbound.take() {
            Some(Outbound::AwaitingAccept {
                meta,
                body,
                started,
            }) if meta.id == id => {
                if !meta.content_type.is_chunked() {
                    let OutboundBody::Bytes(content) = body else {
                        tracing::error!(clipboard_id = %meta.id, "inline accept for a blob item; abandoning");
                        return vec![Action::ReleaseFileBlob { id: meta.id }];
                    };
                    self.outbound = Some(Outbound::AwaitingApplied { meta, started });
                    return vec![Action::Send(OutboundMessage::Data(ClipboardData {
                        meta,
                        content,
                    }))];
                }
                // The split is the same arithmetic the receiver derives
                // from the offered length and chunk 0 — one implementation,
                // both sides (ADR 0014), and one for both bodies: a blob
                // is chunked by the same plan, only read from elsewhere.
                let Ok(plan) = ChunkPlan::for_length(meta.content_length) else {
                    tracing::error!(
                        clipboard_id = %meta.id,
                        byte_count = meta.content_length,
                        "clipboard item cannot be split into chunks; abandoning"
                    );
                    return release_of(&body, meta.id);
                };
                let Some(first) = chunk_action(meta.id, &body, plan, 0) else {
                    tracing::error!(clipboard_id = %meta.id, "empty clipboard chunk plan; abandoning");
                    return release_of(&body, meta.id);
                };
                tracing::debug!(
                    clipboard_id = %meta.id,
                    byte_count = meta.content_length,
                    chunk_count = plan.chunk_count(),
                    "streaming an accepted chunked clipboard item"
                );
                self.outbound = Some(Outbound::Streaming {
                    meta,
                    body,
                    plan,
                    next_index: 1,
                    started,
                });
                vec![first]
            }
            other => {
                self.outbound = other; // restore whatever it was
                tracing::debug!(clipboard_id = %id, "stale or unknown accept; ignoring");
                Vec::new()
            }
        }
    }

    /// A decline closes an offer that is still **awaiting an answer** —
    /// and only that.
    ///
    /// The asymmetry with [`Self::on_peer_applied`], which does stop a
    /// stream in flight, is deliberate and worth stating because the race
    /// that raises the question is legal: chunk 0 leaves as soon as the
    /// accept arrives, so a decline the peer sent for some *other* reason
    /// can cross it on the wire (docs/PROTOCOL.md §4 orders messages
    /// within a class, not between the two directions of one).
    ///
    /// A decline reaching a live stream therefore means the peer answered
    /// the same offer twice. Stopping the stream on it would let one
    /// stray or duplicated frame cancel a transfer the peer has already
    /// accepted and is actively reassembling — trading a real transfer for
    /// a message that contradicts the peer's own earlier answer. Ignoring
    /// it costs at most the rest of one bounded stream, which the receiver
    /// either completes (and acknowledges) or refuses per §7. `Applied` is
    /// different in kind: it is the *verdict*, the only message that ends
    /// a transaction, and a receiver that has rendered one has genuinely
    /// stopped reassembling — continuing to push chunks at it would be
    /// spending the wire on nobody.
    fn on_peer_decline(&mut self, decline: &ClipboardDecline) -> Vec<Action> {
        match self.outbound.take() {
            Some(Outbound::AwaitingAccept {
                meta,
                body,
                started,
            }) if meta.id == decline.id => {
                let latency_ms = elapsed_ms(started);
                self.record(|m| m.record_clipboard_latency(clamp_ms(latency_ms)));
                let outcome = match decline.reason {
                    // Success-shaped: the peer already has the content, or
                    // a newer item won the race.
                    DeclineReason::AlreadyHave | DeclineReason::Superseded => "converged",
                    DeclineReason::TooLarge
                    | DeclineReason::NotReady
                    | DeclineReason::UnsupportedType
                    | DeclineReason::NotPermitted
                    | DeclineReason::InvalidName
                    | DeclineReason::InsufficientSpace => "declined",
                };
                tracing::info!(
                    clipboard_id = %decline.id,
                    reason = ?decline.reason,
                    result = outcome,
                    latency_ms,
                    "clipboard offer resolved"
                );
                // A declined file moves **zero payload bytes**: the blob
                // is handed back here, before a chunk is ever read, which
                // is the whole reason files use the offered flow at any
                // size (ADR 0005, ADR 0015). `AlreadyHave` is the
                // success-shaped case of that — our receiver never sends
                // it for a file, but a peer's may, and dedup is a
                // delivery, not a failure.
                if matches!(body, OutboundBody::Blob) && outcome == "declined" {
                    self.record(Metrics::record_file_send_failed);
                }
                release_of(&body, decline.id)
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
        if self.file.as_ref().is_some_and(|f| f.id() == chunk.id) {
            return self.accept_file_chunk(chunk);
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
        let mut actions = Vec::new();
        if let Some(reason) = self.conflict_verdict(meta, &mut actions) {
            debug_assert_eq!(reason, DeclineReason::Superseded);
            self.record(Metrics::record_clipboard_superseded);
            actions.push(Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: meta.id,
                result: ApplyResult::Superseded,
            })));
            return actions;
        }

        // Loop/echo guard: identical content is a success without a write.
        //
        // An earlier install still pending must not survive this, and the
        // divergence if it does is silent and permanent: we tell the origin
        // this item is `Applied`, the older install then writes its own
        // content over the clipboard we just claimed agreement on, and its
        // own-write notification is loop-suppressed so nothing ever
        // notices. Both machines would believe they show this item while
        // one shows the other.
        if self.current_local_hash == Some(meta.content_hash) {
            actions.extend(
                self.supersede_pending_write_for(
                    meta.id,
                    "an inbound item already on the clipboard",
                ),
            );
            actions.push(Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: meta.id,
                result: ApplyResult::Applied,
            })));
            return actions;
        }

        // Wire validation guarantees UTF-8 for Utf8Text; defensive here.
        let Some(content) = from_wire(meta.content_type, bytes) else {
            actions.push(Action::Send(OutboundMessage::Applied(ClipboardApplied {
                id: meta.id,
                result: ApplyResult::ContentRejected,
            })));
            return actions;
        };

        actions.extend(self.supersede_pending_write_for(meta.id, "a newer inbound item"));
        let content = Arc::new(content);
        self.pending_write = Some(PendingWrite {
            meta,
            content: Arc::clone(&content),
            attempts_made: 1,
            parked_since: None,
            retry_armed: false,
        });
        actions.push(Action::WriteClipboard {
            id: meta.id,
            content,
        });
        actions
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
            Some(closing @ (Outbound::AwaitingApplied { .. } | Outbound::Streaming { .. }))
                if closing.meta().id == applied.id =>
            {
                let (meta, started) = (closing.meta(), closing.started());
                // A verdict ends the transaction, so a stream it cuts
                // short is also the last chance to hand a blob back.
                let released: Vec<Action> = closing.release().into_iter().collect();
                let outcome = match applied.result {
                    ApplyResult::Applied => "applied",
                    ApplyResult::Superseded => "superseded",
                    ApplyResult::ClipboardUnavailable => "clipboard_unavailable",
                    ApplyResult::ContentRejected => "content_rejected",
                    // File verdicts (ADR 0015): nothing produces them
                    // yet, and an unlabelled verdict would be a silent
                    // one, which NFR-3 forbids.
                    ApplyResult::Stored => "stored",
                    ApplyResult::StorageFailed => "storage_failed",
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
                if !released.is_empty() && !matches!(applied.result, ApplyResult::Stored) {
                    // Every file verdict but `Stored` is a delivery that
                    // did not happen, and FR-3.6 wants that counted rather
                    // than merely logged.
                    self.record(Metrics::record_file_send_failed);
                }
                released
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
    /// `released` collects the action that hands back a driver-held blob
    /// when the loser of the race is *ours* — the one exit path from an
    /// outbound transaction that is not reached by taking `outbound`
    /// somewhere the release is already written.
    fn conflict_verdict(
        &mut self,
        inbound: ClipboardMeta,
        released: &mut Vec<Action>,
    ) -> Option<DeclineReason> {
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
            if let Some(release) = self.outbound.take().and_then(|ours| ours.release()) {
                self.record(Metrics::record_file_send_failed);
                released.push(release);
            }
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

/// One typed refusal, as the single action it always is.
fn decline(id: Uuid, reason: DeclineReason) -> Vec<Action> {
    vec![Action::Send(OutboundMessage::Decline(ClipboardDecline {
        id,
        reason,
    }))]
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

/// The action that puts chunk `index` of an outbound item on the wire.
///
/// One function for both bodies, because the *decision* is identical and
/// only the source of the bytes differs: bytes the engine retains are
/// sliced here and travel as a ready [`ClipboardChunk`]; a blob's are
/// named by offset and length so the driver reads exactly that chunk out
/// of the open file when it sends it. The second form is what keeps the
/// sender O(chunk) rather than O(file) (ADR 0015), and it is the mirror
/// of the receiver's write-through.
///
/// `None` when the index is past the transfer or the buffer does not
/// reach — unreachable for a plan derived from the item's own declared
/// length, and a returned value rather than a panic (NFR-1).
fn chunk_action(id: Uuid, body: &OutboundBody, plan: ChunkPlan, index: u32) -> Option<Action> {
    match body {
        OutboundBody::Bytes(content) => Some(Action::Send(OutboundMessage::Chunk(chunk_at(
            id, content, plan, index,
        )?))),
        OutboundBody::Blob => {
            let len = plan.chunk_len(index)?;
            let offset = u64::from(index).checked_mul(u64::from(plan.chunk_bytes()))?;
            // The last byte must be inside the blob the offer declared.
            let end = offset.checked_add(u64::from(len))?;
            if end > plan.total_bytes() {
                return None;
            }
            Some(Action::SendFileChunk {
                id,
                index,
                offset,
                len,
            })
        }
    }
}

/// Whether `path` names something inside `root` — the sender-side half of
/// ADR 0015's loop prevention (SECURITY.md F13).
///
/// A pure comparison over path components: nothing is opened, nothing is
/// resolved, and the spool's handle-only boundary (F15) is untouched.
/// Components are matched case-insensitively, because that is what the
/// only filesystem this rule currently guards does, and matching
/// case-sensitively there would be a check that misses `C:\...\SPOOL`.
///
/// `true` — do not stage — is also the answer for anything that cannot be
/// judged without resolving it: a relative path, or one carrying a `..`
/// component. A shell `CF_HDROP` never produces either, so the concession
/// costs nothing real and the direction is the safe one.
fn inside_spool(root: &Path, path: &Path) -> bool {
    fn judgeable(path: &Path) -> Option<Vec<String>> {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::ParentDir => return None,
                Component::CurDir => {}
                Component::Prefix(prefix) => {
                    parts.push(prefix.as_os_str().to_string_lossy().to_lowercase());
                }
                // A separator on Windows and illegal in a name on Unix,
                // so the root marker cannot collide with a real component.
                Component::RootDir => parts.push("/".to_owned()),
                Component::Normal(part) => parts.push(part.to_string_lossy().to_lowercase()),
            }
        }
        Some(parts)
    }

    let (Some(root), Some(candidate)) = (judgeable(root), judgeable(path)) else {
        return true; // unjudgeable without resolving it: treat as ours
    };
    if !path.is_absolute() || root.is_empty() {
        return true;
    }
    candidate.len() >= root.len() && candidate[..root.len()] == root[..]
}

/// The release action for a body being dropped outside an [`Outbound`]
/// state, as a list so a caller can return it directly.
fn release_of(body: &OutboundBody, id: Uuid) -> Vec<Action> {
    match body {
        OutboundBody::Blob => vec![Action::ReleaseFileBlob { id }],
        OutboundBody::Bytes(_) => Vec::new(),
    }
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

/// Typed platform content → the wire's `(type, bytes)` pair, or `None` for
/// content this call never stages.
///
/// Image bytes move by value and untouched: no transcode, no compression,
/// no inspection — the hash and the length are all that is ever computed
/// over them (ADR 0014).
///
/// `FileList` returns `None`: a local file/folder selection is observable
/// (feature/133), but the engine does not yet stage it for transmission —
/// the archive builder and offer transaction are feature/135's job (ADR
/// 0015 "Sender side"). `on_local_read` is the only caller and already
/// returns early for `FileList` before reaching here, so this arm is
/// defense in depth, not a path production takes; `None` mirrors the shape
/// `on_local_read` already uses for an oversized or empty item, so the
/// outcome is the same either way.
fn into_wire(content: ClipboardContent) -> Option<(ContentType, Vec<u8>)> {
    Some(match content {
        ClipboardContent::Text(text) => (ContentType::Utf8Text, text.into_bytes()),
        ClipboardContent::Image { format, bytes } => {
            (ContentType::Image(wire_format(format)), bytes)
        }
        ClipboardContent::FileList(_) => return None,
    })
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
        // A file is not platform clipboard *content*: it is spooled and
        // then offered as a virtual file list (ADR 0015), which is a
        // different ClipboardProvider call and a different slice. Until
        // that exists this build never negotiates FILE_CLIPBOARD, so a
        // file item cannot arrive from a conforming peer — and one that
        // arrives anyway is refused here rather than mishandled.
        ContentType::File => return None,
    })
}

/// Milliseconds since `started`, saturating into `u64` for logging.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A configured duration as milliseconds, for log fields that report a
/// budget rather than a measurement.
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Narrow a millisecond duration to the `u32` the latency histogram
/// keeps, saturating rather than wrapping (a clipboard round trip past 49
/// days is a broken clock, not a real sample).
fn clamp_ms(ms: u64) -> u32 {
    u32::try_from(ms).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use uuid::Uuid;

    use crossover_platform::{BlobNaming, ClipboardContent, ClipboardImageFormat, FileBlobRefusal};
    use crossover_protocol::clipboard::{
        ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ClipboardAccept, ClipboardApplied, ClipboardChunk,
        ClipboardData, ClipboardDecline, ClipboardMeta, ClipboardOffer, ContentType, DeclineReason,
        FileDescriptor, ImageFormat, MAX_CHUNK_BYTES, chunk_content, content_hash,
    };

    use std::time::Duration;

    use super::{
        Action, BuiltBlob, ClipboardConfig, ClipboardEngine, FileReceive, FileRefusal, FileSend,
        InboundMessage, MAX_CONCURRENT_FILE_TRANSFERS, MAX_SPOOL_BYTES, MAX_SPOOL_ENTRIES,
        OutboundMessage, RetryPolicy, SpooledFile, TransferScope, WriteFailure,
    };
    use crate::metrics::Metrics;
    use crossover_protocol::clipboard::MAX_CLIPBOARD_FILE_ENTRIES;

    /// The one deadline the actions asked for, as `(scope, generation)`.
    fn timeout_of(actions: &[Action]) -> (TransferScope, u64) {
        actions
            .iter()
            .find_map(|action| match action {
                Action::ScheduleTransferTimeout {
                    scope, generation, ..
                } => Some((*scope, *generation)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a transfer deadline, got {actions:?}"))
    }

    fn engine(origin_fill: u8) -> ClipboardEngine {
        connected(ClipboardEngine::new(
            Uuid::from_bytes([origin_fill; 16]),
            ClipboardConfig::new(),
        ))
    }

    /// An engine with one live session — which is the situation almost
    /// every test in this module is about: two connected peers.
    ///
    /// Since the 2026-09-01 addendum to ADR 0006 a local change with no
    /// live session is recorded and not transmitted, so a test about
    /// *transmission* has to say a peer is there first. Tests about the
    /// offline rule itself build a bare engine instead and never call
    /// this.
    fn connected(mut engine: ClipboardEngine) -> ClipboardEngine {
        engine.on_session_established();
        engine
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
            [OutboundMessage::Offer(offer)] => offer.clone(),
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
        let mut actions = engine.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
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

    /// `Default` must be the production configuration, not a field-wise
    /// zero. A derived one would hand `..Default::default()` a
    /// `transfer_timeout` of zero — every transfer abandoned at birth —
    /// and a `transmit_debounce` of zero, silently undoing ADR 0006. Both
    /// would compile, and neither would look wrong at the call site.
    #[test]
    fn the_default_configuration_is_the_production_one() {
        assert_eq!(ClipboardConfig::default(), ClipboardConfig::new());
        assert_eq!(
            ClipboardConfig::default().transfer_timeout,
            super::TRANSFER_TIMEOUT
        );
        assert_eq!(
            ClipboardConfig::default().transmit_debounce,
            super::TRANSMIT_DEBOUNCE
        );
        assert!(!ClipboardConfig::default().transfer_timeout.is_zero());
    }

    /// The arithmetic ADR 0005's 2026-09-01 addendum turns on, pinned so
    /// that raising either budget without the other is a test failure
    /// rather than a class of silent stall.
    ///
    /// A receiver that keeps retrying past the *origin's* transfer
    /// deadline is answering a transaction nobody is listening to: the
    /// origin has already abandoned it and counted it, and the verdict
    /// that eventually arrives lands on nothing. The whole install budget
    /// must therefore finish comfortably inside `TRANSFER_TIMEOUT`, with
    /// room left over for the network and for queueing on either side.
    #[test]
    fn the_whole_install_budget_fits_inside_the_origins_patience() {
        let retry = RetryPolicy::default();
        let fast = retry.delay * retry.max_attempts;
        // The last parked attempt can be scheduled up to one `park_delay`
        // after the budget was last checked, so it counts too.
        let worst_case = fast + retry.park_budget + retry.park_delay;
        assert!(
            worst_case * 2 < super::TRANSFER_TIMEOUT,
            "the install budget ({worst_case:?}) leaves the origin ({:?}) too little margin",
            super::TRANSFER_TIMEOUT
        );
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

    /// A local file/folder selection is observable (feature/133), but the
    /// engine does not yet stage it for transmission: the archive builder
    /// and offer transaction are feature/135's job (ADR 0015 "Sender
    /// side"). Proved as a deliberate no-op rather than merely an absence
    /// of actions — an ordinary text copy right after must still travel
    /// normally, so the no-op is not accidentally wedging outbound state
    /// (`current_local_hash`, `applied_hashes`, or the sequence counter).
    #[test]
    fn a_local_file_selection_is_not_staged_by_a_build_without_a_sender() {
        // The default policy is the closed one, so a build that never
        // wires a sending half refuses by construction rather than by
        // remembering to (ADR 0015).
        let mut e = engine(0xAA);
        let paths = vec![
            std::path::PathBuf::from(r"C:\Users\test\report.pdf"),
            std::path::PathBuf::from(r"C:\Users\test\photos"),
        ];
        assert!(
            e.on_local_read(Some(ClipboardContent::FileList(paths)))
                .is_empty(),
            "a file selection must not be walked with no sender to walk it"
        );
        // Nothing about the refusal should prevent an ordinary text copy
        // from travelling right afterwards.
        assert_eq!(sent(&copy(&mut e, "still works")).len(), 1);
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
            // Zero parked budget: this test is about the *fast* phase's
            // cap and the verdict past it, so the parked phase is
            // configured out of the way rather than reasoned around.
            park_delay: std::time::Duration::from_millis(50),
            park_budget: Duration::ZERO,
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
            e.on_write_result(id, Err(WriteFailure::Busy)).as_slice(),
            [Action::ScheduleRetry { .. }]
        ));
        assert!(matches!(
            e.on_retry_due(id).as_slice(),
            [Action::WriteClipboard { .. }]
        ));
        assert!(matches!(
            e.on_write_result(id, Err(WriteFailure::Busy)).as_slice(),
            [Action::ScheduleRetry { .. }]
        ));
        assert!(matches!(
            e.on_retry_due(id).as_slice(),
            [Action::WriteClipboard { .. }]
        ));
        // Third attempt fails: the cap closes the transaction honestly.
        let actions = e.on_write_result(id, Err(WriteFailure::Busy));
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
        let actions = e.on_write_result(id2, Err(WriteFailure::Unavailable));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Applied(ClipboardApplied {
                result: ApplyResult::ClipboardUnavailable,
                ..
            })]
        ));
    }

    /// A retry policy whose two phases are small enough for a unit test
    /// but still shaped like the production one: a fast phase of a few
    /// attempts, then a parked phase on a slower cadence.
    fn parking_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            delay: Duration::from_millis(1),
            park_delay: Duration::from_millis(1),
            park_budget: Duration::from_secs(30),
        }
    }

    fn parking_engine(metrics: &Arc<Metrics>) -> ClipboardEngine {
        connected(ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xBB; 16]),
            ClipboardConfig {
                retry: parking_policy(),
                ..ClipboardConfig::new()
            },
            Some(Arc::clone(metrics)),
        ))
    }

    /// Stage one inbound text item and return its id, having consumed the
    /// first write attempt the engine asks for.
    fn inbound_text(engine: &mut ClipboardEngine, sequence: u64, text: &str) -> Uuid {
        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            sequence,
            ContentType::Utf8Text,
            text.as_bytes().to_vec(),
        );
        let id = item.meta.id;
        assert!(
            matches!(
                engine
                    .on_peer_message(InboundMessage::Data(item))
                    .as_slice(),
                [Action::WriteClipboard { .. }]
            ),
            "staging an inbound item should ask for a write"
        );
        id
    }

    /// Burn the fast retry budget on `Busy`, leaving the install parked
    /// with its slow timer armed.
    fn park_the_install(engine: &mut ClipboardEngine, id: Uuid) {
        for _ in 1..parking_policy().max_attempts {
            assert!(matches!(
                engine
                    .on_write_result(id, Err(WriteFailure::Busy))
                    .as_slice(),
                [Action::ScheduleRetry { .. }]
            ));
            assert!(matches!(
                engine.on_retry_due(id).as_slice(),
                [Action::WriteClipboard { .. }]
            ));
        }
        // The attempt that used to end the transaction: it parks instead.
        let parked = engine.on_write_result(id, Err(WriteFailure::Busy));
        assert!(
            matches!(parked.as_slice(), [Action::ScheduleRetry { .. }]),
            "the fast budget should park the install, not close it: {parked:?}"
        );
        assert!(
            sent(&parked).is_empty(),
            "the origin heard a verdict while the install was still viable"
        );
    }

    /// The 2026-09-01 defect, at the engine level: about a second of
    /// external contention outlives the fast retry budget, and used to
    /// cost the item permanently. It must cost only time.
    #[test]
    fn a_contended_clipboard_parks_the_install_rather_than_dropping_it() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        let id = inbound_text(&mut e, 0, "must not be lost");
        park_the_install(&mut e, id);
        assert_eq!(metrics.snapshot().clipboard_installs_parked, 1);

        // Parked attempts keep going on the slow cadence, with nothing
        // said to the origin, until one of them lands.
        for _ in 0..5 {
            assert!(matches!(
                e.on_retry_due(id).as_slice(),
                [Action::WriteClipboard { .. }]
            ));
            assert!(matches!(
                e.on_write_result(id, Err(WriteFailure::Busy)).as_slice(),
                [Action::ScheduleRetry { .. }]
            ));
        }
        assert!(matches!(
            e.on_retry_due(id).as_slice(),
            [Action::WriteClipboard { .. }]
        ));
        let applied = e.on_write_result(id, Ok(()));
        assert_eq!(verdict(&applied), ApplyResult::Applied);

        let report = metrics.snapshot();
        assert_eq!(report.clipboard_applied, 1);
        assert_eq!(report.clipboard_installs_failed, 0);
    }

    /// Parking is not waiting forever. ADR 0005 requires every
    /// transaction to end in a typed verdict within a bounded time, and
    /// the parked budget is that bound — a budget of zero makes the
    /// boundary observable without a twenty-second test.
    #[test]
    fn a_parked_install_still_ends_in_a_verdict_when_its_budget_runs_out() {
        let metrics = Arc::new(Metrics::new());
        let mut e = ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xBB; 16]),
            ClipboardConfig {
                retry: RetryPolicy {
                    park_budget: Duration::from_nanos(1),
                    ..parking_policy()
                },
                ..ClipboardConfig::new()
            },
            Some(Arc::clone(&metrics)),
        );
        let id = inbound_text(&mut e, 0, "outlives the budget");
        park_the_install(&mut e, id);

        assert!(matches!(
            e.on_retry_due(id).as_slice(),
            [Action::WriteClipboard { .. }]
        ));
        let closed = e.on_write_result(id, Err(WriteFailure::Busy));
        assert_eq!(closed.len(), 1, "the budget must close the transaction");
        assert_eq!(verdict(&closed), ApplyResult::ClipboardUnavailable);

        let report = metrics.snapshot();
        assert_eq!(report.clipboard_installs_parked, 1);
        assert_eq!(report.clipboard_installs_failed, 1);
        assert_eq!(report.clipboard_applied, 0);
    }

    /// The safe order, and the regression it encodes. A notification must
    /// **not** write: on Windows it almost always means new content just
    /// landed, so a parked install taking that moment would land on top of
    /// the copy the user made an instant earlier — and the settle read
    /// would then find our own content, suppress it as a loop, and report
    /// nothing at all. The user's copy would be gone silently.
    ///
    /// So the notification only starts the clock, and the read decides.
    #[test]
    fn a_notification_starts_the_clock_and_never_writes() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        let id = inbound_text(&mut e, 0, "the peer's item");
        park_the_install(&mut e, id);

        let notified = e.on_local_change();
        assert!(
            matches!(notified.as_slice(), [Action::ScheduleSettle { .. }]),
            "a notification wrote before the read said what was there: {notified:?}"
        );
        // The parked timer is still the backstop underneath, untouched by
        // the notification having come and gone.
        assert!(matches!(
            e.on_retry_due(id).as_slice(),
            [Action::WriteClipboard { id: retried, .. }] if *retried == id
        ));
    }

    /// The read's first answer: content unchanged since we last looked
    /// means the clipboard is merely free again — nothing of the user's is
    /// at risk, so the parked install takes it now rather than waiting out
    /// its timer.
    #[test]
    fn a_read_of_unchanged_content_retries_the_parked_install() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        // Something of ours is already the known local content.
        copy(&mut e, "what was already here");
        let id = inbound_text(&mut e, 9, "the peer's item");
        park_the_install(&mut e, id);

        assert!(matches!(
            e.on_local_change().as_slice(),
            [Action::ScheduleSettle { .. }]
        ));
        assert_eq!(e.on_settle_due(), vec![Action::ReadClipboard]);
        let read = e.on_local_read(Some(ClipboardContent::Text(
            "what was already here".to_owned(),
        )));
        assert!(
            matches!(
                read.as_slice(),
                [Action::WriteClipboard { id: retried, .. }] if *retried == id
            ),
            "an unchanged read should retry the parked install: {read:?}"
        );
        assert_eq!(
            verdict(&e.on_write_result(id, Ok(()))),
            ApplyResult::Applied
        );
        // The slow timer that was armed when the install parked fires
        // afterwards into nothing: the read took its turn, so there is
        // never a second write in flight for one transaction.
        assert!(e.on_retry_due(id).is_empty());
    }

    /// The same answer for the other kind of "not the user's": a read that
    /// finds content this engine installed itself. It is loop-suppressed,
    /// as always, and it frees the parked install to try.
    #[test]
    fn a_read_of_our_own_installed_content_retries_the_parked_install() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        // An earlier install landed, so its hash is in the applied memory.
        let first = inbound_text(&mut e, 0, "installed earlier");
        assert_eq!(
            verdict(&e.on_write_result(first, Ok(()))),
            ApplyResult::Applied
        );
        let parked = inbound_text(&mut e, 1, "the peer's next item");
        park_the_install(&mut e, parked);

        let read = e.on_local_read(Some(ClipboardContent::Text("installed earlier".to_owned())));
        assert!(
            matches!(
                read.as_slice(),
                [Action::WriteClipboard { id: retried, .. }] if *retried == parked
            ),
            "our own applied content should free the parked install: {read:?}"
        );
        assert_eq!(metrics.snapshot().clipboard_loop_suppressed, 1);
    }

    /// A parked install lives long enough that the two things which can
    /// legitimately outrank it must both close it — with the verdict the
    /// rest of the engine already uses for "something newer won", and
    /// never in silence.
    #[test]
    fn a_newer_item_supersedes_a_parked_install_with_a_verdict() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        let parked = inbound_text(&mut e, 0, "the older item");
        park_the_install(&mut e, parked);

        let newer = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            1,
            ContentType::Utf8Text,
            b"the newer item".to_vec(),
        );
        let newer_id = newer.meta.id;
        let actions = e.on_peer_message(InboundMessage::Data(newer));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Applied(ClipboardApplied {
                id,
                result: ApplyResult::Superseded,
            })] if *id == parked
        ));
        assert!(matches!(
            actions.as_slice(),
            [_, Action::WriteClipboard { id, .. }] if *id == newer_id
        ));
        assert!(e.on_retry_due(parked).is_empty(), "a ghost install retried");
    }

    /// The other one that outranks it, and the reason the parked budget is
    /// not simply "forever": a local copy is this machine's user putting
    /// something on this machine's clipboard, and installing a peer item
    /// over it seconds later would destroy what they just made.
    ///
    /// The *order* is the assertion. Nothing may be written between the
    /// notification and the read, because until the read there is no way
    /// to tell this case from the one above — and guessing wrong here is
    /// how the user's copy disappears without a trace.
    #[test]
    fn a_local_copy_supersedes_a_parked_install() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        let parked = inbound_text(&mut e, 0, "the peer's item");
        park_the_install(&mut e, parked);

        // The user copies. The notification arrives first, and must not
        // touch the clipboard.
        let notified = e.on_local_change();
        assert!(
            !notified
                .iter()
                .any(|a| matches!(a, Action::WriteClipboard { .. })),
            "the parked install was written before the read: {notified:?}"
        );
        assert_eq!(e.on_settle_due(), vec![Action::ReadClipboard]);

        // Only now, with genuinely new content in hand, does the parked
        // install lose — and it is told so.
        let actions = e.on_local_read(Some(ClipboardContent::Text("mine".to_owned())));
        let messages = sent(&actions);
        assert!(
            matches!(
                messages.as_slice(),
                [
                    OutboundMessage::Applied(ClipboardApplied {
                        id,
                        result: ApplyResult::Superseded,
                    }),
                    OutboundMessage::Data(_),
                ] if *id == parked
            ),
            "the local copy should close the parked install and travel: {messages:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::WriteClipboard { .. })),
            "the superseded install was still written: {actions:?}"
        );
        assert!(e.on_retry_due(parked).is_empty(), "a ghost install retried");
    }

    /// The echo guard's blind spot, which is silent and permanent when it
    /// bites. A newer item whose content already matches this clipboard is
    /// answered `Applied` without a write — correct — but an *older*
    /// install still pending used to survive that answer, unpark, and
    /// write its own content over the clipboard both machines had just
    /// agreed on. Its own-write notification is loop-suppressed, so
    /// nothing anywhere notices the two machines now disagree.
    #[test]
    fn an_item_that_already_matches_still_closes_a_pending_install() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        // This clipboard already holds "shared"; nothing of ours is in
        // flight to complicate the conflict rule.
        copy(&mut e, "shared");
        e.on_session_lost();

        let parked = inbound_text(&mut e, 9, "the older item");
        park_the_install(&mut e, parked);

        let matching = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xAA; 16]),
            10,
            ContentType::Utf8Text,
            b"shared".to_vec(),
        );
        let matching_id = matching.meta.id;
        let actions = e.on_peer_message(InboundMessage::Data(matching));
        let messages = sent(&actions);
        assert!(
            matches!(
                messages.as_slice(),
                [
                    OutboundMessage::Applied(ClipboardApplied {
                        id: older,
                        result: ApplyResult::Superseded,
                    }),
                    OutboundMessage::Applied(ClipboardApplied {
                        id: newer,
                        result: ApplyResult::Applied,
                    }),
                ] if *older == parked && *newer == matching_id
            ),
            "the pending install outlived the item that agreed with us: {messages:?}"
        );
        assert!(
            e.on_retry_due(parked).is_empty(),
            "the superseded install can still write over the agreed content"
        );
    }

    /// The reconnect case the parked phase exists for, and the one the
    /// re-announce nearly broke. Establishing a session asks for a re-read
    /// so peers converge (ADR 0006, trigger 3) — but a re-read of content
    /// that was already here is not the user copying something, and must
    /// not cost the peer's parked item.
    #[test]
    fn a_reconnect_re_read_of_unchanged_content_spares_a_parked_install() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        copy(&mut e, "already here");
        e.on_session_lost();

        let parked = inbound_text(&mut e, 9, "the peer's item");
        park_the_install(&mut e, parked);

        let established = e.on_session_established();
        assert_eq!(established.last(), Some(&Action::ReadClipboard));

        let actions = e.on_local_read(Some(ClipboardContent::Text("already here".to_owned())));
        let messages = sent(&actions);
        assert!(
            matches!(messages.as_slice(), [OutboundMessage::Data(_)]),
            "the reconnect should re-announce and say nothing else: {messages:?}"
        );
        assert!(matches!(
            e.on_retry_due(parked).as_slice(),
            [Action::WriteClipboard { id, .. }] if *id == parked
        ));
        assert_eq!(metrics.snapshot().clipboard_superseded, 0);
    }

    /// The other half of the same rule: content genuinely copied while the
    /// link was down *is* the user's, and it outranks the parked install
    /// exactly as a copy made with the session up would.
    #[test]
    fn a_reconnect_re_read_of_new_content_supersedes_a_parked_install() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        copy(&mut e, "already here");
        e.on_session_lost();

        let parked = inbound_text(&mut e, 9, "the peer's item");
        park_the_install(&mut e, parked);

        e.on_session_established();
        let actions = e.on_local_read(Some(ClipboardContent::Text(
            "copied during the outage".to_owned(),
        )));
        let messages = sent(&actions);
        assert!(
            matches!(
                messages.as_slice(),
                [
                    OutboundMessage::Applied(ClipboardApplied {
                        id,
                        result: ApplyResult::Superseded,
                    }),
                    OutboundMessage::Data(_),
                ] if *id == parked
            ),
            "new content copied during the gap should win: {messages:?}"
        );
        assert!(e.on_retry_due(parked).is_empty(), "a ghost install retried");
    }

    /// An install that has not landed belongs to the session that carried
    /// it. Left alive, a parked one can outlive the session, land during
    /// the *next* one, and overwrite whatever the user did in between —
    /// answering a peer that stopped waiting long ago.
    ///
    /// Dropped without a verdict (there is nobody to tell) and without a
    /// counted failure, matching the outbound slot beside it. The content
    /// comes back on reconnect.
    #[test]
    fn a_lost_session_drops_the_install_it_was_carrying() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        let parked = inbound_text(&mut e, 0, "belongs to this session");
        park_the_install(&mut e, parked);

        let lost = e.on_session_lost();
        assert!(
            sent(&lost).is_empty(),
            "a verdict was sent to a peer that is gone: {lost:?}"
        );
        assert!(
            e.on_retry_due(parked).is_empty(),
            "the install survived its session and can still write"
        );
        assert!(
            e.on_write_result(parked, Ok(())).is_empty(),
            "a late write result revived a dropped install"
        );

        let report = metrics.snapshot();
        assert_eq!(report.clipboard_installs_failed, 0, "counted as a loss");
        assert_eq!(report.clipboard_superseded, 0, "counted as a supersession");
    }

    /// A repeat of the frame already installing is the same transaction,
    /// not a newer item. Answering it `Superseded` would draw **two
    /// verdicts for one id** — the supersession now and the install's own
    /// verdict later — and one transaction, one verdict is what the
    /// origin's state machine is built on.
    #[test]
    fn a_repeated_data_frame_does_not_draw_a_second_verdict() {
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        let id = Uuid::new_v4();
        let deliver = || {
            ClipboardData::from_content(
                id,
                Uuid::from_bytes([0xAA; 16]),
                0,
                ContentType::Utf8Text,
                b"delivered twice".to_vec(),
            )
        };
        assert!(matches!(
            e.on_peer_message(InboundMessage::Data(deliver()))
                .as_slice(),
            [Action::WriteClipboard { .. }]
        ));
        park_the_install(&mut e, id);

        // The peer repeats it — a retransmit, a duplicate delivery.
        let again = e.on_peer_message(InboundMessage::Data(deliver()));
        assert!(
            sent(&again).is_empty(),
            "a repeat drew a verdict for an install still running: {again:?}"
        );
        assert!(matches!(
            again.as_slice(),
            [Action::WriteClipboard { id: writing, .. }] if *writing == id
        ));

        // Exactly one verdict, when it finally lands.
        assert_eq!(
            verdict(&e.on_write_result(id, Ok(()))),
            ApplyResult::Applied
        );
        assert_eq!(metrics.snapshot().clipboard_superseded, 0);
    }

    /// The parked install must not pay for a mistake of ours. Layer 2's
    /// whole reason to exist is layer 1 missing, so the selection reaching
    /// it may be our own delivered file coming back — and superseding on
    /// that would kill the peer's item and then discard the selection as a
    /// loop, losing the item to catch our own error.
    ///
    /// A refusal is different: a selection this build will not send is
    /// still on the clipboard and still the user's, so it still wins.
    #[test]
    fn only_a_selection_that_is_really_the_users_costs_a_parked_install() {
        let root = spool_root();
        let metrics = Arc::new(Metrics::new());
        let mut e = parking_engine(&metrics);
        e.set_file_send(FileSend::Allowed);
        e.set_spool_root(Some(root.clone()));

        let parked = inbound_text(&mut e, 0, "the peer's item");
        park_the_install(&mut e, parked);

        // Layer 1 missed and our own spool selection came back.
        let ours = e.on_local_read(Some(ClipboardContent::FileList(vec![
            root.join("3f2a.bin"),
        ])));
        assert!(ours.is_empty(), "our own selection produced work: {ours:?}");
        assert!(
            matches!(
                e.on_retry_due(parked).as_slice(),
                [Action::WriteClipboard { id, .. }] if *id == parked
            ),
            "our own selection coming back cost the peer's parked item"
        );
        assert_eq!(metrics.snapshot().clipboard_superseded, 0);

        // A selection that is genuinely theirs still outranks it, even one
        // this build refuses to send: the refusal is about us, not about
        // what they copied.
        e.set_file_send(FileSend::Denied);
        let theirs = e.on_local_read(Some(ClipboardContent::FileList(vec![elsewhere(
            "report.pdf",
        )])));
        assert!(
            matches!(
                sent(&theirs).as_slice(),
                [OutboundMessage::Applied(ClipboardApplied {
                    id,
                    result: ApplyResult::Superseded,
                })] if *id == parked
            ),
            "a refused-but-real local copy should still win: {theirs:?}"
        );
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
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
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
                        // A spool action here would mean the engine
                        // invented a file transfer from a text copy,
                        // which is worth failing on rather than
                        // absorbing into a wildcard.
                        spool => panic!("a text transaction asked for spool work: {spool:?}"),
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

    /// The 2026-09-01 defect, at its source (ADR 0006 addendum).
    ///
    /// On machine A, with the peer asleep for eight hours, every local
    /// copy minted a deadline-bound transaction, broadcast a frame the
    /// application dropped for want of a sink, and produced a WARN and an
    /// `abandoned` sixty seconds later — twenty times in one evening, for
    /// a fault that never happened. Nothing at all may be minted with no
    /// peer to answer it: no frame, no outbound slot, no deadline.
    #[test]
    fn a_local_copy_with_no_peer_mints_no_transaction() {
        let metrics = Arc::new(Metrics::new());
        let mut e = ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        );

        let actions = copy(&mut e, "copied while alone");
        assert!(
            actions.is_empty(),
            "a copy with no peer asked for work: {actions:?}"
        );

        let snap = metrics.snapshot();
        assert_eq!(snap.clipboard_offline_changes, 1);
        // The two counters this defect polluted: nothing was sent, and —
        // the point of the whole change — nothing can later be abandoned,
        // because no deadline was ever armed.
        assert_eq!(snap.clipboard_sent, 0);
        assert_eq!(snap.clipboard_abandoned, 0);
    }

    /// The other half of the rule: a held copy is not a lost copy.
    ///
    /// `on_session_established` clears the dedup hash and re-reads, which
    /// is ADR 0006's trigger 3 — and, since the addendum, the *only*
    /// route by which an offline copy is ever offered. However many
    /// copies the gap contained, the peer is offered the one that is
    /// current, which is the only one anybody can paste.
    #[test]
    fn the_item_copied_while_alone_is_offered_when_a_peer_arrives() {
        let metrics = Arc::new(Metrics::new());
        let mut e = ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        );

        assert!(copy(&mut e, "first while alone").is_empty());
        assert!(copy(&mut e, "last while alone").is_empty());
        assert_eq!(metrics.snapshot().clipboard_offline_changes, 2);

        // Establishing asks for the re-read, and the re-read offers the
        // current item — once, not twice.
        assert_eq!(e.on_session_established(), vec![Action::ReadClipboard]);
        let actions = e.on_local_read(Some(ClipboardContent::Text("last while alone".to_owned())));
        let msgs = sent(&actions);
        assert_eq!(msgs.len(), 1, "expected one offer, got {msgs:?}");
        let OutboundMessage::Data(data) = msgs[0] else {
            panic!("expected inline data");
        };
        assert_eq!(data.content, b"last while alone");
        assert_eq!(metrics.snapshot().clipboard_sent, 1);
    }

    /// Losing the session mid-transaction is unchanged by the new rule:
    /// the in-flight state is released, nothing is sent to a peer that is
    /// gone, and the next copy is simply held instead of minting another
    /// transaction into the void.
    #[test]
    fn a_session_lost_mid_transaction_still_releases_and_then_holds() {
        let mut e = engine(0xAA);
        let actions = copy(&mut e, "in flight");
        assert_eq!(sent(&actions).len(), 1);

        // As today: the transaction is dropped, and no verdict travels.
        let lost = e.on_session_lost();
        assert!(lost.is_empty(), "session loss sent something: {lost:?}");

        // And now there is no peer, so the next copy is held.
        assert!(copy(&mut e, "after the loss").is_empty());
    }

    /// A count, not a flag (ADR 0006 addendum): this process can hold an
    /// inbound and an outbound session at once, and both fan their
    /// lifecycle into this one engine. If losing either one stopped
    /// transmission, a copy would silently stop reaching the peer that
    /// was still connected — a clipboard that quietly stops working,
    /// which is the priority-#2 fault this rule exists to avoid causing.
    #[test]
    fn losing_one_of_two_sessions_does_not_stop_offering() {
        let mut e = engine(0xAA); // one session live
        e.on_session_established(); // and a second

        e.on_session_lost();
        let actions = copy(&mut e, "still one peer left");
        assert_eq!(
            sent(&actions).len(),
            1,
            "the surviving session stopped receiving offers: {actions:?}"
        );

        // Only the last one out turns transmission off.
        e.on_session_lost();
        assert!(copy(&mut e, "now nobody").is_empty());
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
        let mut e = connected(ClipboardEngine::new(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig {
                transmit_debounce: Duration::ZERO,
                ..ClipboardConfig::new()
            },
        ));
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
        let mut e = connected(ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        ));

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
        let mut e = connected(ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        ));

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
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
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
    ///
    /// *Which* failure it reports matters. A type this destination cannot
    /// represent is `ContentRejected`: permanent, about the item. A
    /// clipboard that would not take it is `ClipboardUnavailable`:
    /// transient, about the machine. An origin that cannot tell those
    /// apart cannot tell "never send me images" from "try again".
    #[test]
    fn an_image_the_platform_cannot_install_reports_the_failure() {
        for (failure, verdict) in [
            (WriteFailure::UnsupportedType, ApplyResult::ContentRejected),
            (WriteFailure::Unavailable, ApplyResult::ClipboardUnavailable),
        ] {
            let mut e = engine(0xBB);
            let bytes = image_bytes(4096);
            let (meta, _) = inbound_image(&mut e, 0xAA, 1, &bytes);

            let closed = e.on_write_result(meta.id, Err(failure));
            match sent(&closed).as_slice() {
                [OutboundMessage::Applied(applied)] => {
                    assert_eq!(applied.result, verdict, "for {failure:?}");
                }
                other => panic!("expected a typed failure verdict, got {other:?}"),
            }
        }
    }

    /// An unsupported type is never retried: the retry budget exists for
    /// contention (FR-3.4), and no number of attempts will teach this
    /// build a raster format.
    #[test]
    fn an_unsupported_content_type_is_answered_without_burning_retries() {
        let mut e = engine(0xBB);
        let bytes = image_bytes(4096);
        let (meta, _) = inbound_image(&mut e, 0xAA, 1, &bytes);

        let closed = e.on_write_result(meta.id, Err(WriteFailure::UnsupportedType));
        assert!(
            !closed
                .iter()
                .any(|a| matches!(a, Action::ScheduleRetry { .. })),
            "an unsupported type was retried: {closed:?}"
        );
        assert!(matches!(
            sent(&closed).as_slice(),
            [OutboundMessage::Applied(ClipboardApplied {
                result: ApplyResult::ContentRejected,
                ..
            })]
        ));
        assert_eq!(meta.content_type, ContentType::Image(ImageFormat::Dib));
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
        e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta: first,
            descriptor: None,
        }));
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
        e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
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
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
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
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
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

    /// An unanswered transaction must not occupy the single outbound slot
    /// forever — not for its memory (an `AwaitingApplied` holds almost
    /// none) but because that slot decides conflicts: a peer that never
    /// acknowledges would otherwise leave a zombie item winning races
    /// against everything minted after it (FR-3.5).
    #[test]
    fn an_unacknowledged_inline_item_expires_instead_of_skewing_conflicts() {
        use std::sync::Arc;

        use crate::metrics::Metrics;

        let metrics = Arc::new(Metrics::new());
        let mut e = connected(ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        ));
        let actions = copy(&mut e, "sent into silence");
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::ScheduleTransferTimeout {
                    scope: TransferScope::Outbound,
                    ..
                }
            )),
            "even an inline item must be bounded in time: {actions:?}"
        );

        assert!(e.outbound.is_some());
        assert!(
            e.on_transfer_timeout(TransferScope::Outbound, e.outbound_generation)
                .is_empty()
        );
        assert!(e.outbound.is_none(), "the zombie transaction survived");
        assert_eq!(metrics.snapshot().clipboard_abandoned, 1);

        // With the slot free, a later inbound item is judged on its own
        // merits rather than raced against one nobody is waiting for.
        let inbound = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0x01; 16]), // lower origin: would have lost
            0,
            ContentType::Utf8Text,
            b"theirs".to_vec(),
        );
        let actions = e.on_peer_message(InboundMessage::Data(inbound));
        assert_eq!(written_text(&actions).as_deref(), Some("theirs"));
        assert_eq!(metrics.snapshot().clipboard_conflicts, 0);
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
            e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
                meta,
                descriptor: None,
            }));
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

    /// The recent-transfer ring must remember four *distinct* transfers,
    /// not four copies of one. If a repeated id could evict the others,
    /// the tail of a genuinely different superseded transfer would become
    /// chargeable — the peer's repetition deciding whether a benign race
    /// costs it violations.
    #[test]
    fn the_recent_transfer_ring_remembers_distinct_transfers() {
        use super::RECENT_TRANSFER_MEMORY;

        let mut e = engine(0xBB);
        let bytes = image_bytes(64);

        // The oldest of RECENT_TRANSFER_MEMORY transfers, whose tail must
        // still be recognized at the end.
        let (oldest, _) = inbound_image(&mut e, 0xAA, 0, &bytes);
        e.on_write_result(oldest.id, Ok(()));

        // A second transfer, completed repeatedly — its trailing chunks
        // arrive again and again, each one re-remembering the same id.
        let (repeated, _) = inbound_image(&mut e, 0xAA, 1, &image_bytes(96));
        e.on_write_result(repeated.id, Ok(()));
        let tail = chunk_content(repeated.id, &image_bytes(96)).unwrap();
        for _ in 0..RECENT_TRANSFER_MEMORY * 3 {
            assert!(
                e.on_peer_message(InboundMessage::Chunk(tail[0].clone()))
                    .is_empty()
            );
        }

        // The oldest transfer's tail is still absorbed, not charged.
        let old_tail = chunk_content(oldest.id, &bytes).unwrap();
        let actions = e.on_peer_message(InboundMessage::Chunk(old_tail[0].clone()));
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(
            e.violations, 0,
            "a repeated id crowded the ring and made a benign tail chargeable"
        );
        // A genuinely unknown id is still a violation: the ring absorbs
        // races, not everything.
        e.on_peer_message(InboundMessage::Chunk(ClipboardChunk {
            id: Uuid::from_bytes([0x5A; 16]),
            index: 0,
            payload: vec![0x11; 32],
        }));
        assert_eq!(e.violations, 1);
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
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
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
        let actions = e.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta,
            descriptor: None,
        }));
        assert!(matches!(
            sent(&actions).as_slice(),
            [OutboundMessage::Accept(_)]
        ));
        assert!(e.reassembly.is_none(), "text must never build a reassembly");

        let actions = e.on_peer_message(InboundMessage::Data(data));
        assert_eq!(written_text(&actions).as_deref(), Some(big.as_str()));
    }

    /// The platform boundary mirrors the protocol's image vocabulary
    /// because `crossover-platform` may carry no dependencies
    /// (docs/ARCHITECTURE.md §4). This crate is where the two meet, so it
    /// is where the mirror is proved: every format survives the round trip
    /// in both directions, and the size ceiling the platform crate states
    /// at its boundary is the protocol's ceiling, not a second opinion.
    #[test]
    fn the_platform_mirror_agrees_with_the_protocol() {
        use crossover_protocol::clipboard::MAX_CLIPBOARD_IMAGE_BYTES;

        for format in [ImageFormat::Dib, ImageFormat::Png, ImageFormat::Jpeg] {
            assert_eq!(super::wire_format(super::platform_format(format)), format);
        }
        for format in [
            ClipboardImageFormat::Dib,
            ClipboardImageFormat::Png,
            ClipboardImageFormat::Jpeg,
        ] {
            assert_eq!(super::platform_format(super::wire_format(format)), format);
        }
        assert_eq!(
            crossover_platform::MAX_CLIPBOARD_IMAGE_BYTES,
            MAX_CLIPBOARD_IMAGE_BYTES,
            "the platform boundary's ceiling drifted from the protocol's"
        );
    }

    /// The same mirror, for the file-entry ceiling (ADR 0015, feature/133):
    /// `crossover-platform` states its own `MAX_CLIPBOARD_FILE_ENTRIES`
    /// because it may carry no dependencies, and this is where the two are
    /// proved equal so they cannot drift apart silently.
    #[test]
    fn the_platform_file_entry_ceiling_agrees_with_the_protocol() {
        use crossover_protocol::clipboard::MAX_CLIPBOARD_FILE_ENTRIES;

        assert_eq!(
            crossover_platform::MAX_CLIPBOARD_FILE_ENTRIES,
            MAX_CLIPBOARD_FILE_ENTRIES,
            "the platform boundary's file-entry ceiling drifted from the protocol's"
        );
    }

    /// The same mirror again, for the two bounds the *sender's* blob
    /// builder enforces (ADR 0015, feature/134). They are mirrored rather
    /// than imported for the no-dependencies reason above, and they matter
    /// more than most: the sender is the only party that can refuse an
    /// oversized or too-deeply-nested selection before the bytes exist, so
    /// a platform value that drifted *upward* would build an item the
    /// receiver is then obliged to decline after it arrived.
    #[test]
    fn the_platform_sender_bounds_agree_with_the_protocol() {
        use crossover_protocol::clipboard::{MAX_ARCHIVE_DEPTH, MAX_CLIPBOARD_FILE_BYTES};

        assert_eq!(
            crossover_platform::MAX_CLIPBOARD_FILE_BYTES,
            MAX_CLIPBOARD_FILE_BYTES,
            "the platform boundary's file-byte ceiling drifted from the protocol's"
        );
        assert_eq!(
            crossover_platform::MAX_ARCHIVE_DEPTH,
            MAX_ARCHIVE_DEPTH,
            "the platform boundary's archive-depth ceiling drifted from the protocol's"
        );
    }

    // ---- files (ADR 0015) ----

    fn file_meta(bytes: &[u8], sequence: u64) -> ClipboardMeta {
        ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xBB; 16]),
            sequence,
            content_type: ContentType::File,
            content_length: bytes.len() as u64,
            content_hash: content_hash(bytes),
        }
    }

    fn file_offer(meta: ClipboardMeta, name: &str) -> ClipboardOffer {
        ClipboardOffer {
            meta,
            descriptor: Some(FileDescriptor {
                file_name: name.to_owned(),
                archived: false,
                entry_count: 1,
                original_bytes: meta.content_length,
            }),
        }
    }

    /// An engine configured as the application configures one when a
    /// protected spool is open and the peer holds the grant.
    fn granted(origin_fill: u8) -> ClipboardEngine {
        let mut engine = engine(origin_fill);
        engine.set_file_receive(FileReceive::Allowed);
        engine
    }

    fn admission_of(actions: &[Action]) -> (Uuid, String, u64) {
        actions
            .iter()
            .find_map(|action| match action {
                Action::AdmitFile {
                    id,
                    entry,
                    byte_len,
                } => Some((*id, entry.clone(), *byte_len)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected an admission, got {actions:?}"))
    }

    fn written_chunk(actions: &[Action]) -> Vec<u8> {
        actions
            .iter()
            .find_map(|action| match action {
                Action::WriteFileChunk { payload, .. } => Some(payload.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a spool write, got {actions:?}"))
    }

    fn aborted_entry(actions: &[Action]) -> String {
        actions
            .iter()
            .find_map(|action| match action {
                Action::AbortFile { entry, .. } => Some(entry.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected an abort, got {actions:?}"))
    }

    fn declined_reason(actions: &[Action]) -> DeclineReason {
        match sent(actions).as_slice() {
            [OutboundMessage::Decline(decline)] => decline.reason,
            other => panic!("expected exactly one decline, got {other:?}"),
        }
    }

    fn verdict(actions: &[Action]) -> ApplyResult {
        match sent(actions).as_slice() {
            [OutboundMessage::Applied(applied)] => applied.result,
            other => panic!("expected exactly one verdict, got {other:?}"),
        }
    }

    /// Drive a whole inbound file transfer the way the driver does:
    /// admission, one confirmed write per chunk, then the commit. Returns
    /// the actions the commit produced and everything the spool was asked
    /// to write.
    fn receive_file(
        engine: &mut ClipboardEngine,
        name: &str,
        bytes: &[u8],
        sequence: u64,
    ) -> (Vec<Action>, Vec<u8>, String) {
        let meta = file_meta(bytes, sequence);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, name)));
        let (id, part, byte_len) = admission_of(&offered);
        assert_eq!(id, meta.id);
        assert_eq!(byte_len, bytes.len() as u64);
        assert!(
            sent(&offered).is_empty(),
            "nothing is answered until the spool has taken the transfer: {offered:?}"
        );

        let accepted = engine.on_file_admitted(id, Ok(()));
        assert!(
            matches!(sent(&accepted).as_slice(), [OutboundMessage::Accept(_)]),
            "an admitted offer is accepted: {accepted:?}"
        );

        let mut spooled = Vec::new();
        let mut after_write = Vec::new();
        for chunk in chunk_content(id, bytes).unwrap() {
            let taken = engine.on_peer_message(InboundMessage::Chunk(chunk));
            spooled.extend_from_slice(&written_chunk(&taken));
            after_write = engine.on_file_chunk_written(id);
        }

        let (from, to) = after_write
            .iter()
            .find_map(|action| match action {
                Action::CommitFile { from, to, .. } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the last write should commit, got {after_write:?}"));
        assert_eq!(from, part, "the commit promotes the partial it was given");

        // A promoted entry is not a delivery until something can paste
        // it, so the commit asks for the offer and the verdict waits.
        let committed = engine.on_file_committed(id, true);
        assert!(
            sent(&committed).is_empty(),
            "the origin was answered before the file could be pasted: {committed:?}"
        );
        let offered = file_offer_of(&committed);
        assert_eq!(offered.entry, to);
        (engine.on_file_offered(id, Ok(())), spooled, to)
    }

    fn file_offer_of(actions: &[Action]) -> SpooledFile {
        actions
            .iter()
            .find_map(|action| match action {
                Action::OfferFile { file, .. } => Some(file.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected an offer, got {actions:?}"))
    }

    fn evicted_entries(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::EvictSpoolEntry { entry } => Some(entry.clone()),
                _ => None,
            })
            .collect()
    }

    /// The whole receiving path: nothing is answered before the spool has
    /// room, the bytes are written through rather than buffered, and the
    /// entry appears only once the item has verified.
    #[test]
    fn a_granted_file_is_streamed_to_the_spool_and_registered() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(200_000);

        let (closed, spooled, entry) = receive_file(&mut engine, "quarterly.pdf", &bytes, 1);

        assert_eq!(verdict(&closed), ApplyResult::Stored);
        assert_eq!(spooled, bytes, "the spool receives the item, byte for byte");
        let registered: Vec<&super::SpooledFile> = engine.spooled_files().collect();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].entry, entry);
        assert_eq!(registered[0].byte_len, bytes.len() as u64);
        assert_eq!(registered[0].content_hash, content_hash(&bytes));
        assert_eq!(registered[0].descriptor.file_name, "quarterly.pdf");
    }

    /// The peer's name is metadata and never a filesystem name: the entry
    /// is ours, and the descriptor carries theirs (ADR 0015).
    #[test]
    fn the_peers_name_never_becomes_the_entry_name() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);

        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "report.pdf")));
        let (_, part, _) = admission_of(&offered);
        let stem = part
            .strip_suffix(".part")
            .unwrap_or_else(|| panic!("a partial is named <id>.part: {part}"));
        assert!(
            !part.contains("report"),
            "the partial is named after the peer's file: {part}"
        );
        assert!(
            Uuid::parse_str(stem).is_ok(),
            "the partial is not named by a locally generated id: {part}"
        );

        let (_, _, entry) = receive_file(&mut engine, "report.pdf", &bytes, 2);
        assert!(
            entry
                .strip_suffix(".bin")
                .is_some_and(|stem| Uuid::parse_str(stem).is_ok()),
            "an entry is named <id>.bin: {entry}"
        );
        assert!(!entry.contains("report"), "{entry}");
    }

    /// Default-off, and the two refusals are different answers: no grant
    /// is a permission the user can give, no spool never will be.
    #[test]
    fn a_file_offer_is_refused_without_a_grant_or_a_spool() {
        let bytes = image_bytes(4096);

        // The engine's own default, before anything configures it.
        let mut fresh = engine(0xAA);
        let offered = fresh.on_peer_message(InboundMessage::Offer(file_offer(
            file_meta(&bytes, 1),
            "payload.exe",
        )));
        assert_eq!(declined_reason(&offered), DeclineReason::UnsupportedType);
        assert!(
            !offered
                .iter()
                .any(|a| matches!(a, Action::AdmitFile { .. })),
            "a refused offer must not touch the spool: {offered:?}"
        );

        let mut denied = engine(0xAA);
        denied.set_file_receive(FileReceive::Denied);
        let offered = denied.on_peer_message(InboundMessage::Offer(file_offer(
            file_meta(&bytes, 1),
            "payload.exe",
        )));
        assert_eq!(declined_reason(&offered), DeclineReason::NotPermitted);
        assert_eq!(denied.spooled_files().len(), 0);
    }

    /// Withdrawing the grant stops the next transfer without waiting for
    /// a reconnect — the reason `set_file_receive` is a policy input
    /// rather than a constructor argument.
    #[test]
    fn withdrawing_the_grant_refuses_the_next_offer() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);
        let (closed, _, _) = receive_file(&mut engine, "first.bin", &bytes, 1);
        assert_eq!(verdict(&closed), ApplyResult::Stored);

        engine.set_file_receive(FileReceive::Denied);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(
            file_meta(&bytes, 2),
            "second.bin",
        )));
        assert_eq!(declined_reason(&offered), DeclineReason::NotPermitted);
        // Already-delivered entries stay: revocation stops the next
        // transfer, it does not reach back into the spool (ADR 0015, T20).
        assert_eq!(engine.spooled_files().len(), 1);
    }

    /// The admission answer is what decides the offer, and each refusal
    /// keeps its own meaning on the wire (NFR-3).
    #[test]
    fn an_admission_refusal_declines_with_the_reason_that_is_true() {
        for (refusal, expected) in [
            (
                FileRefusal::InsufficientSpace,
                DeclineReason::InsufficientSpace,
            ),
            (FileRefusal::Storage, DeclineReason::NotReady),
        ] {
            let mut engine = granted(0xAA);
            let bytes = image_bytes(4096);
            let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(
                file_meta(&bytes, 1),
                "big.zip",
            )));
            let (id, _, _) = admission_of(&offered);
            let refused = engine.on_file_admitted(id, Err(refusal));
            assert_eq!(declined_reason(&refused), expected);
            assert_eq!(engine.spooled_files().len(), 0);

            // The slot is free again: the next offer is judged on its own
            // merits rather than inheriting a refusal.
            let next = engine.on_peer_message(InboundMessage::Offer(file_offer(
                file_meta(&bytes, 2),
                "next.zip",
            )));
            admission_of(&next);
        }
    }

    /// An item no spool could hold is refused before room is made for it.
    /// Unreachable through the wire today — `MAX_CLIPBOARD_FILE_BYTES` is
    /// the smaller ceiling — and kept because the spool budget is the one
    /// a receiver may lower (ADR 0015).
    #[test]
    fn an_offer_larger_than_the_whole_spool_budget_is_refused() {
        let mut engine = granted(0xAA);
        let mut meta = file_meta(b"pretend", 1);
        meta.content_length = MAX_SPOOL_BYTES + 1;
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "huge.zip")));
        assert_eq!(declined_reason(&offered), DeclineReason::TooLarge);
    }

    /// Content that is not what was offered never becomes an entry: the
    /// partial is deleted, the origin is told, and the peer is charged a
    /// violation (docs/PROTOCOL.md §7).
    #[test]
    fn a_corrupted_file_transfer_registers_nothing() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(200_000);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, part, _) = admission_of(&offered);
        engine.on_file_admitted(id, Ok(()));

        let mut chunks = chunk_content(id, &bytes).unwrap();
        let taken = engine.on_peer_message(InboundMessage::Chunk(chunks[0].clone()));
        written_chunk(&taken);
        engine.on_file_chunk_written(id);

        // The tail of the item, with one byte of it changed: every length
        // still reconciles, so only the hash can catch this.
        let last = chunks.len() - 1;
        chunks[last].payload[0] ^= 0xFF;
        for chunk in &chunks[1..] {
            let outcome = engine.on_peer_message(InboundMessage::Chunk(chunk.clone()));
            if !outcome.is_empty()
                && outcome
                    .iter()
                    .any(|a| matches!(a, Action::AbortFile { .. }))
            {
                assert_eq!(aborted_entry(&outcome), part);
                assert_eq!(verdict(&outcome), ApplyResult::StorageFailed);
                assert_eq!(engine.spooled_files().len(), 0);
                return;
            }
            engine.on_file_chunk_written(id);
        }
        panic!("a tampered transfer completed");
    }

    /// A write that does not land ends the transfer as surely as a bad
    /// chunk does — and it is *this* machine's fault, so the origin hears
    /// it without the peer being charged anything.
    #[test]
    fn a_failed_spool_write_abandons_the_transfer() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(200_000);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, part, _) = admission_of(&offered);
        engine.on_file_admitted(id, Ok(()));
        let chunks = chunk_content(id, &bytes).unwrap();
        engine.on_peer_message(InboundMessage::Chunk(chunks[0].clone()));

        let failed = engine.on_file_write_failed(id);
        assert_eq!(aborted_entry(&failed), part);
        assert_eq!(verdict(&failed), ApplyResult::StorageFailed);
        assert_eq!(engine.spooled_files().len(), 0);
    }

    /// The rename is the moment bytes become an entry, so a rename that
    /// fails registers nothing and says so.
    #[test]
    fn a_commit_that_fails_registers_nothing() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, part, _) = admission_of(&offered);
        engine.on_file_admitted(id, Ok(()));
        for chunk in chunk_content(id, &bytes).unwrap() {
            engine.on_peer_message(InboundMessage::Chunk(chunk));
            engine.on_file_chunk_written(id);
        }

        let failed = engine.on_file_committed(id, false);
        assert_eq!(aborted_entry(&failed), part);
        assert_eq!(verdict(&failed), ApplyResult::StorageFailed);
        assert_eq!(engine.spooled_files().len(), 0);
    }

    /// A partial must not outlive the transaction that created it, and a
    /// peer that is gone is owed no verdict.
    #[test]
    fn a_lost_session_deletes_the_partial_without_answering() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(200_000);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, part, _) = admission_of(&offered);
        engine.on_file_admitted(id, Ok(()));
        engine.on_peer_message(InboundMessage::Chunk(
            chunk_content(id, &bytes).unwrap()[0].clone(),
        ));

        let lost = engine.on_session_lost();
        assert_eq!(aborted_entry(&lost), part);
        assert!(sent(&lost).is_empty(), "nobody is there to tell: {lost:?}");
        assert_eq!(engine.spooled_files().len(), 0);
    }

    /// A transfer that stops halfway costs a bounded amount of disk for a
    /// bounded time: the deadline deletes the partial and closes the
    /// origin's transaction (ADR 0014's bound, ADR 0015's surface).
    #[test]
    fn the_deadline_abandons_a_stalled_file_transfer() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(200_000);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, part, _) = admission_of(&offered);
        let generation = offered
            .iter()
            .find_map(|action| match action {
                Action::ScheduleTransferTimeout {
                    scope: TransferScope::Inbound,
                    generation,
                    ..
                } => Some(*generation),
                _ => None,
            })
            .expect("an admitted transfer is armed with a deadline");
        engine.on_file_admitted(id, Ok(()));

        let expired = engine.on_transfer_timeout(TransferScope::Inbound, generation);
        assert_eq!(aborted_entry(&expired), part);
        assert_eq!(verdict(&expired), ApplyResult::StorageFailed);
    }

    /// One transfer at a time (`MAX_CONCURRENT_FILE_TRANSFERS`), and the
    /// superseded one leaves nothing behind: the peer holds a single
    /// outbound transaction, so a second offer means the first is already
    /// abandoned at its origin.
    #[test]
    fn a_newer_offer_supersedes_a_file_transfer_and_deletes_its_partial() {
        assert_eq!(MAX_CONCURRENT_FILE_TRANSFERS, 1);
        let mut engine = granted(0xAA);
        let bytes = image_bytes(200_000);
        let first = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(first, "first.pdf")));
        let (id, part, _) = admission_of(&offered);
        engine.on_file_admitted(id, Ok(()));
        engine.on_peer_message(InboundMessage::Chunk(
            chunk_content(id, &bytes).unwrap()[0].clone(),
        ));

        let second = file_meta(&bytes, 2);
        let superseding =
            engine.on_peer_message(InboundMessage::Offer(file_offer(second, "second.pdf")));
        assert_eq!(aborted_entry(&superseding), part);
        let (next_id, next_part, _) = admission_of(&superseding);
        assert_eq!(next_id, second.id);
        assert_ne!(next_part, part, "a new transfer gets a new partial");
        assert!(
            sent(&superseding).is_empty(),
            "the abandoned transfer's origin has already dropped it: {superseding:?}"
        );
    }

    /// Chunks for a transfer this side has not accepted are neither
    /// written nor tolerated: the partial goes and the peer is charged.
    #[test]
    fn chunks_ahead_of_the_acceptance_abandon_the_transfer() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, part, _) = admission_of(&offered);

        // No `on_file_admitted` yet: the offer is still unanswered.
        let early = engine.on_peer_message(InboundMessage::Chunk(
            chunk_content(id, &bytes).unwrap()[0].clone(),
        ));
        assert!(
            !early
                .iter()
                .any(|a| matches!(a, Action::WriteFileChunk { .. })),
            "a chunk arriving before the accept must not be written: {early:?}"
        );
        assert_eq!(aborted_entry(&early), part);
        assert_eq!(verdict(&early), ApplyResult::StorageFailed);
    }

    /// The spool holds a bounded number of entries, and room is made
    /// *before* the transfer that needs it — oldest first, and every
    /// eviction is an action the driver can actually perform.
    #[test]
    fn the_entry_budget_evicts_the_oldest_to_admit_a_new_file() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(1024);
        let mut entries = Vec::new();
        for sequence in 0..MAX_SPOOL_ENTRIES as u64 {
            let (_, _, entry) = receive_file(&mut engine, "doc.pdf", &bytes, sequence);
            entries.push(entry);
        }
        assert_eq!(engine.spooled_files().len(), MAX_SPOOL_ENTRIES);

        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(
            file_meta(&bytes, 99),
            "one-more.pdf",
        )));
        let evicted: Vec<String> = offered
            .iter()
            .filter_map(|action| match action {
                Action::EvictSpoolEntry { entry } => Some(entry.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            evicted,
            vec![entries[0].clone()],
            "exactly the oldest entry makes way"
        );
        assert_eq!(engine.spooled_files().len(), MAX_SPOOL_ENTRIES - 1);
    }

    /// A file nobody can paste is not a delivery: the verdict waits for
    /// the offer, and an offer that never lands takes the entry with it
    /// rather than leaving peer bytes resting on disk for nothing.
    #[test]
    fn a_file_that_cannot_be_offered_is_deleted_and_reported_failed() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, _, _) = admission_of(&offered);
        engine.on_file_admitted(id, Ok(()));
        for chunk in chunk_content(id, &bytes).unwrap() {
            engine.on_peer_message(InboundMessage::Chunk(chunk));
            engine.on_file_chunk_written(id);
        }
        let committed = engine.on_file_committed(id, true);
        let entry = file_offer_of(&committed).entry;

        let failed = engine.on_file_offered(id, Err(WriteFailure::Unavailable));
        assert_eq!(verdict(&failed), ApplyResult::StorageFailed);
        assert_eq!(evicted_entries(&failed), vec![entry]);
        assert_eq!(engine.spooled_files().len(), 0);
    }

    /// Offering takes the machine-global clipboard lock like any other
    /// write, so contention is retried on the same bounded schedule
    /// (FR-3.4) rather than costing the transfer.
    #[test]
    fn a_busy_clipboard_retries_the_offer_before_giving_up() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);
        let meta = file_meta(&bytes, 1);
        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(meta, "doc.pdf")));
        let (id, _, _) = admission_of(&offered);
        engine.on_file_admitted(id, Ok(()));
        for chunk in chunk_content(id, &bytes).unwrap() {
            engine.on_peer_message(InboundMessage::Chunk(chunk));
            engine.on_file_chunk_written(id);
        }
        engine.on_file_committed(id, true);

        let busy = engine.on_file_offered(id, Err(WriteFailure::Busy));
        assert!(
            matches!(busy.as_slice(), [Action::ScheduleRetry { id: retry, .. }] if *retry == id),
            "a busy clipboard should schedule a retry, got {busy:?}"
        );
        assert!(
            sent(&busy).is_empty(),
            "the origin heard a verdict too early"
        );

        // The retry re-offers the same entry, and success closes the
        // transaction normally.
        let again = engine.on_retry_due(id);
        file_offer_of(&again);
        let stored = engine.on_file_offered(id, Ok(()));
        assert_eq!(verdict(&stored), ApplyResult::Stored);
        assert_eq!(engine.spooled_files().len(), 1);
    }

    /// The entry-lifetime rule (ADR 0015): an entry lives while the
    /// clipboard still offers what it backs, and is collected the moment
    /// the clipboard moves on — not on a timer that could delete
    /// something the user was still about to paste.
    #[test]
    fn the_entry_is_collected_when_the_clipboard_moves_on() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);
        let (closed, _, entry) = receive_file(&mut engine, "doc.pdf", &bytes, 1);
        assert_eq!(verdict(&closed), ApplyResult::Stored);
        assert_eq!(engine.spooled_files().len(), 1);

        // While it is still on offer, nothing collects it — this is the
        // half a TTL gets wrong.
        assert!(engine.on_spool_sweep_due().is_empty());
        assert_eq!(engine.spooled_files().len(), 1);

        let moved_on = engine.on_clipboard_moved_on();
        assert_eq!(evicted_entries(&moved_on), vec![entry]);
        assert_eq!(engine.spooled_files().len(), 0);
        // And it is idempotent: a second local copy collects nothing,
        // because there is nothing left to collect.
        assert!(engine.on_clipboard_moved_on().is_empty());
    }

    /// The backstop behind that rule: an entry whose clipboard was never
    /// observed to move on goes on age, and its promise is withdrawn
    /// first so the shell is never left holding one nothing can serve.
    #[test]
    fn an_unobserved_entry_is_swept_on_age_and_its_offer_withdrawn() {
        let mut engine = ClipboardEngine::new(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig {
                // Everything is instantly "old", which is the only way to
                // reach a 24-hour backstop in a unit test.
                spool_sweep_ttl: Duration::ZERO,
                ..ClipboardConfig::new()
            },
        );
        engine.set_file_receive(FileReceive::Allowed);
        let bytes = image_bytes(4096);
        let (_, _, entry) = receive_file(&mut engine, "doc.pdf", &bytes, 1);

        let swept = engine.on_spool_sweep_due();
        assert!(
            swept.contains(&Action::WithdrawFileOffer),
            "an entry still on the clipboard was deleted without withdrawing it: {swept:?}"
        );
        assert_eq!(evicted_entries(&swept), vec![entry]);
        assert_eq!(engine.spooled_files().len(), 0);
    }

    /// Eviction for budget normally takes an entry nothing is offering.
    /// When it cannot — the offered entry *is* the oldest — the promise
    /// goes with the bytes, because a virtual file list whose entry has
    /// been deleted fails at paste time, in the shell, with nothing from
    /// us to explain it.
    #[test]
    fn evicting_the_offered_entry_withdraws_the_offer_with_it() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(1024);
        let mut entries = Vec::new();
        for sequence in 0..MAX_SPOOL_ENTRIES as u64 {
            // Each delivery replaces the last on the clipboard, so only
            // the newest is offered; the oldest is what the budget takes.
            let (_, _, entry) = receive_file(&mut engine, "doc.pdf", &bytes, sequence);
            entries.push(entry);
        }

        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(
            file_meta(&bytes, 99),
            "one-more.pdf",
        )));
        assert_eq!(evicted_entries(&offered), vec![entries[0].clone()]);
        assert!(
            !offered.contains(&Action::WithdrawFileOffer),
            "an entry nothing was offering took the clipboard with it: {offered:?}"
        );
    }

    /// Files never take the `AlreadyHave` shortcut: a spool entry is not
    /// what the clipboard holds, so claiming to have one would refuse an
    /// offer this machine may no longer be able to paste
    /// (docs/PROTOCOL.md §5).
    #[test]
    fn a_file_offer_is_never_declined_as_already_held() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);

        // Put that exact content on the local clipboard first, which for
        // an image would produce an `AlreadyHave` decline.
        copy_image(&mut engine, bytes.clone());
        engine.on_session_lost();

        let offered = engine.on_peer_message(InboundMessage::Offer(file_offer(
            file_meta(&bytes, 9),
            "doc.pdf",
        )));
        admission_of(&offered);
    }

    /// A file that completes goes into the applied-hash memory like any
    /// other delivered item (ADR 0015's third loop-prevention layer), so a
    /// platform that re-reads delivered content as bytes cannot echo it
    /// back to the peer that sent it.
    #[test]
    fn a_stored_file_is_remembered_as_applied_content() {
        let mut engine = granted(0xAA);
        let bytes = image_bytes(4096);
        let (closed, _, _) = receive_file(&mut engine, "doc.pdf", &bytes, 1);
        assert_eq!(verdict(&closed), ApplyResult::Stored);

        // Reading those same bytes back off the local clipboard must not
        // start an outbound transaction.
        let echoed = copy_image(&mut engine, bytes);
        assert!(
            sent(&echoed).is_empty(),
            "delivered content was offered back to its origin: {echoed:?}"
        );
    }

    // ---- the sending half of files (ADR 0015, "Sender side") ----

    /// An engine configured the way the application configures one when
    /// the peer advertised `FILE_CLIPBOARD` and holds `clipboard_send`.
    fn sender(origin_fill: u8) -> ClipboardEngine {
        let mut engine = engine(origin_fill);
        engine.set_file_send(FileSend::Allowed);
        engine
    }

    fn selection(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// An absolute spool root for *this* platform.
    ///
    /// The guard turns on `Path::is_absolute`, which is a per-platform
    /// question: a Windows path is one undivided component on Unix and
    /// not absolute there, so a test written in one dialect would assert
    /// the fallback rather than the rule on the other two OSes the CI
    /// gate runs.
    fn spool_root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\test\AppData\Local\Crossover\spool")
        } else {
            PathBuf::from("/home/test/.local/share/crossover/spool")
        }
    }

    /// A path somewhere else entirely, absolute on this platform.
    fn elsewhere(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\work").join(name)
        } else {
            PathBuf::from("/home/test/work").join(name)
        }
    }

    /// The same path, shouted: the comparison must not turn on case.
    fn shout(path: &Path) -> PathBuf {
        PathBuf::from(path.to_string_lossy().to_uppercase())
    }

    /// Copy a file selection locally, through the same trigger every
    /// other observation goes through.
    fn copy_files(engine: &mut ClipboardEngine, paths: &[&str]) -> Vec<Action> {
        engine.on_local_change();
        engine.on_settle_due();
        engine.on_local_read(Some(ClipboardContent::FileList(selection(paths))))
    }

    /// The one build the actions asked for.
    fn build_of(actions: &[Action]) -> (Uuid, Vec<PathBuf>) {
        actions
            .iter()
            .find_map(|action| match action {
                Action::BuildFileBlob { id, selection } => Some((*id, selection.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a build action, got {actions:?}"))
    }

    fn releases(actions: &[Action]) -> Vec<Uuid> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::ReleaseFileBlob { id } => Some(*id),
                _ => None,
            })
            .collect()
    }

    fn file_chunks(actions: &[Action]) -> Vec<(u32, u64, u32)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::SendFileChunk {
                    index, offset, len, ..
                } => Some((*index, *offset, *len)),
                _ => None,
            })
            .collect()
    }

    /// [`MAX_CHUNK_BYTES`] as the `u32` the plan and the offsets use.
    fn chunk_bytes() -> u32 {
        u32::try_from(MAX_CHUNK_BYTES).expect("the chunk size fits a u32")
    }

    fn blob(name: &str, len: u64) -> BuiltBlob {
        BuiltBlob {
            proposed_name: name.to_owned(),
            naming: BlobNaming::Own,
            archived: false,
            entry_count: 1,
            original_bytes: len,
            content_length: len,
            content_hash: content_hash(&image_bytes(usize::try_from(len).unwrap_or(0))),
        }
    }

    /// Copy a selection and answer its build, returning `(id, actions)`.
    fn pack(engine: &mut ClipboardEngine, name: &str, len: u64) -> (Uuid, Vec<Action>) {
        let staged = copy_files(engine, &[r"C:\work\report.pdf"]);
        let (id, _) = build_of(&staged);
        let offered = engine.on_file_blob_built(id, Ok(blob(name, len)));
        (id, offered)
    }

    /// Drive an accepted file transfer to its last chunk, the way the
    /// driver does: one chunk, then ask for the next.
    fn drain_file_chunks(
        engine: &mut ClipboardEngine,
        id: Uuid,
    ) -> (Vec<(u32, u64, u32)>, Vec<Action>) {
        let mut chunks = Vec::new();
        let mut actions = engine.on_peer_message(InboundMessage::Accept(ClipboardAccept { id }));
        loop {
            let emitted = file_chunks(&actions);
            if emitted.is_empty() {
                return (chunks, actions);
            }
            chunks.extend(emitted);
            assert!(chunks.len() <= 8192, "the chunk stream never terminated");
            actions = engine.on_chunk_sent(id);
        }
    }

    /// The gate order the ADR requires: nothing is walked, packed or
    /// written for a peer that could not take the result anyway. Each
    /// closed gate is a *refusal*, observable rather than silent (FR-3.6).
    #[test]
    fn a_selection_is_never_walked_for_a_peer_that_cannot_take_it() {
        for policy in [
            FileSend::Unsupported,
            FileSend::NotNegotiated,
            FileSend::Denied,
        ] {
            let mut engine = engine(0xAA);
            engine.set_file_send(policy);
            let actions = copy_files(&mut engine, &[r"C:\work\report.pdf"]);
            assert!(
                actions.is_empty(),
                "{policy:?} produced work before the gate: {actions:?}"
            );
        }
    }

    /// The one bound judgeable without touching a disk bites before the
    /// walk; the rest are the builder's, during it.
    #[test]
    fn a_selection_past_the_entry_cap_is_refused_before_the_walk() {
        let mut engine = sender(0xAA);
        let paths: Vec<String> = (0..=MAX_CLIPBOARD_FILE_ENTRIES)
            .map(|i| format!(r"C:\work\{i}.bin"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        assert!(copy_files(&mut engine, &refs).is_empty());

        // One fewer is inside the cap and does start a build.
        let mut engine = sender(0xAA);
        let refs: Vec<&str> = refs[1..].to_vec();
        build_of(&copy_files(&mut engine, &refs));
    }

    /// A refused build is reported and leaves nothing behind: no offer,
    /// no retained state, and the next selection works normally.
    #[test]
    fn a_refused_build_surfaces_and_leaves_nothing_staged() {
        let mut engine = sender(0xAA);
        let staged = copy_files(&mut engine, &[r"C:\work\photos"]);
        let (id, paths) = build_of(&staged);
        assert_eq!(paths, selection(&[r"C:\work\photos"]));

        let refused = engine.on_file_blob_built(id, Err(FileBlobRefusal::ReparsePoint));
        assert!(
            sent(&refused).is_empty() && releases(&refused).is_empty(),
            "a refused build must not offer anything or pin a blob: {refused:?}"
        );
        // And the machine is clean: an ordinary text copy still travels.
        assert_eq!(sent(&copy(&mut engine, "unaffected")).len(), 1);
    }

    /// The happy path, end to end: offer, accept, chunks named by offset
    /// and length, then the blob handed back the moment the last one is
    /// out — before the verdict, because the bytes are no longer needed.
    #[test]
    fn a_packed_selection_is_offered_then_streamed_a_chunk_at_a_time() {
        let mut engine = sender(0xAA);
        let bytes = u64::from(chunk_bytes()) * 2 + 17;
        let (id, offered) = pack(&mut engine, "report.pdf", bytes);

        let offer = offer_of(&offered);
        assert_eq!(offer.meta.id, id);
        assert_eq!(offer.meta.content_type, ContentType::File);
        assert_eq!(offer.meta.content_length, bytes);
        let descriptor = offer.descriptor.clone().expect("a file offer carries one");
        assert_eq!(descriptor.file_name, "report.pdf");
        assert_eq!(descriptor.entry_count, 1);
        assert!(!descriptor.archived);
        offer.validate().expect("the offer must be conforming");
        assert!(
            releases(&offered).is_empty(),
            "the blob is needed until the last chunk is out"
        );

        let (chunks, closing) = drain_file_chunks(&mut engine, id);
        assert_eq!(
            chunks,
            vec![
                (0, 0, chunk_bytes()),
                (1, u64::from(chunk_bytes()), chunk_bytes()),
                (2, u64::from(chunk_bytes()) * 2, 17),
            ],
            "every chunk must name exactly its own slice, and nothing more"
        );

        // The confirmation of the last chunk releases the blob and leaves
        // only the verdict outstanding.
        assert_eq!(releases(&closing), vec![id]);
        assert!(engine.on_chunk_sent(id).is_empty());

        let closed = engine.on_peer_message(InboundMessage::Applied(ClipboardApplied {
            id,
            result: ApplyResult::Stored,
        }));
        assert!(
            closed.is_empty(),
            "a stored file needs nothing more: {closed:?}"
        );
    }

    /// The engine never holds the item: what it emits per chunk is a
    /// slice *description*, so a 256 MiB selection costs it nothing
    /// beyond the plan (ADR 0015's O(chunk) rule, sending side).
    #[test]
    fn the_engine_never_carries_a_byte_of_the_item() {
        let mut engine = sender(0xAA);
        let bytes = crossover_protocol::clipboard::MAX_CLIPBOARD_FILE_BYTES as u64;
        let (id, offered) = pack(&mut engine, "big.zip", bytes);
        assert_eq!(sent(&offered).len(), 1);

        let (chunks, closing) = drain_file_chunks(&mut engine, id);
        assert_eq!(releases(&closing), vec![id]);
        assert_eq!(
            chunks.len(),
            usize::try_from(bytes.div_ceil(u64::from(chunk_bytes()))).unwrap()
        );
        // Contiguous, non-overlapping, and exactly covering the item.
        let mut expected_offset = 0u64;
        for (index, offset, len) in &chunks {
            assert_eq!(
                *offset, expected_offset,
                "chunk {index} starts in the wrong place"
            );
            expected_offset += u64::from(*len);
        }
        assert_eq!(expected_offset, bytes);
    }

    /// Hash dedup is the peer's to claim, and when it does the transfer
    /// costs zero payload bytes — the same success-shaped decline an
    /// image gets. Our own receiver never sends it for a file (ADR 0015),
    /// but a peer's may, and dedup is a delivery rather than a failure.
    #[test]
    fn a_peer_that_already_holds_the_file_costs_zero_chunks() {
        let mut engine = sender(0xAA);
        let (id, offered) = pack(&mut engine, "report.pdf", 4096);
        assert_eq!(sent(&offered).len(), 1);

        let closed = engine.on_peer_message(InboundMessage::Decline(ClipboardDecline {
            id,
            reason: DeclineReason::AlreadyHave,
        }));
        assert!(
            file_chunks(&closed).is_empty(),
            "a dedup decline must move no payload bytes: {closed:?}"
        );
        assert_eq!(
            releases(&closed),
            vec![id],
            "the blob must be handed back on a decline"
        );
        assert!(engine.on_chunk_sent(id).is_empty());
    }

    /// Every typed decline ends the transaction and drops the blob,
    /// whatever the reason was.
    #[test]
    fn a_declined_file_releases_its_blob_before_a_chunk_is_read() {
        for reason in [
            DeclineReason::NotPermitted,
            DeclineReason::InsufficientSpace,
            DeclineReason::TooLarge,
            DeclineReason::NotReady,
            DeclineReason::UnsupportedType,
            DeclineReason::InvalidName,
            DeclineReason::Superseded,
        ] {
            let mut engine = sender(0xAA);
            let (id, _) = pack(&mut engine, "report.pdf", 4096);
            let closed =
                engine.on_peer_message(InboundMessage::Decline(ClipboardDecline { id, reason }));
            assert_eq!(releases(&closed), vec![id], "{reason:?} pinned the blob");
            assert!(file_chunks(&closed).is_empty(), "{reason:?} moved bytes");
        }
    }

    /// Reject, never repair: a name the *selection* gave itself and that
    /// cannot travel refuses the item rather than arriving under a name
    /// nobody picked. A derived name falls back instead.
    #[test]
    fn a_name_the_user_chose_that_cannot_travel_refuses_the_item() {
        let mut engine = sender(0xAA);
        let staged = copy_files(&mut engine, &[r"C:\work\report.pdf"]);
        let (id, _) = build_of(&staged);
        let mut hostile = blob("invoice\u{202e}gnp.exe", 4096);
        hostile.naming = BlobNaming::Own;
        let refused = engine.on_file_blob_built(id, Ok(hostile));
        assert!(sent(&refused).is_empty(), "a hostile name reached the wire");
        assert_eq!(releases(&refused), vec![id]);

        // The same name, *derived* from a folder nobody named for this
        // purpose, falls back rather than refusing.
        let mut engine = sender(0xAA);
        let staged = copy_files(&mut engine, &[r"C:\work\a", r"C:\work\b"]);
        let (id, _) = build_of(&staged);
        let mut derived = blob("invoice\u{202e}gnp.zip", 4096);
        derived.naming = BlobNaming::Derived;
        derived.archived = true;
        derived.entry_count = 2;
        let offered = engine.on_file_blob_built(id, Ok(derived));
        let offer = offer_of(&offered);
        assert_eq!(
            offer.descriptor.expect("descriptor").file_name,
            crate::file_blob::FALLBACK_ARCHIVE_NAME
        );
        offer.meta.validate().expect("conforming meta");
    }

    /// The protocol's own bounds are re-checked here rather than left for
    /// the encoder to discover (NFR-1: validate before the wire).
    #[test]
    fn an_empty_or_oversized_blob_never_becomes_an_offer() {
        for length in [
            0,
            crossover_protocol::clipboard::MAX_CLIPBOARD_FILE_BYTES as u64 + 1,
        ] {
            let mut engine = sender(0xAA);
            let staged = copy_files(&mut engine, &[r"C:\work\report.pdf"]);
            let (id, _) = build_of(&staged);
            let refused = engine.on_file_blob_built(id, Ok(blob("report.pdf", length)));
            assert!(sent(&refused).is_empty(), "{length} bytes was offered");
            assert_eq!(releases(&refused), vec![id]);
        }
    }

    /// A `CF_HDROP` pointing back into the spool is our own delivered item
    /// coming round again, and offering it back is FR-3.3's loop on the
    /// largest payload type in the system (ADR 0015 layer 2, F13).
    #[test]
    fn a_selection_inside_the_spool_is_never_staged() {
        let root = spool_root();
        let mut engine = sender(0xAA);
        engine.set_spool_root(Some(root.clone()));

        let inside = [
            root.join("3f2a.bin"),
            // Case is not a distinction the filesystem this guards makes.
            shout(&root).join("3f2a.bin"),
            // The root itself.
            root.clone(),
            // Unjudgeable without resolving it: treated as ours.
            root.join("..").join("spool").join("3f2a.bin"),
            PathBuf::from("spool").join("3f2a.bin"),
        ];
        for path in inside {
            let actions =
                engine.on_local_read(Some(ClipboardContent::FileList(vec![path.clone()])));
            assert!(
                actions.is_empty(),
                "{} was staged for sending: {actions:?}",
                path.display()
            );
        }

        // A sibling directory whose name merely starts the same way is
        // *not* inside it — component-wise, not a string prefix.
        let sibling = root
            .parent()
            .expect("the spool root has a parent")
            .join("spool-backup")
            .join("note.txt");
        build_of(&engine.on_local_read(Some(ClipboardContent::FileList(vec![sibling]))));

        // One path inside the spool poisons the whole selection: one
        // clipboard item is one blob, so it cannot be sent minus that
        // entry without sending something the user did not select.
        let mixed = engine.on_local_read(Some(ClipboardContent::FileList(vec![
            elsewhere("report.pdf"),
            root.join("3f2a.bin"),
        ])));
        assert!(
            mixed.is_empty(),
            "a partly-ours selection was staged: {mixed:?}"
        );
    }

    /// With no spool there is nothing of ours on disk for a selection to
    /// point at, so the guard is vacuous rather than closed.
    #[test]
    fn without_a_spool_no_path_is_ours() {
        let mut engine = sender(0xAA);
        build_of(&copy_files(&mut engine, &[r"C:\anything\at\all.bin"]));
    }

    /// A newer local copy supersedes a build in flight, and the answer to
    /// the superseded build is released rather than offered — the race
    /// the driver cannot resolve on its own.
    #[test]
    fn a_newer_copy_supersedes_a_build_and_its_answer_is_released() {
        let mut engine = sender(0xAA);
        let first = copy_files(&mut engine, &[r"C:\work\a.pdf"]);
        let (first_id, _) = build_of(&first);

        let second = copy_files(&mut engine, &[r"C:\work\b.pdf"]);
        assert_eq!(
            releases(&second),
            vec![first_id],
            "the superseded build must give its artifact back"
        );
        let (second_id, _) = build_of(&second);
        assert_ne!(first_id, second_id);

        // The late answer to the first build is released, not offered.
        let late = engine.on_file_blob_built(first_id, Ok(blob("a.pdf", 4096)));
        assert!(sent(&late).is_empty());
        assert_eq!(releases(&late), vec![first_id]);

        // The second still offers normally.
        let offered = engine.on_file_blob_built(second_id, Ok(blob("b.pdf", 4096)));
        assert_eq!(offer_of(&offered).meta.id, second_id);
    }

    /// A newer local copy of *anything* supersedes a file transfer in
    /// flight, and the blob goes with it.
    #[test]
    fn a_newer_local_copy_supersedes_a_file_transfer_and_releases_its_blob() {
        let mut engine = sender(0xAA);
        let (id, _) = pack(&mut engine, "report.pdf", 4096);
        let text = copy(&mut engine, "something else entirely");
        assert_eq!(releases(&text), vec![id]);
        assert_eq!(sent(&text).len(), 1);
    }

    /// A build that never answers has its own deadline, on its own
    /// generation — arming it must not disarm an unrelated transfer that
    /// is still in flight.
    #[test]
    fn a_build_that_never_answers_is_abandoned_on_its_own_deadline() {
        let mut engine = sender(0xAA);
        // An image transfer is in flight and keeps its own deadline.
        let image = copy_image(&mut engine, image_bytes(4096));
        let image_deadline = timeout_of(&image);
        assert_eq!(image_deadline.0, TransferScope::Outbound);

        let staged = copy_files(&mut engine, &[r"C:\work\report.pdf"]);
        let (id, _) = build_of(&staged);
        let build_deadline = timeout_of(&staged);
        assert_eq!(build_deadline.0, TransferScope::Build);

        // The image's deadline still fires: the build did not steal it.
        let expired = engine.on_transfer_timeout(image_deadline.0, image_deadline.1);
        assert!(!expired.is_empty() || engine.outbound.is_none());

        let abandoned = engine.on_transfer_timeout(build_deadline.0, build_deadline.1);
        assert_eq!(releases(&abandoned), vec![id]);
        // A late answer is released rather than offered.
        let late = engine.on_file_blob_built(id, Ok(blob("report.pdf", 4096)));
        assert!(sent(&late).is_empty());
        assert_eq!(releases(&late), vec![id]);
    }

    /// An offered file nobody answers expires like any other transaction,
    /// and here the deadline is protecting the sender's *disk* rather
    /// than its memory.
    #[test]
    fn the_deadline_releases_a_blob_nothing_answered_for() {
        let mut engine = sender(0xAA);
        let (id, offered) = pack(&mut engine, "report.pdf", 4096);
        let (scope, generation) = timeout_of(&offered);
        assert_eq!(scope, TransferScope::Outbound);
        let expired = engine.on_transfer_timeout(scope, generation);
        assert_eq!(releases(&expired), vec![id]);
    }

    /// The session going takes the artifact with it, from either state:
    /// a build in flight, or an offer already out.
    #[test]
    fn session_loss_releases_whatever_the_sending_half_was_holding() {
        let mut engine = sender(0xAA);
        let staged = copy_files(&mut engine, &[r"C:\work\report.pdf"]);
        let (building_id, _) = build_of(&staged);
        assert_eq!(releases(&engine.on_session_lost()), vec![building_id]);

        let mut engine = sender(0xAA);
        let (id, _) = pack(&mut engine, "report.pdf", 4096);
        assert_eq!(releases(&engine.on_session_lost()), vec![id]);

        // And re-establishing does the same for a build the gap orphaned.
        let mut engine = sender(0xAA);
        let staged = copy_files(&mut engine, &[r"C:\work\report.pdf"]);
        let (orphan, _) = build_of(&staged);
        assert_eq!(releases(&engine.on_session_established()), vec![orphan]);
    }

    /// Losing the conflict race is an exit path like any other, and the
    /// one that does not go through `outbound.take()` at its call site.
    #[test]
    fn losing_the_conflict_race_releases_the_blob() {
        let mut engine = sender(0xAA);
        let (id, offered) = pack(&mut engine, "report.pdf", 4096);
        let ours = offer_of(&offered).meta;

        // A peer item with a higher (sequence, origin) wins.
        let theirs = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xFF; 16]),
            sequence: ours.sequence + 1,
            content_type: ContentType::Utf8Text,
            content_length: 128,
            content_hash: content_hash(&[7; 128]),
        };
        let raced = engine.on_peer_message(InboundMessage::Offer(ClipboardOffer {
            meta: theirs,
            descriptor: None,
        }));
        assert_eq!(releases(&raced), vec![id]);
    }

    /// A file this engine just applied is not offered back to the peer
    /// that sent it — ADR 0015's third loop-prevention layer, on the
    /// sending side this time.
    #[test]
    fn a_file_we_just_applied_is_never_offered_back() {
        let mut engine = sender(0xAA);
        engine.set_file_receive(FileReceive::Allowed);
        let bytes = image_bytes(4096);
        let (closed, _, _) = receive_file(&mut engine, "doc.pdf", &bytes, 1);
        assert_eq!(verdict(&closed), ApplyResult::Stored);

        // The same bytes, packed from a local copy of the pasted file.
        let staged = copy_files(&mut engine, &[r"C:\work\doc.pdf"]);
        let (id, _) = build_of(&staged);
        let mut same = blob("doc.pdf", bytes.len() as u64);
        same.content_hash = content_hash(&bytes);
        let suppressed = engine.on_file_blob_built(id, Ok(same));
        assert!(
            sent(&suppressed).is_empty(),
            "a delivered file was offered back to its origin: {suppressed:?}"
        );
        assert_eq!(releases(&suppressed), vec![id]);
    }

    /// A blob that cannot be read back ends the transfer rather than
    /// sending half an item, and the artifact goes.
    #[test]
    fn a_blob_that_cannot_be_read_ends_the_transfer() {
        let mut engine = sender(0xAA);
        let (id, _) = pack(&mut engine, "report.pdf", u64::from(chunk_bytes()) * 2);
        let accepted = engine.on_peer_message(InboundMessage::Accept(ClipboardAccept { id }));
        assert_eq!(file_chunks(&accepted).len(), 1);

        let failed = engine.on_file_read_failed(id);
        assert_eq!(releases(&failed), vec![id]);
        assert!(
            engine.on_chunk_sent(id).is_empty(),
            "the stream must be over"
        );
    }

    /// The sending half's outcomes are counted where FR-3.6 needs them:
    /// a refusal here is not "nothing happened".
    #[test]
    fn the_sending_half_counts_what_it_refused_and_what_it_sent() {
        let metrics = Arc::new(Metrics::new());
        let mut engine = connected(ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        ));

        // Gated off: refused, not silent.
        engine.set_file_send(FileSend::NotNegotiated);
        assert!(copy_files(&mut engine, &[r"C:\work\a.pdf"]).is_empty());
        assert_eq!(metrics.snapshot().clipboard_files_send_refused, 1);

        // Allowed, built, offered.
        engine.set_file_send(FileSend::Allowed);
        let (id, offered) = pack(&mut engine, "report.pdf", 4096);
        assert_eq!(sent(&offered).len(), 1);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.clipboard_files_sent, 1);
        assert_eq!(snapshot.clipboard_file_sent_bytes, 4096);

        // Declined for a real reason: a delivery that did not happen.
        engine.on_peer_message(InboundMessage::Decline(ClipboardDecline {
            id,
            reason: DeclineReason::NotPermitted,
        }));
        assert_eq!(metrics.snapshot().clipboard_files_send_failed, 1);
    }

    /// No peer is not a refusal (ADR 0006 addendum), and a selection
    /// copied alone must not be walked or packed.
    ///
    /// The application's own policy already closes to `Denied` with
    /// nothing live, so the expensive build was never at risk — but
    /// answering an empty desk with "this peer holds no clipboard-send
    /// grant" names a peer that does not exist, and charges
    /// `files_send_refused` for a permission nobody was asked for. The
    /// gate is judged before the policy so the diagnostic is true.
    #[test]
    fn a_file_selection_copied_with_no_peer_is_held_not_refused() {
        let metrics = Arc::new(Metrics::new());
        let mut engine = ClipboardEngine::with_metrics(
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig::new(),
            Some(Arc::clone(&metrics)),
        );
        // Even fully permitted, there is nobody to offer it to.
        engine.set_file_send(FileSend::Allowed);

        let actions = copy_files(&mut engine, &[r"C:\work\a.pdf"]);
        assert!(
            actions.is_empty(),
            "a file copy with no peer asked for work: {actions:?}"
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.clipboard_offline_changes, 1);
        assert_eq!(
            snapshot.clipboard_files_send_refused, 0,
            "an absent peer was reported as a refused one"
        );
    }
}
