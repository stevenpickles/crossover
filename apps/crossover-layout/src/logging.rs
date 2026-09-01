//! Minimal, file-only structured logging for the layout editor (NFR-3).
//!
//! `crossover-layout` is a GUI-subsystem binary in release builds (ADR
//! 0019: `windows_subsystem = "windows"`), so a release run has no console
//! to write a diagnostic to at all — `eprintln!` reaches nowhere once
//! installed, which is exactly the gap a state-file read failure must not
//! fall into silently (NFR-3). This is the minimal version of
//! `apps/crossover/src/logging.rs`'s console-plus-rolling-file setup:
//! file only, into the same `~/.crossover/logs` directory the other two
//! binaries of an install already write to (`paths::log_dir`), so a
//! diagnostic from any of the three ends up in one place. No panic hook —
//! this editor's `main.rs` already reports its one fatal failure mode (no
//! window) by other means, and a panic inside egui's own paint loop is not
//! a case this branch adds handling for.
//!
//! See `docs/adr/0019-layout-editor-toolkit.md`'s dated amendment for why
//! `tracing`, `tracing-subscriber`, and `tracing-appender` are direct
//! dependencies here: `tracing` was already transitively present through
//! `winit`, and the other two are the smallest addition that makes a
//! console-less release binary diagnosable at all.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, fmt};

/// Install the global tracing subscriber, file-only, in `~/.crossover/logs`.
///
/// Best-effort and infallible to the caller: with no home directory, an
/// unwritable log directory, or a subscriber already installed, this is a
/// silent no-op — every `tracing::*!` call afterward simply goes nowhere,
/// exactly as it would have before this function ran. `main` cannot
/// usefully fail startup over a missing log sink, and there is nothing
/// louder than this sink itself to report its own absence with.
///
/// Returns the [`WorkerGuard`] the caller must keep alive for the process
/// lifetime — dropping it flushes and stops the file writer.
#[must_use]
pub fn init() -> Option<WorkerGuard> {
    let dir = crate::paths::log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("crossover-layout")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = fmt::layer().with_ansi(false).with_writer(writer);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .try_init()
        .ok()?;

    Some(guard)
}
