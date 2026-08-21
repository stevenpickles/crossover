//! Painting the editor's screens (ADR 0018/0019).
//!
//! Every [`EditorSession`] variant has exactly one painter here, and every
//! one of them runs in a headless `egui::Context` — no window, no OpenGL —
//! which is the property ADR 0019 chose egui for and `test_support.rs`'s
//! harness relies on directly.
//!
//! Colors are never hardcoded: the two machines' hues come from rotating
//! the current [`egui::Style`]'s own selection color, so the canvas reads
//! correctly in both light and dark visuals without a light/dark branch of
//! its own.

use eframe::egui::{self, Color32};

use crate::model::{DrawnMonitor, MachineGroup, Model};
use crate::session::{EditorSession, Freshness};
use crate::viewport::Viewport;

/// Screen-space margin around the drawn arrangement, so a monitor's stroke
/// and its label are never flush against the window edge.
const CANVAS_PADDING: f32 = 32.0;

/// Below this screen-space width or height, a monitor's label would not sit
/// inside its rectangle legibly, so it is skipped rather than drawn
/// overlapping the stroke or its neighbours.
const MIN_LABEL_DIMENSION: f32 = 46.0;

/// Half of the 1px seam every pair of monitors is drawn with — applied to
/// *each* monitor's rectangle, so two that abut exactly in layout space
/// (touching, per ADR 0018) show a visible hairline rather than one
/// unbroken shape.
const SEAM_HALF_WIDTH: f32 = 0.5;

/// Fill alpha for a monitor drawn *unplaced* — live, but not named by an
/// otherwise-authoritative saved arrangement — well below the ordinary
/// fill's, so it reads as provisional against its placed neighbours.
const UNPLACED_FILL_ALPHA: f32 = 0.10;
/// Stroke alpha for the same case — dimmer than the ordinary stroke's, but
/// still clearly outlined rather than invisible.
const UNPLACED_STROKE_ALPHA: f32 = 0.55;

/// Paint whatever `session` currently is into the central area.
pub fn draw_central(ui: &mut egui::Ui, session: &EditorSession) {
    match session {
        EditorSession::Loading => draw_loading(ui),
        EditorSession::NoWorker { reason } => draw_no_worker(ui, reason.as_deref()),
        EditorSession::WaitingForPeer { model, .. } => draw_waiting_for_peer(ui, model),
        EditorSession::Editing { model, .. } => draw_scene(ui, model),
    }
}

/// Paint the bottom status line: freshness and peer state, in one row.
pub fn draw_status_bar(ui: &mut egui::Ui, session: &EditorSession) {
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(status_line(session));
    });
}

fn status_line(session: &EditorSession) -> String {
    match session {
        EditorSession::Loading => "Reading the worker's last report…".to_owned(),
        EditorSession::NoWorker { reason: None } => "Worker: not running".to_owned(),
        EditorSession::NoWorker {
            reason: Some(reason),
        } => format!("Worker: state file unreadable — {reason}"),
        EditorSession::WaitingForPeer { staleness, .. } => {
            format!(
                "Worker: {} · Peer: none seen yet",
                freshness_phrase(*staleness)
            )
        }
        EditorSession::Editing {
            model,
            staleness,
            peer_connected,
        } => {
            let peer_name = model
                .peer
                .as_ref()
                .map_or("the peer", |group| group.name.as_str());
            let peer_phrase = if *peer_connected {
                "connected".to_owned()
            } else {
                format!("disconnected — showing {peer_name}'s last-known screens")
            };
            format!(
                "Worker: {} · Peer: {peer_phrase}",
                freshness_phrase(*staleness)
            )
        }
    }
}

fn freshness_phrase(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Fresh => "running",
        Freshness::Stale => "not responding — showing its last report",
    }
}

/// Before the first state-file read has completed.
fn draw_loading(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 3.0);
        ui.heading("Reading the worker's last report…");
    });
}

/// The empty state: what the editor shows when it has no live facts to
/// draw. ADR 0018 makes this a first-class screen rather than a blank
/// canvas. `reason` is `None` when the state file is simply absent (the
/// original, unchanged empty-state text, so a user who has already seen it
/// keeps seeing the same words) and `Some` when a file is there but could
/// not be used — a different message, because "start the worker" is the
/// wrong instruction for "the editor and the worker disagree about the
/// state-file version".
fn draw_no_worker(ui: &mut egui::Ui, reason: Option<&str>) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 3.0);
        match reason {
            None => draw_worker_never_run(ui),
            Some(reason) => draw_state_file_unusable(ui, reason),
        }
    });
}

fn draw_worker_never_run(ui: &mut egui::Ui) {
    ui.heading("No displays to arrange yet");
    ui.add_space(8.0);
    ui.label("The Crossover worker is not running, so it has reported no displays.");
    ui.add_space(4.0);
    // These two command names are `crossover.exe`'s, spelled out in a
    // different binary: renaming `run` or `service install` in the clap
    // definition (apps/crossover/src/main.rs, which carries the matching
    // note) leaves this window telling the user to type something that no
    // longer exists. Sharing constants would mean a crate between the two
    // for four words, which ADR 0019's dependency rule makes a poor trade
    // — so the coupling is held by these comments and by the test below,
    // which greps for both strings.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label("Start it with ");
        ui.label(egui::RichText::new("crossover run").monospace());
        ui.label(", or install the background service with ");
        ui.label(egui::RichText::new("crossover service install").monospace());
        ui.label(", and this window fills in.");
    });
}

fn draw_state_file_unusable(ui: &mut egui::Ui, reason: &str) {
    ui.heading("The worker's last report could not be used");
    ui.add_space(8.0);
    ui.label(format!("{reason}."));
    ui.add_space(4.0);
    ui.label(
        "If this keeps happening after the worker restarts, the editor and the worker \
         are probably different versions — install matching builds of both.",
    );
}

/// A worker is reporting, but no peer has ever connected: this machine's
/// own monitors are drawn, with a banner in place of a peer group.
fn draw_waiting_for_peer(ui: &mut egui::Ui, model: &Model) {
    ui.add_space(6.0);
    ui.vertical_centered(|ui| {
        ui.heading("Waiting for a peer to connect");
        ui.label(
            "Your own displays are shown below. Once a peer connects, its screens appear here too.",
        );
    });
    ui.add_space(6.0);
    ui.separator();
    draw_scene(ui, model);
}

/// The canvas: both machines' monitors, to scale, filling the rest of the
/// available area.
fn draw_scene(ui: &mut egui::Ui, model: &Model) {
    if let Some(reason) = &model.rejected_layout {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.small(format!(
                "The saved arrangement could not be used ({reason}) — this is a starting guess instead."
            ));
        });
    } else if model.seeded {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.small(
                "This is a starting guess, not a saved arrangement — dragging and saving arrive in a later release.",
            );
        });
    }

    let canvas_size = ui.available_size();
    let (response, painter) = ui.allocate_painter(canvas_size, egui::Sense::hover());
    let canvas_rect = response.rect;

    let bounds = model.bounds();
    let viewport = Viewport::fit(
        bounds,
        (canvas_rect.width(), canvas_rect.height()),
        CANVAS_PADDING,
    );

    let local_hue = base_hsva(ui.visuals());
    let peer_hue = rotated(local_hue);
    let authoritative_scene = !model.seeded;

    paint_group(
        &painter,
        canvas_rect.min,
        &viewport,
        &model.local,
        local_hue,
        authoritative_scene,
        ui,
    );
    if let Some(peer) = &model.peer {
        paint_group(
            &painter,
            canvas_rect.min,
            &viewport,
            peer,
            peer_hue,
            authoritative_scene,
            ui,
        );
    }
}

/// The hue this session's local machine draws in — derived from the
/// current style's selection color rather than a fixed constant, so light
/// and dark visuals each get a hue that already belongs to their palette.
fn base_hsva(visuals: &egui::Visuals) -> egui::ecolor::Hsva {
    egui::ecolor::Hsva::from(visuals.selection.bg_fill)
}

/// The peer's hue: the local hue rotated a half turn round the color
/// wheel, so the two machines are always visually distinct whatever the
/// selection color happens to be.
fn rotated(hsva: egui::ecolor::Hsva) -> egui::ecolor::Hsva {
    egui::ecolor::Hsva {
        h: (hsva.h + 0.5) % 1.0,
        ..hsva
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_group(
    painter: &egui::Painter,
    canvas_origin: egui::Pos2,
    viewport: &Viewport,
    group: &MachineGroup,
    hue: egui::ecolor::Hsva,
    authoritative_scene: bool,
    ui: &egui::Ui,
) {
    let stroke_color: Color32 = hue.into();
    let fill_color: Color32 = egui::ecolor::Hsva { a: 0.28, ..hue }.into();
    let ghost_fill: Color32 = egui::ecolor::Hsva {
        a: UNPLACED_FILL_ALPHA,
        ..hue
    }
    .into();
    let ghost_stroke: Color32 = egui::ecolor::Hsva {
        a: UNPLACED_STROKE_ALPHA,
        ..hue
    }
    .into();
    let text_color = ui.visuals().text_color();

    if let Some(bounds) = group.bounds() {
        let label_anchor = to_absolute(viewport, canvas_origin, (bounds.min_x, bounds.min_y));
        painter.text(
            label_anchor - egui::vec2(0.0, 2.0),
            egui::Align2::LEFT_BOTTOM,
            &group.name,
            egui::FontId::proportional(14.0),
            text_color,
        );
    }

    for monitor in &group.monitors {
        let top_left = to_absolute(
            viewport,
            canvas_origin,
            (f64::from(monitor.rect.x), f64::from(monitor.rect.y)),
        );
        // `right`/`bottom` return `i64` for the overflow-safe derivation
        // arithmetic ADR 0018 specifies; every rect this crate draws stays
        // far inside the `2^24` layout-coordinate ceiling, well short of
        // where widening to `f64` could lose precision.
        #[allow(clippy::cast_precision_loss)]
        let bottom_right = to_absolute(
            viewport,
            canvas_origin,
            (monitor.rect.right() as f64, monitor.rect.bottom() as f64),
        );
        let rect = egui::Rect::from_min_max(top_left, bottom_right).shrink(SEAM_HALF_WIDTH);

        // A monitor drawn unplaced — live, but not named by an otherwise-
        // authoritative saved arrangement — gets the ghost treatment; a
        // wholly seeded scene draws every monitor the ordinary way; its
        // banner above already says the whole thing is provisional.
        let unplaced = authoritative_scene && !monitor.authoritative;
        let (fill, stroke) = if unplaced {
            (ghost_fill, egui::Stroke::new(1.0, ghost_stroke))
        } else {
            (fill_color, egui::Stroke::new(1.5, stroke_color))
        };
        painter.rect(rect, 2.0, fill, stroke, egui::StrokeKind::Inside);

        if rect.width() >= MIN_LABEL_DIMENSION && rect.height() >= MIN_LABEL_DIMENSION {
            let mut label = monitor_label(monitor);
            if unplaced {
                label.push_str("\n(unplaced)");
            }
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::monospace(11.0),
                text_color,
            );
        }
    }
}

fn monitor_label(monitor: &DrawnMonitor) -> String {
    match monitor.native_size {
        Some((width, height)) => {
            format!("{} · {}\n{width}×{height}", monitor.ordinal, monitor.id)
        }
        None => format!("{} · {}", monitor.ordinal, monitor.id),
    }
}

fn to_absolute(viewport: &Viewport, origin: egui::Pos2, point: (f64, f64)) -> egui::Pos2 {
    let (x, y) = viewport.to_screen(point);
    egui::pos2(origin.x + x, origin.y + y)
}

#[cfg(test)]
mod tests {
    use crate::model::Model;
    use crate::session::{EditorSession, Freshness};
    use crate::test_support::{document, live_monitor, painted_text, peer_state};

    #[test]
    fn loading_paints_without_panicking_and_says_so() {
        let painted = painted_text(&EditorSession::Loading);
        assert!(
            painted.contains("Reading the worker's last report"),
            "{painted}"
        );
    }

    #[test]
    fn no_worker_paints_the_original_empty_state_text_when_absent() {
        let painted = painted_text(&EditorSession::NoWorker { reason: None });
        assert!(
            painted.contains("The Crossover worker is not running"),
            "{painted}"
        );
        assert!(painted.contains("crossover run"), "{painted}");
        assert!(painted.contains("crossover service install"), "{painted}");
        assert!(painted.contains("Worker: not running"), "{painted}");
    }

    /// Issue 3: an unreadable-but-present state file names why, both on
    /// the empty-state screen and in the status bar — never the "start the
    /// worker" instruction, which would be wrong advice for this cause.
    #[test]
    fn no_worker_names_the_reason_when_the_file_is_unreadable() {
        let session = EditorSession::NoWorker {
            reason: Some(
                "topology state version 9 is not supported (this build reads 1)".to_owned(),
            ),
        };
        let painted = painted_text(&session);
        assert!(painted.contains("could not be used"), "{painted}");
        assert!(painted.contains("version 9"), "{painted}");
        assert!(painted.contains("different versions"), "{painted}");
        assert!(!painted.contains("crossover run"), "{painted}");
        assert!(painted.contains("state file unreadable"), "{painted}");
    }

    #[test]
    fn waiting_for_peer_paints_the_banner_and_the_local_machine() {
        let model = Model::from_state(&document(None, 0));
        let session = EditorSession::WaitingForPeer {
            model,
            staleness: Freshness::Fresh,
        };
        let painted = painted_text(&session);
        assert!(
            painted.contains("Waiting for a peer to connect"),
            "{painted}"
        );
        assert!(painted.contains("workstation-left"), "{painted}");
        assert!(painted.contains("Peer: none seen yet"), "{painted}");
    }

    #[test]
    fn editing_paints_both_machines_and_the_peer_state() {
        let model = Model::from_state(&document(Some(peer_state(true)), 0));
        let session = EditorSession::Editing {
            model,
            staleness: Freshness::Fresh,
            peer_connected: true,
        };
        let painted = painted_text(&session);
        assert!(painted.contains("workstation-left"), "{painted}");
        assert!(painted.contains("laptop"), "{painted}");
        assert!(painted.contains("1920"), "{painted}"); // native resolution
        assert!(painted.contains("Peer: connected"), "{painted}");
    }

    #[test]
    fn a_disconnected_but_remembered_peer_says_so_in_the_status_bar() {
        let model = Model::from_state(&document(Some(peer_state(false)), 0));
        let session = EditorSession::Editing {
            model,
            staleness: Freshness::Stale,
            peer_connected: false,
        };
        let painted = painted_text(&session);
        assert!(painted.contains("disconnected"), "{painted}");
        assert!(painted.contains("laptop"), "{painted}");
        assert!(painted.contains("not responding"), "{painted}");
        // The peer's last-known screens are still drawn.
        assert!(painted.contains("laptop"), "{painted}");
    }

    /// Issue 5: an unplaced live monitor is painted with a visible cue,
    /// not silently dropped from an otherwise-authoritative scene.
    #[test]
    fn an_unplaced_monitor_paints_its_cue() {
        use crossover_topology::{
            DevicePair, Layout, LayoutRect, LayoutState, MachineState, MonitorId, PlacedMonitor,
            TOPOLOGY_STATE_VERSION, TopologyState,
        };

        let local_device = crossover_topology::DeviceId::from_bytes([0x11; 16]);
        let peer_device = crossover_topology::DeviceId::from_bytes([0x22; 16]);
        let pair = DevicePair::new(local_device, peer_device).unwrap();
        let placed = vec![
            PlacedMonitor {
                device: local_device,
                id: MonitorId::new(r"\\.\DISPLAY1").unwrap(),
                rect: LayoutRect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            PlacedMonitor {
                device: peer_device,
                id: MonitorId::new(r"\\.\DISPLAY1").unwrap(),
                rect: LayoutRect {
                    x: 1920,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
        ];
        let layout = Layout::new(1, local_device, placed, &pair).unwrap();

        let state = TopologyState {
            version: TOPOLOGY_STATE_VERSION,
            written_at: 0,
            local: MachineState {
                device: local_device,
                name: "workstation-left".to_owned(),
                monitors: vec![live_monitor(r"\\.\DISPLAY1"), live_monitor(r"\\.\DISPLAY2")],
            },
            peer: Some(crossover_topology::PeerState {
                device: peer_device,
                name: "laptop".to_owned(),
                connected: true,
                last_seen: 0,
                monitors: vec![live_monitor(r"\\.\DISPLAY1")],
            }),
            layout: Some(LayoutState::from_layout(&layout)),
        };

        let model = Model::from_state(&state);
        assert!(!model.seeded);
        let session = EditorSession::Editing {
            model,
            staleness: Freshness::Fresh,
            peer_connected: true,
        };
        let painted = painted_text(&session);
        assert!(painted.contains("(unplaced)"), "{painted}");
    }

    /// Issue 2: a rejected saved layout shows a distinct, visible note
    /// naming why, not the ordinary "no arrangement yet" seed banner.
    #[test]
    fn a_rejected_layout_paints_its_own_note() {
        use crossover_topology::{DevicePair, Layout, LayoutRect, LayoutState, PlacedMonitor};

        let local_device = crossover_topology::DeviceId::from_bytes([0x11; 16]);
        let stranger = crossover_topology::DeviceId::from_bytes([0x99; 16]);
        let pair = DevicePair::new(local_device, stranger).unwrap();
        let placed = vec![
            PlacedMonitor {
                device: local_device,
                id: crossover_topology::MonitorId::new(r"\\.\DISPLAY1").unwrap(),
                rect: LayoutRect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            PlacedMonitor {
                device: stranger,
                id: crossover_topology::MonitorId::new(r"\\.\DISPLAY1").unwrap(),
                rect: LayoutRect {
                    x: 1920,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
        ];
        let layout = Layout::new(1, local_device, placed, &pair).unwrap();
        let mut doc = document(Some(peer_state(true)), 0);
        doc.local.device = local_device;
        doc.layout = Some(LayoutState::from_layout(&layout));

        let model = Model::from_state(&doc);
        assert!(model.seeded);
        assert!(model.rejected_layout.is_some());
        let session = EditorSession::Editing {
            model,
            staleness: Freshness::Fresh,
            peer_connected: true,
        };
        let painted = painted_text(&session);
        assert!(painted.contains("could not be used"), "{painted}");
        assert!(painted.contains("starting guess instead"), "{painted}");
    }
}
