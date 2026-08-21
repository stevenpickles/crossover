//! Protocol version negotiation (docs/PROTOCOL.md §3).
//!
//! A session runs at the highest mutually supported version; disjoint
//! ranges terminate the session — there is no silent downgrade below
//! either side's minimum (docs/SECURITY.md invariant 4).

use crate::ProtocolError;

/// The highest protocol version this build speaks.
///
/// v2 (Phase 5): the control-transfer messages carry a normalized edge
/// crossing position (ADR 0009), an incompatible layout change from v1.
///
/// v3 (Phase 7): `ClipboardOffer` carries an optional `FileDescriptor`
/// (ADR 0015). Unlike ADR 0014's additions — a new message type and an
/// appended enum variant, both reachable only after a feature bit is
/// negotiated — this one appends a field to a message that already
/// travels, so *every* offer gains a byte and no feature bit can hide it
/// from a peer that predates the change. A v2 peer would read the extra
/// byte as trailing data and fail the payload, which by
/// docs/PROTOCOL.md §7 is fatal to the session. The bump turns that into
/// a clean, diagnosable refusal at `Hello` instead.
///
/// v4 (Phase 8, [ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)):
/// `ControlRequest.entry` and `ControlRelease.entry` change shape, from
/// `Option<u16>` to `Option<EntryPoint>` (docs/PROTOCOL.md §6.1) — a
/// structural change to messages that already travel between every pair
/// of peers, exactly the v2→v3 case above and ADR 0017's rule applied
/// unchanged: no feature bit can hide a layout change to a message that
/// is not gated by one, so it is a version bump rather than a bit. A v3
/// peer would read `EntryPoint`'s extra fields as trailing data (or
/// misread `Option<u16>`'s single byte as `EntryPoint`'s multi-field
/// encoding) and fail the payload — fatal per docs/PROTOCOL.md §7 — so
/// the bump turns that into a clean, diagnosable refusal at `Hello`
/// instead.
pub const PROTOCOL_VERSION: u16 = 4;

/// The lowest protocol version this build accepts. Each bump has been an
/// incompatible layout change (v1's control messages cannot be decoded by
/// v2; v2's offers cannot be decoded by v3; v3's `entry` cannot be decoded
/// by v4), and peers are deployed in lockstep, so the floor tracks the
/// ceiling rather than carrying compatibility code for a version nobody
/// runs.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u16 = 4;

/// An inclusive range of supported protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    /// Lowest acceptable version.
    pub min: u16,
    /// Highest supported version.
    pub max: u16,
}

impl VersionRange {
    /// The range this build supports.
    pub const CURRENT: Self = Self {
        min: MIN_SUPPORTED_PROTOCOL_VERSION,
        max: PROTOCOL_VERSION,
    };
}

/// Pick the session version: the highest version inside both ranges.
///
/// # Errors
///
/// [`ProtocolError::NoCommonVersion`] when the ranges do not intersect
/// (or either range is inverted — impossible states fail closed).
pub fn negotiate(local: VersionRange, peer: VersionRange) -> Result<u16, ProtocolError> {
    let highest = local.max.min(peer.max);
    let lowest = local.min.max(peer.min);
    if lowest <= highest {
        Ok(highest)
    } else {
        Err(ProtocolError::NoCommonVersion {
            local_min: local.min,
            local_max: local.max,
            peer_min: peer.min,
            peer_max: peer.max,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{PROTOCOL_VERSION, VersionRange, negotiate};
    use crate::ProtocolError;

    fn range(min: u16, max: u16) -> VersionRange {
        VersionRange { min, max }
    }

    #[test]
    fn identical_ranges_pick_that_version() {
        assert_eq!(
            negotiate(VersionRange::CURRENT, VersionRange::CURRENT).unwrap(),
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn overlap_picks_highest_mutual_version() {
        assert_eq!(negotiate(range(1, 3), range(2, 5)).unwrap(), 3);
        assert_eq!(negotiate(range(2, 5), range(1, 3)).unwrap(), 3);
        // A newer peer that still accepts our best: our best wins.
        assert_eq!(negotiate(range(1, 2), range(2, 9)).unwrap(), 2);
    }

    #[test]
    fn disjoint_ranges_fail_with_both_ranges_reported() {
        let err = negotiate(range(1, 2), range(3, 4)).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::NoCommonVersion {
                local_min: 1,
                local_max: 2,
                peer_min: 3,
                peer_max: 4,
            }
        ));
        // Symmetric case.
        assert!(negotiate(range(3, 4), range(1, 2)).is_err());
    }

    #[test]
    fn inverted_ranges_fail_closed() {
        assert!(negotiate(range(5, 1), range(1, 5)).is_err());
    }
}
