//! Hiding the local cursor with a transparent overlay window (ADR 0009).
//!
//! While this machine drives the peer its own cursor is frozen, pinned at
//! the linked edge (ADR 0007) — a second, motionless pointer. [`WindowsCursorMask`]
//! removes it with a full-desktop, fully transparent, top-most window that
//! sets a **null cursor** (`WM_SETCURSOR` → `SetCursor(None)`). Shown while
//! controlling, hidden on return.
//!
//! Why an overlay and not `SetSystemCursor`/`ShowCursor`:
//! - `ShowCursor(FALSE)` only affects the calling thread's own windows, not
//!   the cursor sitting over other applications — useless for a background
//!   process.
//! - `SetSystemCursor` with a blank cursor is *global* and, crucially, does
//!   **not** revert when the process dies: a crash mid-control would leave
//!   the machine with no cursor until the user reset it. The overlay is the
//!   inverse — the window is destroyed with the process, so the cursor
//!   always comes back (the hard requirement, the mirror of a stuck key).
//!
//! The window lives on its own thread with a message loop (a window needs
//! one), mirroring the capture pump. `hide`/`show` post a message to that
//! thread. Hiding is not just showing the overlay: while controlling, the
//! cursor is frozen (input is suppressed), so no mouse move ever reaches the
//! overlay and a bare `SetCursor(None)` from another thread does not stick.
//! The overlay thread therefore **warps the cursor onto the overlay** with
//! `SetCursorPos`, which generates a genuine mouse move → `WM_SETCURSOR` on
//! the thread owning the top-most window → the blank takes and holds. The
//! warp produces no Raw Input and is invisible to the capture hook, so
//! nothing is forwarded to the peer; the controlling machine's cursor
//! position is irrelevant to edge detection (idle while driving). The saved
//! position is restored when the overlay comes down.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use crossover_platform::{CursorMask, CursorMaskError};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, HCURSOR, LWA_ALPHA, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE,
    SW_SHOWNA, SetCursor, SetCursorPos, SetLayeredWindowAttributes, ShowWindow, TranslateMessage,
    WM_APP, WM_DESTROY, WM_SETCURSOR, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::w;

/// The cursor position saved when the overlay is shown, restored when it
/// comes down. Statics because the window procedure that reads and writes
/// them holds no state of its own, and there is only ever one overlay.
static SAVED_X: AtomicI32 = AtomicI32::new(0);
static SAVED_Y: AtomicI32 = AtomicI32::new(0);
/// Whether [`SAVED_X`]/[`SAVED_Y`] hold a position to restore.
static SAVED_VALID: AtomicBool = AtomicBool::new(false);

/// Post targets for the overlay thread. Show/hide are routed through the
/// overlay's own thread (not `ShowWindowAsync` from the caller) so the
/// cursor is blanked with `SetCursor` *there* — the thread owning the
/// now-topmost window — the instant it is shown, rather than waiting for a
/// mouse move that never comes while the cursor is frozen.
const WM_APP_SHOW: u32 = WM_APP + 1;
const WM_APP_HIDE: u32 = WM_APP + 2;
/// Ask the overlay thread to destroy its window and exit — the only way to
/// end a `GetMessage` loop from another thread.
const WM_APP_CLOSE: u32 = WM_APP + 3;

/// Transparent overlay that blanks the cursor while shown (ADR 0009).
///
/// [`CursorMask::hide`] shows the overlay (blanking the cursor);
/// [`CursorMask::show`] hides it (restoring the cursor). Both are
/// idempotent, as the trait requires.
pub struct WindowsCursorMask {
    /// The overlay window handle as an `isize`, `0` until the thread has
    /// created it. Shared with the overlay thread.
    hwnd: Arc<AtomicIsize>,
    /// The overlay thread; joined on drop.
    thread: Option<JoinHandle<()>>,
}

impl WindowsCursorMask {
    /// Create the overlay (hidden) and its thread. Blocks until the window
    /// exists.
    ///
    /// # Errors
    ///
    /// [`CursorMaskError::Failed`] if the thread or its window cannot be
    /// created — the caller falls back to no masking, never a failed run.
    pub fn new() -> Result<Self, CursorMaskError> {
        let (init_tx, init_rx) = mpsc::channel::<Result<isize, String>>();
        let hwnd = Arc::new(AtomicIsize::new(0));
        let hwnd_thread = Arc::clone(&hwnd);
        let thread = std::thread::Builder::new()
            .name("crossover-cursor-mask".to_owned())
            .spawn(move || overlay_thread(&hwnd_thread, &init_tx))
            .map_err(|e| CursorMaskError::Failed {
                reason: format!("spawning overlay thread: {e}"),
            })?;
        match init_rx.recv() {
            Ok(Ok(_)) => Ok(Self {
                hwnd,
                thread: Some(thread),
            }),
            Ok(Err(reason)) => {
                let _ = thread.join();
                Err(CursorMaskError::Failed { reason })
            }
            Err(e) => Err(CursorMaskError::Failed {
                reason: format!("overlay thread ended before creating its window: {e}"),
            }),
        }
    }
}

impl WindowsCursorMask {
    /// Post a message to the overlay thread's window. `false` if the window
    /// is not available (never created, or already torn down).
    fn post(&self, message: u32) -> bool {
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd == 0 {
            return false;
        }
        // SAFETY: PostMessageW is thread-safe by contract; it queues the
        // message to the overlay thread, which owns the window. A stale
        // handle fails harmlessly.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut core::ffi::c_void)),
                message,
                WPARAM(0),
                LPARAM(0),
            )
        };
        true
    }
}

impl CursorMask for WindowsCursorMask {
    fn hide(&self) -> Result<(), CursorMaskError> {
        tracing::debug!("cursor mask: hide (show overlay, blank cursor)");
        if self.post(WM_APP_SHOW) {
            Ok(())
        } else {
            Err(CursorMaskError::Failed {
                reason: "overlay window not available".to_owned(),
            })
        }
    }

    fn show(&self) -> Result<(), CursorMaskError> {
        tracing::debug!("cursor mask: show (hide overlay, restore cursor)");
        // A missing window means nothing was ever hidden — not an error.
        let _ = self.post(WM_APP_HIDE);
        Ok(())
    }
}

impl Drop for WindowsCursorMask {
    fn drop(&mut self) {
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            // SAFETY: PostMessageW is thread-safe; WM_APP_CLOSE tells the
            // overlay thread to destroy its window and quit its loop.
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(hwnd as *mut core::ffi::c_void)),
                    WM_APP_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The virtual desktop rectangle `(x, y, width, height)` in physical
/// pixels (the process is per-monitor DPI aware), so the overlay covers
/// every monitor.
fn virtual_desktop_rect() -> (i32, i32, i32, i32) {
    // SAFETY: GetSystemMetrics reads cached system values; no preconditions.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Register the class, create the (hidden) overlay, report readiness, then
/// pump messages until asked to close.
fn overlay_thread(hwnd_out: &Arc<AtomicIsize>, init: &mpsc::Sender<Result<isize, String>>) {
    // SAFETY: GetModuleHandleW(None) returns this module's handle with no
    // preconditions; it is the instance the window class is registered to.
    let instance = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => HINSTANCE(module.0),
        Err(e) => {
            let _ = init.send(Err(format!("GetModuleHandleW failed: {e}")));
            return;
        }
    };

    let class = WNDCLASSW {
        lpfnWndProc: Some(overlay_wndproc),
        hInstance: instance,
        lpszClassName: w!("CrossoverCursorMask"),
        // No class cursor: WM_SETCURSOR blanks it explicitly. hbrBackground
        // is null, so the window paints nothing — with alpha 0 it is fully
        // transparent regardless.
        hCursor: HCURSOR(std::ptr::null_mut()),
        ..Default::default()
    };
    // SAFETY: registering a class with a valid static window procedure. A
    // zero atom means the class already exists (a prior instance in this
    // process); CreateWindowExW below then reuses it, so we do not treat it
    // as fatal here.
    let _atom = unsafe { RegisterClassW(&raw const class) };

    let (x, y, width, height) = virtual_desktop_rect();
    // SAFETY: a top-level layered pop-up over the whole desktop. Not
    // WS_EX_TRANSPARENT — the window must remain a hit-test target so
    // WM_SETCURSOR reaches it; TOOLWINDOW keeps it out of Alt-Tab and
    // NOACTIVATE keeps it from taking focus.
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("CrossoverCursorMask"),
            w!("crossover-cursor-mask"),
            WS_POPUP,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            let _ = init.send(Err(format!("creating overlay window: {e}")));
            return;
        }
    };

    // Fully transparent (alpha 0): invisible, yet still a hit-test target.
    // SAFETY: hwnd is our just-created layered window.
    let _ = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA) };

    hwnd_out.store(hwnd.0 as isize, Ordering::SeqCst);
    let _ = init.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    loop {
        // SAFETY: standard message pump for this thread's window.
        let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if result.0 <= 0 {
            break; // 0 = WM_QUIT (from WM_DESTROY), -1 = error
        }
        // SAFETY: msg was just filled by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
    hwnd_out.store(0, Ordering::SeqCst);
}

/// Overlay window procedure: blank the cursor while the overlay is up, and
/// tear down cleanly on request.
unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_APP_SHOW => {
            // Show the overlay, then warp the cursor onto it so a real
            // WM_SETCURSOR fires on this thread and the blank sticks (see
            // the module docs). Save the current position first so it can
            // be restored when the overlay comes down.
            let mut point = POINT::default();
            // SAFETY: GetCursorPos writes into our local POINT.
            if unsafe { GetCursorPos(&raw mut point) }.is_ok() {
                SAVED_X.store(point.x, Ordering::SeqCst);
                SAVED_Y.store(point.y, Ordering::SeqCst);
                SAVED_VALID.store(true, Ordering::SeqCst);
            }
            let (x, y, width, height) = virtual_desktop_rect();
            // The desktop centre is unambiguously inside the overlay and
            // (barring the cursor already sitting exactly there) a real
            // move, which is what triggers WM_SETCURSOR.
            let (cx, cy) = (x + width / 2, y + height / 2);
            // SAFETY: hwnd is our window; SW_SHOWNA shows without stealing
            // focus, SetCursorPos moves the cursor onto the overlay, and
            // SetCursor(None) blanks it immediately for good measure.
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNA);
                let _ = SetCursorPos(cx, cy);
                SetCursor(None);
            }
            LRESULT(0)
        }
        WM_APP_HIDE => {
            // Restore the cursor to where it was before we warped it, then
            // take the overlay down so the cursor is drawn normally again.
            let restore = SAVED_VALID.swap(false, Ordering::SeqCst);
            // SAFETY: SetCursorPos/ShowWindow have no preconditions here; a
            // hidden overlay leaves the cursor to whatever window it is over.
            unsafe {
                if restore {
                    let _ = SetCursorPos(
                        SAVED_X.load(Ordering::SeqCst),
                        SAVED_Y.load(Ordering::SeqCst),
                    );
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // The warp on WM_APP_SHOW lands here (and so does any later move
            // while the overlay is up): blank the cursor. Returning TRUE
            // stops Windows resetting it to a class/parent cursor.
            // SAFETY: SetCursor(None) hides the cursor; no preconditions.
            unsafe { SetCursor(None) };
            LRESULT(1)
        }
        WM_APP_CLOSE => {
            // SAFETY: hwnd is our window; DestroyWindow yields WM_DESTROY.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: no preconditions; ends this thread's GetMessage loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: forwarding exactly the arguments we received.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use crossover_platform::CursorMask;

    use super::WindowsCursorMask;

    /// On a real session the overlay is created and can be toggled without
    /// panicking; on a headless agent with no window station creation fails
    /// cleanly. Either outcome is acceptable — a panic is not.
    #[test]
    fn constructs_and_toggles_without_panicking() {
        if let Ok(mask) = WindowsCursorMask::new() {
            assert!(mask.hide().is_ok());
            assert!(mask.show().is_ok());
            // Idempotent: a second show is still fine.
            assert!(mask.show().is_ok());
        }
    }
}
