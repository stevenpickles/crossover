//! User-facing file locations, under `~/.crossover`.
//!
//! Config and logs are things a user reads and edits, so they live in a
//! discoverable dotfolder in the home directory (`~/.crossover`) rather than
//! buried in `%LOCALAPPDATA%`. Secrets are the deliberate exception: identity
//! and the trust store stay DPAPI-encrypted under `%LOCALAPPDATA%\Crossover`
//! (non-roaming, machine-bound), owned by the platform secure-storage backend —
//! a location is a security decision for a machine-bound blob, not a
//! convenience one, so it does not move here.
//!
//! Resolved from the user's home so the service-launched worker — which runs as
//! the console user with that user's environment — finds the same files the
//! interactive CLI does.

use std::path::PathBuf;

/// `~/.crossover` — the base for user-editable Crossover files. `None` when the
/// home directory cannot be determined (then there is simply no config file and
/// no file logging).
#[must_use]
pub fn crossover_home() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".crossover"))
}

/// `~/.crossover/config.toml`.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    crossover_home().map(|home| home.join("config.toml"))
}

/// `~/.crossover/logs` — the directory for rotating log files.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    crossover_home().map(|home| home.join("logs"))
}

/// The user's home directory: `%USERPROFILE%` on Windows, `$HOME` elsewhere.
/// Explicit rather than `std::env::home_dir` so the resolution is obvious and
/// identical in the service worker's inherited environment.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}
