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

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crossover_platform::{ClipboardError, ClipboardProvider};
use crossover_protocol::RawFrame;
use crossover_protocol::clipboard::{ApplyResult, ClipboardApplied};
use crossover_protocol::hello::MessageType;

use crate::clipboard::{Action, ClipboardConfig, ClipboardEngine, InboundMessage};
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
}

/// Which session(s) a [`SessionCommand`] is directed at.
///
/// Clipboard sync is session-agnostic (FR-5.4) and broadcasts; control
/// and input traffic is authority for one authenticated session and is
/// routed to exactly that one (FR-5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameTarget {
    /// Every active session.
    Broadcast,
    /// One session, by its locally generated id.
    Session(Uuid),
}

/// What a driver asks the app to do.
#[derive(Debug)]
pub enum SessionCommand {
    /// Send this frame to the target session(s).
    SendFrame {
        /// Which session(s) to send it to.
        target: FrameTarget,
        /// Frame message type.
        message_type: u16,
        /// Encoded payload.
        payload: Vec<u8>,
    },
    /// The target sent an invalid payload: terminate it (fail closed);
    /// supervision handles the rest.
    TerminateSession {
        /// Which session(s) to terminate.
        target: FrameTarget,
        /// Diagnostic for logs.
        reason: String,
    },
}

/// The clipboard sync driver. Create with [`clipboard_sync`], then spawn
/// [`ClipboardSyncDriver::run`].
pub struct ClipboardSyncDriver {
    engine: ClipboardEngine,
    provider: Arc<dyn ClipboardProvider>,
    events_rx: mpsc::Receiver<SyncEvent>,
    events_tx: mpsc::Sender<SyncEvent>,
    commands_tx: CommandSender,
    /// Consecutive `Busy` reads; reset by any successful read.
    busy_reads: u32,
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
    let (events_tx, events_rx) = mpsc::channel(64);
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
        events_rx,
        events_tx: events_tx.clone(),
        commands_tx,
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
    pub async fn run(mut self) {
        while let Some(event) = self.events_rx.recv().await {
            // Coalesce before acting. The OS clipboard is a single-value
            // register, not a queue: when several items are already
            // waiting, applying the older ones writes content nobody can
            // ever paste, and every wasted write takes the machine-global
            // clipboard lock — which is how Crossover made other
            // applications' clipboard calls fail in the two-machine soak
            // (52 writes in one second while draining a backlog).
            let batch = self.coalesce(event).await;
            for event in batch {
                let actions = match event {
                    SyncEvent::SessionEstablished => self.engine.on_session_established(),
                    SyncEvent::SessionLost => self.engine.on_session_lost(),
                    SyncEvent::LocalChanged => self.engine.on_local_change(),
                    SyncEvent::RetryDue(id) => self.engine.on_retry_due(id),
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
                                // Peer nonconformance: fail closed. Clipboard
                                // is session-agnostic and does not track which
                                // session a frame arrived on, so the fail-
                                // closed kill is a broadcast (two-machine: the
                                // one session).
                                let _ = self
                                    .commands_tx
                                    .send(SessionCommand::TerminateSession {
                                        target: FrameTarget::Broadcast,
                                        reason: error.to_string(),
                                    })
                                    .await;
                                Vec::new()
                            }
                        }
                    }
                };
                if !self.execute(actions).await {
                    return; // command receiver gone: the app is shutting down
                }
            }
        }
        tracing::debug!("clipboard sync driver stopped");
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

    /// Execute engine actions, feeding results back through the engine
    /// until the queue drains. Returns `false` when the app side is gone.
    async fn execute(&mut self, actions: Vec<Action>) -> bool {
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::ReadClipboard => match self.provider.read_text() {
                    Ok(content) => {
                        self.busy_reads = 0;
                        queue.extend(self.engine.on_local_read(content));
                    }
                    Err(ClipboardError::Busy { reason }) => {
                        self.busy_reads += 1;
                        self.record(Metrics::record_clipboard_contention);
                        if self.busy_reads > MAX_CONSECUTIVE_BUSY_READS {
                            // Stop nudging: the change listener will wake
                            // us for anything that actually changes, and
                            // continuing would starve inbound frames on
                            // this same queue.
                            tracing::warn!(
                                error = %reason,
                                attempt_count = self.busy_reads,
                                "clipboard read still busy; waiting for the next change                                  notification instead of re-checking"
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
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "clipboard read failed");
                    }
                },
                Action::WriteClipboard { id, text } => {
                    let result = match self.provider.write_text(&text) {
                        Ok(()) => Ok(()),
                        Err(ClipboardError::Busy { reason }) => {
                            tracing::debug!(clipboard_id = %id, error = %reason, "write busy");
                            self.record(Metrics::record_clipboard_contention);
                            Err(true)
                        }
                        Err(error) => {
                            tracing::warn!(clipboard_id = %id, error = %error, "write failed");
                            Err(false)
                        }
                    };
                    queue.extend(self.engine.on_write_result(id, result));
                }
                Action::Send(message) => match message.encode() {
                    Ok((message_type, payload)) => {
                        if self
                            .commands_tx
                            .send(SessionCommand::SendFrame {
                                target: FrameTarget::Broadcast,
                                message_type,
                                payload,
                            })
                            .await
                            .is_err()
                        {
                            return false;
                        }
                    }
                    Err(error) => {
                        // Engine-built messages are always valid; log the
                        // impossible rather than panic (NFR-1 discipline).
                        tracing::error!(error = %error, "unencodable engine message dropped");
                    }
                },
                Action::ScheduleRetry { id, delay } => {
                    self.record(Metrics::record_clipboard_retry);
                    let notify = self.events_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = notify.send(SyncEvent::RetryDue(id)).await;
                    });
                }
                Action::ScheduleSettle { delay } => {
                    // Bump the generation: any timer already in flight
                    // becomes a no-op when it fires, so the debounce
                    // restarts without cancellation bookkeeping.
                    self.settle_generation += 1;
                    let generation = self.settle_generation;
                    let notify = self.events_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = notify.send(SyncEvent::SettleDue(generation)).await;
                    });
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crossover_platform::fakes::{ClipboardFailure, ClipboardOp, InMemoryClipboard};
    use crossover_protocol::RawFrame;
    use crossover_protocol::clipboard::{
        ApplyResult, ClipboardApplied, ClipboardData, ContentType,
    };
    use crossover_protocol::hello::MessageType;

    use super::{SessionCommand, SyncEvent, clipboard_sync};
    use crate::clipboard::{ClipboardConfig, RetryPolicy};
    use crate::metrics::Metrics;

    struct Rig {
        clipboard: Arc<InMemoryClipboard>,
        events: mpsc::Sender<SyncEvent>,
        commands: crate::outbound::CommandReceiver,
        metrics: Arc<Metrics>,
    }

    fn rig() -> Rig {
        let clipboard = Arc::new(InMemoryClipboard::new());
        let config = ClipboardConfig {
            retry: RetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(20),
            },
            // Tests drive the trigger's *behaviour*, not the wait.
            transmit_debounce: Duration::from_millis(5),
        };
        let metrics = Arc::new(Metrics::new());
        let (driver, events, commands) = clipboard_sync(
            Arc::clone(&clipboard) as Arc<dyn crossover_platform::ClipboardProvider>,
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
}
