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

/// Bound on [`EntryPoint::monitor`] (ADR 0018, docs/PROTOCOL.md §6.1/§8):
/// printable ASCII, twice Windows' `CCHDEVICENAME`. This is the same
/// constant `MonitorTopology`/`LayoutSync` will share once a later branch
/// introduces the `crossover-topology` crate (ADR 0018); this crate does
/// not yet depend on it, so the bound lives here for the one field that
/// needs it today. Once this crate depends on `crossover-topology`,
/// replace this definition with a re-export of that crate's constant (or,
/// if both must exist independently for some reason, a
/// `const _: () = assert!(...)` pinning them equal) rather than trusting
/// two hand-kept numbers to agree.
pub const MAX_MONITOR_ID_BYTES: usize = 64;

/// A monitor edge ([`EntryPoint::edge`], ADR 0018, docs/PROTOCOL.md §6.1).
///
/// `Top` and `Bottom` have no producer in this phase: the side model this
/// branch adapts still knows only `Left`/`Right` (ADR 0009). They are part
/// of the v4 wire shape now, ahead of the drawn-layout branch that will
/// send them, so a receiver never needs a second version bump to
/// recognize them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Edge {
    /// The left edge of a monitor.
    Left = 0,
    /// The right edge of a monitor.
    Right = 1,
    /// The top edge of a monitor.
    Top = 2,
    /// The bottom edge of a monitor.
    Bottom = 3,
}

/// Where a crossing lands, in the receiver's terms (ADR 0018,
/// docs/PROTOCOL.md §6.1): which monitor, which of its edges, and how far
/// along — replacing v3's bare fraction now that a machine can have more
/// than one crossing edge.
///
/// **Transitional (feature/147).** The side-model sender — today's code,
/// still one linked edge pair (ADR 0009) — does not yet know a
/// destination monitor id; that arrives only once a later branch lands
/// `MonitorTopology` and `LayoutSync` and fills [`EntryPoint::monitor`]
/// with a real device string. Until then `monitor` travels **empty**, and
/// an empty `monitor` is valid and means "unaddressed": the receiver
/// places the cursor against its own desktop-bounds edge matching `edge`,
/// with `fraction` taken against those bounds. This is not a special case
/// bolted on for the transition — it is *exactly* ADR 0018's degraded
/// fallback for a monitor id the receiver does not recognize
/// (docs/PROTOCOL.md §6.1's "cannot honour" clause), reached deliberately
/// here, on every crossing, rather than by mismatch. A later branch simply
/// starts filling `monitor` with real ids, at which point most crossings
/// stop taking this path rather than the path itself changing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPoint {
    /// The destination monitor's id, or empty for "unaddressed" (above).
    /// At most [`MAX_MONITOR_ID_BYTES`] bytes of printable ASCII —
    /// validated on encode and decode like every other bounded field, and
    /// satisfied vacuously by the empty string.
    pub monitor: String,
    /// Which of that monitor's edges the cursor arrives on.
    pub edge: Edge,
    /// Normalized position along the edge (ADR 0009), unchanged: `0` at
    /// its start — the smaller coordinate on the perpendicular axis, top
    /// for a Left/Right edge, left for a Top/Bottom edge — `u16::MAX` at
    /// its end. Resolution- and DPI-independent — the grantee maps it
    /// through its own geometry.
    pub fraction: u16,
    /// The layout revision the sender derived this from. `0` when there is
    /// no real layout (the transitional case above); a receiver holding a
    /// different revision treats the entry point as unaddressed too (ADR
    /// 0018) — expected, and brief, during an edit's propagation window.
    pub layout_revision: u64,
}

impl EntryPoint {
    /// Build an "unaddressed" entry point — empty monitor, revision `0` —
    /// the transitional case the type docs above explain in full. The one
    /// constructor for it, so that reading is stated once rather than
    /// wherever a crossing gets wrapped.
    #[must_use]
    pub fn unaddressed(edge: Edge, fraction: u16) -> Self {
        Self {
            monitor: String::new(),
            edge,
            fraction,
            layout_revision: 0,
        }
    }

    /// Semantic validation, applied on both encode and decode — the
    /// discipline every wire message in this crate follows: a bound we
    /// would reject from a peer must be impossible to send.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for a monitor id over
    /// [`MAX_MONITOR_ID_BYTES`] bytes or containing a non-printable-ASCII
    /// byte. The empty string passes both checks vacuously — see the type
    /// docs for what that means.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.monitor.len() > MAX_MONITOR_ID_BYTES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "entry point monitor id is {} bytes, over the \
                     {MAX_MONITOR_ID_BYTES}-byte maximum",
                    self.monitor.len()
                ),
            });
        }
        if let Some(byte) = self.monitor.bytes().find(|b| !(0x20..=0x7E).contains(b)) {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "entry point monitor id contains non-printable-ASCII byte 0x{byte:02X}"
                ),
            });
        }
        Ok(())
    }
}

/// Ask the peer for control: "my input becomes yours to apply."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    /// Requester-assigned, monotonic per session; echoed by the
    /// response so a late answer cannot be mistaken for a current one.
    pub request_id: u64,
    /// Where the cursor crossed, in the receiver's terms (ADR 0018,
    /// docs/PROTOCOL.md §6.1), when the request came from an edge
    /// crossing. `None` for an explicit (console) request, which places
    /// no cursor. See [`EntryPoint`]'s docs for the transitional
    /// empty-`monitor` reading this phase relies on.
    pub entry: Option<EntryPoint>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRelease {
    /// Where the reclaiming cursor left, in the receiver's terms (ADR
    /// 0018, docs/PROTOCOL.md §6.1), when the release is an edge return
    /// from the controlled side (ADR 0009). `None` for an explicit
    /// hand-back or console revoke, which places no cursor. See
    /// [`EntryPoint`]'s docs for the transitional empty-`monitor` reading
    /// this phase relies on.
    pub entry: Option<EntryPoint>,
}

impl ControlRequest {
    /// Semantic validation, applied on both encode and decode: an
    /// [`EntryPoint`], if present, must itself be valid.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from [`EntryPoint::validate`].
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.entry.as_ref().map_or(Ok(()), EntryPoint::validate)
    }

    /// Encode the payload (postcard, ADR 0001). Validates first: this
    /// crate never sends what it would refuse to receive.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or invalid payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "ControlRequest")?;
        message.validate()?;
        Ok(message)
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
    /// Semantic validation, applied on both encode and decode: an
    /// [`EntryPoint`], if present, must itself be valid.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from [`EntryPoint::validate`].
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.entry.as_ref().map_or(Ok(()), EntryPoint::validate)
    }

    /// Encode the payload (postcard, ADR 0001). Validates first: this
    /// crate never sends what it would refuse to receive.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] if serialization fails.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        postcard::to_stdvec(self).map_err(|e| ProtocolError::Encode {
            reason: e.to_string(),
        })
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or invalid payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "ControlRelease")?;
        message.validate()?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ControlRelease, ControlRequest, ControlResponse, ControlVerdict, DenyReason, Edge,
        EntryPoint, MAX_MONITOR_ID_BYTES,
    };
    use crate::ProtocolError;

    /// An [`EntryPoint`] for tests that only care about a distinct,
    /// round-trippable value — most of them, since the engine layer
    /// (`crossover-core`) is what gives the fields real meaning.
    fn entry_point(monitor: &str, edge: Edge, fraction: u16, layout_revision: u64) -> EntryPoint {
        EntryPoint {
            monitor: monitor.to_owned(),
            edge,
            fraction,
            layout_revision,
        }
    }

    fn sample_entries() -> Vec<Option<EntryPoint>> {
        vec![
            None,
            // Unaddressed (transitional, feature/147): via the dedicated
            // constructor, exercising it directly.
            Some(EntryPoint::unaddressed(Edge::Left, 0)),
            Some(EntryPoint::unaddressed(Edge::Top, 32_768)),
            // A real id, once a later branch starts sending one.
            Some(entry_point("DISPLAY1", Edge::Bottom, 1, 7)),
            // Boundary fraction.
            Some(EntryPoint::unaddressed(Edge::Right, u16::MAX)),
        ]
    }

    #[test]
    fn request_and_response_round_trip() {
        for entry in sample_entries() {
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
        for entry in sample_entries() {
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
        // A release is `Option<EntryPoint>`: `0x00` is a valid `None`, but
        // a trailing byte past it is not (strict decode, no padding).
        assert!(matches!(
            ControlRelease::decode_payload(&[0x00, 0xFF]),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Truncating a valid `Some(EntryPoint)` payload anywhere — mid
    /// monitor id, mid edge, mid fraction, mid revision — must never
    /// panic and must always be malformed (NFR-1).
    #[test]
    fn truncated_entry_points_are_malformed_never_panicking() {
        let full = ControlRequest {
            request_id: 1,
            entry: Some(entry_point("DISPLAY1", Edge::Bottom, u16::MAX, 7)),
        }
        .encode_payload()
        .unwrap();
        for cut in 1..full.len() {
            assert!(
                matches!(
                    ControlRequest::decode_payload(&full[..cut]),
                    Err(ProtocolError::Malformed { .. })
                ),
                "truncation at {cut} bytes was not rejected"
            );
        }
    }

    /// Trailing bytes after a well-formed `Some(EntryPoint)` are rejected
    /// (strict decode, docs/PROTOCOL.md §2) — not silently ignored as
    /// padding.
    #[test]
    fn trailing_bytes_after_an_entry_point_are_malformed() {
        let mut bytes = ControlRelease {
            entry: Some(entry_point("M1", Edge::Left, 100, 1)),
        }
        .encode_payload()
        .unwrap();
        bytes.push(0xAA);
        assert!(matches!(
            ControlRelease::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// [`MAX_MONITOR_ID_BYTES`] is enforced on encode (a local defect
    /// cannot put an oversized id on the wire) and independently on
    /// decode (the bound holds even against a peer that skips
    /// `encode_payload`'s validation).
    #[test]
    fn oversized_monitor_ids_are_rejected_on_encode_and_decode() {
        let oversized = entry_point(&"x".repeat(MAX_MONITOR_ID_BYTES + 1), Edge::Left, 0, 0);
        let request = ControlRequest {
            request_id: 1,
            entry: Some(oversized.clone()),
        };
        assert!(matches!(
            request.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        // Bypass `encode_payload`'s validation to prove the decode side
        // enforces the bound independently (mirrors `Hello`'s device-name
        // test).
        let unvalidated = postcard::to_stdvec(&ControlRequest {
            request_id: 1,
            entry: Some(oversized),
        })
        .unwrap();
        assert!(matches!(
            ControlRequest::decode_payload(&unvalidated),
            Err(ProtocolError::Malformed { .. })
        ));

        // The boundary itself is accepted.
        let at_limit = entry_point(&"x".repeat(MAX_MONITOR_ID_BYTES), Edge::Left, 0, 0);
        let request = ControlRequest {
            request_id: 1,
            entry: Some(at_limit),
        };
        assert!(ControlRequest::decode_payload(&request.encode_payload().unwrap()).is_ok());
    }

    /// A non-printable-ASCII byte in the monitor id is rejected on encode
    /// and decode — the same discipline as an oversized one.
    #[test]
    fn non_printable_monitor_ids_are_rejected_on_encode_and_decode() {
        for &byte in &[0x00u8, 0x09, 0x0A, 0x1F, 0x7F, 0x80, 0xFF] {
            let monitor = String::from_utf8_lossy(&[b'D', byte, b'1']).into_owned();
            let bad = entry_point(&monitor, Edge::Right, 0, 0);
            let request = ControlRequest {
                request_id: 1,
                entry: Some(bad.clone()),
            };
            assert!(
                matches!(
                    request.encode_payload(),
                    Err(ProtocolError::Malformed { .. })
                ),
                "byte 0x{byte:02X} was accepted on encode"
            );

            let unvalidated = postcard::to_stdvec(&ControlRelease { entry: Some(bad) }).unwrap();
            assert!(
                matches!(
                    ControlRelease::decode_payload(&unvalidated),
                    Err(ProtocolError::Malformed { .. })
                ),
                "byte 0x{byte:02X} was accepted on decode"
            );
        }
    }

    /// An `edge` discriminant outside `0..=3` must be rejected, never
    /// guessed at (docs/PROTOCOL.md §7) — the same rule
    /// `garbage_and_padding_are_malformed` already exercises for
    /// [`ControlVerdict`].
    #[test]
    fn unknown_edge_discriminants_are_malformed() {
        // A well-formed `Some(EntryPoint)` for `request_id: 1`, with the
        // edge discriminant overwritten to a value no `Edge` variant uses.
        let valid = ControlRequest {
            request_id: 1,
            entry: Some(EntryPoint::unaddressed(Edge::Left, 1)),
        }
        .encode_payload()
        .unwrap();
        // Layout: request_id(1) | Some-tag(1) | monitor-len(1, 0x00) |
        // edge(1) | fraction | revision(1). The edge byte is index 3.
        assert_eq!(valid[3], Edge::Left as u8);
        for bogus_edge in [0x04u8, 0x05, 0xFF] {
            let mut bytes = valid.clone();
            bytes[3] = bogus_edge;
            assert!(
                matches!(
                    ControlRequest::decode_payload(&bytes),
                    Err(ProtocolError::Malformed { .. })
                ),
                "edge discriminant 0x{bogus_edge:02X} was accepted"
            );
        }
    }

    /// Golden wire snapshots (ADR 0001): schema change = version bump.
    /// v4 ([`ADR 0018`](../../../docs/adr/0018-drawn-display-topology.md)):
    /// `entry` moves from `Option<u16>` to `Option<EntryPoint>`.
    #[test]
    fn golden_wire_snapshots_v4() {
        // `entry: None` is unchanged from v2/v3 — `Option<T>`'s `None` tag
        // is one byte regardless of `T`.
        assert_eq!(
            ControlRequest {
                request_id: 1,
                entry: None,
            }
            .encode_payload()
            .unwrap(),
            vec![0x01, 0x00],
            "v4 ControlRequest wire layout changed: bump the protocol version"
        );
        // Unaddressed (empty monitor), boundary fraction 0.
        assert_eq!(
            ControlRequest {
                request_id: 1,
                entry: Some(EntryPoint::unaddressed(Edge::Left, 0)),
            }
            .encode_payload()
            .unwrap(),
            vec![
                0x01, // request_id
                0x01, // entry: Some
                0x00, // monitor: 0-byte string
                0x00, // edge: Left (variant 0)
                0x00, // fraction: 0
                0x00, // layout_revision: 0
            ],
            "v4 ControlRequest EntryPoint layout changed: bump the protocol version"
        );
        // A real monitor id, a non-Left/Right edge, and the top fraction
        // boundary.
        assert_eq!(
            ControlRequest {
                request_id: 2,
                entry: Some(entry_point("DISPLAY1", Edge::Bottom, u16::MAX, 7)),
            }
            .encode_payload()
            .unwrap(),
            vec![
                0x02, // request_id
                0x01, // entry: Some
                0x08, b'D', b'I', b'S', b'P', b'L', b'A', b'Y', b'1', // monitor
                0x03, // edge: Bottom (variant 3)
                0xFF, 0xFF, 0x03, // fraction: u16::MAX, LEB128
                0x07, // layout_revision
            ],
            "v4 ControlRequest EntryPoint layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlResponse {
                request_id: 1,
                verdict: ControlVerdict::Granted,
            }
            .encode_payload()
            .unwrap(),
            vec![0x01, 0x00],
            "ControlResponse wire layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlResponse {
                request_id: 2,
                verdict: ControlVerdict::Denied(DenyReason::AlreadyControlled),
            }
            .encode_payload()
            .unwrap(),
            vec![0x02, 0x01, 0x01],
            "ControlResponse deny layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlRelease { entry: None }.encode_payload().unwrap(),
            vec![0x00],
            "v4 ControlRelease wire layout changed: bump the protocol version"
        );
        assert_eq!(
            ControlRelease {
                entry: Some(EntryPoint::unaddressed(Edge::Right, u16::MAX)),
            }
            .encode_payload()
            .unwrap(),
            vec![
                0x01, // entry: Some
                0x00, // monitor: 0-byte string
                0x01, // edge: Right (variant 1)
                0xFF, 0xFF, 0x03, // fraction: u16::MAX, LEB128
                0x00, // layout_revision: 0
            ],
            "v4 ControlRelease EntryPoint layout changed: bump the protocol version"
        );
    }
}
