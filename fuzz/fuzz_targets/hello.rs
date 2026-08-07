//! Fuzz `Hello` payload decoding.
//!
//! Beyond no-panic: anything that decodes must survive an
//! encode → decode round trip unchanged (re-decode equality — safe to
//! assert regardless of input canonicality).

#![no_main]

use crossover_protocol::Hello;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(hello) = Hello::decode_payload(data) {
        let encoded = hello
            .encode_payload()
            .expect("a decoded Hello must re-encode");
        let again = Hello::decode_payload(&encoded).expect("a re-encoded Hello must decode");
        assert_eq!(hello, again, "Hello round trip must be lossless");
    }
});
