//! Fuzz `PairingConfirm` payload decoding (same goals as `hello`).

#![no_main]

use crossover_protocol::PairingConfirm;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = PairingConfirm::decode_payload(data) {
        let encoded = message
            .encode_payload()
            .expect("a decoded PairingConfirm must re-encode");
        let again = PairingConfirm::decode_payload(&encoded)
            .expect("a re-encoded PairingConfirm must decode");
        assert_eq!(message, again, "PairingConfirm round trip must be lossless");
    }
});
