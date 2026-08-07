//! Clipboard transaction messages (ADR 0005, docs/PROTOCOL.md §5).
//!
//! Two flows share these messages: inline (`Data` → `Applied`) for
//! content at or below [`CLIPBOARD_INLINE_MAX_BYTES`], and offered
//! (`Offer` → `Accept`/`Decline` → `Data` → `Applied`) above it. The
//! non-negotiable semantic lives in `Applied`: a sync succeeded only if
//! the destination OS clipboard was updated (FR-3.2).
//!
//! Wire-level validation is deliberately strong: a `ClipboardData` whose
//! declared length, hash, or UTF-8 validity disagrees with its content is
//! rejected at decode — corrupt items are unrepresentable past the
//! parser, so the engine's dedup and loop prevention can trust every item
//! identity they see.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::ProtocolError;
use crate::decode_strict;

/// SHA-256 of clipboard content — the identity that dedup and loop
/// prevention key on. Exposed so the engine hashes local observations
/// identically to the wire layer without its own crypto dependency.
#[must_use]
pub fn content_hash(content: &[u8]) -> [u8; 32] {
    Sha256::digest(content).into()
}

/// Content at or below this rides the inline flow; above it, the offered
/// flow (ADR 0005).
pub const CLIPBOARD_INLINE_MAX_BYTES: usize = 64 * 1024;

/// Hard cap on clipboard item content. Larger items are rejected
/// gracefully on both send and receive (FR-3.6) — never truncated.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Clipboard content types defined by protocol version 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentType {
    /// UTF-8 text (FR-3.7) — the only type in version 1.
    Utf8Text,
}

/// The identity and description of one clipboard item, shared by
/// [`ClipboardOffer`] and [`ClipboardData`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardMeta {
    /// Globally unique item id, minted by the origin at observation.
    pub id: Uuid,
    /// Origin peer's device id (bookkeeping and diagnostics; loop
    /// prevention keys on `content_hash`, never on this).
    pub origin: Uuid,
    /// Origin-local observation counter (conflict ordering, FR-3.5).
    pub sequence: u64,
    /// What the content is.
    pub content_type: ContentType,
    /// Exact content byte length. Validated against the actual content
    /// in `ClipboardData` and against bounds everywhere.
    pub content_length: u64,
    /// SHA-256 of the content (integrity, dedup, loop prevention).
    pub content_hash: [u8; 32],
}

impl ClipboardMeta {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.content_length > MAX_CLIPBOARD_TEXT_BYTES as u64 {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "clipboard content length {} exceeds maximum {MAX_CLIPBOARD_TEXT_BYTES}",
                    self.content_length
                ),
            });
        }
        Ok(())
    }
}

/// Announce a large item without its content: the receiver decides
/// whether the bytes should travel at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardOffer {
    /// The offered item.
    pub meta: ClipboardMeta,
}

impl ClipboardOffer {
    /// Semantic validation: offers exist only above the inline threshold
    /// (ADR 0005) — a conforming peer never offers what it should send.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for out-of-range lengths.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.meta.validate()?;
        if self.meta.content_length <= CLIPBOARD_INLINE_MAX_BYTES as u64 {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "offer for {} bytes at or below the {CLIPBOARD_INLINE_MAX_BYTES}-byte \
                     inline threshold",
                    self.meta.content_length
                ),
            });
        }
        Ok(())
    }

    /// Encode the payload, validating first.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] on serialization failure.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        encode(self)
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or invalid payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "ClipboardOffer")?;
        message.validate()?;
        Ok(message)
    }
}

/// Accept an offered item: send the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardAccept {
    /// The item being accepted.
    pub id: Uuid,
}

/// Why an offered item was declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclineReason {
    /// The receiver already holds content with this hash — a
    /// synchronization *success* with zero payload bytes moved.
    AlreadyHave,
    /// The receiver will not take an item this large.
    TooLarge,
    /// The receiver cannot take an item right now.
    NotReady,
    /// A newer item (by the deterministic conflict order, FR-3.5) has
    /// superseded this one; synchronization converges on the newer item.
    /// A success-shaped outcome, not a failure.
    Superseded,
}

/// Decline an offered item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardDecline {
    /// The item being declined.
    pub id: Uuid,
    /// Why — typed, so the origin can distinguish success-equivalent
    /// declines from failures (NFR-3).
    pub reason: DeclineReason,
}

/// The item itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardData {
    /// The item's identity — every field validated against `content`.
    pub meta: ClipboardMeta,
    /// The content bytes.
    pub content: Vec<u8>,
}

impl ClipboardData {
    /// Build a consistent `ClipboardData` from content, computing length
    /// and hash — the only way a conforming sender should construct one.
    #[must_use]
    pub fn from_content(
        id: Uuid,
        origin: Uuid,
        sequence: u64,
        content_type: ContentType,
        content: Vec<u8>,
    ) -> Self {
        let digest = content_hash(&content);
        Self {
            meta: ClipboardMeta {
                id,
                origin,
                sequence,
                content_type,
                content_length: content.len() as u64,
                content_hash: digest,
            },
            content,
        }
    }

    /// Full consistency validation: bounds, declared length == actual,
    /// hash matches content, and (for [`ContentType::Utf8Text`]) valid
    /// UTF-8.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for any inconsistency.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.meta.validate()?;
        if self.meta.content_length != self.content.len() as u64 {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "declared clipboard length {} but {} content bytes",
                    self.meta.content_length,
                    self.content.len()
                ),
            });
        }
        let digest: [u8; 32] = Sha256::digest(&self.content).into();
        if digest != self.meta.content_hash {
            return Err(ProtocolError::Malformed {
                reason: "clipboard content hash mismatch".to_owned(),
            });
        }
        match self.meta.content_type {
            ContentType::Utf8Text => {
                if std::str::from_utf8(&self.content).is_err() {
                    return Err(ProtocolError::Malformed {
                        reason: "Utf8Text content is not valid UTF-8".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Encode the payload, validating first.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] on serialization failure.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        encode(self)
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or inconsistent
    /// payloads — including hash and UTF-8 mismatches.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "ClipboardData")?;
        message.validate()?;
        Ok(message)
    }
}

/// The transaction verdict from the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyResult {
    /// The destination OS clipboard now holds the item (FR-3.2's only
    /// definition of success).
    Applied,
    /// The destination clipboard stayed unavailable through the bounded
    /// retry budget (FR-3.4).
    ClipboardUnavailable,
    /// The destination refused the content (validation failed locally).
    ContentRejected,
    /// A newer item (by the deterministic conflict order, FR-3.5) won the
    /// race; the destination kept the newer content. Closes the losing
    /// transaction as converged, not failed.
    Superseded,
}

/// Close a transaction: what happened at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardApplied {
    /// The item the verdict is about.
    pub id: Uuid,
    /// The verdict.
    pub result: ApplyResult,
}

macro_rules! plain_payload {
    ($ty:ty, $name:literal) => {
        impl $ty {
            /// Encode the payload.
            ///
            /// # Errors
            ///
            /// [`ProtocolError::Encode`] on serialization failure.
            pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
                encode(self)
            }

            /// Decode a payload (strict: no trailing bytes).
            ///
            /// # Errors
            ///
            /// [`ProtocolError::Malformed`] for undecodable payloads.
            pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
                decode_strict(payload, $name)
            }
        }
    };
}

plain_payload!(ClipboardAccept, "ClipboardAccept");
plain_payload!(ClipboardDecline, "ClipboardDecline");
plain_payload!(ClipboardApplied, "ClipboardApplied");

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_stdvec(value).map_err(|e| ProtocolError::Encode {
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{
        ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ClipboardAccept, ClipboardApplied, ClipboardData,
        ClipboardDecline, ClipboardMeta, ClipboardOffer, ContentType, DeclineReason,
        MAX_CLIPBOARD_TEXT_BYTES,
    };
    use crate::ProtocolError;

    fn data(content: &[u8]) -> ClipboardData {
        ClipboardData::from_content(
            Uuid::from_bytes([0x11; 16]),
            Uuid::from_bytes([0x22; 16]),
            7,
            ContentType::Utf8Text,
            content.to_vec(),
        )
    }

    #[test]
    fn data_round_trips_when_consistent() {
        let item = data(b"hello clipboard");
        let decoded = ClipboardData::decode_payload(&item.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, item);
    }

    #[test]
    fn tampered_content_hash_or_length_is_rejected_at_decode() {
        let item = data(b"integrity matters");

        let mut wrong_hash = item.clone();
        wrong_hash.meta.content_hash[0] ^= 0xFF;
        let bytes = super::encode(&wrong_hash).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));

        let mut wrong_len = item;
        wrong_len.meta.content_length += 1;
        let bytes = super::encode(&wrong_len).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn non_utf8_text_content_is_rejected() {
        let mut item = data(&[0xFF, 0xFE, 0xFD]);
        // from_content computed a correct hash over invalid UTF-8; both
        // encode and decode must refuse it.
        assert!(matches!(
            item.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        item.meta.content_type = ContentType::Utf8Text;
        let bytes = super::encode(&item).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn oversized_items_are_rejected_without_allocation_tricks() {
        // Craft a meta declaring over-limit length; content stays small so
        // the test is cheap — the length check must fire on the declared
        // value regardless.
        let mut item = data(b"small");
        item.meta.content_length = (MAX_CLIPBOARD_TEXT_BYTES as u64) + 1;
        let bytes = super::encode(&item).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn offers_below_the_inline_threshold_are_malformed() {
        let small = data(b"tiny");
        let offer = ClipboardOffer { meta: small.meta };
        assert!(matches!(
            offer.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        let mut big_meta = small.meta;
        big_meta.content_length = (CLIPBOARD_INLINE_MAX_BYTES as u64) + 1;
        let offer = ClipboardOffer { meta: big_meta };
        let decoded = ClipboardOffer::decode_payload(&offer.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, offer);
    }

    #[test]
    fn control_messages_round_trip() {
        let accept = ClipboardAccept {
            id: Uuid::from_bytes([0x33; 16]),
        };
        assert_eq!(
            ClipboardAccept::decode_payload(&accept.encode_payload().unwrap()).unwrap(),
            accept
        );

        for reason in [
            DeclineReason::AlreadyHave,
            DeclineReason::TooLarge,
            DeclineReason::NotReady,
            DeclineReason::Superseded,
        ] {
            let decline = ClipboardDecline {
                id: Uuid::from_bytes([0x44; 16]),
                reason,
            };
            assert_eq!(
                ClipboardDecline::decode_payload(&decline.encode_payload().unwrap()).unwrap(),
                decline
            );
        }

        for result in [
            ApplyResult::Applied,
            ApplyResult::ClipboardUnavailable,
            ApplyResult::ContentRejected,
            ApplyResult::Superseded,
        ] {
            let applied = ClipboardApplied {
                id: Uuid::from_bytes([0x55; 16]),
                result,
            };
            assert_eq!(
                ClipboardApplied::decode_payload(&applied.encode_payload().unwrap()).unwrap(),
                applied
            );
        }
    }

    /// Golden wire snapshots (ADR 0001): schema change = version bump.
    #[test]
    fn golden_wire_snapshots_v1() {
        let item = ClipboardData::from_content(
            Uuid::from_bytes([0x11; 16]),
            Uuid::from_bytes([0x22; 16]),
            7,
            ContentType::Utf8Text,
            b"hi".to_vec(),
        );
        let mut expected: Vec<u8> = Vec::new();
        expected.push(0x10); // id: 16-byte length prefix
        expected.extend([0x11; 16]); // id bytes
        expected.push(0x10); // origin: 16-byte length prefix
        expected.extend([0x22; 16]); // origin bytes
        expected.push(0x07); // sequence varint
        expected.push(0x00); // ContentType::Utf8Text
        expected.push(0x02); // content_length varint
        expected.extend(item.meta.content_hash); // hash: fixed 32, no prefix
        expected.extend([0x02, b'h', b'i']); // content: len-prefixed bytes
        assert_eq!(
            item.encode_payload().unwrap(),
            expected,
            "v1 ClipboardData wire layout changed: bump the protocol version"
        );

        let applied = ClipboardApplied {
            id: Uuid::from_bytes([0x55; 16]),
            result: ApplyResult::ClipboardUnavailable,
        };
        let mut expected: Vec<u8> = Vec::new();
        expected.push(0x10);
        expected.extend([0x55; 16]);
        expected.push(0x01); // ApplyResult::ClipboardUnavailable
        assert_eq!(
            applied.encode_payload().unwrap(),
            expected,
            "v1 ClipboardApplied wire layout changed: bump the protocol version"
        );
    }

    #[test]
    fn garbage_and_padded_payloads_are_malformed() {
        assert!(matches!(
            ClipboardData::decode_payload(&[0xFF; 30]),
            Err(ProtocolError::Malformed { .. })
        ));
        let good = data(b"ok").encode_payload().unwrap();
        let mut padded = good;
        padded.push(0x00);
        assert!(matches!(
            ClipboardData::decode_payload(&padded),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn meta_is_copy_and_cheap_to_pass_around() {
        // ClipboardMeta is the engine's working currency; keep it Copy.
        fn assert_copy<T: Copy>() {}
        assert_copy::<ClipboardMeta>();
    }
}
