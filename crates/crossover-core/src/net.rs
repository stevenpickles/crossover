//! Secure session establishment (docs/ARCHITECTURE.md §5.3, Phase 1).
//!
//! One fully-established session per call: TCP, then mutually
//! authenticated TLS 1.3 (SPKI-pinned configs from `crossover-security`),
//! then the `Hello` exchange and version negotiation from
//! `crossover-protocol`. Every stage is bounded by a timeout, every
//! failure is typed and fail-closed, and application traffic is
//! impossible before `ESTABLISHED` (threat T8: the send/recv surface is
//! only reachable through an [`EstablishedSession`], which cannot exist
//! earlier).
//!
//! Reconnection and keepalive supervision arrive in the next slice; this
//! module owns exactly the lifecycle prefix
//! `CONNECTING → AUTHENTICATING → NEGOTIATING → ESTABLISHED`.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use uuid::Uuid;

use crossover_protocol::framing::MAX_FRAME_BODY_BYTES;
use crossover_protocol::hello::{FeatureFlags, Hello, MessageType, OsFamily};
use crossover_protocol::version::{MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION, VersionRange};
use crossover_protocol::{FrameDecoder, ProtocolError, RawFrame, encode_frame, negotiate};
use crossover_security::{
    CertifiedIdentity, DeviceIdentity, SpkiFingerprint, TlsError, TrustStore,
    certificate_spki_fingerprint, client_tls_config, server_tls_config,
};

/// Everything establishment needs about the local side. TLS configs are
/// built per call from `trust` — the documented snapshot semantics:
/// changes to trust apply to every subsequent establishment.
pub struct LocalNode<'a> {
    /// The local device identity (names the `Hello`).
    pub identity: &'a DeviceIdentity,
    /// The TLS-presentable credential for that identity.
    pub certified: &'a CertifiedIdentity,
    /// The authorization authority.
    pub trust: &'a TrustStore,
}

/// Establishment tuning.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Ceiling on the whole establishment (TCP included on the connect
    /// side, TLS + Hello on both): a peer that connects and stalls must
    /// not hold resources forever (NFR-1).
    pub establish_timeout: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            establish_timeout: Duration::from_secs(10),
        }
    }
}

/// Failures on the way to (or inside) a session. Families stay
/// distinguishable per docs/ARCHITECTURE.md §9: transport vs TLS vs
/// protocol vs timeout are different diagnoses.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// TCP-level failure (bind, connect, read, write).
    #[error("transport I/O failed: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    /// TLS handshake or configuration failure — includes rejection of
    /// untrusted peers by the pinned verifiers.
    #[error("TLS failure: {reason}")]
    Tls { reason: String },

    /// Local TLS credential/configuration construction failed.
    #[error(transparent)]
    Security(#[from] TlsError),

    /// Protocol violation during or after establishment.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// The peer closed the connection.
    #[error("peer closed the connection")]
    PeerClosed,

    /// Establishment exceeded [`SessionOptions::establish_timeout`].
    #[error("session establishment timed out")]
    Timeout,

    /// Mutual TLS completed but no peer certificate was surfaced —
    /// impossible by configuration, rejected anyway (fail closed).
    #[error("peer presented no certificate after mutual TLS")]
    MissingPeerCertificate,
}

/// Facts about an established session, fixed at establishment.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Locally generated id carried by every log line about this session
    /// (FR-7.3; docs/ARCHITECTURE.md §10 field conventions).
    pub session_id: Uuid,
    /// The identity the TLS layer actually authenticated (pinned SPKI).
    pub peer_fingerprint: SpkiFingerprint,
    /// Peer's self-reported device id (bookkeeping, never authorization).
    pub peer_device_id: Uuid,
    /// Peer's self-reported device name (diagnostics only).
    pub peer_device_name: String,
    /// Peer's self-reported OS family.
    pub peer_os: OsFamily,
    /// The negotiated protocol version this session speaks.
    pub protocol_version: u16,
}

/// A mutually authenticated, version-negotiated session. The only type in
/// the system that can send or receive application frames.
pub struct EstablishedSession {
    stream: TlsStream<TcpStream>,
    decoder: FrameDecoder,
    next_message_id: u64,
    info: SessionInfo,
}

impl EstablishedSession {
    /// Facts about this session.
    #[must_use]
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Send one frame; returns the assigned message id.
    ///
    /// # Errors
    ///
    /// [`SessionError::Protocol`] if the payload exceeds frame bounds;
    /// [`SessionError::Io`] on transport failure.
    pub async fn send(&mut self, message_type: u16, payload: &[u8]) -> Result<u64, SessionError> {
        let message_id = self.next_message_id;
        let frame = encode_frame(message_type, message_id, payload)?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        self.next_message_id += 1;
        Ok(message_id)
    }

    /// Receive the next complete frame.
    ///
    /// # Errors
    ///
    /// [`SessionError::PeerClosed`] on EOF; [`SessionError::Protocol`] on
    /// framing violations (fail closed — callers terminate the session);
    /// [`SessionError::Io`] on transport failure.
    pub async fn recv(&mut self) -> Result<RawFrame, SessionError> {
        read_frame(&mut self.stream, &mut self.decoder).await
    }

    /// Gracefully close the session.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if shutdown fails.
    pub async fn close(mut self) -> Result<(), SessionError> {
        self.stream.shutdown().await?;
        Ok(())
    }
}

/// Listener for inbound sessions.
pub struct SessionListener {
    tcp: TcpListener,
}

impl SessionListener {
    /// Bind the TCP listener.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if binding fails.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self, SessionError> {
        Ok(Self {
            tcp: TcpListener::bind(addr).await?,
        })
    }

    /// The bound address (useful with port 0 in tests).
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the socket refuses to report it.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, SessionError> {
        Ok(self.tcp.local_addr()?)
    }

    /// Accept one inbound connection and establish it fully (TLS with
    /// mandatory client auth, Hello, negotiation) before returning.
    ///
    /// # Errors
    ///
    /// Any [`SessionError`]: untrusted or misbehaving peers fail here and
    /// never reach the caller as a session.
    pub async fn accept(
        &self,
        local: &LocalNode<'_>,
        options: &SessionOptions,
    ) -> Result<EstablishedSession, SessionError> {
        let (tcp, remote) = self.tcp.accept().await?;
        let session_id = Uuid::new_v4();
        tracing::debug!(session_id = %session_id, remote = %remote, "inbound connection");

        let result = tokio::time::timeout(options.establish_timeout, async {
            tcp.set_nodelay(true)?;
            let acceptor = TlsAcceptor::from(std::sync::Arc::new(server_tls_config(
                local.certified,
                local.trust,
            )?));
            let tls = acceptor.accept(tcp).await.map_err(|e| SessionError::Tls {
                reason: e.to_string(),
            })?;
            let peer_fingerprint =
                peer_fingerprint_from_certs(tls.get_ref().1.peer_certificates())?;
            establish(TlsStream::Server(tls), peer_fingerprint, local, session_id).await
        })
        .await
        .map_err(|_| SessionError::Timeout)?;

        log_outcome(session_id, "inbound", &result);
        result
    }
}

/// Connect to a peer and establish the session fully before returning.
///
/// # Errors
///
/// Any [`SessionError`]; an untrusted server fails TLS here.
pub async fn connect(
    addr: impl ToSocketAddrs,
    local: &LocalNode<'_>,
    options: &SessionOptions,
) -> Result<EstablishedSession, SessionError> {
    let session_id = Uuid::new_v4();
    let result = tokio::time::timeout(options.establish_timeout, async {
        let tcp = TcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        let connector = TlsConnector::from(std::sync::Arc::new(client_tls_config(
            local.certified,
            local.trust,
        )?));
        // The name is a placeholder: trust is the pinned SPKI, never a
        // name (ADR 0003).
        let name = ServerName::try_from("crossover.invalid").map_err(|e| SessionError::Tls {
            reason: format!("invalid placeholder server name: {e}"),
        })?;
        let tls = connector
            .connect(name, tcp)
            .await
            .map_err(|e| SessionError::Tls {
                reason: e.to_string(),
            })?;
        let peer_fingerprint = peer_fingerprint_from_certs(tls.get_ref().1.peer_certificates())?;
        establish(TlsStream::Client(tls), peer_fingerprint, local, session_id).await
    })
    .await
    .map_err(|_| SessionError::Timeout)?;

    log_outcome(session_id, "outbound", &result);
    result
}

/// Extract the fingerprint of the certificate the peer actually presented.
fn peer_fingerprint_from_certs(
    certs: Option<&[tokio_rustls::rustls::pki_types::CertificateDer<'_>]>,
) -> Result<SpkiFingerprint, SessionError> {
    let end_entity = certs
        .and_then(<[_]>::first)
        .ok_or(SessionError::MissingPeerCertificate)?;
    Ok(certificate_spki_fingerprint(end_entity)?)
}

/// Shared post-TLS establishment: exchange `Hello`s, negotiate, build the
/// session (`NEGOTIATING → ESTABLISHED`).
async fn establish(
    mut stream: TlsStream<TcpStream>,
    peer_fingerprint: SpkiFingerprint,
    local: &LocalNode<'_>,
    session_id: Uuid,
) -> Result<EstablishedSession, SessionError> {
    // Send our Hello (message id 1).
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        min_protocol_version: MIN_SUPPORTED_PROTOCOL_VERSION,
        device_id: local.identity.device_id(),
        device_name: local.identity.device_name().to_owned(),
        operating_system: local_os_family(),
        supported_features: FeatureFlags::NONE,
    };
    let payload = hello.encode_payload()?;
    let frame = encode_frame(MessageType::Hello.wire(), 1, &payload)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;

    // The peer's first frame must be a Hello — anything else is a
    // protocol violation, fatal before the session exists.
    let mut decoder = FrameDecoder::new();
    let first = read_frame(&mut stream, &mut decoder).await?;
    if MessageType::from_wire(first.message_type) != Some(MessageType::Hello) {
        return Err(ProtocolError::Malformed {
            reason: format!(
                "first frame must be Hello, got message type {}",
                first.message_type
            ),
        }
        .into());
    }
    let peer_hello = Hello::decode_payload(&first.payload)?;

    let protocol_version = negotiate(
        VersionRange::CURRENT,
        VersionRange {
            min: peer_hello.min_protocol_version,
            max: peer_hello.protocol_version,
        },
    )?;

    Ok(EstablishedSession {
        stream,
        decoder,
        // Our Hello used id 1; application frames continue from 2.
        next_message_id: 2,
        info: SessionInfo {
            session_id,
            peer_fingerprint,
            peer_device_id: peer_hello.device_id,
            peer_device_name: peer_hello.device_name,
            peer_os: peer_hello.operating_system,
            protocol_version,
        },
    })
}

/// Read one frame from the stream, growing the decoder as bytes arrive.
async fn read_frame<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    decoder: &mut FrameDecoder,
) -> Result<RawFrame, SessionError> {
    // Read chunks are far below the decoder's internal cap, so a drained
    // decoder can always absorb one read (MAX_FRAME_BODY_BYTES >> chunk).
    let mut buf = vec![0u8; 16 * 1024.min(MAX_FRAME_BODY_BYTES)];
    loop {
        if let Some(frame) = decoder.next_frame()? {
            return Ok(frame);
        }
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Err(SessionError::PeerClosed);
        }
        decoder.extend(&buf[..n])?;
    }
}

fn local_os_family() -> OsFamily {
    if cfg!(windows) {
        OsFamily::Windows
    } else if cfg!(target_os = "macos") {
        OsFamily::MacOs
    } else if cfg!(target_os = "linux") {
        OsFamily::Linux
    } else {
        OsFamily::Other
    }
}

fn log_outcome(session_id: Uuid, role: &str, result: &Result<EstablishedSession, SessionError>) {
    match result {
        Ok(session) => {
            let info = session.info();
            tracing::info!(
                session_id = %session_id,
                role,
                peer_id = %info.peer_fingerprint,
                peer_device_id = %info.peer_device_id,
                peer_name = %info.peer_device_name,
                protocol_version = info.protocol_version,
                state = "established",
                "session established"
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                role,
                error = %error,
                state = "failed",
                "session establishment failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use crossover_protocol::hello::MessageType;
    use crossover_protocol::{ProtocolError, encode_frame};
    use crossover_security::{CertifiedIdentity, DeviceIdentity, TrustStore, TrustedPeer};

    use super::{LocalNode, SessionError, SessionListener, SessionOptions, connect};

    struct Node {
        identity: DeviceIdentity,
        certified: CertifiedIdentity,
        trust: TrustStore,
    }

    impl Node {
        fn new(name: &str) -> Self {
            let identity = DeviceIdentity::generate(name).unwrap();
            let certified = CertifiedIdentity::from_identity(&identity).unwrap();
            Self {
                identity,
                certified,
                trust: TrustStore::new(),
            }
        }

        fn trust_peer(&mut self, other: &Node) {
            self.trust
                .add_peer(
                    TrustedPeer::new(
                        other.identity.device_id(),
                        other.identity.device_name(),
                        other.certified.fingerprint(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }

        fn local(&self) -> LocalNode<'_> {
            LocalNode {
                identity: &self.identity,
                certified: &self.certified,
                trust: &self.trust,
            }
        }
    }

    fn options() -> SessionOptions {
        SessionOptions {
            establish_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn trusted_peers_establish_and_exchange_frames() {
        let mut a = Node::new("machine-a");
        let mut b = Node::new("machine-b");
        a.trust_peer(&b);
        b.trust_peer(&a);

        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (a_local, b_local, opts) = (a.local(), b.local(), options());
        let (inbound, outbound) = tokio::join!(
            listener.accept(&b_local, &opts),
            connect(addr, &a_local, &opts),
        );
        let mut server_session = inbound.unwrap();
        let mut client_session = outbound.unwrap();

        // Both sides agree on version 1 and name the peer they
        // authenticated by fingerprint and Hello metadata.
        assert_eq!(client_session.info().protocol_version, 1);
        assert_eq!(server_session.info().protocol_version, 1);
        assert_eq!(
            client_session.info().peer_fingerprint,
            b.certified.fingerprint()
        );
        assert_eq!(
            server_session.info().peer_fingerprint,
            a.certified.fingerprint()
        );
        assert_eq!(server_session.info().peer_device_name, "machine-a");
        assert_eq!(client_session.info().peer_device_id, b.identity.device_id());

        // Application frames flow both ways with monotonic ids from 2.
        let id = client_session.send(0x0042, b"ping").await.unwrap();
        assert_eq!(id, 2);
        let frame = server_session.recv().await.unwrap();
        assert_eq!(frame.message_type, 0x0042);
        assert_eq!(frame.message_id, 2);
        assert_eq!(frame.payload, b"ping");

        server_session.send(0x0043, b"pong").await.unwrap();
        let frame = client_session.recv().await.unwrap();
        assert_eq!(frame.payload, b"pong");

        client_session.close().await.unwrap();
        assert!(matches!(
            server_session.recv().await,
            Err(SessionError::PeerClosed)
        ));
    }

    #[tokio::test]
    async fn untrusted_connector_is_rejected_by_the_listener() {
        let mut intruder = Node::new("intruder");
        let server = Node::new("server");
        // The intruder even trusts the server; the server has never
        // paired with it (threat T1).
        intruder.trust_peer(&server);

        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (intruder_local, server_local, opts) = (intruder.local(), server.local(), options());
        let (inbound, outbound) = tokio::join!(
            listener.accept(&server_local, &opts),
            connect(addr, &intruder_local, &opts),
        );
        assert!(inbound.is_err());
        assert!(outbound.is_err());
    }

    #[tokio::test]
    async fn untrusted_listener_is_rejected_by_the_connector() {
        let client = Node::new("client");
        let mut impostor = Node::new("impostor");
        impostor.trust_peer(&client);

        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (client_local, impostor_local, opts) = (client.local(), impostor.local(), options());
        let (inbound, outbound) = tokio::join!(
            listener.accept(&impostor_local, &opts),
            connect(addr, &client_local, &opts),
        );
        assert!(outbound.is_err());
        assert!(inbound.is_err());
    }

    #[tokio::test]
    async fn plaintext_garbage_fails_the_listener_without_panic() {
        let server = Node::new("server");
        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (server_local, opts) = (server.local(), options());
        let (inbound, ()) = tokio::join!(listener.accept(&server_local, &opts), async {
            let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            tcp.write_all(&[0xFF; 512]).await.unwrap();
        });
        assert!(matches!(
            inbound,
            Err(SessionError::Tls { .. } | SessionError::Io { .. })
        ));
    }

    #[tokio::test]
    async fn stalled_peer_hits_the_establishment_timeout() {
        let server = Node::new("server");
        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let short = SessionOptions {
            establish_timeout: Duration::from_millis(200),
        };
        let server_local = server.local();
        let (inbound, ()) = tokio::join!(listener.accept(&server_local, &short), async {
            // Connect and send nothing, keeping the socket open past the
            // server's timeout.
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            drop(tcp);
        });
        assert!(matches!(inbound, Err(SessionError::Timeout)));
    }

    /// A trusted peer that speaks TLS correctly but violates the session
    /// protocol: its first frame is not Hello.
    #[tokio::test]
    async fn non_hello_first_frame_is_fatal() {
        let mut a = Node::new("rule-breaker");
        let mut b = Node::new("server");
        a.trust_peer(&b);
        b.trust_peer(&a);

        let listener = SessionListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (b_local, opts) = (b.local(), options());
        let (inbound, ()) = tokio::join!(listener.accept(&b_local, &opts), async {
            // Hand-rolled client: real TLS, wrong first frame.
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let config = crossover_security::client_tls_config(&a.certified, &a.trust).unwrap();
            let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
            let name =
                tokio_rustls::rustls::pki_types::ServerName::try_from("crossover.invalid").unwrap();
            let mut tls = connector.connect(name, tcp).await.unwrap();
            let frame = encode_frame(0x7777, 1, b"not a hello").unwrap();
            tls.write_all(&frame).await.unwrap();
            tls.flush().await.unwrap();
            // Hold the connection open so the server's failure is the
            // protocol violation, not a disconnect race.
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(tls);
        });
        assert!(matches!(
            inbound,
            Err(SessionError::Protocol(ProtocolError::Malformed { .. }))
        ));
        // Sanity: the type constant used above is genuinely unknown.
        assert_eq!(MessageType::from_wire(0x7777), None);
    }
}
