//! Peer-supplied file names (ADR 0015) — the one string of a file
//! transfer that reaches the destination shell.
//!
//! A received file is spooled under a **locally generated** name; the
//! peer's name never becomes a filesystem name on this machine. It is
//! used in exactly one place: `FILEDESCRIPTORW.cFileName`, which is what
//! Explorer uses to name the file it creates in whatever folder the user
//! pastes into. That is enough to make it a security boundary, so it is
//! validated here — as network input, at the parser, before any message
//! carrying it is considered well-formed.
//!
//! Two properties are deliberate:
//!
//! - **Reject, never repair.** A name that fails any rule below is a
//!   typed refusal. Nothing is stripped, escaped, or renamed into
//!   something safe: we do not guess what the sender meant, and a
//!   repaired name is a name the user never chose being written by a
//!   shell that trusts us.
//! - **The error names the fault, never the name.** A file name is user
//!   data (docs/SECURITY.md invariant 6), and a rejection is logged. The
//!   variants below carry a category, a length, or a single offending
//!   code point — never the string.
//!
//! What the list is *not* is an anti-malware filter (ADR 0015 says so
//! plainly): `report.pdf.exe` and Cyrillic homoglyphs are valid names no
//! validator can refuse without refusing real ones, and they are
//! contained downstream by Mark-of-the-Web instead. This is "the names
//! that would break a *path*, plus the ones that lie about their own
//! rendering".

use core::cmp::Ordering;

use crate::ProtocolError;

/// Maximum encoded length of a file name (ADR 0015).
///
/// 255 is NTFS's per-component limit, so a name that could not be a file
/// name anywhere never enters the system. Checked at decode and again
/// before a descriptor is built for the shell.
pub const MAX_FILE_NAME_BYTES: usize = 255;

/// Maximum length of a file name in UTF-16 code units (ADR 0015).
///
/// The bound that matters downstream: `FILEDESCRIPTORW.cFileName` is
/// `WCHAR[260]`, so 259 units plus the NUL is its exact capacity. Today
/// [`MAX_FILE_NAME_BYTES`] already implies this one — 255 UTF-8 bytes
/// encode at most 255 UTF-16 units — but both are checked and tested
/// separately so that raising either cap cannot silently overrun a
/// fixed-size Win32 buffer.
pub const MAX_FILE_NAME_UTF16_UNITS: usize = 259;

/// Characters that would make the name something other than a bare name:
/// the Win32 reserved set. `/` and `\` are reported as
/// [`FileNameError::PathSeparator`] instead, because "this is a path"
/// is the more useful diagnostic.
const RESERVED_CHARACTERS: [char; 7] = [':', '*', '?', '"', '<', '>', '|'];

/// Windows device names, which resolve to a device rather than a file
/// wherever they appear — with or without an extension, in any case
/// (ADR 0015).
const RESERVED_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Unicode general category `Cf` (format), as inclusive code-point
/// ranges, sorted and non-overlapping (asserted in the tests).
///
/// `Cc` alone — which is all `char::is_control` tests — is the classic
/// miss: `U+202E RIGHT-TO-LEFT OVERRIDE` is `Cf`, so a control-only rule
/// admits `invoice\u{202E}gnp.exe`, which a shell renders as
/// `invoiceexe.png`. Rejecting the whole category covers the bidi
/// overrides and isolates plus the zero-width and annotation characters,
/// at the cost of the occasional legitimate name containing a joiner —
/// the trade ADR 0015 makes explicitly.
///
/// This table is a **pinned snapshot** of the category (Unicode 16.0),
/// not a live query: the crate has no Unicode-table dependency, and
/// adding one to the workspace's most security-critical, deliberately
/// dependency-light crate is a supply-chain cost the ADR did not weigh.
/// The consequence is stated rather than hidden: a *future* addition to
/// `Cf` would be admitted until this table is updated. Every format
/// character that exists today, including all of the bidi controls the
/// ADR names, is here.
const FORMAT_CHARACTER_RANGES: [(u32, u32); 21] = [
    (0x00AD, 0x00AD),     // SOFT HYPHEN
    (0x0600, 0x0605),     // Arabic number signs
    (0x061C, 0x061C),     // ARABIC LETTER MARK
    (0x06DD, 0x06DD),     // ARABIC END OF AYAH
    (0x070F, 0x070F),     // SYRIAC ABBREVIATION MARK
    (0x0890, 0x0891),     // Arabic pound/piastre marks
    (0x08E2, 0x08E2),     // ARABIC DISPUTED END OF AYAH
    (0x180E, 0x180E),     // MONGOLIAN VOWEL SEPARATOR
    (0x200B, 0x200F),     // ZWSP, ZWNJ, ZWJ, LRM, RLM
    (0x202A, 0x202E),     // LRE, RLE, PDF, LRO, RLO
    (0x2060, 0x2064),     // word joiner, invisible operators
    (0x2066, 0x206F),     // LRI, RLI, FSI, PDI, deprecated format controls
    (0xFEFF, 0xFEFF),     // ZERO WIDTH NO-BREAK SPACE (BOM)
    (0xFFF9, 0xFFFB),     // interlinear annotation
    (0x1_10BD, 0x1_10BD), // KAITHI NUMBER SIGN
    (0x1_10CD, 0x1_10CD), // KAITHI NUMBER SIGN ABOVE
    (0x1_3430, 0x1_343F), // Egyptian hieroglyph format controls
    (0x1_BCA0, 0x1_BCA3), // shorthand format controls
    (0x1_D173, 0x1_D17A), // musical symbol beams and slurs
    (0xE_0001, 0xE_0001), // LANGUAGE TAG
    (0xE_0020, 0xE_007F), // tag characters
];

/// Why a peer-supplied file name was refused.
///
/// One variant per rejection class, so a diagnostic can say what was
/// wrong (NFR-3, FR-7.1) without quoting user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FileNameError {
    /// Nothing at all. A file has a name.
    #[error("the file name is empty")]
    Empty,
    /// Over [`MAX_FILE_NAME_BYTES`] once encoded.
    #[error("the file name is {bytes} bytes, over the {MAX_FILE_NAME_BYTES}-byte maximum")]
    TooManyBytes {
        /// Encoded length that was offered.
        bytes: usize,
    },
    /// Over [`MAX_FILE_NAME_UTF16_UNITS`] once encoded as UTF-16 — the
    /// units `FILEDESCRIPTORW.cFileName` is measured in.
    #[error(
        "the file name is {units} UTF-16 units, over the {MAX_FILE_NAME_UTF16_UNITS}-unit maximum"
    )]
    TooManyUtf16Units {
        /// UTF-16 length that was offered.
        units: usize,
    },
    /// A leading `\\` or `//`: a UNC path, which names a host.
    #[error("the file name starts with a UNC prefix")]
    UncPrefix,
    /// A leading separator: an absolute path, not a bare name.
    #[error("the file name is an absolute path")]
    AbsolutePath,
    /// A `X:` prefix: a drive-relative or drive-absolute path.
    #[error("the file name starts with a drive letter")]
    DriveLetter,
    /// The name is `.` or `..` — a directory, and in the second case the
    /// parent one.
    #[error("the file name is a directory traversal component")]
    Traversal,
    /// A `/` or `\` anywhere: the wire carries a bare name, never a path,
    /// so the sender's directory layout is not disclosed and the receiver
    /// has nothing to resolve.
    #[error("the file name contains a path separator")]
    PathSeparator,
    /// One of `: * ? " < > |`.
    #[error("the file name contains the reserved character {character:?}")]
    ReservedCharacter {
        /// The offending character — a fixed member of a known set, so
        /// naming it discloses nothing about the name.
        character: char,
    },
    /// Unicode general category `Cc`, including NUL.
    #[error("the file name contains control character U+{code_point:04X}")]
    ControlCharacter {
        /// The offending code point.
        code_point: u32,
    },
    /// Unicode general category `Cf` — the bidi overrides and their
    /// relatives, which lie about how the name renders.
    #[error("the file name contains format character U+{code_point:04X}")]
    FormatCharacter {
        /// The offending code point.
        code_point: u32,
    },
    /// A trailing space or dot. Windows strips them silently, so the name
    /// that appears is not the name that was checked — a name-confusion
    /// vector rather than a naming mistake.
    #[error("the file name ends in a space or a dot")]
    TrailingDotOrSpace,
    /// A Windows reserved device name, with or without an extension.
    #[error("the file name is the reserved device name {device}")]
    ReservedDeviceName {
        /// Which device it collides with, from our own table.
        device: &'static str,
    },
}

impl From<FileNameError> for ProtocolError {
    /// A refused name is a malformed message: the name is a field of a
    /// wire message, and an invalid one makes the message unrepresentable
    /// past the parser exactly as a bad hash or bad UTF-8 already is.
    ///
    /// The reason carries the *fault*, never the name.
    fn from(error: FileNameError) -> Self {
        Self::Malformed {
            reason: format!("invalid file name: {error}"),
        }
    }
}

/// Whether `character` is in Unicode general category `Cf`, per the
/// pinned table above.
fn is_format_character(character: char) -> bool {
    let code_point = u32::from(character);
    // Every entry is above U+00AD, and names are overwhelmingly ASCII.
    if code_point < 0x00AD {
        return false;
    }
    FORMAT_CHARACTER_RANGES
        .binary_search_by(|&(low, high)| {
            if code_point < low {
                Ordering::Greater
            } else if code_point > high {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// Validate a peer-supplied file name (ADR 0015).
///
/// Pure and total: every input is a value, no input allocates, and the
/// bounded checks run before the unbounded-looking ones — length first,
/// so a scan is only ever over at most [`MAX_FILE_NAME_BYTES`] bytes.
///
/// A conforming name is valid UTF-8 (guaranteed by the time it is a
/// `&str`), 1..=[`MAX_FILE_NAME_BYTES`] bytes and
/// 1..=[`MAX_FILE_NAME_UTF16_UNITS`] UTF-16 units, carries no path
/// syntax (separator, drive letter, UNC prefix, `.`/`..`), contains no
/// character of Unicode category `Cc` or `Cf` and none of `: * ? " < >
/// |`, does not end in a space or a dot, and is not a Windows reserved
/// device name.
///
/// # Errors
///
/// [`FileNameError`], one variant per rejection class, naming what was
/// wrong without quoting the name.
pub fn validate_file_name(name: &str) -> Result<(), FileNameError> {
    if name.is_empty() {
        return Err(FileNameError::Empty);
    }
    // Bounds first: everything below walks the string, and this is what
    // says how far that walk can go.
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err(FileNameError::TooManyBytes { bytes: name.len() });
    }
    let utf16_units: usize = name.chars().map(char::len_utf16).sum();
    if utf16_units > MAX_FILE_NAME_UTF16_UNITS {
        return Err(FileNameError::TooManyUtf16Units { units: utf16_units });
    }

    // Path syntax, most specific first, so the diagnostic names the
    // shape rather than the character that happened to come first.
    if name.starts_with("\\\\") || name.starts_with("//") {
        return Err(FileNameError::UncPrefix);
    }
    if name.starts_with('\\') || name.starts_with('/') {
        return Err(FileNameError::AbsolutePath);
    }
    let mut bytes = name.bytes();
    if let (Some(first), Some(b':')) = (bytes.next(), bytes.next())
        && first.is_ascii_alphabetic()
    {
        return Err(FileNameError::DriveLetter);
    }
    // `.` and `..` are the only traversal *components* reachable here:
    // separators are refused below, so the whole name is one component,
    // and `a..b` is an ordinary file name rather than a path.
    if name == "." || name == ".." {
        return Err(FileNameError::Traversal);
    }

    for character in name.chars() {
        if character == '/' || character == '\\' {
            return Err(FileNameError::PathSeparator);
        }
        if character.is_control() {
            return Err(FileNameError::ControlCharacter {
                code_point: u32::from(character),
            });
        }
        if is_format_character(character) {
            return Err(FileNameError::FormatCharacter {
                code_point: u32::from(character),
            });
        }
        if RESERVED_CHARACTERS.contains(&character) {
            return Err(FileNameError::ReservedCharacter { character });
        }
    }

    if name.ends_with('.') || name.ends_with(' ') {
        return Err(FileNameError::TrailingDotOrSpace);
    }

    // A device name resolves to a device wherever it appears, extension
    // or not: `CON`, `CON.txt` and `con` are the same thing to Win32.
    let stem = name.split('.').next().unwrap_or(name);
    if let Some(device) = RESERVED_DEVICE_NAMES
        .iter()
        .copied()
        .find(|device| stem.eq_ignore_ascii_case(device))
    {
        return Err(FileNameError::ReservedDeviceName { device });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        FORMAT_CHARACTER_RANGES, FileNameError, MAX_FILE_NAME_BYTES, MAX_FILE_NAME_UTF16_UNITS,
        validate_file_name,
    };
    use crate::ProtocolError;

    #[test]
    fn ordinary_names_are_accepted() {
        for name in [
            "report.pdf",
            "holiday photos.zip",
            "a",
            "..leading dots are fine",
            "a..b",
            "Ünïcödé — naïve.txt",
            "日本語のファイル.txt",
            "COM0.txt",   // not a reserved device: only COM1..=COM9 are
            "LPT10.txt",  // ditto
            "CONTACT.md", // a longer name that merely starts like CON
            "not.con",    // the device check is on the stem
        ] {
            assert!(
                validate_file_name(name).is_ok(),
                "a legitimate name was refused: {name:?}"
            );
        }
        // The boundary itself is a legitimate name.
        assert!(validate_file_name(&"x".repeat(MAX_FILE_NAME_BYTES)).is_ok());
    }

    #[test]
    fn empty_and_oversized_names_are_refused() {
        assert_eq!(validate_file_name(""), Err(FileNameError::Empty));

        let too_long = "x".repeat(MAX_FILE_NAME_BYTES + 1);
        assert_eq!(
            validate_file_name(&too_long),
            Err(FileNameError::TooManyBytes {
                bytes: MAX_FILE_NAME_BYTES + 1
            })
        );

        // The UTF-16 bound is checked independently of the byte bound, so
        // that raising either cap cannot silently overrun `WCHAR[260]`.
        // Today it is unreachable through the byte bound — every UTF-8
        // byte is at most one UTF-16 unit — which is exactly what this
        // asserts.
        let widest: String = "\u{10000}".repeat(MAX_FILE_NAME_BYTES / 4);
        let units: usize = widest.chars().map(char::len_utf16).sum();
        assert!(
            units <= MAX_FILE_NAME_UTF16_UNITS,
            "the byte bound must imply the UTF-16 bound: {units} units"
        );
    }

    #[test]
    fn path_syntax_is_refused_by_shape() {
        for (name, expected) in [
            ("\\\\server\\share", FileNameError::UncPrefix),
            ("//server/share", FileNameError::UncPrefix),
            ("\\windows", FileNameError::AbsolutePath),
            ("/etc/passwd", FileNameError::AbsolutePath),
            (
                "C:\\Windows\\System32\\evil.dll",
                FileNameError::DriveLetter,
            ),
            ("c:relative.txt", FileNameError::DriveLetter),
            (".", FileNameError::Traversal),
            ("..", FileNameError::Traversal),
            ("../../etc/passwd", FileNameError::PathSeparator),
            ("..\\..\\windows\\system32", FileNameError::PathSeparator),
            ("subdir/file.txt", FileNameError::PathSeparator),
            ("subdir\\file.txt", FileNameError::PathSeparator),
        ] {
            assert_eq!(
                validate_file_name(name),
                Err(expected),
                "wrong rejection class for {name:?}"
            );
        }
    }

    #[test]
    fn reserved_characters_are_refused() {
        for (name, character) in [
            ("stream.txt:hidden", ':'),
            ("wild*card", '*'),
            ("what?", '?'),
            ("quo\"te", '"'),
            ("less<than", '<'),
            ("greater>than", '>'),
            ("pipe|d", '|'),
        ] {
            assert_eq!(
                validate_file_name(name),
                Err(FileNameError::ReservedCharacter { character }),
                "wrong rejection class for {name:?}"
            );
        }
    }

    /// `Cc` and `Cf` both, because `Cc` alone is the classic miss and
    /// `U+202E` is the reason ADR 0015 says so explicitly.
    #[test]
    fn control_and_format_characters_are_refused() {
        for control in ['\u{0}', '\u{1}', '\n', '\r', '\t', '\u{7F}', '\u{85}'] {
            assert_eq!(
                validate_file_name(&format!("na{control}me.txt")),
                Err(FileNameError::ControlCharacter {
                    code_point: u32::from(control)
                }),
                "a control character was admitted: U+{:04X}",
                u32::from(control)
            );
        }

        for format in [
            '\u{202E}', // RIGHT-TO-LEFT OVERRIDE — invoice⁠<RLO>gnp.exe
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{200E}',
            '\u{200F}',
            '\u{2066}',
            '\u{2069}',
            '\u{200B}',
            '\u{00AD}',
            '\u{FEFF}',
            '\u{E0001}',
            '\u{E0041}',
        ] {
            assert_eq!(
                validate_file_name(&format!("invoice{format}gnp.exe")),
                Err(FileNameError::FormatCharacter {
                    code_point: u32::from(format)
                }),
                "a format character was admitted: U+{:04X}",
                u32::from(format)
            );
        }

        // The specific name ADR 0015 names, spelled out.
        assert!(validate_file_name("invoice\u{202E}gnp.exe").is_err());
    }

    #[test]
    fn trailing_dots_and_spaces_are_refused() {
        for name in ["report.pdf.", "report.pdf ", "trailing...", "a "] {
            assert_eq!(
                validate_file_name(name),
                Err(FileNameError::TrailingDotOrSpace),
                "wrong rejection class for {name:?}"
            );
        }
    }

    #[test]
    fn reserved_device_names_are_refused_in_any_case_with_any_extension() {
        for (name, device) in [
            ("CON", "CON"),
            ("con", "CON"),
            ("Con.txt", "CON"),
            ("NUL", "NUL"),
            ("nul.log.txt", "NUL"),
            ("PRN", "PRN"),
            ("AUX", "AUX"),
            ("com1", "COM1"),
            ("COM9.dat", "COM9"),
            ("lpt1.txt", "LPT1"),
            ("LPT9", "LPT9"),
        ] {
            assert_eq!(
                validate_file_name(name),
                Err(FileNameError::ReservedDeviceName { device }),
                "wrong rejection class for {name:?}"
            );
        }
    }

    /// Rejection is a typed decline, never a repair: there is no API that
    /// returns a *fixed* name, and the error is the only outcome.
    #[test]
    fn a_refusal_carries_the_fault_and_never_the_name() {
        let secret = "quarterly-layoffs\u{202E}.txt";
        let error = validate_file_name(secret).unwrap_err();
        let rendered = error.to_string();
        assert!(
            !rendered.contains("quarterly"),
            "a rejection must not quote the name: {rendered}"
        );

        let protocol: ProtocolError = error.into();
        let ProtocolError::Malformed { reason } = protocol else {
            panic!("a bad name is a malformed message");
        };
        assert!(!reason.contains("quarterly"), "{reason}");
        assert!(reason.contains("file name"), "{reason}");
    }

    #[test]
    fn the_format_character_table_is_sorted_and_disjoint() {
        let mut previous_high = 0u32;
        for (index, &(low, high)) in FORMAT_CHARACTER_RANGES.iter().enumerate() {
            assert!(low <= high, "inverted range at {index}");
            if index > 0 {
                assert!(
                    low > previous_high,
                    "unsorted or overlapping range at {index}"
                );
            }
            previous_high = high;
        }
    }

    proptest! {
        /// No input panics, and validation is a pure predicate: the same
        /// name always gets the same verdict, and an accepted name is
        /// still accepted after a round trip through UTF-8 bytes (the
        /// form it arrives in).
        #[test]
        fn arbitrary_names_never_panic(name in ".{0,300}") {
            let verdict = validate_file_name(&name);
            prop_assert_eq!(verdict, validate_file_name(&name));
            if verdict.is_ok() {
                let bytes = name.as_bytes().to_vec();
                let again = String::from_utf8(bytes).unwrap();
                prop_assert!(validate_file_name(&again).is_ok());
                // Anything accepted is inside both bounds, which is what
                // the Win32 buffer downstream depends on.
                prop_assert!(name.len() <= MAX_FILE_NAME_BYTES);
                let units: usize = name.chars().map(char::len_utf16).sum();
                prop_assert!(units <= MAX_FILE_NAME_UTF16_UNITS);
            }
        }

        /// Arbitrary code points, including the whole `Cc`/`Cf` space and
        /// the astral planes: an accepted name never contains one.
        #[test]
        fn accepted_names_carry_no_control_or_format_characters(
            characters in proptest::collection::vec(any::<char>(), 1..40),
        ) {
            let name: String = characters.into_iter().collect();
            if validate_file_name(&name).is_ok() {
                prop_assert!(!name.chars().any(char::is_control));
                prop_assert!(!name.chars().any(super::is_format_character));
                prop_assert!(!name.ends_with('.') && !name.ends_with(' '));
                prop_assert!(!name.contains('/') && !name.contains('\\'));
            }
        }
    }
}
