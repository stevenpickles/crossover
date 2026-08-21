//! Display topology wire messages (docs/PROTOCOL.md §6.2, ADR 0018):
//! `MonitorTopology` and `LayoutSync`, both CONTROL class, both base
//! protocol at v4 — no feature bit, because a v3 peer is already excluded
//! at `Hello` by the `entry` shape change ([`crate::control`]).
//!
//! `MonitorTopology` states a fact about the sender: its own live
//! monitors, in its own local coordinates. `LayoutSync` states the drawn
//! arrangement, which describes *both* machines of a session's pair.
//! Neither message decides which two machines a layout may name — that is
//! this session's pair, and only `crossover-core` knows it. See "What this
//! module does not check" below.
//!
//! # Reuse, not a wire copy
//!
//! Every shape here rides `crossover-topology`'s validated model types
//! directly rather than re-declaring them:
//!
//! - A monitor id is [`crossover_topology::MonitorId`] — a smart
//!   constructor that cannot exist unvalidated, so a decoded id is already
//!   printable ASCII within [`crossover_topology::MAX_MONITOR_ID_BYTES`]
//!   bytes with no check of our own to write.
//! - A device identity is [`crossover_topology::DeviceId`] — sixteen raw
//!   bytes, postcard-encoded with **no length prefix** (unlike
//!   `uuid::Uuid`'s length-prefixed form). The goldens below pin this
//!   deliberately.
//! - A rectangle is [`crossover_topology::LayoutRect`], and its bounds are
//!   checked through [`crossover_topology::LayoutRect::check_bounds`] —
//!   never re-implemented here.
//! - [`LayoutSync::monitors`] is `Vec<`[`crossover_topology::PlacedMonitor`]`>`
//!   directly: the model's own (device, id, rect) triple, whose `Deserialize`
//!   already runs `check_bounds` as part of decoding it. This module's own
//!   [`LayoutSync::validate`] repeats that check anyway, because
//!   `PlacedMonitor`'s fields are public — a hand-built value can carry an
//!   unchecked rectangle that only decode-time `Deserialize` would catch,
//!   and this crate validates encode and decode alike (below).
//!
//! # Validate on encode and decode
//!
//! The same discipline [`crate::hello`] and [`crate::control`] use: a bound
//! this crate would reject from a peer must be impossible to send. Every
//! `encode_payload` calls `validate` first; every `decode_payload` decodes
//! strictly (no trailing bytes) and then calls `validate` again, so the
//! bound holds even against a peer that skips its own validation.
//!
//! # What this module does not check
//!
//! Wire validation is the malformed/well-formed split docs/PROTOCOL.md §6.2
//! and §7 draw, and this module implements only the **malformed** half —
//! the counts, the per-monitor rectangle and scale bounds, and the
//! structural rule that a `LayoutSync` names at most two distinct devices.
//! It deliberately does **not** check:
//!
//! - **Session-pair membership** — whether the device(s) named are this
//!   session's actual pair, or whether `LayoutSync::origin` is one of them.
//!   Only `crossover-core` knows the session's identities.
//! - **Overlap**, or any other rule that needs the whole arrangement
//!   compared against known geometry.
//! - **The full `Layout::validate`** (`crossover_topology::Layout::new`),
//!   which folds in both of the above plus "both machines present". A
//!   `LayoutSync` that passes this module's `validate` is well-formed; core
//!   still runs it through `Layout::from_raw`/`Layout::new` against the
//!   session's `DevicePair` before adopting it (docs/PROTOCOL.md §6.2: a
//!   well-formed but semantically impossible layout is rejected and
//!   logged, never adopted, but must not cost a healthy session its first
//!   frame).

use serde::{Deserialize, Serialize};

use crossover_topology::{DeviceId, LayoutRect, MonitorId, PlacedMonitor, bounded_seq};
pub use crossover_topology::{
    MAX_LAYOUT_MONITORS, MAX_MONITORS_PER_MACHINE, MAX_SCALE_PERCENT, MIN_SCALE_PERCENT,
};

use crate::ProtocolError;
use crate::{decode_strict, encode};

/// Map a [`crossover_topology::LayoutError`] onto this crate's error type.
/// The `Display` a `LayoutError` produces already names the rule and the
/// monitor or machine at fault (`crossover-topology`'s own diagnostic
/// discipline), so this is a wrap, not a rewrite.
fn malformed_layout(error: &crossover_topology::LayoutError) -> ProtocolError {
    ProtocolError::Malformed {
        reason: error.to_string(),
    }
}

/// One monitor as [`MonitorTopology`] reports it: the sender's own local
/// rectangle plus the display scale that seeds the editor's to-scale
/// drawing (ADR 0018). Scale never enters crossing mapping — see
/// [`MonitorTopology`]'s docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorReport {
    /// The monitor's platform-supplied identity, validated by construction
    /// ([`crossover_topology::MonitorId`]).
    pub id: MonitorId,
    /// Where the sender's own OS places it, in the sender's own local
    /// coordinates. Bounds are checked by [`MonitorReport::validate`]
    /// through [`LayoutRect::check_bounds`], not by decoding this struct
    /// alone — `LayoutRect`'s fields are public and its `Deserialize` does
    /// not enforce them.
    pub rect: LayoutRect,
    /// Display scale, in percent (100 = unscaled),
    /// [`MIN_SCALE_PERCENT`]..=[`MAX_SCALE_PERCENT`]. A seeding input for
    /// the editor's DIP sizing only — it never enters crossing mapping,
    /// which stays proportional through the drawn geometry (ADR 0018).
    pub scale_percent: u16,
}

impl MonitorReport {
    /// Semantic validation: the rectangle satisfies
    /// [`LayoutRect::check_bounds`], and `scale_percent` is within range.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] naming the monitor and the broken rule.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.rect
            .check_bounds()
            .map_err(|violation| ProtocolError::Malformed {
                reason: format!("monitor {}: {violation}", self.id),
            })?;
        if !(MIN_SCALE_PERCENT..=MAX_SCALE_PERCENT).contains(&self.scale_percent) {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "monitor {} scale_percent {} outside {MIN_SCALE_PERCENT}..={MAX_SCALE_PERCENT}",
                    self.id, self.scale_percent
                ),
            });
        }
        Ok(())
    }
}

/// States a fact about the sender: its own live monitors, in its own local
/// coordinates (message type 17, CONTROL class, docs/PROTOCOL.md §6.2,
/// ADR 0018).
///
/// Sent after `Hello` and again whenever the local display configuration
/// changes. It is not an arrangement and never changes crossing behaviour
/// on its own — it is what lets either machine's editor draw the peer's
/// screens to scale, and what lets layout adoption tell a real monitor id
/// from a fiction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorTopology {
    /// The sender's monitors, `1..=`[`MAX_MONITORS_PER_MACHINE`] of them.
    /// A machine that genuinely enumerates more does not truncate this
    /// list — it refuses to send `MonitorTopology` at all (ADR 0018), a
    /// decision `crossover-core` makes; this type only bounds what is
    /// already here.
    ///
    /// The cap is enforced **while this list decodes**
    /// ([`bounded_seq`]), not after: a frame claiming far more elements
    /// than any legitimate `MonitorTopology` never gets the chance to
    /// materialize them (CLAUDE.md: "validate lengths before allocating").
    #[serde(deserialize_with = "bounded_seq::<_, _, MAX_MONITORS_PER_MACHINE>")]
    pub monitors: Vec<MonitorReport>,
}

impl MonitorTopology {
    /// Semantic validation, applied on both encode and decode: the count is
    /// `1..=`[`MAX_MONITORS_PER_MACHINE`], every monitor is individually
    /// valid ([`MonitorReport::validate`]), and no id repeats.
    ///
    /// This does **not** reuse [`crossover_topology::check_structure`]:
    /// that function groups by [`DeviceId`], and a `MonitorTopology`
    /// monitor has none — every one of them is implicitly the sender's.
    /// Wrapping each in a synthetic, shared device would make the shapes
    /// line up, but at the cost of a diagnostic that names a device this
    /// message never carried; a message with no device axis is exactly the
    /// shape that helper does not fit, so this keeps its own small loop
    /// instead (docs/PROTOCOL.md §6.2's "reused, not duplicated" is about
    /// the rules, not a literal function call at any cost to their
    /// diagnostics).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] naming the broken rule.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.monitors.is_empty() {
            return Err(ProtocolError::Malformed {
                reason: "MonitorTopology has no monitors".to_owned(),
            });
        }
        if self.monitors.len() > MAX_MONITORS_PER_MACHINE {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "MonitorTopology has {} monitors, over the {MAX_MONITORS_PER_MACHINE} maximum",
                    self.monitors.len()
                ),
            });
        }
        let mut seen: Vec<&MonitorId> = Vec::with_capacity(self.monitors.len());
        for monitor in &self.monitors {
            monitor.validate()?;
            if seen.contains(&&monitor.id) {
                return Err(ProtocolError::Malformed {
                    reason: format!("MonitorTopology lists monitor {} twice", monitor.id),
                });
            }
            seen.push(&monitor.id);
        }
        Ok(())
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
        encode(self)
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or invalid payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "MonitorTopology")?;
        message.validate()?;
        Ok(message)
    }
}

/// States the drawn arrangement, which describes *both* machines of a
/// session's pair (message type 18, CONTROL class, docs/PROTOCOL.md §6.2,
/// ADR 0018).
///
/// Sent after `Hello` when the sender holds an explicit layout, and on
/// every edit. A layout that exists only implicitly — the compatibility
/// layout a v1 config or a `--left`/`--right` flag produces — is never
/// sent. See the module docs for exactly what wire validation checks and
/// what it leaves to `crossover-core`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSync {
    /// The arrangement's revision. Newest wins (ADR 0018); the ordering key
    /// is `(revision, origin)`, which is `crossover-core`'s to apply.
    pub revision: u64,
    /// The device that drew this arrangement — the tiebreak when two edits
    /// claim the same revision. Not checked here against the session's
    /// pair; see the module docs.
    pub origin: DeviceId,
    /// The placed monitors, `1..=`[`MAX_LAYOUT_MONITORS`] of them, at most
    /// [`MAX_MONITORS_PER_MACHINE`] per device and at most two distinct
    /// devices. Reuses [`PlacedMonitor`] directly — the model's own
    /// (device, id, rect) triple.
    ///
    /// The [`MAX_LAYOUT_MONITORS`] cap is enforced **while this list
    /// decodes** ([`bounded_seq`]), not after — see
    /// [`MonitorTopology::monitors`]'s docs for why that matters.
    #[serde(deserialize_with = "bounded_seq::<_, _, MAX_LAYOUT_MONITORS>")]
    pub monitors: Vec<PlacedMonitor>,
}

impl LayoutSync {
    /// Semantic validation, applied on both encode and decode. See the
    /// module docs for the malformed/well-formed split this stops at.
    ///
    /// The empty/cap/rectangle-bounds/per-machine-cap/duplicate-id rules
    /// are [`crossover_topology::check_structure`]'s — one definition,
    /// shared with [`crossover_topology::Layout::new`] — leaving only the
    /// rule specific to this wire message: at most two distinct devices,
    /// which needs no knowledge of *which* two devices this session's pair
    /// is (that check is core's, once it has the session's actual pair;
    /// see the module docs).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] naming the broken rule: a count past
    /// its cap, an empty list, an out-of-bounds rectangle, more than two
    /// distinct devices, a per-machine count past its cap, or a repeated
    /// `(device, id)` pair.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        crossover_topology::check_structure(&self.monitors)
            .map_err(|error| malformed_layout(&error))?;

        // At most two distinct devices — a purely structural rule that
        // needs no knowledge of *which* two devices this session's pair
        // is, so it stays here rather than in `check_structure`, which
        // groups by however many devices are actually present.
        let mut devices: Vec<DeviceId> = Vec::with_capacity(2);
        for monitor in &self.monitors {
            if !devices.contains(&monitor.device) {
                if devices.len() == 2 {
                    return Err(ProtocolError::Malformed {
                        reason: "LayoutSync names more than two devices".to_owned(),
                    });
                }
                devices.push(monitor.device);
            }
        }

        Ok(())
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
        encode(self)
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or invalid payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "LayoutSync")?;
        message.validate()?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use crossover_topology::{DeviceId, LayoutRect, MonitorId, PlacedMonitor};

    use super::{
        LayoutSync, MAX_LAYOUT_MONITORS, MAX_MONITORS_PER_MACHINE, MAX_SCALE_PERCENT,
        MIN_SCALE_PERCENT, MonitorReport, MonitorTopology,
    };
    use crate::ProtocolError;
    use crate::hello::MessageType;

    const LOCAL: DeviceId = DeviceId::from_bytes([0x11; 16]);
    const PEER: DeviceId = DeviceId::from_bytes([0x22; 16]);
    const THIRD: DeviceId = DeviceId::from_bytes([0x33; 16]);

    fn rect(x: i32, y: i32, width: u32, height: u32) -> LayoutRect {
        LayoutRect {
            x,
            y,
            width,
            height,
        }
    }

    fn report(id: &str, x: i32, y: i32, width: u32, height: u32, scale: u16) -> MonitorReport {
        MonitorReport {
            id: MonitorId::new(id).unwrap(),
            rect: rect(x, y, width, height),
            scale_percent: scale,
        }
    }

    fn placed(
        device: DeviceId,
        id: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> PlacedMonitor {
        PlacedMonitor {
            device,
            id: MonitorId::new(id).unwrap(),
            rect: rect(x, y, width, height),
        }
    }

    // ---- message type wiring ----------------------------------------

    #[test]
    fn message_types_17_and_18_are_monitor_topology_and_layout_sync() {
        assert_eq!(
            MessageType::from_wire(17),
            Some(MessageType::MonitorTopology)
        );
        assert_eq!(MessageType::from_wire(18), Some(MessageType::LayoutSync));
        assert_eq!(MessageType::MonitorTopology.wire(), 17);
        assert_eq!(MessageType::LayoutSync.wire(), 18);
    }

    // ---- round trips ---------------------------------------------------

    #[test]
    fn monitor_topology_round_trips() {
        for topology in [
            MonitorTopology {
                monitors: vec![report(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 100)],
            },
            MonitorTopology {
                monitors: vec![
                    report(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 100),
                    report(r"\\.\DISPLAY2", 1920, 0, 2560, 1440, 150),
                ],
            },
            // Boundary scale values, both ends.
            MonitorTopology {
                monitors: vec![report("A", 0, 0, 100, 100, MIN_SCALE_PERCENT)],
            },
            MonitorTopology {
                monitors: vec![report("B", 0, 0, 100, 100, MAX_SCALE_PERCENT)],
            },
        ] {
            let encoded = topology.encode_payload().unwrap();
            assert_eq!(MonitorTopology::decode_payload(&encoded).unwrap(), topology);
        }
    }

    #[test]
    fn layout_sync_round_trips() {
        for sync in [
            LayoutSync {
                revision: 1,
                origin: LOCAL,
                monitors: vec![
                    placed(LOCAL, r"\\.\DISPLAY1", 0, 0, 1920, 1080),
                    placed(PEER, r"\\.\DISPLAY1", 1920, 0, 1920, 1080),
                ],
            },
            LayoutSync {
                revision: u64::MAX,
                origin: PEER,
                monitors: vec![
                    placed(LOCAL, "A", 0, 0, 1000, 1000),
                    placed(LOCAL, "B", 0, 1000, 1000, 1000),
                    placed(PEER, "C", 1000, 500, 1000, 1000),
                ],
            },
        ] {
            let encoded = sync.encode_payload().unwrap();
            assert_eq!(LayoutSync::decode_payload(&encoded).unwrap(), sync);
        }
    }

    // ---- golden wire snapshots (ADR 0001) ------------------------------
    //
    // Postcard encodes: `u16`/`u32`/`u64` as unsigned LEB128; `i32` as
    // zigzag then LEB128; `String` as a LEB128 byte length then UTF-8
    // bytes (same shape for `MonitorId`, which serializes as a plain
    // string); `Vec<T>` as a LEB128 element count then the elements; and
    // `DeviceId` as sixteen **raw bytes with no length prefix** — the
    // documented difference from `uuid::Uuid`'s length-prefixed form,
    // pinned here on purpose.

    #[test]
    fn golden_monitor_topology_single_monitor() {
        let topology = MonitorTopology {
            monitors: vec![report("A", 0, 0, 1920, 1080, 100)],
        };
        assert_eq!(
            topology.encode_payload().unwrap(),
            vec![
                0x01, // monitors: 1 element
                0x01, b'A', // id: 1-byte string "A"
                0x00, // x: 0 (zigzag)
                0x00, // y: 0 (zigzag)
                0x80, 0x0F, // width: 1920 (LEB128)
                0xB8, 0x08, // height: 1080 (LEB128)
                0x64, // scale_percent: 100
            ],
            "MonitorTopology wire layout changed: bump the protocol version"
        );
    }

    #[test]
    fn golden_monitor_topology_two_monitors_negative_coordinate() {
        let topology = MonitorTopology {
            monitors: vec![
                report("A", -1, 0, 100, 100, MIN_SCALE_PERCENT),
                report("BB", 1, -1, 200, 300, MAX_SCALE_PERCENT),
            ],
        };
        assert_eq!(
            topology.encode_payload().unwrap(),
            vec![
                0x02, // monitors: 2 elements
                0x01, b'A', // id: "A"
                0x01, // x: -1 (zigzag: -1 -> 1)
                0x00, // y: 0
                0x64, // width: 100
                0x64, // height: 100
                0x19, // scale_percent: 25 (MIN_SCALE_PERCENT)
                0x02, b'B', b'B', // id: "BB"
                0x02, // x: 1 (zigzag: 1 -> 2)
                0x01, // y: -1 (zigzag: -1 -> 1)
                0xC8, 0x01, // width: 200
                0xAC, 0x02, // height: 300
                0xF4, 0x03, // scale_percent: 500 (MAX_SCALE_PERCENT), LEB128
            ],
            "MonitorTopology wire layout changed: bump the protocol version"
        );
    }

    #[test]
    fn golden_layout_sync_two_devices() {
        let sync = LayoutSync {
            revision: 7,
            origin: LOCAL,
            monitors: vec![
                placed(LOCAL, "A", 0, 0, 1920, 1080),
                placed(PEER, "B", 1920, 0, 1920, 1080),
            ],
        };
        let mut expected = vec![
            0x07, // revision: 7
        ];
        expected.extend_from_slice(&[0x11; 16]); // origin: 16 raw bytes, no length prefix
        expected.push(0x02); // monitors: 2 elements
        expected.extend_from_slice(&[0x11; 16]); // monitor 0 device: LOCAL, 16 raw bytes
        expected.extend_from_slice(&[0x01, b'A']); // monitor 0 id
        expected.extend_from_slice(&[0x00, 0x00]); // monitor 0 x, y
        expected.extend_from_slice(&[0x80, 0x0F]); // monitor 0 width: 1920
        expected.extend_from_slice(&[0xB8, 0x08]); // monitor 0 height: 1080
        expected.extend_from_slice(&[0x22; 16]); // monitor 1 device: PEER, 16 raw bytes
        expected.extend_from_slice(&[0x01, b'B']); // monitor 1 id
        expected.extend_from_slice(&[0x80, 0x1E]); // monitor 1 x: 1920 (zigzag: 1920 -> 3840)
        expected.push(0x00); // monitor 1 y: 0
        expected.extend_from_slice(&[0x80, 0x0F]); // monitor 1 width: 1920
        expected.extend_from_slice(&[0xB8, 0x08]); // monitor 1 height: 1080
        assert_eq!(
            sync.encode_payload().unwrap(),
            expected,
            "LayoutSync wire layout changed: bump the protocol version"
        );
    }

    #[test]
    fn golden_layout_sync_revision_zero_single_device() {
        let sync = LayoutSync {
            revision: 0,
            origin: PEER,
            monitors: vec![placed(LOCAL, "A", 0, 0, 1, 1)],
        };
        let mut expected = vec![0x00]; // revision: 0
        expected.extend_from_slice(&[0x22; 16]); // origin: PEER, 16 raw bytes
        expected.push(0x01); // monitors: 1 element
        expected.extend_from_slice(&[0x11; 16]); // device: LOCAL
        expected.extend_from_slice(&[0x01, b'A']); // id: "A"
        expected.extend_from_slice(&[0x00, 0x00]); // x, y: 0
        expected.extend_from_slice(&[0x01, 0x01]); // width, height: 1
        assert_eq!(
            sync.encode_payload().unwrap(),
            expected,
            "LayoutSync wire layout changed: bump the protocol version"
        );
    }

    // ---- malformed suite -------------------------------------------------

    #[test]
    fn empty_monitor_lists_are_rejected() {
        assert!(matches!(
            MonitorTopology { monitors: vec![] }.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        assert!(matches!(
            LayoutSync {
                revision: 0,
                origin: LOCAL,
                monitors: vec![],
            }
            .encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn monitor_topology_count_bound_and_bound_plus_one() {
        let at_cap = MonitorTopology {
            monitors: (0..MAX_MONITORS_PER_MACHINE)
                .map(|n| report(&format!("D{n}"), 0, 0, 100, 100, 100))
                .collect(),
        };
        assert!(at_cap.encode_payload().is_ok());

        let over_cap = MonitorTopology {
            monitors: (0..=MAX_MONITORS_PER_MACHINE)
                .map(|n| report(&format!("D{n}"), 0, 0, 100, 100, 100))
                .collect(),
        };
        assert!(matches!(
            over_cap.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        // Bypass encode-side validation to prove decode enforces the bound
        // independently. `MonitorTopology` has one field, so postcard's
        // positional encoding makes its bytes identical to the bare
        // `Vec<MonitorReport>`'s own — no shadow struct needed to build an
        // otherwise-unvalidated payload.
        let unvalidated = postcard::to_stdvec(&over_cap.monitors).unwrap();
        assert!(matches!(
            MonitorTopology::decode_payload(&unvalidated),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn layout_sync_count_bound_and_bound_plus_one() {
        // At the layout cap, split 16/16 across the two devices.
        let mut monitors: Vec<PlacedMonitor> = (0..MAX_MONITORS_PER_MACHINE)
            .map(|n| {
                placed(
                    LOCAL,
                    &format!("L{n}"),
                    i32::try_from(n).unwrap() * 200,
                    0,
                    100,
                    100,
                )
            })
            .collect();
        monitors.extend((0..MAX_MONITORS_PER_MACHINE).map(|n| {
            placed(
                PEER,
                &format!("P{n}"),
                i32::try_from(n).unwrap() * 200,
                500,
                100,
                100,
            )
        }));
        assert_eq!(monitors.len(), MAX_LAYOUT_MONITORS);
        let at_cap = LayoutSync {
            revision: 0,
            origin: LOCAL,
            monitors,
        };
        assert!(at_cap.encode_payload().is_ok());

        let mut over = at_cap.monitors.clone();
        over.push(placed(LOCAL, "OVERFLOW", 100_000, 100_000, 10, 10));
        assert_eq!(over.len(), MAX_LAYOUT_MONITORS + 1);
        let over_cap = LayoutSync {
            revision: 0,
            origin: LOCAL,
            monitors: over.clone(),
        };
        assert!(matches!(
            over_cap.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        // `LayoutSync`'s three fields encode, in postcard, exactly as the
        // matching tuple does — no shadow struct needed here either.
        let unvalidated = postcard::to_stdvec(&(0u64, LOCAL, over)).unwrap();
        assert!(matches!(
            LayoutSync::decode_payload(&unvalidated),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn coordinate_bound_and_bound_plus_one() {
        use crossover_topology::MAX_LAYOUT_COORDINATE;

        // Within bounds, both signs, is fine.
        assert!(
            MonitorTopology {
                monitors: vec![report(
                    "A",
                    -MAX_LAYOUT_COORDINATE,
                    MAX_LAYOUT_COORDINATE,
                    100,
                    100,
                    100
                )],
            }
            .encode_payload()
            .is_ok()
        );

        for coordinate in [MAX_LAYOUT_COORDINATE + 1, -MAX_LAYOUT_COORDINATE - 1] {
            assert!(matches!(
                MonitorTopology {
                    monitors: vec![report("A", coordinate, 0, 100, 100, 100)],
                }
                .encode_payload(),
                Err(ProtocolError::Malformed { .. })
            ));
            assert!(matches!(
                LayoutSync {
                    revision: 0,
                    origin: LOCAL,
                    monitors: vec![placed(LOCAL, "A", coordinate, 0, 100, 100)],
                }
                .encode_payload(),
                Err(ProtocolError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn zero_size_monitors_are_rejected() {
        for (width, height) in [(0, 100), (100, 0), (0, 0)] {
            assert!(matches!(
                MonitorTopology {
                    monitors: vec![report("A", 0, 0, width, height, 100)],
                }
                .encode_payload(),
                Err(ProtocolError::Malformed { .. })
            ));
            assert!(matches!(
                LayoutSync {
                    revision: 0,
                    origin: LOCAL,
                    monitors: vec![placed(LOCAL, "A", 0, 0, width, height)],
                }
                .encode_payload(),
                Err(ProtocolError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn extent_bound_and_bound_plus_one() {
        use crossover_topology::MAX_MONITOR_EXTENT;

        assert!(
            MonitorTopology {
                monitors: vec![report(
                    "A",
                    0,
                    0,
                    MAX_MONITOR_EXTENT,
                    MAX_MONITOR_EXTENT,
                    100
                )],
            }
            .encode_payload()
            .is_ok()
        );
        assert!(matches!(
            MonitorTopology {
                monitors: vec![report("A", 0, 0, MAX_MONITOR_EXTENT + 1, 100, 100)],
            }
            .encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn scale_bound_24_and_501_are_rejected() {
        assert!(matches!(
            MonitorTopology {
                monitors: vec![report("A", 0, 0, 100, 100, MIN_SCALE_PERCENT - 1)],
            }
            .encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        assert!(matches!(
            MonitorTopology {
                monitors: vec![report("A", 0, 0, 100, 100, MAX_SCALE_PERCENT + 1)],
            }
            .encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        // The boundaries themselves are accepted.
        assert!(
            MonitorTopology {
                monitors: vec![report("A", 0, 0, 100, 100, MIN_SCALE_PERCENT)],
            }
            .encode_payload()
            .is_ok()
        );
        assert!(
            MonitorTopology {
                monitors: vec![report("A", 0, 0, 100, 100, MAX_SCALE_PERCENT)],
            }
            .encode_payload()
            .is_ok()
        );
    }

    #[test]
    fn duplicate_monitor_ids_are_rejected() {
        assert!(matches!(
            MonitorTopology {
                monitors: vec![
                    report("A", 0, 0, 100, 100, 100),
                    report("A", 200, 0, 100, 100, 100),
                ],
            }
            .encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        // Duplicate within one device is rejected; the same id on two
        // *different* devices is fine (each machine's ids are its own).
        assert!(matches!(
            LayoutSync {
                revision: 0,
                origin: LOCAL,
                monitors: vec![
                    placed(LOCAL, "A", 0, 0, 100, 100),
                    placed(LOCAL, "A", 200, 0, 100, 100),
                ],
            }
            .encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        assert!(
            LayoutSync {
                revision: 0,
                origin: LOCAL,
                monitors: vec![
                    placed(LOCAL, "A", 0, 0, 100, 100),
                    placed(PEER, "A", 200, 0, 100, 100),
                ],
            }
            .encode_payload()
            .is_ok()
        );
    }

    #[test]
    fn more_than_two_devices_is_rejected() {
        assert!(matches!(
            LayoutSync {
                revision: 0,
                origin: LOCAL,
                monitors: vec![
                    placed(LOCAL, "A", 0, 0, 100, 100),
                    placed(PEER, "B", 200, 0, 100, 100),
                    placed(THIRD, "C", 400, 0, 100, 100),
                ],
            }
            .encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn truncated_payloads_never_panic_and_are_malformed() {
        let full = MonitorTopology {
            monitors: vec![report("DISPLAY1", 10, -10, 1920, 1080, 125)],
        }
        .encode_payload()
        .unwrap();
        for cut in 0..full.len() {
            assert!(
                matches!(
                    MonitorTopology::decode_payload(&full[..cut]),
                    Err(ProtocolError::Malformed { .. })
                ),
                "truncation at {cut} bytes was not rejected"
            );
        }

        let full = LayoutSync {
            revision: 42,
            origin: LOCAL,
            monitors: vec![
                placed(LOCAL, "A", 0, 0, 1920, 1080),
                placed(PEER, "B", 1920, 0, 1920, 1080),
            ],
        }
        .encode_payload()
        .unwrap();
        for cut in 0..full.len() {
            assert!(
                matches!(
                    LayoutSync::decode_payload(&full[..cut]),
                    Err(ProtocolError::Malformed { .. })
                ),
                "truncation at {cut} bytes was not rejected"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = MonitorTopology {
            monitors: vec![report("A", 0, 0, 100, 100, 100)],
        }
        .encode_payload()
        .unwrap();
        bytes.push(0xAA);
        assert!(matches!(
            MonitorTopology::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));

        let mut bytes = LayoutSync {
            revision: 1,
            origin: LOCAL,
            monitors: vec![
                placed(LOCAL, "A", 0, 0, 100, 100),
                placed(PEER, "B", 200, 0, 100, 100),
            ],
        }
        .encode_payload()
        .unwrap();
        bytes.push(0xAA);
        assert!(matches!(
            LayoutSync::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn garbage_bytes_never_panic() {
        for pattern in [[0xFFu8; 24], [0x00; 24], [0x7F; 24]] {
            assert!(matches!(
                MonitorTopology::decode_payload(&pattern),
                Err(ProtocolError::Malformed { .. })
            ));
            assert!(matches!(
                LayoutSync::decode_payload(&pattern),
                Err(ProtocolError::Malformed { .. })
            ));
        }
    }

    proptest::proptest! {
        /// Arbitrary bytes are a typed rejection or a value that itself
        /// satisfies every bound — never a panic (NFR-1).
        #[test]
        fn arbitrary_bytes_never_panic_monitor_topology(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            if let Ok(topology) = MonitorTopology::decode_payload(&bytes) {
                proptest::prop_assert!(!topology.monitors.is_empty());
                proptest::prop_assert!(topology.monitors.len() <= MAX_MONITORS_PER_MACHINE);
                for monitor in &topology.monitors {
                    proptest::prop_assert!(monitor.rect.check_bounds().is_ok());
                    proptest::prop_assert!(
                        (MIN_SCALE_PERCENT..=MAX_SCALE_PERCENT).contains(&monitor.scale_percent)
                    );
                }
            }
        }

        #[test]
        fn arbitrary_bytes_never_panic_layout_sync(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            if let Ok(sync) = LayoutSync::decode_payload(&bytes) {
                proptest::prop_assert!(!sync.monitors.is_empty());
                proptest::prop_assert!(sync.monitors.len() <= MAX_LAYOUT_MONITORS);
                for monitor in &sync.monitors {
                    proptest::prop_assert!(monitor.rect.check_bounds().is_ok());
                }
            }
        }
    }
}
