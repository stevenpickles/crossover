//! Hiding the local cursor by blanking the system cursors (ADR 0009).
//!
//! While this machine drives the peer its own cursor is frozen, pinned at
//! the linked edge (ADR 0007) — a second, motionless pointer.
//! [`WindowsCursorMask`] removes it by replacing every standard system
//! cursor with a fully transparent one via `SetSystemCursor`, restoring the
//! defaults when control ends.
//!
//! Why not the two obvious alternatives:
//! - `ShowCursor(FALSE)` only affects the calling thread's own windows, not
//!   the cursor sitting over other applications — useless for a background
//!   process.
//! - A transparent top-most overlay window with a null cursor works on a
//!   single monitor, but is fragile across **multiple monitors of different
//!   size and DPI**: one window spanning the mismatched virtual desktop, and
//!   a cursor that is frozen (so no `WM_SETCURSOR` fires without warping it,
//!   which then perturbs capture). The Phase 5 soak showed it hiding on a
//!   one-monitor machine and *not* on a two-monitor one.
//!
//! `SetSystemCursor` is geometry-, monitor-, and DPI-independent — it swaps
//! the cursor *image*, touching no window and never moving the pointer. Its
//! one cost is that a blanked system cursor does **not** revert when the
//! process dies, so a crash mid-control could leave the machine cursor-less.
//! That is mitigated on three fronts: the defaults are restored on every
//! exit from control and on shutdown, and — the self-heal — they are also
//! restored when a mask is *created*, so the next launch of Crossover (or a
//! sign-out, or any app that reloads cursors) undoes a crash's blanking.
//! Masking is a display nicety: a failure to hide or restore is logged and
//! never disturbs control.

use std::sync::atomic::{AtomicBool, Ordering};

use crossover_platform::{CursorMask, CursorMaskError};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateCursor, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO, OCR_NORMAL,
    OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT,
    SPI_SETCURSORS, SYSTEM_CURSOR_ID, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor,
    SystemParametersInfoW,
};

/// The standard system cursors to blank while controlling. Covers the ones
/// any application under the frozen pointer might be showing (arrow, text
/// caret, hand, resize arrows, …), so nothing peeks through.
const SYSTEM_CURSORS: &[SYSTEM_CURSOR_ID] = &[
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_WAIT,
    OCR_CROSS,
    OCR_UP,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_SIZEALL,
    OCR_NO,
    OCR_HAND,
    OCR_APPSTARTING,
    OCR_HELP,
];

/// A blank-system-cursor [`CursorMask`]. Idempotent: hiding while already
/// hidden re-blanks (harmless), restoring while already shown reloads the
/// defaults (harmless).
pub struct WindowsCursorMask {
    hidden: AtomicBool,
}

impl WindowsCursorMask {
    /// A mask with the cursor visible. Restores the default system cursors
    /// first, self-healing any blanking left behind by a prior crash.
    ///
    /// # Errors
    ///
    /// Never, currently — kept fallible so the caller's fallback-to-no-mask
    /// path stays uniform if a future backend can fail to initialize.
    pub fn new() -> Result<Self, CursorMaskError> {
        restore_system_cursors();
        Ok(Self {
            hidden: AtomicBool::new(false),
        })
    }
}

impl CursorMask for WindowsCursorMask {
    fn hide(&self) -> Result<(), CursorMaskError> {
        self.hidden.store(true, Ordering::SeqCst);
        tracing::debug!("cursor mask: hide (blank system cursors)");
        let mut blanked = 0usize;
        for &id in SYSTEM_CURSORS {
            let Some(blank) = blank_cursor() else {
                continue; // could not build one; the rest may still take
            };
            // SAFETY: `blank` is a valid cursor handle; SetSystemCursor
            // takes ownership of it (it destroys it), replacing the system
            // cursor `id`. A failure leaves that one cursor unchanged.
            if unsafe { SetSystemCursor(blank, id) }.is_ok() {
                blanked += 1;
            }
        }
        if blanked == 0 {
            return Err(CursorMaskError::Failed {
                reason: "SetSystemCursor blanked no cursors".to_owned(),
            });
        }
        Ok(())
    }

    fn show(&self) -> Result<(), CursorMaskError> {
        // Only reload if we blanked; reloading is global, so avoid doing it
        // needlessly, but always honor an explicit restore.
        if self.hidden.swap(false, Ordering::SeqCst) {
            tracing::debug!("cursor mask: show (restore system cursors)");
            restore_system_cursors();
        }
        Ok(())
    }
}

impl Drop for WindowsCursorMask {
    fn drop(&mut self) {
        if *self.hidden.get_mut() {
            restore_system_cursors();
        }
    }
}

/// Reload the default system cursors from the user's settings, undoing any
/// blanking (ours or a prior crash's). Best-effort; a failure is logged.
/// Public so the binary can call it **synchronously** on shutdown — a quit
/// or lost connection must never leave the machine with a blanked cursor,
/// and the async applier's restore cannot be relied on to run before the
/// process exits.
pub fn restore_system_cursors() {
    // SAFETY: SPI_SETCURSORS reloads the cursors and takes no in/out
    // parameter, so a null pvparam is correct.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_SETCURSORS,
            0,
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if let Err(error) = result {
        tracing::warn!(%error, "could not restore system cursors");
    }
}

/// A 32×32 fully transparent cursor (AND-mask all ones, XOR-mask all
/// zeros = every pixel transparent), or `None` if it could not be created.
fn blank_cursor() -> Option<windows::Win32::UI::WindowsAndMessaging::HCURSOR> {
    const SIDE: i32 = 32;
    const PLANE_BYTES: usize = (SIDE * SIDE / 8) as usize; // 128
    let and_plane = [0xFFu8; PLANE_BYTES];
    let xor_plane = [0x00u8; PLANE_BYTES];
    // SAFETY: CreateCursor reads SIDE×SIDE bits from each plane; both planes
    // are exactly that many bits. A null hinstance is valid. It returns an
    // error rather than faulting on failure.
    unsafe {
        CreateCursor(
            None,
            0,
            0,
            SIDE,
            SIDE,
            and_plane.as_ptr().cast(),
            xor_plane.as_ptr().cast(),
        )
    }
    .ok()
}

#[cfg(test)]
mod tests {
    use crossover_platform::CursorMask;

    use super::WindowsCursorMask;

    /// Constructing and toggling the mask must not panic; on a headless
    /// agent the underlying calls may no-op, which is fine.
    #[test]
    fn constructs_and_toggles_without_panicking() {
        let mask = WindowsCursorMask::new().expect("mask construction is infallible");
        let _ = mask.hide();
        let _ = mask.show();
        // Idempotent: a second show with nothing hidden is still fine.
        let _ = mask.show();
    }
}
