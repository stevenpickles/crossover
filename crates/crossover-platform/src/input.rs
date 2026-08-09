//! The input boundary and the vocabulary it speaks (FR-4.x, ADR 0007).
//!
//! The event types live here rather than in `crossover-core` because
//! this is the layer both sides must agree on: platform crates produce
//! and consume them, and core cannot be a dependency of the trait that
//! describes them (docs/ARCHITECTURE.md §2). Policy — what is held down,
//! what may be coalesced — stays in core, which owns the state machines.

use thiserror::Error;

/// A pointer button, named by role rather than by any OS's numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PointerButton {
    /// Primary button (the OS has already applied any left-handed swap;
    /// Crossover sees the logical button).
    Left,
    /// Secondary button.
    Right,
    /// Wheel button.
    Middle,
    /// First extended button (typically "back").
    X1,
    /// Second extended button (typically "forward").
    X2,
}

impl PointerButton {
    /// Every button, in a stable order — the order releases are emitted
    /// in, so `ReleaseAllInput` is deterministic (NFR-2).
    pub const ALL: [Self; 5] = [Self::Left, Self::Right, Self::Middle, Self::X1, Self::X2];

    /// Dense index, for array-backed state.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
            Self::X1 => 3,
            Self::X2 => 4,
        }
    }
}

/// One unit of scroll: 120 per traditional detent.
///
/// Windows' `WHEEL_DELTA` convention, adopted because high-resolution
/// wheels report fractions of a detent and a coarser unit would discard
/// them. Platforms whose native unit differs scale at their own
/// boundary, not here.
pub const SCROLL_UNITS_PER_DETENT: i32 = 120;

/// A platform-neutral pointer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEvent {
    /// Relative movement in device units (ADR 0007: unaccelerated,
    /// unclamped — the desktop's pointer ballistics are not applied).
    Motion {
        /// Rightward delta.
        dx: i32,
        /// Downward delta.
        dy: i32,
    },
    /// A button transition. Ordering matters and is never coalesced.
    Button {
        /// Which button.
        button: PointerButton,
        /// True for press, false for release.
        pressed: bool,
    },
    /// Wheel movement in [`SCROLL_UNITS_PER_DETENT`] units.
    Scroll {
        /// Horizontal (positive: right).
        dx: i32,
        /// Vertical (positive: away from the user).
        dy: i32,
    },
}

/// A platform-neutral keyboard event (FR-4.1, ADR 0008).
///
/// Unlike [`PointerEvent`] this is not `Copy`: it may carry produced
/// text. Key transitions are ordered and lossless — never coalesced
/// (FR-4.2), because dropping or reordering a press/release is exactly
/// how a key gets stuck.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// Physical key identity as a USB HID keyboard/keypad usage ID
    /// (Usage Page 0x07) — layout- and OS-independent, the standard every
    /// OS derives its own codes from (ADR 0008). See [`hid`] for named
    /// examples.
    pub key: u16,
    /// True for press, false for release.
    pub pressed: bool,
    /// True when this is an OS-generated auto-repeat of an already-held
    /// key, so key-state accounting is not fooled into double-counting.
    pub repeat: bool,
    /// The Unicode text the source produced, carried so mismatched
    /// layouts can be reproduced without a wire change (ADR 0008). `None`
    /// for keys that produce no text — modifiers, arrows, function keys —
    /// and always `None` for a release.
    pub text: Option<String>,
}

impl KeyEvent {
    /// A press of `key` that produces no text.
    #[must_use]
    pub fn press(key: u16) -> Self {
        Self {
            key,
            pressed: true,
            repeat: false,
            text: None,
        }
    }

    /// A release of `key`. Releases never carry text or a repeat flag.
    #[must_use]
    pub fn release(key: u16) -> Self {
        Self {
            key,
            pressed: false,
            repeat: false,
            text: None,
        }
    }
}

/// A few well-known USB HID keyboard/keypad usage IDs (Usage Page 0x07),
/// named for readability in code and tests. This is deliberately *not*
/// the full table — the exhaustive HID ↔ platform-scancode mapping lives
/// at the Windows boundary (`crossover-platform-windows`), where platform
/// specifics belong, not in this neutral vocabulary (ADR 0008).
pub mod hid {
    /// Keyboard `a` / `A` — the canonical example usage.
    pub const A: u16 = 0x04;
    /// Keyboard Return (Enter).
    pub const ENTER: u16 = 0x28;
    /// Keyboard Escape.
    pub const ESCAPE: u16 = 0x29;
    /// Keyboard Tab.
    pub const TAB: u16 = 0x2B;
    /// Keyboard Spacebar.
    pub const SPACE: u16 = 0x2C;
    /// Left Control.
    pub const LEFT_CONTROL: u16 = 0xE0;
    /// Left Shift.
    pub const LEFT_SHIFT: u16 = 0xE1;
    /// Left Alt.
    pub const LEFT_ALT: u16 = 0xE2;
    /// Left GUI (Windows / Command / Super).
    pub const LEFT_GUI: u16 = 0xE3;
    /// Right Control.
    pub const RIGHT_CONTROL: u16 = 0xE4;
    /// Right Shift.
    pub const RIGHT_SHIFT: u16 = 0xE5;
    /// Right Alt.
    pub const RIGHT_ALT: u16 = 0xE6;
    /// Right GUI.
    pub const RIGHT_GUI: u16 = 0xE7;
}

/// One input event of either kind, in a single ordered stream.
///
/// Pointer and keyboard events must interleave — a chord like Shift+click
/// depends on the key press landing between the pointer events around it —
/// so injection replays them from one `InputEvent` sequence rather than
/// two, which would lose the ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A pointer event.
    Pointer(PointerEvent),
    /// A keyboard event.
    Key(KeyEvent),
}

impl From<PointerEvent> for InputEvent {
    fn from(event: PointerEvent) -> Self {
        Self::Pointer(event)
    }
}

impl From<KeyEvent> for InputEvent {
    fn from(event: KeyEvent) -> Self {
        Self::Key(event)
    }
}

/// Failures from input capture or injection.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum InputError {
    /// Capture could not be established or was lost unrecoverably.
    ///
    /// Losing observation while the peer holds control is a fail-closed
    /// event (ADR 0007): the caller releases control and issues
    /// `ReleaseAllInput` rather than leaving a dead pointer.
    #[error("input capture unavailable: {reason}")]
    CaptureUnavailable { reason: String },

    /// Injection failed outright.
    ///
    /// Note what this does *not* cover: on Windows, injection into a
    /// higher-integrity window is silently discarded by UIPI and reports
    /// success (R-1). A caller cannot rely on `Ok` meaning the
    /// destination saw the event.
    #[error("input injection failed: {reason}")]
    InjectionFailed { reason: String },
}

/// Receives captured events, pointer and keyboard in one stream. Invoked
/// on the platform's input thread, so it must return promptly and never
/// block — on Windows it descends from a low-level hook callback whose
/// overrun causes Windows to silently remove the hook (R-2).
pub type InputSink = Box<dyn Fn(InputEvent) + Send + Sync>;

/// Captures local pointer input and suppresses its local effect.
///
/// Suppression is the whole point: while the peer holds control, moving
/// the mouse must drive the far machine *without* also acting here.
/// Implementations that can observe but not suppress do not satisfy this
/// trait (ADR 0007).
pub trait InputCapture: Send + Sync {
    /// Begin capturing and suppressing local pointer input, delivering
    /// events to `sink`.
    ///
    /// Idempotent: starting while already capturing replaces the sink.
    ///
    /// # Errors
    ///
    /// [`InputError::CaptureUnavailable`] if capture cannot be
    /// established — fatal to the control transfer that requested it,
    /// never a silent partial success.
    fn start_capture(&self, sink: InputSink) -> Result<(), InputError>;

    /// Stop capturing; local input resumes acting locally.
    ///
    /// Idempotent, and safe to call when not capturing — callers reach
    /// for it on error paths where the state is uncertain, which is
    /// exactly when it must not fail.
    ///
    /// # Errors
    ///
    /// [`InputError::CaptureUnavailable`] if teardown fails.
    fn stop_capture(&self) -> Result<(), InputError>;

    /// Whether capture is currently active *and observed to be healthy*.
    ///
    /// Implementations detect their own loss where the platform allows
    /// it (Windows removes an overrunning hook without telling anyone),
    /// so a caller polling this can fail closed rather than believe it
    /// still holds input it stopped receiving.
    fn is_capturing(&self) -> bool;

    /// Whether the user asked to release control via the platform escape
    /// gesture since the last poll — both Control keys, on Windows
    /// (ADR 0008). Read-and-clear: a caller polling this true releases
    /// control. It exists because, once the keyboard is captured, every
    /// ordinary key goes to the peer, so the usual console command cannot
    /// reach the user; the escape is caught in the hook and never
    /// forwarded. Platforms with no capture escape keep the default.
    fn escape_requested(&self) -> bool {
        false
    }

    /// A monotonic tick (milliseconds since boot, on Windows) of the most
    /// recent local keyboard or mouse input, or `None` if unavailable.
    ///
    /// This is a **system-wide** query, independent of whether capture is
    /// active — it reports physical local input as well as injected input.
    /// The control driver uses it as the cursor fail-safe (ADR 0009): while
    /// the cursor is hidden and this machine is *not* driving the peer,
    /// fresh local input means the user is here, so the cursor is shown
    /// again. Platforms without the query keep the default and simply do
    /// not offer the fail-safe.
    fn last_input_tick(&self) -> Option<u32> {
        None
    }
}

/// Injects pointer and keyboard input on this machine.
pub trait InputInjector: Send + Sync {
    /// Replay `events` in order, pointer and keyboard interleaved.
    ///
    /// Implementations must mark their own injections so that a
    /// concurrently active [`InputCapture`] does not capture them back
    /// (ADR 0007) — the same mark-what-you-emit discipline clipboard
    /// loop prevention uses.
    ///
    /// # Errors
    ///
    /// [`InputError::InjectionFailed`] if the platform rejects the
    /// events. Success does **not** guarantee the destination window
    /// received them (UIPI, R-1).
    fn inject(&self, events: &[InputEvent]) -> Result<(), InputError>;

    /// Place the pointer at an absolute `position` on the primary display
    /// (top-left origin, the same pixel space [`DisplayInfo`] reports).
    /// Seamless transfer uses this to make the cursor *appear* at the edge
    /// it crossed (ADR 0009), which a relative delta cannot express. Only
    /// meaningful while this machine is not capturing, which is exactly
    /// when control arrives or returns.
    ///
    /// # Errors
    ///
    /// [`InputError::InjectionFailed`] if the platform refuses the move.
    fn place_cursor(&self, position: crate::display::CursorPoint) -> Result<(), InputError>;
}
