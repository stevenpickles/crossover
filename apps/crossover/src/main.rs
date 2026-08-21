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
mod paths;
mod storage;

// Shared with `crossover-svc` so both binaries of one install report the same
// identity (apps/build_identity.rs explains why it is an include, not a crate).
#[path = "../../build_info.rs"]
mod build_info;

use clap::{Args, Parser, Subcommand};
use uuid::Uuid;

/// Secure keyboard, mouse, and clipboard sharing between trusted computers.
///
/// The CLI surface required by FR-7.1 (docs/SPECIFICATION.md). Remaining
/// stubs name the phase that implements them so failures are actionable
/// rather than mysterious.
#[derive(Debug, Parser)]
#[command(
    name = "crossover",
    // `-V` stays terse for scripts and logs; `--version` spells the build out
    // in full, which is what you actually want when two machines disagree.
    version = build_info::VERSION,
    long_version = long_version(),
    about,
    propagate_version = true
)]
struct Cli {
    /// Device name for this machine (defaults to the hostname). Used when
    /// the identity is first generated; ignored afterwards.
    #[arg(long, global = true)]
    name: Option<String>,

    #[command(subcommand)]
    command: Command,
}

// Two of these verbs are also spelled out in another binary: the layout
// editor's empty state tells the user to run `crossover run` or `crossover
// service install` (apps/crossover-layout/src/render.rs's
// `draw_worker_never_run`, which carries the matching note). Renaming either
// here without changing that text leaves the editor naming a command that no
// longer exists. Sharing the strings would mean a crate between the two
// binaries for four words, which ADR 0019's dependency rule makes a poor
// trade — so the coupling is deliberately held by these two comments and the
// editor's test.
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
    /// Report this build in full: version, source commit, toolchain, and the
    /// protocol versions it speaks.
    Version {
        /// Emit the report as a JSON object instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Manage the background service that runs Crossover unattended (ADR 0011).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Open the display layout editor: arrange both machines' monitors and
    /// save the arrangement (ADR 0018, ADR 0019).
    Layout,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum ServiceAction {
    /// Install and register the background service (requires Administrator).
    Install,
    /// Stop and remove the background service (requires Administrator).
    Uninstall,
    /// Report whether the background service is installed and running.
    Status,
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

    /// Deprecated: seamless mode, this machine is the LEFT screen of a
    /// left–right pair, so its right edge crosses to the peer (ADR 0009).
    /// The cursor follows across the edge with no manual switch. Draw an
    /// arrangement with `crossover layout` instead (ADR 0018); an explicit
    /// layout in the config wins over this flag.
    #[arg(long, conflicts_with = "right")]
    left: bool,

    /// Deprecated: seamless mode, this machine is the RIGHT screen, so its
    /// left edge crosses to the peer. Draw an arrangement with `crossover
    /// layout` instead (ADR 0018); an explicit layout in the config wins
    /// over this flag.
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

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum PeersAction {
    /// Revoke a trusted peer by device id (`crossover peers` lists them).
    Remove {
        /// The peer's device id (UUID).
        device_id: Uuid,
    },
    /// Let a trusted peer send you files. Off for every peer until you run
    /// this: pairing does not grant it (ADR 0015).
    AllowFiles {
        /// The peer's device id (UUID).
        device_id: Uuid,
    },
    /// Withdraw a peer's permission to send you files.
    DenyFiles {
        /// The peer's device id (UUID).
        device_id: Uuid,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse first so `--help`/`--version` exit without emitting log lines.
    let cli = Cli::parse();
    // When launched by the background service there is no console, so invalid
    // stdout/stderr would make the first `println!`/log line panic and crash
    // the worker into a relaunch loop. Repoint them at NUL before any output
    // (a no-op for interactive runs and redirections) (ADR 0011).
    crossover_platform_windows::ensure_standard_streams();
    // Become per-monitor DPI aware before anything reads display geometry
    // or creates a window/hook, so coordinates are real pixels across
    // mixed-DPI monitors (R-3, ADR 0009).
    crossover_platform_windows::set_process_dpi_awareness();
    // A pure query about the binary itself, answered before the logger
    // starts: `crossover version --json` must be parseable output, not
    // output with a log line in front of it.
    if let Command::Version { json } = &cli.command {
        print_version_report(*json);
        return Ok(());
    }
    // Hold the guard for the whole process: dropping it flushes and stops the
    // rolling-file writer (docs/SOAK.md Phase 6 observability).
    let _log_guard = logging::init()?;
    // Installed the moment there is somewhere for it to write: from here on,
    // a panic reaches the log file rather than a NUL stderr (ADR 0011).
    logging::install_panic_hook();
    // Structured-field exemplar (docs/ARCHITECTURE.md §10): values as
    // fields, snake_case canonical names, message as the human summary.
    // The build version, not the Cargo version: a log that says "0.1.0" when
    // the binary is an untagged dev build names the wrong thing.
    tracing::info!(
        version = build_info::VERSION,
        commit = build_info::BUILD_INFO.git_short_commit,
        command = ?cli.command,
        "starting"
    );

    let outcome = dispatch(cli).await;
    // Record a fatal error in the log too. For a headless service-launched
    // worker, stderr goes to NUL (ADR 0011), so without this a startup failure
    // (e.g. no role from a missing config) is invisible behind the relaunch
    // loop — you see that it restarts, never why. `{error:#}` includes the full
    // anyhow cause chain; the error is still returned, so the exit code and the
    // interactive stderr message are unchanged.
    if let Err(error) = &outcome {
        tracing::error!(error = format!("{error:#}"), "command failed");
    }
    outcome
}

/// Write the report to stdout. Shared by the early exit in `main` and the
/// dispatch arm, which stay separate because the early exit is what keeps
/// the logger out of the output.
fn print_version_report(json: bool) {
    print!("{}", version_report(json));
}

/// The text report, cached: clap wants a `&'static str`, and it asks for one
/// every time the command is built — which is once per process, `--version`
/// or not.
fn long_version() -> &'static str {
    static LONG_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LONG_VERSION
        .get_or_init(|| build_info::long_version(&protocol_fields()))
        .as_str()
}

/// The full build report, with the protocol range appended. A version
/// mismatch and a protocol mismatch look identical from the outside — two
/// machines that will not talk — so the report answers both at once
/// (docs/PROTOCOL.md §3).
fn version_report(json: bool) -> String {
    build_info::report(&protocol_fields(), json)
}

/// The versions this build speaks, as report fields.
fn protocol_fields() -> [(&'static str, build_info::Value); 2] {
    let supported = crossover_protocol::VersionRange::CURRENT;
    [
        (
            "protocol_version",
            build_info::Value::Num(u64::from(supported.max)),
        ),
        (
            "min_protocol_version",
            build_info::Value::Num(u64::from(supported.min)),
        ),
    ]
}

/// Route the parsed command to its handler. Separate from `main` so every
/// failure path — including a `?` or `bail!` inside an arm — returns here and is
/// logged, not only the errors an arm yields as its value.
async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run(args) => {
            // Merge CLI flags over the startup config file: a flag present on
            // the command line wins; otherwise the file supplies the value
            // (Phase 6). `--name` is global, so it rides in from `cli`.
            // `load_run_config` is the one place `[layout]` is validated
            // (ADR 0018); `merge` consumes that already-validated result and
            // returns every warning it decided as data, rendered here, once.
            let (effective, notices) = config::load_run_config()?.merge(config::CliRun {
                name: cli.name,
                listen: args.listen,
                bind: args.bind,
                connect: args.connect,
                left: args.left,
                right: args.right,
                no_cursor_mask: args.no_cursor_mask,
            });
            for notice in &notices {
                render_config_notice(notice);
            }
            // Log the resolved role up front so a headless worker's log shows
            // what it is trying to do (listen/dial/layout), not just that it
            // started — the fastest read on a "not connecting" soak. The
            // layout summary is the discriminant and a revision, not the
            // full monitor list: this line stays one line even with a large
            // drawn arrangement configured.
            let layout_summary = effective
                .layout_source
                .as_ref()
                .map_or_else(|| "none".to_owned(), config::LayoutSource::summary);
            tracing::info!(
                listen = effective.listen,
                connect = ?effective.connect,
                layout = %layout_summary,
                "run configuration",
            );
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
            let device_name = storage::resolve_device_name(effective.name);
            let result = commands::run(
                &device_name,
                listen_bind,
                effective.connect,
                effective.layout_source,
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
            Some(PeersAction::AllowFiles { device_id }) => {
                commands::peers_set_file_receive(device_id, true)
            }
            Some(PeersAction::DenyFiles { device_id }) => {
                commands::peers_set_file_receive(device_id, false)
            }
        },
        Command::Status => commands::status(&storage::resolve_device_name(cli.name)),
        Command::Config => commands::config_show(),
        Command::Version { json } => {
            print_version_report(json);
            Ok(())
        }
        Command::Service { action } => match action {
            ServiceAction::Install => commands::service_install(),
            ServiceAction::Uninstall => commands::service_uninstall(),
            ServiceAction::Status => commands::service_status(),
        },
        Command::Layout => commands::layout(),
    }
}

/// Render one [`config::ConfigNotice`] into the log and stderr — the single
/// place `crossover run` warns about a deprecated flag or key, an
/// override, or a `[layout]` that failed validation. `config::merge`
/// returns notices as data specifically so this is the only place that
/// does it, rather than every decision point inside `merge` emitting its
/// own near-identical `tracing::warn!`/`eprintln!` pair.
fn render_config_notice(notice: &config::ConfigNotice) {
    match notice {
        config::ConfigNotice::DeprecatedFlag { flag } => {
            tracing::warn!(
                flag,
                "deprecated: draw an arrangement with `crossover layout` instead (ADR 0018)"
            );
            eprintln!(
                "Warning: {flag} is deprecated; draw an arrangement with `crossover layout` \
                 instead (ADR 0018)."
            );
        }
        config::ConfigNotice::DeprecatedSideKey => {
            tracing::warn!(
                "deprecated: [seamless] side is retired by ADR 0018; draw an arrangement \
                 with `crossover layout` instead"
            );
            eprintln!(
                "Warning: [seamless] side is deprecated; draw an arrangement with \
                 `crossover layout` instead (ADR 0018)."
            );
        }
        config::ConfigNotice::ExplicitLayoutWins { overridden } => {
            // Both facts, so the message cannot mislead: the layout wins,
            // AND this build cannot drive it yet, so seamless is off
            // either way until the layout engine lands.
            tracing::warn!(
                overridden,
                "ignored: the config holds an explicit [layout], which wins over it (ADR \
                 0018) — but this build's crossing engine predates the layout model, so \
                 seamless stays off regardless until it lands; remove [layout] to fall back \
                 to the side model"
            );
            eprintln!(
                "Warning: {overridden} is ignored — the config already holds an explicit \
                 [layout], which takes precedence (ADR 0018). This build's crossing engine \
                 cannot drive a drawn layout yet, so seamless transfer stays OFF regardless, \
                 until that engine lands; remove [layout] to fall back to the side model in \
                 the meantime."
            );
        }
        config::ConfigNotice::InvalidLayout { error } => {
            tracing::warn!(
                error = %error,
                "the [layout] section is invalid; treating this run as having no layout \
                 (seamless off, explicit control intact)"
            );
            eprintln!(
                "Warning: the [layout] section in the config is invalid and is being ignored \
                 for this run — seamless transfer is off, but explicit control still works: \
                 {error}\nRun `crossover config` to see this diagnosis again, or fix it with \
                 `crossover layout`."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use uuid::Uuid;

    use super::{Cli, Command, PeersAction, ServiceAction};

    // Catches invalid clap derive configurations (conflicting flags,
    // ambiguous subcommands) at test time instead of first invocation.
    #[test]
    fn cli_definition_is_internally_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// `--left`/`--right` are deprecated (ADR 0018), but still parse — a
    /// script or a service registration that still names one keeps working
    /// (`config::RunConfig::merge` is what actually retires them, by
    /// turning an explicit `[layout]` into the winner).
    #[test]
    fn run_still_parses_the_deprecated_left_and_right_flags() {
        let cli = Cli::try_parse_from(["crossover", "run", "--left"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert!(args.left);
        assert!(!args.right);

        let cli = Cli::try_parse_from(["crossover", "run", "--right"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert!(args.right);

        // Still mutually exclusive.
        assert!(Cli::try_parse_from(["crossover", "run", "--left", "--right"]).is_err());
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
    fn file_permission_is_granted_and_withdrawn_by_dedicated_verbs() {
        let id: Uuid = "8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8".parse().unwrap();
        for (args, expected) in [
            (
                ["crossover", "peers", "allow-files", &id.to_string()],
                PeersAction::AllowFiles { device_id: id },
            ),
            (
                ["crossover", "peers", "deny-files", &id.to_string()],
                PeersAction::DenyFiles { device_id: id },
            ),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Peers {
                action: Some(action),
            } = cli.command
            else {
                panic!("expected peers action for {args:?}");
            };
            assert_eq!(action, expected);
        }

        // Each verb names exactly one peer: no bare form that could act on
        // every peer at once, and no id that is not a device id (ADR 0015 —
        // the grant is explicit and per peer).
        assert!(Cli::try_parse_from(["crossover", "peers", "allow-files"]).is_err());
        assert!(Cli::try_parse_from(["crossover", "peers", "deny-files"]).is_err());
        assert!(Cli::try_parse_from(["crossover", "peers", "allow-files", "all"]).is_err());
        assert!(
            Cli::try_parse_from([
                "crossover",
                "peers",
                "allow-files",
                &id.to_string(),
                &id.to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn global_name_flag_applies_across_subcommands() {
        let cli = Cli::try_parse_from(["crossover", "--name", "left", "status"]).unwrap();
        assert_eq!(cli.name.as_deref(), Some("left"));
        let cli = Cli::try_parse_from(["crossover", "status", "--name", "left"]).unwrap();
        assert_eq!(cli.name.as_deref(), Some("left"));
    }

    #[test]
    fn service_parses_its_three_subcommands() {
        for (args, expected) in [
            (["crossover", "service", "install"], ServiceAction::Install),
            (
                ["crossover", "service", "uninstall"],
                ServiceAction::Uninstall,
            ),
            (["crossover", "service", "status"], ServiceAction::Status),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Service { action } = cli.command else {
                panic!("expected service command for {args:?}");
            };
            assert_eq!(action, expected);
        }
        // `service` with no subcommand is a usage error, not a default action.
        assert!(Cli::try_parse_from(["crossover", "service"]).is_err());
    }

    #[test]
    fn layout_is_a_bare_verb_that_takes_no_arguments() {
        let cli = Cli::try_parse_from(["crossover", "layout"]).unwrap();
        assert!(matches!(cli.command, Command::Layout));
        // The editor takes its input from the state file and the config, not
        // from this command line (ADR 0018) — so anything here is a mistake
        // worth reporting rather than an argument to forward.
        assert!(Cli::try_parse_from(["crossover", "layout", "--edit"]).is_err());
        assert!(Cli::try_parse_from(["crossover", "layout", "topology.json"]).is_err());
    }

    #[test]
    fn bare_invocation_is_rejected_with_usage() {
        // No default subcommand: running `crossover` bare must show usage,
        // not silently pick a behavior.
        assert!(Cli::try_parse_from(["crossover"]).is_err());
    }
}
