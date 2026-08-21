//! Shared test-only harness: install the real fonts, run one headless egui
//! pass through the real panels, and collect the text it painted — plus a
//! small set of fixture builders. One copy of each, reused by `app.rs`,
//! `render.rs`, `session.rs`, and `state_file.rs`'s own tests, rather than
//! every module repeating its own `include_bytes!` and painter walk.
//!
//! `model.rs`'s tests keep their own, more specific fixture builders
//! (parametrized monitor position/size/scale, always-present peers) —
//! different enough in shape from the simple "one local monitor, an
//! optional peer" documents the other four modules need that sharing them
//! would cost more indirection than it saves.

#![cfg(test)]

use eframe::egui;

use crate::app::install_fonts;
use crate::session::EditorSession;

/// Run one headless pass of the real `render.rs` painters for `session`,
/// through the same panel order `app.rs`'s `ui` uses, and return every
/// string of text the frame painted, in paint order.
///
/// egui needs no window to lay out and rasterize a frame, which is what
/// makes the editor's screens testable on all three CI OSes — none of
/// which has a display server (docs/TESTING.md §2).
pub(crate) fn painted_text(session: &EditorSession) -> String {
    let context = egui::Context::default();
    install_fonts(&context);
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            ..egui::RawInput::default()
        },
        |ui| {
            // The bottom panel must claim its strip before the central
            // panel takes the rest — the one order that leaves both
            // panels any space at all.
            egui::Panel::bottom("status_bar")
                .show(ui, |ui| crate::render::draw_status_bar(ui, session));
            egui::CentralPanel::default().show(ui, |ui| crate::render::draw_central(ui, session));
        },
    );
    // No renderer here to upload the font atlas to, and epaint treats an
    // unapplied texture delta as a defect rather than letting it drop
    // quietly — so a headless pass has to say it meant it.
    output.textures_delta.clear();

    let mut painted = String::new();
    for clipped in output.shapes {
        collect_text(&clipped.shape, &mut painted);
    }
    painted
}

/// A frame is a tree of shapes, and text sits at its leaves.
fn collect_text(shape: &egui::epaint::Shape, into: &mut String) {
    match shape {
        egui::epaint::Shape::Text(text) => {
            into.push_str(text.galley.text());
            into.push('\n');
        }
        egui::epaint::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, into);
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
