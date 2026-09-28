//! `NSPasteboard`-backed [`ClipboardProvider`] (Phase 9.1; ADR 0020).
//!
//! **Text only, in this slice.** A clipboard holding anything else — an
//! image, a file list, an application's private type — reads as
//! [`ClipboardRead::Unreadable`]: the user's copy, protected from a waiting
//! peer install, and not yet something this build sends (ADR 0005,
//! addendum 2026-09-28). Images follow with ADR 0016's format negotiation,
//! files with their own design.
//!
//! **No change notification exists** (platform-risks-macos.md M-4), so a
//! thread polls the pasteboard's `changeCount` — a counter, not content —
//! and raises the listener when it moves. Writes made through this provider
//! move it too, which the trait says consumers must expect (FR-3.3).
//!
//! **Reading the general pasteboard is a privacy-gated act on current
//! macOS** (M-11). By default the system asks the user the first time an
//! application reads it programmatically; afterwards the user can set the
//! application to always allow, always deny, or ask, in System Settings.
//! Only *content* is gated — the change counter is not — so the poller
//! never prompts, and a read happens only after something changed. A
//! pasteboard set to always deny answers reads as if it were empty, which
//! would read here as an empty clipboard and sync nothing, silently; so the
//! access behaviour is checked first and a denial is an error naming the
//! setting, never `Empty`.
//!
//! The pasteboard is addressed by **name** and looked up per call rather
//! than held: a name is `Send + Sync`, an `NSPasteboard` handle is not, and
//! the lookup is a cheap dictionary hit. Production uses the general
//! pasteboard; tests use a uniquely named private one, which the system
//! never gates, so no test can stall on a permission prompt.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crossover_platform::{
    ClipboardContent, ClipboardError, ClipboardListener, ClipboardProvider, ClipboardRead,
};
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSPasteboard, NSPasteboardAccessBehavior, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// How often the change counter is sampled. Well inside the engine's
/// 300 ms settle window (ADR 0006), so polling adds at most one interval
/// to the latency a user sees, and a counter read is cheap enough that a
/// fifth of a second costs nothing measurable.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Which pasteboard, by name, so the provider stays `Send + Sync`.
#[derive(Debug, Clone)]
enum Board {
    /// The system-wide pasteboard the user copies to and pastes from.
    General,
    /// A private pasteboard by name — tests.
    Named(String),
}

impl Board {
    /// Look the pasteboard up. Must run inside an autorelease pool.
    fn open(&self) -> Retained<NSPasteboard> {
        match self {
            Self::General => NSPasteboard::generalPasteboard(),
            Self::Named(name) => NSPasteboard::pasteboardWithName(&NSString::from_str(name)),
        }
    }
}

/// The macOS clipboard, with a polling change listener.
pub struct MacClipboard {
    board: Board,
    listener: Arc<Mutex<Option<ClipboardListener>>>,
    stop: Arc<AtomicBool>,
    poller: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for MacClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacClipboard")
            .field("board", &self.board)
            .finish_non_exhaustive()
    }
}

impl MacClipboard {
    /// The general pasteboard, observed from now on.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Unavailable`] if the polling thread cannot start.
    pub fn new() -> Result<Self, ClipboardError> {
        Self::observing(Board::General)
    }

    /// A private pasteboard by name, observed from now on — tests, which
    /// must never touch the user's clipboard or its permission prompt.
    ///
    /// # Errors
    ///
    /// As [`MacClipboard::new`].
    pub fn with_pasteboard_name(name: impl Into<String>) -> Result<Self, ClipboardError> {
        Self::observing(Board::Named(name.into()))
    }

    fn observing(board: Board) -> Result<Self, ClipboardError> {
        let listener: Arc<Mutex<Option<ClipboardListener>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        // The baseline is taken here, before the thread exists, not by the
        // thread when it is first scheduled: a change landing between
        // construction and that first read would otherwise become the
        // baseline and never be reported.
        let baseline = autoreleasepool(|_| board.open().changeCount());
        let poller = {
            let board = board.clone();
            let listener = Arc::clone(&listener);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("crossover-pasteboard-poll".to_owned())
                .spawn(move || poll(&board, baseline, &listener, &stop))
                .map_err(|error| ClipboardError::Unavailable {
                    reason: format!("starting the pasteboard poller: {error}"),
                })?
        };
        Ok(Self {
            board,
            listener,
            stop,
            poller: Some(poller),
        })
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(poller) = self.poller.take() {
            // Bounded by one poll interval: the thread checks the flag
            // every time it wakes.
            let _ = poller.join();
        }
    }
}

/// Sample the change counter until told to stop, raising the listener each
/// time it moves.
fn poll(
    board: &Board,
    baseline: isize,
    listener: &Mutex<Option<ClipboardListener>>,
    stop: &AtomicBool,
) {
    let mut last = baseline;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL_INTERVAL);
        let now = autoreleasepool(|_| board.open().changeCount());
        if now != last {
            last = now;
            // Called with the lock held, which the trait allows: the
            // listener must return quickly and never block.
            if let Some(notify) = listener
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                notify();
            }
        }
    }
}

/// Refuse to read a pasteboard the user has told the system to deny —
/// before the read, because a denied read looks exactly like an empty one.
fn check_access(board: &NSPasteboard) -> Result<(), ClipboardError> {
    if board.accessBehavior() == NSPasteboardAccessBehavior::AlwaysDeny {
        return Err(ClipboardError::Unavailable {
            reason: "macOS is set to deny this app access to the clipboard; allow it in \
                     System Settings > Privacy & Security > Paste from Other Apps \
                     (docs/platform-risks-macos.md M-11)"
                .to_owned(),
        });
    }
    Ok(())
}

impl ClipboardProvider for MacClipboard {
    fn read(&self) -> Result<ClipboardRead, ClipboardError> {
        autoreleasepool(|_| {
            let board = self.board.open();
            check_access(&board)?;
            let holds_anything = board.types().is_some_and(|types| types.count() > 0);
            if !holds_anything {
                return Ok(ClipboardRead::Empty);
            }
            // SAFETY: an immutable AppKit constant, valid for the process.
            let string_type = unsafe { NSPasteboardTypeString };
            Ok(match board.stringForType(string_type) {
                Some(text) => ClipboardRead::Content(ClipboardContent::Text(text.to_string())),
                // Something is there, and it is not text this slice sends:
                // the user's copy all the same.
                None => ClipboardRead::Unreadable,
            })
        })
    }

    fn write(&self, content: &ClipboardContent) -> Result<(), ClipboardError> {
        let ClipboardContent::Text(text) = content else {
            return Err(ClipboardError::Unsupported {
                reason: "this build installs text only on macOS; images arrive with \
                         ADR 0016's format negotiation, files with their own design"
                    .to_owned(),
            });
        };
        autoreleasepool(|_| {
            let board = self.board.open();
            board.clearContents();
            // SAFETY: an immutable AppKit constant, valid for the process.
            let string_type = unsafe { NSPasteboardTypeString };
            if board.setString_forType(&NSString::from_str(text), string_type) {
                Ok(())
            } else {
                Err(ClipboardError::Unavailable {
                    reason: "the pasteboard refused the text".to_owned(),
                })
            }
        })
    }

    fn set_change_listener(
        &self,
        listener: Option<ClipboardListener>,
    ) -> Result<(), ClipboardError> {
        *self.listener.lock().unwrap_or_else(PoisonError::into_inner) = listener;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use crossover_platform::{ClipboardContent, ClipboardProvider, ClipboardRead};
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSData, NSString};

    use super::{MacClipboard, POLL_INTERVAL};

    /// A private pasteboard of this test's own, cleared on drop. Never the
    /// general one: the user's clipboard is not a test fixture, and its
    /// access prompt would stall CI.
    struct Scratch {
        name: String,
    }

    impl Scratch {
        fn new() -> Self {
            let name =
                autoreleasepool(|_| NSPasteboard::pasteboardWithUniqueName().name().to_string());
            Self { name }
        }

        fn clipboard(&self) -> MacClipboard {
            MacClipboard::with_pasteboard_name(self.name.clone()).unwrap()
        }

        fn with_board<T>(&self, f: impl FnOnce(&NSPasteboard) -> T) -> T {
            autoreleasepool(|_| {
                f(&NSPasteboard::pasteboardWithName(&NSString::from_str(
                    &self.name,
                )))
            })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            self.with_board(|board| {
                board.clearContents();
            });
        }
    }

    #[test]
    fn text_round_trips_verbatim() {
        let scratch = Scratch::new();
        let clipboard = scratch.clipboard();
        let text = "line one\nline two — ünïcödé ✓ \u{1F600}";
        clipboard
            .write(&ClipboardContent::Text(text.to_owned()))
            .unwrap();
        assert_eq!(
            clipboard.read().unwrap(),
            ClipboardRead::Content(ClipboardContent::Text(text.to_owned()))
        );
        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some(text));
    }

    #[test]
    fn a_cleared_pasteboard_reads_as_empty() {
        let scratch = Scratch::new();
        scratch.with_board(|board| {
            board.clearContents();
        });
        assert_eq!(scratch.clipboard().read().unwrap(), ClipboardRead::Empty);
    }

    /// The residual ADR 0005's 2026-09-28 addendum closed, on this
    /// platform: a copy in a type this build does not send is the user's
    /// content, never an empty clipboard.
    #[test]
    fn a_private_type_only_copy_reads_as_unreadable() {
        let scratch = Scratch::new();
        scratch.with_board(|board| {
            board.clearContents();
            assert!(board.setData_forType(
                Some(&NSData::with_bytes(b"application-private bytes")),
                &NSString::from_str("com.crossover.test.private"),
            ));
        });
        assert_eq!(
            scratch.clipboard().read().unwrap(),
            ClipboardRead::Unreadable
        );
    }

    #[test]
    fn non_text_content_is_refused_permanently_on_write() {
        let scratch = Scratch::new();
        let refusal = scratch
            .clipboard()
            .write(&ClipboardContent::FileList(Vec::new()));
        assert!(
            matches!(
                refusal,
                Err(crossover_platform::ClipboardError::Unsupported { .. })
            ),
            "expected a permanent refusal, got {refusal:?}"
        );
    }

    /// The listener fires for a change made by someone else, and for our
    /// own write — the contract term loop prevention is built on.
    #[test]
    fn a_change_raises_the_listener_within_a_few_polls() {
        let scratch = Scratch::new();
        let clipboard = scratch.clipboard();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        clipboard
            .set_change_listener(Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap();

        let wait_for = |count: usize| {
            let deadline = Instant::now() + POLL_INTERVAL * 20;
            while calls.load(Ordering::SeqCst) < count && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            calls.load(Ordering::SeqCst)
        };

        // Someone else writes.
        scratch.with_board(|board| {
            board.clearContents();
            board.setString_forType(
                &NSString::from_str("from elsewhere"),
                &NSString::from_str("public.utf8-plain-text"),
            );
        });
        assert!(wait_for(1) >= 1, "an external change was never noticed");

        // We write.
        let before = calls.load(Ordering::SeqCst);
        clipboard
            .write(&ClipboardContent::Text("ours".to_owned()))
            .unwrap();
        assert!(
            wait_for(before + 1) > before,
            "our own write was never noticed"
        );
    }

    /// Dropping the provider stops its poller promptly rather than leaving
    /// a thread behind.
    #[test]
    fn dropping_the_provider_stops_the_poller() {
        let scratch = Scratch::new();
        let started = Instant::now();
        drop(scratch.clipboard());
        assert!(started.elapsed() < POLL_INTERVAL * 5);
    }
}
