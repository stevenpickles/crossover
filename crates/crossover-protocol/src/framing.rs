//! Explicit frame codec (docs/PROTOCOL.md §2).
//!
//! Wire layout, all integers big-endian, independent of TCP packet
//! boundaries:
//!
//! ```text
//! length        u32   — byte length of the body that follows
//! body:
//!   message_type  u16
//!   message_id    u64   — monotonic per session (logging, duplicate detection)
//!   payload       [u8]  — serialized message (postcard, ADR 0001)
//! ```
//!
//! NFR-1 discipline: the declared length is validated against
//! [`MAX_FRAME_BODY_BYTES`] as soon as the four-byte prefix arrives —
//! before any payload is buffered — and the decoder's internal buffer is
//! itself bounded. Malformed framing is fatal to the session (fail
//! closed, docs/PROTOCOL.md §7); the *unknown message type* case is
//! deliberately not a framing error, so higher layers can skip unknown
//! messages when version negotiation permits.

use crate::ProtocolError;

/// Size of the length prefix, in bytes.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Fixed body header: `message_type` (2) + `message_id` (8).
pub const BODY_HEADER_BYTES: usize = 10;

/// Maximum accepted frame body (header + payload): one maximum clipboard
/// item (4 MiB, ADR 0005) plus envelope headroom, so a full item always
/// fits a single frame and no chunking/reassembly state exists. Safe
/// against NFR-1 because the declared length is validated before
/// allocation and the decode buffer is capped.
pub const MAX_FRAME_BODY_BYTES: usize = 4 * 1024 * 1024 + 64 * 1024;

/// Maximum payload bytes a single frame can carry.
pub const MAX_PAYLOAD_BYTES: usize = MAX_FRAME_BODY_BYTES - BODY_HEADER_BYTES;

/// Hard cap on the decoder's internal buffer. One maximum frame plus its
/// prefix, with headroom for the next frame's prefix to arrive in the same
/// read; a peer that streams past this without completing a frame is
/// violating the protocol.
const MAX_BUFFERED_BYTES: usize = MAX_FRAME_BODY_BYTES + 2 * LENGTH_PREFIX_BYTES;

/// One decoded frame, payload still serialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Wire message type. Deliberately raw (`u16`, not an enum): the
    /// framing layer does not decide which types exist — that is version-
    /// negotiation policy at the session layer.
    pub message_type: u16,
    /// Sender-assigned, monotonically increasing per session.
    pub message_id: u64,
    /// Serialized message body (postcard, ADR 0001).
    pub payload: Vec<u8>,
}

/// Encode one frame.
///
/// # Errors
///
/// [`ProtocolError::FrameTooLarge`] if `payload` exceeds
/// [`MAX_PAYLOAD_BYTES`]. Outbound and inbound frames obey the same bound:
/// we never send what we would refuse to receive.
pub fn encode_frame(
    message_type: u16,
    message_id: u64,
    payload: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            declared: (BODY_HEADER_BYTES + payload.len()) as u64,
            max: MAX_FRAME_BODY_BYTES as u64,
        });
    }
    let body_len = BODY_HEADER_BYTES + payload.len();
    // Unreachable after the guard above (MAX_FRAME_BODY_BYTES < u32::MAX),
    // but the codec has no panic paths: impossible states fail closed.
    let Ok(body_len_u32) = u32::try_from(body_len) else {
        return Err(ProtocolError::FrameTooLarge {
            declared: body_len as u64,
            max: MAX_FRAME_BODY_BYTES as u64,
        });
    };
    let mut frame = Vec::with_capacity(LENGTH_PREFIX_BYTES + body_len);
    frame.extend_from_slice(&body_len_u32.to_be_bytes());
    frame.extend_from_slice(&message_type.to_be_bytes());
    frame.extend_from_slice(&message_id.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Incremental decoder over an arbitrary byte stream.
///
/// Feed bytes with [`FrameDecoder::extend`], drain complete frames with
/// [`FrameDecoder::next_frame`]. Any error is terminal for the session;
/// the decoder is not meant to be reused after one.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// A decoder with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] if buffering would exceed the internal
    /// cap — callers must drain [`FrameDecoder::next_frame`] between reads,
    /// so hitting the cap means the peer is streaming garbage (NFR-1: no
    /// unbounded buffering of untrusted input).
    pub fn extend(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.buf.len().saturating_add(bytes.len()) > MAX_BUFFERED_BYTES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "decode buffer would exceed {MAX_BUFFERED_BYTES} bytes; \
                     frames must be drained between reads"
                ),
            });
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Pop the next complete frame, or `Ok(None)` if more bytes are needed.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::FrameTooLarge`] the moment a length prefix declares
    /// a body over [`MAX_FRAME_BODY_BYTES`] — the payload is never awaited,
    /// let alone allocated. [`ProtocolError::Malformed`] for a declared
    /// body too short to hold the body header.
    pub fn next_frame(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
        let Some(prefix) = self.buf.first_chunk::<LENGTH_PREFIX_BYTES>() else {
            return Ok(None);
        };
        let body_len = u32::from_be_bytes(*prefix) as usize;

        // Validate the declared length before waiting for (or allocating
        // room for) a single payload byte.
        if body_len > MAX_FRAME_BODY_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                declared: body_len as u64,
                max: MAX_FRAME_BODY_BYTES as u64,
            });
        }
        if body_len < BODY_HEADER_BYTES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "declared body length {body_len} is shorter than the \
                     {BODY_HEADER_BYTES}-byte body header"
                ),
            });
        }

        let frame_len = LENGTH_PREFIX_BYTES + body_len;
        if self.buf.len() < frame_len {
            return Ok(None);
        }

        let body = &self.buf[LENGTH_PREFIX_BYTES..frame_len];
        // Unreachable (body_len >= BODY_HEADER_BYTES was checked), but the
        // codec has no panic paths: impossible states fail closed.
        let (Some(type_bytes), Some(id_bytes)) = (
            body.first_chunk::<2>(),
            body.get(2..BODY_HEADER_BYTES)
                .and_then(|s| s.first_chunk::<8>()),
        ) else {
            return Err(ProtocolError::Malformed {
                reason: "frame body shorter than its validated header".to_owned(),
            });
        };
        let frame = RawFrame {
            message_type: u16::from_be_bytes(*type_bytes),
            message_id: u64::from_be_bytes(*id_bytes),
            payload: body[BODY_HEADER_BYTES..].to_vec(),
        };
        self.buf.drain(..frame_len);
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        BODY_HEADER_BYTES, FrameDecoder, LENGTH_PREFIX_BYTES, MAX_FRAME_BODY_BYTES,
        MAX_PAYLOAD_BYTES, RawFrame, encode_frame,
    };
    use crate::ProtocolError;

    fn decode_all(bytes: &[u8]) -> Vec<RawFrame> {
        let mut decoder = FrameDecoder::new();
        decoder.extend(bytes).unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().unwrap() {
            frames.push(frame);
        }
        frames
    }

    #[test]
    fn round_trip_single_frame() {
        let encoded = encode_frame(7, 42, b"payload").unwrap();
        let frames = decode_all(&encoded);
        assert_eq!(
            frames,
            vec![RawFrame {
                message_type: 7,
                message_id: 42,
                payload: b"payload".to_vec(),
            }]
        );
    }

    #[test]
    fn empty_payload_is_valid() {
        let encoded = encode_frame(1, 1, b"").unwrap();
        assert_eq!(encoded.len(), LENGTH_PREFIX_BYTES + BODY_HEADER_BYTES);
        assert_eq!(decode_all(&encoded)[0].payload, Vec::<u8>::new());
    }

    #[test]
    fn two_frames_in_one_read_decode_in_order() {
        let mut bytes = encode_frame(1, 1, b"first").unwrap();
        bytes.extend(encode_frame(2, 2, b"second").unwrap());
        let frames = decode_all(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].payload, b"first");
        assert_eq!(frames[1].payload, b"second");
    }

    #[test]
    fn byte_by_byte_delivery_decodes_identically() {
        // The protocol must never depend on TCP packet boundaries
        // (docs/PROTOCOL.md §2): the worst fragmentation — one byte per
        // read — must yield the same frames.
        let encoded = encode_frame(9, 100, b"fragmented").unwrap();
        let mut decoder = FrameDecoder::new();
        let mut frames = Vec::new();
        for byte in &encoded {
            decoder.extend(std::slice::from_ref(byte)).unwrap();
            while let Some(frame) = decoder.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"fragmented");
    }

    #[test]
    fn truncated_frame_waits_rather_than_errors() {
        let encoded = encode_frame(1, 1, b"payload").unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.extend(&encoded[..encoded.len() - 1]).unwrap();
        assert_eq!(decoder.next_frame().unwrap(), None);
        // The final byte completes it.
        decoder.extend(&encoded[encoded.len() - 1..]).unwrap();
        assert!(decoder.next_frame().unwrap().is_some());
    }

    #[test]
    fn oversized_declared_length_rejected_from_prefix_alone() {
        let declared = u32::try_from(MAX_FRAME_BODY_BYTES + 1).unwrap();
        let mut decoder = FrameDecoder::new();
        // Only the 4-byte prefix — no payload ever arrives.
        decoder.extend(&declared.to_be_bytes()).unwrap();
        assert!(matches!(
            decoder.next_frame(),
            Err(ProtocolError::FrameTooLarge { declared: d, .. }) if d == u64::from(declared)
        ));
    }

    #[test]
    fn declared_length_below_body_header_is_malformed() {
        let mut decoder = FrameDecoder::new();
        decoder.extend(&5u32.to_be_bytes()).unwrap();
        assert!(matches!(
            decoder.next_frame(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn encode_refuses_oversized_payload() {
        let payload = vec![0u8; MAX_PAYLOAD_BYTES + 1];
        assert!(matches!(
            encode_frame(1, 1, &payload),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
        // The boundary itself is fine.
        assert!(encode_frame(1, 1, &vec![0u8; MAX_PAYLOAD_BYTES]).is_ok());
    }

    #[test]
    fn undrained_buffer_hits_the_cap_instead_of_growing() {
        let mut decoder = FrameDecoder::new();
        let chunk = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        loop {
            match decoder.extend(&chunk) {
                Ok(()) => {
                    total += chunk.len();
                    assert!(total <= 2 * MAX_FRAME_BODY_BYTES, "cap never enforced");
                }
                Err(ProtocolError::Malformed { .. }) => break,
                Err(other) => panic!("unexpected error: {other}"),
            }
        }
    }

    proptest! {
        /// Any sequence of frames survives encode → arbitrary re-chunking →
        /// decode, byte-identically and in order.
        #[test]
        fn frames_survive_arbitrary_rechunking(
            messages in proptest::collection::vec(
                (any::<u16>(), any::<u64>(), proptest::collection::vec(any::<u8>(), 0..512)),
                0..8,
            ),
            chunk_size in 1usize..64,
        ) {
            let mut stream = Vec::new();
            for (ty, id, payload) in &messages {
                stream.extend(encode_frame(*ty, *id, payload).unwrap());
            }

            let mut decoder = FrameDecoder::new();
            let mut decoded = Vec::new();
            for chunk in stream.chunks(chunk_size) {
                decoder.extend(chunk).unwrap();
                while let Some(frame) = decoder.next_frame().unwrap() {
                    decoded.push((frame.message_type, frame.message_id, frame.payload));
                }
            }
            prop_assert_eq!(decoded, messages);
        }

        /// Arbitrary byte soup never panics the decoder: every outcome is a
        /// frame, a wait, or a typed error (NFR-1).
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut decoder = FrameDecoder::new();
            if decoder.extend(&bytes).is_ok() {
                while let Ok(Some(_frame)) = decoder.next_frame() {}
            }
        }
    }
}
