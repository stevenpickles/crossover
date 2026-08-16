//! Stopping a message-pump thread without betting the process on it.
//!
//! The clipboard observer and the input capture each own a thread running a
//! Win32 message loop. Both are told to stop by posting a message to their
//! window, and both used to be `join()`ed unconditionally afterwards. That
//! is a bet: a pump that does not reach its next `GetMessage` — because the
//! OS is holding it inside a hook, a window it owns is wedged, or the
//! posted message never lands — takes the whole process down with it, and
//! the process is left alive with no session, no listener, and nothing to
//! do but hold its file locks.
//!
//! That is not hypothetical. A soak worker on 2026-08-16 completed its
//! shutdown — closed its listener, dropped its session, wrote its metrics —
//! and then sat there, two threads, idle, until it was killed. The
//! signature (main plus one pump thread, nothing running) is what an
//! unbounded join on a wedged pump looks like from the outside.
//!
//! It also has a consequence beyond an untidy exit: `chocolateyInstall`
//! upgrades by waiting for both executables to exit so the file locks
//! release (packaging/README.md). A worker that never exits on its own
//! stalls that wait.
//!
//! So shutdown now waits a bounded time and, if the pump has not
//! acknowledged, says which one and leaves it detached. The process exits;
//! the log names the culprit. A hang becomes a diagnosable line.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long a pump gets to acknowledge its shutdown message.
///
/// Generous by the standards of the work involved — leaving a message loop
/// is immediate — and short by the standards of a person waiting for a
/// process to quit.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Whether the pump acknowledged that it left its loop, within `grace`.
///
/// A disconnected channel counts as stopped: the sender lives in the pump
/// closure, so it can only have been dropped by that closure ending.
pub(crate) fn acknowledged(stopped: &Receiver<()>, grace: Duration) -> bool {
    match stopped.recv_timeout(grace) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
    }
}

/// Join a pump thread that acknowledged; detach and warn about one that did
/// not. Called from `Drop`, where returning is the only option — panicking
/// or blocking forever both make a bad situation worse.
pub(crate) fn stop(name: &'static str, stopped: &Receiver<()>, pump: &mut Option<JoinHandle<()>>) {
    if acknowledged(stopped, SHUTDOWN_GRACE) {
        if let Some(handle) = pump.take() {
            // It has left its loop, so this returns promptly.
            let _ = handle.join();
        }
        return;
    }
    // Deliberately not joined: the handle drops, the thread is detached,
    // and the process can finish exiting. Whatever is holding the pump is
    // now named in the log rather than invisible.
    tracing::warn!(
        pump = name,
        grace_ms = SHUTDOWN_GRACE.as_millis(),
        "message pump did not acknowledge shutdown; detaching it so the process can exit"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::{acknowledged, stop};

    #[test]
    fn a_pump_that_signals_is_acknowledged() {
        let (tx, rx) = mpsc::channel();
        tx.send(()).unwrap();
        assert!(acknowledged(&rx, Duration::from_secs(5)));
    }

    #[test]
    fn a_pump_thread_that_ended_without_signalling_still_counts_as_stopped() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        assert!(acknowledged(&rx, Duration::from_secs(5)));
    }

    /// The point of the whole module: a pump that never answers must cost a
    /// bounded wait, not the process.
    #[test]
    fn a_silent_pump_times_out_instead_of_blocking_forever() {
        let (tx, rx) = mpsc::channel::<()>();
        let started = Instant::now();
        assert!(!acknowledged(&rx, Duration::from_millis(150)));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(tx);
    }

    /// `stop` must return even when the thread it was handed never ends —
    /// the exact case that wedged a shutdown.
    #[test]
    fn stopping_a_wedged_pump_returns_rather_than_hanging() {
        let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
        let (_stop_tx, stop_rx) = mpsc::channel::<()>();
        let mut pump = Some(std::thread::spawn(move || {
            // Stands in for a pump stuck below its message loop.
            let _ = unblock_rx.recv();
        }));

        let started = Instant::now();
        stop("test-pump", &stop_rx, &mut pump);
        assert!(
            started.elapsed() < super::SHUTDOWN_GRACE + Duration::from_secs(2),
            "stop() waited {:?}",
            started.elapsed()
        );
        // Still held, never joined: detaching is what let the caller return.
        assert!(pump.is_some());

        let _ = unblock_tx.send(());
    }
}
