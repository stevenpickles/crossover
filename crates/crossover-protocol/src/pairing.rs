//! Pairing wire messages (ADR 0002, docs/SECURITY.md §3).
//!
//! Exchanged over a **plain TCP** connection — pairing is how trust is
//! created, so no TLS identity exists yet. All security comes from the
//! SPAKE2 exchange and MAC confirmation implemented in
//! `crossover-security`; these structs are just the bytes on the wire,
//! validated with the same strictness as every other message.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ProtocolError;
use crate::decode_strict;
use crate::hello::MAX_DEVICE_NAME_BYTES;

/// Bound on the SPAKE2 exchange message (an Ed25519 group element plus
/// framing; 33 bytes today, bounded generously per NFR-1).
pub const MAX_SPAKE_MESSAGE_BYTES: usize = 64;

/// First pairing message, both directions: the sender's SPAKE2 element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingStart {
    /// Opaque SPAKE2 exchange message.
    pub spake_message: Vec<u8>,
}

impl PairingStart {
    /// Semantic validation, applied on encode and decode alike.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for an empty or oversized element.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.spake_message.is_empty() || self.spake_message.len() > MAX_SPAKE_MESSAGE_BYTES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "SPAKE2 message must be 1..={MAX_SPAKE_MESSAGE_BYTES} bytes, got {}",
                    self.spake_message.len()
                ),
            });
        }
        Ok(())
    }

    /// Encode the payload (postcard, ADR 0001), validating first.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable bytes, trailing
    /// bytes, or out-of-bound contents.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "PairingStart")?;
        message.validate()?;
        Ok(message)
    }
}

/// Second pairing message, both directions: the sender's identity claim,
/// authenticated by a MAC keyed from the SPAKE2 output over the ceremony
/// transcript. A wrong code or an active MITM makes this MAC fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingConfirm {
    /// Sender's device UUID (bookkeeping).
    pub device_id: Uuid,
    /// Sender's device name.
    pub device_name: String,
    /// Sender's identity: SPKI SHA-256 fingerprint (what the trust store
    /// will pin, ADR 0003).
    pub spki_fingerprint: [u8; 32],
    /// HMAC-SHA256 confirmation over the ceremony transcript and the
    /// identity fields above (`crossover-security` defines the exact
    /// derivation).
    pub mac: [u8; 32],
}

impl PairingConfirm {
    /// Semantic validation, applied on encode and decode alike.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for an out-of-bound device name.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.device_name.is_empty() || self.device_name.len() > MAX_DEVICE_NAME_BYTES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "device name must be 1..={MAX_DEVICE_NAME_BYTES} bytes, got {}",
                    self.device_name.len()
                ),
            });
        }
        Ok(())
    }

    /// Encode the payload (postcard, ADR 0001), validating first.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable bytes, trailing
    /// bytes, or out-of-bound contents.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "PairingConfirm")?;
        message.validate()?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{MAX_SPAKE_MESSAGE_BYTES, PairingConfirm, PairingStart};
    use crate::ProtocolError;

    #[test]
    fn pairing_start_round_trips_and_enforces_bounds() {
        let start = PairingStart {
            spake_message: vec![0xAB; 33],
        };
        let decoded = PairingStart::decode_payload(&start.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, start);

        for bad in [vec![], vec![0u8; MAX_SPAKE_MESSAGE_BYTES + 1]] {
            let msg = PairingStart { spake_message: bad };
            assert!(matches!(
                msg.encode_payload(),
                Err(ProtocolError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn pairing_confirm_round_trips_and_enforces_bounds() {
        let confirm = PairingConfirm {
            device_id: Uuid::from_bytes([0x22; 16]),
            device_name: "left".to_owned(),
            spki_fingerprint: [0x33; 32],
            mac: [0x44; 32],
        };
        let decoded = PairingConfirm::decode_payload(&confirm.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, confirm);

        let mut bad = confirm.clone();
        bad.device_name = String::new();
        assert!(matches!(
            bad.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Golden wire snapshots (ADR 0001): schema change = version bump.
    #[test]
    fn golden_wire_snapshots_v1() {
        let start = PairingStart {
            spake_message: vec![0xAB; 3],
        };
        assert_eq!(
            start.encode_payload().unwrap(),
            vec![0x03, 0xAB, 0xAB, 0xAB],
            "v1 PairingStart wire layout changed: bump the protocol version"
        );

        let confirm = PairingConfirm {
            device_id: Uuid::from_bytes([0x22; 16]),
            device_name: "l".to_owned(),
            spki_fingerprint: [0x33; 32],
            mac: [0x44; 32],
        };
        let expected: Vec<u8> = [
            &[0x10][..],       // device_id: 16-byte length prefix
            &[0x22; 16][..],   // device_id bytes
            &[0x01, b'l'][..], // device_name
            &[0x33; 32][..],   // fingerprint (fixed array: no prefix)
            &[0x44; 32][..],   // mac (fixed array: no prefix)
        ]
        .concat();
        assert_eq!(
            confirm.encode_payload().unwrap(),
            expected,
            "v1 PairingConfirm wire layout changed: bump the protocol version"
        );
    }

    #[test]
    fn garbage_truncated_and_padded_payloads_are_malformed() {
        for decode in [PairingStart::decode_payload, |b: &[u8]| {
            PairingConfirm::decode_payload(b).map(|_| PairingStart {
                spake_message: vec![1],
            })
        }] {
            assert!(matches!(
                decode(&[0xFF; 20]),
                Err(ProtocolError::Malformed { .. })
            ));
        }

        let good = PairingStart {
            spake_message: vec![1, 2, 3],
        }
        .encode_payload()
        .unwrap();
        let mut padded = good.clone();
        padded.push(0);
        assert!(matches!(
            PairingStart::decode_payload(&padded),
            Err(ProtocolError::Malformed { .. })
        ));
        assert!(matches!(
            PairingStart::decode_payload(&good[..good.len() - 1]),
            Err(ProtocolError::Malformed { .. })
        ));
    }
}
