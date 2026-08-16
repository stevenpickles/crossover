//! Control-transfer wire messages (docs/PROTOCOL.md §6, FR-5.1/5.3).
//!
//! Control ownership is explicit, negotiated state — never inferred
//! (FR-5.1). The negotiation is request → acknowledge → switch
//! (FR-5.3): the requester captures nothing until the destination has
//! said yes, so both peers agree on ownership even when the answer is
//! delayed. Phase 3 triggers requests explicitly (CLI); Phase 5 will
//! trigger them from edge crossings without touching these messages.
//!
//! `request_id` pairs a response with its request. A response for a
//! request the requester no longer has in flight (it timed out, or a
//! newer request superseded it) is ignored, not an error: delay is the
//! condition the negotiation exists to survive.

use serde::{Deserialize, Serialize};

use crate::ProtocolError;
use crate::decode_strict;

/// Ask the peer for control: "my input becomes yours to apply."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    /// Requester-assigned, monotonic per session; echoed by the
    /// response so a late answer cannot be mistaken for a current one.
    pub request_id: u64,
    /// Normalized entry position along the linked edge when the request
    /// came from an edge crossing (ADR 0009): `0` is the top of the edge,
    /// `u16::MAX` the bottom. `None` for an explicit (console) request,
    /// which places no cursor. Resolution- and DPI-independent — the
    /// grantee maps it through its own geometry.
    pub entry: Option<u16>,
}

/// Why a control request was denied. On the wire so the *requester* can
/// say why nothing happened (NFR-3: a failed control transfer produces
/// a diagnostic on the side where the user is looking).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// The destination is itself controlling, or requesting control of,
    /// a peer. Exactly one active destination (FR-5.1) makes
    /// simultaneous requests from both sides resolve to two denials —
    /// deterministic, if unsatisfying; either user simply retries.
    Busy,
    /// The destination is already being controlled.
    AlreadyControlled,
}

/// The destination's verdict on a [`ControlRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlVerdict {
    /// Control is granted: the requester may begin capturing and
    /// sending input.
    Granted,
    /// Control is denied, with the reason.
    Denied(DenyReason),
}

/// Answer to a [`ControlRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponse {
    /// The request this answers.
    pub request_id: u64,
    /// Granted or denied.
    pub verdict: ControlVerdict,
}

/// End the control relationship.
///
/// Sent by the controller to hand control back (after `ReleaseAllInput`,
/// which TCP orders ahead of it), or by the *controlled* side to revoke
/// a grant — the local user's escape hatch, and the reverse-edge return
/// (ADR 0009). Whichever direction it travels, the relationship it ends
/// is unambiguous, because only one may exist (FR-5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRelease {
    /// Normalized position where the reclaiming cursor left the linked
    /// edge, when the release is an edge return from the controlled side
    /// (ADR 0009): `0` top, `u16::MAX` bottom. `None` for an explicit
    /// hand-back or console revoke, which places no cursor.
    pub entry: Option<u16>,
}

impl ControlRequest {
    /// Encode the payload (postcard, ADR 0001).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        decode_strict(payload, "ControlRequest")
    }
}

impl ControlResponse {
    /// Encode the payload.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode a payload (strict: no trailing bytes; unknown verdict or
    /// reason discriminants are malformed — fail closed, not guessed).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        decode_strict(payload, "ControlResponse")
    }
}

impl ControlRelease {
    /// Encode the payload (postcard, ADR 0001).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        decode_strict(payload, "ControlRelease")
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlRelease, ControlRequest, ControlResponse, ControlVerdict, DenyReason};
    use crate::ProtocolError;

    #[test]
    fn request_and_response_round_trip() {
        for entry in [None, Some(0), Some(32_768), Some(u16::MAX)] {
            let request = ControlRequest {
                request_id: 42,
                entry,
            };
            assert_eq!(
                ControlRequest::decode_payload(&request.encode_payload().unwrap()).unwrap(),
                request
            );
        }

        for verdict in [
            ControlVerdict::Granted,
            ControlVerdict::Denied(DenyReason::Busy),
            ControlVerdict::Denied(DenyReason::AlreadyControlled),
        ] {
            let response = ControlResponse {
                request_id: 42,
                verdict,
            };
            assert_eq!(
                ControlResponse::decode_payload(&response.encode_payload().unwrap()).unwrap(),
                response
            );
        }
    }

    #[test]
    fn release_round_trips_with_and_without_a_position() {
        for entry in [None, Some(0), Some(u16::MAX)] {
            let release = ControlRelease { entry };
            assert_eq!(
                ControlRelease::decode_payload(&release.encode_payload().unwrap()).unwrap(),
                release
            );
        }
    }

    #[test]
    fn garbage_and_padding_are_malformed() {
        assert!(matches!(
            ControlRequest::decode_payload(&[0xFF; 12]),
            Err(ProtocolError::Malformed { .. })
        ));
        // An unknown verdict discriminant must be rejected, never guessed
        // at (docs/PROTOCOL.md §7).
        assert!(matches!(
            ControlResponse::decode_payload(&[0x01, 0x07]),
            Err(ProtocolError::Malformed { .. })
        ));
        // A release is now `Option<u16>`: `0x00` is a valid `None`, but a
        // trailing byte past it is not (strict decode, no padding).
        assert!(matches!(
            ControlRelease::decode_payload(&[0x00, 0xFF]),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Golden wire snapshots (ADR 0001): schema change = version bump.
    #[test]
    fn golden_wire_snapshots_v2() {
        // v2 (ADR 0009): the request and release carry an `Option<u16>`
        // edge position — `None` is a trailing `0x00`.
        assert_eq!(
            ControlRequest {
                request_id: 1,
                entry: None,
            }
            .encode_payload()
            .unwrap(),
            vec![0x01, 0x00],
            "v2 ControlRequest wire layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlRequest {
                request_id: 1,
                entry: Some(1),
            }
            .encode_payload()
            .unwrap(),
            vec![0x01, 0x01, 0x01],
            "v2 ControlRequest position layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlResponse {
                request_id: 1,
                verdict: ControlVerdict::Granted,
            }
            .encode_payload()
            .unwrap(),
            vec![0x01, 0x00],
            "v2 ControlResponse wire layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlResponse {
                request_id: 2,
                verdict: ControlVerdict::Denied(DenyReason::AlreadyControlled),
            }
            .encode_payload()
            .unwrap(),
            vec![0x02, 0x01, 0x01],
            "v2 ControlResponse deny layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlRelease { entry: None }.encode_payload().unwrap(),
            vec![0x00],
            "v2 ControlRelease wire layout changed: bump the protocol version"
        );
    }
}
