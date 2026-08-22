//! Fuzz the `MonitorTopology` payload decoder (ADR 0018, docs/PROTOCOL.md
//! §6.2): a peer's claimed local monitors. A truncated payload, a count
//! past `MAX_MONITORS_PER_MACHINE`, a zero or oversized rectangle, an
//! out-of-range coordinate, a `scale_percent` outside its bound, a
//! duplicate monitor id, an unusable monitor id, an unusable monitor
//! label — over `MAX_MONITOR_LABEL_BYTES`, carrying a control character,
//! or not valid UTF-8 at all — or an unusable physical size — zero or over
//! `MAX_PHYSICAL_SIZE_MM` on either axis, or a millimetre count past what
//! the field can even hold — must all reject, never panic (NFR-1).
//!
//! The round trip below is what makes the two optional fields interesting
//! to fuzz rather than merely to decode. A label is the one field where
//! decode admits more shapes than an id does (UTF-8 rather than ASCII), and
//! a physical size is the one whose *encoded* form is two unframed LEB128
//! runs back to back, where a decoder reading one boundary differently from
//! the encoder would silently shift every byte after it. "Whatever decoded
//! must re-encode to itself" is the property that catches either.

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
