//! Startup configuration file for `crossover run` (Phase 6).
//!
//! For continuous daily use the run parameters — role, peer address, bind,
//! seamless side, device name — should not have to be retyped every launch.
//! They live in a human-editable TOML file at
//! `%LOCALAPPDATA%\Crossover\config.toml` (next to the `secure` store), and
//! `crossover run` reads them when the corresponding CLI flag is absent.
//!
//! **CLI flags always win.** The file supplies defaults; a flag on the
//! command line overrides the file for that field. Absent from both, the
//! usual defaults and validation apply (a role is still required). Nothing
//! secret goes here — identity and trust stay in the DPAPI-encrypted store.

use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

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
/// Unknown keys are rejected so a typo fails loudly instead of silently
/// doing nothing.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// Device name (as `--name`), used only when the identity is generated.
    pub name: Option<String>,
    /// Accept inbound sessions (as `--listen`).
    pub listen: Option<bool>,
    /// Bind address for listening (as `--bind`).
    pub bind: Option<String>,
    /// Peer address to dial (as `--connect`).
    pub connect: Option<String>,
    /// Seamless side (as `--left` / `--right`).
    pub side: Option<Side>,
    /// Disable cursor masking (as `--no-cursor-mask`).
    pub no_cursor_mask: Option<bool>,
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
            self.side
        };
        EffectiveRun {
            name: cli.name.or(self.name),
            listen: cli.listen || self.listen.unwrap_or(false),
            bind: cli.bind.or(self.bind),
            connect: cli.connect.or(self.connect),
            side,
            no_cursor_mask: cli.no_cursor_mask || self.no_cursor_mask.unwrap_or(false),
        }
    }
}

/// The config file path, `%LOCALAPPDATA%\Crossover\config.toml`, or `None`
/// if the per-user location cannot be determined (e.g. `%LOCALAPPDATA%`
/// unset, or off Windows) — in which case there is simply no config file.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local_app_data)
            .join("Crossover")
            .join("config.toml"),
    )
}

/// Load the config file, or an empty config if there is none.
///
/// # Errors
///
/// If the file exists but cannot be read or parsed — a broken config must
/// fail loudly, not be silently ignored, or the machine would run with
/// surprising defaults.
pub fn load_run_config() -> anyhow::Result<RunConfig> {
    let Some(path) = config_path() else {
        return Ok(RunConfig::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RunConfig::default()),
        Err(error) => Err(error).with_context(|| format!("reading config file {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::{CliRun, RunConfig, Side};

    #[test]
    fn parses_a_full_config() {
        let toml = r#"
            name = "machine-b"
            connect = "192.168.1.151:27677"
            side = "right"
            no_cursor_mask = true
        "#;
        let config: RunConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.name.as_deref(), Some("machine-b"));
        assert_eq!(config.connect.as_deref(), Some("192.168.1.151:27677"));
        assert_eq!(config.side, Some(Side::Right));
        assert_eq!(config.no_cursor_mask, Some(true));
        assert_eq!(config.listen, None);
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        // A typo (`conect`) must fail loudly, not be silently ignored.
        let err = toml::from_str::<RunConfig>("conect = \"x\"").unwrap_err();
        assert!(err.to_string().contains("conect") || err.to_string().contains("unknown"));
    }

    #[test]
    fn side_only_accepts_left_or_right() {
        assert!(toml::from_str::<RunConfig>("side = \"left\"").is_ok());
        assert!(toml::from_str::<RunConfig>("side = \"middle\"").is_err());
    }

    #[test]
    fn a_cli_flag_overrides_the_file() {
        let config = RunConfig {
            side: Some(Side::Left),
            connect: Some("10.0.0.1:1".to_owned()),
            ..Default::default()
        };
        let cli = CliRun {
            right: true, // overrides the file's `left`
            connect: Some("10.0.0.2:2".to_owned()),
            ..Default::default()
        };
        let effective = config.merge(cli);
        assert_eq!(effective.side, Some(Side::Right));
        assert_eq!(effective.connect.as_deref(), Some("10.0.0.2:2"));
    }

    #[test]
    fn the_file_supplies_values_absent_from_the_cli() {
        let config = RunConfig {
            listen: Some(true),
            side: Some(Side::Left),
            name: Some("machine-a".to_owned()),
            ..Default::default()
        };
        let effective = config.merge(CliRun::default());
        assert!(effective.listen);
        assert_eq!(effective.side, Some(Side::Left));
        assert_eq!(effective.name.as_deref(), Some("machine-a"));
        assert!(!effective.no_cursor_mask);
    }

    #[test]
    fn a_boolean_flag_adds_to_the_file() {
        // A `false` flag defers to the file; a `true` flag turns it on.
        let base = RunConfig {
            listen: Some(true),
            ..Default::default()
        };
        assert!(base.merge(CliRun::default()).listen);
        let on_by_flag = RunConfig::default().merge(CliRun {
            listen: true,
            ..Default::default()
        });
        assert!(on_by_flag.listen);
    }
}
