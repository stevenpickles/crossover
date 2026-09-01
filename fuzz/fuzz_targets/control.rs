//! Fuzz the v4 control-transfer payloads (ADR 0018, docs/PROTOCOL.md
//! §6.1): `ControlRequest` and `ControlRelease`, each carrying an
//! `Option<EntryPoint>`. `EntryPoint` has no wire message of its own — it
//! only ever travels nested here — so fuzzing these two decoders is what
//! exercises its decode and validation: a truncated payload, a monitor id
//! over `MAX_MONITOR_ID_BYTES` or containing a non-printable byte, or an
//! unknown `Edge` discriminant must all reject, never panic (NFR-1).

#![no_main]

use crossover_protocol::{ControlRelease, ControlRequest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = ControlRequest::decode_payload(data) {
        let encoded = message.encode_payload().expect("request must re-encode");
        assert_eq!(
            ControlRequest::decode_payload(&encoded).expect("request must re-decode"),
            message
        );
    }
    if let Ok(message) = ControlRelease::decode_payload(data) {
        let encoded = message.encode_payload().expect("release must re-encode");
        assert_eq!(
            ControlRelease::decode_payload(&encoded).expect("release must re-decode"),
            message
        );
    }
});
