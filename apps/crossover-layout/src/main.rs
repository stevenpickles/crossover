//! `crossover-layout` — the Crossover **display layout editor** (ADR 0019).
//!
//! The third binary of an install, and the only one with a window. The worker
//! (`crossover.exe run`) is headless and service-launched (ADR 0011), so the
//! editor is a separate user-session surface rather than a mode of it: the
//! user starts it on demand, arranges their monitors, and closes it.
//!
//! ## What it does not contain
//!
//! It never opens a socket, reads the clipboard, injects input, or touches the
//! trust store, and its dependency graph says so: the GUI stack,
//! `crossover-topology` for the layout model and the state-file schema, and —
//! as of ADR 0019's logging amendment — the `tracing` family for the one
//! diagnostic a console-less release binary otherwise has nowhere to put
//! (see `Cargo.toml`). Everything it exchanges with the worker travels
//! through files: the state file the worker publishes at
//! `~/.crossover/state/`, which it reads once a second, and the `[layout]`
//! section of `config.toml`, which it writes when the user saves and which
//! the worker re-reads on its own ~2 s poll (ADR 0018). So the editor runs
//! at plain integrity, needs no elevation, and is a process the service
//! never starts, stops, or knows about.

// Windows gives a console-subsystem process a console window, and one started
// from Explorer or the Start menu would carry that black window for its whole
// life — a defect in the only part of Crossover a user looks at. Release
// builds therefore link as a GUI subsystem binary; debug builds keep the
// console so `println!` and a panic message stay visible while developing.
//
// The cost, accepted rather than discovered: in a release build the version
// report below reaches a *redirected* stdout (a script, a pipe) but not the
// console of a shell that ran it by hand. Attaching to the parent console is
// Win32, which this crate deliberately cannot reach (ADR 0019) — and the same
// facts are in the exe's version resource, which is where Explorer and the
// packaging script read them anyway. This is also exactly why `logging.rs`
// exists: a release run's *other* diagnostics (a state file that could not
// be used) need a sink that does not depend on a console existing at all.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod logging;
mod model;
mod paths;
mod render;
mod save;
mod session;
mod snap;
mod state_file;
#[cfg(test)]
mod test_support;
mod viewport;

// The identity source shared with `crossover.exe` and `crossover-svc.exe`, so
// all three binaries of one install report the same build. An included source
// file rather than a crate, which is what keeps the dependency graph above as
// short as it claims to be (apps/build_identity.rs).
#[path = "../../build_info.rs"]
mod build_info;

use std::process::ExitCode;

/// The window's title, and the name eframe gives the application to the
/// window manager.
const WINDOW_TITLE: &str = "Crossover Display Layout";

fn main() -> ExitCode {
    // Answered before any window — or any logging — exists: the packaging
    // script and anyone inspecting an installed copy ask this of all three
    // binaries, and they must answer it identically — which is why the
    // parser is shared with `crossover-svc` rather than copied
    // (apps/build_info.rs).
    if build_info::reported_version() {
        return ExitCode::SUCCESS;
    }

    // Kept alive for the rest of `main`: dropping it flushes and stops the
    // non-blocking file writer. `logging::init` is best-effort and never
    // fails startup — see its own doc for why a missing log sink cannot
    // usefully abort the one thing this binary exists to do.
    let _logging_guard = logging::init();

    match app::run(WINDOW_TITLE) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The editor may well have no console to print to, but a message
            // is still worth writing: it reaches a terminal when there is one,
            // and a redirected launch otherwise. A window that cannot open is
            // the one failure this binary has to report by other means —
            // unlike the state-file diagnostics `app.rs` logs through
            // `tracing`, this happens before any window (and often before
            // the log file's first flush) exists to explain itself with.
            eprintln!("Crossover's layout editor could not open its window: {error}");
            ExitCode::FAILURE
        }
    }
}
