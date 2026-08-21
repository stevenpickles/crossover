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
//! trust store, and its dependency graph says so: the GUI stack and — once the
//! canvas lands — `crossover-topology` for the layout model, and nothing else
//! (see `Cargo.toml`). Everything it exchanges with the worker travels through
//! files: the state file the worker publishes at `~/.crossover/state/`, and the
//! `[layout]` section of `config.toml` (ADR 0018). So the editor runs at plain
//! integrity, needs no elevation, and is a process the service never starts,
//! stops, or knows about.

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
// packaging script read them anyway.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;

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
    // Answered before any window exists: the packaging script and anyone
    // inspecting an installed copy ask this of all three binaries, and they
    // must answer it identically — which is why the parser is shared with
    // `crossover-svc` rather than copied (apps/build_info.rs).
    if build_info::reported_version() {
        return ExitCode::SUCCESS;
    }

    match app::run(WINDOW_TITLE) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The editor may well have no console to print to, but a message
            // is still worth writing: it reaches a terminal when there is one,
            // and a redirected launch otherwise. A window that cannot open is
            // the one failure this binary has to report by other means.
            eprintln!("Crossover's layout editor could not open its window: {error}");
            ExitCode::FAILURE
        }
    }
}
