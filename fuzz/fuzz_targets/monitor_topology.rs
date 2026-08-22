//! Fuzz the `MonitorTopology` payload decoder (ADR 0018, docs/PROTOCOL.md
//! §6.2): a peer's claimed local monitors. A truncated payload, a count
//! past `MAX_MONITORS_PER_MACHINE`, a zero or oversized rectangle, an
//! out-of-range coordinate, a `scale_percent` outside its bound, a
//! duplicate monitor id, an unusable monitor id, or an unusable monitor
//! label — over `MAX_MONITOR_LABEL_BYTES`, carrying a control character,
//! or not valid UTF-8 at all — must all reject, never panic (NFR-1).
//!
//! The round trip below is what makes the label interesting to fuzz rather
//! than merely to decode: a label is the one field where decode admits
//! more shapes than an id does (UTF-8 rather than ASCII), so "whatever
//! decoded must re-encode to itself" is the property that catches a
//! decoder and an encoder disagreeing about it.

#![no_main]

use crossover_protocol::MonitorTopology;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = MonitorTopology::decode_payload(data) {
        let encoded = message.encode_payload().expect("topology must re-encode");
        assert_eq!(
            MonitorTopology::decode_payload(&encoded).expect("topology must re-decode"),
            message
        );
    }
});
