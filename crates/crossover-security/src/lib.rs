//! Security layer for Crossover: device identity, pairing, the trust store,
//! and TLS configuration.
//!
//! Depends on `crossover-platform` for the `SecureStorage` trait that
//! protects private key material at rest. Threat model and security
//! invariants live in `docs/SECURITY.md`; layering in `docs/ARCHITECTURE.md`.

pub mod identity;
pub mod tls;
pub mod trust;

pub use identity::{DeviceIdentity, IdentityError, SpkiFingerprint};
pub use tls::{
    CertifiedIdentity, TlsError, certificate_spki_fingerprint, client_tls_config, server_tls_config,
};
pub use trust::{PeerPermissions, TrustStore, TrustStoreError, TrustedPeer};

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str =
    "device identity, pairing, trust store, and TLS configuration (docs/SECURITY.md)";

#[cfg(test)]
mod tests {
    use super::CRATE_PURPOSE;

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }
}
