//! Virtual-desktop geometry and cursor position (ADR 0009).
//!
//! [`WindowsDisplayInfo`] reports the **virtual desktop** — every monitor
//! as one rectangle — via `GetSystemMetrics(SM_*VIRTUALSCREEN)`, the
//! per-monitor layout via `EnumDisplayMonitors` plus `GetMonitorInfoW` for
//! each monitor's `szDevice` identity (ADR 0018), and the cursor via
//! `GetCursorPos`, all normalized to the desktop's top-left. The three
//! monitor queries share one `EnumDisplayMonitors` sweep and differ only in
//! how much they lay over it: `monitors()` is pure geometry for the edge
//! detector's hot path, `monitor_layout()` adds best-effort identity, so a
//! monitor the OS declines to name is reported unnamed and never dropped,
//! and `monitor_descriptions()` adds the EDID product name on top of that.
//!
//! The product name is a **second sweep**, `QueryDisplayConfig` rather than
//! `EnumDisplayMonitors`, because it is the only Win32 route to the string
//! Windows Settings shows. It is far more expensive than the geometry
//! enumeration, which is precisely why it sits behind its own trait method
//! and why the ~8 ms edge poll never reaches it. The two sweeps are joined
//! by `szDevice` — the DisplayConfig source's `viewGdiDeviceName` is the
//! same string `GetMonitorInfoW` reports — never by enumeration position.
//! A failure anywhere in it costs captions and nothing else. The desktop
//! bounds keep the seam *between* two monitors from being treated as the
//! crossing edge (a primary-only region put a false edge at the primary's
//! boundary, so roaming onto the second monitor triggered spurious
//! transfers); the per-monitor layout lets core map a crossing against the
//! actual monitor on the linked edge, not the mismatched-resolution
//! bounding box (ADR 0009). All reads come from this process and (with
//! per-monitor DPI awareness, R-3) are real pixels; cross-machine mapping
//! goes through the fraction in core's topology model.

use std::sync::atomic::{AtomicBool, Ordering};

use crossover_platform::{
    CursorPoint, DisplayError, DisplayInfo, MonitorDescription, MonitorInfo, MonitorRect, Screen,
};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LPARAM, POINT, RECT, TRUE,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::BOOL;

/// Win32 [`DisplayInfo`]. Effectively stateless — the queries read live
/// system state; the one field is a log-once latch, not cached data.
#[derive(Debug, Default)]
pub struct WindowsDisplayInfo {
    /// Whether the current run of failed label lookups has already been
    /// logged. `QueryDisplayConfig` failing is a nuisance, not an error —
    /// the caller loses captions and nothing else — and this query runs
    /// once a second, so an unlatched log line would be a debug line every
    /// second for as long as the condition lasts.
    label_lookup_failed: AtomicBool,
}

impl WindowsDisplayInfo {
    /// A new display-info provider.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that a label lookup failed, logging `reason` only on the first
    /// failure of a streak.
    fn note_label_failure(&self, reason: &str) {
        if !self.label_lookup_failed.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                reason,
                "could not read monitor product names; the editor will caption \
                 monitors by device string until this clears"
            );
        }
    }

    /// Note that a label lookup succeeded, so the next failure logs again.
    fn note_label_success(&self) {
        self.label_lookup_failed.store(false, Ordering::Relaxed);
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

    fn monitor_descriptions(&self) -> Result<Vec<MonitorDescription>, DisplayError> {
        // The identified enumeration is the spine: same monitors, same
        // order, same rectangles. Labels are a third best-effort pass laid
        // over it, joined by the very device string `monitor_layout()`
        // already read — `viewGdiDeviceName` from a DisplayConfig source is
        // literally `szDevice`, which is what makes the join exact rather
        // than positional.
        let layout = self.monitor_layout()?;
        let labels = match friendly_names() {
            Ok(labels) => {
                self.note_label_success();
                labels
            }
            Err(reason) => {
                // Never an error and never a panic: a failure here costs
                // captions, and a monitor with no caption still draws, still
                // crosses, and is still addressable by its id.
                self.note_label_failure(&reason);
                Vec::new()
            }
        };

        Ok(layout
            .into_iter()
            .map(|info| {
                let label = info.id.as_ref().and_then(|id| {
                    labels
                        .iter()
                        .find(|(device, _)| device == id)
                        .map(|(_, label)| label.clone())
                });
                MonitorDescription { info, label }
            })
            .collect())
    }
}

/// How many active display paths this build will ask the OS to describe.
///
/// A real desk has a handful; `MAX_MONITORS_PER_MACHINE` upstream is 16.
/// This is two orders of magnitude of headroom over any of that, and exists
/// so an implausible count from a driver costs a comparison rather than a
/// multi-megabyte allocation — the house rule applied to a value that is
/// not network input but is still someone else's number.
const MAX_DISPLAY_CONFIG_PATHS: u32 = 1024;

/// How many times to retry the size-then-query pair before giving up.
///
/// `GetDisplayConfigBufferSizes` and `QueryDisplayConfig` are two calls
/// with a window between them, and a display arriving or leaving in that
/// window makes the second one return `ERROR_INSUFFICIENT_BUFFER` — the
/// documented, expected race. Retrying re-reads the sizes; a small bound
/// keeps a display configuration changing continuously from spinning here.
const DISPLAY_CONFIG_ATTEMPTS: usize = 8;

/// Every active display path's `(szDevice, friendly name)` pair, as
/// Windows' own display configuration reports them.
///
/// `viewGdiDeviceName` from `DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME`
/// **is** the `szDevice` string `GetMonitorInfoW` reports, which is what
/// lets this join onto the monitor ids ADR 0018 already uses; the friendly
/// name is `monitorFriendlyDeviceName` from
/// `DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME`, the EDID product name
/// Windows Settings shows.
///
/// Best effort throughout: a path whose source or target will not answer,
/// or whose friendly name is empty, contributes nothing and stops nothing.
/// The `Err` case is reserved for a failure of the *sweep* — the caller
/// logs it once per streak and carries on with no labels at all.
fn friendly_names() -> Result<Vec<(String, String)>, String> {
    let (paths, _modes) = query_display_config()?;

    let mut named: Vec<(String, String)> = Vec::with_capacity(paths.len());
    for path in &paths {
        let Some(device) = source_device_name(path) else {
            continue;
        };
        // A source can drive several targets (clone mode). The first target
        // that answers names the screen; a second name for the same
        // `szDevice` would be a second caption for one rectangle, which the
        // model has no way to show.
        if named.iter().any(|(held, _)| held == &device) {
            continue;
        }
        if let Some(label) = target_friendly_name(path) {
            named.push((device, label));
        }
    }
    Ok(named)
}

/// The active display paths, sized and fetched with the documented retry.
///
/// The buffer can grow *between* the two calls — a monitor plugged in at
/// exactly the wrong moment — so `ERROR_INSUFFICIENT_BUFFER` re-reads the
/// sizes rather than failing (Microsoft documents this loop).
fn query_display_config()
-> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
    for _ in 0..DISPLAY_CONFIG_ATTEMPTS {
        let mut path_count: u32 = 0;
        let mut mode_count: u32 = 0;
        // SAFETY: both out-parameters are live locals for the whole call;
        // the function only writes counts into them.
        let sized = unsafe {
            GetDisplayConfigBufferSizes(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                &raw mut mode_count,
            )
        };
        if sized != ERROR_SUCCESS {
            return Err(format!("GetDisplayConfigBufferSizes failed: {sized:?}"));
        }
        if path_count == 0 {
            // No active paths is a legitimate answer (a locked or headless
            // session), and an empty label set is exactly right for it.
            return Ok((Vec::new(), Vec::new()));
        }
        // Bound before allocating, as everywhere else.
        if path_count > MAX_DISPLAY_CONFIG_PATHS {
            return Err(format!(
                "the display configuration claims {path_count} active paths, over the \
                 {MAX_DISPLAY_CONFIG_PATHS} this build will describe"
            ));
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: the two count variables hold the true element counts of
        // the two buffers we pass, both live for the whole call; the OS
        // writes at most that many elements and updates the counts with how
        // many it actually wrote. `None` declines the optional topology
        // out-parameter.
        let queried = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                paths.as_mut_ptr(),
                &raw mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if queried == ERROR_INSUFFICIENT_BUFFER {
            // The configuration changed between the two calls. Re-read.
            continue;
        }
        if queried != ERROR_SUCCESS {
            return Err(format!("QueryDisplayConfig failed: {queried:?}"));
        }
        // The OS may have written fewer elements than it sized for.
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Ok((paths, modes));
    }
    Err(format!(
        "the display configuration kept changing across {DISPLAY_CONFIG_ATTEMPTS} attempts"
    ))
}

/// The `szDevice` string for `path`'s source (`\\.\DISPLAY1`), or `None`
/// where the OS declines to answer or reports an empty name.
fn source_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: u32::try_from(size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>()).ok()?,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: `DisplayConfigGetDeviceInfo` fills the packet behind the
    // header pointer, using the `size` we just set to know how much of it
    // there is — the documented calling convention for every
    // `DISPLAYCONFIG_*` request struct, which all begin with an embedded
    // header. `request` is a live local for the whole call.
    let status = unsafe { DisplayConfigGetDeviceInfo((&raw mut request).cast()) };
    if status != 0 {
        return None;
    }
    wide_string(&request.viewGdiDeviceName)
}

/// The EDID product name for `path`'s target (`DELL U2720Q`), or `None`
/// where the OS declines to answer or reports an empty name.
fn target_friendly_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut request = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: u32::try_from(size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>()).ok()?,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: as `source_device_name` above — same documented convention,
    // same live local, a different request type and size.
    let status = unsafe { DisplayConfigGetDeviceInfo((&raw mut request).cast()) };
    if status != 0 {
        return None;
    }
    wide_string(&request.monitorFriendlyDeviceName)
}

/// A fixed-size `WCHAR` field as a `String`: the run of units before the
/// first NUL, trimmed, or `None` when that leaves nothing.
///
/// Trimming matters because some drivers pad the friendly name with
/// spaces, and a caption of `"DELL U2720Q   "` would look like a rendering
/// bug. Lossy decoding for the reason `device_string` gives: a name with
/// one unpaired surrogate in it is still a better caption than none, and
/// the layout model validates it before trusting it either way.
fn wide_string(units: &[u16]) -> Option<String> {
    let name: Vec<u16> = units
        .iter()
        .copied()
        .take_while(|&unit| unit != 0)
        .collect();
    if name.is_empty() {
        return None;
    }
    let text = String::from_utf16_lossy(&name).trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
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

    /// Descriptions are the identified enumeration with labels laid over
    /// it: never a monitor lost, never one reordered, and every label the
    /// layout model would accept.
    ///
    /// The last clause is what stops a driver's padded or exotic product
    /// name reaching the wire as something a peer would be right to
    /// refuse — imported from `crossover_topology` rather than restated, so
    /// the two cannot drift. A headless agent has no display and every
    /// query fails cleanly, which is an acceptable outcome; a panic is not.
    #[test]
    fn descriptions_are_the_identified_enumeration_with_usable_labels() {
        use crossover_topology::{MAX_MONITOR_LABEL_BYTES, validate_monitor_label};

        let display = WindowsDisplayInfo::new();
        let (Ok(descriptions), Ok(layout)) =
            (display.monitor_descriptions(), display.monitor_layout())
        else {
            return;
        };

        assert_eq!(
            descriptions
                .iter()
                .map(|description| description.info.clone())
                .collect::<Vec<_>>(),
            layout,
            "the described enumeration lost, renamed, or reordered a monitor"
        );

        for description in &descriptions {
            let Some(label) = description.label.as_deref() else {
                continue;
            };
            assert!(
                validate_monitor_label(label).is_ok(),
                "product name {label:?} is not a usable monitor label"
            );
            assert!(label.len() <= MAX_MONITOR_LABEL_BYTES, "{label:?}");
            assert_eq!(label.trim(), label, "a product name kept its padding");
        }
    }

    /// Repeated sweeps agree. A `QueryDisplayConfig` join that depended on
    /// enumeration order rather than on the device string would drift here
    /// as soon as anything re-enumerated.
    #[test]
    fn repeated_description_sweeps_agree() {
        let display = WindowsDisplayInfo::new();
        let (Ok(first), Ok(second)) = (
            display.monitor_descriptions(),
            display.monitor_descriptions(),
        ) else {
            return;
        };
        assert_eq!(first, second);
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
