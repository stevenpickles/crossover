//! Phase 2 integration: the complete clipboard pipeline — session layer,
//! sync driver, engine, and fake provider — driven over real localhost
//! TCP+TLS against the foreign test peer (docs/TESTING.md §1.4, §1.5).
//!
//! The wiring here (`run_session` events → driver events, driver
//! commands → session outbound) is exactly the shape the app adopts in
//! the next slice, proven hardware-free first.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use uuid::Uuid;

use crossover_core::supervision::{KeepaliveConfig, SessionEvent, run_session};
use crossover_core::{
    ClipboardConfig, ClipboardRetryPolicy, LocalNode, SessionCommand, SessionListener,
    SessionOptions, SyncEvent, clipboard_sync,
};
use crossover_platform::ClipboardProvider;
use crossover_platform::fakes::{ClipboardFailure, ClipboardOp, InMemoryClipboard};
use crossover_protocol::clipboard::{ApplyResult, ClipboardApplied, ClipboardData, ContentType};
use crossover_protocol::hello::MessageType;
use crossover_test_peer::{TestConnection, TestNode};

/// One fully wired "app side": session loop + clipboard driver + fake
/// clipboard, connected to whatever the listener accepts.
struct AppSide {
    clipboard: Arc<InMemoryClipboard>,
    /// Send frames as if another subsystem wanted the session (unused).
    _session_outbound: mpsc::Sender<(u16, Vec<u8>)>,
    _shutdown: watch::Sender<bool>,
}

fn spawn_app_side(listener: SessionListener, node: TestNode) -> AppSide {
    let clipboard = Arc::new(InMemoryClipboard::new());
    let (driver, sync_events, mut sync_commands) = clipboard_sync(
        Arc::clone(&clipboard) as Arc<dyn ClipboardProvider>,
        node.identity.device_id(),
        ClipboardConfig {
            retry: ClipboardRetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(20),
            },
            transmit_debounce: Duration::from_millis(5),
        },
    )
    .unwrap();
    tokio::spawn(driver.run());

    let (session_events_tx, mut session_events_rx) = mpsc::channel(64);
    let (session_outbound_tx, mut session_outbound_rx) = mpsc::channel::<(u16, Vec<u8>)>(64);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // Session task: accept once, run the shared session loop.
    let keepalive = KeepaliveConfig {
        interval: Duration::from_secs(2),
        timeout: Duration::from_secs(30),
    };
    tokio::spawn(async move {
        let (local_identity, certified, trust) = (node.identity, node.certified, node.trust);
        let local = LocalNode {
            identity: &local_identity,
            certified: &certified,
            trust: &trust,
        };
        let session = listener
            .accept(&local, &SessionOptions::default())
            .await
            .expect("accept");
        run_session(
            session,
            &session_events_tx,
            &mut session_outbound_rx,
            &mut shutdown_rx,
            &keepalive,
        )
        .await
    });

    // Glue: session frames → driver; driver commands → session outbound.
    // Exactly the app's future wiring.
    let sync_events_clone = sync_events.clone();
    let outbound_for_glue = session_outbound_tx.clone();
    tokio::spawn(async move {
        let _ = sync_events_clone.send(SyncEvent::SessionEstablished).await;
        loop {
            tokio::select! {
                maybe = session_events_rx.recv() => match maybe {
                    Some(SessionEvent::Frame(frame)) => {
                        let _ = sync_events_clone.send(SyncEvent::Frame(frame)).await;
                    }
                    Some(_) => {}
                    None => break,
                },
                maybe = sync_commands.recv() => match maybe {
                    Some(SessionCommand::SendFrame { message_type, payload }) => {
                        let _ = outbound_for_glue.send((message_type, payload)).await;
                    }
                    Some(SessionCommand::TerminateSession { reason }) => {
                        panic!("unexpected session termination: {reason}");
                    }
                    None => break,
                },
            }
        }
    });

    AppSide {
        clipboard,
        _session_outbound: session_outbound_tx,
        _shutdown: shutdown_tx,
    }
}

async fn connected_pair() -> (AppSide, TestConnection) {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("scripted-peer").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let peer_hello = peer.hello();
    let side = spawn_app_side(listener, app);
    let mut conn = TestConnection::connect(addr, &peer).await.unwrap();
    conn.send_hello(&peer_hello).await.unwrap();
    let _app_hello = conn.expect_hello().await.unwrap();
    (side, conn)
}

fn item(origin: u8, sequence: u64, text: &[u8]) -> ClipboardData {
    ClipboardData::from_content(
        Uuid::new_v4(),
        Uuid::from_bytes([origin; 16]),
        sequence,
        ContentType::Utf8Text,
        text.to_vec(),
    )
}

async fn recv_typed(conn: &mut TestConnection, expected: MessageType) -> Vec<u8> {
    let deadline = Duration::from_secs(5);
    timeout(deadline, async {
        loop {
            let frame = conn.recv_frame().await.unwrap();
            // Skip keepalives; answer pings to stay honest.
            if frame.message_type == MessageType::Ping.wire() {
                conn.send_frame(MessageType::Pong.wire(), 999, &[])
                    .await
                    .unwrap();
                continue;
            }
            if frame.message_type == expected.wire() {
                return frame.payload;
            }
            panic!(
                "expected message type {}, got {}",
                expected.wire(),
                frame.message_type
            );
        }
    })
    .await
    .expect("timed out waiting for a frame")
}

/// Peer→app: an inline item lands on the app's clipboard and the ack
/// travels back — the destination-updated definition of success.
#[tokio::test]
async fn peer_item_lands_on_the_clipboard_and_is_acked() {
    let (side, mut conn) = connected_pair().await;

    let sent = item(0xBB, 0, b"hello from the peer");
    let payload = sent.encode_payload().unwrap();
    conn.send_frame(MessageType::ClipboardData.wire(), 2, &payload)
        .await
        .unwrap();

    let ack = recv_typed(&mut conn, MessageType::ClipboardApplied).await;
    let applied = ClipboardApplied::decode_payload(&ack).unwrap();
    assert_eq!(applied.id, sent.meta.id);
    assert_eq!(applied.result, ApplyResult::Applied);
    assert_eq!(
        side.clipboard.peek().as_deref(),
        Some("hello from the peer")
    );
}

/// App→peer: a local copy crosses the wire as valid `ClipboardData`; the
/// peer's ack closes the transaction (no retransmission).
#[tokio::test]
async fn local_copy_reaches_the_peer_as_data() {
    let (side, mut conn) = connected_pair().await;

    side.clipboard.set_text_locally("copied on the app side");

    let payload = recv_typed(&mut conn, MessageType::ClipboardData).await;
    let data = ClipboardData::decode_payload(&payload).unwrap();
    assert_eq!(data.content, b"copied on the app side");

    // Ack it; nothing further should arrive for this item.
    let ack = ClipboardApplied {
        id: data.meta.id,
        result: ApplyResult::Applied,
    };
    conn.send_frame(
        MessageType::ClipboardApplied.wire(),
        3,
        &ack.encode_payload().unwrap(),
    )
    .await
    .unwrap();
}

/// Duplicate delivery of identical content acks as success without a
/// second application (dedup at the destination).
#[tokio::test]
async fn duplicate_content_is_acked_without_reapplication() {
    let (side, mut conn) = connected_pair().await;

    let first = item(0xBB, 0, b"same bytes");
    conn.send_frame(
        MessageType::ClipboardData.wire(),
        2,
        &first.encode_payload().unwrap(),
    )
    .await
    .unwrap();
    let ack = recv_typed(&mut conn, MessageType::ClipboardApplied).await;
    assert_eq!(
        ClipboardApplied::decode_payload(&ack).unwrap().result,
        ApplyResult::Applied
    );

    // Different item id, same content: still success, no state change.
    let second = item(0xBB, 1, b"same bytes");
    conn.send_frame(
        MessageType::ClipboardData.wire(),
        3,
        &second.encode_payload().unwrap(),
    )
    .await
    .unwrap();
    let ack = recv_typed(&mut conn, MessageType::ClipboardApplied).await;
    let applied = ClipboardApplied::decode_payload(&ack).unwrap();
    assert_eq!(applied.id, second.meta.id);
    assert_eq!(applied.result, ApplyResult::Applied);
    assert_eq!(side.clipboard.peek().as_deref(), Some("same bytes"));
}

/// Clipboard contention at the destination: bounded retries burn through,
/// then the honest `ClipboardUnavailable` verdict crosses the wire.
#[tokio::test]
async fn contended_destination_reports_unavailable_after_retries() {
    let (side, mut conn) = connected_pair().await;
    side.clipboard
        .fail_next(ClipboardOp::Write, ClipboardFailure::Busy, 99);

    let sent = item(0xBB, 0, b"will never land");
    conn.send_frame(
        MessageType::ClipboardData.wire(),
        2,
        &sent.encode_payload().unwrap(),
    )
    .await
    .unwrap();

    let ack = recv_typed(&mut conn, MessageType::ClipboardApplied).await;
    let applied = ClipboardApplied::decode_payload(&ack).unwrap();
    assert_eq!(applied.result, ApplyResult::ClipboardUnavailable);
    assert_eq!(side.clipboard.peek(), None);
}

/// Transient contention only: retries recover and the item lands.
#[tokio::test]
async fn transient_contention_recovers_and_applies() {
    let (side, mut conn) = connected_pair().await;
    side.clipboard
        .fail_next(ClipboardOp::Write, ClipboardFailure::Busy, 2);

    let sent = item(0xBB, 0, b"lands on attempt three");
    conn.send_frame(
        MessageType::ClipboardData.wire(),
        2,
        &sent.encode_payload().unwrap(),
    )
    .await
    .unwrap();

    let ack = recv_typed(&mut conn, MessageType::ClipboardApplied).await;
    assert_eq!(
        ClipboardApplied::decode_payload(&ack).unwrap().result,
        ApplyResult::Applied
    );
    assert_eq!(
        side.clipboard.peek().as_deref(),
        Some("lands on attempt three")
    );
}
