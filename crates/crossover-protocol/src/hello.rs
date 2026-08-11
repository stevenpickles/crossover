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
    /// Keepalive probe (CONTROL class, empty payload). Sent on an idle
    /// session; the peer answers with [`MessageType::Pong`].
    Ping = 2,
    /// Keepalive answer (CONTROL class, empty payload).
    Pong = 3,
    /// Pairing: SPAKE2 exchange element (plain-TCP ceremony, ADR 0002).
    PairingStart = 4,
    /// Pairing: MAC-authenticated identity claim.
    PairingConfirm = 5,
    /// Clipboard: announce a large item (ADR 0005 offered flow).
    ClipboardOffer = 6,
    /// Clipboard: accept an offered item.
    ClipboardAccept = 7,
    /// Clipboard: decline an offered item (typed reason).
    ClipboardDecline = 8,
    /// Clipboard: the item itself (inline flow, or after an accept).
    ClipboardData = 9,
    /// Clipboard: destination verdict — the only definition of success.
    ClipboardApplied = 10,
    /// Input: an ordered batch of pointer (later keyboard) events.
    InputBatch = 11,
    /// Input: release everything the destination believes is held
    /// (FR-4.4).
    ReleaseAllInput = 12,
    /// Control: ask the peer for control (FR-5.3).
    ControlRequest = 13,
    /// Control: grant or deny a request.
    ControlResponse = 14,
    /// Control: end the control relationship (hand-back or revocation).
    ControlRelease = 15,
    /// Clipboard: one fragment of a chunked item (ADR 0014). Gated by
    /// [`FeatureFlags::CHUNKED_CLIPBOARD`] — a peer that has not
    /// advertised it is never sent one.
    ClipboardChunk = 16,
}

impl MessageType {
    /// Map a wire value to a known message type; `None` means unknown —
    /// skippable or fatal per negotiation policy, decided by the caller
    /// (docs/PROTOCOL.md §7), not by this layer.
    #[must_use]
    pub const fn from_wire(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Hello),
            2 => Some(Self::Ping),
            3 => Some(Self::Pong),
            4 => Some(Self::PairingStart),
            5 => Some(Self::PairingConfirm),
            6 => Some(Self::ClipboardOffer),
            7 => Some(Self::ClipboardAccept),
            8 => Some(Self::ClipboardDecline),
            9 => Some(Self::ClipboardData),
            10 => Some(Self::ClipboardApplied),
            11 => Some(Self::InputBatch),
            12 => Some(Self::ReleaseAllInput),
            13 => Some(Self::ControlRequest),
            14 => Some(Self::ControlResponse),
            15 => Some(Self::ControlRelease),
            16 => Some(Self::ClipboardChunk),
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
/// Bit assignments are defined per protocol version. Unknown bits from a
/// peer are ignored, not an error: a feature is active only when *both*
/// sides advertise it ([`FeatureFlags::negotiate`]), so an unknown bit
/// simply never activates.
///
/// This is the route PROTOCOL.md §3 fixes for capabilities beyond the base
/// protocol, and ADR 0014's answer to its own interop question. It matters
/// because unknown *message types* are skipped rather than fatal (§2): a
/// peer sent content it does not understand would answer nothing at all,
/// and a transaction that never gets an answer is exactly the silent
/// failure NFR-3 forbids. So the sender asks first, via these bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FeatureFlags(pub u64);

impl FeatureFlags {
    /// No features.
    pub const NONE: Self = Self(0);

    /// Bit 0 — chunked rich-clipboard transfer (ADR 0014): the sender may
    /// offer `ContentType::Image` items and stream them as
    /// [`MessageType::ClipboardChunk`] messages. Advertising it means "I
    /// can reassemble a chunked item and install it", so a build only sets
    /// it once it genuinely can.
    pub const CHUNKED_CLIPBOARD: Self = Self(1 << 0);

    /// Every bit this protocol version defines.
    pub const ALL: Self = Self(Self::CHUNKED_CLIPBOARD.0);

    /// What **this build** advertises in its `Hello`.
    ///
    /// Empty today, deliberately, and now for exactly one reason: the wire
    /// layer carries chunked images and the clipboard engine reassembles,
    /// verifies and installs them (ADR 0014's protocol and engine slices),
    /// but no platform backend can yet put a raster format on a real
    /// clipboard — `crossover-platform-windows` reads an image as absent
    /// and refuses to write one. Advertising is a promise to *handle*, so
    /// promising it here would mean accepting a transfer this build must
    /// then fail at the last step, after the peer moved every byte.
    ///
    /// **The ADR 0014 platform slice sets this to [`FeatureFlags::ALL`]**
    /// — one line, and image transfer switches on for both sides at once,
    /// with nothing else to change. Tests that need the negotiated path
    /// before then override the advertisement per session
    /// (`SessionOptions::advertised_features`) rather than weakening this
    /// constant.
    pub const ADVERTISED: Self = Self::NONE;

    /// Whether every bit in `feature` is set. `NONE` is contained by
    /// everything, so base-protocol capabilities never need a bit.
    #[must_use]
    pub const fn contains(self, feature: Self) -> bool {
        self.0 & feature.0 == feature.0
    }

    /// The features active on a session: the intersection, because a
    /// capability is usable only if both sides have it.
    #[must_use]
    pub const fn negotiate(local: Self, peer: Self) -> Self {
        Self(local.0 & peer.0)
    }
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
            protocol_version: 2,
            min_protocol_version: 2,
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
            &[0x02, 0x02][..],                   // versions 2, 2 (varints)
            &[0x10],                             // device_id: 16-byte length
            &[0x11; 16][..],                     // device_id bytes
            &[0x04, b'l', b'e', b'f', b't'][..], // device_name
            &[0x00],                             // OsFamily::Windows
            &[0x00],                             // FeatureFlags(0)
        ]
        .concat();
        assert_eq!(
            encoded, expected,
            "Hello wire layout changed: bump the protocol version (ADR 0001)"
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
        assert_eq!(MessageType::from_wire(2), Some(MessageType::Ping));
        assert_eq!(MessageType::from_wire(3), Some(MessageType::Pong));
    }

    #[test]
    fn wire_values_round_trip_for_all_known_types() {
        for ty in [
            MessageType::Hello,
            MessageType::Ping,
            MessageType::Pong,
            MessageType::PairingStart,
            MessageType::PairingConfirm,
            MessageType::ClipboardOffer,
            MessageType::ClipboardAccept,
            MessageType::ClipboardDecline,
            MessageType::ClipboardData,
            MessageType::ClipboardApplied,
            MessageType::InputBatch,
            MessageType::ReleaseAllInput,
        ] {
            assert_eq!(MessageType::from_wire(ty.wire()), Some(ty));
        }
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
