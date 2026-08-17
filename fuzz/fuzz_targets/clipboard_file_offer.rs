//! Fuzz the file-offer decode path (ADR 0015) — the only place a
//! peer-supplied *name* enters the system, and the one string of a file
//! transfer that later reaches a shell.
//!
//! Two properties, both fail-closed: arbitrary bytes must reject rather
//! than panic, and anything the decoder *accepts* must carry a name that
//! passes validation, so no descriptor with a hostile name can exist.

#![no_main]

use crossover_protocol::{ClipboardOffer, validate_file_name};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Names arrive as UTF-8 on the wire; validation must be total over
    // every one of them, including the astral planes and every control
    // and format character.
    if let Ok(name) = std::str::from_utf8(data) {
        let _ = validate_file_name(name);
    }

    let Ok(offer) = ClipboardOffer::decode_payload(data) else {
        return;
    };
    let encoded = offer.encode_payload().expect("offer must re-encode");
    assert_eq!(
        ClipboardOffer::decode_payload(&encoded).expect("offer must re-decode"),
        offer,
        "ClipboardOffer round trip must be lossless"
    );
    if let Some(descriptor) = &offer.descriptor {
        assert!(
            validate_file_name(&descriptor.file_name).is_ok(),
            "an accepted offer carried a name that does not validate"
        );
    }
});
