//! The Windows service machinery behind Crossover's unattended operation
//! (ADR 0011). Two responsibilities live here, both privileged Win32 kept
//! behind the platform boundary rather than in the app or the thin binaries:
//!
//! This module holds [`WindowsServiceManager`] — the [`ServiceManager`]
//! implementation the `crossover.exe service` subcommands call: register /
//! remove / query the service with the Service Control Manager (SCM). Install
//! and uninstall need Administrator; status does not.
//!
//! The service *daemon* it registers — the process the SCM actually runs, which
//! launches and supervises the worker — lives in [`crate::service_daemon`].

use std::path::{Path, PathBuf};

use crossover_platform::{ServiceError, ServiceManager, ServiceStatus};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS, WIN32_ERROR,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_WIN32_OWN_PROCESS,
};
use windows::core::PCWSTR;

/// SCM key name for the service. Stable — it identifies the installed service
/// across upgrades, so it must not change once shipped.
const SERVICE_NAME: &str = "Crossover";
/// Human-facing name shown in services.msc.
const DISPLAY_NAME: &str = "Crossover Input Sharing";

/// Registers Crossover's background service with the Windows SCM (ADR 0011).
///
/// Holds the path to the daemon binary (`crossover-svc.exe`) to register; the
/// composition root resolves it (a sibling of the running `crossover.exe`) and
/// hands it in, keeping path policy out of the platform layer.
#[derive(Debug, Clone)]
pub struct WindowsServiceManager {
    service_binary: PathBuf,
}

impl WindowsServiceManager {
    /// A manager that will register `service_binary` as the service image.
    #[must_use]
    pub fn new(service_binary: PathBuf) -> Self {
        Self { service_binary }
    }
}

impl ServiceManager for WindowsServiceManager {
    fn install(&self) -> Result<(), ServiceError> {
        // Fail early and clearly rather than registering a service pointed at
        // a path that does not exist (which would fail obscurely at boot).
        if !self.service_binary.is_file() {
            return Err(ServiceError::Backend {
                reason: format!(
                    "the service daemon was not found at {}; reinstall Crossover so \
                     crossover-svc.exe sits next to crossover.exe",
                    self.service_binary.display()
                ),
            });
        }

        let scm = open_scm(SC_MANAGER_CREATE_SERVICE)?;
        let name = wide(SERVICE_NAME);
        let display = wide(DISPLAY_NAME);
        let binary = quoted_wide_path(&self.service_binary);

        // SAFETY: `scm` is a live SCM handle opened with create access; every
        // wide string is null-terminated and outlives the call. A null start
        // name registers the service under LocalSystem (with a null password),
        // which is the intended account (ADR 0011).
        let handle = unsafe {
            CreateServiceW(
                scm.0,
                PCWSTR(name.as_ptr()),
                PCWSTR(display.as_ptr()),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(binary.as_ptr()),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            )
        }
        .map_err(|err| {
            if is_win32(&err, ERROR_ACCESS_DENIED) {
                ServiceError::PermissionDenied {
                    reason: "creating a Windows service requires Administrator privileges"
                        .to_owned(),
                }
            } else if is_win32(&err, ERROR_SERVICE_EXISTS) {
                ServiceError::Backend {
                    reason: "the Crossover service is already installed".to_owned(),
                }
            } else {
                ServiceError::Backend {
                    reason: format!("creating the Crossover service: {err}"),
                }
            }
        })?;
        // Close the returned handle promptly; registration is already durable.
        drop(ScHandle(handle));
        Ok(())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        let scm = open_scm(SC_MANAGER_CONNECT)?;
        let Some(service) = open_service(&scm, SERVICE_ALL_ACCESS)? else {
            return Err(ServiceError::Backend {
                reason: "the Crossover service is not installed".to_owned(),
            });
        };

        // Best-effort stop so the removal takes effect immediately instead of
        // being deferred until a running instance exits. Any error here (not
        // running, not stoppable) is irrelevant to the delete that follows.
        let mut status = SERVICE_STATUS::default();
        // SAFETY: `service` is a live handle opened with full access (includes
        // stop); ControlService writes the resulting status into `status`.
        let _ = unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &raw mut status) };

        // SAFETY: `service` is a live handle opened with delete access.
        unsafe { DeleteService(service.0) }.map_err(|err| {
            if is_win32(&err, ERROR_ACCESS_DENIED) {
                ServiceError::PermissionDenied {
                    reason: "removing a Windows service requires Administrator privileges"
                        .to_owned(),
                }
            } else {
                ServiceError::Backend {
                    reason: format!("removing the Crossover service: {err}"),
                }
            }
        })
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let scm = open_scm(SC_MANAGER_CONNECT)?;
        let Some(service) = open_service(&scm, SERVICE_QUERY_STATUS)? else {
            return Ok(ServiceStatus::NotInstalled);
        };

        let mut status = SERVICE_STATUS::default();
        // SAFETY: `service` is a live handle opened with query-status access;
        // QueryServiceStatus fills `status` with the current service state.
        unsafe { QueryServiceStatus(service.0, &raw mut status) }.map_err(|err| {
            ServiceError::Backend {
                reason: format!("querying Crossover service status: {err}"),
            }
        })?;
        Ok(ServiceStatus::Installed {
            running: status.dwCurrentState == SERVICE_RUNNING,
        })
    }
}

/// Owns an SCM handle and closes it on drop, so the many early returns above
/// cannot leak one.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a handle from a successful Open*/CreateService
        // call, closed exactly once (at drop).
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

/// Open the local SCM with `access`, mapping access-denied to a typed
/// permission error so the CLI can advise elevation.
fn open_scm(access: u32) -> Result<ScHandle, ServiceError> {
    // SAFETY: null machine and database names select the local, active SCM;
    // the call only reads the access mask and returns a handle or an error.
    let handle =
        unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }.map_err(|err| {
            if is_win32(&err, ERROR_ACCESS_DENIED) {
                ServiceError::PermissionDenied {
                    reason: "access to the service control manager was denied".to_owned(),
                }
            } else {
                ServiceError::Backend {
                    reason: format!("opening the service control manager: {err}"),
                }
            }
        })?;
    Ok(ScHandle(handle))
}

/// Open the Crossover service with `access`. `Ok(None)` means it is not
/// installed — a normal state, not an error.
fn open_service(scm: &ScHandle, access: u32) -> Result<Option<ScHandle>, ServiceError> {
    let name = wide(SERVICE_NAME);
    // SAFETY: `scm` is a live SCM handle; `name` is a null-terminated wide
    // string valid for the duration of the call.
    match unsafe { OpenServiceW(scm.0, PCWSTR(name.as_ptr()), access) } {
        Ok(handle) => Ok(Some(ScHandle(handle))),
        Err(err) if is_win32(&err, ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
        Err(err) if is_win32(&err, ERROR_ACCESS_DENIED) => Err(ServiceError::PermissionDenied {
            reason: "access to the Crossover service was denied".to_owned(),
        }),
        Err(err) => Err(ServiceError::Backend {
            reason: format!("opening the Crossover service: {err}"),
        }),
    }
}

/// Whether a Win32 API error carries the given status code.
fn is_win32(err: &windows::core::Error, code: WIN32_ERROR) -> bool {
    err.code() == code.to_hresult()
}

/// UTF-16, null-terminated — the encoding every `*W` Win32 entry point wants.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The binary path as a quoted, null-terminated wide string. Quoting protects
/// paths with spaces (`C:\Program Files\...`); encoding the `OsStr` directly
/// avoids a lossy round-trip through `String`.
fn quoted_wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let quote = u16::from(b'"');
    std::iter::once(quote)
        .chain(path.as_os_str().encode_wide())
        .chain(std::iter::once(quote))
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossover_platform::{ServiceError, ServiceManager, ServiceStatus};

    use super::WindowsServiceManager;

    // Status is read-only (SC_MANAGER_CONNECT, no elevation), so it is safe to
    // exercise against the real SCM on CI: Crossover is not installed there, so
    // the query must resolve to NotInstalled. This proves the SCM open + status
    // + does-not-exist error mapping against live Win32, not a mock.
    #[test]
    fn status_of_uninstalled_service_is_not_installed() {
        let manager =
            WindowsServiceManager::new(PathBuf::from(r"C:\nonexistent\crossover-svc.exe"));
        assert_eq!(manager.status().unwrap(), ServiceStatus::NotInstalled);
    }

    // The missing-binary guard fires before any SCM call, so this never mutates
    // the machine (safe on CI regardless of elevation) and is deterministic.
    #[test]
    fn install_rejects_a_missing_daemon_binary() {
        let manager =
            WindowsServiceManager::new(PathBuf::from(r"C:\nonexistent\crossover-svc.exe"));
        assert!(matches!(
            manager.install(),
            Err(ServiceError::Backend { .. })
        ));
    }
}
