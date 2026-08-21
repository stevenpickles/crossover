//! Virtual-desktop geometry and cursor position (ADR 0009).
//!
//! [`WindowsDisplayInfo`] reports the **virtual desktop** — every monitor
//! as one rectangle — via `GetSystemMetrics(SM_*VIRTUALSCREEN)`, the
//! per-monitor layout via `EnumDisplayMonitors` plus `GetMonitorInfoW` for
//! each monitor's `szDevice` identity (ADR 0018), and the cursor via
//! `GetCursorPos`, all normalized to the desktop's top-left. The two
//! monitor queries share one `EnumDisplayMonitors` sweep and differ only in
//! whether the per-monitor `GetMonitorInfoW` runs: `monitors()` is pure
//! geometry for the edge detector's hot path, and `monitor_layout()` adds
//! best-effort identity, so a monitor the OS declines to name is reported
//! unnamed and never dropped. The desktop
//! bounds keep the seam *between* two monitors from being treated as the
//! crossing edge (a primary-only region put a false edge at the primary's
//! boundary, so roaming onto the second monitor triggered spurious
//! transfers); the per-monitor layout lets core map a crossing against the
//! actual monitor on the linked edge, not the mismatched-resolution
//! bounding box (ADR 0009). All reads come from this process and (with
//! per-monitor DPI awareness, R-3) are real pixels; cross-machine mapping
//! goes through the fraction in core's topology model.

use crossover_platform::{
    CursorPoint, DisplayError, DisplayInfo, MonitorInfo, MonitorRect, Screen,
};
use windows::Win32::Foundation::{LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
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
        // Geometry only: no `GetMonitorInfoW`, no allocation per monitor
        // beyond the one `Vec`. This is polled continuously by the edge
        // detector, and it must not be able to lose a monitor to an
        // identity failure — a short list moves the desktop's outer edge
        // inward and turns an interior seam into a crossing edge.
        Ok(enumerate()?.into_iter().map(|found| found.rect).collect())
    }

    fn monitor_layout(&self) -> Result<Vec<MonitorInfo>, DisplayError> {
        // Same enumeration, same order, same rectangles — identity is a
        // second, per-monitor, best-effort pass laid over it (ADR 0018:
        // an unknown id degrades placement, never geometry).
        Ok(enumerate()?
            .into_iter()
            .map(|found| {
                let id = device_string(found.handle);
                if id.is_none() {
                    // NFR-3: an important fact silently absent is a fact
                    // nobody can diagnose. The rectangle names the monitor
                    // here because its name is precisely what is missing.
                    tracing::warn!(
                        left = found.rect.left,
                        top = found.rect.top,
                        width = found.rect.width,
                        height = found.rect.height,
                        "GetMonitorInfoW reported no device string; this monitor \
                         cannot be addressed by a drawn layout"
                    );
                }
                MonitorInfo {
                    id,
                    rect: found.rect,
                }
            })
            .collect())
    }
}

/// One monitor as the enumeration found it: the handle identity can later
/// be asked about, and the bounds every caller needs.
#[derive(Clone, Copy)]
struct FoundMonitor {
    handle: HMONITOR,
    rect: MonitorRect,
}

/// Every monitor the OS reports, normalized to the desktop origin and in a
/// canonical order.
///
/// The single enumeration behind both trait methods, so the geometry list
/// and the identified list can never disagree about which monitors exist —
/// they are the same list, projected twice.
fn enumerate() -> Result<Vec<FoundMonitor>, DisplayError> {
    let mut monitors: Vec<FoundMonitor> = Vec::new();
    // SAFETY: EnumDisplayMonitors synchronously invokes `collect_monitor`
    // once per monitor; the lparam carries a pointer to `monitors`, which
    // lives for the whole call, and the callback only pushes to it. A null
    // device context and clip rectangle enumerate all monitors.
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
    // successive layouts for equality (the edge detector re-primes on any
    // change), so an order flap between identical layouts must not read as
    // a change — sort into a canonical order here. `sort_by_key` is
    // stable, which is what makes both projections agree monitor for
    // monitor even if two ever shared an origin.
    monitors.sort_by_key(|found| (found.rect.left, found.rect.top));
    Ok(monitors)
}

/// `EnumDisplayMonitors` callback: append each monitor's handle and its
/// bounds, normalized to the desktop origin, to the `Vec` behind `lparam`.
///
/// Nothing is skipped for want of a *name*: the name is read later, per
/// monitor, and its absence is recorded on that monitor rather than
/// deleting it from the list (ADR 0018). The one thing that can still drop
/// a monitor is a rectangle the OS reports inverted, which is not a
/// monitor.
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: EnumDisplayMonitors passes the exact lparam we handed it — a
    // live `*mut Vec<FoundMonitor>` borrowed only for this synchronous call.
    let monitors = unsafe { &mut *(lparam.0 as *mut Vec<FoundMonitor>) };
    // SAFETY: `rect` is the valid, aligned monitor rectangle the OS
    // provides; we only read it.
    let rect = unsafe { *rect };
    let (origin_x, origin_y) = virtual_origin();
    if let (Ok(width), Ok(height)) = (
        u32::try_from(rect.right - rect.left),
        u32::try_from(rect.bottom - rect.top),
    ) {
        monitors.push(FoundMonitor {
            handle: monitor,
            rect: MonitorRect {
                left: rect.left - origin_x,
                top: rect.top - origin_y,
                width,
                height,
            },
        });
    }
    TRUE // keep enumerating
}

/// The `szDevice` name `GetMonitorInfoW` reports for `monitor` —
/// `\\.\DISPLAY1` and friends — or `None` if it cannot be read.
///
/// `szDevice` is a `WCHAR[CCHDEVICENAME]` that the OS NUL-terminates, so
/// the name is the run of units before the first NUL; a full field with no
/// terminator is taken whole rather than trusted to have one.
fn device_string(monitor: HMONITOR) -> Option<String> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(size_of::<MONITORINFOEXW>()).ok()?;
    // SAFETY: `GetMonitorInfoW` fills the structure behind the pointer,
    // whose size it takes from the `cbSize` we just set. The cast to
    // `MONITORINFO` is the documented calling convention for the EX form:
    // the struct begins with an embedded `MONITORINFO`, and the larger
    // `cbSize` is what tells the OS the device-name tail is there. `info`
    // is a live local for the whole call.
    let ok = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return None;
    }
    let name: Vec<u16> = info
        .szDevice
        .iter()
        .copied()
        .take_while(|&unit| unit != 0)
        .collect();
    if name.is_empty() {
        return None;
    }
    // Lossy rather than fallible: a device string is ASCII in practice,
    // and a name with one unpaired surrogate in it is still a better
    // identity than no identity — the layout model validates it before
    // trusting it either way.
    Some(String::from_utf16_lossy(&name))
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
    use crossover_topology::{MAX_MONITOR_ID_BYTES, validate_monitor_id};

    use super::WindowsDisplayInfo;

    /// On real hardware every enumerated monitor carries a device string
    /// the layout model will accept, those strings are unique, and the two
    /// queries report **the same rectangles in the same order**.
    ///
    /// The last clause is the one that matters most, and it is why the
    /// geometry list is fetched independently rather than derived: ADR
    /// 0018 requires an unknown id to cost placement and never geometry,
    /// so a `monitor_layout()` that quietly dropped a monitor the OS would
    /// not name must fail here.
    ///
    /// The id assertions go through `crossover_topology::validate_monitor_id`
    /// rather than restating its rules, so a `U+FFFD` from a lossy decode,
    /// an over-long name, or a control character is a test failure rather
    /// than a layout that silently cannot be saved. As with the test above,
    /// a headless agent has no display and both queries fail cleanly —
    /// an acceptable outcome; a panic is not.
    #[test]
    fn every_enumerated_monitor_is_identified_and_geometry_never_depends_on_it() {
        let display = WindowsDisplayInfo::new();
        let (Ok(monitors), Ok(geometry)) = (display.monitor_layout(), display.monitors()) else {
            return;
        };
        assert!(!monitors.is_empty());

        // Same monitors, same order, whether or not identity was readable.
        assert_eq!(
            geometry,
            monitors.iter().map(|m| m.rect).collect::<Vec<_>>(),
            "the identified enumeration lost or reordered a monitor"
        );

        for monitor in &monitors {
            assert!(monitor.rect.width > 0 && monitor.rect.height > 0);
            let id = monitor
                .id
                .as_deref()
                .expect("Windows reported no device string for an enumerated monitor");
            // The bound and the charset rule as the layout model states
            // them — imported, not restated.
            assert!(
                validate_monitor_id(id).is_ok(),
                "device string {id:?} is not a usable monitor id"
            );
            assert!(id.len() <= MAX_MONITOR_ID_BYTES, "{id:?}");
        }

        let mut ids: Vec<&str> = monitors.iter().filter_map(|m| m.id.as_deref()).collect();
        ids.sort_unstable();
        let unique = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), unique, "two monitors share a device string");
    }

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
