//! Fuzz `PairingStart` payload decoding (same goals as `hello`).

#![no_main]

use crossover_protocol::PairingStart;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = PairingStart::decode_payload(data) {
        let encoded = message
            .encode_payload()
            .expect("a decoded PairingStart must re-encode");
        let again =
            PairingStart::decode_payload(&encoded).expect("a re-encoded PairingStart must decode");
        assert_eq!(message, again, "PairingStart round trip must be lossless");
    }
});
