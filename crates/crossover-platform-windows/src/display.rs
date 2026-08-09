//! Primary-display geometry and cursor position (ADR 0009).
//!
//! [`WindowsDisplayInfo`] reports the primary monitor's pixel size with
//! `GetSystemMetrics` and the cursor with `GetCursorPos`. Both are read
//! from this process, so they share its DPI context and edge detection
//! compares like with like (R-3); cross-machine mapping never uses these
//! pixels directly — it goes through the fraction in core's topology
//! model (ADR 0009).

use crossover_platform::{CursorPoint, DisplayError, DisplayInfo, Screen};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

/// Win32 [`DisplayInfo`]. Stateless — the queries read live system state.
#[derive(Debug, Default)]
pub struct WindowsDisplayInfo;

impl WindowsDisplayInfo {
    /// A new display-info provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl DisplayInfo for WindowsDisplayInfo {
    fn primary_screen(&self) -> Result<Screen, DisplayError> {
        // SAFETY: GetSystemMetrics reads cached system values; it has no
        // preconditions and returns 0 for an unavailable metric.
        let (width, height) =
            unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        // A non-positive primary size is "no usable display", not a
        // zero-sized screen: fail rather than hand back a degenerate one.
        let width = u32::try_from(width).ok().filter(|&w| w > 0);
        let height = u32::try_from(height).ok().filter(|&h| h > 0);
        match (width, height) {
            (Some(width), Some(height)) => Ok(Screen { width, height }),
            _ => Err(DisplayError::Unavailable {
                reason: "GetSystemMetrics reported no usable primary display".to_owned(),
            }),
        }
    }

    fn cursor_position(&self) -> Result<CursorPoint, DisplayError> {
        let mut point = POINT::default();
        // SAFETY: GetCursorPos writes the cursor position into the local
        // POINT we pass; on failure it returns an error and leaves it
        // untouched, which the `?` propagates.
        unsafe { GetCursorPos(&raw mut point) }.map_err(|e| DisplayError::Unavailable {
            reason: format!("GetCursorPos failed: {e}"),
        })?;
        Ok(CursorPoint {
            x: point.x,
            y: point.y,
        })
    }
}

#[cfg(test)]
mod tests {
    use crossover_platform::DisplayInfo;

    use super::WindowsDisplayInfo;

    /// On a real Windows session the primary display has a positive size
    /// and the cursor reports a position. On a headless agent with no
    /// display the query fails cleanly rather than panicking — both
    /// outcomes are acceptable; a panic or a zero-sized success is not.
    #[test]
    fn reports_a_plausible_display_and_cursor() {
        let display = WindowsDisplayInfo::new();
        // On a headless agent with no display the query fails cleanly
        // rather than panicking, which is acceptable; a real session
        // reports a positive size and a cursor position. Either way,
        // reaching here without a panic is itself part of the assertion.
        if let Ok(screen) = display.primary_screen() {
            assert!(screen.width > 0 && screen.height > 0);
            assert!(display.cursor_position().is_ok());
        }
    }
}
