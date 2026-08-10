//! Startup configuration file for `crossover run` (Phase 6).
//!
//! For continuous daily use the run parameters — role, peer address, seamless
//! side, device name — should not have to be retyped every launch. They live
//! in a human-editable TOML file at `~/.crossover/config.toml`
//! ([`crate::paths`]), and `crossover run` reads them when the corresponding
//! CLI flag is absent.
//!
//! The schema is **sectioned and versioned** (ARCHITECTURE.md §8): a
//! `schema_version` guards evolution, and settings are grouped so new areas
//! (a `[service]` section, later) slot in without reshaping the file:
//!
//! ```toml
//! schema_version = 1
//!
//! [device]
//! name = "machine-b"
//!
//! [network]
//! listen = "0.0.0.0:27677"          # presence = accept inbound peers
//! connect = "192.168.1.151:27677"   # dial this peer
//!
//! [seamless]
//! side = "right"                    # "left" | "right"
//!
//! [cursor]
//! mask = false                      # default true; false = never hide
//! ```
//!
//! **CLI flags always win.** The file supplies defaults; a flag on the
//! command line overrides the file for that field. Nothing secret goes here —
//! identity and trust stay in the DPAPI-encrypted store.

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::paths::config_path;

/// The config schema version this build understands. A file may omit it
/// (assumed current) but must not name a newer one.
const SCHEMA_VERSION: u32 = 1;

/// Which seamless side a machine is, in the config file (`side = "left"`).
/// Kept separate from `crossover_core::LinkSide` so the wire/core type needs
/// no serde; [`EffectiveRun`] is mapped to `LinkSide` at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The left screen; its right edge crosses to the peer.
    Left,
    /// The right screen; its left edge crosses to the peer.
    Right,
}

/// The parsed `config.toml`. Every field is optional — an absent field just
/// means "no default from the file, use the CLI or the built-in default".
/// Unknown keys are rejected (in every section) so a typo fails loudly.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// Schema version; absent means the current version.
    pub schema_version: Option<u32>,
    /// `[device]`.
    #[serde(default)]
    pub device: DeviceConfig,
    /// `[network]`.
    #[serde(default)]
    pub network: NetworkConfig,
    /// `[seamless]`.
    #[serde(default)]
    pub seamless: SeamlessConfig,
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

/// `[seamless]` — edge-crossing layout.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeamlessConfig {
    /// Seamless side (as `--left` / `--right`).
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
    /// `--left`.
    pub left: bool,
    /// `--right`.
    pub right: bool,
    /// `--no-cursor-mask`.
    pub no_cursor_mask: bool,
}

/// The effective run parameters after merging CLI over file. Still
/// unvalidated — the caller checks a role is present, that `bind` implies
/// `listen`, and maps [`Side`] to `LinkSide`.
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
    /// Seamless side, if configured.
    pub side: Option<Side>,
    /// Whether cursor masking is disabled.
    pub no_cursor_mask: bool,
}

impl RunConfig {
    /// Reject a config written for a schema this build does not understand.
    ///
    /// # Errors
    ///
    /// If `schema_version` names anything other than the supported version.
    fn check_version(&self) -> anyhow::Result<()> {
        match self.schema_version {
            None | Some(SCHEMA_VERSION) => Ok(()),
            Some(other) => bail!(
                "config schema_version {other} is not supported by this build \
                 (understands {SCHEMA_VERSION}); update Crossover or the file"
            ),
        }
    }

    /// Merge command-line values over this file: a flag present on the
    /// command line wins; otherwise the file supplies the value.
    #[must_use]
    pub fn merge(self, cli: CliRun) -> EffectiveRun {
        // `--left` / `--right` conflict in clap, so at most one is set.
        let side = if cli.left {
            Some(Side::Left)
        } else if cli.right {
            Some(Side::Right)
        } else {
            self.seamless.side
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
        EffectiveRun {
            name: cli.name.or(self.device.name),
            listen,
            bind,
            connect: cli.connect.or(self.network.connect),
            side,
            // The flag forces masking off; else the file's `cursor.mask`.
            no_cursor_mask: cli.no_cursor_mask || self.cursor.mask == Some(false),
        }
    }
}

/// Load the config file, or an empty config if there is none.
///
/// # Errors
///
/// If the file exists but cannot be read or parsed, or names an unsupported
/// schema version — a broken config must fail loudly, not be silently
/// ignored, or the machine would run with surprising defaults.
pub fn load_run_config() -> anyhow::Result<RunConfig> {
    let Some(path) = config_path() else {
        return Ok(RunConfig::default());
    };
    let config: RunConfig = match std::fs::read_to_string(&path) {
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
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::{CliRun, RunConfig, Side};

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

    #[test]
    fn a_cli_flag_overrides_the_file() {
        let config: RunConfig =
            toml::from_str("[seamless]\nside = \"left\"\n[network]\nconnect = \"10.0.0.1:1\"")
                .unwrap();
        let effective = config.merge(CliRun {
            right: true, // overrides the file's `left`
            connect: Some("10.0.0.2:2".to_owned()),
            ..Default::default()
        });
        assert_eq!(effective.side, Some(Side::Right));
        assert_eq!(effective.connect.as_deref(), Some("10.0.0.2:2"));
    }

    #[test]
    fn the_file_supplies_values_absent_from_the_cli() {
        let config: RunConfig = toml::from_str(
            "[device]\nname = \"machine-a\"\n[network]\nlisten = \"0.0.0.0:27677\"\n\
             [seamless]\nside = \"left\"",
        )
        .unwrap();
        let effective = config.merge(CliRun::default());
        // `network.listen` present means "listen", carrying its address.
        assert!(effective.listen);
        assert_eq!(effective.bind.as_deref(), Some("0.0.0.0:27677"));
        assert_eq!(effective.side, Some(Side::Left));
        assert_eq!(effective.name.as_deref(), Some("machine-a"));
        assert!(!effective.no_cursor_mask);
    }

    #[test]
    fn cursor_mask_false_in_the_file_disables_masking() {
        let config: RunConfig = toml::from_str("[cursor]\nmask = false").unwrap();
        assert!(config.merge(CliRun::default()).no_cursor_mask);
        // Default (absent) keeps masking on.
        assert!(!RunConfig::default().merge(CliRun::default()).no_cursor_mask);
    }
}
