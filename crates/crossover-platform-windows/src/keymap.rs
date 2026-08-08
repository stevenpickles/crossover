//! USB HID usage → Windows scan code, for keyboard injection (ADR 0008).
//!
//! ADR 0008 chose the USB HID keyboard/keypad usage (Usage Page 0x07) as
//! the wire identity precisely so that Windows specifics — the Set-1 scan
//! codes and their extended-key `E0` prefix — live *here*, at the
//! platform boundary, and nowhere else. Injection is by scan code
//! (`KEYEVENTF_SCANCODE`), so this is the table the injector consults.
//!
//! One published table is the single source of truth. The extended flag
//! is the fiddly part the ADR called out: the grey navigation cluster
//! (not the numpad), the arrows, right-hand Control/Alt, the GUI keys,
//! numpad Enter, and numpad Divide all carry `E0`, and getting one wrong
//! sends the keystroke to the wrong key. The full table is verified
//! against real hardware in the Phase 4 soak; the unit tests here pin the
//! shape (uniqueness, the extended set) and a representative sample.
//!
//! Deliberate gaps, documented rather than mis-mapped: Pause/Break (an
//! `E1` sequence, not expressible as a single scan code + extended flag)
//! and keys with no standard Set-1 code. `hid_to_scancode` returns `None`
//! for anything unmapped, and the injector skips it rather than guess.

/// One row of the mapping: a HID usage and the Windows Set-1 scan code it
/// injects as, with whether that scan code needs the `E0` extended prefix.
struct KeyMap {
    hid: u16,
    scancode: u16,
    extended: bool,
}

/// Shorthand for a non-extended row.
const fn base(hid: u16, scancode: u16) -> KeyMap {
    KeyMap {
        hid,
        scancode,
        extended: false,
    }
}

/// Shorthand for an `E0`-extended row.
const fn ext(hid: u16, scancode: u16) -> KeyMap {
    KeyMap {
        hid,
        scancode,
        extended: true,
    }
}

/// The mapping, HID usage ascending. USB HID Usage Tables (Keyboard/
/// Keypad, Page 0x07) on the left; Windows Set-1 make codes on the right.
#[rustfmt::skip]
const KEY_MAP: &[KeyMap] = &[
    // Letters a–z (HID 0x04–0x1D).
    base(0x04, 0x1E), base(0x05, 0x30), base(0x06, 0x2E), base(0x07, 0x20),
    base(0x08, 0x12), base(0x09, 0x21), base(0x0A, 0x22), base(0x0B, 0x23),
    base(0x0C, 0x17), base(0x0D, 0x24), base(0x0E, 0x25), base(0x0F, 0x26),
    base(0x10, 0x32), base(0x11, 0x31), base(0x12, 0x18), base(0x13, 0x19),
    base(0x14, 0x10), base(0x15, 0x13), base(0x16, 0x1F), base(0x17, 0x14),
    base(0x18, 0x16), base(0x19, 0x2F), base(0x1A, 0x11), base(0x1B, 0x2D),
    base(0x1C, 0x15), base(0x1D, 0x2C),
    // Digits 1–9, 0 (HID 0x1E–0x27).
    base(0x1E, 0x02), base(0x1F, 0x03), base(0x20, 0x04), base(0x21, 0x05),
    base(0x22, 0x06), base(0x23, 0x07), base(0x24, 0x08), base(0x25, 0x09),
    base(0x26, 0x0A), base(0x27, 0x0B),
    // Enter, Escape, Backspace, Tab, Space.
    base(0x28, 0x1C), base(0x29, 0x01), base(0x2A, 0x0E), base(0x2B, 0x0F),
    base(0x2C, 0x39),
    // Punctuation: - = [ ] \ ; ' ` , . /
    base(0x2D, 0x0C), base(0x2E, 0x0D), base(0x2F, 0x1A), base(0x30, 0x1B),
    base(0x31, 0x2B), base(0x33, 0x27), base(0x34, 0x28), base(0x35, 0x29),
    base(0x36, 0x33), base(0x37, 0x34), base(0x38, 0x35),
    // CapsLock.
    base(0x39, 0x3A),
    // Function keys F1–F12 (HID 0x3A–0x45).
    base(0x3A, 0x3B), base(0x3B, 0x3C), base(0x3C, 0x3D), base(0x3D, 0x3E),
    base(0x3E, 0x3F), base(0x3F, 0x40), base(0x40, 0x41), base(0x41, 0x42),
    base(0x42, 0x43), base(0x43, 0x44), base(0x44, 0x57), base(0x45, 0x58),
    // PrintScreen (extended), ScrollLock.
    ext(0x46, 0x37), base(0x47, 0x46),
    // Grey navigation cluster — all extended.
    ext(0x49, 0x52), ext(0x4A, 0x47), ext(0x4B, 0x49), ext(0x4C, 0x53),
    ext(0x4D, 0x4F), ext(0x4E, 0x51),
    // Arrows — extended.
    ext(0x4F, 0x4D), ext(0x50, 0x4B), ext(0x51, 0x50), ext(0x52, 0x48),
    // NumLock, then numpad. KP Divide and KP Enter are extended.
    base(0x53, 0x45),
    ext(0x54, 0x35), base(0x55, 0x37), base(0x56, 0x4A), base(0x57, 0x4E),
    ext(0x58, 0x1C),
    base(0x59, 0x4F), base(0x5A, 0x50), base(0x5B, 0x51), base(0x5C, 0x4B),
    base(0x5D, 0x4C), base(0x5E, 0x4D), base(0x5F, 0x47), base(0x60, 0x48),
    base(0x61, 0x49), base(0x62, 0x52), base(0x63, 0x53),
    // Application/Menu key — extended.
    ext(0x65, 0x5D),
    // Modifiers. Right Control/Alt and both GUI keys are extended.
    base(0xE0, 0x1D), base(0xE1, 0x2A), base(0xE2, 0x38), ext(0xE3, 0x5B),
    ext(0xE4, 0x1D), base(0xE5, 0x36), ext(0xE6, 0x38), ext(0xE7, 0x5C),
];

/// The Windows Set-1 scan code and extended-key flag for a HID usage, or
/// `None` if the usage has no standard Set-1 mapping (an unmapped or
/// special key, e.g. Pause). Callers skip a `None` rather than guess.
#[must_use]
pub fn hid_to_scancode(hid: u16) -> Option<(u16, bool)> {
    KEY_MAP
        .iter()
        .find(|entry| entry.hid == hid)
        .map(|entry| (entry.scancode, entry.extended))
}

/// The HID usage for a Windows Set-1 scan code and its extended flag, the
/// reverse of [`hid_to_scancode`] used by keyboard capture. `None` for a
/// scan code Crossover does not carry — the capture skips it rather than
/// forward a key it cannot name. The `(scancode, extended)` pair is
/// unique (a table invariant tested here), so the reverse is unambiguous.
#[must_use]
pub fn scancode_to_hid(scancode: u16, extended: bool) -> Option<u16> {
    KEY_MAP
        .iter()
        .find(|entry| entry.scancode == scancode && entry.extended == extended)
        .map(|entry| entry.hid)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{KEY_MAP, hid_to_scancode, scancode_to_hid};

    #[test]
    fn representative_keys_map_correctly() {
        // Letter, digit, and the canonical shortcut key.
        assert_eq!(hid_to_scancode(0x04), Some((0x1E, false))); // 'a'
        assert_eq!(hid_to_scancode(0x06), Some((0x2E, false))); // 'c' (Ctrl+C)
        assert_eq!(hid_to_scancode(0x27), Some((0x0B, false))); // '0'
        assert_eq!(hid_to_scancode(0x28), Some((0x1C, false))); // Enter
        assert_eq!(hid_to_scancode(0x29), Some((0x01, false))); // Escape
        assert_eq!(hid_to_scancode(0x2C), Some((0x39, false))); // Space
    }

    #[test]
    fn extended_keys_carry_the_e0_flag() {
        // The set the ADR flagged as the fiddly part.
        for hid in [
            0x46, // PrintScreen
            0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, // nav cluster
            0x4F, 0x50, 0x51, 0x52, // arrows
            0x54, // KP Divide
            0x58, // KP Enter
            0x65, // Application
            0xE3, 0xE4, 0xE6, 0xE7, // Left/Right GUI, Right Control, Right Alt
        ] {
            assert_eq!(
                hid_to_scancode(hid).map(|(_, ext)| ext),
                Some(true),
                "HID {hid:#04x} must be an extended key"
            );
        }
    }

    #[test]
    fn left_and_right_modifiers_differ_by_the_extended_flag() {
        // Same physical scan code (Control = 0x1D, Alt = 0x38); the E0
        // prefix is the only thing distinguishing the right-hand key.
        assert_eq!(hid_to_scancode(0xE0), Some((0x1D, false))); // Left Control
        assert_eq!(hid_to_scancode(0xE4), Some((0x1D, true))); // Right Control
        assert_eq!(hid_to_scancode(0xE2), Some((0x38, false))); // Left Alt
        assert_eq!(hid_to_scancode(0xE6), Some((0x38, true))); // Right Alt
    }

    #[test]
    fn unmapped_usages_return_none() {
        assert_eq!(hid_to_scancode(0x00), None); // reserved
        assert_eq!(hid_to_scancode(0x48), None); // Pause — deliberate gap
        assert_eq!(hid_to_scancode(0xFFFF), None); // nonsense
    }

    #[test]
    fn scancode_reverse_round_trips_every_entry() {
        // Capture (scancode → HID) must invert injection (HID → scancode)
        // for every key, extended flag included — a right-hand modifier
        // must not come back as its left-hand twin.
        for entry in KEY_MAP {
            let (scancode, extended) = hid_to_scancode(entry.hid).unwrap();
            assert_eq!(
                scancode_to_hid(scancode, extended),
                Some(entry.hid),
                "HID {:#04x} did not round-trip through the scan code",
                entry.hid
            );
        }
    }

    #[test]
    fn scancode_reverse_respects_the_extended_flag() {
        // Left and Right Control share scancode 0x1D; only the flag tells
        // them apart, in both directions.
        assert_eq!(scancode_to_hid(0x1D, false), Some(0xE0)); // Left Control
        assert_eq!(scancode_to_hid(0x1D, true), Some(0xE4)); // Right Control
        assert_eq!(scancode_to_hid(0x00, false), None); // unmapped
    }

    #[test]
    fn no_hid_usage_appears_twice() {
        let mut seen = BTreeSet::new();
        for entry in KEY_MAP {
            assert!(
                seen.insert(entry.hid),
                "duplicate HID usage {:#04x}",
                entry.hid
            );
        }
    }

    #[test]
    fn a_scancode_and_extended_pair_is_never_shared() {
        // Injection is by (scancode, extended); if two usages collided on
        // one pair they would be indistinguishable on the destination.
        let mut seen = BTreeSet::new();
        for entry in KEY_MAP {
            assert!(
                seen.insert((entry.scancode, entry.extended)),
                "scancode {:#04x} (extended {}) is mapped from two usages",
                entry.scancode,
                entry.extended
            );
        }
    }
}
