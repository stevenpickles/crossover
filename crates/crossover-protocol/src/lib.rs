//! Wire protocol for Crossover: message definitions, framing, versioning,
//! and validation of everything that arrives from the network.
//!
//! This crate has no I/O and no dependencies so that the protocol is
//! testable (and fuzzable) without sockets. Wire-level invariants are
//! specified in `docs/PROTOCOL.md`; layering rules in `docs/ARCHITECTURE.md`.

pub mod clipboard;
pub mod control;
pub mod file_name;
pub mod framing;
pub mod hello;
pub mod input;
pub mod pairing;
pub mod version;

pub use clipboard::{
    ApplyResult, ChunkOutcome, ChunkPlan, ChunkReassembly, ChunkStream, ClipboardAccept,
    ClipboardApplied, ClipboardChunk, ClipboardData, ClipboardDecline, ClipboardMeta,
    ClipboardOffer, ContentType, DeclineReason, FileDescriptor, ImageFormat, StreamOutcome,
};
pub use control::{ControlRelease, ControlRequest, ControlResponse, ControlVerdict, DenyReason};
pub use file_name::{FileNameError, validate_file_name};
pub use framing::{FrameDecoder, RawFrame, encode_frame};
pub use hello::{FeatureFlags, Hello, MessageType, OsFamily};
pub use input::{InputBatch, ReleaseAllInput, WireButton, WireInputEvent};
pub use pairing::{PairingConfirm, PairingStart};
pub use version::{PROTOCOL_VERSION, VersionRange, negotiate};

/// Decode a full postcard payload, rejecting trailing bytes — the strict
/// contract every message decoder shares (docs/PROTOCOL.md §2: payload
/// lengths are exact).
pub(crate) fn decode_strict<'a, T: serde::Deserialize<'a>>(
    payload: &'a [u8],
    what: &str,
) -> Result<T, ProtocolError> {
    let (value, rest): (T, &[u8]) =
        postcard::take_from_bytes(payload).map_err(|e| ProtocolError::Malformed {
            reason: format!("undecodable {what} payload: {e}"),
        })?;
    if !rest.is_empty() {
        return Err(ProtocolError::Malformed {
            reason: format!("{} trailing bytes after {what} payload", rest.len()),
        });
    }
    Ok(value)
}

/// Decode the leading value of a payload and **ignore what follows** —
/// the deliberate opposite of [`decode_strict`], for the one case that
/// wants a prefix: reading a message's fixed-size header without touching
/// (or copying) the variable-length content behind it. postcard is
/// sequential, so a struct's first field is a decodable prefix of it.
pub(crate) fn decode_prefix<'a, T: serde::Deserialize<'a>>(
    payload: &'a [u8],
    what: &str,
) -> Result<T, ProtocolError> {
    let (value, _rest): (T, &[u8]) =
        postcard::take_from_bytes(payload).map_err(|e| ProtocolError::Malformed {
            reason: format!("undecodable {what} prefix: {e}"),
        })?;
    Ok(value)
}

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str =
    "wire messages, framing, versioning, and validation (docs/PROTOCOL.md)";

/// Default TCP port — "CROSS" on a phone keypad (ADR 0004). Defined once
/// here; configuration, documentation, and diagnostics all reference this
/// constant.
pub const DEFAULT_PORT: u16 = 27677;

/// Errors produced by protocol framing and message validation.
///
/// This enum is the workspace's exemplar for error-handling conventions
/// (`docs/ARCHITECTURE.md` §9): library crates define typed errors with
/// `thiserror`; variants carry the data an actionable diagnostic needs
/// rather than pre-formatted strings; and failures caused by untrusted
/// network input are ordinary values — never panics (NFR-1).
///
/// Every variant here corresponds to a rejection path required by
/// `docs/PROTOCOL.md`. Each maps to the fail-closed handling of §7.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// A frame declared a length larger than the negotiated maximum.
    ///
    /// Detected before any allocation occurs (NFR-1): the declared length
    /// is validated against `max` while only the fixed-size frame header
    /// has been read.
    #[error("frame length {declared} exceeds maximum {max}")]
    FrameTooLarge { declared: u64, max: u64 },

    /// The peers' supported protocol version ranges do not intersect
    /// (`docs/PROTOCOL.md` §3). The session terminates; there is no
    /// silent downgrade.
    #[error(
        "no common protocol version (local supports {local_min}..={local_max}, \
         peer offered {peer_min}..={peer_max})"
    )]
    NoCommonVersion {
        local_min: u16,
        local_max: u16,
        peer_min: u16,
        peer_max: u16,
    },

    /// A structurally invalid message was received. Framing-level
    /// malformation is fatal to the session (`docs/PROTOCOL.md` §7).
    #[error("malformed message: {reason}")]
    Malformed { reason: String },

    /// Serializing an outbound message failed. A local fault, not a peer
    /// fault — kept distinct from [`ProtocolError::Malformed`] so
    /// diagnostics never blame a peer for our own encoding failure.
    #[error("encoding outbound message failed: {reason}")]
    Encode { reason: String },

    /// A bounded, already-validated allocation could not be made.
    ///
    /// Distinct from [`ProtocolError::Malformed`] because the peer did
    /// nothing wrong: the length was inside its limit, and this machine
    /// could not honour it anyway. It exists so that path returns a value
    /// the caller can decline with, instead of the process aborting —
    /// Rust's default on allocation failure is `abort`, and a remote peer
    /// must never be able to reach it (NFR-1).
    #[error("cannot reserve {requested} bytes for {what}")]
    ResourceExhausted {
        /// What the memory was for, for the diagnostic.
        what: &'static str,
        /// How many bytes were asked for.
        requested: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::{CRATE_PURPOSE, ProtocolError};

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }

    // Diagnostics must be actionable (FR-7.1): an operator reading the
    // message alone should see the offending values, not just a category.
    #[test]
    fn frame_too_large_reports_declared_and_maximum() {
        let err = ProtocolError::FrameTooLarge {
            declared: 10_000,
            max: 4_096,
        };
        let msg = err.to_string();
        assert!(msg.contains("10000"), "missing declared length: {msg}");
        assert!(msg.contains("4096"), "missing maximum: {msg}");
    }

    #[test]
    fn no_common_version_reports_both_ranges() {
        let err = ProtocolError::NoCommonVersion {
            local_min: 1,
            local_max: 2,
            peer_min: 3,
            peer_max: 4,
        };
        let msg = err.to_string();
        assert!(msg.contains("1..=2"), "missing local range: {msg}");
        assert!(msg.contains("3..=4"), "missing peer range: {msg}");
    }

    // Errors are plain comparable values so state-machine tests can assert
    // on exact rejection reasons.
    #[test]
    fn errors_are_comparable_values() {
        let a = ProtocolError::Malformed {
            reason: "truncated payload".to_owned(),
        };
        let b = ProtocolError::Malformed {
            reason: "truncated payload".to_owned(),
        };
        assert_eq!(a, b);
    }
}
