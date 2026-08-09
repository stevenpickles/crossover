//! The Crossover binary: CLI, configuration, and composition root.
//!
//! Wires platform implementations into `crossover-core` behind the
//! `crossover-platform` traits (docs/ARCHITECTURE.md §2, §3).
//!
//! Error-handling boundary (docs/ARCHITECTURE.md §9): library crates
//! return typed errors; this binary attaches operational context via
//! `anyhow` and renders concise user-facing messages.

mod commands;
mod config;
mod console;
mod logging;
mod storage;

use clap::{Args, Parser, Subcommand};
use uuid::Uuid;

/// Secure keyboard, mouse, and clipboard sharing between trusted computers.
///
/// The CLI surface required by FR-7.1 (docs/SPECIFICATION.md). Remaining
/// stubs name the phase that implements them so failures are actionable
/// rather than mysterious.
#[derive(Debug, Parser)]
#[command(name = "crossover", version, about, propagate_version = true)]
struct Cli {
    /// Device name for this machine (defaults to the hostname). Used when
    /// the identity is first generated; ignored afterwards.
    #[arg(long, global = true)]
    name: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run Crossover in the foreground (clipboard sync arrives in Phase 2).
    Run(RunArgs),
    /// Pair this computer with a trusted peer (ADR 0002).
    Pair(PairArgs),
    /// List trusted peers, or manage them with a subcommand.
    Peers {
        #[command(subcommand)]
        action: Option<PeersAction>,
    },
    /// Report identity and trust status.
    Status,
    /// Show the startup config file path and its effective settings.
    Config,
}

// A CLI flag struct is naturally a bag of independent bools (clap maps each
// `--flag` to one); the excessive-bools heuristic does not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Args)]
struct RunArgs {
    /// Accept inbound sessions from trusted peers.
    #[arg(long)]
    listen: bool,

    /// Bind address for --listen (default 0.0.0.0:27677).
    /// (Validated in code: requires --listen; `requires=` cannot express
    /// this against a defaulted bool flag.)
    #[arg(long)]
    bind: Option<String>,

    /// Maintain an outbound session to this peer address.
    #[arg(long)]
    connect: Option<String>,

    /// Seamless mode: this machine is the LEFT screen of a left–right
    /// pair, so its right edge crosses to the peer (ADR 0009). The cursor
    /// follows across the edge with no manual switch.
    #[arg(long, conflicts_with = "right")]
    left: bool,

    /// Seamless mode: this machine is the RIGHT screen, so its left edge
    /// crosses to the peer.
    #[arg(long)]
    right: bool,

    /// Diagnostic: do not hide the local cursor while controlling the peer.
    /// Isolates cursor-masking behavior from control transfer when a soak
    /// misbehaves (ADR 0009).
    #[arg(long)]
    no_cursor_mask: bool,
}

#[derive(Debug, Args)]
struct PairArgs {
    /// Address of the machine that ran `crossover pair --listen`
    /// (e.g. 192.168.1.25:27677). You will be prompted for its code.
    #[arg(required_unless_present = "listen", conflicts_with = "listen")]
    address: Option<String>,

    /// Listen for one pairing attempt and display a one-time code.
    #[arg(long)]
    listen: bool,

    /// Bind address for --listen (default 0.0.0.0:27677).
    #[arg(long, conflicts_with = "address")]
    bind: Option<String>,
}

#[derive(Debug, Subcommand)]
enum PeersAction {
    /// Revoke a trusted peer by device id (`crossover peers` lists them).
    Remove {
        /// The peer's device id (UUID).
        device_id: Uuid,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse first so `--help`/`--version` exit without emitting log lines.
    let cli = Cli::parse();
    // Become per-monitor DPI aware before anything reads display geometry
    // or creates a window/hook, so coordinates are real pixels across
    // mixed-DPI monitors (R-3, ADR 0009).
    crossover_platform_windows::set_process_dpi_awareness();
    logging::init()?;
    // Structured-field exemplar (docs/ARCHITECTURE.md §10): values as
    // fields, snake_case canonical names, message as the human summary.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        command = ?cli.command,
        "starting"
    );

    match cli.command {
        Command::Run(args) => {
            // Merge CLI flags over the startup config file: a flag present on
            // the command line wins; otherwise the file supplies the value
            // (Phase 6). `--name` is global, so it rides in from `cli`.
            let effective = config::load_run_config()?.merge(config::CliRun {
                name: cli.name,
                listen: args.listen,
                bind: args.bind,
                connect: args.connect,
                left: args.left,
                right: args.right,
                no_cursor_mask: args.no_cursor_mask,
            });
            if !effective.listen && effective.connect.is_none() {
                anyhow::bail!(
                    "`crossover run` needs a role: --listen (or `listen = true` in \
                     config.toml) to accept trusted peers, --connect <address> (or \
                     `connect = ...`) to dial one, or both"
                );
            }
            if effective.bind.is_some() && !effective.listen {
                anyhow::bail!("a bind address only applies with --listen / listen = true");
            }
            let listen_bind = effective.listen.then(|| {
                effective
                    .bind
                    .clone()
                    .unwrap_or_else(|| format!("0.0.0.0:{}", crossover_protocol::DEFAULT_PORT))
            });
            let side = effective.side.map(|side| match side {
                config::Side::Left => crossover_core::LinkSide::Left,
                config::Side::Right => crossover_core::LinkSide::Right,
            });
            let device_name = storage::resolve_device_name(effective.name);
            let result = commands::run(
                &device_name,
                listen_bind,
                effective.connect,
                side,
                effective.no_cursor_mask,
            )
            .await;
            // Seamless masking may have blanked the system cursor; restore it
            // synchronously on the way out so a quit — however `run` ended —
            // never leaves the machine cursor-less (ADR 0009). Idempotent
            // when nothing was masked.
            crossover_platform_windows::restore_system_cursors();
            result
        }
        Command::Pair(args) => {
            let device_name = storage::resolve_device_name(cli.name);
            match args.address {
                Some(address) => commands::pair_connect(&device_name, &address).await,
                None => commands::pair_listen(&device_name, args.bind).await,
            }
        }
        Command::Peers { action } => match action {
            None => commands::peers_list(),
            Some(PeersAction::Remove { device_id }) => commands::peers_remove(device_id),
        },
        Command::Status => commands::status(&storage::resolve_device_name(cli.name)),
        Command::Config => commands::config_show(),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, PeersAction};

    // Catches invalid clap derive configurations (conflicting flags,
    // ambiguous subcommands) at test time instead of first invocation.
    #[test]
    fn cli_definition_is_internally_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn pair_requires_address_or_listen_but_not_both() {
        assert!(Cli::try_parse_from(["crossover", "pair"]).is_err());
        assert!(Cli::try_parse_from(["crossover", "pair", "10.0.0.2:27677", "--listen"]).is_err());
        // --bind only makes sense while listening.
        assert!(
            Cli::try_parse_from(["crossover", "pair", "10.0.0.2:27677", "--bind", "x"]).is_err()
        );

        let cli = Cli::try_parse_from(["crossover", "pair", "10.0.0.2:27677"]).unwrap();
        let Command::Pair(args) = cli.command else {
            panic!("expected pair");
        };
        assert_eq!(args.address.as_deref(), Some("10.0.0.2:27677"));

        let cli = Cli::try_parse_from(["crossover", "pair", "--listen", "--bind", "0.0.0.0:1234"])
            .unwrap();
        let Command::Pair(args) = cli.command else {
            panic!("expected pair");
        };
        assert!(args.listen);
        assert_eq!(args.bind.as_deref(), Some("0.0.0.0:1234"));
    }

    #[test]
    fn peers_parses_bare_and_remove_forms() {
        let cli = Cli::try_parse_from(["crossover", "peers"]).unwrap();
        assert!(matches!(cli.command, Command::Peers { action: None }));

        let id = "8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8";
        let cli = Cli::try_parse_from(["crossover", "peers", "remove", id]).unwrap();
        let Command::Peers {
            action: Some(PeersAction::Remove { device_id }),
        } = cli.command
        else {
            panic!("expected peers remove");
        };
        assert_eq!(device_id.to_string(), id);

        // A non-UUID id is rejected at parse time.
        assert!(Cli::try_parse_from(["crossover", "peers", "remove", "not-a-uuid"]).is_err());
    }

    #[test]
    fn global_name_flag_applies_across_subcommands() {
        let cli = Cli::try_parse_from(["crossover", "--name", "left", "status"]).unwrap();
        assert_eq!(cli.name.as_deref(), Some("left"));
        let cli = Cli::try_parse_from(["crossover", "status", "--name", "left"]).unwrap();
        assert_eq!(cli.name.as_deref(), Some("left"));
    }

    #[test]
    fn bare_invocation_is_rejected_with_usage() {
        // No default subcommand: running `crossover` bare must show usage,
        // not silently pick a behavior.
        assert!(Cli::try_parse_from(["crossover"]).is_err());
    }
}
