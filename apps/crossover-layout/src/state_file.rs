//! Reading the worker's state file (ADR 0018), and classifying its
//! freshness from the heartbeat.
//!
//! This module is deliberately split into an impure half — find the file,
//! measure it, read its bytes — and a pure half, [`classify`], that turns
//! those bytes (or their absence, or their being too large to read at all)
//! plus the current time into a [`StateFileStatus`]. The pure half is what
//! every fixture test below exercises; the impure half is a thin,
//! untested-by-design wrapper around `std::fs`. The file's location is
//! `paths::state_file_path`.

use std::io;
use std::path::Path;

use crossover_topology::{StateError, TopologyState, parse_state};

use crate::paths;

/// What reading the file from disk produced, before any parsing.
///
/// The seam between the impure half of this module and the pure half:
/// nothing here is fixture-testable (it touches the filesystem), and
/// nothing in [`classify`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawRead {
    /// The file existed, was within [`crossover_topology::state::MAX_STATE_FILE_BYTES`],
    /// and this many bytes came back.
    Present(Vec<u8>),
    /// No home directory, or the file does not exist there. Not an error —
    /// a worker that has never run has never written one.
    Absent,
    /// `stat` reported a size over [`crossover_topology::state::MAX_STATE_FILE_BYTES`],
    /// checked *before* `read` ran, so an enormous or truncation-bombed
    /// file costs a length comparison rather than an allocation — the same
    /// bound [`crossover_topology::parse_state`] documents, enforced one
    /// step earlier because this caller can ask the filesystem for the
    /// length without opening the file at all.
    TooLarge {
        /// The size `stat` reported.
        bytes: u64,
    },
    /// The file exists but could not be read (permissions, a locked handle
    /// mid-rename, …). Distinct from [`RawRead::Absent`] only for a
    /// diagnostic; [`classify`] folds both into the same *unreadable*
    /// verdict the editor shows the same empty state for.
    Io,
}

/// Read the state file's raw bytes, without interpreting them.
///
/// The size check runs first and costs only a `stat`: a file already over
/// the cap is refused before `read` ever allocates a buffer for it.
fn read_raw(path: &Path) -> RawRead {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return RawRead::Absent,
        Err(_) => return RawRead::Io,
    };
    let cap = u64::try_from(crossover_topology::state::MAX_STATE_FILE_BYTES).unwrap_or(u64::MAX);
    if metadata.len() > cap {
        return RawRead::TooLarge {
            bytes: metadata.len(),
        };
    }
    match std::fs::read(path) {
        Ok(bytes) => RawRead::Present(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => RawRead::Absent,
        Err(_) => RawRead::Io,
    }
}

/// Why a present file's bytes did not become a usable [`TopologyState`].
///
/// A hand-written [`core::fmt::Display`] rather than `thiserror`: ADR
/// 0019 fixes this crate's dependency graph at the GUI stack,
/// `crossover-topology`, and the `tracing` family (see its dated
/// amendment), so a further crate for one error enum's boilerplate is not
/// a trade this crate gets to make on its own.
#[derive(Debug)]
#[non_exhaustive]
pub enum UnreadableReason {
    /// The OS could not read the file at all (permissions, a handle held
    /// mid-rename by the worker's atomic writer).
    Io,
    /// The bytes are not valid UTF-8, so they are not JSON at all.
    NotUtf8,
    /// Present, UTF-8, and still refused: too large (measured by `stat`
    /// before the read, or — for a document that squeaked past `stat` but
    /// still exceeds the same bound once opened — by
    /// [`crossover_topology::parse_state`] itself), the wrong version, or
    /// malformed per that function's own bounds.
    Parse(StateError),
}

impl core::fmt::Display for UnreadableReason {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io => formatter.write_str("the state file could not be read"),
            Self::NotUtf8 => formatter.write_str("the state file is not valid UTF-8"),
            Self::Parse(source) => write!(formatter, "{source}"),
        }
    }
}

impl std::error::Error for UnreadableReason {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(source) => Some(source),
            Self::Io | Self::NotUtf8 => None,
        }
    }
}

/// The editor's classification of the state file, as of one read.
///
/// Exactly the four states ADR 0018/0019's contract calls for: the worker
/// publishes a heartbeat precisely so a reader can tell *fresh* from *stale*
/// rather than presenting an old arrangement as current, and *absent* from
/// *unreadable* is the same "no usable facts" verdict for two different
/// reasons, kept apart only for diagnostics.
#[derive(Debug)]
pub enum StateFileStatus {
    /// Parsed, and the heartbeat is within [`crossover_topology::HEARTBEAT_STALE_AFTER_MS`].
    Fresh(TopologyState),
    /// Parsed, but the heartbeat is older than that — the worker has
    /// probably stopped running, though its last report is still shown.
    Stale(TopologyState),
    /// No home directory, or no file there yet.
    Absent,
    /// A file is there but it could not be turned into a [`TopologyState`].
    Unreadable(UnreadableReason),
}

/// Turn a raw read plus the current time into a [`StateFileStatus`]. Pure:
/// every branch is a function of its two arguments alone, which is what
/// makes every case below fixture-driven rather than filesystem-driven.
#[must_use]
pub fn classify(raw: RawRead, now_unix_millis: u64) -> StateFileStatus {
    let bytes = match raw {
        RawRead::Absent => return StateFileStatus::Absent,
        RawRead::Io => return StateFileStatus::Unreadable(UnreadableReason::Io),
        RawRead::TooLarge { bytes } => {
            let reported = usize::try_from(bytes).unwrap_or(usize::MAX);
            return StateFileStatus::Unreadable(UnreadableReason::Parse(StateError::TooLarge {
                bytes: reported,
            }));
        }
        RawRead::Present(bytes) => bytes,
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return StateFileStatus::Unreadable(UnreadableReason::NotUtf8);
    };
    match parse_state(text) {
        Ok(state) if state.is_stale(now_unix_millis) => StateFileStatus::Stale(state),
        Ok(state) => StateFileStatus::Fresh(state),
        Err(error) => StateFileStatus::Unreadable(UnreadableReason::Parse(error)),
    }
}

/// Locate, read, and classify the state file against the current time.
///
/// The one impure entry point: everywhere else in this module (and every
/// test below) is a pure function over bytes that this call is the sole
/// producer of.
#[must_use]
pub fn read_state_file() -> StateFileStatus {
    let now = crossover_topology::now_unix_millis();
    let raw = match paths::state_file_path() {
        Some(path) => read_raw(&path),
        None => RawRead::Absent,
    };
    classify(raw, now)
}

#[cfg(test)]
mod tests {
    use super::{RawRead, StateFileStatus, UnreadableReason, classify, read_raw};
    use crate::test_support::Sandbox;
    use crossover_topology::{
        HEARTBEAT_STALE_AFTER_MS, LayoutRect, LayoutState, LiveMonitor, MachineState, MonitorId,
        PeerState, TOPOLOGY_STATE_VERSION, TopologyState, serialize_state,
    };

    const NOW: u64 = 1_766_000_000_000;

    fn live(id: &str) -> LiveMonitor {
        LiveMonitor {
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_percent: 100,
        }
    }

    fn document(written_at: u64) -> TopologyState {
        TopologyState {
            version: TOPOLOGY_STATE_VERSION,
            written_at,
            local: MachineState {
                device: crossover_topology::DeviceId::from_bytes([0x11; 16]),
                name: "desk-left".to_owned(),
                monitors: vec![live(r"\\.\DISPLAY1")],
            },
            peer: Some(PeerState {
                device: crossover_topology::DeviceId::from_bytes([0x22; 16]),
                name: "laptop".to_owned(),
                connected: true,
                last_seen: written_at,
                monitors: vec![live(r"\\.\DISPLAY1")],
            }),
            layout: None,
        }
    }

    #[test]
    fn a_missing_file_is_absent() {
        assert!(matches!(
            classify(RawRead::Absent, NOW),
            StateFileStatus::Absent
        ));
    }

    #[test]
    fn an_os_read_error_is_unreadable() {
        assert!(matches!(
            classify(RawRead::Io, NOW),
            StateFileStatus::Unreadable(UnreadableReason::Io)
        ));
    }

    #[test]
    fn non_utf8_bytes_are_unreadable() {
        let bytes = vec![0xFF, 0xFE, 0xFD];
        assert!(matches!(
            classify(RawRead::Present(bytes), NOW),
            StateFileStatus::Unreadable(UnreadableReason::NotUtf8)
        ));
    }

    #[test]
    fn a_torn_write_is_unreadable() {
        let whole = serialize_state(&document(NOW)).unwrap();
        // Truncated mid-document, the shape an atomic rename never actually
        // produces but a defensive reader still has to survive.
        let torn = whole[..whole.len() / 2].to_owned();
        assert!(matches!(
            classify(RawRead::Present(torn.into_bytes()), NOW),
            StateFileStatus::Unreadable(UnreadableReason::Parse(_))
        ));
    }

    #[test]
    fn an_unknown_version_is_unreadable() {
        let json = serialize_state(&document(NOW)).unwrap().replace(
            &format!("\"version\": {TOPOLOGY_STATE_VERSION}"),
            "\"version\": 9999",
        );
        assert!(matches!(
            classify(RawRead::Present(json.into_bytes()), NOW),
            StateFileStatus::Unreadable(UnreadableReason::Parse(_))
        ));
    }

    #[test]
    fn a_too_large_raw_read_is_unreadable_and_names_the_size() {
        let status = classify(RawRead::TooLarge { bytes: 999 }, NOW);
        match status {
            StateFileStatus::Unreadable(UnreadableReason::Parse(error)) => {
                assert!(error.to_string().contains("999"), "{error}");
            }
            other => panic!("expected Unreadable(Parse), got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_document_that_reaches_the_parser_is_still_unreadable() {
        let huge = "x".repeat(crossover_topology::state::MAX_STATE_FILE_BYTES + 1);
        assert!(matches!(
            classify(RawRead::Present(huge.into_bytes()), NOW),
            StateFileStatus::Unreadable(UnreadableReason::Parse(_))
        ));
    }

    #[test]
    fn a_fresh_heartbeat_reads_as_fresh() {
        let json = serialize_state(&document(NOW)).unwrap();
        match classify(RawRead::Present(json.into_bytes()), NOW) {
            StateFileStatus::Fresh(state) => assert_eq!(state.written_at, NOW),
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    #[test]
    fn a_heartbeat_at_the_stale_threshold_is_still_fresh() {
        let written_at = NOW - HEARTBEAT_STALE_AFTER_MS;
        let json = serialize_state(&document(written_at)).unwrap();
        assert!(matches!(
            classify(RawRead::Present(json.into_bytes()), NOW),
            StateFileStatus::Fresh(_)
        ));
    }

    #[test]
    fn a_heartbeat_past_the_stale_threshold_is_stale() {
        let written_at = NOW - HEARTBEAT_STALE_AFTER_MS - 1;
        let json = serialize_state(&document(written_at)).unwrap();
        match classify(RawRead::Present(json.into_bytes()), NOW) {
            StateFileStatus::Stale(state) => assert_eq!(state.written_at, written_at),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn a_document_with_a_layout_survives_classification_intact() {
        let mut doc = document(NOW);
        let layout = crossover_topology::Layout::new(
            3,
            doc.local.device,
            vec![
                crossover_topology::PlacedMonitor {
                    device: doc.local.device,
                    id: MonitorId::new(r"\\.\DISPLAY1").unwrap(),
                    rect: LayoutRect {
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    },
                },
                crossover_topology::PlacedMonitor {
                    device: doc.peer.as_ref().unwrap().device,
                    id: MonitorId::new(r"\\.\DISPLAY1").unwrap(),
                    rect: LayoutRect {
                        x: 1920,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    },
                },
            ],
            &crossover_topology::DevicePair::new(
                doc.local.device,
                doc.peer.as_ref().unwrap().device,
            )
            .unwrap(),
        )
        .unwrap();
        doc.layout = Some(LayoutState::from_layout(&layout));
        let json = serialize_state(&doc).unwrap();
        match classify(RawRead::Present(json.into_bytes()), NOW) {
            StateFileStatus::Fresh(state) => {
                assert_eq!(state.layout.unwrap().monitors.len(), 2);
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    /// Issue 7: the size cap is enforced *before* `read`, against a real
    /// file — `stat` refuses it without a byte ever coming back.
    #[test]
    fn a_real_oversized_file_is_refused_by_stat_before_it_is_read() {
        let sandbox = Sandbox::new("oversized");
        let path = sandbox.path("topology.json");
        let cap = crossover_topology::state::MAX_STATE_FILE_BYTES;
        std::fs::write(&path, "x".repeat(cap + 1)).unwrap();
        assert!(
            matches!(read_raw(&path), RawRead::TooLarge { bytes } if bytes == u64::try_from(cap + 1).unwrap())
        );
    }

    /// A real file at (not over) the cap is read normally — the bound is a
    /// ceiling, not a trap for a large-but-legal document.
    #[test]
    fn a_real_file_at_the_cap_is_read_normally() {
        let sandbox = Sandbox::new("at-cap");
        let path = sandbox.path("topology.json");
        let mut padded = serialize_state(&document(NOW)).unwrap();
        let cap = crossover_topology::state::MAX_STATE_FILE_BYTES;
        let room = cap - padded.len();
        padded.insert_str(0, &" ".repeat(room));
        std::fs::write(&path, &padded).unwrap();
        assert!(matches!(read_raw(&path), RawRead::Present(bytes) if bytes.len() == cap));
    }

    #[test]
    fn a_real_missing_file_is_absent() {
        let sandbox = Sandbox::new("missing");
        let path = sandbox.path("nope.json");
        assert_eq!(read_raw(&path), RawRead::Absent);
    }
}
