//! Win32 [`ClipboardProvider`] (FR-3.1; platform risks R-4/R-5 in
//! docs/SPECIFICATION.md §6).
//!
//! Change observation uses the modern listener API
//! (`AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE`) on a dedicated
//! thread owning a message-only window — robust to other clipboard
//! listeners by construction, unlike the legacy viewer chain (R-4).
//! `WM_CLIPBOARDUPDATE` fires once per clipboard state change, including
//! for our own writes — exactly the contract term `ClipboardProvider`
//! documents. Sequence-number polling is unnecessary on top of it: the
//! engine's content hashing already collapses spurious wakeups.
//!
//! Contention (R-5): `OpenClipboard` failing is routine — another process
//! holds the clipboard — and maps to [`ClipboardError::Busy`] for the
//! engine's bounded retry. Real failures map to `Unavailable`.
//!
//! Text is `CF_UNICODETEXT` (Windows synthesizes it from `CF_TEXT`
//! automatically), converted UTF-16 ↔ UTF-8 at this boundary.

use std::sync::{Arc, Mutex, PoisonError};

use crossover_platform::{ClipboardError, ClipboardListener, ClipboardProvider};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE,
};
use windows::core::w;

/// Private message asking the pump thread to shut down.
const WM_APP_SHUTDOWN: u32 = WM_APP + 1;

type SharedListener = Arc<Mutex<Option<ClipboardListener>>>;

/// The Win32 clipboard provider. Owns the observation thread; dropping it
/// shuts the thread down.
pub struct WindowsClipboard {
    listener: SharedListener,
    /// The message-only window, as a raw value so it is `Send` (used only
    /// to post the shutdown message; the pump thread owns the window).
    hwnd_raw: isize,
    pump: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: `hwnd_raw` is only used with PostMessageW (thread-safe by API
// contract); the window itself is owned and destroyed by the pump thread.
unsafe impl Send for WindowsClipboard {}
// SAFETY: all shared state (`listener`) is Mutex-guarded; `hwnd_raw` is
// an inert value between the thread-safe PostMessageW uses.
unsafe impl Sync for WindowsClipboard {}

impl WindowsClipboard {
    /// Start the observation thread and create the provider.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Unavailable`] if the observation window or
    /// listener registration fails — fatal by the trait contract, since
    /// silent non-observation would be silent sync failure (NFR-3).
    pub fn new() -> Result<Self, ClipboardError> {
        let listener: SharedListener = Arc::new(Mutex::new(None));
        let pump_listener = Arc::clone(&listener);
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<isize, String>>();

        let pump = std::thread::Builder::new()
            .name("crossover-clipboard-pump".to_owned())
            .spawn(move || pump_thread(&pump_listener, &init_tx))
            .map_err(|e| ClipboardError::Unavailable {
                reason: format!("spawning clipboard pump thread: {e}"),
            })?;

        let hwnd_raw = init_rx
            .recv()
            .map_err(|_| ClipboardError::Unavailable {
                reason: "clipboard pump thread died during startup".to_owned(),
            })?
            .map_err(|reason| ClipboardError::Unavailable { reason })?;

        Ok(Self {
            listener,
            hwnd_raw,
            pump: Some(pump),
        })
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        let hwnd = HWND(self.hwnd_raw as *mut core::ffi::c_void);
        // SAFETY: PostMessageW is safe to call from any thread with any
        // window handle; a stale handle at worst fails harmlessly.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0)) };
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

impl ClipboardProvider for WindowsClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        // SAFETY: no arguments; checks format availability only.
        if unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_err() {
            return Ok(None); // empty clipboard or no text representation
        }

        let _open = OpenGuard::open()?;
        // SAFETY: the clipboard is open (guard); the returned handle is
        // owned by the clipboard, not by us.
        // Contention-shaped, observed live: clipboard ownership can churn
        // between our successful open and this call (another process
        // taking the clipboard), surfacing as ERROR_CLIPBOARD_NOT_OPEN.
        // Retryable, not fatal (R-5).
        let handle = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }.map_err(|e| {
            ClipboardError::Busy {
                reason: format!("GetClipboardData failed (ownership churn?): {e}"),
            }
        })?;
        if handle.is_invalid() {
            return Ok(None);
        }

        let hglobal = HGLOBAL(handle.0);
        // SAFETY: `hglobal` came from GetClipboardData while the clipboard
        // is open; GlobalLock pins it and yields the base pointer.
        let ptr = unsafe { GlobalLock(hglobal) }.cast::<u16>();
        if ptr.is_null() {
            // Same churn window as above: the block can vanish with its
            // owner. Retryable.
            return Err(ClipboardError::Busy {
                reason: "GlobalLock on clipboard data failed".to_owned(),
            });
        }
        // SAFETY: CF_UNICODETEXT is null-terminated UTF-16 by contract;
        // scan for the terminator from the locked base pointer.
        let text = unsafe {
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
        };
        // SAFETY: balances the successful GlobalLock above. GlobalUnlock
        // reports "no longer locked" as an error-shaped success; ignore.
        let _ = unsafe { GlobalUnlock(hglobal) };
        Ok(Some(text))
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = utf16.len() * 2;

        let _open = OpenGuard::open()?;
        // SAFETY: allocating a movable global block for the clipboard.
        let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) }.map_err(|e| {
            ClipboardError::Unavailable {
                reason: format!("GlobalAlloc failed: {e}"),
            }
        })?;
        // SAFETY: `hglobal` is ours and unlocked; lock, copy the UTF-16
        // (exactly `utf16.len()` units fit by construction), unlock.
        unsafe {
            let ptr = GlobalLock(hglobal).cast::<u16>();
            if ptr.is_null() {
                return Err(ClipboardError::Unavailable {
                    reason: "GlobalLock on fresh allocation failed".to_owned(),
                });
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
            let _ = GlobalUnlock(hglobal);
        }

        // SAFETY: the clipboard is open (guard).
        unsafe { EmptyClipboard() }.map_err(|e| ClipboardError::Busy {
            reason: format!("EmptyClipboard failed (ownership churn?): {e}"),
        })?;
        // SAFETY: on success the system takes ownership of `hglobal`; we
        // must not free it. On failure ownership stays with us — the
        // block leaks rather than risking a double-free; failure here is
        // rare and the leak is bounded by the item size.
        unsafe { SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(hglobal.0))) }.map_err(
            |e| ClipboardError::Busy {
                reason: format!("SetClipboardData failed (ownership churn?): {e}"),
            },
        )?;
        Ok(())
    }

    fn set_change_listener(
        &self,
        listener: Option<ClipboardListener>,
    ) -> Result<(), ClipboardError> {
        *self.listener.lock().unwrap_or_else(PoisonError::into_inner) = listener;
        Ok(())
    }
}

/// RAII for `OpenClipboard`/`CloseClipboard`. Open failure is `Busy`:
/// another process holding the clipboard is routine contention (R-5).
struct OpenGuard;

impl OpenGuard {
    fn open() -> Result<Self, ClipboardError> {
        // SAFETY: opening with no owning window associates the open with
        // the calling thread; EmptyClipboard then assigns no owner, which
        // is correct for immediate (non-delayed) rendering.
        unsafe { OpenClipboard(None) }.map_err(|e| ClipboardError::Busy {
            reason: format!("OpenClipboard failed (clipboard held elsewhere?): {e}"),
        })?;
        Ok(Self)
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful OpenClipboard in `open`.
        let _ = unsafe { CloseClipboard() };
    }
}

/// The observation thread: create a message-only window, register the
/// clipboard listener, pump messages, invoke the callback per
/// `WM_CLIPBOARDUPDATE`.
fn pump_thread(listener: &SharedListener, init: &std::sync::mpsc::Sender<Result<isize, String>>) {
    // SAFETY: creating a message-only window from the prebuilt STATIC
    // class (no custom window procedure needed; we read our messages from
    // the queue directly).
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("crossover-clipboard-observer"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            let _ = init.send(Err(format!("creating observer window: {e}")));
            return;
        }
    };

    // SAFETY: `hwnd` is a live window owned by this thread.
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        // SAFETY: destroying the window this thread created.
        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = init.send(Err(format!("registering clipboard listener: {e}")));
        return;
    }
    let _ = init.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    loop {
        // SAFETY: standard message pump for this thread's queue.
        let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if result.0 <= 0 {
            break; // WM_QUIT or an error: stop observing
        }
        match msg.message {
            WM_CLIPBOARDUPDATE => {
                if let Some(callback) = listener
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    // Contract: quick and non-blocking; the driver's
                    // bridge is a try_send.
                    callback();
                }
            }
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

    // SAFETY: tearing down what this thread set up, in reverse order.
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = DestroyWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use crossover_platform::ClipboardProvider;

    use super::WindowsClipboard;

    /// The Windows clipboard is machine-global: serialize every test that
    /// touches it, across this whole test binary.
    fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn write_then_read_round_trips_unicode() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let text = "crossover test: héllo 👋 line\r\nbreak";
        clipboard.write_text(text).unwrap();
        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some(text));
    }

    #[test]
    fn own_writes_trigger_the_change_listener() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let notifications = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&notifications);
        clipboard
            .set_change_listener(Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap();

        clipboard.write_text("notify me").unwrap();

        // WM_CLIPBOARDUPDATE arrives asynchronously on the pump thread.
        let deadline = Instant::now() + Duration::from_secs(5);
        while notifications.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "no change notification within 5s of our own write"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn replacing_content_keeps_working_across_instances() {
        let _serial = clipboard_lock();
        {
            let clipboard = WindowsClipboard::new().unwrap();
            clipboard.write_text("first instance").unwrap();
        } // drops: pump thread must shut down cleanly

        let clipboard = WindowsClipboard::new().unwrap();
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("first instance")
        );
        clipboard.write_text("second instance").unwrap();
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("second instance")
        );
    }
}
