//! Phase 1 integration suite (docs/TESTING.md §1.4, §1.5): the real
//! application core (`crossover-core`) driven against the independent
//! test peer, over real TCP and TLS on localhost.
//!
//! Where the core's own unit tests prove core-talks-to-core, these prove
//! core-talks-to-a-foreign-implementation — and survives a hostile one.

use std::time::Duration;

use tokio::net::TcpListener;

use crossover_core::supervision::{
    DisconnectReason, KeepaliveConfig, ReconnectPolicy, SessionEvent, SupervisorConfig,
    supervise_outbound,
};
use crossover_core::{LocalNode, SessionError, SessionListener, SessionOptions};
use crossover_protocol::ProtocolError;
use crossover_protocol::hello::MessageType;
use crossover_test_peer::{TestConnection, TestNode};

fn options() -> SessionOptions {
    SessionOptions {
        establish_timeout: Duration::from_secs(5),
        metrics: None,
    }
}

fn local(node: &TestNode) -> LocalNode<'_> {
    LocalNode {
        identity: &node.identity,
        certified: &node.certified,
        trust: &node.trust,
    }
}

/// The honest path: a well-behaved foreign peer establishes with the
/// core listener and exchanges application frames.
#[tokio::test]
async fn core_establishes_with_a_foreign_peer() {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("foreign-peer").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (app_local, opts) = (local(&app), options());
    let (inbound, scripted) = tokio::join!(listener.accept(&app_local, &opts), async {
        let mut conn = TestConnection::connect(addr, &peer).await?;
        conn.send_hello(&peer.hello()).await?;
        let app_hello = conn.expect_hello().await?;
        anyhow::Ok((conn, app_hello))
    });

    let mut session = inbound.unwrap();
    let (mut conn, app_hello) = scripted.unwrap();

    assert_eq!(app_hello.device_name, "app");
    assert_eq!(session.info().peer_device_name, "foreign-peer");
    assert_eq!(
        session.info().peer_fingerprint,
        peer.certified.fingerprint()
    );

    // Application frames flow both directions.
    conn.send_frame(0x0200, 2, b"from-peer").await.unwrap();
    let frame = session.recv().await.unwrap();
    assert_eq!(
        (frame.message_type, frame.payload.as_slice()),
        (0x0200, &b"from-peer"[..])
    );

    session.send(0x0201, b"from-app").await.unwrap();
    let frame = conn.recv_frame().await.unwrap();
    assert_eq!(frame.payload, b"from-app");

    conn.close().await.unwrap();
}

/// A peer offering only an incompatible version range is refused with
/// the no-silent-downgrade diagnostic.
#[tokio::test]
async fn incompatible_version_range_is_refused() {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("future-peer").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (app_local, opts) = (local(&app), options());
    let (inbound, ()) = tokio::join!(listener.accept(&app_local, &opts), async {
        let mut conn = TestConnection::connect(addr, &peer).await.unwrap();
        let mut hello = peer.hello();
        hello.min_protocol_version = 99;
        hello.protocol_version = 100;
        conn.send_hello(&hello).await.unwrap();
        // Keep the socket open so the listener's failure is negotiation,
        // not a disconnect race.
        tokio::time::sleep(Duration::from_millis(500)).await;
        conn.abort();
    });

    assert!(matches!(
        inbound,
        Err(SessionError::Protocol(ProtocolError::NoCommonVersion {
            peer_min: 99,
            ..
        }))
    ));
}

/// A structurally broken Hello payload is a malformed-message failure,
/// not a panic or a hang.
#[tokio::test]
async fn malformed_hello_payload_fails_closed() {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("mangler").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (app_local, opts) = (local(&app), options());
    let (inbound, ()) = tokio::join!(listener.accept(&app_local, &opts), async {
        let mut conn = TestConnection::connect(addr, &peer).await.unwrap();
        conn.send_frame(MessageType::Hello.wire(), 1, &[0xFF; 60])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        conn.abort();
    });

    assert!(matches!(
        inbound,
        Err(SessionError::Protocol(ProtocolError::Malformed { .. }))
    ));
}

/// A frame prefix declaring an oversized body is rejected from the
/// length alone (NFR-1: no allocation, no waiting for the payload).
#[tokio::test]
async fn oversized_frame_declaration_is_rejected() {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("flooder").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (app_local, opts) = (local(&app), options());
    let (inbound, ()) = tokio::join!(listener.accept(&app_local, &opts), async {
        let mut conn = TestConnection::connect(addr, &peer).await.unwrap();
        // 512 MiB declared; only the 4-byte prefix ever sent.
        conn.send_raw(&(512u32 * 1024 * 1024).to_be_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        conn.abort();
    });

    assert!(matches!(
        inbound,
        Err(SessionError::Protocol(ProtocolError::FrameTooLarge { .. }))
    ));
}

/// Untrusted credentials die in the TLS handshake, before any protocol
/// bytes (threat T1 via the tool's bad-credentials capability).
#[tokio::test]
async fn untrusted_test_peer_is_rejected_at_tls() {
    let app = TestNode::generate("app").unwrap();
    let mut stranger = TestNode::generate("stranger").unwrap();
    stranger.trust(&app).unwrap(); // one-sided: app never trusted it

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (app_local, opts) = (local(&app), options());
    let (inbound, scripted) = tokio::join!(
        listener.accept(&app_local, &opts),
        TestConnection::connect(addr, &stranger),
    );
    assert!(inbound.is_err());
    // The stranger either fails during its own handshake or on first use
    // of the connection; it must never get a working session.
    if let Ok(mut conn) = scripted {
        assert!(conn.recv_frame().await.is_err());
    }
}

/// An abrupt mid-establishment disconnect surfaces as a clean error.
#[tokio::test]
async fn abrupt_disconnect_mid_establishment_fails_cleanly() {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("quitter").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (app_local, opts) = (local(&app), options());
    let (inbound, ()) = tokio::join!(listener.accept(&app_local, &opts), async {
        let mut conn = TestConnection::connect(addr, &peer).await.unwrap();
        // Half a frame: a 100-byte body declared, 4 bytes delivered.
        conn.send_raw(&100u32.to_be_bytes()).await.unwrap();
        conn.send_raw(&[0x01, 0x02, 0x03, 0x04]).await.unwrap();
        conn.abort();
    });

    assert!(matches!(
        inbound,
        Err(SessionError::PeerClosed | SessionError::Io { .. } | SessionError::Tls { .. })
    ));
}

/// The supervisor composes with the test peer: session loss at the
/// foreign end triggers automatic reestablishment.
#[tokio::test]
async fn supervisor_reestablishes_through_the_test_peer() {
    let mut app = TestNode::generate("app").unwrap();
    let mut peer = TestNode::generate("flaky-peer").unwrap();
    app.trust(&peer).unwrap();
    peer.trust(&app).unwrap();

    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap().to_string();

    let config = SupervisorConfig {
        reconnect: ReconnectPolicy {
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(200),
            // Any session here counts as stable (resets the backoff).
            reset_after: Duration::from_millis(1),
        },
        keepalive: KeepaliveConfig {
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(30),
        },
        session: options(),
    };
    let (handle, mut events) = supervise_outbound(
        addr,
        app.identity,
        app.certified,
        std::sync::Arc::new(std::sync::RwLock::new(app.trust)),
        config,
    );

    // First scripted acceptance: establish, then vanish abruptly.
    let mut conn = TestConnection::accept(&tcp, &peer).await.unwrap();
    conn.send_hello(&peer.hello()).await.unwrap();
    let _app_hello = conn.expect_hello().await.unwrap();
    let established = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(established, SessionEvent::Established(_)));
    conn.abort();

    let disconnected = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        disconnected,
        SessionEvent::Disconnected {
            reason: DisconnectReason::PeerClosed | DisconnectReason::Transport { .. },
            retry_in: Some(_),
            ..
        }
    ));

    // Second scripted acceptance: the supervisor comes back by itself.
    let mut conn = TestConnection::accept(&tcp, &peer).await.unwrap();
    conn.send_hello(&peer.hello()).await.unwrap();
    let _app_hello = conn.expect_hello().await.unwrap();
    let reestablished = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(reestablished, SessionEvent::Established(_)));

    handle.shutdown();
}
