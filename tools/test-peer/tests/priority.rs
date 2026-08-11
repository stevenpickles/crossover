//! Interactive-over-bulk prioritization on a real session
//! (docs/TESTING.md §1.4, §1.5; [ADR 0013]).
//!
//! Everything above the socket is already covered by unit tests. What this
//! suite adds is the part that only shows up with a real writer on a real
//! TLS-over-TCP stream: a **saturating background transfer** big enough to
//! fill every queue *and* the kernel send buffer, with live input injected
//! into the middle of it and read back by an independent peer.
//!
//! The assertions are structural — frame counts and arrival order, never a
//! stopwatch — so the suite states a guarantee rather than a measurement
//! that a loaded CI runner can miss. The one thing wall time is used for is
//! *setting up* saturation (waiting for a stalled TCP connection), never for
//! deciding whether the test passed.
//!
//! [ADR 0013]: ../../../docs/adr/0013-interactive-over-bulk-prioritization.md

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use crossover_core::supervision::{DisconnectReason, KeepaliveConfig, run_session};
use crossover_core::{
    LocalNode, MAX_BACKGROUND_QUEUE_FRAMES, OutboundSender, SessionListener, SessionOptions,
};
use crossover_protocol::hello::MessageType;
use crossover_test_peer::{TestConnection, TestNode};

/// One bulk frame. Sized so the Background lane's *message* bound binds
/// well before its byte budget (64 KiB × 64 frames = 4 MiB, half the
/// budget), which makes the queued depth at saturation an exact,
/// assertable number rather than a function of what the kernel buffered.
const BULK_FRAME_BYTES: usize = 64 * 1024;

/// Ceiling on the saturation loop, so a machine that somehow swallows
/// everything fails the test instead of running forever (NFR-1 discipline
/// applies to test harnesses too).
const MAX_BULK_FRAMES: usize = 512;

/// An app side that is nothing but the prioritized send path and the
/// session writer: no drivers, so the only scheduling under test is
/// `OutboundReceiver` → `SessionWriter`.
struct AppSide {
    outbound: OutboundSender,
    /// The session loop, so a test can wait for the reason it ended.
    session: tokio::task::JoinHandle<DisconnectReason>,
    _shutdown: watch::Sender<bool>,
}

/// A generous keepalive: most of this suite deliberately stalls the writer,
/// and a deliberate stall must not be mistaken for a dead peer. The one test
/// that *wants* the stall detected passes its own short config.
fn patient_keepalive() -> KeepaliveConfig {
    KeepaliveConfig::new(Duration::from_secs(30), Duration::from_mins(2)).unwrap()
}

async fn connected_pair() -> (AppSide, TestConnection) {
    connected_pair_with(patient_keepalive()).await
}

async fn connected_pair_with(keepalive: KeepaliveConfig) -> (AppSide, TestConnection) {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("scripted-peer").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (outbound_tx, mut outbound_rx) = crossover_core::outbound_channel();
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
    let session = tokio::spawn(async move {
        let (identity, certified, trust) = (app.identity, app.certified, app.trust);
        let local = LocalNode {
            identity: &identity,
            certified: &certified,
            trust: &trust,
        };
        let session = listener
            .accept(&local, &SessionOptions::default())
            .await
            .expect("accept");
        run_session(
            session,
            &events_tx,
            &mut outbound_rx,
            &mut shutdown_rx,
            &keepalive,
        )
        .await
    });

    let peer_hello = peer.hello();
    let mut conn = TestConnection::connect(addr, &peer).await.unwrap();
    conn.send_hello(&peer_hello).await.unwrap();
    let _app_hello = conn.expect_hello().await.unwrap();
    (
        AppSide {
            outbound: outbound_tx,
            session,
            _shutdown: shutdown_tx,
        },
        conn,
    )
}

/// Push bulk clipboard frames until the send path refuses them — every
/// queue full and the socket stalled behind a peer that is not reading.
/// Returns how many were accepted.
///
/// The peer must not be reading while this runs.
async fn saturate_background(app: &AppSide) -> usize {
    let mut accepted = 0usize;
    loop {
        let payload = vec![0xBB; BULK_FRAME_BYTES];
        if app
            .outbound
            .try_send(MessageType::ClipboardData.wire(), payload)
            .is_err()
        {
            // One confirmation pass: give the writer a moment to prove it
            // is genuinely stalled rather than momentarily between frames.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let payload = vec![0xBB; BULK_FRAME_BYTES];
            if app
                .outbound
                .try_send(MessageType::ClipboardData.wire(), payload)
                .is_err()
            {
                return accepted;
            }
        }
        accepted += 1;
        assert!(
            accepted < MAX_BULK_FRAMES,
            "the background path never saturated after {accepted} frames"
        );
    }
}

/// Read every frame the app has for us, answering keepalives, until the
/// stream goes quiet. Returns the message types in arrival order.
async fn drain_frames(conn: &mut TestConnection, expected: usize) -> Vec<u16> {
    let mut seen = Vec::new();
    while seen.len() < expected {
        let Ok(frame) = timeout(Duration::from_secs(20), conn.recv_frame()).await else {
            break;
        };
        let frame = frame.expect("the app closed the session mid-transfer");
        if frame.message_type == MessageType::Ping.wire() {
            conn.send_frame(MessageType::Pong.wire(), 999, &[])
                .await
                .unwrap();
            continue;
        }
        seen.push(frame.message_type);
    }
    seen
}

/// A peer that simply stops reading must not be able to hold the session
/// open forever.
///
/// The write runs as a `select!` branch *body*, so while it is stalled the
/// loop polls neither the reader nor the keepalive tick — an unbounded write
/// would freeze the idle clock along with it, and a session that never
/// disconnects never runs the release-all-input that a disconnect triggers.
/// That is the stuck-key defect with a hostile peer in place of a crash.
///
/// The assertion is on the *reason*, not the timing: the session must end
/// itself, fail-closed, rather than wait for a peer that is not coming back.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_stops_reading_cannot_suppress_disconnect_detection() {
    // Short enough that the test is quick, long enough to be a real stall.
    let (app, _conn) = connected_pair_with(
        KeepaliveConfig::new(Duration::from_millis(200), Duration::from_secs(1)).unwrap(),
    )
    .await;

    // The peer never reads a byte from here on; push until the writer is
    // stuck mid-frame with a full socket behind it.
    let _bulk = saturate_background(&app).await;

    let reason = timeout(Duration::from_secs(30), app.session)
        .await
        .expect("the session never noticed a peer that stopped reading")
        .expect("the session task panicked");
    assert!(
        matches!(
            reason,
            DisconnectReason::Transport { .. } | DisconnectReason::KeepaliveTimeout
        ),
        "expected a fail-closed end to a stalled session, got {reason:?}"
    );
}

/// The headline guarantee: with the whole background path saturated and the
/// socket stalled, an input batch handed over *after* the backlog still
/// reaches the wire ahead of that backlog.
///
/// The bound is structural. Frames already handed to the kernel are beyond
/// any scheduler's reach and legitimately precede the input, but everything
/// still queued must follow it — so at least a full Background lane's worth
/// of bulk has to arrive *after* the input batch. Nothing is dropped: every
/// accepted bulk frame is accounted for.
///
/// The `bulk_before` count is whatever the OS chose to buffer, so the
/// assertion deliberately says nothing about it. A platform with very large
/// autotuned loopback buffers could swallow more than a full lane before the
/// producer is refused; that would fail here loudly rather than silently
/// weaken the guarantee, which is the right way round.
#[tokio::test(flavor = "multi_thread")]
async fn input_preempts_a_saturating_background_transfer() {
    let (app, mut conn) = connected_pair().await;

    let bulk = saturate_background(&app).await;
    assert!(
        bulk >= MAX_BACKGROUND_QUEUE_FRAMES,
        "expected a full bulk backlog, got {bulk} frames"
    );

    // The High lane is untouched by that saturation — this send must not
    // block on the bulk backpressure at all.
    timeout(
        Duration::from_secs(5),
        app.outbound
            .send(MessageType::InputBatch.wire(), b"pointer moved".to_vec()),
    )
    .await
    .expect("a saturated background lane blocked the high lane")
    .expect("the session ended");

    // Now let the transfer run and watch the order it arrives in.
    let seen = drain_frames(&mut conn, bulk + 1).await;
    let position = seen
        .iter()
        .position(|&ty| ty == MessageType::InputBatch.wire())
        .expect("the input batch never arrived");

    let bulk_before = position;
    let bulk_after = seen.len() - position - 1;
    assert_eq!(
        bulk_before + bulk_after,
        bulk,
        "bulk frames were dropped, not deferred"
    );
    // `- 1`: the frame the writer already had in hand when the lane filled
    // is committed to the socket and legitimately precedes the input; every
    // frame still *queued* must follow it.
    assert!(
        bulk_after >= MAX_BACKGROUND_QUEUE_FRAMES - 1,
        "input preempted only {bulk_after} of {bulk} queued bulk frames \
         (arrived at position {position})"
    );
}

/// A delayed release is a stuck key, which is release-blocking — so
/// `ReleaseAllInput` gets the same proof in its own right.
#[tokio::test(flavor = "multi_thread")]
async fn release_all_input_preempts_a_saturating_background_transfer() {
    let (app, mut conn) = connected_pair().await;

    let bulk = saturate_background(&app).await;
    assert!(bulk >= MAX_BACKGROUND_QUEUE_FRAMES);

    timeout(
        Duration::from_secs(5),
        app.outbound
            .send(MessageType::ReleaseAllInput.wire(), b"release".to_vec()),
    )
    .await
    .expect("a saturated background lane blocked a release")
    .expect("the session ended");

    let seen = drain_frames(&mut conn, bulk + 1).await;
    let position = seen
        .iter()
        .position(|&ty| ty == MessageType::ReleaseAllInput.wire())
        .expect("the release never arrived");
    assert!(
        seen.len() - position > MAX_BACKGROUND_QUEUE_FRAMES - 1,
        "a release queued behind {position} of {bulk} bulk frames is a stuck key"
    );
}

/// The Background lane is bounded in **bytes**, and hitting that bound
/// blocks the producer rather than dropping anything — while the High lane
/// carries on regardless.
///
/// Multi-megabyte frames make the distinction visible: the lane fills long
/// before its sixty-four-message bound, which can only be the byte budget
/// binding. A producer parked on that budget is then shown not to hold up
/// an input batch, and its own frame is shown to arrive once there is room.
#[tokio::test(flavor = "multi_thread")]
async fn the_background_byte_budget_blocks_the_producer_without_delaying_input() {
    let (app, mut conn) = connected_pair().await;

    // 2 MiB frames: MAX_BACKGROUND_QUEUE_BYTES (8 MiB) is reached with a
    // handful of frames, nowhere near MAX_BACKGROUND_QUEUE_FRAMES.
    let mut accepted = 0usize;
    while app
        .outbound
        .try_send(
            MessageType::ClipboardData.wire(),
            vec![0xCC; 2 * 1024 * 1024],
        )
        .is_ok()
    {
        accepted += 1;
        assert!(accepted < MAX_BULK_FRAMES, "the byte budget never bound");
    }
    assert!(
        accepted < MAX_BACKGROUND_QUEUE_FRAMES,
        "the message bound bound first ({accepted} frames); this case must \
         exercise the byte budget"
    );

    // One more, this time waiting for room: the producer parks.
    let producer = {
        let outbound = app.outbound.clone();
        tokio::spawn(async move {
            outbound
                .send(
                    MessageType::ClipboardData.wire(),
                    vec![0xDD; 2 * 1024 * 1024],
                )
                .await
        })
    };

    // The parked producer holds nothing that the High lane needs.
    timeout(
        Duration::from_secs(5),
        app.outbound
            .send(MessageType::InputBatch.wire(), b"still responsive".to_vec()),
    )
    .await
    .expect("a producer blocked on the byte budget delayed the high lane")
    .expect("the session ended");
    assert!(
        !producer.is_finished(),
        "the byte budget did not actually block the producer"
    );

    // Draining makes room; the blocked frame lands, and nothing was lost.
    let seen = drain_frames(&mut conn, accepted + 2).await;
    producer
        .await
        .expect("the producer task panicked")
        .expect("the blocked frame was dropped instead of deferred");
    assert_eq!(
        seen.iter()
            .filter(|&&ty| ty == MessageType::ClipboardData.wire())
            .count(),
        accepted + 1,
        "bulk frames were dropped to stay inside the byte budget"
    );
    assert!(seen.contains(&MessageType::InputBatch.wire()));
}

/// Prioritization reorders *classes*, never a class against itself: a
/// clipboard transaction interleaved with a stream of input must reach the
/// peer in exactly the order it was produced (docs/PROTOCOL.md §4).
#[tokio::test(flavor = "multi_thread")]
async fn a_clipboard_transaction_keeps_its_order_under_input_pressure() {
    let (app, mut conn) = connected_pair().await;

    let transaction = [
        MessageType::ClipboardOffer,
        MessageType::ClipboardAccept,
        MessageType::ClipboardData,
        MessageType::ClipboardApplied,
    ];
    for message in transaction {
        app.outbound
            .send(message.wire(), vec![0x11; 4096])
            .await
            .unwrap();
        // Input at every step, which preempts each time.
        for _ in 0..8 {
            app.outbound
                .send(MessageType::InputBatch.wire(), b"move".to_vec())
                .await
                .unwrap();
        }
    }

    let seen = drain_frames(&mut conn, transaction.len() + 32).await;
    let clipboard: Vec<u16> = seen
        .into_iter()
        .filter(|ty| transaction.iter().any(|message| message.wire() == *ty))
        .collect();
    let expected: Vec<u16> = transaction.iter().map(|m| m.wire()).collect();
    assert_eq!(
        clipboard, expected,
        "the clipboard transaction was reordered against itself"
    );
}
