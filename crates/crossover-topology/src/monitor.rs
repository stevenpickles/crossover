//! Monitor identity: the platform's device string, validated (ADR 0018).
//!
//! A drawn layout has to survive a reboot, an unplugged screen, and a
//! re-enumeration, which is why a monitor is addressed by the
//! platform-supplied device string — `GetMonitorInfoW`'s `szDevice` on
//! Windows, `\\.\DISPLAY1` and friends — rather than by its position in an
//! enumeration. An index is positional: unplug a monitor and index 1
//! silently becomes a different screen, so a saved layout would be wrong in
//! the way that is hardest to see. A device string that changes leaves the
//! monitor simply *unknown*, which the editor can show and a diagnostic can
//! name.
//!
//! Two things follow from where this value comes from. It reaches this
//! machine over the wire inside `MonitorTopology` and `LayoutSync`, so it
//! is **network input**: bounded before anything is allocated, validated
//! before use, rejected rather than repaired (NFR-1, SECURITY.md invariant
//! 5). And it is displayed — in the editor, in logs, in the state file — so
//! it is held to printable ASCII, which no real device string exceeds and
//! which leaves no room for a control character or a bidi override to
//! misrepresent which screen a diagnostic is talking about.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum encoded length of a monitor id (ADR 0018).
///
/// Twice Windows' `CCHDEVICENAME` (32), so the longest `szDevice` fits with
/// headroom for the macOS and Linux device strings a later port will supply
/// (docs/platform-risks-linux.md).
pub const MAX_MONITOR_ID_BYTES: usize = 64;

/// A monitor's platform-supplied identity, validated on construction.
///
/// Unique within a machine — a rule [`crate::Layout`] enforces, because
/// uniqueness is a property of a set rather than of one string.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonitorId(String);

impl MonitorId {
    /// The identity `id` names.
    ///
    /// # Errors
    ///
    /// [`MonitorIdError`], one variant per rejection class.
    pub fn new(id: &str) -> Result<Self, MonitorIdError> {
        validate_monitor_id(id)?;
        Ok(Self(id.to_owned()))
    }

    /// The device string, as the platform reported it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a monitor id was refused.
///
/// The variants carry a length or a single byte value, never the string:
/// a device string is not a secret, but the house rule is that a rejection
/// names the fault (docs/SECURITY.md invariant 6, and the exemplar in
/// `crossover-protocol::file_name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MonitorIdError {
    /// Nothing at all. A monitor the layout can address has a name.
    #[error("the monitor id is empty")]
    Empty,
    /// Over [`MAX_MONITOR_ID_BYTES`] once encoded. Checked before the
    /// character scan, so the scan is bounded by that constant.
    #[error("the monitor id is {bytes} bytes, over the {MAX_MONITOR_ID_BYTES}-byte maximum")]
    TooManyBytes {
        /// Encoded length that was offered.
        bytes: usize,
    },
    /// Something outside printable ASCII (`0x20..=0x7E`) — a control
    /// character, a non-ASCII byte, or anything else that would not render
    /// as itself in the editor or a log line.
    #[error("the monitor id contains the non-printable-ASCII byte 0x{byte:02X}")]
    NotPrintableAscii {
        /// The offending byte.
        byte: u8,
    },
}

/// Validate a monitor id (ADR 0018).
///
/// Pure and total: every input is a value, nothing allocates, and the
/// bounded check runs before the scan.
///
/// A conforming id is 1..=[`MAX_MONITOR_ID_BYTES`] bytes, every one of them
/// printable ASCII (`0x20` space through `0x7E` tilde).
///
/// # Errors
///
/// [`MonitorIdError`], naming what was wrong without quoting the id.
pub fn validate_monitor_id(id: &str) -> Result<(), MonitorIdError> {
    if id.is_empty() {
        return Err(MonitorIdError::Empty);
    }
    // The bound first: it is what says how far the scan below can go.
    if id.len() > MAX_MONITOR_ID_BYTES {
        return Err(MonitorIdError::TooManyBytes { bytes: id.len() });
    }
    if let Some(&byte) = id
        .as_bytes()
        .iter()
        .find(|&&b| !b.is_ascii_graphic() && b != b' ')
    {
        return Err(MonitorIdError::NotPrintableAscii { byte });
    }
    Ok(())
}

impl FromStr for MonitorId {
    type Err = MonitorIdError;

    /// # Errors
    ///
    /// [`MonitorIdError`], as [`MonitorId::new`].
    fn from_str(id: &str) -> Result<Self, Self::Err> {
        Self::new(id)
    }
}

impl fmt::Display for MonitorId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for MonitorId {
    /// Quoted, so a device string full of backslashes reads as one value
    /// in a diagnostic rather than as escaping noise.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MonitorId({:?})", self.0)
    }
}

impl Serialize for MonitorId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MonitorId {
    /// Validating, in every format. This is the property that makes the
    /// type worth having: an id cannot exist unvalidated, so no decoder —
    /// wire, config, or state file — can introduce one by forgetting to
    /// check.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        validate_monitor_id(&id).map_err(serde::de::Error::custom)?;
        Ok(Self(id))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{MAX_MONITOR_ID_BYTES, MonitorId, MonitorIdError, validate_monitor_id};

    #[test]
    fn real_device_strings_are_accepted() {
        for id in [
            r"\\.\DISPLAY1",
            r"\\.\DISPLAY12",
            "HDMI-A-1",   // the Linux/DRM connector spelling
            "1F0E2A3B",   // a macOS display id printed as hex
            "a",          // the shortest possible id
            "screen one", // space is printable, and some platforms use it
            "~!@#$%^&*()_+={}[]|;:'\",.<>/?`",
        ] {
            assert!(
                MonitorId::new(id).is_ok(),
                "a legitimate device string was refused: {id:?}"
            );
        }
    }

    /// The exact byte boundary in both directions — the edge a length
    /// check is most often written one off from.
    #[test]
    fn sixty_four_bytes_is_accepted_and_sixty_five_is_not() {
        let at_the_cap = "x".repeat(MAX_MONITOR_ID_BYTES);
        assert!(MonitorId::new(&at_the_cap).is_ok());

        let one_over = "x".repeat(MAX_MONITOR_ID_BYTES + 1);
        assert_eq!(
            validate_monitor_id(&one_over),
            Err(MonitorIdError::TooManyBytes {
                bytes: MAX_MONITOR_ID_BYTES + 1
            })
        );
    }

    #[test]
    fn empty_is_refused() {
        assert_eq!(validate_monitor_id(""), Err(MonitorIdError::Empty));
    }

    /// The bound is on **bytes**, not characters, because that is what the
    /// wire and the file carry — so a multi-byte id can be well under 64
    /// characters and still over the cap.
    #[test]
    fn the_bound_counts_bytes_not_characters() {
        // 17 characters, 68 bytes: over the cap on the axis that matters.
        let wide = "\u{1F5A5}".repeat(17);
        assert_eq!(wide.chars().count(), 17);
        assert!(wide.len() > MAX_MONITOR_ID_BYTES);
        // Non-ASCII is refused anyway, but the length check comes first,
        // so it is the length that is reported.
        assert_eq!(
            validate_monitor_id(&wide),
            Err(MonitorIdError::TooManyBytes { bytes: wide.len() })
        );
    }

    #[test]
    fn non_printable_and_non_ascii_bytes_are_refused() {
        // Control characters, including the ones a log line would obey.
        for control in ['\u{0}', '\n', '\r', '\t', '\u{7F}', '\u{1B}'] {
            assert_eq!(
                validate_monitor_id(&format!("DISPLAY{control}1")),
                Err(MonitorIdError::NotPrintableAscii {
                    byte: u8::try_from(u32::from(control)).unwrap()
                }),
                "a control character was admitted: U+{:04X}",
                u32::from(control)
            );
        }

        // Non-ASCII, reported as its first offending byte. `é` is
        // U+00E9, encoded 0xC3 0xA9.
        assert_eq!(
            validate_monitor_id("DISPLAY\u{E9}"),
            Err(MonitorIdError::NotPrintableAscii { byte: 0xC3 })
        );
        // A bidi override, which would lie about how the id renders.
        assert!(matches!(
            validate_monitor_id("DISPLAY\u{202E}1"),
            Err(MonitorIdError::NotPrintableAscii { .. })
        ));
    }

    #[test]
    fn deserialization_validates_rather_than_trusting_the_format() {
        let good: MonitorId = serde_json::from_str(r#""\\\\.\\DISPLAY1""#).unwrap();
        assert_eq!(good.as_str(), r"\\.\DISPLAY1");
        assert_eq!(
            serde_json::to_string(&good).unwrap(),
            r#""\\\\.\\DISPLAY1""#
        );

        assert!(serde_json::from_str::<MonitorId>(r#""""#).is_err());
        assert!(
            serde_json::from_str::<MonitorId>(&format!("\"{}\"", "x".repeat(65))).is_err(),
            "an over-long id must not survive a decoder"
        );
        assert!(serde_json::from_str::<MonitorId>(r#""bad\u0000id""#).is_err());
    }

    #[test]
    fn a_refusal_names_the_fault_and_never_the_id() {
        let error = validate_monitor_id("secret-monitor\u{7}").unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("0x07"), "{rendered}");
    }

    proptest! {
        /// Arbitrary text is a value, never a panic, and the verdict is a
        /// pure function of the input.
        #[test]
        fn arbitrary_ids_never_panic(id in ".{0,200}") {
            let verdict = validate_monitor_id(&id);
            prop_assert_eq!(verdict, validate_monitor_id(&id));
            if verdict.is_ok() {
                prop_assert!(!id.is_empty());
                prop_assert!(id.len() <= MAX_MONITOR_ID_BYTES);
                prop_assert!(id.bytes().all(|b| (0x20..=0x7E).contains(&b)));
                let accepted = MonitorId::new(&id).unwrap();
                prop_assert_eq!(accepted.as_str(), id.as_str());
            }
        }

        /// Arbitrary code points, astral planes included: an accepted id
        /// is printable ASCII throughout.
        #[test]
        fn accepted_ids_are_printable_ascii(
            characters in proptest::collection::vec(any::<char>(), 1..40),
        ) {
            let id: String = characters.into_iter().collect();
            if validate_monitor_id(&id).is_ok() {
                prop_assert!(id.is_ascii());
                prop_assert!(!id.chars().any(char::is_control));
            }
        }
    }
}
