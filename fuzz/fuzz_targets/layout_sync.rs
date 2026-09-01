//! Fuzz the `LayoutSync` payload decoder (ADR 0018, docs/PROTOCOL.md §6.2):
//! a peer's claimed drawn arrangement, `Vec<PlacedMonitor>` carried
//! directly from `crossover-topology`. A truncated payload, a count past
//! `MAX_LAYOUT_MONITORS`, a per-machine count past
//! `MAX_MONITORS_PER_MACHINE`, more than two distinct devices, a zero or
//! oversized rectangle, an out-of-range coordinate, a duplicate
//! `(device, id)` pair, or an unusable monitor id must all reject, never
//! panic (NFR-1). Session-pair membership and overlap are deliberately
//! **not** exercised here — this module never checks them; see
//! `crossover_protocol::layout`'s docs for where that lives.

#![no_main]

use crossover_protocol::LayoutSync;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = LayoutSync::decode_payload(data) {
        let encoded = message.encode_payload().expect("layout sync must re-encode");
        assert_eq!(
            LayoutSync::decode_payload(&encoded).expect("layout sync must re-decode"),
            message
        );
    }
});
