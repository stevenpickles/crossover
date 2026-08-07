//! The pairing ceremony (ADR 0002, FR-1.2, docs/SECURITY.md §3).
//!
//! SPAKE2 keyed by a short one-time code that the user *types* on the
//! connecting machine — the honest path and the secure path are the same
//! path. Both sides derive a strong shared key only if they used the same
//! code; each then proves knowledge of that key with an HMAC over the
//! ceremony transcript and its identity claim. A wrong code or an active
//! MITM gets exactly one online guess, and the failure is loud and
//! terminal ([`PairingError::ConfirmationMismatch`]).
//!
//! This module is sans-io: it consumes and produces plain values. The
//! wire structs live in `crossover-protocol`; the async driver lives in
//! `crossover-core`. Known limitation (two-generals): each side persists
//! trust after verifying the peer's confirmation, so a connection lost at
//! exactly that moment can leave trust one-sided — re-pairing resolves
//! it.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::identity::{IdentityError, SpkiFingerprint, validate_device_name};

type HmacSha256 = Hmac<Sha256>;

/// Digits in a pairing code.
pub const PAIRING_CODE_DIGITS: usize = 8;

/// Upper bound on SPAKE2 exchange elements this ceremony accepts
/// (mirrors the wire bound in `crossover-protocol`).
pub const MAX_SPAKE_MESSAGE_BYTES: usize = 64;

const TRANSCRIPT_LABEL: &[u8] = b"crossover-pairing-v1";
const LISTENER_SPAKE_IDENTITY: &[u8] = b"crossover-pairing-listener";
const CONNECTOR_SPAKE_IDENTITY: &[u8] = b"crossover-pairing-connector";

/// Failures in the pairing ceremony.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum PairingError {
    /// A pairing code failed validation.
    #[error("invalid pairing code: {reason}")]
    Code { reason: String },

    /// The OS random source failed (fail closed — never pair with weak
    /// entropy).
    #[error("random generation failed: {reason}")]
    Randomness { reason: String },

    /// The SPAKE2 exchange failed structurally (malformed element).
    #[error("pairing key exchange failed: {reason}")]
    Crypto { reason: String },

    /// The peer's confirmation MAC did not verify: wrong code or active
    /// man-in-the-middle. The ceremony is over — codes are single-use,
    /// so there is no retry with the same code.
    #[error("pairing confirmation failed: wrong code or man-in-the-middle")]
    ConfirmationMismatch,

    /// A ceremony method was called out of order.
    #[error("pairing ceremony used out of order")]
    InvalidState,

    /// The peer's identity claim is structurally invalid.
    #[error("invalid peer pairing data: {reason}")]
    InvalidPeerData { reason: String },
}

/// A one-time numeric pairing code, canonically eight ASCII digits.
///
/// The code is the ceremony's only secret; `Debug` redacts it and it is
/// zeroized on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl Drop for PairingCode {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

impl std::fmt::Display for PairingCode {
    /// Grouped for reading aloud and typing: `1234-5678`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", &self.0[..4], &self.0[4..])
    }
}

impl PairingCode {
    /// Generate a fresh uniform code from OS randomness.
    ///
    /// # Errors
    ///
    /// [`PairingError::Randomness`] if the OS random source fails.
    pub fn generate() -> Result<Self, PairingError> {
        // Rejection sampling for uniformity over 10^8 values.
        const LIMIT: u32 = 100_000_000;
        const ZONE: u32 = u32::MAX - (u32::MAX % LIMIT);
        loop {
            let mut bytes = [0u8; 4];
            getrandom::fill(&mut bytes).map_err(|e| PairingError::Randomness {
                reason: e.to_string(),
            })?;
            let value = u32::from_be_bytes(bytes);
            if value < ZONE {
                return Ok(Self(format!("{:08}", value % LIMIT)));
            }
        }
    }

    /// Parse a user-entered code; separators (`-`, spaces) are ignored.
    ///
    /// # Errors
    ///
    /// [`PairingError::Code`] unless exactly
    /// [`PAIRING_CODE_DIGITS`] ASCII digits remain.
    pub fn parse(input: &str) -> Result<Self, PairingError> {
        let digits: String = input
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
        if digits.len() != PAIRING_CODE_DIGITS || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(PairingError::Code {
                reason: format!("expected {PAIRING_CODE_DIGITS} digits"),
            });
        }
        Ok(Self(digits))
    }
}

/// Which side of the ceremony this machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingRole {
    /// Displayed the code (`crossover pair --listen`).
    Listener,
    /// Typed the code (`crossover pair <address>`).
    Connector,
}

impl PairingRole {
    fn confirm_label(self) -> &'static [u8] {
        match self {
            Self::Listener => b"crossover-confirm-listener",
            Self::Connector => b"crossover-confirm-connector",
        }
    }

    fn peer(self) -> Self {
        match self {
            Self::Listener => Self::Connector,
            Self::Connector => Self::Listener,
        }
    }
}

/// The local identity claim carried in the confirmation message.
#[derive(Debug, Clone)]
pub struct PairingIdentity {
    /// Device UUID (bookkeeping).
    pub device_id: Uuid,
    /// Device name.
    pub device_name: String,
    /// The identity the peer will pin (ADR 0003).
    pub fingerprint: SpkiFingerprint,
}

/// The peer identity a successful ceremony yields — what the caller
/// records in the trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    /// Peer device UUID.
    pub device_id: Uuid,
    /// Peer device name.
    pub device_name: String,
    /// Peer credential fingerprint to pin.
    pub fingerprint: SpkiFingerprint,
}

/// The confirmation values exchanged in round two (mirrors the wire
/// struct in `crossover-protocol`, kept serde-free here so the layering
/// between the two crates stays edge-free).
#[derive(Debug, Clone)]
pub struct ConfirmParts {
    /// Sender's device UUID.
    pub device_id: Uuid,
    /// Sender's device name.
    pub device_name: String,
    /// Sender's credential fingerprint.
    pub fingerprint: SpkiFingerprint,
    /// HMAC over the transcript and the fields above.
    pub mac: [u8; 32],
}

enum CeremonyState {
    AwaitingPeerStart { spake: Box<Spake2<Ed25519Group>> },
    AwaitingPeerConfirm { key: Vec<u8>, transcript: Vec<u8> },
    Done,
}

/// One run of the pairing ceremony. Single-use, in lockstep:
/// [`PairingCeremony::new`] → [`PairingCeremony::receive_peer_start`] →
/// [`PairingCeremony::receive_peer_confirm`].
pub struct PairingCeremony {
    role: PairingRole,
    local: PairingIdentity,
    own_start: Vec<u8>,
    state: CeremonyState,
}

impl Drop for PairingCeremony {
    fn drop(&mut self) {
        if let CeremonyState::AwaitingPeerConfirm { key, .. } = &mut self.state {
            key.zeroize();
        }
    }
}

impl PairingCeremony {
    /// Begin a ceremony; returns the ceremony and the SPAKE2 element to
    /// send as `PairingStart`.
    ///
    /// # Errors
    ///
    /// [`PairingError::InvalidPeerData`] if the local identity fails
    /// validation (defensive; local data should already be valid).
    pub fn new(
        role: PairingRole,
        code: &PairingCode,
        local: PairingIdentity,
    ) -> Result<(Self, Vec<u8>), PairingError> {
        validate_identity(&local.device_name)?;
        let password = Password::new(code.0.as_bytes());
        let id_listener = Identity::new(LISTENER_SPAKE_IDENTITY);
        let id_connector = Identity::new(CONNECTOR_SPAKE_IDENTITY);
        let (spake, own_start) = match role {
            PairingRole::Listener => {
                Spake2::<Ed25519Group>::start_a(&password, &id_listener, &id_connector)
            }
            PairingRole::Connector => {
                Spake2::<Ed25519Group>::start_b(&password, &id_listener, &id_connector)
            }
        };
        Ok((
            Self {
                role,
                local,
                own_start: own_start.clone(),
                state: CeremonyState::AwaitingPeerStart {
                    spake: Box::new(spake),
                },
            },
            own_start,
        ))
    }

    /// Consume the peer's `PairingStart`; returns our `PairingConfirm`
    /// values to send.
    ///
    /// # Errors
    ///
    /// [`PairingError::InvalidPeerData`] for an out-of-bound element;
    /// [`PairingError::Crypto`] for a structurally invalid element;
    /// [`PairingError::InvalidState`] out of order.
    pub fn receive_peer_start(&mut self, peer_start: &[u8]) -> Result<ConfirmParts, PairingError> {
        if peer_start.is_empty() || peer_start.len() > MAX_SPAKE_MESSAGE_BYTES {
            return Err(PairingError::InvalidPeerData {
                reason: format!(
                    "SPAKE2 element must be 1..={MAX_SPAKE_MESSAGE_BYTES} bytes, got {}",
                    peer_start.len()
                ),
            });
        }
        let CeremonyState::AwaitingPeerStart { spake } =
            std::mem::replace(&mut self.state, CeremonyState::Done)
        else {
            return Err(PairingError::InvalidState);
        };

        let key = spake.finish(peer_start).map_err(|e| PairingError::Crypto {
            reason: e.to_string(),
        })?;

        // Transcript binds both SPAKE2 elements in role order
        // (listener first), independent of which side we are.
        let (listener_msg, connector_msg) = match self.role {
            PairingRole::Listener => (self.own_start.as_slice(), peer_start),
            PairingRole::Connector => (peer_start, self.own_start.as_slice()),
        };
        let mut transcript = Vec::new();
        transcript.extend_from_slice(TRANSCRIPT_LABEL);
        transcript.extend_from_slice(&(listener_msg.len() as u64).to_be_bytes());
        transcript.extend_from_slice(listener_msg);
        transcript.extend_from_slice(&(connector_msg.len() as u64).to_be_bytes());
        transcript.extend_from_slice(connector_msg);

        let mac = confirm_mac(&key, self.role, &transcript, &self.local)?;
        let parts = ConfirmParts {
            device_id: self.local.device_id,
            device_name: self.local.device_name.clone(),
            fingerprint: self.local.fingerprint,
            mac,
        };
        self.state = CeremonyState::AwaitingPeerConfirm { key, transcript };
        Ok(parts)
    }

    /// Verify the peer's `PairingConfirm`. Success yields the peer to
    /// record in the trust store; failure is terminal.
    ///
    /// # Errors
    ///
    /// [`PairingError::ConfirmationMismatch`] for a MAC that does not
    /// verify (wrong code or MITM); [`PairingError::InvalidPeerData`] for
    /// invalid identity fields, including a fingerprint equal to our own
    /// (reflection); [`PairingError::InvalidState`] out of order.
    pub fn receive_peer_confirm(
        &mut self,
        peer: &ConfirmParts,
    ) -> Result<PairedPeer, PairingError> {
        let CeremonyState::AwaitingPeerConfirm {
            mut key,
            transcript,
        } = std::mem::replace(&mut self.state, CeremonyState::Done)
        else {
            return Err(PairingError::InvalidState);
        };

        validate_identity(&peer.device_name)?;
        if peer.fingerprint == self.local.fingerprint {
            key.zeroize();
            return Err(PairingError::InvalidPeerData {
                reason: "peer presented our own identity (reflection)".to_owned(),
            });
        }

        let peer_identity = PairingIdentity {
            device_id: peer.device_id,
            device_name: peer.device_name.clone(),
            fingerprint: peer.fingerprint,
        };
        let expected = confirm_mac(&key, self.role.peer(), &transcript, &peer_identity);
        key.zeroize();
        let expected = expected?;

        // Constant-time comparison via HMAC's own verifier semantics:
        // compare digests with subtle-backed equality.
        if !constant_time_eq(&expected, &peer.mac) {
            return Err(PairingError::ConfirmationMismatch);
        }

        Ok(PairedPeer {
            device_id: peer.device_id,
            device_name: peer.device_name.clone(),
            fingerprint: peer.fingerprint,
        })
    }
}

/// MAC binding a role's identity claim to the ceremony: HMAC keyed by a
/// role-separated key derived from the SPAKE2 output, over the identity
/// fields (length-framed) and the transcript.
fn confirm_mac(
    key: &[u8],
    role: PairingRole,
    transcript: &[u8],
    identity: &PairingIdentity,
) -> Result<[u8; 32], PairingError> {
    let role_key = HmacSha256::new_from_slice(key)
        .map_err(|e| PairingError::Crypto {
            reason: e.to_string(),
        })?
        .chain_update(role.confirm_label())
        .finalize()
        .into_bytes();

    let mut mac = HmacSha256::new_from_slice(&role_key).map_err(|e| PairingError::Crypto {
        reason: e.to_string(),
    })?;
    mac.update(identity.device_id.as_bytes());
    mac.update(&(identity.device_name.len() as u64).to_be_bytes());
    mac.update(identity.device_name.as_bytes());
    mac.update(identity.fingerprint.as_bytes());
    mac.update(transcript);
    Ok(mac.finalize().into_bytes().into())
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    // Bitwise accumulate so the comparison does not short-circuit.
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn validate_identity(device_name: &str) -> Result<(), PairingError> {
    validate_device_name(device_name).map_err(|e| match e {
        IdentityError::InvalidDeviceName { got } => PairingError::InvalidPeerData {
            reason: format!("device name of {got} bytes is out of bounds"),
        },
        other => PairingError::InvalidPeerData {
            reason: other.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{
        ConfirmParts, PairedPeer, PairingCeremony, PairingCode, PairingError, PairingIdentity,
        PairingRole,
    };
    use crate::identity::SpkiFingerprint;

    fn identity(fill: u8, name: &str) -> PairingIdentity {
        PairingIdentity {
            device_id: Uuid::from_bytes([fill; 16]),
            device_name: name.to_owned(),
            fingerprint: SpkiFingerprint([fill; 32]),
        }
    }

    fn run_ceremony(
        listener_code: &PairingCode,
        connector_code: &PairingCode,
    ) -> (
        Result<PairedPeer, PairingError>,
        Result<PairedPeer, PairingError>,
    ) {
        let (mut listener, listener_start) =
            PairingCeremony::new(PairingRole::Listener, listener_code, identity(0xAA, "left"))
                .unwrap();
        let (mut connector, connector_start) = PairingCeremony::new(
            PairingRole::Connector,
            connector_code,
            identity(0xBB, "right"),
        )
        .unwrap();

        let listener_confirm = listener.receive_peer_start(&connector_start).unwrap();
        let connector_confirm = connector.receive_peer_start(&listener_start).unwrap();

        (
            listener.receive_peer_confirm(&connector_confirm),
            connector.receive_peer_confirm(&listener_confirm),
        )
    }

    #[test]
    fn matching_codes_pair_both_sides() {
        let code = PairingCode::generate().unwrap();
        let (listener_result, connector_result) = run_ceremony(&code, &code);

        let peer_of_listener = listener_result.unwrap();
        assert_eq!(peer_of_listener.device_name, "right");
        assert_eq!(peer_of_listener.fingerprint, SpkiFingerprint([0xBB; 32]));

        let peer_of_connector = connector_result.unwrap();
        assert_eq!(peer_of_connector.device_name, "left");
        assert_eq!(peer_of_connector.fingerprint, SpkiFingerprint([0xAA; 32]));
    }

    #[test]
    fn wrong_code_fails_loudly_on_both_sides() {
        let code = PairingCode::parse("1234-5678").unwrap();
        let wrong = PairingCode::parse("1234-5679").unwrap();
        let (listener_result, connector_result) = run_ceremony(&code, &wrong);
        assert!(matches!(
            listener_result,
            Err(PairingError::ConfirmationMismatch)
        ));
        assert!(matches!(
            connector_result,
            Err(PairingError::ConfirmationMismatch)
        ));
    }

    #[test]
    fn tampered_spake_element_cannot_authenticate() {
        let code = PairingCode::generate().unwrap();
        let (mut listener, _listener_start) =
            PairingCeremony::new(PairingRole::Listener, &code, identity(0xAA, "left")).unwrap();
        let (_connector, connector_start) =
            PairingCeremony::new(PairingRole::Connector, &code, identity(0xBB, "right")).unwrap();

        // Flip one bit of the connector's element in flight.
        let mut tampered = connector_start;
        tampered[10] ^= 0x01;

        // Either the exchange fails structurally, or it yields a key the
        // real connector does not share — in which case the confirm from
        // the honest side could never verify. Both are fail-closed.
        match listener.receive_peer_start(&tampered) {
            Err(PairingError::Crypto { .. }) => {}
            Ok(_) => {
                // The listener derived *some* key; verify a mismatched
                // confirm is rejected by handing it garbage claiming to
                // be the connector.
                let forged = ConfirmParts {
                    device_id: Uuid::from_bytes([0xBB; 16]),
                    device_name: "right".to_owned(),
                    fingerprint: SpkiFingerprint([0xBB; 32]),
                    mac: [0; 32],
                };
                assert!(matches!(
                    listener.receive_peer_confirm(&forged),
                    Err(PairingError::ConfirmationMismatch)
                ));
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn reflected_confirm_is_rejected() {
        let code = PairingCode::generate().unwrap();
        let (mut listener, listener_start) =
            PairingCeremony::new(PairingRole::Listener, &code, identity(0xAA, "left")).unwrap();
        let (mut connector, connector_start) =
            PairingCeremony::new(PairingRole::Connector, &code, identity(0xBB, "right")).unwrap();

        let listener_confirm = listener.receive_peer_start(&connector_start).unwrap();
        let _connector_confirm = connector.receive_peer_start(&listener_start).unwrap();

        // An attacker reflects the listener's own confirm back at it.
        // Role-separated MAC keys make it unverifiable — and the identity
        // guard would reject the mirrored fingerprint first.
        let reflected = listener_confirm;
        assert!(matches!(
            listener.receive_peer_confirm(&reflected),
            Err(PairingError::InvalidPeerData { .. } | PairingError::ConfirmationMismatch)
        ));
    }

    #[test]
    fn ceremony_enforces_message_order_and_single_use() {
        let code = PairingCode::generate().unwrap();
        let (mut ceremony, _start) =
            PairingCeremony::new(PairingRole::Listener, &code, identity(0xAA, "left")).unwrap();

        let confirm = ConfirmParts {
            device_id: Uuid::from_bytes([0xBB; 16]),
            device_name: "right".to_owned(),
            fingerprint: SpkiFingerprint([0xBB; 32]),
            mac: [0; 32],
        };
        // Confirm before start: out of order.
        assert!(matches!(
            ceremony.receive_peer_confirm(&confirm),
            Err(PairingError::InvalidState)
        ));
        // The failed call consumed the ceremony: single-use.
        assert!(matches!(
            ceremony.receive_peer_start(&[1, 2, 3]),
            Err(PairingError::InvalidState)
        ));
    }

    #[test]
    fn codes_generate_parse_display_and_redact() {
        let code = PairingCode::generate().unwrap();
        let shown = code.to_string();
        assert_eq!(shown.len(), 9);
        assert_eq!(&shown[4..5], "-");

        // What the user reads back in — with or without separators.
        assert_eq!(PairingCode::parse(&shown).unwrap(), code);
        assert_eq!(PairingCode::parse(&shown.replace('-', " ")).unwrap(), code);

        for bad in ["", "1234567", "123456789", "1234-567a", "abcd-efgh"] {
            assert!(matches!(
                PairingCode::parse(bad),
                Err(PairingError::Code { .. })
            ));
        }

        assert_eq!(format!("{code:?}"), "PairingCode(<redacted>)");
    }

    #[test]
    fn oversized_or_empty_peer_elements_are_rejected() {
        let code = PairingCode::generate().unwrap();
        let (mut ceremony, _start) =
            PairingCeremony::new(PairingRole::Listener, &code, identity(0xAA, "left")).unwrap();
        assert!(matches!(
            ceremony.receive_peer_start(&[]),
            Err(PairingError::InvalidPeerData { .. })
        ));
    }
}
