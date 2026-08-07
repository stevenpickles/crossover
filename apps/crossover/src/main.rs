//! The Crossover binary: CLI, configuration, and composition root.
//!
//! Wires platform implementations into `crossover-core` behind the
//! `crossover-platform` traits (docs/ARCHITECTURE.md §2, §3).
//!
//! Error-handling boundary (docs/ARCHITECTURE.md §9): library crates
//! return typed errors; this binary attaches operational context via
//! `anyhow` and renders concise user-facing messages.

use anyhow::bail;
use clap::{Parser, Subcommand};

/// Secure keyboard, mouse, and clipboard sharing between trusted computers.
///
/// The CLI surface required by FR-7.1 (docs/SPECIFICATION.md). Subcommands
/// are stubs until their roadmap phase arrives; each names the phase that
/// implements it so failures are actionable rather than mysterious.
#[derive(Debug, Parser)]
#[command(name = "crossover", version, about, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run Crossover in the foreground (clipboard sync arrives in Phase 2).
    Run,
    /// Pair this computer with a trusted peer (Phase 1).
    Pair,
    /// List trusted peers (Phase 1).
    Peers,
    /// Report connection and session status (Phase 1).
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => not_yet("run", "Phase 2 (Reliable Text Clipboard)"),
        Command::Pair => not_yet("pair", "Phase 1 (Secure Peer Connection)"),
        Command::Peers => not_yet("peers", "Phase 1 (Secure Peer Connection)"),
        Command::Status => not_yet("status", "Phase 1 (Secure Peer Connection)"),
    }
}

/// Stub failure for commands whose roadmap phase has not been reached.
///
/// Failing (rather than exiting 0 after a message) keeps scripting honest:
/// nothing that did not happen reports success.
fn not_yet(command: &str, phase: &str) -> anyhow::Result<()> {
    bail!(
        "`crossover {command}` is not implemented yet — it arrives in {phase}; see docs/ROADMAP.md"
    )
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    // Catches invalid clap derive configurations (conflicting flags,
    // ambiguous subcommands) at test time instead of first invocation.
    #[test]
    fn cli_definition_is_internally_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn all_stub_subcommands_parse() {
        for (argv, expected) in [
            ("run", "Run"),
            ("pair", "Pair"),
            ("peers", "Peers"),
            ("status", "Status"),
        ] {
            let cli = Cli::try_parse_from(["crossover", argv])
                .unwrap_or_else(|e| panic!("`{argv}` failed to parse: {e}"));
            let name = match cli.command {
                Command::Run => "Run",
                Command::Pair => "Pair",
                Command::Peers => "Peers",
                Command::Status => "Status",
            };
            assert_eq!(name, expected);
        }
    }

    #[test]
    fn bare_invocation_is_rejected_with_usage() {
        // No default subcommand: running `crossover` bare must show usage,
        // not silently pick a behavior.
        assert!(Cli::try_parse_from(["crossover"]).is_err());
    }
}
