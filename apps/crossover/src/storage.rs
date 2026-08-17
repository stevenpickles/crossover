//! Platform secure-storage selection for the binary.
//!
//! The composition root is the one place allowed to name a platform crate
//! (docs/ARCHITECTURE.md §2): everything else sees `dyn SecureStorage`.

use crossover_platform::SecureStorage;

/// Open the platform's secure storage.
///
/// # Errors
///
/// On Windows, if the per-user location cannot be determined. On other
/// platforms, always — their `SecureStorage` backends arrive in Phase 7
/// (docs/ROADMAP.md), and pretending otherwise would violate the
/// no-silent-plaintext-fallback contract.
pub fn open_secure_storage() -> anyhow::Result<Box<dyn SecureStorage>> {
    #[cfg(windows)]
    {
        use anyhow::Context;
        let storage = crossover_platform_windows::DpapiSecureStorage::in_default_location()
            .context("locating per-user secure storage")?;
        Ok(Box::new(storage))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!(
            "secure storage is not implemented for this platform yet \
             (Windows first; macOS/Linux arrive in Phase 7 — docs/ROADMAP.md)"
        )
    }
}

/// Open the platform clipboard provider.
///
/// # Errors
///
/// On Windows, if clipboard observation cannot be established. On other
/// platforms, always — their providers arrive in Phase 7.
pub fn open_clipboard_provider()
-> anyhow::Result<std::sync::Arc<dyn crossover_platform::ClipboardProvider>> {
    #[cfg(windows)]
    {
        use anyhow::Context;
        let provider = crossover_platform_windows::WindowsClipboard::new()
            .context("starting clipboard observation")?;
        Ok(std::sync::Arc::new(provider))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!(
            "the clipboard is not implemented for this platform yet              (Windows first; macOS/Linux arrive in Phase 7 — docs/ROADMAP.md)"
        )
    }
}

/// Open the protected spool peer files are received into (ADR 0015), and
/// sweep whatever a previous run left in it.
///
/// Never fatal, and never fails the run: a spool that cannot be opened
/// means **file receive is off for this run** and everything else works
/// exactly as before. It is also never repaired by deleting what is
/// there — deleting whatever sits at the spool path is precisely the
/// operation SECURITY.md F15 defends against, so the diagnostic names the
/// path and leaves it to the user.
///
/// The sweep is unconditional and part of opening: entries from a previous
/// run cannot correspond to the current clipboard, so nothing from before
/// this process is reachable, and an unpasted delivery does not survive a
/// restart by construction rather than by intention (ADR 0015).
#[must_use]
pub fn open_spool() -> Option<std::sync::Arc<dyn crossover_platform::SpoolStorage>> {
    #[cfg(windows)]
    {
        use crossover_platform::SpoolStorage;

        let spool = match crossover_platform_windows::WindowsSpoolStorage::in_default_location() {
            Ok(spool) => spool,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "file receive is disabled for this run; text and images are unaffected"
                );
                eprintln!("File receive is disabled for this run: {error}");
                return None;
            }
        };
        match spool.sweep() {
            Ok(report) => {
                if !report.removed.is_empty() || !report.retained.is_empty() {
                    tracing::info!(
                        removed = report.removed.len(),
                        removed_bytes = report.removed_bytes,
                        retained = report.retained.len(),
                        "swept the spool of a previous run's entries"
                    );
                }
                if !report.retained.is_empty() {
                    // Something in the spool root is not ours to remove —
                    // a planted directory, or an entry held open. Worth a
                    // warning: nothing else writes there (F15).
                    tracing::warn!(
                        retained = ?report.retained,
                        "objects remain in the spool root after the startup sweep"
                    );
                }
            }
            Err(error) => tracing::warn!(%error, "the spool could not be swept at startup"),
        }
        Some(std::sync::Arc::new(spool))
    }
    #[cfg(not(windows))]
    {
        // No protected spool off Windows yet, and no unprotected fallback:
        // ADR 0015 leaves the Linux answer open, and an ordinary directory
        // would void the guarantee the spool exists to make.
        None
    }
}

/// Open the platform display-info provider (primary-display geometry and
/// cursor position, ADR 0009).
///
/// # Errors
///
/// Always, on non-Windows platforms: display enumeration is Windows-first
/// (macOS/Linux arrive in later phases — docs/ROADMAP.md).
#[cfg(windows)]
// The Windows provider is infallible to construct; the `Result` matches
// the fallible non-Windows signature and the other `open_*` openers.
#[allow(clippy::unnecessary_wraps)]
pub fn open_display() -> anyhow::Result<std::sync::Arc<dyn crossover_platform::DisplayInfo>> {
    Ok(std::sync::Arc::new(
        crossover_platform_windows::WindowsDisplayInfo::new(),
    ))
}

/// Open the platform display-info provider.
///
/// # Errors
///
/// Always, on non-Windows platforms (macOS/Linux arrive in later phases).
#[cfg(not(windows))]
pub fn open_display() -> anyhow::Result<std::sync::Arc<dyn crossover_platform::DisplayInfo>> {
    anyhow::bail!(
        "display enumeration is not implemented for this platform yet \
         (Windows first; macOS/Linux arrive in later phases — docs/ROADMAP.md)"
    )
}

/// Open the platform pointer-input capture and injector.
///
/// Returned together because control transfer needs both, and both are
/// Windows-only until Phase 7 (docs/ROADMAP.md). The capture provider
/// owns a pump thread from construction; it installs no hook until the
/// control engine first grants control.
///
/// # Errors
///
/// On Windows, if the capture pump thread or its window cannot be
/// created. On other platforms, always — injection and capture arrive
/// with those platforms in later phases.
#[cfg(windows)]
pub fn open_input() -> anyhow::Result<(
    std::sync::Arc<dyn crossover_platform::InputCapture>,
    std::sync::Arc<dyn crossover_platform::InputInjector>,
)> {
    use anyhow::Context;
    let capture =
        crossover_platform_windows::WindowsInputCapture::new().context("starting input capture")?;
    let injector = crossover_platform_windows::WindowsInputInjector::new();
    Ok((std::sync::Arc::new(capture), std::sync::Arc::new(injector)))
}

/// Open the local cursor mask (ADR 0009): hides the controller's frozen,
/// edge-pinned cursor while it drives the peer, so only the peer's cursor
/// is visible. Never fails — masking is a display nicety, so a platform
/// that cannot create the overlay falls back to no masking and control
/// still works.
#[cfg(windows)]
#[must_use]
pub fn open_cursor_mask() -> std::sync::Arc<dyn crossover_platform::CursorMask> {
    match crossover_platform_windows::WindowsCursorMask::new() {
        Ok(mask) => std::sync::Arc::new(mask),
        Err(error) => {
            tracing::warn!(
                %error,
                "cursor masking unavailable; the controller's cursor will stay visible"
            );
            std::sync::Arc::new(crossover_platform::NoopCursorMask)
        }
    }
}

/// No overlay off Windows yet (input forwarding is Windows-first); control
/// works, just without cursor masking.
#[cfg(not(windows))]
#[must_use]
pub fn open_cursor_mask() -> std::sync::Arc<dyn crossover_platform::CursorMask> {
    std::sync::Arc::new(crossover_platform::NoopCursorMask)
}

/// Open the platform pointer-input capture and injector.
///
/// # Errors
///
/// Always, on non-Windows platforms: input forwarding is Windows-first
/// (macOS/Linux arrive in later phases — docs/ROADMAP.md).
#[cfg(not(windows))]
pub fn open_input() -> anyhow::Result<(
    std::sync::Arc<dyn crossover_platform::InputCapture>,
    std::sync::Arc<dyn crossover_platform::InputInjector>,
)> {
    anyhow::bail!(
        "input forwarding is not implemented for this platform yet \
         (Windows first; macOS/Linux arrive in later phases — docs/ROADMAP.md)"
    )
}

/// Open the platform's auto-start [`ServiceManager`] (ADR 0011).
///
/// On Windows, the manager registers `crossover-svc.exe` — resolved as a
/// sibling of the running `crossover.exe`, since the two binaries ship
/// together. On other platforms it returns the `Unsupported` manager, so
/// `crossover service` reports a clear "not on this platform yet" rather than
/// being absent.
///
/// # Errors
///
/// On Windows, if the running executable's path or its parent directory cannot
/// be determined (needed to locate the sibling daemon binary).
#[cfg(windows)]
pub fn open_service_manager() -> anyhow::Result<Box<dyn crossover_platform::ServiceManager>> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("locating the running executable")?;
    let dir = exe
        .parent()
        .context("the running executable has no parent directory")?;
    let service_binary = dir.join("crossover-svc.exe");
    Ok(Box::new(
        crossover_platform_windows::WindowsServiceManager::new(service_binary),
    ))
}

/// Open the platform's auto-start [`ServiceManager`] (ADR 0011).
///
/// # Errors
///
/// Never on non-Windows: returns the `Unsupported` manager so the command
/// reports a clear platform message. The `Result` matches the Windows
/// signature, which is fallible.
#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
pub fn open_service_manager() -> anyhow::Result<Box<dyn crossover_platform::ServiceManager>> {
    Ok(Box::new(crossover_platform::UnsupportedServiceManager))
}

/// Resolve the local device name: `--name` flag, else the machine's
/// hostname-ish environment, else a fixed fallback — truncated on a char
/// boundary to the identity bound.
pub fn resolve_device_name(flag: Option<String>) -> String {
    let mut name = flag
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "crossover-device".to_owned());
    while name.len() > crossover_security::identity::MAX_DEVICE_NAME_BYTES {
        name.pop();
    }
    if name.is_empty() {
        "crossover-device".clone_into(&mut name);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::resolve_device_name;

    #[test]
    fn explicit_name_wins_and_is_bounded() {
        assert_eq!(resolve_device_name(Some("left".into())), "left");

        let oversized = "x".repeat(100);
        let resolved = resolve_device_name(Some(oversized));
        assert_eq!(
            resolved.len(),
            crossover_security::identity::MAX_DEVICE_NAME_BYTES
        );

        // Multi-byte truncation stays on a char boundary (pop is
        // char-wise): no panic, valid UTF-8.
        let emoji = "é".repeat(60); // 120 bytes
        let resolved = resolve_device_name(Some(emoji));
        assert!(resolved.len() <= crossover_security::identity::MAX_DEVICE_NAME_BYTES);
        assert!(!resolved.is_empty());
    }
}
