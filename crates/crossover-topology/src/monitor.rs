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
//!
//! # Identity is not the same thing as a label
//!
//! [`MonitorLabel`] is the second string a monitor can carry, and it is
//! deliberately the *opposite* kind of value: the monitor's human-readable
//! name (`DELL U2720Q`, from its EDID — or a name the platform substitutes
//! for a panel whose EDID carries none, such as a laptop's), optional,
//! **not unique**, and never a key. Nothing about layout matching, the
//! config `[layout]` section, `EntryPoint`, or crossing derivation ever
//! consults it — see ADR 0018's 2026-08-21 amendment. It exists because
//! `\\.\DISPLAY1` is the right identity and the wrong caption: a user
//! arranging three screens cannot tell which rectangle is which from a
//! device string, and the platform already knows the name they read on the
//! bezel.
//!
//! Being display-only relaxes the charset (a product name is legitimately
//! not ASCII) but not the bound: it still arrives over the wire, so it is
//! still bounded before allocation and refused rather than repaired.
//!
//! # And nor is a *physical size*
//!
//! [`PhysicalSizeMm`] is the third thing a monitor can carry, and it joins
//! the label on the display-only side of that line rather than the identity
//! side: how many millimetres wide a panel is says nothing about *which*
//! panel it is. Two identical screens report identical sizes, a projector
//! reports a number that is a polite fiction, and a virtual display reports
//! nothing at all — none of which may be allowed to decide where a crossing
//! goes. It exists so the editor can draw rectangles in proportion to real
//! screens, which is what makes a crossing look continuous across the seam
//! between two desks (ADR 0018, amended 2026-08-22).
//!
//! It is bounded like everything else that arrives over the wire, and the
//! bound here is deliberately loose — see [`MAX_PHYSICAL_SIZE_MM`] for why
//! the *wire* refuses the impossible while the *platform* refuses the
//! merely implausible.

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

/// Maximum encoded length of a monitor label.
///
/// The same 64 bytes as [`MAX_MONITOR_ID_BYTES`], and for the same reason
/// rather than by coincidence: this is a short human-readable name that
/// arrives over the wire, so the ceiling has to be generous over every real
/// EDID product name (the longest are around 20 bytes) and small enough
/// that a full `MonitorTopology` stays trivially bounded. Bytes, not
/// characters, because bytes are what the wire and the file carry — and a
/// label may legitimately be non-ASCII, so the two counts differ here in a
/// way they never do for an id.
pub const MAX_MONITOR_LABEL_BYTES: usize = 64;

/// A monitor's human-readable name — the EDID product name Windows Settings
/// shows, e.g. `DELL U2720Q` — validated on construction.
///
/// **Display only, never identity.** It is optional (a platform that cannot
/// read one reports none), it is **not unique** (two identical monitors on
/// one desk share a name, which is exactly the case the editor's `(1)` /
/// `(2)` suffixes exist for), and nothing keys off it: layout matching,
/// `[layout]`, `EntryPoint`, and crossing derivation all address a monitor
/// by its [`MonitorId`] and never look here (ADR 0018, amended
/// 2026-08-21).
///
/// A conforming label is 1..=[`MAX_MONITOR_LABEL_BYTES`] bytes of UTF-8
/// with no control, bidirectional, or invisible format characters
/// ([`validate_monitor_label`] states the exact rule). Unlike an id it is
/// **not** held to ASCII: a product name is a manufacturer's string, and
/// refusing a legitimate non-ASCII one would cost the caption for nothing —
/// the value steers no behaviour.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonitorLabel(String);

impl MonitorLabel {
    /// The label `label` names.
    ///
    /// # Errors
    ///
    /// [`MonitorLabelError`], one variant per rejection class.
    pub fn new(label: &str) -> Result<Self, MonitorLabelError> {
        validate_monitor_label(label)?;
        Ok(Self(label.to_owned()))
    }

    /// The name, as the platform reported it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a monitor label was refused.
///
/// As with [`MonitorIdError`], a variant carries a length or one code
/// point, never the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MonitorLabelError {
    /// Nothing at all. A platform with no name to report says `None`; an
    /// empty string is a different claim, and a caption of nothing is worse
    /// than falling back to the id.
    #[error("the monitor label is empty")]
    Empty,
    /// Over [`MAX_MONITOR_LABEL_BYTES`] once encoded. Checked before the
    /// character scan, so the scan is bounded by that constant.
    #[error("the monitor label is {bytes} bytes, over the {MAX_MONITOR_LABEL_BYTES}-byte maximum")]
    TooManyBytes {
        /// Encoded length that was offered.
        bytes: usize,
    },
    /// A control character — anything [`char::is_control`] admits, which is
    /// the C0 and C1 ranges plus `U+007F`.
    #[error("the monitor label contains the control character U+{codepoint:04X}")]
    ControlCharacter {
        /// The offending code point.
        codepoint: u32,
    },
    /// A character that renders as nothing, or that reorders what renders
    /// around it — see [`FORMAT_CHARACTERS`] for the list and the reason.
    #[error("the monitor label contains the format character U+{codepoint:04X}")]
    FormatCharacter {
        /// The offending code point.
        codepoint: u32,
    },
}

/// Code points a label may not contain because they are **invisible or
/// reordering**, and a caption that does not render as itself is a caption
/// that lies.
///
/// This is not fastidiousness about Unicode; it is the duplicate rule in
/// `crossover-layout`'s `caption` module, defended. That rule suffixes
/// `(1)` / `(2)` onto labels that repeat, and it decides "repeat" by string
/// equality. A peer sending `DELL\u{200B} U2720Q` alongside `DELL U2720Q`
/// sends two labels that render **identically** in the editor and compare
/// **unequal** — so neither is suffixed, and the user is looking at two
/// rectangles captioned the same, which is precisely the failure labels
/// exist to prevent. Reachable from the wire, so it is refused at the
/// decoder like every other malformed field.
///
/// The bidi half is the older concern (and the one an id is held to ASCII
/// for): an override or isolate can make a caption render in an order the
/// bytes do not have.
///
/// **An explicit list rather than a Unicode general-category test**, so
/// this crate keeps the dependency graph ADR 0018 fixes for it (`serde` and
/// `thiserror`, nothing else) and so the rule is auditable by reading it.
/// It is a deny-list, which means it is not exhaustive over everything
/// Unicode can hide with — accepted, because the containment that matters
/// is elsewhere: a label is display-only and never an identity, so the
/// worst a novel invisible character buys is the ambiguous caption above,
/// never a mis-crossing.
pub const FORMAT_CHARACTERS: &[char] = &[
    // Bidirectional controls: marks, embeddings, overrides, isolates.
    '\u{061C}', // ARABIC LETTER MARK
    '\u{200E}', // LEFT-TO-RIGHT MARK
    '\u{200F}', // RIGHT-TO-LEFT MARK
    '\u{202A}', // LEFT-TO-RIGHT EMBEDDING
    '\u{202B}', // RIGHT-TO-LEFT EMBEDDING
    '\u{202C}', // POP DIRECTIONAL FORMATTING
    '\u{202D}', // LEFT-TO-RIGHT OVERRIDE
    '\u{202E}', // RIGHT-TO-LEFT OVERRIDE
    '\u{2066}', // LEFT-TO-RIGHT ISOLATE
    '\u{2067}', // RIGHT-TO-LEFT ISOLATE
    '\u{2068}', // FIRST STRONG ISOLATE
    '\u{2069}', // POP DIRECTIONAL ISOLATE
    // Zero-width and joining controls: render as nothing at all.
    '\u{200B}', // ZERO WIDTH SPACE
    '\u{200C}', // ZERO WIDTH NON-JOINER
    '\u{200D}', // ZERO WIDTH JOINER
    '\u{2060}', // WORD JOINER
    '\u{FEFF}', // ZERO WIDTH NO-BREAK SPACE (byte order mark)
];

/// Validate a monitor label.
///
/// Pure and total: every input is a value, nothing allocates, and the
/// bounded check runs before the scan.
///
/// A conforming label is 1..=[`MAX_MONITOR_LABEL_BYTES`] bytes of UTF-8
/// containing no control character ([`char::is_control`]) and none of
/// [`FORMAT_CHARACTERS`]. Everything else — every script, every accent, a
/// space, punctuation — is admitted, because a product name is the
/// manufacturer's string and this value steers nothing.
///
/// # Errors
///
/// [`MonitorLabelError`], naming what was wrong without quoting the label.
pub fn validate_monitor_label(label: &str) -> Result<(), MonitorLabelError> {
    if label.is_empty() {
        return Err(MonitorLabelError::Empty);
    }
    // The bound first: it is what says how far the scan below can go.
    if label.len() > MAX_MONITOR_LABEL_BYTES {
        return Err(MonitorLabelError::TooManyBytes { bytes: label.len() });
    }
    // One pass, both rules, bounded by the check above: at most
    // `MAX_MONITOR_LABEL_BYTES` characters each compared against a
    // fixed-size list.
    for character in label.chars() {
        if character.is_control() {
            return Err(MonitorLabelError::ControlCharacter {
                codepoint: u32::from(character),
            });
        }
        if FORMAT_CHARACTERS.contains(&character) {
            return Err(MonitorLabelError::FormatCharacter {
                codepoint: u32::from(character),
            });
        }
    }
    Ok(())
}

impl FromStr for MonitorLabel {
    type Err = MonitorLabelError;

    /// # Errors
    ///
    /// [`MonitorLabelError`], as [`MonitorLabel::new`].
    fn from_str(label: &str) -> Result<Self, Self::Err> {
        Self::new(label)
    }
}

impl fmt::Display for MonitorLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for MonitorLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MonitorLabel({:?})", self.0)
    }
}

impl Serialize for MonitorLabel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MonitorLabel {
    /// Validating, in every format — the same property [`MonitorId`]'s
    /// decoder has, so no decoder (wire or state file) can introduce an
    /// unusable label by forgetting to check. On the wire this is the
    /// rejection, not a truncation: a peer sending an over-long or
    /// control-bearing label has sent a malformed `MonitorTopology`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let label = String::deserialize(deserializer)?;
        validate_monitor_label(&label).map_err(serde::de::Error::custom)?;
        Ok(Self(label))
    }
}

/// Largest physical dimension, in millimetres, either side of a monitor may
/// claim — ten metres.
///
/// **This is the wire's bound, and it is not the platform's.** The split is
/// deliberate and the two halves are doing different jobs:
///
/// - *Here*, the rule refuses the **impossible**. A decoder's business is
///   arithmetic safety and allocation safety, and it must not be in the
///   business of second-guessing what hardware exists — a 5 m video wall
///   reporting itself honestly is not a malformed frame, and a decoder that
///   called it one would terminate a healthy session over a value that
///   steers nothing. Ten metres is far past any panel and far short of
///   anything that could overflow the `i32` arithmetic a drawn layout does.
///   The editor *does* seed rectangles from a size, so "steers nothing" is
///   not "changes nothing" — but a value this loose bound admits and
///   [`MIN_PLAUSIBLE_PHYSICAL_MM`] does not is drawn as an **estimate**
///   rather than as a measurement, so the loose bound still cannot distort
///   a drawing, and it still never costs a session.
/// - *At the consumers*, the rule refuses the merely **implausible** —
///   [`MIN_PLAUSIBLE_PHYSICAL_MM`]`..=`[`MAX_PLAUSIBLE_PHYSICAL_MM`],
///   applied both by the platform backend that reads an EDID and by the
///   editor that seeds a rectangle. That gate is a *policy about belief*,
///   not a decoding rule, and it can be tightened for a newly-discovered
///   class of lying display with no protocol change and no peer to
///   coordinate with.
///
/// Zero is refused on both sides, on the same reasoning a zero-sized
/// rectangle is: a screen with no width is not a screen, and a proportion
/// derived from it would divide by zero.
pub const MAX_PHYSICAL_SIZE_MM: u16 = 10_000;

/// Smallest panel dimension anything in this workspace will *believe*, in
/// millimetres.
///
/// 50 mm is under two inches on the short axis — smaller than any display a
/// desktop OS drives, and comfortably below a 7" panel's ~87 mm height.
///
/// **This is a plausibility gate, not a validity bound**, and it is
/// deliberately far tighter than [`MAX_PHYSICAL_SIZE_MM`]. The two answer
/// different questions, and both answers are needed:
///
/// - The wire bound decides whether a *peer's* claim is decodable and safe
///   to do arithmetic on. It refuses the impossible and no more, because
///   terminating a healthy session over an eccentric measurement would be
///   absurd.
/// - This range decides whether a measurement is worth **drawing from**. A
///   size is a *proportion*: the editor seeds every rectangle against every
///   other, so one monitor claiming 1 mm or 10 m does not draw one wrong
///   rectangle, it draws every rectangle on the desk wrong — and there is
///   no gesture in the editor to repair a size by hand yet. Withholding a
///   size costs the improvement and nothing else: the rectangle is seeded
///   from its pixels instead, and says so.
///
/// It lives here, rather than in the EDID reader that first needed it, so
/// that the acquiring platform and the consuming editor cannot drift apart
/// about what counts as believable — one of them is looking at local
/// hardware and the other at whatever a peer sent, and a size that this
/// machine would have refused to claim must not be one it will draw from
/// merely because it arrived over the network.
pub const MIN_PLAUSIBLE_PHYSICAL_MM: u16 = 50;

/// Largest panel dimension anything in this workspace will *believe*, in
/// millimetres.
///
/// 3000 mm is a 3-metre screen — past any panel sold as a monitor, and past
/// most walls. Anything larger is a projector describing its current throw,
/// a driver reporting garbage, or a virtual display inventing a number. See
/// [`MIN_PLAUSIBLE_PHYSICAL_MM`] for why this range is tighter than the
/// wire's, and why it is shared rather than per-crate.
pub const MAX_PLAUSIBLE_PHYSICAL_MM: u16 = 3_000;

/// Whether both axes of `size` are inside the plausible range — the one
/// question every consumer of a physical size asks before drawing from it.
///
/// Pure, total, and `const`: it is a comparison, and every caller wants it
/// on a path where an allocation or an error would be absurd.
#[must_use]
pub const fn is_plausible_physical_size(size: PhysicalSizeMm) -> bool {
    is_plausible_millimetres(size.width_mm) && is_plausible_millimetres(size.height_mm)
}

/// Whether one dimension is inside the plausible range — the single-axis
/// half of [`is_plausible_physical_size`], for a producer (an EDID reader)
/// that is still holding two loose integers rather than a validated size.
#[must_use]
pub const fn is_plausible_millimetres(millimetres: u16) -> bool {
    millimetres >= MIN_PLAUSIBLE_PHYSICAL_MM && millimetres <= MAX_PLAUSIBLE_PHYSICAL_MM
}

/// A monitor's physical size — the panel's real width and height in
/// millimetres, as the platform measured it — validated on construction.
///
/// **Proportion only, never identity**, exactly as [`MonitorLabel`] is
/// caption only. Nothing matches, keys, or derives on it: layout matching,
/// the config `[layout]` section, `EntryPoint`, and crossing derivation all
/// address a monitor by its [`MonitorId`] and never look here. Its one job
/// is to let the editor seed a rectangle at a size proportional to the real
/// screen, so a cursor leaving one desk at some height up its bezel arrives
/// at the same physical height on the other (ADR 0018, amended
/// 2026-08-22).
///
/// It is optional wherever it appears, because most of the reasons a
/// platform cannot measure a panel are ordinary: a virtual display has no
/// panel, a remote session's screen is somebody else's, and an EDID can be
/// absent, unreadable, or lying. A monitor without a size is drawn the way
/// every monitor was drawn before sizes existed.
///
/// A conforming size is 1..=[`MAX_PHYSICAL_SIZE_MM`] millimetres on each
/// axis ([`validate_physical_size`] states the rule, and
/// [`MAX_PHYSICAL_SIZE_MM`] says why that ceiling is the loose one).
/// **Conforming is not the same as believable**: a size outside
/// [`MIN_PLAUSIBLE_PHYSICAL_MM`]`..=`[`MAX_PLAUSIBLE_PHYSICAL_MM`] decodes
/// and travels perfectly well, and every consumer that *draws* from a size
/// treats it as absent — see [`is_plausible_physical_size`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawPhysicalSizeMm")]
pub struct PhysicalSizeMm {
    width_mm: u16,
    height_mm: u16,
}

impl PhysicalSizeMm {
    /// The size `width_mm` × `height_mm` names.
    ///
    /// # Errors
    ///
    /// [`PhysicalSizeError`], one variant per rejection class.
    pub fn new(width_mm: u16, height_mm: u16) -> Result<Self, PhysicalSizeError> {
        validate_physical_size(width_mm, height_mm)?;
        Ok(Self {
            width_mm,
            height_mm,
        })
    }

    /// The panel's width in millimetres.
    #[must_use]
    pub const fn width_mm(self) -> u16 {
        self.width_mm
    }

    /// The panel's height in millimetres.
    #[must_use]
    pub const fn height_mm(self) -> u16 {
        self.height_mm
    }
}

/// [`PhysicalSizeMm`] before its bounds have been checked — the shape serde
/// builds, which [`TryFrom`] then admits or refuses. Same field names and
/// same order as the validated type, so the encoded form is identical in
/// every format (postcard is positional; JSON is by name).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhysicalSizeMm {
    width_mm: u16,
    height_mm: u16,
}

impl TryFrom<RawPhysicalSizeMm> for PhysicalSizeMm {
    type Error = PhysicalSizeError;

    fn try_from(raw: RawPhysicalSizeMm) -> Result<Self, Self::Error> {
        Self::new(raw.width_mm, raw.height_mm)
    }
}

/// Why a physical size was refused.
///
/// Unlike [`MonitorIdError`] and [`MonitorLabelError`], these variants quote
/// the whole value rather than only naming the fault — and that is not an
/// exception to the house rule but the rule applied. A size *is* two
/// integers, so the value and the fault are the same thing; there is no
/// string here that could carry something a diagnostic should not repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PhysicalSizeError {
    /// One axis or both measured zero. A screen with no width is not a
    /// screen, and a proportion derived from one would divide by zero.
    #[error("the monitor's physical size {width_mm}x{height_mm} mm has a zero dimension")]
    ZeroDimension {
        /// The width that was offered.
        width_mm: u16,
        /// The height that was offered.
        height_mm: u16,
    },
    /// One axis or both is over [`MAX_PHYSICAL_SIZE_MM`].
    #[error(
        "the monitor's physical size {width_mm}x{height_mm} mm is over the \
         {MAX_PHYSICAL_SIZE_MM} mm maximum"
    )]
    TooLarge {
        /// The width that was offered.
        width_mm: u16,
        /// The height that was offered.
        height_mm: u16,
    },
}

/// Validate a physical size (ADR 0018, amended 2026-08-22).
///
/// Pure and total: every input is a value, and nothing allocates.
///
/// A conforming size is 1..=[`MAX_PHYSICAL_SIZE_MM`] millimetres on each
/// axis. See [`MAX_PHYSICAL_SIZE_MM`] for why this ceiling is deliberately
/// looser than the plausibility gate the Windows backend applies before it
/// will claim a size at all.
///
/// # Errors
///
/// [`PhysicalSizeError`], naming which rule the pair broke.
pub const fn validate_physical_size(
    width_mm: u16,
    height_mm: u16,
) -> Result<(), PhysicalSizeError> {
    if width_mm == 0 || height_mm == 0 {
        return Err(PhysicalSizeError::ZeroDimension {
            width_mm,
            height_mm,
        });
    }
    if width_mm > MAX_PHYSICAL_SIZE_MM || height_mm > MAX_PHYSICAL_SIZE_MM {
        return Err(PhysicalSizeError::TooLarge {
            width_mm,
            height_mm,
        });
    }
    Ok(())
}

impl fmt::Display for PhysicalSizeMm {
    /// `597x336 mm` — the shape a log line or a status bar wants.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}x{} mm", self.width_mm, self.height_mm)
    }
}

impl fmt::Debug for PhysicalSizeMm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "PhysicalSizeMm({}x{} mm)",
            self.width_mm, self.height_mm
        )
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        FORMAT_CHARACTERS, MAX_MONITOR_ID_BYTES, MAX_MONITOR_LABEL_BYTES, MAX_PHYSICAL_SIZE_MM,
        MAX_PLAUSIBLE_PHYSICAL_MM, MIN_PLAUSIBLE_PHYSICAL_MM, MonitorId, MonitorIdError,
        MonitorLabel, MonitorLabelError, PhysicalSizeError, PhysicalSizeMm,
        is_plausible_physical_size, validate_monitor_id, validate_monitor_label,
        validate_physical_size,
    };

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

    // ---- labels ---------------------------------------------------------

    #[test]
    fn real_product_names_are_accepted() {
        for label in [
            "DELL U2720Q",
            "LG ULTRAGEAR",
            "Generic PnP Monitor",
            "Built-in Retina Display",
            // Non-ASCII is legitimate here, unlike an id: a label steers
            // nothing, and a manufacturer's string is theirs to choose.
            "LG \u{30E2}\u{30CB}\u{30BF}\u{30FC}",
            "x",
        ] {
            assert!(
                MonitorLabel::new(label).is_ok(),
                "a legitimate product name was refused: {label:?}"
            );
        }
    }

    #[test]
    fn sixty_four_label_bytes_is_accepted_and_sixty_five_is_not() {
        let at_the_cap = "x".repeat(MAX_MONITOR_LABEL_BYTES);
        assert!(MonitorLabel::new(&at_the_cap).is_ok());

        let one_over = "x".repeat(MAX_MONITOR_LABEL_BYTES + 1);
        assert_eq!(
            validate_monitor_label(&one_over),
            Err(MonitorLabelError::TooManyBytes {
                bytes: MAX_MONITOR_LABEL_BYTES + 1
            })
        );
    }

    /// The bound counts **bytes**, so a label well under 64 *characters*
    /// can still be over the cap — the axis the wire and the file use.
    #[test]
    fn the_label_bound_counts_bytes_not_characters() {
        // 22 characters, 66 bytes: legal UTF-8, legal characters, over the
        // cap on the axis that matters.
        let wide = "\u{30E2}".repeat(22);
        assert_eq!(wide.chars().count(), 22);
        assert_eq!(
            validate_monitor_label(&wide),
            Err(MonitorLabelError::TooManyBytes { bytes: wide.len() })
        );
    }

    #[test]
    fn an_empty_label_is_refused() {
        assert_eq!(validate_monitor_label(""), Err(MonitorLabelError::Empty));
    }

    #[test]
    fn control_characters_are_refused_in_a_label() {
        for control in ['\u{0}', '\n', '\r', '\t', '\u{7F}', '\u{1B}', '\u{85}'] {
            assert_eq!(
                validate_monitor_label(&format!("DELL{control}U2720Q")),
                Err(MonitorLabelError::ControlCharacter {
                    codepoint: u32::from(control)
                }),
                "a control character was admitted: U+{:04X}",
                u32::from(control)
            );
        }
    }

    /// The attack the deny-list exists for: a label that renders exactly
    /// like another one but does not compare equal to it, so the editor's
    /// duplicate rule gives neither a suffix and the user sees two
    /// identical captions.
    #[test]
    fn invisible_and_reordering_characters_are_refused_in_a_label() {
        for &format in FORMAT_CHARACTERS {
            assert_eq!(
                validate_monitor_label(&format!("DELL{format} U2720Q")),
                Err(MonitorLabelError::FormatCharacter {
                    codepoint: u32::from(format)
                }),
                "an invisible or reordering character was admitted: U+{:04X}",
                u32::from(format)
            );
        }

        // The two shapes named in review, spelled out rather than only
        // covered by the loop above: a zero-width space that forges a
        // duplicate, and a right-to-left override that forges a rendering.
        assert!(validate_monitor_label("DELL\u{200B} U2720Q").is_err());
        assert!(validate_monitor_label("DELL\u{202E}U2720Q").is_err());

        // A label that merely *looks* like it might be suspicious is not:
        // the rule denies a listed code point, not non-ASCII text.
        assert!(validate_monitor_label("LG \u{30E2}\u{30CB}\u{30BF}\u{30FC}").is_ok());
    }

    /// A rule stated as a list is only as good as the list, so the list
    /// itself is held to being a list — sorted-unique would be nicer to
    /// read but is not what `contains` needs; what it does need is that
    /// nothing in it is already covered by the control-character check,
    /// which would make the variant it reports wrong.
    #[test]
    fn the_format_deny_list_is_disjoint_from_the_control_characters() {
        for &format in FORMAT_CHARACTERS {
            assert!(
                !format.is_control(),
                "U+{:04X} is a control character and would report the wrong variant",
                u32::from(format)
            );
        }
        let mut seen: Vec<char> = FORMAT_CHARACTERS.to_vec();
        seen.sort_unstable();
        let total = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), total, "the deny-list repeats a code point");
    }

    #[test]
    fn label_deserialization_validates_rather_than_trusting_the_format() {
        let good: MonitorLabel = serde_json::from_str(r#""DELL U2720Q""#).unwrap();
        assert_eq!(good.as_str(), "DELL U2720Q");
        assert_eq!(serde_json::to_string(&good).unwrap(), r#""DELL U2720Q""#);

        assert!(serde_json::from_str::<MonitorLabel>(r#""""#).is_err());
        assert!(
            serde_json::from_str::<MonitorLabel>(&format!("\"{}\"", "x".repeat(65))).is_err(),
            "an over-long label must not survive a decoder"
        );
        assert!(serde_json::from_str::<MonitorLabel>(r#""bad\u0000label""#).is_err());
    }

    #[test]
    fn a_label_refusal_names_the_fault_and_never_the_label() {
        let error = validate_monitor_label("secret-monitor\u{7}").unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("0007"), "{rendered}");
    }

    // ---- physical sizes -------------------------------------------------

    #[test]
    fn real_panel_sizes_are_accepted_and_read_back_unchanged() {
        // A 27" 16:9 desktop panel, a 13" laptop panel, and a 5 m video
        // wall — the last one is why the wire's ceiling is not the
        // platform's plausibility gate.
        for (width, height) in [(597, 336), (286, 179), (5_000, 2_812), (1, 1)] {
            let size = PhysicalSizeMm::new(width, height).unwrap();
            assert_eq!(size.width_mm(), width);
            assert_eq!(size.height_mm(), height);
        }
    }

    /// The exact boundary in both directions, on both axes — the edge a
    /// range check is most often written one off from.
    #[test]
    fn ten_thousand_millimetres_is_accepted_and_ten_thousand_and_one_is_not() {
        assert!(PhysicalSizeMm::new(MAX_PHYSICAL_SIZE_MM, MAX_PHYSICAL_SIZE_MM).is_ok());

        for (width, height) in [
            (MAX_PHYSICAL_SIZE_MM + 1, MAX_PHYSICAL_SIZE_MM),
            (MAX_PHYSICAL_SIZE_MM, MAX_PHYSICAL_SIZE_MM + 1),
            (u16::MAX, u16::MAX),
        ] {
            assert_eq!(
                validate_physical_size(width, height),
                Err(PhysicalSizeError::TooLarge {
                    width_mm: width,
                    height_mm: height
                }),
                "{width}x{height} mm was admitted"
            );
        }
    }

    /// The plausibility range is a *belief* policy layered on top of the
    /// wire's validity bound, so it must sit strictly inside it — and both
    /// its edges are inclusive, which is the half a range check is most
    /// often written one off from.
    #[test]
    fn the_plausible_range_sits_inside_the_valid_one_and_includes_its_edges() {
        const {
            assert!(MIN_PLAUSIBLE_PHYSICAL_MM >= 1);
            assert!(MAX_PLAUSIBLE_PHYSICAL_MM < MAX_PHYSICAL_SIZE_MM);
        }

        let size = |width, height| PhysicalSizeMm::new(width, height).unwrap();
        for (width, height) in [
            (MIN_PLAUSIBLE_PHYSICAL_MM, MIN_PLAUSIBLE_PHYSICAL_MM),
            (MAX_PLAUSIBLE_PHYSICAL_MM, MAX_PLAUSIBLE_PHYSICAL_MM),
            (597, 336),
        ] {
            assert!(is_plausible_physical_size(size(width, height)));
        }
        for (width, height) in [
            (MIN_PLAUSIBLE_PHYSICAL_MM - 1, 300),
            (300, MIN_PLAUSIBLE_PHYSICAL_MM - 1),
            (MAX_PLAUSIBLE_PHYSICAL_MM + 1, 300),
            (300, MAX_PLAUSIBLE_PHYSICAL_MM + 1),
            (1, 1),
            (MAX_PHYSICAL_SIZE_MM, MAX_PHYSICAL_SIZE_MM),
        ] {
            // Valid on the wire, and still not something to draw from.
            assert!(validate_physical_size(width, height).is_ok());
            assert!(
                !is_plausible_physical_size(size(width, height)),
                "{width}x{height} mm was believed"
            );
        }
    }

    /// Zero on either axis, because a proportion derived from it would
    /// divide by zero — the same reasoning a zero-sized rectangle is
    /// refused on.
    #[test]
    fn a_zero_dimension_is_refused_on_either_axis() {
        for (width, height) in [(0, 336), (597, 0), (0, 0)] {
            assert_eq!(
                validate_physical_size(width, height),
                Err(PhysicalSizeError::ZeroDimension {
                    width_mm: width,
                    height_mm: height
                }),
                "{width}x{height} mm was admitted"
            );
        }
    }

    /// Validating on deserialize, in every format — the property that makes
    /// the newtype worth having, and the one that means the wire decoder
    /// needs no check of its own to write.
    #[test]
    fn size_deserialization_validates_rather_than_trusting_the_format() {
        let good: PhysicalSizeMm =
            serde_json::from_str(r#"{"width_mm":597,"height_mm":336}"#).unwrap();
        assert_eq!(good, PhysicalSizeMm::new(597, 336).unwrap());
        assert_eq!(
            serde_json::to_string(&good).unwrap(),
            r#"{"width_mm":597,"height_mm":336}"#
        );

        for json in [
            r#"{"width_mm":0,"height_mm":336}"#,
            r#"{"width_mm":597,"height_mm":0}"#,
            r#"{"width_mm":10001,"height_mm":336}"#,
            r#"{"width_mm":597,"height_mm":10001}"#,
            // The shape itself, not only the range.
            r#"{"width_mm":597}"#,
            r#"{"width_mm":597,"height_mm":336,"depth_mm":50}"#,
        ] {
            assert!(
                serde_json::from_str::<PhysicalSizeMm>(json).is_err(),
                "an unusable size survived a decoder: {json}"
            );
        }
    }

    /// A size is two integers, so a refusal quoting them tells the whole
    /// story — and the rendered form is the one a log line wants.
    #[test]
    fn a_size_refusal_and_its_display_both_name_the_measurements() {
        let error = validate_physical_size(0, 336).unwrap_err();
        assert!(error.to_string().contains("0x336"), "{error}");
        assert_eq!(
            PhysicalSizeMm::new(597, 336).unwrap().to_string(),
            "597x336 mm"
        );
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

        /// The same total, pure, never-panicking contract for a label —
        /// and an accepted one satisfies exactly the two rules the type
        /// documents, no more (non-ASCII is admitted on purpose).
        #[test]
        fn arbitrary_labels_never_panic(label in ".{0,200}") {
            let verdict = validate_monitor_label(&label);
            prop_assert_eq!(verdict, validate_monitor_label(&label));
            if verdict.is_ok() {
                prop_assert!(!label.is_empty());
                prop_assert!(label.len() <= MAX_MONITOR_LABEL_BYTES);
                prop_assert!(!label.chars().any(char::is_control));
                prop_assert!(
                    !label.chars().any(|c| FORMAT_CHARACTERS.contains(&c))
                );
                let accepted = MonitorLabel::new(&label).unwrap();
                prop_assert_eq!(accepted.as_str(), label.as_str());
            }
        }

        /// The same contract for a size, over the whole `u16` square: a
        /// value or a typed refusal, never a panic, and an accepted pair
        /// reads back exactly as it went in.
        #[test]
        fn arbitrary_sizes_never_panic(width in any::<u16>(), height in any::<u16>()) {
            let verdict = validate_physical_size(width, height);
            prop_assert_eq!(verdict, validate_physical_size(width, height));
            if verdict.is_ok() {
                prop_assert!((1..=MAX_PHYSICAL_SIZE_MM).contains(&width));
                prop_assert!((1..=MAX_PHYSICAL_SIZE_MM).contains(&height));
                let accepted = PhysicalSizeMm::new(width, height).unwrap();
                prop_assert_eq!(accepted.width_mm(), width);
                prop_assert_eq!(accepted.height_mm(), height);
            }
        }
    }
}
