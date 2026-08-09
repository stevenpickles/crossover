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
//! cursor reaches the linked edge after being away — so a cursor pinned
//! at the screen edge fires the transfer once, not on every poll. The
//! async [`EdgeDetectDriver`] polls the display, applies the mode, and
//! emits the crossings.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crossover_platform::DisplayInfo;

use crate::topology::{CursorPoint, EdgeFraction, Screen, Topology};

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
}

/// The pure crossing detector: rising-edge detection against the linked
/// edge. Holds only whether the cursor was at the edge last time, so a
/// crossing fires once on arrival, never repeatedly while the cursor sits
/// pinned there. No I/O.
#[derive(Debug)]
pub struct EdgeDetector {
    topology: Topology,
    /// Whether the cursor was against the linked edge at the last
    /// observation.
    at_edge: bool,
}

impl EdgeDetector {
    /// A detector for a machine of this `topology`, starting away from the
    /// edge.
    #[must_use]
    pub fn new(topology: Topology) -> Self {
        Self {
            topology,
            at_edge: false,
        }
    }

    /// Set the at-edge state from a cursor **without** emitting a
    /// crossing. Used when detection (re)starts, so a cursor already
    /// sitting at the edge does not fire an immediate, unintended
    /// transfer — only a fresh arrival does.
    pub fn prime(&mut self, cursor: CursorPoint, screen: Screen) {
        self.at_edge = self.topology.leaving(cursor, screen).is_some();
    }

    /// Observe a cursor position. Returns the crossing fraction only on
    /// the rising edge — the cursor reaching the linked edge after being
    /// away from it — and `None` otherwise (away, or still pinned).
    #[must_use]
    pub fn observe(&mut self, cursor: CursorPoint, screen: Screen) -> Option<EdgeFraction> {
        let touching = self.topology.leaving(cursor, screen);
        let rising = touching.is_some() && !self.at_edge;
        self.at_edge = touching.is_some();
        if rising { touching } else { None }
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
        if let (Ok(screen), Ok(cursor)) = (
            self.display.primary_screen(),
            self.display.cursor_position(),
        ) {
            self.detector.prime(cursor, screen);
        }
    }

    /// One poll: read the display, and emit a crossing if one just began.
    /// Returns `false` only when the crossings receiver is gone.
    async fn poll(&mut self) -> bool {
        let screen = match self.display.primary_screen() {
            Ok(screen) => screen,
            Err(error) => {
                // A transient display query failure skips this tick rather
                // than spinning; a persistent one is the platform's fault.
                tracing::debug!(%error, "edge poll: primary display unavailable");
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
        if let Some(position) = self.detector.observe(cursor, screen) {
            let Some(kind) = self.mode.crossing_kind() else {
                return true; // idle: nothing to emit (unreachable while polling)
            };
            if self
                .crossings_tx
                .send(EdgeCrossing { kind, position })
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

    use super::{CrossingKind, EdgeCrossing, EdgeDetector, EdgeMode, edge_detect};
    use crate::topology::{CursorPoint, LinkSide, Screen, Topology};

    const HD: Screen = Screen {
        width: 1920,
        height: 1080,
    };

    fn at(x: i32, y: i32) -> CursorPoint {
        CursorPoint { x, y }
    }

    // ---- the pure detector ----

    #[test]
    fn a_crossing_fires_once_on_arrival_not_while_pinned() {
        // Left member links on its right edge (x == 1919).
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Left));
        // Away from the edge: nothing.
        assert!(d.observe(at(960, 540), HD).is_none());
        // Arrival at the edge: one crossing.
        let crossing = d.observe(at(1919, 300), HD);
        assert!(crossing.is_some());
        // Still pinned: no repeat.
        assert!(d.observe(at(1919, 300), HD).is_none());
        assert!(d.observe(at(1919, 305), HD).is_none());
        // Leaves and returns: fires again.
        assert!(d.observe(at(900, 305), HD).is_none());
        assert!(d.observe(at(1919, 305), HD).is_some());
    }

    #[test]
    fn priming_suppresses_a_crossing_for_a_cursor_already_at_the_edge() {
        let mut d = EdgeDetector::new(Topology::new(LinkSide::Right)); // links left, x == 0
        d.prime(at(0, 500), HD);
        // Already at the edge when detection began: no crossing.
        assert!(d.observe(at(0, 500), HD).is_none());
        // Only after leaving and returning does it fire.
        assert!(d.observe(at(400, 500), HD).is_none());
        assert!(d.observe(at(0, 500), HD).is_some());
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
