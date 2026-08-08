//! Command implementations: pairing, trusted-peer management, status.
//!
//! Each command follows the same shape: open secure storage, load what it
//! needs, act, persist, and print a concise human summary — detailed
//! diagnostics go to structured logs (docs/ARCHITECTURE.md §9, §10).

use std::io::Write as _;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crossover_core::pairing::{PairingListener, pair_with};
use crossover_core::supervision::{
    KeepaliveConfig, SessionEvent, SupervisorConfig, run_session, supervise_outbound,
};
use crossover_core::{
    ClipboardConfig, ControlConfig, ControlNotice, FrameTarget, InputControlEvent, LocalNode,
    Metrics, SessionCommand, SessionListener, SessionOptions, SyncEvent, clipboard_sync,
    input_control,
};
use crossover_platform::SecureStorage;
use crossover_protocol::DEFAULT_PORT;
use crossover_protocol::hello::MessageType;
use crossover_protocol::input::{InputBatch, WireInputEvent};
use crossover_security::pairing::{PairedPeer, PairingCode, PairingIdentity};
use crossover_security::{
    CertifiedIdentity, DeviceIdentity, SpkiFingerprint, TrustStore, TrustedPeer,
};

use crate::console::{self, ConsoleCommand};
use crate::storage::{open_clipboard_provider, open_input, open_secure_storage};

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
/// Foreground session maintenance: accept trusted peers, keep an
/// outbound session alive with reconnect, and run the clipboard and
/// input-control engines over whichever session is live. Console
/// commands (`c` take control, `r` release) drive explicit control
/// transfer (Phase 3, FR-5.1); Ctrl-C or `q` stops.
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

    // One metrics registry for the whole run, shared by every layer that
    // knows something worth counting and dumped on the way out (FR-7.3,
    // FR-7.5). Local only: nothing here leaves the machine.
    let metrics = Arc::new(Metrics::new());

    // Clipboard sync: one driver for the peer relationship; sessions of
    // either role feed it and carry its frames.
    let provider = open_clipboard_provider()?;
    let (sync_driver, sync_events, sync_commands) = clipboard_sync(
        provider,
        identity.device_id(),
        ClipboardConfig::new(),
        Some(Arc::clone(&metrics)),
    )
    .context("starting clipboard sync")?;
    tokio::spawn(sync_driver.run());

    // Input control: capture/inject behind the platform traits, driving
    // the control-transfer engine. Capture installs no hook until the
    // first grant.
    let (capture, injector) = open_input()?;
    let (control_driver, control_events, control_commands, control_notices) =
        input_control(capture, injector, ControlConfig::default());
    tokio::spawn(control_driver.run());
    tokio::spawn(print_control_notices(control_notices, Arc::clone(&metrics)));

    // Session lifecycle and frames fan out to both drivers.
    let fanout = SessionFanout {
        sync: sync_events,
        control: control_events.clone(),
        metrics: Some(Arc::clone(&metrics)),
    };

    // Every live session, keyed by id, so the mux can route control and
    // input frames to the exact session the engine named and broadcast
    // clipboard frames to all.
    let registry: SessionRegistry =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // Both drivers emit the same SessionCommands; merge them into one
    // stream for the mux.
    let commands = merge_command_streams(sync_commands, control_commands);

    // Outbound role: supervised session with automatic reconnect.
    let (handle, events) =
        start_outbound(connect.as_ref(), &identity, &certified, &store, &metrics);

    spawn_command_mux(Arc::clone(&registry), commands, Some(Arc::clone(&metrics)));

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

    println!();
    println!("{}", console::HELP);
    println!("Press Ctrl-C to stop.");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.context("waiting for Ctrl-C")?;
            println!("Shutting down.");
        }
        () = console_loop(&control_events) => {
            // The user typed a quit command (or stdin closed with one).
            println!("Shutting down.");
        }
        () = listener_loop(
            listener.as_ref(),
            &identity,
            &certified,
            &storage,
            &fanout,
            &registry,
            &metrics,
        ) => {}
        () = outbound_event_loop(
            events,
            handle.clone(),
            &storage,
            &fanout,
            &registry,
            &metrics,
        ) => {}
    }
    if let Some(handle) = &handle {
        handle.shutdown();
    }
    print_execution_metrics(&metrics);
    Ok(())
}

/// Dump the run's statistics on shutdown (FR-7.3): the human-readable
/// block to stdout, then the same numbers as one structured record for
/// log post-processing. Always on shutdown, by design — no flag to
/// forget. Everything here is local (FR-7.5).
fn print_execution_metrics(metrics: &Metrics) {
    let report = metrics.snapshot();
    println!();
    println!("{report}");
    report.log();
}

/// Start the outbound role if an address was given: a supervised session
/// with automatic reconnect, its `SessionOptions` carrying the metrics
/// registry so its traffic is counted like the inbound side's. Returns
/// `(None, None)` for a listen-only run.
fn start_outbound(
    connect: Option<&String>,
    identity: &DeviceIdentity,
    certified: &CertifiedIdentity,
    store: &TrustStore,
    metrics: &Arc<Metrics>,
) -> (
    Option<Arc<crossover_core::supervision::SupervisorHandle>>,
    Option<mpsc::Receiver<SessionEvent>>,
) {
    let Some(addr) = connect else {
        return (None, None);
    };
    println!("Maintaining an outbound session to {addr}.");
    let mut supervisor_config = SupervisorConfig::default();
    supervisor_config.session.metrics = Some(Arc::clone(metrics));
    let (handle, events) = supervise_outbound(
        addr.clone(),
        identity.clone(),
        certified.clone(),
        Arc::new(RwLock::new(store.clone())),
        supervisor_config,
    );
    (Some(Arc::new(handle)), Some(events))
}

/// Session lifecycle and inbound frames fanned out to both the clipboard
/// and input-control drivers. Each driver ignores what is not its
/// traffic, so broadcasting is simpler and cheaper than routing by type.
/// The session id travels with every event: clipboard sync is
/// session-agnostic (FR-5.4), but the control driver binds to one
/// session and drops the rest (FR-5.1), so it must know which session
/// each frame arrived on.
#[derive(Clone)]
struct SessionFanout {
    sync: mpsc::Sender<SyncEvent>,
    control: mpsc::Sender<InputControlEvent>,
    metrics: Option<Arc<Metrics>>,
}

impl SessionFanout {
    async fn established(&self, session: Uuid) {
        let _ = self.sync.send(SyncEvent::SessionEstablished).await;
        let _ = self
            .control
            .send(InputControlEvent::SessionEstablished { session })
            .await;
    }

    async fn lost(&self, session: Uuid) {
        let _ = self.sync.send(SyncEvent::SessionLost).await;
        let _ = self
            .control
            .send(InputControlEvent::SessionLost { session })
            .await;
    }

    async fn frame(&self, session: Uuid, frame: crossover_protocol::RawFrame) {
        // Count input events injected from the peer at the point they
        // arrive; the network layer already counts the frames, this adds
        // the events inside them (total and how many were keys).
        if frame.message_type == MessageType::InputBatch.wire()
            && let Some(metrics) = &self.metrics
            && let Some((total, keys)) = input_counts(&frame.payload)
        {
            metrics.record_input_received(total, keys);
        }
        let _ = self.sync.send(SyncEvent::Frame(frame.clone())).await;
        let _ = self
            .control
            .send(InputControlEvent::Frame { session, frame })
            .await;
    }
}

/// Count the events in an encoded input batch: total, and how many are
/// key events. `None` if the payload does not decode — the network layer
/// still counts the frame, so a malformed batch is not lost here, only
/// its per-event detail.
fn input_counts(payload: &[u8]) -> Option<(u64, u64)> {
    let batch = InputBatch::decode_payload(payload).ok()?;
    let total = batch.events.len() as u64;
    let keys = batch
        .events
        .iter()
        .filter(|event| matches!(event, WireInputEvent::Key { .. }))
        .count() as u64;
    Some((total, keys))
}

/// Forward two `SessionCommand` streams into one, so the mux has a single
/// receiver. The forwarders end when their drivers drop the senders.
fn merge_command_streams(
    mut a: mpsc::Receiver<SessionCommand>,
    mut b: mpsc::Receiver<SessionCommand>,
) -> mpsc::Receiver<SessionCommand> {
    let (merged_tx, merged_rx) = mpsc::channel(64);
    let tx_a = merged_tx.clone();
    tokio::spawn(async move {
        while let Some(command) = a.recv().await {
            if tx_a.send(command).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(command) = b.recv().await {
            if merged_tx.send(command).await.is_err() {
                break;
            }
        }
    });
    merged_rx
}

/// Print control-transfer state changes as they happen. Every failed or
/// ended transfer is user-visible here (NFR-3); detail is in the logs.
async fn print_control_notices(mut notices: mpsc::Receiver<ControlNotice>, metrics: Arc<Metrics>) {
    while let Some(notice) = notices.recv().await {
        count_control_notice(&metrics, notice);
        println!("{}", describe_notice(notice));
    }
}

/// Tally a control-transfer notice into the metrics registry. The notice
/// stream is the app's window onto the control engine's decisions, so the
/// control counters are sourced here rather than reaching into the engine.
fn count_control_notice(metrics: &Metrics, notice: ControlNotice) {
    use crossover_core::control::ControlEndReason;

    match notice {
        ControlNotice::RequestSent => metrics.record_control_request(),
        ControlNotice::RequestDenied(_) => metrics.record_control_denied(),
        ControlNotice::RequestTimedOut => metrics.record_control_timeout(),
        ControlNotice::ControlGained => metrics.record_control_gained(),
        ControlNotice::PeerTookControl => metrics.record_control_given(),
        ControlNotice::ControlEnded(ControlEndReason::HandedBack) => {
            metrics.record_control_handback();
        }
        ControlNotice::ControlEnded(ControlEndReason::CaptureLost) => metrics.record_capture_loss(),
        ControlNotice::PeerControlRevoked => metrics.record_control_revocation(),
        // The remaining notices (blocked requests, cancellations,
        // disconnect-driven ends, peer releases) have no dedicated counter.
        _ => {}
    }
}

/// One-line human phrasing for a control notice.
fn describe_notice(notice: ControlNotice) -> String {
    use crossover_core::control::{ControlEndReason, RequestBlocked};

    match notice {
        ControlNotice::RequestSent => "Requesting control of the peer…".to_owned(),
        ControlNotice::RequestBlocked(RequestBlocked::NoSession) => {
            "Cannot take control: no session with the peer yet.".to_owned()
        }
        ControlNotice::RequestBlocked(RequestBlocked::PeerHoldsControl) => {
            "Cannot take control: the peer is controlling this machine (release first).".to_owned()
        }
        ControlNotice::RequestBlocked(RequestBlocked::AlreadyControlling) => {
            "Already controlling the peer.".to_owned()
        }
        ControlNotice::RequestBlocked(RequestBlocked::RequestPending) => {
            "A control request is already pending.".to_owned()
        }
        ControlNotice::RequestDenied(reason) => {
            format!("The peer denied control ({reason:?}).")
        }
        ControlNotice::RequestTimedOut => "Control request timed out; still local.".to_owned(),
        ControlNotice::ControlGained => {
            "You now control the peer. Move the mouse; press 'r' to hand back.".to_owned()
        }
        ControlNotice::ControlEnded(ControlEndReason::HandedBack) => {
            "Control handed back to the peer.".to_owned()
        }
        ControlNotice::ControlEnded(ControlEndReason::Cancelled) => {
            "Control request cancelled.".to_owned()
        }
        ControlNotice::ControlEnded(ControlEndReason::Revoked) => {
            "The peer revoked your control.".to_owned()
        }
        ControlNotice::ControlEnded(ControlEndReason::Disconnected) => {
            "Control ended: session lost.".to_owned()
        }
        ControlNotice::ControlEnded(ControlEndReason::CaptureLost) => {
            "Control ended: local input capture was lost (failing closed).".to_owned()
        }
        ControlNotice::PeerTookControl => {
            "The peer is now controlling this machine. Press 'r' to revoke.".to_owned()
        }
        ControlNotice::PeerReleasedControl => "The peer released control.".to_owned(),
        ControlNotice::PeerControlRevoked => {
            "Revoked the peer's control of this machine.".to_owned()
        }
        ControlNotice::PeerControlLostOnDisconnect => {
            "The peer's control ended with the session; input released.".to_owned()
        }
    }
}

/// Read console commands until the user quits. On EOF (no interactive
/// terminal — e.g. `crossover run > log`) it pends forever, so the
/// non-interactive soak usage still relies on Ctrl-C rather than exiting
/// the moment stdin closes.
async fn console_loop(control: &mpsc::Sender<InputControlEvent>) {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match console::parse(&line) {
                Some(ConsoleCommand::TakeControl) => {
                    let _ = control.send(InputControlEvent::RequestControl).await;
                }
                Some(ConsoleCommand::Release) => {
                    let _ = control.send(InputControlEvent::ReleaseControl).await;
                }
                Some(ConsoleCommand::Help) => println!("{}", console::HELP),
                Some(ConsoleCommand::Quit) => return,
                None => {
                    if !line.trim().is_empty() {
                        println!("{}", console::HELP);
                    }
                }
            },
            // EOF or a read error: stop reading, but do not trigger
            // shutdown — leave that to Ctrl-C so piped runs are unaffected.
            Ok(None) | Err(_) => std::future::pending().await,
        }
    }
}

/// How to reach one live session: its frame sink, and its kill switch
/// when it has one (inbound sessions do; the outbound supervisor has no
/// per-session kill).
#[derive(Clone)]
struct SessionRoute {
    sink: FrameSink,
    kill: Option<watch::Sender<bool>>,
}

/// Where a session's outbound frames go. Cloned out from under the
/// registry lock before any `.await`, so sends never hold it.
#[derive(Clone)]
enum FrameSink {
    /// An accepted inbound session: frames queue to its writer task.
    Inbound(mpsc::Sender<(u16, Vec<u8>)>),
    /// The supervised outbound session: frames go through the handle,
    /// which targets whatever session is currently established.
    Outbound(Arc<crossover_core::supervision::SupervisorHandle>),
}

impl FrameSink {
    async fn deliver(&self, message_type: u16, payload: Vec<u8>) {
        match self {
            Self::Inbound(tx) => {
                let _ = tx.send((message_type, payload)).await;
            }
            Self::Outbound(handle) => {
                let _ = handle.send(message_type, payload).await;
            }
        }
    }
}

/// Every live session keyed by its locally generated id, so a command
/// can be routed to exactly the session the driver named (control/input)
/// or broadcast to all (clipboard, FR-5.4).
type SessionRegistry = Arc<std::sync::Mutex<std::collections::HashMap<Uuid, SessionRoute>>>;

fn registry_lock(
    registry: &SessionRegistry,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<Uuid, SessionRoute>> {
    registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Route driver commands to the session(s) they name: control and input
/// frames to the one session the engine addressed (FR-5.1), clipboard
/// frames broadcast (FR-5.4). Fail-closed terminations kill the named
/// session where it can be killed — the supervised outbound session has
/// no per-session kill, an accepted limitation logged when it bites.
fn spawn_command_mux(
    registry: SessionRegistry,
    mut commands: mpsc::Receiver<SessionCommand>,
    metrics: Option<Arc<Metrics>>,
) {
    tokio::spawn(async move {
        while let Some(command) = commands.recv().await {
            match command {
                SessionCommand::SendFrame {
                    target,
                    message_type,
                    payload,
                } => {
                    // Count input events forwarded to the peer as they pass
                    // through the one place every outbound frame does.
                    if message_type == MessageType::InputBatch.wire()
                        && let Some(metrics) = &metrics
                        && let Some((total, keys)) = input_counts(&payload)
                    {
                        metrics.record_input_sent(total, keys);
                    }
                    // Collect matching sinks under the lock, then send
                    // after releasing it (never hold a std Mutex over an
                    // await).
                    let sinks: Vec<FrameSink> = {
                        let routes = registry_lock(&registry);
                        match target {
                            FrameTarget::Broadcast => {
                                routes.values().map(|r| r.sink.clone()).collect()
                            }
                            FrameTarget::Session(id) => routes
                                .get(&id)
                                .map(|r| r.sink.clone())
                                .into_iter()
                                .collect(),
                        }
                    };
                    for sink in sinks {
                        sink.deliver(message_type, payload.clone()).await;
                    }
                }
                SessionCommand::TerminateSession { target, reason } => {
                    let kills: Vec<watch::Sender<bool>> = {
                        let routes = registry_lock(&registry);
                        match target {
                            FrameTarget::Broadcast => {
                                routes.values().filter_map(|r| r.kill.clone()).collect()
                            }
                            FrameTarget::Session(id) => routes
                                .get(&id)
                                .and_then(|r| r.kill.clone())
                                .into_iter()
                                .collect(),
                        }
                    };
                    if kills.is_empty() {
                        // No killable route: either the session already
                        // ended, or it is the outbound session the
                        // supervisor cannot kill per-session. Fail-closed
                        // still holds — the driver emitted Terminate
                        // instead of injecting — but say so (NFR-3).
                        tracing::warn!(
                            error = %reason,
                            ?target,
                            "payload violation with no killable session route"
                        );
                    } else {
                        tracing::error!(error = %reason, ?target, "terminating session on violation");
                        for kill in kills {
                            let _ = kill.send(true);
                        }
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
    fanout: &SessionFanout,
    registry: &SessionRegistry,
    metrics: &Arc<Metrics>,
) {
    let Some(listener) = listener else {
        return std::future::pending().await;
    };
    let options = SessionOptions {
        metrics: Some(Arc::clone(metrics)),
        ..SessionOptions::default()
    };
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
                metrics.record_session_established(true);
                let established_at = Instant::now();

                let (events_tx, mut events_rx) = mpsc::channel(64);
                let (outbound_tx, mut outbound_rx) = mpsc::channel::<(u16, Vec<u8>)>(64);
                let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
                let session_id = info.session_id;
                registry_lock(registry).insert(
                    session_id,
                    SessionRoute {
                        sink: FrameSink::Inbound(outbound_tx),
                        kill: Some(shutdown_tx),
                    },
                );
                fanout.established(session_id).await;

                let frame_sink = fanout.clone();
                let drain = tokio::spawn(async move {
                    while let Some(event) = events_rx.recv().await {
                        if let SessionEvent::Frame(frame) = event {
                            frame_sink.frame(session_id, frame).await;
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
                metrics.record_session_ended(&reason, established_at.elapsed());
                registry_lock(registry).remove(&session_id);
                fanout.lost(session_id).await;
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
    handle: Option<Arc<crossover_core::supervision::SupervisorHandle>>,
    storage: &Arc<dyn SecureStorage>,
    fanout: &SessionFanout,
    registry: &SessionRegistry,
    metrics: &Arc<Metrics>,
) {
    let Some(mut events) = events else {
        return std::future::pending().await;
    };
    // The supervisor runs one session at a time, so every Frame between an
    // Established and its Disconnected belongs to this id — which the
    // control driver needs to scope the grant to the right session.
    let mut current_session: Option<uuid::Uuid> = None;
    // When the live session was established, for its connected duration.
    let mut established_at: Option<Instant> = None;
    while let Some(event) = events.recv().await {
        match event {
            SessionEvent::Established(info) => {
                println!(
                    "Session established with \"{}\" (outbound).",
                    info.peer_device_name
                );
                touch_last_connected(&**storage, info.peer_fingerprint);
                metrics.record_session_established(false);
                established_at = Some(Instant::now());
                current_session = Some(info.session_id);
                // Register the outbound session so control frames route to
                // it by id. No kill switch: the supervisor has no
                // per-session termination (an accepted limitation).
                if let Some(handle) = &handle {
                    registry_lock(registry).insert(
                        info.session_id,
                        SessionRoute {
                            sink: FrameSink::Outbound(Arc::clone(handle)),
                            kill: None,
                        },
                    );
                }
                fanout.established(info.session_id).await;
            }
            SessionEvent::Disconnected {
                session_id,
                reason,
                retry_in,
            } => {
                if let Some(since) = established_at.take() {
                    metrics.record_session_ended(&reason, since.elapsed());
                }
                match retry_in {
                    Some(delay) => println!(
                        "Outbound session ended ({reason}); retrying in {}s.",
                        delay.as_secs_f32()
                    ),
                    None => println!("Outbound session ended ({reason})."),
                }
                current_session = None;
                registry_lock(registry).remove(&session_id);
                fanout.lost(session_id).await;
            }
            SessionEvent::ConnectFailed { error, retry_in } => {
                // A failed establish that will be retried — the app-level
                // stand-in for "retransmitted" (TCP hides its own).
                metrics.record_reconnect_attempt();
                println!(
                    "Connect failed ({error}); retrying in {}s.",
                    retry_in.as_secs_f32()
                );
            }
            SessionEvent::Frame(frame) => {
                if let Some(session) = current_session {
                    fanout.frame(session, frame).await;
                }
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
