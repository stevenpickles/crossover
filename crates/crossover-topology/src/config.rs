//! The `[layout]` config section, and the one thing in the tree that
//! writes a config file (ADR 0018).
//!
//! Schema version 2 replaces `[seamless] side` with a drawn arrangement:
//!
//! ```toml
//! schema_version = 2
//!
//! [layout]
//! revision = 7
//! origin = "8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8"
//!
//! [[layout.monitor]]
//! device = "8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8"
//! id = '\\.\DISPLAY1'
//! x = 0
//! y = 0
//! width = 1920
//! height = 1080
//! ```
//!
//! # Why a format-preserving writer
//!
//! `~/.crossover/config.toml` has always been a hand-edited file, and this
//! phase gives it a **second and a third writer**: the layout editor, and
//! the worker adopting an arrangement drawn at the other desk. A
//! serialize-and-truncate writer would silently delete the user's comments,
//! their key ordering, and any section it did not know about — a write they
//! did not make, destroying work they did. So the write is a targeted
//! read-modify-write through `toml_edit`: `schema_version`, `[layout]`, and
//! the retired `[seamless] side` are the only things it touches.
//!
//! Two refusals are deliberate. **An unparseable existing file is never
//! clobbered** — a file we cannot understand is a file whose contents we
//! cannot preserve, and overwriting it would turn a typo into lost
//! configuration. And a **revision TOML cannot represent** is refused
//! rather than wrapped, because a wrapped revision persists a different
//! arrangement identity than the one that was adopted.
//!
//! The `[layout]` section itself is replaced wholesale rather than merged,
//! which is what makes a removed monitor disappear. Comments *inside*
//! `[layout]` do not survive a write; comments anywhere else do. That
//! section is machine-owned — the editor draws it — and a merge that tried
//! to keep stale rows alive would be the bug this replacement prevents.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::device::DeviceId;
use crate::layout::{DevicePair, Layout, LayoutError, LayoutRect, RawPlacedMonitor};

/// The config schema this build reads and writes (ADR 0018).
///
/// Version 1 — a `[seamless] side` file — still loads, as an *implicit*
/// layout that reproduces the old left–right behaviour; that compatibility
/// is the app's business. What this constant fixes is what a **write**
/// produces, and a write is always an upgrade to 2.
///
/// # Wired into the reader (feature/151)
///
/// `apps/crossover/src/config.rs` accepts this constant's range — 1
/// through [`CONFIG_SCHEMA_VERSION`] — reads `[layout]` into
/// [`LayoutSection`], and treats a v1 file or a lingering `side` as an
/// implicit layout, exactly as the coupling this doc used to describe as
/// outstanding required. What is **not** yet wired is the write side: no
/// caller in the app calls [`persist_layout`] yet — that lands with the
/// editor (feature/152) — so a v1 file stays v1 until then, and this
/// constant only ever describes what a future write would upgrade it to.
pub const CONFIG_SCHEMA_VERSION: u32 = 2;

/// Oldest config schema this build still reads (ADR 0018): a bare
/// `[seamless] side` file, with no `[layout]` section at all.
///
/// Named beside [`CONFIG_SCHEMA_VERSION`] rather than left as a literal at
/// each reader, because "which versions does this build understand" is one
/// fact, and the writer and every reader (today: `apps/crossover`;
/// tomorrow: the editor's own re-read) should compute it from the same two
/// constants — see [`config_schema_supported`].
pub const CONFIG_SCHEMA_MIN_SUPPORTED: u32 = 1;

/// Whether `version` is a config schema this build can read — the range
/// [`CONFIG_SCHEMA_MIN_SUPPORTED`] through [`CONFIG_SCHEMA_VERSION`],
/// inclusive. A reader's `schema_version` check should delegate to this
/// rather than re-deriving the bound, so the writer and every reader agree
/// on one definition instead of drifting literals.
#[must_use]
pub const fn config_schema_supported(version: u32) -> bool {
    version >= CONFIG_SCHEMA_MIN_SUPPORTED && version <= CONFIG_SCHEMA_VERSION
}

/// The `[layout]` section as it sits in the file: still untrusted, and
/// with monitor ids still bare strings.
///
/// Deserializing this proves the file is *shaped* like a layout. Proving
/// it *is* one — the counts, the bounds, the overlap rule, and that it
/// describes this session's pair — is [`LayoutSection::validate`], which
/// is the only way to get a [`Layout`] out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutSection {
    /// The arrangement's revision. Newest wins (ADR 0018).
    pub revision: u64,
    /// The device that drew it.
    pub origin: DeviceId,
    /// The placed monitors, as `[[layout.monitor]]` rows. Defaulted so a
    /// `[layout]` section with no rows is a shape error from
    /// [`LayoutError::NoMonitors`] — a rule with a diagnostic — rather
    /// than a serde message about a missing field.
    #[serde(default, rename = "monitor")]
    pub monitors: Vec<LayoutMonitorRow>,
}

/// One `[[layout.monitor]]` row.
///
/// Flat rather than nesting a rectangle, because this is a table a person
/// reads and occasionally edits: six keys at one indentation level, in the
/// order the ADR names them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutMonitorRow {
    /// The machine the screen is attached to.
    pub device: DeviceId,
    /// Its platform-supplied identity, validated by [`LayoutSection::validate`].
    pub id: String,
    /// Rightward offset of the left edge from the shared origin.
    pub x: i32,
    /// Downward offset of the top edge from the shared origin.
    pub y: i32,
    /// Extent along the horizontal axis.
    pub width: u32,
    /// Extent along the vertical axis.
    pub height: u32,
}

impl LayoutSection {
    /// The section that records `layout`.
    #[must_use]
    pub fn from_layout(layout: &Layout) -> Self {
        Self {
            revision: layout.revision(),
            origin: layout.origin(),
            monitors: layout
                .monitors()
                .iter()
                .map(|placed| LayoutMonitorRow {
                    device: placed.device,
                    id: placed.id.as_str().to_owned(),
                    x: placed.rect.x,
                    y: placed.rect.y,
                    width: placed.rect.width,
                    height: placed.rect.height,
                })
                .collect(),
        }
    }

    /// The two-device pair this section's own bytes imply: the origin, and
    /// the first other device its monitors mention.
    ///
    /// For a reader that does not yet know the *real* pair to validate
    /// against — a config loader, before this machine's identity is even
    /// loaded and before a session (and therefore a peer) exists; the
    /// editor's own re-read of what it just wrote — asking for the real
    /// pair is not an option. This is the fallback: derive the pair the
    /// file's *own* bytes name, so shape rules that do not need the real
    /// pair — the counts, the bounds, duplicate ids, overlap, "exactly two
    /// machines" — still run. What this does **not** prove is whether
    /// these two devices are actually *this session's* ends; that check
    /// belongs where the real pair is known (`crossover-core`, at session
    /// establishment) and is deliberately not attempted here.
    ///
    /// # Errors
    ///
    /// [`LayoutError::NoMonitors`] for an empty section — checked before
    /// any pair is derived, so an empty list is reported as itself rather
    /// than manufacturing a degenerate pair out of nothing. Otherwise
    /// [`LayoutError::DegeneratePair`] when every monitor belongs to the
    /// origin: not "no other device happened to be found", but the actual
    /// shape of a single-device file — there is no second machine to pair
    /// it with.
    pub fn implied_pair(&self) -> Result<DevicePair, LayoutError> {
        if self.monitors.is_empty() {
            return Err(LayoutError::NoMonitors);
        }
        let other = self
            .monitors
            .iter()
            .map(|row| row.device)
            .find(|&device| device != self.origin);
        match other {
            Some(other) => DevicePair::new(self.origin, other),
            None => Err(LayoutError::DegeneratePair {
                device: self.origin,
            }),
        }
    }

    /// Validate this section against the pair its own bytes imply
    /// ([`Self::implied_pair`]), for a caller that does not yet know the
    /// real session pair. See that method's docs for exactly what is, and
    /// is not, checked this way.
    ///
    /// # Errors
    ///
    /// [`LayoutError`], from either [`Self::implied_pair`] or
    /// [`Self::validate`].
    pub fn validate_standalone(&self) -> Result<Layout, LayoutError> {
        let pair = self.implied_pair()?;
        self.validate(&pair)
    }

    /// Validate this section as an arrangement of `pair`.
    ///
    /// # Errors
    ///
    /// [`LayoutError`], including [`LayoutError::UnexpectedDevice`] for the
    /// residue of a re-pair — a layout naming a machine that is no longer
    /// at the other end, which ADR 0018 treats as no layout at all rather
    /// than guessing which rectangles belonged to whom.
    pub fn validate(&self, pair: &DevicePair) -> Result<Layout, LayoutError> {
        // The count bound before the allocation, as everywhere else.
        if self.monitors.is_empty() {
            return Err(LayoutError::NoMonitors);
        }
        if self.monitors.len() > crate::layout::MAX_LAYOUT_MONITORS {
            return Err(LayoutError::TooManyMonitors {
                count: self.monitors.len(),
            });
        }
        let raw = self
            .monitors
            .iter()
            .map(|row| RawPlacedMonitor {
                device: row.device,
                id: row.id.clone(),
                rect: LayoutRect {
                    x: row.x,
                    y: row.y,
                    width: row.width,
                    height: row.height,
                },
            })
            .collect();
        Layout::from_raw(self.revision, self.origin, raw, pair)
    }
}

/// Why a layout could not be written to the config file.
///
/// No `PartialEq`: the I/O variants carry a `std::io::Error`, whose cause
/// is worth more than comparability here (docs/ARCHITECTURE.md §9).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PersistError {
    /// The existing file could not be read. Not-found is *not* this — an
    /// absent config is an empty document to add a `[layout]` to.
    #[error("reading the existing config file failed")]
    Read {
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The existing file is not valid TOML.
    ///
    /// Refused rather than replaced: a file we cannot parse is a file
    /// whose other sections we cannot preserve, and a user who mistyped a
    /// key should get a diagnostic, not a config silently rewritten
    /// without whatever else was in it.
    #[error("the existing config file is not valid TOML; refusing to overwrite it")]
    Unparseable {
        /// Where and why the parse failed.
        #[source]
        source: toml_edit::TomlError,
    },
    /// A revision past `i64::MAX`.
    ///
    /// TOML integers are signed 64-bit and a layout revision is a `u64`,
    /// so the very top of the range has no representation. Wrapping it
    /// would persist a *different* arrangement identity than the one
    /// adopted, so the write is refused instead — reachable only from a
    /// peer asserting more revisions than a machine could ever make, which
    /// ADR 0018 already treats as nonsense that must not be able to break
    /// anything. Publication to the live topology is unaffected; only the
    /// persistence degrades, observably.
    #[error("layout revision {revision} cannot be represented in TOML")]
    RevisionUnrepresentable {
        /// The revision that was offered.
        revision: u64,
    },
    /// The containing directory could not be created.
    #[error("creating the config directory failed")]
    CreateDirectory {
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The temporary file could not be written.
    #[error("writing the temporary config file failed")]
    Write {
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The temporary file could not replace the real one. The original
    /// file is untouched, which is the point of writing beside it first.
    #[error("replacing the config file failed")]
    Replace {
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

/// Where the write lands before it becomes the config file.
///
/// Beside the target, on the same volume — `rename` across volumes is a
/// copy, and a copy is not atomic. The rename then decides which of two
/// concurrent writers wins, atomically, with no window in which the file
/// is half a document.
///
/// The name carries **two** discriminators and both are load-bearing. The
/// process id separates the worker adopting a peer's arrangement from the
/// editor saving one. A process-wide counter separates two writes inside
/// *one* process: the worker's adoption path is driven by network input
/// and a future editor-triggered write is not, so two of them can be in
/// flight on the same runtime — and sharing a temporary name would have
/// one truncating the other's half-written file and then renaming that
/// over the real config, which is the exact failure the temp-and-rename
/// exists to prevent. Monotonic, so no two calls in this process collide
/// however they interleave.
fn temp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let name = path.file_name().map_or_else(
        || String::from("config.toml"),
        |n| n.to_string_lossy().into_owned(),
    );
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    directory.join(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write `layout` into the config file at `path`, preserving everything
/// else in it (ADR 0018).
///
/// Sets `schema_version` to [`CONFIG_SCHEMA_VERSION`], replaces `[layout]`,
/// and retires `[seamless] side` — dropping the `[seamless]` table once it
/// holds nothing else. Every other section, key order, and comment survives
/// byte for byte. An absent file is created; its containing directory is
/// created with it.
///
/// The write is atomic: the new document lands in a temporary file beside
/// the target and is then renamed over it, so a reader — the worker's own
/// ~2 s modification-time poll included — sees a whole document or the
/// previous one, never a half-written one.
///
/// # Errors
///
/// [`PersistError`]. Note the two refusals: an existing file that is not
/// valid TOML is never overwritten, and a revision past `i64::MAX` is not
/// written rather than being wrapped.
pub fn persist_layout(path: &Path, layout: &Layout) -> Result<(), PersistError> {
    let revision =
        i64::try_from(layout.revision()).map_err(|_| PersistError::RevisionUnrepresentable {
            revision: layout.revision(),
        })?;

    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(PersistError::Read { source }),
    };
    let mut document: DocumentMut = existing
        .parse()
        .map_err(|source| PersistError::Unparseable { source })?;

    document["schema_version"] = value(i64::from(CONFIG_SCHEMA_VERSION));
    retire_seamless(&mut document);
    document["layout"] = Item::Table(layout_table(layout, revision));

    let rendered = document.to_string();
    if let Some(directory) = path.parent()
        && !directory.as_os_str().is_empty()
    {
        std::fs::create_dir_all(directory)
            .map_err(|source| PersistError::CreateDirectory { source })?;
    }
    let temporary = temp_path(path);
    std::fs::write(&temporary, rendered).map_err(|source| PersistError::Write { source })?;
    if let Err(source) = std::fs::rename(&temporary, path) {
        // Best-effort cleanup; the original failure is the diagnostic, and
        // the real file is still whatever it was.
        let _ = std::fs::remove_file(&temporary);
        return Err(PersistError::Replace { source });
    }
    Ok(())
}

/// Retire the schema-1 `side` key, and the `[seamless]` table with it once
/// nothing else lives there.
///
/// Removing the key rather than the table is the careful half: `side` is
/// the only key `[seamless]` has today, so in practice the table goes — but
/// a key some later schema adds is not this function's to delete.
fn retire_seamless(document: &mut DocumentMut) {
    let Some(seamless) = document.get_mut("seamless").and_then(Item::as_table_mut) else {
        return;
    };
    seamless.remove("side");
    if seamless.is_empty() {
        document.remove("seamless");
    }
}

/// The `[layout]` table for `layout`, built fresh rather than merged.
fn layout_table(layout: &Layout, revision: i64) -> Table {
    let mut monitors = ArrayOfTables::new();
    for placed in layout.monitors() {
        let mut row = Table::new();
        row["device"] = value(placed.device.to_string());
        row["id"] = value(placed.id.as_str());
        row["x"] = value(i64::from(placed.rect.x));
        row["y"] = value(i64::from(placed.rect.y));
        row["width"] = value(i64::from(placed.rect.width));
        row["height"] = value(i64::from(placed.rect.height));
        monitors.push(row);
    }
    let mut table = Table::new();
    table["revision"] = value(revision);
    table["origin"] = value(layout.origin().to_string());
    table.insert("monitor", Item::ArrayOfTables(monitors));
    table
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    use super::{
        CONFIG_SCHEMA_MIN_SUPPORTED, CONFIG_SCHEMA_VERSION, LayoutMonitorRow, LayoutSection,
        PersistError, config_schema_supported, persist_layout, temp_path,
    };
    use crate::device::DeviceId;
    use crate::layout::tests::{LOCAL, PEER, monitor, pair, side_by_side, valid_layout};
    use crate::layout::{Layout, LayoutError, MAX_LAYOUT_MONITORS};

    /// A private directory removed on drop — the house substitute for a
    /// `tempfile` dependency (see `crossover-platform-windows`'s
    /// `test_support::Sandbox`).
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crossover-topology-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("sandbox");
            Self(dir)
        }

        fn path(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Whatever the writer left beside the target — there should never be
    /// anything.
    fn stray_files(directory: &Path) -> Vec<String> {
        std::fs::read_dir(directory)
            .expect("read sandbox")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect()
    }

    #[test]
    fn a_section_round_trips_through_the_layout_it_describes() {
        let layout = valid_layout();
        let section = LayoutSection::from_layout(&layout);
        assert_eq!(section.revision, 7);
        assert_eq!(section.origin, LOCAL);
        assert_eq!(section.monitors.len(), 2);
        assert_eq!(section.validate(&pair()).unwrap(), layout);
    }

    #[test]
    fn config_schema_supported_is_the_closed_range_one_through_current() {
        assert!(!config_schema_supported(0));
        assert!(config_schema_supported(CONFIG_SCHEMA_MIN_SUPPORTED));
        assert!(config_schema_supported(CONFIG_SCHEMA_VERSION));
        assert!(!config_schema_supported(CONFIG_SCHEMA_VERSION + 1));
        assert_eq!(CONFIG_SCHEMA_MIN_SUPPORTED, 1);
    }

    #[test]
    fn implied_pair_is_the_origin_and_the_first_other_device_the_monitors_name() {
        let section = LayoutSection::from_layout(&valid_layout());
        let implied = section.implied_pair().unwrap();
        assert!(implied.contains(LOCAL) && implied.contains(PEER));
        assert_eq!(section.validate_standalone().unwrap(), valid_layout());
    }

    /// An empty section reports `NoMonitors`, not a fabricated
    /// `DegeneratePair` — the count is checked before any pair is derived.
    #[test]
    fn implied_pair_reports_no_monitors_before_attempting_a_pair() {
        let empty = LayoutSection {
            revision: 1,
            origin: LOCAL,
            monitors: Vec::new(),
        };
        assert_eq!(empty.implied_pair().unwrap_err(), LayoutError::NoMonitors);
        assert_eq!(
            empty.validate_standalone().unwrap_err(),
            LayoutError::NoMonitors
        );
    }

    /// Every monitor belonging to the origin is a real degenerate pair —
    /// not an artifact of the derivation.
    #[test]
    fn implied_pair_reports_a_single_device_file_as_degenerate() {
        let section = LayoutSection {
            revision: 1,
            origin: LOCAL,
            monitors: vec![
                LayoutMonitorRow {
                    device: LOCAL,
                    id: "A".to_owned(),
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
                LayoutMonitorRow {
                    device: LOCAL,
                    id: "B".to_owned(),
                    x: 20,
                    y: 0,
                    width: 10,
                    height: 10,
                },
            ],
        };
        assert_eq!(
            section.implied_pair().unwrap_err(),
            LayoutError::DegeneratePair { device: LOCAL }
        );
    }

    /// The config file, as far as this crate's half of it goes — the
    /// shape `apps/crossover` will deserialize.
    #[derive(serde::Deserialize)]
    struct ConfigFile {
        schema_version: u32,
        layout: LayoutSection,
    }

    #[test]
    fn the_written_file_parses_back_with_the_parser_the_worker_reads_it_with() {
        let sandbox = Sandbox::new("write");
        let path = sandbox.path("config.toml");
        let layout = valid_layout();
        persist_layout(&path, &layout).unwrap();

        // Read back through `toml`, the crate `apps/crossover` loads the
        // config with — not through `toml_edit`, which would only prove
        // the writer agrees with itself.
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: ConfigFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.schema_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(parsed.layout.validate(&pair()).unwrap(), layout);
        assert!(stray_files(&sandbox.0).is_empty(), "a temp file survived");
    }

    /// The property the whole `toml_edit` choice exists for: a write
    /// nobody asked for must not delete what the user wrote.
    #[test]
    fn unrelated_sections_comments_and_ordering_survive_a_write() {
        let sandbox = Sandbox::new("preserve");
        let path = sandbox.path("config.toml");
        let original = concat!(
            "# Crossover configuration — hand written, and it shows.\n",
            "schema_version = 1\n",
            "\n",
            "[device]\n",
            "name = \"workstation-left\"   # the name the peer sees\n",
            "\n",
            "# Dial the machine under the desk.\n",
            "[network]\n",
            "connect = \"192.168.1.25:27677\"\n",
            "listen  = \"0.0.0.0:27677\"\n",
            "\n",
            "[seamless]\n",
            "side = \"right\"\n",
            "\n",
            "[cursor]\n",
            "mask = true\n",
        );
        std::fs::write(&path, original).unwrap();

        persist_layout(&path, &valid_layout()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        assert!(written.contains("# Crossover configuration — hand written"));
        assert!(written.contains("# the name the peer sees"));
        assert!(written.contains("# Dial the machine under the desk."));
        assert!(written.contains("listen  = \"0.0.0.0:27677\""), "{written}");
        assert!(written.contains("[cursor]"));
        assert!(written.contains("mask = true"));
        // `[device]` still precedes `[network]`: key and section order is
        // the user's, not the writer's.
        assert!(written.find("[device]").unwrap() < written.find("[network]").unwrap());

        // The upgrade itself.
        assert!(written.contains("schema_version = 2"), "{written}");
        assert!(!written.contains("side ="), "{written}");
        assert!(!written.contains("[seamless]"), "{written}");
        assert!(written.contains("[[layout.monitor]]"), "{written}");

        // And the whole file still parses as a config.
        let _: toml::Value = toml::from_str(&written).unwrap();
    }

    /// `[seamless]` loses `side` but not a key that is not ours to delete.
    #[test]
    fn a_seamless_section_with_other_keys_keeps_them() {
        let sandbox = Sandbox::new("seamless");
        let path = sandbox.path("config.toml");
        std::fs::write(&path, "[seamless]\nside = \"left\"\nsomething_else = 3\n").unwrap();
        persist_layout(&path, &valid_layout()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("side ="), "{written}");
        assert!(written.contains("[seamless]"), "{written}");
        assert!(written.contains("something_else = 3"), "{written}");
    }

    /// The section is replaced, not merged, so an edit that removes a
    /// monitor actually removes it.
    #[test]
    fn rewriting_replaces_the_section_rather_than_accumulating_rows() {
        let sandbox = Sandbox::new("replace");
        let path = sandbox.path("config.toml");
        persist_layout(&path, &valid_layout()).unwrap();

        let three = Layout::new(
            9,
            PEER,
            vec![
                monitor(LOCAL, "A", 0, 0, 100, 100),
                monitor(LOCAL, "B", 0, 100, 100, 100),
                monitor(PEER, "C", 100, 0, 100, 100),
            ],
            &pair(),
        )
        .unwrap();
        persist_layout(&path, &three).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written.matches("[[layout.monitor]]").count(),
            3,
            "{written}"
        );
        assert!(!written.contains("DISPLAY1"), "{written}");
        assert!(written.contains("revision = 9"), "{written}");

        // And back down to two: the third row is gone.
        persist_layout(&path, &valid_layout()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written.matches("[[layout.monitor]]").count(),
            2,
            "{written}"
        );
        assert!(stray_files(&sandbox.0).is_empty());
    }

    #[test]
    fn an_unparseable_file_is_refused_and_left_exactly_as_it_was() {
        let sandbox = Sandbox::new("corrupt");
        let path = sandbox.path("config.toml");
        let corrupt = "schema_version = 1\n[network\nconnect = broken\n";
        std::fs::write(&path, corrupt).unwrap();

        let error = persist_layout(&path, &valid_layout()).unwrap_err();
        assert!(
            matches!(error, PersistError::Unparseable { .. }),
            "{error:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        assert!(
            stray_files(&sandbox.0).is_empty(),
            "a refused write must leave nothing behind"
        );
    }

    #[test]
    fn an_absent_file_and_an_absent_directory_are_created() {
        let sandbox = Sandbox::new("create");
        let path = sandbox.path("nested").join("deeper").join("config.toml");
        persist_layout(&path, &valid_layout()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("schema_version = 2"), "{written}");
        assert!(written.contains("[layout]"), "{written}");
    }

    /// The temp file sits beside the target — same directory, so the
    /// rename is a rename and not a cross-volume copy — and is gone once
    /// the write lands.
    #[test]
    fn the_temporary_file_is_a_sibling_and_never_survives() {
        let path = Path::new("/some/where/config.toml");
        let temporary = temp_path(path);
        assert_eq!(temporary.parent(), path.parent());
        let name = temporary
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("config.toml."), "{name}");
        assert!(name.rsplit('.').next() == Some("tmp"), "{name}");
        assert!(name.contains(&std::process::id().to_string()), "{name}");

        let sandbox = Sandbox::new("atomic");
        let real = sandbox.path("config.toml");
        persist_layout(&real, &valid_layout()).unwrap();
        assert!(!temp_path(&real).exists());
        assert!(stray_files(&sandbox.0).is_empty());
    }

    #[test]
    fn a_revision_toml_cannot_represent_is_refused_rather_than_wrapped() {
        let sandbox = Sandbox::new("revision");
        let path = sandbox.path("config.toml");

        // The top of the signed range still writes.
        let representable = Layout::new(
            u64::try_from(i64::MAX).unwrap(),
            LOCAL,
            side_by_side(),
            &pair(),
        )
        .unwrap();
        persist_layout(&path, &representable).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(&format!("revision = {}", i64::MAX))
        );

        // One past it does not.
        let saturated = Layout::new(u64::MAX, LOCAL, side_by_side(), &pair()).unwrap();
        let error = persist_layout(&path, &saturated).unwrap_err();
        assert!(
            matches!(
                error,
                PersistError::RevisionUnrepresentable { revision: u64::MAX }
            ),
            "{error:?}"
        );
        // The refusal happens before anything is read or written.
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(&format!("revision = {}", i64::MAX))
        );
        assert!(stray_files(&sandbox.0).is_empty());
    }

    #[test]
    fn a_section_that_is_shaped_right_but_impossible_is_refused_by_rule() {
        // An unknown key anywhere in the section is a parse failure, as
        // everywhere else in this config file.
        assert!(
            toml::from_str::<LayoutSection>(
                "revision = 1\norigin = \"11111111-1111-1111-1111-111111111111\"\nmystery = 2\n"
            )
            .is_err()
        );

        // A section with no rows is `NoMonitors`, a rule with a
        // diagnostic — not a missing-field message.
        let empty: LayoutSection =
            toml::from_str(&format!("revision = 1\norigin = \"{LOCAL}\"\n")).unwrap();
        assert_eq!(
            empty.validate(&pair()).unwrap_err(),
            LayoutError::NoMonitors
        );

        // The residue of a re-pair: a device that is not this session's.
        let stranger = DeviceId::from_bytes([0x99; 16]);
        let section: LayoutSection = toml::from_str(&format!(
            "revision = 4\norigin = \"{LOCAL}\"\n\n\
             [[monitor]]\ndevice = \"{LOCAL}\"\nid = \"A\"\nx = 0\ny = 0\nwidth = 10\nheight = 10\n\n\
             [[monitor]]\ndevice = \"{stranger}\"\nid = \"B\"\nx = 20\ny = 0\nwidth = 10\nheight = 10\n"
        ))
        .unwrap();
        assert_eq!(
            section.validate(&pair()).unwrap_err(),
            LayoutError::UnexpectedDevice { device: stranger }
        );

        // And an oversized section is refused for its size, before any
        // row is turned into a monitor.
        let mut rows = format!("revision = 1\norigin = \"{LOCAL}\"\n");
        for index in 0..=MAX_LAYOUT_MONITORS {
            write!(
                rows,
                "\n[[monitor]]\ndevice = \"{LOCAL}\"\nid = \"\"\nx = {index}\ny = 0\nwidth = 1\nheight = 1\n"
            )
            .unwrap();
        }
        let oversized: LayoutSection = toml::from_str(&rows).unwrap();
        assert_eq!(
            oversized.validate(&pair()).unwrap_err(),
            LayoutError::TooManyMonitors {
                count: MAX_LAYOUT_MONITORS + 1
            }
        );
    }
}
