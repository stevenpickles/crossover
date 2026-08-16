//! Making the process's standard streams safe in a console-less session.
//!
//! When the background service launches `crossover.exe run` via
//! `CreateProcessAsUser` (ADR 0011), the worker has **no console** and no
//! redirected output, so `GetStdHandle(STD_OUTPUT_HANDLE)` returns a null
//! handle. Rust's `println!`/`eprintln!` treat a failed write as fatal and
//! **panic** ("failed printing to stdout"), which would crash the worker on its
//! first status line and leave the service relaunching it forever.
//!
//! [`ensure_standard_streams`] repoints only the *invalid* standard handles at
//! `NUL`, so all output is silently discarded instead of panicking. A real
//! console or a user's `>` redirection leaves the handles valid, so it is left
//! untouched — the fix targets exactly the headless case.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle as _;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};

/// Repoint invalid stdout/stderr at `NUL` so output never panics in a
/// console-less (service-launched) session. A no-op when the handles are valid
/// (an interactive console or a redirection), so interactive runs are
/// unaffected. Call once at startup, before any output.
pub fn ensure_standard_streams() {
    // Only the output streams can panic on write; stdin's missing handle is
    // already handled as EOF by the reader, so it is left alone.
    let targets = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    if !targets.iter().any(|&id| is_invalid(id)) {
        return;
    }

    // One shared write handle to NUL for whichever streams are invalid. Leaked
    // deliberately: a standard handle must stay valid for the whole process, so
    // its lifetime is the process's.
    let Ok(nul) = std::fs::OpenOptions::new().write(true).open("NUL") else {
        return;
    };
    let handle = HANDLE(nul.as_raw_handle().cast::<c_void>());

    for id in targets {
        if is_invalid(id) {
            // SAFETY: `handle` is a live NUL handle kept alive by the `forget`
            // below; SetStdHandle only stores the handle value for `id`.
            unsafe {
                let _ = SetStdHandle(id, handle);
            }
        }
    }
    std::mem::forget(nul);
}

/// Whether the process currently has no usable handle for `id`.
fn is_invalid(id: STD_HANDLE) -> bool {
    // SAFETY: GetStdHandle merely returns the current standard handle for `id`.
    unsafe { GetStdHandle(id) }.map_or(true, |handle| handle.is_invalid())
}

#[cfg(test)]
mod tests {
    use super::ensure_standard_streams;

    // Under the test harness stdout/stderr are valid (captured pipes), so this
    // takes the no-op path: it must be callable without panicking and must not
    // disturb the harness's own streams.
    #[test]
    fn is_a_no_op_when_streams_are_valid() {
        ensure_standard_streams();
        println!("stdout still works after ensure_standard_streams()");
    }
}
