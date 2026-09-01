//! User-facing file locations this editor needs, under `~/.crossover`.
//!
//! The *resolution* is this module's, not a dependency on the `crossover`
//! binary crate: ADR 0019 fixes this crate's dependency graph at the GUI
//! stack and `crossover-topology` alone (plus, as of the logging amendment,
//! the `tracing` family — see `Cargo.toml`), precisely so the process a
//! person clicks in never links the crate that owns the trust store, the
//! protocol decoder, or the input injector. So the few lines of
//! `env::var_os` below are duplicated deliberately — the same env-var
//! order and the same empty-string guard as `apps/crossover/src/paths.rs`,
//! because the service-launched worker and this editor must resolve the
//! same home from the same environment. That is the trade ADR 0011 already
//! made for `crossover-svc`'s equivalents.
//!
//! What is **not** duplicated is any *file name*. Both files this editor
//! touches are named by `crossover-topology`, the one crate both binaries
//! share: [`crossover_topology::STATE_FILE_RELATIVE_PATH`] for the worker's
//! report and [`crossover_topology::CONFIG_FILE_NAME`] for the config it
//! writes. A name held in lockstep by hand is a name that eventually drifts,
//! and an editor writing a file the worker does not read would look exactly
//! like a save that did nothing.

use std::path::{Path, PathBuf};

use crossover_topology::STATE_FILE_RELATIVE_PATH;

/// The user's home directory: `%USERPROFILE%` on Windows, `$HOME` elsewhere.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// `~/.crossover` — the base every file this editor and the worker share
/// sits under.
fn crossover_home() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".crossover"))
}

/// `~/.crossover/state/topology.json`, or `None` when the home directory
/// cannot be resolved — which callers treat exactly like a missing file,
/// since there is equally nothing to read.
#[must_use]
pub fn state_file_path() -> Option<PathBuf> {
    crossover_home().map(|home| compose_state_path(&home))
}

/// `~/.crossover/config.toml` — the worker's own startup input, and the
/// one file this editor **writes** (ADR 0018: an edit reaches the worker
/// through the config file, which it re-reads on a ~2 s
/// modification-time poll).
///
/// Composed by [`crossover_topology::config_path_in`], the same call
/// `apps/crossover/src/paths.rs` makes, so the two binaries cannot disagree
/// about which file this is (module doc).
///
/// `None` when the home directory cannot be resolved, which callers treat
/// as "there is nowhere to save" — the same answer they already give for
/// "there is nothing to read".
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    crossover_home().map(|home| crossover_topology::config_path_in(&home))
}

/// `~/.crossover/logs` — the same rolling-log directory `crossover.exe`
/// and `crossover-svc.exe` already write to (`apps/crossover/src/logging.rs`),
/// so a diagnostic from any of the three binaries of one install ends up
/// in one place a user or a soak script already knows to look.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    crossover_home().map(|home| home.join("logs"))
}

/// The pure half of [`state_file_path`]: join a known `~/.crossover` onto
/// the shared relative path. Split out so the composition can be tested
/// against a fixed, fake home without touching real environment variables
/// — this workspace forbids `unsafe` outright (NFR-6), and mutating
/// process environment for a test is `unsafe` as of this edition.
fn compose_state_path(crossover_home: &Path) -> PathBuf {
    crossover_home.join(STATE_FILE_RELATIVE_PATH)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{compose_state_path, config_path, state_file_path};
    use crossover_topology::CONFIG_FILE_NAME;

    /// Built through the same `Path` joins the real resolution uses,
    /// rather than a hand-written path literal: a literal spelled with
    /// backslashes compares unequal to the composed `PathBuf` on the
    /// Linux/macOS CI legs, where a backslash is an ordinary filename byte
    /// rather than a separator — this construction is correct on all
    /// three regardless.
    #[test]
    fn the_composed_path_ends_in_the_shared_relative_path() {
        let home = Path::new("fake-home").join(".crossover");
        let path = compose_state_path(&home);
        let expected: PathBuf = [
            home.as_path(),
            Path::new("state"),
            Path::new("topology.json"),
        ]
        .into_iter()
        .collect();
        assert_eq!(path, expected);
    }

    /// Both files this editor touches resolve from one `~/.crossover`, so
    /// a machine where the state file is readable is a machine where the
    /// config file is writable — the property the save flow's
    /// "no home directory" branch is the negation of. Reads the real
    /// environment rather than mutating it, for the reason
    /// `apps/crossover/src/paths.rs`'s own tests give: env vars are
    /// process-global and tests run concurrently.
    #[test]
    fn the_config_file_sits_beside_the_state_file_under_one_home() {
        let (Some(config), Some(state)) = (config_path(), state_file_path()) else {
            assert_eq!(config_path().is_some(), state_file_path().is_some());
            return;
        };
        assert_eq!(
            config.file_name().and_then(|name| name.to_str()),
            Some(CONFIG_FILE_NAME)
        );
        assert_eq!(config.parent(), state.parent().and_then(Path::parent));
    }
}
