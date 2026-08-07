//! Win32 input injection (ADR 0007; risks R-1, R-3).
//!
//! `SendInput` is the injection path. Two details do real work here:
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
//!
//! Honest limitation (R-1): `SendInput` returns success for events that
//! UIPI then discards, so a higher-integrity foreground window swallows
//! injection silently. `Ok(())` here means Windows accepted the events,
//! not that anything acted on them.

use crossover_platform::{InputError, InputInjector, PointerButton, PointerEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput,
};
// The XBUTTON constants live under WindowsAndMessaging as u16 (those in
// KeyboardAndMouse are virtual-key codes, a different thing) while
// mouseData is u32, so widen once here rather than at each use.
use windows::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

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
    fn inject(&self, events: &[PointerEvent]) -> Result<(), InputError> {
        if events.is_empty() {
            return Ok(());
        }
        // One SendInput call for the whole batch: Windows guarantees the
        // events are not interleaved with other input, which keeps a
        // press and its motion from being split by an unrelated event.
        let inputs: Vec<INPUT> = events.iter().map(|event| build_input(*event)).collect();

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
}

/// Translate one platform-neutral event into a Win32 `INPUT`.
fn build_input(event: PointerEvent) -> INPUT {
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
    use crossover_platform::{InputInjector, PointerButton, PointerEvent};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_MOVE, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    };
    use windows::Win32::UI::WindowsAndMessaging::XBUTTON2;

    use super::{CROSSOVER_INJECTION_TAG, WindowsInputInjector, build_input};

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
            let input = build_input(event);
            // SAFETY: `build_input` always constructs the `mi` variant.
            let extra = unsafe { input.Anonymous.mi.dwExtraInfo };
            assert_eq!(extra, CROSSOVER_INJECTION_TAG, "untagged: {event:?}");
        }
    }

    #[test]
    fn motion_is_relative_not_absolute() {
        let input = build_input(PointerEvent::Motion { dx: 7, dy: -3 });
        // SAFETY: as above.
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!(mouse.dwFlags, MOUSEEVENTF_MOVE);
        assert_eq!((mouse.dx, mouse.dy), (7, -3));
    }

    #[test]
    fn extended_buttons_are_distinguished_by_mouse_data() {
        let input = build_input(PointerEvent::Button {
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
        let input = build_input(PointerEvent::Scroll { dx: 0, dy: -120 });
        // SAFETY: as above.
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!(mouse.dwFlags, MOUSEEVENTF_WHEEL);
        assert_eq!(mouse.mouseData.cast_signed(), -120);
    }

    #[test]
    fn button_flags_are_not_confused_with_each_other() {
        let down = build_input(PointerEvent::Button {
            button: PointerButton::Left,
            pressed: true,
        });
        // SAFETY: as above.
        assert_eq!(unsafe { down.Anonymous.mi.dwFlags }, MOUSEEVENTF_LEFTDOWN);
        let up = build_input(PointerEvent::Button {
            button: PointerButton::Left,
            pressed: false,
        });
        // SAFETY: as above.
        assert_ne!(unsafe { up.Anonymous.mi.dwFlags }, MOUSEEVENTF_LEFTDOWN);
    }

    #[test]
    fn injecting_nothing_is_a_no_op() {
        // Called on paths where the release list may be empty; it must
        // not reach SendInput with a zero-length slice.
        assert!(WindowsInputInjector::new().inject(&[]).is_ok());
    }
}
