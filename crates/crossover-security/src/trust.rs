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
/// protocol version. Version 2 added `file_receive` to the per-peer
/// permissions (ADR 0015).
const STORED_FORMAT_VERSION: u8 = 2;

/// The one older layout still readable: the four-flag permission record
/// that predates file receive. Its blobs are upgraded on load and written
/// back at [`STORED_FORMAT_VERSION`].
const STORED_FORMAT_VERSION_V1: u8 = 1;

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
// Five named booleans is the documented permission model (docs/SECURITY.md
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
    /// May send us files (ADR 0015). The only flag that reaches the
    /// filesystem, so it is **off by default** for every peer, existing
    /// records included, and turns on only by an explicit user grant
    /// (`crossover peers allow-files`) — never by pairing and never by an
    /// upgrade (docs/SECURITY.md invariant 8, §4).
    pub file_receive: bool,
}

impl PeerPermissions {
    /// What pairing grants: the input and clipboard capabilities the
    /// ceremony's text describes.
    ///
    /// `file_receive` is deliberately **not** part of it — pairing is not
    /// consent to a filesystem write surface (ADR 0015, docs/SECURITY.md
    /// invariant 8), so "full" here means full *pairing* capability, not
    /// every flag set.
    pub const FULL: Self = Self {
        keyboard: true,
        mouse: true,
        clipboard_send: true,
        clipboard_receive: true,
        file_receive: false,
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

    /// Whether this peer may send us files (ADR 0015).
    #[must_use]
    pub fn may_receive_files(&self) -> bool {
        self.permissions.file_receive
    }

    /// Grant or revoke this peer's file-receive permission, returning the
    /// value it replaced.
    ///
    /// The *only* way the flag is ever set: it takes an explicit local
    /// call with an explicit value, so nothing on the wire and no other
    /// mutation path can raise it (docs/SECURITY.md invariant 8).
    pub fn set_file_receive(&mut self, allowed: bool) -> bool {
        std::mem::replace(&mut self.permissions.file_receive, allowed)
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

/// Decode one at-rest layout; a decode failure is corruption, whichever
/// version it was.
fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, TrustStoreError> {
    postcard::from_bytes(bytes).map_err(|e| TrustStoreError::Corrupt {
        reason: e.to_string(),
    })
}

/// At-rest layout, format version 2 — what [`TrustStore::save`] writes.
#[derive(Serialize, Deserialize)]
struct StoredTrustStoreV2 {
    format_version: u8,
    peers: Vec<TrustedPeer>,
}

/// At-rest layout, format version 1 — **frozen**, read only.
///
/// The v1 shapes are spelled out here instead of being expressed in terms
/// of the live types, and that duplication is the point. postcard is a
/// non-self-describing format: fields are positional, with no names and no
/// defaults, so ADR 0015's "additive and optional, so existing files keep
/// loading" does not hold for this store. Appending `file_receive` to the
/// live [`PeerPermissions`] makes a v1 record decode out of step from the
/// fifth permission byte onwards, and the byte that lands where the new
/// flag is read is the length prefix of `remembered_addresses` — `1`, i.e.
/// **`file_receive: true`**, for any peer that has one. What follows then
/// desynchronizes, so the realistic outcome is a store that fails to load
/// at all; the unacceptable one is a store that loads with a
/// filesystem-write permission the user never gave. Neither is a migration.
///
/// A frozen v1 decoder with no such field, selected by the version byte
/// before any decoding, plus the literal `false` in
/// [`TrustedPeerV1::upgrade`], is what makes both unrepresentable
/// (docs/SECURITY.md invariant 8; ADR 0015).
#[derive(Serialize, Deserialize)]
struct StoredTrustStoreV1 {
    format_version: u8,
    peers: Vec<TrustedPeerV1>,
}

/// A v1 peer record: [`TrustedPeer`] as it was before file receive.
#[derive(Serialize, Deserialize)]
struct TrustedPeerV1 {
    peer_id: Uuid,
    device_name: String,
    fingerprint: SpkiFingerprint,
    first_paired_unix: u64,
    last_connected_unix: Option<u64>,
    permissions: PeerPermissionsV1,
    remembered_addresses: Vec<String>,
}

/// The v1 permission record: four flags, no file receive.
#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize, Deserialize)]
struct PeerPermissionsV1 {
    keyboard: bool,
    mouse: bool,
    clipboard_send: bool,
    clipboard_receive: bool,
}

impl TrustedPeerV1 {
    /// Upgrade to the current record. The store this came from could not
    /// have expressed consent to file receive, so it did not give it: the
    /// flag is a literal `false` here, not a default, not a carried-over
    /// byte, and not derived from anything in the blob.
    fn upgrade(self) -> TrustedPeer {
        TrustedPeer {
            peer_id: self.peer_id,
            device_name: self.device_name,
            fingerprint: self.fingerprint,
            first_paired_unix: self.first_paired_unix,
            last_connected_unix: self.last_connected_unix,
            permissions: PeerPermissions {
                keyboard: self.permissions.keyboard,
                mouse: self.permissions.mouse,
                clipboard_send: self.permissions.clipboard_send,
                clipboard_receive: self.permissions.clipboard_receive,
                file_receive: false,
            },
            remembered_addresses: self.remembered_addresses,
        }
    }
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
        // Dispatch on the version byte *before* decoding: each layout is
        // parsed by the decoder written for it, never by whichever decoder
        // happens to be current (see `StoredTrustStoreV1`).
        let peers = match bytes.first() {
            None => {
                return Err(TrustStoreError::Corrupt {
                    reason: "empty trust store blob".to_owned(),
                });
            }
            Some(&STORED_FORMAT_VERSION_V1) => {
                let stored: StoredTrustStoreV1 = decode(&bytes)?;
                stored
                    .peers
                    .into_iter()
                    .map(TrustedPeerV1::upgrade)
                    .collect()
            }
            Some(&STORED_FORMAT_VERSION) => {
                let stored: StoredTrustStoreV2 = decode(&bytes)?;
                stored.peers
            }
            Some(&version) => {
                return Err(TrustStoreError::UnsupportedFormatVersion {
                    found: version,
                    max: STORED_FORMAT_VERSION,
                });
            }
        };

        let store = Self { peers };
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
        let stored = StoredTrustStoreV2 {
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

    /// The record with this device UUID, or `None`. Bookkeeping lookup for
    /// the CLI; authorization is [`TrustStore::find_by_fingerprint`].
    #[must_use]
    pub fn find_by_peer_id(&self, peer_id: Uuid) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| p.peer_id == peer_id)
    }

    /// Grant or revoke a peer's file-receive permission, returning the
    /// value it replaced — or `None` if no peer has that device UUID.
    /// Callers persist with [`TrustStore::save`].
    ///
    /// Addressed by device UUID because that is what the user reads off
    /// `crossover peers` and types back; a grant is a local administrative
    /// act, so no peer-supplied value reaches this call.
    pub fn set_file_receive(&mut self, peer_id: Uuid, allowed: bool) -> Option<bool> {
        let peer = self.peers.iter_mut().find(|p| p.peer_id == peer_id)?;
        Some(peer.set_file_receive(allowed))
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
            Err(TrustStoreError::UnsupportedFormatVersion { found: 9, max: 2 })
        ));
    }

    #[test]
    fn stored_duplicate_fingerprints_are_corrupt() {
        // Serialize a store containing two records with one fingerprint,
        // bypassing add_peer's upsert, to prove load() enforces the
        // invariant independently.
        let stored = super::StoredTrustStoreV2 {
            format_version: super::STORED_FORMAT_VERSION,
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

    /// A trust store as written by the previous binary: format version 1,
    /// captured as bytes rather than re-encoded from today's types, so the
    /// fixture cannot drift when the live record changes. One peer, all
    /// four v1 flags granted, and **one remembered address** — so the byte
    /// sitting where the new flag would be read is that address count,
    /// `1`, the value a positional re-decode would take for consent.
    const V1_STORE_BLOB: &[u8] = &[
        0x01, 0x01, 0x10, 0x8F, 0x8B, 0x1A, 0x2C, 0x3D, 0x4E, 0x5F, 0x60, 0x71, 0x82, 0x93, 0xA4,
        0xB5, 0xC6, 0xD7, 0xE8, 0x11, 0x77, 0x6F, 0x72, 0x6B, 0x73, 0x74, 0x61, 0x74, 0x69, 0x6F,
        0x6E, 0x2D, 0x72, 0x69, 0x67, 0x68, 0x74, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0xE2, 0xCF, 0xAA, 0x06, 0x01,
        0xF4, 0xE5, 0xCF, 0xAA, 0x06, 0x01, 0x01, 0x01, 0x01, 0x01, 0x12, 0x31, 0x39, 0x32, 0x2E,
        0x31, 0x36, 0x38, 0x2E, 0x31, 0x2E, 0x32, 0x35, 0x3A, 0x32, 0x37, 0x36, 0x37, 0x37,
    ];

    #[test]
    fn previous_format_version_loads_with_file_receive_off() {
        let storage = InMemorySecureStorage::new();
        storage
            .store(TRUST_STORE_STORAGE_KEY, V1_STORE_BLOB)
            .unwrap();

        let loaded = TrustStore::load(&storage).unwrap();
        let peer = loaded.find_by_fingerprint(fingerprint(0xAA)).unwrap();
        // The whole point: an upgrade never confers a filesystem-write
        // permission (docs/SECURITY.md invariant 8, §4).
        assert!(!peer.may_receive_files());
        assert!(!peer.permissions().file_receive);
        // …and nothing else shifted, which is what proves the flag came
        // from the upgrade's literal `false` rather than from a blob byte
        // that decoded one field out of step.
        assert_eq!(peer.device_name(), "workstation-right");
        assert_eq!(peer.first_paired_unix(), 1_700_000_000);
        assert_eq!(peer.last_connected_unix(), Some(1_700_000_500));
        assert_eq!(peer.remembered_addresses(), &["192.168.1.25:27677"]);
        assert_eq!(
            peer.permissions(),
            PeerPermissions {
                keyboard: true,
                mouse: true,
                clipboard_send: true,
                clipboard_receive: true,
                file_receive: false,
            }
        );
    }

    #[test]
    fn the_two_layouts_are_genuinely_incompatible() {
        // Why the version byte was bumped instead of the field simply
        // appended: postcard is positional, so the current decoder reads a
        // v1 record out of step from the fifth permission byte onwards —
        // the byte landing on `file_receive` is the remembered-address
        // count, which is `1` (i.e. `true`) for any peer that has one.
        // Here the desync runs off the end instead, which is the *other*
        // failure: an upgrade that loses the whole store. Neither outcome
        // is acceptable, and the frozen v1 decoder is what avoids both —
        // this asserts it is load-bearing, not decoration.
        assert!(postcard::from_bytes::<super::StoredTrustStoreV2>(V1_STORE_BLOB).is_err());
    }

    #[test]
    fn v1_blob_fixture_matches_the_frozen_v1_encoder() {
        // Guards the fixture above against a typo, and the frozen decoder
        // against drift: they must still describe the same bytes.
        let stored = super::StoredTrustStoreV1 {
            format_version: super::STORED_FORMAT_VERSION_V1,
            peers: vec![super::TrustedPeerV1 {
                peer_id: Uuid::parse_str("8f8b1a2c-3d4e-5f60-7182-93a4b5c6d7e8").unwrap(),
                device_name: "workstation-right".to_owned(),
                fingerprint: fingerprint(0xAA),
                first_paired_unix: 1_700_000_000,
                last_connected_unix: Some(1_700_000_500),
                permissions: super::PeerPermissionsV1 {
                    keyboard: true,
                    mouse: true,
                    clipboard_send: true,
                    clipboard_receive: true,
                },
                remembered_addresses: vec!["192.168.1.25:27677".to_owned()],
            }],
        };
        assert_eq!(postcard::to_stdvec(&stored).unwrap(), V1_STORE_BLOB);
    }

    #[test]
    fn upgraded_store_is_rewritten_at_the_current_version() {
        let storage = InMemorySecureStorage::new();
        storage
            .store(TRUST_STORE_STORAGE_KEY, V1_STORE_BLOB)
            .unwrap();

        let loaded = TrustStore::load(&storage).unwrap();
        loaded.save(&storage).unwrap();
        let rewritten = storage.load(TRUST_STORE_STORAGE_KEY).unwrap().unwrap();
        assert_eq!(rewritten.first(), Some(&super::STORED_FORMAT_VERSION));
        // A rewrite is not a grant either.
        assert!(!TrustStore::load(&storage).unwrap().peers()[0].may_receive_files());
    }

    #[test]
    fn pairing_does_not_grant_file_receive() {
        // Pairing consents to input and clipboard, not to the filesystem
        // (ADR 0015; docs/SECURITY.md invariant 8).
        // Const-asserted: the grant that pairing hands out cannot acquire
        // the flag without failing the build.
        const { assert!(!PeerPermissions::FULL.file_receive) };
        assert!(!PeerPermissions::default().file_receive);
        assert!(!peer(0xAA, "freshly-paired").may_receive_files());
    }

    #[test]
    fn file_receive_grant_and_revoke_survive_a_reload() {
        let storage = InMemorySecureStorage::new();
        let mut store = TrustStore::new();
        let record = peer(0xAA, "sender");
        let id = record.peer_id();
        store.add_peer(record).unwrap();
        store.add_peer(peer(0xBB, "bystander")).unwrap();

        assert_eq!(store.set_file_receive(id, true), Some(false));
        store.save(&storage).unwrap();

        let reloaded = TrustStore::load(&storage).unwrap();
        assert!(reloaded.find_by_peer_id(id).unwrap().may_receive_files());
        // The grant is per peer: nobody else moved.
        assert!(
            !reloaded
                .find_by_fingerprint(fingerprint(0xBB))
                .unwrap()
                .may_receive_files()
        );

        let mut store = reloaded;
        assert_eq!(store.set_file_receive(id, false), Some(true));
        store.save(&storage).unwrap();
        assert!(
            !TrustStore::load(&storage)
                .unwrap()
                .find_by_peer_id(id)
                .unwrap()
                .may_receive_files()
        );
    }

    #[test]
    fn granting_an_unknown_peer_changes_nothing() {
        let mut store = TrustStore::new();
        store.add_peer(peer(0xAA, "known")).unwrap();
        assert_eq!(store.set_file_receive(Uuid::new_v4(), true), None);
        assert!(!store.peers()[0].may_receive_files());
    }

    #[test]
    fn re_pairing_a_granted_peer_drops_the_grant() {
        // add_peer replaces the record wholesale, and a fresh record is
        // PeerPermissions::FULL — so re-pairing fails closed rather than
        // carrying a stale filesystem grant across a new ceremony.
        let mut store = TrustStore::new();
        let record = peer(0xAA, "sender");
        let id = record.peer_id();
        store.add_peer(record).unwrap();
        store.set_file_receive(id, true);

        assert!(store.add_peer(peer(0xAA, "sender")).unwrap());
        assert!(!store.peers()[0].may_receive_files());
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
