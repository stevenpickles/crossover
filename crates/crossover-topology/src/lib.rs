//! The drawn display topology: the layout model, its validation, the
//! `[layout]` config section and its writer, and the worker→editor state
//! file ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)).
//!
//! A layout is a set of monitors placed in one shared, unit-agnostic
//! coordinate space, carrying the machine each belongs to. It answers
//! exactly one question — *which peer monitor lies across which of my
//! edges, and where along it* — and everything local stays the OS's.
//!
//! # Why this is a crate of its own
//!
//! The layout editor is a GUI binary, and it must share the model **and the
//! config writer** with the worker. Linking `crossover-core` to get them
//! would drag the protocol, security, and platform crates into a process
//! that has no business holding the TLS stack or the input injector. So the
//! model lives here, behind a dependency boundary a reviewer verifies by
//! reading one `Cargo.toml` and a `cargo tree` — the same reasoning ADR
//! 0011 applied to `crossover-svc`.
//!
//! # The dependency graph, exactly
//!
//! - **Default: `serde` and `thiserror`, and nothing else.** This is the
//!   graph ADR 0018 fixes, and the one every consumer of the model alone
//!   gets.
//! - **With the non-default `config` feature: plus `toml_edit` and
//!   `serde_json`** — the [`config`] section writer and the [`state`] file
//!   schema respectively. (The ADR's dependency sentence names `toml_edit`
//!   but not `serde_json`; its dated amendment records that versioned JSON
//!   needs a JSON implementation and that the sentence describes the
//!   default graph.)
//!
//! The worker and the editor enable `config`. `crossover-protocol` depends
//! on this crate — for the wire shapes of `MonitorTopology`, `LayoutSync`,
//! and `EntryPoint`, so the model and its validation have one definition
//! instead of a wire copy and a config copy that can drift — and takes the
//! default graph, staying as dependency-light and socket-free as
//! `docs/ARCHITECTURE.md` §3.1 requires.
//!
//! Both feature-gated modules are also compiled for this crate's own tests,
//! so the CI gate — which enables no extra features — exercises them
//! without any consumer having to opt in.
//!
//! # Everything here treats its input as hostile
//!
//! A layout reaches this machine from the peer and decides where control is
//! handed away, so it is peer-influenced local state (docs/SECURITY.md
//! T23). Every bound is a named constant, counts are checked before
//! anything is allocated, all derivation arithmetic runs in `i64` where the
//! bounds make overflow impossible rather than improbable, and malformed
//! input produces a typed refusal — never a panic (NFR-1).

pub mod bounded;
pub mod device;
pub mod layout;
pub mod monitor;

// The on-disk halves. `cfg(test)` alongside the feature so the CI gate,
// which enables no extra features, still compiles and runs their suites —
// the shape `crossover-platform` uses for its `fakes` module.
#[cfg(any(test, feature = "config"))]
pub mod atomic_write;
#[cfg(any(test, feature = "config"))]
pub mod config;
#[cfg(any(test, feature = "config"))]
pub mod state;

pub use bounded::bounded_seq;
pub use device::{DEVICE_ID_BYTES, DeviceId, DeviceIdParseError};
pub use layout::{
    DevicePair, Layout, LayoutError, LayoutRect, MAX_LAYOUT_COORDINATE, MAX_LAYOUT_MONITORS,
    MAX_MONITOR_EXTENT, MAX_MONITORS_PER_MACHINE, MAX_SCALE_PERCENT, MIN_SCALE_PERCENT, MonitorKey,
    PlacedMonitor, RawPlacedMonitor, check_structure,
};
pub use monitor::{
    FORMAT_CHARACTERS, MAX_MONITOR_ID_BYTES, MAX_MONITOR_LABEL_BYTES, MAX_PHYSICAL_SIZE_MM,
    MAX_PLAUSIBLE_PHYSICAL_MM, MIN_PLAUSIBLE_PHYSICAL_MM, MonitorId, MonitorIdError, MonitorLabel,
    MonitorLabelError, PhysicalSizeError, PhysicalSizeMm, is_plausible_millimetres,
    is_plausible_physical_size, validate_monitor_id, validate_monitor_label,
    validate_physical_size,
};

#[cfg(any(test, feature = "config"))]
pub use atomic_write::{AtomicWriteError, temp_path, write_atomic};
#[cfg(any(test, feature = "config"))]
pub use config::{
    CONFIG_FILE_NAME, CONFIG_SCHEMA_MIN_SUPPORTED, CONFIG_SCHEMA_VERSION, LayoutMonitorRow,
    LayoutSection, PersistError, config_path_in, config_schema_supported, persist_layout,
    read_layout_revision,
};
#[cfg(any(test, feature = "config"))]
pub use state::{
    HEARTBEAT_INTERVAL_MS, HEARTBEAT_STALE_AFTER_MS, LayoutState, LiveMonitor, MachineState,
    PeerState, STATE_FILE_RELATIVE_PATH, StateError, TOPOLOGY_STATE_VERSION, TopologyState,
    now_unix_millis, parse_state, serialize_state,
};

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str =
    "the drawn layout model, its validation, and its on-disk shapes (ADR 0018)";

#[cfg(test)]
mod tests {
    use super::CRATE_PURPOSE;

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }
}
