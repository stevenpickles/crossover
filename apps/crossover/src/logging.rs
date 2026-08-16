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
/// is not writable), in which case console logging still installs and a
/// warning names the reason.
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
    let (file_layer, guard, unavailable) = match file_appender() {
        Ok((writer, guard)) => (
            Some(fmt::layer().with_ansi(false).with_writer(writer)),
            Some(guard),
            None,
        ),
        Err(reason) => (None, None, Some(reason)),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))?;

    // Said out loud, not swallowed. Losing the file sink turns a
    // service-launched run into a black box (ADR 0011 sends its console to
    // NUL), so "why is there no log file?" has to be answerable at all —
    // and it can only be answered where a console still exists.
    if let Some(reason) = unavailable {
        tracing::warn!(
            reason,
            "file logging unavailable; this run logs to the console only"
        );
    }
    Ok(guard)
}

/// Route panics through `tracing` before the default hook prints them.
///
/// A service-launched worker's stderr is `NUL` (ADR 0011), so a panic used
/// to be perfectly invisible: the process vanished, the supervisor
/// relaunched it on backoff, and the log said nothing at all. That is the
/// "exits before its run loop, with no reason recorded" case the Phase 6
/// soak turned up. The default hook still runs afterwards, so an
/// interactive run's output is unchanged.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(
            panic = panic_message(info.payload()),
            location = info.location().map(ToString::to_string),
            "panicked"
        );
        previous(info);
    }));
}

/// The human-readable part of a panic payload. `panic!` produces a `&str`
/// for a literal and a `String` once formatted; anything else is a custom
/// payload nothing can render usefully.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic payload".to_owned())
        },
        |message| (*message).to_owned(),
    )
}

/// A non-blocking, daily-rotating file writer in `~/.crossover/logs`, keeping a
/// week of files.
///
/// The error carries *why* rather than a bare `None`: this sink failing is
/// the difference between a diagnosable soak and a black box, so the reason
/// is worth more than the fact.
fn file_appender() -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard), String> {
    let dir = crate::paths::log_dir()
        .ok_or_else(|| "no home directory to place ~/.crossover/logs in".to_owned())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("crossover")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .map_err(|e| format!("opening a rolling log in {}: {e}", dir.display()))?;
    Ok(tracing_appender::non_blocking(appender))
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

    /// Both shapes `panic!` actually produces have to render, because the
    /// message is the whole value of logging the panic at all.
    #[test]
    fn panic_payloads_render_for_both_literal_and_formatted_panics() {
        assert_eq!(super::panic_message(&"a literal"), "a literal");
        assert_eq!(
            super::panic_message(&format!("formatted {}", 7)),
            "formatted 7"
        );
    }

    /// An unrenderable payload must still produce a line: "something
    /// panicked and we cannot say what" beats silence.
    #[test]
    fn an_unrenderable_panic_payload_still_says_something() {
        assert_eq!(super::panic_message(&42_u32), "non-string panic payload");
    }
}
