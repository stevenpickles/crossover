//! Auto-start / background-operation management (ADR 0011).
//!
//! Unlike the other platform traits — which invert dependencies so *core*
//! can call a capability the platform provides — this one abstracts an OS
//! lifecycle mechanism that *wraps* the app: making Crossover start
//! automatically and stay running unattended. Each OS supplies its own
//! implementation (Windows: a `LocalSystem` service that launches the worker
//! into the user session; macOS: a `LaunchAgent`; Linux: a `systemd --user`
//! unit), so the CLI `crossover service` command stays platform-neutral and
//! the OS machinery never leaks into the app or core.
//!
//! It is deliberately abstracted at the **goal** level — install, uninstall,
//! status — not at the mechanism level, because those mechanisms differ
//! wildly (a Windows service is nothing like a launchd plist). A platform
//! with no implementation yet returns [`ServiceError::Unsupported`].

use thiserror::Error;

/// Whether auto-start is installed, and if so whether it is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    /// No auto-start is registered for Crossover.
    NotInstalled,
    /// Auto-start is registered; `running` reflects whether it is active now.
    Installed {
        /// Whether the service/agent is currently running.
        running: bool,
    },
}

/// Failures from a [`ServiceManager`].
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ServiceError {
    /// This platform has no auto-start implementation wired yet.
    #[error("background service is not supported on this platform yet")]
    Unsupported,
    /// The operation needs elevation (e.g. admin to register a service).
    #[error("insufficient privileges: {reason}")]
    PermissionDenied {
        /// What privilege was missing, for an actionable message (FR-7.3).
        reason: String,
    },
    /// The OS service backend failed.
    #[error("service operation failed: {reason}")]
    Backend {
        /// Diagnostic detail.
        reason: String,
    },
}

/// Manage Crossover's auto-start so it runs unattended (ADR 0011). The CLI
/// `crossover service` subcommands map one-to-one onto these.
pub trait ServiceManager: Send + Sync {
    /// Register Crossover to start automatically (and supervise it). May
    /// require elevation.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Unsupported`] where there is no implementation,
    /// [`ServiceError::PermissionDenied`] without the required privilege, or
    /// [`ServiceError::Backend`] if the OS registration fails.
    fn install(&self) -> Result<(), ServiceError>;

    /// Stop and unregister the auto-start.
    ///
    /// # Errors
    ///
    /// As [`Self::install`].
    fn uninstall(&self) -> Result<(), ServiceError>;

    /// Report whether auto-start is installed and running.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Unsupported`], or [`ServiceError::Backend`] if the
    /// state cannot be queried.
    fn status(&self) -> Result<ServiceStatus, ServiceError>;
}

/// A [`ServiceManager`] for platforms with no auto-start implementation yet
/// (every OS other than Windows, currently): every operation reports
/// [`ServiceError::Unsupported`] rather than pretending.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnsupportedServiceManager;

impl ServiceManager for UnsupportedServiceManager {
    fn install(&self) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported)
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported)
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        Err(ServiceError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::{ServiceError, ServiceManager, UnsupportedServiceManager};

    #[test]
    fn unsupported_reports_unsupported_for_every_operation() {
        let manager = UnsupportedServiceManager;
        assert!(matches!(manager.install(), Err(ServiceError::Unsupported)));
        assert!(matches!(
            manager.uninstall(),
            Err(ServiceError::Unsupported)
        ));
        assert!(matches!(manager.status(), Err(ServiceError::Unsupported)));
    }
}
