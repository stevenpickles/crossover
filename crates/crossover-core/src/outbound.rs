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

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crossover_protocol::hello::MessageType;

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
    /// `Offer`/`Accept`/`Decline`/`Applied` ones the ADR left open. Splitting
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
    #[must_use]
    pub fn of(message_type: u16) -> Self {
        match MessageType::from_wire(message_type) {
            Some(
                MessageType::InputBatch
                | MessageType::ReleaseAllInput
                | MessageType::ControlRequest
                | MessageType::ControlResponse
                | MessageType::ControlRelease
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
                | MessageType::ClipboardApplied,
            )
            | None => Self::Background,
        }
    }

    /// Whether this is the interactive class.
    #[must_use]
    pub const fn is_high(self) -> bool {
        matches!(self, Self::High)
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
    /// # Errors
    ///
    /// The item itself, handed back untouched, when the lane is at one of
    /// its bounds or closed — a *rejection*, never a silent drop. Used to
    /// probe saturation without blocking.
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
#[derive(Debug)]
pub struct BudgetedReceiver<T> {
    rx: mpsc::Receiver<Budgeted<T>>,
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
#[must_use]
pub fn budgeted_channel<T>(
    max_items: usize,
    max_bytes: usize,
) -> (BudgetedSender<T>, BudgetedReceiver<T>) {
    let (tx, rx) = mpsc::channel(max_items);
    let capacity_bytes = u32::try_from(max_bytes).unwrap_or(u32::MAX);
    (
        BudgetedSender {
            tx,
            budget: Arc::new(Semaphore::new(capacity_bytes as usize)),
            capacity_bytes,
        },
        BudgetedReceiver { rx },
    )
}

// ---------------------------------------------------------------------------
// The two-lane outbound path
// ---------------------------------------------------------------------------

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
    _hold: Option<BudgetHold>,
}

impl OutboundFrame {
    fn high(message_type: u16, payload: Vec<u8>) -> Self {
        Self {
            message_type,
            payload,
            priority: SendPriority::High,
            _hold: None,
        }
    }

    fn background(budgeted: Budgeted<(u16, Vec<u8>)>) -> Self {
        let ((message_type, payload), hold) = budgeted.into_parts();
        Self {
            message_type,
            payload,
            priority: SendPriority::Background,
            _hold: Some(hold),
        }
    }
}

/// The application's end of a session's send path. Cloneable: every live
/// producer shares the same two lanes, and classification happens here, at
/// the moment a frame is handed over.
#[derive(Debug, Clone)]
pub struct OutboundSender {
    high: mpsc::Sender<(u16, Vec<u8>)>,
    background: BudgetedSender<(u16, Vec<u8>)>,
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
                .send((message_type, payload))
                .await
                .map_err(|_| OutboundClosed),
            SendPriority::Background => {
                let bytes = payload.len();
                self.background.send((message_type, payload), bytes).await
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
                .try_send((message_type, payload))
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full((_, queued))
                    | mpsc::error::TrySendError::Closed((_, queued)) => queued,
                }),
            SendPriority::Background => {
                let bytes = payload.len();
                self.background
                    .try_send((message_type, payload), bytes)
                    .map_err(|(_, queued)| queued)
            }
        }
    }
}

/// The writer's end of a session's send path: the scheduler that makes the
/// priority real.
#[derive(Debug)]
pub struct OutboundReceiver {
    high: mpsc::Receiver<(u16, Vec<u8>)>,
    background: BudgetedReceiver<(u16, Vec<u8>)>,
    high_closed: bool,
    background_closed: bool,
}

impl OutboundReceiver {
    /// The next frame to write: **everything queued High first, then at most
    /// one Background frame** (ADR 0013).
    ///
    /// Because the caller writes one frame per call, the High lane is
    /// re-checked between every pair of frames — which is what keeps the
    /// kernel send buffer shallow enough for app-level priority to reach the
    /// wire. Never reorders within a class, never drops.
    ///
    /// `None` once both lanes are closed and drained. Cancel-safe: no frame
    /// is taken without being returned.
    pub async fn recv(&mut self) -> Option<OutboundFrame> {
        loop {
            // Strict priority. This runs before *every* Background frame, so
            // an input batch that arrived while the previous frame was on the
            // wire preempts the rest of the bulk queue.
            match self.high.try_recv() {
                Ok((message_type, payload)) => {
                    return Some(OutboundFrame::high(message_type, payload));
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => self.high_closed = true,
            }
            match self.background.try_recv() {
                Ok(budgeted) => return Some(OutboundFrame::background(budgeted)),
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => self.background_closed = true,
            }
            if self.high_closed && self.background_closed {
                return None;
            }

            // Both lanes are empty: wait for whichever wakes first, still
            // biased so a simultaneous arrival resolves to High.
            tokio::select! {
                biased;
                item = self.high.recv(), if !self.high_closed => match item {
                    Some((message_type, payload)) => {
                        return Some(OutboundFrame::high(message_type, payload));
                    }
                    None => self.high_closed = true,
                },
                item = self.background.recv(), if !self.background_closed => match item {
                    Some(budgeted) => return Some(OutboundFrame::background(budgeted)),
                    None => self.background_closed = true,
                },
            }
        }
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

#[cfg(test)]
mod tests {
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
