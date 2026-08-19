//! Edge-detection driver for seamless control transfer (ADR 0009).
//!
//! While this machine controls itself, the cursor reaching the linked
//! edge means *leave* — request control of the peer. While the peer
//! controls this machine, the cursor reaching the same linked edge means
//! *return* — reclaim control. Both are the identical geometric test on
//! the real local cursor ([`crate::topology`], ADR 0009); the **mode**,
//! supplied by the control wiring, says which meaning applies. While this
//! machine is driving the peer, its own cursor is frozen, so detection is
//! idle and the driver does not poll.
//!
//! The pure [`EdgeDetector`] turns a stream of cursor observations into a
//! crossing on the **rising edge only** — the first observation where the
//! cursor reaches the linked edge after being clear of it — so a cursor
//! pinned at the screen edge fires the transfer once, not on every poll.
//! "Clear of it" is a Schmitt trigger, not a bare threshold: the cursor
//! must travel [`REARM_MARGIN`] pixels inward before the next touch counts
//! (see that constant). The async [`EdgeDetectDriver`] polls the display,
//! applies the mode, and emits the crossings.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crossover_platform::DisplayInfo;

use crate::topology::{CursorPoint, EdgeFraction, MonitorRect, Topology};

/// What the detector is currently watching for — driven by the control
/// state (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeMode {
    /// Not watching: this machine is driving the peer, so its own cursor
    /// is frozen and no edge is meaningful.
    Idle,
    /// This machine controls itself; a linked-edge crossing is a request
    /// to control the peer.
    Leaving,
    /// The peer controls this machine; a linked-edge crossing reclaims
    /// control.
    Returning,
}

impl EdgeMode {
    /// The kind of crossing a linked-edge touch means in this mode, or
    /// `None` when idle (no crossing is meaningful).
    fn crossing_kind(self) -> Option<CrossingKind> {
        match self {
            Self::Leaving => Some(CrossingKind::Leave),
            Self::Returning => Some(CrossingKind::Return),
            Self::Idle => None,
        }
    }
}

/// Which direction a detected crossing goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossingKind {
    /// The cursor left across the linked edge: request control of the peer.
    Leave,
    /// The cursor returned to the linked edge: reclaim control.
    Return,
}

/// A detected edge crossing: which way it goes and where along the edge,
/// as a fraction the peer maps through its own geometry (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeCrossing {
    /// Leave or return.
    pub kind: CrossingKind,
    /// Normalized position along the edge.
    pub position: EdgeFraction,
    /// The [`EdgeMode`] generation this crossing was detected under — the
    /// number of mode updates the detector had received when it fired.
    /// `kind` is frozen at detection time, so a crossing that queues behind
    /// a control-state change would otherwise act on a state that no longer
    /// exists (a stale `Return` revoking a *fresh* grant). The control
    /// driver counts the mode updates it sends and discards any crossing
    /// whose generation is not the current one.
    pub generation: u64,
}

/// How far the cursor must travel back inside the screen, in pixels, before
/// a touch of the linked edge counts as a new crossing (ADR 0009 addendum,
/// 2026-08-19).
///
/// A transfer leaves the cursor resting *exactly* on the linked column at
/// both ends — that is deliberate, for cursor continuity — and the linked
/// edge does double duty (leave while local, return while controlled). With
/// a bare one-pixel rising edge, a two-pixel wobble at 125 Hz polling was
/// enough to fire a complete reverse transfer, which re-parked both cursors
/// on their trigger columns and so repeated: ten take/revoke cycles in five
/// seconds on hardware. Requiring real inward travel first makes a wobble
/// inert while costing a deliberate crossing nothing — a real crossing
/// covers far more than this margin on its way to the edge.
pub const REARM_MARGIN: u32 = 24;

/// The pure crossing detector: rising-edge detection against the linked
/// edge, with hysteresis. Holds whether a touch of the edge is currently
/// *armed* — and the monitor layout that judgment was made against — so a
/// crossing fires once on a genuine arrival, never repeatedly while the
/// cursor sits pinned there or jitters against it, and never because the
/// *edge* moved under a stationary cursor. No I/O.
#[derive(Debug)]
pub struct EdgeDetector {
    topology: Topology,
    /// Whether a touch of the linked edge would count as a crossing: set
    /// only by an observation [`REARM_MARGIN`] pixels clear of the linked
    /// column, cleared by every crossing and by priming near the edge.
    armed: bool,
    /// The monitor layout `armed` was computed against. A layout change
    /// (dock, undock, a monitor powering off) moves the linked edge without
    /// the cursor moving, so the flag no longer describes the geometry the
    /// cursor is actually in: an interior column can become the edge in one
    /// tick. The next observation after a change re-primes instead of
    /// firing, so a hotplug never transfers control by itself.
    layout: Vec<MonitorRect>,
}

impl EdgeDetector {
    /// A detector for a machine of this `topology`, disarmed: the first
    /// observation primes it against the layout it carries.
    #[must_use]
    pub fn new(topology: Topology) -> Self {
        Self {
            topology,
            armed: false,
            layout: Vec::new(),
        }
    }

    /// Set the armed state from a cursor **without** emitting a crossing.
    /// Used when detection (re)starts, so a cursor already sitting at the
    /// edge — where a transfer's entry placement puts it — does not fire an
    /// immediate, unintended transfer; only an arrival from clear of the
    /// edge does.
    ///
    /// Priming applies the same [`REARM_MARGIN`] test as
    /// [`observe`](Self::observe), so it arms only for a cursor already
    /// well inside the screen. That also closes the race between the entry
    /// placement and this prime — they run on different tasks, so a few
    /// injected pixels of motion can land in between, and they are nowhere
    /// near enough to arm.
    pub fn prime(&mut self, cursor: CursorPoint, monitors: &[MonitorRect]) {
        self.armed = self.topology.clear_of_edge(cursor, monitors, REARM_MARGIN);
        if self.layout.as_slice() != monitors {
            self.layout = monitors.to_vec();
        }
    }

    /// Whether `monitors` differs from the layout of the last observation.
    #[must_use]
    pub fn layout_changed(&self, monitors: &[MonitorRect]) -> bool {
        self.layout.as_slice() != monitors
    }

    /// Observe a cursor position. Returns the crossing fraction only on an
    /// armed rising edge — the cursor reaching the linked edge after having
    /// been [`REARM_MARGIN`] pixels clear of it — and `None` otherwise
    /// (clear of the edge, still pinned, jittering against it, or a
    /// monitor-layout change, which re-primes against the new geometry
    /// rather than treating a moved edge as an arrival).
    #[must_use]
    pub fn observe(
        &mut self,
        cursor: CursorPoint,
        monitors: &[MonitorRect],
    ) -> Option<EdgeFraction> {
        if self.layout_changed(monitors) {
            self.prime(cursor, monitors);
            return None;
        }
        if self.topology.clear_of_edge(cursor, monitors, REARM_MARGIN) {
            self.armed = true;
        }
        let touching = self.topology.leaving(cursor, monitors);
        if touching.is_some() && self.armed {
            // One crossing per approach: the next needs a fresh re-arm.
            self.armed = false;
            touching
        } else {
            None
        }
    }
}

/// Build an edge-detection driver for `topology`, polling `display` every
/// `poll_interval`. Returns the driver (spawn [`EdgeDetectDriver::run`]),
/// a sender the control wiring uses to set the [`EdgeMode`], and a
/// receiver of detected [`EdgeCrossing`]s.
#[must_use]
pub fn edge_detect(
    display: Arc<dyn DisplayInfo>,
    topology: Topology,
    poll_interval: Duration,
) -> (
    EdgeDetectDriver,
    mpsc::Sender<EdgeMode>,
    mpsc::Receiver<EdgeCrossing>,
) {
    let (mode_tx, mode_rx) = mpsc::channel(8);
    let (crossings_tx, crossings_rx) = mpsc::channel(8);
    let driver = EdgeDetectDriver {
        display,
        detector: EdgeDetector::new(topology),
        mode: EdgeMode::Idle,
        generation: 0,
        mode_rx,
        crossings_tx,
        poll_interval,
    };
    (driver, mode_tx, crossings_rx)
}

/// The async shell: polls the display while watching and emits crossings.
pub struct EdgeDetectDriver {
    display: Arc<dyn DisplayInfo>,
    detector: EdgeDetector,
    mode: EdgeMode,
    /// How many mode updates have been applied. Stamped on every crossing
    /// so a consumer can tell one detected under the current control state
    /// from one that queued behind a state change (see
    /// [`EdgeCrossing::generation`]). The sender counts its own updates, so
    /// the two counts agree without a wider channel type.
    generation: u64,
    mode_rx: mpsc::Receiver<EdgeMode>,
    crossings_tx: mpsc::Sender<EdgeCrossing>,
    poll_interval: Duration,
}

impl EdgeDetectDriver {
    /// Run until the mode sender or the crossings receiver is dropped.
    /// Spawn this. Polls only while watching (not [`EdgeMode::Idle`]), so
    /// an idle machine costs nothing.
    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(self.poll_interval);
        // A skipped poll is fine — the cursor stays pinned at the edge, so
        // the next tick still catches it; never burst-catch-up.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                update = self.mode_rx.recv() => {
                    let Some(mode) = update else { break }; // wiring gone
                    self.mode = mode;
                    // Saturating so a very long-lived process cannot wrap a
                    // stale generation onto a current one.
                    self.generation = self.generation.saturating_add(1);
                    tracing::debug!(?mode, generation = self.generation, "edge: watching mode changed");
                    if mode != EdgeMode::Idle {
                        // Begin from the current cursor so a position
                        // already at the edge does not fire immediately.
                        self.prime();
                        ticker.reset();
                    }
                }
                _ = ticker.tick(), if self.mode != EdgeMode::Idle => {
                    if !self.poll().await {
                        break; // crossings receiver gone
                    }
                }
            }
        }
        tracing::debug!("edge-detection driver stopped");
    }

    /// Read the cursor once and prime the detector's edge state.
    fn prime(&mut self) {
        if let (Ok(monitors), Ok(cursor)) =
            (self.display.monitors(), self.display.cursor_position())
        {
            self.detector.prime(cursor, &monitors);
        }
    }

    /// One poll: read the display, and emit a crossing if one just began.
    /// Returns `false` only when the crossings receiver is gone.
    async fn poll(&mut self) -> bool {
        let monitors = match self.display.monitors() {
            Ok(monitors) => monitors,
            Err(error) => {
                // A transient display query failure skips this tick rather
                // than spinning; a persistent one is the platform's fault.
                tracing::debug!(%error, "edge poll: monitor layout unavailable");
                return true;
            }
        };
        let cursor = match self.display.cursor_position() {
            Ok(cursor) => cursor,
            Err(error) => {
                tracing::debug!(%error, "edge poll: cursor position unavailable");
                return true;
            }
        };
        if self.detector.layout_changed(&monitors) {
            tracing::debug!(
                ?monitors,
                "edge: monitor layout changed; re-priming against the new layout"
            );
        }
        if let Some(position) = self.detector.observe(cursor, &monitors) {
            // The monitors and cursor reads race a display change: each is
            // normalized to the virtual origin at its own call time, so a
            // change landing between them pairs the cursor with the wrong
            // origin — which can read as an edge touch from anywhere on
            // screen. A crossing is trusted only if the layout is unchanged
            // when re-read after the cursor; otherwise it is dropped, and
            // the next tick observes the settled layout and re-primes.
            match self.display.monitors() {
                Ok(after) if after == monitors => {}
                _ => {
                    tracing::debug!("edge: display changed mid-poll; crossing discarded");
                    return true;
                }
            }
            let Some(kind) = self.mode.crossing_kind() else {
                return true; // idle: nothing to emit (unreachable while polling)
            };
            tracing::debug!(
                ?kind,
                position = position.value(),
                cursor_x = cursor.x,
                cursor_y = cursor.y,
                generation = self.generation,
                "edge: crossing detected"
            );
            if self
                .crossings_tx
                .send(EdgeCrossing {
                    kind,
                    position,
                    generation: self.generation,
                })
                .await
                .is_err()
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout};

    use crossover_platform::DisplayInfo;
    use crossover_platform::fakes::FakeDisplay;

    use super::{CrossingKind, EdgeCrossing, EdgeDetector, EdgeMode, REARM_MARGIN, edge_detect};
    use crate::topology::{CursorPoint, LinkSide, MonitorRect, Screen, Topology};

    const HD: Screen = Screen {
        width: 1920,
        height: 1080,
    };

    /// The same 1920×1080 as [`HD`], as a one-monitor layout for the pure
    /// detector (which speaks monitors, not the [`FakeDisplay`]'s screen).
    const HD_MON: [MonitorRect; 1] = [MonitorRect {
        left: 0,
        top: 0,
        width: 1920,
        height: 1080,
    }];

    fn at(x: i32, y: i32) -> CursorPoint {
        CursorPoint { x, y }
    }

    // ---- the pure detector ----

    #[test]
    fn a_crossing_fires_once_on_arrival_not_while_pinned() {
        // Left member links on its right edge (x == 1919).
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Left));
        // Away from the edge: nothing.
        assert!(d.observe(at(960, 540), &HD_MON).is_none());
        // Arrival at the edge: one crossing.
        let crossing = d.observe(at(1919, 300), &HD_MON);
        assert!(crossing.is_some());
        // Still pinned: no repeat.
        assert!(d.observe(at(1919, 300), &HD_MON).is_none());
        assert!(d.observe(at(1919, 305), &HD_MON).is_none());
        // Leaves and returns: fires again.
        assert!(d.observe(at(900, 305), &HD_MON).is_none());
        assert!(d.observe(at(1919, 305), &HD_MON).is_some());
    }

    /// The soak layout: a laptop panel with an external monitor to its
    /// right, so the laptop's right column (x == 1919) is *interior* while
    /// the external is present and becomes the linked edge when it goes.
    const LAPTOP_AND_EXTERNAL: [MonitorRect; 2] = [
        MonitorRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        },
        MonitorRect {
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
        },
    ];

    #[test]
    fn unplugging_the_edge_monitor_does_not_fire_under_a_stationary_cursor() {
        // Left member: the linked edge is the rightmost monitor's right
        // column — the external's while it is plugged in.
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Left));
        // Cursor on the laptop's right column: interior, not the edge.
        assert!(d.observe(at(1919, 540), &LAPTOP_AND_EXTERNAL).is_none());
        // The external is unplugged. The cursor has not moved, but the
        // column under it is suddenly the linked edge — which must read as
        // a moved edge, never as an arrival, or an unplug would transfer
        // control by itself.
        assert!(d.observe(at(1919, 540), &HD_MON).is_none());
        // Pinned there afterwards: still nothing.
        assert!(d.observe(at(1919, 540), &HD_MON).is_none());
        // A genuine arrival on the new layout fires as usual.
        assert!(d.observe(at(900, 540), &HD_MON).is_none());
        assert!(d.observe(at(1919, 540), &HD_MON).is_some());
    }

    #[test]
    fn plugging_a_monitor_in_moves_the_edge_off_a_pinned_cursor() {
        // The plug-in direction never had a firing bug — the edge moves
        // *away* from the cursor — so the observable behavior here matches
        // the pre-fix detector. What this pins is layout adoption: the
        // change must be recognized and stored, or a later refactor could
        // lose the plug-in re-prime while the unplug tests stay green.
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Left));
        // Pinned at the single monitor's linked edge (fires once on arrival).
        assert!(d.observe(at(960, 540), &HD_MON).is_none());
        assert!(d.observe(at(1919, 540), &HD_MON).is_some());
        // The external arrives: the linked edge is now its far column, and
        // the pinned cursor is interior. No crossing, the new layout is
        // adopted, and the state re-primes as away-from-edge...
        assert!(d.layout_changed(&LAPTOP_AND_EXTERNAL));
        assert!(d.observe(at(1919, 540), &LAPTOP_AND_EXTERNAL).is_none());
        assert!(!d.layout_changed(&LAPTOP_AND_EXTERNAL));
        // ...so reaching the *new* edge fires.
        assert!(d.observe(at(3839, 540), &LAPTOP_AND_EXTERNAL).is_some());
    }

    #[test]
    fn priming_suppresses_a_crossing_for_a_cursor_already_at_the_edge() {
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Right)); // links left, x == 0
        d.prime(at(0, 500), &HD_MON);
        // Already at the edge when detection began: no crossing.
        assert!(d.observe(at(0, 500), &HD_MON).is_none());
        // Only after leaving and returning does it fire.
        assert!(d.observe(at(400, 500), &HD_MON).is_none());
        assert!(d.observe(at(0, 500), &HD_MON).is_some());
    }

    /// The hardware bounce (ADR 0009 addendum, 2026-08-19): a transfer
    /// leaves the cursor resting on the linked column, so a one- or
    /// two-pixel wobble there used to read as a fresh arrival and fire a
    /// complete reverse transfer. Only travel clear of the column by more
    /// than [`REARM_MARGIN`] re-arms the trigger.
    #[test]
    fn a_wobble_at_the_edge_does_not_cross_but_real_travel_does() {
        let margin = i32::try_from(REARM_MARGIN).unwrap();
        let column = 1919; // the left member's linked column
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Left));
        // Detection begins with the cursor parked on the column, exactly
        // where an entry placement leaves it.
        d.prime(at(column, 540), &HD_MON);

        // Jitter of a pixel, and of the whole margin, is inert — however
        // often it repeats.
        for _ in 0..3 {
            assert!(d.observe(at(column - 1, 540), &HD_MON).is_none());
            assert!(d.observe(at(column, 540), &HD_MON).is_none());
            assert!(d.observe(at(column - margin, 540), &HD_MON).is_none());
            assert!(d.observe(at(column, 540), &HD_MON).is_none());
        }

        // One pixel past the margin is real travel: the next touch crosses.
        assert!(
            d.observe(at(column - margin - 1, 540), &HD_MON).is_none(),
            "moving clear of the edge is not itself a crossing"
        );
        assert!(d.observe(at(column, 540), &HD_MON).is_some());
        // And only once: the crossing disarms it again.
        assert!(d.observe(at(column, 540), &HD_MON).is_none());
    }

    /// The same hysteresis on the mirrored side, where the linked column is
    /// `x == 0` and clearing it means moving *right*.
    #[test]
    fn the_rearm_margin_applies_on_the_left_linked_edge_too() {
        let margin = i32::try_from(REARM_MARGIN).unwrap();
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Right));
        d.prime(at(0, 500), &HD_MON);
        assert!(d.observe(at(1, 500), &HD_MON).is_none());
        assert!(d.observe(at(0, 500), &HD_MON).is_none());
        assert!(d.observe(at(margin, 500), &HD_MON).is_none());
        assert!(d.observe(at(0, 500), &HD_MON).is_none());
        assert!(d.observe(at(margin + 1, 500), &HD_MON).is_none());
        assert!(d.observe(at(0, 500), &HD_MON).is_some());
    }

    /// A deliberate crossing pays nothing for the hysteresis: a cursor
    /// crossing the screen is clear of the edge the whole way, so the very
    /// first observation that reaches the column fires.
    #[test]
    fn a_deliberate_crossing_still_fires_on_the_first_touch() {
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Left));
        d.prime(at(100, 400), &HD_MON);
        for x in [400, 900, 1400, 1800] {
            assert!(d.observe(at(x, 400), &HD_MON).is_none());
        }
        assert!(d.observe(at(1919, 400), &HD_MON).is_some());
    }

    // ---- the async driver ----
    //
    // The driver polls a real clock, so these tests start the cursor in
    // the middle of the screen (away from any edge) and use short sleeps
    // to let a poll observe an intermediate position before the next move
    // — the same way a real cursor passes through the screen before
    // reaching the edge.

    const MIDDLE: CursorPoint = CursorPoint { x: 960, y: 540 };
    /// A few poll intervals — long enough for the driver to prime on the
    /// current cursor and for one poll to land.
    const SETTLE: Duration = Duration::from_millis(40);

    async fn next_crossing(rx: &mut mpsc::Receiver<EdgeCrossing>) -> EdgeCrossing {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a crossing")
            .expect("crossings channel closed")
    }

    fn rig(
        side: LinkSide,
    ) -> (
        Arc<FakeDisplay>,
        mpsc::Sender<EdgeMode>,
        mpsc::Receiver<EdgeCrossing>,
    ) {
        let display = Arc::new(FakeDisplay::new(HD));
        display.set_cursor(MIDDLE); // away from either edge to start
        let (driver, mode_tx, crossings_rx) = edge_detect(
            Arc::clone(&display) as Arc<dyn DisplayInfo>,
            Topology::new(side),
            Duration::from_millis(5),
        );
        tokio::spawn(driver.run());
        (display, mode_tx, crossings_rx)
    }

    #[tokio::test]
    async fn leaving_mode_emits_a_leave_at_the_linked_edge() {
        let (display, mode_tx, mut crossings) = rig(LinkSide::Left);
        mode_tx.send(EdgeMode::Leaving).await.unwrap();
        sleep(SETTLE).await; // primes on the middle cursor: away from the edge
        display.set_cursor(at(1919, 540)); // right edge, half-way down
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
        assert!((crossing.position.value() - 0.5).abs() < 0.01);
    }

    #[tokio::test]
    async fn returning_mode_emits_a_return() {
        let (display, mode_tx, mut crossings) = rig(LinkSide::Right); // links left
        mode_tx.send(EdgeMode::Returning).await.unwrap();
        sleep(SETTLE).await;
        display.set_cursor(at(0, 270)); // left edge
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Return);
    }

    /// Every crossing is stamped with the number of mode updates the
    /// detector had applied when it fired, so a consumer can tell a
    /// crossing detected under the current control state from one that
    /// queued behind a state change.
    #[tokio::test]
    async fn a_crossing_carries_the_mode_generation_it_was_detected_under() {
        let (display, mode_tx, mut crossings) = rig(LinkSide::Left);
        mode_tx.send(EdgeMode::Leaving).await.unwrap(); // generation 1
        sleep(SETTLE).await;
        mode_tx.send(EdgeMode::Returning).await.unwrap(); // generation 2
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Return);
        assert_eq!(crossing.generation, 2);
    }

    #[tokio::test]
    async fn idle_mode_never_emits_even_at_the_edge() {
        let (display, _mode_tx, mut crossings) = rig(LinkSide::Left);
        // No mode set (defaults to Idle). Park the cursor at the edge.
        display.set_cursor(at(1919, 540));
        let quiet = timeout(Duration::from_millis(200), crossings.recv()).await;
        assert!(quiet.is_err(), "idle driver emitted a crossing");
    }

    #[tokio::test]
    async fn a_cursor_already_at_the_edge_when_watching_begins_does_not_fire() {
        let (display, mode_tx, mut crossings) = rig(LinkSide::Left);
        // Cursor at the edge *before* the mode turns on (primed there).
        display.set_cursor(at(1919, 540));
        mode_tx.send(EdgeMode::Leaving).await.unwrap();
        let quiet = timeout(Duration::from_millis(150), crossings.recv()).await;
        assert!(quiet.is_err(), "fired on a cursor already at the edge");
        // A fresh arrival does fire: leave the edge, let a poll see it,
        // then return.
        display.set_cursor(at(900, 540));
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
    }

    #[tokio::test]
    async fn a_mid_watch_unplug_emits_no_crossing_until_a_fresh_arrival() {
        let (display, mode_tx, mut crossings) = rig(LinkSide::Left);
        display.set_monitors(LAPTOP_AND_EXTERNAL.to_vec());
        mode_tx.send(EdgeMode::Leaving).await.unwrap();
        sleep(SETTLE).await; // primes: middle of the laptop, away from any edge
        // Park the cursor on the laptop's right column — interior while the
        // external monitor is present — and let a poll observe it there.
        display.set_cursor(at(1919, 540));
        sleep(SETTLE).await;
        // Unplug the external: the parked cursor is now on the linked edge.
        display.set_monitors(vec![HD_MON[0]]);
        let quiet = timeout(Duration::from_millis(200), crossings.recv()).await;
        assert!(quiet.is_err(), "an unplug fired a crossing by itself");
        // The user moving away and back to the (new) edge still crosses.
        display.set_cursor(at(900, 540));
        sleep(SETTLE).await;
        display.set_cursor(at(1919, 540));
        let crossing = next_crossing(&mut crossings).await;
        assert_eq!(crossing.kind, CrossingKind::Leave);
    }

    #[tokio::test]
    async fn a_display_failure_is_survived_without_a_crossing() {
        let (display, mode_tx, mut crossings) = rig(LinkSide::Left);
        display.fail_with("no display");
        mode_tx.send(EdgeMode::Leaving).await.unwrap();
        display.set_cursor(at(1919, 540));
        // The driver keeps polling, gets errors, and emits nothing — no
        // panic, no crossing.
        let quiet = timeout(Duration::from_millis(200), crossings.recv()).await;
        assert!(quiet.is_err(), "emitted a crossing despite display failure");
    }
}
