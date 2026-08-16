//! Fuzz `ClipboardChunk` decoding and the reassembly accounting behind
//! it (ADR 0014) — the parse path that feeds a 64 MiB buffer, so both
//! the per-chunk bounds and the cross-chunk arithmetic must reject
//! rather than panic, overflow, or over-allocate.

#![no_main]

use crossover_protocol::clipboard::{ChunkReassembly, ClipboardMeta, ContentType, ImageFormat};
use crossover_protocol::{ClipboardChunk, ProtocolError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(chunk) = ClipboardChunk::decode_payload(data) else {
        return;
    };
    let encoded = chunk.encode_payload().expect("chunk must re-encode");
    assert_eq!(
        ClipboardChunk::decode_payload(&encoded).expect("chunk must re-decode"),
        chunk
    );

    // Feed the decoded chunk into a reassembly whose declared length is
    // derived from the fuzzed bytes: most combinations are rejections,
    // and every one of them must be a value rather than a panic.
    let declared = u64::from(chunk.index)
        .saturating_mul(u64::from(u32::try_from(chunk.payload.len()).unwrap_or(u32::MAX)))
        .saturating_add(1);
    let meta = ClipboardMeta {
        id: chunk.id,
        // Reusing the item id keeps this target dependency-free; origin
        // plays no part in chunk accounting.
        origin: chunk.id,
        sequence: 0,
        content_type: ContentType::Image(ImageFormat::Dib),
        content_length: declared,
        content_hash: [0u8; 32],
    };
    if let Ok(mut reassembly) = ChunkReassembly::begin(meta) {
        // Repeat it: the same chunk twice must fail closed on the second.
        let first = reassembly.accept(&chunk);
        if first.is_ok() {
            assert!(
                matches!(
                    reassembly.accept(&chunk),
                    Err(ProtocolError::Malformed { .. })
                ),
                "a repeated chunk index must be rejected"
            );
        }
    }
});
