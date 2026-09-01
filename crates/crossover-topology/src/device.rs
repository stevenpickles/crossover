//! The device identity a layout is expressed in terms of (ADR 0018).
//!
//! A layout names exactly two machines, and the ordering key that decides
//! which of two competing layouts wins is `(revision, origin)` — where
//! ADR 0018 says the origin "compares as its **16 raw bytes**". That
//! sentence is the whole specification of the type below: sixteen bytes,
//! ordered lexicographically, with a canonical text form for the files a
//! human reads.
//!
//! # Why a local newtype rather than `uuid::Uuid`
//!
//! The rest of the workspace carries this identity as a bare
//! `uuid::Uuid` — `Hello::device_id`, `TrustedPeer::peer_id`,
//! `EstablishedSession::peer_device_id`. This crate does not, because
//! ADR 0018 fixes its dependencies at "`serde`, `toml_edit`, and
//! `thiserror` and nothing else" so the layout editor can link it without
//! dragging a graph behind it. A sixteen-byte newtype needs no dependency
//! at all, and it says the ordering rule in its own `Ord` instead of
//! inheriting one.
//!
//! The conversion is total and lossless in both directions —
//! [`DeviceId::from_bytes`] against `Uuid::as_bytes`, [`DeviceId::to_bytes`]
//! against `Uuid::from_bytes` — and belongs at the boundary where a session
//! or a trust-store record meets a layout, not here. The text form is the
//! hyphenated one those `Uuid`s already print, so a device id copied out of
//! `crossover peers list` pastes into a config file unchanged.
//!
//! One consequence is worth stating rather than discovering: on the wire
//! this encodes as a **fixed 16-byte array with no length prefix**, where
//! `Uuid`'s own serde impl emits postcard's length-prefixed byte string
//! (`0x10` then the bytes). The two are not interchangeable in a golden
//! snapshot. Protocol v4 is a fresh encoding with no deployed peers
//! (ADR 0018), so the difference costs nothing — but a snapshot written for
//! one form will not match the other, and `Hello::device_id` keeps its
//! `Uuid` shape.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How many bytes a device identity is. Not a policy — the width of the
/// uuid the rest of the workspace already assigns each device.
pub const DEVICE_ID_BYTES: usize = 16;

/// The length of the canonical hyphenated text form, `8-4-4-4-12`.
const TEXT_LENGTH: usize = 36;

/// Where the hyphens sit in the canonical text form.
const HYPHEN_POSITIONS: [usize; 4] = [8, 13, 18, 23];

/// A machine's bookkeeping identity, as a layout addresses it (ADR 0018).
///
/// Ordering is over the raw bytes, which is what makes `(revision, origin)`
/// a deterministic tiebreak between two edits that independently claimed
/// the same revision.
///
/// This is **not** an authentication credential and never becomes one:
/// authorization is the SPKI fingerprint's job (ADR 0003). A layout naming
/// a device is a statement about which desk a rectangle sits on, checked
/// against the session's pair before it is believed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId([u8; DEVICE_ID_BYTES]);

impl DeviceId {
    /// The identity those sixteen bytes name.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; DEVICE_ID_BYTES]) -> Self {
        Self(bytes)
    }

    /// The sixteen bytes, borrowed — the form the ordering rule and any
    /// hash of a layout are defined over.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; DEVICE_ID_BYTES] {
        &self.0
    }

    /// The sixteen bytes, by value, for handing back to `Uuid::from_bytes`.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; DEVICE_ID_BYTES] {
        self.0
    }
}

/// Why a text device identity was refused.
///
/// One variant per rejection class, each carrying what was wrong rather
/// than a formatted message (docs/ARCHITECTURE.md §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeviceIdParseError {
    /// Not [`TEXT_LENGTH`] characters. Checked first, so nothing below
    /// walks an unbounded string.
    #[error("a device id is {TEXT_LENGTH} characters, got {length}")]
    WrongLength {
        /// The length that was offered, in bytes.
        length: usize,
    },
    /// A hyphen is missing from one of the four positions that separate
    /// the groups, or one appears where a hex digit belongs.
    #[error("a device id needs a hyphen at position {position}")]
    MisplacedHyphen {
        /// Which of the four separator positions was wrong.
        position: usize,
    },
    /// Something that is not a hexadecimal digit inside a group.
    #[error("a device id is hexadecimal; position {position} is not")]
    NotHexadecimal {
        /// Where the offending character sits.
        position: usize,
    },
}

impl FromStr for DeviceId {
    type Err = DeviceIdParseError;

    /// Parse the canonical hyphenated form, `8-4-4-4-12`, in either case.
    ///
    /// Deliberately the *only* accepted form. The braced and unhyphenated
    /// spellings `uuid` also accepts would give one identity several
    /// textual shapes in a file a human edits and a second machine reads
    /// back, which is a difference that only ever shows up as a layout
    /// that mysteriously does not match.
    ///
    /// # Errors
    ///
    /// [`DeviceIdParseError`], naming the fault and its position.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != TEXT_LENGTH {
            return Err(DeviceIdParseError::WrongLength { length: text.len() });
        }
        let mut bytes = [0u8; DEVICE_ID_BYTES];
        let mut nibbles: [u8; DEVICE_ID_BYTES * 2] = [0; DEVICE_ID_BYTES * 2];
        let mut filled = 0usize;
        for (position, character) in text.bytes().enumerate() {
            if HYPHEN_POSITIONS.contains(&position) {
                if character != b'-' {
                    return Err(DeviceIdParseError::MisplacedHyphen { position });
                }
                continue;
            }
            let Some(value) = hex_value(character) else {
                return Err(DeviceIdParseError::NotHexadecimal { position });
            };
            // `filled` cannot run past the array: the length check above
            // fixes exactly `DEVICE_ID_BYTES * 2` non-separator positions.
            nibbles[filled] = value;
            filled += 1;
        }
        for (index, pair) in nibbles.chunks_exact(2).enumerate() {
            bytes[index] = (pair[0] << 4) | pair[1];
        }
        Ok(Self(bytes))
    }
}

/// The value of one hexadecimal digit, in either case.
fn hex_value(character: u8) -> Option<u8> {
    match character {
        b'0'..=b'9' => Some(character - b'0'),
        b'a'..=b'f' => Some(character - b'a' + 10),
        b'A'..=b'F' => Some(character - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for DeviceId {
    /// The canonical hyphenated form, lowercase — byte for byte what
    /// `Uuid::to_string` produces for the same sixteen bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index == 4 || index == 6 || index == 8 || index == 10 {
                formatter.write_str("-")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for DeviceId {
    /// The text form, not sixteen loose integers: a diagnostic naming a
    /// device is only useful if it names it the way every other diagnostic
    /// in the system does.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "DeviceId({self})")
    }
}

impl Serialize for DeviceId {
    /// Text in a human-readable format (the config file, the state file),
    /// sixteen raw bytes otherwise (postcard, ADR 0001).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(self)
        } else {
            self.0.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for DeviceId {
    /// The mirror of [`Serialize`], and validating in both directions: a
    /// text form that is not a device id is a rejection, never a repair.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            // Owned rather than borrowed: a borrowed `&str` cannot be
            // produced by every deserializer (an escaped string, or a
            // reader-backed one), and failing there would make the format
            // decide whether a device id parses.
            let text = String::deserialize(deserializer)?;
            text.parse().map_err(serde::de::Error::custom)
        } else {
            <[u8; DEVICE_ID_BYTES]>::deserialize(deserializer).map(Self)
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{DEVICE_ID_BYTES, DeviceId, DeviceIdParseError, TEXT_LENGTH};

    /// The spelling `uuid` produces for these bytes, so a device id copied
    /// from `crossover peers list` parses here unchanged.
    const SAMPLE_TEXT: &str = "8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8";
    const SAMPLE_BYTES: [u8; DEVICE_ID_BYTES] = [
        0x8f, 0x8b, 0x1a, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5, 0xc6, 0xd7,
        0xe8,
    ];

    #[test]
    fn the_text_form_matches_the_hyphenated_uuid_spelling() {
        let device: DeviceId = SAMPLE_TEXT.parse().unwrap();
        assert_eq!(device.as_bytes(), &SAMPLE_BYTES);
        assert_eq!(device.to_string(), SAMPLE_TEXT);
        assert_eq!(DeviceId::from_bytes(SAMPLE_BYTES), device);
        // Uppercase parses to the same identity and prints back lowercase.
        let shouted: DeviceId = SAMPLE_TEXT.to_uppercase().parse().unwrap();
        assert_eq!(shouted, device);
        assert_eq!(shouted.to_string(), SAMPLE_TEXT);
        assert_eq!(format!("{device:?}"), format!("DeviceId({SAMPLE_TEXT})"));
    }

    #[test]
    fn malformed_text_is_refused_by_class() {
        assert_eq!(
            "".parse::<DeviceId>(),
            Err(DeviceIdParseError::WrongLength { length: 0 })
        );
        // The unhyphenated form `uuid` accepts is deliberately not a form
        // here: one identity, one spelling.
        let simple = SAMPLE_TEXT.replace('-', "");
        assert_eq!(
            simple.parse::<DeviceId>(),
            Err(DeviceIdParseError::WrongLength { length: 32 })
        );
        assert_eq!(
            format!("{{{SAMPLE_TEXT}}}").parse::<DeviceId>(),
            Err(DeviceIdParseError::WrongLength { length: 38 })
        );
        // A hyphen where a digit belongs, and a digit where a hyphen does.
        let mut swapped = SAMPLE_TEXT.to_owned();
        swapped.replace_range(8..9, "0");
        assert_eq!(
            swapped.parse::<DeviceId>(),
            Err(DeviceIdParseError::MisplacedHyphen { position: 8 })
        );
        let mut hyphenated = SAMPLE_TEXT.to_owned();
        hyphenated.replace_range(0..1, "-");
        assert_eq!(
            hyphenated.parse::<DeviceId>(),
            Err(DeviceIdParseError::NotHexadecimal { position: 0 })
        );
        let mut zed = SAMPLE_TEXT.to_owned();
        zed.replace_range(35..36, "z");
        assert_eq!(
            zed.parse::<DeviceId>(),
            Err(DeviceIdParseError::NotHexadecimal { position: 35 })
        );
    }

    /// ADR 0018's tiebreak: the origin "compares as its 16 raw bytes".
    #[test]
    fn ordering_is_over_the_raw_bytes() {
        let low = DeviceId::from_bytes([0x00; DEVICE_ID_BYTES]);
        let high = DeviceId::from_bytes([0xff; DEVICE_ID_BYTES]);
        assert!(low < high);

        // The first differing byte decides, whatever follows it.
        let mut a = [0u8; DEVICE_ID_BYTES];
        let mut b = [0u8; DEVICE_ID_BYTES];
        a[0] = 1;
        b[1] = 0xff;
        assert!(DeviceId::from_bytes(b) < DeviceId::from_bytes(a));
    }

    /// The human-readable/binary split, asserted through two real formats
    /// rather than by inspecting the impl.
    #[test]
    fn serde_is_text_when_human_readable_and_bytes_otherwise() {
        let device = DeviceId::from_bytes(SAMPLE_BYTES);
        let json = serde_json::to_string(&device).unwrap();
        assert_eq!(json, format!("\"{SAMPLE_TEXT}\""));
        assert_eq!(serde_json::from_str::<DeviceId>(&json).unwrap(), device);

        // A malformed text form is a deserialization error, not a panic
        // and not a silently repaired value.
        assert!(serde_json::from_str::<DeviceId>("\"not-a-device\"").is_err());
    }

    proptest! {
        /// Round-tripping is exact for every possible identity, in both
        /// the text and the JSON form.
        #[test]
        fn any_identity_round_trips_through_text_and_json(bytes in any::<[u8; DEVICE_ID_BYTES]>()) {
            let device = DeviceId::from_bytes(bytes);
            let text = device.to_string();
            prop_assert_eq!(text.len(), TEXT_LENGTH);
            prop_assert_eq!(text.parse::<DeviceId>().unwrap(), device);
            let json = serde_json::to_string(&device).unwrap();
            prop_assert_eq!(serde_json::from_str::<DeviceId>(&json).unwrap(), device);
            prop_assert_eq!(device.to_bytes(), bytes);
        }

        /// Arbitrary text is a value, never a panic — the parser sits
        /// behind a config file a peer's edit can reach (NFR-1).
        #[test]
        fn arbitrary_text_never_panics(text in ".{0,80}") {
            let verdict = text.parse::<DeviceId>();
            prop_assert_eq!(verdict, text.parse::<DeviceId>());
            if let Ok(device) = verdict {
                prop_assert_eq!(device.to_string().len(), TEXT_LENGTH);
            }
        }
    }
}
