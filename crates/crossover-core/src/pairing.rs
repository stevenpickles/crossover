//! Async driver for the pairing ceremony (ADR 0002) over plain TCP.
//!
//! Transport only: all ceremony security lives in
//! `crossover_security::pairing`; this module moves its messages across
//! the network using the standard frame codec, bounded by one timeout for
//! the whole ceremony. One listener accept handles exactly one pairing
//! attempt — codes are single-use, so the process, the code, and the
//! connection share a lifetime.

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

use crossover_protocol::hello::MessageType;
use crossover_protocol::pairing::{PairingConfirm, PairingStart};
use crossover_protocol::{FrameDecoder, ProtocolError, encode_frame};
use crossover_security::pairing::{
    ConfirmParts, PairedPeer, PairingCeremony, PairingCode, PairingError, PairingIdentity,
    PairingRole,
};

use crate::net::{SessionError, read_frame};

/// Failures while driving a pairing ceremony over the network.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PairingDriveError {
    /// TCP-level failure.
    #[error("pairing transport failed: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    /// Framing or message-level violation from the peer.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// The ceremony itself failed — including the all-important
    /// wrong-code-or-MITM confirmation mismatch.
    #[error(transparent)]
    Ceremony(#[from] PairingError),

    /// The peer sent the wrong message type for this point in the
    /// ceremony.
    #[error("unexpected message type {message_type} during pairing")]
    UnexpectedMessage {
        /// The offending wire type.
        message_type: u16,
    },

    /// The peer closed the connection mid-ceremony.
    #[error("peer closed the connection during pairing")]
    PeerClosed,

    /// The ceremony did not complete within the allowed time.
    #[error("pairing timed out")]
    Timeout,
}

/// Listener side of pairing: bind, display the code, accept one attempt.
pub struct PairingListener {
    tcp: TcpListener,
}

impl PairingListener {
    /// Bind the pairing listener.
    ///
    /// # Errors
    ///
    /// [`PairingDriveError::Io`] if binding fails.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self, PairingDriveError> {
        Ok(Self {
            tcp: TcpListener::bind(addr).await?,
        })
    }

    /// The bound address (displayed to the user alongside the code).
    ///
    /// # Errors
    ///
    /// [`PairingDriveError::Io`] if the socket refuses to report it.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, PairingDriveError> {
        Ok(self.tcp.local_addr()?)
    }

    /// Accept **one** connection and run the ceremony to completion.
    ///
    /// # Errors
    ///
    /// Any [`PairingDriveError`]; on failure nothing has been persisted
    /// and the code must not be reused.
    pub async fn accept_and_pair(
        &self,
        local: PairingIdentity,
        code: &PairingCode,
        timeout: Duration,
    ) -> Result<PairedPeer, PairingDriveError> {
        let result = tokio::time::timeout(timeout, async {
            let (stream, remote) = self.tcp.accept().await?;
            tracing::info!(remote = %remote, "pairing connection accepted");
            drive(stream, PairingRole::Listener, local, code).await
        })
        .await
        .map_err(|_| PairingDriveError::Timeout)?;
        log_outcome(&result);
        result
    }
}

/// Connector side of pairing: dial the listener and run the ceremony with
/// the code the user typed.
///
/// # Errors
///
/// Any [`PairingDriveError`]; on failure nothing has been persisted.
pub async fn pair_with(
    addr: impl ToSocketAddrs,
    local: PairingIdentity,
    code: &PairingCode,
    timeout: Duration,
) -> Result<PairedPeer, PairingDriveError> {
    let result = tokio::time::timeout(timeout, async {
        let stream = TcpStream::connect(addr).await?;
        drive(stream, PairingRole::Connector, local, code).await
    })
    .await
    .map_err(|_| PairingDriveError::Timeout)?;
    log_outcome(&result);
    result
}

/// Run the two-round ceremony over one TCP stream.
async fn drive(
    mut stream: TcpStream,
    role: PairingRole,
    local: PairingIdentity,
    code: &PairingCode,
) -> Result<PairedPeer, PairingDriveError> {
    stream.set_nodelay(true)?;
    let (mut ceremony, own_start) = PairingCeremony::new(role, code, local)?;
    let mut decoder = FrameDecoder::new();

    // Round 1: exchange SPAKE2 elements.
    let start_payload = PairingStart {
        spake_message: own_start,
    }
    .encode_payload()?;
    let frame = encode_frame(MessageType::PairingStart.wire(), 1, &start_payload)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;

    let inbound = recv_expected(&mut stream, &mut decoder, MessageType::PairingStart).await?;
    let peer_start = PairingStart::decode_payload(&inbound)?;
    let own_confirm = ceremony.receive_peer_start(&peer_start.spake_message)?;

    // Round 2: exchange MAC-authenticated identity claims.
    let confirm_payload = PairingConfirm {
        device_id: own_confirm.device_id,
        device_name: own_confirm.device_name.clone(),
        spki_fingerprint: *own_confirm.fingerprint.as_bytes(),
        mac: own_confirm.mac,
    }
    .encode_payload()?;
    let frame = encode_frame(MessageType::PairingConfirm.wire(), 2, &confirm_payload)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;

    let inbound = recv_expected(&mut stream, &mut decoder, MessageType::PairingConfirm).await?;
    let peer_confirm = PairingConfirm::decode_payload(&inbound)?;
    let paired = ceremony.receive_peer_confirm(&ConfirmParts {
        device_id: peer_confirm.device_id,
        device_name: peer_confirm.device_name,
        fingerprint: peer_confirm.spki_fingerprint.into(),
        mac: peer_confirm.mac,
    })?;
    Ok(paired)
}

/// Receive one frame and require the expected ceremony message type.
async fn recv_expected(
    stream: &mut TcpStream,
    decoder: &mut FrameDecoder,
    expected: MessageType,
) -> Result<Vec<u8>, PairingDriveError> {
    let frame = read_frame(stream, decoder).await.map_err(|e| match e {
        SessionError::PeerClosed => PairingDriveError::PeerClosed,
        SessionError::Protocol(p) => PairingDriveError::Protocol(p),
        SessionError::Io { source } => PairingDriveError::Io { source },
        other => PairingDriveError::Protocol(ProtocolError::Malformed {
            reason: other.to_string(),
        }),
    })?;
    if frame.message_type != expected.wire() {
        return Err(PairingDriveError::UnexpectedMessage {
            message_type: frame.message_type,
        });
    }
    Ok(frame.payload)
}

fn log_outcome(result: &Result<PairedPeer, PairingDriveError>) {
    match result {
        Ok(peer) => tracing::info!(
            peer_device_id = %peer.device_id,
            peer_name = %peer.device_name,
            peer_id = %peer.fingerprint,
            "pairing succeeded"
        ),
        Err(error) => tracing::warn!(error = %error, "pairing failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use crossover_security::pairing::{PairingCode, PairingError, PairingIdentity};
    use crossover_security::{DeviceIdentity, PairingRole};

    use super::{PairingDriveError, PairingListener, pair_with};

    fn identity_of(device: &DeviceIdentity) -> PairingIdentity {
        PairingIdentity {
            device_id: device.device_id(),
            device_name: device.device_name().to_owned(),
            fingerprint: device.spki_fingerprint().unwrap(),
        }
    }

    #[tokio::test]
    async fn matching_codes_pair_over_the_network() {
        let left = DeviceIdentity::generate("left").unwrap();
        let right = DeviceIdentity::generate("right").unwrap();
        let code = PairingCode::generate().unwrap();

        let listener = PairingListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (listener_result, connector_result) = tokio::join!(
            listener.accept_and_pair(identity_of(&left), &code, Duration::from_secs(5)),
            pair_with(addr, identity_of(&right), &code, Duration::from_secs(5)),
        );

        let peer_of_left = listener_result.unwrap();
        assert_eq!(peer_of_left.device_id, right.device_id());
        assert_eq!(peer_of_left.device_name, "right");
        assert_eq!(peer_of_left.fingerprint, right.spki_fingerprint().unwrap());

        let peer_of_right = connector_result.unwrap();
        assert_eq!(peer_of_right.device_id, left.device_id());
        assert_eq!(peer_of_right.fingerprint, left.spki_fingerprint().unwrap());
    }

    #[tokio::test]
    async fn wrong_code_fails_both_sides_over_the_network() {
        let left = DeviceIdentity::generate("left").unwrap();
        let right = DeviceIdentity::generate("right").unwrap();
        let code = PairingCode::parse("1111-2222").unwrap();
        let wrong = PairingCode::parse("1111-2223").unwrap();

        let listener = PairingListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (listener_result, connector_result) = tokio::join!(
            listener.accept_and_pair(identity_of(&left), &code, Duration::from_secs(5)),
            pair_with(addr, identity_of(&right), &wrong, Duration::from_secs(5)),
        );

        assert!(matches!(
            listener_result,
            Err(PairingDriveError::Ceremony(
                PairingError::ConfirmationMismatch
            ))
        ));
        assert!(matches!(
            connector_result,
            Err(PairingDriveError::Ceremony(
                PairingError::ConfirmationMismatch
            ))
        ));
    }

    #[tokio::test]
    async fn listener_times_out_when_nobody_pairs() {
        let left = DeviceIdentity::generate("left").unwrap();
        let code = PairingCode::generate().unwrap();
        let listener = PairingListener::bind("127.0.0.1:0").await.unwrap();

        let result = listener
            .accept_and_pair(identity_of(&left), &code, Duration::from_millis(200))
            .await;
        assert!(matches!(result, Err(PairingDriveError::Timeout)));
    }

    #[tokio::test]
    async fn garbage_on_the_pairing_port_fails_without_panic() {
        let left = DeviceIdentity::generate("left").unwrap();
        let code = PairingCode::generate().unwrap();
        let listener = PairingListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (result, ()) = tokio::join!(
            listener.accept_and_pair(identity_of(&left), &code, Duration::from_secs(2)),
            async {
                let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                tcp.write_all(&[0xFF; 256]).await.unwrap();
                // Keep the socket open past the failure so the listener's
                // error is the garbage, not a disconnect race.
                tokio::time::sleep(Duration::from_millis(500)).await;
                drop(tcp);
            }
        );
        assert!(matches!(
            result,
            Err(PairingDriveError::Protocol(_) | PairingDriveError::UnexpectedMessage { .. })
        ));
    }

    #[test]
    fn roles_are_reexported_for_the_cli() {
        // The CLI slice consumes these through crossover-core's re-export
        // surface; keep the path compiling.
        let _ = PairingRole::Listener;
    }
}
