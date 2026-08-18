//! Async driver for the clipboard engine: the thin I/O shell around the
//! pure state machine in [`crate::clipboard`].
//!
//! The driver owns nothing clever — that is the point. It bridges the
//! provider's change callback into its event loop, executes engine
//! [`Action`]s (provider reads/writes, frame sends, retry timers) with a
//! work queue, and forwards decoded peer frames in. All policy lives in
//! the engine; everything here is mechanical and replaceable.
//!
//! Fail-closed lever: a clipboard-typed frame whose payload fails
//! validation produces [`SessionCommand::TerminateSession`] — the app kills
//! that session (docs/PROTOCOL.md §7) and supervision reconnects.
//!
//! One thing here is *not* mechanical, and it is the reason
//! [`ClipboardSyncDriver::send_command`] exists rather than a bare
//! `send().await`: a driver parked on send backpressure stops consuming
//! its own event channel, and with a chunk stream that lasts long enough
//! to matter, that turns four independently-correct pieces of
//! backpressure into a session that wedges instead of failing closed. The
//! full cycle is written out on that method.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crossover_platform::{
    ClipboardError, ClipboardProvider, SpoolError, SpoolStorage, VirtualFile, VirtualFileClipboard,
};
use crossover_protocol::RawFrame;
use crossover_protocol::clipboard::{ApplyResult, ClipboardApplied};
use crossover_protocol::hello::MessageType;

use crate::clipboard::{
    Action, ClipboardConfig, ClipboardEngine, FileReceive, FileRefusal, InboundMessage,
    MIN_FREE_SPACE_MARGIN_BYTES, OutboundMessage, SpooledFile, TransferScope, WriteFailure,
};
use crate::command::{FrameTarget, SessionCommand};
use crate::metrics::Metrics;
use crate::outbound::{CommandReceiver, CommandSender, command_lanes};

/// How long after a `Busy` *read* before re-checking the clipboard. Reads
/// have no transaction to retry inside the engine; the driver simply
/// looks again shortly (the next change notification would also do it).
const READ_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Upper bound on events drained in one coalescing pass, so a flood
/// cannot stall the loop (NFR-1).
const MAX_COALESCE_BATCH: usize = 512;

/// How many consecutive `Busy` reads before the driver stops re-nudging
/// itself and waits for the next real change notification.
///
/// Found in the two-machine soak (docs/SOAK.md): with the local
/// clipboard under sustained contention, an unbounded nudge cycle
/// re-enqueues itself indefinitely, and because inbound frames share
/// this one serial event queue, a peer's acknowledgement can sit
/// unprocessed behind the churn — 27 seconds of it, in the run that
/// exposed this. Bounding the cycle costs nothing real: the clipboard
/// listener will notify us again for any change we miss, so giving up
/// here loses no content, only a redundant look.
const MAX_CONSECUTIVE_BUSY_READS: u32 = 5;

/// How many events the driver may hold aside while it is parked on send
/// backpressure (see [`ClipboardSyncDriver::send_command`]).
///
/// Deferring is what keeps the driver a live consumer of its own event
/// channel while it waits for lane room, which is the link it owns in the
/// wedge cycle documented there. It is bounded, and bounded at the
/// channel's own depth on purpose: draining a full channel into this queue
/// moves the same events to a different place rather than admitting more
/// of them, so the worst case is unchanged in order of magnitude (NFR-1).
const MAX_DEFERRED_EVENTS: usize = 64;

/// Depth of the driver's inbound event channel. Named because
/// [`MAX_DEFERRED_EVENTS`] is deliberately equal to it, and because the two
/// together are the driver's deafness threshold — the number of events it
/// can absorb while parked before backpressure starts propagating outwards
/// (docs/ARCHITECTURE.md §5.4).
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// What became of a command handed to the send path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendOutcome {
    /// Queued for the wire.
    Sent,
    /// The session was lost while it waited for room, so it was dropped
    /// rather than queued — nothing is left to send it to.
    Abandoned,
    /// The consuming side is gone: the app is shutting down.
    Closed,
}

/// Events the app (or the driver itself) feeds in.
#[derive(Debug)]
pub enum SyncEvent {
    /// A session to the peer reached `ESTABLISHED`.
    SessionEstablished,
    /// The session ended (any reason); in-flight sync state is void.
    SessionLost,
    /// A frame arrived on the session (any type; non-clipboard frames are
    /// ignored here).
    Frame(RawFrame),
    /// The local clipboard may have changed (listener bridge; coalesced).
    LocalChanged,
    /// A scheduled write retry came due.
    RetryDue(Uuid),
    /// The settle window elapsed (ADR 0006): time to read.
    SettleDue(u64),
    /// The spool's age backstop came due (ADR 0015).
    SpoolSweepDue,
    /// A transfer deadline came due (ADR 0014).
    TransferTimeout {
        /// Which half of the transaction machine it covers.
        scope: TransferScope,
        /// Which transfer it belongs to; a stale one is a no-op.
        generation: u64,
    },
    /// Whether peer files may be received (ADR 0015), as the application
    /// currently reads the trust store.
    ///
    /// An event rather than construction-time configuration because the
    /// answer changes while the process runs: `crossover peers deny-files`
    /// happens in another process, and the running one re-reads the store
    /// on its revocation poll. Sending the policy in stops the *next*
    /// transfer without waiting for a reconnect.
    FileReceivePolicy(FileReceive),
}

/// The clipboard sync driver. Create with [`clipboard_sync`], then spawn
/// [`ClipboardSyncDriver::run`].
pub struct ClipboardSyncDriver {
    engine: ClipboardEngine,
    provider: Arc<dyn ClipboardProvider>,
    /// The protected spool peer files are written into (ADR 0015), or
    /// `None` where this build has none. `None` is not a degraded mode:
    /// the engine is told file receive is unsupported and every offer is
    /// refused, because an unprotected fallback would void the security
    /// claim the spool exists to make.
    spool: Option<Arc<dyn SpoolStorage>>,
    /// The open partial of the transfer in flight, keyed by transaction.
    /// One at a time, because the engine admits one at a time.
    file_write: Option<(Uuid, std::fs::File)>,
    /// Where a verified file is offered for paste (ADR 0015), or `None`
    /// where this build has no such mechanism. Separate from `provider`
    /// because on Windows it owns an apartment thread of its own, which
    /// the ADR requires rather than prefers.
    virtual_files: Option<Arc<dyn VirtualFileClipboard>>,
    events_rx: mpsc::Receiver<SyncEvent>,
    events_tx: mpsc::Sender<SyncEvent>,
    commands_tx: CommandSender,
    /// Consecutive `Busy` reads; reset by any successful read.
    busy_reads: u32,
    /// Actions still to perform, carried across turns of the event loop
    /// so a long chunk stream cannot monopolize the driver (see `run`).
    pending: VecDeque<Action>,
    /// Events taken off the channel while parked on send backpressure,
    /// waiting to be processed in the order they arrived. Bounded by
    /// [`MAX_DEFERRED_EVENTS`].
    deferred: VecDeque<SyncEvent>,
    /// Generation of the newest settle timer; older ones are ignored
    /// when they fire, which is how the debounce restarts cleanly
    /// without cancelling tasks.
    settle_generation: u64,
    /// Optional metrics sink for the physical I/O outcomes only the driver
    /// sees — `Busy` contention and write retries. The semantic outcomes
    /// (sent, applied, superseded, latency) are recorded inside the engine
    /// itself, which owns those decisions.
    metrics: Option<Arc<Metrics>>,
}

/// Build a driver for `provider`, returning the handles the app uses:
/// an event sender (session lifecycle + frames) and the command receiver.
///
/// # Errors
///
/// [`ClipboardError::Unavailable`] if change observation cannot be
/// established — fatal by contract, because silent non-observation would
/// be silent sync failure (NFR-3).
pub fn clipboard_sync(
    provider: Arc<dyn ClipboardProvider>,
    spool: Option<Arc<dyn SpoolStorage>>,
    virtual_files: Option<Arc<dyn VirtualFileClipboard>>,
    origin: Uuid,
    config: ClipboardConfig,
    metrics: Option<Arc<Metrics>>,
) -> Result<
    (
        ClipboardSyncDriver,
        mpsc::Sender<SyncEvent>,
        CommandReceiver,
    ),
    ClipboardError,
> {
    let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    // Two lanes, not one queue: a driver parked on bulk backpressure must
    // still have a clear path for its fail-closed termination (ADR 0013).
    let (commands_tx, commands_rx) = command_lanes();

    // Bridge the dataless provider callback into the event loop.
    // try_send + drop-on-full IS the documented coalescing: a full queue
    // already holds a pending "look at the clipboard" signal.
    let notify = events_tx.clone();
    provider.set_change_listener(Some(Box::new(move || {
        let _ = notify.try_send(SyncEvent::LocalChanged);
    })))?;

    let driver = ClipboardSyncDriver {
        engine: ClipboardEngine::with_metrics(origin, config, metrics.clone()),
        provider,
        spool,
        file_write: None,
        virtual_files,
        events_rx,
        events_tx: events_tx.clone(),
        commands_tx,
        pending: VecDeque::new(),
        deferred: VecDeque::new(),
        busy_reads: 0,
        settle_generation: 0,
        metrics,
    };
    Ok((driver, events_tx, commands_rx))
}

impl ClipboardSyncDriver {
    /// Record into the metrics sink if one is attached; a no-op otherwise.
    fn record(&self, f: impl FnOnce(&Metrics)) {
        if let Some(metrics) = &self.metrics {
            f(metrics);
        }
    }

    /// Run until every event sender is dropped. Spawn this.
    ///
    /// One turn of the loop takes **at most one coalesce batch** (up to
    /// [`MAX_COALESCE_BATCH`] events, all of them fed to the engine) **and
    /// performs at most one action**. The action half is the load-bearing
    /// one, and it is a bound on *monopoly*, not on throughput: an action
    /// can produce the next action — a chunk send asks the engine for the
    /// following chunk — so draining actions to exhaustion would run an
    /// entire image transfer inside one turn, with the event channel unread
    /// for its whole duration. That is how a long transfer turns into a deaf
    /// driver, and a deaf driver is one hop of the wedge cycle documented on
    /// [`Self::send_command`]. Alternating costs a `try_recv` per action and
    /// bounds the delay on any event at one action.
    ///
    /// The event half is deliberately *not* one-at-a-time: coalescing is
    /// what stops a backlog of clipboard items becoming a burst of writes to
    /// a machine-global lock, and it consumes a bounded batch without
    /// performing any action, so it cannot monopolise anything.
    pub async fn run(mut self) {
        loop {
            // An event that is already waiting is taken first; the loop
            // only *blocks* for one when there is no work to do.
            let ready = self
                .deferred
                .pop_front()
                .or_else(|| self.events_rx.try_recv().ok());
            let event = match ready {
                Some(event) => Some(event),
                None if !self.pending.is_empty() => None,
                None => match self.events_rx.recv().await {
                    Some(event) => Some(event),
                    None => break,
                },
            };

            if let Some(event) = event {
                // Coalesce before acting. The OS clipboard is a
                // single-value register, not a queue: when several items
                // are already waiting, applying the older ones writes
                // content nobody can ever paste, and every wasted write
                // takes the machine-global clipboard lock — which is how
                // Crossover made other applications' clipboard calls fail
                // in the two-machine soak (52 writes in one second while
                // draining a backlog).
                let batch = self.coalesce(event).await;
                for event in batch {
                    let actions = self.dispatch(event).await;
                    // Appended, never prepended: outbound order is what
                    // makes the newest item the last one the peer sees.
                    self.pending.extend(actions);
                }
            }

            if let Some(action) = self.pending.pop_front()
                && !self.perform(action).await
            {
                return; // command receiver gone: the app is shutting down
            }
        }
        tracing::debug!("clipboard sync driver stopped");
    }

    /// Feed one event to the engine and return what it wants done.
    async fn dispatch(&mut self, event: SyncEvent) -> Vec<Action> {
        match event {
            SyncEvent::SessionEstablished => self.engine.on_session_established(),
            SyncEvent::SessionLost => self.engine.on_session_lost(),
            SyncEvent::LocalChanged => self.on_local_change(),
            SyncEvent::SpoolSweepDue => self.engine.on_spool_sweep_due(),
            SyncEvent::RetryDue(id) => self.engine.on_retry_due(id),
            SyncEvent::TransferTimeout { scope, generation } => {
                self.engine.on_transfer_timeout(scope, generation)
            }
            SyncEvent::FileReceivePolicy(policy) => {
                // Clamped, not merely forwarded: a file needs somewhere
                // to land *and* somewhere to be pasted from, and without
                // both this build cannot deliver one whatever the trust
                // store says. The permission and the capability are
                // separate answers, and the closed one wins.
                let policy = if self.spool.is_some() && self.virtual_files.is_some() {
                    policy
                } else {
                    FileReceive::Unsupported
                };
                self.engine.set_file_receive(policy);
                Vec::new()
            }
            SyncEvent::SettleDue(generation) => {
                if generation == self.settle_generation {
                    self.engine.on_settle_due()
                } else {
                    Vec::new() // a newer local change restarted the timer
                }
            }
            SyncEvent::Frame(frame) => {
                match InboundMessage::decode(frame.message_type, &frame.payload) {
                    Ok(Some(message)) => self.engine.on_peer_message(message),
                    Ok(None) => Vec::new(), // not clipboard traffic
                    Err(error) => {
                        // Peer nonconformance: fail closed. Clipboard is
                        // session-agnostic and does not track which session
                        // a frame arrived on, so the fail-closed kill is a
                        // broadcast (two-machine: the one session).
                        self.terminate_session(error.to_string()).await;
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Drain what is immediately available and drop superseded clipboard
    /// items, acknowledging each honestly rather than silently.
    ///
    /// Only *inbound items* coalesce. Everything else — acks, session
    /// lifecycle, retries, local-change nudges — passes through in order,
    /// because dropping any of those would lose state rather than lose a
    /// value that was already stale.
    async fn coalesce(&mut self, first: SyncEvent) -> Vec<SyncEvent> {
        let mut batch = vec![first];
        while batch.len() < MAX_COALESCE_BATCH {
            // Deferred events were taken off the channel earlier, so they
            // precede anything still on it — order is preserved across the
            // two sources, not just within each.
            if let Some(event) = self.deferred.pop_front() {
                batch.push(event);
                continue;
            }
            match self.events_rx.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break, // empty or closed; closed is handled by run()
            }
        }
        if batch.len() == 1 {
            return batch;
        }

        // Index of the last inbound clipboard item in the batch: every
        // earlier one is stale before it is ever applied.
        let last_data = batch.iter().rposition(|event| {
            matches!(event, SyncEvent::Frame(frame)
                if frame.message_type == MessageType::ClipboardData.wire())
        });
        let Some(last_data) = last_data else {
            return batch;
        };

        let mut kept = Vec::with_capacity(batch.len());
        let mut superseded = 0usize;
        for (index, event) in batch.into_iter().enumerate() {
            let is_stale_item = index < last_data
                && matches!(&event, SyncEvent::Frame(frame)
                    if frame.message_type == MessageType::ClipboardData.wire());
            if !is_stale_item {
                kept.push(event);
                continue;
            }
            // Acknowledge the item we are deliberately not applying. The
            // origin learns the truth — a newer item won — instead of
            // waiting for a verdict that never comes.
            let SyncEvent::Frame(frame) = event else {
                continue;
            };
            if let Ok(Some(InboundMessage::Data(data))) =
                InboundMessage::decode(frame.message_type, &frame.payload)
            {
                let applied = ClipboardApplied {
                    id: data.meta.id,
                    result: ApplyResult::Superseded,
                };
                if let Ok(payload) = applied.encode_payload() {
                    let _ = self
                        .commands_tx
                        .send(SessionCommand::SendFrame {
                            target: FrameTarget::Broadcast,
                            message_type: MessageType::ClipboardApplied.wire(),
                            payload,
                        })
                        .await;
                }
                superseded += 1;
                self.record(Metrics::record_clipboard_superseded);
            }
        }
        if superseded > 0 {
            tracing::debug!(
                superseded,
                "coalesced stale inbound clipboard items; applying only the newest"
            );
        }
        kept
    }

    /// Pull the fail-closed lever: the same session kill a malformed
    /// payload triggers (docs/PROTOCOL.md §7), reached instead by
    /// repetition of individually-survivable violations. `false` once the
    /// app side is gone.
    async fn terminate_session(&mut self, reason: String) -> bool {
        tracing::warn!(error = %reason, "terminating session on clipboard violations");
        self.send_command(SessionCommand::TerminateSession {
            target: FrameTarget::Broadcast,
            reason,
        })
        .await
            != SendOutcome::Closed
    }

    /// Hand one command to the send path **without going deaf.**
    ///
    /// A plain `send().await` parks until the lane has room, and a parked
    /// driver consumes no [`SyncEvent`]s. That is fine for one frame and
    /// fatal for a chunk stream, because it closes a four-hop cycle with
    /// no timeout anywhere on it:
    ///
    /// 1. this driver parks on a saturated Background lane, so it stops
    ///    consuming its own event channel;
    /// 2. that channel (bounded) fills, so the app's fan-out parks trying
    ///    to deliver the next frame to it;
    /// 3. the fan-out holding still means the session-events channel fills;
    /// 4. `run_session`'s frame dispatch parks on *that* — inside the one
    ///    `select!` that also drains the outbound lane and answers
    ///    keepalives — so the writer stops, the lane never drains, and (1)
    ///    holds forever.
    ///
    /// Every hop is legitimate backpressure; together they are a wedge
    /// rather than a fail-closed disconnect, because the keepalive tick
    /// that would notice is in the loop that stopped turning.
    ///
    /// This driver owns exactly one link of that cycle — the first — so
    /// this is where it is broken: while waiting for lane room, keep
    /// taking events off the channel (bounded by
    /// [`MAX_DEFERRED_EVENTS`], so nothing here is unbounded), and if the
    /// session is *lost* while waiting, stop waiting. A frame queued for a
    /// session that no longer exists is not worth parking for.
    async fn send_command(&mut self, command: SessionCommand) -> SendOutcome {
        // Cloned so the in-flight send borrows nothing of `self`, leaving
        // the event channel free to be drained alongside it.
        let sender = self.commands_tx.clone();
        let send = sender.send(command);
        tokio::pin!(send);
        loop {
            tokio::select! {
                // Biased: making progress on the send is the point; the
                // event drain is what keeps us reachable meanwhile.
                biased;
                result = &mut send => {
                    return if result.is_ok() { SendOutcome::Sent } else { SendOutcome::Closed };
                }
                event = self.events_rx.recv(), if self.deferred.len() < MAX_DEFERRED_EVENTS => {
                    let Some(event) = event else {
                        // Unreachable while the driver holds a sender of
                        // its own; finish the send rather than spin.
                        return if send.await.is_ok() {
                            SendOutcome::Sent
                        } else {
                            SendOutcome::Closed
                        };
                    };
                    let lost = matches!(event, SyncEvent::SessionLost);
                    self.deferred.push_back(event);
                    // The only place the queue grows, so the high-water
                    // mark is exact: how close a run came to the bound
                    // above, reportable at shutdown (FR-7.3, NFR-1).
                    let depth = self.deferred.len();
                    self.record(|metrics| metrics.record_deferred_depth(depth));
                    if lost {
                        tracing::debug!(
                            deferred = self.deferred.len(),
                            "session lost while queueing a frame; abandoning it"
                        );
                        return SendOutcome::Abandoned;
                    }
                }
            }
        }
    }

    /// Read the provider and feed the result back, absorbing contention
    /// with the bounded nudge cycle the soak forced (see
    /// [`MAX_CONSECUTIVE_BUSY_READS`]).
    fn read_clipboard(&mut self) -> Vec<Action> {
        match self.provider.read() {
            Ok(content) => {
                self.busy_reads = 0;
                self.engine.on_local_read(content)
            }
            Err(ClipboardError::Busy { reason }) => {
                self.busy_reads += 1;
                self.record(Metrics::record_clipboard_contention);
                if self.busy_reads > MAX_CONSECUTIVE_BUSY_READS {
                    // Stop nudging: the change listener will wake us for
                    // anything that actually changes, and continuing would
                    // starve inbound frames on this same queue.
                    tracing::warn!(
                        error = %reason,
                        attempt_count = self.busy_reads,
                        "clipboard read still busy; waiting for the next change \
                         notification instead of re-checking"
                    );
                } else {
                    tracing::debug!(
                        error = %reason,
                        attempt_count = self.busy_reads,
                        "clipboard read busy; will look again"
                    );
                    let notify = self.events_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(READ_RETRY_DELAY).await;
                        let _ = notify.try_send(SyncEvent::LocalChanged);
                    });
                }
                Vec::new()
            }
            Err(error) => {
                tracing::error!(error = %error, "clipboard read failed");
                Vec::new()
            }
        }
    }

    /// Hand one engine message to the send path.
    ///
    /// One chunk per command, on purpose: a chunk is ADR 0013's preemption
    /// unit, and [`Self::send_command`] awaits the Background lane's byte
    /// budget, so a streaming transfer is paced by the wire rather than
    /// racing ahead of the input it must never delay.
    async fn send_message(&mut self, message: OutboundMessage) -> (SendOutcome, Vec<Action>) {
        let (message_type, payload) = match message.encode() {
            Ok(encoded) => encoded,
            Err(error) => {
                // Engine-built messages are always valid; log the
                // impossible rather than panic (NFR-1 discipline).
                tracing::error!(error = %error, "unencodable engine message dropped");
                return (SendOutcome::Sent, Vec::new());
            }
        };
        let streamed = match &message {
            OutboundMessage::Chunk(chunk) => Some(chunk.id),
            _ => None,
        };
        let outcome = self
            .send_command(SessionCommand::SendFrame {
                target: FrameTarget::Broadcast,
                message_type,
                payload,
            })
            .await;
        if outcome != SendOutcome::Sent {
            return (outcome, Vec::new());
        }
        (
            outcome,
            match streamed {
                // Only now, with the chunk genuinely on the path, is the
                // next one asked for: the stream advances at the rate the
                // wire accepts it.
                Some(id) => self.engine.on_chunk_sent(id),
                None => Vec::new(),
            },
        )
    }

    /// A local clipboard change, judged before it is acted on.
    ///
    /// Two things are decided here, and both need an answer the engine
    /// cannot produce, because both are about an object only the platform
    /// layer can recognize:
    ///
    /// - **Loop prevention** (F13). Our own virtual file list raises a
    ///   change notification exactly as any write does, and staging it
    ///   would offer the file straight back to the peer that sent it —
    ///   FR-3.3's loop, on the largest payload type in the system. Asking
    ///   whether the clipboard still holds *our* object settles it without
    ///   reading anything or rendering a byte, which is why this guard
    ///   fires before the read rather than after it.
    /// - **Entry lifetime** (ADR 0015). A change that is *not* ours means
    ///   the clipboard has moved on, so the entry behind the item it was
    ///   offering can no longer be pasted and is collected.
    fn on_local_change(&mut self) -> Vec<Action> {
        if self
            .virtual_files
            .as_ref()
            .is_some_and(|files| files.is_current())
        {
            self.record(Metrics::record_clipboard_loop_suppressed);
            tracing::debug!("clipboard change is our own virtual file list; not staging it");
            return Vec::new();
        }
        let mut actions = self.engine.on_clipboard_moved_on();
        actions.extend(self.engine.on_local_change());
        actions
    }

    /// Offer a verified entry for paste, and report what the clipboard
    /// said back into the engine.
    fn offer_file(&mut self, id: Uuid, file: &SpooledFile) -> Vec<Action> {
        let Some(files) = &self.virtual_files else {
            // No paste mechanism in this build. The engine deletes the
            // entry rather than leaving bytes nothing can reach.
            return self
                .engine
                .on_file_offered(id, Err(WriteFailure::UnsupportedType));
        };
        let offer = VirtualFile {
            entry: file.entry.clone(),
            file_name: file.descriptor.file_name.clone(),
            byte_len: file.byte_len,
        };
        let result = match files.offer(&offer) {
            Ok(()) => Ok(()),
            Err(ClipboardError::Busy { reason }) => {
                tracing::debug!(clipboard_id = %id, error = %reason, "offer busy");
                self.record(Metrics::record_clipboard_contention);
                Err(WriteFailure::Busy)
            }
            Err(error @ ClipboardError::Unsupported { .. }) => {
                tracing::warn!(clipboard_id = %id, error = %error, "virtual file paste unsupported");
                Err(WriteFailure::UnsupportedType)
            }
            Err(error) => {
                tracing::warn!(clipboard_id = %id, error = %error, "offering a file failed");
                Err(WriteFailure::Unavailable)
            }
        };
        self.engine.on_file_offered(id, result)
    }

    /// Feed `event` back into the loop after `delay`.
    ///
    /// Every timer here has the same shape and the same non-guarantee: a
    /// task that outlives its reason fires into a no-op, because each
    /// event carries the generation or the id that makes it stale — so
    /// nothing has to be cancelled.
    fn schedule(&self, delay: Duration, event: SyncEvent) {
        let notify = self.events_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = notify.send(event).await;
        });
    }

    /// Take our virtual file list off the clipboard.
    fn withdraw_file_offer(&self) {
        let Some(files) = &self.virtual_files else {
            return;
        };
        if let Err(error) = files.withdraw() {
            // Not fatal: the entry is going regardless, and the worst
            // case is a promise the shell finds it cannot serve — which
            // fails observably rather than producing wrong bytes.
            tracing::warn!(error = %error, "withdrawing the file offer failed");
        }
    }

    /// Reserve room for an offered file and open the partial it streams
    /// into (ADR 0015).
    ///
    /// Space is checked *before* the partial exists, and the margin is
    /// part of the check: a transfer that would leave the volume with no
    /// headroom is refused rather than started, because filling a user's
    /// system volume is a fault of its own and the refusal costs the
    /// origin one frame (FR-3.6).
    fn admit_file(&mut self, id: Uuid, entry: &str, byte_len: u64) -> Vec<Action> {
        let outcome = self.reserve_partial(id, entry, byte_len);
        self.engine.on_file_admitted(id, outcome)
    }

    /// The spool half of [`Self::admit_file`]: the checks, and the open
    /// partial if they all pass.
    fn reserve_partial(&mut self, id: Uuid, entry: &str, byte_len: u64) -> Result<(), FileRefusal> {
        let Some(spool) = &self.spool else {
            return Err(FileRefusal::Storage);
        };
        // A partial left open from an earlier transfer would keep a handle
        // on an entry the engine has already abandoned. Closed here, so
        // the slot is genuinely free before another is opened.
        self.file_write = None;

        let free = spool.free_bytes().map_err(|error| {
            tracing::warn!(error = %error, "spool free space could not be read; refusing the transfer");
            FileRefusal::Storage
        })?;
        let required = byte_len.saturating_add(MIN_FREE_SPACE_MARGIN_BYTES);
        if free < required {
            tracing::warn!(
                clipboard_id = %id,
                byte_count = byte_len,
                free_bytes = free,
                required_bytes = required,
                "declining a file offer: not enough room on the spool volume"
            );
            return Err(FileRefusal::InsufficientSpace);
        }
        match spool.create_entry(entry) {
            Ok(file) => {
                self.file_write = Some((id, file));
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    clipboard_id = %id,
                    spool_entry = %entry,
                    error = %error,
                    "the spool partial could not be created"
                );
                Err(FileRefusal::Storage)
            }
        }
    }

    /// Append one chunk to the open partial. `false` means the transfer
    /// is over: the engine deletes the partial and answers the origin.
    fn write_file_chunk(&mut self, id: Uuid, payload: &[u8]) -> Vec<Action> {
        if self.append_to_partial(id, payload) {
            self.engine.on_file_chunk_written(id)
        } else {
            self.engine.on_file_write_failed(id)
        }
    }

    /// The write itself. `false` means the bytes did not land.
    fn append_to_partial(&mut self, id: Uuid, payload: &[u8]) -> bool {
        use std::io::Write;

        let Some((open_id, file)) = self.file_write.as_mut() else {
            tracing::warn!(clipboard_id = %id, "file chunk with no open spool partial");
            return false;
        };
        if *open_id != id {
            tracing::warn!(clipboard_id = %id, "file chunk for a partial that is not open");
            return false;
        }
        if let Err(error) = file.write_all(payload) {
            tracing::warn!(clipboard_id = %id, error = %error, "writing to the spool partial failed");
            return false;
        }
        true
    }

    /// Promote the verified partial to a spool entry.
    ///
    /// Flushed and **closed first**: the rename is what makes the bytes
    /// advertisable, and promoting an entry whose last write is still in a
    /// buffer would register something that is not yet the item.
    fn commit_file(&mut self, id: Uuid, from: &str, to: &str) -> Vec<Action> {
        let stored = self.promote_partial(id, from, to);
        self.engine.on_file_committed(id, stored)
    }

    /// The promotion itself. `false` means nothing was registered.
    fn promote_partial(&mut self, id: Uuid, from: &str, to: &str) -> bool {
        use std::io::Write;

        let Some(spool) = &self.spool else {
            return false;
        };
        match self.file_write.take() {
            Some((open_id, mut file)) if open_id == id => {
                if let Err(error) = file.flush() {
                    tracing::warn!(clipboard_id = %id, error = %error, "flushing the spool partial failed");
                    return false;
                }
                drop(file);
            }
            other => {
                self.file_write = other;
                tracing::warn!(clipboard_id = %id, "commit for a partial that is not open");
                return false;
            }
        }
        match spool.rename_entry(from, to) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    clipboard_id = %id,
                    spool_entry = %to,
                    error = %error,
                    "the verified partial could not be promoted to a spool entry"
                );
                false
            }
        }
    }

    /// Close and remove the partial for an abandoned transfer.
    ///
    /// Best-effort by design and idempotent underneath (`unlink_entry`
    /// succeeds on an absent name), because this runs on every failure
    /// path including ones where the partial was never created.
    fn abort_file(&mut self, id: Uuid, entry: &str) {
        if self
            .file_write
            .as_ref()
            .is_some_and(|(open, _)| *open == id)
        {
            self.file_write = None; // close before unlinking
        }
        let Some(spool) = &self.spool else {
            return;
        };
        // A partial that outlives its transfer is the one thing this path
        // exists to prevent, so a failure to remove it is a warning rather
        // than a debug note: it names bytes left on disk that nothing will
        // now collect until the next startup sweep. `Unsupported` is not
        // one of those — there is no spool, so there is no partial.
        match spool.unlink_entry(entry) {
            Ok(()) | Err(SpoolError::Unsupported) => {}
            Err(error) => tracing::warn!(
                clipboard_id = %id,
                spool_entry = %entry,
                error = %error,
                "the abandoned spool partial could not be removed"
            ),
        }
    }

    /// Remove a completed entry the spool budget evicted.
    fn evict_entry(&mut self, entry: &str) {
        let Some(spool) = &self.spool else {
            return;
        };
        if let Err(error) = spool.unlink_entry(entry) {
            // Not fatal and not silent: the engine has already dropped it
            // from the budget, so the honest record is a warning naming
            // what is still on disk.
            tracing::warn!(
                spool_entry = %entry,
                error = %error,
                "evicted spool entry could not be removed"
            );
        }
    }

    /// Perform one engine action, queueing whatever it produces.
    ///
    /// Deliberately **one** action, not a drain: see [`Self::run`] for why
    /// the loop alternates. Returns `false` when the app side is gone.
    async fn perform(&mut self, action: Action) -> bool {
        match action {
            Action::ReadClipboard => {
                let more = self.read_clipboard();
                self.pending.extend(more);
            }
            Action::WriteClipboard { id, content } => {
                let result = match self.provider.write(&content) {
                    Ok(()) => Ok(()),
                    Err(ClipboardError::Busy { reason }) => {
                        tracing::debug!(clipboard_id = %id, error = %reason, "write busy");
                        self.record(Metrics::record_clipboard_contention);
                        Err(WriteFailure::Busy)
                    }
                    Err(error @ ClipboardError::Unsupported { .. }) => {
                        tracing::warn!(
                            clipboard_id = %id,
                            error = %error,
                            "clipboard content type not supported by this platform"
                        );
                        Err(WriteFailure::UnsupportedType)
                    }
                    Err(error) => {
                        tracing::warn!(clipboard_id = %id, error = %error, "write failed");
                        Err(WriteFailure::Unavailable)
                    }
                };
                let more = self.engine.on_write_result(id, result);
                self.pending.extend(more);
            }
            Action::Send(message) => match self.send_message(message).await {
                (SendOutcome::Sent, more) => self.pending.extend(more),
                (SendOutcome::Closed, _) => return false,
                // The session died while this frame waited for room; the
                // deferred `SessionLost` is the next event the loop takes,
                // and the engine's own cleanup makes the rest moot.
                (SendOutcome::Abandoned, _) => {}
            },
            Action::ScheduleRetry { id, delay } => {
                self.record(Metrics::record_clipboard_retry);
                self.schedule(delay, SyncEvent::RetryDue(id));
            }
            Action::TerminateSession { reason } => {
                if !self.terminate_session(reason).await {
                    return false;
                }
            }
            Action::ScheduleTransferTimeout {
                scope,
                generation,
                delay,
            } => self.schedule(delay, SyncEvent::TransferTimeout { scope, generation }),
            Action::AdmitFile {
                id,
                entry,
                byte_len,
            } => {
                let more = self.admit_file(id, &entry, byte_len);
                self.pending.extend(more);
            }
            Action::WriteFileChunk { id, payload } => {
                let more = self.write_file_chunk(id, &payload);
                self.pending.extend(more);
            }
            Action::CommitFile { id, from, to } => {
                let more = self.commit_file(id, &from, &to);
                self.pending.extend(more);
            }
            Action::AbortFile { id, entry } => self.abort_file(id, &entry),
            Action::OfferFile { id, file } => {
                let more = self.offer_file(id, &file);
                self.pending.extend(more);
            }
            Action::WithdrawFileOffer => self.withdraw_file_offer(),
            Action::ScheduleSpoolSweep { delay } => {
                self.schedule(delay, SyncEvent::SpoolSweepDue);
            }
            Action::EvictSpoolEntry { entry } => self.evict_entry(&entry),
            Action::ScheduleSettle { delay } => {
                // Bump the generation: any timer already in flight becomes
                // a no-op when it fires, so the debounce restarts without
                // cancellation bookkeeping.
                self.settle_generation += 1;
                self.schedule(delay, SyncEvent::SettleDue(self.settle_generation));
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crossover_platform::VirtualFileClipboard;
    use crossover_platform::fakes::{
        ClipboardFailure, ClipboardOp, FakeVirtualFiles, InMemoryClipboard,
    };
    use crossover_protocol::RawFrame;
    use crossover_protocol::clipboard::{
        ApplyResult, ClipboardApplied, ClipboardData, ClipboardDecline, ClipboardMeta,
        ClipboardOffer, ContentType, DeclineReason, FileDescriptor, chunk_content, content_hash,
    };
    use crossover_protocol::hello::MessageType;

    use super::{
        EVENT_CHANNEL_CAPACITY, MAX_DEFERRED_EVENTS, SessionCommand, SyncEvent, clipboard_sync,
    };
    use crossover_platform::SpoolError;

    use crate::clipboard::{ClipboardConfig, FileReceive, RetryPolicy};
    use crate::metrics::Metrics;

    struct Rig {
        clipboard: Arc<InMemoryClipboard>,
        events: mpsc::Sender<SyncEvent>,
        commands: crate::outbound::CommandReceiver,
        metrics: Arc<Metrics>,
    }

    fn rig() -> Rig {
        rig_with_spool(None, None)
    }

    fn rig_with_spool(
        spool: Option<Arc<dyn crossover_platform::SpoolStorage>>,
        virtual_files: Option<Arc<dyn VirtualFileClipboard>>,
    ) -> Rig {
        let clipboard = Arc::new(InMemoryClipboard::new());
        let config = ClipboardConfig {
            retry: RetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(20),
            },
            // Tests drive the trigger's *behaviour*, not the wait.
            transmit_debounce: Duration::from_millis(5),
            ..ClipboardConfig::new()
        };
        let metrics = Arc::new(Metrics::new());
        let (driver, events, commands) = clipboard_sync(
            Arc::clone(&clipboard) as Arc<dyn crossover_platform::ClipboardProvider>,
            spool,
            virtual_files,
            Uuid::from_bytes([0xAA; 16]),
            config,
            Some(Arc::clone(&metrics)),
        )
        .unwrap();
        tokio::spawn(driver.run());
        Rig {
            clipboard,
            events,
            commands,
            metrics,
        }
    }

    async fn next_command(rig: &mut Rig) -> SessionCommand {
        timeout(Duration::from_secs(5), rig.commands.recv())
            .await
            .expect("timed out waiting for a sync command")
            .expect("command channel closed")
    }

    fn frame(message_type: MessageType, payload: Vec<u8>) -> SyncEvent {
        SyncEvent::Frame(RawFrame {
            message_type: message_type.wire(),
            message_id: 42,
            payload,
        })
    }

    #[tokio::test]
    async fn local_copy_flows_out_as_a_data_frame() {
        let mut rig = rig();
        // The listener bridge is installed: a local copy alone drives the
        // whole pipeline, no manual events.
        rig.clipboard.set_text_locally("copied text");

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut rig).await
        else {
            panic!("expected SendFrame");
        };
        assert_eq!(message_type, MessageType::ClipboardData.wire());
        let data = ClipboardData::decode_payload(&payload).unwrap();
        assert_eq!(data.content, b"copied text");
    }

    #[tokio::test]
    async fn inbound_data_is_applied_acked_and_not_echoed() {
        let mut rig = rig();
        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xBB; 16]),
            0,
            ContentType::Utf8Text,
            b"from peer".to_vec(),
        );
        let id = item.meta.id;
        rig.events
            .send(frame(
                MessageType::ClipboardData,
                item.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut rig).await
        else {
            panic!("expected SendFrame");
        };
        assert_eq!(message_type, MessageType::ClipboardApplied.wire());
        let applied = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(applied.id, id);
        assert_eq!(applied.result, ApplyResult::Applied);

        // The clipboard holds the content, and the own-write notification
        // the fake fired must NOT have produced another outbound frame.
        assert_eq!(rig.clipboard.peek().as_deref(), Some("from peer"));
        let quiet = timeout(Duration::from_millis(300), rig.commands.recv()).await;
        assert!(quiet.is_err(), "echoed an applied item: {quiet:?}");
    }

    #[tokio::test]
    async fn busy_writes_retry_through_real_timers_then_succeed() {
        let mut rig = rig();
        rig.clipboard
            .fail_next(ClipboardOp::Write, ClipboardFailure::Busy, 2);

        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xBB; 16]),
            0,
            ContentType::Utf8Text,
            b"contended".to_vec(),
        );
        rig.events
            .send(frame(
                MessageType::ClipboardData,
                item.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        // Two Busy failures burn through real ScheduleRetry timers before
        // the third attempt lands.
        let SessionCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("expected SendFrame");
        };
        let applied = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(applied.result, ApplyResult::Applied);
        assert_eq!(rig.clipboard.peek().as_deref(), Some("contended"));

        // Two Busy writes were two contention events and two retries; the
        // third write applied the item once.
        let report = rig.metrics.snapshot();
        assert_eq!(report.clipboard_contention, 2);
        assert_eq!(report.clipboard_retries, 2);
        assert_eq!(report.clipboard_applied, 1);
    }

    #[tokio::test]
    async fn exhausted_retries_report_clipboard_unavailable() {
        let mut rig = rig();
        rig.clipboard
            .fail_next(ClipboardOp::Write, ClipboardFailure::Busy, 99);

        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xBB; 16]),
            0,
            ContentType::Utf8Text,
            b"never lands".to_vec(),
        );
        rig.events
            .send(frame(
                MessageType::ClipboardData,
                item.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        let SessionCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("expected SendFrame");
        };
        let applied = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(applied.result, ApplyResult::ClipboardUnavailable);
        assert_eq!(rig.clipboard.peek(), None);
    }

    #[tokio::test]
    async fn malformed_clipboard_payload_terminates_the_session() {
        let mut rig = rig();
        rig.events
            .send(frame(MessageType::ClipboardData, vec![0xFF; 40]))
            .await
            .unwrap();
        assert!(matches!(
            next_command(&mut rig).await,
            SessionCommand::TerminateSession { .. }
        ));
    }

    /// The soak defect, in a test: sustained read contention must not
    /// starve an inbound frame. Before the bound, the nudge cycle
    /// re-enqueued itself indefinitely and the peer's item waited behind
    /// it (27 seconds, on real hardware).
    #[tokio::test]
    async fn sustained_read_contention_does_not_starve_inbound_items() {
        let mut rig = rig();
        // Far more busy reads than the bound allows.
        rig.clipboard
            .fail_next(ClipboardOp::Read, ClipboardFailure::Busy, 1000);
        // Kick the read cycle, then deliver a peer item behind it.
        rig.events.send(SyncEvent::LocalChanged).await.unwrap();

        let item = ClipboardData::from_content(
            Uuid::new_v4(),
            Uuid::from_bytes([0xBB; 16]),
            0,
            ContentType::Utf8Text,
            b"must not wait for the read storm".to_vec(),
        );
        rig.events
            .send(frame(
                MessageType::ClipboardData,
                item.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        // The ack must arrive promptly despite the ongoing read failures.
        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = timeout(Duration::from_secs(3), rig.commands.recv())
            .await
            .expect("inbound item starved by read contention")
            .expect("command channel closed")
        else {
            panic!("expected SendFrame");
        };
        assert_eq!(message_type, MessageType::ClipboardApplied.wire());
        let applied = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(applied.result, ApplyResult::Applied);
    }

    /// The soak's real defect: a backlog of inbound items must not
    /// become a burst of clipboard writes. Only the newest is applied;
    /// the rest are acknowledged as superseded rather than written or
    /// silently dropped.
    #[tokio::test]
    async fn a_backlog_of_items_applies_only_the_newest() {
        let mut rig = rig();

        let mut ids = Vec::new();
        for i in 0..20 {
            let item = ClipboardData::from_content(
                Uuid::new_v4(),
                Uuid::from_bytes([0xBB; 16]),
                i,
                ContentType::Utf8Text,
                format!("backlog item {i}").into_bytes(),
            );
            ids.push(item.meta.id);
            rig.events
                .send(frame(
                    MessageType::ClipboardData,
                    item.encode_payload().unwrap(),
                ))
                .await
                .unwrap();
        }

        // Collect every ack the driver produces for the burst.
        let mut results = Vec::new();
        for _ in 0..ids.len() {
            let SessionCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
                panic!("expected SendFrame");
            };
            let applied = ClipboardApplied::decode_payload(&payload).unwrap();
            results.push((applied.id, applied.result));
        }

        let applied_count = results
            .iter()
            .filter(|(_, r)| *r == ApplyResult::Applied)
            .count();
        let superseded_count = results
            .iter()
            .filter(|(_, r)| *r == ApplyResult::Superseded)
            .count();

        // Every item is answered — none silently dropped (NFR-3).
        assert_eq!(results.len(), ids.len(), "some items were never answered");
        // The clipboard was written far less than 20 times; in a single
        // coalescing pass, exactly once.
        assert!(
            applied_count <= 2,
            "applied {applied_count} items — the backlog was not coalesced"
        );
        assert!(superseded_count >= ids.len() - 2);
        // The surviving content is the newest item.
        assert_eq!(rig.clipboard.peek().as_deref(), Some("backlog item 19"));
    }

    #[tokio::test]
    async fn non_clipboard_frames_are_ignored() {
        let mut rig = rig();
        rig.events
            .send(SyncEvent::Frame(RawFrame {
                message_type: 0x7777,
                message_id: 1,
                payload: b"other subsystem".to_vec(),
            }))
            .await
            .unwrap();
        let quiet = timeout(Duration::from_millis(200), rig.commands.recv()).await;
        assert!(quiet.is_err());
    }

    /// The driver's half of a chunked transfer (ADR 0014): one chunk per
    /// `SessionCommand`, in order, straight from the engine's retained
    /// buffer — no pre-rendered chunk list anywhere in the path.
    #[tokio::test]
    async fn a_local_image_streams_out_one_chunk_per_command() {
        use crossover_platform::ClipboardImageFormat;
        use crossover_protocol::clipboard::{
            ClipboardAccept, ClipboardChunk, ClipboardOffer, MAX_CHUNK_BYTES,
        };

        let mut rig = rig();
        let bytes: Vec<u8> = (0..MAX_CHUNK_BYTES * 2 + 9)
            .map(|i| u8::try_from(i % 256).unwrap_or(0))
            .collect();
        rig.clipboard
            .set_image_locally(ClipboardImageFormat::Dib, bytes.clone());

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut rig).await
        else {
            panic!("expected an offer");
        };
        assert_eq!(message_type, MessageType::ClipboardOffer.wire());
        let offer = ClipboardOffer::decode_payload(&payload).unwrap();
        assert_eq!(offer.meta.content_length, bytes.len() as u64);

        // Accept it, and the stream follows as individual chunk frames.
        let accept = ClipboardAccept { id: offer.meta.id };
        rig.events
            .send(frame(
                MessageType::ClipboardAccept,
                accept.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        let mut streamed = Vec::new();
        for index in 0..3u32 {
            let SessionCommand::SendFrame {
                message_type,
                payload,
                ..
            } = next_command(&mut rig).await
            else {
                panic!("expected chunk {index}");
            };
            assert_eq!(message_type, MessageType::ClipboardChunk.wire());
            let chunk = ClipboardChunk::decode_payload(&payload).unwrap();
            assert_eq!(chunk.id, offer.meta.id);
            assert_eq!(chunk.index, index);
            streamed.extend(chunk.payload);
        }
        assert_eq!(streamed, bytes, "the image was not streamed verbatim");

        // The transfer is over; nothing else is emitted.
        let quiet = timeout(Duration::from_millis(200), rig.commands.recv()).await;
        assert!(quiet.is_err(), "extra traffic after the stream: {quiet:?}");
    }

    /// The other direction end to end through the driver: offer, accept,
    /// chunks, verified reassembly, a typed clipboard install, and only
    /// then the verdict (FR-3.2).
    #[tokio::test]
    async fn an_inbound_image_is_installed_typed_and_then_acknowledged() {
        use crossover_platform::{ClipboardContent, ClipboardImageFormat};
        use crossover_protocol::clipboard::{
            ClipboardMeta, ClipboardOffer, ImageFormat, MAX_CHUNK_BYTES, chunk_content,
            content_hash,
        };

        let mut rig = rig();
        // Deliberately hostile bytes for anything that assumes text.
        let bytes: Vec<u8> = (0..=MAX_CHUNK_BYTES)
            .map(|i| if i % 3 == 0 { 0xFF } else { 0x00 })
            .collect();
        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xBB; 16]),
            sequence: 0,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: bytes.len() as u64,
            content_hash: content_hash(&bytes),
        };
        rig.events
            .send(frame(
                MessageType::ClipboardOffer,
                ClipboardOffer {
                    meta,
                    descriptor: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();

        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected an accept");
        };
        assert_eq!(message_type, MessageType::ClipboardAccept.wire());

        for chunk in chunk_content(meta.id, &bytes).unwrap() {
            rig.events
                .send(frame(
                    MessageType::ClipboardChunk,
                    chunk.encode_payload().unwrap(),
                ))
                .await
                .unwrap();
        }

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut rig).await
        else {
            panic!("expected a verdict");
        };
        assert_eq!(message_type, MessageType::ClipboardApplied.wire());
        let applied = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(applied.id, meta.id);
        assert_eq!(applied.result, ApplyResult::Applied);

        // Destination-updated is the definition of success, and the bytes
        // are the ones that were offered.
        assert_eq!(
            rig.clipboard.peek_content(),
            Some(ClipboardContent::Image {
                format: ClipboardImageFormat::Dib,
                bytes,
            })
        );
        // The own-write notification must not echo the image back.
        let quiet = timeout(Duration::from_millis(300), rig.commands.recv()).await;
        assert!(quiet.is_err(), "echoed an applied image: {quiet:?}");
    }

    /// The wedge, as a test. A chunk stream far longer than the command
    /// lane can hold, with a consumer that has stopped: the driver must
    /// **still consume events**, and a `SessionLost` delivered mid-stream
    /// must take effect rather than sit in a channel behind a driver
    /// parked on backpressure.
    ///
    /// Before the fix the whole stream ran inside one `execute` call, the
    /// event channel filled, and nothing reached the engine until the
    /// transfer finished — which, with the consumer stalled, was never. In
    /// the real app that closes a cycle through the session loop's single
    /// `select!` and the session stops failing closed (see
    /// `send_command`). Here it is reduced to its observable core.
    #[tokio::test]
    async fn a_saturated_chunk_stream_does_not_deafen_the_driver() {
        use crossover_platform::ClipboardImageFormat;
        use crossover_protocol::clipboard::{ClipboardAccept, ClipboardOffer, MAX_CHUNK_BYTES};

        use crate::outbound::MAX_BACKGROUND_QUEUE_FRAMES;

        // Far more chunks than the Background lane's 64-frame bound, so
        // the driver is genuinely parked partway through.
        const CHUNKS: usize = 200;

        let mut rig = rig();
        rig.clipboard.set_image_locally(
            ClipboardImageFormat::Dib,
            vec![0xAB; MAX_CHUNK_BYTES * CHUNKS],
        );

        let SessionCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("expected an offer");
        };
        let offer = ClipboardOffer::decode_payload(&payload).unwrap();
        rig.events
            .send(frame(
                MessageType::ClipboardAccept,
                ClipboardAccept { id: offer.meta.id }
                    .encode_payload()
                    .unwrap(),
            ))
            .await
            .unwrap();

        // Consume nothing: the lane fills and the driver parks mid-stream.
        // Give it a moment to get there, then pull the session out from
        // under it. This send must not block on a full event channel, and
        // the driver must act on it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        timeout(
            Duration::from_millis(500),
            rig.events.send(SyncEvent::SessionLost),
        )
        .await
        .expect("the driver stopped consuming events during the stream")
        .unwrap();

        // Now drain. The stream must have stopped early — a session that
        // no longer exists gets no more frames.
        let mut chunks = 0usize;
        while let Ok(Some(command)) = timeout(Duration::from_millis(200), rig.commands.recv()).await
        {
            let SessionCommand::SendFrame { message_type, .. } = command else {
                panic!("unexpected termination");
            };
            assert_eq!(message_type, MessageType::ClipboardChunk.wire());
            chunks += 1;
        }
        // Only what was already committed to the lane before the loss
        // may appear — in practice the lane's depth plus the frame in
        // hand, nowhere near the whole transfer.
        assert!(
            chunks <= MAX_BACKGROUND_QUEUE_FRAMES * 2,
            "{chunks} of {CHUNKS} chunks were emitted for a lost session; the              driver kept streaming instead of noticing"
        );

        // And the driver is alive, not wedged: a fresh local copy still
        // flows all the way out.
        rig.events
            .send(SyncEvent::SessionEstablished)
            .await
            .unwrap();
        rig.clipboard.set_text_locally("still responsive");
        let SessionCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("the driver wedged after the abandoned stream");
        };
        assert_eq!(
            ClipboardData::decode_payload(&payload).unwrap().content,
            b"still responsive"
        );
    }

    /// The driver actually wires the deadline: a peer that offers and then
    /// goes silent gets an answer instead of holding a buffer forever
    /// (ADR 0014, NFR-3).
    #[tokio::test]
    async fn a_stalled_inbound_transfer_times_out_through_the_driver() {
        use crossover_protocol::clipboard::{
            ClipboardMeta, ClipboardOffer, ImageFormat, content_hash,
        };

        let clipboard = Arc::new(InMemoryClipboard::new());
        let (driver, events, mut commands) = clipboard_sync(
            Arc::clone(&clipboard) as Arc<dyn crossover_platform::ClipboardProvider>,
            None,
            None,
            Uuid::from_bytes([0xAA; 16]),
            ClipboardConfig {
                // The deadline is the subject here, so it is short — the
                // production default is a minute (see TRANSFER_TIMEOUT).
                transfer_timeout: Duration::from_millis(50),
                ..ClipboardConfig::new()
            },
            None,
        )
        .unwrap();
        tokio::spawn(driver.run());

        let meta = ClipboardMeta {
            id: Uuid::new_v4(),
            origin: Uuid::from_bytes([0xBB; 16]),
            sequence: 0,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: 4 * 1024 * 1024,
            content_hash: content_hash(b"never arrives"),
        };
        events
            .send(frame(
                MessageType::ClipboardOffer,
                ClipboardOffer {
                    meta,
                    descriptor: None,
                }
                .encode_payload()
                .unwrap(),
            ))
            .await
            .unwrap();

        let mut verdict = None;
        for _ in 0..2 {
            let command = timeout(Duration::from_secs(5), commands.recv())
                .await
                .expect("the stalled transfer was never answered")
                .expect("command channel closed");
            let SessionCommand::SendFrame {
                message_type,
                payload,
                ..
            } = command
            else {
                panic!("unexpected termination");
            };
            if message_type == MessageType::ClipboardApplied.wire() {
                verdict = Some(ClipboardApplied::decode_payload(&payload).unwrap());
                break;
            }
            assert_eq!(message_type, MessageType::ClipboardAccept.wire());
        }
        let verdict = verdict.expect("no verdict for the abandoned transfer");
        assert_eq!(verdict.id, meta.id);
        assert_eq!(verdict.result, ApplyResult::ContentRejected);
        // Nothing was installed from a transfer that never completed.
        assert_eq!(clipboard.peek_content(), None);
    }

    #[tokio::test]
    async fn reconnect_re_announces_current_content() {
        let mut rig = rig();
        rig.clipboard.set_text_locally("survives the gap");
        // Drain the initial announcement.
        let _ = next_command(&mut rig).await;

        rig.events.send(SyncEvent::SessionLost).await.unwrap();
        rig.events
            .send(SyncEvent::SessionEstablished)
            .await
            .unwrap();

        let SessionCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("expected SendFrame");
        };
        let data = ClipboardData::decode_payload(&payload).unwrap();
        assert_eq!(data.content, b"survives the gap");
    }

    /// `MAX_DEFERRED_EVENTS` is the *bound* on the deferred queue, and the
    /// bound is the part nothing else pins. The neighbouring chunk-stream
    /// test proves a parked driver still consumes — but it delivers a single
    /// event, so the queue never exceeds one and the constant could read 1
    /// or a billion without a test noticing.
    ///
    /// Here the parked driver is flooded and the queue's own high-water mark
    /// is read back: it must reach the bound (deleting the drain leaves it at
    /// zero) and never pass it (unbounding the queue runs it away). Both
    /// mutants are caught.
    ///
    /// The measurement is deliberately the depth and not "how many events the
    /// driver swallowed". Absorption is cumulative — a driver that gets one
    /// frame out, drains its deferred queue and parks again has legitimately
    /// taken another queue's worth — so counting it measures the scheduler on
    /// the day rather than the bound, which is how the previous version of
    /// this test came to fail on slower CI runners and pass everywhere else.
    #[tokio::test]
    async fn a_parked_driver_defers_a_bounded_number_of_events() {
        /// How long to keep flooding after the queue first hits its bound,
        /// so an unbounded queue has room to overshoot and be caught.
        const OVERSHOOT_WINDOW: Duration = Duration::from_millis(250);
        /// Give up rather than hang if the queue never fills at all.
        const DEADLINE: Duration = Duration::from_secs(10);

        use crossover_platform::ClipboardImageFormat;
        use crossover_protocol::clipboard::{ClipboardAccept, ClipboardOffer, MAX_CHUNK_BYTES};

        // Park the driver the way the app does: a chunk stream far longer
        // than the Background lane, with nothing draining the lane. It stops
        // inside `send_command`, the only place deferring happens — and,
        // crucially, it stops *without* having drained the event channel,
        // which is what makes the count below mean something.
        let mut rig = rig();
        rig.clipboard
            .set_image_locally(ClipboardImageFormat::Dib, vec![0xAB; MAX_CHUNK_BYTES * 200]);
        let SessionCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("expected an offer");
        };
        let offer = ClipboardOffer::decode_payload(&payload).unwrap();
        rig.events
            .send(frame(
                MessageType::ClipboardAccept,
                ClipboardAccept { id: offer.meta.id }
                    .encode_payload()
                    .unwrap(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Flood the parked driver. `LocalChanged` is the cheapest event that
        // reaches the deferred queue; what it carries does not matter, only
        // that the driver keeps being offered more than it can hold.
        let peak = |rig: &Rig| {
            usize::try_from(rig.metrics.snapshot().clipboard_deferred_peak)
                .expect("a queue depth fits in usize")
        };
        let started = Instant::now();
        let mut filled_at = None;
        let mut refused = 0usize;
        while started.elapsed() < DEADLINE {
            // A full channel is the expected state once the driver is
            // holding all it can; keep offering rather than concluding
            // anything from a single refusal.
            if rig.events.try_send(SyncEvent::LocalChanged).is_err() {
                refused += 1;
            }
            tokio::task::yield_now().await;
            match filled_at {
                None if peak(&rig) >= MAX_DEFERRED_EVENTS => filled_at = Some(Instant::now()),
                Some(at) if at.elapsed() >= OVERSHOOT_WINDOW => break,
                _ => {}
            }
        }

        let deferred_peak = peak(&rig);
        assert!(
            filled_at.is_some(),
            "a parked driver's deferred queue peaked at {deferred_peak}, never reaching \
             {MAX_DEFERRED_EVENTS} — is it still draining the event channel while parked?"
        );
        assert_eq!(
            deferred_peak, MAX_DEFERRED_EVENTS,
            "a parked driver deferred {deferred_peak} events, past the \
             {MAX_DEFERRED_EVENTS} its bound allows (NFR-1)"
        );
        // Holding a bounded amount is only half of it: what the driver
        // cannot hold has to be refused, so backpressure reaches whoever is
        // producing rather than stopping here.
        assert!(
            refused > 0,
            "the {EVENT_CHANNEL_CAPACITY}-deep event channel never filled — a parked \
             driver must push backpressure outwards, not absorb without limit"
        );
    }

    /// A spool over a real temporary directory, for the one thing the
    /// engine's own tests cannot show: that the driver's actions actually
    /// move bytes onto disk and promote them.
    ///
    /// Plain `std::fs`, and therefore **not** a spool in the sense F15
    /// means — no protected descriptor, no handle-relative operation. That
    /// is exactly why it is `cfg(test)` and lives here rather than beside
    /// `UnsupportedSpoolStorage`, which refuses precisely so that no
    /// unprotected implementation can be mistaken for the real one.
    struct TempSpool(std::path::PathBuf);

    impl TempSpool {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crossover-driver-spool-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("test spool");
            Self(dir)
        }

        fn read(&self, name: &str) -> Vec<u8> {
            std::fs::read(self.0.join(name)).expect("reading a spool entry")
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .expect("listing")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempSpool {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl crossover_platform::SpoolStorage for TempSpool {
        fn entries(&self) -> Result<Vec<crossover_platform::SpoolEntry>, SpoolError> {
            Ok(self
                .names()
                .into_iter()
                .map(|name| crossover_platform::SpoolEntry {
                    len: std::fs::metadata(self.0.join(&name)).map_or(0, |m| m.len()),
                    name,
                    is_file: true,
                })
                .collect())
        }

        fn create_entry(&self, name: &str) -> Result<std::fs::File, SpoolError> {
            crossover_platform::validate_entry_name(name)?;
            std::fs::File::create_new(self.0.join(name)).map_err(|error| SpoolError::Backend {
                reason: error.to_string(),
            })
        }

        fn open_entry(&self, name: &str) -> Result<std::fs::File, SpoolError> {
            crossover_platform::validate_entry_name(name)?;
            std::fs::File::open(self.0.join(name)).map_err(|error| SpoolError::Backend {
                reason: error.to_string(),
            })
        }

        fn unlink_entry(&self, name: &str) -> Result<(), SpoolError> {
            crossover_platform::validate_entry_name(name)?;
            match std::fs::remove_file(self.0.join(name)) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(SpoolError::Backend {
                    reason: error.to_string(),
                }),
            }
        }

        fn rename_entry(&self, from: &str, to: &str) -> Result<(), SpoolError> {
            crossover_platform::validate_entry_name(from)?;
            crossover_platform::validate_entry_name(to)?;
            std::fs::rename(self.0.join(from), self.0.join(to)).map_err(|error| {
                SpoolError::Backend {
                    reason: error.to_string(),
                }
            })
        }

        fn free_bytes(&self) -> Result<u64, SpoolError> {
            Ok(u64::MAX / 2)
        }
    }

    fn file_offer(id: Uuid, content: &[u8], name: &str) -> ClipboardOffer {
        ClipboardOffer {
            meta: ClipboardMeta {
                id,
                origin: Uuid::from_bytes([0xBB; 16]),
                sequence: 1,
                content_type: ContentType::File,
                content_length: content.len() as u64,
                content_hash: content_hash(content),
            },
            descriptor: Some(FileDescriptor {
                file_name: name.to_owned(),
                archived: false,
                entry_count: 1,
                original_bytes: content.len() as u64,
            }),
        }
    }

    /// End to end through the driver: a peer file arrives as frames, lands
    /// in the spool as one verified entry, and is acknowledged `Stored`.
    #[tokio::test]
    async fn a_peer_file_is_written_through_to_the_spool() {
        let spool = Arc::new(TempSpool::new());
        let files = Arc::new(FakeVirtualFiles::new());
        let mut rig = rig_with_spool(
            Some(Arc::clone(&spool) as Arc<dyn crossover_platform::SpoolStorage>),
            Some(Arc::clone(&files) as Arc<dyn VirtualFileClipboard>),
        );
        rig.events
            .send(SyncEvent::FileReceivePolicy(FileReceive::Allowed))
            .await
            .unwrap();

        let content: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        let id = Uuid::new_v4();
        let offer = file_offer(id, &content, "quarterly.pdf");
        rig.events
            .send(frame(
                MessageType::ClipboardOffer,
                offer.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        let SessionCommand::SendFrame { message_type, .. } = next_command(&mut rig).await else {
            panic!("expected an accept");
        };
        assert_eq!(message_type, MessageType::ClipboardAccept.wire());

        for chunk in chunk_content(id, &content).unwrap() {
            rig.events
                .send(frame(
                    MessageType::ClipboardChunk,
                    chunk.encode_payload().unwrap(),
                ))
                .await
                .unwrap();
        }

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut rig).await
        else {
            panic!("expected a verdict");
        };
        assert_eq!(message_type, MessageType::ClipboardApplied.wire());
        let applied = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(applied.result, ApplyResult::Stored);

        // One entry, promoted out of its partial, holding exactly what the
        // peer offered — and no `.part` left behind.
        let names = spool.names();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(
            names[0]
                .strip_suffix(".bin")
                .is_some_and(|stem| Uuid::parse_str(stem).is_ok()),
            "a promoted entry is named <id>.bin: {names:?}"
        );
        assert_eq!(spool.read(&names[0]), content);

        // And it is *offered*, which is the difference between bytes on
        // disk and a delivery: the peer's name reaches the paste
        // mechanism, while the entry name stays ours.
        let offers = files.offers();
        assert_eq!(offers.len(), 1, "{offers:?}");
        assert_eq!(offers[0].entry, names[0]);
        assert_eq!(offers[0].file_name, "quarterly.pdf");
        assert_eq!(offers[0].byte_len, content.len() as u64);

        // Our own offer raised a clipboard change, and staging it would
        // send the file straight back to the peer that sent it (F13). The
        // driver recognizes the object and stays quiet — and the entry
        // survives, because the clipboard has not moved on.
        rig.events.send(SyncEvent::LocalChanged).await.unwrap();
        assert!(
            timeout(Duration::from_millis(300), rig.commands.recv())
                .await
                .is_err(),
            "our own file offer was staged back to the peer"
        );
        assert_eq!(spool.names().len(), 1);

        // Somebody else copies: the clipboard has moved on, so the entry
        // behind the item it was offering is collected (ADR 0015).
        files.moved_on();
        rig.events.send(SyncEvent::LocalChanged).await.unwrap();
        for _ in 0..50 {
            if spool.names().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "the entry outlived the clipboard that offered it: {:?}",
            spool.names()
        );
    }

    /// Without a spool the driver refuses files whatever the trust store
    /// says, and the refusal is the permanent one: there is nowhere for a
    /// file to go, so it is not a matter of permission.
    #[tokio::test]
    async fn a_driver_without_a_spool_refuses_files_however_it_is_configured() {
        let mut rig = rig();
        rig.events
            .send(SyncEvent::FileReceivePolicy(FileReceive::Allowed))
            .await
            .unwrap();

        let content = b"a small document".to_vec();
        let offer = file_offer(Uuid::new_v4(), &content, "doc.pdf");
        rig.events
            .send(frame(
                MessageType::ClipboardOffer,
                offer.encode_payload().unwrap(),
            ))
            .await
            .unwrap();

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut rig).await
        else {
            panic!("expected a decline");
        };
        assert_eq!(message_type, MessageType::ClipboardDecline.wire());
        let decline = ClipboardDecline::decode_payload(&payload).unwrap();
        assert_eq!(decline.reason, DeclineReason::UnsupportedType);
    }
}
