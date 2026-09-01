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
    ClipboardConfig, ClipboardRetryPolicy, LocalNode, OutboundSender, SessionCommand,
    SessionListener, SessionOptions, SyncEvent, clipboard_sync, outbound_channel,
};
use crossover_platform::ClipboardProvider;
use crossover_platform::fakes::{ClipboardFailure, ClipboardOp, InMemoryClipboard};
use crossover_protocol::clipboard::{
    ApplyResult, ChunkOutcome, ChunkReassembly, ClipboardAccept, ClipboardApplied, ClipboardChunk,
    ClipboardData, ClipboardDecline, ClipboardMeta, ClipboardOffer, ContentType, DeclineReason,
    ImageFormat, MAX_CHUNK_BYTES, chunk_content, content_hash,
};
use crossover_protocol::hello::{FeatureFlags, MessageType};
use crossover_test_peer::{TestConnection, TestNode};

/// One fully wired "app side": session loop + clipboard driver + fake
/// clipboard, connected to whatever the listener accepts.
struct AppSide {
    clipboard: Arc<InMemoryClipboard>,
    /// Send frames as if another subsystem wanted the session (unused).
    _session_outbound: OutboundSender,
    _shutdown: watch::Sender<bool>,
}

fn spawn_app_side(listener: SessionListener, node: TestNode, features: FeatureFlags) -> AppSide {
    let clipboard = Arc::new(InMemoryClipboard::new());
    let (driver, sync_events, mut sync_commands) = clipboard_sync(
        Arc::clone(&clipboard) as Arc<dyn ClipboardProvider>,
        None,
        None,
        None,
        node.identity.device_id(),
        ClipboardConfig {
            retry: ClipboardRetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(20),
                // The parked phase is real here, only shrunk: the
                // proportions (a slower cadence, a budget an order of
                // magnitude past it) are what the test exercises.
                park_delay: Duration::from_millis(20),
                park_budget: Duration::from_millis(200),
            },
            transmit_debounce: Duration::from_millis(5),
            ..ClipboardConfig::new()
        },
        None,
    )
    .unwrap();
    tokio::spawn(driver.run());

    let (session_events_tx, mut session_events_rx) = mpsc::channel(64);
    let (session_outbound_tx, mut session_outbound_rx) = outbound_channel();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // Session task: accept once, run the shared session loop.
    let keepalive = KeepaliveConfig::new(Duration::from_secs(2), Duration::from_secs(30)).unwrap();
    tokio::spawn(async move {
        let (local_identity, certified, trust) = (node.identity, node.certified, node.trust);
        let local = LocalNode {
            identity: &local_identity,
            certified: &certified,
            trust: &trust,
        };
        let options = SessionOptions {
            // What this side promises the peer. Production reads
            // `FeatureFlags::ADVERTISED` (now `ALL`, since ADR 0014's
            // platform slice); the suites below set it explicitly so each
            // test states the negotiation it depends on rather than
            // inheriting whatever the constant currently says
            // (PROTOCOL.md §3.1).
            advertised_features: features,
            ..SessionOptions::default()
        };
        let session = listener.accept(&local, &options).await.expect("accept");
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
                    Some(SessionCommand::SendFrame { message_type, payload, .. }) => {
                        let _ = outbound_for_glue.send(message_type, payload).await;
                    }
                    Some(SessionCommand::TerminateSession { reason, .. }) => {
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
    connected_pair_with(FeatureFlags::ADVERTISED, FeatureFlags::NONE).await
}

/// A connected pair where each side advertises exactly what it is given.
async fn connected_pair_with(
    app_features: FeatureFlags,
    peer_features: FeatureFlags,
) -> (AppSide, TestConnection) {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("scripted-peer").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut peer_hello = peer.hello();
    peer_hello.supported_features = peer_features;
    let side = spawn_app_side(listener, app, app_features);
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

/// Assert the app sends nothing but keepalives for `window`.
///
/// "Zero payload bytes moved" is a statement about what does *not* appear
/// on the wire, so proving it means watching the wire stay quiet rather
/// than inspecting engine state.
async fn expect_quiet(conn: &mut TestConnection, window: Duration) {
    let unexpected = timeout(window, async {
        loop {
            let frame = conn.recv_frame().await.unwrap();
            if frame.message_type == MessageType::Ping.wire() {
                conn.send_frame(MessageType::Pong.wire(), 998, &[])
                    .await
                    .unwrap();
                continue;
            }
            return frame;
        }
    })
    .await;
    if let Ok(frame) = unexpected {
        panic!(
            "expected no further traffic, got message type {} carrying {} bytes",
            frame.message_type,
            frame.payload.len()
        );
    }
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

// --- chunked rich clipboard over a real session (ADR 0014) -----------------

/// Deliberately hostile bytes for anything that assumes text: non-UTF-8
/// lead bytes, embedded NULs, runs of 0xFF. An image is opaque, and the
/// only thing computed over it anywhere is its hash and its length.
fn snip_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| match i % 4 {
            0 => 0xFF,
            1 => 0x00,
            2 => 0xFE,
            _ => u8::try_from(i % 251).unwrap_or(0),
        })
        .collect()
}

fn image_meta(origin: u8, sequence: u64, bytes: &[u8]) -> ClipboardMeta {
    ClipboardMeta {
        id: Uuid::new_v4(),
        origin: Uuid::from_bytes([origin; 16]),
        sequence,
        content_type: ContentType::Image(ImageFormat::Dib),
        content_length: bytes.len() as u64,
        content_hash: content_hash(bytes),
    }
}

/// Peer→app over real TLS: an offered image is accepted, streamed as
/// chunks, reassembled, verified, installed, and only then acknowledged
/// (FR-3.2). The bytes on the destination clipboard must be byte-identical
/// to the source's — that is the whole promise of verbatim transfer.
#[tokio::test]
async fn an_offered_image_round_trips_over_a_real_session() {
    use crossover_platform::{ClipboardContent, ClipboardImageFormat};

    let (side, mut conn) =
        connected_pair_with(FeatureFlags::ALL, FeatureFlags::CHUNKED_CLIPBOARD).await;

    let bytes = snip_bytes(MAX_CHUNK_BYTES * 3 + 17);
    let meta = image_meta(0xBB, 0, &bytes);
    conn.send_frame(
        MessageType::ClipboardOffer.wire(),
        2,
        &ClipboardOffer {
            meta,
            descriptor: None,
        }
        .encode_payload()
        .unwrap(),
    )
    .await
    .unwrap();

    let accept = recv_typed(&mut conn, MessageType::ClipboardAccept).await;
    assert_eq!(
        ClipboardAccept::decode_payload(&accept).unwrap().id,
        meta.id
    );

    let chunks = chunk_content(meta.id, &bytes).unwrap();
    assert_eq!(chunks.len(), 4);
    for (id, chunk) in chunks.iter().enumerate() {
        conn.send_frame(
            MessageType::ClipboardChunk.wire(),
            10 + id as u64,
            &chunk.encode_payload().unwrap(),
        )
        .await
        .unwrap();
    }

    let ack = recv_typed(&mut conn, MessageType::ClipboardApplied).await;
    let applied = ClipboardApplied::decode_payload(&ack).unwrap();
    assert_eq!(applied.id, meta.id);
    assert_eq!(applied.result, ApplyResult::Applied);
    assert_eq!(
        side.clipboard.peek_content(),
        Some(ClipboardContent::Image {
            format: ClipboardImageFormat::Dib,
            bytes,
        }),
        "the installed image is not byte-identical to the source"
    );
}

/// App→peer over real TLS: a local snip is offered (never inline), and on
/// acceptance streams as one chunk per frame, in order, reassembling to
/// exactly what was copied.
#[tokio::test]
async fn a_local_image_is_offered_and_streamed_to_the_peer() {
    use crossover_platform::ClipboardImageFormat;

    let (side, mut conn) =
        connected_pair_with(FeatureFlags::ALL, FeatureFlags::CHUNKED_CLIPBOARD).await;

    let bytes = snip_bytes(MAX_CHUNK_BYTES * 2 + 5);
    side.clipboard
        .set_image_locally(ClipboardImageFormat::Dib, bytes.clone());

    let payload = recv_typed(&mut conn, MessageType::ClipboardOffer).await;
    let offer = ClipboardOffer::decode_payload(&payload).unwrap();
    assert_eq!(
        offer.meta.content_type,
        ContentType::Image(ImageFormat::Dib)
    );
    assert_eq!(offer.meta.content_length, bytes.len() as u64);

    conn.send_frame(
        MessageType::ClipboardAccept.wire(),
        3,
        &ClipboardAccept { id: offer.meta.id }
            .encode_payload()
            .unwrap(),
    )
    .await
    .unwrap();

    // Reassemble with the receiver's own machinery: it enforces the
    // sequence, the per-chunk lengths, and finally the item hash, so a
    // successful completion *is* the assertion that the stream was well
    // formed and complete.
    let mut reassembly = ChunkReassembly::begin(offer.meta).unwrap();
    let received = loop {
        let payload = recv_typed(&mut conn, MessageType::ClipboardChunk).await;
        let chunk = ClipboardChunk::decode_payload(&payload).unwrap();
        match reassembly.accept(&chunk).unwrap() {
            ChunkOutcome::More => {}
            ChunkOutcome::Complete(bytes) => break bytes,
        }
    };
    assert_eq!(received, bytes, "the streamed image was not verbatim");

    // Closing the transaction produces no further traffic.
    conn.send_frame(
        MessageType::ClipboardApplied.wire(),
        4,
        &ClipboardApplied {
            id: offer.meta.id,
            result: ApplyResult::Applied,
        }
        .encode_payload()
        .unwrap(),
    )
    .await
    .unwrap();
}

/// The payoff the offer round exists for (ADR 0014), and the Phase 7 exit
/// criterion attached to it: an image crosses **once**. Re-offering content
/// the app already holds costs one offer and one decline — no accept, no
/// chunks, no payload bytes — instead of megabytes.
///
/// The engine has a unit test for the *decision*. What only a real session
/// can show is that nothing followed the decline onto the wire, which is
/// the half the criterion is actually about.
#[tokio::test]
async fn an_image_that_already_matches_is_declined_with_no_bytes_behind_it() {
    use crossover_platform::ClipboardImageFormat;

    let (side, mut conn) =
        connected_pair_with(FeatureFlags::ALL, FeatureFlags::CHUNKED_CLIPBOARD).await;

    // Pay the full cost once: the app copies a snip and streams it out.
    let bytes = snip_bytes(MAX_CHUNK_BYTES * 2 + 5);
    side.clipboard
        .set_image_locally(ClipboardImageFormat::Dib, bytes.clone());

    let payload = recv_typed(&mut conn, MessageType::ClipboardOffer).await;
    let offer = ClipboardOffer::decode_payload(&payload).unwrap();
    conn.send_frame(
        MessageType::ClipboardAccept.wire(),
        3,
        &ClipboardAccept { id: offer.meta.id }
            .encode_payload()
            .unwrap(),
    )
    .await
    .unwrap();

    let mut reassembly = ChunkReassembly::begin(offer.meta).unwrap();
    let streamed = loop {
        let payload = recv_typed(&mut conn, MessageType::ClipboardChunk).await;
        let chunk = ClipboardChunk::decode_payload(&payload).unwrap();
        match reassembly.accept(&chunk).unwrap() {
            ChunkOutcome::More => {}
            ChunkOutcome::Complete(bytes) => break bytes,
        }
    };
    assert_eq!(streamed, bytes);
    conn.send_frame(
        MessageType::ClipboardApplied.wire(),
        4,
        &ClipboardApplied {
            id: offer.meta.id,
            result: ApplyResult::Applied,
        }
        .encode_payload()
        .unwrap(),
    )
    .await
    .unwrap();

    // Now the peer copies that same image and offers it back — the
    // re-paste. Same content, so the same hash; a new transaction id,
    // because it is a new transaction.
    let repeat = image_meta(0xBB, 9, &bytes);
    assert_eq!(repeat.content_hash, offer.meta.content_hash);
    conn.send_frame(
        MessageType::ClipboardOffer.wire(),
        5,
        &ClipboardOffer {
            meta: repeat,
            descriptor: None,
        }
        .encode_payload()
        .unwrap(),
    )
    .await
    .unwrap();

    let payload = recv_typed(&mut conn, MessageType::ClipboardDecline).await;
    let decline = ClipboardDecline::decode_payload(&payload).unwrap();
    assert_eq!(decline.id, repeat.id);
    assert_eq!(
        decline.reason,
        DeclineReason::AlreadyHave,
        "a re-offered image must be recognised, not re-transferred"
    );

    // The assertion the criterion is really made of: nothing follows.
    expect_quiet(&mut conn, Duration::from_millis(400)).await;
}

/// The honesty rule, end to end (docs/PROTOCOL.md §3.1). The app side
/// advertises `FeatureFlags::ADVERTISED` — `ALL` since ADR 0014's platform
/// slice — and the scripted peer advertises nothing, exactly as a build
/// from before the bit would. So this is the compatibility case the flip
/// has to survive: with the capability un-negotiated, an image copy must
/// not reach the wire and must not wedge the pipeline: text keeps
/// synchronizing immediately after.
#[tokio::test]
async fn an_un_negotiated_image_never_reaches_the_wire_and_text_still_flows() {
    use crossover_platform::ClipboardImageFormat;

    let (side, mut conn) = connected_pair().await; // peer advertises nothing
    side.clipboard
        .set_image_locally(ClipboardImageFormat::Dib, snip_bytes(MAX_CHUNK_BYTES * 2));

    // Whatever the engine produced was refused at the send gate; the very
    // next thing the peer sees is the text copied after it.
    side.clipboard
        .set_text_locally("text after a refused image");
    let payload = recv_typed(&mut conn, MessageType::ClipboardData).await;
    let data = ClipboardData::decode_payload(&payload).unwrap();
    assert_eq!(data.content, b"text after a refused image");
    assert_eq!(data.meta.content_type, ContentType::Utf8Text);
}

/// The other side of the same coin, and the end-to-end proof that flipping
/// `FeatureFlags::ADVERTISED` is all it takes: **neither** side overrides
/// anything, so both advertise exactly what a shipped build advertises,
/// and a local image copy reaches the wire as a chunked offer.
///
/// The tests above deliberately state their negotiation, which means none
/// of them would notice the constant regressing to empty. This one would.
#[tokio::test]
async fn two_default_builds_negotiate_image_transfer_with_no_overrides() {
    use crossover_platform::ClipboardImageFormat;

    let (side, mut conn) =
        connected_pair_with(FeatureFlags::ADVERTISED, FeatureFlags::ADVERTISED).await;

    let bytes = snip_bytes(MAX_CHUNK_BYTES + 9);
    side.clipboard
        .set_image_locally(ClipboardImageFormat::Dib, bytes.clone());

    // An image is always offered, never inlined (ADR 0014), and it got
    // past the send gate on the negotiated capability alone.
    let payload = recv_typed(&mut conn, MessageType::ClipboardOffer).await;
    let offer = ClipboardOffer::decode_payload(&payload).unwrap();
    assert_eq!(
        offer.meta.content_type,
        ContentType::Image(ImageFormat::Dib)
    );
    assert_eq!(offer.meta.content_length, bytes.len() as u64);

    // And the chunks follow, which is the traffic an un-negotiated peer
    // must never see.
    conn.send_frame(
        MessageType::ClipboardAccept.wire(),
        3,
        &ClipboardAccept { id: offer.meta.id }
            .encode_payload()
            .unwrap(),
    )
    .await
    .unwrap();
    let mut reassembly = ChunkReassembly::begin(offer.meta).unwrap();
    let received = loop {
        let payload = recv_typed(&mut conn, MessageType::ClipboardChunk).await;
        let chunk = ClipboardChunk::decode_payload(&payload).unwrap();
        match reassembly.accept(&chunk).unwrap() {
            ChunkOutcome::More => {}
            ChunkOutcome::Complete(bytes) => break bytes,
        }
    };
    assert_eq!(received, bytes);
}
