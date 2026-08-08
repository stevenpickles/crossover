//! Win32 input capture (ADR 0007; risks R-2, R-6 in
//! docs/SPECIFICATION.md §6).
//!
//! Two Win32 mechanisms run together because neither suffices alone
//! (ADR 0007):
//!
//! - **The `WH_MOUSE_LL` hook decides what is suppressed.** A low-level
//!   hook is the only user-mode mechanism that can swallow an event
//!   before the rest of the system sees it, which is the point of
//!   capture: while the peer has control, moving the mouse here must
//!   not drag windows here. Its data is wrong for forwarding, though —
//!   accelerated, and clamped to the local desktop, so once the cursor
//!   pins at a screen edge the hook reports no further motion in that
//!   direction.
//! - **Raw Input supplies what is sent.** `WM_INPUT` reports
//!   unaccelerated, unclamped device deltas — what a remote pointer
//!   needs. It cannot suppress anything, which is why it does not run
//!   alone.
//!
//! The hook callback does near-zero work (R-2): Windows silently
//! removes a low-level hook whose callback overruns
//! `LowLevelHooksTimeout` (~300 ms), so the callback reads one field,
//! touches two atomics, and returns — no locks, no allocation, no
//! logging. Everything else (translation, the sink call, diagnostics)
//! happens on the pump thread's `WM_INPUT` path, outside the hook.
//!
//! **Hook loss is detected, and the response is to fail closed.** A
//! watchdog compares the hook's event count against Raw Input's: both
//! observe the same stream, so raw events arriving while the hook
//! counts nothing means Windows removed the hook. Capture is then torn
//! down — local input acts locally again immediately — and
//! `is_capturing()` reports `false` so the caller releases control and
//! issues `ReleaseAllInput`. Deliberately *not* done: re-installing the
//! hook in place. That would resume suppressing input while the caller
//! is abandoning the control transfer, and a suppressed mouse nobody is
//! consuming is a dead mouse — the release-blocking defect class. The
//! loss stays visible (logged, NFR-3) and recovery is the caller's
//! fresh `start_capture`.
//!
//! Unlike the clipboard listener, the pump window is a hidden
//! *top-level* window rather than a message-only one: Raw Input is not
//! delivered to message-only windows.
//!
//! A hook callback is a C function pointer and cannot capture state, so
//! the flag it reads and the counter it advances are `static`. That
//! makes capture per-process-exclusive, which `start_capture` enforces
//! explicitly rather than leaves to whichever instance's flags win.
//!
//! Known limitation (R-6): applications taking exclusive raw input
//! (games, some remote-desktop clients) may not honour suppression.
//! Out of scope for Phase 3.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crossover_platform::{InputCapture, InputError, InputSink, PointerButton, PointerEvent};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HID_USAGE_GENERIC_MOUSE, HID_USAGE_PAGE_GENERIC,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, MOUSE_VIRTUAL_DESKTOP, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RAWMOUSE, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, HHOOK, KillTimer, MSG, MSLLHOOKSTRUCT, PostMessageW, RI_MOUSE_BUTTON_4_DOWN,
    RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, RI_MOUSE_HWHEEL,
    RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN,
    RI_MOUSE_MIDDLE_BUTTON_UP, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP,
    RI_MOUSE_WHEEL, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SetTimer,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_INPUT, WM_TIMER,
};
use windows::core::w;

use crate::input::CROSSOVER_INJECTION_TAG;

/// Private message asking the pump thread to shut down.
const WM_APP_SHUTDOWN: u32 = WM_APP + 1;
/// Private message asking the pump thread to install capture.
const WM_APP_START_CAPTURE: u32 = WM_APP + 2;
/// Private message asking the pump thread to tear capture down.
const WM_APP_STOP_CAPTURE: u32 = WM_APP + 3;

/// Watchdog timer identity and period. One period bounds how long a
/// silently removed hook can leak input locally before detection.
const WATCHDOG_TIMER_ID: usize = 1;
const WATCHDOG_PERIOD_MS: u32 = 500;

/// Raw events in one watchdog period with zero hook events before loss
/// is declared. The hook and Raw Input observe the same stream but are
/// not delivered in lockstep, so a small skew near a tick boundary is
/// routine; a dead hook under a moving mouse crosses this within one
/// period, while the threshold keeps a boundary race from being misread
/// as loss.
const WATCHDOG_MIN_RAW_EVENTS: usize = 5;

// State the hook procedure reaches. A hook callback is a C function
// pointer — it cannot capture, so what it touches must be statically
// addressable. `start_capture` enforces one active capture per process,
// which keeps these unambiguous. All accesses are Relaxed: they are
// advisory signals (suppress or not, activity counters) with no data
// ordered behind them — the sink and its data travel under a Mutex.

/// Whether the hook should swallow untagged events right now.
static SUPPRESSING: AtomicBool = AtomicBool::new(false);
/// Untagged mouse events the hook has seen (watchdog evidence).
static HOOK_EVENTS: AtomicUsize = AtomicUsize::new(0);
/// Process-wide exclusivity: exactly one capture may be installed.
static CAPTURE_INSTALLED: AtomicBool = AtomicBool::new(false);

/// What the hook does with one event. Factored out of the callback so
/// the decision — the security-relevant part — is testable without
/// installing a real hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookVerdict {
    /// Our own injection (tag match): pass through untouched, uncounted.
    /// Suppressing it would deaden injection; counting it would let our
    /// own output masquerade as user activity.
    PassOurInjection,
    /// User input while capturing: swallow it (the point of capture).
    Suppress,
    /// User input while not capturing: pass through, but still counted —
    /// the hook only exists during capture, so this is the teardown
    /// window where the flag is already clear.
    Pass,
}

fn hook_verdict(extra_info: usize, suppressing: bool) -> HookVerdict {
    if extra_info == CROSSOVER_INJECTION_TAG {
        HookVerdict::PassOurInjection
    } else if suppressing {
        HookVerdict::Suppress
    } else {
        HookVerdict::Pass
    }
}

/// Watchdog judgment for one period: raw events flowing while the hook
/// saw nothing means Windows removed the hook (R-2).
fn hook_lost(hook_delta: usize, raw_delta: usize) -> bool {
    hook_delta == 0 && raw_delta >= WATCHDOG_MIN_RAW_EVENTS
}

/// The `WH_MOUSE_LL` callback. Near-zero work (R-2): one field read,
/// at most two Relaxed atomic operations, return.
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for code >= 0 the WH_MOUSE_LL contract says lparam
        // points to a valid MSLLHOOKSTRUCT for the duration of the call.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        match hook_verdict(info.dwExtraInfo, SUPPRESSING.load(Ordering::Relaxed)) {
            HookVerdict::Suppress => {
                HOOK_EVENTS.fetch_add(1, Ordering::Relaxed);
                return LRESULT(1);
            }
            HookVerdict::Pass => {
                HOOK_EVENTS.fetch_add(1, Ordering::Relaxed);
            }
            HookVerdict::PassOurInjection => {}
        }
    }
    // SAFETY: forwarding exactly the arguments we received, as the hook
    // contract requires for events we do not consume.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// State shared between the public handle and the pump thread.
struct Shared {
    sink: Mutex<Option<InputSink>>,
    /// Reply channel for the in-flight start/stop request; `ops` on the
    /// handle serializes requests, so one slot suffices.
    reply: Mutex<Option<mpsc::Sender<Result<(), String>>>>,
    /// `is_capturing()`: capture installed *and* believed healthy. The
    /// watchdog clears it on hook loss so callers fail closed.
    active: AtomicBool,
    /// Untagged raw mouse reports seen (watchdog evidence).
    raw_events: AtomicUsize,
}

/// Win32 [`InputCapture`]. Owns the pump thread; dropping it shuts the
/// thread down (releasing capture if active).
pub struct WindowsInputCapture {
    shared: Arc<Shared>,
    /// The pump window, as a raw value so the handle stays `Send` (used
    /// only with `PostMessageW`; the pump thread owns the window).
    hwnd_raw: isize,
    pump: Option<std::thread::JoinHandle<()>>,
    /// Serializes start/stop so the single reply slot is unambiguous.
    ops: Mutex<()>,
}

impl WindowsInputCapture {
    /// Start the pump thread and create the capture provider. Capture
    /// itself is not installed until [`InputCapture::start_capture`] —
    /// an idle provider costs the system nothing.
    ///
    /// # Errors
    ///
    /// [`InputError::CaptureUnavailable`] if the pump thread or its
    /// window cannot be created.
    pub fn new() -> Result<Self, InputError> {
        let shared = Arc::new(Shared {
            sink: Mutex::new(None),
            reply: Mutex::new(None),
            active: AtomicBool::new(false),
            raw_events: AtomicUsize::new(0),
        });
        let pump_shared = Arc::clone(&shared);
        let (init_tx, init_rx) = mpsc::channel::<Result<isize, String>>();

        let pump = std::thread::Builder::new()
            .name("crossover-input-pump".to_owned())
            .spawn(move || pump_thread(&pump_shared, &init_tx))
            .map_err(|e| InputError::CaptureUnavailable {
                reason: format!("spawning input pump thread: {e}"),
            })?;

        let hwnd_raw = init_rx
            .recv()
            .map_err(|_| InputError::CaptureUnavailable {
                reason: "input pump thread died during startup".to_owned(),
            })?
            .map_err(|reason| InputError::CaptureUnavailable { reason })?;

        Ok(Self {
            shared,
            hwnd_raw,
            pump: Some(pump),
            ops: Mutex::new(()),
        })
    }

    /// Post `message` to the pump thread and wait for its verdict.
    fn request(&self, message: u32) -> Result<(), InputError> {
        let (tx, rx) = mpsc::channel();
        *self
            .shared
            .reply
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);

        let hwnd = HWND(self.hwnd_raw as *mut core::ffi::c_void);
        // SAFETY: PostMessageW is thread-safe by API contract; the pump
        // thread owns and outlives the window until Drop joins it.
        unsafe { PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0)) }.map_err(|e| {
            InputError::CaptureUnavailable {
                reason: format!("posting to input pump failed: {e}"),
            }
        })?;

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(InputError::CaptureUnavailable { reason }),
            Err(_) => Err(InputError::CaptureUnavailable {
                reason: "input pump did not respond".to_owned(),
            }),
        }
    }
}

impl InputCapture for WindowsInputCapture {
    fn start_capture(&self, sink: InputSink) -> Result<(), InputError> {
        let _ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
        // Installing the sink first makes the already-capturing case the
        // documented idempotent sink replacement; the pump only ever
        // reads it per delivered event.
        *self
            .shared
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sink);
        let result = self.request(WM_APP_START_CAPTURE);
        if result.is_err() {
            *self
                .shared
                .sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        }
        result
    }

    fn stop_capture(&self) -> Result<(), InputError> {
        let _ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
        self.request(WM_APP_STOP_CAPTURE)
    }

    fn is_capturing(&self) -> bool {
        self.shared.active.load(Ordering::Relaxed)
    }
}

impl Drop for WindowsInputCapture {
    fn drop(&mut self) {
        let hwnd = HWND(self.hwnd_raw as *mut core::ffi::c_void);
        // SAFETY: PostMessageW is safe from any thread with any window
        // handle; a stale handle at worst fails harmlessly.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0)) };
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

/// One raw mouse report, reduced to the fields translation needs —
/// plain integers so tests can build reports without Win32 types.
struct RawMouseReport {
    /// `RAWMOUSE::usFlags` (relative/absolute/virtual-desktop bits).
    flags: u16,
    /// `usButtonFlags`, widened once: the windows crate declares the
    /// `RI_MOUSE_*` constants as u32 while the field is u16.
    buttons: u32,
    /// `usButtonData` (wheel delta when a wheel flag is set).
    data: u16,
    /// `lLastX`/`lLastY`: relative delta, or absolute position in the
    /// normalized 0..=65535 space.
    x: i32,
    y: i32,
}

/// Screen extents for scaling absolute-mode motion into pixels.
#[derive(Clone, Copy)]
struct ScreenExtents {
    primary: (i32, i32),
    virtual_desktop: (i32, i32),
}

/// Converts absolute-mode positions (RDP sessions, some touchpads and
/// tablets) into the deltas the wire vocabulary speaks. Stateful: a
/// delta needs the previous position, and the first sample after a mode
/// switch has none, so it establishes position and emits nothing.
#[derive(Default)]
struct AbsoluteTracker {
    last: Option<(i32, i32)>,
}

/// Scale a delta in the normalized 0..=65535 absolute space onto a
/// screen extent in pixels.
fn scale_absolute(delta: i32, extent: i32) -> i32 {
    let scaled = i64::from(delta) * i64::from(extent) / 65536;
    // |delta| <= 65535 and extent fits i32, so the product / 65536 fits
    // i32 again; try_from cannot fail, but stays panic-free regardless.
    i32::try_from(scaled).unwrap_or(0)
}

/// `RI_MOUSE` button transitions in a fixed emission order, so one
/// report carrying several transitions translates deterministically
/// (NFR-2).
const BUTTON_TRANSITIONS: [(u32, PointerButton, bool); 10] = [
    (RI_MOUSE_LEFT_BUTTON_DOWN, PointerButton::Left, true),
    (RI_MOUSE_LEFT_BUTTON_UP, PointerButton::Left, false),
    (RI_MOUSE_RIGHT_BUTTON_DOWN, PointerButton::Right, true),
    (RI_MOUSE_RIGHT_BUTTON_UP, PointerButton::Right, false),
    (RI_MOUSE_MIDDLE_BUTTON_DOWN, PointerButton::Middle, true),
    (RI_MOUSE_MIDDLE_BUTTON_UP, PointerButton::Middle, false),
    (RI_MOUSE_BUTTON_4_DOWN, PointerButton::X1, true),
    (RI_MOUSE_BUTTON_4_UP, PointerButton::X1, false),
    (RI_MOUSE_BUTTON_5_DOWN, PointerButton::X2, true),
    (RI_MOUSE_BUTTON_5_UP, PointerButton::X2, false),
];

/// Translate one raw mouse report into pointer events, in a fixed
/// order: motion, then button transitions, then wheel (NFR-2).
fn translate_raw_mouse(
    report: &RawMouseReport,
    tracker: &mut AbsoluteTracker,
    extents: ScreenExtents,
    out: &mut Vec<PointerEvent>,
) {
    if report.flags & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        let (width, height) = if report.flags & MOUSE_VIRTUAL_DESKTOP.0 != 0 {
            extents.virtual_desktop
        } else {
            extents.primary
        };
        if let Some((prev_x, prev_y)) = tracker.last {
            let dx = scale_absolute(report.x - prev_x, width);
            let dy = scale_absolute(report.y - prev_y, height);
            if dx != 0 || dy != 0 {
                out.push(PointerEvent::Motion { dx, dy });
            }
        }
        tracker.last = Some((report.x, report.y));
    } else {
        // Back in relative mode: a stale absolute anchor must not turn
        // the next absolute sample into a huge jump.
        tracker.last = None;
        if report.x != 0 || report.y != 0 {
            out.push(PointerEvent::Motion {
                dx: report.x,
                dy: report.y,
            });
        }
    }

    for (flag, button, pressed) in BUTTON_TRANSITIONS {
        if report.buttons & flag != 0 {
            out.push(PointerEvent::Button { button, pressed });
        }
    }

    // usButtonData carries the wheel delta as a signed value through an
    // unsigned field — same reinterpretation the injector performs in
    // the other direction, and already in SCROLL_UNITS_PER_DETENT units
    // (both are Windows' WHEEL_DELTA convention).
    let wheel = i32::from(report.data.cast_signed());
    if report.buttons & RI_MOUSE_WHEEL != 0 && wheel != 0 {
        out.push(PointerEvent::Scroll { dx: 0, dy: wheel });
    }
    if report.buttons & RI_MOUSE_HWHEEL != 0 && wheel != 0 {
        out.push(PointerEvent::Scroll { dx: wheel, dy: 0 });
    }
}

/// The raw-side twin of the hook's tag check: `ulExtraInformation` is
/// the u32 view of the `dwExtraInfo` the injector stamps.
fn is_our_injection(extra: u32) -> bool {
    usize::try_from(extra).is_ok_and(|e| e == CROSSOVER_INJECTION_TAG)
}

fn screen_extents() -> ScreenExtents {
    // SAFETY: GetSystemMetrics reads cached system values; no
    // preconditions.
    unsafe {
        ScreenExtents {
            primary: (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)),
            virtual_desktop: (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            ),
        }
    }
}

/// Everything the pump thread owns. Not shared: the hook handle, the
/// absolute-motion anchor, and the watchdog baselines live and die on
/// this thread.
struct Pump {
    shared: Arc<Shared>,
    hwnd: HWND,
    hook: Option<HHOOK>,
    /// Whether this pump holds the process-wide capture (guards the
    /// statics: a second, idle instance must never touch them).
    owns_capture: bool,
    tracker: AbsoluteTracker,
    /// Reused per report so steady-state forwarding does not allocate.
    events: Vec<PointerEvent>,
    watchdog_hook_baseline: usize,
    watchdog_raw_baseline: usize,
}

impl Pump {
    fn reply(&self, result: Result<(), String>) {
        if let Some(tx) = self
            .shared
            .reply
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(result);
        }
    }

    fn on_start(&mut self) {
        let result = if self.hook.is_some() {
            // Already capturing: the caller has replaced the sink, and
            // that is the whole of the documented idempotent restart.
            Ok(())
        } else if CAPTURE_INSTALLED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            Err("another WindowsInputCapture is already active in this process".to_owned())
        } else {
            self.owns_capture = true;
            let installed = self.install();
            if installed.is_err() {
                // Roll back whatever partially succeeded; we own the
                // guard, so teardown is ours to run.
                self.teardown_capture();
            }
            installed
        };
        self.reply(result);
    }

    fn install(&mut self) -> Result<(), String> {
        // SAFETY: installing a low-level hook whose callback lives in
        // this module; a null module handle is documented-correct for
        // WH_*_LL hooks (the callback is in this process, not a DLL).
        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) }
            .map_err(|e| format!("SetWindowsHookExW(WH_MOUSE_LL) failed: {e}"))?;
        self.hook = Some(hook);

        let size = u32::try_from(size_of::<RAWINPUTDEVICE>())
            .map_err(|_| "RAWINPUTDEVICE size does not fit in u32".to_owned())?;
        let device = RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_MOUSE,
            // INPUTSINK: deliver WM_INPUT regardless of focus — capture
            // runs precisely when the user is *not* working in our
            // (invisible) window.
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: self.hwnd,
        };
        // SAFETY: registering one correctly initialised RAWINPUTDEVICE
        // whose target window this thread owns.
        unsafe { RegisterRawInputDevices(&[device], size) }
            .map_err(|e| format!("RegisterRawInputDevices failed: {e}"))?;

        // SAFETY: associating a timer with the window this thread owns;
        // no callback function, so WM_TIMER arrives through the pump.
        let timer =
            unsafe { SetTimer(Some(self.hwnd), WATCHDOG_TIMER_ID, WATCHDOG_PERIOD_MS, None) };
        if timer == 0 {
            // Without the watchdog, hook loss would be silent — exactly
            // what R-2 forbids. No watchdog, no capture.
            return Err("SetTimer for the hook-loss watchdog failed".to_owned());
        }

        HOOK_EVENTS.store(0, Ordering::Relaxed);
        self.shared.raw_events.store(0, Ordering::Relaxed);
        self.watchdog_hook_baseline = 0;
        self.watchdog_raw_baseline = 0;
        self.tracker = AbsoluteTracker::default();
        SUPPRESSING.store(true, Ordering::Relaxed);
        self.shared.active.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Tear down whatever part of capture is installed. Lenient by
    /// design: stop is what error paths call when state is uncertain,
    /// so individual teardown failures are ignored — including
    /// unhooking a hook Windows already removed (R-2).
    fn teardown_capture(&mut self) {
        if !self.owns_capture {
            return; // idle instance: the statics belong to someone else
        }
        SUPPRESSING.store(false, Ordering::Relaxed);
        self.shared.active.store(false, Ordering::Relaxed);

        // SAFETY: cancelling the timer set on this thread's window; may
        // not exist if install failed early, which KillTimer tolerates.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), WATCHDOG_TIMER_ID);
        }
        if let Ok(size) = u32::try_from(size_of::<RAWINPUTDEVICE>()) {
            let device = RAWINPUTDEVICE {
                usUsagePage: HID_USAGE_PAGE_GENERIC,
                usUsage: HID_USAGE_GENERIC_MOUSE,
                // RIDEV_REMOVE requires a null target window.
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            };
            // SAFETY: removing our own registration; fails harmlessly if
            // registration never happened.
            unsafe {
                let _ = RegisterRawInputDevices(&[device], size);
            }
        }
        if let Some(hook) = self.hook.take() {
            // SAFETY: unhooking the hook this thread installed. Fails if
            // Windows already removed it — the very condition the
            // watchdog detects — and that failure changes nothing.
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        *self
            .shared
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.owns_capture = false;
        CAPTURE_INSTALLED.store(false, Ordering::Relaxed);
    }

    fn on_stop(&mut self) {
        self.teardown_capture();
        self.reply(Ok(()));
    }

    fn on_watchdog_tick(&mut self) {
        if !self.owns_capture {
            return;
        }
        let hook_now = HOOK_EVENTS.load(Ordering::Relaxed);
        let raw_now = self.shared.raw_events.load(Ordering::Relaxed);
        let hook_delta = hook_now.wrapping_sub(self.watchdog_hook_baseline);
        let raw_delta = raw_now.wrapping_sub(self.watchdog_raw_baseline);
        self.watchdog_hook_baseline = hook_now;
        self.watchdog_raw_baseline = raw_now;

        if hook_lost(hook_delta, raw_delta) {
            // NFR-3: the loss itself is the diagnostic; is_capturing()
            // flipping false is what the caller acts on.
            tracing::warn!(
                raw_delta,
                "low-level mouse hook lost (R-2); failing closed and releasing capture"
            );
            self.teardown_capture();
        }
    }

    fn on_raw_input(&mut self, lparam: LPARAM) {
        if !self.owns_capture {
            return; // stray report queued around teardown
        }
        let Some(mouse) = read_raw_mouse(lparam) else {
            return;
        };
        if is_our_injection(mouse.ulExtraInformation) {
            return;
        }
        self.shared.raw_events.fetch_add(1, Ordering::Relaxed);

        // SAFETY: RAWMOUSE's button union is two views of the same bits
        // (one u32 versus two u16s); reading the split view is always
        // valid for a mouse report.
        let (button_flags, button_data) = unsafe {
            (
                mouse.Anonymous.Anonymous.usButtonFlags,
                mouse.Anonymous.Anonymous.usButtonData,
            )
        };
        let report = RawMouseReport {
            flags: mouse.usFlags.0,
            buttons: u32::from(button_flags),
            data: button_data,
            x: mouse.lLastX,
            y: mouse.lLastY,
        };

        self.events.clear();
        translate_raw_mouse(
            &report,
            &mut self.tracker,
            screen_extents(),
            &mut self.events,
        );
        if self.events.is_empty() {
            return;
        }
        if let Some(sink) = self
            .shared
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            for event in &self.events {
                // Contract (InputSink): quick and non-blocking — this
                // runs on the thread whose stall would overrun the hook
                // timeout (R-2).
                sink(*event);
            }
        }
    }
}

/// Fetch the `RAWMOUSE` behind a `WM_INPUT` message. `None` for
/// non-mouse input or any API failure — skipping a report is always
/// safe; panicking or blocking here never is.
fn read_raw_mouse(lparam: LPARAM) -> Option<RAWMOUSE> {
    let hrawinput = HRAWINPUT(lparam.0 as *mut core::ffi::c_void);
    let mut raw = RAWINPUT::default();
    let mut size = u32::try_from(size_of::<RAWINPUT>()).ok()?;
    let header_size = u32::try_from(size_of::<RAWINPUTHEADER>()).ok()?;
    // SAFETY: the buffer is a live RAWINPUT and `size` is its true
    // size; GetRawInputData copies at most that many bytes. A mouse
    // report always fits; anything larger is refused by the API (it
    // returns -1), which the check below treats as "skip".
    let copied = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            Some((&raw mut raw).cast()),
            &raw mut size,
            header_size,
        )
    };
    if copied == u32::MAX || copied == 0 {
        return None;
    }
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    // SAFETY: dwType == RIM_TYPEMOUSE guarantees the union holds the
    // mouse variant.
    Some(unsafe { raw.data.mouse })
}

/// The pump thread: create a hidden top-level window (message-only
/// windows do not receive Raw Input), then serve capture requests,
/// `WM_INPUT` reports, and watchdog ticks until shutdown.
fn pump_thread(shared: &Arc<Shared>, init: &mpsc::Sender<Result<isize, String>>) {
    // SAFETY: creating a window from the prebuilt STATIC class (no
    // custom window procedure; we read our messages from the queue).
    // Zero-sized, never shown — invisible, but a real top-level window,
    // which Raw Input delivery requires.
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("crossover-input-capture"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            let _ = init.send(Err(format!("creating capture window: {e}")));
            return;
        }
    };
    let _ = init.send(Ok(hwnd.0 as isize));

    let mut pump = Pump {
        shared: Arc::clone(shared),
        hwnd,
        hook: None,
        owns_capture: false,
        tracker: AbsoluteTracker::default(),
        events: Vec::new(),
        watchdog_hook_baseline: 0,
        watchdog_raw_baseline: 0,
    };

    let mut msg = MSG::default();
    loop {
        // SAFETY: standard message pump for this thread's queue. The
        // hook callback also runs inside this call, which is why
        // everything dispatched from here must stay prompt (R-2).
        let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if result.0 <= 0 {
            break; // WM_QUIT or an error: stop capturing
        }
        match msg.message {
            WM_INPUT => {
                pump.on_raw_input(msg.lParam);
                // Still dispatched: DefWindowProc performs the API's
                // required cleanup for WM_INPUT.
                // SAFETY: standard message dispatch.
                unsafe {
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
            WM_TIMER if msg.wParam.0 == WATCHDOG_TIMER_ID => pump.on_watchdog_tick(),
            WM_APP_START_CAPTURE => pump.on_start(),
            WM_APP_STOP_CAPTURE => pump.on_stop(),
            WM_APP_SHUTDOWN => break,
            _ => {
                // SAFETY: standard message dispatch.
                unsafe {
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
        }
    }

    pump.teardown_capture();
    // SAFETY: destroying the window this thread created.
    let _ = unsafe { DestroyWindow(hwnd) };
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use crossover_platform::{InputCapture, InputError, InputSink, PointerButton, PointerEvent};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    };
    use windows::Win32::UI::Input::{MOUSE_MOVE_ABSOLUTE, MOUSE_VIRTUAL_DESKTOP};
    use windows::Win32::UI::WindowsAndMessaging::{
        RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_HWHEEL, RI_MOUSE_LEFT_BUTTON_DOWN,
        RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_WHEEL,
    };

    use super::{
        AbsoluteTracker, HookVerdict, RawMouseReport, ScreenExtents, WindowsInputCapture,
        hook_lost, hook_verdict, is_our_injection, translate_raw_mouse,
    };
    use crate::input::CROSSOVER_INJECTION_TAG;

    /// Input capture is process-exclusive by design; serialize every
    /// test that installs the real hook.
    fn capture_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    const NO_EXTENTS: ScreenExtents = ScreenExtents {
        primary: (0, 0),
        virtual_desktop: (0, 0),
    };

    fn translate(report: &RawMouseReport, tracker: &mut AbsoluteTracker) -> Vec<PointerEvent> {
        let mut out = Vec::new();
        translate_raw_mouse(report, tracker, NO_EXTENTS, &mut out);
        out
    }

    fn relative(x: i32, y: i32, buttons: u32, data: u16) -> RawMouseReport {
        RawMouseReport {
            flags: 0,
            buttons,
            data,
            x,
            y,
        }
    }

    // ---- the hook's decision, without a hook ----

    /// The tag check is loop prevention (ADR 0007): our own injections
    /// must pass through unsuppressed no matter what state capture is
    /// in, or injection and capture on one machine would fight.
    #[test]
    fn tagged_events_always_pass() {
        assert_eq!(
            hook_verdict(CROSSOVER_INJECTION_TAG, true),
            HookVerdict::PassOurInjection
        );
        assert_eq!(
            hook_verdict(CROSSOVER_INJECTION_TAG, false),
            HookVerdict::PassOurInjection
        );
    }

    #[test]
    fn untagged_events_are_suppressed_only_while_capturing() {
        assert_eq!(hook_verdict(0, true), HookVerdict::Suppress);
        assert_eq!(hook_verdict(0, false), HookVerdict::Pass);
        // An arbitrary non-tag extra-info value (drivers stash things
        // here) is still user input.
        assert_eq!(hook_verdict(0xDEAD_BEEF, true), HookVerdict::Suppress);
    }

    #[test]
    fn raw_side_tag_check_matches_the_hook_side() {
        // The tag constant fits u32 by construction; if that ever
        // changes, the raw path would stop recognising injections.
        assert!(is_our_injection(0x584F_5652));
        assert!(!is_our_injection(0));
        assert!(!is_our_injection(0xDEAD_BEEF));
    }

    // ---- the watchdog's judgment, without losing a hook ----

    #[test]
    fn watchdog_declares_loss_only_on_unmatched_raw_flow() {
        // Dead hook, moving mouse: loss.
        assert!(hook_lost(0, 5));
        assert!(hook_lost(0, 500));
        // Idle machine: indistinguishable from healthy, so healthy.
        assert!(!hook_lost(0, 0));
        // Hook alive: never loss, whatever the skew.
        assert!(!hook_lost(1, 500));
        // Below threshold: could be tick-boundary skew, not loss.
        assert!(!hook_lost(0, 4));
    }

    // ---- translation ----

    #[test]
    fn relative_motion_passes_through_unscaled() {
        let mut tracker = AbsoluteTracker::default();
        let events = translate(&relative(7, -3, 0, 0), &mut tracker);
        assert_eq!(events, vec![PointerEvent::Motion { dx: 7, dy: -3 }]);
    }

    #[test]
    fn motionless_reports_emit_no_motion() {
        let mut tracker = AbsoluteTracker::default();
        let events = translate(&relative(0, 0, RI_MOUSE_LEFT_BUTTON_DOWN, 0), &mut tracker);
        assert_eq!(
            events,
            vec![PointerEvent::Button {
                button: PointerButton::Left,
                pressed: true,
            }]
        );
    }

    #[test]
    fn buttons_and_wheel_translate_in_fixed_order() {
        let mut tracker = AbsoluteTracker::default();
        let events = translate(
            &relative(
                2,
                0,
                RI_MOUSE_LEFT_BUTTON_UP | RI_MOUSE_BUTTON_5_DOWN | RI_MOUSE_WHEEL,
                120,
            ),
            &mut tracker,
        );
        assert_eq!(
            events,
            vec![
                PointerEvent::Motion { dx: 2, dy: 0 },
                PointerEvent::Button {
                    button: PointerButton::Left,
                    pressed: false,
                },
                PointerEvent::Button {
                    button: PointerButton::X2,
                    pressed: true,
                },
                PointerEvent::Scroll { dx: 0, dy: 120 },
            ]
        );
    }

    #[test]
    fn negative_wheel_survives_the_u16_reinterpretation() {
        let mut tracker = AbsoluteTracker::default();
        // -120 as the u16 the wire carries.
        let events = translate(
            &relative(0, 0, RI_MOUSE_WHEEL, (-120i16).cast_unsigned()),
            &mut tracker,
        );
        assert_eq!(events, vec![PointerEvent::Scroll { dx: 0, dy: -120 }]);
    }

    #[test]
    fn horizontal_wheel_maps_to_dx() {
        let mut tracker = AbsoluteTracker::default();
        let events = translate(&relative(0, 0, RI_MOUSE_HWHEEL, 120), &mut tracker);
        assert_eq!(events, vec![PointerEvent::Scroll { dx: 120, dy: 0 }]);
    }

    /// Absolute mode (RDP, tablets): the first sample only anchors;
    /// subsequent samples emit deltas scaled from the 0..=65535 space
    /// onto the screen extent.
    #[test]
    fn absolute_motion_becomes_deltas_after_an_anchor() {
        let mut tracker = AbsoluteTracker::default();
        let extents = ScreenExtents {
            primary: (1920, 1080),
            virtual_desktop: (0, 0),
        };
        let absolute = |x: i32, y: i32| RawMouseReport {
            flags: MOUSE_MOVE_ABSOLUTE.0,
            buttons: 0,
            data: 0,
            x,
            y,
        };

        let mut out = Vec::new();
        translate_raw_mouse(&absolute(32768, 32768), &mut tracker, extents, &mut out);
        assert!(out.is_empty(), "first absolute sample must only anchor");

        // 65536 units across 1920 pixels: 1024 units ≈ 30 pixels.
        translate_raw_mouse(&absolute(33792, 32768), &mut tracker, extents, &mut out);
        assert_eq!(out, vec![PointerEvent::Motion { dx: 30, dy: 0 }]);
    }

    #[test]
    fn virtual_desktop_flag_selects_the_virtual_extent() {
        let mut tracker = AbsoluteTracker::default();
        let extents = ScreenExtents {
            primary: (1920, 1080),
            virtual_desktop: (3840, 1080),
        };
        let report = |x: i32| RawMouseReport {
            flags: MOUSE_MOVE_ABSOLUTE.0 | MOUSE_VIRTUAL_DESKTOP.0,
            buttons: 0,
            data: 0,
            x,
            y: 0,
        };
        let mut out = Vec::new();
        translate_raw_mouse(&report(0), &mut tracker, extents, &mut out);
        translate_raw_mouse(&report(1024), &mut tracker, extents, &mut out);
        // 1024 units on a 3840-wide virtual desktop: 60 pixels, not 30.
        assert_eq!(out, vec![PointerEvent::Motion { dx: 60, dy: 0 }]);
    }

    #[test]
    fn returning_to_relative_mode_drops_the_absolute_anchor() {
        let mut tracker = AbsoluteTracker::default();
        let extents = ScreenExtents {
            primary: (1920, 1080),
            virtual_desktop: (0, 0),
        };
        let mut out = Vec::new();
        translate_raw_mouse(
            &RawMouseReport {
                flags: MOUSE_MOVE_ABSOLUTE.0,
                buttons: 0,
                data: 0,
                x: 100,
                y: 100,
            },
            &mut tracker,
            extents,
            &mut out,
        );
        translate_raw_mouse(&relative(1, 1, 0, 0), &mut tracker, extents, &mut out);
        assert!(tracker.last.is_none(), "relative report must clear anchor");
    }

    // ---- the real thing: hook + raw input on this machine's desktop ----
    //
    // These install the real WH_MOUSE_LL hook, so while one runs, real
    // local mouse input is briefly suppressed. Each holds capture for
    // well under a second. A live desktop can also feed real events into
    // the sink, so assertions use distinctive markers, never counts.

    fn collecting_sink() -> (InputSink, Arc<Mutex<Vec<PointerEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let sink: InputSink = Box::new(move |event| {
            sink_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        });
        (sink, seen)
    }

    /// A wheel event with a delta no human scroll produces mid-test,
    /// used as an unambiguous marker in both directions.
    const MARKER_WHEEL: i32 = 360;

    fn send_marker_wheel(extra_info: usize) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: MARKER_WHEEL.cast_unsigned(),
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: extra_info,
                },
            },
        };
        let size = i32::try_from(size_of::<INPUT>()).expect("INPUT size fits in i32");
        // SAFETY: one correctly initialised INPUT and its true size.
        let accepted = unsafe { SendInput(&[input], size) };
        assert_eq!(accepted, 1, "test SendInput was blocked");
    }

    fn saw_marker(seen: &Mutex<Vec<PointerEvent>>) -> bool {
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|e| matches!(e, PointerEvent::Scroll { dy, .. } if dy.abs() == MARKER_WHEEL))
    }

    #[test]
    fn capture_state_is_reported_truthfully() {
        let _serial = capture_lock();
        let capture = WindowsInputCapture::new().unwrap();
        assert!(!capture.is_capturing());

        let (sink, _seen) = collecting_sink();
        capture.start_capture(sink).unwrap();
        assert!(capture.is_capturing());

        // Idempotent restart replaces the sink and stays capturing.
        let (sink2, _seen2) = collecting_sink();
        capture.start_capture(sink2).unwrap();
        assert!(capture.is_capturing());

        capture.stop_capture().unwrap();
        assert!(!capture.is_capturing());
        // Stop when already stopped: the error-path contract.
        capture.stop_capture().unwrap();
        assert!(!capture.is_capturing());
    }

    #[test]
    fn second_active_capture_is_refused() {
        let _serial = capture_lock();
        let first = WindowsInputCapture::new().unwrap();
        let second = WindowsInputCapture::new().unwrap();

        let (sink, _seen) = collecting_sink();
        first.start_capture(sink).unwrap();

        let (sink2, _seen2) = collecting_sink();
        match second.start_capture(sink2) {
            Err(InputError::CaptureUnavailable { .. }) => {}
            other => panic!("second capture must be refused, got {other:?}"),
        }
        assert!(!second.is_capturing());

        // Releasing the first frees the slot for the second.
        first.stop_capture().unwrap();
        let (sink3, _seen3) = collecting_sink();
        second.start_capture(sink3).unwrap();
        assert!(second.is_capturing());
        second.stop_capture().unwrap();
    }

    /// End to end through the real pipeline: an untagged synthetic
    /// event must reach the sink (proving the whole hook, Raw Input,
    /// translate, delivery chain), and a tagged one must not (proving
    /// loop prevention, ADR 0007). Untagged input is also suppressed
    /// while this runs, so the test does not disturb the desktop it
    /// runs on.
    #[test]
    fn tagged_input_is_ignored_and_untagged_input_is_delivered() {
        let _serial = capture_lock();
        let capture = WindowsInputCapture::new().unwrap();
        let (sink, seen) = collecting_sink();
        capture.start_capture(sink).unwrap();

        // Our own injection: must never come back through the sink.
        send_marker_wheel(CROSSOVER_INJECTION_TAG);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !saw_marker(&seen),
            "tagged injection was captured back — input loop (ADR 0007)"
        );

        // Untagged input: must be captured.
        send_marker_wheel(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !saw_marker(&seen) {
            assert!(
                Instant::now() < deadline,
                "untagged input never reached the sink within 5s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        capture.stop_capture().unwrap();
    }

    /// Manual probe (docs/SOAK.md): run alone, on purpose, with a real
    /// mouse. Captures for ten seconds — the local pointer goes dead,
    /// which IS suppression working — then releases and reports what
    /// was observed. Move, click, and scroll during the window.
    ///
    /// ```text
    /// cargo test -p crossover-platform-windows manual_probe_capture -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "manual probe: freezes the local mouse for 10 seconds (docs/SOAK.md)"]
    fn manual_probe_capture() {
        let _serial = capture_lock();
        let capture = WindowsInputCapture::new().unwrap();
        let (sink, seen) = collecting_sink();

        eprintln!();
        eprintln!("capturing for 10 seconds: the mouse should go DEAD locally;");
        eprintln!("move it, click, and scroll anyway — events are being counted.");
        capture.start_capture(sink).unwrap();
        std::thread::sleep(Duration::from_secs(10));
        let healthy = capture.is_capturing();
        capture.stop_capture().unwrap();

        let events = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let motions = events
            .iter()
            .filter(|e| matches!(e, PointerEvent::Motion { .. }))
            .count();
        let buttons = events
            .iter()
            .filter(|e| matches!(e, PointerEvent::Button { .. }))
            .count();
        let scrolls = events
            .iter()
            .filter(|e| matches!(e, PointerEvent::Scroll { .. }))
            .count();
        eprintln!();
        eprintln!("released: the mouse should be alive again.");
        eprintln!(
            "observed while capturing: {motions} motion, {buttons} button, {scrolls} scroll \
             events; capture healthy at end: {healthy}"
        );
        eprintln!("if the cursor moved while \"dead\", suppression failed — that is a bug.");
        assert!(healthy, "capture reported unhealthy during the probe");
    }
}
