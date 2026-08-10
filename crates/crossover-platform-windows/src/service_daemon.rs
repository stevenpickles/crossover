//! The Windows service daemon: the privileged Win32 that `crossover-svc` runs
//! as a `LocalSystem` service (ADR 0011). It launches and supervises the worker
//! (`crossover.exe run`) inside the active user's console session.
//!
//! Decision-making lives in the pure [`WorkerSupervisor`]; this module is only
//! the glue — the SCM control dispatcher and handler, the
//! WTS/token/`CreateProcessAsUser` launcher, and the event loop that turns OS
//! events into supervisor calls and executes the actions it returns.
//!
//! Security invariant (ADR 0011): the daemon's only inputs are OS session state
//! and its child's exit code. It never touches the network, clipboard, or peer
//! data — and `crossover-svc` links no code that could.
//!
//! Untestable in CI (it needs `LocalSystem` privilege and a real logon session),
//! so the risk is concentrated here and kept thin: the branching logic it drives
//! is the separately, exhaustively tested [`WorkerSupervisor`].

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TokenPrimary,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    INFINITE, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForMultipleObjects,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::worker_supervisor::{WorkerAction, WorkerSupervisor, WorkerSupervisorConfig};

/// Service key name; must match what [`crate::service::WindowsServiceManager`]
/// registers.
const SERVICE_NAME: PCWSTR = w!("Crossover");

/// The Win32 sentinel `WTSGetActiveConsoleSessionId` returns when no session is
/// attached to the physical console.
const NO_ACTIVE_SESSION: u32 = 0xFFFF_FFFF;

/// `GetExitCodeProcess` reports this while the process is still running.
const STILL_ACTIVE: u32 = 259;

/// How long to wait for a terminated worker to actually exit before moving on.
const TERMINATE_WAIT_MS: u32 = 5_000;

// The SCM invokes the service-main and control-handler as C callbacks with no
// way to pass a Rust context, so the handler thread and the supervision thread
// communicate through these process-globals: the status handle (to report
// state), a manual-reset wake event (to break the supervision loop's wait), and
// a pending-controls mailbox.
static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static WAKE_EVENT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PENDING: Mutex<Pending> = Mutex::new(Pending::new());

/// Controls the handler has received but the loop has not yet processed.
struct Pending {
    stop: bool,
    session_changed: bool,
}

impl Pending {
    const fn new() -> Self {
        Self {
            stop: false,
            session_changed: false,
        }
    }
}

/// Run as the Windows service: hand control to the SCM dispatcher, which calls
/// [`service_main`] on a service thread. Blocks until the service stops.
///
/// This is the entry point `crossover-svc` calls. If the process was not
/// started by the SCM (e.g. launched from a console), the dispatcher fails and
/// this logs and returns.
pub fn run_service_daemon() {
    let mut name = wide("Crossover");
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        // Null terminator entry.
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(null_mut()),
            lpServiceProc: None,
        },
    ];
    // SAFETY: `table` is a valid, null-terminated service table that outlives
    // the (blocking) dispatcher call; `name` outlives it too.
    if let Err(error) = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        tracing::error!(
            %error,
            "not started by the service control manager; install with \
             `crossover service install` and let the SCM run this binary"
        );
    }
}

/// SCM service entry point. Registers the control handler, reports running, and
/// runs the supervision loop until stopped.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    // SAFETY: SERVICE_NAME is a static wide string; the handler is a valid
    // `extern "system"` function; a null context is allowed.
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(SERVICE_NAME, Some(handler_ex), Some(null_mut()))
    } {
        Ok(handle) => handle,
        Err(error) => {
            tracing::error!(%error, "failed to register the service control handler");
            return;
        }
    };
    STATUS_HANDLE.store(handle.0, Ordering::SeqCst);

    // Manual-reset wake event: the handler signals it to break the loop's wait.
    // SAFETY: default attributes; manual-reset, initially non-signaled; no name.
    let wake = match unsafe {
        windows::Win32::System::Threading::CreateEventW(None, true, false, PCWSTR::null())
    } {
        Ok(event) => event,
        Err(error) => {
            tracing::error!(%error, "failed to create the service wake event");
            set_status(SERVICE_STOPPED, 0, 1);
            return;
        }
    };
    WAKE_EVENT.store(wake.0, Ordering::SeqCst);

    set_status(SERVICE_START_PENDING, 0, 0);
    set_status(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SESSIONCHANGE | SERVICE_ACCEPT_SHUTDOWN,
        0,
    );

    supervise();

    // Swap to null so nothing double-closes it, then close it once.
    let wake = HANDLE(WAKE_EVENT.swap(null_mut(), Ordering::SeqCst));
    if !wake.is_invalid() {
        // SAFETY: `wake` is the live event handle created above, closed once.
        unsafe {
            let _ = CloseHandle(wake);
        }
    }
    set_status(SERVICE_STOPPED, 0, 0);
}

/// SCM control-request handler. Runs on an SCM-owned thread, so it only records
/// intent and wakes the supervision loop rather than doing work itself.
unsafe extern "system" fn handler_ex(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        lock_pending().stop = true;
        set_status(SERVICE_STOP_PENDING, 0, 0);
        signal_wake();
    } else if control == SERVICE_CONTROL_SESSIONCHANGE {
        // The loop re-queries the active session on wake, so the specific
        // logon/logoff/connect event type does not need decoding here.
        lock_pending().session_changed = true;
        signal_wake();
    }
    0 // NO_ERROR
}

/// The supervision loop: translate OS events into [`WorkerSupervisor`] calls and
/// execute the actions it returns, until it reports [`WorkerAction::Stopped`].
fn supervise() {
    let Some(worker_exe) = worker_exe_path() else {
        tracing::error!("cannot locate crossover.exe next to the service binary");
        return;
    };

    let mut supervisor = WorkerSupervisor::new(WorkerSupervisorConfig::default());
    let start = Instant::now();
    let now_ms = || u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut child: Option<Handle> = None;

    supervisor.note_active_session(probe_active_session());

    loop {
        // Drive every action that is ready before waiting again.
        loop {
            match supervisor.poll(now_ms()) {
                WorkerAction::Launch(session) => match launch_worker(&worker_exe, session) {
                    Ok(handle) => {
                        tracing::info!(session, "launched worker in user session");
                        child = Some(handle);
                    }
                    Err(error) => {
                        tracing::warn!(%error, session, "failed to launch worker");
                        supervisor.note_launch_failed(now_ms());
                    }
                },
                WorkerAction::StopWorker => {
                    if let Some(handle) = child.take() {
                        stop_child(&handle);
                    }
                    supervisor.note_worker_exited(false, now_ms());
                }
                WorkerAction::Idle => break,
                WorkerAction::Stopped => return,
            }
        }

        let timeout_ms = supervisor
            .wake_deadline()
            .map(|deadline| deadline.saturating_sub(now_ms()));
        wait_for_event(child.as_ref(), timeout_ms);

        // Process controls the handler recorded.
        let (stop, session_changed) = {
            let mut pending = lock_pending();
            let controls = (pending.stop, pending.session_changed);
            pending.session_changed = false;
            controls
        };
        reset_wake();
        if stop {
            supervisor.request_stop();
        }
        if session_changed {
            supervisor.note_active_session(probe_active_session());
        }

        // Reap the child if it exited.
        let exit_code = child.as_ref().and_then(child_exit_code);
        if let Some(code) = exit_code {
            child = None;
            supervisor.note_worker_exited(code != 0, now_ms());
        }
    }
}

/// `%SYSTEMDRIVE%\...\crossover.exe`, resolved as a sibling of the running
/// service binary (they ship together).
fn worker_exe_path() -> Option<PathBuf> {
    let service_exe = std::env::current_exe().ok()?;
    Some(service_exe.parent()?.join("crossover.exe"))
}

/// The active console session, if a user is logged on to it. `None` at the
/// logon screen (session present but no user token) or with no console session.
fn probe_active_session() -> Option<u32> {
    // SAFETY: no preconditions; returns the session id or NO_ACTIVE_SESSION.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == NO_ACTIVE_SESSION {
        return None;
    }
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out-pointer; success means a user is logged on.
    let logged_on = unsafe { WTSQueryUserToken(session, &raw mut token) }.is_ok();
    if logged_on {
        // SAFETY: `token` was populated by the successful query above.
        unsafe {
            let _ = CloseHandle(token);
        }
        Some(session)
    } else {
        None
    }
}

/// Launch `crossover.exe run` as the user of `session`, in their session and
/// desktop, and return the process handle.
fn launch_worker(worker_exe: &Path, session: u32) -> windows::core::Result<Handle> {
    let mut user_token = HANDLE::default();
    // SAFETY: valid out-pointer; on success `user_token` is the session user's
    // token, which we own and free.
    unsafe { WTSQueryUserToken(session, &raw mut user_token) }?;
    let user_token = Handle(user_token);

    let mut primary_token = HANDLE::default();
    // SAFETY: `user_token` is live; a primary token is required for
    // CreateProcessAsUser. `primary_token` is a valid out-pointer we then own.
    unsafe {
        DuplicateTokenEx(
            user_token.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut primary_token,
        )
    }?;
    let primary_token = Handle(primary_token);

    // The user's environment block so the worker sees %APPDATA% etc. Best
    // effort: on failure the worker inherits the service environment.
    let mut env_ptr: *mut c_void = null_mut();
    // SAFETY: valid out-pointer; `primary_token` is live.
    let env = unsafe { CreateEnvironmentBlock(&raw mut env_ptr, Some(primary_token.0), false) }
        .is_ok()
        .then_some(EnvBlock(env_ptr));

    let mut command_line = worker_command_line(worker_exe);
    let mut desktop = wide(r"winsta0\default");
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

    // SAFETY: `primary_token` is a live primary token; `command_line`, `desktop`,
    // `startup`, and `process` outlive the call; the environment pointer, when
    // present, is a valid Unicode block matching CREATE_UNICODE_ENVIRONMENT.
    unsafe {
        CreateProcessAsUserW(
            Some(primary_token.0),
            PCWSTR::null(),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            env.as_ref().map(|block| block.0.cast_const()),
            PCWSTR::null(),
            &raw const startup,
            &raw mut process,
        )
    }?;

    // We do not use the primary thread handle; close it. Keep the process
    // handle to wait on and terminate.
    let _thread = Handle(process.hThread);
    Ok(Handle(process.hProcess))
}

/// `"C:\...\crossover.exe" run`, null-terminated, as a mutable wide buffer
/// (`CreateProcessAsUserW` may write to the command line).
fn worker_command_line(worker_exe: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let quote = u16::from(b'"');
    let mut line: Vec<u16> = std::iter::once(quote)
        .chain(worker_exe.as_os_str().encode_wide())
        .chain(std::iter::once(quote))
        .collect();
    line.extend(" run".encode_utf16());
    line.push(0);
    line
}

/// Terminate the worker and wait (bounded) for it to exit.
fn stop_child(child: &Handle) {
    // SAFETY: `child.0` is a live process handle with terminate/synchronize
    // rights (CreateProcessAsUser grants full access to the returned handle).
    unsafe {
        let _ = TerminateProcess(child.0, 1);
        let _ = WaitForSingleObject(child.0, TERMINATE_WAIT_MS);
    }
}

/// The worker's exit code, or `None` while it is still running.
fn child_exit_code(child: &Handle) -> Option<u32> {
    let mut code = 0u32;
    // SAFETY: `child.0` is a live process handle; `code` is a valid out-pointer.
    unsafe { GetExitCodeProcess(child.0, &raw mut code) }.ok()?;
    (code != STILL_ACTIVE).then_some(code)
}

/// Block until the wake event fires, the child exits, or the timeout elapses.
fn wait_for_event(child: Option<&Handle>, timeout_ms: Option<u64>) {
    let wake = HANDLE(WAKE_EVENT.load(Ordering::SeqCst));
    let mut handles: Vec<HANDLE> = Vec::with_capacity(2);
    if let Some(handle) = child {
        handles.push(handle.0);
    }
    handles.push(wake);
    let timeout = timeout_ms.map_or(INFINITE, |ms| u32::try_from(ms).unwrap_or(INFINITE));
    // SAFETY: every handle in the slice is live for the duration of the wait.
    unsafe {
        let _ = WaitForMultipleObjects(&handles, false, timeout);
    }
}

/// Reset the manual-reset wake event after draining pending controls.
fn reset_wake() {
    let wake = HANDLE(WAKE_EVENT.load(Ordering::SeqCst));
    if !wake.is_invalid() {
        // SAFETY: `wake` is the live manual-reset event created in service_main.
        unsafe {
            let _ = windows::Win32::System::Threading::ResetEvent(wake);
        }
    }
}

/// Signal the wake event so the supervision loop re-evaluates.
fn signal_wake() {
    let wake = HANDLE(WAKE_EVENT.load(Ordering::SeqCst));
    if !wake.is_invalid() {
        // SAFETY: `wake` is the live manual-reset event created in service_main.
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(wake);
        }
    }
}

/// Report a service state to the SCM. `accepted` is honored only in the running
/// state; `exit_code` is the Win32 exit code for a failed stop.
fn set_status(
    state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE,
    accepted: u32,
    exit_code: u32,
) {
    let handle = SERVICE_STATUS_HANDLE(STATUS_HANDLE.load(Ordering::SeqCst));
    if handle.0.is_null() {
        return;
    }
    let mut status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            accepted
        } else {
            0
        },
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: `handle` is the live status handle from RegisterServiceCtrlHandlerEx;
    // `status` is a valid, fully-initialized SERVICE_STATUS.
    unsafe {
        let _ = SetServiceStatus(handle, &raw mut status);
    }
}

/// Lock the pending-controls mailbox, recovering from poisoning (a panicked
/// holder must not wedge the handler or loop).
fn lock_pending() -> std::sync::MutexGuard<'static, Pending> {
    PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// UTF-16, null-terminated.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns a Win32 `HANDLE` and closes it on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is a handle we own, closed exactly once at drop.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Owns a user environment block and destroys it on drop.
struct EnvBlock(*mut c_void);

impl Drop for EnvBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful CreateEnvironmentBlock and
            // is destroyed exactly once.
            unsafe {
                let _ = DestroyEnvironmentBlock(self.0);
            }
        }
    }
}
