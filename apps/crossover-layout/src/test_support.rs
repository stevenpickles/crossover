//! Shared test-only harness: install the real fonts, run the real headless
//! egui pass and collect the text it painted, a private directory that
//! cleans up after itself, the fixture documents, and the two-line drag
//! choreography every test that needs an *edited* scene would otherwise
//! write for itself. One copy of each, reused by every module's tests
//! rather than repeated per module.
//!
//! `model.rs`'s tests keep their own document builder (parametrized monitor
//! position/size/scale, always-present peers) — different enough in shape
//! from the simple "one local monitor, an optional peer" documents the
//! other modules need that sharing it would cost more indirection than it
//! saves. It takes the devices, the viewport, the keys, and the drag from
//! here all the same.

#![cfg(test)]

use std::path::PathBuf;

use eframe::egui;

use crate::app::install_fonts;
use crate::model::Model;
use crate::render::Chrome;
use crate::session::EditorSession;
use crate::viewport::Viewport;

/// What one headless frame drew.
pub(crate) struct Painted {
    /// Every string of text the frame painted, in paint order.
    pub(crate) text: String,
    /// How many line segments it painted — the snap guides, which have no
    /// text of their own to assert on.
    pub(crate) line_segments: usize,
}

impl Painted {
    /// Whether the painted text contains `needle`.
    pub(crate) fn says(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }
}

/// Run one headless pass of the real `render.rs` frame for `session`,
/// through `app.rs`'s own entry point, and return what it drew.
///
/// egui needs no window to lay out and rasterize a frame, which is what
/// makes the editor's screens testable on all three CI OSes — none of
/// which has a display server (docs/TESTING.md §2). The session is cloned
/// because the real frame edits the model as it paints it (the canvas is
/// where dragging happens); a caller that wants the edit back drives
/// `Model`'s own operations directly, which are pure.
///
/// One frame, which is what the real window paints for a steady scene. A
/// screen that needs more than one is [`paint_settled`]'s business, and
/// says so at its call site.
pub(crate) fn paint(session: &EditorSession, chrome: Chrome<'_>) -> Painted {
    paint_frames(session, chrome, 1)
}

/// [`paint`], but reporting the **second** frame.
///
/// egui shows a freshly created `Area` — which is what a modal is —
/// invisibly on its first frame, to size it without the reader seeing it
/// jump; it asks for a repaint and appears on the next. So a test that
/// asserts on a modal needs two frames, and a one-frame harness would be
/// structurally unable to see one — reporting its absence as a pass. Every
/// other screen is steady from the first frame and uses [`paint`].
pub(crate) fn paint_settled(session: &EditorSession, chrome: Chrome<'_>) -> Painted {
    paint_frames(session, chrome, 2)
}

/// Run `frames` headless passes and report what the last one drew.
fn paint_frames(session: &EditorSession, chrome: Chrome<'_>, frames: usize) -> Painted {
    assert!(frames >= 1, "a pass needs at least one frame");
    let context = egui::Context::default();
    install_fonts(&context);
    let mut session = session.clone();
    let mut output = None;
    for _ in 0..frames {
        let mut frame = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 640.0),
                )),
                ..egui::RawInput::default()
            },
            |ui| {
                let _ = crate::render::draw_frame(ui, &mut session, chrome);
            },
        );
        // No renderer here to upload the font atlas to, and epaint treats
        // an unapplied texture delta as a defect rather than letting it
        // drop quietly — so a headless pass has to say it meant it.
        frame.textures_delta.clear();
        output = Some(frame);
    }
    let output = output.expect("at least one frame was run");

    let mut painted = Painted {
        text: String::new(),
        line_segments: 0,
    };
    for clipped in output.shapes {
        collect(&clipped.shape, &mut painted);
    }
    painted
}

/// [`paint`] with no save status and no dialog — the ordinary window.
pub(crate) fn painted_text(session: &EditorSession) -> String {
    paint(session, Chrome::default()).text
}

/// A frame is a tree of shapes, and text sits at its leaves.
fn collect(shape: &egui::epaint::Shape, into: &mut Painted) {
    match shape {
        egui::epaint::Shape::Text(text) => {
            into.text.push_str(text.galley.text());
            into.text.push('\n');
        }
        egui::epaint::Shape::LineSegment { .. } => into.line_segments += 1,
        egui::epaint::Shape::Vec(shapes) => {
            for shape in shapes {
                collect(shape, into);
            }
        }
        _ => {}
    }
}

/// This session's local device — fixed across every shared fixture so a
/// test that builds a [`crossover_topology::LayoutState`] separately can
/// still name the same machine.
pub(crate) const LOCAL_DEVICE: crossover_topology::DeviceId =
    crossover_topology::DeviceId::from_bytes([0x11; 16]);
/// This session's peer device, likewise fixed.
pub(crate) const PEER_DEVICE: crossover_topology::DeviceId =
    crossover_topology::DeviceId::from_bytes([0x22; 16]);

/// One ordinary 1920×1080, 100%-scale live monitor.
pub(crate) fn live_monitor(id: &str) -> crossover_topology::LiveMonitor {
    crossover_topology::LiveMonitor {
        id: crossover_topology::MonitorId::new(id).unwrap(),
        rect: crossover_topology::LayoutRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
        scale_percent: 100,
    }
}

/// The peer, as [`crossover_topology::PeerState`] — one ordinary monitor,
/// `connected` as given.
pub(crate) fn peer_state(connected: bool) -> crossover_topology::PeerState {
    crossover_topology::PeerState {
        device: PEER_DEVICE,
        name: "laptop".to_owned(),
        connected,
        last_seen: 0,
        monitors: vec![live_monitor(r"\\.\DISPLAY1")],
    }
}

/// A whole [`crossover_topology::TopologyState`]: one local monitor, the
/// given (optional) peer, no saved layout, at the given heartbeat.
pub(crate) fn document(
    peer: Option<crossover_topology::PeerState>,
    written_at: u64,
) -> crossover_topology::TopologyState {
    crossover_topology::TopologyState {
        version: crossover_topology::TOPOLOGY_STATE_VERSION,
        written_at,
        local: crossover_topology::MachineState {
            device: LOCAL_DEVICE,
            name: "workstation-left".to_owned(),
            monitors: vec![live_monitor(r"\\.\DISPLAY1")],
        },
        peer,
        layout: None,
    }
}

/// The revision the [`arranged_document`] fixture's saved layout carries —
/// deliberately not 0 or 1, so a test that asserts a *new* revision cannot
/// pass by accident.
pub(crate) const ARRANGED_REVISION: u64 = 4;

/// A document with a peer **and** a saved arrangement: the two machines
/// side by side, drawn (per the origin) at the other desk. The starting
/// point for anything that drags, validates, or saves.
pub(crate) fn arranged_document(written_at: u64) -> crossover_topology::TopologyState {
    let pair = crossover_topology::DevicePair::new(LOCAL_DEVICE, PEER_DEVICE).unwrap();
    let layout = crossover_topology::Layout::new(
        ARRANGED_REVISION,
        PEER_DEVICE,
        vec![
            placed_monitor(LOCAL_DEVICE, r"\\.\DISPLAY1", 0),
            placed_monitor(PEER_DEVICE, r"\\.\DISPLAY1", 1920),
        ],
        &pair,
    )
    .expect("the fixture arrangement is valid");

    let mut state = document(Some(peer_state(true)), written_at);
    state.layout = Some(crossover_topology::LayoutState::from_layout(&layout));
    state
}

/// One 1920×1080 monitor placed at `x` on the shared axis.
pub(crate) fn placed_monitor(
    device: crossover_topology::DeviceId,
    id: &str,
    x: i32,
) -> crossover_topology::PlacedMonitor {
    crossover_topology::PlacedMonitor {
        device,
        id: crossover_topology::MonitorId::new(id).unwrap(),
        rect: crossover_topology::LayoutRect {
            x,
            y: 0,
            width: 1920,
            height: 1080,
        },
    }
}

/// Which monitor of which machine — the thing a drag takes hold of.
pub(crate) fn monitor_key(
    device: crossover_topology::DeviceId,
    id: &str,
) -> crossover_topology::MonitorKey {
    crossover_topology::MonitorKey {
        device,
        id: crossover_topology::MonitorId::new(id).unwrap(),
    }
}

/// A viewport at 1:1 with no offset, so a layout unit is a screen pixel and
/// the snap threshold is `SNAP_SCREEN_PX` layout units — the arithmetic
/// every drag test reasons about directly.
pub(crate) fn unit_viewport() -> Viewport {
    Viewport {
        scale: 1.0,
        offset: (0.0, 0.0),
    }
}

/// Grab `device`'s machine by monitor `id`, move the pointer by `delta`,
/// and let go — the whole gesture, since `Model` only commits on the drop.
pub(crate) fn drag_by(
    model: &mut Model,
    device: crossover_topology::DeviceId,
    id: &str,
    delta: (f64, f64),
) {
    let grab = (10.0, 10.0);
    model.begin_drag(&monitor_key(device, id), grab, unit_viewport());
    model.drag_to((grab.0 + delta.0, grab.1 + delta.1));
    model.end_drag();
}

/// Drag the local machine far enough down to leave the scene **dirty** —
/// the starting condition for anything about saving, closing, or
/// reconciling an unsaved edit.
///
/// Downward and large on purpose: the two machines end up apart, which is a
/// legal (if warned-about) arrangement, so nothing built on this is blocked
/// from saving by an overlap it did not ask for.
pub(crate) fn drag_until_dirty(model: &mut Model) {
    drag_by(model, LOCAL_DEVICE, r"\\.\DISPLAY1", (0.0, 4_000.0));
    assert!(
        model.is_dirty(),
        "the choreography must actually leave unsaved work"
    );
}

/// A private directory removed on drop — the house substitute for a
/// `tempfile` dependency (`crossover-topology`'s own test `Sandbox`, and
/// `crossover-platform-windows`'s before it). One copy for this crate,
/// shared by every test here that needs a real file.
pub(crate) struct Sandbox(PathBuf);

impl Sandbox {
    /// A fresh directory, named for `label` and unique per process and per
    /// call, so concurrently running tests cannot collide.
    pub(crate) fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crossover-layout-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("sandbox");
        Self(dir)
    }

    /// A path to `leaf` inside this directory. The file need not exist.
    pub(crate) fn path(&self, leaf: &str) -> PathBuf {
        self.0.join(leaf)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
