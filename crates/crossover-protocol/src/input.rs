//! Input wire messages (docs/PROTOCOL.md §6, FR-4.1/4.2; ADR 0008).
//!
//! Pointer and keyboard events share one ordered stream — they must
//! interleave, or a chord like Shift+click loses its ordering. Two
//! delivery classes coexist in it, because the difference is semantic
//! rather than structural:
//!
//! - **Pointer motion and scroll may be coalesced.** Newer movement
//!   supersedes older, so under backpressure intermediate positions are
//!   dropped rather than queued (docs/SPECIFICATION.md §6.7: latency
//!   matters more than motion durability).
//! - **Button and key transitions are ordered and lossless.** A press
//!   that arrives after its release, or not at all, leaves the
//!   destination holding a button or key nobody is pressing — the defect
//!   class `ReleaseAllInput` exists to clean up and which must not be
//!   created casually.
//!
//! A batch therefore travels as an ordered sequence within one frame:
//! the sender coalesces before sending, and the receiver replays in
//! order, so ordering is preserved without a message per event. Key
//! identity is a USB HID usage id; produced text rides alongside, bounded
//! by [`MAX_KEY_TEXT_BYTES`] because it is network-influenced (ADR 0008,
//! NFR-1).

use serde::{Deserialize, Serialize};

use crate::ProtocolError;
use crate::decode_strict;

/// Maximum events in one [`InputBatch`].
///
/// Bounded like everything else (NFR-1). Generous for a coalesced batch:
/// at a 1 kHz mouse, 256 events is a quarter-second of un-coalesced
/// motion, far more than a healthy session should ever accumulate.
pub const MAX_INPUT_BATCH_EVENTS: usize = 256;

/// Scroll units per traditional detent, mirroring the core model.
pub const SCROLL_UNITS_PER_DETENT: i32 = 120;

/// Maximum bytes of produced text a single [`WireInputEvent::Key`] may
/// carry.
///
/// A keystroke normally produces one grapheme; this bounds even a
/// combining sequence, and — being network-influenced — is validated
/// before it is trusted (NFR-1). Dead-key composition and IME, which
/// could produce more, are out of Phase 4 scope (ADR 0008).
pub const MAX_KEY_TEXT_BYTES: usize = 32;

/// Pointer buttons as they travel. Wire values are explicit and never
/// reused; unknown values are rejected rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireButton {
    /// Primary.
    Left,
    /// Secondary.
    Right,
    /// Wheel button.
    Middle,
    /// First extended button.
    X1,
    /// Second extended button.
    X2,
}

/// One input event on the wire.
///
/// Not `Copy`: a [`WireInputEvent::Key`] may carry produced text. Both
/// button and key transitions are ordered and never coalesced (FR-4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireInputEvent {
    /// Relative pointer movement in device units (ADR 0007: Raw Input
    /// deltas, unaccelerated and unclamped).
    Motion {
        /// Rightward delta.
        dx: i32,
        /// Downward delta.
        dy: i32,
    },
    /// A button transition — ordered, never coalesced.
    Button {
        /// Which button.
        button: WireButton,
        /// True for press.
        pressed: bool,
    },
    /// Wheel movement in [`SCROLL_UNITS_PER_DETENT`] units.
    Scroll {
        /// Horizontal (positive: right).
        dx: i32,
        /// Vertical (positive: away from the user).
        dy: i32,
    },
    /// A keyboard transition (ADR 0008). Ordered and never coalesced —
    /// interleaved with pointer events in one stream so a chord like
    /// Shift+click keeps its ordering.
    Key {
        /// Physical key identity as a USB HID keyboard/keypad usage ID
        /// (Usage Page 0x07) — layout- and OS-independent.
        key: u16,
        /// True for press, false for release.
        pressed: bool,
        /// True for an OS-generated auto-repeat of a held key.
        repeat: bool,
        /// The Unicode text the source produced, if any — carried so
        /// mismatched layouts can be reproduced (ADR 0008). Bounded by
        /// [`MAX_KEY_TEXT_BYTES`]; `None` for keys that produce no text
        /// and for releases.
        text: Option<String>,
    },
}

/// An ordered batch of input events for the destination to replay.
///
/// `sequence` is per-session and strictly increasing: the destination
/// uses it to detect loss and reordering, which the spec requires
/// (docs/SPECIFICATION.md §28) and which matters far more for buttons
/// than for motion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputBatch {
    /// Sender's monotonic batch counter for this session.
    pub sequence: u64,
    /// Events in the order they occurred.
    pub events: Vec<WireInputEvent>,
}

impl InputBatch {
    /// Semantic validation, applied on encode and decode alike.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for an empty batch (nothing to
    /// replay is a sender bug worth surfacing) or one exceeding
    /// [`MAX_INPUT_BATCH_EVENTS`].
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.events.is_empty() {
            return Err(ProtocolError::Malformed {
                reason: "input batch carries no events".to_owned(),
            });
        }
        if self.events.len() > MAX_INPUT_BATCH_EVENTS {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "input batch of {} events exceeds maximum {MAX_INPUT_BATCH_EVENTS}",
                    self.events.len()
                ),
            });
        }
        for event in &self.events {
            if let WireInputEvent::Key {
                text: Some(text), ..
            } = event
                && text.len() > MAX_KEY_TEXT_BYTES
            {
                return Err(ProtocolError::Malformed {
                    reason: format!(
                        "key event text of {} bytes exceeds maximum {MAX_KEY_TEXT_BYTES}",
                        text.len()
                    ),
                });
            }
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
    /// [`ProtocolError::Malformed`] for undecodable, oversized, or empty
    /// batches.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let batch: Self = decode_strict(payload, "InputBatch")?;
        batch.validate()?;
        Ok(batch)
    }
}

/// Instruct the destination to release everything it believes is held
/// (FR-4.4).
///
/// Carries no state: the *destination* knows what it applied, and asking
/// it to release its own belief is more robust than sending a list that
/// could disagree with reality. Sent on control hand-back and on any
/// path where the session is ending cleanly enough to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAllInput {
    /// Sequence of the last batch this release follows, so the
    /// destination can order it against in-flight input.
    pub after_sequence: u64,
}

impl ReleaseAllInput {
    /// Encode the payload.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        decode_strict(payload, "ReleaseAllInput")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InputBatch, MAX_INPUT_BATCH_EVENTS, MAX_KEY_TEXT_BYTES, ReleaseAllInput, WireButton,
        WireInputEvent,
    };
    use crate::ProtocolError;

    fn key(key: u16, pressed: bool, repeat: bool, text: Option<&str>) -> WireInputEvent {
        WireInputEvent::Key {
            key,
            pressed,
            repeat,
            text: text.map(str::to_owned),
        }
    }

    fn batch(events: Vec<WireInputEvent>) -> InputBatch {
        InputBatch {
            sequence: 7,
            events,
        }
    }

    #[test]
    fn batches_round_trip() {
        let original = batch(vec![
            WireInputEvent::Motion { dx: 12, dy: -4 },
            WireInputEvent::Button {
                button: WireButton::Left,
                pressed: true,
            },
            WireInputEvent::Scroll { dx: 0, dy: 120 },
            WireInputEvent::Button {
                button: WireButton::Left,
                pressed: false,
            },
        ]);
        let decoded = InputBatch::decode_payload(&original.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, original);
        // Order is the contract: a replayed batch must be identical.
        assert_eq!(decoded.events, original.events);
    }

    #[test]
    fn negative_deltas_survive_the_wire() {
        // Motion is signed in both axes; a naive varint encoding would
        // bloat or mangle negatives, so assert them explicitly.
        let original = batch(vec![WireInputEvent::Motion {
            dx: i32::MIN,
            dy: i32::MAX,
        }]);
        let decoded = InputBatch::decode_payload(&original.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn empty_and_oversized_batches_are_rejected() {
        assert!(matches!(
            batch(vec![]).encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        let too_many = vec![WireInputEvent::Motion { dx: 1, dy: 1 }; MAX_INPUT_BATCH_EVENTS + 1];
        assert!(matches!(
            batch(too_many).encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        // The boundary itself is fine.
        let at_limit = vec![WireInputEvent::Motion { dx: 1, dy: 1 }; MAX_INPUT_BATCH_EVENTS];
        assert!(batch(at_limit).encode_payload().is_ok());
    }

    #[test]
    fn oversized_batches_are_rejected_at_decode_too() {
        // Built past the encoder's guard to prove the decoder enforces
        // the bound independently — a hostile peer will not use our
        // encoder.
        let hostile = InputBatch {
            sequence: 1,
            events: vec![WireInputEvent::Motion { dx: 1, dy: 1 }; MAX_INPUT_BATCH_EVENTS + 1],
        };
        let bytes = postcard::to_stdvec(&hostile).unwrap();
        assert!(matches!(
            InputBatch::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn release_all_round_trips() {
        let release = ReleaseAllInput { after_sequence: 42 };
        assert_eq!(
            ReleaseAllInput::decode_payload(&release.encode_payload().unwrap()).unwrap(),
            release
        );
    }

    /// Golden wire snapshots (ADR 0001): schema change = version bump.
    #[test]
    fn golden_wire_snapshots_v1() {
        let batch = InputBatch {
            sequence: 1,
            events: vec![
                WireInputEvent::Motion { dx: 1, dy: -1 },
                WireInputEvent::Button {
                    button: WireButton::Right,
                    pressed: true,
                },
            ],
        };
        let expected: Vec<u8> = vec![
            0x01, // sequence varint
            0x02, // event count
            0x00, // variant 0: Motion
            0x02, // dx = 1  (zigzag)
            0x01, // dy = -1 (zigzag)
            0x01, // variant 1: Button
            0x01, // WireButton::Right
            0x01, // pressed = true
        ];
        assert_eq!(
            batch.encode_payload().unwrap(),
            expected,
            "v1 InputBatch wire layout changed: bump the protocol version"
        );

        let release = ReleaseAllInput { after_sequence: 3 };
        assert_eq!(
            release.encode_payload().unwrap(),
            vec![0x03],
            "v1 ReleaseAllInput wire layout changed: bump the protocol version"
        );

        // The Key variant (ADR 0008) is wire variant 3. The Motion/Button
        // snapshot above is unchanged, proving the addition is backward
        // compatible; this pins Key's own layout.
        let key_batch = InputBatch {
            sequence: 1,
            events: vec![key(0x04, true, false, Some("a"))],
        };
        let expected_key: Vec<u8> = vec![
            0x01, // sequence varint
            0x01, // event count
            0x03, // variant 3: Key
            0x04, // key = 0x04 (HID usage for 'a')
            0x01, // pressed = true
            0x00, // repeat = false
            0x01, // text = Some
            0x01, // string length 1
            0x61, // 'a'
        ];
        assert_eq!(
            key_batch.encode_payload().unwrap(),
            expected_key,
            "v1 Key wire layout changed: bump the protocol version"
        );
    }

    #[test]
    fn key_events_round_trip_and_keep_order_among_pointer_events() {
        let original = batch(vec![
            key(0x04, true, false, Some("a")), // press with text
            key(0x04, true, true, Some("a")),  // auto-repeat
            key(0x04, false, false, None),     // release, no text
            WireInputEvent::Button {
                button: WireButton::Left,
                pressed: true,
            },
            key(0xE1, true, false, None), // Left Shift: usage 0xE1 needs a 2-byte varint
        ]);
        let decoded = InputBatch::decode_payload(&original.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, original);
        // Order is the contract — a chord must replay exactly.
        assert_eq!(decoded.events, original.events);
    }

    #[test]
    fn unicode_key_text_survives_the_wire() {
        let original = batch(vec![
            key(0x04, true, false, Some("é")),
            key(0x1D, true, false, Some("🎹")),
        ]);
        let decoded = InputBatch::decode_payload(&original.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn oversized_key_text_is_rejected_on_encode_and_decode() {
        let big = "x".repeat(MAX_KEY_TEXT_BYTES + 1);
        let bad = batch(vec![key(0x04, true, false, Some(&big))]);
        assert!(matches!(
            bad.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        // Built past the encoder's guard to prove the decoder enforces the
        // bound independently — a hostile peer will not use our encoder.
        let hostile = InputBatch {
            sequence: 1,
            events: vec![key(0x04, true, false, Some(&big))],
        };
        let bytes = postcard::to_stdvec(&hostile).unwrap();
        assert!(matches!(
            InputBatch::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));

        // The boundary length itself is fine.
        let at_limit = batch(vec![key(
            0x04,
            true,
            false,
            Some(&"x".repeat(MAX_KEY_TEXT_BYTES)),
        )]);
        assert!(at_limit.encode_payload().is_ok());
    }

    #[test]
    fn garbage_truncated_and_padded_payloads_are_malformed() {
        assert!(matches!(
            InputBatch::decode_payload(&[0xFF; 24]),
            Err(ProtocolError::Malformed { .. })
        ));

        let good = batch(vec![WireInputEvent::Motion { dx: 1, dy: 1 }])
            .encode_payload()
            .unwrap();
        assert!(matches!(
            InputBatch::decode_payload(&good[..good.len() - 1]),
            Err(ProtocolError::Malformed { .. })
        ));

        let mut padded = good;
        padded.push(0x00);
        assert!(matches!(
            InputBatch::decode_payload(&padded),
            Err(ProtocolError::Malformed { .. })
        ));
    }
}
