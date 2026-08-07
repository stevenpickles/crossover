//! TLS credential handling (ADR 0003, docs/SECURITY.md §5).
//!
//! The identity keypair is the identity; the X.509 certificate generated
//! here is nothing but TLS's native container for it. Certificate fields —
//! subject, validity, extensions — carry **no trust semantics**, and
//! verification (this module's pinned verifiers) compares only the SPKI
//! fingerprint against the trust store.

use std::collections::HashSet;
use std::sync::Arc;

use ed25519_dalek::pkcs8::EncodePrivateKey;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::identity::{DeviceIdentity, SpkiFingerprint};
use crate::trust::TrustStore;

/// Failures in TLS credential construction or peer certificate handling.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum TlsError {
    /// Converting the identity key into PKCS#8 failed.
    #[error("identity key conversion failed: {reason}")]
    IdentityKey { reason: String },

    /// Generating the self-signed certificate container failed.
    #[error("certificate generation failed: {reason}")]
    CertificateGeneration { reason: String },

    /// A peer certificate could not be parsed or is structurally
    /// unacceptable. Network input: fail closed.
    #[error("invalid peer certificate: {reason}")]
    InvalidPeerCertificate { reason: String },

    /// Building a rustls configuration failed.
    #[error("TLS configuration failed: {reason}")]
    Config { reason: String },
}

/// The local identity in TLS-presentable form: the Ed25519 key wrapped in
/// a minimal self-signed certificate (ADR 0003).
///
/// The certificate is regenerated freely (e.g., each process start);
/// peers pin the key's SPKI fingerprint, so regeneration never breaks
/// trust.
pub struct CertifiedIdentity {
    certificate: CertificateDer<'static>,
    private_key: PrivateKeyDer<'static>,
    fingerprint: SpkiFingerprint,
}

impl std::fmt::Debug for CertifiedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertifiedIdentity")
            .field("fingerprint", &self.fingerprint)
            .field("private_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl CertifiedIdentity {
    /// Wrap `identity`'s keypair in a fresh self-signed certificate.
    ///
    /// Invariant checked before returning: the certificate's SPKI
    /// fingerprint equals the identity's — a container that changed the
    /// key would silently break pinning, so it is verified here rather
    /// than trusted.
    ///
    /// # Errors
    ///
    /// [`TlsError::IdentityKey`] if PKCS#8 conversion fails;
    /// [`TlsError::CertificateGeneration`] if rcgen fails or produced a
    /// certificate whose SPKI does not match the identity.
    pub fn from_identity(identity: &DeviceIdentity) -> Result<Self, TlsError> {
        // Ed25519 private key → PKCS#8 DER (zeroized on drop by pkcs8).
        let pkcs8 = identity
            .signing_key()
            .to_pkcs8_der()
            .map_err(|e| TlsError::IdentityKey {
                reason: e.to_string(),
            })?;

        let key_pair =
            rcgen::KeyPair::try_from(pkcs8.as_bytes()).map_err(|e| TlsError::IdentityKey {
                reason: e.to_string(),
            })?;

        // Minimal params: no subject names, no extensions of meaning. The
        // certificate is a carrier, not a credential (ADR 0003).
        let params = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| {
            TlsError::CertificateGeneration {
                reason: e.to_string(),
            }
        })?;
        let certificate =
            params
                .self_signed(&key_pair)
                .map_err(|e| TlsError::CertificateGeneration {
                    reason: e.to_string(),
                })?;
        let certificate_der = certificate.der().clone().into_owned();

        let fingerprint = certificate_spki_fingerprint(&certificate_der)?;
        let expected = identity
            .spki_fingerprint()
            .map_err(|e| TlsError::IdentityKey {
                reason: e.to_string(),
            })?;
        if fingerprint != expected {
            return Err(TlsError::CertificateGeneration {
                reason: format!(
                    "generated certificate SPKI {fingerprint} does not match \
                     identity SPKI {expected}"
                ),
            });
        }

        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.as_bytes().to_vec()));
        Ok(Self {
            certificate: certificate_der,
            private_key,
            fingerprint,
        })
    }

    /// The certificate to present during handshakes.
    #[must_use]
    pub fn certificate(&self) -> &CertificateDer<'static> {
        &self.certificate
    }

    /// The private key in rustls form (crate-internal: only the config
    /// builders in this module may take it).
    pub(crate) fn private_key(&self) -> PrivateKeyDer<'static> {
        self.private_key.clone_key()
    }

    /// This identity's SPKI fingerprint (matches
    /// [`DeviceIdentity::spki_fingerprint`]).
    #[must_use]
    pub fn fingerprint(&self) -> SpkiFingerprint {
        self.fingerprint
    }
}

/// Extract the SPKI SHA-256 fingerprint from a certificate in DER form.
///
/// This is the identity a peer *actually presented*, used both by the
/// pinned verifiers during handshakes and by the session layer to name
/// the peer afterwards.
///
/// # Errors
///
/// [`TlsError::InvalidPeerCertificate`] for undecodable DER or trailing
/// bytes (network input: strict, fail closed).
pub fn certificate_spki_fingerprint(
    cert: &CertificateDer<'_>,
) -> Result<SpkiFingerprint, TlsError> {
    let (rest, parsed) =
        X509Certificate::from_der(cert.as_ref()).map_err(|e| TlsError::InvalidPeerCertificate {
            reason: format!("undecodable X.509: {e}"),
        })?;
    if !rest.is_empty() {
        return Err(TlsError::InvalidPeerCertificate {
            reason: format!("{} trailing bytes after certificate", rest.len()),
        });
    }
    let spki_der = parsed.tbs_certificate.subject_pki.raw;
    let digest: [u8; 32] = Sha256::digest(spki_der).into();
    Ok(SpkiFingerprint(digest))
}

/// The pinned-peer check shared by both verifier directions
/// (docs/SECURITY.md §5): exactly one certificate, decodable, whose SPKI
/// fingerprint is in the trust snapshot. Everything else rejects.
#[derive(Debug)]
struct PinnedPeerCheck {
    trusted: HashSet<SpkiFingerprint>,
    provider: Arc<CryptoProvider>,
}

impl PinnedPeerCheck {
    fn from_store(trust: &TrustStore, provider: Arc<CryptoProvider>) -> Self {
        Self {
            trusted: trust
                .peers()
                .iter()
                .map(super::trust::TrustedPeer::fingerprint)
                .collect(),
            provider,
        }
    }

    fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), rustls::Error> {
        // There is no chain in this system (ADR 0003): a peer presenting
        // intermediates is not a Crossover peer.
        if !intermediates.is_empty() {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ));
        }
        let fingerprint = certificate_spki_fingerprint(end_entity).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        if self.trusted.contains(&fingerprint) {
            Ok(())
        } else {
            // Unknown identity: reject before any application data
            // (invariant 2). The session layer logs the fingerprint from
            // its side; rustls errors carry no custom payload.
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
}

/// Client-side verifier: authenticates the *server* by pinned SPKI.
#[derive(Debug)]
struct PinnedServerVerifier(PinnedPeerCheck);

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // server_name is deliberately ignored: names carry no trust
        // semantics in this system, only the pinned key does (ADR 0003).
        self.0.check(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is never negotiated (TLS 1.3-only configs below); a
        // 1.2 signature reaching us is a protocol violation.
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Identity keys are Ed25519 (ADR 0003); nothing else is accepted.
        vec![SignatureScheme::ED25519]
    }
}

/// Server-side verifier: authenticates the *client* by pinned SPKI.
/// Client certificates are mandatory — mutual authentication is not
/// optional (FR-2.1).
#[derive(Debug)]
struct PinnedClientVerifier(PinnedPeerCheck);

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CAs, no hints: peers always present their self-signed cert.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.0.check(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Client-side TLS 1.3 configuration: present our identity, accept only a
/// server whose SPKI is pinned in `trust`.
///
/// The trust set is a **snapshot**: rebuild configurations after trust
/// changes (new connections then see the update); revoking a peer must
/// also terminate its live sessions at the session layer (FR-1.4).
///
/// # Errors
///
/// [`TlsError::Config`] if rustls rejects the configuration or credential.
pub fn client_tls_config(
    identity: &CertifiedIdentity,
    trust: &TrustStore,
) -> Result<rustls::ClientConfig, TlsError> {
    let provider = provider();
    let verifier = PinnedServerVerifier(PinnedPeerCheck::from_store(trust, Arc::clone(&provider)));
    rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| config_error(&e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![identity.certificate().clone()], identity.private_key())
        .map_err(|e| config_error(&e))
}

/// Server-side TLS 1.3 configuration: present our identity, require and
/// accept only a client whose SPKI is pinned in `trust`.
///
/// Same snapshot semantics as [`client_tls_config`].
///
/// # Errors
///
/// [`TlsError::Config`] if rustls rejects the configuration or credential.
pub fn server_tls_config(
    identity: &CertifiedIdentity,
    trust: &TrustStore,
) -> Result<rustls::ServerConfig, TlsError> {
    let provider = provider();
    let verifier = PinnedClientVerifier(PinnedPeerCheck::from_store(trust, Arc::clone(&provider)));
    rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| config_error(&e))?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(vec![identity.certificate().clone()], identity.private_key())
        .map_err(|e| config_error(&e))
}

fn config_error(e: &rustls::Error) -> TlsError {
    TlsError::Config {
        reason: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use rustls::pki_types::CertificateDer;

    use super::{CertifiedIdentity, TlsError, certificate_spki_fingerprint};
    use crate::identity::DeviceIdentity;

    #[test]
    fn certificate_carries_the_identity_key_exactly() {
        let identity = DeviceIdentity::generate("machine").unwrap();
        let certified = CertifiedIdentity::from_identity(&identity).unwrap();

        // The container did not change the key: cert SPKI == identity SPKI.
        assert_eq!(
            certified.fingerprint(),
            identity.spki_fingerprint().unwrap()
        );
        assert_eq!(
            certificate_spki_fingerprint(certified.certificate()).unwrap(),
            identity.spki_fingerprint().unwrap()
        );
    }

    #[test]
    fn regeneration_changes_certificate_but_never_fingerprint() {
        let identity = DeviceIdentity::generate("machine").unwrap();
        let first = CertifiedIdentity::from_identity(&identity).unwrap();
        let second = CertifiedIdentity::from_identity(&identity).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn distinct_identities_produce_distinct_cert_fingerprints() {
        let a = CertifiedIdentity::from_identity(&DeviceIdentity::generate("a").unwrap()).unwrap();
        let b = CertifiedIdentity::from_identity(&DeviceIdentity::generate("b").unwrap()).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn garbage_and_truncated_certificates_fail_closed() {
        let garbage = CertificateDer::from(vec![0xFF; 64]);
        assert!(matches!(
            certificate_spki_fingerprint(&garbage),
            Err(TlsError::InvalidPeerCertificate { .. })
        ));

        let identity = DeviceIdentity::generate("machine").unwrap();
        let certified = CertifiedIdentity::from_identity(&identity).unwrap();
        let full = certified.certificate().as_ref();
        let truncated = CertificateDer::from(full[..full.len() / 2].to_vec());
        assert!(matches!(
            certificate_spki_fingerprint(&truncated),
            Err(TlsError::InvalidPeerCertificate { .. })
        ));

        // Trailing bytes are rejected too.
        let mut padded = full.to_vec();
        padded.push(0x00);
        assert!(matches!(
            certificate_spki_fingerprint(&CertificateDer::from(padded)),
            Err(TlsError::InvalidPeerCertificate { .. })
        ));
    }

    #[test]
    fn debug_output_redacts_the_private_key() {
        let identity = DeviceIdentity::generate("machine").unwrap();
        let certified = CertifiedIdentity::from_identity(&identity).unwrap();
        let debug = format!("{certified:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("PrivatePkcs8KeyDer"));
    }
}

/// Full in-memory TLS handshakes: both rustls endpoints driven over byte
/// buffers — real handshakes, no sockets (docs/ARCHITECTURE.md §3: the
/// security layer is testable without I/O).
#[cfg(test)]
mod handshake_tests {
    use std::sync::Arc;

    use rustls::pki_types::ServerName;
    use rustls::{ClientConnection, ServerConnection};

    use super::{
        CertifiedIdentity, certificate_spki_fingerprint, client_tls_config, server_tls_config,
    };
    use crate::identity::DeviceIdentity;
    use crate::trust::{TrustStore, TrustedPeer};

    struct Endpoint {
        identity: DeviceIdentity,
        certified: CertifiedIdentity,
        trust: TrustStore,
    }

    fn endpoint(name: &str) -> Endpoint {
        let identity = DeviceIdentity::generate(name).unwrap();
        let certified = CertifiedIdentity::from_identity(&identity).unwrap();
        Endpoint {
            identity,
            certified,
            trust: TrustStore::new(),
        }
    }

    fn trust(who: &mut Endpoint, whom: &Endpoint) {
        who.trust
            .add_peer(
                TrustedPeer::new(
                    whom.identity.device_id(),
                    whom.identity.device_name(),
                    whom.certified.fingerprint(),
                )
                .unwrap(),
            )
            .unwrap();
    }

    /// Drive both connections to handshake completion (or failure) by
    /// shuttling TLS bytes through in-memory buffers.
    fn handshake(
        client_ep: &Endpoint,
        server_ep: &Endpoint,
    ) -> Result<(ClientConnection, ServerConnection), rustls::Error> {
        let client_config =
            Arc::new(client_tls_config(&client_ep.certified, &client_ep.trust).unwrap());
        let server_config =
            Arc::new(server_tls_config(&server_ep.certified, &server_ep.trust).unwrap());

        // The name is required by the API but carries no trust semantics
        // (pinned SPKI only); any placeholder works.
        let name = ServerName::try_from("crossover.invalid").unwrap();
        let mut client = ClientConnection::new(client_config, name)?;
        let mut server = ServerConnection::new(server_config)?;

        // Bounded loop: a full TLS 1.3 handshake takes a handful of
        // flights; 32 rounds of no progress means a logic error.
        for _ in 0..32 {
            while client.wants_write() {
                let mut buf = Vec::new();
                client.write_tls(&mut buf).unwrap();
                let mut cursor = &buf[..];
                while !cursor.is_empty() {
                    server.read_tls(&mut cursor).unwrap();
                }
                server.process_new_packets()?;
            }
            while server.wants_write() {
                let mut buf = Vec::new();
                server.write_tls(&mut buf).unwrap();
                let mut cursor = &buf[..];
                while !cursor.is_empty() {
                    client.read_tls(&mut cursor).unwrap();
                }
                client.process_new_packets()?;
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok((client, server));
            }
        }
        panic!("handshake made no progress");
    }

    #[test]
    fn mutually_trusted_peers_complete_a_tls13_handshake() {
        let mut a = endpoint("machine-a");
        let mut b = endpoint("machine-b");
        trust(&mut a, &b);
        trust(&mut b, &a);

        let (client, server) = handshake(&a, &b).unwrap();

        assert_eq!(
            client.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        assert_eq!(
            server.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );

        // Each side can name the peer it actually authenticated, by
        // fingerprint — what the session layer will log and authorize on.
        let server_seen = server.peer_certificates().unwrap();
        assert_eq!(
            certificate_spki_fingerprint(&server_seen[0]).unwrap(),
            a.certified.fingerprint()
        );
        let client_seen = client.peer_certificates().unwrap();
        assert_eq!(
            certificate_spki_fingerprint(&client_seen[0]).unwrap(),
            b.certified.fingerprint()
        );
    }

    #[test]
    fn server_rejects_a_client_it_does_not_trust() {
        let mut a = endpoint("intruder-client");
        let b = endpoint("server");
        // Client trusts the server, but the server has never paired with
        // this client: threat T1 — reachability is not authorization.
        trust(&mut a, &b);

        assert!(handshake(&a, &b).is_err());
    }

    #[test]
    fn client_rejects_a_server_it_does_not_trust() {
        let a = endpoint("client");
        let mut b = endpoint("impostor-server");
        // Server would accept the client, but the client has never paired
        // with this server: threat T2's post-pairing analogue.
        trust(&mut b, &a);

        assert!(handshake(&a, &b).is_err());
    }

    #[test]
    fn mutual_strangers_cannot_connect() {
        let a = endpoint("stranger-a");
        let b = endpoint("stranger-b");
        assert!(handshake(&a, &b).is_err());
    }

    #[test]
    fn revocation_applies_to_rebuilt_configs() {
        let mut a = endpoint("machine-a");
        let mut b = endpoint("machine-b");
        trust(&mut a, &b);
        trust(&mut b, &a);
        assert!(handshake(&a, &b).is_ok());

        // Revoke A on B, rebuild configs (the documented snapshot
        // semantics): the next handshake must fail.
        let removed = b.trust.remove_by_peer_id(a.identity.device_id());
        assert!(removed.is_some());
        assert!(handshake(&a, &b).is_err());
    }

    #[test]
    fn trust_is_pinned_to_the_key_not_the_device_id() {
        let mut a = endpoint("machine-a");
        let mut b = endpoint("machine-b");
        trust(&mut a, &b);

        // B "trusts" a record carrying A's device id but a different key's
        // fingerprint. A's real handshake must still be rejected: the
        // UUID never authorizes (ADR 0003).
        let unrelated = endpoint("unrelated");
        b.trust
            .add_peer(
                TrustedPeer::new(
                    a.identity.device_id(),
                    a.identity.device_name(),
                    unrelated.certified.fingerprint(),
                )
                .unwrap(),
            )
            .unwrap();

        assert!(handshake(&a, &b).is_err());
    }
}
