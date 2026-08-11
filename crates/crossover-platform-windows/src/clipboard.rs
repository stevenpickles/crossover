//! Win32 [`ClipboardProvider`] (FR-3.1; platform risks R-4/R-5 in
//! docs/SPECIFICATION.md §6).
//!
//! Change observation uses the modern listener API
//! (`AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE`) on a dedicated
//! thread owning a message-only window — robust to other clipboard
//! listeners by construction, unlike the legacy viewer chain (R-4).
//! `WM_CLIPBOARDUPDATE` fires once per clipboard state change, including
//! for our own writes — exactly the contract term `ClipboardProvider`
//! documents. Sequence-number polling is unnecessary on top of it: the
//! engine's content hashing already collapses spurious wakeups.
//!
//! Contention (R-5): `OpenClipboard` failing is routine — another process
//! holds the clipboard — and maps to [`ClipboardError::Busy`] for the
//! engine's bounded retry. Real failures map to `Unavailable`.
//!
//! Contention runs both ways, and Crossover must be a good neighbour:
//! the clipboard is a machine-global lock, so every other application's
//! copy and paste blocks while we hold it. Both paths therefore keep the
//! critical section to the clipboard calls alone — allocation, the byte
//! copy, and UTF-16 conversion all happen outside it. The two-machine
//! soak proved this matters: holding across that work made PowerShell's
//! `Set-Clipboard` fail outright during bidirectional sync.
//!
//! Text is `CF_UNICODETEXT` (Windows synthesizes it from `CF_TEXT`
//! automatically), converted UTF-16 ↔ UTF-8 at this boundary.
//!
//! # Images (ADR 0014's platform slice)
//!
//! **`CF_DIB`, verbatim.** The blob `GetClipboardData(CF_DIB)` hands back —
//! `BITMAPINFOHEADER`, colour table/masks, pixels — travels exactly as it
//! is. Nothing here transcodes, compresses, or re-encodes; the ADR's whole
//! image story is "the source's own raster bytes, byte-identical".
//!
//! **Synthesis is relied on, not reimplemented.** A source that publishes
//! only `CF_BITMAP` or `CF_DIBV5` still answers a `CF_DIB` request:
//! Windows synthesizes the missing member of that family on demand, and
//! `IsClipboardFormatAvailable` reports synthesized formats as available.
//! So one availability probe plus one `GetClipboardData` covers all three,
//! and Crossover never converts pixels itself — the conversion is the OS's,
//! written once and correct for every source.
//!
//! **Precedence when the clipboard holds both: text wins.** Mixed content
//! is common (Excel, Word, and browsers publish `CF_UNICODETEXT` alongside
//! a rendered `CF_DIB`), and the transaction carries exactly one type, so
//! this is a real choice:
//!
//! - The image in a mixed item is nearly always a *rendering* of the text —
//!   the user copied cells or a formatted selection, and text is what they
//!   mean to paste. Sending the picture instead would be a silent
//!   downgrade: text pastes into anything, the DIB into far less.
//! - Text is byte-identical at a fraction of the size (FR-3.2 costs
//!   nothing here, and FR-3.6's ceiling is never approached).
//! - The case ADR 0014 exists for — a screenshot or a Snipping Tool
//!   capture — publishes **no** text at all, so image-first would buy that
//!   case nothing while degrading every mixed one.
//! - It is also the behaviour that already shipped and soaked: text-only
//!   reads mean this slice adds a capability without changing any existing
//!   item's outcome.
//!
//! One carve-out, because precedence must not become suppression: the text
//! has to be **non-empty** to win. A source publishing a zero-length
//! `CF_UNICODETEXT` beside a picture would otherwise propagate `""` and
//! blank the peer's clipboard — strictly worse than sending either
//! content. An empty text with no image behind it is unchanged.
//!
//! **The bytes are canonicalized to the DIB's own length.** `GlobalSize`
//! reports the *allocation*, which may be larger than the bitmap inside
//! it, and trailing allocator slack is not part of the image. Worse, it
//! would make a round trip unstable: loop prevention keys on the content
//! hash (FR-3.3), so bytes that grow by a few pad bytes each time they
//! cross the clipboard would read back as *new* content after our own
//! write. So the header — and only the header — is parsed, to compute the
//! blob's logical length; the result is a prefix of what the OS gave us,
//! never a re-encode, and anything unrecognized falls back to the whole
//! blob verbatim.
//!
//! **On write**, `Dib` installs as `CF_DIB` and Windows synthesizes
//! `CF_BITMAP`/`CF_DIBV5`/`CF_PALETTE` for applications that want those.
//! `Png` installs verbatim under the registered `"PNG"` clipboard format —
//! honest for a source that had only PNG, with the documented limitation
//! that Windows synthesizes nothing from it, so `CF_DIB`-only applications
//! see an empty clipboard. `Jpeg` has no comparable convention and is
//! refused permanently rather than guessed at. No format is transcoded
//! into another (ADR 0014); this build's Windows sender emits `Dib`, so the
//! other two arise only from a future non-Windows peer.

use std::sync::{Arc, Mutex, PoisonError};

use crossover_platform::{
    ClipboardContent, ClipboardError, ClipboardImageFormat, ClipboardListener, ClipboardProvider,
    MAX_CLIPBOARD_IMAGE_BYTES,
};
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE,
};
use windows::core::w;

/// Private message asking the pump thread to shut down.
const WM_APP_SHUTDOWN: u32 = WM_APP + 1;

type SharedListener = Arc<Mutex<Option<ClipboardListener>>>;

/// The Win32 clipboard provider. Owns the observation thread; dropping it
/// shuts the thread down.
pub struct WindowsClipboard {
    listener: SharedListener,
    /// The message-only window, as a raw value so it is `Send` (used only
    /// to post the shutdown message; the pump thread owns the window).
    hwnd_raw: isize,
    pump: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: `hwnd_raw` is only used with PostMessageW (thread-safe by API
// contract); the window itself is owned and destroyed by the pump thread.
unsafe impl Send for WindowsClipboard {}
// SAFETY: all shared state (`listener`) is Mutex-guarded; `hwnd_raw` is
// an inert value between the thread-safe PostMessageW uses.
unsafe impl Sync for WindowsClipboard {}

impl WindowsClipboard {
    /// Start the observation thread and create the provider.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Unavailable`] if the observation window or
    /// listener registration fails — fatal by the trait contract, since
    /// silent non-observation would be silent sync failure (NFR-3).
    pub fn new() -> Result<Self, ClipboardError> {
        let listener: SharedListener = Arc::new(Mutex::new(None));
        let pump_listener = Arc::clone(&listener);
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<isize, String>>();

        let pump = std::thread::Builder::new()
            .name("crossover-clipboard-pump".to_owned())
            .spawn(move || pump_thread(&pump_listener, &init_tx))
            .map_err(|e| ClipboardError::Unavailable {
                reason: format!("spawning clipboard pump thread: {e}"),
            })?;

        let hwnd_raw = init_rx
            .recv()
            .map_err(|_| ClipboardError::Unavailable {
                reason: "clipboard pump thread died during startup".to_owned(),
            })?
            .map_err(|reason| ClipboardError::Unavailable { reason })?;

        Ok(Self {
            listener,
            hwnd_raw,
            pump: Some(pump),
        })
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        let hwnd = HWND(self.hwnd_raw as *mut core::ffi::c_void);
        // SAFETY: PostMessageW is safe to call from any thread with any
        // window handle; a stale handle at worst fails harmlessly.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0)) };
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

impl ClipboardProvider for WindowsClipboard {
    /// Reads `CF_UNICODETEXT`, else `CF_DIB` (ADR 0014).
    ///
    /// Text first, deliberately: a clipboard holding both is holding a
    /// rendering of its own text, and the transaction carries one type.
    /// The reasoning is on the module.
    ///
    /// **Non-empty** text, precisely. A source may publish a zero-length
    /// `CF_UNICODETEXT` beside a picture, and letting that win would
    /// propagate `""` — blanking the peer's clipboard instead of sending
    /// the image, which is worse than either content type. So an empty
    /// text representation steps aside for an image, and only for an
    /// image: an empty clipboard with no picture behind it still reads
    /// exactly as it always has.
    ///
    /// An image past [`MAX_CLIPBOARD_IMAGE_BYTES`] reads as *absent* —
    /// the trait's meaning for "nothing this backend represents" — and is
    /// refused before its bytes are copied, never truncated (FR-3.6).
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        match read_unicode_text()? {
            Some(text) if !text.is_empty() => Ok(Some(ClipboardContent::Text(text))),
            empty_or_absent => {
                if let Some(bytes) = read_dib(MAX_CLIPBOARD_IMAGE_BYTES)? {
                    return Ok(Some(ClipboardContent::Image {
                        format: ClipboardImageFormat::Dib,
                        bytes,
                    }));
                }
                Ok(empty_or_absent.map(ClipboardContent::Text))
            }
        }
    }

    /// Writes `CF_UNICODETEXT`, `CF_DIB`, or the registered `"PNG"`
    /// format — each verbatim, none transcoded into another (ADR 0014).
    ///
    /// `Jpeg` is refused as [`ClipboardError::Unsupported`]: permanent, so
    /// the engine does not retry it, and distinguishable by the origin
    /// from a clipboard that is merely busy or broken (FR-3.2, NFR-3).
    fn write(&self, content: &ClipboardContent) -> Result<(), ClipboardError> {
        match content {
            ClipboardContent::Text(text) => write_unicode_text(text),
            ClipboardContent::Image { format, bytes } => write_image(*format, bytes),
        }
    }

    fn set_change_listener(
        &self,
        listener: Option<ClipboardListener>,
    ) -> Result<(), ClipboardError> {
        *self.listener.lock().unwrap_or_else(PoisonError::into_inner) = listener;
        Ok(())
    }
}

/// Read `CF_UNICODETEXT`, or `None` when the clipboard holds no text.
fn read_unicode_text() -> Result<Option<String>, ClipboardError> {
    // SAFETY: no arguments; checks format availability only.
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_err() {
        return Ok(None); // empty clipboard or no text representation
    }

    let open = OpenGuard::open()?;
    // SAFETY: the clipboard is open (guard); the returned handle is
    // owned by the clipboard, not by us. Everything between here and
    // the explicit drop below is the critical section — keep it to
    // the bytes and nothing else.
    // Contention-shaped, observed live: clipboard ownership can churn
    // between our successful open and this call (another process
    // taking the clipboard), surfacing as ERROR_CLIPBOARD_NOT_OPEN.
    // Retryable, not fatal (R-5).
    let handle = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }.map_err(|e| {
        ClipboardError::Busy {
            reason: format!("GetClipboardData failed (ownership churn?): {e}"),
        }
    })?;
    if handle.is_invalid() {
        return Ok(None);
    }

    let hglobal = HGLOBAL(handle.0);
    // SAFETY: `hglobal` came from GetClipboardData while the clipboard
    // is open; GlobalLock pins it and yields the base pointer.
    let ptr = unsafe { GlobalLock(hglobal) }.cast::<u16>();
    if ptr.is_null() {
        // Same churn window as above: the block can vanish with its
        // owner. Retryable.
        return Err(ClipboardError::Busy {
            reason: "GlobalLock on clipboard data failed".to_owned(),
        });
    }
    // SAFETY: CF_UNICODETEXT is null-terminated UTF-16 by contract;
    // scan for the terminator, then copy the units out. Only the copy
    // happens under the clipboard lock — the UTF-16 → String
    // conversion (which allocates, and for a multi-megabyte item is
    // not cheap) happens after releasing, so Crossover is not the
    // reason another application's clipboard call fails.
    let units: Vec<u16> = unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(ptr, len).to_vec()
    };
    // SAFETY: balances the successful GlobalLock above. GlobalUnlock
    // reports "no longer locked" as an error-shaped success; ignore.
    let _ = unsafe { GlobalUnlock(hglobal) };
    drop(open);

    Ok(Some(String::from_utf16_lossy(&units)))
}

/// Read `CF_DIB`, or `None` when the clipboard holds no image, holds one
/// larger than `max_bytes`, or hands back an empty block.
///
/// `max_bytes` is a parameter rather than a constant read inline so the
/// refusal path is testable without fabricating a 64 MiB clipboard item;
/// production always passes [`MAX_CLIPBOARD_IMAGE_BYTES`].
///
/// The ceiling is checked from `GlobalSize` **before** the block is locked
/// or a byte is copied (NFR-1): an oversized item can never be
/// synchronized, so copying it out of the OS clipboard — with the
/// machine-global lock held — would be an allocation spike bought for
/// nothing.
///
/// Note what that compares: `GlobalSize` is the *allocation*, which may be
/// rounded up past the bitmap inside it, so an image whose logical length
/// is a hair under the ceiling can be refused for its allocation being
/// over it. Deliberate, and the right direction to err — the check has to
/// happen before anything is copied, and the logical length is only
/// knowable after. The headroom absorbs it: the ceiling is 64 MiB and the
/// worst realistic capture, a dual-4K span, is 63.3 MiB
/// (docs/PROTOCOL.md §8), so roughly 0.7 MiB of rounding is tolerated
/// before the distinction could ever matter.
fn read_dib(max_bytes: usize) -> Result<Option<Vec<u8>>, ClipboardError> {
    // SAFETY: no arguments; checks format availability only. Synthesized
    // formats count as available, which is exactly what makes this one
    // probe cover CF_BITMAP and CF_DIBV5 sources too.
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_DIB.0)) }.is_err() {
        return Ok(None); // empty clipboard, or no raster representation
    }

    let open = OpenGuard::open()?;
    // SAFETY: the clipboard is open (guard); the returned handle stays
    // owned by the clipboard, never by us. Ownership can churn between
    // our open and this call, which surfaces as an error here and is
    // retryable contention, not a fault (R-5).
    let handle =
        unsafe { GetClipboardData(u32::from(CF_DIB.0)) }.map_err(|e| ClipboardError::Busy {
            reason: format!("GetClipboardData(CF_DIB) failed (ownership churn?): {e}"),
        })?;
    if handle.is_invalid() {
        return Ok(None);
    }

    let hglobal = HGLOBAL(handle.0);
    // SAFETY: `hglobal` came from GetClipboardData while the clipboard is
    // open. GlobalSize reads the block's size without locking or copying
    // it, which is what lets the bound below be enforced before any
    // allocation. It reports 0 for an invalid or discarded block.
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 {
        return Ok(None);
    }
    if size > max_bytes {
        drop(open); // release the machine-global lock before logging
        tracing::warn!(
            byte_count = size,
            max_bytes,
            "clipboard image exceeds the maximum; not synchronized"
        );
        return Ok(None); // graceful refusal, never a truncated image
    }

    // SAFETY: `hglobal` is a live clipboard block; GlobalLock pins it and
    // yields its base pointer.
    let ptr = unsafe { GlobalLock(hglobal) }.cast::<u8>();
    if ptr.is_null() {
        // Same churn window as above: the block can vanish with its
        // owner. Retryable.
        return Err(ClipboardError::Busy {
            reason: "GlobalLock on clipboard image failed".to_owned(),
        });
    }
    // SAFETY: `size` bytes starting at `ptr` are exactly this block, as
    // GlobalSize reported and GlobalLock pinned. The copy is the only
    // work under the machine-global lock — the header parse that
    // canonicalizes the length happens after releasing it, so Crossover
    // is not the reason another application's paste fails (FR-3.1a).
    let blob = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
    // SAFETY: balances the successful GlobalLock above. GlobalUnlock
    // reports "no longer locked" as an error-shaped success; ignore.
    let _ = unsafe { GlobalUnlock(hglobal) };
    drop(open);

    Ok(Some(canonical_dib(blob)))
}

/// Replace the clipboard with `text` as `CF_UNICODETEXT`.
fn write_unicode_text(text: &str) -> Result<(), ClipboardError> {
    // UTF-16LE with the terminator `CF_UNICODETEXT` requires, built
    // straight into bytes: a `str` never encodes to more UTF-16 units
    // than it has bytes, so one allocation covers it and the block that
    // reaches the clipboard is a plain copy of this one.
    let mut encoded = Vec::with_capacity((text.len() + 1) * 2);
    for unit in text.encode_utf16().chain(std::iter::once(0)) {
        encoded.extend_from_slice(&unit.to_le_bytes());
    }
    install_formats(&[(u32::from(CF_UNICODETEXT.0), &encoded)])
}

/// Replace the clipboard with an image, verbatim in the format it arrived
/// in (ADR 0014: no transcoding, here least of all).
fn write_image(format: ClipboardImageFormat, bytes: &[u8]) -> Result<(), ClipboardError> {
    let clipboard_format = match format {
        ClipboardImageFormat::Dib => u32::from(CF_DIB.0),
        // Registered, not predefined: the de-facto interchange name that
        // browsers and image editors publish and accept. Windows
        // synthesizes nothing from it, so a PNG-only clipboard is
        // invisible to CF_DIB-only applications — accepted knowingly,
        // because the alternative would be transcoding.
        ClipboardImageFormat::Png => registered_format(w!("PNG"))?,
        // No comparable Windows convention exists for JPEG on the
        // clipboard. Refusing permanently is the honest answer: the
        // origin learns the type will never install here, rather than
        // watching a retry budget expire (NFR-3).
        ClipboardImageFormat::Jpeg => {
            return Err(ClipboardError::Unsupported {
                reason: "Windows has no clipboard format for verbatim JPEG; \
                         this build does not transcode (ADR 0014)"
                    .to_owned(),
            });
        }
    };
    install_formats(&[(clipboard_format, bytes)])
}

/// Resolve (registering on first use) a named clipboard format.
fn registered_format(name: windows::core::PCWSTR) -> Result<u32, ClipboardError> {
    // SAFETY: `name` is a static null-terminated wide literal. The call
    // is idempotent — a name already registered returns its existing id —
    // and returns 0 on failure.
    let id = unsafe { RegisterClipboardFormatW(name) };
    if id == 0 {
        return Err(ClipboardError::Unavailable {
            reason: "RegisterClipboardFormatW failed".to_owned(),
        });
    }
    Ok(id)
}

/// Replace the clipboard contents with one block per `(format, bytes)`.
///
/// The single place the Win32 ownership rules are honoured, because they
/// are the part that leaks or double-frees if restated: a block belongs to
/// us until `SetClipboardData` **succeeds** for it, and to the system
/// forever after.
///
/// Several formats exist as one call rather than several because
/// `SetClipboardData` only works inside the same open that called
/// `EmptyClipboard` — a second open cannot add to what the first
/// installed. Production installs one format at a time; the shape is what
/// makes a mixed clipboard testable, and what a future paste-compatibility
/// change would build on.
fn install_formats(items: &[(u32, &[u8])]) -> Result<(), ClipboardError> {
    // Allocate and fill every block BEFORE taking the clipboard. None of
    // this needs the lock, and for a multi-megabyte item the copy is long
    // enough that holding it here made other applications' clipboard calls
    // fail outright (found in the two-machine soak).
    let mut blocks: Vec<(u32, HGLOBAL)> = Vec::with_capacity(items.len());
    for (format, bytes) in items {
        match alloc_block(bytes) {
            Ok(hglobal) => blocks.push((*format, hglobal)),
            Err(error) => {
                free_blocks(&blocks);
                return Err(error);
            }
        }
    }

    // The critical section starts here and holds only the clipboard calls.
    let _open = match OpenGuard::open() {
        Ok(guard) => guard,
        Err(error) => {
            free_blocks(&blocks);
            return Err(error);
        }
    };

    // SAFETY: the clipboard is open (guard).
    if let Err(e) = unsafe { EmptyClipboard() } {
        free_blocks(&blocks); // ownership never transferred
        return Err(ClipboardError::Busy {
            reason: format!("EmptyClipboard failed (ownership churn?): {e}"),
        });
    }

    for (index, (format, hglobal)) in blocks.iter().enumerate() {
        // SAFETY: the clipboard is open (guard). On success the system
        // takes ownership of the block and we must never free it; on
        // failure ownership stays with us, which the error arm handles.
        if let Err(e) = unsafe { SetClipboardData(*format, Some(HANDLE(hglobal.0))) } {
            // Everything before `index` now belongs to the system; only
            // the blocks from here on are still ours to free.
            free_blocks(&blocks[index..]);
            return Err(ClipboardError::Busy {
                reason: format!("SetClipboardData failed (ownership churn?): {e}"),
            });
        }
    }
    Ok(())
}

/// Allocate a movable global block holding a copy of `bytes`, ready to be
/// handed to the clipboard.
fn alloc_block(bytes: &[u8]) -> Result<HGLOBAL, ClipboardError> {
    if bytes.is_empty() {
        // GlobalAlloc(0) yields a block that cannot be locked, so this
        // would surface later as a confusing lock failure. The layers
        // above already reject empty items; say so plainly here.
        return Err(ClipboardError::Unavailable {
            reason: "refusing to install an empty clipboard block".to_owned(),
        });
    }
    // GMEM_ZEROINIT is not decoration. `GlobalAlloc` may return a block
    // larger than the requested size, `GlobalSize` reports that larger
    // size, and the read path copies all of it — so without zeroing,
    // reading back a block Crossover itself installed would copy
    // uninitialized bytes through a `&[u8]`, which the abstract machine
    // calls undefined even though canonicalization then discards them.
    // Zeroing costs nothing measurable and makes any slack deterministic,
    // which the round-trip stability the loop guard rests on can only
    // benefit from.
    // SAFETY: allocating a zeroed movable global block for the clipboard.
    let hglobal =
        unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }.map_err(|e| {
            ClipboardError::Unavailable {
                reason: format!("GlobalAlloc failed: {e}"),
            }
        })?;
    // SAFETY: `hglobal` is ours and unlocked; lock it, copy exactly the
    // `bytes.len()` bytes it was allocated for, unlock. On failure before
    // the system takes ownership we free it rather than leak.
    unsafe {
        let ptr = GlobalLock(hglobal).cast::<u8>();
        if ptr.is_null() {
            let _ = GlobalFree(Some(hglobal));
            return Err(ClipboardError::Unavailable {
                reason: "GlobalLock on fresh allocation failed".to_owned(),
            });
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(hglobal);
    }
    Ok(hglobal)
}

/// Free blocks the system never took ownership of.
fn free_blocks(blocks: &[(u32, HGLOBAL)]) {
    for (_, hglobal) in blocks {
        // SAFETY: each block came from GlobalAlloc here and has not been
        // accepted by SetClipboardData, so it is still ours to free.
        unsafe {
            let _ = GlobalFree(Some(*hglobal));
        }
    }
}

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
fn canonical_dib(mut blob: Vec<u8>) -> Vec<u8> {
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
fn dib_logical_len(blob: &[u8]) -> Option<usize> {
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

/// RAII for `OpenClipboard`/`CloseClipboard`. Open failure is `Busy`:
/// another process holding the clipboard is routine contention (R-5).
struct OpenGuard;

impl OpenGuard {
    fn open() -> Result<Self, ClipboardError> {
        // SAFETY: opening with no owning window associates the open with
        // the calling thread; EmptyClipboard then assigns no owner, which
        // is correct for immediate (non-delayed) rendering.
        unsafe { OpenClipboard(None) }.map_err(|e| ClipboardError::Busy {
            reason: format!("OpenClipboard failed (clipboard held elsewhere?): {e}"),
        })?;
        Ok(Self)
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful OpenClipboard in `open`.
        let _ = unsafe { CloseClipboard() };
    }
}

/// The observation thread: create a message-only window, register the
/// clipboard listener, pump messages, invoke the callback per
/// `WM_CLIPBOARDUPDATE`.
fn pump_thread(listener: &SharedListener, init: &std::sync::mpsc::Sender<Result<isize, String>>) {
    // SAFETY: creating a message-only window from the prebuilt STATIC
    // class (no custom window procedure needed; we read our messages from
    // the queue directly).
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("crossover-clipboard-observer"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            let _ = init.send(Err(format!("creating observer window: {e}")));
            return;
        }
    };

    // SAFETY: `hwnd` is a live window owned by this thread.
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        // SAFETY: destroying the window this thread created.
        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = init.send(Err(format!("registering clipboard listener: {e}")));
        return;
    }
    let _ = init.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    loop {
        // SAFETY: standard message pump for this thread's queue.
        let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if result.0 <= 0 {
            break; // WM_QUIT or an error: stop observing
        }
        match msg.message {
            WM_CLIPBOARDUPDATE => {
                if let Some(callback) = listener
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    // Contract: quick and non-blocking; the driver's
                    // bridge is a try_send.
                    callback();
                }
            }
            WM_APP_SHUTDOWN => break,
            _ => {
                // SAFETY: standard message dispatch.
                unsafe {
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
        }
    }

    // SAFETY: tearing down what this thread set up, in reverse order.
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = DestroyWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use crossover_platform::{ClipboardError, ClipboardProvider};

    use super::WindowsClipboard;

    /// Bounded retry over `Busy`, mirroring what the engine does in
    /// production (FR-3.4). These tests drive the real machine clipboard,
    /// which any application on a live desktop may hold momentarily;
    /// treating that as failure would make the suite flaky about the one
    /// condition the design explicitly expects.
    fn with_retry<T>(
        mut op: impl FnMut() -> Result<T, ClipboardError>,
    ) -> Result<T, ClipboardError> {
        let mut last = None;
        for _ in 0..20 {
            match op() {
                Ok(value) => return Ok(value),
                Err(ClipboardError::Busy { reason }) => {
                    last = Some(ClipboardError::Busy { reason });
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fatal) => return Err(fatal),
            }
        }
        Err(last.unwrap_or(ClipboardError::Busy {
            reason: "clipboard stayed busy".to_owned(),
        }))
    }

    /// JPEG has no Windows clipboard convention, and ADR 0014 forbids
    /// transcoding it into one that does. The refusal must be
    /// `Unsupported`, not `Busy` and not `Unavailable`: retrying will
    /// never make it work, and the origin is owed "this type never
    /// installs here" rather than "try again later" (NFR-3). Touches no
    /// clipboard lock at all — the refusal happens before any Win32 call —
    /// so this case is immune to the contention the others live with.
    #[test]
    fn jpeg_images_are_refused_permanently_rather_than_transcoded() {
        use crossover_platform::{ClipboardContent, ClipboardImageFormat};

        let clipboard = WindowsClipboard::new().unwrap();
        let refusal = clipboard.write(&ClipboardContent::Image {
            format: ClipboardImageFormat::Jpeg,
            bytes: vec![0u8; 64],
        });
        match refusal {
            Err(ClipboardError::Unsupported { reason }) => {
                assert!(
                    reason.contains("ADR 0014"),
                    "the diagnostic must name why: {reason}"
                );
            }
            other => panic!("expected a permanent refusal, got {other:?}"),
        }
    }

    /// The Windows clipboard is machine-global: serialize every test that
    /// touches it, across this whole test binary.
    fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn write_then_read_round_trips_unicode() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let text = "crossover test: héllo 👋 line\r\nbreak";
        with_retry(|| clipboard.write_text(text)).unwrap();
        assert_eq!(
            with_retry(|| clipboard.read_text()).unwrap().as_deref(),
            Some(text)
        );
    }

    #[test]
    fn own_writes_trigger_the_change_listener() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let notifications = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&notifications);
        clipboard
            .set_change_listener(Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap();

        with_retry(|| clipboard.write_text("notify me")).unwrap();

        // WM_CLIPBOARDUPDATE arrives asynchronously on the pump thread.
        let deadline = Instant::now() + Duration::from_secs(5);
        while notifications.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "no change notification within 5s of our own write"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// R-5, the real thing: another thread holds the clipboard open, so
    /// our operations must report retryable `Busy` — not `Unavailable`,
    /// which the engine would never retry.
    #[test]
    fn contention_from_another_holder_reports_busy() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();
        with_retry(|| clipboard.write_text("before contention")).unwrap();

        // A separate thread opens the clipboard and sits on it; Win32
        // clipboard ownership is per-thread, so this genuinely locks us
        // out the way another application would.
        let (holding_tx, holding_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            // The desktop may itself be holding the clipboard; retry as
            // the engine would before giving up on the simulation.
            let mut acquired = false;
            for _ in 0..20 {
                // SAFETY: opening with no owner window associates the
                // open with this thread; closed before the thread exits.
                if unsafe { windows::Win32::System::DataExchange::OpenClipboard(None) }.is_ok() {
                    acquired = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            holding_tx.send(acquired).ok();
            if !acquired {
                return;
            }
            let _ = release_rx.recv();
            // SAFETY: balances the successful open above.
            unsafe {
                let _ = windows::Win32::System::DataExchange::CloseClipboard();
            }
        });

        let held = holding_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("holder thread did not report");
        if !held {
            // The machine would not let us stage contention. Skip rather
            // than fail: this test cannot control a live desktop, and a
            // red build here would say nothing about Crossover.
            release_tx.send(()).ok();
            holder.join().unwrap();
            eprintln!("skipped: could not acquire the clipboard to stage contention");
            return;
        }

        // Classification is what matters: whatever the outcome, a
        // contention failure must be Busy (retryable), never Unavailable
        // (which the engine would never retry).
        //
        // Note the honest limitation: Windows may admit another thread of
        // the *same process* even while this one holds the clipboard, so
        // an in-process holder cannot guarantee lockout. When it does let
        // us through, this exercises the success path instead — the
        // assertion below still fails the build if a contention failure
        // is ever misclassified.
        for outcome in [
            clipboard.write_text("during contention").err(),
            clipboard.read_text().err(),
        ] {
            match outcome {
                None | Some(ClipboardError::Busy { .. }) => {}
                Some(other) => {
                    panic!("contention must classify as Busy, got {other:?}")
                }
            }
        }

        release_tx.send(()).ok();
        holder.join().unwrap();

        // Recovery: once released, normal operation resumes.
        with_retry(|| clipboard.write_text("after contention")).unwrap();
        assert_eq!(
            with_retry(|| clipboard.read_text()).unwrap().as_deref(),
            Some("after contention")
        );
    }

    /// Rapid replacement (FR-6.1): a burst of writes must leave the last
    /// one installed, with no crash, deadlock, or leaked clipboard lock.
    #[test]
    fn rapid_replacement_settles_on_the_last_write() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let mut installed = 0;
        for i in 0..50 {
            // Contention from the desktop can legitimately bounce a write;
            // Busy is acceptable, Unavailable is not.
            match clipboard.write_text(&format!("burst item {i}")) {
                Ok(()) => installed += 1,
                Err(ClipboardError::Busy { .. }) => {}
                Err(other) => panic!("unexpected write failure: {other}"),
            }
        }
        assert!(installed > 0, "no write in the burst succeeded");

        // Settle, then confirm we can still read a coherent value.
        std::thread::sleep(Duration::from_millis(100));
        let final_text = with_retry(|| clipboard.read_text()).unwrap();
        assert!(
            final_text.is_some_and(|t| t.starts_with("burst item")),
            "clipboard does not hold a burst item after rapid replacement"
        );
    }

    /// The protocol's maximum item (4 MiB) survives the Win32 boundary —
    /// the allocation, UTF-16 conversion, and read-back path at scale.
    #[test]
    fn maximum_sized_item_round_trips() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let large = "L".repeat(4 * 1024 * 1024);
        match clipboard.write_text(&large) {
            Ok(()) => {}
            Err(ClipboardError::Busy { .. }) => return, // desktop contention
            Err(other) => panic!("unexpected failure writing 4 MiB: {other}"),
        }
        let read_back = with_retry(|| clipboard.read_text()).unwrap();
        assert_eq!(read_back.as_deref().map(str::len), Some(large.len()));
    }

    #[test]
    fn replacing_content_keeps_working_across_instances() {
        let _serial = clipboard_lock();
        {
            let clipboard = WindowsClipboard::new().unwrap();
            with_retry(|| clipboard.write_text("first instance")).unwrap();
        } // drops: pump thread must shut down cleanly

        let clipboard = WindowsClipboard::new().unwrap();
        assert_eq!(
            with_retry(|| clipboard.read_text()).unwrap().as_deref(),
            Some("first instance")
        );
        with_retry(|| clipboard.write_text("second instance")).unwrap();
        assert_eq!(
            with_retry(|| clipboard.read_text()).unwrap().as_deref(),
            Some("second instance")
        );
    }

    // ---- images (ADR 0014 platform slice) --------------------------------

    /// A minimal, well-formed 32-bpp `BI_RGB` DIB: a 40-byte
    /// `BITMAPINFOHEADER`, no colour table, `width * height * 4` pixel
    /// bytes with a recognizable pattern. Bottom-up (positive height),
    /// which is what a Windows screen capture produces.
    fn dib(width: i32, height: i32) -> Vec<u8> {
        let pixel_bytes = usize::try_from(width * height * 4).expect("test dimensions");
        let mut blob = Vec::with_capacity(40 + pixel_bytes);
        blob.extend_from_slice(&40u32.to_le_bytes()); // biSize
        blob.extend_from_slice(&width.to_le_bytes()); // biWidth
        blob.extend_from_slice(&height.to_le_bytes()); // biHeight
        blob.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        blob.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        blob.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
        blob.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage (0 is legal)
        blob.extend_from_slice(&2835i32.to_le_bytes()); // biXPelsPerMeter
        blob.extend_from_slice(&2835i32.to_le_bytes()); // biYPelsPerMeter
        blob.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
        blob.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        blob.extend((0..pixel_bytes).map(|i| u8::try_from(i % 251).unwrap_or(0)));
        blob
    }

    /// FR-3.2 for images: what comes back off the clipboard is what went
    /// on, byte for byte. Verbatim transfer is the entire ADR 0014 image
    /// story, and it starts here — a backend that re-encoded on the way in
    /// or out would break it before the wire ever saw the bytes.
    #[test]
    fn an_image_round_trips_through_the_real_clipboard_verbatim() {
        use crossover_platform::{ClipboardContent, ClipboardImageFormat};

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let image = ClipboardContent::Image {
            format: ClipboardImageFormat::Dib,
            bytes: dib(16, 16),
        };
        with_retry(|| clipboard.write(&image)).unwrap();
        let read_back = with_retry(|| clipboard.read()).unwrap();
        assert!(
            read_back.as_ref() == Some(&image),
            "the image did not survive the clipboard verbatim (read back {:?} bytes)",
            read_back
                .as_ref()
                .map(crossover_platform::ClipboardContent::byte_len)
        );
    }

    /// **A clipboard sync loop is release-blocking.** Loop prevention
    /// (FR-3.3) works by content hash: the engine remembers what it
    /// applied and suppresses the notification its own write provokes.
    /// That only holds if reading back an installed image yields the
    /// *identical* bytes — one pad byte of difference and the hash misses,
    /// the read looks like fresh local content, and it is offered straight
    /// back to the peer that sent it.
    ///
    /// So this test pins the exact property the suppression depends on:
    /// our own write notifies us, and the read that follows is
    /// byte-identical and stable across repeats.
    #[test]
    fn an_installed_image_reads_back_identical_so_own_writes_cannot_loop() {
        use crossover_platform::{ClipboardContent, ClipboardImageFormat};

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let notifications = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&notifications);
        clipboard
            .set_change_listener(Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap();

        let bytes = dib(24, 18);
        let image = ClipboardContent::Image {
            format: ClipboardImageFormat::Dib,
            bytes: bytes.clone(),
        };
        with_retry(|| clipboard.write(&image)).unwrap();

        // The write does provoke a notification — the contract term
        // `ClipboardProvider` documents, and the reason suppression is
        // needed at all.
        let deadline = Instant::now() + Duration::from_secs(5);
        while notifications.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "no change notification within 5s of our own image write"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Twice: the round trip must be a fixed point, not merely equal
        // once. An unstable length would loop on the second hop instead
        // of the first.
        for attempt in 1..=2 {
            match with_retry(|| clipboard.read()).unwrap() {
                Some(ClipboardContent::Image {
                    format: ClipboardImageFormat::Dib,
                    bytes: read_back,
                }) => assert!(
                    read_back == bytes,
                    "read {} back {} bytes, wrote {} — the content hash would miss \
                     and the item would be offered back to its own origin",
                    attempt,
                    read_back.len(),
                    bytes.len()
                ),
                other => panic!(
                    "an installed image must read back as an image, got {:?}",
                    other.map(|c| c.byte_len())
                ),
            }
        }
    }

    /// Mixed content, the Excel/Word/browser case: the clipboard holds
    /// both `CF_UNICODETEXT` and `CF_DIB`, and exactly one type may
    /// travel. Text wins — the image in a mixed item is a rendering of the
    /// text, and text pastes into strictly more places (module docs). The
    /// image being genuinely present is asserted too, so this proves a
    /// *choice* rather than an absence.
    #[test]
    fn text_wins_when_the_clipboard_holds_both_text_and_an_image() {
        use crossover_platform::ClipboardContent;

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let text = "cells copied from a spreadsheet";
        let mut utf16 = Vec::new();
        for unit in text.encode_utf16().chain(std::iter::once(0)) {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let picture = dib(12, 9);
        with_retry(|| {
            super::install_formats(&[
                (
                    u32::from(windows::Win32::System::Ole::CF_UNICODETEXT.0),
                    &utf16,
                ),
                (u32::from(windows::Win32::System::Ole::CF_DIB.0), &picture),
            ])
        })
        .unwrap();

        match with_retry(|| clipboard.read()).unwrap() {
            Some(ClipboardContent::Text(read_back)) => assert_eq!(read_back, text),
            other => panic!(
                "mixed content must read as text, got {:?}",
                other.map(|c| c.byte_len())
            ),
        }
        // The image really was there: precedence, not absence.
        assert_eq!(
            with_retry(|| super::read_dib(super::MAX_CLIPBOARD_IMAGE_BYTES))
                .unwrap()
                .map(|b| b.len()),
            Some(picture.len()),
            "the mixed clipboard was supposed to hold an image as well"
        );
    }

    /// Precedence must not become suppression. A `CF_UNICODETEXT` that is
    /// nothing but its terminator reads as `Some("")`, and letting that
    /// win over a picture beside it would propagate an empty string —
    /// **blanking the peer's clipboard** instead of sending the image,
    /// which is worse than either content type. The carve-out is narrow:
    /// only an image displaces empty text, and an empty clipboard with no
    /// picture behind it must read exactly as it always has.
    #[test]
    fn empty_text_steps_aside_for_an_image_but_nothing_else() {
        use crossover_platform::ClipboardContent;

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let terminator_only = 0u16.to_le_bytes(); // an empty CF_UNICODETEXT
        let picture = dib(10, 6);
        with_retry(|| {
            super::install_formats(&[
                (
                    u32::from(windows::Win32::System::Ole::CF_UNICODETEXT.0),
                    &terminator_only,
                ),
                (u32::from(windows::Win32::System::Ole::CF_DIB.0), &picture),
            ])
        })
        .unwrap();

        match with_retry(|| clipboard.read()).unwrap() {
            Some(ClipboardContent::Image { bytes, .. }) => assert_eq!(bytes, picture),
            other => panic!(
                "empty text must not mask an image, got {:?}",
                other.map(|c| c.byte_len())
            ),
        }

        // No image behind it: unchanged behaviour, empty text reads as
        // empty text rather than becoming absent.
        with_retry(|| {
            super::install_formats(&[(
                u32::from(windows::Win32::System::Ole::CF_UNICODETEXT.0),
                &terminator_only,
            )])
        })
        .unwrap();
        assert_eq!(
            with_retry(|| clipboard.read()).unwrap(),
            Some(ClipboardContent::Text(String::new()))
        );
    }

    /// FR-3.6 at the source. An image past the ceiling is refused where it
    /// is cheapest to refuse — from `GlobalSize`, before the block is
    /// locked or a byte copied — and reported as *absent*, never
    /// truncated. The ceiling is a parameter so the refusal is provable
    /// without putting a 64 MiB item on a live desktop's clipboard.
    #[test]
    fn an_image_over_the_ceiling_reads_as_absent_rather_than_truncated() {
        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let picture = dib(16, 16);
        with_retry(|| {
            super::install_formats(&[(u32::from(windows::Win32::System::Ole::CF_DIB.0), &picture)])
        })
        .unwrap();

        // A ceiling below the item: absent, and specifically not an error
        // and not a short read.
        assert_eq!(
            with_retry(|| super::read_dib(picture.len() - 1)).unwrap(),
            None,
            "an oversized image must read as absent"
        );
        // The same item under the real ceiling: present and whole.
        assert_eq!(
            with_retry(|| super::read_dib(super::MAX_CLIPBOARD_IMAGE_BYTES))
                .unwrap()
                .map(|b| b.len()),
            Some(picture.len())
        );
        // And the trait-level read agrees with the ceiling it applies.
        assert!(with_retry(|| clipboard.read()).unwrap().is_some());
    }

    /// PNG installs verbatim under the registered `"PNG"` format —
    /// nothing is transcoded (ADR 0014). The documented limitation is
    /// asserted rather than left implicit: Windows synthesizes no `CF_DIB`
    /// from it, so `read` (which prefers `CF_DIB`) sees nothing, and a
    /// `CF_DIB`-only application would see an empty clipboard.
    #[test]
    fn png_installs_under_the_registered_png_format_and_synthesizes_nothing() {
        use crossover_platform::{ClipboardContent, ClipboardImageFormat};
        use windows::Win32::System::DataExchange::{
            IsClipboardFormatAvailable, RegisterClipboardFormatW,
        };
        use windows::core::w;

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        // Not a real PNG, and deliberately so: this backend never parses
        // image bytes, so any non-empty blob exercises the same path.
        let bytes = b"\x89PNG\r\n\x1a\n not really a png, verbatim regardless".to_vec();
        with_retry(|| {
            clipboard.write(&ClipboardContent::Image {
                format: ClipboardImageFormat::Png,
                bytes: bytes.clone(),
            })
        })
        .unwrap();

        // SAFETY: registering a name is idempotent and returns the
        // existing id; the availability probe takes no clipboard lock.
        let (png_format, available) = unsafe {
            let id = RegisterClipboardFormatW(w!("PNG"));
            (id, IsClipboardFormatAvailable(id).is_ok())
        };
        assert_ne!(png_format, 0, "the PNG clipboard format did not register");
        assert!(available, "PNG bytes were not installed under \"PNG\"");

        // The limitation, pinned so it cannot be forgotten: no CF_DIB is
        // synthesized from a registered PNG, so this build's own reader
        // reports the clipboard as holding nothing it represents.
        assert_eq!(
            with_retry(|| super::read_dib(super::MAX_CLIPBOARD_IMAGE_BYTES)).unwrap(),
            None,
            "Windows unexpectedly synthesized CF_DIB from PNG; \
             the write path's documented caveat needs revisiting"
        );
    }

    // ---- DIB length canonicalization (pure; no clipboard involved) --------

    #[test]
    fn the_canonical_length_of_a_well_formed_dib_is_its_whole_blob() {
        let blob = dib(8, 4);
        assert_eq!(super::dib_logical_len(&blob), Some(blob.len()));
        assert_eq!(super::canonical_dib(blob.clone()), blob);
    }

    /// The property the no-loop test depends on, isolated: allocator slack
    /// past the pixels is dropped, and dropping it is *idempotent*, so a
    /// blob that has crossed the clipboard once does not change again.
    #[test]
    fn canonicalization_drops_allocator_slack_and_is_a_fixed_point() {
        let exact = dib(8, 4);
        let mut padded = exact.clone();
        padded.extend_from_slice(&[0xAB; 13]); // what GlobalSize may report

        let trimmed = super::canonical_dib(padded);
        assert_eq!(trimmed, exact, "slack past the pixels must not travel");
        assert_eq!(
            super::canonical_dib(trimmed.clone()),
            trimmed,
            "canonicalization must be a fixed point or the round trip drifts"
        );
    }

    /// Palette and bit-field layouts, where the bytes between header and
    /// pixels are not zero. Getting these wrong in the *truncating*
    /// direction would corrupt an image, so each is pinned.
    #[test]
    fn the_canonical_length_covers_palette_and_bitfield_layouts() {
        // 8 bpp, implicit 256-entry palette, 4-byte-aligned rows.
        let mut paletted = Vec::new();
        paletted.extend_from_slice(&40u32.to_le_bytes());
        paletted.extend_from_slice(&5i32.to_le_bytes()); // width 5 → stride 8
        paletted.extend_from_slice(&3i32.to_le_bytes());
        paletted.extend_from_slice(&1u16.to_le_bytes());
        paletted.extend_from_slice(&8u16.to_le_bytes());
        paletted.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        paletted.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
        paletted.extend_from_slice(&[0u8; 8]); // pels-per-meter
        paletted.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed = 0 → 256
        paletted.extend_from_slice(&0u32.to_le_bytes());
        let expected = 40 + 256 * 4 + 8 * 3;
        paletted.resize(expected + 7, 0); // + slack
        assert_eq!(super::dib_logical_len(&paletted), Some(expected));

        // 16 bpp BI_BITFIELDS: three DWORD masks sit before the pixels.
        let mut masked = Vec::new();
        masked.extend_from_slice(&40u32.to_le_bytes());
        masked.extend_from_slice(&4i32.to_le_bytes());
        masked.extend_from_slice(&(-2i32).to_le_bytes()); // top-down
        masked.extend_from_slice(&1u16.to_le_bytes());
        masked.extend_from_slice(&16u16.to_le_bytes());
        masked.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        masked.extend_from_slice(&0u32.to_le_bytes());
        masked.extend_from_slice(&[0u8; 8]);
        masked.extend_from_slice(&0u32.to_le_bytes());
        masked.extend_from_slice(&0u32.to_le_bytes());
        let expected = 40 + 12 + 8 * 2; // masks + stride(4×16bpp = 8) × 2 rows
        masked.resize(expected + 3, 0);
        assert_eq!(super::dib_logical_len(&masked), Some(expected));
    }

    /// Conservative in the only direction that matters: anything this code
    /// cannot model confidently keeps the blob whole. Over-including a few
    /// bytes is harmless; cutting a valid image short is not.
    #[test]
    fn anything_unmodelled_keeps_the_blob_whole() {
        // A V5 header (CF_DIBV5 shape) — not what CF_DIB hands back.
        let mut v5 = dib(4, 4);
        v5[0..4].copy_from_slice(&124u32.to_le_bytes());
        assert_eq!(super::dib_logical_len(&v5), None);
        assert_eq!(super::canonical_dib(v5.clone()), v5);

        // Too short to hold a header at all.
        assert_eq!(super::dib_logical_len(&[0u8; 12]), None);
        assert_eq!(super::dib_logical_len(&[]), None);

        // Dimensions that claim far more than the blob holds.
        let mut liar = dib(4, 4);
        liar[4..8].copy_from_slice(&40_000i32.to_le_bytes());
        assert_eq!(super::dib_logical_len(&liar), None);
        assert_eq!(super::canonical_dib(liar.clone()), liar);

        // A compressed encoding with no declared size cannot be measured.
        let mut rle = dib(4, 4);
        rle[16..20].copy_from_slice(&1u32.to_le_bytes()); // BI_RLE8
        rle[20..24].copy_from_slice(&0u32.to_le_bytes()); // biSizeImage = 0
        assert_eq!(super::dib_logical_len(&rle), None);
    }

    /// NFR-1: header bytes are network-influenced (a peer's image is
    /// installed, read back, and canonicalized), so malformed input must
    /// never panic and must never *grow* a blob.
    #[test]
    fn canonicalization_never_panics_on_arbitrary_bytes() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for len in 0..200usize {
            let blob: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    u8::try_from(state & 0xFF).unwrap_or(0)
                })
                .collect();
            let out = super::canonical_dib(blob.clone());
            assert!(out.len() <= blob.len(), "canonicalization grew a blob");
            assert_eq!(out.as_slice(), &blob[..out.len()], "it must stay a prefix");
        }
    }

    // ---- manual hardware validation (ADR 0014, docs/TESTING.md) ----------

    /// **Manual.** Copy a real screenshot before running: press
    /// `Win+Shift+S`, snip any region, then
    /// `cargo test -p crossover-platform-windows -- --ignored
    /// manual_a_real_snip`.
    ///
    /// Automated tests can only fabricate a DIB; this asserts that what
    /// the Snipping Tool actually publishes is read as an image, is inside
    /// the ceiling, and canonicalizes to a stable length (two consecutive
    /// reads agree). It is the source-side half of the owner's
    /// hardware-validation checklist.
    #[test]
    #[ignore = "manual: requires a real screenshot on the clipboard (Win+Shift+S)"]
    fn manual_a_real_snip_is_read_as_a_stable_image() {
        use crossover_platform::{ClipboardContent, ClipboardImageFormat};

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let first = with_retry(|| clipboard.read()).unwrap();
        let Some(ClipboardContent::Image {
            format: ClipboardImageFormat::Dib,
            bytes,
        }) = first
        else {
            panic!("no image on the clipboard: take a snip with Win+Shift+S first");
        };
        assert!(bytes.len() <= super::MAX_CLIPBOARD_IMAGE_BYTES);
        eprintln!("snip read as {} bytes of CF_DIB", bytes.len());

        let again = with_retry(|| clipboard.read()).unwrap();
        assert!(
            again
                == Some(ClipboardContent::Image {
                    format: ClipboardImageFormat::Dib,
                    bytes,
                }),
            "consecutive reads of the same snip disagreed"
        );
    }

    /// **Manual.** Run it, then paste (`Ctrl+V`) into Paint, Word, and a
    /// browser compose box, and confirm the gradient appears in each.
    ///
    /// It installs a recognizable 320×200 image and leaves it on the
    /// clipboard. Automation can prove the bytes round-trip through Win32;
    /// only a human can confirm that third-party applications accept what
    /// this backend installs, which is the destination-side half of the
    /// owner's checklist.
    #[test]
    #[ignore = "manual: leaves an image on the clipboard for a human to paste"]
    fn manual_an_installed_image_pastes_into_other_applications() {
        use crossover_platform::{ClipboardContent, ClipboardImageFormat};

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();

        let (width, height) = (320i32, 200i32);
        let mut bytes = dib(width, height);
        for y in 0..height {
            for x in 0..width {
                let at = 40 + usize::try_from(y * width + x).expect("in range") * 4;
                bytes[at] = u8::try_from(x * 255 / width).unwrap_or(0); // blue
                bytes[at + 1] = u8::try_from(y * 255 / height).unwrap_or(0); // green
                bytes[at + 2] = 0x40; // red
                bytes[at + 3] = 0xFF; // alpha, ignored by BI_RGB
            }
        }
        with_retry(|| {
            clipboard.write(&ClipboardContent::Image {
                format: ClipboardImageFormat::Dib,
                bytes: bytes.clone(),
            })
        })
        .unwrap();
        eprintln!("a 320x200 blue/green gradient is on the clipboard; paste it now");
    }
}
