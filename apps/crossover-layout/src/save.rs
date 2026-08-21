//! Writing the drawn arrangement back to the worker's config file
//! (ADR 0018), and numbering it.
//!
//! This is the whole return path. The worker publishes
//! `~/.crossover/state/topology.json` and the editor reads it; an edit
//! travels the other way through `config.toml`, which the worker re-reads
//! on its ~2 s modification-time poll. There is no socket, no pipe, and no
//! lifetime coupling between the two processes (ADR 0019) — a save is a
//! file write, and the loop closes because the worker is already watching
//! that file.
//!
//! # The revision, and why it is `seen_max + 1`
//!
//! Convergence is newest-revision-wins on the key `(revision, origin)`
//! (ADR 0018), and the ADR fixes what an editor assigns:
//! `seen_max.saturating_add(1)` — one past the highest revision it has
//! seen **from either side**. Two sources are consulted, and both matter:
//!
//! - **The state file's layout revision** ([`crate::model::Model::seen_revision`]),
//!   which is what the worker currently holds — including a layout adopted
//!   from the peer moments ago.
//! - **The config file's own `[layout] revision`**, read at the moment of
//!   the write. The worker may have persisted an adoption that the state
//!   file has not caught up with, and a save numbered below it would be
//!   silently superseded the instant the worker read it back — an edit
//!   that vanished with no diagnostic, which is exactly the failure ADR
//!   0018's supersession rule exists to make observable.
//!
//! Saturating, so a peer asserting `u64::MAX` cannot wrap the counter and
//! cannot brick editing: the local edit ties at the ceiling and the
//! deterministic `(revision, origin)` tiebreak still resolves it.
//!
//! The **origin** is this machine's device id, taken from the state file
//! the worker wrote (`model.local.device`). It is an identity, not a
//! choice: the origin is the device that drew the arrangement, and that is
//! this one.

use std::error::Error as _;
use std::fmt;
use std::path::Path;

use crossover_topology::{Layout, PersistError, persist_layout};

use crate::model::{Model, SceneError};
use crate::paths;

/// Why a save did not happen.
///
/// Written out by hand rather than derived: `thiserror` is not one of this
/// crate's direct dependencies, and ADR 0019 fixes that list at the GUI
/// stack, `crossover-topology`, and the `tracing` family. Three variants
/// are not worth an amendment.
///
/// No `PartialEq`: [`SaveError::Persist`] carries a `PersistError`, whose
/// I/O cause is worth more than comparability (docs/ARCHITECTURE.md §9).
#[derive(Debug)]
pub enum SaveError {
    /// No home directory, so no `~/.crossover/config.toml` to write. The
    /// same condition that leaves the editor with no state file to read.
    NoConfigPath,
    /// The drawing is not a layout. Blocked before any file is touched —
    /// the Save button is already disabled for this, and this is the
    /// belt-and-braces check on the way to the filesystem.
    Scene(SceneError),
    /// The write itself failed, or was refused (an unparseable existing
    /// file is never clobbered).
    Persist(PersistError),
}

impl fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigPath => {
                formatter.write_str("no home directory, so there is no config file to write")
            }
            Self::Scene(_) => formatter.write_str("the arrangement is not valid"),
            Self::Persist(_) => formatter.write_str("writing the config file failed"),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoConfigPath => None,
            Self::Scene(error) => Some(error),
            Self::Persist(error) => Some(error),
        }
    }
}

impl SaveError {
    /// This failure and every cause under it, as one line — what the
    /// status bar shows. `PersistError`'s own message names the operation
    /// ("replacing the config file failed") and its source names the
    /// system's reason, and a user diagnosing a read-only profile needs
    /// both (NFR-3).
    #[must_use]
    pub fn chain(&self) -> String {
        let mut message = self.to_string();
        let mut cause: Option<&dyn std::error::Error> = self.source();
        while let Some(error) = cause {
            message.push_str(": ");
            message.push_str(&error.to_string());
            cause = error.source();
        }
        message
    }
}

/// The revision a save claims: one past the highest anything has seen.
///
/// See the module doc. `config` is `None` when the config file is absent
/// or holds no readable `[layout]` — a first save on a fresh install, or
/// on a schema-1 file that still has `[seamless] side`.
#[must_use]
pub fn next_revision(state_file: u64, config: Option<u64>) -> u64 {
    state_file.max(config.unwrap_or(0)).saturating_add(1)
}

/// Write `model` to the worker's config file.
///
/// Resolves `~/.crossover/config.toml` (see [`crate::paths::config_path`]),
/// reads the revision already there, and hands the whole thing to
/// [`crossover_topology::persist_layout`], which preserves every other
/// section, key order, and comment in the file.
///
/// # Errors
///
/// [`SaveError`]. Nothing is written unless the arrangement validates.
pub fn save(model: &Model) -> Result<u64, SaveError> {
    let path = paths::config_path().ok_or(SaveError::NoConfigPath)?;
    save_to(&path, model)
}

/// [`save`], against a named path — the seam its tests write through.
///
/// # Errors
///
/// [`SaveError`].
pub fn save_to(path: &Path, model: &Model) -> Result<u64, SaveError> {
    let revision = next_revision(
        model.seen_revision,
        crossover_topology::read_layout_revision(path),
    );
    let layout = build(model, revision)?;
    persist_layout(path, &layout).map_err(SaveError::Persist)?;
    Ok(revision)
}

/// The arrangement `model` draws, at `revision`, drawn by this machine.
fn build(model: &Model, revision: u64) -> Result<Layout, SaveError> {
    model.to_layout(revision).map_err(SaveError::Scene)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{SaveError, next_revision, save_to};
    use crate::model::Model;
    use crate::test_support::{
        ARRANGED_REVISION, LOCAL_DEVICE, PEER_DEVICE, Sandbox, arranged_document, document,
        drag_until_dirty, monitor_key, unit_viewport,
    };
    use crossover_topology::LayoutSection;

    /// Re-read what was written the way the worker's own loader does:
    /// through `toml` into a [`LayoutSection`], then through the shared
    /// validation. `apps/crossover`'s `LoadedConfig` is not importable
    /// here — it belongs to a binary crate, and ADR 0019 forbids the edge
    /// anyway — so this reproduces its two steps against the same types
    /// that loader uses.
    fn reread(path: &Path) -> LayoutSection {
        let text = std::fs::read_to_string(path).expect("read back");
        let document: toml::Value = toml::from_str(&text).expect("parse back");
        assert_eq!(
            document
                .get("schema_version")
                .and_then(toml::Value::as_integer),
            Some(i64::from(crossover_topology::CONFIG_SCHEMA_VERSION)),
            "a write is always an upgrade to the current schema"
        );
        document
            .get("layout")
            .cloned()
            .expect("a written config has a [layout]")
            .try_into()
            .expect("the section deserializes as the worker's loader would")
    }

    /// A scene that has actually been dragged, so it is dirty and worth
    /// saving.
    fn dirty_model() -> Model {
        let mut model = Model::from_state(&arranged_document(0));
        drag_until_dirty(&mut model);
        model
    }

    /// The whole loop, file to file: the bytes a worker would have written
    /// to `topology.json` go in, a drag and a snap happen, and what comes
    /// out is a `config.toml` the worker's own parser accepts with an
    /// **exactly abutting** arrangement in it.
    ///
    /// Everything either side of this is covered in smaller pieces; this
    /// is the one test that never leaves the real types — `parse_state`
    /// (the editor's reader), `Model`, `snap`, `persist_layout`, `toml` —
    /// so a break anywhere along the chain surfaces here as a failure
    /// rather than as a working editor that saves the wrong file.
    #[test]
    fn a_state_file_becomes_a_saved_arrangement_with_an_exact_seam() {
        let sandbox = Sandbox::new("loop");
        let path = sandbox.path("config.toml");

        // 1. What the worker published, as JSON on disk would have it.
        let json = crossover_topology::serialize_state(&document(
            Some(crate::test_support::peer_state(true)),
            crossover_topology::now_unix_millis(),
        ))
        .expect("serialize the fixture state");
        let state = crossover_topology::parse_state(&json).expect("the editor reads it back");

        // 2. Nothing has been drawn yet, so the editor seeds a guess and
        //    says the machines do not touch.
        let mut model = Model::from_state(&state);
        assert!(model.seeded);
        assert_eq!(model.diagnostics().warnings.len(), 1);
        assert!(
            !model.can_save(),
            "a seed nobody has touched is not an edit"
        );

        // 3. Drag the local machine toward the peer, stopping *short* of
        //    the seam — the snap closes the remaining gap.
        let seed_gap = model.peer.as_ref().unwrap().monitors[0].rect.left()
            - model.local.monitors[0].rect.right();
        assert!(seed_gap > 0, "the seed leaves a gap to close");
        let target = monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY1");
        #[allow(clippy::cast_precision_loss)]
        let asked_for = (seed_gap - 6) as f64;
        model.begin_drag(&target, (0.0, 0.0), unit_viewport());
        model.drag_to((asked_for, 0.0));
        model.end_drag();
        assert!(model.can_save());
        assert!(
            model.diagnostics().warnings.is_empty(),
            "the snap should have made a seam: {:?}",
            model.diagnostics()
        );

        // 4. Save, and read the file the worker would read.
        let revision = save_to(&path, &model).expect("save");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("schema_version = 2"), "{written}");
        assert!(
            written.contains(&format!("revision = {revision}")),
            "{written}"
        );

        let layout = reread(&path)
            .validate_standalone()
            .expect("the worker's own validation accepts it");
        let local = layout
            .monitors()
            .iter()
            .find(|monitor| monitor.device == LOCAL_DEVICE)
            .expect("this machine is in the file");
        let peer = layout
            .monitors()
            .iter()
            .find(|monitor| monitor.device == PEER_DEVICE)
            .expect("the peer is in the file");
        assert_eq!(
            local.rect.right(),
            peer.rect.left(),
            "the saved seam must be exact — zero tolerance is what the \
             derivation needs (ADR 0018)"
        );
    }

    #[test]
    fn a_save_writes_a_layout_the_worker_can_read_back_and_validate() {
        let sandbox = Sandbox::new("save");
        let path = sandbox.path("config.toml");
        let model = dirty_model();

        let revision = save_to(&path, &model).expect("save");
        let section = reread(&path);
        assert_eq!(section.revision, revision);
        assert_eq!(section.origin, LOCAL_DEVICE);
        let layout = section
            .validate_standalone()
            .expect("the written layout validates on its own terms");
        assert_eq!(layout.revision(), revision);
        assert_eq!(layout.monitors().len(), model.placed().len());
        assert!(
            layout
                .monitors()
                .iter()
                .any(|monitor| monitor.device == PEER_DEVICE),
            "both machines must be written"
        );
    }

    /// A fresh install has no config file at all — `persist_layout`'s
    /// create path, exercised from this side so the first save a new user
    /// makes is covered rather than assumed.
    #[test]
    fn a_first_save_creates_the_file_and_its_directory() {
        let sandbox = Sandbox::new("create");
        let path = sandbox.path("nested").join("config.toml");
        assert!(!path.exists());
        let revision = save_to(&path, &dirty_model()).expect("save");
        // Nothing in the (absent) file to beat, so the state file's own
        // revision is the whole of `seen_max`.
        assert_eq!(revision, ARRANGED_REVISION + 1);
        assert_eq!(reread(&path).revision, revision);
    }

    /// The revision exceeds both sources it is derived from, and cannot
    /// wrap (ADR 0018).
    #[test]
    fn the_revision_is_one_past_the_highest_either_source_has_seen() {
        assert_eq!(next_revision(0, None), 1);
        assert_eq!(next_revision(7, None), 8);
        assert_eq!(next_revision(0, Some(12)), 13);
        assert_eq!(next_revision(12, Some(3)), 13);
        assert_eq!(next_revision(u64::MAX, Some(4)), u64::MAX);
        assert_eq!(next_revision(4, Some(u64::MAX)), u64::MAX);
    }

    /// The config file's revision counts even when the state file's is
    /// lower — the worker adopting the peer's arrangement is exactly this
    /// case, and a save numbered under it would be superseded on sight.
    #[test]
    fn a_higher_revision_already_in_the_config_file_is_beaten_not_ignored() {
        let sandbox = Sandbox::new("existing");
        let path = sandbox.path("config.toml");
        let first = save_to(&path, &dirty_model()).expect("first save");
        assert_eq!(first, ARRANGED_REVISION + 1);

        // A second model built from the same state file still believes the
        // highest revision *it* has seen is the fixture's, but the config
        // file now holds a higher one — so the next save must beat the
        // file, not the model.
        let second = save_to(&path, &dirty_model()).expect("second save");
        assert_eq!(second, first + 1);
        assert_eq!(reread(&path).revision, second);
    }

    /// Everything else in the file survives a save — the property
    /// `toml_edit` was chosen for, asserted from the editor's side because
    /// this is the writer a *user's* file actually meets.
    #[test]
    fn a_save_preserves_the_rest_of_the_users_config() {
        let sandbox = Sandbox::new("preserve");
        let path = sandbox.path("config.toml");
        std::fs::write(
            &path,
            "# my config\nschema_version = 1\n\n[device]\nname = \"desk\"\n\n[seamless]\nside = \"right\"\n",
        )
        .unwrap();

        save_to(&path, &dirty_model()).expect("save");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# my config"), "{written}");
        assert!(written.contains("name = \"desk\""), "{written}");
        assert!(written.contains("schema_version = 2"), "{written}");
        assert!(!written.contains("side ="), "{written}");
    }

    /// A scene with no peer cannot be a layout, and the refusal happens
    /// before the filesystem is touched.
    #[test]
    fn a_scene_with_no_peer_is_refused_without_writing_anything() {
        let sandbox = Sandbox::new("nopeer");
        let path = sandbox.path("config.toml");
        let model = Model::from_state(&document(None, 0));
        let error = save_to(&path, &model).expect_err("no peer, no layout");
        assert!(matches!(error, SaveError::Scene(_)), "{error:?}");
        assert!(!path.exists(), "nothing may be written for a refused save");
    }

    /// A file that is not TOML is never overwritten, and the editor
    /// reports the whole chain rather than "save failed".
    #[test]
    fn an_unparseable_config_is_refused_with_a_readable_chain() {
        let sandbox = Sandbox::new("corrupt");
        let path = sandbox.path("config.toml");
        let corrupt = "[network\nconnect = broken\n";
        std::fs::write(&path, corrupt).unwrap();

        let error = save_to(&path, &dirty_model()).expect_err("refused");
        let chain = error.chain();
        assert!(chain.contains("writing the config file failed"), "{chain}");
        assert!(chain.contains("not valid TOML"), "{chain}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
    }

    /// The origin is this machine — the device the state file names as
    /// local, never the peer, and never whoever drew the arrangement that
    /// was on screen a moment ago.
    #[test]
    fn the_origin_is_this_machine() {
        let sandbox = Sandbox::new("origin");
        let path = sandbox.path("config.toml");
        // The fixture's saved layout was drawn by the peer, so an origin
        // copied from what was loaded would be wrong here.
        assert_eq!(
            arranged_document(0).layout.expect("fixture layout").origin,
            PEER_DEVICE
        );
        save_to(&path, &dirty_model()).expect("save");
        assert_eq!(reread(&path).origin, LOCAL_DEVICE);
    }
}
