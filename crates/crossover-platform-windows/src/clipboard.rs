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
//!
//! # Files (ADR 0015's sender-side observation, feature/133)
//!
//! **`CF_HDROP`, read as a list of local paths.** A file/folder selection
//! copied in Explorer publishes `CF_HDROP`: a shell structure naming the
//! selected paths on *this* machine, read through `DragQueryFileW` rather
//! than `GlobalLock`ed and copied like the other two formats. Bounded to
//! [`MAX_CLIPBOARD_FILE_ENTRIES`] entries, checked from the structure's own
//! count before a single path is read, so an oversized selection costs
//! nothing to refuse (NFR-1). This is an *observation* only — what the
//! engine does with it (walking the selection, building an archive,
//! offering it to a peer) is `crossover-core`'s job, staged for feature/135
//! and deliberately a no-op until then.
//!
//! **This build's own virtual file list never round-trips through here.**
//! What Crossover places on the clipboard for a *received* file
//! ([`crate::virtual_file`]) advertises `CFSTR_FILEDESCRIPTORW` and
//! `CFSTR_FILECONTENTS`, never `CF_HDROP` — so a probe here finds nothing
//! to read while our own object is current, which is ADR 0015's loop
//! prevention layer 2 ("no `CF_HDROP`, no send") holding structurally,
//! independent of the ownership check `crossover_core::clipboard_driver`
//! performs before a read is ever attempted (layer 1). A virtual file list
//! from another application — Outlook's attachment promise is the usual
//! example — is the same story for the same reason: it has no `CF_HDROP`
//! representation either, so it was already invisible to this probe before
//! this feature existed, and stays that way.

use std::sync::{Arc, Mutex, PoisonError};

use std::path::{Path, PathBuf};

use crossover_platform::{
    ClipboardContent, ClipboardError, ClipboardImageFormat, ClipboardListener, ClipboardProvider,
    MAX_CLIPBOARD_FILE_ENTRIES, MAX_CLIPBOARD_IMAGE_BYTES,
};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetOpenClipboardWindow, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetClassNameW, GetMessageW,
    GetWindowThreadProcessId, HWND_MESSAGE, MSG, PostMessageW, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE,
};
use windows::core::{PWSTR, w};

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
    /// Signalled by the pump as it leaves its loop, so `Drop` can wait a
    /// bounded time for it rather than joining a thread that may never
    /// return (see [`crate::pump`]). Behind a `Mutex` so the provider stays
    /// `Sync` without widening the assertions below; it is read once, from
    /// `Drop`.
    stopped: Mutex<std::sync::mpsc::Receiver<()>>,
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
        // Signalled as the pump leaves its loop, so shutdown can tell
        // "stopped" from "wedged" without joining blind (see `pump`).
        let (stopped_tx, stopped) = std::sync::mpsc::channel::<()>();

        let pump = std::thread::Builder::new()
            .name("crossover-clipboard-pump".to_owned())
            .spawn(move || {
                pump_thread(&pump_listener, &init_tx);
                let _ = stopped_tx.send(());
            })
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
            stopped: Mutex::new(stopped),
        })
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        let hwnd = HWND(self.hwnd_raw as *mut core::ffi::c_void);
        // SAFETY: PostMessageW is safe to call from any thread with any
        // window handle; a stale handle at worst fails harmlessly.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0)) };
        let stopped = self
            .stopped
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        crate::pump::stop("clipboard", stopped, &mut self.pump);
    }
}

impl ClipboardProvider for WindowsClipboard {
    /// Reads `CF_UNICODETEXT`, else `CF_HDROP`, else `CF_DIB` (ADR 0014,
    /// ADR 0015).
    ///
    /// Text first, deliberately: a clipboard holding both is holding a
    /// rendering of its own text, and the transaction carries one type.
    /// The reasoning is on the module. A file/folder selection is checked
    /// next, ahead of the image: like text, it is exactly what the user
    /// selected rather than a rendering of something else, and in
    /// practice the two never coexist — Explorer's file copy publishes no
    /// `CF_UNICODETEXT` or `CF_DIB` alongside its `CF_HDROP`.
    ///
    /// **Non-empty** text, precisely. A source may publish a zero-length
    /// `CF_UNICODETEXT` beside a picture, and letting that win would
    /// propagate `""` — blanking the peer's clipboard instead of sending
    /// the image, which is worse than either content type. So an empty
    /// text representation steps aside for a file list or an image, and
    /// only for those: an empty clipboard with neither behind it still
    /// reads exactly as it always has.
    ///
    /// An image past [`MAX_CLIPBOARD_IMAGE_BYTES`], or a selection past
    /// [`MAX_CLIPBOARD_FILE_ENTRIES`], reads as *absent* — the trait's
    /// meaning for "nothing this backend represents" — refused before its
    /// bytes (or its paths) are copied, never truncated (FR-3.6).
    ///
    /// A virtual file list this process itself placed (ADR 0015) never
    /// reaches here as `CF_HDROP` at all: the object we offer serves
    /// `CFSTR_FILEDESCRIPTORW`/`CFSTR_FILECONTENTS`, not `CF_HDROP`, so
    /// there is no format for this probe to find (ADR 0015's loop
    /// prevention, layer 2 — see `crossover_core::clipboard_driver`, whose
    /// layer 1 ownership check already stops a change notification from
    /// reaching a read at all while our object is current).
    ///
    /// All three probes happen inside one open, so the precedence above is
    /// decided from a single clipboard state ([`read_current`]).
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        read_current(MAX_CLIPBOARD_IMAGE_BYTES, MAX_CLIPBOARD_FILE_ENTRIES)
    }

    /// Writes `CF_UNICODETEXT`, `CF_DIB`, or the registered `"PNG"`
    /// format — each verbatim, none transcoded into another (ADR 0014).
    ///
    /// `Jpeg` is refused as [`ClipboardError::Unsupported`]: permanent, so
    /// the engine does not retry it, and distinguishable by the origin
    /// from a clipboard that is merely busy or broken (FR-3.2, NFR-3). An
    /// image past [`MAX_CLIPBOARD_IMAGE_BYTES`] is refused the same way,
    /// mirroring the ceiling the read path enforces.
    ///
    /// `FileList` is refused the same way, permanently: a file list is
    /// placed on the clipboard through [`VirtualFileClipboard`], a
    /// separate mechanism with its own apartment thread (ADR 0015), not
    /// through this trait. Nothing in this build ever constructs a
    /// `FileList` to write — it is a local *observation* the engine does
    /// not yet stage for transmission (feature/133) — so this arm is a
    /// defensive statement of the contract, not a path production takes.
    ///
    /// [`VirtualFileClipboard`]: crossover_platform::VirtualFileClipboard
    fn write(&self, content: &ClipboardContent) -> Result<(), ClipboardError> {
        match content {
            ClipboardContent::Text(text) => write_unicode_text(text),
            ClipboardContent::Image { format, bytes } => {
                write_image(*format, bytes, MAX_CLIPBOARD_IMAGE_BYTES)
            }
            ClipboardContent::FileList(_) => Err(ClipboardError::Unsupported {
                reason: "a file list is placed via VirtualFileClipboard, not \
                         ClipboardProvider::write (ADR 0015)"
                    .to_owned(),
            }),
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

/// Decide text-versus-file-list-versus-image from **one** clipboard state.
///
/// All three probes run under a single `OpenClipboard`, deliberately.
/// Opening more than once would let the clipboard change in between, and
/// the precedence rule would then be applied to a pair of states that never
/// existed together: text read as absent from the old contents, an image
/// found in the new ones, and an image synchronized while the source's
/// clipboard actually held text. That window is exactly what a user creates
/// by copying twice in quick succession, and mixed-content precedence is the
/// part of this backend a human is asked to confirm by eye
/// (docs/SOAK.md, Phase 7 hardware validation), so it should not be
/// deciding across two different clipboards.
///
/// It costs nothing in lock time for the common case: non-empty text
/// returns without ever probing `CF_HDROP` or `CF_DIB`, so only a clipboard
/// that is file-list-or-image-or-empty is examined further under the one
/// open.
fn read_current(
    max_image_bytes: usize,
    max_file_entries: u32,
) -> Result<Option<ClipboardContent>, ClipboardError> {
    let mut raw_image = None;
    let mut oversized_image = None;
    let mut file_list = None;
    let mut oversized_file_list = None;
    // Held to the probes and the copies. The UTF-16 decode, the
    // canonicalization, and the refusal logs all run below, once this
    // guard has dropped and the machine-global lock is free.
    let units = {
        let open = OpenGuard::open()?;
        match probe_unicode_text(&open)? {
            Some(units) if !units.is_empty() => Some(units),
            empty_or_absent => {
                match probe_hdrop(&open, max_file_entries)? {
                    HdropProbe::Raw(paths) => file_list = Some(paths),
                    HdropProbe::TooManyEntries { entry_count } => {
                        oversized_file_list = Some(entry_count);
                    }
                    HdropProbe::Absent => {}
                }
                // A file list, once found, wins outright: skip the image
                // probe rather than pay for it. An oversized selection is
                // *absent* for precedence purposes, exactly like an
                // oversized image below, so it still falls through here.
                if file_list.is_none() {
                    match probe_dib(&open, max_image_bytes)? {
                        DibProbe::Raw(blob) => raw_image = Some(blob),
                        DibProbe::TooLarge { byte_count } => oversized_image = Some(byte_count),
                        DibProbe::Absent => {}
                    }
                }
                empty_or_absent
            }
        }
    };

    if let Some(entry_count) = oversized_file_list {
        tracing::warn!(
            entry_count,
            max_entries = max_file_entries,
            "clipboard file selection exceeds the maximum entry count; not synchronized"
        );
    }
    if let Some(byte_count) = oversized_image {
        tracing::warn!(
            byte_count,
            max_bytes = max_image_bytes,
            "clipboard image exceeds the maximum; not synchronized"
        );
    }
    if let Some(paths) = file_list {
        return Ok(Some(ClipboardContent::FileList(paths)));
    }
    if let Some(blob) = raw_image {
        return Ok(Some(ClipboardContent::Image {
            format: ClipboardImageFormat::Dib,
            bytes: canonical_dib(blob),
        }));
    }
    // An oversized file list or image is *absent*, which leaves an empty
    // text representation beside it reading as it always has.
    Ok(units.map(|units| ClipboardContent::Text(String::from_utf16_lossy(&units))))
}

/// Probe `CF_UNICODETEXT` on the already-open clipboard, yielding its
/// UTF-16 units, or `None` when the clipboard holds no text.
///
/// Takes the open guard rather than opening: the caller decides precedence
/// across this and [`probe_dib`], and that decision is only meaningful if
/// both saw the same clipboard ([`read_current`]).
///
/// Units rather than a `String` for the same reason [`probe_dib`] returns
/// [`DibProbe::Raw`]: the decode allocates, and for a multi-megabyte item
/// that is not cheap, so the caller runs it once the clipboard is closed.
fn probe_unicode_text(_open: &OpenGuard) -> Result<Option<Vec<u16>>, ClipboardError> {
    // SAFETY: no arguments; checks format availability only.
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_err() {
        return Ok(None); // empty clipboard or no text representation
    }

    // SAFETY: the clipboard is open (caller's guard); the returned handle is
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
    // SAFETY: `hglobal` came from GetClipboardData while the clipboard is
    // open. GlobalSize reads the block's size without locking or copying
    // it; it reports 0 for an invalid or discarded block.
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 {
        // Not an empty string — a zero-byte block cannot hold even the
        // terminator an empty `CF_UNICODETEXT` is made of, so this is a
        // block that went away, not a clipboard holding "".
        return Ok(None);
    }
    // The terminator scan below is bounded by the block, not by trust.
    // `CF_UNICODETEXT` is null-terminated UTF-16 *by contract*, but the
    // producer is any application on the machine and this process cannot
    // verify that it obeyed: an unterminated block would send the scan
    // off the end of the allocation — undefined behaviour, its trigger
    // chosen by whatever else is running. `GlobalSize` is the only bound
    // available, so the scan stops there and an unterminated block reads
    // as its whole contents rather than reading past them.
    let max_units = size / 2; // whole UTF-16 units; a stray odd byte is not one

    // SAFETY: `hglobal` is a live clipboard block; GlobalLock pins it and
    // yields the base pointer.
    let ptr = unsafe { GlobalLock(hglobal) }.cast::<u16>();
    if ptr.is_null() {
        // Same churn window as above: the block can vanish with its
        // owner. Retryable.
        return Err(ClipboardError::Busy {
            reason: "GlobalLock on clipboard data failed".to_owned(),
        });
    }
    // SAFETY: `max_units` UTF-16 units starting at `ptr` are within this
    // block, as GlobalSize reported and GlobalLock pinned, so both the
    // scan and the copy stay inside it. The copy is the only work under
    // the clipboard lock — the UTF-16 → String conversion is the caller's,
    // run once the clipboard is closed, so Crossover is not the reason
    // another application's clipboard call fails.
    let units: Vec<u16> = unsafe {
        let len = terminated_len(ptr, max_units);
        std::slice::from_raw_parts(ptr, len).to_vec()
    };
    // SAFETY: balances the successful GlobalLock above. GlobalUnlock
    // reports "no longer locked" as an error-shaped success; ignore.
    let _ = unsafe { GlobalUnlock(hglobal) };

    Ok(Some(units))
}

/// Units before the null terminator, scanning at most `max_units`.
///
/// The bound is the whole point. `CF_UNICODETEXT` is null-terminated *by
/// contract*, but its producer is any application on the machine and this
/// process cannot verify it obeyed; an unterminated block would otherwise
/// send the scan off the end of the allocation — undefined behaviour whose
/// trigger is another program's bug. `GlobalSize` is the only bound
/// available, so an unterminated block reads as its whole contents rather
/// than as whatever follows it in memory.
///
/// Windows makes this hard to reach in practice — it normalizes an
/// unterminated `CF_UNICODETEXT` into a terminated block of the same byte
/// length, dropping the final character, so a block installed through the
/// clipboard comes back terminated whatever the producer wrote. That is
/// observed behaviour of one Windows version, not a documented guarantee,
/// and it says nothing about a block from a delayed-render producer, so
/// the bound stays. `the_terminator_scan_stops_at_the_bound` proves it
/// over fixtures, which is the only place the unterminated case is
/// reachable.
///
/// # Safety
///
/// `ptr` must be valid for reads of up to `max_units` `u16`s.
unsafe fn terminated_len(ptr: *const u16, max_units: usize) -> usize {
    let mut len = 0usize;
    // SAFETY: the caller guarantees `max_units` readable units, and `len`
    // never reaches past that bound before the loop stops.
    while len < max_units && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    len
}

/// What a `CF_DIB` probe found.
///
/// "Too large" is a variant rather than a log line inside the probe
/// because the probe runs with the clipboard open: `tracing` under the
/// machine-global lock can block on a subscriber's I/O, and every other
/// application's clipboard call blocks behind it. The caller reports it
/// after the guard drops.
enum DibProbe {
    /// No raster representation, or a block that came back empty.
    Absent,
    /// Present, but past the ceiling — refused whole, never truncated.
    TooLarge { byte_count: usize },
    /// Present and inside the ceiling, exactly as the block held it.
    /// Still to be canonicalized — [`canonical_dib`] is header
    /// arithmetic the caller runs once the clipboard is closed.
    Raw(Vec<u8>),
}

/// Probe `CF_DIB` on the already-open clipboard.
///
/// Takes the open guard rather than opening, so this and
/// [`read_unicode_text`] see one clipboard state — see [`read_current`].
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
fn probe_dib(_open: &OpenGuard, max_bytes: usize) -> Result<DibProbe, ClipboardError> {
    // SAFETY: no arguments; checks format availability only. Synthesized
    // formats count as available, which is exactly what makes this one
    // probe cover CF_BITMAP and CF_DIBV5 sources too.
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_DIB.0)) }.is_err() {
        return Ok(DibProbe::Absent); // empty clipboard, or no raster representation
    }

    // SAFETY: the clipboard is open (caller's guard); the returned handle stays
    // owned by the clipboard, never by us. Ownership can churn between
    // our open and this call, which surfaces as an error here and is
    // retryable contention, not a fault (R-5).
    let handle =
        unsafe { GetClipboardData(u32::from(CF_DIB.0)) }.map_err(|e| ClipboardError::Busy {
            reason: format!("GetClipboardData(CF_DIB) failed (ownership churn?): {e}"),
        })?;
    if handle.is_invalid() {
        return Ok(DibProbe::Absent);
    }

    let hglobal = HGLOBAL(handle.0);
    // SAFETY: `hglobal` came from GetClipboardData while the clipboard is
    // open. GlobalSize reads the block's size without locking or copying
    // it, which is what lets the bound below be enforced before any
    // allocation. It reports 0 for an invalid or discarded block.
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 {
        return Ok(DibProbe::Absent);
    }
    if size > max_bytes {
        // Graceful refusal, never a truncated image — and reported, not
        // logged here, so the caller can close the clipboard first.
        return Ok(DibProbe::TooLarge { byte_count: size });
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
    // canonicalizes the length is left to the caller, which runs it once
    // the clipboard is closed, so Crossover is not the reason another
    // application's paste fails (FR-3.1a).
    let blob = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
    // SAFETY: balances the successful GlobalLock above. GlobalUnlock
    // reports "no longer locked" as an error-shaped success; ignore.
    let _ = unsafe { GlobalUnlock(hglobal) };

    Ok(DibProbe::Raw(blob))
}

/// One `DragQueryFileW` name's own length bound, independent of
/// [`MAX_CLIPBOARD_FILE_ENTRIES`] (which bounds the *list*, not one name).
///
/// This is not `crossover_protocol`'s `MAX_FILE_NAME_*` — those bound the
/// *sanitized* name that later travels the wire, and applying them here
/// would be validating a source path as though it were already the
/// network input it is not yet (ADR 0015 leaves that to the sender-side
/// selection walk, feature/135). This bound exists purely so a length
/// `DragQueryFileW` reports cannot drive an unbounded allocation (NFR-1);
/// it is Windows' own long-path ceiling (`\\?\`-prefixed paths run to
/// about 32K UTF-16 units), far past anything a real path needs.
const MAX_HDROP_PATH_UNITS: u32 = 32_767;

/// What a `CF_HDROP` probe found.
///
/// "Too many entries" is a variant rather than a log line inside the probe
/// for the same reason [`DibProbe::TooLarge`] is: the probe runs with the
/// clipboard open, and `tracing` under the machine-global lock can block on
/// a subscriber's I/O. The caller reports it once the guard has dropped.
enum HdropProbe {
    /// No `CF_HDROP` representation, an empty one, or one this probe could
    /// not safely enumerate (a name past [`MAX_HDROP_PATH_UNITS`], or a
    /// `DragQueryFileW` call that failed mid-list) — refused whole rather
    /// than reported as a partial selection.
    Absent,
    /// Present, but past [`MAX_CLIPBOARD_FILE_ENTRIES`] — refused whole,
    /// never truncated.
    TooManyEntries { entry_count: u32 },
    /// Present, within the ceiling, and every path successfully read.
    Raw(Vec<PathBuf>),
}

/// Probe `CF_HDROP` on the already-open clipboard (ADR 0015).
///
/// Takes the open guard rather than opening, so this and
/// [`probe_unicode_text`]/[`probe_dib`] see one clipboard state — see
/// [`read_current`].
///
/// Unlike `CF_UNICODETEXT`/`CF_DIB`, there is no raw block to `GlobalLock`
/// and copy out before parsing: an `HDROP` is an opaque shell structure,
/// and `DragQueryFileW` is the only sanctioned way to read it. So, unlike
/// [`probe_dib`], the UTF-16-to-`PathBuf` conversion happens *inside* the
/// critical section here — there is no separable "copy now, decode later"
/// step to defer. This is bounded work regardless: at most
/// [`MAX_CLIPBOARD_FILE_ENTRIES`] calls, each against a length already
/// capped by [`MAX_HDROP_PATH_UNITS`], nothing like the multi-megabyte
/// image case the deferred-decode discipline exists for.
///
/// The entry count is read from `DragQueryFileW(hdrop, u32::MAX, None)` —
/// documented Win32 behaviour for "how many files" — and checked against
/// `max_entries` **before** a single name is queried (NFR-1): a selection
/// past the ceiling is refused without touching its paths at all.
fn probe_hdrop(_open: &OpenGuard, max_entries: u32) -> Result<HdropProbe, ClipboardError> {
    // SAFETY: no arguments; checks format availability only.
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_HDROP.0)) }.is_err() {
        return Ok(HdropProbe::Absent); // empty clipboard, or no file-list representation
    }

    // SAFETY: the clipboard is open (caller's guard); the returned handle
    // stays owned by the clipboard, never by us. Ownership can churn
    // between our open and this call, which surfaces as an error here and
    // is retryable contention, not a fault (R-5).
    let handle =
        unsafe { GetClipboardData(u32::from(CF_HDROP.0)) }.map_err(|e| ClipboardError::Busy {
            reason: format!("GetClipboardData(CF_HDROP) failed (ownership churn?): {e}"),
        })?;
    if handle.is_invalid() {
        return Ok(HdropProbe::Absent);
    }
    let hdrop = HDROP(handle.0);

    // SAFETY: `hdrop` came from GetClipboardData while the clipboard is
    // open. `ifile = u32::MAX` with no buffer is the documented
    // "how many files" query — it reads the structure's own count, not a
    // name, so nothing is copied yet.
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    if count == 0 {
        return Ok(HdropProbe::Absent);
    }
    if count > max_entries {
        // Reported, not logged here, so the caller can close the
        // clipboard first — see the module note on `HdropProbe`.
        return Ok(HdropProbe::TooManyEntries { entry_count: count });
    }

    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: `hdrop` is a live HDROP from the open clipboard; a
        // `None` buffer with a real index is documented to report the
        // name's length (in UTF-16 units, excluding the terminator)
        // rather than copy anything.
        let needed = unsafe { DragQueryFileW(hdrop, index, None) };
        if needed == 0 || needed > MAX_HDROP_PATH_UNITS {
            // A name this probe cannot safely size is refused as part of
            // the whole selection, never as a silently shorter list
            // (the same "never truncated" discipline the length ceiling
            // above already keeps at the list level).
            return Ok(HdropProbe::Absent);
        }
        let mut buffer = vec![0u16; needed as usize + 1]; // +1 for the terminator DragQueryFileW writes
        // SAFETY: `buffer` has room for `needed` units plus the
        // terminator; `hdrop` and `index` are unchanged from the sizing
        // call above.
        let written = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) };
        if written == 0 {
            return Ok(HdropProbe::Absent);
        }
        // `written` excludes the terminator (documented behaviour),
        // matching `probe_unicode_text`'s own terminator handling.
        buffer.truncate(written as usize);
        paths.push(PathBuf::from(String::from_utf16_lossy(&buffer)));
    }
    Ok(HdropProbe::Raw(paths))
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
///
/// `max_bytes` mirrors the read path's ceiling onto the write path, and is
/// a parameter for the same reason [`probe_dib`]'s is: so the refusal is
/// provable without a 64 MiB fixture. Production passes
/// [`MAX_CLIPBOARD_IMAGE_BYTES`].
///
/// Nothing should reach here oversized — an inbound image is checked
/// against the same bound before its reassembly buffer is allocated
/// (`crossover_protocol::clipboard`), which is where the bound has to bite
/// for NFR-1. This is the backstop for a caller that is not the session:
/// it fails closed rather than handing Win32 an allocation the rest of the
/// system was promised it would never see.
///
/// Type is judged before size, deliberately. An oversized JPEG is refused
/// for being a JPEG, because that is the durable answer — it will never
/// install at any size — where "too big" invites a smaller retry that
/// would also fail.
fn write_image(
    format: ClipboardImageFormat,
    bytes: &[u8],
    max_bytes: usize,
) -> Result<(), ClipboardError> {
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
    if bytes.len() > max_bytes {
        // Permanent for this item, like an unsupported type: no retry
        // makes it smaller, and the origin is owed the wall it hit rather
        // than an expiring retry budget (NFR-3). The size is named; the
        // pixels never are (FR-7.4).
        return Err(ClipboardError::Unsupported {
            reason: format!(
                "image is {} bytes, past the {max_bytes}-byte clipboard ceiling",
                bytes.len()
            ),
        });
    }
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

/// A clipboard holder identified well enough to name — the Win32 lookups
/// in [`find_clipboard_holder`] populate this; [`format_holder`] renders
/// it, kept separate so the rendering is unit-testable without a live
/// window or process.
struct ClipboardHolder {
    pid: u32,
    thread: u32,
    /// Never the window title (FR-7.4): a title can carry document
    /// content, a class name cannot.
    window_class: String,
    is_this_process: bool,
    /// Never the full path (FR-7.4): a path can carry a username.
    image_file_name: Option<String>,
}

/// Identify who currently holds the clipboard open, for the `Busy`
/// diagnostic (FR-7.3) — hardware evidence (2026-09-01) showed a bare
/// "held elsewhere?" leaves no way to tell an external holder (Clipboard
/// History, a password manager, an RDP client) from contention inside this
/// process (e.g. the OLE virtual-file apartment thread in
/// `crate::virtual_file`).
///
/// Every lookup here is best-effort and degrades to "unidentified" on any
/// failure — never panics, never blocks (no waits), and every buffer is
/// fixed-size.
pub(crate) fn describe_clipboard_holder() -> String {
    format_holder(find_clipboard_holder())
}

/// The Win32 side of [`describe_clipboard_holder`]: who, if anyone
/// identifiable, has the clipboard open right now.
fn find_clipboard_holder() -> Option<ClipboardHolder> {
    // SAFETY: a bare query of global clipboard state; touches no handle
    // or buffer of our own.
    let holder = match unsafe { GetOpenClipboardWindow() } {
        Ok(hwnd) if !hwnd.is_invalid() => hwnd,
        // No window is associated with the open — the common shape for
        // `OpenClipboard(NULL)` (ours included), and also the signature
        // docs/SOAK.md already documents for a wedged Clipboard User
        // Service. Either way, there is nothing further to identify.
        _ => return None,
    };

    let mut pid = 0u32;
    // SAFETY: `holder` was just returned live by `GetOpenClipboardWindow`;
    // the out-pointer is valid for the duration of this call.
    let thread = unsafe { GetWindowThreadProcessId(holder, Some(&raw mut pid)) };
    if pid == 0 {
        return None;
    }

    let window_class = window_class_name(holder);
    // SAFETY: a bare read of the calling process's own id.
    let is_this_process = pid == unsafe { GetCurrentProcessId() };
    // The single most important bit is `is_this_process`: it separates
    // in-process contention from an external holder. An image name for
    // our own process would say nothing that pid doesn't already.
    let image_file_name = if is_this_process {
        None
    } else {
        process_image_file_name(pid)
    };

    Some(ClipboardHolder {
        pid,
        thread,
        window_class,
        is_this_process,
        image_file_name,
    })
}

/// Render a holder lookup into the `Busy` diagnostic's trailing clause.
/// Pure formatting — no Win32 calls — so this shape is covered by a plain
/// unit test.
fn format_holder(holder: Option<ClipboardHolder>) -> String {
    let Some(holder) = holder else {
        return "held by an unidentified owner (no window)".to_owned();
    };
    let ClipboardHolder {
        pid,
        thread,
        window_class,
        is_this_process,
        image_file_name,
    } = holder;
    if is_this_process {
        return format!(
            "held by this process (pid {pid}, thread {thread}, window class \"{window_class}\")"
        );
    }
    match image_file_name {
        Some(name) => format!("held by pid {pid} \"{name}\" (window class \"{window_class}\")"),
        None => format!("held by pid {pid} (window class \"{window_class}\")"),
    }
}

/// A window's class name, bounded and best-effort — never the window
/// title (FR-7.4). Any Win32 failure degrades to `"unknown"`.
fn window_class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: `hwnd` is a live window handle from the caller; `buffer` is
    // a real, fixed-size allocation for the duration of the call.
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    let Ok(len) = usize::try_from(len) else {
        return "unknown".to_owned();
    };
    match buffer.get(..len) {
        Some(units) if len > 0 => String::from_utf16_lossy(units),
        _ => "unknown".to_owned(),
    }
}

/// A process's own executable **file name** (never the full path — a path
/// can carry a username, FR-7.4), best-effort: any failure anywhere in
/// this chain — the process not opening, the query failing, a name this
/// build cannot decode — is `None` rather than propagated.
fn process_image_file_name(pid: u32) -> Option<String> {
    // SAFETY: `PROCESS_QUERY_LIMITED_INFORMATION` is read-only and
    // available even for a process we do not own; the handle is closed
    // below on every path out of this function.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = [0u16; 260]; // MAX_PATH; a longer path is reported truncated, never overrun
    let mut len = u32::try_from(buffer.len()).unwrap_or(0);
    // SAFETY: `process` is the handle opened above, live for this call;
    // `buffer`/`len` describe a real, sized allocation the API writes
    // into and reports the written length back through.
    let queried = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )
    };
    // SAFETY: closes the handle opened above, exactly once, regardless of
    // whether the query succeeded.
    unsafe {
        let _ = CloseHandle(process);
    }
    queried.ok()?;
    let path = String::from_utf16_lossy(buffer.get(..len as usize)?);
    Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
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
            reason: format!(
                "OpenClipboard failed (clipboard held elsewhere?): {e}; {}",
                describe_clipboard_holder()
            ),
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    /// Probe `CF_DIB` on its own, in the order production uses it: open,
    /// probe, close, *then* canonicalize (`read_current` keeps the header
    /// arithmetic out of the machine-global lock). `None` covers both
    /// "no image" and "refused for size" — which is what the trait-level
    /// read reports for either, and what these tests assert against.
    fn probe_dib(max_bytes: usize) -> Result<Option<Vec<u8>>, ClipboardError> {
        let probe = {
            let open = super::OpenGuard::open()?;
            super::probe_dib(&open, max_bytes)?
        };
        Ok(match probe {
            super::DibProbe::Raw(blob) => Some(super::canonical_dib(blob)),
            super::DibProbe::Absent | super::DibProbe::TooLarge { .. } => None,
        })
    }

    /// The holder diagnostic's formatting, isolated from the Win32 lookups
    /// that populate it (feature/162) — deterministic, no live clipboard
    /// or window involved, covering every shape `format_holder` produces.
    #[test]
    fn format_holder_names_the_owner_or_says_unidentified() {
        use super::{ClipboardHolder, format_holder};

        assert_eq!(
            format_holder(None),
            "held by an unidentified owner (no window)"
        );

        assert_eq!(
            format_holder(Some(ClipboardHolder {
                pid: 4321,
                thread: 9,
                window_class: "Notepad".to_owned(),
                is_this_process: false,
                image_file_name: Some("notepad.exe".to_owned()),
            })),
            "held by pid 4321 \"notepad.exe\" (window class \"Notepad\")"
        );

        // A pid resolved but the image name lookup failed (protected
        // process, race with exit, etc.) — degrades gracefully rather
        // than dropping the pid it does have.
        assert_eq!(
            format_holder(Some(ClipboardHolder {
                pid: 4321,
                thread: 9,
                window_class: "Notepad".to_owned(),
                is_this_process: false,
                image_file_name: None,
            })),
            "held by pid 4321 (window class \"Notepad\")"
        );

        // The case the feature exists for: our own process, not an
        // external application.
        assert_eq!(
            format_holder(Some(ClipboardHolder {
                pid: 1234,
                thread: 42,
                window_class: "CLIPBRDWNDCLASS".to_owned(),
                is_this_process: true,
                image_file_name: None,
            })),
            "held by this process (pid 1234, thread 42, window class \"CLIPBRDWNDCLASS\")"
        );
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

    /// The ceiling mirrored onto the write path. An oversized image is
    /// refused *before* `install_formats`, so nothing reaches Win32 and no
    /// clipboard lock is taken — the same permanent `Unsupported` the read
    /// path's absence corresponds to, rather than a `Busy` the engine
    /// would retry until its budget expired (NFR-3).
    #[test]
    fn an_image_over_the_ceiling_is_refused_rather_than_installed() {
        use crossover_platform::ClipboardImageFormat;

        let picture = dib(8, 8);
        match super::write_image(ClipboardImageFormat::Dib, &picture, picture.len() - 1) {
            Err(ClipboardError::Unsupported { reason }) => {
                assert!(
                    reason.contains(&picture.len().to_string()),
                    "the diagnostic must name the size it refused: {reason}"
                );
            }
            other => panic!("an oversized image must be refused, got {other:?}"),
        }
        // The same bytes under a ceiling that admits them still install,
        // so the check bounds the size and nothing else.
        let _serial = clipboard_lock();
        with_retry(|| super::write_image(ClipboardImageFormat::Dib, &picture, picture.len()))
            .unwrap();
    }

    /// Type is judged before size: an oversized JPEG is refused for being
    /// a JPEG. "Too big" would invite a smaller retry that must also fail,
    /// where the type refusal is the durable answer (NFR-3).
    #[test]
    fn an_oversized_unsupported_type_is_refused_for_its_type() {
        use crossover_platform::ClipboardImageFormat;

        match super::write_image(ClipboardImageFormat::Jpeg, &[0u8; 64], 8) {
            Err(ClipboardError::Unsupported { reason }) => assert!(
                reason.contains("ADR 0014"),
                "the type refusal must win over the size one: {reason}"
            ),
            other => panic!("expected a permanent refusal, got {other:?}"),
        }
    }

    /// The terminator scan is bounded by the block, not by trust in the
    /// application that produced it: an unterminated `CF_UNICODETEXT` must
    /// stop at `GlobalSize` rather than read past the allocation.
    ///
    /// Fixtures rather than the real clipboard, of necessity — Windows
    /// normalizes an unterminated block into a terminated one on the way
    /// through (measured: a 26-unit unterminated block reads back as 25
    /// units and a terminator), so the case this bound exists for cannot
    /// be staged through the OS. Owned `Vec`s put it in reach.
    #[test]
    fn the_terminator_scan_stops_at_the_bound() {
        let terminated: Vec<u16> = vec![b'h'.into(), b'i'.into(), 0, b'?'.into()];
        // SAFETY: every call below reads within its own live allocation.
        unsafe {
            // The ordinary case: the terminator ends it, short of the bound.
            assert_eq!(super::terminated_len(terminated.as_ptr(), 4), 2);

            // Unterminated: the bound ends it, and nothing past it is read.
            let unterminated: Vec<u16> = vec![b'h'.into(), b'i'.into()];
            assert_eq!(super::terminated_len(unterminated.as_ptr(), 2), 2);

            // The bound wins even with a terminator beyond it.
            assert_eq!(super::terminated_len(terminated.as_ptr(), 1), 1);

            // A block that is nothing but its terminator is empty text,
            // which is the case `empty_text_steps_aside_for_an_image`
            // depends on telling apart from absent.
            let empty: Vec<u16> = vec![0];
            assert_eq!(super::terminated_len(empty.as_ptr(), 1), 0);

            // A zero bound reads nothing at all, which is what a block too
            // small to hold one unit must produce.
            assert_eq!(super::terminated_len(terminated.as_ptr(), 0), 0);
        }
    }

    /// The Windows clipboard is machine-global, and the virtual-file
    /// object shares it, so the lock is crate-wide rather than per module
    /// (see `test_support`).
    use crate::test_support::clipboard_lock;

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

    /// The diagnostic's most important bit (feature/162): when the holder
    /// is identifiable at all, it must say *this process*, not merely
    /// "held elsewhere" — distinguishing in-process contention (the OLE
    /// virtual-file apartment thread's shape) from a genuinely external
    /// application. Unlike the test above, the holder thread opens with a
    /// **real window** rather than `OpenClipboard(None)`: Win32 makes a
    /// NULL-owner open invisible to `GetOpenClipboardWindow` (the
    /// wedged-service signature docs/SOAK.md documents), so naming the
    /// holder needs a window to name.
    #[test]
    fn contention_reason_names_this_process_when_a_window_is_identifiable() {
        use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
        };
        use windows::core::w;

        let _serial = clipboard_lock();
        let clipboard = WindowsClipboard::new().unwrap();
        with_retry(|| clipboard.write_text("before named contention")).unwrap();

        let (holding_tx, holding_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            // SAFETY: a message-only window from the prebuilt STATIC
            // class, destroyed before this thread exits.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("crossover-holder-test"),
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
            };
            let Ok(hwnd) = hwnd else {
                holding_tx.send(false).ok();
                return;
            };

            let mut acquired = false;
            for _ in 0..20 {
                // SAFETY: `hwnd` is the live window just created, kept
                // alive for the rest of this closure.
                if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                    acquired = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            holding_tx.send(acquired).ok();
            if acquired {
                let _ = release_rx.recv();
                // SAFETY: balances the successful open above.
                unsafe {
                    let _ = CloseClipboard();
                }
            }
            // SAFETY: destroys the window this thread created; nothing
            // else references it once the clipboard is closed.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        });

        let held = holding_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("holder thread did not report");
        if !held {
            release_tx.send(()).ok();
            holder.join().unwrap();
            eprintln!("skipped: could not stage window-owned contention");
            return;
        }

        // Same honest limitation as the test above: Win32 may admit
        // another thread of this same process anyway, in which case there
        // is no Busy reason to inspect.
        match clipboard.write_text("during named contention") {
            Err(ClipboardError::Busy { reason }) => {
                // `GetClassNameW` reports the predefined "STATIC" class
                // back as "Static" — the window's own canonical casing,
                // not the name it was created with.
                assert!(
                    reason.contains("held by this process") && reason.contains("Static"),
                    "reason must name this process and the holder window's class: {reason}"
                );
            }
            Ok(()) => eprintln!(
                "skipped: this thread's window-owned open admitted us, nothing to inspect"
            ),
            Err(other) => panic!("contention must classify as Busy, got {other:?}"),
        }

        release_tx.send(()).ok();
        holder.join().unwrap();

        with_retry(|| clipboard.write_text("after named contention")).unwrap();
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
            with_retry(|| probe_dib(super::MAX_CLIPBOARD_IMAGE_BYTES))
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
            with_retry(|| probe_dib(picture.len() - 1)).unwrap(),
            None,
            "an oversized image must read as absent"
        );
        // The same item under the real ceiling: present and whole.
        assert_eq!(
            with_retry(|| probe_dib(super::MAX_CLIPBOARD_IMAGE_BYTES))
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
            with_retry(|| probe_dib(super::MAX_CLIPBOARD_IMAGE_BYTES)).unwrap(),
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

    /// A `BITMAPINFOHEADER` with every field under the test's control, so
    /// the fuzz corpus can reach the arithmetic instead of bouncing off
    /// the first field check.
    #[derive(Clone, Copy)]
    struct Header {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        clr_used: u32,
    }

    impl Header {
        /// The 40 bytes, always — `size` is the *declared* header size,
        /// which is a field like any other and may disagree.
        fn bytes(self) -> Vec<u8> {
            let mut out = Vec::with_capacity(40);
            out.extend_from_slice(&self.size.to_le_bytes());
            out.extend_from_slice(&self.width.to_le_bytes());
            out.extend_from_slice(&self.height.to_le_bytes());
            out.extend_from_slice(&self.planes.to_le_bytes());
            out.extend_from_slice(&self.bit_count.to_le_bytes());
            out.extend_from_slice(&self.compression.to_le_bytes());
            out.extend_from_slice(&self.size_image.to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
            out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
            out.extend_from_slice(&self.clr_used.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
            out
        }
    }

    /// The DIB length, computed independently of the implementation:
    /// textbook row-stride arithmetic, written out longhand, for the
    /// uncompressed layouts only. A cross-check, not a mirror — if the
    /// production arithmetic is refactored into agreement with itself but
    /// out of agreement with the format, this disagrees.
    fn textbook_len(h: Header) -> Option<usize> {
        if h.size != 40 || h.planes != 1 || h.width <= 0 || h.height == 0 {
            return None;
        }
        let table = match h.bit_count {
            1 | 4 | 8 => {
                let entries = if h.clr_used == 0 {
                    1usize << h.bit_count
                } else {
                    usize::try_from(h.clr_used).ok()?
                };
                if entries > 256 {
                    return None;
                }
                entries * 4
            }
            16 | 24 | 32 => {
                let masks = match h.compression {
                    3 => 12,
                    6 => 16,
                    _ => 0,
                };
                masks + usize::try_from(h.clr_used).ok()? * 4
            }
            _ => return None,
        };
        if !matches!(h.compression, 0 | 3 | 6) {
            return None; // compressed payloads are not computable
        }
        let row_bits = usize::try_from(h.width).ok()? * usize::from(h.bit_count);
        let stride = row_bits.div_ceil(32) * 4;
        let pixels = (stride * usize::try_from(h.height.unsigned_abs()).ok()?)
            .max(usize::try_from(h.size_image).ok()?);
        Some(40 + table + pixels)
    }

    /// What one corpus case exercised, so the caller can prove the corpus
    /// reaches the code rather than assuming it.
    struct CaseOutcome {
        /// The blob was longer than its canonical form: the trimming path
        /// ran.
        trimmed: bool,
        /// The canonical form describes itself completely, so the loop
        /// guard property applies to it.
        self_describing: bool,
    }

    /// The four properties, asserted over one blob. Extracted so the
    /// corpus generator stays readable; every assertion names the
    /// iteration, so a failure is reproducible from the seed.
    fn assert_canonical_properties(blob: &[u8], header: Header, iteration: u32) -> CaseOutcome {
        // 1 + 2: no panic, and the result is a prefix — never grown,
        // never rewritten.
        let once = super::canonical_dib(blob.to_vec());
        assert!(
            once.len() <= blob.len() && once.as_slice() == &blob[..once.len()],
            "canonicalization must return a prefix (iteration {iteration})"
        );

        // 3: idempotent.
        assert_eq!(
            super::canonical_dib(once.clone()),
            once,
            "canonicalization is not idempotent (iteration {iteration})"
        );

        // Cross-check: the independent formula must agree wherever it has
        // an opinion and the blob is long enough to satisfy it.
        if let Some(expected) = textbook_len(header)
            && expected <= blob.len()
        {
            assert_eq!(
                super::dib_logical_len(blob),
                Some(expected),
                "the implementation and the textbook formula disagree \
                 (iteration {iteration})"
            );
        }

        // 4: the loop guard, over blobs that describe themselves.
        let self_describing = super::dib_logical_len(&once).is_some();
        if self_describing {
            for pad in [1usize, 7, 32] {
                let mut padded = once.clone();
                padded.resize(padded.len() + pad, 0);
                assert_eq!(
                    super::canonical_dib(padded),
                    once,
                    "allocator slack changed the canonical form — our own write \
                     would read back as new content and loop (iteration {iteration})"
                );
            }
        }

        CaseOutcome {
            trimmed: once.len() < blob.len(),
            self_describing,
        }
    }

    /// **The properties the loop guard rests on, over a corpus that
    /// actually reaches the arithmetic.**
    ///
    /// The predecessor of this test fed unstructured random bytes, which
    /// meant every single case died on the first field check (a random
    /// `u32` is `40` with probability 2⁻³²) and none of the geometry
    /// arithmetic below it ever ran — a refactor of that arithmetic would
    /// have sailed through green. So the corpus is built from headers
    /// instead of from noise, and the test asserts it measurably reaches
    /// the trimming path rather than trusting that it does.
    ///
    /// Four properties, over both a *coherent* corpus (blobs sized to
    /// their own geometry, plus slack) and a *hostile* one (fields chosen
    /// to fight the arithmetic — zero and absurd depths, every
    /// compression, palette counts either side of 256, extreme and
    /// negative dimensions, header sizes from every `BITMAPINFO` variant):
    ///
    /// 1. no panic on anything (NFR-1: these bytes are network-influenced,
    ///    since a peer's image is installed, read back, and canonicalized);
    /// 2. the output is always a *prefix* — never grown, never rewritten;
    /// 3. canonicalization is idempotent;
    /// 4. **the loop guard itself**: for a blob that describes itself
    ///    completely, appending allocator slack cannot change the result.
    ///    That is the fixed point FR-3.3 needs — our own write reads back
    ///    identical, so its content hash matches and the item is not
    ///    offered back to the peer that sent it.
    ///
    /// Property 4 is stated over self-describing blobs on purpose, and the
    /// exclusion is honest rather than convenient: a blob *shorter* than
    /// its own header claims is kept whole (conservative, so a valid image
    /// is never cut short), and appending enough bytes can complete it, so
    /// its canonical form legitimately changes. Reaching that needs a
    /// malformed source DIB *and* allocator rounding large enough to close
    /// the gap, and even then it costs one extra bounce and then settles —
    /// the completed blob is self-describing, so the next hop is stable.
    /// It is not an unbounded loop, and the test says which set it covers.
    #[test]
    fn dib_length_arithmetic_holds_over_a_corpus_that_reaches_it() {
        const HEADER_SIZES: [u32; 6] = [12, 40, 52, 56, 108, 124];
        const WIDTHS: [i32; 12] = [0, 1, 2, 3, 5, 16, 64, 1024, 3840, 7680, i32::MAX, i32::MIN];
        const HEIGHTS: [i32; 10] = [0, 1, 2, 3, 16, 1080, 2160, -1, -16, i32::MIN];
        const PLANES: [u16; 4] = [0, 1, 2, u16::MAX];
        const BIT_COUNTS: [u16; 10] = [0, 1, 2, 4, 8, 16, 24, 32, 48, 64];
        const COMPRESSIONS: [u32; 9] = [0, 1, 2, 3, 4, 5, 6, 7, u32::MAX];
        const SIZE_IMAGES: [u32; 6] = [0, 1, 64, 4096, 1 << 20, u32::MAX];
        const CLR_USEDS: [u32; 6] = [0, 1, 255, 256, 257, u32::MAX];
        const SLACKS: [usize; 6] = [0, 1, 3, 8, 16, 64];

        // Deterministic, so a failure is reproducible from the seed alone.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let pick = |len: usize, r: u64| usize::try_from(r % len as u64).unwrap_or(0);

        let mut trimmed = 0usize; // property-4 evidence
        let mut modelled = 0usize; // headers the arithmetic accepted

        for iteration in 0..40_000u32 {
            // Half the corpus is coherent — sized to its own geometry, so
            // the trimming path is genuinely reached — and half is
            // hostile, sized independently of what the header claims.
            let coherent = iteration % 2 == 0;
            let r = [
                next(),
                next(),
                next(),
                next(),
                next(),
                next(),
                next(),
                next(),
            ];
            let header = if coherent {
                Header {
                    size: 40,
                    width: [1, 2, 3, 5, 16, 64, 127, 256][pick(8, r[0])],
                    height: [1, 2, 3, 16, 64, -1, -16, -64][pick(8, r[1])],
                    planes: 1,
                    bit_count: [1, 4, 8, 16, 24, 32][pick(6, r[2])],
                    compression: [0, 3, 6][pick(3, r[3])],
                    size_image: [0, 0, 0, 16][pick(4, r[4])],
                    clr_used: [0, 0, 1, 16, 255, 256][pick(6, r[5])],
                }
            } else {
                Header {
                    size: HEADER_SIZES[pick(HEADER_SIZES.len(), r[0])],
                    width: WIDTHS[pick(WIDTHS.len(), r[1])],
                    height: HEIGHTS[pick(HEIGHTS.len(), r[2])],
                    planes: PLANES[pick(PLANES.len(), r[3])],
                    bit_count: BIT_COUNTS[pick(BIT_COUNTS.len(), r[4])],
                    compression: COMPRESSIONS[pick(COMPRESSIONS.len(), r[5])],
                    size_image: SIZE_IMAGES[pick(SIZE_IMAGES.len(), r[6])],
                    clr_used: CLR_USEDS[pick(CLR_USEDS.len(), r[7])],
                }
            };
            let slack = SLACKS[pick(SLACKS.len(), next())];

            let mut blob = header.bytes();
            if coherent {
                // Grow to exactly what an independent reading of the
                // format says this geometry needs, then add slack.
                if let Some(len) = textbook_len(header)
                    && len <= 1 << 20
                {
                    blob.resize(len, 0x5A);
                }
            } else {
                let body = [0usize, 4, 40, 111, 1024, 4096][pick(6, next())];
                blob.resize(40 + body, 0x5A);
            }
            blob.resize(blob.len() + slack, 0xAB);

            let outcome = assert_canonical_properties(&blob, header, iteration);
            trimmed += usize::from(outcome.trimmed);
            modelled += usize::from(outcome.self_describing);
        }

        // Without this the test could silently regress into the shape it
        // replaced: green, and never once past the first field check.
        assert!(
            trimmed > 1_000,
            "the corpus barely reached the trimming path ({trimmed} trims); \
             it is not testing the arithmetic"
        );
        assert!(
            modelled > 1_000,
            "too few self-describing blobs ({modelled}) to pin the loop guard"
        );
        eprintln!("structured DIB corpus: {trimmed} trims, {modelled} self-describing");

        // Unstructured noise still must not panic — cheap, and the old
        // test's one genuine contribution.
        for len in 0..200usize {
            let blob: Vec<u8> = (0..len)
                .map(|_| u8::try_from(next() & 0xFF).unwrap_or(0))
                .collect();
            let out = super::canonical_dib(blob.clone());
            assert_eq!(out.as_slice(), &blob[..out.len()]);
        }
    }

    // ---- files: CF_HDROP observation (ADR 0015, feature/133) -------------

    /// Bytes of a `CF_HDROP` block for `paths` — a `DROPFILES` header
    /// (wide strings, no non-client drop) followed by a
    /// double-null-terminated list of null-terminated wide names, exactly
    /// what Explorer's own file/folder copy publishes.
    ///
    /// Built by hand rather than through the provider: production never
    /// *writes* this format — `ClipboardContent::FileList` is refused by
    /// `WindowsClipboard::write` by design, because a file list reaches
    /// the clipboard through `VirtualFileClipboard`, not this trait — so
    /// there is no production-shaped helper to stage a fixture with.
    fn hdrop_bytes(paths: &[&str]) -> Vec<u8> {
        use windows::Win32::UI::Shell::DROPFILES;

        // The struct's own size, not a hardcoded number: `#[repr(C,
        // packed(1))]` makes this exactly the byte offset DROPFILES.pFiles
        // must name for the list to follow immediately.
        let header_len =
            u32::try_from(std::mem::size_of::<DROPFILES>()).expect("DROPFILES fits a u32");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header_len.to_le_bytes()); // pFiles
        bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
        bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
        bytes.extend_from_slice(&0i32.to_le_bytes()); // fNC = FALSE
        bytes.extend_from_slice(&1i32.to_le_bytes()); // fWide = TRUE
        debug_assert_eq!(u32::try_from(bytes.len()).unwrap_or(u32::MAX), header_len);
        for path in paths {
            for unit in path.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes.extend_from_slice(&0u16.to_le_bytes()); // per-name terminator
        }
        bytes.extend_from_slice(&0u16.to_le_bytes()); // list terminator
        bytes
    }

    /// Install `paths` on the real clipboard as `CF_HDROP`.
    fn set_hdrop(paths: &[&str]) {
        let bytes = hdrop_bytes(paths);
        with_retry(|| super::install_formats(&[(u32::from(super::CF_HDROP.0), &bytes)])).unwrap();
    }

    /// The whole point of the slice: a local Explorer copy reads back as
    /// the paths the shell reported, not as text and not as an image.
    #[test]
    fn a_local_file_selection_reads_back_as_absolute_paths() {
        use crossover_platform::ClipboardContent;

        let _serial = clipboard_lock();
        let paths = [r"C:\Users\test\report.pdf", r"C:\Users\test\photos"];
        set_hdrop(&paths);

        let clipboard = WindowsClipboard::new().unwrap();
        match with_retry(|| clipboard.read()).unwrap() {
            Some(ClipboardContent::FileList(observed)) => {
                assert_eq!(
                    observed,
                    paths
                        .iter()
                        .map(std::path::PathBuf::from)
                        .collect::<Vec<_>>()
                );
            }
            other => panic!("expected a file-list observation, got {other:?}"),
        }
        // The text convenience must not surface a file selection as text.
        assert_eq!(with_retry(|| clipboard.read_text()).unwrap(), None);
    }

    /// A selection past [`crossover_platform::MAX_CLIPBOARD_FILE_ENTRIES`]
    /// reads as absent — refused before a single path is queried, never
    /// truncated to the first N (FR-3.6, NFR-1).
    #[test]
    fn a_selection_over_the_entry_ceiling_reads_as_absent() {
        let _serial = clipboard_lock();
        let too_many: Vec<String> = (0..=crossover_platform::MAX_CLIPBOARD_FILE_ENTRIES)
            .map(|i| format!(r"C:\overflow\{i}.txt"))
            .collect();
        let refs: Vec<&str> = too_many.iter().map(String::as_str).collect();
        set_hdrop(&refs);

        let clipboard = WindowsClipboard::new().unwrap();
        assert_eq!(with_retry(|| clipboard.read()).unwrap(), None);
    }

    /// `ClipboardContent::FileList` is a local observation, not something
    /// this trait installs: a file list reaches the clipboard through
    /// `VirtualFileClipboard`, a separate mechanism (ADR 0015). The
    /// refusal must be permanent (`Unsupported`), matching every other
    /// "this backend does not do that" answer on the write path.
    #[test]
    fn writing_a_file_list_through_the_provider_is_refused() {
        use crossover_platform::ClipboardContent;

        let clipboard = WindowsClipboard::new().unwrap();
        let refusal = clipboard.write(&ClipboardContent::FileList(vec![std::path::PathBuf::from(
            r"C:\a.txt",
        )]));
        assert!(
            matches!(refusal, Err(ClipboardError::Unsupported { .. })),
            "expected a permanent refusal, got {refusal:?}"
        );
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
