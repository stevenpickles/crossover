//! The prioritized outbound send path: two lanes, one socket
//! ([ADR 0013](../../../docs/adr/0013-interactive-over-bulk-prioritization.md)).
//!
//! A session is a single TLS-over-TCP stream, so everything the
//! application sends is serialized onto one ordered byte pipe. Left as a
//! plain FIFO, a multi-megabyte clipboard payload queued ahead of an input
//! batch head-of-line blocks the pointer and keyboard — the exact thing
//! NFR-5 and priority #5 forbid.
//!
//! This module is the fix: every outbound frame is classified into a
//! [`SendPriority`] the moment it enters the path, and the two classes
//! travel in **separate queues all the way to the writer**. The writer
//! drains High to empty before taking a *single* Background frame, then
//! re-checks High — so a freshly-arrived input batch goes out ahead of the
//! next bulk frame, and no Background backpressure at any hop can delay a
//! High frame.
//!
//! Three rules make that guarantee real:
//!
//! - **Strict High-first, no aging.** Input is bursty, so Background makes
//!   progress in the gaps; a clipboard transfer has no deadline, while a
//!   late `ReleaseAllInput` is a stuck key. Starvation of Background under
//!   *sustained* input is accepted deliberately (docs/ARCHITECTURE.md §5.4).
//! - **Reordered, never dropped.** Background is bulk, and bulk is
//!   lossless: each class keeps its own FIFO order, and cross-class
//!   reordering is the only thing prioritization changes
//!   (docs/PROTOCOL.md §4).
//! - **Bounded in bytes, not just messages.** Sixty-four queued maximum-size
//!   clipboard frames would be a quarter-gigabyte memory commitment per hop,
//!   so the Background lane carries a byte budget as well as a message count
//!   (NFR-1). Producers block on it; the High lane never does.
//!
//! Keepalive is High by construction and never enters these queues at all:
//! `run_session` writes `Ping` straight to the writer on its idle tick and
//! answers `Pong` from the dispatch path.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crossover_protocol::hello::MessageType;

use crate::command::SessionCommand;

/// Depth of a High lane, in frames. Interactive frames are small (an input
/// batch is tens of bytes, a control message a handful), so a message count
/// is the honest bound here: sixty-four of them is well under a kilobyte of
/// queue, and a lane this shallow means a stall is felt as backpressure
/// rather than hidden as latency.
pub const MAX_HIGH_QUEUE_FRAMES: usize = 64;

/// Message-count bound on a Background lane. The byte budget below is the
/// bound that normally binds; this one caps the *number* of in-flight bulk
/// items so a flood of tiny frames cannot grow the queue without limit.
pub const MAX_BACKGROUND_QUEUE_FRAMES: usize = 64;

/// Byte budget for a Background lane — the bound that matters for memory
/// (NFR-1). Comfortably above `MAX_FRAME_BODY_BYTES` (4 MiB + 64 KiB) so a
/// single maximum-size clipboard frame always fits on an empty lane, and
/// small enough that a saturated lane is a bounded, predictable commitment.
/// Producers wait for room; nothing is ever discarded to stay inside it.
pub const MAX_BACKGROUND_QUEUE_BYTES: usize = 8 * 1024 * 1024;

/// The clamp in `BudgetedSender::charge` keeps an over-budget item moving by
/// charging it the whole budget, which serializes it against every other
/// producer. That is a deadlock-avoidance fallback, not a mode the session
/// path should ever enter — so assert at compile time that the largest frame
/// the protocol can carry genuinely fits inside the budget. If
/// `MAX_PAYLOAD_BYTES` ever grows past it, this fails the build rather than
/// silently degrading the Background lane to one frame at a time.
const _: () = assert!(
    crossover_protocol::framing::MAX_PAYLOAD_BYTES < MAX_BACKGROUND_QUEUE_BYTES,
    "MAX_BACKGROUND_QUEUE_BYTES must exceed the largest protocol payload, or \
     every maximum-size frame serializes the Background lane"
);

/// Which lane an outbound frame rides (ADR 0013).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPriority {
    /// Interactive traffic: live input, control transfer, keepalive.
    /// Always drained first, never queued behind bulk.
    High,
    /// Bulk transfers: the clipboard transaction in all its forms. Ordered
    /// among themselves, reordered behind High, never dropped.
    Background,
}

impl SendPriority {
    /// Classify a wire message type.
    ///
    /// `InputBatch` and `ReleaseAllInput` are the live input path;
    /// `ReleaseAllInput` emphatically so, because a delayed release is a
    /// stuck key and a stuck key is release-blocking. Control transfer
    /// negotiates who owns that path, and keepalive decides whether the
    /// session is alive at all — both are interactive by consequence.
    ///
    /// **Every clipboard message rides Background**, including the small
    /// `Offer`/`Accept`/`Decline`/`Applied` ones the ADR left open, and
    /// `ClipboardChunk` — which is the reason the lane exists at all: a
    /// chunk *is* the preemption unit (ADR 0014), and the writer takes one
    /// of them between High checks. Splitting
    /// a transaction across classes would let its acknowledgement overtake
    /// its data, and the transaction state machine (ADR 0005) depends on
    /// those messages arriving in the order they were produced. One lane for
    /// the whole transaction keeps that invariant for free, and costs the
    /// clipboard only latency — which SPECIFICATION.md §2 never ranks above
    /// input.
    ///
    /// A message type this build does not know is Background: it cannot be
    /// interactive traffic *we* emit, and the High lane is reserved for
    /// frames whose latency budget we can vouch for.
    /// This is a priority partition, not a class one — `MessageType::class`
    /// exists for the §4 class fact and is used where that fact alone is
    /// what a caller needs (`crossover-core::metrics`); this match groups
    /// by latency budget instead, which is a different question with the
    /// same answer for most types and a different one for a few (INPUT
    /// class's `InputBatch` rides High for the reason above, alongside
    /// every CONTROL-class type).
    #[must_use]
    pub fn of(message_type: u16) -> Self {
        match MessageType::from_wire(message_type) {
            Some(
                MessageType::InputBatch
                | MessageType::ReleaseAllInput
                | MessageType::ControlRequest
                | MessageType::ControlResponse
                | MessageType::ControlRelease
                // Display topology (ADR 0018) is CONTROL class
                // (docs/PROTOCOL.md §4): a stale arrangement degrades
                // crossing placement (never control correctness), but it
                // is still interactive-sized negotiation traffic, not bulk.
                | MessageType::MonitorTopology
                | MessageType::LayoutSync
                | MessageType::Ping
                | MessageType::Pong
                | MessageType::Hello
                | MessageType::PairingStart
                | MessageType::PairingConfirm,
            ) => Self::High,
            Some(
                MessageType::ClipboardOffer
                | MessageType::ClipboardAccept
                | MessageType::ClipboardDecline
                | MessageType::ClipboardData
                | MessageType::ClipboardChunk
                | MessageType::ClipboardApplied,
            )
            | None => Self::Background,
        }
    }
}

/// The send path is gone: the session (or supervisor) behind it stopped.
#[derive(Debug, thiserror::Error)]
#[error("the outbound send path is closed")]
pub struct OutboundClosed;

// ---------------------------------------------------------------------------
// Byte-budgeted queue
// ---------------------------------------------------------------------------

/// The byte budget one queued item holds. Released on drop — i.e. when the
/// consumer has *finished* with the item, not merely dequeued it — so the
/// budget covers the frame being written as well as the ones waiting.
#[derive(Debug)]
pub struct BudgetHold {
    _permit: OwnedSemaphorePermit,
}

/// An item on a byte-budgeted queue, carrying the budget it holds.
/// Dereferences to the item; drop it to return the budget.
#[derive(Debug)]
pub struct Budgeted<T> {
    item: T,
    hold: BudgetHold,
}

impl<T> Budgeted<T> {
    /// Split the item from the budget it holds, so the budget can outlive
    /// the queue slot (a frame is charged until it has been written).
    #[must_use]
    pub fn into_parts(self) -> (T, BudgetHold) {
        (self.item, self.hold)
    }
}

impl<T> std::ops::Deref for Budgeted<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.item
    }
}

/// Sending half of a queue bounded by **both** message count and total
/// queued bytes. Cloneable like an `mpsc::Sender`; every clone shares the
/// one budget.
#[derive(Debug)]
pub struct BudgetedSender<T> {
    tx: mpsc::Sender<Budgeted<T>>,
    budget: Arc<Semaphore>,
    capacity_bytes: u32,
}

// A manual impl: `T` need not be `Clone` for the sender to be.
impl<T> Clone for BudgetedSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            budget: Arc::clone(&self.budget),
            capacity_bytes: self.capacity_bytes,
        }
    }
}

impl<T> BudgetedSender<T> {
    /// What `bytes` costs against the budget. Clamped to the whole budget so
    /// an item larger than the lane still makes progress once the lane is
    /// empty, rather than deadlocking on permits that can never exist.
    fn charge(&self, bytes: usize) -> u32 {
        u32::try_from(bytes)
            .unwrap_or(u32::MAX)
            .min(self.capacity_bytes)
    }

    /// Queue `item`, charging `bytes` against the byte budget. Waits while
    /// either bound is reached — producer-side backpressure is the design,
    /// and nothing is ever dropped to avoid it.
    ///
    /// # Errors
    ///
    /// [`OutboundClosed`] once the receiving half is gone.
    pub async fn send(&self, item: T, bytes: usize) -> Result<(), OutboundClosed> {
        let permit = Arc::clone(&self.budget)
            .acquire_many_owned(self.charge(bytes))
            .await
            .map_err(|_| OutboundClosed)?;
        self.tx
            .send(Budgeted {
                item,
                hold: BudgetHold { _permit: permit },
            })
            .await
            .map_err(|_| OutboundClosed)
    }

    /// Queue `item` only if it fits right now.
    ///
    /// **Not fair against [`Self::send`].** A `try_send` takes budget with
    /// tokio's `try_acquire`, which barges past producers already queued on
    /// the semaphore rather than joining the queue behind them. Mixing the
    /// two on one lane can therefore starve a parked producer indefinitely,
    /// so production code picks one: `send` for real traffic, `try_send`
    /// only to *probe* saturation (which is what tests do here).
    ///
    /// # Errors
    ///
    /// The item itself, handed back untouched, when the lane is at one of
    /// its bounds or closed — a *rejection*, never a silent drop.
    pub fn try_send(&self, item: T, bytes: usize) -> Result<(), T> {
        let Ok(permit) = Arc::clone(&self.budget).try_acquire_many_owned(self.charge(bytes)) else {
            return Err(item);
        };
        self.tx
            .try_send(Budgeted {
                item,
                hold: BudgetHold { _permit: permit },
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(queued)
                | mpsc::error::TrySendError::Closed(queued) => queued.into_parts().0,
            })
    }
}

/// Receiving half of a byte-budgeted queue.
///
/// Dropping it **closes the byte budget**, not just the queue. Without that,
/// a producer parked in `acquire_many_owned` would wait forever whenever the
/// budget it needs is held by a [`BudgetHold`] that outlived the receiver —
/// which is the normal state during teardown, because the writer holds the
/// in-flight frame's budget until the write finishes. Closing makes every
/// parked producer unwind to [`OutboundClosed`] instead.
#[derive(Debug)]
pub struct BudgetedReceiver<T> {
    rx: mpsc::Receiver<Budgeted<T>>,
    budget: Arc<Semaphore>,
}

impl<T> Drop for BudgetedReceiver<T> {
    fn drop(&mut self) {
        // Existing permits are unaffected; only waiters and future
        // acquisitions are failed, which is exactly the teardown signal.
        self.budget.close();
    }
}

impl<T> BudgetedReceiver<T> {
    /// Await the next item; `None` once every sender is gone and the queue
    /// is drained.
    pub async fn recv(&mut self) -> Option<Budgeted<T>> {
        self.rx.recv().await
    }

    /// Take the next item if one is already queued.
    ///
    /// # Errors
    ///
    /// `Empty` when nothing is queued, `Disconnected` when nothing is queued
    /// and every sender is gone.
    pub fn try_recv(&mut self) -> Result<Budgeted<T>, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

/// A queue bounded by `max_items` messages *and* `max_bytes` of queued
/// payload, whichever binds first.
///
/// `max_bytes` is clamped to what a semaphore can actually represent —
/// `Semaphore::MAX_PERMITS` first (which is `usize::MAX >> 3`, and so is the
/// binding limit on a 32-bit target), then `u32::MAX` (the width
/// `acquire_many` takes). A request beyond the clamp is a bound the caller
/// asked for and did not get, never a panic.
#[must_use]
pub fn budgeted_channel<T>(
    max_items: usize,
    max_bytes: usize,
) -> (BudgetedSender<T>, BudgetedReceiver<T>) {
    let (tx, rx) = mpsc::channel(max_items);
    let capacity_bytes = u32::try_from(max_bytes.min(Semaphore::MAX_PERMITS)).unwrap_or(u32::MAX);
    let budget = Arc::new(Semaphore::new(capacity_bytes as usize));
    (
        BudgetedSender {
            tx,
            budget: Arc::clone(&budget),
            capacity_bytes,
        },
        BudgetedReceiver { rx, budget },
    )
}

// ---------------------------------------------------------------------------
// The two-lane outbound path
// ---------------------------------------------------------------------------

/// A frame sitting in a lane, carrying when it was handed over.
///
/// The timestamp rides with the frame rather than being taken at the
/// writer, because the wait it measures is the queueing itself — the exact
/// thing a saturating Background transfer is not allowed to lengthen for
/// interactive traffic (ADR 0013).
#[derive(Debug)]
struct Pending {
    message_type: u16,
    payload: Vec<u8>,
    queued_at: Instant,
}

impl Pending {
    fn now(message_type: u16, payload: Vec<u8>) -> Self {
        Self {
            message_type,
            payload,
            queued_at: Instant::now(),
        }
    }
}

/// One frame on its way to the writer, with the class it was scheduled as
/// and (for Background) the byte budget it still holds. Dropping it after
/// the write returns that budget to the lane.
#[derive(Debug)]
pub struct OutboundFrame {
    /// Wire message type.
    pub message_type: u16,
    /// Encoded payload.
    pub payload: Vec<u8>,
    /// The lane it came from — diagnostics and tests; the writer treats
    /// every frame the same once scheduled.
    pub priority: SendPriority,
    /// When the producer handed this frame over. The writer measures
    /// against it to report how long interactive traffic waited for the
    /// wire — the ADR 0013 guarantee, as a number.
    pub queued_at: Instant,
    _hold: Option<BudgetHold>,
}

impl OutboundFrame {
    fn high(queued: Pending) -> Self {
        Self {
            message_type: queued.message_type,
            payload: queued.payload,
            priority: SendPriority::High,
            queued_at: queued.queued_at,
            _hold: None,
        }
    }

    fn background(budgeted: Budgeted<Pending>) -> Self {
        let (queued, hold) = budgeted.into_parts();
        Self {
            message_type: queued.message_type,
            payload: queued.payload,
            priority: SendPriority::Background,
            queued_at: queued.queued_at,
            _hold: Some(hold),
        }
    }
}

/// The application's end of a session's send path. Cloneable: every live
/// producer shares the same two lanes, and classification happens here, at
/// the moment a frame is handed over.
#[derive(Debug, Clone)]
pub struct OutboundSender {
    high: mpsc::Sender<Pending>,
    background: BudgetedSender<Pending>,
}

impl OutboundSender {
    /// Queue a frame on the lane its message type belongs to.
    ///
    /// Waits for room. On the High lane that wait is a message-count queue
    /// of small frames; on the Background lane it is the byte budget, and it
    /// is where a saturating bulk transfer is meant to block — Background
    /// backpressure never reaches the High lane.
    ///
    /// # Errors
    ///
    /// [`OutboundClosed`] once the session behind the path is gone.
    pub async fn send(&self, message_type: u16, payload: Vec<u8>) -> Result<(), OutboundClosed> {
        match SendPriority::of(message_type) {
            SendPriority::High => self
                .high
                .send(Pending::now(message_type, payload))
                .await
                .map_err(|_| OutboundClosed),
            SendPriority::Background => {
                let bytes = payload.len();
                self.background
                    .send(Pending::now(message_type, payload), bytes)
                    .await
            }
        }
    }

    /// Queue a frame only if its lane has room right now.
    ///
    /// # Errors
    ///
    /// The payload, handed back, when the lane is at a bound or closed.
    /// Nothing is dropped — the caller still owns the bytes.
    pub fn try_send(&self, message_type: u16, payload: Vec<u8>) -> Result<(), Vec<u8>> {
        match SendPriority::of(message_type) {
            SendPriority::High => self
                .high
                .try_send(Pending::now(message_type, payload))
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(queued)
                    | mpsc::error::TrySendError::Closed(queued) => queued.payload,
                }),
            SendPriority::Background => {
                let bytes = payload.len();
                self.background
                    .try_send(Pending::now(message_type, payload), bytes)
                    .map_err(|queued| queued.payload)
            }
        }
    }
}

/// One item taken off a two-lane pair, tagged with the lane it came from.
#[derive(Debug)]
enum Lane<T> {
    High(T),
    Background(Budgeted<T>),
}

/// The scheduler both two-lane receivers share: **everything queued High
/// first, then at most one Background item** (ADR 0013).
///
/// The High check runs before *every* Background item, so a caller that
/// consumes one item per call re-checks High between every pair — which is
/// what keeps a bulk backlog from re-serializing ahead of interactive work.
/// Never reorders within a lane, never drops.
///
/// `None` once both lanes are closed and drained. Cancel-safe: nothing is
/// taken off a lane without being returned.
async fn drain_high_first<T>(
    high: &mut mpsc::Receiver<T>,
    background: &mut BudgetedReceiver<T>,
    high_closed: &mut bool,
    background_closed: &mut bool,
) -> Option<Lane<T>> {
    loop {
        match high.try_recv() {
            Ok(item) => return Some(Lane::High(item)),
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => *high_closed = true,
        }
        match background.try_recv() {
            Ok(budgeted) => return Some(Lane::Background(budgeted)),
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => *background_closed = true,
        }
        if *high_closed && *background_closed {
            return None;
        }

        // Both lanes are empty: wait for whichever wakes first, still biased
        // so a simultaneous arrival resolves to High.
        tokio::select! {
            biased;
            item = high.recv(), if !*high_closed => match item {
                Some(item) => return Some(Lane::High(item)),
                None => *high_closed = true,
            },
            item = background.recv(), if !*background_closed => match item {
                Some(budgeted) => return Some(Lane::Background(budgeted)),
                None => *background_closed = true,
            },
        }
    }
}

/// The writer's end of a session's send path: the scheduler that makes the
/// priority real.
#[derive(Debug)]
pub struct OutboundReceiver {
    high: mpsc::Receiver<Pending>,
    background: BudgetedReceiver<Pending>,
    high_closed: bool,
    background_closed: bool,
}

impl OutboundReceiver {
    /// The next frame to write. See [`drain_high_first`] for the policy;
    /// because [`crate::supervision::run_session`] writes exactly one frame
    /// per call, the High lane is re-checked between every pair of frames.
    ///
    /// `None` once both lanes are closed and drained. Cancel-safe.
    pub async fn recv(&mut self) -> Option<OutboundFrame> {
        let lane = drain_high_first(
            &mut self.high,
            &mut self.background,
            &mut self.high_closed,
            &mut self.background_closed,
        )
        .await?;
        Some(match lane {
            Lane::High(queued) => OutboundFrame::high(queued),
            Lane::Background(budgeted) => OutboundFrame::background(budgeted),
        })
    }
}

/// Build one session's send path: a [`SendPriority::High`] lane bounded by
/// message count and a [`SendPriority::Background`] lane bounded by bytes,
/// drained High-first by [`OutboundReceiver::recv`].
#[must_use]
pub fn outbound_channel() -> (OutboundSender, OutboundReceiver) {
    let (high_tx, high_rx) = mpsc::channel(MAX_HIGH_QUEUE_FRAMES);
    let (background_tx, background_rx) =
        budgeted_channel(MAX_BACKGROUND_QUEUE_FRAMES, MAX_BACKGROUND_QUEUE_BYTES);
    (
        OutboundSender {
            high: high_tx,
            background: background_tx,
        },
        OutboundReceiver {
            high: high_rx,
            background: background_rx,
            high_closed: false,
            background_closed: false,
        },
    )
}

// ---------------------------------------------------------------------------
// The drivers' end of the path
// ---------------------------------------------------------------------------

/// Which lane a driver command rides.
///
/// A fail-closed `TerminateSession` is High: it is a security action
/// (docs/PROTOCOL.md §7), and a peer that has just sent an invalid payload
/// must not be able to postpone its own termination by keeping the wire
/// busy.
#[must_use]
pub fn command_priority(command: &SessionCommand) -> SendPriority {
    match command {
        SessionCommand::SendFrame { message_type, .. } => SendPriority::of(*message_type),
        SessionCommand::TerminateSession { .. } => SendPriority::High,
    }
}

/// What a command charges against the Background lane's byte budget.
fn command_bytes(command: &SessionCommand) -> usize {
    match command {
        SessionCommand::SendFrame { payload, .. } => payload.len(),
        SessionCommand::TerminateSession { .. } => 0,
    }
}

/// A driver's end of its command stream: the same two lanes, classified on
/// the way in.
///
/// A driver emitting into **one** mixed queue would re-create the head-of-line
/// block this design exists to remove — its High commands would wait behind
/// its own bulk in that queue, and once the queue filled they would never be
/// emitted at all. Splitting here means a driver parked on bulk backpressure
/// still has a clear path for a termination or a control frame.
#[derive(Debug, Clone)]
pub struct CommandSender {
    high: mpsc::Sender<SessionCommand>,
    background: BudgetedSender<SessionCommand>,
}

impl CommandSender {
    /// Queue a command on the lane its class belongs to, waiting for room.
    ///
    /// # Errors
    ///
    /// [`OutboundClosed`] once the consuming side is gone.
    pub async fn send(&self, command: SessionCommand) -> Result<(), OutboundClosed> {
        match command_priority(&command) {
            SendPriority::High => self.high.send(command).await.map_err(|_| OutboundClosed),
            SendPriority::Background => {
                let bytes = command_bytes(&command);
                self.background.send(command, bytes).await
            }
        }
    }
}

/// The consuming end of a driver's command stream.
#[derive(Debug)]
pub struct CommandReceiver {
    high: mpsc::Receiver<SessionCommand>,
    background: BudgetedReceiver<SessionCommand>,
    high_closed: bool,
    background_closed: bool,
    /// Budget for the command handed out by the last [`Self::recv`], held
    /// until the caller comes back for the next one. See `recv`.
    in_flight: Option<BudgetHold>,
}

impl CommandReceiver {
    /// The next command, High lane first. Cancel-safe.
    ///
    /// The Background byte budget behaves exactly as it does everywhere else
    /// on this path: it covers the queued commands **plus the one the
    /// consumer is currently holding**, and is returned when the consumer
    /// comes back for the next command (or drops the receiver). A consumer
    /// that needs the two lanes separately — to forward each with its own
    /// task — takes them apart with [`Self::into_lanes`] instead, and the
    /// bound means the same thing there.
    ///
    /// A consequence worth stating plainly: because the in-hand command
    /// holds budget, **a consumer must not send into the same lane while
    /// holding one**. On a full lane that is a self-deadlock — it would be
    /// waiting for budget only it can release. Consumers here forward
    /// *onward*, into a different lane, which is why it does not arise.
    pub async fn recv(&mut self) -> Option<SessionCommand> {
        // The previous command is finished with by definition: the caller is
        // asking for another one.
        self.in_flight = None;
        let lane = drain_high_first(
            &mut self.high,
            &mut self.background,
            &mut self.high_closed,
            &mut self.background_closed,
        )
        .await?;
        Some(match lane {
            Lane::High(command) => command,
            Lane::Background(budgeted) => {
                let (command, hold) = budgeted.into_parts();
                self.in_flight = Some(hold);
                command
            }
        })
    }

    /// Take the two lanes apart, so each can be forwarded by its own task.
    /// This is what keeps a parked Background forwarder from holding up High
    /// commands from the same driver.
    ///
    /// Any budget still held for a command handed out by [`Self::recv`] is
    /// released here; mixing the two access styles is not a pattern the
    /// path uses.
    #[must_use]
    pub fn into_lanes(
        self,
    ) -> (
        mpsc::Receiver<SessionCommand>,
        BudgetedReceiver<SessionCommand>,
    ) {
        (self.high, self.background)
    }
}

/// Build one driver's command stream: the same High/Background lanes, with
/// the same bounds, as a session's send path.
#[must_use]
pub fn command_lanes() -> (CommandSender, CommandReceiver) {
    let (high_tx, high_rx) = mpsc::channel(MAX_HIGH_QUEUE_FRAMES);
    let (background_tx, background_rx) =
        budgeted_channel(MAX_BACKGROUND_QUEUE_FRAMES, MAX_BACKGROUND_QUEUE_BYTES);
    (
        CommandSender {
            high: high_tx,
            background: background_tx,
        },
        CommandReceiver {
            high: high_rx,
            background: background_rx,
            high_closed: false,
            background_closed: false,
            in_flight: None,
        },
    )
}

#[cfg(test)]
mod tests {
    /// The stamp has to be taken when the producer hands the frame over,
    /// not when the writer picks it up — otherwise a frame that waited
    /// behind a bulk backlog would report no wait at all, which is the one
    /// reading that must never be wrong.
    #[tokio::test]
    async fn a_frame_reports_the_time_it_spent_waiting() {
        use std::time::Duration;

        use crossover_protocol::hello::MessageType;

        let (tx, mut rx) = super::outbound_channel();
        tx.send(MessageType::InputBatch.wire(), b"moved".to_vec())
            .await
            .unwrap();

        // Held in the lane, exactly as a saturated writer would hold it.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let frame = rx.recv().await.expect("a queued frame");
        // A lower bound: sleeping longer can only make this larger, so a
        // loaded runner cannot flake it.
        assert!(
            frame.queued_at.elapsed() >= Duration::from_millis(30),
            "a frame held for 30ms reported {:?}",
            frame.queued_at.elapsed()
        );
    }

    use std::time::Duration;

    use tokio::time::timeout;

    use crossover_protocol::hello::MessageType;

    use super::{
        MAX_BACKGROUND_QUEUE_BYTES, MAX_BACKGROUND_QUEUE_FRAMES, SendPriority, budgeted_channel,
        outbound_channel,
    };

    /// The classification table, spelled out. This is the contract ADR 0013
    /// fixes; changing a row is a design decision, not a test edit.
    #[test]
    fn every_message_type_has_a_deliberate_class() {
        for high in [
            MessageType::InputBatch,
            MessageType::ReleaseAllInput,
            MessageType::ControlRequest,
            MessageType::ControlResponse,
            MessageType::ControlRelease,
            MessageType::Ping,
            MessageType::Pong,
            MessageType::Hello,
            MessageType::PairingStart,
            MessageType::PairingConfirm,
        ] {
            assert_eq!(
                SendPriority::of(high.wire()),
                SendPriority::High,
                "{high:?} must ride the interactive lane"
            );
        }
        // The whole clipboard transaction shares one lane, so its messages
        // can never overtake each other (ADR 0005's ordering invariant).
        for background in [
            MessageType::ClipboardOffer,
            MessageType::ClipboardAccept,
            MessageType::ClipboardDecline,
            MessageType::ClipboardData,
            MessageType::ClipboardChunk,
            MessageType::ClipboardApplied,
        ] {
            assert_eq!(
                SendPriority::of(background.wire()),
                SendPriority::Background,
                "{background:?} must ride the bulk lane"
            );
        }
        // Unknown to this build: bulk, never interactive.
        assert_eq!(SendPriority::of(0x7777), SendPriority::Background);
        assert_eq!(SendPriority::of(0), SendPriority::Background);
    }

    /// Fill the Background lane to its bound, then queue one input batch:
    /// the input frame must be the very next thing the writer sees, and
    /// every bulk frame must still arrive afterwards, in order.
    #[tokio::test]
    async fn an_input_batch_preempts_a_saturated_background_lane() {
        let (tx, mut rx) = outbound_channel();

        // Saturate: 64 KiB frames, so the message-count bound binds and the
        // lane holds MAX_BACKGROUND_QUEUE_FRAMES of them.
        let mut queued = 0usize;
        while tx
            .try_send(MessageType::ClipboardData.wire(), vec![0xBB; 64 * 1024])
            .is_ok()
        {
            queued += 1;
            assert!(queued < 10_000, "the background lane never filled");
        }
        assert_eq!(queued, MAX_BACKGROUND_QUEUE_FRAMES);

        // The High lane is untouched by that saturation.
        tx.send(MessageType::InputBatch.wire(), b"move".to_vec())
            .await
            .expect("a saturated background lane must not block the high lane");

        let first = rx.recv().await.expect("a frame");
        assert_eq!(first.message_type, MessageType::InputBatch.wire());
        assert_eq!(first.priority, SendPriority::High);
        assert_eq!(first.payload, b"move");

        // Reordered, not discarded: every bulk frame still comes out, FIFO.
        drop(tx);
        let mut bulk = 0usize;
        while let Some(frame) = rx.recv().await {
            assert_eq!(frame.message_type, MessageType::ClipboardData.wire());
            assert_eq!(frame.priority, SendPriority::Background);
            bulk += 1;
        }
        assert_eq!(bulk, queued, "background frames were dropped, not deferred");
    }

    /// A stuck key is release-blocking, so `ReleaseAllInput` gets its own
    /// case: it must jump a full bulk queue too.
    #[tokio::test]
    async fn release_all_input_preempts_pending_bulk() {
        let (tx, mut rx) = outbound_channel();
        for _ in 0..MAX_BACKGROUND_QUEUE_FRAMES {
            tx.send(MessageType::ClipboardData.wire(), vec![0xCC; 1024])
                .await
                .unwrap();
        }
        tx.send(MessageType::ReleaseAllInput.wire(), b"release".to_vec())
            .await
            .unwrap();

        let first = rx.recv().await.expect("a frame");
        assert_eq!(
            first.message_type,
            MessageType::ReleaseAllInput.wire(),
            "a release queued behind bulk is a stuck key"
        );
    }

    /// High is drained to empty before a single Background frame goes, and
    /// the check repeats between frames — a burst arriving mid-drain still
    /// wins over the bulk backlog.
    #[tokio::test]
    async fn high_drains_to_empty_before_one_background_frame_then_rechecks() {
        let (tx, mut rx) = outbound_channel();
        for i in 0..4u8 {
            tx.send(MessageType::ClipboardData.wire(), vec![i; 16])
                .await
                .unwrap();
        }
        for i in 0..3u8 {
            tx.send(MessageType::InputBatch.wire(), vec![i])
                .await
                .unwrap();
        }

        // All three input frames first, in order.
        for i in 0..3u8 {
            let frame = rx.recv().await.unwrap();
            assert_eq!(frame.priority, SendPriority::High);
            assert_eq!(frame.payload, vec![i]);
        }
        // Then exactly one bulk frame...
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.priority, SendPriority::Background);
        assert_eq!(frame.payload, vec![0u8; 16]);
        drop(frame);

        // ...and a batch that arrives now still overtakes the remaining bulk.
        tx.send(MessageType::InputBatch.wire(), b"late".to_vec())
            .await
            .unwrap();
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.payload, b"late");
        assert_eq!(frame.priority, SendPriority::High);
    }

    /// Clipboard messages share one lane precisely so a transaction cannot
    /// be reordered against itself, however much input interleaves.
    #[tokio::test]
    async fn clipboard_messages_keep_their_relative_order_across_the_split() {
        let (tx, mut rx) = outbound_channel();
        let transaction = [
            MessageType::ClipboardOffer,
            MessageType::ClipboardAccept,
            MessageType::ClipboardData,
            MessageType::ClipboardApplied,
        ];
        for (step, message) in transaction.iter().enumerate() {
            let marker = u8::try_from(step).unwrap();
            tx.send(message.wire(), vec![marker]).await.unwrap();
            // Interleave input at every step, which will preempt.
            tx.send(MessageType::InputBatch.wire(), vec![0xEE])
                .await
                .unwrap();
        }
        drop(tx);

        let mut clipboard_order = Vec::new();
        while let Some(frame) = rx.recv().await {
            if SendPriority::of(frame.message_type) == SendPriority::Background {
                clipboard_order.push((frame.message_type, frame.payload[0]));
            }
        }
        let expected: Vec<(u16, u8)> = transaction
            .iter()
            .enumerate()
            .map(|(step, message)| (message.wire(), u8::try_from(step).unwrap()))
            .collect();
        assert_eq!(clipboard_order, expected);
    }

    /// The Background bound is bytes, not just messages: a lane with room
    /// for eight items refuses the fifth 256-byte one, hands it back rather
    /// than dropping it, and accepts it the moment a slot's budget returns.
    #[tokio::test]
    async fn the_background_bound_is_bytes_and_rejection_never_drops() {
        let (tx, mut rx) = budgeted_channel::<u32>(8, 1024);
        for value in 0..4u32 {
            tx.try_send(value, 256).expect("within the byte budget");
        }
        // Four of eight slots used, but the byte budget is spent.
        let handed_back = tx.try_send(4, 256).expect_err("the byte budget must bind");
        assert_eq!(handed_back, 4, "a rejected item is returned, never dropped");

        // Freeing one item's budget makes exactly one more fit.
        let first = rx.try_recv().expect("a queued item");
        assert_eq!(*first, 0);
        drop(first);
        tx.try_send(4, 256)
            .expect("budget released by the drained item");

        drop(tx);
        let mut order = vec![];
        while let Some(item) = rx.recv().await {
            order.push(*item);
        }
        assert_eq!(order, vec![1, 2, 3, 4], "queued items kept their order");
    }

    /// Session teardown must not strand a producer. The writer holds the
    /// in-flight frame's budget while it writes, so at the moment a session
    /// dies the budget is routinely held by something the queue no longer
    /// owns — and a producer parked on that budget would wait for a permit
    /// nobody will ever return.
    #[tokio::test]
    async fn a_producer_parked_on_the_budget_unwinds_when_the_writer_goes_away() {
        let (tx, mut rx) = outbound_channel();
        let mut queued = 0usize;
        while tx
            .try_send(MessageType::ClipboardData.wire(), vec![0u8; 1024 * 1024])
            .is_ok()
        {
            queued += 1;
            assert!(queued < 1000, "the byte budget never bound");
        }

        // A maximum-size frame: it needs the *whole* budget, so draining the
        // queue is not enough — the in-flight frame alone keeps it parked,
        // which is the case that strands a producer.
        let parked = {
            let tx = tx.clone();
            tokio::spawn(async move {
                tx.send(
                    MessageType::ClipboardData.wire(),
                    vec![0u8; MAX_BACKGROUND_QUEUE_BYTES],
                )
                .await
            })
        };
        // Take one frame out: its budget now lives with the "writer".
        let in_flight = rx.recv().await.expect("a frame");
        assert!(!parked.is_finished(), "the producer did not actually park");

        drop(rx);
        let outcome = timeout(Duration::from_secs(5), parked)
            .await
            .expect("a producer parked on the byte budget never unwound");
        assert!(
            outcome.expect("the producer task panicked").is_err(),
            "teardown must report the path closed"
        );
        drop(in_flight);
    }

    /// The same teardown, reduced to its sharpest form: the *only* thing
    /// holding budget is a frame the writer already took.
    #[tokio::test]
    async fn teardown_unwinds_even_when_only_an_in_flight_frame_holds_budget() {
        let (tx, mut rx) = outbound_channel();
        tx.send(
            MessageType::ClipboardData.wire(),
            vec![0u8; MAX_BACKGROUND_QUEUE_BYTES],
        )
        .await
        .unwrap();
        let in_flight = rx.recv().await.expect("a frame");

        let parked = {
            let tx = tx.clone();
            tokio::spawn(async move {
                tx.send(MessageType::ClipboardData.wire(), vec![0u8; 8])
                    .await
            })
        };
        // Nothing is queued and nothing can be: the in-flight frame holds
        // the entire budget.
        assert!(
            timeout(Duration::from_millis(200), async {
                while !parked.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "the producer did not park on the budget"
        );

        drop(rx);
        let outcome = timeout(Duration::from_secs(5), parked)
            .await
            .expect("the in-flight frame's budget stranded the producer forever");
        assert!(outcome.expect("the producer task panicked").is_err());
        drop(in_flight);
    }

    /// The byte budget is a *bound*, not a ceiling on what can be sent: a
    /// frame larger than the whole budget still goes, once the lane is
    /// empty, instead of deadlocking on permits that cannot exist.
    #[tokio::test]
    async fn an_item_larger_than_the_whole_budget_still_makes_progress() {
        let (tx, mut rx) = outbound_channel();
        let huge = vec![0xAB; MAX_BACKGROUND_QUEUE_BYTES + 1];
        tx.send(MessageType::ClipboardData.wire(), huge.clone())
            .await
            .unwrap();
        let frame = rx.recv().await.expect("the oversized frame");
        assert_eq!(frame.payload.len(), huge.len());
    }
}
