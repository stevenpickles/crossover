//! Clipboard transaction messages (ADR 0005, ADR 0014, docs/PROTOCOL.md §5).
//!
//! Three flows share these messages:
//!
//! - **inline** (`Data` → `Applied`) for text at or below
//!   [`CLIPBOARD_INLINE_MAX_BYTES`];
//! - **offered** (`Offer` → `Accept`/`Decline` → `Data` → `Applied`) for
//!   text above it;
//! - **offered and chunked** (`Offer` → `Accept`/`Decline` →
//!   `Chunk`×N → `Applied`) for the types [`ContentType::is_chunked`]
//!   marks — images (ADR 0014) and files (ADR 0015).
//!
//! The non-negotiable semantic lives in `Applied`: a sync succeeded only
//! if the destination OS clipboard was updated (FR-3.2).
//!
//! Wire-level validation is deliberately strong: a `ClipboardData` whose
//! declared length, hash, or UTF-8 validity disagrees with its content is
//! rejected at decode — corrupt items are unrepresentable past the
//! parser, so the engine's dedup and loop prevention can trust every item
//! identity they see. Chunked items keep that guarantee by *construction*
//! rather than per-message: a chunk is only ever a fragment, so
//! [`ChunkReassembly`] is the sole way to obtain the bytes and it hands
//! them out only after the item's `content_hash` verifies over the whole
//! reassembly (ADR 0014 — nothing partially-verified reaches the OS
//! clipboard).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::ProtocolError;
use crate::decode_strict;
use crate::hello::{FeatureFlags, MessageType};

/// SHA-256 of clipboard content — the identity that dedup and loop
/// prevention key on. Exposed so the engine hashes local observations
/// identically to the wire layer without its own crypto dependency.
#[must_use]
pub fn content_hash(content: &[u8]) -> [u8; 32] {
    Sha256::digest(content).into()
}

/// Text at or below this rides the inline flow; above it, the offered
/// flow (ADR 0005). Chunked types ignore this threshold entirely — see
/// [`ClipboardOffer::validate`].
pub const CLIPBOARD_INLINE_MAX_BYTES: usize = 64 * 1024;

/// Hard cap on text clipboard content. Larger items are rejected
/// gracefully on both send and receive (FR-3.6) — never truncated.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Hard cap on image clipboard content (ADR 0014, FR-3.6).
///
/// Images travel as the source's native raster bytes, verbatim and
/// uncompressed, so the ceiling has to cover a full-screen grab in that
/// form rather than a codec's idea of one. At 32 bits per pixel a 4K
/// screenshot is 3840 × 2160 × 4 = 31.6 MiB, and a dual-4K span
/// (7680 × 2160 × 4) is 63.3 MiB — so 64 MiB admits the worst realistic
/// screenshot with margin, while the maintainer's actual case (snips of a
/// few MB, ADR 0014) sits two orders of magnitude below it.
///
/// It is also the receiver's memory commitment: the reassembly buffer is
/// sized from the *offered* length, which is validated against this bound
/// **before** the allocation happens (NFR-1), and the engine holds one
/// reassembly at a time.
pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap on file clipboard content (ADR 0015, FR-3.6).
///
/// One clipboard item is one blob: a single file verbatim, or one zip
/// archive built by the sender for a folder or a multi-entry selection.
/// 256 MiB covers documents, archives and photo sets — this is a
/// convenience feature, not a file-sync product — and a selection over it
/// is refused observably rather than truncated.
///
/// Unlike an image, this is **not** a memory commitment: file content is
/// written straight through to the receiver's spool as chunks arrive, so
/// the receiver holds one chunk, never the item (which is why
/// [`ChunkReassembly`] refuses this type outright). It bounds the wire
/// and the disk instead.
pub const MAX_CLIPBOARD_FILE_BYTES: usize = 256 * 1024 * 1024;

/// Maximum number of filesystem entries one archived item may pack
/// (ADR 0015). The sender refuses a larger selection before any bytes
/// leave; the receiver refuses a descriptor that claims more.
pub const MAX_CLIPBOARD_FILE_ENTRIES: u32 = 256;

/// Maximum payload bytes in one [`ClipboardChunk`].
///
/// A chunk is the *preemption unit* (ADR 0013): the writer emits at most
/// one background chunk before re-checking the interactive lane, so the
/// worst-case delay a live keystroke can suffer behind a bulk transfer is
/// roughly one chunk's transmit time. The arithmetic that fixes the value:
///
/// | Link | Bytes/ms | 64 KiB chunk |
/// |------|----------|--------------|
/// | 2.5 `GbE` | 312 500 | 0.21 ms |
/// | 1 `GbE` | 125 000 | 0.52 ms |
///
/// Both are sub-millisecond, which is ADR 0013's budget, and the same
/// chunk keeps per-message overhead negligible: a 64 MiB image is 1024
/// chunks whose envelopes (frame header plus postcard fields, well under
/// 64 bytes each) total under 0.1 % of the payload. Smaller chunks would
/// buy latency nobody can perceive at the cost of more messages; larger
/// ones eat directly into the input-latency budget.
///
/// Far below [`crate::framing::MAX_FRAME_BODY_BYTES`], which does **not**
/// grow for chunking (ADR 0014).
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// [`MAX_CHUNK_BYTES`] as the `u32` the chunk arithmetic works in, so the
/// bound is stated once and the two forms cannot drift.
const MAX_CHUNK_BYTES_U32: u32 = 64 * 1024;
const _: () = assert!(MAX_CHUNK_BYTES_U32 as usize == MAX_CHUNK_BYTES);

/// Maximum number of chunks one item may be split into.
///
/// Derived, not chosen: the largest chunked item divided by the largest
/// chunk. That is 256 MiB ÷ 64 KiB since ADR 0015 — files are the largest
/// chunked type, and a bound that could not carry one would refuse a
/// conforming transfer rather than an abusive one. It is what stops a
/// peer from declaring a legal-looking transfer made of millions of tiny
/// chunks — a sender that picks a smaller chunk size simply has to keep
/// the *count* inside this bound, which [`ChunkPlan::derive`] enforces
/// before a single byte is buffered.
///
/// Raising it does not raise what any transfer may cost: a plan must
/// reconcile *exactly* with the offered `content_length`, which is itself
/// bounded per type, so the count a given item is allowed is still its
/// own length divided by its own chunk size.
pub const MAX_CHUNK_COUNT: u32 = 4096;

/// Keep [`MAX_CHUNK_COUNT`] tied to the constants it is derived from: if
/// any of them moves, the build fails here rather than silently admitting
/// a maximum item that cannot be split inside the count bound. One
/// assertion per chunked type, so adding a type without revisiting the
/// bound is a compile error.
const _: () = assert!(
    (MAX_CHUNK_COUNT as usize) * MAX_CHUNK_BYTES >= MAX_CLIPBOARD_IMAGE_BYTES,
    "MAX_CHUNK_COUNT chunks of MAX_CHUNK_BYTES must cover MAX_CLIPBOARD_IMAGE_BYTES"
);
const _: () = assert!(
    (MAX_CHUNK_COUNT as usize) * MAX_CHUNK_BYTES >= MAX_CLIPBOARD_FILE_BYTES,
    "MAX_CHUNK_COUNT chunks of MAX_CHUNK_BYTES must cover MAX_CLIPBOARD_FILE_BYTES"
);

/// Raster formats an image item may carry (ADR 0014).
///
/// The bytes are whatever the source clipboard held, verbatim: Crossover
/// neither transcodes nor compresses, so the wire is *format-tagged*
/// rather than format-normalized. `Dib` is the default because it is the
/// format Windows applications near-universally both provide and accept;
/// the compressed tags exist for sources that offer nothing else.
///
/// The tag is descriptive metadata, not a promise about the bytes: no
/// component parses image data, and validation is length and hash only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    /// Windows device-independent bitmap (`CF_DIB`).
    Dib,
    /// PNG, verbatim.
    Png,
    /// JPEG, verbatim.
    Jpeg,
}

/// Clipboard content types.
///
/// Variants are **appended, never renumbered** — the postcard
/// discriminant is the wire value (ADR 0001), and `Utf8Text` must stay 0
/// for the golden snapshots below to keep meaning what they mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentType {
    /// UTF-8 text (FR-3.7) — the base protocol's only type.
    Utf8Text,
    /// A raster image in the source's own format (ADR 0014). Gated by
    /// [`FeatureFlags::CHUNKED_CLIPBOARD`]; always offered, always
    /// chunked.
    Image(ImageFormat),
    /// One file's bytes, or one zip archive the sender built from a
    /// folder or a multi-entry selection (ADR 0015). Gated by
    /// [`FeatureFlags::FILE_CLIPBOARD`]; always offered, always chunked.
    ///
    /// The item's *name* does not live here — it rides
    /// [`ClipboardOffer::descriptor`], because [`ClipboardMeta`] stays
    /// `Copy` and fixed-size and a name is neither. Appended after
    /// `Image`: discriminants are wire values and are never renumbered.
    File,
}

impl ContentType {
    /// The size ceiling for this type (FR-3.6). Per-type since ADR 0014:
    /// a 64 MiB image bound would be absurd for text, and a 4 MiB text
    /// bound would reject ordinary screenshots.
    #[must_use]
    pub const fn max_content_bytes(self) -> u64 {
        match self {
            Self::Utf8Text => MAX_CLIPBOARD_TEXT_BYTES as u64,
            Self::Image(_) => MAX_CLIPBOARD_IMAGE_BYTES as u64,
            Self::File => MAX_CLIPBOARD_FILE_BYTES as u64,
        }
    }

    /// Whether items of this type travel as [`ClipboardChunk`]s after an
    /// accepted offer, rather than in a single [`ClipboardData`].
    ///
    /// Chunked types are **always** offered — at any size, the inline
    /// threshold notwithstanding — because the offer round is where the
    /// receiver's `AlreadyHave` decline short-circuits a re-paste to zero
    /// payload bytes, and where it bounds its memory before megabytes
    /// arrive (ADR 0014).
    #[must_use]
    pub const fn is_chunked(self) -> bool {
        matches!(self, Self::Image(_) | Self::File)
    }

    /// Whether this type carries a [`FileDescriptor`] on its offer.
    ///
    /// Exactly the file type, and stated as a predicate so the "a file
    /// offer has a descriptor, nothing else does" rule is written once
    /// and enforced in both directions.
    #[must_use]
    pub const fn needs_file_descriptor(self) -> bool {
        matches!(self, Self::File)
    }

    /// The capability a peer must advertise in its `Hello` before this
    /// type may be sent to it (docs/PROTOCOL.md §3).
    ///
    /// [`FeatureFlags::NONE`] for the base protocol's types, which every
    /// peer at the negotiated version understands.
    #[must_use]
    pub const fn required_feature(self) -> FeatureFlags {
        match self {
            Self::Utf8Text => FeatureFlags::NONE,
            Self::Image(_) => FeatureFlags::CHUNKED_CLIPBOARD,
            // A separate bit from images, and necessarily so: an ADR 0014
            // peer advertises CHUNKED_CLIPBOARD and has no `File`
            // discriminant, so sending it one is fatal to its session
            // rather than skippable (docs/PROTOCOL.md §3.1).
            Self::File => FeatureFlags::FILE_CLIPBOARD,
        }
    }

    /// Whether a peer advertising `features` can receive this type.
    ///
    /// The sender-side gate. Unknown message types are *ignored* rather
    /// than fatal (docs/PROTOCOL.md §2), so offering chunked content to a
    /// peer that does not understand it would produce no answer at all —
    /// a silent stall, which NFR-3 forbids. Nothing is sent that the peer
    /// has not said it can take.
    #[must_use]
    pub const fn negotiated_by(self, features: FeatureFlags) -> bool {
        features.contains(self.required_feature())
    }
}

/// The identity and description of one clipboard item, shared by
/// [`ClipboardOffer`] and [`ClipboardData`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardMeta {
    /// Globally unique item id, minted by the origin at observation.
    pub id: Uuid,
    /// Origin peer's device id (bookkeeping and diagnostics; loop
    /// prevention keys on `content_hash`, never on this).
    pub origin: Uuid,
    /// Origin-local observation counter (conflict ordering, FR-3.5).
    pub sequence: u64,
    /// What the content is.
    pub content_type: ContentType,
    /// Exact content byte length. Validated against the actual content
    /// in `ClipboardData` and against bounds everywhere.
    pub content_length: u64,
    /// SHA-256 of the content (integrity, dedup, loop prevention).
    pub content_hash: [u8; 32],
}

impl ClipboardMeta {
    /// Per-type bounds (ADR 0014): the declared length is checked against
    /// *this type's* maximum, and it is checked before anything is sized
    /// from it (NFR-1).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for an out-of-range declared length,
    /// or an empty item of a chunked type (a zero-byte image is not an
    /// image, and it would leave the chunk arithmetic with nothing to
    /// reconcile).
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let max = self.content_type.max_content_bytes();
        if self.content_length > max {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "clipboard content length {} exceeds the maximum {max} for {:?}",
                    self.content_length, self.content_type
                ),
            });
        }
        if self.content_type.is_chunked() && self.content_length == 0 {
            return Err(ProtocolError::Malformed {
                reason: format!("empty {:?} clipboard item", self.content_type),
            });
        }
        Ok(())
    }
}

/// What a file item is, beyond its bytes (ADR 0015).
///
/// It rides [`ClipboardOffer`] rather than [`ClipboardMeta`] because the
/// meta is the engine's working currency: fixed-size and `Copy`, which a
/// variable-length name cannot be.
///
/// The whole struct is peer-controlled, and `file_name` is the field that
/// reaches a shell, so it is validated as network input here — the same
/// check that runs again before a descriptor is built for the OS. A
/// descriptor that does not validate makes its offer malformed, so a
/// hostile name is unrepresentable past the parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDescriptor {
    /// The bare file name to present at the destination — never a path.
    /// Validated by [`crate::validate_file_name`].
    pub file_name: String,
    /// Whether the blob is a zip archive the sender built (a folder or a
    /// multi-entry selection), rather than one file verbatim.
    pub archived: bool,
    /// How many filesystem entries the blob packs, at most
    /// [`MAX_CLIPBOARD_FILE_ENTRIES`]. Exactly 1 when not archived.
    pub entry_count: u32,
    /// Total uncompressed bytes of those entries, for the user-facing
    /// report. Not a promise about the blob: nothing in Crossover reads
    /// an archive, so this number is never used to size anything.
    pub original_bytes: u64,
}

impl FileDescriptor {
    /// Everything a descriptor can be judged on alone: a conforming name,
    /// an entry count inside its bound, and an entry count that agrees
    /// with `archived`.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for an invalid name (naming the
    /// fault, never the name), a zero or over-large `entry_count`, a
    /// multi-entry blob that claims not to be an archive, or
    /// `original_bytes` past [`MAX_CLIPBOARD_FILE_BYTES`].
    pub fn validate(&self) -> Result<(), ProtocolError> {
        crate::validate_file_name(&self.file_name)?;
        if self.entry_count == 0 {
            return Err(ProtocolError::Malformed {
                reason: "file descriptor packing no entries".to_owned(),
            });
        }
        if self.entry_count > MAX_CLIPBOARD_FILE_ENTRIES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "file descriptor packs {} entries, over the {MAX_CLIPBOARD_FILE_ENTRIES}-entry \
                     maximum",
                    self.entry_count
                ),
            });
        }
        if self.entry_count > 1 && !self.archived {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "file descriptor packs {} entries without being an archive",
                    self.entry_count
                ),
            });
        }
        if self.original_bytes > MAX_CLIPBOARD_FILE_BYTES as u64 {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "file descriptor declares {} original bytes, over the \
                     {MAX_CLIPBOARD_FILE_BYTES}-byte maximum",
                    self.original_bytes
                ),
            });
        }
        Ok(())
    }
}

/// Announce a large item without its content: the receiver decides
/// whether the bytes should travel at all.
///
/// Not `Copy` since ADR 0015: a file offer carries a variable-length
/// [`FileDescriptor`]. [`ClipboardMeta`] keeps `Copy`, which is what the
/// engine actually passes around.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardOffer {
    /// The offered item.
    pub meta: ClipboardMeta,
    /// The file half of the offer: `Some` for — and only for —
    /// [`ContentType::File`] (ADR 0015).
    pub descriptor: Option<FileDescriptor>,
}

impl ClipboardOffer {
    /// Semantic validation: for the types that have an inline flow, offers
    /// exist only above the inline threshold (ADR 0005) — a conforming
    /// peer never offers what it should send. The rule is type-aware since
    /// ADR 0014: a chunked type has no inline flow at all and is
    /// legitimately offered at any size.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for out-of-range lengths, for a
    /// non-chunked offer at or below the inline threshold, and for a
    /// descriptor that is missing, unexpected, invalid, or inconsistent
    /// with the offered length.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.meta.validate()?;
        if !self.meta.content_type.is_chunked()
            && self.meta.content_length <= CLIPBOARD_INLINE_MAX_BYTES as u64
        {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "offer for {} bytes at or below the {CLIPBOARD_INLINE_MAX_BYTES}-byte \
                     inline threshold",
                    self.meta.content_length
                ),
            });
        }
        self.validate_descriptor()
    }

    /// The descriptor rules of ADR 0015, in both directions: a file offer
    /// has a descriptor, no other offer has one, and the descriptor
    /// agrees with the item it describes.
    fn validate_descriptor(&self) -> Result<(), ProtocolError> {
        match (
            self.meta.content_type.needs_file_descriptor(),
            &self.descriptor,
        ) {
            (false, None) => Ok(()),
            (true, None) => Err(ProtocolError::Malformed {
                reason: "file offer without a file descriptor".to_owned(),
            }),
            (false, Some(_)) => Err(ProtocolError::Malformed {
                reason: format!("file descriptor on a {:?} offer", self.meta.content_type),
            }),
            (true, Some(descriptor)) => {
                descriptor.validate()?;
                // A single file travels verbatim and uncompressed
                // (ADR 0014's principle, kept by ADR 0015), so its
                // uncompressed size *is* the offered length. Only an
                // archive may declare a different one.
                if !descriptor.archived && descriptor.original_bytes != self.meta.content_length {
                    return Err(ProtocolError::Malformed {
                        reason: format!(
                            "unarchived file offer declares {} original bytes for {} content bytes",
                            descriptor.original_bytes, self.meta.content_length
                        ),
                    });
                }
                Ok(())
            }
        }
    }

    /// Whether this offer may be sent to a peer advertising `features`
    /// (docs/PROTOCOL.md §3).
    ///
    /// The sender's gate, checked before the offer travels: an
    /// un-negotiated content type would be answered by silence, because
    /// unknown types are skipped rather than fatal — and a transaction
    /// that never gets an answer is the silent failure NFR-3 forbids.
    #[must_use]
    pub const fn negotiated_by(&self, features: FeatureFlags) -> bool {
        self.meta.content_type.negotiated_by(features)
    }

    /// Encode the payload, validating first.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] on serialization failure.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        encode(self)
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or invalid payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "ClipboardOffer")?;
        message.validate()?;
        Ok(message)
    }
}

/// Accept an offered item: send the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardAccept {
    /// The item being accepted.
    pub id: Uuid,
}

/// Why an offered item was declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclineReason {
    /// The receiver already holds content with this hash — a
    /// synchronization *success* with zero payload bytes moved.
    AlreadyHave,
    /// The receiver will not take an item this large.
    TooLarge,
    /// The receiver cannot take an item right now.
    NotReady,
    /// A newer item (by the deterministic conflict order, FR-3.5) has
    /// superseded this one; synchronization converges on the newer item.
    /// A success-shaped outcome, not a failure.
    Superseded,
    /// The receiver does not handle this [`ContentType`]. A *permanent*
    /// answer, unlike [`DeclineReason::NotReady`]: the origin learns not
    /// to expect this type to travel, instead of waiting on an offer that
    /// will never be accepted (NFR-3). Appended after `Superseded` —
    /// discriminants are wire values and are never renumbered.
    UnsupportedType,
    /// The receiver has not been granted the permission this item needs
    /// — `file_receive` for a file item, which is default-off and not
    /// part of `PeerPermissions::FULL` (ADR 0015). Permanent for the
    /// session, and deliberately distinct from
    /// [`DeclineReason::UnsupportedType`]: the type is understood, the
    /// user simply has not consented to it.
    NotPermitted,
    /// The offered file name failed validation (ADR 0015). The name is
    /// never echoed back — the reason is the diagnostic.
    InvalidName,
    /// The receiver has no room: volume headroom below the required
    /// margin, or an item larger than the whole spool budget (ADR 0015).
    /// Distinct from [`DeclineReason::TooLarge`], which is about the
    /// item's own ceiling rather than this machine's free space.
    InsufficientSpace,
}

/// Decline an offered item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardDecline {
    /// The item being declined.
    pub id: Uuid,
    /// Why — typed, so the origin can distinguish success-equivalent
    /// declines from failures (NFR-3).
    pub reason: DeclineReason,
}

/// The item itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardData {
    /// The item's identity — every field validated against `content`.
    pub meta: ClipboardMeta,
    /// The content bytes.
    pub content: Vec<u8>,
}

impl ClipboardData {
    /// Build a consistent `ClipboardData` from content, computing length
    /// and hash — the only way a conforming sender should construct one.
    #[must_use]
    pub fn from_content(
        id: Uuid,
        origin: Uuid,
        sequence: u64,
        content_type: ContentType,
        content: Vec<u8>,
    ) -> Self {
        let digest = content_hash(&content);
        Self {
            meta: ClipboardMeta {
                id,
                origin,
                sequence,
                content_type,
                content_length: content.len() as u64,
                content_hash: digest,
            },
            content,
        }
    }

    /// Full consistency validation: bounds, declared length == actual,
    /// hash matches content, and per-type rules — valid UTF-8 for
    /// [`ContentType::Utf8Text`], and *no* `ClipboardData` at all for a
    /// chunked type (ADR 0014: those travel as [`ClipboardChunk`]s, and
    /// one path per type means no ambiguity about which validation ran).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for any inconsistency.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.meta.validate()?;
        if self.meta.content_length != self.content.len() as u64 {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "declared clipboard length {} but {} content bytes",
                    self.meta.content_length,
                    self.content.len()
                ),
            });
        }
        let digest: [u8; 32] = Sha256::digest(&self.content).into();
        if digest != self.meta.content_hash {
            return Err(ProtocolError::Malformed {
                reason: "clipboard content hash mismatch".to_owned(),
            });
        }
        match self.meta.content_type {
            ContentType::Utf8Text => {
                if std::str::from_utf8(&self.content).is_err() {
                    return Err(ProtocolError::Malformed {
                        reason: "Utf8Text content is not valid UTF-8".to_owned(),
                    });
                }
            }
            // No UTF-8 rule for binary types — and no ClipboardData
            // either: a chunked item is only ever assembled from chunks.
            ContentType::Image(_) | ContentType::File => {
                return Err(ProtocolError::Malformed {
                    reason: format!(
                        "{:?} content travels as chunks, not as ClipboardData",
                        self.meta.content_type
                    ),
                });
            }
        }
        Ok(())
    }

    /// Encode the payload, validating first.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] on serialization failure.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        encode(self)
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or inconsistent
    /// payloads — including hash and UTF-8 mismatches.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "ClipboardData")?;
        message.validate()?;
        Ok(message)
    }
}

/// One fragment of a chunked item (ADR 0014).
///
/// Deliberately **not** a [`ClipboardData`]: that message validates at
/// decode that its declared length equals the bytes it carries and that
/// the hash covers all of them, which a fragment can never satisfy. A
/// chunk therefore carries only what a fragment can prove about itself —
/// which item it belongs to, where it sits in the sequence, and its own
/// bounded payload. Everything cross-chunk (sizes reconciling with the
/// offered length, strictly sequential indices, and finally the item
/// hash) is [`ChunkReassembly`]'s to enforce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardChunk {
    /// The offered item this fragment belongs to. Chunks are
    /// offer-scoped: an id with no accepted offer behind it is a protocol
    /// violation, not an implicit new transfer.
    pub id: Uuid,
    /// Position in the sequence, from 0. Strictly sequential: a gap, a
    /// repeat, or an index past the item's chunk count is a protocol
    /// violation (fail closed, docs/PROTOCOL.md §7).
    pub index: u32,
    /// The fragment itself, at most [`MAX_CHUNK_BYTES`].
    pub payload: Vec<u8>,
}

impl ClipboardChunk {
    /// Validation a chunk can do alone: a non-empty payload inside
    /// [`MAX_CHUNK_BYTES`] and an index inside [`MAX_CHUNK_COUNT`].
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for an empty or oversized payload, or
    /// an out-of-range index.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.payload.is_empty() {
            return Err(ProtocolError::Malformed {
                reason: format!("empty clipboard chunk at index {}", self.index),
            });
        }
        if self.payload.len() > MAX_CHUNK_BYTES {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "clipboard chunk of {} bytes exceeds the {MAX_CHUNK_BYTES}-byte maximum",
                    self.payload.len()
                ),
            });
        }
        if self.index >= MAX_CHUNK_COUNT {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "clipboard chunk index {} is past the {MAX_CHUNK_COUNT}-chunk maximum",
                    self.index
                ),
            });
        }
        Ok(())
    }

    /// Encode the payload, validating first.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] from validation;
    /// [`ProtocolError::Encode`] on serialization failure.
    pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        encode(self)
    }

    /// Decode and validate a payload (strict: no trailing bytes).
    ///
    /// The frame layer has already bounded the bytes handed in here
    /// (`MAX_FRAME_BODY_BYTES`, validated from the length prefix before
    /// any payload is buffered), so nothing in this path sizes an
    /// allocation from an unvalidated declaration.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for undecodable or invalid payloads.
    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        let message: Self = decode_strict(payload, "ClipboardChunk")?;
        message.validate()?;
        Ok(message)
    }
}

/// How a chunked item is split.
///
/// **Derived, never declared.** The receiver computes the plan from the
/// offered `content_length` and the size of chunk 0, so there is no
/// second declaration on the wire that could disagree with the first —
/// the class of bug where a peer's stated chunk count and stated length
/// describe different transfers simply cannot be expressed. What the
/// sender does declare implicitly (its chunk size) is reconciled against
/// the offered length exactly, once, before any chunk is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkPlan {
    total_bytes: u64,
    chunk_bytes: u32,
    chunk_count: u32,
}

impl ChunkPlan {
    /// The plan a sender should use for `total_bytes`: full
    /// [`MAX_CHUNK_BYTES`] chunks and a remainder.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] if `total_bytes` is zero or cannot be
    /// split inside [`MAX_CHUNK_COUNT`].
    pub fn for_length(total_bytes: u64) -> Result<Self, ProtocolError> {
        Self::derive(total_bytes, MAX_CHUNK_BYTES_U32)
    }

    /// The plan implied by an item of `total_bytes` split into chunks of
    /// `chunk_bytes` (the last one shorter).
    ///
    /// All arithmetic is checked or saturating: no input combination
    /// overflows, and every failure is a value (NFR-1).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] when `chunk_bytes` is zero or over
    /// [`MAX_CHUNK_BYTES`], when `total_bytes` is zero, when the implied
    /// count exceeds [`MAX_CHUNK_COUNT`], or when the three quantities do
    /// not reconcile exactly.
    pub fn derive(total_bytes: u64, chunk_bytes: u32) -> Result<Self, ProtocolError> {
        if chunk_bytes == 0 || chunk_bytes > MAX_CHUNK_BYTES_U32 {
            return Err(ProtocolError::Malformed {
                reason: format!("chunk size {chunk_bytes} outside 1..={MAX_CHUNK_BYTES}"),
            });
        }
        if total_bytes == 0 {
            return Err(ProtocolError::Malformed {
                reason: "chunked transfer of zero bytes".to_owned(),
            });
        }
        let chunk_bytes_u64 = u64::from(chunk_bytes);
        let count = total_bytes.div_ceil(chunk_bytes_u64);
        let Ok(chunk_count) = u32::try_from(count) else {
            return Err(ProtocolError::Malformed {
                reason: format!("chunk count {count} exceeds the {MAX_CHUNK_COUNT}-chunk maximum"),
            });
        };
        if chunk_count > MAX_CHUNK_COUNT {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "{total_bytes} bytes in {chunk_bytes}-byte chunks needs {chunk_count} chunks, \
                     over the {MAX_CHUNK_COUNT}-chunk maximum"
                ),
            });
        }
        // The three quantities must reconcile exactly: the full chunks
        // plus a final chunk of 1..=chunk_bytes account for every declared
        // byte and no more. Guaranteed by div_ceil, but stated as a check
        // rather than trusted, because it is the invariant every later
        // per-chunk length check derives from.
        let full_chunks = u64::from(chunk_count.saturating_sub(1));
        let Some(final_bytes) = full_chunks
            .checked_mul(chunk_bytes_u64)
            .and_then(|before_final| total_bytes.checked_sub(before_final))
        else {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk plan arithmetic does not close for {total_bytes} bytes in \
                     {chunk_bytes}-byte chunks"
                ),
            });
        };
        if final_bytes == 0 || final_bytes > chunk_bytes_u64 {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "{chunk_count} chunks of {chunk_bytes} bytes do not reconcile with a declared \
                     length of {total_bytes}"
                ),
            });
        }
        Ok(Self {
            total_bytes,
            chunk_bytes,
            chunk_count,
        })
    }

    /// Total declared item length.
    #[must_use]
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    /// Size of every chunk but the last.
    #[must_use]
    pub const fn chunk_bytes(self) -> u32 {
        self.chunk_bytes
    }

    /// How many chunks the item is split into.
    #[must_use]
    pub const fn chunk_count(self) -> u32 {
        self.chunk_count
    }

    /// The exact length chunk `index` must carry, or `None` if `index` is
    /// past the end of the transfer.
    #[must_use]
    pub fn chunk_len(self, index: u32) -> Option<u32> {
        if index >= self.chunk_count {
            return None;
        }
        if index.saturating_add(1) < self.chunk_count {
            return Some(self.chunk_bytes);
        }
        // The final chunk: whatever the full ones left over, which
        // `derive` already proved is 1..=chunk_bytes.
        let consumed = u64::from(index) * u64::from(self.chunk_bytes);
        u32::try_from(self.total_bytes.saturating_sub(consumed)).ok()
    }
}

/// What a chunk did to a reassembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// Accepted; more chunks are expected.
    More,
    /// The final chunk landed and the item's `content_hash` verified over
    /// the whole reassembly: these bytes are the item, and this is the
    /// only way they leave the protocol layer.
    Complete(Vec<u8>),
}

/// Accumulates an offered item's chunks and proves the result is the item
/// that was offered (ADR 0014).
///
/// Pure accounting — no I/O, no platform, no clock — so every rejection
/// path is reachable in a unit test (docs/ARCHITECTURE.md §3). The
/// receiver's whole memory commitment is this buffer, sized from the
/// offered length *after* that length was validated against the type's
/// maximum (NFR-1).
#[derive(Debug)]
pub struct ChunkReassembly {
    meta: ClipboardMeta,
    plan: Option<ChunkPlan>,
    buffer: Vec<u8>,
    next_index: u32,
    complete: bool,
}

impl ChunkReassembly {
    /// Begin reassembling the offered item.
    ///
    /// Validates `meta` — crucially the declared length against the
    /// per-type maximum — **before** sizing the buffer from it.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] if the meta is invalid, if the type
    /// does not travel as chunks, or if the declared length cannot be
    /// represented as a buffer on this target;
    /// [`ProtocolError::ResourceExhausted`] if the (bounded, validated)
    /// buffer cannot be reserved — the caller declines the transfer
    /// rather than the process dying.
    pub fn begin(meta: ClipboardMeta) -> Result<Self, ProtocolError> {
        meta.validate()?;
        if !meta.content_type.is_chunked() {
            return Err(ProtocolError::Malformed {
                reason: format!("{:?} items do not travel as chunks", meta.content_type),
            });
        }
        // Chunked, but never buffered: a file is written straight through
        // to the spool as chunks arrive, so the receiver's commitment is
        // O(chunk) rather than O(file) (ADR 0015). Reassembling one here
        // would reserve up to MAX_CLIPBOARD_FILE_BYTES of memory for an
        // item that is never supposed to be in memory, so this type is
        // refused rather than served by the wrong mechanism.
        if meta.content_type.needs_file_descriptor() {
            return Err(ProtocolError::Malformed {
                reason: "file content is spooled as it arrives, never reassembled in memory"
                    .to_owned(),
            });
        }
        // Bounded by the check inside `validate` above; `try_from` covers
        // the 32-bit target where the bound alone would not.
        let Ok(capacity) = usize::try_from(meta.content_length) else {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "declared length {} cannot be buffered on this target",
                    meta.content_length
                ),
            });
        };
        // `try_reserve`, not `with_capacity`: the length is legal and
        // bounded, but a legal 64 MiB is still 64 MiB, and infallible
        // allocation turns a memory-pressured machine into an aborted
        // process at a peer's choosing. The ordering NFR-1 cares about is
        // unchanged — validate, *then* allocate.
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(capacity).is_err() {
            return Err(ProtocolError::ResourceExhausted {
                what: "a clipboard reassembly buffer",
                requested: meta.content_length,
            });
        }
        Ok(Self {
            meta,
            plan: None,
            buffer,
            next_index: 0,
            complete: false,
        })
    }

    /// The item being reassembled.
    #[must_use]
    pub const fn meta(&self) -> ClipboardMeta {
        self.meta
    }

    /// Bytes accumulated so far.
    #[must_use]
    pub fn received_bytes(&self) -> u64 {
        self.buffer.len() as u64
    }

    /// The plan, once chunk 0 has fixed the sender's chunk size.
    #[must_use]
    pub const fn plan(&self) -> Option<ChunkPlan> {
        self.plan
    }

    /// Take one chunk.
    ///
    /// The reject list, all fail-closed: a chunk for a different item; an
    /// index that is not exactly the next one (a gap, a repeat, or
    /// backwards); a chunk after the transfer completed; a first chunk
    /// whose size implies a plan that does not reconcile with the offered
    /// length or exceeds [`MAX_CHUNK_COUNT`]; any chunk whose length is
    /// not the exact length its position requires — which is also what
    /// makes the running total incapable of passing the declared length.
    /// Completion additionally requires the item's `content_hash` to
    /// verify over the reassembled bytes.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for every case above.
    pub fn accept(&mut self, chunk: &ClipboardChunk) -> Result<ChunkOutcome, ProtocolError> {
        chunk.validate()?;
        if self.complete {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk {} for item {} arrived after the transfer completed",
                    chunk.index, self.meta.id
                ),
            });
        }
        if chunk.id != self.meta.id {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk for item {} during reassembly of {}",
                    chunk.id, self.meta.id
                ),
            });
        }
        if chunk.index != self.next_index {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk index {} out of sequence; expected {}",
                    chunk.index, self.next_index
                ),
            });
        }

        // Chunk 0 fixes the sender's chunk size, and with it the whole
        // plan — reconciled against the offered length before a byte of it
        // is kept. Computed here but **not stored yet**: a rejection must
        // leave no trace in a fail-closed parser, and storing before the
        // checks below would let a refused chunk decide how the rest of
        // the transfer is measured.
        let plan = if let Some(plan) = self.plan {
            plan
        } else {
            let Ok(chunk_bytes) = u32::try_from(chunk.payload.len()) else {
                return Err(ProtocolError::Malformed {
                    reason: "chunk payload length is not representable".to_owned(),
                });
            };
            ChunkPlan::derive(self.meta.content_length, chunk_bytes)?
        };

        let Some(expected) = plan.chunk_len(chunk.index) else {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk index {} is past the {}-chunk transfer",
                    chunk.index,
                    plan.chunk_count()
                ),
            });
        };
        if chunk.payload.len() as u64 != u64::from(expected) {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk {} carries {} bytes; the plan requires exactly {expected}",
                    chunk.index,
                    chunk.payload.len()
                ),
            });
        }

        // Accepted: only now does any of it become state.
        self.plan = Some(plan);
        self.buffer.extend_from_slice(&chunk.payload);
        self.next_index = self.next_index.saturating_add(1);
        if self.next_index < plan.chunk_count() {
            return Ok(ChunkOutcome::More);
        }

        // Every declared byte is here. Verify the item's identity before
        // anything downstream is allowed to touch the OS clipboard.
        if self.buffer.len() as u64 != self.meta.content_length {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "reassembled {} bytes for a declared length of {}",
                    self.buffer.len(),
                    self.meta.content_length
                ),
            });
        }
        let digest: [u8; 32] = Sha256::digest(&self.buffer).into();
        if digest != self.meta.content_hash {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "reassembled content hash mismatch for item {}",
                    self.meta.id
                ),
            });
        }
        self.complete = true;
        Ok(ChunkOutcome::Complete(std::mem::take(&mut self.buffer)))
    }
}

/// What a chunk did to a [`ChunkStream`].
///
/// Both variants mean "this payload is admissible — write it". The
/// difference is what follows the write, and the caller must not act on
/// [`StreamOutcome::Final`] before the write is durable: the item is only
/// complete once the last payload is where it is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// Accepted; more chunks are expected.
    More,
    /// The final chunk, and the item's `content_hash` verified over every
    /// payload accepted: what has been handed out *is* the offered item.
    Final,
}

/// Accounts for a chunked item written straight through instead of
/// buffered (ADR 0015) — the file receiver's counterpart to
/// [`ChunkReassembly`].
///
/// Same rules, same reject list, one difference that is the whole point:
/// payloads are never retained. The caller writes each accepted payload
/// to its own sink and this type keeps only what proves the result is the
/// offered item — a running hash, a running length, and the next expected
/// index. The receiver's commitment is therefore O(chunk), not O(item),
/// which is what lets a file be 256 MiB while an image is capped at the
/// 64 MiB a machine must actually hold (docs/PROTOCOL.md §5).
///
/// **A chunk is judged before it is written, never after.** `accept`
/// returns before the caller writes, so a payload that fails any check
/// never reaches the sink at all — the sink's contents are always a
/// prefix of a conforming transfer, and a rejected transfer leaves a
/// partial that is deleted rather than a partial that is corrupt.
///
/// Pure accounting — no I/O, no platform, no clock — so every rejection
/// path is reachable in a unit test (docs/ARCHITECTURE.md §3).
#[derive(Debug)]
pub struct ChunkStream {
    meta: ClipboardMeta,
    plan: Option<ChunkPlan>,
    /// Running digest over every payload accepted so far. The item's
    /// `content_hash` is verified against it when the last chunk lands —
    /// the same guarantee [`ChunkReassembly`] gives, obtained without
    /// keeping the bytes.
    digest: Sha256,
    received: u64,
    next_index: u32,
    complete: bool,
}

impl ChunkStream {
    /// Begin streaming the offered item.
    ///
    /// Validates `meta` — crucially the declared length against the
    /// per-type maximum — before the caller commits any resource to it
    /// (NFR-1). Nothing is allocated here: the whole point of this type
    /// is that the item's size buys it no memory.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] if the meta is invalid or if the type
    /// does not travel as chunks.
    pub fn begin(meta: ClipboardMeta) -> Result<Self, ProtocolError> {
        meta.validate()?;
        if !meta.content_type.is_chunked() {
            return Err(ProtocolError::Malformed {
                reason: format!("{:?} items do not travel as chunks", meta.content_type),
            });
        }
        Ok(Self {
            meta,
            plan: None,
            digest: Sha256::new(),
            received: 0,
            next_index: 0,
            complete: false,
        })
    }

    /// The item being streamed.
    #[must_use]
    pub const fn meta(&self) -> ClipboardMeta {
        self.meta
    }

    /// Bytes handed out for writing so far.
    #[must_use]
    pub const fn received_bytes(&self) -> u64 {
        self.received
    }

    /// The plan, once chunk 0 has fixed the sender's chunk size.
    #[must_use]
    pub const fn plan(&self) -> Option<ChunkPlan> {
        self.plan
    }

    /// Take one chunk, judging it against the transfer before any of it
    /// is written.
    ///
    /// The reject list is [`ChunkReassembly::accept`]'s, unchanged: a
    /// chunk for a different item; an index that is not exactly the next
    /// one; a chunk after the transfer completed; a first chunk implying
    /// a plan that does not reconcile with the offered length; any chunk
    /// whose length is not the exact length its position requires — which
    /// is what makes the running total incapable of passing the declared
    /// length, so the receiver never trusts the sender to stop.
    /// Completion additionally requires the running length to equal the
    /// declared one and the item's `content_hash` to verify.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Malformed`] for every case above.
    pub fn accept(&mut self, chunk: &ClipboardChunk) -> Result<StreamOutcome, ProtocolError> {
        chunk.validate()?;
        if self.complete {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk {} for item {} arrived after the transfer completed",
                    chunk.index, self.meta.id
                ),
            });
        }
        if chunk.id != self.meta.id {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk for item {} during the transfer of {}",
                    chunk.id, self.meta.id
                ),
            });
        }
        if chunk.index != self.next_index {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk index {} out of sequence; expected {}",
                    chunk.index, self.next_index
                ),
            });
        }

        // Chunk 0 fixes the sender's chunk size, and with it the whole
        // plan — reconciled against the offered length before a byte of
        // it is admitted. Computed here but **not stored yet**: a
        // rejection must leave no trace, and storing before the checks
        // below would let a refused chunk decide how the rest of the
        // transfer is measured.
        let plan = if let Some(plan) = self.plan {
            plan
        } else {
            let Ok(chunk_bytes) = u32::try_from(chunk.payload.len()) else {
                return Err(ProtocolError::Malformed {
                    reason: "chunk payload length is not representable".to_owned(),
                });
            };
            ChunkPlan::derive(self.meta.content_length, chunk_bytes)?
        };

        let Some(expected) = plan.chunk_len(chunk.index) else {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk index {} is past the {}-chunk transfer",
                    chunk.index,
                    plan.chunk_count()
                ),
            });
        };
        if chunk.payload.len() as u64 != u64::from(expected) {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "chunk {} carries {} bytes; the plan requires exactly {expected}",
                    chunk.index,
                    chunk.payload.len()
                ),
            });
        }

        // Accepted: only now does any of it become state, and only the
        // accounting — the payload itself belongs to the caller's sink.
        self.plan = Some(plan);
        self.digest.update(&chunk.payload);
        self.received = self.received.saturating_add(chunk.payload.len() as u64);
        self.next_index = self.next_index.saturating_add(1);
        if self.next_index < plan.chunk_count() {
            return Ok(StreamOutcome::More);
        }

        // Every declared byte has been handed out. Verify the item's
        // identity before the caller is allowed to treat the sink's
        // contents as the item.
        if self.received != self.meta.content_length {
            return Err(ProtocolError::Malformed {
                reason: format!(
                    "streamed {} bytes for a declared length of {}",
                    self.received, self.meta.content_length
                ),
            });
        }
        let digest: [u8; 32] = std::mem::replace(&mut self.digest, Sha256::new())
            .finalize()
            .into();
        if digest != self.meta.content_hash {
            return Err(ProtocolError::Malformed {
                reason: format!("streamed content hash mismatch for item {}", self.meta.id),
            });
        }
        self.complete = true;
        Ok(StreamOutcome::Final)
    }
}

/// The capability a peer must have advertised before this frame may be
/// sent to it (docs/PROTOCOL.md §3.1), from the frame alone.
///
/// This is the shape the **send-path gate** needs: a chokepoint that sees
/// every outbound frame sees `(message_type, payload)` and nothing else,
/// so the rule has to be answerable from exactly that. Gating there
/// rather than at each call site is what makes the guarantee
/// unbypassable — no future caller can forget it.
///
/// Cheap by construction: `ClipboardMeta` is the first field of both
/// [`ClipboardOffer`] and [`ClipboardData`], and postcard is sequential,
/// so only that prefix is decoded — never the content.
///
/// **A payload whose meta prefix does not decode claims no capability**,
/// and that is sound rather than lenient: decoding is deterministic and
/// schema-identical on both ends, so bytes this side cannot read as a
/// `ClipboardMeta` are bytes the peer cannot read as a typed item either.
/// They can therefore neither carry an un-negotiated content type nor
/// kill a session by discriminant. Classifying a capability and
/// validating a payload are separate jobs; `encode_payload` owns the
/// second one, at the point the message is built.
#[must_use]
pub fn required_feature_for_frame(message_type: u16, payload: &[u8]) -> FeatureFlags {
    match MessageType::from_wire(message_type) {
        // The message type *is* the capability: a peer without the bit
        // has no decoder for it, whatever it carries.
        Some(MessageType::ClipboardChunk) => FeatureFlags::CHUNKED_CLIPBOARD,
        // These two carry a content type, and a content type a peer
        // cannot decode is fatal to it, not skippable (§2).
        Some(MessageType::ClipboardOffer | MessageType::ClipboardData) => {
            crate::decode_prefix::<ClipboardMeta>(payload, "ClipboardMeta")
                .map_or(FeatureFlags::NONE, |meta| {
                    meta.content_type.required_feature()
                })
        }
        // Everything else is base protocol.
        _ => FeatureFlags::NONE,
    }
}

/// Split `content` into the chunks that carry it (ADR 0014).
///
/// The sender's counterpart to [`ChunkReassembly`], here so both sides of
/// the arithmetic live together and are tested against each other.
///
/// # Errors
///
/// [`ProtocolError::Malformed`] if `content` is empty or does not fit
/// inside [`MAX_CHUNK_COUNT`] chunks.
pub fn chunk_content(id: Uuid, content: &[u8]) -> Result<Vec<ClipboardChunk>, ProtocolError> {
    let plan = ChunkPlan::for_length(content.len() as u64)?;
    let mut chunks = Vec::with_capacity(plan.chunk_count() as usize);
    for (index, payload) in content.chunks(MAX_CHUNK_BYTES).enumerate() {
        let Ok(index) = u32::try_from(index) else {
            return Err(ProtocolError::Malformed {
                reason: "chunk index is not representable".to_owned(),
            });
        };
        chunks.push(ClipboardChunk {
            id,
            index,
            payload: payload.to_vec(),
        });
    }
    Ok(chunks)
}

/// The transaction verdict from the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyResult {
    /// The destination OS clipboard now holds the item (FR-3.2's only
    /// definition of success).
    Applied,
    /// The destination clipboard stayed unavailable through the bounded
    /// retry budget (FR-3.4).
    ClipboardUnavailable,
    /// The destination refused the content (validation failed locally).
    ContentRejected,
    /// A newer item (by the deterministic conflict order, FR-3.5) won the
    /// race; the destination kept the newer content. Closes the losing
    /// transaction as converged, not failed.
    Superseded,
    /// A file item is verified, spooled, and offered on the destination's
    /// clipboard as a virtual file (ADR 0015) — the file type's
    /// definition of FR-3.2 success: the destination clipboard holds a
    /// promise of bytes that are already local. Appended after
    /// `Superseded`; discriminants are wire values.
    Stored,
    /// A file item arrived intact but could not be spooled — a write
    /// error, a spool that could not be opened, or an abort that left
    /// nothing advertisable. A failure, reported rather than swallowed
    /// (NFR-3).
    StorageFailed,
}

/// Close a transaction: what happened at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardApplied {
    /// The item the verdict is about.
    pub id: Uuid,
    /// The verdict.
    pub result: ApplyResult,
}

macro_rules! plain_payload {
    ($ty:ty, $name:literal) => {
        impl $ty {
            /// Encode the payload.
            ///
            /// # Errors
            ///
            /// [`ProtocolError::Encode`] on serialization failure.
            pub fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
                encode(self)
            }

            /// Decode a payload (strict: no trailing bytes).
            ///
            /// # Errors
            ///
            /// [`ProtocolError::Malformed`] for undecodable payloads.
            pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
                decode_strict(payload, $name)
            }
        }
    };
}

plain_payload!(ClipboardAccept, "ClipboardAccept");
plain_payload!(ClipboardDecline, "ClipboardDecline");
plain_payload!(ClipboardApplied, "ClipboardApplied");

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_stdvec(value).map_err(|e| ProtocolError::Encode {
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::{
        ApplyResult, CLIPBOARD_INLINE_MAX_BYTES, ChunkOutcome, ChunkPlan, ChunkReassembly,
        ChunkStream, ClipboardAccept, ClipboardApplied, ClipboardChunk, ClipboardData,
        ClipboardDecline, ClipboardMeta, ClipboardOffer, ContentType, DeclineReason,
        FileDescriptor, ImageFormat, MAX_CHUNK_BYTES, MAX_CHUNK_BYTES_U32, MAX_CHUNK_COUNT,
        MAX_CLIPBOARD_FILE_BYTES, MAX_CLIPBOARD_FILE_ENTRIES, MAX_CLIPBOARD_IMAGE_BYTES,
        MAX_CLIPBOARD_TEXT_BYTES, StreamOutcome, chunk_content, content_hash,
    };
    use crate::ProtocolError;
    use crate::file_name::MAX_FILE_NAME_BYTES;
    use crate::hello::FeatureFlags;

    const ITEM: Uuid = Uuid::from_bytes([0x11; 16]);
    const ORIGIN: Uuid = Uuid::from_bytes([0x22; 16]);

    /// An offer with no file descriptor — every type but
    /// [`ContentType::File`].
    fn offer(meta: ClipboardMeta) -> ClipboardOffer {
        ClipboardOffer {
            meta,
            descriptor: None,
        }
    }

    fn data(content: &[u8]) -> ClipboardData {
        ClipboardData::from_content(
            Uuid::from_bytes([0x11; 16]),
            Uuid::from_bytes([0x22; 16]),
            7,
            ContentType::Utf8Text,
            content.to_vec(),
        )
    }

    #[test]
    fn data_round_trips_when_consistent() {
        let item = data(b"hello clipboard");
        let decoded = ClipboardData::decode_payload(&item.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, item);
    }

    #[test]
    fn tampered_content_hash_or_length_is_rejected_at_decode() {
        let item = data(b"integrity matters");

        let mut wrong_hash = item.clone();
        wrong_hash.meta.content_hash[0] ^= 0xFF;
        let bytes = super::encode(&wrong_hash).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));

        let mut wrong_len = item;
        wrong_len.meta.content_length += 1;
        let bytes = super::encode(&wrong_len).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn non_utf8_text_content_is_rejected() {
        let mut item = data(&[0xFF, 0xFE, 0xFD]);
        // from_content computed a correct hash over invalid UTF-8; both
        // encode and decode must refuse it.
        assert!(matches!(
            item.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        item.meta.content_type = ContentType::Utf8Text;
        let bytes = super::encode(&item).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn oversized_items_are_rejected_without_allocation_tricks() {
        // Craft a meta declaring over-limit length; content stays small so
        // the test is cheap — the length check must fire on the declared
        // value regardless.
        let mut item = data(b"small");
        item.meta.content_length = (MAX_CLIPBOARD_TEXT_BYTES as u64) + 1;
        let bytes = super::encode(&item).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn offers_below_the_inline_threshold_are_malformed() {
        let small = data(b"tiny");
        let tiny = offer(small.meta);
        assert!(matches!(
            tiny.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));

        let mut big_meta = small.meta;
        big_meta.content_length = (CLIPBOARD_INLINE_MAX_BYTES as u64) + 1;
        let big = offer(big_meta);
        let decoded = ClipboardOffer::decode_payload(&big.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, big);
    }

    #[test]
    fn control_messages_round_trip() {
        let accept = ClipboardAccept {
            id: Uuid::from_bytes([0x33; 16]),
        };
        assert_eq!(
            ClipboardAccept::decode_payload(&accept.encode_payload().unwrap()).unwrap(),
            accept
        );

        for reason in [
            DeclineReason::AlreadyHave,
            DeclineReason::TooLarge,
            DeclineReason::NotReady,
            DeclineReason::Superseded,
            DeclineReason::UnsupportedType,
            DeclineReason::NotPermitted,
            DeclineReason::InvalidName,
            DeclineReason::InsufficientSpace,
        ] {
            let decline = ClipboardDecline {
                id: Uuid::from_bytes([0x44; 16]),
                reason,
            };
            assert_eq!(
                ClipboardDecline::decode_payload(&decline.encode_payload().unwrap()).unwrap(),
                decline
            );
        }

        for result in [
            ApplyResult::Applied,
            ApplyResult::ClipboardUnavailable,
            ApplyResult::ContentRejected,
            ApplyResult::Superseded,
            ApplyResult::Stored,
            ApplyResult::StorageFailed,
        ] {
            let applied = ClipboardApplied {
                id: Uuid::from_bytes([0x55; 16]),
                result,
            };
            assert_eq!(
                ClipboardApplied::decode_payload(&applied.encode_payload().unwrap()).unwrap(),
                applied
            );
        }
    }

    /// Golden wire snapshots (ADR 0001): schema change = version bump.
    #[test]
    fn golden_wire_snapshots_v1() {
        let item = ClipboardData::from_content(
            Uuid::from_bytes([0x11; 16]),
            Uuid::from_bytes([0x22; 16]),
            7,
            ContentType::Utf8Text,
            b"hi".to_vec(),
        );
        let mut expected: Vec<u8> = Vec::new();
        expected.push(0x10); // id: 16-byte length prefix
        expected.extend([0x11; 16]); // id bytes
        expected.push(0x10); // origin: 16-byte length prefix
        expected.extend([0x22; 16]); // origin bytes
        expected.push(0x07); // sequence varint
        expected.push(0x00); // ContentType::Utf8Text
        expected.push(0x02); // content_length varint
        expected.extend(item.meta.content_hash); // hash: fixed 32, no prefix
        expected.extend([0x02, b'h', b'i']); // content: len-prefixed bytes
        assert_eq!(
            item.encode_payload().unwrap(),
            expected,
            "v1 ClipboardData wire layout changed: bump the protocol version"
        );

        let applied = ClipboardApplied {
            id: Uuid::from_bytes([0x55; 16]),
            result: ApplyResult::ClipboardUnavailable,
        };
        let mut expected: Vec<u8> = Vec::new();
        expected.push(0x10);
        expected.extend([0x55; 16]);
        expected.push(0x01); // ApplyResult::ClipboardUnavailable
        assert_eq!(
            applied.encode_payload().unwrap(),
            expected,
            "v1 ClipboardApplied wire layout changed: bump the protocol version"
        );
    }

    /// Golden discriminants for every typed enum that crosses the wire
    /// (ADR 0001).
    ///
    /// Round-trip tests cannot catch what this catches: encode and decode
    /// move together inside one build, so reordering a variant round-trips
    /// perfectly while silently changing what the *other* build reads —
    /// an `AlreadyHave` arriving as `TooLarge` turns a synchronization
    /// success into a refusal, with no error anywhere. Only bytes pin
    /// that. Variants are appended, never reordered; a failure here is a
    /// protocol version bump, not a test to update.
    #[test]
    fn golden_wire_discriminants_for_typed_enums() {
        for (reason, discriminant) in [
            (DeclineReason::AlreadyHave, 0x00),
            (DeclineReason::TooLarge, 0x01),
            (DeclineReason::NotReady, 0x02),
            (DeclineReason::Superseded, 0x03),
            (DeclineReason::UnsupportedType, 0x04),
            (DeclineReason::NotPermitted, 0x05),
            (DeclineReason::InvalidName, 0x06),
            (DeclineReason::InsufficientSpace, 0x07),
        ] {
            let mut expected: Vec<u8> = vec![0x10];
            expected.extend([0x11; 16]);
            expected.push(discriminant);
            assert_eq!(
                ClipboardDecline { id: ITEM, reason }
                    .encode_payload()
                    .unwrap(),
                expected,
                "{reason:?} changed wire value: bump the protocol version"
            );
        }

        for (result, discriminant) in [
            (ApplyResult::Applied, 0x00),
            (ApplyResult::ClipboardUnavailable, 0x01),
            (ApplyResult::ContentRejected, 0x02),
            (ApplyResult::Superseded, 0x03),
            (ApplyResult::Stored, 0x04),
            (ApplyResult::StorageFailed, 0x05),
        ] {
            let mut expected: Vec<u8> = vec![0x10];
            expected.extend([0x11; 16]);
            expected.push(discriminant);
            assert_eq!(
                ClipboardApplied { id: ITEM, result }
                    .encode_payload()
                    .unwrap(),
                expected,
                "{result:?} changed wire value: bump the protocol version"
            );
        }

        for (content_type, encoded) in [
            (ContentType::Utf8Text, vec![0x00]),
            (ContentType::Image(ImageFormat::Dib), vec![0x01, 0x00]),
            (ContentType::Image(ImageFormat::Png), vec![0x01, 0x01]),
            (ContentType::Image(ImageFormat::Jpeg), vec![0x01, 0x02]),
            (ContentType::File, vec![0x02]),
        ] {
            assert_eq!(
                super::encode(&content_type).unwrap(),
                encoded,
                "{content_type:?} changed wire value: bump the protocol version"
            );
        }
    }

    #[test]
    fn garbage_and_padded_payloads_are_malformed() {
        assert!(matches!(
            ClipboardData::decode_payload(&[0xFF; 30]),
            Err(ProtocolError::Malformed { .. })
        ));
        let good = data(b"ok").encode_payload().unwrap();
        let mut padded = good;
        padded.push(0x00);
        assert!(matches!(
            ClipboardData::decode_payload(&padded),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn meta_is_copy_and_cheap_to_pass_around() {
        // ClipboardMeta is the engine's working currency; keep it Copy.
        fn assert_copy<T: Copy>() {}
        assert_copy::<ClipboardMeta>();
    }

    // --- chunked rich clipboard (ADR 0014) ---------------------------------

    fn image_meta(content: &[u8]) -> ClipboardMeta {
        ClipboardMeta {
            id: ITEM,
            origin: ORIGIN,
            sequence: 3,
            content_type: ContentType::Image(ImageFormat::Dib),
            content_length: content.len() as u64,
            content_hash: content_hash(content),
        }
    }

    /// Feed every chunk into a reassembly, returning the verified bytes.
    fn reassemble(
        meta: ClipboardMeta,
        chunks: &[ClipboardChunk],
    ) -> Result<Vec<u8>, ProtocolError> {
        let mut reassembly = ChunkReassembly::begin(meta)?;
        let mut last = ChunkOutcome::More;
        for chunk in chunks {
            last = reassembly.accept(chunk)?;
        }
        match last {
            ChunkOutcome::Complete(bytes) => Ok(bytes),
            ChunkOutcome::More => Err(ProtocolError::Malformed {
                reason: "transfer never completed".to_owned(),
            }),
        }
    }

    #[test]
    fn chunks_round_trip_through_payload_encoding() {
        let chunk = ClipboardChunk {
            id: ITEM,
            index: 7,
            payload: vec![0xAB; 1024],
        };
        assert_eq!(
            ClipboardChunk::decode_payload(&chunk.encode_payload().unwrap()).unwrap(),
            chunk
        );
    }

    /// Golden wire snapshot (ADR 0001): a schema change here is a protocol
    /// version bump, and `ContentType`'s discriminants are wire values —
    /// `Utf8Text` stays 0 and `Image` was appended as 1.
    #[test]
    fn golden_wire_snapshots_for_chunked_transfer() {
        let chunk = ClipboardChunk {
            id: ITEM,
            index: 2,
            payload: b"px".to_vec(),
        };
        let mut expected: Vec<u8> = Vec::new();
        expected.push(0x10); // id: 16-byte length prefix
        expected.extend([0x11; 16]);
        expected.push(0x02); // index varint
        expected.extend([0x02, b'p', b'x']); // payload: len-prefixed bytes
        assert_eq!(
            chunk.encode_payload().unwrap(),
            expected,
            "v2 ClipboardChunk wire layout changed: bump the protocol version"
        );

        let offer = offer(image_meta(b"px"));
        let encoded = offer.encode_payload().unwrap();
        // ContentType sits after id (17), origin (17) and the sequence
        // varint (1): Image = 1, ImageFormat::Dib = 0.
        assert_eq!(
            &encoded[35..37],
            &[0x01, 0x00],
            "ContentType discriminants are wire values and are never renumbered"
        );
        assert_eq!(ClipboardOffer::decode_payload(&encoded).unwrap(), offer);
    }

    /// The inline-threshold rule is a *text* rule: an image is always
    /// offered, at any size, because the offer round is where the
    /// already-have-this-hash decline makes a re-paste move zero bytes.
    #[test]
    fn images_are_offered_at_any_size_while_text_keeps_the_inline_threshold() {
        let tiny_image = offer(image_meta(b"a tiny snip"));
        assert!(tiny_image.encode_payload().is_ok());

        let mut tiny_text = tiny_image.meta;
        tiny_text.content_type = ContentType::Utf8Text;
        assert!(matches!(
            offer(tiny_text).encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// One path per type: a chunked type never travels as `ClipboardData`,
    /// so there is no ambiguity about which validation ran over it.
    #[test]
    fn image_content_is_rejected_as_clipboard_data() {
        let content = vec![0xFFu8; 128];
        let item = ClipboardData {
            meta: image_meta(&content),
            content,
        };
        assert!(matches!(
            item.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        let bytes = super::encode(&item).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Per-type maxima: an image may be far larger than any text item, and
    /// each type's own ceiling is what its declared length is checked
    /// against — before anything is sized from it.
    #[test]
    fn each_content_type_is_bounded_by_its_own_maximum() {
        let at_limit = ClipboardMeta {
            content_length: MAX_CLIPBOARD_IMAGE_BYTES as u64,
            ..image_meta(b"unused")
        };
        assert!(offer(at_limit).validate().is_ok());

        let over = ClipboardMeta {
            content_length: (MAX_CLIPBOARD_IMAGE_BYTES as u64) + 1,
            ..at_limit
        };
        assert!(matches!(
            offer(over).validate(),
            Err(ProtocolError::Malformed { .. })
        ));
        // An image-sized *text* item is still refused at the text bound.
        let text = ClipboardMeta {
            content_type: ContentType::Utf8Text,
            content_length: (MAX_CLIPBOARD_TEXT_BYTES as u64) + 1,
            ..at_limit
        };
        assert!(matches!(
            offer(text).validate(),
            Err(ProtocolError::Malformed { .. })
        ));
        // A zero-byte image is not an image.
        let empty = ClipboardMeta {
            content_length: 0,
            ..at_limit
        };
        assert!(matches!(
            ChunkReassembly::begin(empty),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Image bytes are arbitrary binary; the UTF-8 rule belongs to text
    /// alone, and it must not follow the bytes into reassembly.
    #[test]
    fn image_bytes_need_not_be_utf8() {
        let content: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
        let meta = image_meta(&content);
        let chunks = chunk_content(meta.id, &content).unwrap();
        assert_eq!(reassemble(meta, &chunks).unwrap(), content);
    }

    #[test]
    fn chunk_validation_rejects_empty_oversized_and_out_of_range() {
        let empty = ClipboardChunk {
            id: ITEM,
            index: 0,
            payload: Vec::new(),
        };
        assert!(matches!(
            empty.validate(),
            Err(ProtocolError::Malformed { .. })
        ));

        let oversized = ClipboardChunk {
            id: ITEM,
            index: 0,
            payload: vec![0u8; MAX_CHUNK_BYTES + 1],
        };
        assert!(matches!(
            oversized.validate(),
            Err(ProtocolError::Malformed { .. })
        ));
        // The boundary itself is fine.
        assert!(
            ClipboardChunk {
                id: ITEM,
                index: 0,
                payload: vec![0u8; MAX_CHUNK_BYTES],
            }
            .validate()
            .is_ok()
        );

        let far_index = ClipboardChunk {
            id: ITEM,
            index: MAX_CHUNK_COUNT,
            payload: vec![1u8],
        };
        assert!(matches!(
            far_index.validate(),
            Err(ProtocolError::Malformed { .. })
        ));
        // Encode refuses what decode would refuse.
        assert!(matches!(
            far_index.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Length, chunk size and chunk count must reconcile *exactly*.
    #[test]
    fn chunk_plans_reconcile_length_size_and_count() {
        // An exact multiple: every chunk full, none left over.
        let plan = ChunkPlan::derive(1024, 256).unwrap();
        assert_eq!((plan.chunk_count(), plan.chunk_bytes()), (4, 256));
        assert_eq!(plan.chunk_len(3), Some(256));
        assert_eq!(plan.chunk_len(4), None);

        // A remainder: the final chunk is short, and only the final one.
        let plan = ChunkPlan::derive(1025, 256).unwrap();
        assert_eq!(plan.chunk_count(), 5);
        assert_eq!(plan.chunk_len(3), Some(256));
        assert_eq!(plan.chunk_len(4), Some(1));

        // Smaller than one chunk: a single, short chunk.
        let plan = ChunkPlan::derive(10, 256).unwrap();
        assert_eq!((plan.chunk_count(), plan.chunk_len(0)), (1, Some(10)));

        // The sender's default plan uses full chunks.
        let plan = ChunkPlan::for_length((MAX_CHUNK_BYTES as u64) + 1).unwrap();
        assert_eq!(plan.chunk_count(), 2);
        assert_eq!(plan.chunk_bytes(), MAX_CHUNK_BYTES_U32);
    }

    #[test]
    fn inconsistent_chunk_declarations_are_rejected() {
        // Zero-sized chunks would never terminate.
        assert!(matches!(
            ChunkPlan::derive(1024, 0),
            Err(ProtocolError::Malformed { .. })
        ));
        // A chunk larger than the preemption unit defeats ADR 0013.
        assert!(matches!(
            ChunkPlan::derive(1024, MAX_CHUNK_BYTES_U32 + 1),
            Err(ProtocolError::Malformed { .. })
        ));
        // Nothing to transfer.
        assert!(matches!(
            ChunkPlan::derive(0, 256),
            Err(ProtocolError::Malformed { .. })
        ));
        // A legal-looking transfer of millions of tiny chunks: the count
        // bound refuses it before anything is buffered.
        assert!(matches!(
            ChunkPlan::derive(MAX_CLIPBOARD_IMAGE_BYTES as u64, 1),
            Err(ProtocolError::Malformed { .. })
        ));
        // The largest item of each chunked type still fits at the largest
        // chunk size, and the largest of all — a file — is exactly what
        // MAX_CHUNK_COUNT is derived from.
        let plan =
            ChunkPlan::derive(MAX_CLIPBOARD_IMAGE_BYTES as u64, MAX_CHUNK_BYTES_U32).unwrap();
        assert!(plan.chunk_count() <= MAX_CHUNK_COUNT);
        let plan = ChunkPlan::derive(MAX_CLIPBOARD_FILE_BYTES as u64, MAX_CHUNK_BYTES_U32).unwrap();
        assert_eq!(plan.chunk_count(), MAX_CHUNK_COUNT);
        // One byte past the largest chunked item needs one chunk too many.
        assert!(matches!(
            ChunkPlan::derive((MAX_CLIPBOARD_FILE_BYTES as u64) + 1, MAX_CHUNK_BYTES_U32),
            Err(ProtocolError::Malformed { .. })
        ));
        // No length math overflows, however absurd the declaration.
        assert!(matches!(
            ChunkPlan::derive(u64::MAX, 1),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn reassembly_completes_only_after_the_hash_verifies() {
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let meta = image_meta(&content);
        let chunks = chunk_content(meta.id, &content).unwrap();
        assert_eq!(chunks.len(), 4);

        let mut reassembly = ChunkReassembly::begin(meta).unwrap();
        for chunk in &chunks[..chunks.len() - 1] {
            assert_eq!(reassembly.accept(chunk).unwrap(), ChunkOutcome::More);
        }
        assert_eq!(reassembly.received_bytes(), 3 * MAX_CHUNK_BYTES as u64);
        let ChunkOutcome::Complete(bytes) = reassembly.accept(chunks.last().unwrap()).unwrap()
        else {
            panic!("the final chunk must complete the item");
        };
        assert_eq!(bytes, content);
    }

    /// The hash is verified over the *reassembled* bytes: a single flipped
    /// byte in one chunk, with every length still exact, must not reach the
    /// caller (ADR 0014 — nothing unverified touches the OS clipboard).
    #[test]
    fn reassembly_detects_a_hash_mismatch_over_the_whole_item() {
        let content = vec![0x5Au8; 100_000];
        let meta = image_meta(&content);
        let mut chunks = chunk_content(meta.id, &content).unwrap();
        chunks[0].payload[17] ^= 0xFF;
        assert!(matches!(
            reassemble(meta, &chunks),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn reassembly_requires_strictly_sequential_indices() {
        let content = vec![0x11u8; 3 * MAX_CHUNK_BYTES];
        let meta = image_meta(&content);
        let chunks = chunk_content(meta.id, &content).unwrap();

        // A gap.
        let gapped = [chunks[0].clone(), chunks[2].clone()];
        assert!(matches!(
            reassemble(meta, &gapped),
            Err(ProtocolError::Malformed { .. })
        ));
        // A repeat.
        let repeated = [chunks[0].clone(), chunks[0].clone()];
        assert!(matches!(
            reassemble(meta, &repeated),
            Err(ProtocolError::Malformed { .. })
        ));
        // Backwards.
        let backwards = [chunks[0].clone(), chunks[1].clone(), chunks[0].clone()];
        assert!(matches!(
            reassemble(meta, &backwards),
            Err(ProtocolError::Malformed { .. })
        ));
        // Out of range for this transfer: index 3 of a 3-chunk item.
        let past_end = [
            chunks[0].clone(),
            chunks[1].clone(),
            chunks[2].clone(),
            ClipboardChunk {
                id: meta.id,
                index: 3,
                payload: vec![0x11; 16],
            },
        ];
        assert!(matches!(
            reassemble(meta, &past_end),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn reassembly_requires_the_exact_length_each_position_demands() {
        let content = vec![0x22u8; 2 * MAX_CHUNK_BYTES + 100];
        let meta = image_meta(&content);
        let chunks = chunk_content(meta.id, &content).unwrap();

        // A short non-final chunk would make the running total drift.
        let mut short_first = chunks.clone();
        short_first[0].payload.truncate(MAX_CHUNK_BYTES - 1);
        assert!(matches!(
            reassemble(meta, &short_first),
            Err(ProtocolError::Malformed { .. })
        ));

        // A final chunk that overshoots the declared length.
        let mut long_final = chunks.clone();
        long_final[2].payload.extend(std::iter::repeat_n(0x22, 8));
        assert!(matches!(
            reassemble(meta, &long_final),
            Err(ProtocolError::Malformed { .. })
        ));

        // A first chunk whose size implies a plan that cannot reconcile
        // with the offered length: 1-byte chunks for a 128 KiB item needs
        // more chunks than the protocol allows.
        let degenerate = [ClipboardChunk {
            id: meta.id,
            index: 0,
            payload: vec![0x22; 1],
        }];
        assert!(matches!(
            reassemble(meta, &degenerate),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn reassembly_rejects_foreign_ids_and_late_chunks() {
        let content = vec![0x33u8; 32];
        let meta = image_meta(&content);
        let chunks = chunk_content(meta.id, &content).unwrap();

        let foreign = [ClipboardChunk {
            id: Uuid::from_bytes([0x99; 16]),
            ..chunks[0].clone()
        }];
        assert!(matches!(
            reassemble(meta, &foreign),
            Err(ProtocolError::Malformed { .. })
        ));

        // The item completed on chunk 0; anything after it is a violation.
        let mut reassembly = ChunkReassembly::begin(meta).unwrap();
        assert!(matches!(
            reassembly.accept(&chunks[0]).unwrap(),
            ChunkOutcome::Complete(_)
        ));
        assert!(matches!(
            reassembly.accept(&chunks[0]),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn reassembly_refuses_non_chunked_types_and_oversized_declarations() {
        let text = ClipboardMeta {
            content_type: ContentType::Utf8Text,
            ..image_meta(b"not an image")
        };
        assert!(matches!(
            ChunkReassembly::begin(text),
            Err(ProtocolError::Malformed { .. })
        ));

        // The declared length is checked against the type maximum before
        // the buffer is sized from it (NFR-1): this must fail without
        // attempting a 64 MiB + 1 allocation.
        let oversized = ClipboardMeta {
            content_length: (MAX_CLIPBOARD_IMAGE_BYTES as u64) + 1,
            ..image_meta(b"unused")
        };
        assert!(matches!(
            ChunkReassembly::begin(oversized),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// The sender's gate (docs/PROTOCOL.md §3): unknown message types are
    /// skipped rather than fatal, so an un-negotiated chunked offer would
    /// simply never be answered. Nothing goes out that the peer has not
    /// said it can take.
    #[test]
    fn chunked_content_is_offered_only_to_a_peer_that_advertised_it() {
        let image = offer(image_meta(b"a snip"));
        let text = offer(ClipboardMeta {
            content_type: ContentType::Utf8Text,
            content_length: (CLIPBOARD_INLINE_MAX_BYTES as u64) + 1,
            ..image.meta
        });

        assert!(!image.negotiated_by(FeatureFlags::NONE));
        assert!(image.negotiated_by(FeatureFlags::CHUNKED_CLIPBOARD));
        assert!(image.negotiated_by(FeatureFlags::ALL));
        // The base protocol's types need no bit at all.
        assert!(text.negotiated_by(FeatureFlags::NONE));

        // Files take a bit of their own: an ADR 0014 peer advertises
        // CHUNKED_CLIPBOARD and has no `File` discriminant, so the image
        // bit must not carry a file offer to it.
        let file = file_offer(b"a document");
        assert!(!file.negotiated_by(FeatureFlags::NONE));
        assert!(!file.negotiated_by(FeatureFlags::CHUNKED_CLIPBOARD));
        assert!(file.negotiated_by(FeatureFlags::FILE_CLIPBOARD));
        assert!(file.negotiated_by(FeatureFlags::ALL));
        // ... and the file bit does not carry an image either.
        assert!(!image.negotiated_by(FeatureFlags::FILE_CLIPBOARD));

        // A feature is active only when both sides advertise it.
        assert_eq!(
            FeatureFlags::negotiate(FeatureFlags::ALL, FeatureFlags::NONE),
            FeatureFlags::NONE
        );
        assert_eq!(
            FeatureFlags::negotiate(FeatureFlags::ALL, FeatureFlags::ALL),
            FeatureFlags::ALL
        );
        assert_eq!(
            FeatureFlags::negotiate(FeatureFlags::ALL, FeatureFlags::CHUNKED_CLIPBOARD),
            FeatureFlags::CHUNKED_CLIPBOARD
        );
        // An unknown bit from a future peer never activates anything.
        assert_eq!(
            FeatureFlags::negotiate(FeatureFlags::ALL, FeatureFlags(1 << 63)),
            FeatureFlags::NONE
        );
    }

    #[test]
    fn garbage_truncated_and_padded_chunk_payloads_are_malformed() {
        assert!(matches!(
            ClipboardChunk::decode_payload(&[0xFF; 30]),
            Err(ProtocolError::Malformed { .. })
        ));
        let good = ClipboardChunk {
            id: ITEM,
            index: 1,
            payload: b"bytes".to_vec(),
        }
        .encode_payload()
        .unwrap();
        assert!(matches!(
            ClipboardChunk::decode_payload(&good[..good.len() - 1]),
            Err(ProtocolError::Malformed { .. })
        ));
        let mut padded = good;
        padded.push(0x00);
        assert!(matches!(
            ClipboardChunk::decode_payload(&padded),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    proptest! {
        /// Any content splits and reassembles byte-identically, through
        /// the same encode/decode every chunk takes on the wire.
        #[test]
        fn any_content_survives_chunking_and_reassembly(
            content in proptest::collection::vec(any::<u8>(), 1..200_000),
        ) {
            let meta = image_meta(&content);
            let chunks = chunk_content(meta.id, &content).unwrap();
            let mut reassembly = ChunkReassembly::begin(meta).unwrap();
            let mut assembled = None;
            for chunk in &chunks {
                let wire = chunk.encode_payload().unwrap();
                let decoded = ClipboardChunk::decode_payload(&wire).unwrap();
                if let ChunkOutcome::Complete(bytes) = reassembly.accept(&decoded).unwrap() {
                    assembled = Some(bytes);
                }
            }
            prop_assert_eq!(assembled, Some(content));
        }

        /// Arbitrary chunk sequences never panic: every outcome is bytes,
        /// "more", or a typed rejection (NFR-1).
        #[test]
        fn arbitrary_chunk_sequences_never_panic(
            declared in 1u64..300_000,
            chunks in proptest::collection::vec(
                (0u32..8, proptest::collection::vec(any::<u8>(), 0..70_000)),
                0..8,
            ),
        ) {
            let meta = ClipboardMeta {
                content_length: declared,
                ..image_meta(b"unused")
            };
            let Ok(mut reassembly) = ChunkReassembly::begin(meta) else { return Ok(()); };
            for (index, payload) in chunks {
                let chunk = ClipboardChunk { id: meta.id, index, payload };
                let (bytes_before, plan_before) =
                    (reassembly.received_bytes(), reassembly.plan());
                if reassembly.accept(&chunk).is_err() {
                    // A rejection leaves no trace: nothing buffered, and
                    // no plan fixed by a chunk that was refused.
                    prop_assert_eq!(reassembly.received_bytes(), bytes_before);
                    prop_assert_eq!(reassembly.plan(), plan_before);
                    break; // fail closed: the transfer is over
                }
            }
        }

        /// The same, for the stream a file rides: arbitrary sequences
        /// never panic, a refusal leaves no trace, and — the property
        /// only this type has — the accounting never runs ahead of the
        /// item, so what a caller wrote can never exceed what was
        /// declared.
        #[test]
        fn arbitrary_file_chunk_sequences_never_panic(
            declared in 1u64..300_000,
            chunks in proptest::collection::vec(
                (0u32..8, proptest::collection::vec(any::<u8>(), 0..70_000)),
                0..8,
            ),
        ) {
            let meta = ClipboardMeta {
                content_length: declared,
                ..file_meta(b"unused")
            };
            let Ok(mut stream) = ChunkStream::begin(meta) else { return Ok(()); };
            for (index, payload) in chunks {
                let chunk = ClipboardChunk { id: meta.id, index, payload };
                let (bytes_before, plan_before) = (stream.received_bytes(), stream.plan());
                if stream.accept(&chunk).is_err() {
                    // A rejection leaves no trace: nothing counted, and no
                    // plan fixed by a chunk that was refused.
                    prop_assert_eq!(stream.received_bytes(), bytes_before);
                    prop_assert_eq!(stream.plan(), plan_before);
                    break; // fail closed: the transfer is over
                }
                prop_assert!(stream.received_bytes() <= declared);
            }
        }

        /// Arbitrary bytes never panic the chunk decoder, and anything
        /// that decodes survives a re-encode unchanged.
        #[test]
        fn arbitrary_bytes_decode_or_reject_without_panicking(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            if let Ok(chunk) = ClipboardChunk::decode_payload(&bytes) {
                let again = ClipboardChunk::decode_payload(&chunk.encode_payload().unwrap())
                    .unwrap();
                prop_assert_eq!(chunk, again);
            }
        }
    }

    // --- files (ADR 0015) --------------------------------------------------

    fn file_meta(content: &[u8]) -> ClipboardMeta {
        ClipboardMeta {
            id: ITEM,
            origin: ORIGIN,
            sequence: 5,
            content_type: ContentType::File,
            content_length: content.len() as u64,
            content_hash: content_hash(content),
        }
    }

    /// A single-file descriptor: one entry, not an archive, verbatim.
    fn file_descriptor(name: &str, original_bytes: u64) -> FileDescriptor {
        FileDescriptor {
            file_name: name.to_owned(),
            archived: false,
            entry_count: 1,
            original_bytes,
        }
    }

    fn file_offer(content: &[u8]) -> ClipboardOffer {
        let meta = file_meta(content);
        ClipboardOffer {
            descriptor: Some(file_descriptor("report.pdf", meta.content_length)),
            meta,
        }
    }

    #[test]
    fn file_offers_round_trip_with_their_descriptor() {
        let item = file_offer(b"a document's bytes");
        let decoded = ClipboardOffer::decode_payload(&item.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, item);

        // The archived shape too: a folder or multi-entry selection is one
        // zip, so entry_count may exceed 1 and original_bytes need not
        // equal the compressed blob.
        let archive = ClipboardOffer {
            descriptor: Some(FileDescriptor {
                file_name: "holiday photos.zip".to_owned(),
                archived: true,
                entry_count: 42,
                original_bytes: 900_000,
            }),
            ..file_offer(b"pretend zip bytes")
        };
        let decoded = ClipboardOffer::decode_payload(&archive.encode_payload().unwrap()).unwrap();
        assert_eq!(decoded, archive);
    }

    /// Golden wire snapshot (ADR 0001). Two things are pinned here: that
    /// `ContentType::File` is discriminant 2 — appended after `Image`,
    /// never renumbered — and the descriptor's own layout.
    #[test]
    fn golden_wire_snapshot_for_a_file_offer() {
        let meta = file_meta(b"pdf");
        let item = ClipboardOffer {
            meta,
            descriptor: Some(file_descriptor("a.txt", 3)),
        };
        let mut expected: Vec<u8> = Vec::new();
        expected.push(0x10); // id: 16-byte length prefix
        expected.extend([0x11; 16]);
        expected.push(0x10); // origin: 16-byte length prefix
        expected.extend([0x22; 16]);
        expected.push(0x05); // sequence varint
        expected.push(0x02); // ContentType::File
        expected.push(0x03); // content_length varint
        expected.extend(meta.content_hash); // hash: fixed 32, no prefix
        expected.push(0x01); // descriptor: Some
        expected.extend([0x05, b'a', b'.', b't', b'x', b't']); // file_name
        expected.push(0x00); // archived: false
        expected.push(0x01); // entry_count varint
        expected.push(0x03); // original_bytes varint
        assert_eq!(
            item.encode_payload().unwrap(),
            expected,
            "v3 file ClipboardOffer wire layout changed: bump the protocol version"
        );
    }

    /// The layout change that made this protocol version 3: every offer,
    /// of every type, now ends in the descriptor's `Option` tag. A v2 peer
    /// reads that byte as trailing data and fails the payload, which is
    /// why the floor moved with the ceiling.
    #[test]
    fn every_offer_carries_the_descriptor_tag() {
        let image = offer(image_meta(b"px")).encode_payload().unwrap();
        assert_eq!(
            image.last(),
            Some(&0x00),
            "a non-file offer ends in the None tag"
        );
    }

    /// A descriptor belongs to a file offer and to nothing else, in both
    /// directions — the rule that keeps a name from arriving attached to
    /// an item nothing will validate it for.
    #[test]
    fn descriptor_presence_must_match_the_content_type() {
        let naked = ClipboardOffer {
            descriptor: None,
            ..file_offer(b"a document")
        };
        assert!(matches!(
            naked.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        // And on the way in, not merely on the way out.
        let bytes = super::encode(&naked).unwrap();
        assert!(matches!(
            ClipboardOffer::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));

        let dressed_image = ClipboardOffer {
            meta: image_meta(b"a snip"),
            descriptor: Some(file_descriptor("sneaky.txt", 6)),
        };
        assert!(matches!(
            dressed_image.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        let bytes = super::encode(&dressed_image).unwrap();
        assert!(matches!(
            ClipboardOffer::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// The corpus ADR 0015 requires, run through the *wire* path rather
    /// than the validator alone: a hostile name must not survive decode,
    /// so no descriptor carrying one ever exists to be handed to a shell.
    #[test]
    fn hostile_file_names_never_survive_decode() {
        let hostile = [
            "",                                    // empty
            "../../etc/passwd",                    // traversal
            "..\\..\\Windows\\System32\\evil.dll", // traversal, Windows form
            "..",                                  // the parent directory itself
            ".",                                   // the current one
            "/etc/passwd",                         // absolute
            "\\Windows\\System32\\evil.dll",       // absolute, Windows form
            "\\\\server\\share\\payload.exe",      // UNC
            "C:\\Windows\\System32\\evil.dll",     // drive letter
            "c:payload.exe",                       // drive-relative
            "subdir/payload.exe",                  // a path, not a name
            "CON",                                 // reserved device name
            "nul.txt",                             // ... with an extension
            "LPT9",                                // ... and the numbered family
            "report.pdf.",                         // trailing dot
            "report.pdf ",                         // trailing space
            "na\u{0}me.txt",                       // NUL
            "na\nme.txt",                          // Cc
            "invoice\u{202E}gnp.exe",              // Cf: right-to-left override
            "invoice\u{200F}gnp.exe",              // Cf: right-to-left mark
            "stream.txt:$DATA",                    // reserved character
            "wild*.txt",                           // ... and the rest of the set
        ];
        for name in hostile {
            let item = ClipboardOffer {
                descriptor: Some(file_descriptor(name, 10)),
                ..file_offer(b"ten bytes!")
            };
            // Encoding refuses it: we never send what we would refuse.
            assert!(
                matches!(item.encode_payload(), Err(ProtocolError::Malformed { .. })),
                "a hostile name encoded: {name:?}"
            );
            // And decoding refuses it, which is the direction that matters:
            // the bytes come from a peer that does not run our encoder.
            let bytes = super::encode(&item).unwrap();
            assert!(
                matches!(
                    ClipboardOffer::decode_payload(&bytes),
                    Err(ProtocolError::Malformed { .. })
                ),
                "a hostile name survived decode: {name:?}"
            );
        }

        // Over-length in bytes, built past the validator the same way.
        let item = ClipboardOffer {
            descriptor: Some(file_descriptor(&"x".repeat(MAX_FILE_NAME_BYTES + 1), 10)),
            ..file_offer(b"ten bytes!")
        };
        let bytes = super::encode(&item).unwrap();
        assert!(matches!(
            ClipboardOffer::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// A name is a `String` on the wire, so invalid UTF-8 is refused by
    /// the decoder itself — before any of our rules run, and without
    /// allocating a name.
    #[test]
    fn a_non_utf8_file_name_is_refused_by_the_decoder() {
        let good = file_offer(b"a document").encode_payload().unwrap();
        // The name is the only text field; corrupt its first byte into an
        // invalid UTF-8 lead byte.
        let start = good
            .windows(10)
            .position(|window| window == b"report.pdf")
            .expect("the encoded name is findable");
        let mut broken = good;
        broken[start] = 0xFF; // never a valid UTF-8 byte
        assert!(matches!(
            ClipboardOffer::decode_payload(&broken),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn descriptor_counts_and_sizes_are_bounded() {
        let base = file_offer(b"ten bytes!");
        let with = |descriptor: FileDescriptor| ClipboardOffer {
            descriptor: Some(descriptor),
            ..base.clone()
        };

        // Nothing packed at all.
        let mut empty = file_descriptor("a.zip", 10);
        empty.entry_count = 0;
        assert!(matches!(
            with(empty).validate(),
            Err(ProtocolError::Malformed { .. })
        ));

        // More entries than an archive may pack.
        let over = FileDescriptor {
            file_name: "a.zip".to_owned(),
            archived: true,
            entry_count: MAX_CLIPBOARD_FILE_ENTRIES + 1,
            original_bytes: 10,
        };
        assert!(matches!(
            with(over).validate(),
            Err(ProtocolError::Malformed { .. })
        ));
        // The boundary itself is fine.
        let at_limit = FileDescriptor {
            entry_count: MAX_CLIPBOARD_FILE_ENTRIES,
            ..FileDescriptor {
                file_name: "a.zip".to_owned(),
                archived: true,
                entry_count: 1,
                original_bytes: 10,
            }
        };
        assert!(with(at_limit).validate().is_ok());

        // Many entries but not an archive: the two fields disagree.
        let inconsistent = FileDescriptor {
            file_name: "a.txt".to_owned(),
            archived: false,
            entry_count: 7,
            original_bytes: 10,
        };
        assert!(matches!(
            with(inconsistent).validate(),
            Err(ProtocolError::Malformed { .. })
        ));

        // An uncompressed total past the item ceiling.
        let huge = FileDescriptor {
            file_name: "a.zip".to_owned(),
            archived: true,
            entry_count: 2,
            original_bytes: (MAX_CLIPBOARD_FILE_BYTES as u64) + 1,
        };
        assert!(matches!(
            with(huge).validate(),
            Err(ProtocolError::Malformed { .. })
        ));

        // A single file travels verbatim, so its uncompressed size is the
        // offered length; only an archive may declare a different one.
        let lying = file_descriptor("a.txt", 9);
        assert!(matches!(
            with(lying).validate(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    #[test]
    fn files_are_bounded_by_their_own_ceiling() {
        let at_limit = ClipboardOffer {
            meta: ClipboardMeta {
                content_length: MAX_CLIPBOARD_FILE_BYTES as u64,
                ..file_meta(b"unused")
            },
            descriptor: Some(file_descriptor("big.zip", MAX_CLIPBOARD_FILE_BYTES as u64)),
        };
        assert!(at_limit.validate().is_ok());

        let over = ClipboardOffer {
            meta: ClipboardMeta {
                content_length: (MAX_CLIPBOARD_FILE_BYTES as u64) + 1,
                ..file_meta(b"unused")
            },
            ..at_limit.clone()
        };
        assert!(matches!(
            over.validate(),
            Err(ProtocolError::Malformed { .. })
        ));

        // A file is far larger than an image is allowed to be, and each
        // type is judged by its own maximum.
        assert_eq!(
            ContentType::File.max_content_bytes(),
            MAX_CLIPBOARD_FILE_BYTES as u64
        );
        assert!(
            ContentType::File.max_content_bytes()
                > ContentType::Image(ImageFormat::Dib).max_content_bytes()
        );

        // A zero-byte item is not a transfer, for files as for images.
        let empty = ClipboardOffer {
            meta: ClipboardMeta {
                content_length: 0,
                ..file_meta(b"unused")
            },
            ..at_limit
        };
        assert!(matches!(
            empty.validate(),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Files are chunked, but never buffered: the receiver writes them
    /// through to its spool, so the in-memory reassembly is refused
    /// outright rather than quietly reserving up to 256 MiB (ADR 0015).
    #[test]
    fn file_content_is_never_reassembled_in_memory() {
        assert!(ContentType::File.is_chunked());
        assert!(matches!(
            ChunkReassembly::begin(file_meta(b"a document")),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Feed every chunk into a stream, collecting what the caller would
    /// have written. The bytes come back out of the *caller's* sink, never
    /// out of the stream — which is the difference being tested.
    fn stream_through(
        meta: ClipboardMeta,
        chunks: &[ClipboardChunk],
    ) -> Result<Vec<u8>, ProtocolError> {
        let mut stream = ChunkStream::begin(meta)?;
        let mut sink: Vec<u8> = Vec::new();
        let mut last = StreamOutcome::More;
        for chunk in chunks {
            last = stream.accept(chunk)?;
            sink.extend_from_slice(&chunk.payload);
        }
        assert_eq!(last, StreamOutcome::Final, "the last chunk must complete");
        assert_eq!(stream.received_bytes(), meta.content_length);
        Ok(sink)
    }

    /// The happy path, over more than one chunk: what the caller wrote is
    /// the offered item, proved by a hash the stream computed without ever
    /// holding the bytes.
    #[test]
    fn a_streamed_file_verifies_without_being_buffered() {
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let meta = file_meta(&content);
        let chunks = chunk_content(ITEM, &content).unwrap();
        assert!(chunks.len() > 1, "the fixture must span several chunks");
        assert_eq!(stream_through(meta, &chunks).unwrap(), content);
    }

    /// The one type `ChunkReassembly` refuses is the one this exists for,
    /// and the two agree on everything else about what may be streamed.
    #[test]
    fn a_stream_takes_the_types_a_reassembly_does_and_the_file_type_too() {
        assert!(ChunkStream::begin(file_meta(b"a document")).is_ok());
        assert!(ChunkStream::begin(image_meta(b"raster")).is_ok());

        let text = ClipboardMeta {
            content_type: ContentType::Utf8Text,
            ..file_meta(b"not chunked")
        };
        assert!(matches!(
            ChunkStream::begin(text),
            Err(ProtocolError::Malformed { .. })
        ));

        let oversized = ClipboardMeta {
            content_length: (MAX_CLIPBOARD_FILE_BYTES as u64) + 1,
            ..file_meta(b"unused")
        };
        assert!(matches!(
            ChunkStream::begin(oversized),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Content that is not what was offered never completes: the final
    /// chunk is refused, so the caller deletes its partial rather than
    /// registering bytes nobody vouched for.
    #[test]
    fn a_stream_refuses_content_that_is_not_the_offered_item() {
        let content = b"the document that was offered".to_vec();
        let chunks = chunk_content(ITEM, &content).unwrap();
        let lying = ClipboardMeta {
            content_hash: content_hash(b"something else entirely"),
            ..file_meta(&content)
        };
        let mut stream = ChunkStream::begin(lying).unwrap();
        assert!(matches!(
            stream.accept(&chunks[0]),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// Every fail-closed rule of the reassembly holds here too, and each
    /// one fires *before* the payload would have been written.
    #[test]
    fn a_stream_rejects_out_of_sequence_short_and_late_chunks() {
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 241) as u8).collect();
        let meta = file_meta(&content);
        let chunks = chunk_content(ITEM, &content).unwrap();

        // A gap: chunk 1 without chunk 0.
        let mut stream = ChunkStream::begin(meta).unwrap();
        assert!(matches!(
            stream.accept(&chunks[1]),
            Err(ProtocolError::Malformed { .. })
        ));
        assert_eq!(stream.received_bytes(), 0, "a refusal writes nothing");

        // A repeat.
        let mut stream = ChunkStream::begin(meta).unwrap();
        assert_eq!(stream.accept(&chunks[0]).unwrap(), StreamOutcome::More);
        assert!(matches!(
            stream.accept(&chunks[0]),
            Err(ProtocolError::Malformed { .. })
        ));

        // A chunk that is not the length its position requires: the check
        // that stops a running total from ever passing the declared one.
        let mut stream = ChunkStream::begin(meta).unwrap();
        let short = ClipboardChunk {
            id: ITEM,
            index: 0,
            payload: chunks[0].payload[..chunks[0].payload.len() - 1].to_vec(),
        };
        assert_eq!(stream.accept(&chunks[0]).unwrap(), StreamOutcome::More);
        let mut fresh = ChunkStream::begin(meta).unwrap();
        assert_eq!(fresh.accept(&short).unwrap(), StreamOutcome::More);
        assert!(
            matches!(
                fresh.accept(&chunks[1]),
                Err(ProtocolError::Malformed { .. })
            ),
            "a plan derived from a short chunk 0 cannot admit a full chunk 1"
        );

        // A chunk for another item.
        let mut stream = ChunkStream::begin(meta).unwrap();
        let foreign = ClipboardChunk {
            id: Uuid::from_u128(0xfeed),
            index: 0,
            payload: chunks[0].payload.clone(),
        };
        assert!(matches!(
            stream.accept(&foreign),
            Err(ProtocolError::Malformed { .. })
        ));

        // A tail after completion.
        let mut stream = ChunkStream::begin(meta).unwrap();
        for chunk in &chunks {
            stream.accept(chunk).unwrap();
        }
        assert!(matches!(
            stream.accept(&chunks[0]),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    /// One path per type: a file never travels as `ClipboardData` either.
    #[test]
    fn file_content_is_rejected_as_clipboard_data() {
        let content = b"a document".to_vec();
        let item = ClipboardData {
            meta: file_meta(&content),
            content,
        };
        assert!(matches!(
            item.encode_payload(),
            Err(ProtocolError::Malformed { .. })
        ));
        let bytes = super::encode(&item).unwrap();
        assert!(matches!(
            ClipboardData::decode_payload(&bytes),
            Err(ProtocolError::Malformed { .. })
        ));
    }

    proptest! {
        /// Arbitrary bytes never panic the offer decoder — the path that
        /// now carries a peer-supplied name — and anything it accepts
        /// round-trips and carries a name that still validates.
        #[test]
        fn arbitrary_bytes_never_panic_the_offer_decoder(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            if let Ok(item) = ClipboardOffer::decode_payload(&bytes) {
                let again = ClipboardOffer::decode_payload(&item.encode_payload().unwrap())
                    .unwrap();
                prop_assert_eq!(&item, &again);
                if let Some(descriptor) = &item.descriptor {
                    prop_assert!(crate::validate_file_name(&descriptor.file_name).is_ok());
                    prop_assert!(descriptor.file_name.len() <= MAX_FILE_NAME_BYTES);
                    prop_assert!(descriptor.entry_count >= 1);
                    prop_assert!(descriptor.entry_count <= MAX_CLIPBOARD_FILE_ENTRIES);
                }
            }
        }

        /// Arbitrary *descriptors*, most of them nonsense: every outcome
        /// is a value, and an accepted one survives the wire unchanged.
        #[test]
        fn arbitrary_file_offers_reject_or_round_trip(
            name in ".{0,300}",
            archived in any::<bool>(),
            entry_count in 0u32..1000,
            original_bytes in 0u64..(2 * MAX_CLIPBOARD_FILE_BYTES as u64),
            content_length in 0u64..(2 * MAX_CLIPBOARD_FILE_BYTES as u64),
        ) {
            let item = ClipboardOffer {
                meta: ClipboardMeta { content_length, ..file_meta(b"unused") },
                descriptor: Some(FileDescriptor {
                    file_name: name,
                    archived,
                    entry_count,
                    original_bytes,
                }),
            };
            let wire = super::encode(&item).unwrap();
            match ClipboardOffer::decode_payload(&wire) {
                Ok(decoded) => {
                    prop_assert_eq!(&decoded, &item);
                    let descriptor = decoded.descriptor.expect("a file offer has a descriptor");
                    prop_assert!(crate::validate_file_name(&descriptor.file_name).is_ok());
                    prop_assert!(content_length >= 1);
                    prop_assert!(content_length <= MAX_CLIPBOARD_FILE_BYTES as u64);
                    prop_assert!(descriptor.original_bytes <= MAX_CLIPBOARD_FILE_BYTES as u64);
                    prop_assert!(descriptor.archived || descriptor.entry_count == 1);
                }
                Err(ProtocolError::Malformed { .. }) => {}
                Err(other) => prop_assert!(false, "unexpected error: {other:?}"),
            }
        }
    }
}
