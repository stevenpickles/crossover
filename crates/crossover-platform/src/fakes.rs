//! In-memory fakes of the platform traits.
//!
//! docs/ARCHITECTURE.md §4: every platform trait has a scriptable in-memory
//! fake so all core logic is exercisable with no OS interaction. Enabled via
//! the `fakes` feature (dev-dependencies of consuming crates) and for this
//! crate's own tests.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::clipboard::{ClipboardError, ClipboardListener, ClipboardProvider};
use crate::display::{CursorPoint, DisplayError, DisplayInfo, Screen};
use crate::input::{
    InputCapture, InputError, InputEvent, InputInjector, InputSink, KeyEvent, PointerEvent,
};
use crate::secure_storage::{SecureStorage, SecureStorageError};

/// In-memory [`SecureStorage`] with scriptable fault injection.
#[derive(Debug, Default)]
pub struct InMemorySecureStorage {
    entries: Mutex<HashMap<String, Vec<u8>>>,
    /// When set, the next operation fails with this reason (then clears).
    fail_next: Mutex<Option<String>>,
}

impl InMemorySecureStorage {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the next `store`/`load`/`delete` call fail with `reason`.
    ///
    /// Supports fault-injection tests (docs/TESTING.md §1.5) without a
    /// bespoke failing mock per test.
    pub fn fail_next_operation(&self, reason: &str) {
        *lock(&self.fail_next) = Some(reason.to_owned());
    }

    fn take_injected_failure(&self) -> Result<(), SecureStorageError> {
        match lock(&self.fail_next).take() {
            Some(reason) => Err(SecureStorageError::Backend { reason }),
            None => Ok(()),
        }
    }
}

impl SecureStorage for InMemorySecureStorage {
    fn store(&self, key: &str, secret: &[u8]) -> Result<(), SecureStorageError> {
        self.take_injected_failure()?;
        lock(&self.entries).insert(key.to_owned(), secret.to_vec());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
        self.take_injected_failure()?;
        Ok(lock(&self.entries).get(key).cloned())
    }

    fn delete(&self, key: &str) -> Result<(), SecureStorageError> {
        self.take_injected_failure()?;
        lock(&self.entries).remove(key);
        Ok(())
    }
}

/// Which fake-clipboard operation an injected failure applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOp {
    /// Fail upcoming `read_text` calls.
    Read,
    /// Fail upcoming `write_text` calls.
    Write,
}

/// The kind of failure to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFailure {
    /// Transient contention (the R-5 scenario the engine retries).
    Busy,
    /// Permanent failure (never retried).
    Unavailable,
}

#[derive(Default)]
struct ClipboardState {
    content: Option<String>,
    listener: Option<ClipboardListener>,
    fail_reads: (usize, Option<ClipboardFailure>),
    fail_writes: (usize, Option<ClipboardFailure>),
}

/// In-memory [`ClipboardProvider`] with scriptable contention.
///
/// Mirrors the documented contract, including the part that matters most
/// for loop prevention: `write_text` triggers the change listener, just
/// as the Windows clipboard notifies for programmatic writes.
#[derive(Default)]
pub struct InMemoryClipboard {
    state: Mutex<ClipboardState>,
}

impl InMemoryClipboard {
    /// An empty clipboard.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a local user copy: set content and notify the listener,
    /// as the OS would for a change made by another application.
    pub fn set_text_locally(&self, text: &str) {
        let listener = {
            let mut state = lock(&self.state);
            state.content = Some(text.to_owned());
            state.listener.take()
        };
        self.notify_and_restore(listener);
    }

    /// Make the next `count` operations of `op` fail with `kind`, then
    /// succeed again — the shape of every bounded-retry scenario
    /// (docs/TESTING.md §1.5).
    pub fn fail_next(&self, op: ClipboardOp, kind: ClipboardFailure, count: usize) {
        let mut state = lock(&self.state);
        match op {
            ClipboardOp::Read => state.fail_reads = (count, Some(kind)),
            ClipboardOp::Write => state.fail_writes = (count, Some(kind)),
        }
    }

    /// Current content, bypassing failure injection (test assertions).
    #[must_use]
    pub fn peek(&self) -> Option<String> {
        lock(&self.state).content.clone()
    }

    fn notify_and_restore(&self, listener: Option<ClipboardListener>) {
        // Invoke outside the lock (the real provider notifies from a
        // separate thread with no lock held), then restore.
        if let Some(listener) = listener {
            listener();
            let mut state = lock(&self.state);
            if state.listener.is_none() {
                state.listener = Some(listener);
            }
        }
    }

    fn take_failure(slot: &mut (usize, Option<ClipboardFailure>)) -> Option<ClipboardFailure> {
        if slot.0 > 0 {
            slot.0 -= 1;
            let kind = slot.1;
            if slot.0 == 0 {
                slot.1 = None;
            }
            kind
        } else {
            None
        }
    }

    fn failure_error(kind: ClipboardFailure) -> ClipboardError {
        match kind {
            ClipboardFailure::Busy => ClipboardError::Busy {
                reason: "injected contention".to_owned(),
            },
            ClipboardFailure::Unavailable => ClipboardError::Unavailable {
                reason: "injected failure".to_owned(),
            },
        }
    }
}

impl ClipboardProvider for InMemoryClipboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        let mut state = lock(&self.state);
        if let Some(kind) = Self::take_failure(&mut state.fail_reads) {
            return Err(Self::failure_error(kind));
        }
        Ok(state.content.clone())
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        let listener = {
            let mut state = lock(&self.state);
            if let Some(kind) = Self::take_failure(&mut state.fail_writes) {
                return Err(Self::failure_error(kind));
            }
            state.content = Some(text.to_owned());
            state.listener.take()
        };
        // Contract term under test everywhere: our own writes notify too.
        self.notify_and_restore(listener);
        Ok(())
    }

    fn set_change_listener(
        &self,
        listener: Option<ClipboardListener>,
    ) -> Result<(), ClipboardError> {
        lock(&self.state).listener = listener;
        Ok(())
    }
}

/// In-memory [`InputCapture`], driven by the test rather than a mouse.
///
/// The real implementation suppresses local input as a side effect of
/// capturing; there is nothing to suppress here, so what this fake
/// models is the *contract*: what the sink receives, when capture is
/// considered healthy, and how loss is reported.
#[derive(Default)]
pub struct FakeInputCapture {
    sink: Mutex<Option<InputSink>>,
    capturing: Mutex<bool>,
    /// Simulates the platform losing capture without telling us — the
    /// Windows hook-timeout behaviour (R-2) that `is_capturing` exists
    /// to expose.
    silently_lost: Mutex<bool>,
    fail_next_start: Mutex<Option<String>>,
    /// Simulates the user pressing the release escape gesture (both
    /// Control keys on Windows); read-and-cleared by `escape_requested`.
    escape: Mutex<bool>,
}

impl FakeInputCapture {
    /// Not capturing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Deliver a pointer event as though the user had produced it.
    ///
    /// Events raised while not capturing are dropped, exactly as the
    /// real implementation would never see them.
    pub fn raise(&self, event: PointerEvent) {
        self.deliver(InputEvent::Pointer(event));
    }

    /// Deliver a keyboard event as though the user had produced it.
    pub fn raise_key(&self, event: KeyEvent) {
        self.deliver(InputEvent::Key(event));
    }

    fn deliver(&self, event: InputEvent) {
        if !*lock(&self.capturing) || *lock(&self.silently_lost) {
            return;
        }
        let sink = lock(&self.sink).take();
        if let Some(sink) = sink {
            sink(event);
            let mut slot = lock(&self.sink);
            if slot.is_none() {
                *slot = Some(sink);
            }
        }
    }

    /// Simulate the platform dropping capture without notice: events
    /// stop arriving and `is_capturing` reports the truth.
    pub fn lose_capture_silently(&self) {
        *lock(&self.silently_lost) = true;
    }

    /// Make the next `start_capture` fail.
    pub fn fail_next_start(&self, reason: &str) {
        *lock(&self.fail_next_start) = Some(reason.to_owned());
    }

    /// Simulate the user's release escape gesture; the next
    /// `escape_requested` returns true (once).
    pub fn request_escape(&self) {
        *lock(&self.escape) = true;
    }
}

impl InputCapture for FakeInputCapture {
    fn start_capture(&self, sink: InputSink) -> Result<(), InputError> {
        if let Some(reason) = lock(&self.fail_next_start).take() {
            return Err(InputError::CaptureUnavailable { reason });
        }
        *lock(&self.sink) = Some(sink);
        *lock(&self.capturing) = true;
        *lock(&self.silently_lost) = false;
        Ok(())
    }

    fn stop_capture(&self) -> Result<(), InputError> {
        *lock(&self.capturing) = false;
        *lock(&self.sink) = None;
        *lock(&self.silently_lost) = false;
        Ok(())
    }

    fn is_capturing(&self) -> bool {
        *lock(&self.capturing) && !*lock(&self.silently_lost)
    }

    fn escape_requested(&self) -> bool {
        let mut escape = lock(&self.escape);
        std::mem::replace(&mut escape, false)
    }
}

/// In-memory [`InputInjector`] that records what it was asked to replay.
#[derive(Default)]
pub struct FakeInputInjector {
    injected: Mutex<Vec<InputEvent>>,
    fail_next: Mutex<Option<String>>,
}

impl FakeInputInjector {
    /// Nothing injected yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything injected so far, in order.
    #[must_use]
    pub fn injected(&self) -> Vec<InputEvent> {
        lock(&self.injected).clone()
    }

    /// Only the pointer events injected, in order — a convenience for the
    /// many tests that predate the keyboard and reason in pointer terms.
    #[must_use]
    pub fn injected_pointers(&self) -> Vec<PointerEvent> {
        lock(&self.injected)
            .iter()
            .filter_map(|event| match event {
                InputEvent::Pointer(pointer) => Some(*pointer),
                InputEvent::Key(_) => None,
            })
            .collect()
    }

    /// Forget the record.
    pub fn clear(&self) {
        lock(&self.injected).clear();
    }

    /// Make the next `inject` fail.
    pub fn fail_next(&self, reason: &str) {
        *lock(&self.fail_next) = Some(reason.to_owned());
    }
}

impl InputInjector for FakeInputInjector {
    fn inject(&self, events: &[InputEvent]) -> Result<(), InputError> {
        if let Some(reason) = lock(&self.fail_next).take() {
            return Err(InputError::InjectionFailed { reason });
        }
        lock(&self.injected).extend_from_slice(events);
        Ok(())
    }
}

/// In-memory [`DisplayInfo`] with a scriptable screen size and cursor, so
/// edge-detection logic is exercisable with no real display.
pub struct FakeDisplay {
    screen: Mutex<Screen>,
    cursor: Mutex<CursorPoint>,
    /// When set, both queries fail with this reason — the platform
    /// refusing to report geometry.
    fail: Mutex<Option<String>>,
}

impl FakeDisplay {
    /// A display of `screen`, cursor parked at the top-left.
    #[must_use]
    pub fn new(screen: Screen) -> Self {
        Self {
            screen: Mutex::new(screen),
            cursor: Mutex::new(CursorPoint { x: 0, y: 0 }),
            fail: Mutex::new(None),
        }
    }

    /// Move the fake cursor.
    pub fn set_cursor(&self, cursor: CursorPoint) {
        *lock(&self.cursor) = cursor;
    }

    /// Change the fake screen size.
    pub fn set_screen(&self, screen: Screen) {
        *lock(&self.screen) = screen;
    }

    /// Make both queries fail until cleared, simulating a platform that
    /// cannot report geometry.
    pub fn fail_with(&self, reason: &str) {
        *lock(&self.fail) = Some(reason.to_owned());
    }

    fn guard(&self) -> Result<(), DisplayError> {
        match lock(&self.fail).clone() {
            Some(reason) => Err(DisplayError::Unavailable { reason }),
            None => Ok(()),
        }
    }
}

impl DisplayInfo for FakeDisplay {
    fn primary_screen(&self) -> Result<Screen, DisplayError> {
        self.guard()?;
        Ok(*lock(&self.screen))
    }

    fn cursor_position(&self) -> Result<CursorPoint, DisplayError> {
        self.guard()?;
        Ok(*lock(&self.cursor))
    }
}

/// Locks a mutex, recovering from poisoning: a panicked test thread must
/// not cascade opaque failures into unrelated tests.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod input_tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use super::{FakeInputCapture, FakeInputInjector};
    use crate::input::{
        InputCapture, InputError, InputEvent, InputInjector, KeyEvent, PointerButton, PointerEvent,
        hid,
    };

    fn collecting_sink() -> (Arc<StdMutex<Vec<InputEvent>>>, crate::input::InputSink) {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        (
            seen,
            Box::new(move |event| {
                sink_seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event);
            }),
        )
    }

    #[test]
    fn events_reach_the_sink_only_while_capturing() {
        let capture = FakeInputCapture::new();
        let (seen, sink) = collecting_sink();

        // Before starting: nothing is delivered.
        capture.raise(PointerEvent::Motion { dx: 1, dy: 1 });
        assert!(seen.lock().unwrap().is_empty());

        capture.start_capture(sink).unwrap();
        assert!(capture.is_capturing());
        capture.raise(PointerEvent::Motion { dx: 3, dy: 4 });
        capture.raise(PointerEvent::Button {
            button: PointerButton::Left,
            pressed: true,
        });
        assert_eq!(seen.lock().unwrap().len(), 2);

        capture.stop_capture().unwrap();
        assert!(!capture.is_capturing());
        capture.raise(PointerEvent::Motion { dx: 9, dy: 9 });
        assert_eq!(seen.lock().unwrap().len(), 2, "delivered after stopping");
    }

    #[test]
    fn pointer_and_key_events_share_the_sink_stream() {
        let capture = FakeInputCapture::new();
        let (seen, sink) = collecting_sink();
        capture.start_capture(sink).unwrap();

        capture.raise(PointerEvent::Motion { dx: 1, dy: 0 });
        capture.raise_key(KeyEvent::press(hid::LEFT_SHIFT));
        capture.raise(PointerEvent::Button {
            button: PointerButton::Left,
            pressed: true,
        });

        // Both kinds arrive, interleaved, in the one ordered stream.
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                InputEvent::Pointer(PointerEvent::Motion { dx: 1, dy: 0 }),
                InputEvent::Key(KeyEvent::press(hid::LEFT_SHIFT)),
                InputEvent::Pointer(PointerEvent::Button {
                    button: PointerButton::Left,
                    pressed: true,
                }),
            ]
        );
    }

    /// The R-2 scenario: Windows removes an overrunning hook silently.
    /// `is_capturing` must report the truth so callers can fail closed.
    #[test]
    fn silent_capture_loss_is_visible_and_stops_delivery() {
        let capture = FakeInputCapture::new();
        let (seen, sink) = collecting_sink();
        capture.start_capture(sink).unwrap();

        capture.lose_capture_silently();
        assert!(
            !capture.is_capturing(),
            "loss must not be reported as healthy"
        );
        capture.raise(PointerEvent::Motion { dx: 1, dy: 1 });
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_capture_is_idempotent_and_start_failure_surfaces() {
        let capture = FakeInputCapture::new();
        // Safe on the error paths callers reach for it from.
        capture.stop_capture().unwrap();
        capture.stop_capture().unwrap();

        capture.fail_next_start("hook rejected");
        let (_seen, sink) = collecting_sink();
        assert!(matches!(
            capture.start_capture(sink),
            Err(InputError::CaptureUnavailable { .. })
        ));
        assert!(!capture.is_capturing());
    }

    #[test]
    fn injector_records_order_and_reports_failure() {
        let injector = FakeInputInjector::new();
        let events = [
            InputEvent::Pointer(PointerEvent::Motion { dx: 5, dy: 0 }),
            InputEvent::Key(KeyEvent::press(hid::LEFT_SHIFT)),
            InputEvent::Pointer(PointerEvent::Button {
                button: PointerButton::Right,
                pressed: true,
            }),
        ];
        injector.inject(&events).unwrap();
        // Order is preserved across the pointer/key interleave.
        assert_eq!(injector.injected(), events.to_vec());
        assert_eq!(
            injector.injected_pointers(),
            vec![
                PointerEvent::Motion { dx: 5, dy: 0 },
                PointerEvent::Button {
                    button: PointerButton::Right,
                    pressed: true,
                },
            ]
        );

        injector.fail_next("UIPI");
        assert!(matches!(
            injector.inject(&events),
            Err(InputError::InjectionFailed { .. })
        ));
        // The failed call recorded nothing.
        assert_eq!(injector.injected().len(), 3);
    }
}

#[cfg(test)]
mod clipboard_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ClipboardFailure, ClipboardOp, InMemoryClipboard};
    use crate::clipboard::{ClipboardError, ClipboardProvider};

    fn counting_listener(clipboard: &InMemoryClipboard) -> Arc<AtomicUsize> {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        clipboard
            .set_change_listener(Some(Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap();
        count
    }

    #[test]
    fn local_copies_and_own_writes_both_notify() {
        let clipboard = InMemoryClipboard::new();
        let notifications = counting_listener(&clipboard);

        clipboard.set_text_locally("user copied this");
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("user copied this")
        );

        // The contract term loop prevention exists for: writes through
        // the provider notify as well.
        clipboard.write_text("engine applied this").unwrap();
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
        assert_eq!(clipboard.peek().as_deref(), Some("engine applied this"));
    }

    #[test]
    fn injected_contention_fails_n_times_then_clears() {
        let clipboard = InMemoryClipboard::new();
        clipboard.set_text_locally("content");
        clipboard.fail_next(ClipboardOp::Read, ClipboardFailure::Busy, 2);

        assert!(matches!(
            clipboard.read_text(),
            Err(ClipboardError::Busy { .. })
        ));
        assert!(matches!(
            clipboard.read_text(),
            Err(ClipboardError::Busy { .. })
        ));
        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some("content"));

        clipboard.fail_next(ClipboardOp::Write, ClipboardFailure::Unavailable, 1);
        assert!(matches!(
            clipboard.write_text("x"),
            Err(ClipboardError::Unavailable { .. })
        ));
        clipboard.write_text("y").unwrap();
        assert_eq!(clipboard.peek().as_deref(), Some("y"));
    }

    #[test]
    fn failed_writes_do_not_notify_or_mutate() {
        let clipboard = InMemoryClipboard::new();
        clipboard.set_text_locally("original");
        let notifications = counting_listener(&clipboard);

        clipboard.fail_next(ClipboardOp::Write, ClipboardFailure::Busy, 1);
        assert!(clipboard.write_text("rejected").is_err());
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        assert_eq!(clipboard.peek().as_deref(), Some("original"));
    }

    #[test]
    fn listener_replacement_and_removal() {
        let clipboard = InMemoryClipboard::new();
        let first = counting_listener(&clipboard);
        let second = counting_listener(&clipboard); // replaces the first

        clipboard.set_text_locally("x");
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);

        clipboard.set_change_listener(None).unwrap();
        clipboard.set_text_locally("y");
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_clipboard_reads_none_not_error() {
        let clipboard = InMemoryClipboard::new();
        assert_eq!(clipboard.read_text().unwrap(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{InMemorySecureStorage, SecureStorage, SecureStorageError};

    #[test]
    fn store_load_delete_round_trip() {
        let storage = InMemorySecureStorage::new();
        assert_eq!(storage.load("k").unwrap(), None);

        storage.store("k", b"secret").unwrap();
        assert_eq!(storage.load("k").unwrap().as_deref(), Some(&b"secret"[..]));

        storage.store("k", b"replaced").unwrap();
        assert_eq!(
            storage.load("k").unwrap().as_deref(),
            Some(&b"replaced"[..])
        );

        storage.delete("k").unwrap();
        assert_eq!(storage.load("k").unwrap(), None);
        // Idempotent delete.
        storage.delete("k").unwrap();
    }

    #[test]
    fn injected_failure_fires_once_then_clears() {
        let storage = InMemorySecureStorage::new();
        storage.fail_next_operation("disk on fire");

        let err = storage.store("k", b"secret").unwrap_err();
        let SecureStorageError::Backend { reason } = err;
        assert_eq!(reason, "disk on fire");

        // The failure was consumed; the store is usable again.
        storage.store("k", b"secret").unwrap();
        assert!(storage.load("k").unwrap().is_some());
    }
}

#[cfg(test)]
mod display_tests {
    use super::FakeDisplay;
    use crate::display::{CursorPoint, DisplayError, DisplayInfo, Screen};

    #[test]
    fn reports_the_scripted_screen_and_cursor() {
        let display = FakeDisplay::new(Screen {
            width: 1920,
            height: 1080,
        });
        assert_eq!(
            display.primary_screen().unwrap(),
            Screen {
                width: 1920,
                height: 1080,
            }
        );
        // Cursor starts parked at the origin, then moves where scripted.
        assert_eq!(
            display.cursor_position().unwrap(),
            CursorPoint { x: 0, y: 0 }
        );
        display.set_cursor(CursorPoint { x: 1919, y: 540 });
        assert_eq!(
            display.cursor_position().unwrap(),
            CursorPoint { x: 1919, y: 540 }
        );
        display.set_screen(Screen {
            width: 2560,
            height: 1440,
        });
        assert_eq!(display.primary_screen().unwrap().width, 2560);
    }

    #[test]
    fn scripted_failure_surfaces_on_both_queries() {
        let display = FakeDisplay::new(Screen {
            width: 800,
            height: 600,
        });
        display.fail_with("no display attached");
        assert!(matches!(
            display.primary_screen(),
            Err(DisplayError::Unavailable { .. })
        ));
        assert!(matches!(
            display.cursor_position(),
            Err(DisplayError::Unavailable { .. })
        ));
    }
}
