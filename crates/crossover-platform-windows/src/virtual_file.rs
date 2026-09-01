//! The virtual file list Crossover offers to Explorer (ADR 0015's platform
//! slice, docs/SECURITY.md F14/F16).
//!
//! A completed transfer is bytes in the spool. What makes them pasteable is
//! an `IDataObject` we own, advertising `CFSTR_FILEDESCRIPTORW` (the name
//! and size) and `CFSTR_FILECONTENTS` (the bytes, produced only when
//! something asks). The user presses Ctrl+V wherever they intend, and **the
//! shell** creates the file there. Crossover writes nothing the user can
//! see.
//!
//! # Why a thread of its own
//!
//! An OLE clipboard object must live on a single-threaded apartment with a
//! message pump, and its render callbacks arrive on that thread, driven by
//! whichever application is pasting. That thread is deliberately **not**
//! the clipboard-change listener's: `GetData` is callable by any local
//! process, so serving renders on the listener's pump would let an
//! unprivileged process in a loop starve clipboard change notifications for
//! the whole machine — text and image sync included. Two threads, plus the
//! one-render-at-a-time bound below, keep an enthusiastic consumer confined
//! to the thread it is abusing.
//!
//! # What the object will and will not do
//!
//! The accepted `FORMATETC` set is exact, and everything else is refused
//! with the error the caller can act on:
//!
//! - `CFSTR_FILECONTENTS` is served **only** as `TYMED_ISTREAM`, and only
//!   for `lindex == 0`. A request for it as `TYMED_HGLOBAL` returns
//!   `DV_E_TYMED` rather than being honoured, because honouring it would
//!   force the whole item — up to 256 MiB — into one global allocation on
//!   demand, reachable by any local process. That is the exact cost delayed
//!   rendering exists to avoid, and the accepted consequence is that a
//!   paste target which cannot consume `IStream` cannot paste the item.
//! - The descriptor and the zone identifier are served only as
//!   `TYMED_HGLOBAL`, where they are tiny and fixed-size.
//! - `SetData` is refused outright. Nothing a consumer hands us is stored:
//!   the object is a promise about one spool entry and has no state a
//!   caller may contribute to.
//! - The stream is **read-only**. `Write`, `SetSize`, `Commit` and the lock
//!   operations all refuse, so a consumer cannot reach back through the
//!   promise into the spool.
//!
//! # What it never does
//!
//! Renders resolve an **opaque entry name** through the spool this object
//! was built with. No path or name from the caller is ever used, the only
//! caller-supplied index honoured is zero, and a render never touches the
//! network, needs a live session, or waits on the peer — the bytes are
//! already local and already verified (F14).
//!
//! `OleFlushClipboard` is never called, and the omission is deliberate:
//! flushing *renders* every promised format so the data survives the
//! process, which would pull the entire file into memory with no paste and
//! no user gesture. On shutdown the object is withdrawn instead.

// `#[implement]` generates the COM vtable glue — `#[inline(always)]`
// accessors and a reference-to-pointer cast per interface — which these
// two pedantic lints flag in code this module does not write and cannot
// style. Allowed here, at the smallest scope that covers the macro, rather
// than by relaxing them for the crate.
#![allow(clippy::inline_always, clippy::ref_as_ptr)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crossover_platform::{
    ClipboardError, SpoolStorage, VirtualFile, VirtualFileClipboard, validate_entry_name,
};
use windows::Win32::Foundation::{
    DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_ACCESSDENIED, E_INVALIDARG,
    E_NOTIMPL, E_POINTER, ERROR_BUSY, HGLOBAL, HWND, LPARAM, S_FALSE, S_OK, WPARAM,
};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA, ISequentialStream_Impl, IStream,
    IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    OleInitialize, OleIsCurrentClipboard, OleSetClipboard, OleUninitialize,
};
use windows::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_ZONEIDENTIFIER, FD_ATTRIBUTES, FD_FILESIZE,
    FD_PROGRESSUI, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
};
use windows::core::{BOOL, HRESULT, Ref, implement, w};

/// Private message telling the apartment thread a command is queued.
const WM_APP_COMMAND: u32 = WM_APP + 1;
/// Private message asking the apartment thread to shut down.
const WM_APP_SHUTDOWN: u32 = WM_APP + 2;

/// The zone stamped on a pasted file, as the shell writes it into the
/// `Zone.Identifier` alternate data stream.
///
/// **Zone 1, Local intranet — which is what the file actually is.** It
/// arrived from a paired machine on the local network, so that is the
/// honest description, and the marking is kept for what it genuinely
/// provides: provenance. Anything asking where the file came from can see
/// that it did not originate on this machine.
///
/// Zone 3 ("Internet") was built first and changed on a maintainer
/// decision. It is the marking that makes `SmartScreen` challenge an
/// executable and Office open a document in Protected View, and on a
/// two-machine LAN link that is friction on every ordinary paste in
/// exchange for a defence against an attacker SECURITY.md §6 already
/// places out of scope — a paired peer that has itself been compromised.
/// The cost is stated rather than hidden: ADR 0015 accepts that no
/// validator can reject a name like `report.pdf.exe` and argues those are
/// "contained downstream" by the zone marking. At zone 1 that containment
/// is weaker — the stream is still written and still readable, but the
/// execution-warning machinery does not treat the file as untrusted
/// content. F10's rule that Crossover itself never launches anything is
/// unaffected.
const ZONE_IDENTIFIER: &[u8] = b"[ZoneTransfer]\r\nZoneId=1\r\n";

/// `CanIncludeInClipboardHistory` and
/// `ExcludeClipboardContentFromMonitorProcessing` (F16). Both are
/// `DWORD`-valued formats offered by the data object; zero opts out.
///
/// The item they exclude is a *promise* that only this process, holding
/// this spool entry, can keep — so a retained history entry would fail on
/// paste, later, with no diagnostic from us. "No entry" is a better outcome
/// than "an entry that breaks", and the cloud half of the same exclusion
/// keeps peer-delivered file content off a Microsoft account, which
/// invariant 7 has no answer for.
const EXCLUDE_VALUE: [u8; 4] = 0u32.to_le_bytes();

/// Capacity of `FILEDESCRIPTORW::cFileName`, which is a fixed
/// `WCHAR[260]`. Named because it is the bound
/// `MAX_FILE_NAME_UTF16_UNITS` (259, plus the terminator) was chosen
/// against, and a test asserts the binding still agrees with it.
const NAME_UNITS: usize = 260;

/// The formats this object advertises, resolved once on the apartment
/// thread. Registered names are process-wide and idempotent.
#[derive(Debug, Clone, Copy)]
struct Formats {
    descriptor: u16,
    contents: u16,
    zone: u16,
    history: u16,
    monitoring: u16,
}

impl Formats {
    fn register() -> Result<Self, ClipboardError> {
        Ok(Self {
            descriptor: register(CFSTR_FILEDESCRIPTORW)?,
            contents: register(CFSTR_FILECONTENTS)?,
            zone: register(CFSTR_ZONEIDENTIFIER)?,
            history: register(w!("CanIncludeInClipboardHistory"))?,
            monitoring: register(w!("ExcludeClipboardContentFromMonitorProcessing"))?,
        })
    }

    /// Every format, in the order `EnumFormatEtc` reports them: the two a
    /// paste actually needs first, then the markers.
    fn all(self) -> [(u16, u32); 5] {
        [
            (self.descriptor, TYMED_HGLOBAL.0 as u32),
            (self.contents, TYMED_ISTREAM.0 as u32),
            (self.zone, TYMED_HGLOBAL.0 as u32),
            (self.history, TYMED_HGLOBAL.0 as u32),
            (self.monitoring, TYMED_HGLOBAL.0 as u32),
        ]
    }
}

fn register(name: windows::core::PCWSTR) -> Result<u16, ClipboardError> {
    // SAFETY: `name` is a static NUL-terminated wide literal. Registration
    // is idempotent — an already-registered name returns its existing id —
    // and returns 0 on failure.
    let id = unsafe { RegisterClipboardFormatW(name) };
    u16::try_from(id)
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| ClipboardError::Unavailable {
            reason: "RegisterClipboardFormatW failed for a virtual-file format".to_owned(),
        })
}

/// Build the `FILEGROUPDESCRIPTORW` block: one descriptor, carrying the
/// validated name and the exact byte length.
///
/// Pure, so the part that decides what a shell is told about a
/// peer-supplied name is testable without a clipboard. The name is
/// re-checked here — validation precedes descriptor construction, not just
/// the write (ADR 0015) — and `cFileName` is a fixed `WCHAR[260]`, so a
/// name that would not fit with its terminator is refused rather than
/// truncated into something else.
fn file_group_descriptor(file_name: &str, byte_len: u64) -> Result<Vec<u8>, ClipboardError> {
    let units: Vec<u16> = file_name.encode_utf16().collect();
    if units.is_empty() || units.len() >= NAME_UNITS {
        return Err(ClipboardError::Unavailable {
            reason: format!(
                "a file name of {} UTF-16 units does not fit a {NAME_UNITS}-unit descriptor field",
                units.len()
            ),
        });
    }

    // Filled in a local and assigned whole: `FILEDESCRIPTORW` is packed, so
    // there is no reference to its array field to be taken, and the trailing
    // zeroes make the name NUL-terminated by construction rather than by a
    // separate write an off-by-one could skip.
    let mut name = [0u16; NAME_UNITS];
    name[..units.len()].copy_from_slice(&units);

    let descriptor = FILEDESCRIPTORW {
        dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32,
        dwFileAttributes: windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL.0,
        nFileSizeHigh: u32::try_from(byte_len >> 32).unwrap_or(u32::MAX),
        nFileSizeLow: u32::try_from(byte_len & u64::from(u32::MAX)).unwrap_or(u32::MAX),
        cFileName: name,
        ..FILEDESCRIPTORW::default()
    };

    let group = FILEGROUPDESCRIPTORW {
        cItems: 1,
        fgd: [descriptor],
    };
    // SAFETY: `group` is a live, fully initialized POD struct; this reads
    // its own bytes for the length it reports, and the copy outlives it.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&raw const group).cast::<u8>(),
            std::mem::size_of::<FILEGROUPDESCRIPTORW>(),
        )
    };
    Ok(bytes.to_vec())
}

/// A movable global block holding a copy of `bytes`, ready to be handed to
/// a consumer as a `TYMED_HGLOBAL` medium (which the consumer then frees
/// through `ReleaseStgMedium`).
fn global_block(bytes: &[u8]) -> Result<HGLOBAL, HRESULT> {
    if bytes.is_empty() {
        return Err(E_INVALIDARG);
    }
    // SAFETY: allocating a zeroed movable block of exactly this size.
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }
        .map_err(|_| E_ACCESSDENIED)?;
    // SAFETY: the block is ours and unlocked; lock it, copy exactly the
    // bytes it was allocated for, unlock. The consumer owns it afterwards.
    unsafe {
        let ptr = GlobalLock(hglobal).cast::<u8>();
        if ptr.is_null() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(hglobal));
            return Err(E_ACCESSDENIED);
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(hglobal);
    }
    Ok(hglobal)
}

/// The one-render-at-a-time bound (F14), held for as long as a content
/// stream is outstanding rather than for the duration of the `GetData`
/// call — the render *is* the stream, and the reading happens afterwards.
#[derive(Debug)]
struct RenderSlot(Arc<AtomicBool>);

impl RenderSlot {
    fn take(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for RenderSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A read-only `IStream` over one spool entry.
///
/// Read-only is a security property, not a simplification: the consumer of
/// a promise must not be able to write back through it into the spool,
/// where F14's "verified when written, protected since" claim lives.
#[implement(IStream)]
struct EntryStream {
    /// Guarded because COM may marshal this interface to another apartment
    /// and call it from there; the file's cursor is the shared state.
    file: Mutex<File>,
    byte_len: u64,
    /// Released when this stream is dropped, freeing the render slot.
    _slot: RenderSlot,
}

impl ISequentialStream_Impl for EntryStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_POINTER;
        }
        let want = cb as usize;
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: the caller guarantees `pv` addresses at least `cb` bytes,
        // which is the ISequentialStream::Read contract; nothing is read
        // from it, only written.
        let buffer = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), want) };
        let Ok(read) = file.read(buffer) else {
            return E_ACCESSDENIED;
        };
        if !pcbread.is_null() {
            // SAFETY: a non-null out-parameter supplied by the caller.
            unsafe { *pcbread = u32::try_from(read).unwrap_or(0) };
        }
        // S_FALSE for a short read is the documented contract, and some
        // consumers stop on it rather than on a zero count.
        if read < want { S_FALSE } else { S_OK }
    }

    fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        // The promise is one-way. A consumer that could write here would
        // be writing into the spool, past every check that put the entry
        // there.
        E_ACCESSDENIED
    }
}

impl IStream_Impl for EntryStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let from = match dworigin {
            STREAM_SEEK_SET => SeekFrom::Start(u64::try_from(dlibmove).unwrap_or(0)),
            STREAM_SEEK_CUR => SeekFrom::Current(dlibmove),
            STREAM_SEEK_END => SeekFrom::End(dlibmove),
            _ => return Err(E_INVALIDARG.into()),
        };
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let at = file
            .seek(from)
            .map_err(|_| windows::core::Error::from(E_ACCESSDENIED))?;
        if !plibnewposition.is_null() {
            // SAFETY: a non-null out-parameter supplied by the caller.
            unsafe { *plibnewposition = at };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        _pstm: Ref<IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        // Consumers that want the bytes read them; nothing needs us to
        // drive a copy into a stream of the caller's choosing, and doing so
        // would be an unbounded write we did not ask for.
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Err(E_ACCESSDENIED.into())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_ACCESSDENIED.into())
    }

    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: u32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(E_POINTER.into());
        }
        let stat = STATSTG {
            // No name: the descriptor carries it, and handing the same
            // peer-supplied string out twice is one more place for it to
            // be trusted as something other than data.
            pwcsName: windows::core::PWSTR::null(),
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.byte_len,
            ..Default::default()
        };
        // SAFETY: a non-null out-parameter supplied by the caller.
        unsafe { *pstatstg = stat };
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        // A clone would be a second reader of one entry under one render
        // slot, which is exactly what the bound exists to prevent.
        Err(E_NOTIMPL.into())
    }
}

/// The format enumerator handed to `EnumFormatEtc`.
#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
    formats: Vec<(u16, u32)>,
    next: AtomicUsize,
}

impl FormatEnumerator {
    fn new(formats: Vec<(u16, u32)>) -> Self {
        Self {
            formats,
            next: AtomicUsize::new(0),
        }
    }
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        if rgelt.is_null() {
            return E_POINTER;
        }
        let mut written = 0usize;
        let wanted = celt as usize;
        while written < wanted {
            let index = self.next.fetch_add(1, Ordering::AcqRel);
            let Some((format, tymed)) = self.formats.get(index) else {
                // Past the end: undo the speculative bump so a later
                // `Next` does not walk the counter away from the list.
                self.next.store(self.formats.len(), Ordering::Release);
                break;
            };
            // SAFETY: the caller guarantees `rgelt` addresses `celt`
            // elements; `written` is below that bound.
            unsafe {
                *rgelt.add(written) = FORMATETC {
                    cfFormat: *format,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: *tymed,
                };
            }
            written += 1;
        }
        if !pceltfetched.is_null() {
            // SAFETY: a non-null out-parameter supplied by the caller.
            unsafe { *pceltfetched = u32::try_from(written).unwrap_or(0) };
        }
        if written == wanted { S_OK } else { S_FALSE }
    }

    fn Skip(&self, celt: u32) -> windows::core::Result<()> {
        let at = self
            .next
            .load(Ordering::Acquire)
            .saturating_add(celt as usize);
        let capped = at.min(self.formats.len());
        self.next.store(capped, Ordering::Release);
        if capped == at {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn Reset(&self) -> windows::core::Result<()> {
        self.next.store(0, Ordering::Release);
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
        let clone = FormatEnumerator::new(self.formats.clone());
        clone
            .next
            .store(self.next.load(Ordering::Acquire), Ordering::Release);
        Ok(clone.into())
    }
}

/// The data object itself: one spool entry, offered as one virtual file.
#[implement(IDataObject)]
struct VirtualFileObject {
    spool: Arc<dyn SpoolStorage>,
    /// Ours, and the only thing a render resolves. Validated before the
    /// object was built, and validated again by the spool on every open.
    entry: String,
    /// Pre-built, so a render never formats a peer-supplied name.
    descriptor: Vec<u8>,
    byte_len: u64,
    formats: Formats,
    rendering: Arc<AtomicBool>,
}

impl VirtualFileObject {
    /// The medium for a format this object serves, or the typed refusal
    /// the ADR names for one it does not.
    fn medium_for(&self, request: &FORMATETC) -> Result<STGMEDIUM, HRESULT> {
        if request.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT);
        }
        let format = request.cfFormat;
        let wants = request.tymed;

        if format == self.formats.contents {
            // The one format with an index, and the only index served.
            if request.lindex != 0 {
                return Err(DV_E_LINDEX);
            }
            if wants & (TYMED_ISTREAM.0 as u32) == 0 {
                // Refused rather than served as HGLOBAL: see the module
                // header. A consumer that asks only for HGLOBAL cannot
                // paste this item, and says so through the shell's own
                // failure.
                return Err(DV_E_TYMED);
            }
            return self.content_stream();
        }

        if wants & (TYMED_HGLOBAL.0 as u32) == 0 {
            return Err(DV_E_TYMED);
        }
        let bytes: &[u8] = if format == self.formats.descriptor {
            &self.descriptor
        } else if format == self.formats.zone {
            ZONE_IDENTIFIER
        } else if format == self.formats.history || format == self.formats.monitoring {
            &EXCLUDE_VALUE
        } else {
            return Err(DV_E_FORMATETC);
        };
        Ok(global_medium(global_block(bytes)?))
    }

    /// Open the entry and hand back a stream over it, if no other render
    /// is outstanding.
    fn content_stream(&self) -> Result<STGMEDIUM, HRESULT> {
        let Some(slot) = RenderSlot::take(&self.rendering) else {
            // Refused, never queued: queueing would turn a refusal into
            // unbounded pending work driven by whoever asked, which is the
            // same denial with extra steps (F14). A genuine paste is a
            // single user gesture and never collides with itself.
            tracing::warn!(
                spool_entry = %self.entry,
                "refusing a file render while one is already in flight"
            );
            return Err(ERROR_BUSY.to_hresult());
        };
        let file = self.spool.open_entry(&self.entry).map_err(|error| {
            tracing::warn!(
                spool_entry = %self.entry,
                %error,
                "a paste asked for a spool entry that could not be opened"
            );
            E_ACCESSDENIED
        })?;
        let stream: IStream = EntryStream {
            file: Mutex::new(file),
            byte_len: self.byte_len,
            _slot: slot,
        }
        .into();
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: std::mem::ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    fn serves(&self, request: &FORMATETC) -> bool {
        self.formats
            .all()
            .iter()
            .any(|(format, tymed)| *format == request.cfFormat && request.tymed & *tymed != 0)
    }
}

fn global_medium(hglobal: HGLOBAL) -> STGMEDIUM {
    STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: hglobal },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    }
}

impl IDataObject_Impl for VirtualFileObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if pformatetcin.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: a non-null input pointer supplied by the caller, read
        // once into a local before anything is decided from it.
        let request = unsafe { *pformatetcin };
        self.medium_for(&request).map_err(Into::into)
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        // Rendering into a medium the caller allocated is exactly the
        // fixed-size-buffer path this design avoids; consumers that need
        // the bytes take the stream.
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if pformatetc.is_null() {
            return E_POINTER;
        }
        // SAFETY: a non-null input pointer supplied by the caller.
        let request = unsafe { *pformatetc };
        if request.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        if self.serves(&request) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        if !pformatetcout.is_null() {
            // The documented contract for a data object with no canonical
            // mapping: clear the output and report E_NOTIMPL.
            // SAFETY: a non-null out-parameter supplied by the caller.
            unsafe { *pformatetcout = FORMATETC::default() };
        }
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        // This object is a promise about one verified spool entry. There
        // is nothing a consumer may add to it, and accepting anything
        // would be accepting data from a caller we know nothing about.
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            // Nothing may be set on this object, so there is nothing to
            // enumerate in that direction.
            return Err(E_NOTIMPL.into());
        }
        Ok(FormatEnumerator::new(self.formats.all().to_vec()).into())
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// What the apartment thread is asked to do. Each command carries the
/// channel its answer goes back on, so callers see a real result rather
/// than a hope.
enum Command {
    Offer(
        VirtualFile,
        std::sync::mpsc::Sender<Result<(), ClipboardError>>,
    ),
    Withdraw(std::sync::mpsc::Sender<Result<(), ClipboardError>>),
    IsCurrent(std::sync::mpsc::Sender<bool>),
}

/// The Windows virtual-file clipboard: an apartment thread, and a handle
/// to it.
pub struct WindowsVirtualFiles {
    commands: Mutex<std::sync::mpsc::Sender<Command>>,
    /// The apartment's message-only window, as a raw value so this type is
    /// `Send`; used only with `PostMessageW`, which is thread-safe.
    hwnd_raw: isize,
    thread: Option<std::thread::JoinHandle<()>>,
    stopped: Mutex<std::sync::mpsc::Receiver<()>>,
}

// SAFETY: `hwnd_raw` is only ever passed to PostMessageW (thread-safe by
// API contract) and the window is owned and destroyed by the apartment
// thread; every other field is Mutex-guarded.
unsafe impl Send for WindowsVirtualFiles {}
// SAFETY: as above — no field is reachable without its Mutex, and the
// COM objects themselves never leave the apartment thread.
unsafe impl Sync for WindowsVirtualFiles {}

impl WindowsVirtualFiles {
    /// Start the apartment thread for `spool`.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Unavailable`] if the apartment, its window, or
    /// the clipboard format registration fails — file paste is then
    /// absent for the run rather than silently broken (NFR-3).
    pub fn new(spool: Arc<dyn SpoolStorage>) -> Result<Self, ClipboardError> {
        let (commands_tx, commands_rx) = std::sync::mpsc::channel::<Command>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<isize, String>>();
        let (stopped_tx, stopped) = std::sync::mpsc::channel::<()>();

        let thread = std::thread::Builder::new()
            .name("crossover-virtual-file-sta".to_owned())
            .spawn(move || {
                apartment_thread(&spool, &commands_rx, &init_tx);
                let _ = stopped_tx.send(());
            })
            .map_err(|e| ClipboardError::Unavailable {
                reason: format!("spawning the virtual-file apartment thread: {e}"),
            })?;

        let hwnd_raw = init_rx
            .recv()
            .map_err(|_| ClipboardError::Unavailable {
                reason: "the virtual-file apartment thread died during startup".to_owned(),
            })?
            .map_err(|reason| ClipboardError::Unavailable { reason })?;

        Ok(Self {
            commands: Mutex::new(commands_tx),
            hwnd_raw,
            thread: Some(thread),
            stopped: Mutex::new(stopped),
        })
    }

    /// Queue a command and wake the apartment thread, then wait for its
    /// answer.
    fn call<T>(
        &self,
        make: impl FnOnce(std::sync::mpsc::Sender<T>) -> Command,
        lost: impl FnOnce() -> T,
    ) -> T {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<T>();
        {
            let commands = self.commands.lock().unwrap_or_else(PoisonError::into_inner);
            if commands.send(make(reply_tx)).is_err() {
                return lost();
            }
        }
        // SAFETY: PostMessageW is safe from any thread with any window
        // handle; a stale handle at worst fails harmlessly, which the
        // recv below then reports as a lost apartment.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd_raw as *mut core::ffi::c_void)),
                WM_APP_COMMAND,
                WPARAM(0),
                LPARAM(0),
            )
        };
        reply_rx.recv().unwrap_or_else(|_| lost())
    }
}

impl Drop for WindowsVirtualFiles {
    fn drop(&mut self) {
        // SAFETY: as in `call` — PostMessageW is thread-safe.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd_raw as *mut core::ffi::c_void)),
                WM_APP_SHUTDOWN,
                WPARAM(0),
                LPARAM(0),
            )
        };
        let stopped = self
            .stopped
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        crate::pump::stop("virtual-file apartment", stopped, &mut self.thread);
    }
}

impl VirtualFileClipboard for WindowsVirtualFiles {
    fn offer(&self, file: &VirtualFile) -> Result<(), ClipboardError> {
        // Rejected here rather than inside the apartment: an entry name is
        // ours, so a malformed one is a defect, and it should not become a
        // clipboard error two thread hops away.
        validate_entry_name(&file.entry).map_err(|error| ClipboardError::Unavailable {
            reason: format!("refusing to offer an invalid spool entry: {error}"),
        })?;
        let file = file.clone();
        self.call(
            |reply| Command::Offer(file, reply),
            || {
                Err(ClipboardError::Unavailable {
                    reason: "the virtual-file apartment thread is gone".to_owned(),
                })
            },
        )
    }

    fn is_current(&self) -> bool {
        // A lost apartment cannot be holding the clipboard, so the
        // fail-safe answer is also the true one.
        self.call(Command::IsCurrent, || false)
    }

    fn withdraw(&self) -> Result<(), ClipboardError> {
        self.call(Command::Withdraw, || {
            // Nothing is offered if the apartment is gone: the object died
            // with it, and the postcondition the caller wants holds.
            Ok(())
        })
    }
}

/// The apartment thread: initialize OLE, create a message-only window,
/// then serve commands and COM callbacks until told to stop.
fn apartment_thread(
    spool: &Arc<dyn SpoolStorage>,
    commands: &std::sync::mpsc::Receiver<Command>,
    init: &std::sync::mpsc::Sender<Result<isize, String>>,
) {
    // SAFETY: initializing this thread as a single-threaded apartment,
    // which is what an OLE clipboard object requires; balanced below.
    if let Err(e) = unsafe { OleInitialize(None) } {
        let _ = init.send(Err(format!("OleInitialize failed: {e}")));
        return;
    }

    let formats = match Formats::register() {
        Ok(formats) => formats,
        Err(error) => {
            // SAFETY: balancing the OleInitialize above.
            unsafe { OleUninitialize() };
            let _ = init.send(Err(error.to_string()));
            return;
        }
    };

    // SAFETY: creating a message-only window from the prebuilt STATIC
    // class; this thread owns and destroys it.
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("crossover-virtual-file-apartment"),
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
            // SAFETY: balancing the OleInitialize above.
            unsafe { OleUninitialize() };
            let _ = init.send(Err(format!("creating the apartment window: {e}")));
            return;
        }
    };
    let _ = init.send(Ok(hwnd.0 as isize));

    // The object currently on the clipboard, if it is ours. Held so
    // `OleIsCurrentClipboard` has something to compare against, and so the
    // renders it serves keep working for as long as it is offered.
    let mut current: Option<Placement> = None;

    let mut msg = MSG::default();
    loop {
        // SAFETY: standard message pump for this thread's queue. Pumping
        // is what lets COM deliver render callbacks into this apartment.
        let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        match msg.message {
            WM_APP_COMMAND => {
                while let Ok(command) = commands.try_recv() {
                    serve(command, spool, formats, &mut current);
                }
            }
            WM_APP_SHUTDOWN => break,
            _ => {
                // SAFETY: standard message dispatch; this is the path COM
                // marshalled calls arrive on.
                unsafe {
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
        }
    }

    // Withdraw rather than flush. `OleFlushClipboard` would render every
    // promised format so the data outlives this process — which for a file
    // means pulling the whole entry into memory with nobody pasting.
    if current.is_some() {
        let _ = clear_clipboard(&mut current);
    }
    // SAFETY: tearing down what this thread created, in reverse order.
    unsafe {
        let _ = DestroyWindow(hwnd);
        OleUninitialize();
    }
}

/// Our object, and the clipboard state it was placed into.
///
/// The sequence number is not belt-and-braces. `OleIsCurrentClipboard`
/// compares against the object OLE last set, and it was observed here
/// answering "still ours" after a plain Win32 `SetClipboardData` in the
/// same process had replaced the contents — OLE learns about ownership
/// changes through its own hidden window, and a non-OLE write next door
/// does not always tell it. Loop prevention is exactly the case where a
/// stale "yes" is harmful: it would suppress staging a copy the user
/// genuinely made. `GetClipboardSequenceNumber` changes on *every*
/// clipboard update by anyone, so requiring both makes the answer
/// conservative in the safe direction — a race can only make us report
/// "not ours", which costs a redundant read.
struct Placement {
    object: IDataObject,
    sequence: u32,
}

/// Perform one command on the apartment thread.
fn serve(
    command: Command,
    spool: &Arc<dyn SpoolStorage>,
    formats: Formats,
    current: &mut Option<Placement>,
) {
    match command {
        Command::Offer(file, reply) => {
            let _ = reply.send(place(&file, spool, formats, current));
        }
        Command::Withdraw(reply) => {
            let _ = reply.send(clear_clipboard(current));
        }
        Command::IsCurrent(reply) => {
            let _ = reply.send(is_ours(current.as_ref()));
        }
    }
}

/// Build the object and put it on the clipboard.
fn place(
    file: &VirtualFile,
    spool: &Arc<dyn SpoolStorage>,
    formats: Formats,
    current: &mut Option<Placement>,
) -> Result<(), ClipboardError> {
    // Nothing is advertised to the shell until the name has passed
    // validation *again*, here, immediately before the descriptor that
    // carries it is built (ADR 0015).
    let descriptor = file_group_descriptor(&file.file_name, file.byte_len)?;
    let object: IDataObject = VirtualFileObject {
        spool: Arc::clone(spool),
        entry: file.entry.clone(),
        descriptor,
        byte_len: file.byte_len,
        formats,
        rendering: Arc::new(AtomicBool::new(false)),
    }
    .into();

    // Marked around the call itself, not held any longer: unlike
    // `OpenGuard`, `OleSetClipboard` has no separate close, so this is the
    // whole window in which a `Busy` failure elsewhere in this process
    // could name this site (feature/162's `OWN_HOLD`, `clipboard.rs`).
    crate::clipboard::mark_own_hold("ole");
    // SAFETY: called on this thread's STA with a live object; OLE takes
    // its own reference, and the local one is kept below so the object
    // outlives the placement.
    let placed = unsafe { OleSetClipboard(Some(&object)) };
    crate::clipboard::clear_own_hold();
    placed.map_err(|e| ClipboardError::Busy {
        reason: format!(
            "OleSetClipboard failed (clipboard held elsewhere?): {e}; {}",
            crate::clipboard::describe_clipboard_holder()
        ),
    })?;
    tracing::debug!(
        spool_entry = %file.entry,
        byte_count = file.byte_len,
        "offered a spooled file to the clipboard"
    );
    // Read after the placement: the number this offer is identified by is
    // the one the clipboard has now.
    // SAFETY: a bare read of a process-wide counter; no state is touched.
    let sequence = unsafe { GetClipboardSequenceNumber() };
    *current = Some(Placement { object, sequence });
    Ok(())
}

/// Empty the clipboard if our object is still on it.
fn clear_clipboard(current: &mut Option<Placement>) -> Result<(), ClipboardError> {
    let ours = is_ours(current.as_ref());
    *current = None;
    if !ours {
        // Someone else owns the clipboard now, which is the normal end of
        // an offer's life: nothing to withdraw.
        return Ok(());
    }
    // SAFETY: called on this thread's STA; `None` empties the clipboard
    // without rendering anything.
    unsafe { OleSetClipboard(None) }.map_err(|e| ClipboardError::Busy {
        reason: format!("clearing the clipboard failed: {e}"),
    })
}

fn is_ours(placement: Option<&Placement>) -> bool {
    let Some(placement) = placement else {
        return false;
    };
    // SAFETY: a bare read of a process-wide counter.
    if unsafe { GetClipboardSequenceNumber() } != placement.sequence {
        // Something changed the clipboard after we placed our object —
        // whoever it was, the item on offer is no longer ours.
        return false;
    }
    // SAFETY: called on this thread's STA with a live object reference.
    unsafe { OleIsCurrentClipboard(&placement.object) }.is_ok()
}

#[cfg(test)]
mod tests {
    use super::{NAME_UNITS, ZONE_IDENTIFIER, file_group_descriptor};
    use windows::Win32::UI::Shell::{FILEDESCRIPTORW, FILEGROUPDESCRIPTORW};

    /// The block a shell reads to learn what it is about to create. Built
    /// from a validated name and a verified length, and never from
    /// anything else.
    #[test]
    fn the_descriptor_carries_exactly_the_name_and_the_size() {
        let bytes = file_group_descriptor("quarterly report.pdf", 0x1_0000_0002).unwrap();
        assert_eq!(bytes.len(), std::mem::size_of::<FILEGROUPDESCRIPTORW>());

        // SAFETY: the block was built from this exact type in this
        // module; read unaligned because the struct is packed.
        let group = unsafe {
            bytes
                .as_ptr()
                .cast::<FILEGROUPDESCRIPTORW>()
                .read_unaligned()
        };
        // Every field is copied out before it is looked at: the struct is
        // packed, so a reference to one would be unaligned.
        let items = group.cItems;
        let descriptor = group.fgd[0];
        let (high, low) = (descriptor.nFileSizeHigh, descriptor.nFileSizeLow);
        assert_eq!(items, 1);
        assert_eq!(high, 1);
        assert_eq!(low, 2);

        let units = descriptor.cFileName;
        let end = units.iter().position(|unit| *unit == 0).unwrap();
        assert_eq!(
            String::from_utf16(&units[..end]).unwrap(),
            "quarterly report.pdf"
        );
        // Everything after the terminator is zero, so nothing of a
        // previous name (or of uninitialized memory) can trail it.
        assert!(units[end..].iter().all(|unit| *unit == 0));
    }

    /// A name that cannot fit the fixed Win32 field is refused, never
    /// truncated: a truncated name is a *different* name, and telling the
    /// user one thing while writing another is the confusion the whole
    /// validation chain exists to prevent.
    #[test]
    fn a_name_too_long_for_the_field_is_refused_rather_than_cut() {
        // The binding's own field, so a change to it fails here rather
        // than silently overrunning a fixed Win32 buffer.
        let field = FILEDESCRIPTORW::default().cFileName;
        assert_eq!(field.len(), NAME_UNITS);

        let longest = "a".repeat(NAME_UNITS - 1);
        assert!(file_group_descriptor(&longest, 1).is_ok());

        let too_long = "a".repeat(NAME_UNITS);
        assert!(file_group_descriptor(&too_long, 1).is_err());

        // Empty is not a name either.
        assert!(file_group_descriptor("", 1).is_err());
    }

    // ---- through the real clipboard, as a consumer sees it ----

    use std::io::Write as _;

    use crossover_platform::{
        ClipboardContent, ClipboardError, ClipboardProvider as _, SpoolStorage, VirtualFile,
        VirtualFileClipboard,
    };
    use windows::Win32::System::Com::{
        FORMATETC, IDataObject, IStream, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::{
        OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium,
    };
    use windows::Win32::UI::Shell::{CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW};

    use super::{
        AtomicBool, DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, DVASPECT_CONTENT,
        E_NOTIMPL, EXCLUDE_VALUE, Formats, S_OK, VirtualFileObject, WindowsVirtualFiles, register,
    };
    use crate::spool::WindowsSpoolStorage;
    use crate::test_support::{Sandbox, clipboard_lock};
    use std::sync::Arc;
    use windows::Win32::System::Com::DVASPECT_THUMBNAIL;
    use windows::core::w;

    /// OLE on the *consumer's* thread. Balanced on drop, so a test thread
    /// reused for the next test is left as it was found.
    struct Ole;

    impl Ole {
        fn init() -> Self {
            // SAFETY: initializing this thread's apartment; balanced in
            // Drop. An already-initialized thread returns a non-fatal
            // status, which is why the result is not asserted on.
            let _ = unsafe { OleInitialize(None) };
            Self
        }
    }

    impl Drop for Ole {
        fn drop(&mut self) {
            // SAFETY: balances the OleInitialize above.
            unsafe { OleUninitialize() };
        }
    }

    /// A spool holding one entry, and the offer that describes it.
    fn spooled(sandbox: &Sandbox, entry: &str, bytes: &[u8]) -> Arc<dyn SpoolStorage> {
        let spool = WindowsSpoolStorage::open_or_create(&sandbox.path("spool")).expect("spool");
        spool
            .create_entry(entry)
            .expect("create")
            .write_all(bytes)
            .expect("write");
        Arc::new(spool)
    }

    fn offer_of(entry: &str, file_name: &str, byte_len: u64) -> VirtualFile {
        VirtualFile {
            entry: entry.to_owned(),
            file_name: file_name.to_owned(),
            byte_len,
        }
    }

    fn request(format: u16, tymed: u32, lindex: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed,
        }
    }

    /// The error code a request is refused with. Written as a helper
    /// because `STGMEDIUM` has no `Debug`, so `expect_err` cannot be used
    /// on the success arm.
    fn refusal(object: &IDataObject, format: &FORMATETC) -> windows::core::HRESULT {
        // SAFETY: a well-formed request the object is expected to refuse;
        // any medium it did hand back is released before reporting.
        match unsafe { object.GetData(format) } {
            Ok(mut medium) => {
                // SAFETY: the medium came from GetData and is ours.
                unsafe { ReleaseStgMedium(&raw mut medium) };
                panic!("the object served a request it should have refused");
            }
            Err(error) => error.code(),
        }
    }

    /// What the shell has: the object currently on the clipboard.
    fn clipboard_object() -> IDataObject {
        // SAFETY: called on an OLE-initialized thread; the returned object
        // is a proxy into whichever apartment owns the clipboard.
        unsafe { OleGetClipboard() }.expect("OleGetClipboard")
    }

    /// The bytes behind an `HGLOBAL` medium, copied out before release.
    fn global_bytes(medium: &STGMEDIUM) -> Vec<u8> {
        // SAFETY: the medium is TYMED_HGLOBAL, so the union's hGlobal arm
        // is the live one; lock, copy the block, unlock.
        unsafe {
            let hglobal = medium.u.hGlobal;
            let ptr = GlobalLock(hglobal).cast::<u8>();
            assert!(!ptr.is_null(), "GlobalLock on a medium we were handed");
            let len = windows::Win32::System::Memory::GlobalSize(hglobal);
            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
            let _ = GlobalUnlock(hglobal);
            bytes
        }
    }

    /// Read a stream medium to the end, exactly as a paste target does.
    fn stream_bytes(stream: &IStream, expect: usize) -> Vec<u8> {
        let mut out = vec![0u8; expect + 16];
        let mut total = 0usize;
        loop {
            let mut read: u32 = 0;
            // SAFETY: `out` is a live buffer of the length passed, and
            // `read` is a live out-parameter.
            let hr = unsafe {
                stream.Read(
                    out.as_mut_ptr().add(total).cast(),
                    u32::try_from(out.len() - total).unwrap_or(0),
                    Some(&raw mut read),
                )
            };
            total += read as usize;
            if read == 0 || hr != S_OK {
                break;
            }
        }
        out.truncate(total);
        out
    }

    /// Offer with a little tolerance for the machine clipboard being held
    /// elsewhere, which every clipboard test here treats as routine.
    fn offer_with_retry(files: &WindowsVirtualFiles, file: &VirtualFile) {
        for _ in 0..20 {
            match files.offer(file) {
                Ok(()) => return,
                Err(ClipboardError::Busy { .. }) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => panic!("offering a virtual file: {error}"),
            }
        }
        panic!("the clipboard stayed busy for the whole retry schedule");
    }

    /// The whole point of the slice: what Crossover offers, a consumer can
    /// read — a name, a size, and the entry's bytes, produced only when
    /// asked for.
    #[test]
    fn an_offered_file_reads_back_as_a_name_a_size_and_its_bytes() {
        let _serial = clipboard_lock();
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file");
        let content: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let entry = "aaaaaaaa-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, &content);

        let files = WindowsVirtualFiles::new(spool).expect("apartment");
        offer_with_retry(
            &files,
            &offer_of(entry, "quarterly.pdf", content.len() as u64),
        );

        let object = clipboard_object();
        let descriptor_format = register(CFSTR_FILEDESCRIPTORW).unwrap();
        let contents_format = register(CFSTR_FILECONTENTS).unwrap();

        // The descriptor: the name the shell will create, and the size it
        // will report while doing it.
        let format = request(descriptor_format, TYMED_HGLOBAL.0 as u32, -1);
        // SAFETY: a well-formed request for a format the object serves.
        let mut medium = unsafe { object.GetData(&raw const format) }.expect("descriptor");
        let bytes = global_bytes(&medium);
        // SAFETY: the medium came from GetData and is ours to release.
        unsafe { ReleaseStgMedium(&raw mut medium) };

        // SAFETY: the block is a FILEGROUPDESCRIPTORW; read unaligned
        // because the struct is packed.
        let group = unsafe {
            bytes
                .as_ptr()
                .cast::<FILEGROUPDESCRIPTORW>()
                .read_unaligned()
        };
        let items = group.cItems;
        let described = group.fgd[0];
        let (high, low) = (described.nFileSizeHigh, described.nFileSizeLow);
        let units = described.cFileName;
        let end = units.iter().position(|unit| *unit == 0).unwrap();
        assert_eq!(items, 1);
        assert_eq!(String::from_utf16(&units[..end]).unwrap(), "quarterly.pdf");
        assert_eq!(
            (u64::from(high) << 32) | u64::from(low),
            content.len() as u64
        );

        // The contents: delayed until asked for, then exactly the entry.
        let format = request(contents_format, TYMED_ISTREAM.0 as u32, 0);
        // SAFETY: a well-formed request for index 0 as a stream.
        let mut medium = unsafe { object.GetData(&raw const format) }.expect("contents");
        // SAFETY: the medium is TYMED_ISTREAM, so the union's stream arm is
        // the live one; the clone is released with the medium below.
        let stream = unsafe { (*medium.u.pstm).clone() }.expect("a stream");
        assert_eq!(stream_bytes(&stream, content.len()), content);
        drop(stream);
        // SAFETY: as above.
        unsafe { ReleaseStgMedium(&raw mut medium) };
    }

    /// Build the object directly, without the clipboard between us and
    /// it.
    ///
    /// Deliberate: the OLE clipboard hands consumers a *mediating* object
    /// that answers out-of-enumeration requests itself — asking it for
    /// file contents as `HGLOBAL` returns `DV_E_FORMATETC` without our
    /// `GetData` ever running. That is fine in production (the request
    /// still fails, which is the point) but it means the specific codes
    /// ADR 0015 requires are only observable by calling the object.
    fn object_for(spool: Arc<dyn SpoolStorage>, entry: &str, name: &str, len: u64) -> IDataObject {
        VirtualFileObject {
            spool,
            entry: entry.to_owned(),
            descriptor: file_group_descriptor(name, len).expect("descriptor"),
            byte_len: len,
            formats: Formats::register().expect("formats"),
            rendering: Arc::new(AtomicBool::new(false)),
        }
        .into()
    }

    /// The exact `FORMATETC` set, and the typed refusal for everything
    /// else (ADR 0015). Each of these is a decision, not an omission.
    #[test]
    fn every_request_the_object_does_not_serve_is_refused_by_its_own_error() {
        let sandbox = Sandbox::new("virtual-file-refusals");
        let entry = "bbbbbbbb-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, b"a small document");
        let object = object_for(spool, entry, "doc.pdf", 16);
        let contents = register(CFSTR_FILECONTENTS).unwrap();

        // Contents as HGLOBAL: refused, because serving it would force the
        // whole item into one allocation on demand, reachable by any local
        // process — the exact cost delayed rendering exists to avoid.
        assert_eq!(
            refusal(&object, &request(contents, TYMED_HGLOBAL.0 as u32, 0)),
            DV_E_TYMED
        );

        // Any index but zero: one item, one stream.
        assert_eq!(
            refusal(&object, &request(contents, TYMED_ISTREAM.0 as u32, 1)),
            DV_E_LINDEX
        );

        // A format nobody here serves.
        let unknown = register(w!("CrossoverNotAFormat")).unwrap();
        assert_eq!(
            refusal(&object, &request(unknown, TYMED_HGLOBAL.0 as u32, -1)),
            DV_E_FORMATETC
        );

        // An aspect other than the content itself (a thumbnail, an icon).
        let mut aspect = request(contents, TYMED_ISTREAM.0 as u32, 0);
        aspect.dwAspect = DVASPECT_THUMBNAIL.0;
        assert_eq!(refusal(&object, &aspect), DV_E_DVASPECT);

        // Nothing may be pushed into the object: it is a promise about one
        // verified entry, with no state a caller may contribute to.
        let format = request(contents, TYMED_ISTREAM.0 as u32, 0);
        let medium = STGMEDIUM::default();
        // SAFETY: a well-formed SetData call the object is expected to
        // refuse; `frelease` is false, so the medium stays the caller's.
        let error = unsafe { object.SetData(&raw const format, &raw const medium, false) }
            .expect_err("SetData accepted data from a consumer");
        assert_eq!(error.code(), E_NOTIMPL);

        // And the stream it does serve is read-only, so a consumer cannot
        // reach back through the promise into the spool.
        // SAFETY: a well-formed request for the content stream.
        let mut medium = unsafe { object.GetData(&raw const format) }.expect("contents");
        // SAFETY: the medium is TYMED_ISTREAM, so the stream arm is live.
        let stream = unsafe { (*medium.u.pstm).clone() }.expect("a stream");
        let refused = "no".as_bytes();
        // SAFETY: a well-formed write the stream is expected to refuse.
        let hr = unsafe {
            stream.Write(
                refused.as_ptr().cast(),
                u32::try_from(refused.len()).unwrap(),
                None,
            )
        };
        assert!(hr.is_err(), "the content stream accepted a write");
        drop(stream);
        // SAFETY: the medium came from GetData and is ours to release.
        unsafe { ReleaseStgMedium(&raw mut medium) };
    }

    /// Both exclusion markers are advertised, so history and cloud sync
    /// have something to honour (F16). Whether they *do* honour it is the
    /// manual probe below — this is the half that is ours to get right.
    #[test]
    fn the_object_advertises_the_history_and_cloud_exclusions() {
        let _serial = clipboard_lock();
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file-exclusions");
        let entry = "cccccccc-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, b"a small document");

        let files = WindowsVirtualFiles::new(spool).expect("apartment");
        offer_with_retry(&files, &offer_of(entry, "doc.pdf", 16));

        let object = clipboard_object();
        for name in [
            w!("CanIncludeInClipboardHistory"),
            w!("ExcludeClipboardContentFromMonitorProcessing"),
        ] {
            let format = request(register(name).unwrap(), TYMED_HGLOBAL.0 as u32, -1);
            // SAFETY: a well-formed query for a format the object serves.
            assert_eq!(unsafe { object.QueryGetData(&raw const format) }, S_OK);
            // SAFETY: as above.
            let mut medium = unsafe { object.GetData(&raw const format) }.expect("exclusion");
            let bytes = global_bytes(&medium);
            assert_eq!(&bytes[..4], &EXCLUDE_VALUE);
            // SAFETY: the medium came from GetData and is ours to release.
            unsafe { ReleaseStgMedium(&raw mut medium) };
        }
    }

    /// `GetData` is callable by any local process, so an unbounded number
    /// of concurrent renders would be an unprivileged denial of service:
    /// unbounded reads and an occupied apartment thread. One at a time,
    /// refused rather than queued (F14).
    #[test]
    fn a_second_render_is_refused_while_the_first_is_outstanding() {
        let _serial = clipboard_lock();
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file-renders");
        let entry = "dddddddd-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, b"a small document");

        let files = WindowsVirtualFiles::new(spool).expect("apartment");
        offer_with_retry(&files, &offer_of(entry, "doc.pdf", 16));

        let object = clipboard_object();
        let format = request(
            register(CFSTR_FILECONTENTS).unwrap(),
            TYMED_ISTREAM.0 as u32,
            0,
        );

        // SAFETY: a well-formed request for the content stream.
        let mut first = unsafe { object.GetData(&raw const format) }.expect("first render");
        // SAFETY: as above — while the first stream is still outstanding.
        let refused = unsafe { object.GetData(&raw const format) };
        assert!(
            refused.is_err(),
            "a second render was served while the first was still open"
        );

        // Releasing the first frees the slot: a paste that follows another
        // paste must still work, which is the whole reason the bound is
        // tied to the stream's life rather than to the call.
        // SAFETY: the medium came from GetData and is ours to release.
        unsafe { ReleaseStgMedium(&raw mut first) };
        // SAFETY: as above.
        let mut again = unsafe { object.GetData(&raw const format) }.expect("render after release");
        // SAFETY: as above.
        unsafe { ReleaseStgMedium(&raw mut again) };
    }

    /// The cheap, exact half of loop prevention (F13), and its
    /// self-limiting property: our own placement is recognized, and an
    /// ordinary local copy by any other application ends that recognition.
    #[test]
    fn our_own_offer_is_recognized_until_something_else_copies() {
        let _serial = clipboard_lock();
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file-ownership");
        let entry = "eeeeeeee-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, b"a small document");

        let files = WindowsVirtualFiles::new(spool).expect("apartment");
        assert!(!files.is_current(), "nothing has been offered yet");

        offer_with_retry(&files, &offer_of(entry, "doc.pdf", 16));
        assert!(files.is_current(), "our own object is on the clipboard");

        // Somebody else copies. Ordinary sync must resume, which is
        // exactly what "no longer ours" means here.
        let clipboard = crate::clipboard::WindowsClipboard::new().expect("clipboard");
        for attempt in 0..20 {
            match clipboard.write_text("an ordinary local copy") {
                Ok(()) => break,
                Err(ClipboardError::Busy { .. }) if attempt < 19 => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => panic!("writing text: {error}"),
            }
        }
        assert!(
            !files.is_current(),
            "our object was still reported as current after another write"
        );

        // Withdrawing something we no longer own is a success: the
        // postcondition already holds.
        files.withdraw().expect("withdraw");
    }

    /// Layer 2 of ADR 0015's loop prevention (feature/133), made concrete:
    /// the object we place advertises `CFSTR_FILEDESCRIPTORW` and
    /// `CFSTR_FILECONTENTS`, never `CF_HDROP`, so a plain clipboard read
    /// finds nothing to report as a file-list observation while it is
    /// current — independent of the ownership check
    /// `crossover_core::clipboard_driver` performs before a read is ever
    /// attempted at all (layer 1, proved above by
    /// `our_own_offer_is_recognized_until_something_else_copies`).
    #[test]
    fn our_own_offer_never_reads_back_as_a_file_list_observation() {
        let _serial = clipboard_lock();
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file-no-hdrop");
        let entry = "77777777-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, b"a small document");

        let files = WindowsVirtualFiles::new(spool).expect("apartment");
        offer_with_retry(&files, &offer_of(entry, "doc.pdf", 16));
        assert!(files.is_current());

        let clipboard = crate::clipboard::WindowsClipboard::new().expect("clipboard");
        let mut content = None;
        for attempt in 0..20 {
            match clipboard.read() {
                Ok(read) => {
                    content = read;
                    break;
                }
                Err(ClipboardError::Busy { .. }) if attempt < 19 => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => panic!("reading the clipboard: {error}"),
            }
        }
        assert!(
            !matches!(content, Some(ClipboardContent::FileList(_))),
            "our own virtual file object read back as a file-list observation: {content:?}"
        );
    }

    /// Withdrawal empties the clipboard, so a promise is never left
    /// advertised once the entry behind it is going away.
    #[test]
    fn withdrawing_takes_the_offer_off_the_clipboard() {
        let _serial = clipboard_lock();
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file-withdraw");
        let entry = "ffffffff-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, b"a small document");

        let files = WindowsVirtualFiles::new(spool).expect("apartment");
        offer_with_retry(&files, &offer_of(entry, "doc.pdf", 16));
        assert!(files.is_current());

        files.withdraw().expect("withdraw");
        assert!(!files.is_current(), "the offer survived its withdrawal");
    }

    /// An entry that is gone fails the render *observably* rather than
    /// falling back to anything — there is nothing to fall back to, and a
    /// paste that silently produced something else would be far worse.
    #[test]
    fn a_render_for_a_missing_entry_fails_rather_than_improvising() {
        let _serial = clipboard_lock();
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file-missing");
        let entry = "99999999-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, b"a small document");

        let files = WindowsVirtualFiles::new(Arc::clone(&spool)).expect("apartment");
        offer_with_retry(&files, &offer_of(entry, "doc.pdf", 16));

        // The entry is evicted while the offer is still advertised — the
        // race the lifetime rule exists to close, and which must fail
        // cleanly until it does.
        spool.unlink_entry(entry).expect("unlink");

        let object = clipboard_object();
        let format = request(
            register(CFSTR_FILECONTENTS).unwrap(),
            TYMED_ISTREAM.0 as u32,
            0,
        );
        // SAFETY: a well-formed request whose entry no longer exists.
        assert!(unsafe { object.GetData(&raw const format) }.is_err());
    }

    /// The behavioural half of F16, which no automated test can reach: it
    /// asks the *system* whether it honours the markers, and only a human
    /// with Win+V and a Microsoft account can see the answer.
    ///
    /// Run it, then check both things it prints.
    #[test]
    #[ignore = "manual: needs a human to open Win+V and check cloud sync"]
    fn manual_the_offer_stays_out_of_clipboard_history_and_cloud_sync() {
        let _ole = Ole::init();
        let sandbox = Sandbox::new("virtual-file-manual");
        let content = b"crossover manual probe: this should never appear in Win+V".to_vec();
        let entry = "12345678-0000-1111-2222-333333333333.bin";
        let spool = spooled(&sandbox, entry, &content);

        let files = WindowsVirtualFiles::new(spool).expect("apartment");
        offer_with_retry(
            &files,
            &offer_of(entry, "crossover-probe.txt", content.len() as u64),
        );

        println!();
        println!("A virtual file is now on the clipboard. Check, in this order:");
        println!("  1. Press Win+V. `crossover-probe.txt` must NOT be listed.");
        println!("  2. Paste (Ctrl+V) into a folder. The file must appear with");
        println!("     its content, and must open without a SmartScreen or");
        println!("     Protected View prompt — but `Get-Content -Stream");
        println!("     Zone.Identifier <file>` must show ZoneId=1.");
        println!("  3. On a second machine signed into the same Microsoft");
        println!("     account with clipboard sync on, Win+V must not show it.");
        println!();
        println!("Press Enter when done.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    /// The zone the shell stamps into `Zone.Identifier`: intranet, which
    /// is where the file came from. Asserted as an exact string because it
    /// is a decision (see the constant), not a detail — a change to it
    /// changes whether every pasted document opens in Protected View.
    #[test]
    fn a_pasted_file_is_marked_as_coming_from_the_local_intranet() {
        let text = std::str::from_utf8(ZONE_IDENTIFIER).unwrap();
        assert!(text.starts_with("[ZoneTransfer]"));
        assert!(text.contains("ZoneId=1"), "{text}");
        assert!(
            !text.contains("ZoneId=3"),
            "the internet zone was restored: {text}"
        );
    }
}
