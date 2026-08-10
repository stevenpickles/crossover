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

#[cfg(windows)]
fn main() {
    // The service has no console; log to stderr (the SCM/event pipeline
    // captures it) so a launch or supervision failure is never silent (NFR-3).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    // Returns () today (stub); becomes a Result the dispatcher propagates once
    // the SCM/launcher lands, at which point main gains error handling.
    crossover_platform_windows::run_service_daemon();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "crossover-svc is the Windows service daemon (ADR 0011) and runs only \
         on Windows. Elsewhere, auto-start uses the OS-native user-session \
         mechanism (Linux: systemd --user; macOS: launchd) with no launcher."
    );
    std::process::exit(1);
}
