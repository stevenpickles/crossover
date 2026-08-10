//! Structured logging setup for the Crossover binary.
//!
//! FR-7.3 requires structured logging from the first commit; field, span,
//! and level conventions live in docs/ARCHITECTURE.md §10. The discipline
//! that matters most (FR-7.4): clipboard contents and private key material
//! never appear in logs at any level — transactions are logged by metadata
//! only.
//!
//! Two sinks: the console (as before) and a rolling file under
//! `~/.crossover/logs`. The file sink is what makes a headless,
//! service-launched worker diagnosable at all — its console output goes to
//! `NUL` (ADR 0011), so without a file the multi-day soak would be a black box.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, fmt};

/// Install the global tracing subscriber: console plus a rolling file in
/// `~/.crossover/logs`.
///
/// Filtering follows `RUST_LOG` when set (e.g. `RUST_LOG=crossover=debug`)
/// and defaults to `info`: enough to observe every important state
/// transition (NFR-3) without per-event input chatter.
///
/// Returns a [`WorkerGuard`] the caller must keep alive for the process
/// lifetime — dropping it flushes and stops the file writer — or `None` when
/// file logging could not be set up (no home directory, or the log directory
/// is not writable), in which case console logging still installs.
///
/// # Errors
///
/// Fails if a global subscriber is already installed; the subscriber is
/// process-global, so this must be called exactly once, from `main`.
pub fn init() -> anyhow::Result<Option<WorkerGuard>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Console: unchanged behavior (ANSI, target shown). Off Windows and in an
    // interactive shell this is the primary sink.
    let console = fmt::layer().with_target(true);

    // File: plain (no ANSI), so log files are greppable rather than full of
    // escape codes. Best-effort — a missing home dir or unwritable directory
    // means console-only, never a startup failure.
    let (file_layer, guard) = match file_appender() {
        Some((writer, guard)) => (
            Some(fmt::layer().with_ansi(false).with_writer(writer)),
            Some(guard),
        ),
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))?;
    Ok(guard)
}

/// A non-blocking, daily-rotating file writer in `~/.crossover/logs`, keeping a
/// week of files. `None` if the directory cannot be determined or created.
fn file_appender() -> Option<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let dir = crate::paths::log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("crossover")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    Some(tracing_appender::non_blocking(appender))
}

#[cfg(test)]
mod tests {
    // The subscriber is process-global: first install succeeds, a second
    // must fail loudly rather than silently replacing the first.
    #[test]
    fn init_installs_once_and_rejects_reinstall() {
        assert!(super::init().is_ok());
        assert!(super::init().is_err());
    }
}
