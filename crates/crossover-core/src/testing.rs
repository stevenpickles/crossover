//! Test-only helpers shared across this crate's unit tests.
//!
//! Compiled under `#[cfg(test)]` only, so nothing here reaches a shipped
//! binary.
//!
//! It exists for one thing so far: reading the log a maintainer would
//! read. Several of this crate's guarantees are *diagnostics* — ADR 0018's
//! degraded cursor placement must "log a diagnostic naming the mismatch",
//! a disconnect must say whether the local link went down (NFR-3) — and
//! the honest test of a diagnostic is the emitted line, not an internal
//! value a refactor could keep while the line rots.

use std::sync::{Arc, Mutex, PoisonError};

/// A subscriber writing into a buffer, so a test can read the line a
/// maintainer would read.
#[derive(Clone, Default)]
pub(crate) struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `body` with everything it logs captured, and return the text.
pub(crate) fn captured(body: impl FnOnce()) -> String {
    let sink = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = sink
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    String::from_utf8(bytes).expect("log output was not UTF-8")
}
