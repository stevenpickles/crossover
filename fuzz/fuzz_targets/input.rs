//! Fuzz the input message decoders.
//!
//! `InputBatch` is the one message a hostile peer can use to make us
//! allocate a lot cheaply — a short payload can declare many events — so
//! the bound must hold against crafted bytes, not just against our own
//! encoder.

#![no_main]

use crossover_protocol::{InputBatch, ReleaseAllInput};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(batch) = InputBatch::decode_payload(data) {
        assert!(
            !batch.events.is_empty()
                && batch.events.len() <= crossover_protocol::input::MAX_INPUT_BATCH_EVENTS,
            "a decoded batch escaped its bounds"
        );
        let encoded = batch
            .encode_payload()
            .expect("a decoded InputBatch must re-encode");
        let again =
            InputBatch::decode_payload(&encoded).expect("a re-encoded InputBatch must decode");
        assert_eq!(batch, again, "InputBatch round trip must be lossless");
    }

    if let Ok(release) = ReleaseAllInput::decode_payload(data) {
        let encoded = release
            .encode_payload()
            .expect("a decoded ReleaseAllInput must re-encode");
        assert_eq!(
            ReleaseAllInput::decode_payload(&encoded).expect("must re-decode"),
            release
        );
    }
});
