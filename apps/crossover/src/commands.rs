//! Command implementations: pairing, trusted-peer management, status.
//!
//! Each command follows the same shape: open secure storage, load what it
//! needs, act, persist, and print a concise human summary — detailed
//! diagnostics go to structured logs (docs/ARCHITECTURE.md §9, §10).

use std::io::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use uuid::Uuid;

use crossover_core::pairing::{PairingListener, pair_with};
use crossover_protocol::DEFAULT_PORT;
use crossover_security::pairing::{PairedPeer, PairingCode, PairingIdentity};
use crossover_security::{DeviceIdentity, TrustStore, TrustedPeer};

use crate::storage::open_secure_storage;

/// One ceremony's allowance, listener and connector alike. Generous
/// enough to read a code off one screen and type it on another; bounded
/// because everything is (NFR-1).
const PAIRING_TIMEOUT: Duration = Duration::from_mins(2);

/// `crossover pair --listen [--bind <addr>]`
pub async fn pair_listen(device_name: &str, bind: Option<String>) -> anyhow::Result<()> {
    let storage = open_secure_storage()?;
    let (identity, generated) = DeviceIdentity::load_or_generate(&*storage, device_name)
        .context("loading device identity")?;
    if generated {
        println!(
            "Generated a new device identity for \"{}\".",
            identity.device_name()
        );
    }

    let bind = bind.unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
    let listener = PairingListener::bind(bind.as_str())
        .await
        .with_context(|| format!("binding pairing listener on {bind}"))?;
    let code = PairingCode::generate().context("generating pairing code")?;

    println!();
    println!(
        "Listening for a pairing attempt on {}.",
        listener.local_addr()?
    );
    println!();
    println!("    Pairing code:  {code}");
    println!();
    println!(
        "On the other machine, run:  crossover pair <this-machine-address:port>\n\
         and type the code when prompted. This code is valid for one\n\
         attempt, for {} seconds.",
        PAIRING_TIMEOUT.as_secs()
    );

    let peer = listener
        .accept_and_pair(local_pairing_identity(&identity)?, &code, PAIRING_TIMEOUT)
        .await
        .context("pairing failed")?;

    persist_paired_peer(&*storage, &peer, None)?;
    print_paired(&peer);
    Ok(())
}

/// `crossover pair <address>`
pub async fn pair_connect(device_name: &str, address: &str) -> anyhow::Result<()> {
    let storage = open_secure_storage()?;
    let (identity, generated) = DeviceIdentity::load_or_generate(&*storage, device_name)
        .context("loading device identity")?;
    if generated {
        println!(
            "Generated a new device identity for \"{}\".",
            identity.device_name()
        );
    }

    // Typing the code IS the verification ceremony (ADR 0002); it is
    // read interactively, never passed on the command line where shell
    // history would retain it.
    print!("Enter the pairing code shown on the other machine: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading pairing code")?;
    let code = PairingCode::parse(&line).context("invalid pairing code")?;

    let peer = pair_with(
        address,
        local_pairing_identity(&identity)?,
        &code,
        PAIRING_TIMEOUT,
    )
    .await
    .context("pairing failed")?;

    persist_paired_peer(&*storage, &peer, Some(address))?;
    print_paired(&peer);
    Ok(())
}

/// `crossover peers`
pub fn peers_list() -> anyhow::Result<()> {
    let storage = open_secure_storage()?;
    let store = TrustStore::load(&*storage).context("loading trust store")?;

    if store.peers().is_empty() {
        println!("No trusted peers. Pair one with `crossover pair`.");
        return Ok(());
    }

    println!("{} trusted peer(s):", store.peers().len());
    println!();
    for peer in store.peers() {
        println!("  {}  ({})", peer.device_name(), peer.peer_id());
        println!("      fingerprint:    {}", peer.fingerprint());
        println!("      first paired:   {}", age(peer.first_paired_unix()));
        println!(
            "      last connected: {}",
            peer.last_connected_unix()
                .map_or_else(|| "never".to_owned(), age)
        );
        if !peer.remembered_addresses().is_empty() {
            println!(
                "      addresses:      {}",
                peer.remembered_addresses().join(", ")
            );
        }
    }
    println!();
    println!("Revoke a peer with `crossover peers remove <device-id>`.");
    Ok(())
}

/// `crossover peers remove <device-id>`
pub fn peers_remove(device_id: Uuid) -> anyhow::Result<()> {
    let storage = open_secure_storage()?;
    let mut store = TrustStore::load(&*storage).context("loading trust store")?;

    let Some(removed) = store.remove_by_peer_id(device_id) else {
        anyhow::bail!("no trusted peer with device id {device_id}; `crossover peers` lists them");
    };
    store.save(&*storage).context("persisting trust store")?;

    println!(
        "Revoked \"{}\" ({}). Its connections will be rejected from now on.",
        removed.device_name(),
        removed.peer_id()
    );
    Ok(())
}

/// `crossover status`
pub fn status(device_name: &str) -> anyhow::Result<()> {
    let storage = open_secure_storage()?;

    match DeviceIdentity::load(&*storage).context("loading device identity")? {
        Some(identity) => {
            println!("Device identity");
            println!("  name:        {}", identity.device_name());
            println!("  device id:   {}", identity.device_id());
            println!("  fingerprint: {}", identity.spki_fingerprint()?);
            println!("  created:     {}", age(identity.created_at_unix()));
        }
        None => {
            println!(
                "No device identity yet (one will be generated as \"{device_name}\" \
                 on first pairing)."
            );
        }
    }

    let store = TrustStore::load(&*storage).context("loading trust store")?;
    println!();
    println!("Trusted peers: {}", store.peers().len());
    println!();
    println!(
        "Live session status will be reported here once `crossover run` \
         is implemented."
    );
    Ok(())
}

fn local_pairing_identity(identity: &DeviceIdentity) -> anyhow::Result<PairingIdentity> {
    Ok(PairingIdentity {
        device_id: identity.device_id(),
        device_name: identity.device_name().to_owned(),
        fingerprint: identity.spki_fingerprint()?,
    })
}

fn persist_paired_peer(
    storage: &dyn crossover_platform::SecureStorage,
    peer: &PairedPeer,
    dialed_address: Option<&str>,
) -> anyhow::Result<()> {
    let mut store = TrustStore::load(storage).context("loading trust store")?;
    let mut record = TrustedPeer::new(peer.device_id, &peer.device_name, peer.fingerprint)
        .context("building trust record")?;
    if let Some(address) = dialed_address {
        record.add_remembered_address(address)?;
    }
    let replaced = store.add_peer(record).context("recording trusted peer")?;
    store.save(storage).context("persisting trust store")?;
    if replaced {
        println!("(Refreshed an existing pairing with the same identity key.)");
    }
    Ok(())
}

fn print_paired(peer: &PairedPeer) {
    println!();
    println!("Paired with \"{}\" ({}).", peer.device_name, peer.device_id);
    println!("  pinned fingerprint: {}", peer.fingerprint);
}

/// Compact age for human output; structured logs carry exact values.
fn age(unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let elapsed = now.saturating_sub(unix);
    if elapsed < 60 {
        "just now".to_owned()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::age;

    #[test]
    fn ages_read_naturally() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(age(now), "just now");
        assert_eq!(age(now - 120), "2m ago");
        assert_eq!(age(now - 7200), "2h ago");
        assert_eq!(age(now - 200_000), "2d ago");
        // A future timestamp (clock skew) saturates instead of panicking.
        assert_eq!(age(now + 500), "just now");
    }
}
