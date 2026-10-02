//! WIC-backed [`ImageConverter`]: this machine's own `CF_DIB` to PNG
//! (ADR 0016 and its 2026-09-28 amendment).
//!
//! Windows needs one conversion: a peer that installs only PNG — a Mac —
//! is sent PNG, encoded here from the DIB the local clipboard gave. The
//! reverse is the other platform's job, because the sender converts.
//!
//! **Only local content reaches this code.** The DIB comes from this
//! machine's clipboard, read moments ago, and ADR 0016's whole security
//! argument is that nothing a peer sent is ever handed to an image decoder.
//! The Windows Imaging Component does decode here — it reads the DIB as a
//! BMP — and that is acceptable exactly because the input is trusted.
//!
//! The DIB is wrapped in a 14-byte `BITMAPFILEHEADER` so WIC's BMP decoder
//! can read it from memory; the header's pixel offset comes from the same
//! arithmetic `canonical_dib` uses (`dib::dib_pixel_offset`). A 32-bpp
//! `BI_RGB` DIB is read the way Windows defines it — the fourth byte is
//! unused, not alpha — so its PNG has no transparency; that is what the
//! local clipboard said.
//!
//! **Blocking**: the clipboard driver runs it off its loop. COM is
//! initialized for the calling thread for the duration of a call.

use crossover_platform::{ClipboardImageFormat, ImageConvertError, ImageConverter};
use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, IWICBitmapFrameEncode, IWICImagingFactory,
    WICBitmapEncoderNoCache, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    IStream, STREAM_SEEK_CUR,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

use crate::dib::{dib_logical_len, dib_pixel_offset};

/// Converts local clipboard images with the Windows Imaging Component.
#[derive(Debug, Default, Clone, Copy)]
pub struct WicImageConverter;

impl ImageConverter for WicImageConverter {
    fn convert(
        &self,
        from: ClipboardImageFormat,
        to: ClipboardImageFormat,
        bytes: &[u8],
    ) -> Result<Vec<u8>, ImageConvertError> {
        if (from, to) != (ClipboardImageFormat::Dib, ClipboardImageFormat::Png) {
            return Err(ImageConvertError::Unsupported { from, to });
        }
        let bmp = bmp_file(bytes)?;
        let _com = ComGuard::init();
        dib_file_to_png(&bmp)
    }
}

/// COM for this thread, for the length of one conversion. A thread that
/// already has an apartment keeps it — WIC works in either — and is not
/// uninitialized by us.
struct ComGuard {
    initialized: bool,
}

impl ComGuard {
    fn init() -> Self {
        // SAFETY: no reserved pointer; initializes COM for this thread only.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK and S_FALSE both need a matching CoUninitialize;
        // RPC_E_CHANGED_MODE (an existing STA) does not.
        Self {
            initialized: result.is_ok(),
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful CoInitializeEx above, on the
            // same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// The DIB as a BMP file: a `BITMAPFILEHEADER`, then the DIB exactly as
/// the clipboard gave it (trimmed to its own logical length).
fn bmp_file(dib: &[u8]) -> Result<Vec<u8>, ImageConvertError> {
    let unreadable = || ImageConvertError::Failed {
        reason: "the local DIB's header does not describe it".to_owned(),
    };
    let logical = dib_logical_len(dib).ok_or_else(unreadable)?;
    let pixel_offset = dib_pixel_offset(dib).ok_or_else(unreadable)?;
    let file_len = u32::try_from(14 + logical).map_err(|_| unreadable())?;
    let mut bmp = Vec::with_capacity(14 + logical);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_len.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]); // bfReserved1, bfReserved2
    bmp.extend_from_slice(&(14 + pixel_offset).to_le_bytes());
    bmp.extend_from_slice(&dib[..logical]);
    Ok(bmp)
}

fn failed(step: &str, error: &windows::core::Error) -> ImageConvertError {
    ImageConvertError::Failed {
        reason: format!("{step}: {error}"),
    }
}

/// Decode a BMP file from memory and encode its first frame as PNG.
fn dib_file_to_png(bmp: &[u8]) -> Result<Vec<u8>, ImageConvertError> {
    // SAFETY: every call below is a WIC / COM method on interfaces this
    // function created and holds; `bmp` outlives the stream that reads it
    // (the stream and decoder are dropped before this function returns);
    // the output HGLOBAL is owned by the stream (delete-on-release) and is
    // read under GlobalLock before the stream is released.
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| failed("creating the imaging factory", &e))?;

        let input = factory
            .CreateStream()
            .map_err(|e| failed("creating the input stream", &e))?;
        input
            .InitializeFromMemory(bmp)
            .map_err(|e| failed("reading the image from memory", &e))?;
        let decoder = factory
            .CreateDecoderFromStream(&input, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .map_err(|e| failed("reading the image", &e))?;
        let frame = decoder
            .GetFrame(0)
            .map_err(|e| failed("reading the image's frame", &e))?;

        let output: IStream = CreateStreamOnHGlobal(HGLOBAL::default(), true)
            .map_err(|e| failed("creating the output stream", &e))?;
        let encoder = factory
            .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())
            .map_err(|e| failed("creating the PNG encoder", &e))?;
        encoder
            .Initialize(&output, WICBitmapEncoderNoCache)
            .map_err(|e| failed("starting the PNG encoder", &e))?;
        let mut frame_encode: Option<IWICBitmapFrameEncode> = None;
        encoder
            .CreateNewFrame(&raw mut frame_encode, std::ptr::null_mut())
            .map_err(|e| failed("creating the PNG frame", &e))?;
        let frame_encode = frame_encode.ok_or_else(|| ImageConvertError::Failed {
            reason: "the PNG encoder returned no frame".to_owned(),
        })?;
        frame_encode
            .Initialize(None)
            .map_err(|e| failed("starting the PNG frame", &e))?;
        frame_encode
            .WriteSource(&frame, std::ptr::null())
            .map_err(|e| failed("encoding the image", &e))?;
        frame_encode
            .Commit()
            .map_err(|e| failed("finishing the PNG frame", &e))?;
        encoder
            .Commit()
            .map_err(|e| failed("finishing the PNG", &e))?;

        // How much the encoder wrote: the stream's position. The HGLOBAL
        // behind it may be larger, and what lies past the written bytes is
        // not ours to interpret.
        let mut written = 0u64;
        output
            .Seek(0, STREAM_SEEK_CUR, Some(&raw mut written))
            .map_err(|e| failed("measuring the encoded PNG", &e))?;
        let written = usize::try_from(written).map_err(|_| ImageConvertError::Failed {
            reason: "the encoded PNG is larger than this process can hold".to_owned(),
        })?;
        let hglobal =
            GetHGlobalFromStream(&output).map_err(|e| failed("finding the encoded PNG", &e))?;
        // The stream owns the HGLOBAL (delete-on-release); it is freed when
        // `output` drops, after this copy.
        copy_hglobal(hglobal, written)
    }
}

/// Copy the first `written` bytes out of an HGLOBAL.
///
/// # Safety
///
/// `hglobal` must be a live global memory handle.
unsafe fn copy_hglobal(hglobal: HGLOBAL, written: usize) -> Result<Vec<u8>, ImageConvertError> {
    // SAFETY: per the contract, `hglobal` is live; GlobalSize reads its
    // allocation size without touching its contents.
    let size = unsafe { GlobalSize(hglobal) };
    if written == 0 || written > size {
        return Err(ImageConvertError::Failed {
            reason: "the encoded PNG could not be read back".to_owned(),
        });
    }
    // SAFETY: as above; GlobalLock pins the block and yields its base.
    let ptr = unsafe { GlobalLock(hglobal) }.cast::<u8>();
    if ptr.is_null() {
        return Err(ImageConvertError::Failed {
            reason: "the encoded PNG could not be locked".to_owned(),
        });
    }
    // SAFETY: `written` <= `size` bytes starting at `ptr` are inside the
    // pinned block.
    let png = unsafe { std::slice::from_raw_parts(ptr, written) }.to_vec();
    // SAFETY: balances the GlobalLock above.
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(png)
}

#[cfg(test)]
mod tests {
    use crossover_platform::{ClipboardImageFormat, ImageConvertError, ImageConverter};

    use super::WicImageConverter;

    /// A 32-bpp bottom-up `BI_RGB` DIB of `width` x `height`, every pixel
    /// the same colour.
    fn dib(width: i32, height: i32) -> Vec<u8> {
        let stride = usize::try_from(width).unwrap() * 4;
        let pixels = stride * usize::try_from(height).unwrap();
        let mut blob = Vec::with_capacity(40 + pixels);
        blob.extend_from_slice(&40u32.to_le_bytes());
        blob.extend_from_slice(&width.to_le_bytes());
        blob.extend_from_slice(&height.to_le_bytes());
        blob.extend_from_slice(&1u16.to_le_bytes()); // planes
        blob.extend_from_slice(&32u16.to_le_bytes()); // bit count
        blob.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        blob.extend_from_slice(&u32::try_from(pixels).unwrap().to_le_bytes());
        blob.extend_from_slice(&[0; 16]); // resolution, colours used/important
        for _ in 0..width * height {
            blob.extend_from_slice(&[0x40, 0x80, 0xC0, 0x00]);
        }
        blob
    }

    /// PNG's dimensions, read from its `IHDR` — enough to prove the image
    /// survived without a decoder in the test.
    fn png_size(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
        assert_eq!(&png[12..16], b"IHDR");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        (width, height)
    }

    #[test]
    fn a_local_dib_becomes_a_png_of_the_same_size() {
        let png = WicImageConverter
            .convert(
                ClipboardImageFormat::Dib,
                ClipboardImageFormat::Png,
                &dib(37, 11),
            )
            .unwrap();
        assert_eq!(png_size(&png), (37, 11));
        assert!(
            png.ends_with(&[0xAE, 0x42, 0x60, 0x82]),
            "no IEND CRC at the end"
        );
    }

    /// Deterministic for the same input — the property that lets the
    /// receiver's dedup recognise a re-sent image by hash.
    #[test]
    fn the_same_dib_encodes_to_the_same_png() {
        let dib = dib(64, 48);
        let once = WicImageConverter
            .convert(ClipboardImageFormat::Dib, ClipboardImageFormat::Png, &dib)
            .unwrap();
        let again = WicImageConverter
            .convert(ClipboardImageFormat::Dib, ClipboardImageFormat::Png, &dib)
            .unwrap();
        assert_eq!(once, again);
    }

    #[test]
    fn only_dib_to_png_is_offered_and_the_rest_is_refused_permanently() {
        for (from, to) in [
            (ClipboardImageFormat::Png, ClipboardImageFormat::Dib),
            (ClipboardImageFormat::Dib, ClipboardImageFormat::Jpeg),
            (ClipboardImageFormat::Jpeg, ClipboardImageFormat::Png),
        ] {
            assert!(matches!(
                WicImageConverter.convert(from, to, &dib(2, 2)),
                Err(ImageConvertError::Unsupported { .. })
            ));
        }
    }

    #[test]
    fn a_dib_whose_header_does_not_describe_it_is_refused() {
        let mut liar = dib(8, 8);
        liar.truncate(60);
        assert!(matches!(
            WicImageConverter.convert(ClipboardImageFormat::Dib, ClipboardImageFormat::Png, &liar),
            Err(ImageConvertError::Failed { .. })
        ));
    }
}
