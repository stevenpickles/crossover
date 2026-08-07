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
