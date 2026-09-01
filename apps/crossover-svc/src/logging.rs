//! Durable supervision logging for `crossover-svc` (Windows only).
//!
//! The service runs as `LocalSystem`, so its stderr is not "captured by the
//! SCM/event pipeline" — that claim was wrong (see `main.rs`'s prior comment)
//! and it cost real diagnosis time: a worker died abruptly during a
//! two-machine soak on 2026-08-19 and nothing durable recorded why, because
//! this binary logged to stderr only. Fixed by adding a rolling-file sink
//! here, mirroring `apps/crossover/src/logging.rs`'s pattern.
//!
//! ## Where the log lives, and why it differs from the worker's
//!
//! The worker (`crossover.exe run`) logs to `~/.crossover/logs`, which is
//! correct for a process running as the console user. This service does not
//! run as a user at all — `~` for a `LocalSystem` process resolves to the
//! SYSTEM profile, a location nobody looks at (and arguably nobody should
//! have to find). So the supervision log instead goes to a fixed,
//! machine-local path: `%ProgramData%\Crossover\logs`, discoverable without
//! knowing which profile the service happens to run under.
//!
//! As with the worker's file sink, this never logs clipboard contents or key
//! material (FR-7.4) — trivially true here, since this binary's dependency
//! graph excludes the crates that ever see either (ADR 0011), but the
//! invariant is worth stating even where it holds by construction.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, fmt};

/// Install the global tracing subscriber: stderr plus a rolling file in
/// `%ProgramData%\Crossover\logs`.
///
/// Filtering follows `RUST_LOG` when set and defaults to `info` — enough to
/// answer "why did the worker exit" and "did we ask it to stop" without
/// per-poll chatter.
///
/// Returns the [`WorkerGuard`] the caller must keep alive for the service's
/// lifetime — dropping it flushes and stops the file writer — or `None` when
/// file logging could not be set up (`%ProgramData%` unset, or the directory
/// is not writable), in which case the service still logs to stderr and a
/// warning names the reason.
pub fn init() -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // stderr: unchanged from before this fix. The SCM gives the service no
    // console, so this is only useful when the binary is run interactively
    // (e.g. `crossover-svc.exe` from a console for manual testing).
    let console = fmt::layer().with_target(true).with_writer(std::io::stderr);

    // File: plain (no ANSI), so the log stays greppable. Best-effort — an
    // unwritable or unresolvable directory falls back to stderr-only rather
    // than failing service startup over a diagnostics sink.
    let (file_layer, guard, unavailable) = match file_appender() {
        Ok((writer, guard)) => (
            Some(fmt::layer().with_ansi(false).with_writer(writer)),
            Some(guard),
            None,
        ),
        Err(reason) => (None, None, Some(reason)),
    };

    let init_result = tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .try_init();

    if let Err(error) = init_result {
        // No subscriber at all means every log line below is lost too; this
        // is the one place this module writes straight to stderr.
        eprintln!("crossover-svc: failed to initialize logging: {error}");
        return None;
    }

    // Said out loud, not swallowed: losing the file sink turns the very
    // incident this module exists to prevent into a black box again.
    if let Some(reason) = unavailable {
        tracing::warn!(
            reason,
            "supervision file logging unavailable; this run logs to stderr only"
        );
    }
    guard
}

/// `%ProgramData%\Crossover\logs`, daily-rotating, keeping a week of files —
/// the same retention shape as the worker's `~/.crossover/logs`.
fn file_appender() -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard), String> {
    let dir = service_log_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("crossover-svc")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .map_err(|e| format!("opening a rolling log in {}: {e}", dir.display()))?;
    Ok(tracing_appender::non_blocking(appender))
}

/// `%ProgramData%\Crossover\logs`. Fixed and machine-local rather than
/// derived from a home directory, because this process runs as `LocalSystem`
/// and has no user profile anyone would think to check.
fn service_log_dir() -> Result<std::path::PathBuf, String> {
    resolve_service_log_dir(std::env::var_os("ProgramData"))
}

/// The pure half of [`service_log_dir`]: given what `%ProgramData%` resolved
/// to (or didn't), the log directory or why not. Split out so the decision is
/// testable without mutating process-global environment state — this crate
/// forbids `unsafe` (NFR-6), which `std::env::set_var` requires.
fn resolve_service_log_dir(
    program_data: Option<std::ffi::OsString>,
) -> Result<std::path::PathBuf, String> {
    let program_data = program_data
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "%ProgramData% is not set".to_owned())?;
    Ok(std::path::PathBuf::from(program_data)
        .join("Crossover")
        .join("logs"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    // The subscriber is process-global: exercising a real `init()` here would
    // collide with every other test binary's own subscriber, so this only
    // covers the pure path-resolution logic.
    #[test]
    fn resolves_under_program_data_when_set() {
        let dir = super::resolve_service_log_dir(Some(OsString::from(r"C:\ProgramData"))).unwrap();
        assert_eq!(dir, std::path::Path::new(r"C:\ProgramData\Crossover\logs"));
    }

    #[test]
    fn missing_program_data_is_reported_not_guessed() {
        assert!(super::resolve_service_log_dir(None).is_err());
        assert!(super::resolve_service_log_dir(Some(OsString::new())).is_err());
    }
}
