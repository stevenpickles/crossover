//! Fuzz `canonical_dib` (crossover-platform-windows `dib`, ADR 0016's
//! 2026-09-28 amendment): the one piece of image arithmetic peer-originated
//! bytes reach. After a peer's DIB is installed, the read that follows
//! Crossover's own write canonicalizes it, so its input is whatever a
//! trusted-but-possibly-buggy peer — or anything else on the machine —
//! put on the clipboard.
//!
//! Beyond "never panic" (NFR-1), three properties, each of which a
//! regression would break silently:
//!
//! - **It only ever trims.** The output is a prefix of the input: nothing
//!   is invented, and the pixels that remain are the pixels that came in.
//! - **It is a fixed point.** Canonicalizing twice changes nothing more.
//!   This is loop prevention's whole requirement (FR-3.3): a blob that
//!   read back differently on every hop would look like a new copy after
//!   Crossover's own write — a clipboard sync loop, release-blocking.
//! - **A trimmed length is the header's own.** When it trims, the length it
//!   keeps is exactly what `dib_logical_len` computed for the input.

#![no_main]

use crossover_platform_windows::dib::{canonical_dib, dib_logical_len};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let once = canonical_dib(data.to_vec());
    assert!(data.starts_with(&once), "canonical_dib invented bytes");
    assert_eq!(canonical_dib(once.clone()), once, "not a fixed point");
    if once.len() < data.len() {
        assert_eq!(dib_logical_len(data), Some(once.len()));
    }
});
