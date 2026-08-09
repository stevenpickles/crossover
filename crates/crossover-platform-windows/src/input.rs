//! Win32 input injection (ADR 0007, ADR 0008; risks R-1, R-3).
//!
//! `SendInput` is the injection path for both pointer and keyboard, in
//! one ordered stream so a chord keeps its ordering. Three details do
//! real work here:
//!
//! - **Every injected event is tagged** with [`CROSSOVER_INJECTION_TAG`]
//!   in `dwExtraInfo`, which surfaces in low-level hook callbacks. That
//!   is how capture recognises its own injections and refuses to capture
//!   them back — the same mark-what-you-emit discipline that prevents
//!   clipboard loops (FR-3.3). The `LLMHF_INJECTED` flag alone would be
//!   weaker: it says *some* process injected, not that we did.
//! - **Motion is relative.** `MOUSEEVENTF_MOVE` without
//!   `MOUSEEVENTF_ABSOLUTE` applies the destination's own pointer
//!   ballistics to our unaccelerated deltas, which is what makes the
//!   remote pointer feel like the local one rather than doubly
//!   accelerated (ADR 0007).
//! - **Keys inject by scan code.** `KEYEVENTF_SCANCODE` (with
//!   `KEYEVENTF_EXTENDEDKEY` for the `E0` keys) means the destination's
//!   own layout resolves the character, so matching-layout typing and
//!   positional shortcuts land right (ADR 0008). The HID→scancode table
//!   lives in [`crate::keymap`]; an unmapped key produces no `INPUT` and
//!   is skipped rather than sent to the wrong place.
//!
//! Honest limitation (R-1): `SendInput` returns success for events that
//! UIPI then discards, so a higher-integrity foreground window swallows
//! injection silently. `Ok(())` here means Windows accepted the events,
//! not that anything acted on them.

use crossover_platform::{
    CursorPoint, InputError, InputEvent, InputInjector, KeyEvent, PointerButton, PointerEvent,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
// The XBUTTON constants live under WindowsAndMessaging as u16 (those in
// KeyboardAndMouse are virtual-key codes, a different thing) while
// mouseData is u32, so widen once here rather than at each use.
use windows::Win32::UI::WindowsAndMessaging::{SetCursorPos, XBUTTON1, XBUTTON2};

use crate::keymap;

/// Marks input this process injected, so our own capture ignores it.
///
/// An arbitrary constant with no meaning beyond being ours and unlikely
/// to collide: "XOVR" in ASCII.
pub const CROSSOVER_INJECTION_TAG: usize = 0x584F_5652;

/// Win32 [`InputInjector`].
#[derive(Debug, Default)]
pub struct WindowsInputInjector;

impl WindowsInputInjector {
    /// A new injector. Stateless: `SendInput` needs no setup.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl InputInjector for WindowsInputInjector {
    fn inject(&self, events: &[InputEvent]) -> Result<(), InputError> {
        // One SendInput call for the whole batch: Windows guarantees the
        // events are not interleaved with other input, which keeps a
        // press and its motion — or a modifier and the key it guards —
        // from being split by an unrelated event. A key with no scan-code
        // mapping (ADR 0008) produces no INPUT and is skipped, so the
        // built count, not the event count, is what SendInput must accept.
        let inputs: Vec<INPUT> = events.iter().filter_map(build_input).collect();
        if inputs.is_empty() {
            return Ok(());
        }

        let size = i32::try_from(size_of::<INPUT>()).map_err(|_| InputError::InjectionFailed {
            reason: "INPUT size does not fit in i32".to_owned(),
        })?;
        // SAFETY: `inputs` is a live slice of correctly initialised
        // INPUT structures, and `size` is that type's size, which is
        // exactly the contract SendInput documents.
        let accepted = unsafe { SendInput(&inputs, size) };

        if accepted as usize == inputs.len() {
            Ok(())
        } else {
            // A short count means the input stream was blocked — most
            // often another process holding it, or a higher-integrity
            // desktop. Report what was actually accepted.
            Err(InputError::InjectionFailed {
                reason: format!(
                    "SendInput accepted {accepted} of {} events (input blocked?)",
                    inputs.len()
                ),
            })
        }
    }

    fn place_cursor(&self, position: CursorPoint) -> Result<(), InputError> {
        // Absolute placement in screen pixels, the exact inverse of the
        // GetCursorPos this process reads for detection, so the two share
        // one coordinate space. Placement runs only while not capturing
        // (control arriving or returning), so there is no hook to capture
        // it back and no tag is needed.
        // SAFETY: SetCursorPos takes screen coordinates and has no
        // preconditions; it returns an error on failure.
        unsafe { SetCursorPos(position.x, position.y) }.map_err(|e| InputError::InjectionFailed {
            reason: format!("SetCursorPos failed: {e}"),
        })
    }
}

/// Translate one platform-neutral event into a Win32 `INPUT`, or `None`
/// for a keyboard event whose HID usage has no scan-code mapping.
fn build_input(event: &InputEvent) -> Option<INPUT> {
    match event {
        InputEvent::Pointer(pointer) => Some(build_mouse_input(*pointer)),
        InputEvent::Key(key) => build_key_input(key),
    }
}

/// Translate a keyboard event into a scan-code `INPUT` (ADR 0008), or
/// `None` if the HID usage has no Set-1 scan code — the injector skips it
/// rather than send a wrong key. Text is not consulted: Phase 4 injects
/// by physical key, and the Unicode-`text` path is a documented follow-on.
fn build_key_input(event: &KeyEvent) -> Option<INPUT> {
    let (scancode, extended) = keymap::hid_to_scancode(event.key)?;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !event.pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    Some(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                // Ignored under KEYEVENTF_SCANCODE — Windows resolves the
                // key from the scan code, applying the destination's own
                // layout, which is the point (ADR 0008).
                wVk: VIRTUAL_KEY(0),
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: CROSSOVER_INJECTION_TAG,
            },
        },
    })
}

/// Translate one pointer event into a Win32 mouse `INPUT`.
fn build_mouse_input(event: PointerEvent) -> INPUT {
    let mouse = match event {
        PointerEvent::Motion { dx, dy } => MOUSEINPUT {
            dx,
            dy,
            mouseData: 0,
            // Relative: the destination applies its own acceleration to
            // our unaccelerated deltas (ADR 0007).
            dwFlags: MOUSEEVENTF_MOVE,
            time: 0,
            dwExtraInfo: CROSSOVER_INJECTION_TAG,
        },
        PointerEvent::Button { button, pressed } => {
            let (flags, data) = button_flags(button, pressed);
            MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: CROSSOVER_INJECTION_TAG,
            }
        }
        PointerEvent::Scroll { dx, dy } => {
            // Windows carries wheel delta in mouseData as a signed value
            // reinterpreted through u32; vertical and horizontal are
            // separate flags, so a diagonal scroll needs two events.
            // Vertical wins when both are present — the caller emits
            // single-axis scrolls, and silently dropping one axis beats
            // synthesising an event the caller did not ask for.
            let (flags, amount) = if dy != 0 {
                (MOUSEEVENTF_WHEEL, dy)
            } else {
                (MOUSEEVENTF_HWHEEL, dx)
            };
            MOUSEINPUT {
                dx: 0,
                dy: 0,
                // Deliberate sign reinterpretation: Windows carries a
                // signed wheel delta through this unsigned field.
                mouseData: amount.cast_unsigned(),
                dwFlags: flags,
                time: 0,
                dwExtraInfo: CROSSOVER_INJECTION_TAG,
            }
        }
    };

    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: mouse },
    }
}

fn button_flags(
    button: PointerButton,
    pressed: bool,
) -> (
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    u32,
) {
    match (button, pressed) {
        (PointerButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (PointerButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (PointerButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (PointerButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (PointerButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (PointerButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        // Extended buttons share one flag pair and are distinguished by
        // mouseData.
        (PointerButton::X1, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
        (PointerButton::X1, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
        (PointerButton::X2, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
        (PointerButton::X2, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
    }
}

#[cfg(test)]
mod tests {
    use crossover_platform::{
        InputEvent, InputInjector, KeyEvent, PointerButton, PointerEvent, hid,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    };
    use windows::Win32::UI::WindowsAndMessaging::XBUTTON2;

    use super::{
        CROSSOVER_INJECTION_TAG, WindowsInputInjector, build_key_input, build_mouse_input,
    };

    /// Every injected event must carry the tag, or capture will treat
    /// our own injections as user input — the input-layer equivalent of
    /// a clipboard sync loop.
    #[test]
    fn every_injected_event_is_tagged() {
        let events = [
            PointerEvent::Motion { dx: 1, dy: 2 },
            PointerEvent::Button {
                button: PointerButton::Left,
                pressed: true,
            },
            PointerEvent::Scroll { dx: 0, dy: 120 },
        ];
        for event in events {
            let input = build_mouse_input(event);
            // SAFETY: `build_mouse_input` always constructs the `mi` variant.
            let extra = unsafe { input.Anonymous.mi.dwExtraInfo };
            assert_eq!(extra, CROSSOVER_INJECTION_TAG, "untagged: {event:?}");
        }
        // Keyboard injections carry the tag too, in the `ki` variant.
        let input = build_key_input(&KeyEvent::press(hid::A)).unwrap();
        // SAFETY: `build_key_input` always constructs the `ki` variant.
        let extra = unsafe { input.Anonymous.ki.dwExtraInfo };
        assert_eq!(extra, CROSSOVER_INJECTION_TAG);
    }

    #[test]
    fn motion_is_relative_not_absolute() {
        let input = build_mouse_input(PointerEvent::Motion { dx: 7, dy: -3 });
        // SAFETY: as above.
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!(mouse.dwFlags, MOUSEEVENTF_MOVE);
        assert_eq!((mouse.dx, mouse.dy), (7, -3));
    }

    #[test]
    fn extended_buttons_are_distinguished_by_mouse_data() {
        let input = build_mouse_input(PointerEvent::Button {
            button: PointerButton::X2,
            pressed: true,
        });
        // SAFETY: as above.
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!(mouse.dwFlags, MOUSEEVENTF_XDOWN);
        assert_eq!(mouse.mouseData, u32::from(XBUTTON2));
    }

    #[test]
    fn negative_scroll_survives_the_u32_reinterpretation() {
        // mouseData is u32 but carries a signed wheel delta; a scroll
        // toward the user must not become a huge positive value.
        let input = build_mouse_input(PointerEvent::Scroll { dx: 0, dy: -120 });
        // SAFETY: as above.
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!(mouse.dwFlags, MOUSEEVENTF_WHEEL);
        assert_eq!(mouse.mouseData.cast_signed(), -120);
    }

    #[test]
    fn button_flags_are_not_confused_with_each_other() {
        let down = build_mouse_input(PointerEvent::Button {
            button: PointerButton::Left,
            pressed: true,
        });
        // SAFETY: as above.
        assert_eq!(unsafe { down.Anonymous.mi.dwFlags }, MOUSEEVENTF_LEFTDOWN);
        let up = build_mouse_input(PointerEvent::Button {
            button: PointerButton::Left,
            pressed: false,
        });
        // SAFETY: as above.
        assert_ne!(unsafe { up.Anonymous.mi.dwFlags }, MOUSEEVENTF_LEFTDOWN);
    }

    /// A key press injects by scan code (ADR 0008), so the destination's
    /// own layout resolves the character — no virtual key, no source
    /// layout imported.
    #[test]
    fn key_press_injects_by_scancode() {
        let input = build_key_input(&KeyEvent::press(hid::A)).unwrap();
        // SAFETY: the `ki` variant is always what `build_key_input` builds.
        let key = unsafe { input.Anonymous.ki };
        assert_eq!(key.wScan, 0x1E); // HID 'a' → Set-1 scancode
        assert!(key.dwFlags.contains(KEYEVENTF_SCANCODE));
        assert!(!key.dwFlags.contains(KEYEVENTF_KEYUP));
        assert!(!key.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
    }

    #[test]
    fn key_release_sets_keyup() {
        let input = build_key_input(&KeyEvent::release(hid::A)).unwrap();
        // SAFETY: as above.
        assert!(unsafe { input.Anonymous.ki.dwFlags }.contains(KEYEVENTF_KEYUP));
    }

    /// An extended key (right-hand modifiers, arrows, nav cluster) carries
    /// `KEYEVENTF_EXTENDEDKEY` so it lands on the right physical key.
    #[test]
    fn extended_key_carries_the_extended_flag() {
        let input = build_key_input(&KeyEvent::press(hid::RIGHT_CONTROL)).unwrap();
        // SAFETY: as above.
        let key = unsafe { input.Anonymous.ki };
        assert_eq!(key.wScan, 0x1D); // same scancode as Left Control…
        assert!(key.dwFlags.contains(KEYEVENTF_EXTENDEDKEY)); // …distinguished by E0
    }

    #[test]
    fn a_key_with_no_scancode_mapping_is_skipped() {
        // HID Pause (0x48) has no single Set-1 code; it produces no INPUT.
        assert!(build_key_input(&KeyEvent::press(0x48)).is_none());
        // And a batch of only-unmapped keys reaches SendInput with nothing,
        // which is a no-op rather than an error.
        assert!(
            WindowsInputInjector::new()
                .inject(&[InputEvent::Key(KeyEvent::press(0x48))])
                .is_ok()
        );
    }

    #[test]
    fn injecting_nothing_is_a_no_op() {
        // Called on paths where the release list may be empty; it must
        // not reach SendInput with a zero-length slice.
        assert!(WindowsInputInjector::new().inject(&[]).is_ok());
    }
}
