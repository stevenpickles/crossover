//! Startup configuration file for `crossover run` (Phase 6).
//!
//! For continuous daily use the run parameters — role, peer address, display
//! arrangement, device name — should not have to be retyped every launch.
//! They live in a human-editable TOML file at `~/.crossover/config.toml`
//! ([`crate::paths`]), and `crossover run` reads them when the corresponding
//! CLI flag is absent.
//!
//! The schema is **sectioned and versioned** (ARCHITECTURE.md §8): a
//! `schema_version` guards evolution, and settings are grouped so new areas
//! (a `[service]` section, later) slot in without reshaping the file. Schema
//! 2 ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)) replaces
//! the old `[seamless] side` with a drawn `[layout]`:
//!
//! ```toml
//! schema_version = 2
//!
//! [device]
//! name = "machine-b"
//!
//! [network]
//! listen = "0.0.0.0:27677"          # presence = accept inbound peers
//! connect = "192.168.1.151:27677"   # dial this peer
//!
//! [layout]
//! revision = 3
//! origin = "8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8"
//!
//! [[layout.monitor]]
//! device = "8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8"
//! id = '\\.\DISPLAY1'
//! x = 0
//! y = 0
//! width = 1920
//! height = 1080
//!
//! [cursor]
//! mask = false                      # default true; false = never hide
//! ```
//!
//! A schema 1 file — or a schema 2 file with a lingering `[seamless] side`
//! and no `[layout]` — still loads: it becomes an *implicit* layout that
//! reproduces the old left–right behavior exactly ([`LayoutSource::Implicit`]).
//! Nothing here ever writes `[layout]`; the upgrade to an explicit one
//! happens on the first save, once the editor (feature/152) calls
//! `crossover_topology::persist_layout`.
//!
//! **The version stamp must predict the semantics.** A file with `[layout]`
//! but `schema_version` absent or `1` is a config-shape contradiction — the
//! same class of mistake the old unknown-field refusal catches — and is
//! refused outright ([`RunConfig::check_layout_schema`]). This is
//! unreachable from any file this codebase's own writers produce, only
//! from a hand edit.
//!
//! **A `[layout]` that parses but fails *semantic* validation (overlap, a
//! bad device, an empty list, ...) does not kill the run.** It degrades to
//! no layout — seamless off, explicit control intact — with a loud warning,
//! never a fatal error ([`RunConfig::merge`]). The reason is ADR 0011: the
//! background service relaunches `crossover run` on every crash, so a fatal
//! config error here is an infinite relaunch loop that loses ALL sharing,
//! not just the drawn one. `crossover config` still reports the same
//! invalidity loudly, as the check it exists to be — it just does not
//! treat it as fatal either, for the same reason.
//!
//! **CLI flags always win — except an explicit `[layout]` beats them.**
//! Everywhere else in this codebase a command-line flag overrides the file;
//! here that is deliberately inverted when the file holds a drawn
//! arrangement, because the worker is often launched by the background
//! service with a command line fixed at install time (ADR 0011): a
//! `--right` baked into that command line must not flatten a drawn layout
//! back to a side on every launch. The flags still win over an *implicit*
//! layout, where there is nothing to lose. See [`RunConfig::merge`], which
//! returns every warning this produces as data ([`ConfigNotice`]) rather
//! than emitting it directly — `main.rs` renders each one once.
//! Nothing secret goes here — identity and trust stay in the DPAPI-encrypted
//! store.

use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

use crossover_core::LinkSide;
use crossover_topology::{Layout, LayoutError, LayoutSection};

use crate::paths::config_path;

/// Which seamless side a machine is, in the config file (`side = "left"`).
/// Kept separate from [`LinkSide`] so the wire/core type needs no serde;
/// [`RunConfig::merge`] maps a [`Side`] to a [`LinkSide`] as it builds the
/// [`LayoutSource`].
///
/// Deprecated (ADR 0018): `[seamless] side` and the `--left`/`--right`
/// flags that set it still work, producing an *implicit* layout, but a
/// drawn `[layout]` is the supported way to describe an arrangement now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The left screen; its right edge crosses to the peer.
    Left,
    /// The right screen; its left edge crosses to the peer.
    Right,
}

impl Side {
    /// The [`LinkSide`] this config-file value names.
    const fn to_link_side(self) -> LinkSide {
        match self {
            Self::Left => LinkSide::Left,
            Self::Right => LinkSide::Right,
        }
    }
}

/// The parsed `config.toml`. Every field is optional — an absent field just
/// means "no default from the file, use the CLI or the built-in default".
/// Unknown keys are rejected (in every section) so a typo fails loudly.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// Schema version; absent is accepted like any supported version
    /// ([`Self::check_version`]) — **except** when `[layout]` is present,
    /// where absent is treated as too old, not "assume the newest"
    /// ([`Self::check_layout_schema`]).
    pub schema_version: Option<u32>,
    /// `[device]`.
    #[serde(default)]
    pub device: DeviceConfig,
    /// `[network]`.
    #[serde(default)]
    pub network: NetworkConfig,
    /// `[seamless]`. Deprecated (ADR 0018): superseded by `[layout]`, kept
    /// only for the implicit-layout migration.
    #[serde(default)]
    pub seamless: SeamlessConfig,
    /// `[layout]` — a drawn arrangement (ADR 0018, schema 2). Still the
    /// shape straight off the page here: proving it is a *believable*
    /// layout, against the pair the file's own bytes imply, is
    /// [`Self::validated_layout`]'s job — the real session pair is not
    /// known until later still (see [`LayoutSource::Explicit`]).
    #[serde(default)]
    pub layout: Option<LayoutSection>,
    /// `[cursor]`.
    #[serde(default)]
    pub cursor: CursorConfig,
}

/// `[device]` — identity-related settings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// Device name (as `--name`), used only when the identity is generated.
    pub name: Option<String>,
}

/// `[network]` — the session role.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Address to accept inbound peers on (as `--listen`/`--bind`); its
    /// presence *is* "listen".
    pub listen: Option<String>,
    /// Peer address to dial (as `--connect`).
    pub connect: Option<String>,
}

/// `[seamless]` — the retired edge-crossing side (ADR 0018). Read for the
/// implicit-layout migration; never written by this build.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeamlessConfig {
    /// Seamless side (as `--left` / `--right`). Deprecated: superseded by
    /// `[layout]`.
    pub side: Option<Side>,
}

/// `[cursor]` — cursor-masking behavior.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorConfig {
    /// Whether to hide the local cursor while driving the peer. Default
    /// `true`; `false` is the file form of `--no-cursor-mask`.
    pub mask: Option<bool>,
}

/// The run parameters as they arrived on the command line, before merging
/// with the file. Boolean flags are `false` when absent (clap cannot tell
/// "unset" from "set false"), so the merge treats a `true` flag as an
/// override and a `false` flag as "defer to the file".
// Mirrors the CLI flags one-to-one, so several bools are inherent.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct CliRun {
    /// `--name` (global).
    pub name: Option<String>,
    /// `--listen`.
    pub listen: bool,
    /// `--bind`.
    pub bind: Option<String>,
    /// `--connect`.
    pub connect: Option<String>,
    /// `--left`. Deprecated (ADR 0018): see [`Side`].
    pub left: bool,
    /// `--right`. Deprecated (ADR 0018): see [`Side`].
    pub right: bool,
    /// `--no-cursor-mask`.
    pub no_cursor_mask: bool,
}

/// Where this run's display arrangement came from (ADR 0018).
///
/// Two states, not a bare `Option<Layout>` beside a bare `Option<Side>`:
/// carrying both as one type is what lets [`EffectiveRun`] and everything
/// downstream (`commands::build_seamless`) take one value instead of two
/// optionals whose combinations would otherwise have to be reasoned about
/// separately. `None` (i.e. `Option<LayoutSource>` holding nothing) is the
/// third state — no side and no layout — and means what it always has:
/// seamless transfer off, explicit control intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutSource {
    /// The side named by `--left`/`--right`, or a lingering `[seamless]
    /// side` with no `[layout]` in the file. Revision 0 in spirit, never
    /// synced, never written back — the upgrade to an explicit `[layout]`
    /// happens on the first write, which nothing on this branch performs
    /// (feature/152 wires `crossover_topology::persist_layout` in).
    ///
    /// Drives `commands::build_seamless` exactly as a bare side always
    /// has: zero behavior change from every release before this one.
    Implicit(LinkSide),
    /// A `[layout]` section that parsed and validated — structurally,
    /// against the pair its own bytes imply — at load time (see
    /// [`RunConfig::merge`]). Carries its own revision and origin
    /// (`Layout::revision`, `Layout::origin`).
    ///
    /// Cannot drive the side-model `Topology` yet: the crossing engine
    /// that consumes a drawn arrangement (the `CrossingMap` work) is a
    /// later branch. Until it lands, `commands::build_seamless` leaves
    /// seamless transfer off for an `Explicit` source, with a startup log
    /// naming why — explicit control still works. The value is carried
    /// through regardless, so the plumbing is shaped for the branch that
    /// consumes it.
    Explicit(Layout),
}

impl LayoutSource {
    /// A compact one-line summary for the startup log: the discriminant,
    /// plus a layout's revision — not the full monitor list. A soak log
    /// line should stay one line; `crossover config` and `crossover
    /// layout` are where the whole arrangement belongs.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Implicit(side) => format!("implicit({side:?})"),
            Self::Explicit(layout) => format!("explicit(revision={})", layout.revision()),
        }
    }
}

/// The effective run parameters after merging CLI over file. Still
/// unvalidated — the caller checks a role is present and that `bind`
/// implies `listen`.
#[derive(Debug, PartialEq, Eq)]
pub struct EffectiveRun {
    /// Device name, or `None` to fall back to the hostname.
    pub name: Option<String>,
    /// Whether to accept inbound sessions.
    pub listen: bool,
    /// Bind address, if any.
    pub bind: Option<String>,
    /// Peer address to dial, if any.
    pub connect: Option<String>,
    /// The display arrangement this run starts with, if any (ADR 0018).
    pub layout_source: Option<LayoutSource>,
    /// Whether cursor masking is disabled.
    pub no_cursor_mask: bool,
}

/// A warning [`RunConfig::merge`] produced while deciding [`LayoutSource`]
/// — data, not an action. `main.rs` renders each one exactly once (the log,
/// and stderr for an interactive user), rather than every decision point in
/// `merge` doing its own `tracing::warn!`/`eprintln!` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigNotice {
    /// `--left`/`--right` was used and drives an implicit layout. Fires
    /// only when the flag actually takes effect — an explicit `[layout]`
    /// overriding it is [`Self::ExplicitLayoutWins`] instead, which
    /// already says the flag was set and ignored.
    DeprecatedFlag {
        /// `"--left"` or `"--right"`.
        flag: &'static str,
    },
    /// A file-only `[seamless] side`, with no flag and no `[layout]` in
    /// the way. ADR 0018 deprecates the whole side model, not only the
    /// flags that set it, so a config still carrying a bare `side` gets
    /// the same nudge toward the editor — these are exactly the users the
    /// migration needs to reach.
    DeprecatedSideKey,
    /// The config holds an explicit `[layout]`, so it wins over
    /// `overridden` (a flag or the file's own `side` key) — ADR 0018's
    /// deliberate CLI-wins deviation.
    ExplicitLayoutWins {
        /// `"--left"`, `"--right"`, or `"[seamless] side"`.
        overridden: &'static str,
    },
    /// A `[layout]` section was present but failed *semantic* validation
    /// (overlap, a bad device, an empty list, ...). Never fatal for
    /// `crossover run` (see the module docs): degrades to no layout —
    /// seamless off, explicit control intact.
    InvalidLayout {
        /// Why it was refused.
        error: LayoutError,
    },
}

impl RunConfig {
    /// Reject a config written for a schema this build does not understand.
    ///
    /// # Errors
    ///
    /// If `schema_version` names anything this build cannot read —
    /// delegates to [`crossover_topology::config_schema_supported`], so the
    /// writer and every reader agree on the same range from one definition.
    fn check_version(&self) -> anyhow::Result<()> {
        match self.schema_version {
            None => Ok(()),
            Some(version) if crossover_topology::config_schema_supported(version) => Ok(()),
            Some(other) => bail!(
                "config schema_version {other} is not supported by this build (understands \
                 {} through {}); update Crossover or the file",
                crossover_topology::CONFIG_SCHEMA_MIN_SUPPORTED,
                crossover_topology::CONFIG_SCHEMA_VERSION
            ),
        }
    }

    /// Reject a config whose version stamp cannot predict its own
    /// semantics: a `[layout]` section with `schema_version` absent or
    /// less than [`crossover_topology::CONFIG_SCHEMA_VERSION`].
    ///
    /// This is a config-*shape* contradiction — the same class as the
    /// standing unknown-field refusal — not a semantic problem with the
    /// arrangement itself, so unlike an invalid `[layout]`
    /// ([`Self::validated_layout`]) it **is** fatal: nothing this
    /// codebase's own writers produce can trigger it (`persist_layout`
    /// always stamps `CONFIG_SCHEMA_VERSION`), so it is reachable only
    /// from a hand edit that also needs a human to look at it.
    ///
    /// # Errors
    ///
    /// If `[layout]` is present and `schema_version` does not name a
    /// schema that has `[layout]`.
    fn check_layout_schema(&self) -> anyhow::Result<()> {
        if self.layout.is_none() {
            return Ok(());
        }
        // `>=` rather than `==`: the rule is "the stamp must predict the
        // semantics in force", not "must equal today's ceiling exactly" —
        // a version above the ceiling is already refused by `check_version`.
        if self
            .schema_version
            .is_some_and(|version| version >= crossover_topology::CONFIG_SCHEMA_VERSION)
        {
            return Ok(());
        }
        bail!(
            "the config has a `[layout]` section, but schema_version is {}; `[layout]` \
             requires schema_version = {} (the version stamp must name the schema whose \
             semantics apply)",
            self.schema_version
                .map_or_else(|| "absent".to_owned(), |version| version.to_string()),
            crossover_topology::CONFIG_SCHEMA_VERSION
        );
    }

    /// Validate this file's `[layout]` section, if it has one, against the
    /// pair its own bytes imply ([`LayoutSection::implied_pair`] — a
    /// config loader does not yet know this machine's identity, let alone
    /// the real session pair).
    ///
    /// # Errors
    ///
    /// The section's [`LayoutError`] when it fails to validate. Note this
    /// is **not** wrapped in `anyhow::Result`: unlike
    /// [`Self::check_layout_schema`], a semantically invalid `[layout]`
    /// is not fatal — [`Self::merge`] degrades it to no layout instead
    /// (see the module docs for why).
    fn validated_layout(&self) -> Result<Option<Layout>, LayoutError> {
        let Some(section) = &self.layout else {
            return Ok(None);
        };
        section.validate_standalone().map(Some)
    }

    /// Merge command-line values over this file: a flag present on the
    /// command line wins; otherwise the file supplies the value — **except
    /// an explicit `[layout]` in the file wins over `--left`/`--right`**,
    /// ADR 0018's deliberate deviation from CLI-wins (see the module docs).
    ///
    /// `layout` is the file's `[layout]` section, already validated —
    /// [`load_run_config`] is the one call site that produces it (see
    /// [`LoadedConfig`]), so validation happens exactly once, at one error
    /// surface. This never fails: an `Err` degrades to no layout rather
    /// than aborting the run, and every warning this decides — a
    /// deprecated flag, an override, a degraded layout — comes back as a
    /// [`ConfigNotice`] rather than being rendered here, so the caller
    /// (`main.rs`) renders each one exactly once.
    #[must_use]
    pub fn merge(
        self,
        layout: Result<Option<Layout>, LayoutError>,
        cli: CliRun,
    ) -> (EffectiveRun, Vec<ConfigNotice>) {
        let mut notices = Vec::new();

        let explicit = match layout {
            Ok(explicit) => explicit,
            Err(error) => {
                notices.push(ConfigNotice::InvalidLayout { error });
                None
            }
        };

        let layout_source = if let Some(layout) = explicit {
            // ADR 0018's precedence deviation: an explicit drawn layout
            // beats both the flags and a lingering `side`, so both are
            // merely warned about here rather than consulted.
            if cli.left || cli.right {
                notices.push(ConfigNotice::ExplicitLayoutWins {
                    overridden: if cli.left { "--left" } else { "--right" },
                });
            }
            if self.seamless.side.is_some() {
                notices.push(ConfigNotice::ExplicitLayoutWins {
                    overridden: "[seamless] side",
                });
            }
            Some(LayoutSource::Explicit(layout))
        } else if cli.left || cli.right {
            // Flags still win over an implicit layout — there is nothing
            // to lose (ADR 0018) — and are always deprecated when used.
            let (flag, side) = if cli.left {
                ("--left", LinkSide::Left)
            } else {
                ("--right", LinkSide::Right)
            };
            notices.push(ConfigNotice::DeprecatedFlag { flag });
            Some(LayoutSource::Implicit(side))
        } else if let Some(side) = self.seamless.side {
            // A file-only `side`, no flags in the way: exactly who the
            // migration needs to reach.
            notices.push(ConfigNotice::DeprecatedSideKey);
            Some(LayoutSource::Implicit(side.to_link_side()))
        } else {
            None
        };

        // A listen flag/bind on the command line takes over the role; else
        // the file's `network.listen` address (present = listen) applies.
        let (listen, bind) = if cli.listen || cli.bind.is_some() {
            (cli.listen, cli.bind)
        } else {
            match self.network.listen {
                Some(address) => (true, Some(address)),
                None => (false, None),
            }
        };
        let effective = EffectiveRun {
            name: cli.name.or(self.device.name),
            listen,
            bind,
            connect: cli.connect.or(self.network.connect),
            layout_source,
            // The flag forces masking off; else the file's `cursor.mask`.
            no_cursor_mask: cli.no_cursor_mask || self.cursor.mask == Some(false),
        };
        (effective, notices)
    }
}

/// A parsed config file together with its `[layout]` section's validated
/// (or degraded) fate — the output of the one place validation happens.
///
/// Kept separate from a validated field on [`RunConfig`] itself so that
/// type stays a plain `toml::from_str` target, while still validating
/// exactly once: [`Self::merge`] hands [`RunConfig::merge`] the result
/// [`load_run_config`] already computed, rather than re-deriving it, so
/// there is one validation site and one error surface — with the
/// file-path context `load_run_config` attaches — instead of two.
pub struct LoadedConfig {
    /// The parsed file (or the built-in default, if there is none).
    pub config: RunConfig,
    /// The `[layout]` section's fate: absent (`Ok(None)`), valid
    /// (`Ok(Some)`), or present but semantically invalid (`Err`) — see
    /// [`RunConfig::validated_layout`].
    pub layout: Result<Option<Layout>, LayoutError>,
}

impl LoadedConfig {
    /// Merge CLI flags over this loaded config. See [`RunConfig::merge`].
    #[must_use]
    pub fn merge(self, cli: CliRun) -> (EffectiveRun, Vec<ConfigNotice>) {
        self.config.merge(self.layout, cli)
    }
}

/// Load the config file, or an empty config if there is none.
///
/// # Errors
///
/// If the file exists but cannot be read or parsed, names an unsupported
/// schema version, or has a `[layout]` whose `schema_version` cannot
/// predict it ([`RunConfig::check_layout_schema`]) — every one of these is
/// a broken or contradictory *file*, which must fail loudly rather than run
/// with surprising defaults. A `[layout]` that is well-formed but
/// semantically invalid is **not** one of these: see
/// [`LoadedConfig::layout`] and the module docs.
pub fn load_run_config() -> anyhow::Result<LoadedConfig> {
    load_run_config_at(config_path().as_deref())
}

/// [`load_run_config`], parameterized by the file's path rather than
/// resolving it from the environment.
///
/// This is the shape that makes the function testable against a sandboxed
/// file instead of the real `~/.crossover/config.toml` —
/// `commands::apply_config_changes`'s re-read (ADR 0018) calls this
/// directly with the path it was started with, and its tests do the same
/// with a temporary one.
///
/// # Errors
///
/// Same as [`load_run_config`].
pub fn load_run_config_at(path: Option<&Path>) -> anyhow::Result<LoadedConfig> {
    let Some(path) = path else {
        return Ok(LoadedConfig {
            config: RunConfig::default(),
            layout: Ok(None),
        });
    };
    let config: RunConfig = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RunConfig::default(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading config file {}", path.display()));
        }
    };
    config
        .check_version()
        .with_context(|| format!("in config file {}", path.display()))?;
    config
        .check_layout_schema()
        .with_context(|| format!("in config file {}", path.display()))?;
    let layout = config.validated_layout();
    Ok(LoadedConfig { config, layout })
}

/// The config file's modification time and length, if it both exists and
/// has metadata this platform can report — for
/// `commands::apply_config_changes`'s re-read poll (ADR 0018), parameterized
/// by path the same way [`load_run_config_at`] is (`commands::run` calls
/// this with [`config_path`]'s result, captured once at the initial load so
/// the very first poll tick can already tell an edit landed while the run
/// was starting up; tests use a sandboxed path).
///
/// The length rides along with the modification time rather than the time
/// alone: some filesystems (network shares in particular) report mtime at a
/// coarser granularity than the poll interval, so two edits inside one
/// tick's worth of time can carry the same timestamp. A length change
/// still tells them apart; `apply_config_changes`'s own "is this reading
/// recent" fallback covers the residual case of two same-length edits
/// landing in the same coarse tick.
///
/// `None` covers both "no config file" and "could not read its metadata":
/// either way there is nothing to compare the next reading against, so the
/// first `Some` after a run of `None`s always counts as a change and
/// triggers a re-read, which is the safe direction to be wrong in.
#[must_use]
pub fn config_signature_at(path: Option<&Path>) -> Option<(std::time::SystemTime, u64)> {
    let metadata = std::fs::metadata(path?).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

#[cfg(test)]
mod tests {
    use crossover_core::LinkSide;
    use crossover_topology::LayoutError;

    use super::{CliRun, ConfigNotice, EffectiveRun, LayoutSource, RunConfig, Side};

    const LOCAL: &str = "11111111-1111-1111-1111-111111111111";
    const PEER: &str = "22222222-2222-2222-2222-222222222222";
    const STRANGER: &str = "33333333-3333-3333-3333-333333333333";

    /// A `[layout]` section's body, naming `LOCAL` and `PEER` — parses
    /// directly as a bare [`crossover_topology::LayoutSection`], or under a
    /// `[layout]` header as a whole config file (see [`layout_toml`]).
    fn layout_section_toml() -> String {
        format!(
            "revision = 3\norigin = \"{LOCAL}\"\n\n\
             [[monitor]]\ndevice = \"{LOCAL}\"\nid = \"A\"\nx = 0\ny = 0\nwidth = 100\nheight = 100\n\n\
             [[monitor]]\ndevice = \"{PEER}\"\nid = \"B\"\nx = 100\ny = 0\nwidth = 100\nheight = 100\n"
        )
    }

    /// A minimal, valid config-file `[layout]` block naming `LOCAL` and
    /// `PEER`.
    fn layout_toml() -> String {
        format!(
            "[layout]\n{}",
            layout_section_toml().replace("[[monitor]]", "[[layout.monitor]]")
        )
    }

    /// [`RunConfig::validated_layout`] then [`RunConfig::merge`] — the
    /// shape every real caller goes through ([`super::LoadedConfig::merge`]),
    /// so the tests below exercise that path rather than reaching past it.
    fn merge(config: RunConfig, cli: CliRun) -> (EffectiveRun, Vec<ConfigNotice>) {
        let layout = config.validated_layout();
        config.merge(layout, cli)
    }

    #[test]
    fn parses_a_sectioned_config() {
        let toml = r#"
            schema_version = 1
            [device]
            name = "machine-b"
            [network]
            connect = "192.168.1.151:27677"
            [seamless]
            side = "right"
            [cursor]
            mask = false
        "#;
        let config: RunConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.device.name.as_deref(), Some("machine-b"));
        assert_eq!(
            config.network.connect.as_deref(),
            Some("192.168.1.151:27677")
        );
        assert_eq!(config.network.listen, None);
        assert_eq!(config.seamless.side, Some(Side::Right));
        assert_eq!(config.cursor.mask, Some(false));
        assert!(config.layout.is_none());
        config.check_version().unwrap();
    }

    #[test]
    fn an_unknown_key_in_any_section_is_rejected() {
        // A typo in a section fails loudly, not silently.
        assert!(toml::from_str::<RunConfig>("[network]\nconect = \"x\"").is_err());
        // A stray top-level key too.
        assert!(toml::from_str::<RunConfig>("nonsense = 1").is_err());
    }

    #[test]
    fn a_future_schema_version_is_rejected() {
        let config: RunConfig = toml::from_str("schema_version = 999").unwrap();
        assert!(config.check_version().is_err());
        // Absent version is accepted (assumed current).
        RunConfig::default().check_version().unwrap();
    }

    /// Both schema versions ADR 0018 says still load: 1 (pre-`[layout]`)
    /// and 2 (`[layout]`).
    #[test]
    fn schema_versions_one_and_two_are_both_accepted_and_three_is_not() {
        for version in [1, 2] {
            let config: RunConfig =
                toml::from_str(&format!("schema_version = {version}\n")).unwrap();
            config.check_version().unwrap();
        }
        let config: RunConfig = toml::from_str("schema_version = 3\n").unwrap();
        let error = config.check_version().unwrap_err();
        assert!(error.to_string().contains('3'), "{error}");
    }

    /// The version/shape cross-check: `[layout]` requires an explicit
    /// `schema_version = 2` — absent or `1` is a contradiction, refused
    /// outright, unlike an invalid *arrangement* which only degrades.
    #[test]
    fn a_layout_section_requires_schema_version_two() {
        // Omitted schema_version, with `[layout]` present: refused.
        let config: RunConfig = toml::from_str(&layout_toml()).unwrap();
        assert!(config.schema_version.is_none());
        let error = config.check_layout_schema().unwrap_err();
        assert!(error.to_string().contains("schema_version"), "{error}");

        // Explicit schema_version = 1, with `[layout]` present: same refusal.
        let toml = format!("schema_version = 1\n{}", layout_toml());
        let config: RunConfig = toml::from_str(&toml).unwrap();
        assert!(config.check_layout_schema().is_err());

        // schema_version = 2 with `[layout]`: accepted.
        let toml = format!("schema_version = 2\n{}", layout_toml());
        let config: RunConfig = toml::from_str(&toml).unwrap();
        config.check_layout_schema().unwrap();

        // No `[layout]` at all: the check does not apply, whatever the
        // version (or its absence).
        RunConfig::default().check_layout_schema().unwrap();
        let config: RunConfig = toml::from_str("schema_version = 1\n").unwrap();
        config.check_layout_schema().unwrap();
    }

    #[test]
    fn a_cli_flag_overrides_the_file() {
        let config: RunConfig =
            toml::from_str("[seamless]\nside = \"left\"\n[network]\nconnect = \"10.0.0.1:1\"")
                .unwrap();
        let (effective, notices) = merge(
            config,
            CliRun {
                right: true, // overrides the file's `left`
                connect: Some("10.0.0.2:2".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            effective.layout_source,
            Some(LayoutSource::Implicit(LinkSide::Right))
        );
        assert_eq!(effective.connect.as_deref(), Some("10.0.0.2:2"));
        assert_eq!(
            notices,
            vec![ConfigNotice::DeprecatedFlag { flag: "--right" }]
        );
    }

    #[test]
    fn the_file_supplies_values_absent_from_the_cli() {
        let config: RunConfig = toml::from_str(
            "[device]\nname = \"machine-a\"\n[network]\nlisten = \"0.0.0.0:27677\"\n\
             [seamless]\nside = \"left\"",
        )
        .unwrap();
        let (effective, _notices) = merge(config, CliRun::default());
        // `network.listen` present means "listen", carrying its address.
        assert!(effective.listen);
        assert_eq!(effective.bind.as_deref(), Some("0.0.0.0:27677"));
        assert_eq!(
            effective.layout_source,
            Some(LayoutSource::Implicit(LinkSide::Left))
        );
        assert_eq!(effective.name.as_deref(), Some("machine-a"));
        assert!(!effective.no_cursor_mask);
    }

    /// A file-only `[seamless] side`, no flags: ADR 0018 deprecates the
    /// whole side model, not only the flags, so these users get the
    /// deprecation nudge too.
    #[test]
    fn a_file_only_side_with_no_flags_gets_the_deprecation_notice_too() {
        let config: RunConfig = toml::from_str("[seamless]\nside = \"right\"\n").unwrap();
        let (effective, notices) = merge(config, CliRun::default());
        assert_eq!(
            effective.layout_source,
            Some(LayoutSource::Implicit(LinkSide::Right))
        );
        assert_eq!(notices, vec![ConfigNotice::DeprecatedSideKey]);
    }

    #[test]
    fn cursor_mask_false_in_the_file_disables_masking() {
        let config: RunConfig = toml::from_str("[cursor]\nmask = false").unwrap();
        assert!(merge(config, CliRun::default()).0.no_cursor_mask);
        // Default (absent) keeps masking on.
        assert!(
            !merge(RunConfig::default(), CliRun::default())
                .0
                .no_cursor_mask
        );
    }

    /// No `[seamless]`, no `[layout]`, no flags: the pre-ADR-0018 "no side"
    /// run, unchanged.
    #[test]
    fn no_layout_and_no_flags_means_no_layout_source() {
        let (effective, notices) = merge(RunConfig::default(), CliRun::default());
        assert_eq!(effective.layout_source, None);
        assert!(notices.is_empty(), "{notices:?}");
    }

    /// `--left`/`--right` alone: an implicit layout, and a deprecation
    /// notice.
    #[test]
    fn flags_alone_produce_an_implicit_layout() {
        let (effective, notices) = merge(
            RunConfig::default(),
            CliRun {
                left: true,
                ..Default::default()
            },
        );
        assert_eq!(
            effective.layout_source,
            Some(LayoutSource::Implicit(LinkSide::Left))
        );
        assert_eq!(
            notices,
            vec![ConfigNotice::DeprecatedFlag { flag: "--left" }]
        );
    }

    /// A `[layout]` section parses into a validated [`Layout`], reachable
    /// through `merge` with no flags and no `[seamless]` in the way, and
    /// with no notices.
    #[test]
    fn an_explicit_layout_parses_and_validates() {
        let config: RunConfig = toml::from_str(&layout_toml()).unwrap();
        let (effective, notices) = merge(config, CliRun::default());
        let Some(LayoutSource::Explicit(layout)) = effective.layout_source else {
            panic!("expected an explicit layout");
        };
        assert_eq!(layout.revision(), 3);
        assert_eq!(layout.monitors().len(), 2);
        assert!(notices.is_empty(), "{notices:?}");
    }

    /// Both `[seamless] side` and `[layout]` in one file: explicit wins,
    /// `side` is ignored with a notice (ADR 0018's within-file precedence).
    #[test]
    fn explicit_layout_wins_over_a_lingering_side_key_in_the_same_file() {
        let toml = format!("[seamless]\nside = \"left\"\n\n{}", layout_toml());
        let config: RunConfig = toml::from_str(&toml).unwrap();
        let (effective, notices) = merge(config, CliRun::default());
        assert!(matches!(
            effective.layout_source,
            Some(LayoutSource::Explicit(_))
        ));
        assert_eq!(
            notices,
            vec![ConfigNotice::ExplicitLayoutWins {
                overridden: "[seamless] side"
            }]
        );
    }

    /// The flags-vs-explicit-layout precedence deviation from CLI-wins
    /// (ADR 0018): the flag is ignored, not honored, with a notice naming
    /// both facts.
    #[test]
    fn explicit_layout_wins_over_the_flags() {
        let config: RunConfig = toml::from_str(&layout_toml()).unwrap();
        let (effective, notices) = merge(
            config,
            CliRun {
                right: true,
                ..Default::default()
            },
        );
        assert!(matches!(
            effective.layout_source,
            Some(LayoutSource::Explicit(_))
        ));
        assert_eq!(
            notices,
            vec![ConfigNotice::ExplicitLayoutWins {
                overridden: "--right"
            }]
        );
    }

    /// An invalid `[layout]` degrades this run to no layout, with a
    /// notice — never fatal (the module docs explain why: the background
    /// service relaunches `crossover run` on crash, ADR 0011).
    #[test]
    fn an_invalid_layout_degrades_to_no_layout_with_a_notice() {
        // A single monitor: `implied_pair` reports a degenerate pair — a
        // shape the file alone can prove wrong.
        let toml = format!(
            "[layout]\nrevision = 1\norigin = \"{LOCAL}\"\n\n\
             [[layout.monitor]]\ndevice = \"{LOCAL}\"\nid = \"A\"\nx = 0\ny = 0\nwidth = 100\nheight = 100\n"
        );
        let config: RunConfig = toml::from_str(&toml).unwrap();
        let (effective, notices) = merge(config, CliRun::default());
        assert_eq!(effective.layout_source, None);
        assert!(
            matches!(notices.as_slice(), [ConfigNotice::InvalidLayout { .. }]),
            "{notices:?}"
        );

        // Overlapping monitors: a different rule, the same degrade.
        let overlapping = format!(
            "[layout]\nrevision = 1\norigin = \"{LOCAL}\"\n\n\
             [[layout.monitor]]\ndevice = \"{LOCAL}\"\nid = \"A\"\nx = 0\ny = 0\nwidth = 100\nheight = 100\n\n\
             [[layout.monitor]]\ndevice = \"{PEER}\"\nid = \"B\"\nx = 50\ny = 0\nwidth = 100\nheight = 100\n"
        );
        let config: RunConfig = toml::from_str(&overlapping).unwrap();
        let (effective, notices) = merge(config, CliRun::default());
        assert_eq!(effective.layout_source, None);
        assert!(
            matches!(notices.as_slice(), [ConfigNotice::InvalidLayout { .. }]),
            "{notices:?}"
        );
    }

    /// A third device in the file also degrades: "a layout names exactly
    /// two machines" is provable from the file's own bytes, with no
    /// session pair needed (`LayoutSection::implied_pair`).
    #[test]
    fn a_third_device_in_the_file_degrades_without_knowing_the_real_pair() {
        let toml = format!(
            "[layout]\nrevision = 1\norigin = \"{LOCAL}\"\n\n\
             [[layout.monitor]]\ndevice = \"{LOCAL}\"\nid = \"A\"\nx = 0\ny = 0\nwidth = 100\nheight = 100\n\n\
             [[layout.monitor]]\ndevice = \"{PEER}\"\nid = \"B\"\nx = 100\ny = 0\nwidth = 100\nheight = 100\n\n\
             [[layout.monitor]]\ndevice = \"{STRANGER}\"\nid = \"C\"\nx = 200\ny = 0\nwidth = 100\nheight = 100\n"
        );
        let config: RunConfig = toml::from_str(&toml).unwrap();
        let (effective, notices) = merge(config, CliRun::default());
        assert_eq!(effective.layout_source, None);
        assert!(matches!(
            notices.as_slice(),
            [ConfigNotice::InvalidLayout {
                error: LayoutError::UnexpectedDevice { .. }
            }]
        ));
    }

    #[test]
    fn schema_two_with_only_side_still_loads_as_implicit() {
        // A schema-2 file that has not been through the editor yet: no
        // `[layout]`, only the retiring `side` key.
        let config: RunConfig =
            toml::from_str("schema_version = 2\n[seamless]\nside = \"right\"\n").unwrap();
        let (effective, notices) = merge(config, CliRun::default());
        assert_eq!(
            effective.layout_source,
            Some(LayoutSource::Implicit(LinkSide::Right))
        );
        assert_eq!(notices, vec![ConfigNotice::DeprecatedSideKey]);
    }
}
