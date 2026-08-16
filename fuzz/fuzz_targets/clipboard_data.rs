//! Fuzz `ClipboardData` decoding — the largest and most-validated
//! payload: bounds, declared-vs-actual length, hash, and UTF-8 must all
//! agree, and disagreement must reject rather than panic or allocate.

#![no_main]

use crossover_protocol::ClipboardData;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = ClipboardData::decode_payload(data) {
        let encoded = message
            .encode_payload()
            .expect("a decoded ClipboardData must re-encode");
        let again = ClipboardData::decode_payload(&encoded)
            .expect("a re-encoded ClipboardData must decode");
        assert_eq!(message, again, "ClipboardData round trip must be lossless");
    }
});
