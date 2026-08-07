//! The trusted peer store (FR-1.3, FR-1.4, docs/SECURITY.md §4).
//!
//! The store is the single authority on peer authorization: the TLS
//! verifiers ask it "is this SPKI fingerprint trusted?", and removal is
//! revocation. Records hold **no private keys** — only peers' public
//! credentials and bookkeeping — so theft of the store must not enable
//! impersonation (threat T7).
//!
//! Persistence goes through [`SecureStorage`]. Not because the contents
//! are secret (they are not), but because it is the abstraction the
//! workspace already has with an atomic-replace contract and test fakes —
//! and DPAPI-backing adds tamper resistance against other local users and
//! offline modification at zero design cost.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crossover_platform::{SecureStorage, SecureStorageError};

use crate::identity::{IdentityError, SpkiFingerprint, unix_now, validate_device_name};

/// Key under which the trust store blob lives in [`SecureStorage`].
pub const TRUST_STORE_STORAGE_KEY: &str = "trusted-peers";

/// Maximum trusted peers. The initial product scope is two machines; the
/// bound is generous headroom, not a target (NFR-1: everything bounded).
pub const MAX_TRUSTED_PEERS: usize = 32;

/// Maximum remembered addresses per peer.
pub const MAX_REMEMBERED_ADDRESSES: usize = 8;

/// Maximum bytes for one remembered address string.
pub const MAX_ADDRESS_BYTES: usize = 256;

/// Version of the at-rest trust-store blob; independent of the wire
/// protocol version.
const STORED_FORMAT_VERSION: u8 = 1;

/// Failures in trust-store persistence or mutation.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum TrustStoreError {
    /// The platform secure-storage backend failed.
    #[error(transparent)]
    Storage(#[from] SecureStorageError),

    /// A stored blob exists but cannot be decoded or fails validation.
    #[error("stored trust store is corrupt: {reason}")]
    Corrupt { reason: String },

    /// The stored blob was written by a newer layout.
    #[error("trust store format version {found} is unsupported (max {max})")]
    UnsupportedFormatVersion { found: u8, max: u8 },

    /// Encoding the store for persistence failed.
    #[error("encoding trust store failed: {reason}")]
    Encode { reason: String },

    /// The store already holds [`MAX_TRUSTED_PEERS`] records.
    #[error("trust store is full ({max} peers)")]
    StoreFull { max: usize },

    /// A record failed validation (bad name, oversized addresses, …).
    #[error("invalid trusted-peer record: {reason}")]
    InvalidRecord { reason: String },
}

/// Per-peer capability flags (docs/SECURITY.md §4).
///
/// The data model is granular from day one so enforcement can arrive
/// later without a storage migration; pairing currently grants
/// [`PeerPermissions::FULL`].
// Four named booleans is the documented permission model (docs/SECURITY.md
// §4) and the stored wire shape; a bitmask would trade grep-able field
// names for nothing at this scale.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPermissions {
    /// May receive our keyboard input.
    pub keyboard: bool,
    /// May receive our pointer input.
    pub mouse: bool,
    /// May be sent our clipboard contents.
    pub clipboard_send: bool,
    /// May write into our clipboard.
    pub clipboard_receive: bool,
}

impl PeerPermissions {
    /// Full capability — what pairing grants initially.
    pub const FULL: Self = Self {
        keyboard: true,
        mouse: true,
        clipboard_send: true,
        clipboard_receive: true,
    };
}

impl Default for PeerPermissions {
    fn default() -> Self {
        Self::FULL
    }
}

/// One trusted peer: public credential plus bookkeeping. No secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    peer_id: Uuid,
    device_name: String,
    fingerprint: SpkiFingerprint,
    first_paired_unix: u64,
    last_connected_unix: Option<u64>,
    permissions: PeerPermissions,
    remembered_addresses: Vec<String>,
}

impl TrustedPeer {
    /// A freshly paired peer: full permissions, paired now, never yet
    /// connected, no remembered addresses.
    ///
    /// # Errors
    ///
    /// [`TrustStoreError::InvalidRecord`] for an empty or oversized name.
    pub fn new(
        peer_id: Uuid,
        device_name: &str,
        fingerprint: SpkiFingerprint,
    ) -> Result<Self, TrustStoreError> {
        let peer = Self {
            peer_id,
            device_name: device_name.to_owned(),
            fingerprint,
            first_paired_unix: unix_now(),
            last_connected_unix: None,
            permissions: PeerPermissions::FULL,
            remembered_addresses: Vec::new(),
        };
        peer.validate()?;
        Ok(peer)
    }

    fn validate(&self) -> Result<(), TrustStoreError> {
        validate_device_name(&self.device_name).map_err(|e| match e {
            IdentityError::InvalidDeviceName { got } => TrustStoreError::InvalidRecord {
                reason: format!("device name of {got} bytes is out of bounds"),
            },
            other => TrustStoreError::InvalidRecord {
                reason: other.to_string(),
            },
        })?;
        if self.remembered_addresses.len() > MAX_REMEMBERED_ADDRESSES {
            return Err(TrustStoreError::InvalidRecord {
                reason: format!(
                    "{} remembered addresses exceeds maximum {MAX_REMEMBERED_ADDRESSES}",
                    self.remembered_addresses.len()
                ),
            });
        }
        if let Some(oversized) = self
            .remembered_addresses
            .iter()
            .find(|a| a.is_empty() || a.len() > MAX_ADDRESS_BYTES)
        {
            return Err(TrustStoreError::InvalidRecord {
                reason: format!(
                    "remembered address of {} bytes is out of 1..={MAX_ADDRESS_BYTES}",
                    oversized.len()
                ),
            });
        }
        Ok(())
    }

    /// Peer device UUID (bookkeeping identity; the fingerprint authorizes).
    #[must_use]
    pub fn peer_id(&self) -> Uuid {
        self.peer_id
    }

    /// Peer's human-readable device name.
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The pinned credential: SPKI SHA-256 fingerprint (ADR 0003).
    #[must_use]
    pub fn fingerprint(&self) -> SpkiFingerprint {
        self.fingerprint
    }

    /// When the peer was first paired (Unix seconds).
    #[must_use]
    pub fn first_paired_unix(&self) -> u64 {
        self.first_paired_unix
    }

    /// Last successful authenticated connection, if any (Unix seconds).
    #[must_use]
    pub fn last_connected_unix(&self) -> Option<u64> {
        self.last_connected_unix
    }

    /// Granted capabilities.
    #[must_use]
    pub fn permissions(&self) -> PeerPermissions {
        self.permissions
    }

    /// Known addresses for reconnection attempts.
    #[must_use]
    pub fn remembered_addresses(&self) -> &[String] {
        &self.remembered_addresses
    }

    /// Remember an address this peer was reached at (deduplicated; most
    /// recent last). Bounded like everything else: adding beyond
    /// [`MAX_REMEMBERED_ADDRESSES`] drops the oldest entry.
    ///
    /// # Errors
    ///
    /// [`TrustStoreError::InvalidRecord`] for an empty or oversized
    /// address string.
    pub fn add_remembered_address(&mut self, address: &str) -> Result<(), TrustStoreError> {
        if address.is_empty() || address.len() > MAX_ADDRESS_BYTES {
            return Err(TrustStoreError::InvalidRecord {
                reason: format!(
                    "remembered address of {} bytes is out of 1..={MAX_ADDRESS_BYTES}",
                    address.len()
                ),
            });
        }
        self.remembered_addresses.retain(|a| a != address);
        if self.remembered_addresses.len() >= MAX_REMEMBERED_ADDRESSES {
            self.remembered_addresses.remove(0);
        }
        self.remembered_addresses.push(address.to_owned());
        Ok(())
    }
}

/// At-rest layout, format version 1.
#[derive(Serialize, Deserialize)]
struct StoredTrustStoreV1 {
    format_version: u8,
    peers: Vec<TrustedPeer>,
}

/// The set of trusted peers, indexed by credential fingerprint.
///
/// `Clone` supports snapshot semantics: long-lived tasks clone the store
/// under a short lock and build TLS configs from the snapshot, so trust
/// changes apply to every subsequent establishment without locks held
/// across awaits.
#[derive(Debug, Default, Clone)]
pub struct TrustStore {
    peers: Vec<TrustedPeer>,
}

impl TrustStore {
    /// An empty store (a machine that has never paired).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the persisted store; absent storage yields an empty store —
    /// "never paired" is a normal state, not an error.
    ///
    /// # Errors
    ///
    /// [`TrustStoreError::Storage`] on backend failure;
    /// [`TrustStoreError::UnsupportedFormatVersion`] for a newer layout;
    /// [`TrustStoreError::Corrupt`] for undecodable or invalid contents.
    pub fn load(storage: &dyn SecureStorage) -> Result<Self, TrustStoreError> {
        let Some(bytes) = storage.load(TRUST_STORE_STORAGE_KEY)? else {
            return Ok(Self::new());
        };
        match bytes.first() {
            None => {
                return Err(TrustStoreError::Corrupt {
                    reason: "empty trust store blob".to_owned(),
                });
            }
            Some(&version) if version != STORED_FORMAT_VERSION => {
                return Err(TrustStoreError::UnsupportedFormatVersion {
                    found: version,
                    max: STORED_FORMAT_VERSION,
                });
            }
            Some(_) => {}
        }
        let stored: StoredTrustStoreV1 =
            postcard::from_bytes(&bytes).map_err(|e| TrustStoreError::Corrupt {
                reason: e.to_string(),
            })?;

        let store = Self {
            peers: stored.peers,
        };
        if store.peers.len() > MAX_TRUSTED_PEERS {
            return Err(TrustStoreError::Corrupt {
                reason: format!(
                    "{} peers exceeds maximum {MAX_TRUSTED_PEERS}",
                    store.peers.len()
                ),
            });
        }
        for peer in &store.peers {
            peer.validate().map_err(|e| TrustStoreError::Corrupt {
                reason: format!("peer {}: {e}", peer.peer_id),
            })?;
        }
        let mut fingerprints: Vec<_> = store.peers.iter().map(|p| p.fingerprint.0).collect();
        fingerprints.sort_unstable();
        fingerprints.dedup();
        if fingerprints.len() != store.peers.len() {
            return Err(TrustStoreError::Corrupt {
                reason: "duplicate fingerprints in stored trust store".to_owned(),
            });
        }
        Ok(store)
    }

    /// Persist the store.
    ///
    /// # Errors
    ///
    /// [`TrustStoreError::Encode`] on serialization failure;
    /// [`TrustStoreError::Storage`] on backend failure.
    pub fn save(&self, storage: &dyn SecureStorage) -> Result<(), TrustStoreError> {
        let stored = StoredTrustStoreV1 {
            format_version: STORED_FORMAT_VERSION,
            peers: self.peers.clone(),
        };
        let bytes = postcard::to_stdvec(&stored).map_err(|e| TrustStoreError::Encode {
            reason: e.to_string(),
        })?;
        storage.store(TRUST_STORE_STORAGE_KEY, &bytes)?;
        Ok(())
    }

    /// Add a peer, or refresh the existing record holding the same
    /// fingerprint (same fingerprint = same key holder, so re-pairing
    /// updates bookkeeping rather than duplicating identity). Returns
    /// `true` if an existing record was replaced.
    ///
    /// # Errors
    ///
    /// [`TrustStoreError::StoreFull`] when adding a *new* peer would
    /// exceed [`MAX_TRUSTED_PEERS`].
    pub fn add_peer(&mut self, peer: TrustedPeer) -> Result<bool, TrustStoreError> {
        if let Some(existing) = self
            .peers
            .iter_mut()
            .find(|p| p.fingerprint == peer.fingerprint)
        {
            *existing = peer;
            return Ok(true);
        }
        if self.peers.len() >= MAX_TRUSTED_PEERS {
            return Err(TrustStoreError::StoreFull {
                max: MAX_TRUSTED_PEERS,
            });
        }
        self.peers.push(peer);
        Ok(false)
    }

    /// The authorization check (docs/SECURITY.md §4): the record pinned to
    /// `fingerprint`, or `None` — and `None` means *reject*.
    #[must_use]
    pub fn find_by_fingerprint(&self, fingerprint: SpkiFingerprint) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| p.fingerprint == fingerprint)
    }

    /// Remove (revoke) a peer by device UUID, returning the removed record.
    /// `None` means no such peer. Callers persist with
    /// [`TrustStore::save`] and terminate live sessions (FR-1.4).
    pub fn remove_by_peer_id(&mut self, peer_id: Uuid) -> Option<TrustedPeer> {
        let index = self.peers.iter().position(|p| p.peer_id == peer_id)?;
        Some(self.peers.remove(index))
    }

    /// Record a successful authenticated connection right now. Returns
    /// `false` if the fingerprint is not trusted (nothing recorded).
    pub fn record_connection(&mut self, fingerprint: SpkiFingerprint) -> bool {
        match self.peers.iter_mut().find(|p| p.fingerprint == fingerprint) {
            Some(peer) => {
                peer.last_connected_unix = Some(unix_now());
                true
            }
            None => false,
        }
    }

    /// All trusted peers, in pairing order (CLI listing).
    #[must_use]
    pub fn peers(&self) -> &[TrustedPeer] {
        &self.peers
    }
}

#[cfg(test)]
mod tests {
    use crossover_platform::SecureStorage;
    use crossover_platform::fakes::InMemorySecureStorage;
    use uuid::Uuid;

    use super::{
        MAX_TRUSTED_PEERS, PeerPermissions, TRUST_STORE_STORAGE_KEY, TrustStore, TrustStoreError,
        TrustedPeer,
    };
    use crate::identity::SpkiFingerprint;

    fn fingerprint(fill: u8) -> SpkiFingerprint {
        SpkiFingerprint([fill; 32])
    }

    fn peer(fill: u8, name: &str) -> TrustedPeer {
        TrustedPeer::new(Uuid::new_v4(), name, fingerprint(fill)).unwrap()
    }

    #[test]
    fn absent_storage_loads_an_empty_store() {
        let storage = InMemorySecureStorage::new();
        let store = TrustStore::load(&storage).unwrap();
        assert!(store.peers().is_empty());
    }

    #[test]
    fn add_save_load_round_trips_every_field() {
        let storage = InMemorySecureStorage::new();
        let mut store = TrustStore::new();
        let original = peer(0xAA, "workstation-right");
        assert!(!store.add_peer(original.clone()).unwrap());
        store.save(&storage).unwrap();

        let loaded = TrustStore::load(&storage).unwrap();
        assert_eq!(loaded.peers(), std::slice::from_ref(&original));
        let found = loaded.find_by_fingerprint(fingerprint(0xAA)).unwrap();
        assert_eq!(found.device_name(), "workstation-right");
        assert_eq!(found.permissions(), PeerPermissions::FULL);
        assert_eq!(found.last_connected_unix(), None);
        assert!(found.remembered_addresses().is_empty());
    }

    #[test]
    fn authorization_is_by_fingerprint_and_default_deny() {
        let mut store = TrustStore::new();
        store.add_peer(peer(0xAA, "known")).unwrap();
        assert!(store.find_by_fingerprint(fingerprint(0xAA)).is_some());
        // Unknown fingerprint: None, which callers treat as reject.
        assert!(store.find_by_fingerprint(fingerprint(0xBB)).is_none());
    }

    #[test]
    fn same_fingerprint_repairing_replaces_not_duplicates() {
        let mut store = TrustStore::new();
        store.add_peer(peer(0xAA, "old-name")).unwrap();
        let replaced = store.add_peer(peer(0xAA, "new-name")).unwrap();
        assert!(replaced);
        assert_eq!(store.peers().len(), 1);
        assert_eq!(
            store
                .find_by_fingerprint(fingerprint(0xAA))
                .unwrap()
                .device_name(),
            "new-name"
        );
    }

    #[test]
    fn removal_revokes_authorization() {
        let mut store = TrustStore::new();
        let trusted = peer(0xAA, "revoke-me");
        let id = trusted.peer_id();
        store.add_peer(trusted).unwrap();

        let removed = store.remove_by_peer_id(id).unwrap();
        assert_eq!(removed.peer_id(), id);
        assert!(store.find_by_fingerprint(fingerprint(0xAA)).is_none());
        // Removing again: gone is gone.
        assert!(store.remove_by_peer_id(id).is_none());
    }

    #[test]
    fn record_connection_touches_only_known_peers() {
        let mut store = TrustStore::new();
        store.add_peer(peer(0xAA, "known")).unwrap();

        assert!(store.record_connection(fingerprint(0xAA)));
        assert!(
            store
                .find_by_fingerprint(fingerprint(0xAA))
                .unwrap()
                .last_connected_unix()
                .is_some()
        );
        assert!(!store.record_connection(fingerprint(0xBB)));
    }

    #[test]
    fn store_full_rejects_new_peers_but_allows_refresh() {
        let mut store = TrustStore::new();
        for i in 0..MAX_TRUSTED_PEERS {
            store
                .add_peer(peer(u8::try_from(i).unwrap(), "peer"))
                .unwrap();
        }
        assert!(matches!(
            store.add_peer(peer(0xFE, "one-too-many")),
            Err(TrustStoreError::StoreFull { .. })
        ));
        // Refreshing an existing fingerprint still works at capacity.
        assert!(store.add_peer(peer(0, "refreshed")).unwrap());
    }

    #[test]
    fn corrupt_and_newer_format_blobs_fail_distinctly() {
        let storage = InMemorySecureStorage::new();
        storage
            .store(TRUST_STORE_STORAGE_KEY, &[1, 0xFF, 0xFF])
            .unwrap();
        assert!(matches!(
            TrustStore::load(&storage),
            Err(TrustStoreError::Corrupt { .. })
        ));

        storage.store(TRUST_STORE_STORAGE_KEY, &[9, 0]).unwrap();
        assert!(matches!(
            TrustStore::load(&storage),
            Err(TrustStoreError::UnsupportedFormatVersion { found: 9, max: 1 })
        ));
    }

    #[test]
    fn stored_duplicate_fingerprints_are_corrupt() {
        // Serialize a store containing two records with one fingerprint,
        // bypassing add_peer's upsert, to prove load() enforces the
        // invariant independently.
        let stored = super::StoredTrustStoreV1 {
            format_version: 1,
            peers: vec![peer(0xAA, "one"), peer(0xAA, "two")],
        };
        let bytes = postcard::to_stdvec(&stored).unwrap();
        let storage = InMemorySecureStorage::new();
        storage.store(TRUST_STORE_STORAGE_KEY, &bytes).unwrap();
        assert!(matches!(
            TrustStore::load(&storage),
            Err(TrustStoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn remembered_addresses_dedupe_and_stay_bounded() {
        let mut record = peer(0xAA, "machine");
        record.add_remembered_address("192.168.1.25:27677").unwrap();
        record.add_remembered_address("10.0.0.9:27677").unwrap();
        // Re-adding moves it to most-recent, not duplicated.
        record.add_remembered_address("192.168.1.25:27677").unwrap();
        assert_eq!(
            record.remembered_addresses(),
            &["10.0.0.9:27677", "192.168.1.25:27677"]
        );

        for i in 0..super::MAX_REMEMBERED_ADDRESSES + 2 {
            record
                .add_remembered_address(&format!("10.0.0.{i}:1"))
                .unwrap();
        }
        assert_eq!(
            record.remembered_addresses().len(),
            super::MAX_REMEMBERED_ADDRESSES
        );

        assert!(matches!(
            record.add_remembered_address(""),
            Err(TrustStoreError::InvalidRecord { .. })
        ));
        assert!(matches!(
            record.add_remembered_address(&"x".repeat(300)),
            Err(TrustStoreError::InvalidRecord { .. })
        ));
    }

    #[test]
    fn invalid_names_rejected_at_construction() {
        assert!(matches!(
            TrustedPeer::new(Uuid::new_v4(), "", fingerprint(0xAA)),
            Err(TrustStoreError::InvalidRecord { .. })
        ));
        assert!(matches!(
            TrustedPeer::new(Uuid::new_v4(), &"x".repeat(65), fingerprint(0xAA)),
            Err(TrustStoreError::InvalidRecord { .. })
        ));
    }

    #[test]
    fn storage_failures_propagate() {
        let storage = InMemorySecureStorage::new();
        storage.fail_next_operation("backend unavailable");
        assert!(matches!(
            TrustStore::load(&storage),
            Err(TrustStoreError::Storage(_))
        ));
    }
}
