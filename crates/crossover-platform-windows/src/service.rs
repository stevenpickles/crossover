//! The Windows service daemon behind the `crossover-svc` binary (ADR 0011).
//!
//! The privileged Win32 that makes Crossover run unattended lives here, behind
//! the platform boundary — not in the `crossover-svc` binary, which is a thin
//! entry point, and never in the app or core. When implemented this module
//! hosts:
//!
//! - the SCM service-control dispatcher and control handler
//!   (`StartServiceCtrlDispatcher`, `SERVICE_CONTROL_STOP` /
//!   `SERVICE_CONTROL_SESSIONCHANGE`);
//! - the user-session launcher (`WTSGetActiveConsoleSessionId` →
//!   `WTSQueryUserToken` → `DuplicateTokenEx` → `CreateProcessAsUser`) that
//!   starts `crossover.exe run` as the logged-in user;
//! - the watchdog that relaunches the worker with bounded backoff on crash and
//!   stops it on logoff.
//!
//! Load-bearing invariant (ADR 0011 §security): this daemon's only inputs are
//! OS session state and its child's exit code — never the network, clipboard,
//! or peer data. That is why `crossover-svc` links no `crossover-core` /
//! `-protocol` / `-security` code: the SYSTEM-privileged process cannot even
//! reach untrusted input.

/// Run as the Windows service: connect to the SCM dispatcher and supervise the
/// worker inside the active user session (ADR 0011). This is the entry point
/// the `crossover-svc` binary calls.
///
/// Not yet implemented: the dispatcher, user-session launcher, and
/// session-change watchdog land in a follow-up. For now it logs and returns so
/// the binary, its minimal dependency graph, and the platform boundary exist
/// and are verified by CI before the privileged code is written.
pub fn run_service_daemon() {
    tracing::warn!(
        "crossover-svc: service daemon is not yet implemented; the SCM \
         dispatcher and user-session launcher (ADR 0011) land next"
    );
}
