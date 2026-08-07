//! Wire protocol for Crossover: message definitions, framing, versioning,
//! and validation of everything that arrives from the network.
//!
//! This crate has no I/O and no dependencies so that the protocol is
//! testable (and fuzzable) without sockets. Wire-level invariants are
//! specified in `docs/PROTOCOL.md`; layering rules in `docs/ARCHITECTURE.md`.

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str =
    "wire messages, framing, versioning, and validation (docs/PROTOCOL.md)";

#[cfg(test)]
mod tests {
    use super::CRATE_PURPOSE;

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }
}
