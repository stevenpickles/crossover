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

use crossover_core::outbound::{BudgetedReceiver, CommandReceiver, command_lanes};
use crossover_core::pairing::{PairingListener, pair_with};
use crossover_core::supervision::{
    KeepaliveConfig, SessionEvent, SupervisorConfig, run_session, supervise_outbound,
};
use crossover_core::{
    ClipboardConfig, ControlConfig, ControlNotice, CrossingKind, EdgeCrossing, EdgeDetectDriver,
    FrameTarget, InputControlEvent, LinkSide, LocalNode, Metrics, OutboundSender, SeamlessInputs,
    SessionCommand, SessionListener, SessionOptions, SyncEvent, Topology, clipboard_sync,
    edge_detect, input_control, outbound_channel,
};
use crossover_platform::DisplayInfo;
use crossover_platform::SecureStorage;
use crossover_platform::{ServiceError, ServiceStatus};
use crossover_protocol::DEFAULT_PORT;
use crossover_protocol::hello::MessageType;
use crossover_protocol::input::{InputBatch, WireInputEvent};
use crossover_security::pairing::{PairedPeer, PairingCode, PairingIdentity};
use crossover_security::{
    CertifiedIdentity, DeviceIdentity, SpkiFingerprint, TrustStore, TrustedPeer,
};

use crate::console::{self, ConsoleCommand};
use crate::storage::{
    open_clipboard_provider, open_cursor_mask, open_display, open_input, open_secure_storage,
    open_service_manager,
};

/// One ceremony's allowance, listener and connector alike. Generous
/// enough to read a code off one screen and type it on another; bounded
/// because everything is (NFR-1).
const PAIRING_TIMEOUT: Duration = Duration::from_mins(2);

/// How often the edge detector samples the cursor while watching an edge
/// (ADR 0009). ~125 Hz: snappy enough that a crossing feels instant, and
/// the cursor pins at the edge so nothing is missed between polls.
const EDGE_POLL_INTERVAL: Duration = Duration::from_millis(8);

/// How often the running process reloads the trust store to enforce
/// revocation on *active* sessions (ADR 0010). A deliberate LAN revocation
/// tolerates a bounded few-second latency; this reuses the listener's
/// per-accept load path.
const REVOCATION_POLL: Duration = Duration::from_secs(2);

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
        "Revoked \"{}\" ({}). New connections are rejected; any active \
         session ends within a couple of seconds (ADR 0010).",
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

    // Background service (ADR 0011): surfaced here so a user can tell at a
    // glance whether unattended operation is set up, without opening
    // services.msc. Quiet on platforms with no service implementation.
    match open_service_manager()?.status() {
        Ok(ServiceStatus::NotInstalled) => {
            println!();
            println!("Background service: not installed (`crossover service install`)");
        }
        Ok(ServiceStatus::Installed { running }) => {
            println!();
            println!(
                "Background service: installed, {}",
                if running { "running" } else { "stopped" }
            );
        }
        Err(ServiceError::Unsupported) => {}
        Err(error) => {
            println!();
            println!("Background service: status unavailable ({error})");
        }
    }
    Ok(())
}

/// A commented example config for `crossover config` to show when no file
/// exists yet — the user can save it verbatim and edit.
const EXAMPLE_CONFIG: &str = "\
# Crossover startup configuration (sectioned, versioned). Any `crossover run`
# flag can live here; a flag on the command line always overrides the file.
schema_version = 1

[device]
name = \"machine-b\"

[network]
# connect = \"192.168.1.151:27677\"   # dial this peer
listen = \"0.0.0.0:27677\"            # or accept inbound peers (presence = listen)

[seamless]
side = \"right\"                      # \"left\" | \"right\"

[cursor]
# mask = false                       # default true; false = never hide the cursor
";

/// `crossover config` — show where the startup config lives and its
/// current settings, or an example when there is none (Phase 6).
pub fn config_show() -> anyhow::Result<()> {
    let Some(path) = crate::paths::config_path() else {
        println!("Startup config is unavailable here (home directory could not be determined).");
        return Ok(());
    };
    println!("Startup config file:");
    println!("  {}", path.display());
    println!();
    if path.exists() {
        // Parse it first so a typo or bad value is reported, not hidden.
        crate::config::load_run_config()?;
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        println!("Current contents (parsed OK):");
        println!();
        for line in contents.lines() {
            println!("  {line}");
        }
    } else {
        println!("No config file yet — `crossover run` uses CLI flags until you create one.");
        println!("Save this as the file above and edit to taste:");
        println!();
        for line in EXAMPLE_CONFIG.lines() {
            println!("  {line}");
        }
    }
    Ok(())
}

/// `crossover service install` — register the background service (ADR 0011).
pub fn service_install() -> anyhow::Result<()> {
    let manager = open_service_manager()?;
    match manager.status() {
        Ok(ServiceStatus::Installed { running }) => {
            println!(
                "The Crossover background service is already installed ({}).",
                if running { "running" } else { "stopped" }
            );
            return Ok(());
        }
        Ok(ServiceStatus::NotInstalled) => {}
        Err(ServiceError::Unsupported) => {
            print_service_unsupported();
            return Ok(());
        }
        Err(error) => return Err(service_failure(&error, "checking the Crossover service")),
    }

    manager
        .install()
        .map_err(|error| service_failure(&error, "installing the Crossover service"))?;
    println!("Installed the Crossover background service (auto-start, runs as LocalSystem).");
    println!(
        "It starts at boot and launches `crossover run` in your session, restarting it on \
         crash (ADR 0011)."
    );
    Ok(())
}

/// `crossover service uninstall` — stop and remove the background service.
pub fn service_uninstall() -> anyhow::Result<()> {
    let manager = open_service_manager()?;
    match manager.status() {
        Ok(ServiceStatus::NotInstalled) => {
            println!("The Crossover background service is not installed; nothing to remove.");
            return Ok(());
        }
        Ok(ServiceStatus::Installed { .. }) => {}
        Err(ServiceError::Unsupported) => {
            print_service_unsupported();
            return Ok(());
        }
        Err(error) => return Err(service_failure(&error, "checking the Crossover service")),
    }

    manager
        .uninstall()
        .map_err(|error| service_failure(&error, "removing the Crossover service"))?;
    println!("Removed the Crossover background service.");
    Ok(())
}

/// `crossover service status` — report whether the service is installed.
pub fn service_status() -> anyhow::Result<()> {
    let manager = open_service_manager()?;
    match manager.status() {
        Ok(ServiceStatus::NotInstalled) => {
            println!(
                "The Crossover background service is not installed. Install it with \
                 `crossover service install` from an elevated console."
            );
        }
        Ok(ServiceStatus::Installed { running: true }) => {
            println!("The Crossover background service is installed and running.");
        }
        Ok(ServiceStatus::Installed { running: false }) => {
            println!("The Crossover background service is installed but not running.");
        }
        Err(ServiceError::Unsupported) => {
            print_service_unsupported();
            return Ok(());
        }
        Err(error) => return Err(service_failure(&error, "checking the Crossover service")),
    }
    Ok(())
}

/// Report that auto-start has no implementation on this platform yet, without
/// treating it as a command failure — the CLI surface is intentionally
/// platform-neutral (ADR 0011).
fn print_service_unsupported() {
    println!(
        "The background service is not available on this platform yet. On Linux and macOS, \
         auto-start will use the OS-native user-session mechanism (systemd --user / launchd) \
         when those ports land (docs/ROADMAP.md)."
    );
}

/// Render a `ServiceManager` failure as an actionable CLI error, calling out
/// elevation for the common permission case (FR-7.3).
fn service_failure(error: &ServiceError, context: &str) -> anyhow::Error {
    match error {
        ServiceError::PermissionDenied { reason } => anyhow::anyhow!(
            "{context}: {reason}. Re-run this command from an elevated console \
             (right-click → Run as administrator)."
        ),
        other => anyhow::anyhow!("{context}: {other}"),
    }
}

/// `crossover run [--listen [--bind <addr>]] [--connect <addr>]`
///
/// Foreground session maintenance: accept trusted peers, keep an
/// outbound session alive with reconnect, and run the clipboard and
/// input-control engines over whichever session is live. Console
/// commands (`c` take control, `r` release) drive explicit control
/// transfer (Phase 3, FR-5.1); with `--left`/`--right`, crossing the
/// linked screen edge transfers control automatically (Phase 5, ADR
/// 0009). Ctrl-C or `q` stops.
/// Bring up input control — capture/inject behind the platform traits, the
/// control-transfer engine, cursor masking (ADR 0009), and, in seamless
/// mode, the edge detector — spawning the driver, edge wiring, and notice
/// printer. Returns the event sender and command receiver the session mux
/// needs. Capture installs no hook until the first grant.
fn setup_input_control(
    side: Option<LinkSide>,
    metrics: &Arc<Metrics>,
    no_cursor_mask: bool,
) -> anyhow::Result<(mpsc::Sender<InputControlEvent>, CommandReceiver)> {
    let (capture, injector) = open_input()?;
    // Diagnostic switch: run without masking to isolate cursor behavior
    // from control transfer (ADR 0009).
    let cursor_mask: Arc<dyn crossover_platform::CursorMask> = if no_cursor_mask {
        tracing::info!("cursor masking disabled (--no-cursor-mask)");
        Arc::new(crossover_platform::NoopCursorMask)
    } else {
        open_cursor_mask()
    };
    let display = open_display()?;

    // Log the display and the configured seamless edge at startup (ADR
    // 0009), so a soak report shows the geometry each side used.
    log_display_and_edge(&*display, side);

    // Seamless mode (--left/--right): a topology and an edge detector, plus
    // the mode channel that keeps the detector in step with control state.
    // Built before input_control so its mode sender can ride in.
    let (seamless, edge_wiring) = build_seamless(side, &display);

    let (control_driver, control_events, control_commands, control_notices) = input_control(
        capture,
        injector,
        cursor_mask,
        seamless,
        ControlConfig::default(),
    );
    tokio::spawn(control_driver.run());

    // With the control driver up, run the edge detector and forward its
    // crossings in as leave/return events.
    if let Some((edge_driver, crossings)) = edge_wiring {
        spawn_edge_wiring(edge_driver, crossings, control_events.clone());
    }
    tokio::spawn(print_control_notices(control_notices, Arc::clone(metrics)));

    Ok((control_events, control_commands))
}

pub async fn run(
    device_name: &str,
    listen_bind: Option<String>,
    connect: Option<String>,
    side: Option<LinkSide>,
    no_cursor_mask: bool,
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

    // Input control: capture/inject, the transfer engine, cursor masking,
    // and (in seamless mode) the edge detector.
    let (control_events, control_commands) = setup_input_control(side, &metrics, no_cursor_mask)?;

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

    // Both drivers emit the same SessionCommands; merge them into the two
    // priority lanes the mux drains independently (ADR 0013).
    let commands = merge_command_lanes(sync_commands, control_commands);

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
        // Enforce revocation on live sessions, not just new ones (ADR 0010).
        () = enforce_revocations(&storage, &registry) => {}
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
    /// Deliver to both drivers **concurrently**, never one behind the other.
    ///
    /// Sequential delivery makes the first driver's backpressure the second
    /// driver's latency. That is exactly backwards here: the clipboard driver
    /// is the one that parks under bulk backpressure, and the control driver
    /// is the one that releases held input. Awaiting the clipboard send first
    /// would gate the release-all-input path behind a stalled transfer — a
    /// stuck key after disconnect, which CLAUDE.md ranks release-blocking.
    ///
    /// `join!` polls both in this task, so neither can gate the other. A full
    /// channel still delays *its own* driver, as it must.
    async fn fan_out(&self, sync: SyncEvent, control: InputControlEvent) {
        let (sync_sent, control_sent) =
            tokio::join!(self.sync.send(sync), self.control.send(control));
        let _ = sync_sent;
        let _ = control_sent;
    }

    async fn established(&self, session: Uuid) {
        self.fan_out(
            SyncEvent::SessionEstablished,
            InputControlEvent::SessionEstablished { session },
        )
        .await;
    }

    async fn lost(&self, session: Uuid) {
        self.fan_out(
            SyncEvent::SessionLost,
            InputControlEvent::SessionLost { session },
        )
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
        self.fan_out(
            SyncEvent::Frame(frame.clone()),
            InputControlEvent::Frame { session, frame },
        )
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

/// The drivers' commands, merged and split by priority class (ADR 0013).
///
/// Two lanes, not one queue with a tag: the mux *awaits* delivery into each
/// session's queue, so a single task draining both classes would let one
/// saturated Background path stall High traffic for every session — the
/// head-of-line block moved upstream rather than removed. Each lane gets its
/// own mux task, and only the Background one ever waits on bulk.
struct ClassifiedCommands {
    high: mpsc::Receiver<SessionCommand>,
    background: BudgetedReceiver<SessionCommand>,
}

/// Fold both drivers' already-classified command lanes into one merged pair.
///
/// **One forwarder task per source lane, never per source.** A task that read
/// a driver's two lanes together would serialize them: parked handing a bulk
/// command downstream, it could not pick up that driver's next High command —
/// so a fail-closed `TerminateSession` emitted during a stalled transfer
/// would sit behind the very bulk that stalled it. Four single-class tasks
/// have nothing to serialize; each waits only on its own class.
///
/// Classification is a pure function of the message type, so re-running it on
/// the merged sender maps each lane onto the matching lane, never across.
fn merge_command_lanes(a: CommandReceiver, b: CommandReceiver) -> ClassifiedCommands {
    let (merged_tx, merged_rx) = command_lanes();
    for source in [a, b] {
        let (mut source_high, mut source_background) = source.into_lanes();

        let high_tx = merged_tx.clone();
        tokio::spawn(async move {
            while let Some(command) = source_high.recv().await {
                if high_tx.send(command).await.is_err() {
                    break;
                }
            }
        });

        let background_tx = merged_tx.clone();
        tokio::spawn(async move {
            while let Some(command) = source_background.recv().await {
                // Hold this hop's byte budget until the next hop has taken
                // the command, so the two bounds cannot both be spent on the
                // same bytes.
                let (command, hold) = command.into_parts();
                let delivered = background_tx.send(command).await.is_ok();
                drop(hold);
                if !delivered {
                    break;
                }
            }
        });
    }
    let (high, background) = merged_rx.into_lanes();
    ClassifiedCommands { high, background }
}

/// Build the seamless wiring for a `--left`/`--right` run (ADR 0009): the
/// [`SeamlessInputs`] the control driver takes, and the edge detector plus
/// its crossing stream to spawn once the control driver is up. `(None,
/// None)` for an explicit-only run.
type EdgeWiring = (EdgeDetectDriver, mpsc::Receiver<EdgeCrossing>);
fn build_seamless(
    side: Option<LinkSide>,
    display: &Arc<dyn DisplayInfo>,
) -> (Option<SeamlessInputs>, Option<EdgeWiring>) {
    let Some(side) = side else {
        return (None, None);
    };
    let topology = Topology::new(side);
    let (edge_driver, edge_mode, crossings_rx) =
        edge_detect(Arc::clone(display), topology, EDGE_POLL_INTERVAL);
    let seamless = SeamlessInputs {
        topology,
        display: Arc::clone(display),
        edge_mode,
    };
    (Some(seamless), Some((edge_driver, crossings_rx)))
}

/// Run the edge detector and forward its crossings into the control driver
/// as leave/return events (ADR 0009).
fn spawn_edge_wiring(
    edge_driver: EdgeDetectDriver,
    mut crossings: mpsc::Receiver<EdgeCrossing>,
    control_events: mpsc::Sender<InputControlEvent>,
) {
    tokio::spawn(edge_driver.run());
    tokio::spawn(async move {
        while let Some(crossing) = crossings.recv().await {
            let event = match crossing.kind {
                CrossingKind::Leave => InputControlEvent::EdgeLeave {
                    position: crossing.position,
                },
                CrossingKind::Return => InputControlEvent::EdgeReturn {
                    position: crossing.position,
                },
            };
            if control_events.send(event).await.is_err() {
                break;
            }
        }
    });
}

/// Log the primary display's geometry and the configured seamless edge at
/// startup (ADR 0009) — both structured (FR-7.3) and as a human line, so a
/// soak report records the geometry each machine ran with.
fn log_display_and_edge(display: &dyn DisplayInfo, side: Option<LinkSide>) {
    let screen = match display.desktop_bounds() {
        Ok(screen) => screen,
        Err(error) => {
            tracing::warn!(error = %error, "could not read the desktop geometry at startup");
            println!("Desktop: unavailable ({error}).");
            return;
        }
    };
    if let Some(side) = side {
        let edge = Topology::new(side).linked_edge();
        tracing::info!(
            width = screen.width,
            height = screen.height,
            ?side,
            linked_edge = ?edge,
            "virtual desktop and seamless edge"
        );
        println!(
            "Desktop: {}x{} (all monitors). Seamless: {side:?} screen, crossing on its {edge:?} edge.",
            screen.width, screen.height,
        );
    } else {
        tracing::info!(
            width = screen.width,
            height = screen.height,
            "virtual desktop; seamless transfer disabled"
        );
        println!(
            "Desktop: {}x{} (all monitors). Seamless edge transfer off (pass --left or --right).",
            screen.width, screen.height,
        );
    }
}

/// Surface control-transfer state changes as they happen. Every failed or
/// ended transfer is user-visible here (NFR-3), both on the console and in the
/// log — the latter matters because the service-launched worker is headless, so
/// the `println!` alone goes to `NUL` (ADR 0011) and the soak log would
/// otherwise show no control transfers at all.
async fn print_control_notices(mut notices: mpsc::Receiver<ControlNotice>, metrics: Arc<Metrics>) {
    while let Some(notice) = notices.recv().await {
        count_control_notice(&metrics, notice);
        let message = describe_notice(notice);
        tracing::info!(?notice, "control: {message}");
        println!("{message}");
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
        ControlNotice::PeerControlLostToDesktop => {
            "The peer's control ended: this machine's desktop switched to a secure prompt \
             (e.g. UAC); input released."
                .to_owned()
        }
        ControlNotice::PeerControlReclaimedLocally => {
            "Reclaimed control: local input detected on this machine; input released, \
             neither machine now controls the other."
                .to_owned()
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

/// How to reach one live session: its frame sink, its kill switch when it
/// has one (inbound sessions do; the outbound supervisor has no per-session
/// kill), and the peer's pinned fingerprint so revocation can find and
/// terminate it (ADR 0010).
#[derive(Clone)]
struct SessionRoute {
    sink: FrameSink,
    kill: Option<watch::Sender<bool>>,
    peer_fingerprint: SpkiFingerprint,
}

/// Where a session's outbound frames go. Cloned out from under the
/// registry lock before any `.await`, so sends never hold it.
///
/// Both variants classify the frame into the session's High/Background
/// lanes on the way in (ADR 0013), so the priority split survives this hop
/// as well as the mux's.
#[derive(Clone)]
enum FrameSink {
    /// An accepted inbound session: frames queue to its writer task.
    Inbound(OutboundSender),
    /// The supervised outbound session: frames go through the handle,
    /// which targets whatever session is currently established.
    Outbound(Arc<crossover_core::supervision::SupervisorHandle>),
}

impl FrameSink {
    async fn deliver(&self, message_type: u16, payload: Vec<u8>) {
        match self {
            Self::Inbound(tx) => {
                let _ = tx.send(message_type, payload).await;
            }
            Self::Outbound(handle) => {
                let _ = handle.send(message_type, payload).await;
            }
        }
    }
}

/// Retire a dead session's send path — **synchronously, and before any
/// other teardown step.**
///
/// Dropping the receiver is the only thing that closes the session's
/// Background byte budget, and closing that budget is the only thing that
/// unparks everything queued behind it: the mux's bulk task, the merge
/// forwarder, and finally the driver itself, which is parked mid-`execute`
/// and therefore not draining its own event channel.
///
/// Every later teardown step *depends* on that. `drain.await` and
/// `SessionFanout::lost` both push into the drivers' bounded event channels,
/// so with the budget still held they wait on a driver that is waiting on
/// them — a cycle that runs: teardown → driver event channel → driver
/// command lane → merge forwarder → merged lane → mux → session lane →
/// released only by this drop. A trusted-but-hostile peer that floods the
/// clipboard and then stops reading could park the accept loop there
/// permanently: no new sessions, no input, restart required.
///
/// Removing the registry entry belongs here too, and is likewise
/// synchronous: it stops the mux routing anything else at a session that no
/// longer exists.
fn retire_session_route(
    outbound: crossover_core::OutboundReceiver,
    registry: &SessionRegistry,
    session_id: Uuid,
) {
    drop(outbound);
    registry_lock(registry).remove(&session_id);
}

/// Everything an ended session needs done, **in the order it must be done
/// in**. One function so the order is reviewable in one place and guarded by
/// one test, rather than living as loose statements a tidy-up can reorder.
///
/// The order is not a preference:
///
/// 1. `retire_session_route` — synchronous, and first. Until the receiver
///    drops, the session's byte budget is held and everything queued behind
///    it stays parked, including the drivers.
/// 2. drop the event sender, then wait for the drain task. It calls
///    `SessionFanout::frame`, which pushes into the drivers' bounded event
///    channels — so it can only finish once step 1 has unparked them.
/// 3. fan the loss out, which pushes into those same channels, and which
///    releases held input on the control driver (FR-4.4).
///
/// Steps 2 and 3 both wait on drivers that step 1 releases. Doing either
/// first closes a cycle that parks the accept loop permanently — see
/// `retire_session_route` for the full chain.
async fn finish_session(
    outbound: crossover_core::OutboundReceiver,
    registry: &SessionRegistry,
    session_id: Uuid,
    events_tx: mpsc::Sender<SessionEvent>,
    drain: tokio::task::JoinHandle<()>,
    fanout: &SessionFanout,
) {
    retire_session_route(outbound, registry, session_id);
    drop(events_tx);
    let _ = drain.await;
    fanout.lost(session_id).await;
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

/// Which of the live sessions belong to a peer that is no longer trusted —
/// a session whose pinned fingerprint is absent from the current store
/// (ADR 0010). Pure, so it is unit-tested; the wiring is integration.
fn revoked_session_ids(live: &[(Uuid, SpkiFingerprint)], trust: &TrustStore) -> Vec<Uuid> {
    live.iter()
        .filter(|(_, fingerprint)| trust.find_by_fingerprint(*fingerprint).is_none())
        .map(|(id, _)| *id)
        .collect()
}

/// Terminate one session on revocation: fire an inbound session's kill
/// switch, or shut the outbound supervisor down (its one peer is now
/// untrusted, so stopping reconnection to it is correct). The normal
/// disconnect path then releases any held input (FR-4.4).
fn terminate_on_revocation(route: &SessionRoute) {
    match &route.sink {
        FrameSink::Inbound(_) => {
            if let Some(kill) = &route.kill {
                let _ = kill.send(true);
            }
        }
        FrameSink::Outbound(handle) => handle.shutdown(),
    }
}

/// Enforce revocation on *active* sessions (ADR 0010): periodically reload
/// the trust store and terminate any live session whose peer is no longer
/// trusted. New connections are already rejected by the per-accept/attempt
/// trust read; this closes the active-session half of SECURITY.md §4 / T6.
/// Never returns — runs as a branch of the foreground select.
async fn enforce_revocations(storage: &Arc<dyn SecureStorage>, registry: &SessionRegistry) {
    let mut ticker = tokio::time::interval(REVOCATION_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let trust = match TrustStore::load(&**storage) {
            Ok(trust) => trust,
            // Transient read failure: keep the sessions and try next tick,
            // rather than tearing down live peers on a blip.
            Err(error) => {
                tracing::debug!(error = %error, "revocation check: trust store unreadable");
                continue;
            }
        };
        let live: Vec<(Uuid, SpkiFingerprint)> = registry_lock(registry)
            .iter()
            .map(|(id, route)| (*id, route.peer_fingerprint))
            .collect();
        for id in revoked_session_ids(&live, &trust) {
            // Remove and terminate under the same intent; the session's own
            // teardown also removes by id (a harmless no-op) and fans out
            // the loss so input is released.
            if let Some(route) = registry_lock(registry).remove(&id) {
                tracing::warn!(session = %id, "peer revoked; terminating active session");
                terminate_on_revocation(&route);
            }
        }
    }
}

/// Route driver commands to the session(s) they name: control and input
/// frames to the one session the engine addressed (FR-5.1), clipboard
/// frames broadcast (FR-5.4). Fail-closed terminations kill the named
/// session where it can be killed — the supervised outbound session has
/// no per-session kill, an accepted limitation logged when it bites.
///
/// **One task per priority lane** (ADR 0013). Delivery into a session's
/// queue is awaited, so a saturated Background path parks its own task
/// indefinitely; the High task shares nothing with it and keeps running.
fn spawn_command_mux(
    registry: SessionRegistry,
    commands: ClassifiedCommands,
    metrics: Option<Arc<Metrics>>,
) {
    let ClassifiedCommands {
        mut high,
        mut background,
    } = commands;

    let high_registry = Arc::clone(&registry);
    let high_metrics = metrics.clone();
    tokio::spawn(async move {
        while let Some(command) = high.recv().await {
            dispatch_command(&high_registry, high_metrics.as_ref(), command).await;
        }
    });
    tokio::spawn(async move {
        while let Some(command) = background.recv().await {
            let (command, hold) = command.into_parts();
            dispatch_command(&registry, metrics.as_ref(), command).await;
            // The byte budget returns here, once the command has been
            // handed to every sink — not merely dequeued.
            drop(hold);
        }
    });
}

/// Execute one driver command against the live sessions. Shared by both
/// mux lanes; the lane decides *when* it runs, never *what* it does.
async fn dispatch_command(
    registry: &SessionRegistry,
    metrics: Option<&Arc<Metrics>>,
    command: SessionCommand,
) {
    match command {
        SessionCommand::SendFrame {
            target,
            message_type,
            payload,
        } => {
            // Count input events forwarded to the peer as they pass
            // through the one place every outbound frame does.
            if message_type == MessageType::InputBatch.wire()
                && let Some(metrics) = metrics
                && let Some((total, keys)) = input_counts(&payload)
            {
                metrics.record_input_sent(total, keys);
            }
            // Collect matching sinks under the lock, then send
            // after releasing it (never hold a std Mutex over an
            // await).
            let mut sinks: Vec<FrameSink> = {
                let routes = registry_lock(registry);
                match target {
                    FrameTarget::Broadcast => routes.values().map(|r| r.sink.clone()).collect(),
                    FrameTarget::Session(id) => routes
                        .get(&id)
                        .map(|r| r.sink.clone())
                        .into_iter()
                        .collect(),
                }
            };
            // The last sink takes the payload itself. Broadcast is the
            // rare case and one session the common one, so copying a
            // multi-megabyte clipboard frame for the only recipient would
            // be pure waste (ADR 0014 makes this traffic routine).
            let Some(last) = sinks.pop() else {
                return; // no live session matches this target
            };
            for sink in sinks {
                sink.deliver(message_type, payload.clone()).await;
            }
            last.deliver(message_type, payload).await;
        }
        SessionCommand::TerminateSession { target, reason } => {
            let kills: Vec<watch::Sender<bool>> = {
                let routes = registry_lock(registry);
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
                let (outbound_tx, mut outbound_rx) = outbound_channel();
                let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
                let session_id = info.session_id;
                registry_lock(registry).insert(
                    session_id,
                    SessionRoute {
                        sink: FrameSink::Inbound(outbound_tx),
                        kill: Some(shutdown_tx),
                        peer_fingerprint: info.peer_fingerprint,
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
                metrics.record_session_ended(&reason, established_at.elapsed());
                finish_session(outbound_rx, registry, session_id, events_tx, drain, fanout).await;
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
    // When the link dropped, to measure how long recovery took (§4).
    let mut down_since: Option<Instant> = None;
    while let Some(event) = events.recv().await {
        match event {
            SessionEvent::Established(info) => {
                println!(
                    "Session established with \"{}\" (outbound).",
                    info.peer_device_name
                );
                touch_last_connected(&**storage, info.peer_fingerprint);
                metrics.record_session_established(false);
                if let Some(down) = down_since.take() {
                    metrics.record_reconnect_recovery(down.elapsed());
                }
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
                            peer_fingerprint: info.peer_fingerprint,
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
                    Some(delay) => {
                        // Will retry: start the recovery clock.
                        down_since = Some(Instant::now());
                        println!(
                            "Outbound session ended ({reason}); retrying in {}s.",
                            delay.as_secs_f32()
                        );
                    }
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
    use std::sync::Arc;

    use tokio::sync::watch;
    use uuid::Uuid;

    use crossover_core::outbound::{CommandSender, command_lanes};
    use crossover_core::{
        FrameTarget, MAX_BACKGROUND_QUEUE_FRAMES, SendPriority, SessionCommand, outbound_channel,
    };
    use crossover_protocol::hello::MessageType;
    use crossover_security::{CertifiedIdentity, DeviceIdentity, TrustStore, TrustedPeer};

    use super::{SessionRegistry, age, registry_lock, revoked_session_ids};

    #[test]
    fn revoked_sessions_are_those_whose_peer_left_the_trust_store() {
        // Two identities; only `kept` stays trusted.
        let mk = |name: &str| {
            let id = DeviceIdentity::generate(name).unwrap();
            let cert = CertifiedIdentity::from_identity(&id).unwrap();
            (
                id.device_id(),
                id.device_name().to_owned(),
                cert.fingerprint(),
            )
        };
        let (kept_id, kept_name, kept_fp) = mk("kept");
        let (_revoked_id, _revoked_name, revoked_fp) = mk("revoked");

        let mut trust = TrustStore::new();
        trust
            .add_peer(TrustedPeer::new(kept_id, &kept_name, kept_fp).unwrap())
            .unwrap();

        let kept_session = Uuid::from_bytes([0x11; 16]);
        let revoked_session = Uuid::from_bytes([0x22; 16]);
        let live = vec![(kept_session, kept_fp), (revoked_session, revoked_fp)];

        // Only the session whose fingerprint is no longer trusted is returned.
        assert_eq!(revoked_session_ids(&live, &trust), vec![revoked_session]);
        // With both trusted, nothing is revoked.
        trust
            .add_peer(
                TrustedPeer::new(Uuid::from_bytes([0x33; 16]), "revoked", revoked_fp).unwrap(),
            )
            .unwrap();
        assert!(revoked_session_ids(&live, &trust).is_empty());
    }

    /// A fail-closed termination is a security action, so it rides High
    /// even though it comes off the clipboard driver's otherwise-bulk
    /// stream (docs/PROTOCOL.md §7).
    #[test]
    fn terminations_ride_the_interactive_lane_and_bulk_frames_do_not() {
        use crossover_core::outbound::command_priority;
        use crossover_core::{FrameTarget, SendPriority, SessionCommand};
        use crossover_protocol::hello::MessageType;

        let target = FrameTarget::Broadcast;
        let frame = |message_type: MessageType| SessionCommand::SendFrame {
            target,
            message_type: message_type.wire(),
            payload: Vec::new(),
        };
        assert_eq!(
            command_priority(&SessionCommand::TerminateSession {
                target,
                reason: "invalid payload".to_owned(),
            }),
            SendPriority::High
        );
        assert_eq!(
            command_priority(&frame(MessageType::InputBatch)),
            SendPriority::High
        );
        assert_eq!(
            command_priority(&frame(MessageType::ClipboardData)),
            SendPriority::Background
        );
    }

    /// One live session whose writer never runs, plus a running mux: the rig
    /// the two wedged-path tests share.
    struct WedgeRig {
        session: Uuid,
        clipboard: CommandSender,
        control: CommandSender,
        outbound: crossover_core::OutboundReceiver,
        registry: SessionRegistry,
        killed: watch::Receiver<bool>,
    }

    fn wedge_rig() -> WedgeRig {
        use std::collections::HashMap;
        use std::sync::Mutex;

        use super::{FrameSink, SessionRoute, merge_command_lanes, spawn_command_mux};

        let identity = DeviceIdentity::generate("wedged-peer").unwrap();
        let certified = CertifiedIdentity::from_identity(&identity).unwrap();
        let session = Uuid::from_bytes([0x55; 16]);

        let (sink, outbound) = outbound_channel();
        let (kill_tx, killed) = watch::channel(false);
        let registry: SessionRegistry = Arc::new(Mutex::new(HashMap::from([(
            session,
            SessionRoute {
                sink: FrameSink::Inbound(sink),
                kill: Some(kill_tx),
                peer_fingerprint: certified.fingerprint(),
            },
        )])));

        let (clipboard, clipboard_rx) = command_lanes();
        let (control, control_rx) = command_lanes();
        spawn_command_mux(
            Arc::clone(&registry),
            merge_command_lanes(clipboard_rx, control_rx),
            None,
        );

        WedgeRig {
            session,
            clipboard,
            control,
            outbound,
            registry,
            killed,
        }
    }

    /// A producer parked on the wedged path, and how far it got.
    struct Wedged {
        producer: tokio::task::JoinHandle<()>,
        sent: Arc<std::sync::atomic::AtomicUsize>,
        at_wedge: usize,
    }

    impl Wedged {
        fn sent(&self) -> usize {
            self.sent.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// Wedge the whole Background path solid — the driver's own lane, the
    /// mux's, and the session's — and return once the producer is *provably*
    /// parked, with the number of commands it got in.
    ///
    /// Deterministic rather than timed: `#[tokio::test]` gives a
    /// current-thread runtime, so a sweep of `yield_now` runs every other
    /// task to its next await point. A producer that has not advanced across
    /// a whole sweep is parked on backpressure, not merely behind us.
    async fn wedge_background(sender: &CommandSender, session: Uuid) -> Wedged {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let sent = Arc::new(AtomicUsize::new(0));
        let producer = {
            let sender = sender.clone();
            let sent = Arc::clone(&sent);
            tokio::spawn(async move {
                loop {
                    let bulk = SessionCommand::SendFrame {
                        target: FrameTarget::Session(session),
                        message_type: MessageType::ClipboardData.wire(),
                        payload: vec![0xBB; 128 * 1024],
                    };
                    if sender.send(bulk).await.is_err() {
                        return;
                    }
                    sent.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        let mut previous = usize::MAX;
        loop {
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            let now = sent.load(Ordering::Relaxed);
            assert!(now < 4096, "the background path never saturated");
            if now == previous {
                assert!(!producer.is_finished(), "the producer stopped early");
                return Wedged {
                    producer,
                    sent,
                    at_wedge: now,
                };
            }
            previous = now;
        }
    }

    /// The hop the ADR 0013 drift-check singled out: the command mux
    /// *awaits* delivery into each session's queue, so a single task
    /// draining both classes would let one wedged Background path stall
    /// input for every session.
    #[tokio::test]
    async fn a_wedged_background_path_never_delays_a_high_frame_through_the_mux() {
        let mut rig = wedge_rig();
        let wedged = wedge_background(&rig.clipboard, rig.session).await.at_wedge;
        assert!(
            wedged >= MAX_BACKGROUND_QUEUE_FRAMES,
            "expected a full bulk backlog, got {wedged} commands"
        );

        // With bulk wedged solid, an input batch still crosses the mux…
        rig.control
            .send(SessionCommand::SendFrame {
                target: FrameTarget::Session(rig.session),
                message_type: MessageType::InputBatch.wire(),
                payload: b"pointer moved".to_vec(),
            })
            .await
            .unwrap();
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        // …and is the very next frame the writer would put on the wire.
        let frame = rig.outbound.recv().await.expect("a frame for the writer");
        assert_eq!(
            frame.message_type,
            MessageType::InputBatch.wire(),
            "input queued behind {wedged} bulk commands"
        );
        assert_eq!(frame.priority, SendPriority::High);
        assert_eq!(frame.payload, b"pointer moved");
    }

    /// The sharper case, and the one a per-*source* forwarder gets wrong:
    /// the High command comes from the **same driver** whose bulk wedged the
    /// path.
    ///
    /// A peer that stops reading its socket and then sends a malformed
    /// clipboard payload must still get its session killed. If the
    /// termination queues behind the bulk it just stalled, that peer has
    /// bought immunity from the fail-closed path (docs/PROTOCOL.md §7) by
    /// doing nothing more than refusing to read.
    #[tokio::test]
    async fn a_termination_overtakes_the_bulk_from_its_own_driver() {
        let mut rig = wedge_rig();
        let wedged = wedge_background(&rig.clipboard, rig.session).await.at_wedge;
        assert!(wedged >= MAX_BACKGROUND_QUEUE_FRAMES);
        assert!(
            !*rig.killed.borrow_and_update(),
            "the session died before the test began"
        );

        rig.clipboard
            .send(SessionCommand::TerminateSession {
                target: FrameTarget::Session(rig.session),
                reason: "malformed clipboard payload".to_owned(),
            })
            .await
            .expect("a wedged bulk lane blocked a fail-closed termination");
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        assert!(
            *rig.killed.borrow_and_update(),
            "the session was never killed: the termination is stuck behind \
             {wedged} bulk commands from the same driver"
        );
    }

    /// The teardown deadlock, in a test.
    ///
    /// Every step of session teardown after `run_session` returns —
    /// `drain.await`, `SessionFanout::lost` — pushes into the drivers'
    /// bounded event channels, and a driver parked on Background
    /// backpressure is not draining them. Dropping the session's receiver
    /// is the only thing that frees that path, so if it happens *after*
    /// those awaits the whole cycle closes: teardown waits on the driver,
    /// the driver waits on the send path, the send path waits on teardown.
    ///
    /// Structural: the producer's progress is the evidence. Retiring the
    /// route must unpark it; removing the registry entry alone must not be
    /// mistaken for having done so.
    #[tokio::test]
    async fn retiring_a_dead_session_unparks_everything_queued_behind_it() {
        use super::retire_session_route;

        let rig = wedge_rig();
        let wedged = wedge_background(&rig.clipboard, rig.session).await;
        assert!(wedged.at_wedge >= MAX_BACKGROUND_QUEUE_FRAMES);

        // Registry removal on its own changes nothing: the queues, not the
        // routing table, are what the producer is parked on.
        registry_lock(&rig.registry).remove(&rig.session);
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            wedged.sent(),
            wedged.at_wedge,
            "registry removal appeared to unpark the producer; the test is \
             no longer proving what it claims"
        );

        // Retiring the route drops the receiver, closes the byte budget,
        // and the whole chain drains.
        retire_session_route(rig.outbound, &rig.registry, rig.session);
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
        assert!(
            wedged.sent() > wedged.at_wedge,
            "the producer is still parked after the session was retired: \
             stuck at {} commands",
            wedged.at_wedge
        );
        wedged.producer.abort();
    }

    /// The deadlock guarded at the level that actually ships: the whole
    /// `finish_session` sequence, with a real mux, a real fanout, and a
    /// driver stand-in that parks exactly where the clipboard driver does —
    /// mid-send, and therefore not draining its own event channel.
    ///
    /// Testing `retire_session_route` alone was not enough: it left the one
    /// line a refactor is most likely to move — *where* teardown calls it —
    /// completely unguarded. Move it below `drain.await` and this hangs.
    #[tokio::test]
    async fn the_teardown_sequence_completes_against_a_parked_driver() {
        use crossover_core::supervision::SessionEvent;
        use crossover_protocol::RawFrame;

        use super::{SessionFanout, finish_session};

        let rig = wedge_rig();

        // Deliberately shallow, so the drain task reaches the block quickly.
        let (sync_tx, mut sync_rx) = tokio::sync::mpsc::channel(4);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);
        let fanout = SessionFanout {
            sync: sync_tx,
            control: control_tx,
            metrics: None,
        };

        // The clipboard driver, in miniature: a serial loop that turns each
        // event into an outbound frame. Once the send path wedges it parks
        // mid-send and stops consuming events — the whole point.
        let clipboard_driver = {
            let sender = rig.clipboard.clone();
            let session = rig.session;
            tokio::spawn(async move {
                while sync_rx.recv().await.is_some() {
                    let bulk = SessionCommand::SendFrame {
                        target: FrameTarget::Session(session),
                        message_type: MessageType::ClipboardData.wire(),
                        payload: vec![0xBB; 4 * 1024 * 1024],
                    };
                    if sender.send(bulk).await.is_err() {
                        return;
                    }
                }
            })
        };
        // The control driver drains freely; it is not what parks.
        let control_driver =
            tokio::spawn(async move { while control_rx.recv().await.is_some() {} });

        // The session's own event drain, exactly as `listener_loop` spawns it.
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
        let drain = {
            let fanout = fanout.clone();
            let session = rig.session;
            tokio::spawn(async move {
                while let Some(event) = events_rx.recv().await {
                    if let SessionEvent::Frame(frame) = event {
                        fanout.frame(session, frame).await;
                    }
                }
            })
        };

        // Fill the pipeline until it stops accepting: driver parked, event
        // channels full, drain task blocked pushing into them.
        while events_tx
            .try_send(SessionEvent::Frame(RawFrame {
                message_type: MessageType::ClipboardData.wire(),
                message_id: 1,
                payload: b"inbound".to_vec(),
            }))
            .is_ok()
        {}
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
        assert!(
            !clipboard_driver.is_finished() && !drain.is_finished(),
            "nothing parked; this test is not exercising the deadlock"
        );

        // The session dies. Teardown must complete.
        let teardown = {
            let fanout = fanout.clone();
            let registry = Arc::clone(&rig.registry);
            let session = rig.session;
            tokio::spawn(async move {
                finish_session(rig.outbound, &registry, session, events_tx, drain, &fanout).await;
            })
        };
        for _ in 0..1024 {
            tokio::task::yield_now().await;
        }
        assert!(
            teardown.is_finished(),
            "session teardown deadlocked against a parked driver: the send \
             path is retired too late to unpark it"
        );
        clipboard_driver.abort();
        control_driver.abort();
    }

    /// A session ending must release held input even while the clipboard
    /// driver is backed up, so the release path cannot be sequenced behind
    /// the clipboard one. A stuck key after disconnect is release-blocking
    /// (CLAUDE.md).
    #[tokio::test]
    async fn session_loss_reaches_the_control_driver_past_a_full_clipboard_queue() {
        use super::SessionFanout;
        use crossover_core::{InputControlEvent, SyncEvent};

        let (sync_tx, _sync_rx) = tokio::sync::mpsc::channel(4);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(4);
        let fanout = SessionFanout {
            sync: sync_tx,
            control: control_tx,
            metrics: None,
        };

        // Back the clipboard driver's channel right up; nothing drains it.
        while fanout.sync.try_send(SyncEvent::LocalChanged).is_ok() {}

        let session = Uuid::from_bytes([0x77; 16]);
        let lost = {
            let fanout = fanout.clone();
            tokio::spawn(async move { fanout.lost(session).await })
        };
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        let event = control_rx
            .try_recv()
            .expect("session loss never reached the control driver");
        assert!(matches!(
            event,
            InputControlEvent::SessionLost { session: id } if id == session
        ));
        // And it got there without the clipboard send having completed —
        // proving the two are concurrent, not sequenced.
        assert!(
            !lost.is_finished(),
            "the clipboard channel drained; this test no longer proves the \
             release path is independent of it"
        );
        lost.abort();
    }

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
