//! Scriptable headless Crossover peer for integration testing
//! (docs/TESTING.md §1.4).
//!
//! An independent implementation of the wire contract built from
//! `crossover-protocol` and `crossover-security` alone — deliberately not
//! from `crossover-core` — so tests drive the real application against a
//! foreign peer instead of against itself. Unlike the application, this
//! peer has **no safety rails**: it will happily send Hellos with absurd
//! version ranges, malformed payloads, raw garbage bytes, or nothing at
//! all, because that is its job.

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crossover_protocol::hello::{FeatureFlags, Hello, MessageType, OsFamily};
use crossover_protocol::version::{MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION};
use crossover_protocol::{FrameDecoder, RawFrame, encode_frame};
use crossover_security::{
    CertifiedIdentity, DeviceIdentity, TrustStore, TrustedPeer, client_tls_config,
    server_tls_config,
};

/// A test identity: keys, credential, and a trust store to admit chosen
/// peers.
pub struct TestNode {
    /// The node's device identity.
    pub identity: DeviceIdentity,
    /// Its TLS-presentable credential.
    pub certified: CertifiedIdentity,
    /// Who it trusts.
    pub trust: TrustStore,
}

impl TestNode {
    /// Generate a fresh node.
    ///
    /// # Errors
    ///
    /// If identity generation or certification fails.
    pub fn generate(name: &str) -> anyhow::Result<Self> {
        let identity = DeviceIdentity::generate(name).context("generating identity")?;
        let certified =
            CertifiedIdentity::from_identity(&identity).context("certifying identity")?;
        Ok(Self {
            identity,
            certified,
            trust: TrustStore::new(),
        })
    }

    /// Trust another node (one direction).
    ///
    /// # Errors
    ///
    /// If the trust record is invalid or the store is full.
    pub fn trust(&mut self, other: &TestNode) -> anyhow::Result<()> {
        self.trust
            .add_peer(TrustedPeer::new(
                other.identity.device_id(),
                other.identity.device_name(),
                other.certified.fingerprint(),
            )?)
            .context("recording trusted peer")?;
        Ok(())
    }

    /// A well-formed `Hello` for this node at the current version.
    #[must_use]
    pub fn hello(&self) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            min_protocol_version: MIN_SUPPORTED_PROTOCOL_VERSION,
            device_id: self.identity.device_id(),
            device_name: self.identity.device_name().to_owned(),
            operating_system: OsFamily::Other,
            supported_features: FeatureFlags::NONE,
        }
    }
}

/// One scripted TLS connection. Every method does exactly what it says,
/// with no protocol enforcement.
pub struct TestConnection {
    stream: TlsStream<TcpStream>,
    decoder: FrameDecoder,
}

impl TestConnection {
    /// Connect to `addr` as a TLS client using `node`'s credential and
    /// trust.
    ///
    /// # Errors
    ///
    /// On TCP or TLS failure (including being rejected as untrusted).
    pub async fn connect(addr: impl ToSocketAddrs, node: &TestNode) -> anyhow::Result<Self> {
        let tcp = TcpStream::connect(addr).await.context("TCP connect")?;
        tcp.set_nodelay(true)?;
        let config = client_tls_config(&node.certified, &node.trust)?;
        let connector = TlsConnector::from(std::sync::Arc::new(config));
        let name = ServerName::try_from("crossover.invalid").context("server name")?;
        let tls = connector.connect(name, tcp).await.context("TLS connect")?;
        Ok(Self {
            stream: TlsStream::Client(tls),
            decoder: FrameDecoder::new(),
        })
    }

    /// Accept one TLS connection on `listener` as `node`.
    ///
    /// # Errors
    ///
    /// On TCP or TLS failure (including the client being untrusted).
    pub async fn accept(listener: &TcpListener, node: &TestNode) -> anyhow::Result<Self> {
        let (tcp, _remote) = listener.accept().await.context("TCP accept")?;
        tcp.set_nodelay(true)?;
        let config = server_tls_config(&node.certified, &node.trust)?;
        let acceptor = TlsAcceptor::from(std::sync::Arc::new(config));
        let tls = acceptor.accept(tcp).await.context("TLS accept")?;
        Ok(Self {
            stream: TlsStream::Server(tls),
            decoder: FrameDecoder::new(),
        })
    }

    /// Send a well-formed frame.
    ///
    /// # Errors
    ///
    /// On encoding or transport failure.
    pub async fn send_frame(
        &mut self,
        message_type: u16,
        message_id: u64,
        payload: &[u8],
    ) -> anyhow::Result<()> {
        let frame = encode_frame(message_type, message_id, payload)?;
        self.send_raw(&frame).await
    }

    /// Send arbitrary bytes — including deliberately broken framing.
    ///
    /// # Errors
    ///
    /// On transport failure.
    pub async fn send_raw(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.stream.write_all(bytes).await.context("write")?;
        self.stream.flush().await.context("flush")?;
        Ok(())
    }

    /// Send `hello` as the customary first frame (message id 1).
    ///
    /// # Errors
    ///
    /// On encoding or transport failure. Note: `hello` is validated by
    /// encoding; to send a *structurally* broken Hello use
    /// [`TestConnection::send_frame`] with hand-built payload bytes.
    pub async fn send_hello(&mut self, hello: &Hello) -> anyhow::Result<()> {
        let payload = hello.encode_payload()?;
        self.send_frame(MessageType::Hello.wire(), 1, &payload)
            .await
    }

    /// Receive the next complete frame.
    ///
    /// # Errors
    ///
    /// On EOF, framing violation, or transport failure.
    pub async fn recv_frame(&mut self) -> anyhow::Result<RawFrame> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(frame);
            }
            let n = self.stream.read(&mut buf).await.context("read")?;
            anyhow::ensure!(n > 0, "peer closed the connection");
            self.decoder.extend(&buf[..n])?;
        }
    }

    /// Receive a frame and decode it as the peer's `Hello`.
    ///
    /// # Errors
    ///
    /// If the frame is not a valid `Hello`.
    pub async fn expect_hello(&mut self) -> anyhow::Result<Hello> {
        let frame = self.recv_frame().await?;
        anyhow::ensure!(
            frame.message_type == MessageType::Hello.wire(),
            "expected Hello, got message type {}",
            frame.message_type
        );
        Ok(Hello::decode_payload(&frame.payload)?)
    }

    /// Gracefully close the connection.
    ///
    /// # Errors
    ///
    /// On transport failure during shutdown.
    pub async fn close(mut self) -> anyhow::Result<()> {
        self.stream.shutdown().await.context("shutdown")?;
        Ok(())
    }

    /// Drop the connection abruptly (no TLS close-notify, no TCP
    /// shutdown handshake beyond RST/FIN from the OS).
    pub fn abort(self) {
        drop(self);
    }
}
