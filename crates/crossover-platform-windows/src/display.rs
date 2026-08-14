//! Virtual-desktop geometry and cursor position (ADR 0009).
//!
//! [`WindowsDisplayInfo`] reports the **virtual desktop** — every monitor
//! as one rectangle — via `GetSystemMetrics(SM_*VIRTUALSCREEN)`, the
//! per-monitor layout via `EnumDisplayMonitors`, and the cursor via
//! `GetCursorPos`, all normalized to the desktop's top-left. The desktop
//! bounds keep the seam *between* two monitors from being treated as the
//! crossing edge (a primary-only region put a false edge at the primary's
//! boundary, so roaming onto the second monitor triggered spurious
//! transfers); the per-monitor layout lets core map a crossing against the
//! actual monitor on the linked edge, not the mismatched-resolution
//! bounding box (ADR 0009). All reads come from this process and (with
//! per-monitor DPI awareness, R-3) are real pixels; cross-machine mapping
//! goes through the fraction in core's topology model.

use crossover_platform::{CursorPoint, DisplayError, DisplayInfo, MonitorRect, Screen};
use windows::Win32::Foundation::{LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::BOOL;

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

/// The virtual desktop's top-left origin, subtracted from the raw cursor
/// so both live in a `0`-origin space. A monitor left of or above the
/// primary makes these negative, which is exactly why the normalization
/// is needed.
fn virtual_origin() -> (i32, i32) {
    // SAFETY: GetSystemMetrics reads cached system values; no preconditions.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

impl DisplayInfo for WindowsDisplayInfo {
    fn desktop_bounds(&self) -> Result<Screen, DisplayError> {
        // SAFETY: GetSystemMetrics reads cached system values; it has no
        // preconditions and returns 0 for an unavailable metric.
        let (width, height) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        // A non-positive size is "no usable desktop", not a zero-sized
        // one: fail rather than hand back a degenerate screen.
        let width = u32::try_from(width).ok().filter(|&w| w > 0);
        let height = u32::try_from(height).ok().filter(|&h| h > 0);
        match (width, height) {
            (Some(width), Some(height)) => Ok(Screen { width, height }),
            _ => Err(DisplayError::Unavailable {
                reason: "GetSystemMetrics reported no usable virtual desktop".to_owned(),
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
        let (origin_x, origin_y) = virtual_origin();
        Ok(CursorPoint {
            x: point.x - origin_x,
            y: point.y - origin_y,
        })
    }

    fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError> {
        let mut monitors: Vec<MonitorRect> = Vec::new();
        // SAFETY: EnumDisplayMonitors synchronously invokes `collect_monitor`
        // once per monitor; the lparam carries a pointer to `monitors`,
        // which lives for the whole call, and the callback only pushes to
        // it. A null device context and clip rectangle enumerate all
        // monitors.
        let ok = unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(collect_monitor),
                LPARAM(&raw mut monitors as isize),
            )
        };
        if !ok.as_bool() || monitors.is_empty() {
            return Err(DisplayError::Unavailable {
                reason: "EnumDisplayMonitors reported no monitors".to_owned(),
            });
        }
        // EnumDisplayMonitors guarantees no order. Consumers compare
        // successive layouts for equality (the edge detector re-primes on
        // any change), so an order flap between identical layouts must not
        // read as a change — sort into a canonical order here.
        monitors.sort_by_key(|m| (m.left, m.top));
        Ok(monitors)
    }
}

/// `EnumDisplayMonitors` callback: append each monitor's bounds,
/// normalized to the desktop origin, to the `Vec` behind `lparam`.
unsafe extern "system" fn collect_monitor(
    _monitor: HMONITOR,
    _hdc: HDC,
    rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: EnumDisplayMonitors passes the exact lparam we handed it — a
    // live `*mut Vec<MonitorRect>` borrowed only for this synchronous call.
    let monitors = unsafe { &mut *(lparam.0 as *mut Vec<MonitorRect>) };
    // SAFETY: `rect` is the valid, aligned monitor rectangle the OS
    // provides; we only read it.
    let rect = unsafe { *rect };
    let (origin_x, origin_y) = virtual_origin();
    if let (Ok(width), Ok(height)) = (
        u32::try_from(rect.right - rect.left),
        u32::try_from(rect.bottom - rect.top),
    ) {
        monitors.push(MonitorRect {
            left: rect.left - origin_x,
            top: rect.top - origin_y,
            width,
            height,
        });
    }
    TRUE // keep enumerating
}

/// The virtual desktop's top-left origin in absolute screen coordinates,
/// so a normalized [`CursorPoint`] can be turned back into the absolute
/// coordinates `SetCursorPos` expects. Shared with the injector.
#[must_use]
pub fn desktop_origin() -> (i32, i32) {
    virtual_origin()
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
        if let Ok(screen) = display.desktop_bounds() {
            assert!(screen.width > 0 && screen.height > 0);
            assert!(display.cursor_position().is_ok());
        }
    }
}
