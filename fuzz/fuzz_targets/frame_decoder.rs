//! Fuzz the frame decoder: arbitrary bytes, fuzzer-chosen chunking.
//!
//! Goals (docs/TESTING.md §1.3): no panic, no unbounded allocation, no
//! state corruption — every outcome is a frame, a wait, or a typed error.

#![no_main]

use crossover_protocol::FrameDecoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first byte seeds the chunk size so the fuzzer explores
    // re-chunking behavior (never-depend-on-TCP-boundaries) as well as
    // content.
    let chunk = usize::from(data.first().copied().unwrap_or(1)).max(1);
    let mut decoder = FrameDecoder::new();
    for part in data.chunks(chunk) {
        if decoder.extend(part).is_err() {
            return;
        }
        loop {
            match decoder.next_frame() {
                Ok(Some(_frame)) => {}
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});
