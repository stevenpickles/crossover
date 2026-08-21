//! The worker's half of the worker→editor state file
//! ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)):
//! building `TopologyState` snapshots from what this run knows, and
//! writing them to `~/.crossover/state/topology.json` atomically and
//! coalesced.
//!
//! `crossover-topology::state` is the *schema* — the types, the version
//! check, the round trip. `crossover-topology::atomic_write` is the
//! *mechanism* — temp-file-and-rename, shared with `persist_layout`'s
//! config-file write so the two do not keep independent copies of
//! durability-critical code. This module is the *writer*: the heartbeat,
//! the coalescing, and the two producers that keep the document current (a
//! ~1 s poll of this machine's own displays, and the ~2 s config re-read
//! beside `commands::apply_trust_changes`).
//!
//! # Scope of this branch (feature/153)
//!
//! Only this machine's own facts are reported here: its identity, its live
//! monitors, and the layout this run currently holds. **`peer` stays
//! `None` for the life of this branch** — reporting the peer's last-known
//! monitors needs the live sync/resolver work, which is the next branch's
//! job. `commands::apply_config_changes` leaves the seam that branch
//! connects: a `layout_changed` sender, offered every *changed* valid
//! explicit layout, that this branch always passes as `None`.
//!
//! # What "keep the last good" means here
//!
//! Every producer treats a failure to learn something new as a reason to
//! say nothing, not a reason to erase what is already known:
//!
//! - **A transient enumeration failure** (`DisplayInfo::monitor_layout`
//!   erroring — reachable on the 1 s poll from a session lock, an RDP
//!   disconnect, or a display waking from sleep) leaves the previously
//!   reported monitor list untouched, logged once on the way in and once
//!   on the way out of the failure streak.
//! - **More monitors than [`crossover_topology::MAX_MONITORS_PER_MACHINE`]**
//!   is not truncated to fit — ADR 0018's rule for `MonitorTopology`
//!   applies here too: refuse to report rather than silently describe a
//!   desk with screens missing. The ongoing poll keeps the last known
//!   list; a startup enumeration that is already over the cap has no last
//!   good to fall back to, so it reports an empty list rather than a
//!   truncated, falsely-complete one — both cases logged loudly.
//! - **A config re-read that fails to parse or fails `[layout]`
//!   validation** keeps the state file's last good layout (see
//!   `commands::apply_config_changes`).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crossover_platform::{DisplayError, DisplayInfo};
use crossover_topology::{
    AtomicWriteError, DeviceId, LayoutRect, LayoutState, LiveMonitor, MAX_MONITORS_PER_MACHINE,
    MachineState, MonitorId, StateError, TOPOLOGY_STATE_VERSION, TopologyState, now_unix_millis,
    serialize_state, write_atomic,
};

use crate::config::LayoutSource;

/// How often the own-display poll samples `monitor_layout()` for a change,
/// so the state file picks one up without waiting on a config re-read.
/// Deliberately separate from the 8 ms edge-detection poll
/// (`commands::EDGE_POLL_INTERVAL`): the state file is a report for a
/// human editor, not a control-transfer input, and a monitor
/// reconfiguration is not latency-sensitive the way a crossing is.
const DISPLAY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The writer's coalescing window: the same cadence as
/// [`crossover_topology::HEARTBEAT_INTERVAL_MS`], so a burst of updates —
/// a display change landing in the same tick as a config re-read — costs
/// at most one write rather than one each.
const STATE_WRITE_COALESCE: Duration =
    Duration::from_millis(crossover_topology::HEARTBEAT_INTERVAL_MS);

/// Lock `mutex`, recovering the guard even if a prior holder panicked —
/// this crate never gives a poisoned lock a reason to matter, so surfacing
/// the panic here would only turn one failure into two.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Owns the current `TopologyState` snapshot and a coalesced writer task
/// that keeps `~/.crossover/state/topology.json` in step with it.
///
/// Every setter is content-equality gated, atomically (`send_if_modified`
/// rather than a separate borrow-then-`send_modify`): a call that would
/// not change anything is a no-op, both for the in-memory snapshot and for
/// the file — which is what keeps a periodic poll finding nothing new from
/// writing (or logging) anything at all, and what keeps two producers
/// calling a setter around the same time from racing a read against a
/// write.
pub struct TopologyStateWriter {
    path: PathBuf,
    state: watch::Sender<TopologyState>,
    /// The coalesced background task ([`run_writer`]), taken and stopped
    /// by [`Self::write_final`] before it does its own write. `None` once
    /// that has happened.
    writer_task: Mutex<Option<JoinHandle<()>>>,
}

impl TopologyStateWriter {
    /// Start the writer: an immediate first write of `initial` (ADR 0018's
    /// exception to the coalescing rule), then a background task that
    /// coalesces every update after it.
    #[must_use]
    pub fn start(path: PathBuf, initial: TopologyState) -> Self {
        write_state_file_logged(&path, &initial);
        let (state, updates) = watch::channel(initial);
        let writer_task = tokio::spawn(run_writer(path.clone(), updates));
        Self {
            path,
            state,
            writer_task: Mutex::new(Some(writer_task)),
        }
    }

    /// The snapshot as it stands right now.
    #[must_use]
    pub fn snapshot(&self) -> TopologyState {
        self.state.borrow().clone()
    }

    /// Replace this machine's reported monitors, if `monitors` differs from
    /// what is already held. Returns whether anything changed.
    pub fn set_monitors(&self, monitors: Vec<LiveMonitor>) -> bool {
        self.state.send_if_modified(|state| {
            if state.local.monitors == monitors {
                return false;
            }
            state.local.monitors = monitors;
            state.written_at = now_unix_millis();
            true
        })
    }

    /// Replace the reported layout, if `layout` differs from what is
    /// already held (ADR 0018's content-equality no-op — this is what
    /// keeps a worker's own future adoption-writes from echoing into a
    /// worker↔peer sync loop once the hub branch lands). Returns whether
    /// anything changed.
    pub fn set_layout(&self, layout: Option<LayoutState>) -> bool {
        self.state.send_if_modified(|state| {
            if state.layout == layout {
                return false;
            }
            state.layout = layout;
            state.written_at = now_unix_millis();
            true
        })
    }

    /// Refresh `written_at` — the heartbeat (ADR 0018) — whether or not
    /// anything else changed. Called once per config-poll tick, the same
    /// cadence [`crossover_topology::HEARTBEAT_INTERVAL_MS`] names, so the
    /// worker's periodic work stays one rhythm.
    pub fn heartbeat(&self) {
        self.state.send_modify(|state| {
            state.written_at = now_unix_millis();
        });
    }

    /// The final, synchronous write on clean shutdown (ADR 0018): a fresh
    /// heartbeat over whatever this run last knew. The file is never
    /// deleted — the editor uses the last-known document while the worker
    /// is down.
    ///
    /// Stops the coalesced background task ([`run_writer`]) first. Without
    /// that, a write it already had in flight — queued before shutdown,
    /// possibly still sleeping out its coalescing window against a snapshot
    /// taken before this one — could land *after* this call's write and
    /// silently overwrite the fresher heartbeat with a stale one. Aborting
    /// cancels a pending sleep immediately; awaiting the handle afterward
    /// guarantees this call's own write never races one still in flight,
    /// whichever way that task was stopped.
    pub async fn write_final(&self) {
        let handle = lock(&self.writer_task).take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
        let mut state = self.snapshot();
        state.written_at = now_unix_millis();
        write_state_file_logged(&self.path, &state);
    }
}

/// The coalesced background writer: waits for a change, then waits out the
/// rest of [`STATE_WRITE_COALESCE`] since the last write (none, the first
/// time — [`TopologyStateWriter::start`] already wrote the initial
/// document before this task exists), and writes whatever the latest value
/// is by then. `watch` retains only the latest sender value, so a burst of
/// updates during the wait coalesces for free.
///
/// A write failure is logged once per failure streak
/// (`commands::apply_config_changes` uses the same shape): a full disk or
/// a locked file must not turn a 2 s retry loop into a log line every 2 s.
async fn run_writer(path: PathBuf, mut updates: watch::Receiver<TopologyState>) {
    let mut last_write = Instant::now();
    let mut warned = false;
    loop {
        if updates.changed().await.is_err() {
            // The task was stopped (aborted by `write_final`, or its
            // sender dropped with the process exiting some other way).
            return;
        }
        let elapsed = last_write.elapsed();
        if let Some(remaining) = STATE_WRITE_COALESCE.checked_sub(elapsed) {
            tokio::time::sleep(remaining).await;
        }
        let state = updates.borrow_and_update().clone();
        match write_state_file(&path, &state) {
            Ok(()) => warned = false,
            Err(error) => {
                if !warned {
                    tracing::warn!(
                        error = %error,
                        path = %path.display(),
                        "topology state: failed to write the state file; will keep retrying \
                         quietly until it succeeds"
                    );
                    warned = true;
                }
            }
        }
        last_write = Instant::now();
    }
}

/// The initial snapshot for a fresh run: this machine's identity, its live
/// monitors, the layout this run started with (if explicit), and no peer
/// yet.
#[must_use]
pub fn initial_state(
    device: DeviceId,
    name: String,
    display: &dyn DisplayInfo,
    layout_source: Option<&LayoutSource>,
) -> TopologyState {
    let monitors = match live_monitors(display) {
        Ok(monitors) => monitors,
        Err(LiveMonitorsError::Unavailable(error)) => {
            tracing::warn!(
                error = %error,
                "topology state: could not enumerate monitors at startup"
            );
            Vec::new()
        }
        // No last-good snapshot exists yet to fall back to at startup;
        // an empty list is the honest "nothing usable" report, never a
        // truncated one (ADR 0018).
        Err(LiveMonitorsError::TooManyMonitors { count }) => {
            tracing::error!(
                count,
                max = MAX_MONITORS_PER_MACHINE,
                "topology state: more monitors than this build can report at startup; \
                 refusing to describe an incomplete desk (ADR 0018)"
            );
            Vec::new()
        }
    };
    TopologyState {
        version: TOPOLOGY_STATE_VERSION,
        written_at: now_unix_millis(),
        local: MachineState {
            device,
            name,
            monitors,
        },
        // The hub branch fills this in as sessions come and go; this
        // branch never has a peer to report.
        peer: None,
        layout: layout_state_of(layout_source),
    }
}

/// The state file's report of `layout_source` — `Some` only for an
/// explicit drawn layout; an implicit (side-model) or absent source
/// reports no layout, exactly as the config schema itself only ever
/// persists an explicit one (ADR 0018).
fn layout_state_of(layout_source: Option<&LayoutSource>) -> Option<LayoutState> {
    match layout_source {
        Some(LayoutSource::Explicit(layout)) => Some(LayoutState::from_layout(layout)),
        Some(LayoutSource::Implicit(_)) | None => None,
    }
}

/// Why this machine's live monitors could not be reported this cycle.
#[derive(Debug)]
enum LiveMonitorsError {
    /// The platform could not enumerate monitors at all.
    Unavailable(DisplayError),
    /// More monitors than [`MAX_MONITORS_PER_MACHINE`] were enumerated.
    /// ADR 0018's rule for a machine like this: refuse to report rather
    /// than silently truncate to a desk with screens missing.
    TooManyMonitors { count: usize },
}

/// This machine's live monitors, in the state file's shape.
///
/// The per-machine cap is checked against the **raw** enumerated count,
/// before anything is built or filtered — bound before allocation, as
/// everywhere else — so the check is exact: 16 real monitors
/// ([`MAX_MONITORS_PER_MACHINE`]) never trips it and 17 always does,
/// whatever mix of them the platform can or cannot name.
///
/// A monitor the platform could not name (`MonitorInfo::id == None`) is
/// omitted rather than fabricated an id — the same treatment ADR 0018 gives
/// an unidentified monitor everywhere else: it degrades placement, never
/// geometry, and a layout could not have addressed it either. A monitor
/// whose id or rectangle somehow fails [`LiveMonitor`]'s own bounds (which
/// no real platform report should) is likewise omitted and logged, rather
/// than the whole document being refused for one bad entry.
///
/// # Errors
///
/// [`LiveMonitorsError`] — the caller decides what "nothing new to report"
/// means for it (empty at startup, keep the last known list on an ongoing
/// poll; see this module's header).
fn live_monitors(display: &dyn DisplayInfo) -> Result<Vec<LiveMonitor>, LiveMonitorsError> {
    let monitors = display
        .monitor_layout()
        .map_err(LiveMonitorsError::Unavailable)?;
    if monitors.len() > MAX_MONITORS_PER_MACHINE {
        return Err(LiveMonitorsError::TooManyMonitors {
            count: monitors.len(),
        });
    }

    let mut live = Vec::with_capacity(monitors.len());
    for monitor in monitors {
        let Some(id) = monitor.id else {
            continue;
        };
        let id = match MonitorId::new(&id) {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "topology state: an unusable monitor id; the monitor is omitted"
                );
                continue;
            }
        };
        let rect = LayoutRect {
            x: monitor.rect.left,
            y: monitor.rect.top,
            width: monitor.rect.width,
            height: monitor.rect.height,
        };
        if let Err(violation) = rect.check_bounds() {
            tracing::warn!(
                monitor = %id,
                error = %violation,
                "topology state: monitor rectangle out of bounds; the monitor is omitted"
            );
            continue;
        }
        live.push(LiveMonitor {
            id,
            rect,
            // `DisplayInfo` has no per-monitor scale query yet
            // (crossover-platform's `display` module: `MonitorInfo` carries
            // geometry and an id, not a scale). Until a later branch adds
            // one, every monitor reports 100 (unscaled) here. That only
            // affects the editor's DIP seeding size — crossing mapping
            // stays proportional through the drawn geometry regardless
            // (ADR 0018) — so the gap costs a slightly-off preview picture,
            // never correctness.
            scale_percent: 100,
        });
    }
    Ok(live)
}

/// Poll this machine's own displays for a change and keep `writer` current
/// (ADR 0018) — deliberately separate from edge detection's own re-read on
/// display change (feature/107), which this task does not touch.
///
/// A failure — transient enumeration trouble, or too many monitors to
/// report — leaves the writer's monitor list exactly as it was: see this
/// module's header for why. `erroring` tracks the streak so the log gets
/// one line on the way in and one on the way out, not one every tick.
///
/// Never returns; spawned as its own task rather than a branch of `run`'s
/// foreground select, the same shape as the edge-detector and control
/// driver tasks `commands::setup_input_control` spawns.
pub async fn watch_own_display(
    display: std::sync::Arc<dyn DisplayInfo>,
    writer: std::sync::Arc<TopologyStateWriter>,
) -> ! {
    let mut ticker = tokio::time::interval(DISPLAY_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut erroring = false;
    loop {
        ticker.tick().await;
        match live_monitors(&*display) {
            Ok(monitors) => {
                if erroring {
                    tracing::info!("topology state: display enumeration recovered");
                    erroring = false;
                }
                if writer.set_monitors(monitors) {
                    tracing::info!("topology state: local display configuration changed");
                }
            }
            Err(LiveMonitorsError::Unavailable(error)) => {
                if !erroring {
                    tracing::warn!(
                        error = %error,
                        "topology state: could not enumerate monitors; keeping the last \
                         known list"
                    );
                    erroring = true;
                }
            }
            Err(LiveMonitorsError::TooManyMonitors { count }) => {
                if !erroring {
                    tracing::error!(
                        count,
                        max = MAX_MONITORS_PER_MACHINE,
                        "topology state: more monitors than this build can report; keeping \
                         the last known list rather than describing an incomplete desk \
                         (ADR 0018)"
                    );
                    erroring = true;
                }
            }
        }
    }
}

/// Why writing the state file failed — either half of the job
/// ([`serialize_state`] or [`write_atomic`]) reported as one `Display`.
///
/// Plain `From`/`Display` rather than `thiserror` — this app layer's
/// convention (docs/ARCHITECTURE.md §9) is `anyhow` at the boundary and
/// typed errors only where a caller branches on the variant, which nothing
/// does here; this exists purely so `?` and one log line can see both
/// failure kinds without matching on them.
#[derive(Debug)]
enum WriteStateError {
    Serialize(StateError),
    Atomic(AtomicWriteError),
}

impl std::fmt::Display for WriteStateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(error) => {
                write!(
                    formatter,
                    "serializing the topology state document: {error}"
                )
            }
            Self::Atomic(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<StateError> for WriteStateError {
    fn from(error: StateError) -> Self {
        Self::Serialize(error)
    }
}

impl From<AtomicWriteError> for WriteStateError {
    fn from(error: AtomicWriteError) -> Self {
        Self::Atomic(error)
    }
}

/// Render and atomically write `state` to `path`
/// ([`crossover_topology::write_atomic`], ADR 0018), so a reader — the
/// editor included — sees a whole document or the previous one, never a
/// half-written one.
fn write_state_file(path: &Path, state: &TopologyState) -> Result<(), WriteStateError> {
    let rendered = serialize_state(state)?;
    write_atomic(path, &rendered)?;
    Ok(())
}

/// [`write_state_file`], logged rather than propagated: a failure to write
/// this report must never take down the run it is reporting on. Used only
/// at the two call sites that write exactly once ([`TopologyStateWriter::start`]'s
/// initial write and [`TopologyStateWriter::write_final`]'s), where "once"
/// makes an unconditional log line correct; [`run_writer`]'s loop keeps its
/// own warn-once-per-streak state instead.
fn write_state_file_logged(path: &Path, state: &TopologyState) {
    if let Err(error) = write_state_file(path, state) {
        tracing::warn!(
            error = %error,
            path = %path.display(),
            "topology state: failed to write the state file"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crossover_platform::fakes::{FakeDisplay, fake_monitor_id};
    use crossover_platform::{DisplayInfo, MonitorInfo, MonitorRect, Screen};
    use crossover_topology::{DeviceId, MAX_MONITORS_PER_MACHINE, parse_state};

    use super::{
        LiveMonitorsError, TopologyStateWriter, initial_state, live_monitors, watch_own_display,
    };

    /// A private directory removed on drop — the house substitute for a
    /// `tempfile` dependency (mirrors `crossover_topology::config`'s test
    /// `Sandbox`).
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crossover-topology-state-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("sandbox");
            Self(dir)
        }

        fn path(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stray_files(directory: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(directory)
            .expect("read sandbox")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "topology.json")
            .collect()
    }

    const LOCAL: DeviceId = DeviceId::from_bytes([0x11; 16]);

    fn display(width: u32, height: u32) -> FakeDisplay {
        FakeDisplay::new(Screen { width, height })
    }

    fn monitor_at(index: usize, left: i32) -> MonitorInfo {
        MonitorInfo {
            id: Some(fake_monitor_id(index)),
            rect: MonitorRect {
                left,
                top: 0,
                width: 100,
                height: 100,
            },
        }
    }

    /// Startup writes a valid, round-tripping snapshot — the property the
    /// whole file exists for.
    #[tokio::test]
    async fn startup_writes_a_valid_round_tripping_snapshot() {
        let sandbox = Sandbox::new("startup");
        let path = sandbox.path("topology.json");
        let display = display(1920, 1080);

        let state = initial_state(LOCAL, "workstation".to_owned(), &display, None);
        let writer = TopologyStateWriter::start(path.clone(), state.clone());

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_state(&written).unwrap();
        assert_eq!(parsed, state);
        assert_eq!(parsed.local.device, LOCAL);
        assert_eq!(parsed.local.monitors.len(), 1);
        assert!(parsed.peer.is_none());
        assert!(stray_files(&sandbox.0).is_empty(), "a temp file survived");

        // Keep the writer's background task from outliving the test.
        drop(writer);
    }

    /// A monitor the platform could not name is omitted, never fabricated
    /// an id — the same treatment ADR 0018 gives an unidentified monitor
    /// everywhere else.
    #[test]
    fn an_unnamed_monitor_is_omitted_not_fabricated() {
        let display = display(1920, 1080);
        display.set_monitor_layout(vec![
            monitor_at(0, 0),
            MonitorInfo {
                id: None,
                rect: MonitorRect {
                    left: 1920,
                    top: 0,
                    width: 1280,
                    height: 1024,
                },
            },
        ]);
        let live = live_monitors(&display).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id.as_str(), fake_monitor_id(0));
    }

    /// The exact boundary ADR 0018's per-machine cap is held to: exactly
    /// [`MAX_MONITORS_PER_MACHINE`] real monitors is a machine this build
    /// can fully describe, not one to refuse.
    #[test]
    fn exactly_the_cap_is_reported_in_full_not_refused() {
        let display = display(100, 100);
        display.set_monitor_layout(
            (0..MAX_MONITORS_PER_MACHINE)
                .map(|index| monitor_at(index, i32::try_from(index).unwrap() * 100))
                .collect(),
        );
        let live = live_monitors(&display).unwrap();
        assert_eq!(live.len(), MAX_MONITORS_PER_MACHINE);
    }

    /// One monitor past the cap refuses to report — never truncates to a
    /// desk with screens silently missing (ADR 0018).
    #[test]
    fn one_past_the_cap_refuses_rather_than_truncates() {
        let display = display(100, 100);
        display.set_monitor_layout(
            (0..=MAX_MONITORS_PER_MACHINE)
                .map(|index| monitor_at(index, i32::try_from(index).unwrap() * 100))
                .collect(),
        );
        assert!(matches!(
            live_monitors(&display),
            Err(LiveMonitorsError::TooManyMonitors { count }) if count == MAX_MONITORS_PER_MACHINE + 1
        ));
    }

    /// A startup enumeration already over the cap has no last-good to fall
    /// back to: it reports an empty list (never a truncated, falsely
    /// complete one) rather than failing the whole run.
    #[test]
    fn an_over_cap_startup_enumeration_reports_empty_not_truncated() {
        let display = display(100, 100);
        display.set_monitor_layout(
            (0..=MAX_MONITORS_PER_MACHINE)
                .map(|index| monitor_at(index, i32::try_from(index).unwrap() * 100))
                .collect(),
        );
        let state = initial_state(LOCAL, "workstation".to_owned(), &display, None);
        assert!(state.local.monitors.is_empty());
    }

    /// A content change is coalesced: the file on disk does not move until
    /// the coalescing window elapses, and then it moves to the *latest*
    /// value — proven directly against [`TopologyStateWriter`], under a
    /// paused clock, rather than through the timing of a separate poll
    /// task (which [`own_display_poll_updates_the_state_file_on_a_real_change`]
    /// exercises end to end instead).
    #[tokio::test(start_paused = true)]
    async fn a_monitor_change_triggers_a_new_coalesced_write() {
        let sandbox = Sandbox::new("monitor-change");
        let path = sandbox.path("topology.json");
        let display = display(1920, 1080);
        let state = initial_state(LOCAL, "workstation".to_owned(), &display, None);
        let writer = TopologyStateWriter::start(path.clone(), state);

        let first_written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_state(&first_written).unwrap().local.monitors.len(), 1);

        let mut with_second = writer.snapshot().local.monitors;
        with_second.push(crossover_topology::LiveMonitor {
            id: crossover_topology::MonitorId::new("SECOND").unwrap(),
            rect: crossover_topology::LayoutRect {
                x: 1920,
                y: 0,
                width: 1280,
                height: 1024,
            },
            scale_percent: 100,
        });
        assert!(writer.set_monitors(with_second));

        // Immediately after: the coalescing window has not elapsed, so the
        // file on disk is still the previous, whole document.
        tokio::task::yield_now().await;
        let still_old = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            parse_state(&still_old).unwrap().local.monitors.len(),
            1,
            "wrote before the coalescing window elapsed"
        );

        // Once it elapses, the latest snapshot lands as one atomic write.
        tokio::time::sleep(super::STATE_WRITE_COALESCE + std::time::Duration::from_millis(50))
            .await;
        let updated = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_state(&updated).unwrap();
        assert_eq!(parsed.local.monitors.len(), 2, "{updated}");
        assert!(stray_files(&sandbox.0).is_empty(), "a temp file survived");
    }

    /// The same change, end to end through [`watch_own_display`]'s own
    /// poll — bounded by a generous real-time timeout rather than the
    /// paused clock, since it exercises two independently-scheduled tasks
    /// (the 1 s poll and the coalesced writer) together.
    #[tokio::test]
    async fn own_display_poll_updates_the_state_file_on_a_real_change() {
        let sandbox = Sandbox::new("own-display-poll");
        let path = sandbox.path("topology.json");
        let display = Arc::new(display(1920, 1080));
        let state = initial_state(LOCAL, "workstation".to_owned(), &*display, None);
        let writer = Arc::new(TopologyStateWriter::start(path.clone(), state));

        let display_dyn: Arc<dyn DisplayInfo> = display.clone();
        let poll = tokio::spawn(watch_own_display(display_dyn, Arc::clone(&writer)));

        display.set_monitors(vec![
            MonitorRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
            MonitorRect {
                left: 1920,
                top: 0,
                width: 1280,
                height: 1024,
            },
        ]);

        let deadline = std::time::Duration::from_secs(6);
        let observed = tokio::time::timeout(deadline, async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&path)
                    && let Ok(parsed) = parse_state(&text)
                    && parsed.local.monitors.len() == 2
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;

        poll.abort();
        assert!(
            observed.is_ok(),
            "the state file never picked up the display change within {deadline:?}"
        );
        assert!(stray_files(&sandbox.0).is_empty());
    }

    /// A transient enumeration failure — a session lock, RDP, a display
    /// asleep — leaves the reported monitor list exactly as it was, rather
    /// than clearing it. Real time, bounded, because it exercises the same
    /// two independently-scheduled tasks as the test above.
    #[tokio::test]
    async fn a_transient_enumeration_failure_keeps_the_last_known_monitor_list() {
        let sandbox = Sandbox::new("transient-failure");
        let path = sandbox.path("topology.json");
        let display = Arc::new(display(1920, 1080));
        let state = initial_state(LOCAL, "workstation".to_owned(), &*display, None);
        let writer = Arc::new(TopologyStateWriter::start(path.clone(), state));

        let display_dyn: Arc<dyn DisplayInfo> = display.clone();
        let poll = tokio::spawn(watch_own_display(display_dyn, Arc::clone(&writer)));

        // Let the poll observe the healthy display at least once.
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        let before = writer.snapshot().local.monitors;
        assert_eq!(before.len(), 1);

        // Now the platform cannot enumerate at all — long enough for
        // several poll ticks to see nothing but failure.
        display.fail_with("session locked");
        tokio::time::sleep(std::time::Duration::from_millis(2_200)).await;

        poll.abort();
        assert_eq!(
            writer.snapshot().local.monitors,
            before,
            "a transient enumeration failure changed the reported monitor list"
        );
    }

    /// The naming discipline every write shares
    /// (`crossover_topology::atomic_write`) is that module's own test now;
    /// no torn document and no stray temp file is what every test above
    /// already proves for this writer's own call sites.
    #[tokio::test]
    async fn set_monitors_and_set_layout_are_content_equality_gated() {
        let sandbox = Sandbox::new("no-op");
        let path = sandbox.path("topology.json");
        let display = display(1920, 1080);
        let state = initial_state(LOCAL, "workstation".to_owned(), &display, None);
        let same_monitors = state.local.monitors.clone();
        let writer = TopologyStateWriter::start(path, state);

        assert!(
            !writer.set_monitors(same_monitors),
            "an identical monitor list reported a change"
        );
        assert!(
            !writer.set_layout(None),
            "an identical (absent) layout reported a change"
        );
    }

    /// The final write happens synchronously and lands the freshest
    /// heartbeat, independent of the background task's coalescing timer.
    #[tokio::test]
    async fn write_final_lands_immediately_with_a_fresh_heartbeat() {
        let sandbox = Sandbox::new("final");
        let path = sandbox.path("topology.json");
        let display = display(1920, 1080);
        let state = initial_state(LOCAL, "workstation".to_owned(), &display, None);
        let first_heartbeat = state.written_at;
        let writer = TopologyStateWriter::start(path.clone(), state);

        writer.write_final().await;
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_state(&written).unwrap();
        assert!(parsed.written_at >= first_heartbeat);
        assert!(stray_files(&sandbox.0).is_empty());
    }

    /// The race `write_final` exists to close: a coalesced write already
    /// woken and mid-sleep, over a snapshot older than the one shutdown
    /// wants written, must not land after `write_final`'s own write and
    /// silently overwrite it.
    #[tokio::test(start_paused = true)]
    async fn write_final_wins_over_a_coalesced_write_already_in_flight() {
        let sandbox = Sandbox::new("final-race");
        let path = sandbox.path("topology.json");
        let display = display(1920, 1080);
        let state = initial_state(LOCAL, "workstation".to_owned(), &display, None);
        let writer = TopologyStateWriter::start(path.clone(), state);

        let mut with_second = writer.snapshot().local.monitors;
        with_second.push(crossover_topology::LiveMonitor {
            id: crossover_topology::MonitorId::new("SECOND").unwrap(),
            rect: crossover_topology::LayoutRect {
                x: 1920,
                y: 0,
                width: 1280,
                height: 1024,
            },
            scale_percent: 100,
        });
        assert!(writer.set_monitors(with_second.clone()));

        // Let the background writer wake up and start sleeping out its
        // coalescing window before shutdown races it.
        tokio::task::yield_now().await;

        writer.write_final().await;
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_state(&written).unwrap();
        assert_eq!(parsed.local.monitors, with_second);

        // The background task is stopped: nothing writes again, however
        // long the (paused) clock runs.
        let after_final = std::fs::metadata(&path).unwrap().modified().unwrap();
        tokio::time::sleep(super::STATE_WRITE_COALESCE * 2).await;
        let unchanged = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            after_final, unchanged,
            "something wrote the state file again after write_final stopped the background task"
        );
        assert!(stray_files(&sandbox.0).is_empty());
    }
}
