//! The worker→editor state file: `~/.crossover/state/topology.json`
//! (ADR 0018).
//!
//! The layout editor is a separate, user-session process (ADR 0011 makes
//! the worker headless and service-launched), so it needs somewhere to
//! learn what to draw. This is that somewhere: a **versioned JSON
//! document**, written by the worker and read by the editor, one way only.
//! The reverse direction — an edit reaching the worker — travels through
//! the config file, which the worker already owns as its startup input.
//!
//! What it carries, and why each part is there:
//!
//! - **This device and its live monitors**, so the editor can draw the
//!   screens actually attached rather than the ones a layout remembers.
//! - **The last-known peer**, retained across a disconnect with
//!   `connected: false`. An editor that empties itself the moment the link
//!   drops is an editor you cannot use to fix the link.
//! - **The current layout and its revision**, so the editor starts from
//!   what is in force rather than from a blank canvas.
//! - **A heartbeat**, so the editor can say *the worker is not running*
//!   instead of presenting stale facts as current.
//!
//! **Nothing secret is in it** — device names and ids, monitor device
//! strings, rectangles, and scale factors; no key material, no clipboard
//! content, no peer credentials (docs/SECURITY.md invariant 6). It needs no
//! protection beyond the profile it sits in.
//!
//! This module is the *schema*: the types, the strict version check, and
//! the round trip. The writer task — the heartbeat, the atomic write, the
//! coalescing — is the worker's, and lands with it.
//!
//! # What the decoder enforces, and why it enforces anything at all
//!
//! This file is written by the worker into the user's own profile, so it is
//! not network input in the way a `LayoutSync` is. It is nonetheless
//! decoded with the same discipline, and the reason is concrete rather than
//! ceremonial: **the layout it reports is peer-influenced**. The worker
//! adopts an arrangement drawn at the other desk and reports it here, so
//! peer-supplied numbers reach this document by design — and a reader that
//! trusted them because they arrived via a local file would be trusting the
//! peer with an extra step in between. The file is also, from the editor's
//! point of view, simply a file on disk: truncated, half-written by an
//! older build, or hand-edited.
//!
//! So, in bound order, before anything is used:
//!
//! - **A total size cap** ([`MAX_STATE_FILE_BYTES`]) checked *before*
//!   parsing, so a multi-gigabyte document costs a length comparison rather
//!   than an allocation.
//! - **The version, checked first** and strictly, so a document of another
//!   version is refused whole rather than partly believed.
//! - **Every list is capped while it decodes** — `MAX_MONITORS_PER_MACHINE`
//!   per machine, `MAX_LAYOUT_MONITORS` for the layout — refusing on the
//!   element past the cap, so an over-long list is never built.
//! - **Every monitor id** is a validated [`MonitorId`], **every rectangle**
//!   satisfies [`LayoutRect::check_bounds`], and **every `scale_percent`**
//!   is within [`MIN_SCALE_PERCENT`]..=[`MAX_SCALE_PERCENT`] — all three at
//!   the decoder, so an unusable value cannot exist in a decoded document.
//!
//! What is *not* enforced here is the arrangement's own semantics — the
//! overlap rule, the pair, both machines present. Those need the current
//! session's pair to mean anything, so they live in
//! [`LayoutState::validate`], which a reader calls when it wants to act on
//! the layout rather than merely display it.

use serde::{Deserialize, Serialize};

use crate::bounded::bounded_seq;
use crate::device::DeviceId;
use crate::layout::{
    DevicePair, Layout, LayoutError, LayoutRect, MAX_LAYOUT_MONITORS, MAX_MONITORS_PER_MACHINE,
    MAX_SCALE_PERCENT, MIN_SCALE_PERCENT, PlacedMonitor,
};
use crate::monitor::MonitorId;

/// The schema version this build writes and is willing to read.
///
/// Checked strictly and before anything else: a document of another version
/// is refused whole, never partially believed. The house rule for at-rest
/// formats (`crossover-security`'s identity and trust blobs both do this
/// with a leading version byte), applied to a document rather than a blob.
pub const TOPOLOGY_STATE_VERSION: u32 = 1;

/// Where the file sits, relative to the `~/.crossover` root the config and
/// logs already share.
///
/// Stated here rather than in either process, so the worker that writes it
/// and the editor that reads it cannot disagree about the name. Resolving
/// the home directory stays the application's job.
pub const STATE_FILE_RELATIVE_PATH: &str = "state/topology.json";

/// How often the worker refreshes `written_at` (ADR 0018's heartbeat).
///
/// The same 2 s cadence as the config modification-time poll it rides
/// alongside, so the worker's periodic work stays one rhythm rather than
/// two.
pub const HEARTBEAT_INTERVAL_MS: u64 = 2_000;

/// How old `written_at` may get before a reader should call the worker
/// gone.
///
/// Five missed heartbeats. Long enough that a loaded machine or a slow
/// disk does not make a running worker look dead, short enough that the
/// editor does not present a dead worker's arrangement as live.
pub const HEARTBEAT_STALE_AFTER_MS: u64 = 5 * HEARTBEAT_INTERVAL_MS;

/// Largest state document this build will even attempt to parse.
///
/// A conforming document is *tiny*: two machines of at most
/// [`MAX_MONITORS_PER_MACHINE`] monitors plus a layout of at most
/// [`MAX_LAYOUT_MONITORS`], each row a hundred-odd bytes of pretty-printed
/// JSON — a few tens of kilobytes at the very worst, and well under one in
/// practice. 256 KiB is an order of magnitude of headroom over that and
/// still refuses a document that could only be a mistake or a mischief,
/// before a parser has allocated anything.
///
/// The per-list caps below are the real bound on what a document can
/// *describe*; this one bounds what it can *cost to look at*.
pub const MAX_STATE_FILE_BYTES: usize = 256 * 1024;

/// One monitor as the machine that owns it currently sees it — live
/// geometry in that machine's **own local coordinates**, not layout space.
///
/// This is what `MonitorTopology` states about a sender, recorded so the
/// editor can seed a to-scale drawing. `scale_percent` is a **seeding
/// input only** ([`MIN_SCALE_PERCENT`]..=[`MAX_SCALE_PERCENT`], 100
/// unscaled): it never enters crossing mapping, which stays proportional
/// through the drawn geometry.
///
/// Every field is checked on decode — the id by [`MonitorId`]'s own
/// validation, the rectangle by [`LayoutRect::check_bounds`], the scale
/// against its two constants — so an unusable [`LiveMonitor`] cannot be
/// constructed by deserializing one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawLiveMonitor")]
pub struct LiveMonitor {
    /// The platform-supplied identity a layout addresses it by.
    pub id: MonitorId,
    /// Its live geometry, in its own machine's coordinates.
    pub rect: LayoutRect,
    /// Its display scale in percent, 100 being unscaled.
    pub scale_percent: u16,
}

/// [`LiveMonitor`] before its bounds have been checked — the shape serde
/// builds, which [`TryFrom`] then admits or refuses.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLiveMonitor {
    id: MonitorId,
    rect: LayoutRect,
    scale_percent: u16,
}

impl TryFrom<RawLiveMonitor> for LiveMonitor {
    type Error = String;

    fn try_from(raw: RawLiveMonitor) -> Result<Self, Self::Error> {
        raw.rect
            .check_bounds()
            .map_err(|violation| format!("monitor {}: {violation}", raw.id))?;
        if !(MIN_SCALE_PERCENT..=MAX_SCALE_PERCENT).contains(&raw.scale_percent) {
            return Err(format!(
                "monitor {}: scale {} percent is outside {MIN_SCALE_PERCENT}..={MAX_SCALE_PERCENT}",
                raw.id, raw.scale_percent
            ));
        }
        Ok(Self {
            id: raw.id,
            rect: raw.rect,
            scale_percent: raw.scale_percent,
        })
    }
}

/// A machine and the screens attached to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineState {
    /// Its bookkeeping identity.
    pub device: DeviceId,
    /// Its human name — the one the peer sees, bounded at the protocol
    /// edge (`crossover-protocol`'s `MAX_DEVICE_NAME_BYTES`) long before
    /// it reaches this file. Held as a plain `String` rather than a fourth
    /// copy of that bound; [`MAX_STATE_FILE_BYTES`] is what stops a
    /// hand-edited file putting a megabyte here.
    pub name: String,
    /// What it currently has attached, capped at
    /// [`MAX_MONITORS_PER_MACHINE`] as the list decodes.
    #[serde(deserialize_with = "bounded_seq::<_, _, MAX_MONITORS_PER_MACHINE>")]
    pub monitors: Vec<LiveMonitor>,
}

/// The peer, as last seen — retained across a disconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerState {
    /// Its bookkeeping identity.
    pub device: DeviceId,
    /// Its human name.
    pub name: String,
    /// Whether a session is up **right now**. `false` with the monitors
    /// still listed is the deliberate state: the editor stays usable while
    /// the peer is down.
    pub connected: bool,
    /// When the peer was last known good, in milliseconds since the Unix
    /// epoch — see [`now_unix_millis`].
    pub last_seen: u64,
    /// What it last reported having attached, capped at
    /// [`MAX_MONITORS_PER_MACHINE`] as the list decodes.
    #[serde(deserialize_with = "bounded_seq::<_, _, MAX_MONITORS_PER_MACHINE>")]
    pub monitors: Vec<LiveMonitor>,
}

/// The arrangement in force, as the state file reports it.
///
/// A *report*, not a source of truth: the config file is where a layout
/// lives (ADR 0018's persist-publish-report order). An editor that wants to
/// act on it validates it against the current pair — [`LayoutState::validate`]
/// — rather than trusting the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutState {
    /// Its revision.
    pub revision: u64,
    /// The device that drew it.
    pub origin: DeviceId,
    /// Its placed monitors, capped at [`MAX_LAYOUT_MONITORS`] as the list
    /// decodes. Each one's rectangle is bounds-checked by
    /// [`PlacedMonitor`]'s own decoder; the rules that need the session's
    /// pair to mean anything are [`LayoutState::validate`]'s.
    #[serde(deserialize_with = "bounded_seq::<_, _, MAX_LAYOUT_MONITORS>")]
    pub monitors: Vec<PlacedMonitor>,
}

impl LayoutState {
    /// The report for `layout`.
    #[must_use]
    pub fn from_layout(layout: &Layout) -> Self {
        Self {
            revision: layout.revision(),
            origin: layout.origin(),
            monitors: layout.monitors().to_vec(),
        }
    }

    /// Re-validate this report as an arrangement of `pair`.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] — the same rules as anywhere else. The file is not
    /// trusted merely because this machine wrote it: a pairing can have
    /// changed since, which is exactly the re-pair residue ADR 0018 says
    /// to reject rather than guess about.
    pub fn validate(&self, pair: &DevicePair) -> Result<Layout, LayoutError> {
        Layout::new(self.revision, self.origin, self.monitors.clone(), pair)
    }
}

/// The whole document (ADR 0018).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyState {
    /// Always [`TOPOLOGY_STATE_VERSION`] on write; checked strictly on
    /// read.
    pub version: u32,
    /// The heartbeat: when the worker last wrote this document, in
    /// milliseconds since the Unix epoch. A reader compares it against its
    /// own clock — see [`TopologyState::is_stale`].
    pub written_at: u64,
    /// This machine. Named `self` in the document, which is not a
    /// spellable Rust field.
    #[serde(rename = "self")]
    pub local: MachineState,
    /// The peer, if one has ever been seen. `None` before the first
    /// session; `Some` with `connected: false` after one ends.
    pub peer: Option<PeerState>,
    /// The arrangement in force, if there is one. `None` means seamless
    /// transfer is off — a machine with no arrangement drawn does not
    /// guess one.
    pub layout: Option<LayoutState>,
}

impl TopologyState {
    /// Has the heartbeat gone quiet — is the worker probably not running?
    ///
    /// Saturating, so a document written by a machine whose clock is ahead
    /// of the reader's reads as fresh rather than as a wildly stale one.
    #[must_use]
    pub fn is_stale(&self, now_unix_millis: u64) -> bool {
        now_unix_millis.saturating_sub(self.written_at) > HEARTBEAT_STALE_AFTER_MS
    }
}

/// Why a state document could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateError {
    /// Bigger than [`MAX_STATE_FILE_BYTES`].
    ///
    /// Checked before the parser runs, so an enormous document costs a
    /// length comparison rather than an allocation.
    #[error(
        "the topology state file is {bytes} bytes, over the {MAX_STATE_FILE_BYTES}-byte maximum"
    )]
    TooLarge {
        /// The size that was offered.
        bytes: usize,
    },
    /// Not JSON, or not this shape, or past one of the bounds the decoder
    /// enforces (a list over its cap, an unusable monitor id, a rectangle
    /// or scale outside its range).
    #[error("the topology state file is malformed")]
    Malformed {
        /// Where and why it failed.
        #[source]
        source: serde_json::Error,
    },
    /// A version this build does not understand.
    ///
    /// Refused whole rather than read for the parts that happen to match:
    /// a document of another version is a document written by another
    /// build, and half-believing it is how an editor draws an arrangement
    /// nobody is using.
    #[error("topology state version {found} is not supported (this build reads {expected})")]
    UnsupportedVersion {
        /// The version the document claimed.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
}

/// Just enough of the document to learn its version.
///
/// Read first, and deliberately tolerant of everything else, so the version
/// check happens before the strict schema is applied — otherwise a v2
/// document would be reported as malformed rather than as the newer
/// version it honestly is.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

/// Parse a state document: size, then version, then the strict schema.
///
/// The order is the bound order. The size cap runs before any parsing at
/// all; the version check runs before the schema, so a newer document is
/// reported as newer rather than as broken; and the schema then enforces
/// every per-field and per-list bound this module's header lists.
///
/// # Errors
///
/// [`StateError::TooLarge`] past [`MAX_STATE_FILE_BYTES`],
/// [`StateError::UnsupportedVersion`] for a document of another version,
/// [`StateError::Malformed`] for anything else — including a value that is
/// well-formed JSON but outside one of the bounds.
pub fn parse_state(json: &str) -> Result<TopologyState, StateError> {
    // Before the parser, not after it: this is the only check that bounds
    // what looking at the document can cost.
    if json.len() > MAX_STATE_FILE_BYTES {
        return Err(StateError::TooLarge { bytes: json.len() });
    }
    let probe: VersionProbe =
        serde_json::from_str(json).map_err(|source| StateError::Malformed { source })?;
    if probe.version != TOPOLOGY_STATE_VERSION {
        return Err(StateError::UnsupportedVersion {
            found: probe.version,
            expected: TOPOLOGY_STATE_VERSION,
        });
    }
    serde_json::from_str(json).map_err(|source| StateError::Malformed { source })
}

/// Render a state document.
///
/// Pretty-printed: this is a file a person opens while working out why a
/// crossing went where it did, and a single-line JSON object of 32
/// rectangles is not a file anybody reads.
///
/// # Errors
///
/// [`StateError::Malformed`] if serialization fails, which for these types
/// it cannot — the variant exists so the signature does not have to lie
/// about being infallible.
pub fn serialize_state(state: &TopologyState) -> Result<String, StateError> {
    serde_json::to_string_pretty(state).map_err(|source| StateError::Malformed { source })
}

/// The current time in milliseconds since the Unix epoch, for
/// `written_at` and `last_seen`.
///
/// Milliseconds since the epoch rather than a formatted timestamp, because
/// the only thing anybody does with these is subtract them, and a
/// comparison that needs a date parser is a comparison that can fail.
/// A clock before 1970 reads as 0 rather than panicking.
#[must_use]
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::{
        HEARTBEAT_STALE_AFTER_MS, LayoutState, LiveMonitor, MAX_STATE_FILE_BYTES, MachineState,
        PeerState, StateError, TOPOLOGY_STATE_VERSION, TopologyState, now_unix_millis, parse_state,
        serialize_state,
    };
    use crate::layout::tests::{LOCAL, PEER, pair, valid_layout};
    use crate::layout::{
        LayoutError, LayoutRect, MAX_LAYOUT_MONITORS, MAX_MONITORS_PER_MACHINE, MAX_SCALE_PERCENT,
        MIN_SCALE_PERCENT,
    };
    use crate::monitor::MonitorId;

    fn live(id: &str, x: i32, scale_percent: u16) -> LiveMonitor {
        LiveMonitor {
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_percent,
        }
    }

    fn document() -> TopologyState {
        TopologyState {
            version: TOPOLOGY_STATE_VERSION,
            written_at: 1_766_000_000_000,
            local: MachineState {
                device: LOCAL,
                name: "workstation-left".to_owned(),
                monitors: vec![
                    live(r"\\.\DISPLAY1", 0, 150),
                    live(r"\\.\DISPLAY2", 1920, 100),
                ],
            },
            peer: Some(PeerState {
                device: PEER,
                name: "laptop".to_owned(),
                connected: false,
                last_seen: 1_765_999_000_000,
                monitors: vec![live(r"\\.\DISPLAY1", 0, 200)],
            }),
            layout: Some(LayoutState::from_layout(&valid_layout())),
        }
    }

    #[test]
    fn a_document_round_trips_exactly() {
        let state = document();
        let json = serialize_state(&state).unwrap();
        assert_eq!(parse_state(&json).unwrap(), state);

        // The document keys are the ones ADR 0018 names, `self` included.
        assert!(json.contains("\"self\""), "{json}");
        assert!(json.contains("\"written_at\""), "{json}");
        assert!(json.contains("\"connected\": false"), "{json}");
        assert!(json.contains("\"last_seen\""), "{json}");
        assert!(json.contains("\"scale_percent\""), "{json}");
        assert!(json.contains("\"revision\""), "{json}");
        assert!(json.contains("\"origin\""), "{json}");
        // Device ids are the hyphenated text form a human can match
        // against `crossover peers list`.
        assert!(json.contains(&LOCAL.to_string()), "{json}");
    }

    /// The disconnected-but-drawable state the editor depends on.
    #[test]
    fn a_disconnected_peer_keeps_its_monitors() {
        let state = document();
        let peer = state.peer.as_ref().unwrap();
        assert!(!peer.connected);
        assert_eq!(peer.monitors.len(), 1);

        // And a machine that has never seen a peer is representable.
        let mut fresh = document();
        fresh.peer = None;
        fresh.layout = None;
        let json = serialize_state(&fresh).unwrap();
        assert_eq!(parse_state(&json).unwrap(), fresh);
    }

    #[test]
    fn an_unknown_version_is_refused_whole() {
        let json = serialize_state(&document())
            .unwrap()
            .replace("\"version\": 1", "\"version\": 2");
        let error = parse_state(&json).unwrap_err();
        assert!(
            matches!(
                error,
                StateError::UnsupportedVersion {
                    found: 2,
                    expected: TOPOLOGY_STATE_VERSION
                }
            ),
            "{error:?}"
        );

        // Version 0 and a far-future version alike.
        for version in ["0", "4294967295"] {
            let json = serialize_state(&document())
                .unwrap()
                .replace("\"version\": 1", &format!("\"version\": {version}"));
            assert!(matches!(
                parse_state(&json).unwrap_err(),
                StateError::UnsupportedVersion { .. }
            ));
        }
    }

    /// The version check runs *before* the strict schema, so a newer
    /// document is reported as newer rather than as broken.
    #[test]
    fn a_newer_document_with_unknown_fields_reports_its_version_not_its_shape() {
        let json = r#"{"version":9,"written_at":1,"self":{},"something_new":true}"#;
        assert!(matches!(
            parse_state(json).unwrap_err(),
            StateError::UnsupportedVersion { found: 9, .. }
        ));
    }

    #[test]
    fn malformed_documents_are_values_not_panics() {
        for json in [
            "",
            "{",
            "null",
            "[]",
            r#"{"version":"one"}"#,
            r#"{"version":1}"#, // right version, missing everything else
            r#"{"version":1,"written_at":1,"self":{"device":"nope","name":"x","monitors":[]},"peer":null,"layout":null}"#,
            r#"{"version":1,"written_at":1,"self":{"device":"11111111-1111-1111-1111-111111111111","name":"x","monitors":[]},"peer":null,"layout":null,"surprise":1}"#,
        ] {
            assert!(
                matches!(parse_state(json), Err(StateError::Malformed { .. })),
                "admitted {json:?}"
            );
        }
    }

    /// An id that could not exist is refused at the decoder, because
    /// `MonitorId` validates on deserialize — the property the newtype is
    /// for.
    #[test]
    fn an_unusable_monitor_id_does_not_survive_the_decoder() {
        let json = serialize_state(&document())
            .unwrap()
            .replace(r"\\\\.\\DISPLAY1", &"x".repeat(65));
        assert!(matches!(
            parse_state(&json),
            Err(StateError::Malformed { .. })
        ));
    }

    #[test]
    fn the_reported_layout_re_validates_against_the_current_pair() {
        let layout = valid_layout();
        let reported = LayoutState::from_layout(&layout);
        assert_eq!(reported.validate(&pair()).unwrap(), layout);

        // A pairing that has changed since the file was written: the
        // report is refused rather than guessed about.
        let stranger = crate::device::DeviceId::from_bytes([0x77; 16]);
        let new_pair = crate::layout::DevicePair::new(LOCAL, stranger).unwrap();
        assert_eq!(
            reported.validate(&new_pair).unwrap_err(),
            LayoutError::UnexpectedDevice { device: PEER }
        );
    }

    /// Every bound the module header claims, exercised at its edge.
    ///
    /// The layout it reports is peer-influenced — the worker adopts an
    /// arrangement drawn at the other desk and writes it here — so a
    /// reader that trusted these numbers because they arrived via a local
    /// file would be trusting the peer with a step in between.
    #[test]
    fn the_decoder_enforces_every_bound_it_documents() {
        let json = serialize_state(&document()).unwrap();

        // scale_percent, both ends, inclusive.
        for (scale, admitted) in [
            (MIN_SCALE_PERCENT - 1, false),
            (MIN_SCALE_PERCENT, true),
            (MAX_SCALE_PERCENT, true),
            (MAX_SCALE_PERCENT + 1, false),
            (0, false),
            (u16::MAX, false),
        ] {
            let mutated = json.replace(
                "\"scale_percent\": 150",
                &format!("\"scale_percent\": {scale}"),
            );
            assert_eq!(
                parse_state(&mutated).is_ok(),
                admitted,
                "scale {scale} percent was handled wrong"
            );
        }

        // Rectangle extents and coordinates, through the same rules a
        // layout is held to.
        for (field, value, admitted) in [
            ("\"width\": 1920", "\"width\": 0", false),
            ("\"width\": 1920", "\"width\": 65535", true),
            ("\"width\": 1920", "\"width\": 65536", false),
            ("\"height\": 1080", "\"height\": 0", false),
            ("\"x\": 1920", "\"x\": 16777216", true),
            ("\"x\": 1920", "\"x\": 16777217", false),
            ("\"x\": 1920", "\"x\": -16777217", false),
        ] {
            let mutated = json.replace(field, value);
            assert_ne!(mutated, json, "the fixture no longer contains {field}");
            assert_eq!(
                parse_state(&mutated).is_ok(),
                admitted,
                "{value} was handled wrong"
            );
        }
    }

    /// A list is refused *while* it decodes, so an over-long one is never
    /// built — and the cap is the same one a layout is held to.
    #[test]
    fn monitor_lists_are_capped_as_they_decode() {
        let live_row = |index: usize| {
            format!(
                r#"{{"id":"M{index}","rect":{{"x":{index},"y":0,"width":10,"height":10}},"scale_percent":100}}"#
            )
        };
        let machine = |count: usize| {
            let rows: Vec<String> = (0..count).map(live_row).collect();
            format!(
                r#"{{"version":1,"written_at":1,"self":{{"device":"{LOCAL}","name":"x","monitors":[{}]}},"peer":null,"layout":null}}"#,
                rows.join(",")
            )
        };
        assert!(parse_state(&machine(MAX_MONITORS_PER_MACHINE)).is_ok());
        assert!(
            matches!(
                parse_state(&machine(MAX_MONITORS_PER_MACHINE + 1)),
                Err(StateError::Malformed { .. })
            ),
            "a machine over its per-machine cap was admitted"
        );

        let placed_row = |index: usize| {
            format!(
                r#"{{"device":"{LOCAL}","id":"M{index}","rect":{{"x":{index},"y":0,"width":10,"height":10}}}}"#
            )
        };
        let layout = |count: usize| {
            let rows: Vec<String> = (0..count).map(placed_row).collect();
            format!(
                r#"{{"version":1,"written_at":1,"self":{{"device":"{LOCAL}","name":"x","monitors":[]}},"peer":null,"layout":{{"revision":1,"origin":"{LOCAL}","monitors":[{}]}}}}"#,
                rows.join(",")
            )
        };
        assert!(parse_state(&layout(MAX_LAYOUT_MONITORS)).is_ok());
        assert!(
            matches!(
                parse_state(&layout(MAX_LAYOUT_MONITORS + 1)),
                Err(StateError::Malformed { .. })
            ),
            "a layout over its cap was admitted"
        );
    }

    /// The size cap runs before the parser, so an enormous document costs
    /// a length comparison rather than an allocation.
    #[test]
    fn an_oversized_document_is_refused_before_it_is_parsed() {
        // Not even valid JSON: reaching `TooLarge` proves nothing parsed.
        let huge = "x".repeat(MAX_STATE_FILE_BYTES + 1);
        assert!(
            matches!(
                parse_state(&huge),
                Err(StateError::TooLarge {
                    bytes
                }) if bytes == MAX_STATE_FILE_BYTES + 1
            ),
            "an oversized document reached the parser"
        );

        // A real document padded to exactly the cap still parses, so the
        // bound is a ceiling rather than a trap for a large-but-legal file.
        let mut padded = serialize_state(&document()).unwrap();
        let room = MAX_STATE_FILE_BYTES - padded.len();
        padded.insert_str(0, &" ".repeat(room));
        assert_eq!(padded.len(), MAX_STATE_FILE_BYTES);
        assert!(parse_state(&padded).is_ok());
    }

    #[test]
    fn the_heartbeat_says_when_the_worker_has_gone_quiet() {
        let state = document();
        let written = state.written_at;
        assert!(!state.is_stale(written));
        assert!(!state.is_stale(written + HEARTBEAT_STALE_AFTER_MS));
        assert!(state.is_stale(written + HEARTBEAT_STALE_AFTER_MS + 1));
        // A reader whose clock is behind the writer's sees a fresh file,
        // not a wildly stale one.
        assert!(!state.is_stale(0));
    }

    #[test]
    fn the_clock_helper_reports_a_plausible_epoch_time() {
        // Past 2020 and short of 2100: enough to catch a unit mistake
        // (seconds or nanoseconds instead of milliseconds) without
        // pinning the test to a date.
        let now = now_unix_millis();
        assert!(now > 1_577_836_800_000, "{now}");
        assert!(now < 4_102_444_800_000, "{now}");
    }
}
