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
//! and `monitor_descriptions()` adds the EDID product name and the panel's
//! physical size on top of that.
//!
//! Those descriptions are a **second sweep**, `QueryDisplayConfig` rather
//! than `EnumDisplayMonitors`, because it is the only Win32 route to a
//! monitor's EDID product name — usually the same string Windows Settings
//! shows, though Settings synthesizes and localizes a name for a panel
//! whose EDID carries none (`friendly_name_of` says what this build does
//! there) — and because the same response carries the device interface path
//! the panel's *size* is read behind (`panel_size_of`, then
//! [`crate::edid`]). It is far more expensive than the geometry
//! enumeration, which is precisely why it sits behind its own trait method
//! and why the ~8 ms edge poll never reaches it. The two sweeps are joined
//! by `szDevice` — the `DisplayConfig` source's `viewGdiDeviceName` is the
//! same string `GetMonitorInfoW` reports — never by enumeration position.
//! A failure anywhere in it costs captions and proportions and nothing
//! else. The desktop
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
    CursorPoint, DisplayError, DisplayInfo, MonitorDescription, MonitorInfo, MonitorRect,
    PhysicalSizeMm, Screen,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_FLAG_GLOBAL, DIREG_DEV, HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA,
    SetupDiCreateDeviceInfoList, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiOpenDevRegKey, SetupDiOpenDeviceInterfaceW,
};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LPARAM, POINT, RECT, TRUE,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegQueryValueExW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::{BOOL, PCWSTR};

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
    /// The same latch for the EDID read, kept separate because the two
    /// halves fail independently: a product name comes from
    /// `DisplayConfigGetDeviceInfo` and a panel size from `SetupAPI` plus the
    /// registry, so a machine that names every screen and measures none is
    /// an ordinary state (a VM, a remote session) and a distinct one to
    /// diagnose.
    size_lookup_failed: AtomicBool,
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

    /// Note that a panel-size lookup failed, logging `reason` only on the
    /// first failure of a streak — as [`Self::note_label_failure`], and for
    /// the same once-a-second reason.
    fn note_size_failure(&self, reason: &str) {
        if !self.size_lookup_failed.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                reason,
                "could not read monitor panel sizes; the editor will draw \
                 monitors by pixel count until this clears"
            );
        }
    }

    /// Note that a panel-size lookup succeeded, so the next failure logs
    /// again.
    fn note_size_success(&self) {
        self.size_lookup_failed.store(false, Ordering::Relaxed);
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
        // order, same rectangles. Descriptions are a third best-effort pass
        // laid over it, joined by the very device string `monitor_layout()`
        // already read — `viewGdiDeviceName` from a DisplayConfig source is
        // literally `szDevice`, which is what makes the join exact rather
        // than positional.
        let layout = self.monitor_layout()?;
        // Never an error and never a panic: a failure here costs captions
        // and proportions, and a monitor with neither still draws, still
        // crosses, and is still addressable by its id.
        let (descriptions, failure) = match path_descriptions() {
            Ok(descriptions) => (descriptions, None),
            Err(reason) => (Vec::new(), Some(reason)),
        };

        let described: Vec<MonitorDescription> = layout
            .into_iter()
            .map(|info| {
                let found = info.id.as_ref().and_then(|id| {
                    descriptions
                        .iter()
                        .find(|(device, _)| device == id)
                        .map(|(_, description)| description)
                });
                MonitorDescription {
                    label: found.and_then(|description| description.label.clone()),
                    physical_size: found.and_then(|description| description.physical_size),
                    info,
                }
            })
            .collect();

        // Each streak flag is decided **once**, from the outcome as a whole,
        // and never reset-then-set: a success followed by a failure inside
        // one call would clear the latch and log again, which at this
        // cadence is the log-per-second the latch exists to prevent.
        //
        // NFR-3 wants every failure visible, and they are different facts:
        // the sweep would not run, or the sweep ran and named nothing, or
        // it ran and measured nothing. The latter two are what a broken
        // join, a wrong `header.size`, or a desk of screens the OS will not
        // describe all look like from outside, and they are otherwise
        // indistinguishable from silence. Two latches rather than one,
        // because the two halves fail independently: reading a product name
        // and reading an EDID are different calls to different subsystems,
        // and a machine where every panel is named and none is measured is
        // a real and diagnosable state.
        let nothing_at_all = |pick: fn(&MonitorDescription) -> bool| {
            !described.is_empty() && described.iter().all(pick)
        };
        let named_nothing = nothing_at_all(|description| description.label.is_none());
        let measured_nothing = nothing_at_all(|description| description.physical_size.is_none());
        if let Some(reason) = failure {
            self.note_label_failure(&reason);
            self.note_size_failure(&reason);
        } else {
            if named_nothing {
                self.note_label_failure("the display configuration named no monitor at all");
            } else {
                self.note_label_success();
            }
            if measured_nothing {
                self.note_size_failure("no monitor's EDID could be read");
            } else {
                self.note_size_success();
            }
        }

        Ok(described)
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

/// How many display *modes* this build will let the OS describe alongside
/// those paths.
///
/// A path references at most a source mode, a target mode, and a desktop
/// image mode, so four per path is already generous; this is the path cap
/// times four. It exists for the same reason the path cap does and is
/// checked in the same place: `mode_count` is a number from a driver that
/// sizes an allocation, and "bound before allocating" does not get to skip
/// the second buffer because the first one was the interesting one. Nothing
/// here reads the modes — `QueryDisplayConfig` simply refuses to run
/// without somewhere to put them.
const MAX_DISPLAY_CONFIG_MODES: u32 = MAX_DISPLAY_CONFIG_PATHS * 4;

/// How many times to retry the size-then-query pair before giving up.
///
/// `GetDisplayConfigBufferSizes` and `QueryDisplayConfig` are two calls
/// with a window between them, and a display arriving or leaving in that
/// window makes the second one return `ERROR_INSUFFICIENT_BUFFER` — the
/// documented, expected race. Retrying re-reads the sizes; a small bound
/// keeps a display configuration changing continuously from spinning here.
const DISPLAY_CONFIG_ATTEMPTS: usize = 8;

/// What one `QueryDisplayConfig` path says *about* its monitor, as opposed
/// to where the monitor is.
///
/// Both halves are best effort and independent: a target can answer with a
/// name and no readable EDID (common — Windows names plenty of panels whose
/// EDID it will not hand back), or with an EDID and no product string.
#[derive(Default)]
struct PathDescription {
    label: Option<String>,
    physical_size: Option<PhysicalSizeMm>,
}

impl PathDescription {
    /// Nothing to say about this monitor, so nothing to record for it.
    fn is_empty(&self) -> bool {
        self.label.is_none() && self.physical_size.is_none()
    }
}

/// Every active display path's `szDevice` paired with what Windows' own
/// display configuration says about the monitor on it.
///
/// `viewGdiDeviceName` from `DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME`
/// **is** the `szDevice` string `GetMonitorInfoW` reports, which is what
/// lets this join onto the monitor ids ADR 0018 already uses. Both
/// descriptive halves come out of one
/// `DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME` response: the friendly name
/// from its `monitorFriendlyDeviceName` (see [`friendly_name_of`]), and the
/// panel size from the EDID behind its `monitorDevicePath` (see
/// [`panel_size_of`]). One response, two readings — the target is asked
/// about exactly once per path, which is what keeps the added cost of sizes
/// to the registry read itself.
///
/// Best effort throughout: a path whose source or target will not answer,
/// or that yields neither a name nor a size, contributes nothing and stops
/// nothing. The `Err` case is reserved for a failure of the *sweep* — the
/// caller logs it once per streak and carries on describing nothing.
fn path_descriptions() -> Result<Vec<(String, PathDescription)>, String> {
    let (paths, _modes) = query_display_config()?;

    let mut described: Vec<(String, PathDescription)> = Vec::with_capacity(paths.len());
    for path in &paths {
        let Some(device) = source_device_name(path) else {
            continue;
        };
        // A source can drive several targets (clone mode). The first target
        // that answers describes the screen; a second description for the
        // same `szDevice` would be a second caption and a second size for
        // one rectangle, which the model has no way to show.
        if described.iter().any(|(held, _)| held == &device) {
            continue;
        }
        let Some(target) = target_device_name(path) else {
            continue;
        };
        let description = PathDescription {
            label: friendly_name_of(&target),
            physical_size: panel_size_of(&target),
        };
        if description.is_empty() {
            continue;
        }
        described.push((device, description));
    }
    Ok(described)
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
        // Bound before allocating, as everywhere else — *both* buffers, not
        // only the one whose contents we go on to read.
        if path_count > MAX_DISPLAY_CONFIG_PATHS {
            return Err(format!(
                "the display configuration claims {path_count} active paths, over the \
                 {MAX_DISPLAY_CONFIG_PATHS} this build will describe"
            ));
        }
        if mode_count > MAX_DISPLAY_CONFIG_MODES {
            return Err(format!(
                "the display configuration claims {mode_count} modes, over the \
                 {MAX_DISPLAY_CONFIG_MODES} this build will describe"
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

/// What this build calls a laptop's built-in panel, which has an EDID that
/// carries no product name.
///
/// Windows Settings shows a name there too, but it **synthesizes** one
/// rather than reading it off the panel — and it localizes it. We ship the
/// English constant rather than pretending to reproduce the user's locale,
/// because a caption in the wrong language is still a caption that
/// distinguishes the panel from `\\.\DISPLAY1`, which is the whole job.
/// It is valid as a [`crossover_platform`] label by construction: ASCII,
/// 16 bytes, no control or format characters.
const INTERNAL_DISPLAY_LABEL: &str = "Internal Display";

/// Everything Windows will say about `path`'s target in one response —
/// which is deliberately one call, because both the product name and the
/// device path the EDID lives behind come out of the same structure.
fn target_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
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
    Some(request)
}

/// The human-readable name in `target` (`DELL U2720Q`), or `None` where the
/// OS has none and no substitute worth offering.
///
/// Two sources, in order:
///
/// - `monitorFriendlyDeviceName`, the EDID product name, for anything with
///   an EDID that carries one — every ordinary external monitor.
/// - [`INTERNAL_DISPLAY_LABEL`], where that name is **empty and the output
///   technology is `INTERNAL`**. A laptop's built-in panel is the common
///   case here, and it is why this branch is not an edge case to skip: a
///   laptop is exactly the machine whose one screen most needs a caption
///   that is not `\\.\DISPLAY1`, and it is the machine that would otherwise
///   see no benefit from this feature at all.
///
/// An empty name on a *non*-internal target stays `None`: a virtual,
/// remote, or non-PnP display genuinely has no name, and inventing one
/// would caption several such screens identically — worse than the device
/// strings, which at least differ.
fn friendly_name_of(target: &DISPLAYCONFIG_TARGET_DEVICE_NAME) -> Option<String> {
    if let Some(name) = wide_string(&target.monitorFriendlyDeviceName) {
        return Some(name);
    }
    // The response's own `outputTechnology`, not the path's: this is what
    // the OS says about the target it just described.
    if target.outputTechnology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL {
        return Some(INTERNAL_DISPLAY_LABEL.to_owned());
    }
    None
}

/// The panel size behind `target`'s `monitorDevicePath`, or `None` if it
/// cannot be read or is not one this build believes
/// ([`crate::edid::physical_size`] states the plausibility rule).
///
/// `monitorDevicePath` is a device *interface* path — the
/// `\\?\DISPLAY#DEL41A1#...` string Device Manager knows the monitor by —
/// and it is the handle onto the monitor's own driver key, under which
/// Windows caches the EDID it read off the cable. Every step is best effort
/// and every failure is `None`, because a caption-and-proportion feature has
/// no failure worth propagating: a screen with no readable EDID draws the
/// way every screen drew before sizes existed.
///
/// **Not on any hot path.** This is a `SetupAPI` walk and a registry read per
/// monitor, on top of the `QueryDisplayConfig` sweep the product name
/// already costs — genuinely expensive, and it happens only on the ~1 s
/// topology cadence that already pays for descriptions (ADR 0018). The
/// 8 ms edge poll calls `monitors()` and cannot reach here.
fn panel_size_of(target: &DISPLAYCONFIG_TARGET_DEVICE_NAME) -> Option<PhysicalSizeMm> {
    let path = wide_string(&target.monitorDevicePath)?;
    let edid = monitor_edid(&path)?;
    crate::edid::physical_size(&edid)
}

/// Most bytes this build will read out of a monitor's cached `EDID`
/// registry value.
///
/// A conforming EDID is 128 bytes, or 256 with one extension block; the
/// standard allows 255 extensions, which would be 32 KiB. 1 KiB is generous
/// over anything real and refuses, before allocating, a value that a
/// corrupt cache or a hostile local writer could otherwise make arbitrarily
/// large — the house rule ("bound before allocating") applied to a number
/// that is not network input but is still someone else's.
///
/// Only the base block is parsed, so nothing is lost by the cap even on a
/// monitor with more extensions than fit.
const MAX_EDID_BYTES: u32 = 1024;

/// The `EDID` value Windows cached under the monitor at device interface
/// path `device_path`, or `None` if it is not there or will not read.
///
/// The route is the documented one: an empty device information list, the
/// interface opened *by path* onto it (which adds exactly that one device),
/// the device information for the single member that produces, and then the
/// device's own driver key, where the `EDID` value lives.
///
/// Every failure is `None` rather than an error, per [`panel_size_of`]. The
/// list is destroyed on every exit, including the early ones — a leaked
/// `HDEVINFO` on a query that runs once a second would be a handle leak
/// with a clock on it.
fn monitor_edid(device_path: &str) -> Option<Vec<u8>> {
    let path: Vec<u16> = device_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: a null class GUID and null parent create an empty list this
    // function owns until it destroys it below.
    let devices = unsafe { SetupDiCreateDeviceInfoList(None, None) }.ok()?;
    let edid = read_edid_from(devices, &path);
    // SAFETY: `devices` is the list created above and not used after this.
    // The result is deliberately dropped: nothing can be done about a
    // failure to free a list, and it must not mask the value read.
    let _ = unsafe { SetupDiDestroyDeviceInfoList(devices) };
    edid
}

/// The body of [`monitor_edid`], split out so its every exit runs through
/// that function's single `SetupDiDestroyDeviceInfoList` rather than
/// repeating it on each `?`.
fn read_edid_from(devices: HDEVINFO, path: &[u16]) -> Option<Vec<u8>> {
    let mut interface = SP_DEVICE_INTERFACE_DATA {
        cbSize: u32::try_from(size_of::<SP_DEVICE_INTERFACE_DATA>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `path` is NUL-terminated (built above), `interface` is a live
    // local whose `cbSize` is set as the API requires, and `devices` is the
    // list this call adds the named device to.
    unsafe {
        SetupDiOpenDeviceInterfaceW(devices, PCWSTR(path.as_ptr()), 0, Some(&raw mut interface))
    }
    .ok()?;

    let mut info = SP_DEVINFO_DATA {
        cbSize: u32::try_from(size_of::<SP_DEVINFO_DATA>()).ok()?,
        ..Default::default()
    };
    // Member 0 is the device the call above just added: the list started
    // empty and nothing else has been put in it.
    // SAFETY: `info` is a live local with its `cbSize` set, and `devices`
    // is the list holding that one member.
    unsafe { SetupDiEnumDeviceInfo(devices, 0, &raw mut info) }.ok()?;

    // SAFETY: `info` is the member enumerated above; the call returns a key
    // this function closes below on every path.
    let key = unsafe {
        SetupDiOpenDevRegKey(
            devices,
            &raw const info,
            DICS_FLAG_GLOBAL.0,
            0,
            DIREG_DEV,
            KEY_READ.0,
        )
    }
    .ok()?;
    let edid = read_edid_value(key);
    // SAFETY: `key` is the key opened above and not used after this.
    let _ = unsafe { RegCloseKey(key) };
    edid
}

/// The `EDID` value under an already-open monitor driver key.
///
/// The length is asked for first and **checked before anything is
/// allocated**, so a value claiming to be enormous costs a comparison
/// rather than the allocation ([`MAX_EDID_BYTES`]).
fn read_edid_value(key: HKEY) -> Option<Vec<u8>> {
    let name: Vec<u16> = "EDID".encode_utf16().chain(std::iter::once(0)).collect();

    let mut length: u32 = 0;
    // SAFETY: a null data pointer asks only for the size, which the call
    // writes into `length`; every pointer passed is a live local.
    let sized = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&raw mut length),
        )
    };
    if sized != ERROR_SUCCESS || length == 0 || length > MAX_EDID_BYTES {
        return None;
    }

    let mut buffer = vec![0u8; length as usize];
    // SAFETY: `buffer` holds exactly `length` bytes, which is what the
    // in/out `length` tells the call it may write; both are live locals for
    // the whole call.
    let read = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buffer.as_mut_ptr()),
            Some(&raw mut length),
        )
    };
    if read != ERROR_SUCCESS {
        return None;
    }
    // The second call may have written fewer bytes than the first sized
    // for, if the value shrank between them.
    buffer.truncate(length as usize);
    Some(buffer)
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

    /// Descriptions are the identified enumeration with labels and panel
    /// sizes laid over it: never a monitor lost, never one reordered, and
    /// any label or size that *is* produced is one the layout model would
    /// accept.
    ///
    /// "Any that is produced" rather than "every monitor has one" on
    /// purpose. A virtual, remote, or non-PnP display legitimately has
    /// neither a name nor a readable EDID, and a product name over 64 UTF-8
    /// bytes — a long CJK one, say — is legitimately dropped by
    /// `live_monitors` rather than sent. So the contract here is
    /// `None`-or-valid, and asserting more would fail on somebody's
    /// hardware for behaviour that is correct.
    ///
    /// The rules themselves are imported from `crossover_topology` rather
    /// than restated, so the two cannot drift, and they are what stop a
    /// `U+FFFD` from a lossy decode or a fictional millimetre count
    /// reaching the wire as something a peer would be right to refuse. A
    /// headless agent has no display and every query fails cleanly, which
    /// is an acceptable outcome; a panic is not.
    #[test]
    fn descriptions_are_the_identified_enumeration_with_usable_labels() {
        use crossover_topology::{validate_monitor_label, validate_physical_size};

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
            if let Some(label) = description.label.as_deref() {
                // The byte bound is part of what `validate_monitor_label`
                // checks, so asserting it separately would only restate the
                // rule this deliberately imports. `wide_string` already
                // trims, likewise.
                assert!(
                    validate_monitor_label(label).is_ok(),
                    "product name {label:?} is not a usable monitor label"
                );
            }
            if let Some(size) = description.physical_size {
                assert!(
                    validate_physical_size(size.width_mm, size.height_mm).is_ok(),
                    "panel size {size:?} is not one the layout model would carry"
                );
                // And the tighter rule this backend imposes on itself: a
                // size it claims at all is one it found plausible. The
                // constants are imported so the assertion cannot drift from
                // the gate.
                assert!(
                    (crate::edid::MIN_PLAUSIBLE_MM..=crate::edid::MAX_PLAUSIBLE_MM)
                        .contains(&size.width_mm),
                    "an implausible width escaped the acquisition gate: {size:?}"
                );
                assert!(
                    (crate::edid::MIN_PLAUSIBLE_MM..=crate::edid::MAX_PLAUSIBLE_MM)
                        .contains(&size.height_mm),
                    "an implausible height escaped the acquisition gate: {size:?}"
                );
            }
        }
    }

    /// The EDID read itself, exercised against whatever this machine has —
    /// and, like the join-key test above, written so it cannot pass
    /// vacuously in the interesting direction.
    ///
    /// Every active path is asked for its `monitorDevicePath`. On a session
    /// with real displays at least one target must produce one, because
    /// that string is how the OS itself addresses the monitor; a run where
    /// none does means the target request is malformed (a wrong
    /// `header.size` or `adapterId`) rather than that the hardware is
    /// unusual. What is behind those paths is hardware-dependent and is
    /// therefore only checked for *shape*: an EDID that reads at all is
    /// within the size cap and begins with the header magic.
    #[test]
    fn every_active_target_names_a_device_path_and_any_edid_behind_one_is_an_edid() {
        let Ok((paths, _modes)) = super::query_display_config() else {
            return;
        };
        if paths.is_empty() {
            // A locked or headless session has no active paths; there is
            // nothing to read and nothing to assert about.
            return;
        }

        let targets: Vec<_> = paths.iter().filter_map(super::target_device_name).collect();
        if targets.is_empty() {
            return;
        }
        let device_paths: Vec<String> = targets
            .iter()
            .filter_map(|target| super::wide_string(&target.monitorDevicePath))
            .collect();
        assert!(
            !device_paths.is_empty(),
            "{} active targets answered and not one named a device path — the \
             DisplayConfig target request is malformed",
            targets.len()
        );

        for path in &device_paths {
            let Some(edid) = super::monitor_edid(path) else {
                // A monitor whose EDID Windows did not cache. Ordinary on a
                // VM, a remote session, or a non-PnP display.
                continue;
            };
            assert!(
                edid.len() <= super::MAX_EDID_BYTES as usize,
                "an EDID past the read cap came back: {} bytes",
                edid.len()
            );
            assert!(
                edid.len() >= crate::edid::EDID_BLOCK_BYTES,
                "a short EDID came back for {path:?}: {} bytes",
                edid.len()
            );
            assert_eq!(
                &edid[..8],
                &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
                "the value read for {path:?} is not an EDID — wrong registry value or key"
            );
        }
    }

    /// The join key itself, asserted directly — because the test above
    /// **cannot fail** on a machine where every label is `None`, and every
    /// label being `None` is exactly what a broken join, a wrong
    /// `header.size`, or a mis-set `adapterId` produces.
    ///
    /// So this proves the half of the sweep that is machine-independent:
    /// on any Windows session with an active display, `QueryDisplayConfig`
    /// yields sources whose `viewGdiDeviceName` are `\\.\DISPLAY*` strings,
    /// and every one of them is an id `monitor_layout()` also reports.
    /// Product names vary by hardware; the key that finds them does not.
    #[test]
    fn the_display_config_join_key_is_the_device_string_the_layout_reports() {
        let display = WindowsDisplayInfo::new();
        let (Ok(paths), Ok(layout)) = (super::query_display_config(), display.monitor_layout())
        else {
            return;
        };
        let (paths, _modes) = paths;
        if paths.is_empty() {
            // A locked or headless session has no active paths; there is
            // nothing to join and nothing to assert about.
            return;
        }

        let sources: Vec<String> = paths.iter().filter_map(super::source_device_name).collect();
        assert!(
            !sources.is_empty(),
            "QueryDisplayConfig reported {} active paths and not one source name — \
             the DisplayConfig request is malformed (header size or adapter id)",
            paths.len()
        );

        let ids: Vec<&str> = layout.iter().filter_map(|m| m.id.as_deref()).collect();
        for source in &sources {
            assert!(
                source.starts_with(r"\\.\DISPLAY"),
                "a DisplayConfig source name is not a device string: {source:?}"
            );
            assert!(
                ids.contains(&source.as_str()),
                "DisplayConfig named the source {source:?}, which is not one of the \
                 monitors GetMonitorInfoW reports ({ids:?}) — the join key has drifted"
            );
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
