//! Structured logging setup for the Crossover binary.
//!
//! FR-7.3 requires structured logging from the first commit; field, span,
//! and level conventions live in docs/ARCHITECTURE.md §10. The discipline
//! that matters most (FR-7.4): clipboard contents and private key material
//! never appear in logs at any level — transactions are logged by metadata
//! only.

use tracing_subscriber::EnvFilter;

/// Install the global tracing subscriber.
///
/// Filtering follows `RUST_LOG` when set (e.g. `RUST_LOG=crossover=debug`)
/// and defaults to `info`: enough to observe every important state
/// transition (NFR-3) without per-event input chatter.
///
/// # Errors
///
/// Fails if a global subscriber is already installed; the subscriber is
/// process-global, so this must be called exactly once, from `main`.
pub fn init() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))
}

#[cfg(test)]
mod tests {
    // The subscriber is process-global: first install succeeds, a second
    // must fail loudly rather than silently replacing the first.
    #[test]
    fn init_installs_once_and_rejects_reinstall() {
        assert!(super::init().is_ok());
        assert!(super::init().is_err());
    }
}
