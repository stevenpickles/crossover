//! `crossover-svc` — the Crossover **service daemon** (ADR 0011).
//!
//! This is **not** the Crossover application; it is a separate, minimal binary.
//! It is a Windows `LocalSystem` service whose sole job is to launch and
//! supervise the real worker (`crossover.exe run`) inside the interactive user
//! session — a session-0 service cannot inject input or capture the desktop, so
//! it delegates all of that to the user-privilege worker.
//!
//! ## Why a separate binary from `crossover.exe`, not a mode of it
//!
//! Isolation of the privileged process. `crossover-svc` depends only on
//! `crossover-platform-windows`; it never links `crossover-core`, `-protocol`,
//! or `-security`. So the SYSTEM-privileged daemon contains **no** network,
//! TLS, clipboard, or protocol-parsing code — it *cannot* process untrusted
//! input, because that code is not in the binary. The "launcher never touches
//! attacker-reachable input" security invariant (ADR 0011) is therefore
//! enforced by the dependency graph, not by discipline.
//!
//! All attackable surface (TLS, framing, pairing, injection) lives in
//! `crossover.exe`, which the daemon starts **as the ordinary user** — no
//! privilege escalation of the input path. `crossover.exe service
//! install/uninstall/status` manages this daemon's registration with the SCM;
//! `crossover-svc` is the thing the SCM then runs.

// The identity source shared with `crossover.exe`. It is an included source
// file rather than a crate so that reporting a version costs this binary no
// dependency edge — the isolation described above is the whole point
// (apps/build_identity.rs).
#[path = "../../build_info.rs"]
mod build_info;

// Windows-only: this is a Windows-service concept (ADR 0011), and the file
// sink resolves `%ProgramData%`, a Windows environment variable.
#[cfg(windows)]
mod logging;

fn main() {
    // Handled before anything else: this binary is normally started by the
    // SCM with no arguments, so the only way it sees any is a human asking
    // what it is — typically of the copy sitting in Program Files. The parser
    // lives in the shared identity module, so this binary and the editor
    // cannot drift into disagreeing about what asking looks like.
    if build_info::reported_version() {
        return;
    }
    run();
}

#[cfg(windows)]
fn run() {
    // The service has no console, and — unlike a foreground process — the SCM
    // does *not* capture or persist stderr anywhere a person would look; a
    // stderr-only sink here was simply losing every supervision event (found
    // during the 2026-08-19 incident: a worker died with no record of why).
    // So logging goes to a durable rolling file under
    // `%ProgramData%\Crossover\logs` as well, so a launch or supervision
    // failure is never silent (NFR-3). The guard must outlive the run — held
    // here for the service's whole lifetime, since `run_service_daemon`
    // blocks until the SCM stops it.
    let _log_guard = logging::init();
    // Returns () today (stub); becomes a Result the dispatcher propagates once
    // the SCM/launcher lands, at which point main gains error handling.
    crossover_platform_windows::run_service_daemon();
}

#[cfg(not(windows))]
fn run() {
    eprintln!(
        "crossover-svc is the Windows service daemon (ADR 0011) and runs only \
         on Windows. Elsewhere, auto-start uses the OS-native user-session \
         mechanism (Linux: systemd --user; macOS: launchd) with no launcher."
    );
    std::process::exit(1);
}
