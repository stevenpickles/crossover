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

/// `~/.crossover/state` — the directory the worker's state file for the
/// layout editor lives in ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)).
///
/// A path resolution only, like [`log_dir`]: the directory is created on
/// demand by whoever writes into it, the way `logging.rs` creates
/// [`log_dir`] the first time it opens a log file.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    crossover_home().map(|home| home.join("state"))
}

/// `~/.crossover/state/topology.json` — the worker→editor state file
/// ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)).
///
/// Built from [`crossover_topology::STATE_FILE_RELATIVE_PATH`] rather than a
/// literal here, so the worker that writes it and the editor that reads it
/// cannot disagree about the name — the same reasoning that constant's own
/// docs give.
#[must_use]
pub fn topology_state_path() -> Option<PathBuf> {
    crossover_home().map(|home| home.join(crossover_topology::STATE_FILE_RELATIVE_PATH))
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

#[cfg(test)]
mod tests {
    use super::{state_dir, topology_state_path};

    // These read the real process environment rather than mutating it: env
    // vars are process-global, and tests run concurrently, so scripting
    // `USERPROFILE`/`HOME` here would be a data race with every other test
    // that resolves a path. Every environment this build's tests run in
    // (a dev machine, CI on any of the three OSes) already has a home
    // directory, so the "no home resolvable" branch is exercised as a
    // no-op guard rather than forced.

    #[test]
    fn topology_state_path_sits_under_state_dir_as_topology_json() {
        let Some(dir) = state_dir() else {
            // No home directory in this environment: every path function
            // here agrees there is nothing to resolve, checked below.
            return;
        };
        let path = topology_state_path()
            .expect("state_dir resolved a home directory; topology_state_path must too");
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("topology.json")
        );
    }

    #[test]
    fn state_dir_and_topology_state_path_agree_on_whether_a_home_exists() {
        assert_eq!(state_dir().is_some(), topology_state_path().is_some());
    }
}
