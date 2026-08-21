//! User-facing file locations this editor needs, under `~/.crossover`.
//!
//! **Hand-copied from `apps/crossover/src/paths.rs`**, not a dependency on
//! the `crossover` binary crate: ADR 0019 fixes this crate's dependency
//! graph at the GUI stack and `crossover-topology` alone (plus, as of the
//! logging amendment, the `tracing` family — see `Cargo.toml`), precisely
//! so the process a person clicks in never links the crate that owns the
//! trust store, the protocol decoder, or the input injector. Kept in
//! lockstep by hand — the same env-var order, the same empty-string guard
//! — because the service-launched worker and this editor must resolve the
//! same files from the same environment. A few lines of `env::var_os`
//! duplicated with a comment naming the coupling is the trade ADR 0011
//! already made for `crossover-svc`'s equivalents, applied here rather
//! than discovered.

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

    use super::compose_state_path;

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
}
