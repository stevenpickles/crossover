//! Fuzz the small clipboard control messages (Offer/Accept/Decline/
//! Applied) — one target, since their grammars are tiny and similar.

#![no_main]

use crossover_protocol::{ClipboardAccept, ClipboardApplied, ClipboardDecline, ClipboardOffer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = ClipboardOffer::decode_payload(data) {
        let encoded = message.encode_payload().expect("offer must re-encode");
        assert_eq!(
            ClipboardOffer::decode_payload(&encoded).expect("offer must re-decode"),
            message
        );
    }
    if let Ok(message) = ClipboardAccept::decode_payload(data) {
        let encoded = message.encode_payload().expect("accept must re-encode");
        assert_eq!(
            ClipboardAccept::decode_payload(&encoded).expect("accept must re-decode"),
            message
        );
    }
    if let Ok(message) = ClipboardDecline::decode_payload(data) {
        let encoded = message.encode_payload().expect("decline must re-encode");
        assert_eq!(
            ClipboardDecline::decode_payload(&encoded).expect("decline must re-decode"),
            message
        );
    }
    if let Ok(message) = ClipboardApplied::decode_payload(data) {
        let encoded = message.encode_payload().expect("applied must re-encode");
        assert_eq!(
            ClipboardApplied::decode_payload(&encoded).expect("applied must re-decode"),
            message
        );
    }
});
