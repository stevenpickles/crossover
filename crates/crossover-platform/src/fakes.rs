//! In-memory fakes of the platform traits.
//!
//! docs/ARCHITECTURE.md §4: every platform trait has a scriptable in-memory
//! fake so all core logic is exercisable with no OS interaction. Enabled via
//! the `fakes` feature (dev-dependencies of consuming crates) and for this
//! crate's own tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use crate::clipboard::{
    ClipboardContent, ClipboardError, ClipboardImageFormat, ClipboardListener, ClipboardProvider,
};
use crate::cursor::{CursorMask, CursorMaskError};
use crate::display::{CursorPoint, DisplayError, DisplayInfo, MonitorRect, Screen};
use crate::file_blob::{BlobNaming, FileBlob, FileBlobBuilder, FileBlobRefusal};
use crate::input::{
    InputCapture, InputError, InputEvent, InputInjector, InputSink, KeyEvent, PointerEvent,
};
use crate::link::{LinkState, LinkStateProbe};
use crate::secure_storage::{SecureStorage, SecureStorageError};
use crate::service::{ServiceError, ServiceManager, ServiceStatus};
use crate::virtual_file::{VirtualFile, VirtualFileClipboard};

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
    /// Fail upcoming `read` calls.
    Read,
    /// Fail upcoming `write` calls.
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
    content: Option<ClipboardContent>,
    listener: Option<ClipboardListener>,
    fail_reads: (usize, Option<ClipboardFailure>),
    fail_writes: (usize, Option<ClipboardFailure>),
}

/// In-memory [`ClipboardProvider`] with scriptable contention.
///
/// Mirrors the documented contract, including the part that matters most
/// for loop prevention: `write` triggers the change listener, just as the
/// Windows clipboard notifies for programmatic writes. Typed since
/// ADR 0014: it holds text or image content, and image bytes are stored
/// and returned verbatim — this fake is what stands in for the OS
/// clipboard in every hermetic image test, so it must not normalize
/// anything.
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
        self.set_locally(ClipboardContent::Text(text.to_owned()));
    }

    /// Simulate a local snip: set image content and notify (ADR 0014).
    pub fn set_image_locally(&self, format: ClipboardImageFormat, bytes: Vec<u8>) {
        self.set_locally(ClipboardContent::Image { format, bytes });
    }

    /// Simulate a local Explorer file/folder copy: set a file-list
    /// observation and notify (ADR 0015, feature/133).
    pub fn set_file_list_locally(&self, paths: Vec<std::path::PathBuf>) {
        self.set_locally(ClipboardContent::FileList(paths));
    }

    /// Simulate any local copy: set content and notify the listener.
    pub fn set_locally(&self, content: ClipboardContent) {
        let listener = {
            let mut state = lock(&self.state);
            state.content = Some(content);
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

    /// Current text content, bypassing failure injection (test
    /// assertions). `None` when the clipboard is empty *or* holds an
    /// image — the shape the text suites have always asserted on.
    #[must_use]
    pub fn peek(&self) -> Option<String> {
        lock(&self.state)
            .content
            .as_ref()
            .and_then(|content| content.as_text().map(str::to_owned))
    }

    /// Current typed content, bypassing failure injection.
    #[must_use]
    pub fn peek_content(&self) -> Option<ClipboardContent> {
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
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        let mut state = lock(&self.state);
        if let Some(kind) = Self::take_failure(&mut state.fail_reads) {
            return Err(Self::failure_error(kind));
        }
        Ok(state.content.clone())
    }

    fn write(&self, content: &ClipboardContent) -> Result<(), ClipboardError> {
        let listener = {
            let mut state = lock(&self.state);
            if let Some(kind) = Self::take_failure(&mut state.fail_writes) {
                return Err(Self::failure_error(kind));
            }
            state.content = Some(content.clone());
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
    /// Scriptable last-local-input tick for the cursor fail-safe; `None`
    /// (the default) reports no query available.
    last_input: Mutex<Option<u32>>,
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

    /// Set the tick `last_input_tick` reports — simulating local input
    /// activity for the cursor fail-safe.
    pub fn set_last_input_tick(&self, tick: u32) {
        *lock(&self.last_input) = Some(tick);
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

    fn last_input_tick(&self) -> Option<u32> {
        *lock(&self.last_input)
    }
}

/// In-memory [`InputInjector`] that records what it was asked to replay.
#[derive(Default)]
pub struct FakeInputInjector {
    injected: Mutex<Vec<InputEvent>>,
    placements: Mutex<Vec<CursorPoint>>,
    fail_next: Mutex<Option<String>>,
    /// Scripts [`InputInjector::can_inject`]: `true` (default) = injectable;
    /// set to script a secure desktop. Stored inverted so the derived default
    /// (`false`) means "injectable".
    blocked: Mutex<bool>,
    /// A display whose cursor follows this injector's placements, when one
    /// is linked (see [`FakeInputInjector::follow`]). Unlinked by default,
    /// so placements are only recorded.
    display: Mutex<Option<Arc<FakeDisplay>>>,
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

    /// The absolute cursor placements requested so far, in order.
    #[must_use]
    pub fn placements(&self) -> Vec<CursorPoint> {
        lock(&self.placements).clone()
    }

    /// Forget the record.
    pub fn clear(&self) {
        lock(&self.injected).clear();
        lock(&self.placements).clear();
    }

    /// Make the next `inject` fail.
    pub fn fail_next(&self, reason: &str) {
        *lock(&self.fail_next) = Some(reason.to_owned());
    }

    /// Script whether input can currently be injected — `false` simulates a
    /// secure desktop (a UAC prompt) so the controlled-side release is
    /// testable without a real desktop switch (feature/87).
    pub fn set_can_inject(&self, available: bool) {
        *lock(&self.blocked) = !available;
    }

    /// Make `display`'s cursor follow this injector's placements, the way a
    /// real absolute move is immediately visible to the display query that
    /// edge detection polls. Without this link a placement is only recorded,
    /// which hides feedback loops between placing the cursor and detecting
    /// where it now is.
    pub fn follow(&self, display: Arc<FakeDisplay>) {
        *lock(&self.display) = Some(display);
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

    fn place_cursor(&self, position: CursorPoint) -> Result<(), InputError> {
        lock(&self.placements).push(position);
        // A real placement moves the pointer the display then reports; a
        // linked display models that, so tests see the same feedback the
        // machine does.
        let display = lock(&self.display).clone();
        if let Some(display) = display {
            display.set_cursor(position);
        }
        Ok(())
    }

    fn can_inject(&self) -> bool {
        !*lock(&self.blocked)
    }
}

/// In-memory [`DisplayInfo`] with a scriptable screen size and cursor, so
/// edge-detection logic is exercisable with no real display.
pub struct FakeDisplay {
    screen: Mutex<Screen>,
    cursor: Mutex<CursorPoint>,
    /// The monitor layout. Defaults to a single monitor covering the whole
    /// screen; multi-monitor tests override it via [`Self::set_monitors`].
    monitors: Mutex<Vec<MonitorRect>>,
    /// When set, both queries fail with this reason — the platform
    /// refusing to report geometry.
    fail: Mutex<Option<String>>,
}

impl FakeDisplay {
    /// A display of `screen`, cursor parked at the top-left, with a single
    /// monitor covering the whole screen.
    #[must_use]
    pub fn new(screen: Screen) -> Self {
        Self {
            screen: Mutex::new(screen),
            cursor: Mutex::new(CursorPoint { x: 0, y: 0 }),
            monitors: Mutex::new(vec![MonitorRect {
                left: 0,
                top: 0,
                width: screen.width,
                height: screen.height,
            }]),
            fail: Mutex::new(None),
        }
    }

    /// Move the fake cursor.
    pub fn set_cursor(&self, cursor: CursorPoint) {
        *lock(&self.cursor) = cursor;
    }

    /// Change the fake screen size. Leaves the monitor layout untouched —
    /// call [`Self::set_monitors`] to change that.
    pub fn set_screen(&self, screen: Screen) {
        *lock(&self.screen) = screen;
    }

    /// Replace the monitor layout, for multi-monitor edge-mapping tests.
    pub fn set_monitors(&self, monitors: Vec<MonitorRect>) {
        *lock(&self.monitors) = monitors;
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
    fn desktop_bounds(&self) -> Result<Screen, DisplayError> {
        self.guard()?;
        Ok(*lock(&self.screen))
    }

    fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError> {
        self.guard()?;
        Ok(lock(&self.monitors).clone())
    }

    fn cursor_position(&self) -> Result<CursorPoint, DisplayError> {
        self.guard()?;
        Ok(*lock(&self.cursor))
    }
}

/// In-memory [`CursorMask`] recording hide/show calls and the resulting
/// visibility, so tests can assert the driver hides the cursor while
/// driving the peer and restores it on every exit.
#[derive(Debug, Default)]
pub struct FakeCursorMask {
    hidden: Mutex<bool>,
    hide_calls: Mutex<u32>,
    show_calls: Mutex<u32>,
    /// When set, both operations fail with this reason.
    fail: Mutex<Option<String>>,
}

impl FakeCursorMask {
    /// A visible cursor with no calls recorded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the cursor is currently hidden per the recorded calls.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        *lock(&self.hidden)
    }

    /// How many times [`CursorMask::hide`] has been called.
    #[must_use]
    pub fn hide_calls(&self) -> u32 {
        *lock(&self.hide_calls)
    }

    /// How many times [`CursorMask::show`] has been called.
    #[must_use]
    pub fn show_calls(&self) -> u32 {
        *lock(&self.show_calls)
    }

    /// Make both operations fail until cleared, simulating a platform that
    /// cannot change cursor visibility.
    pub fn fail_with(&self, reason: &str) {
        *lock(&self.fail) = Some(reason.to_owned());
    }

    fn guard(&self) -> Result<(), CursorMaskError> {
        match lock(&self.fail).clone() {
            Some(reason) => Err(CursorMaskError::Failed { reason }),
            None => Ok(()),
        }
    }
}

impl CursorMask for FakeCursorMask {
    fn hide(&self) -> Result<(), CursorMaskError> {
        self.guard()?;
        *lock(&self.hidden) = true;
        *lock(&self.hide_calls) += 1;
        Ok(())
    }

    fn show(&self) -> Result<(), CursorMaskError> {
        self.guard()?;
        *lock(&self.hidden) = false;
        *lock(&self.show_calls) += 1;
        Ok(())
    }
}

/// In-memory [`ServiceManager`] recording install/uninstall and reporting a
/// scriptable running state, for exercising the `crossover service` command
/// wiring without touching a real OS service.
#[derive(Debug, Default)]
pub struct FakeServiceManager {
    installed: Mutex<bool>,
    running: Mutex<bool>,
    install_calls: Mutex<u32>,
    uninstall_calls: Mutex<u32>,
}

impl FakeServiceManager {
    /// Not installed, not running.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `install` has left it installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        *lock(&self.installed)
    }

    /// How many times `install` was called.
    #[must_use]
    pub fn install_calls(&self) -> u32 {
        *lock(&self.install_calls)
    }

    /// How many times `uninstall` was called.
    #[must_use]
    pub fn uninstall_calls(&self) -> u32 {
        *lock(&self.uninstall_calls)
    }

    /// Script whether an installed service reports as running.
    pub fn set_running(&self, running: bool) {
        *lock(&self.running) = running;
    }
}

impl ServiceManager for FakeServiceManager {
    fn install(&self) -> Result<(), ServiceError> {
        *lock(&self.installed) = true;
        *lock(&self.install_calls) += 1;
        Ok(())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        *lock(&self.installed) = false;
        *lock(&self.running) = false;
        *lock(&self.uninstall_calls) += 1;
        Ok(())
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        if *lock(&self.installed) {
            Ok(ServiceStatus::Installed {
                running: *lock(&self.running),
            })
        } else {
            Ok(ServiceStatus::NotInstalled)
        }
    }
}

/// Scriptable [`LinkStateProbe`]: answers whatever a test set, and records
/// which peer it was asked about.
///
/// The recorded peer is half the point. The trait's contract is *per peer* —
/// the interface carrying this session, not "some interface somewhere" — so
/// a test that only checked the answer would not notice a caller asking the
/// wrong question.
#[derive(Debug, Default)]
pub struct FakeLinkStateProbe {
    answer: Mutex<LinkState>,
    asked_about: Mutex<Vec<SocketAddr>>,
}

impl FakeLinkStateProbe {
    /// A probe that answers `answer` for every peer.
    #[must_use]
    pub fn answering(answer: LinkState) -> Self {
        Self {
            answer: Mutex::new(answer),
            asked_about: Mutex::new(Vec::new()),
        }
    }

    /// Change what the next queries answer.
    pub fn set_answer(&self, answer: LinkState) {
        *lock(&self.answer) = answer;
    }

    /// Every peer address the probe was asked about, in order.
    #[must_use]
    pub fn asked_about(&self) -> Vec<SocketAddr> {
        lock(&self.asked_about).clone()
    }
}

impl LinkStateProbe for FakeLinkStateProbe {
    fn link_state(&self, peer: SocketAddr) -> LinkState {
        lock(&self.asked_about).push(peer);
        *lock(&self.answer)
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

    /// Image bytes are opaque (ADR 0014): whatever goes in comes back
    /// byte-identical, including sequences no text path could survive.
    #[test]
    fn image_content_round_trips_verbatim_and_is_not_text() {
        use crate::clipboard::{ClipboardContent, ClipboardImageFormat};

        let clipboard = InMemoryClipboard::new();
        let notifications = counting_listener(&clipboard);
        let bytes = vec![0xFF, 0x00, 0xFE, 0x00, 0x80, 0xC0, 0xFF];

        clipboard.set_image_locally(ClipboardImageFormat::Dib, bytes.clone());
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(
            clipboard.read().unwrap(),
            Some(ClipboardContent::Image {
                format: ClipboardImageFormat::Dib,
                bytes: bytes.clone(),
            })
        );
        // The text convenience reports an image as absence, never as
        // lossily-decoded text.
        assert_eq!(clipboard.read_text().unwrap(), None);
        assert_eq!(clipboard.peek(), None);

        clipboard
            .write(&ClipboardContent::Image {
                format: ClipboardImageFormat::Png,
                bytes: vec![0x89, b'P', b'N', b'G', 0x00, 0xFF],
            })
            .unwrap();
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
        assert_eq!(
            clipboard.peek_content(),
            Some(ClipboardContent::Image {
                format: ClipboardImageFormat::Png,
                bytes: vec![0x89, b'P', b'N', b'G', 0x00, 0xFF],
            })
        );
    }

    /// A file-list observation (ADR 0015, feature/133) round-trips like any
    /// other typed content and is not text — the same shape the image test
    /// above asserts, so core tests can script a local Explorer copy with
    /// no OS clipboard.
    #[test]
    fn file_list_content_round_trips_and_is_not_text() {
        use crate::clipboard::ClipboardContent;
        use std::path::PathBuf;

        let clipboard = InMemoryClipboard::new();
        let notifications = counting_listener(&clipboard);
        let paths = vec![
            PathBuf::from(r"C:\Users\test\report.pdf"),
            PathBuf::from(r"C:\Users\test\photos"),
        ];

        clipboard.set_file_list_locally(paths.clone());
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(
            clipboard.read().unwrap(),
            Some(ClipboardContent::FileList(paths))
        );
        assert_eq!(clipboard.read_text().unwrap(), None);
        assert_eq!(clipboard.peek(), None);
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
    use crate::display::{CursorPoint, DisplayError, DisplayInfo, MonitorRect, Screen};

    #[test]
    fn reports_the_scripted_screen_and_cursor() {
        let display = FakeDisplay::new(Screen {
            width: 1920,
            height: 1080,
        });
        assert_eq!(
            display.desktop_bounds().unwrap(),
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
        assert_eq!(display.desktop_bounds().unwrap().width, 2560);
    }

    #[test]
    fn scripted_failure_surfaces_on_both_queries() {
        let display = FakeDisplay::new(Screen {
            width: 800,
            height: 600,
        });
        display.fail_with("no display attached");
        assert!(matches!(
            display.desktop_bounds(),
            Err(DisplayError::Unavailable { .. })
        ));
        assert!(matches!(
            display.cursor_position(),
            Err(DisplayError::Unavailable { .. })
        ));
    }

    #[test]
    fn monitors_default_to_one_covering_the_screen_and_are_scriptable() {
        let display = FakeDisplay::new(Screen {
            width: 1920,
            height: 1080,
        });
        // The default layout is a single monitor spanning the whole screen.
        assert_eq!(
            display.monitors().unwrap(),
            vec![MonitorRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            }]
        );

        // A scripted multi-monitor layout is reported verbatim.
        let laptop = MonitorRect {
            left: 0,
            top: 0,
            width: 3840,
            height: 2400,
        };
        let external = MonitorRect {
            left: 3840,
            top: 0,
            width: 3840,
            height: 2160,
        };
        display.set_monitors(vec![laptop, external]);
        assert_eq!(display.monitors().unwrap(), vec![laptop, external]);

        // A scripted failure surfaces here too.
        display.fail_with("no display attached");
        assert!(matches!(
            display.monitors(),
            Err(DisplayError::Unavailable { .. })
        ));
    }
}

/// In-memory [`VirtualFileClipboard`]: records what was offered, and lets
/// a test say whether the clipboard has since moved on.
///
/// The two questions this fake answers are the two the real object exists
/// to answer — *did the offer land* and *is it still ours* — so the
/// engine's lifetime rule and loop guard are exercisable with no OS
/// clipboard and no apartment thread.
#[derive(Debug, Default)]
pub struct FakeVirtualFiles {
    offers: Mutex<Vec<VirtualFile>>,
    current: Mutex<bool>,
    withdrawals: Mutex<u32>,
    /// When set, the next offer fails with this error (then clears).
    fail_next: Mutex<Option<ClipboardError>>,
}

impl FakeVirtualFiles {
    /// A fake holding nothing, offering nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything offered so far, oldest first.
    #[must_use]
    pub fn offers(&self) -> Vec<VirtualFile> {
        self.offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many times the offer has been withdrawn.
    #[must_use]
    pub fn withdrawals(&self) -> u32 {
        *self
            .withdrawals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Somebody else copied: the clipboard has moved on from our item.
    pub fn moved_on(&self) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = false;
    }

    /// Fail the next offer with `error`.
    pub fn fail_next(&self, error: ClipboardError) {
        *self
            .fail_next
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(error);
    }
}

impl VirtualFileClipboard for FakeVirtualFiles {
    fn offer(&self, file: &VirtualFile) -> Result<(), ClipboardError> {
        if let Some(error) = self
            .fail_next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(error);
        }
        self.offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(file.clone());
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(())
    }

    fn is_current(&self) -> bool {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn withdraw(&self) -> Result<(), ClipboardError> {
        *self
            .withdrawals
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = false;
        Ok(())
    }
}

/// What a [`FakeFileBlobBuilder`] should produce for the next selection.
///
/// The packing itself is the Windows builder's business (the walk, the
/// reparse-point rule, the archive); this stands in for its *answer*, so
/// the engine transaction above it can be driven over every shape of
/// result without a filesystem to arrange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeBlob {
    /// The name the builder derived, before any validation.
    pub proposed_name: String,
    /// Where that name came from.
    pub naming: BlobNaming,
    /// Whether the blob stands for an archive.
    pub archived: bool,
    /// How many entries it packs.
    pub entry_count: u32,
    /// Uncompressed total of those entries.
    pub original_bytes: u64,
    /// The blob's bytes.
    pub content: Vec<u8>,
}

impl FakeBlob {
    /// One file's bytes, travelling verbatim under `name`.
    #[must_use]
    pub fn verbatim(name: &str, content: Vec<u8>) -> Self {
        Self {
            proposed_name: name.to_owned(),
            naming: BlobNaming::Own,
            archived: false,
            entry_count: 1,
            original_bytes: content.len() as u64,
            content,
        }
    }
}

/// A scriptable [`FileBlobBuilder`].
///
/// The one place it diverges from the real contract is stated rather than
/// hidden: a real builder never lets a caller name the artifact, and this
/// one writes into a directory of its own under the process temp
/// directory so a test can run without a platform builder. It matches the
/// contract where it counts — the returned handle is positioned at the
/// start, and the file is unlinked as soon as it is open, so the bytes
/// live exactly as long as the [`FileBlob`] does.
#[derive(Debug)]
pub struct FakeFileBlobBuilder {
    plan: Mutex<FakeBlob>,
    refuse_next: Mutex<Option<FileBlobRefusal>>,
    selections: Mutex<Vec<Vec<std::path::PathBuf>>>,
    dir: std::path::PathBuf,
}

impl FakeFileBlobBuilder {
    /// A builder that packs every selection into `content`, under `name`.
    ///
    /// # Panics
    ///
    /// If its working directory cannot be created — a fake with nowhere
    /// to write is a broken test rig, not a condition to model.
    #[must_use]
    pub fn new(name: &str, content: Vec<u8>) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let dir = std::env::temp_dir().join(format!(
            "crossover-fake-blob-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("fake blob builder working directory");
        Self {
            plan: Mutex::new(FakeBlob::verbatim(name, content)),
            refuse_next: Mutex::new(None),
            selections: Mutex::new(Vec::new()),
            dir,
        }
    }

    /// Produce `blob` for every following selection.
    pub fn produce(&self, blob: FakeBlob) {
        *self.plan.lock().unwrap_or_else(PoisonError::into_inner) = blob;
    }

    /// Refuse the next selection with `refusal`, then resume producing.
    pub fn refuse_next(&self, refusal: FileBlobRefusal) {
        *self
            .refuse_next
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(refusal);
    }

    /// Every selection handed to this builder, oldest first.
    #[must_use]
    pub fn selections(&self) -> Vec<Vec<std::path::PathBuf>> {
        self.selections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for FakeFileBlobBuilder {
    fn drop(&mut self) {
        // Best effort: the artifacts are already unlinked, so this only
        // removes the directory itself.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl FileBlobBuilder for FakeFileBlobBuilder {
    fn build(&self, selection: &[std::path::PathBuf]) -> Result<FileBlob, FileBlobRefusal> {
        use sha2::Digest as _;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        self.selections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(selection.to_vec());
        if let Some(refusal) = self
            .refuse_next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(refusal);
        }
        let plan = self
            .plan
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let path = self
            .dir
            .join(format!("{}.blob", NEXT.fetch_add(1, Ordering::Relaxed)));
        let backend = |error: std::io::Error| FileBlobRefusal::Backend {
            reason: error.to_string(),
        };
        std::fs::write(&path, &plan.content).map_err(backend)?;
        let content = std::fs::File::open(&path).map_err(backend)?;
        // Unlinked while open, so the bytes outlive the name and die with
        // the handle — the property the real builder gets from
        // `FILE_FLAG_DELETE_ON_CLOSE`.
        let _ = std::fs::remove_file(&path);
        Ok(FileBlob {
            proposed_name: plan.proposed_name,
            naming: plan.naming,
            archived: plan.archived,
            entry_count: plan.entry_count,
            original_bytes: plan.original_bytes,
            content_length: plan.content.len() as u64,
            content_hash: sha2::Sha256::digest(&plan.content).into(),
            content,
        })
    }
}
