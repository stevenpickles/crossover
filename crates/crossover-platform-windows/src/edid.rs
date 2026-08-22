//! Reading a monitor's physical panel size out of its EDID block
//! ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md), amended
//! 2026-08-22).
//!
//! EDID is the block of bytes a monitor reports about itself over the
//! display cable, and it is the only place a panel's real dimensions come
//! from. Windows caches it verbatim in the registry, under the monitor's
//! own device key, which is where this crate's `display` module fetches it
//! — the *fetching* is Win32 and lives there, which is why it is named in
//! prose rather than linked: it does not exist on the other two OSes this
//! module compiles for. Everything here is pure: bytes in, an optional
//! [`PhysicalSizeMm`] out, no Win32, no allocation, no failure path but
//! `None`.
//!
//! **Deliberately not Windows-gated**, for the same reason
//! [`crate::worker_supervisor`] is not: a parser of somebody else's bytes
//! is exactly the code that should be exercised on every CI OS rather than
//! only on the one that can produce the input.
//!
//! # Two places a size can be written, and the order they are read in
//!
//! EDID states the panel's size twice, at different resolutions, and a
//! monitor is free to disagree with itself:
//!
//! - **The first detailed timing descriptor** (offset 54) carries the
//!   *active image* size in **millimetres**. This is the preferred source
//!   and the reason to bother parsing structure at all.
//! - **The base block's maximum image size** (bytes 21 and 22) is in
//!   **centimetres** — ten times coarser, and describing the largest image
//!   the display can show rather than the panel. It is the fallback,
//!   because on a screen whose first descriptor is a monitor *name* or
//!   *range* block rather than a timing there is nothing else.
//!
//! # A wrong size is worse than no size
//!
//! Everything here is written around that one asymmetry, and it is what
//! makes the plausibility gate below not merely defensive tidiness. A size
//! is a *proportion*: the editor seeds every rectangle against every other,
//! so one monitor claiming to be 40 mm wide does not draw one wrong
//! rectangle, it draws every rectangle on the desk wrong. Withholding a
//! size costs the improvement and nothing else — the editor falls back to
//! what it did before sizes existed.
//!
//! Projectors, televisions, virtual displays, and KVM switches all report
//! sizes that are fiction (a projector's "size" is whatever it is currently
//! throwing at a wall), and a monitor with a corrupt or partially-cached
//! EDID reports whatever happens to be in those bytes. So this refuses far
//! more aggressively than the wire does — see [`MIN_PLAUSIBLE_MM`].

use crossover_platform::PhysicalSizeMm;

/// The eight-byte header every EDID block begins with. A block that does
/// not start with it is not an EDID, whatever the registry called it.
const EDID_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// Length of the EDID base block, which is the only part this reads. Later
/// extension blocks exist and are ignored: nothing in them carries a
/// physical size the base block does not.
pub const EDID_BLOCK_BYTES: usize = 128;

/// Offset of the first detailed timing descriptor within the base block.
const FIRST_DESCRIPTOR: usize = 54;

/// Length of each of the four descriptors that follow it.
const DESCRIPTOR_BYTES: usize = 18;

/// Smallest panel dimension this build will believe, in millimetres.
///
/// 50 mm is under two inches on the short axis — smaller than any display a
/// desktop OS drives, and comfortably below a 7" panel's ~87 mm height.
///
/// **This gate is an acquisition policy, and it is deliberately far tighter
/// than the protocol's own bound** (`MAX_PHYSICAL_SIZE_MM`, ten metres).
/// The two are answering different questions. The wire has to decide
/// whether a *peer's* claim is decodable and safe to do arithmetic on, so
/// it refuses the impossible and no more — a 5 m video wall reporting
/// itself honestly is not a malformed frame, and terminating a healthy
/// session over one would be absurd. This gate decides whether *this
/// machine* believes its own hardware enough to make a claim at all, and
/// there the incentives run the other way: the cost of staying quiet is one
/// missing improvement, and the cost of being wrong is every rectangle on
/// two desks drawn to a false scale.
///
/// Keeping them separate means this range can be tightened the day a
/// particular class of lying display turns up, with no protocol change and
/// no peer to coordinate with.
pub const MIN_PLAUSIBLE_MM: u16 = 50;

/// Largest panel dimension this build will believe, in millimetres.
///
/// 3000 mm is a 3-metre screen — past any panel sold as a monitor, and past
/// most walls. Anything larger is a projector describing its current throw,
/// a driver reporting garbage, or a virtual display inventing a number.
/// See [`MIN_PLAUSIBLE_MM`] for why this range is tighter than the wire's.
pub const MAX_PLAUSIBLE_MM: u16 = 3000;

/// The panel size `edid` describes, or `None` if it does not describe one
/// this build is willing to believe.
///
/// Total and pure: every input is a value. A truncated block, a block with
/// the wrong header, a bad checksum, zeroed dimensions, and an implausible
/// measurement are all simply `None` — there is no error to report, because
/// there is no caller who would do anything but shrug at one. A monitor
/// with no readable size is a monitor drawn the way every monitor was drawn
/// before sizes existed.
#[must_use]
pub fn physical_size(edid: &[u8]) -> Option<PhysicalSizeMm> {
    let block = edid.get(..EDID_BLOCK_BYTES)?;
    if block[..EDID_HEADER.len()] != EDID_HEADER {
        return None;
    }
    // The block's own integrity check: its 128 bytes sum to zero mod 256.
    // Cheap, and the difference between reading a monitor's EDID and
    // reading whatever else ended up under that registry value.
    let checksum = block.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte));
    if checksum != 0 {
        return None;
    }

    detailed_timing_size(block)
        .or_else(|| max_image_size(block))
        .filter(|&(width_mm, height_mm)| plausible(width_mm) && plausible(height_mm))
        .map(|(width_mm, height_mm)| PhysicalSizeMm {
            width_mm,
            height_mm,
        })
}

/// The active image size in millimetres from the first detailed timing
/// descriptor — the preferred source, and the finer of the two.
///
/// `None` where the first descriptor is not a timing at all. A descriptor
/// whose first two bytes (its pixel clock) are zero is a *display*
/// descriptor — a monitor name, a serial number, a range limit — and the
/// bytes this reads would be that descriptor's text rather than any size.
/// Zeroed dimensions are `None` too: a panel that says it is 0 mm across
/// has declined to answer, and the centimetre fallback may still know.
fn detailed_timing_size(block: &[u8]) -> Option<(u16, u16)> {
    let descriptor = block.get(FIRST_DESCRIPTOR..FIRST_DESCRIPTOR + DESCRIPTOR_BYTES)?;
    if descriptor[0] == 0 && descriptor[1] == 0 {
        return None;
    }
    // Bytes 12 and 13 hold the low eight bits of each axis; byte 14 packs
    // both high nibbles, horizontal first.
    let high = descriptor[14];
    let width_mm = (u16::from(high >> 4) << 8) | u16::from(descriptor[12]);
    let height_mm = (u16::from(high & 0x0F) << 8) | u16::from(descriptor[13]);
    if width_mm == 0 || height_mm == 0 {
        return None;
    }
    Some((width_mm, height_mm))
}

/// The base block's maximum image size, in centimetres, converted to
/// millimetres — the fallback when the first descriptor is not a timing.
///
/// Coarser by a factor of ten, and describing the largest image rather than
/// the panel, so it is genuinely second best. It is still far better than
/// nothing: the editor needs a *proportion*, and a centimetre-resolution
/// aspect is a good one.
///
/// Both bytes zero is the EDID way of saying "undefined" (a projector is
/// the case the standard names), not a screen with no size.
fn max_image_size(block: &[u8]) -> Option<(u16, u16)> {
    let width_cm = u16::from(block[21]);
    let height_cm = u16::from(block[22]);
    if width_cm == 0 || height_cm == 0 {
        return None;
    }
    // At most 255 cm on each axis, so this cannot overflow a `u16`.
    Some((width_cm * 10, height_cm * 10))
}

/// Is `millimetres` a dimension a real panel could have?
fn plausible(millimetres: u16) -> bool {
    (MIN_PLAUSIBLE_MM..=MAX_PLAUSIBLE_MM).contains(&millimetres)
}

#[cfg(test)]
mod tests {
    use super::{EDID_BLOCK_BYTES, MAX_PLAUSIBLE_MM, MIN_PLAUSIBLE_MM, physical_size};

    /// Build a base block with the right header, a first descriptor of the
    /// caller's choosing, the centimetre fields set, and a correct
    /// checksum — so each test can break exactly one thing.
    ///
    /// A generator rather than a captured dump on purpose: a real 128-byte
    /// hex blob would pin one manufacturer's block and make every "what if
    /// this byte were different" case unwritable.
    fn edid(max_width_cm: u8, max_height_cm: u8, descriptor: [u8; 18]) -> Vec<u8> {
        let mut block = vec![0u8; EDID_BLOCK_BYTES];
        block[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        // A plausible-looking manufacturer id and version, so the fixture
        // reads like the thing it stands for.
        block[8..10].copy_from_slice(&[0x10, 0xAC]); // "DEL"
        block[18] = 1; // EDID version 1
        block[19] = 4; // revision 4
        block[21] = max_width_cm;
        block[22] = max_height_cm;
        block[54..72].copy_from_slice(&descriptor);
        fix_checksum(&mut block);
        block
    }

    /// Set the last byte so the block sums to zero mod 256.
    fn fix_checksum(block: &mut [u8]) {
        block[EDID_BLOCK_BYTES - 1] = 0;
        let sum = block[..EDID_BLOCK_BYTES]
            .iter()
            .fold(0u8, |sum, &byte| sum.wrapping_add(byte));
        block[EDID_BLOCK_BYTES - 1] = sum.wrapping_neg();
    }

    /// A detailed timing descriptor claiming `width_mm` x `height_mm`.
    fn timing(width_mm: u16, height_mm: u16) -> [u8; 18] {
        let mut descriptor = [0u8; 18];
        // A non-zero pixel clock is what makes this a *timing* descriptor
        // rather than a display one.
        descriptor[0] = 0x80;
        descriptor[1] = 0x2F;
        descriptor[12] = u8::try_from(width_mm & 0xFF).unwrap();
        descriptor[13] = u8::try_from(height_mm & 0xFF).unwrap();
        descriptor[14] = u8::try_from(((width_mm >> 8) << 4) | (height_mm >> 8)).unwrap();
        descriptor
    }

    /// A display descriptor — a monitor *name* block, the commonest thing
    /// to find in the first slot on a screen that has no preferred timing
    /// there. Its zero pixel clock is the tag.
    fn monitor_name(name: &str) -> [u8; 18] {
        let mut descriptor = [0u8; 18];
        descriptor[3] = 0xFC; // "monitor name" descriptor type
        let bytes = name.as_bytes();
        descriptor[5..5 + bytes.len()].copy_from_slice(bytes);
        descriptor[5 + bytes.len()] = 0x0A; // the standard's terminator
        descriptor
    }

    /// The ordinary case: a 27" 16:9 panel whose first descriptor is its
    /// preferred timing, measured in millimetres.
    #[test]
    fn the_detailed_timing_size_is_preferred_over_the_centimetre_field() {
        // The two disagree deliberately: 597x336 mm exactly, versus 60x34
        // cm rounded. The finer one must win.
        let block = edid(60, 34, timing(597, 336));
        let size = physical_size(&block).expect("a real-shaped EDID reported no size");
        assert_eq!(size.width_mm, 597);
        assert_eq!(size.height_mm, 336);
    }

    /// The high-nibble packing, which is the one piece of this format easy
    /// to get backwards: horizontal in the top nibble, vertical in the
    /// bottom. A 16:9 panel would look plausible either way round, so the
    /// case is chosen to be obviously wrong if the nibbles swap.
    #[test]
    fn the_packed_high_nibbles_are_horizontal_then_vertical() {
        // 1000 mm needs bits above the low byte on the *wide* axis only.
        let block = edid(100, 20, timing(1000, 200));
        let size = physical_size(&block).unwrap();
        assert_eq!(size.width_mm, 1000);
        assert_eq!(size.height_mm, 200);
    }

    /// A screen whose first descriptor is a monitor name rather than a
    /// timing falls back to the centimetre field, ten times coarser and
    /// still a usable proportion.
    #[test]
    fn a_descriptorless_panel_falls_back_to_the_centimetre_field() {
        let block = edid(60, 34, monitor_name("DELL U2720Q"));
        let size = physical_size(&block).expect("the centimetre fallback did not fire");
        assert_eq!(size.width_mm, 600);
        assert_eq!(size.height_mm, 340);
    }

    /// A timing descriptor that carries zeroed dimensions is not an answer,
    /// so the centimetre field still gets its turn.
    #[test]
    fn a_zeroed_timing_size_falls_through_rather_than_reporting_zero() {
        let block = edid(60, 34, timing(0, 0));
        let size = physical_size(&block).expect("a zeroed timing swallowed the fallback");
        assert_eq!(size.width_mm, 600);
        assert_eq!(size.height_mm, 340);

        // And with nothing to fall back to, the answer is no answer —
        // never a zero-sized panel.
        assert!(physical_size(&edid(0, 0, timing(0, 0))).is_none());
    }

    /// The header is what says these bytes are an EDID at all. Without the
    /// check, whatever else ended up under that registry value would be
    /// parsed as if it were a monitor.
    #[test]
    fn a_block_without_the_header_magic_is_refused() {
        let mut block = edid(60, 34, timing(597, 336));
        block[0] = 0x01;
        fix_checksum(&mut block);
        assert!(physical_size(&block).is_none());

        // Every byte of the header, not just the first.
        for index in 0..8 {
            let mut block = edid(60, 34, timing(597, 336));
            block[index] ^= 0xFF;
            fix_checksum(&mut block);
            assert!(
                physical_size(&block).is_none(),
                "a corrupt header byte at {index} was admitted"
            );
        }
    }

    /// The checksum catches a partially-cached or corrupted block, which is
    /// what a monitor read at the wrong moment actually produces.
    #[test]
    fn a_block_with_a_bad_checksum_is_refused() {
        let mut block = edid(60, 34, timing(597, 336));
        assert!(physical_size(&block).is_some());
        block[EDID_BLOCK_BYTES - 1] = block[EDID_BLOCK_BYTES - 1].wrapping_add(1);
        assert!(physical_size(&block).is_none());

        // A byte flipped in the middle likewise, since the sum is over the
        // whole block and not only its tail.
        let mut block = edid(60, 34, timing(597, 336));
        block[40] ^= 0x20;
        assert!(physical_size(&block).is_none());
    }

    /// Anything shorter than a base block, down to nothing at all: a value
    /// every time, never a panic and never an out-of-bounds read.
    #[test]
    fn a_truncated_block_is_refused_at_every_length() {
        let block = edid(60, 34, timing(597, 336));
        for length in 0..EDID_BLOCK_BYTES {
            assert!(
                physical_size(&block[..length]).is_none(),
                "a {length}-byte block was parsed"
            );
        }
        assert!(physical_size(&[]).is_none());
        assert!(physical_size(&block).is_some());
    }

    /// A block longer than the base — a monitor with extension blocks — is
    /// read for its base block and nothing else, rather than refused for
    /// being long.
    #[test]
    fn extension_blocks_are_ignored_rather_than_refused() {
        let mut block = edid(60, 34, timing(597, 336));
        block.extend_from_slice(&[0x02; EDID_BLOCK_BYTES]);
        let size = physical_size(&block).expect("a monitor with extensions reported no size");
        assert_eq!(size.width_mm, 597);
    }

    /// The plausibility gate, at both ends and on both axes. A wrong size
    /// misdraws every rectangle on the desk, not only its own, so the range
    /// is refused rather than clamped.
    #[test]
    fn implausible_measurements_are_refused_rather_than_clamped() {
        for (width_mm, height_mm, admitted) in [
            (MIN_PLAUSIBLE_MM, MIN_PLAUSIBLE_MM, true),
            (MAX_PLAUSIBLE_MM, MAX_PLAUSIBLE_MM, true),
            (597, 336, true),
            // Under the floor on one axis: a phone-sized "monitor", or a
            // driver reporting a handful of millimetres.
            (MIN_PLAUSIBLE_MM - 1, 336, false),
            (597, MIN_PLAUSIBLE_MM - 1, false),
            (10, 10, false),
            // Over the ceiling: a projector describing its throw, or a
            // virtual display inventing a number.
            (MAX_PLAUSIBLE_MM + 1, 336, false),
            (597, MAX_PLAUSIBLE_MM + 1, false),
            (4095, 4095, false), // the largest a packed 12-bit field holds
        ] {
            let block = edid(0, 0, timing(width_mm, height_mm));
            assert_eq!(
                physical_size(&block).is_some(),
                admitted,
                "{width_mm}x{height_mm} mm was handled wrong"
            );
        }
    }

    /// An implausible *timing* is not a reason to try the centimetre field:
    /// the same panel wrote both, so a block lying in millimetres is a
    /// block whose centimetres are no more believable. Refusing outright is
    /// the conservative reading, and it is the one this implements.
    #[test]
    fn an_implausible_timing_does_not_fall_back_to_the_centimetre_field() {
        let block = edid(60, 34, timing(4000, 4000));
        assert!(
            physical_size(&block).is_none(),
            "an implausible millimetre size was quietly replaced by a centimetre one"
        );
    }

    /// Arbitrary bytes with a valid header and checksum — the shape a
    /// corrupt cache produces — are a value, never a panic.
    #[test]
    fn arbitrary_contents_under_a_valid_header_never_panic() {
        for fill in [0x00u8, 0xFF, 0x7F, 0xAA] {
            let mut block = vec![fill; EDID_BLOCK_BYTES];
            block[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
            fix_checksum(&mut block);
            // Whatever it decides, it decides it without reading past the
            // block or dividing by anything.
            let _ = physical_size(&block);
        }
    }
}
