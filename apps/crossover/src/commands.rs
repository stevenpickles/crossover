//! Command implementations: pairing, trusted-peer management, status.
//!
//! Each command follows the same shape: open secure storage, load what it
//! needs, act, persist, and print a concise human summary — detailed
//! diagnostics go to structured logs (docs/ARCHITECTURE.md §9, §10).

use std::io::Write as _;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crossover_core::pairing::{PairingListener, pair_with};
use crossover_core::supervision::{
    KeepaliveConfig, SessionEvent, SupervisorConfig, run_session, supervise_outbound,
};
use crossover_core::{
    ClipboardConfig, LocalNode, SessionListener, SessionOptions, SyncCommand, SyncEvent,
    clipboard_sync,
};
use crossover_platform::SecureStorage;
use crossover_protocol::DEFAULT_PORT;
use crossover_security::pairing::{PairedPeer, PairingCode, PairingIdentity};
use crossover_security::{
    CertifiedIdentity, DeviceIdentity, SpkiFingerprint, TrustStore, TrustedPeer,
};

use crate::storage::{open_clipboard_provider, open_secure_storage};

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

/// `crossover run [--listen [--bind <addr>]] [--connect <addr>]`
///
/// Foreground session maintenance (docs/ROADMAP.md Phase 1): accept
/// trusted peers, keep an outbound session alive with reconnect, log
/// every establishment and loss, and stop on Ctrl-C. Clipboard and input
/// engines attach to these sessions in later phases.
pub async fn run(
    device_name: &str,
    listen_bind: Option<String>,
    connect: Option<String>,
) -> anyhow::Result<()> {
    let storage: Arc<dyn SecureStorage> = Arc::from(open_secure_storage()?);
    let (identity, generated) = DeviceIdentity::load_or_generate(&*storage, device_name)
        .context("loading device identity")?;
    if generated {
        println!(
            "Generated a new device identity for \"{}\".",
            identity.device_name()
        );
    }
    let certified = CertifiedIdentity::from_identity(&identity).context("certifying identity")?;

    let store = TrustStore::load(&*storage).context("loading trust store")?;
    anyhow::ensure!(
        !store.peers().is_empty(),
        "no trusted peers - pair one first with `crossover pair`"
    );

    println!(
        "Running as \"{}\" ({} trusted peer(s)); fingerprint {}",
        identity.device_name(),
        store.peers().len(),
        identity.spki_fingerprint()?
    );

    // Clipboard sync: one driver for the peer relationship; sessions of
    // either role feed it and carry its frames.
    let provider = open_clipboard_provider()?;
    let (sync_driver, sync_events, sync_commands) =
        clipboard_sync(provider, identity.device_id(), ClipboardConfig::new())
            .context("starting clipboard sync")?;
    tokio::spawn(sync_driver.run());

    // The mux's view of the current inbound session: its outbound sender
    // and its kill switch (for the driver's fail-closed verdicts).
    let listener_slot: SessionSlotRef = Arc::new(std::sync::Mutex::new(None));

    // Outbound role: supervised session with automatic reconnect.
    let (handle, events) = match &connect {
        Some(addr) => {
            println!("Maintaining an outbound session to {addr}.");
            let (handle, events) = supervise_outbound(
                addr.clone(),
                identity.clone(),
                certified.clone(),
                Arc::new(RwLock::new(store.clone())),
                SupervisorConfig::default(),
            );
            (Some(Arc::new(handle)), Some(events))
        }
        None => (None, None),
    };

    spawn_command_mux(handle.clone(), Arc::clone(&listener_slot), sync_commands);

    // Inbound role: accept loop, one session at a time (two-machine scope).
    let listener = match &listen_bind {
        Some(bind) => {
            let listener = SessionListener::bind(bind.as_str())
                .await
                .with_context(|| format!("binding listener on {bind}"))?;
            println!("Listening for trusted peers on {}.", listener.local_addr()?);
            Some(listener)
        }
        None => None,
    };

    println!("Press Ctrl-C to stop.");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.context("waiting for Ctrl-C")?;
            println!("Shutting down.");
            if let Some(handle) = &handle {
                handle.shutdown();
            }
        }
        () = listener_loop(
            listener.as_ref(),
            &identity,
            &certified,
            &storage,
            &sync_events,
            &listener_slot,
        ) => {}
        () = outbound_event_loop(events, &storage, &sync_events) => {}
    }
    Ok(())
}

/// Accept trusted peers forever; each session runs the shared session
/// loop (pings answered, frames logged) until it ends, then accept again.
type SessionSlotRef =
    Arc<std::sync::Mutex<Option<(mpsc::Sender<(u16, Vec<u8>)>, watch::Sender<bool>)>>>;

/// Route driver commands to every active session; enforce fail-closed
/// terminations on the inbound session. (The supervisor offers no
/// per-session kill — an accepted limitation logged when it matters: a
/// malformed payload from a trusted, authenticated peer.)
fn spawn_command_mux(
    handle: Option<Arc<crossover_core::supervision::SupervisorHandle>>,
    listener_slot: SessionSlotRef,
    mut sync_commands: mpsc::Receiver<SyncCommand>,
) {
    tokio::spawn(async move {
        while let Some(command) = sync_commands.recv().await {
            match command {
                SyncCommand::SendFrame {
                    message_type,
                    payload,
                } => {
                    if let Some(handle) = &handle {
                        let _ = handle.send(message_type, payload.clone()).await;
                    }
                    let maybe_tx = listener_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .map(|(tx, _)| tx.clone());
                    if let Some(tx) = maybe_tx {
                        let _ = tx.send((message_type, payload)).await;
                    }
                }
                SyncCommand::TerminateSession { reason } => {
                    tracing::error!(
                        error = %reason,
                        "clipboard payload violation; terminating inbound session"
                    );
                    let maybe_kill = listener_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .map(|(_, kill)| kill.clone());
                    if let Some(kill) = maybe_kill {
                        let _ = kill.send(true);
                    }
                }
            }
        }
    });
}

async fn listener_loop(
    listener: Option<&SessionListener>,
    identity: &DeviceIdentity,
    certified: &CertifiedIdentity,
    storage: &Arc<dyn SecureStorage>,
    sync_events: &mpsc::Sender<SyncEvent>,
    session_slot: &SessionSlotRef,
) {
    let Some(listener) = listener else {
        return std::future::pending().await;
    };
    let options = SessionOptions::default();
    let keepalive = KeepaliveConfig::default();
    loop {
        // Fresh trust per accept: pairings and revocations made by other
        // crossover invocations apply to every new connection.
        let trust = match TrustStore::load(&**storage) {
            Ok(trust) => trust,
            Err(error) => {
                tracing::error!(error = %error, "trust store unreadable; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let local = LocalNode {
            identity,
            certified,
            trust: &trust,
        };
        match listener.accept(&local, &options).await {
            Ok(session) => {
                let info = session.info().clone();
                println!(
                    "Session established with \"{}\" (inbound).",
                    info.peer_device_name
                );
                touch_last_connected(&**storage, info.peer_fingerprint);

                let (events_tx, mut events_rx) = mpsc::channel(64);
                let (outbound_tx, mut outbound_rx) = mpsc::channel::<(u16, Vec<u8>)>(64);
                let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
                *session_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((outbound_tx, shutdown_tx));
                let _ = sync_events.send(SyncEvent::SessionEstablished).await;

                let frame_sink = sync_events.clone();
                let drain = tokio::spawn(async move {
                    while let Some(event) = events_rx.recv().await {
                        if let SessionEvent::Frame(frame) = event {
                            let _ = frame_sink.send(SyncEvent::Frame(frame)).await;
                        }
                    }
                });
                let reason = run_session(
                    session,
                    &events_tx,
                    &mut outbound_rx,
                    &mut shutdown_rx,
                    &keepalive,
                )
                .await;
                drop(events_tx);
                let _ = drain.await;
                *session_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                let _ = sync_events.send(SyncEvent::SessionLost).await;
                println!(
                    "Session with \"{}\" ended: {reason}.",
                    info.peer_device_name
                );
            }
            Err(error) => {
                tracing::warn!(error = %error, "inbound session failed");
                // Avoid a hot loop if the failure is instantaneous.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Narrate the outbound supervisor events; pends forever when there is no
/// outbound role.
async fn outbound_event_loop(
    events: Option<mpsc::Receiver<SessionEvent>>,
    storage: &Arc<dyn SecureStorage>,
    sync_events: &mpsc::Sender<SyncEvent>,
) {
    let Some(mut events) = events else {
        return std::future::pending().await;
    };
    while let Some(event) = events.recv().await {
        match event {
            SessionEvent::Established(info) => {
                println!(
                    "Session established with \"{}\" (outbound).",
                    info.peer_device_name
                );
                touch_last_connected(&**storage, info.peer_fingerprint);
                let _ = sync_events.send(SyncEvent::SessionEstablished).await;
            }
            SessionEvent::Disconnected {
                reason, retry_in, ..
            } => {
                match retry_in {
                    Some(delay) => println!(
                        "Outbound session ended ({reason}); retrying in {}s.",
                        delay.as_secs_f32()
                    ),
                    None => println!("Outbound session ended ({reason})."),
                }
                let _ = sync_events.send(SyncEvent::SessionLost).await;
            }
            SessionEvent::ConnectFailed { error, retry_in } => {
                println!(
                    "Connect failed ({error}); retrying in {}s.",
                    retry_in.as_secs_f32()
                );
            }
            SessionEvent::Frame(frame) => {
                let _ = sync_events.send(SyncEvent::Frame(frame)).await;
            }
        }
        // A Disconnected event also voids sync state.
    }
}

/// Best-effort bookkeeping: record a successful connection in the trust
/// store. Failure is logged, never fatal - bookkeeping must not kill a
/// healthy session.
fn touch_last_connected(storage: &dyn SecureStorage, fingerprint: SpkiFingerprint) {
    let result = TrustStore::load(storage).and_then(|mut store| {
        if store.record_connection(fingerprint) {
            store.save(storage)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!(error = %error, "could not record last-connected time");
    }
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
