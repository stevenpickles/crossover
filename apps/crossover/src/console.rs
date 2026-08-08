//! Interactive console commands for `crossover run`.
//!
//! Phase 3's explicit control switching (docs/ROADMAP.md; FR-5.1) needs
//! a trigger, and `run` is already a foreground process reading nothing
//! from its terminal. Line commands are that trigger: no global hotkey
//! hook (which is Phase 4 keyboard-capture territory), no new platform
//! surface, and it composes with the existing Ctrl-C shutdown.
//!
//! Parsing is separated from I/O so it is testable without a terminal.

/// A parsed console command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleCommand {
    /// Request control of the peer.
    TakeControl,
    /// End whichever control relationship exists (hand back a grant,
    /// cancel a pending request, or revoke the peer's control).
    Release,
    /// Print the command help.
    Help,
    /// Stop `crossover run`.
    Quit,
}

/// Parse one line of console input. `None` for blank lines (a bare
/// Enter is not an error) and unrecognized input, which the caller
/// treats as a nudge to show help.
///
/// Deliberately forgiving: single letters for speed, whole words for
/// discoverability, case-insensitive, surrounding whitespace ignored.
#[must_use]
pub fn parse(line: &str) -> Option<ConsoleCommand> {
    match line.trim().to_ascii_lowercase().as_str() {
        "c" | "control" | "take" => Some(ConsoleCommand::TakeControl),
        "r" | "release" | "back" => Some(ConsoleCommand::Release),
        "h" | "help" | "?" => Some(ConsoleCommand::Help),
        "q" | "quit" | "exit" => Some(ConsoleCommand::Quit),
        _ => None,
    }
}

/// The one-screen help, shown at startup and on any unrecognized line.
pub const HELP: &str = "\
Commands:  c  take control of the peer     r  release / hand back
           h  this help                    q  quit";

#[cfg(test)]
mod tests {
    use super::{ConsoleCommand, parse};

    #[test]
    fn short_and_long_forms_map_to_the_same_command() {
        for take in ["c", "control", "take", "CONTROL", "  take  "] {
            assert_eq!(parse(take), Some(ConsoleCommand::TakeControl), "{take:?}");
        }
        for release in ["r", "release", "back", "Release"] {
            assert_eq!(parse(release), Some(ConsoleCommand::Release), "{release:?}");
        }
        for quit in ["q", "quit", "exit", "QUIT"] {
            assert_eq!(parse(quit), Some(ConsoleCommand::Quit), "{quit:?}");
        }
        for help in ["h", "help", "?"] {
            assert_eq!(parse(help), Some(ConsoleCommand::Help), "{help:?}");
        }
    }

    #[test]
    fn blank_and_unknown_lines_are_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse("take control please"), None);
        assert_eq!(parse("xyzzy"), None);
    }
}
