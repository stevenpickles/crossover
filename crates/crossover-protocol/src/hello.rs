//! The `Hello` message: session establishment and feature negotiation
//! (docs/PROTOCOL.md §3).
//!
//! Sent by each side immediately after TLS establishment and peer
//! authorization. Payload schemas are frozen per protocol version
//! (ADR 0001): any change to this struct's fields is a protocol version
//! bump, guarded by the golden wire-snapshot test below.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ProtocolError;

/// Wire bound on the device name. The canonical protocol limit;
/// `crossover-security` enforces the same value for locally created names.
pub const MAX_DEVICE_NAME_BYTES: usize = 64;

/// Message types defined by protocol version 1.
///
/// Wire values are explicit and never reused. The framing layer carries a
/// raw `u16`; this enum is the session layer's policy for which types
/// exist at the negotiated version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageType {
    /// Session establishment ([`Hello`]).
    Hello = 1,
}

impl MessageType {
    /// Map a wire value to a known message type; `None` means unknown —
    /// skippable or fatal per negotiation policy, decided by the caller
    /// (docs/PROTOCOL.md §7), not by this layer.
    #[must_use]
    pub const fn from_wire(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Hello),
            _ => None,
        }
    }

    /// The value carried in the frame header.
    #[must_use]
    pub const fn wire(self) -> u16 {
        self as u16
    }
}

/// Operating-system family, informational (diagnostics, future
/// platform-specific negotiation). Never used for authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsFamily {
    /// Microsoft Windows.
    Windows,
    /// Apple macOS.
    MacOs,
    /// Linux.
    Linux,
    /// Anything else (reported by future builds; valid on the wire today).
    Other,
}

/// Feature bitmask for capability negotiation beyond the base protocol.
///
/// Bit assignments are defined per protocol version; version 1 defines no
/// bits, so the only valid value is empty. Unknown bits from a peer are
/// ignored, not an error: a feature is active only when *both* sides
/// advertise it, so an unknown bit simply never activates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FeatureFlags(pub u64);

impl FeatureFlags {
    /// No features — the only value version 1 emits.
    pub const NONE: Self = Self(0);
}

/// Session-establishment message (docs/PROTOCOL.md §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Highest protocol version the sender speaks.
    pub protocol_version: u16,
    /// Lowest protocol version the sender accepts.
    pub min_protocol_version: u16,
    /// Sender's device UUID (bookkeeping identity, never authentication —
    /// authentication happened at the TLS layer, docs/SECURITY.md §5).
    pub device_id: Uuid,
    /// Sender's human-readable device name.
    pub device_name: String,
    /// Sender's OS family, informational.
    pub operating_system: OsFamily,
    /// Advertised optional capabilities.
    pub supported_features: FeatureFlags,
}

impl Hello {
    /// Semantic validation, applied on both encode and decode: a `Hello`
    /// we would reject from a peer must also be impossible to send.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for an empty/oversized device name or
    /// an inverted version range.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.device_name.is_empty() || self.device_name.len() > MAX_DEVICE_NAME_BYTES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "device name must be 1..={MAX_DEVICE_NAME_BYTES} bytes, got {}",
                    self.device_name.len()
                ),
            });
        }
        if self.min_protocol_version > self.protocol_version {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "inverted version range {}..={}",
                    self.min_protocol_version, self.protocol_version
                ),
            });
        }
        Ok(())
    }

    /// Encode the payload (postcard, ADR 0001). Validates first: we never
    /// send what we would refuse to receive.
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

    /// Decode and validate a payload.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable bytes, trailing bytes
    /// (strict framing — the payload length is exact, docs/PROTOCOL.md §2),
    /// or semantically invalid contents.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let (hello, rest): (Self, &[u8]) =
            postcard::take_from_bytes(payload).map_err(|e| ProtocolError::Malformed {
                reason: format!("undecodable Hello payload: {e}"),
            })?;
        if !rest.is_empty() {
            return Err(ProtocolError::Malformed {
                reason: format!("{} trailing bytes after Hello payload", rest.len()),
            });
        }
        hello.validate()?;
        Ok(hello)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{FeatureFlags, Hello, MAX_DEVICE_NAME_BYTES, MessageType, OsFamily};
    use crate::ProtocolError;
    use crate::framing::{FrameDecoder, encode_frame};

    fn sample() -> Hello {
        Hello {
            protocol_version: 1,
            min_protocol_version: 1,
            device_id: Uuid::from_bytes([0x11; 16]),
            device_name: "left".to_owned(),
            operating_system: OsFamily::Windows,
            supported_features: FeatureFlags::NONE,
        }
    }

    #[test]
    fn round_trips_through_payload_encoding() {
        let hello = sample();
        let decoded = Hello::decode_payload(&hello.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, hello);
    }

    /// Golden wire snapshot (ADR 0001): if this test fails, the version-1
    /// Hello schema changed — that is a protocol version bump, not a test
    /// to update casually.
    #[test]
    fn golden_wire_snapshot_v1() {
        let encoded = sample().encode_payload().unwrap();
        let expected: Vec<u8> = [
            &[0x01, 0x01][..],                   // versions 1, 1 (varints)
            &[0x10],                             // device_id: 16-byte length
            &[0x11; 16][..],                     // device_id bytes
            &[0x04, b'l', b'e', b'f', b't'][..], // device_name
            &[0x00],                             // OsFamily::Windows
            &[0x00],                             // FeatureFlags(0)
        ]
        .concat();
        assert_eq!(
            encoded, expected,
            "v1 Hello wire layout changed: bump the protocol version (ADR 0001)"
        );
    }

    #[test]
    fn hello_travels_inside_a_frame_end_to_end() {
        let hello = sample();
        let frame_bytes = encode_frame(
            MessageType::Hello.wire(),
            1,
            &hello.encode_payload().unwrap(),
        )
        .unwrap();

        let mut decoder = FrameDecoder::new();
        decoder.extend(&frame_bytes).unwrap();
        let frame = decoder.next_frame().unwrap().unwrap();

        assert_eq!(
            MessageType::from_wire(frame.message_type),
            Some(MessageType::Hello)
        );
        assert_eq!(Hello::decode_payload(&frame.payload).unwrap(), hello);
    }

    #[test]
    fn unknown_message_types_map_to_none_not_error() {
        assert_eq!(MessageType::from_wire(0), None);
        assert_eq!(MessageType::from_wire(0xFFFF), None);
        assert_eq!(MessageType::from_wire(1), Some(MessageType::Hello));
    }

    #[test]
    fn invalid_names_rejected_on_encode_and_decode() {
        let mut hello = sample();
        hello.device_name = String::new();
        assert!(matches!(
            hello.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        // An oversized name can't be built through encode_payload's
        // validation, so encode it directly with serde to prove the
        // decode side independently enforces the bound.
        let mut oversized = sample();
        oversized.device_name = "x".repeat(MAX_DEVICE_NAME_BYTES + 1);
        let bytes = postcard::to_stdvec(&oversized).unwrap();
        assert!(matches!(
            Hello::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));

        // The boundary value is accepted.
        let mut at_limit = sample();
        at_limit.device_name = "x".repeat(MAX_DEVICE_NAME_BYTES);
        let bytes = at_limit.encode_payload().unwrap();
        assert!(Hello::decode_payload(&bytes).is_ok());
    }

    #[test]
    fn inverted_version_range_rejected() {
        let mut hello = sample();
        hello.min_protocol_version = 2;
        hello.protocol_version = 1;
        assert!(matches!(
            hello.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn garbage_truncated_and_padded_payloads_are_malformed() {
        assert!(matches!(
            Hello::decode_payload(&[0xFF; 40]),
            Err(ProtocolError::Malformed { .. })
        ));

        let good = sample().encode_payload().unwrap();
        assert!(matches!(
            Hello::decode_payload(&good[..good.len() - 1]),
            Err(ProtocolError::Malformed { .. })
        ));

        let mut padded = good;
        padded.push(0x00);
        assert!(matches!(
            Hello::decode_payload(&padded),
            Err(ProtocolError::Malformed { .. })
        ));
    }
}
