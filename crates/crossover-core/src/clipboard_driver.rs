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
//! validation produces [`SyncCommand::TerminateSession`] — the app kills
//! that session (docs/PROTOCOL.md §7) and supervision reconnects.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crossover_platform::{ClipboardError, ClipboardProvider};
use crossover_protocol::RawFrame;

use crate::clipboard::{Action, ClipboardEngine, InboundMessage, RetryPolicy};

/// How long after a `Busy` *read* before re-checking the clipboard. Reads
/// have no transaction to retry inside the engine; the driver simply
/// looks again shortly (the next change notification would also do it).
const READ_RETRY_DELAY: Duration = Duration::from_millis(100);

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
}

/// What the driver asks the app to do.
#[derive(Debug)]
pub enum SyncCommand {
    /// Send this frame to the peer over the active session.
    SendFrame {
        /// Frame message type.
        message_type: u16,
        /// Encoded payload.
        payload: Vec<u8>,
    },
    /// The peer sent an invalid clipboard payload: terminate the session
    /// (fail closed); supervision handles the rest.
    TerminateSession {
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
    commands_tx: mpsc::Sender<SyncCommand>,
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
    retry: RetryPolicy,
) -> Result<
    (
        ClipboardSyncDriver,
        mpsc::Sender<SyncEvent>,
        mpsc::Receiver<SyncCommand>,
    ),
    ClipboardError,
> {
    let (events_tx, events_rx) = mpsc::channel(64);
    let (commands_tx, commands_rx) = mpsc::channel(64);

    // Bridge the dataless provider callback into the event loop.
    // try_send + drop-on-full IS the documented coalescing: a full queue
    // already holds a pending "look at the clipboard" signal.
    let notify = events_tx.clone();
    provider.set_change_listener(Some(Box::new(move || {
        let _ = notify.try_send(SyncEvent::LocalChanged);
    })))?;

    let driver = ClipboardSyncDriver {
        engine: ClipboardEngine::new(origin, retry),
        provider,
        events_rx,
        events_tx: events_tx.clone(),
        commands_tx,
    };
    Ok((driver, events_tx, commands_rx))
}

impl ClipboardSyncDriver {
    /// Run until every event sender is dropped. Spawn this.
    pub async fn run(mut self) {
        while let Some(event) = self.events_rx.recv().await {
            let actions = match event {
                SyncEvent::SessionEstablished => self.engine.on_session_established(),
                SyncEvent::SessionLost => self.engine.on_session_lost(),
                SyncEvent::LocalChanged => self.engine.on_local_change(),
                SyncEvent::RetryDue(id) => self.engine.on_retry_due(id),
                SyncEvent::Frame(frame) => {
                    match InboundMessage::decode(frame.message_type, &frame.payload) {
                        Ok(Some(message)) => self.engine.on_peer_message(message),
                        Ok(None) => Vec::new(), // not clipboard traffic
                        Err(error) => {
                            // Peer nonconformance: fail closed.
                            let _ = self
                                .commands_tx
                                .send(SyncCommand::TerminateSession {
                                    reason: error.to_string(),
                                })
                                .await;
                            Vec::new()
                        }
                    }
                }
            };
            if !self.execute(actions).await {
                break; // command receiver gone: the app is shutting down
            }
        }
        tracing::debug!("clipboard sync driver stopped");
    }

    /// Execute engine actions, feeding results back through the engine
    /// until the queue drains. Returns `false` when the app side is gone.
    async fn execute(&mut self, actions: Vec<Action>) -> bool {
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::ReadClipboard => match self.provider.read_text() {
                    Ok(content) => queue.extend(self.engine.on_local_read(content)),
                    Err(ClipboardError::Busy { reason }) => {
                        tracing::debug!(error = %reason, "clipboard read busy; will look again");
                        let notify = self.events_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(READ_RETRY_DELAY).await;
                            let _ = notify.try_send(SyncEvent::LocalChanged);
                        });
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
                            .send(SyncCommand::SendFrame {
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
                    let notify = self.events_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = notify.send(SyncEvent::RetryDue(id)).await;
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

    use super::{SyncCommand, SyncEvent, clipboard_sync};
    use crate::clipboard::RetryPolicy;

    struct Rig {
        clipboard: Arc<InMemoryClipboard>,
        events: mpsc::Sender<SyncEvent>,
        commands: mpsc::Receiver<SyncCommand>,
    }

    fn rig() -> Rig {
        let clipboard = Arc::new(InMemoryClipboard::new());
        let retry = RetryPolicy {
            max_attempts: 3,
            delay: Duration::from_millis(20),
        };
        let (driver, events, commands) = clipboard_sync(
            Arc::clone(&clipboard) as Arc<dyn crossover_platform::ClipboardProvider>,
            Uuid::from_bytes([0xAA; 16]),
            retry,
        )
        .unwrap();
        tokio::spawn(driver.run());
        Rig {
            clipboard,
            events,
            commands,
        }
    }

    async fn next_command(rig: &mut Rig) -> SyncCommand {
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

        let SyncCommand::SendFrame {
            message_type,
            payload,
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

        let SyncCommand::SendFrame {
            message_type,
            payload,
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
        let SyncCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("expected SendFrame");
        };
        let applied = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(applied.result, ApplyResult::Applied);
        assert_eq!(rig.clipboard.peek().as_deref(), Some("contended"));
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

        let SyncCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
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
            SyncCommand::TerminateSession { .. }
        ));
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

        let SyncCommand::SendFrame { payload, .. } = next_command(&mut rig).await else {
            panic!("expected SendFrame");
        };
        let data = ClipboardData::decode_payload(&payload).unwrap();
        assert_eq!(data.content, b"survives the gap");
    }
}
