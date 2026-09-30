//! `CF_DIB` header arithmetic (ADR 0014; ADR 0016's 2026-09-28 amendment).
//!
//! Pure bytes-in, length-out, and deliberately **not** Windows-gated, for
//! the reason `edid` is not: nothing here calls Win32, and keeping it
//! compiled on every OS lets the Linux fuzz harness reach it
//! (`fuzz/fuzz_targets/canonical_dib.rs`). That matters because this is the
//! one piece of image arithmetic peer-originated bytes reach: after a
//! peer's DIB is installed, the read that follows Crossover's own write
//! runs [`canonical_dib`] over it. It reads the header's geometry fields
//! and never a pixel — arithmetic, not a decoder.

/// Size of the `BITMAPINFOHEADER` that opens every `CF_DIB` blob. The
/// larger V4/V5 headers belong to `CF_DIBV5`; Windows' synthesis hands
/// `CF_DIB` requests this one.
const BITMAPINFOHEADER_BYTES: u32 = 40;

// `biCompression` values, from wingdi.h. Only the arithmetic each implies
// is used; no pixel data is ever examined.
const BI_RGB: u32 = 0;
const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const BI_JPEG: u32 = 4;
const BI_PNG: u32 = 5;
const BI_ALPHABITFIELDS: u32 = 6;

/// Trim allocator slack from a `CF_DIB` blob, or keep it whole.
///
/// Verbatim means *the bitmap*, and a global block may be larger than the
/// bitmap it carries. Trimming it is not cosmetic: loop prevention (FR-3.3)
/// keys on the content hash, so a blob that gained pad bytes on every hop
/// would read back as new content after Crossover's own write — a clipboard
/// sync loop, which is release-blocking. Truncating to the header's own
/// arithmetic makes the round trip a fixed point instead.
///
/// Conservative by construction: anything the header does not describe
/// confidently, or any computed length the blob is too short for, keeps
/// the blob exactly as the OS gave it. The failure mode is therefore "a
/// few unused bytes travel", never "a valid image is cut short".
#[must_use]
pub fn canonical_dib(mut blob: Vec<u8>) -> Vec<u8> {
    if let Some(logical) = dib_logical_len(&blob) {
        blob.truncate(logical);
    }
    blob
}

/// The logical byte length of a `CF_DIB` blob: header + colour
/// table/masks + pixel data, per the `BITMAPINFOHEADER` contract.
///
/// `None` means "do not trust this" — an unrecognized header, implausible
/// dimensions, or arithmetic the blob cannot satisfy — and the caller then
/// keeps the whole blob. Nothing here reads a single pixel; the fields
/// consumed are the geometry ones that fix the layout.
#[must_use]
pub fn dib_logical_len(blob: &[u8]) -> Option<usize> {
    if le_u32(blob, 0)? != BITMAPINFOHEADER_BYTES {
        return None; // not a BITMAPINFOHEADER-shaped DIB
    }
    let width = le_i32(blob, 4)?;
    let height = le_i32(blob, 8)?;
    let planes = le_u16(blob, 12)?;
    let bit_count = le_u16(blob, 14)?;
    let compression = le_u32(blob, 16)?;
    let size_image = u64::from(le_u32(blob, 20)?);
    let clr_used = u64::from(le_u32(blob, 32)?);

    // Plausibility, not validation: a DIB whose geometry we cannot trust
    // is one whose length we must not compute.
    if planes != 1 || width <= 0 || height == 0 {
        return None;
    }
    if !matches!(bit_count, 1 | 4 | 8 | 16 | 24 | 32) {
        return None;
    }

    // What sits between the header and the pixels. At <= 8 bpp that is a
    // palette (biClrUsed entries, or the full 2^bpp when it is zero); at
    // higher depths it is the bit-field masks, plus any optimization
    // palette biClrUsed still claims. Over-counting here is safe: the
    // total simply fails the length check below and the blob stays whole.
    let table = if bit_count <= 8 {
        let entries = if clr_used == 0 {
            1u64 << bit_count
        } else {
            clr_used
        };
        if entries > 256 {
            return None;
        }
        entries * 4
    } else {
        let masks = match compression {
            BI_BITFIELDS => 12,
            BI_ALPHABITFIELDS => 16,
            _ => 0,
        };
        masks + clr_used * 4
    };

    let pixels = match compression {
        BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS => {
            // Rows are padded to a 4-byte boundary; height may be
            // negative for a top-down DIB, which changes the row order,
            // not the size. `biSizeImage` is allowed to be 0 for
            // uncompressed data, and is allowed to be larger than the
            // strict minimum — take whichever is bigger so a producer
            // that padded the buffer is not cut short.
            let stride = (u64::from(width.unsigned_abs()) * u64::from(bit_count)).div_ceil(32) * 4;
            let rows = u64::from(height.unsigned_abs());
            stride.checked_mul(rows)?.max(size_image)
        }
        // Compressed payloads have no computable size: `biSizeImage` is
        // the only statement of it, and is mandatory here.
        BI_RLE4 | BI_RLE8 | BI_JPEG | BI_PNG => {
            if size_image == 0 {
                return None;
            }
            size_image
        }
        _ => return None, // an encoding this code does not model
    };

    let total = u64::from(BITMAPINFOHEADER_BYTES)
        .checked_add(table)?
        .checked_add(pixels)?;
    let total = usize::try_from(total).ok()?;
    // A blob shorter than its own header claims is either malformed or
    // beyond this model; either way, hand it back untouched.
    (total <= blob.len()).then_some(total)
}

/// Little-endian field readers. Bounds-checked, so a truncated blob is
/// `None` rather than a panic (NFR-1: malformed input never panics).
fn le_u16(blob: &[u8], at: usize) -> Option<u16> {
    blob.get(at..at + 2)?
        .try_into()
        .ok()
        .map(u16::from_le_bytes)
}

fn le_u32(blob: &[u8], at: usize) -> Option<u32> {
    blob.get(at..at + 4)?
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}

fn le_i32(blob: &[u8], at: usize) -> Option<i32> {
    blob.get(at..at + 4)?
        .try_into()
        .ok()
        .map(i32::from_le_bytes)
}
