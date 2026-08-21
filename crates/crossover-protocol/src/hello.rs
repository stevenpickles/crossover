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
    /// Control: the sender's own live monitors, in its own local
    /// coordinates (CONTROL class, ADR 0018, docs/PROTOCOL.md §6.2). Sent
    /// after `Hello` and again whenever the local display configuration
    /// changes. Base protocol at v4 — no feature bit, since a v3 peer is
    /// already excluded at `Hello` by the `entry` shape change.
    MonitorTopology = 17,
    /// Control: the drawn arrangement describing both machines (CONTROL
    /// class, ADR 0018, docs/PROTOCOL.md §6.2). Sent after `Hello` when the
    /// sender holds an explicit layout, and on every edit. Base protocol at
    /// v4, for the same reason as [`MessageType::MonitorTopology`].
    LayoutSync = 18,
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
            17 => Some(Self::MonitorTopology),
            18 => Some(Self::LayoutSync),
            _ => None,
        }
    }

    /// The value carried in the frame header.
    #[must_use]
    pub const fn wire(self) -> u16 {
        self as u16
    }

    /// Which of docs/PROTOCOL.md §4's four logical classes this message
    /// type belongs to — the wire-level fact the table there states, made
    /// queryable instead of re-transcribed at each site that needs it.
    ///
    /// This is one partition of the sixteen (now eighteen) message types;
    /// it is not the *only* one this crate's callers need. `SendPriority`
    /// (`crossover-core::outbound`), inbound routing
    /// (`apps/crossover::commands::inbound_route`), and session dispatch
    /// (`crossover-core::supervision::dispatch_frame`) each partition the
    /// same types differently, for reasons specific to what they are
    /// deciding — `ReleaseAllInput` is CONTROL class here but rides the
    /// same High-priority lane and INPUT-driver route as `InputBatch`, for
    /// instance. Each of those three notes that this accessor exists;
    /// none of them is wrong to partition differently.
    #[must_use]
    pub const fn class(self) -> MessageClass {
        match self {
            Self::Hello
            | Self::Ping
            | Self::Pong
            | Self::PairingStart
            | Self::PairingConfirm
            | Self::ReleaseAllInput
            | Self::ControlRequest
            | Self::ControlResponse
            | Self::ControlRelease
            | Self::MonitorTopology
            | Self::LayoutSync => MessageClass::Control,
            Self::InputBatch => MessageClass::Input,
            Self::ClipboardOffer
            | Self::ClipboardAccept
            | Self::ClipboardDecline
            | Self::ClipboardData
            | Self::ClipboardApplied
            | Self::ClipboardChunk => MessageClass::Clipboard,
        }
    }
}

/// One of docs/PROTOCOL.md §4's four logical message classes —
/// [`MessageType::class`]'s return type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// Hello, control-transfer negotiation, display topology (§6.2),
    /// keepalive, `ReleaseAllInput`, session management. Ordered within
    /// the class, lossless.
    Control,
    /// Key transitions, pointer motion/buttons/scroll. Keys ordered within
    /// the class and lossless; pointer motion coalescable (§6).
    Input,
    /// Clipboard transaction messages. Ordered within the class, lossless,
    /// acknowledged.
    Clipboard,
    /// Latency probes, statistics. Best effort. Not yet used by any
    /// defined message type.
    Telemetry,
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

    /// Bit 1 — file clipboard transfer (ADR 0015): the sender may offer
    /// `ContentType::File` items, carrying a `FileDescriptor` on the
    /// offer and streaming the blob as [`MessageType::ClipboardChunk`]
    /// messages. A bit of its own rather than a widening of bit 0,
    /// because a peer that implements ADR 0014 and not ADR 0015
    /// advertises bit 0 and has no `File` discriminant: an un-negotiated
    /// *content type* is not skipped, it fails that peer's payload decode
    /// and terminates its session (docs/PROTOCOL.md §3.1).
    ///
    /// Advertising it means "I can spool a file and offer it to my OS
    /// clipboard", which is a strictly larger promise than reassembling
    /// bytes: it involves a permission grant, a disk budget, and a
    /// virtual-file paste mechanism.
    pub const FILE_CLIPBOARD: Self = Self(1 << 1);

    /// Every bit this protocol version defines.
    pub const ALL: Self = Self(Self::CHUNKED_CLIPBOARD.0 | Self::FILE_CLIPBOARD.0);

    /// What **this build** advertises in its `Hello`.
    ///
    /// [`FeatureFlags::ALL`] since ADR 0015's final slice (feature/136):
    /// every layer beneath *both* bits is now real. Bit 0 has carried
    /// chunked images since ADR 0014's platform slice — offered, streamed,
    /// reassembled, verified and installed, with
    /// `crossover-platform-windows` reading and writing `CF_DIB` on the
    /// actual OS clipboard. Bit 1 is the same promise for files: the
    /// receiving half — spool, verify, virtual-file paste — landed in
    /// feature/126-132, and the sending half — observation, blob builder,
    /// engine transaction — in feature/133-135; this bit is the deliberate
    /// final act that lets a conforming peer actually reach either half.
    ///
    /// Flipping a bit is wire-visible (the `Hello` a peer receives
    /// changes) and deliberately safe: a feature activates only on the
    /// *intersection* of the two advertisements, so a peer that predates
    /// the bit negotiates it away and is sent nothing new
    /// (docs/PROTOCOL.md §3.1).
    pub const ADVERTISED: Self = Self::ALL;

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

    use super::{FeatureFlags, Hello, MAX_DEVICE_NAME_BYTES, MessageClass, MessageType, OsFamily};
    use crate::ProtocolError;
    use crate::framing::{FrameDecoder, encode_frame};

    fn sample() -> Hello {
        Hello {
            protocol_version: 2,
            min_protocol_version: 2,
            device_id: Uuid::from_bytes([0x11; 16]),
            device_name: "left".to_owned(),
            operating_system: OsFamily::Windows,
            // What this build really sends, so the golden snapshot below
            // pins the actual advertisement rather than a convenient zero.
            supported_features: FeatureFlags::ADVERTISED,
        }
    }

    #[test]
    fn round_trips_through_payload_encoding() {
        let hello = sample();
        let decoded = Hello::decode_payload(&hello.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, hello);
    }

    /// Golden wire snapshot (ADR 0001): if this test fails, either the
    /// version-1 Hello *schema* changed — a protocol version bump, not a
    /// test to update casually — or this build's advertisement did.
    ///
    /// The last byte is the one that moves without a version bump. It is
    /// pinned on purpose: `supported_features` is wire-visible, so a
    /// change to [`FeatureFlags::ADVERTISED`] must be a deliberate edit
    /// here rather than something a peer discovers first. It is also the
    /// safe kind of change — a feature activates only on the intersection
    /// of the two advertisements, so a peer without the bit negotiates it
    /// away (docs/PROTOCOL.md §3.1).
    #[test]
    fn golden_wire_snapshot_v1() {
        let encoded = sample().encode_payload().unwrap();
        let expected: Vec<u8> = [
            &[0x02, 0x02][..],                   // versions 2, 2 (varints)
            &[0x10],                             // device_id: 16-byte length
            &[0x11; 16][..],                     // device_id bytes
            &[0x04, b'l', b'e', b'f', b't'][..], // device_name
            &[0x00],                             // OsFamily::Windows
            &[0x03],                             // FeatureFlags(CHUNKED_CLIPBOARD | FILE_CLIPBOARD)
        ]
        .concat();
        assert_eq!(
            encoded, expected,
            "Hello wire layout changed: bump the protocol version (ADR 0001)"
        );
    }

    /// The bits the snapshot above pins, stated as an invariant rather
    /// than a byte: bit 0 is `CHUNKED_CLIPBOARD`, advertised since ADR
    /// 0014's platform slice, and bit 1 is `FILE_CLIPBOARD`, advertised
    /// since ADR 0015's final slice (feature/136) now that both the
    /// receiving half (feature/126-132) and the sending half
    /// (feature/133-135) can honour the promise. This assertion is the one
    /// that would have to be edited — deliberately — if either promise
    /// ever had to be withdrawn.
    #[test]
    fn this_build_advertises_both_clipboard_feature_bits() {
        assert_eq!(FeatureFlags::ADVERTISED, FeatureFlags::ALL);
        assert!(FeatureFlags::ADVERTISED.contains(FeatureFlags::CHUNKED_CLIPBOARD));
        assert!(FeatureFlags::ADVERTISED.contains(FeatureFlags::FILE_CLIPBOARD));
        // And a peer that has never heard of either still gets nothing: the
        // intersection with an empty advertisement is empty.
        assert_eq!(
            FeatureFlags::negotiate(FeatureFlags::ADVERTISED, FeatureFlags::NONE),
            FeatureFlags::NONE
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

    /// One class per PROTOCOL.md §4's table, over every message type this
    /// build knows — total, and pinned so a new message type is a
    /// deliberate edit here rather than a silent gap.
    #[test]
    fn every_message_type_has_a_class() {
        let expectations = [
            (MessageType::Hello, MessageClass::Control),
            (MessageType::Ping, MessageClass::Control),
            (MessageType::Pong, MessageClass::Control),
            (MessageType::PairingStart, MessageClass::Control),
            (MessageType::PairingConfirm, MessageClass::Control),
            (MessageType::ClipboardOffer, MessageClass::Clipboard),
            (MessageType::ClipboardAccept, MessageClass::Clipboard),
            (MessageType::ClipboardDecline, MessageClass::Clipboard),
            (MessageType::ClipboardData, MessageClass::Clipboard),
            (MessageType::ClipboardApplied, MessageClass::Clipboard),
            (MessageType::ClipboardChunk, MessageClass::Clipboard),
            (MessageType::InputBatch, MessageClass::Input),
            (MessageType::ReleaseAllInput, MessageClass::Control),
            (MessageType::ControlRequest, MessageClass::Control),
            (MessageType::ControlResponse, MessageClass::Control),
            (MessageType::ControlRelease, MessageClass::Control),
            (MessageType::MonitorTopology, MessageClass::Control),
            (MessageType::LayoutSync, MessageClass::Control),
        ];
        assert_eq!(expectations.len(), 18, "a message type is missing here");
        for (ty, expected) in expectations {
            assert_eq!(ty.class(), expected, "{ty:?} classified wrong");
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
