//! Painting the editor's screens (ADR 0018/0019).
//!
//! Every [`EditorSession`] variant has exactly one painter here, and every
//! one of them runs in a headless `egui::Context` — no window, no OpenGL —
//! which is the property ADR 0019 chose egui for and `test_support.rs`'s
//! harness relies on directly.
//!
//! Colors are never hardcoded: the two machines' hues come from rotating
//! the current [`egui::Style`]'s own selection color, and a diagnostic
//! borrows the style's own error and warning colors, so the canvas reads
//! correctly in both light and dark visuals without a light/dark branch of
//! its own.
//!
//! # The canvas is where dragging happens
//!
//! ADR 0019 chose an immediate-mode toolkit precisely so that "hit-testing,
//! dragging, and snapping are arithmetic inside the code that paints the
//! frame" rather than a widget with its own state machine to keep in step
//! with the model. [`draw_central`] is that code: it hit-tests the pointer
//! against the scene, hands the drag to [`Model`]'s pure operations, and
//! paints the result — including the guides the snap produced and the red
//! outlines validation asked for.

use eframe::egui::{self, Color32};

use crate::inspector::{Inspector, MonitorFacts, Request};
use crate::model::{Diagnostics, MachineGroup, Model, Severity};
use crate::session::{EditorSession, Freshness};
use crate::snap::{Axis, Guide};
use crate::viewport::Viewport;
use crossover_topology::MonitorKey;

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

/// Stroke width for a monitor a blocking diagnostic names — heavier than
/// the ordinary outline, so an offender is obvious without reading the
/// status bar.
const OFFENDER_STROKE_WIDTH: f32 = 2.5;

/// How far outside a selected monitor's rectangle its selection ring is
/// drawn — outside rather than on the stroke, so selecting a screen never
/// changes the size or the colour of the shape whose *size* the inspector
/// is about to talk about.
const SELECTION_RING_GAP: f32 = 2.5;

/// The size inspector's starting width, in points. Wide enough for the two
/// millimetre fields side by side under a caption, and resizable.
const INSPECTOR_WIDTH: f32 = 232.0;

/// Everything the frame draws that is not the scene or the session: the
/// last save's outcome, and whether the close-confirmation is up.
#[derive(Debug, Clone, Copy, Default)]
pub struct Chrome<'a> {
    /// The last save attempt's outcome, if there has been one: the line to
    /// show, and whether it is a failure (painted in the style's error
    /// color) rather than a success.
    ///
    /// One field rather than a line and a flag beside it, because "is this
    /// an error" is only answerable when there is a line to ask it about —
    /// a separate boolean has a meaningless state (an error with nothing to
    /// say) that nothing prevents.
    pub status: Option<(&'a str, bool)>,
    /// Whether the "you have unsaved changes" dialog is showing.
    pub confirming_close: bool,
}

/// What the user asked for during a frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    /// The Save button was clicked.
    pub save_requested: bool,
    /// A button in the close-confirmation was clicked.
    pub close_choice: Option<CloseChoice>,
}

/// The three answers to "you have unsaved changes".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseChoice {
    /// Write the arrangement, then close.
    Save,
    /// Close without writing.
    Discard,
    /// Stay open.
    Cancel,
}

/// Paint one whole frame — toolbar, status bar, canvas, and the
/// close-confirmation when it is up — and report what was clicked.
///
/// One entry point rather than three, so `app.rs` and the headless test
/// harness paint the *same* frame in the same panel order: a screen that
/// only the real window can produce is a screen the ordinary `cargo test`
/// gate cannot assert on, which is exactly the hole ADR 0019 chose egui to
/// avoid.
pub fn draw_frame(
    ui: &mut egui::Ui,
    session: &mut EditorSession,
    inspector: &mut Inspector,
    chrome: Chrome<'_>,
) -> FrameOutcome {
    let mut outcome = FrameOutcome::default();
    // The fields describe whatever the canvas has selected, before anything
    // is drawn from them (`inspector.rs`).
    inspector.sync(session.model());
    // The edge panels claim their strips before the central panel takes the
    // rest — the one order that leaves all of them any space.
    egui::Panel::top("toolbar").show(ui, |ui| {
        outcome.save_requested = draw_toolbar(ui, session);
    });
    egui::Panel::bottom("status_bar").show(ui, |ui| {
        draw_status_bar(ui, session, chrome);
    });
    if session.model().is_some() {
        egui::Panel::right("inspector")
            .default_size(INSPECTOR_WIDTH)
            .show(ui, |ui| {
                draw_inspector(ui, session, inspector);
            });
    }
    egui::CentralPanel::default().show(ui, |ui| {
        draw_central(ui, session);
    });
    if chrome.confirming_close {
        let context = ui.ctx().clone();
        outcome.close_choice = draw_close_confirm(&context);
    }
    outcome
}

/// Paint whatever `session` currently is into the central area.
///
/// Takes the session by `&mut` because the canvas is where dragging
/// happens (module doc): the pointer's effect on the arrangement is
/// computed in the same pass that paints it.
pub fn draw_central(ui: &mut egui::Ui, session: &mut EditorSession) {
    match session {
        EditorSession::Loading => draw_loading(ui),
        EditorSession::NoWorker { reason } => draw_no_worker(ui, reason.as_deref()),
        EditorSession::WaitingForPeer { model, .. } => draw_waiting_for_peer(ui, model),
        EditorSession::Editing { model, .. } => draw_scene(ui, model),
    }
}

/// The top strip: the Save button, and what the arrangement's state is.
///
/// Save is enabled by exactly [`Model::can_save`] — something to write,
/// and nothing blocking — so the button's own state is the answer to "may
/// this be saved", not a second opinion about it.
fn draw_toolbar(ui: &mut egui::Ui, session: &EditorSession) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let model = session.model();
        let enabled = model.is_some_and(Model::can_save);
        clicked = ui.add_enabled(enabled, egui::Button::new("Save")).clicked();
        let Some(model) = model else {
            return;
        };
        if model.diagnostics().blocks_save() {
            ui.label("Cannot be saved yet");
        } else if model.is_dirty() {
            ui.label("Unsaved changes");
        } else {
            ui.label("Drag a machine's screens to arrange them.");
        }
    });
    clicked
}

/// The size inspector: what the selected screen is, how big it is drawn,
/// and the two millimetre fields that correct it.
///
/// A projection, as thin as the canvas is: every rule it needs — what the
/// screen is called, what the fields hold, when they refill, what the
/// aspect lock does, which entries are refused and in what words, whether
/// the reset has anywhere to go — is `crate::inspector`'s, tested without a
/// window. What is here is widgets, and which of them was clicked.
fn draw_inspector(ui: &mut egui::Ui, session: &mut EditorSession, inspector: &mut Inspector) {
    ui.add_space(4.0);
    ui.heading("Screen size");
    ui.add_space(6.0);

    let facts = inspector.target().and_then(|target| {
        session
            .model()
            .and_then(|model| crate::inspector::facts(model, target))
    });
    let Some(facts) = facts else {
        // The empty state of a panel, in the manner of the editor's other
        // empty states: what to do, not an apology for having nothing.
        ui.label("Click a screen to see the size it is drawn at, and to correct it.");
        return;
    };

    draw_inspector_identity(ui, &facts);
    ui.add_space(8.0);
    let request = draw_size_fields(ui, inspector, &facts);
    if let Some((request, model)) = request.zip(session.model_mut()) {
        inspector.act(model, request);
        // The toolbar and the status bar claimed their strips before this
        // panel ran, so they are showing the diagnostics and the Save
        // button of the arrangement as it was a moment ago. Ask for another
        // frame rather than reordering the panels — a right panel drawn
        // before the top one takes the full window height and pushes the
        // toolbar out of the way, which is a worse picture than a Save
        // button that lights one frame late. The staleness is bounded at
        // that one frame, not at the ~1 s poll.
        ui.ctx().request_repaint();
    }
    if let Some(message) = inspector.message() {
        ui.add_space(6.0);
        let color = if message.refused {
            ui.visuals().error_fg_color
        } else {
            ui.visuals().text_color()
        };
        ui.colored_label(color, &message.text);
    }
}

/// Which screen the fields below are about: its machine, its caption, its
/// pixels, and — where it applies — why its size is a guess.
fn draw_inspector_identity(ui: &mut egui::Ui, facts: &MonitorFacts) {
    ui.small(&facts.machine);
    ui.label(
        egui::RichText::new(format!("{} · {}", facts.ordinal, facts.caption))
            .monospace()
            .strong(),
    );
    match facts.native_size {
        Some((width, height)) => ui.small(format!("{width}×{height} pixels")),
        // The same absence the caption shows: an arrangement can place a
        // screen the machine no longer reports, and its size is still
        // correctable — that rectangle is what the cursor crosses into when
        // it is plugged back in.
        None => ui.small("not attached right now"),
    };
    if facts.estimated {
        ui.add_space(4.0);
        ui.small(format!(
            "{} — this machine could not measure the panel, so this rectangle is drawn \
             from its pixels. Correcting it below is what makes the crossing land where \
             the drawing says.",
            crate::caption::SIZE_ESTIMATED_BADGE
        ));
    }
}

/// The editable half: two millimetre fields, the aspect lock, and the two
/// actions. Reports which action the user asked for, if either.
fn draw_size_fields(
    ui: &mut egui::Ui,
    inspector: &mut Inspector,
    facts: &MonitorFacts,
) -> Option<Request> {
    let mut request = None;
    // Enter commits from either field, because a two-field form where the
    // only way to commit is a mouse trip to a button is a form nobody
    // finishes with the keyboard.
    let entered = |ui: &egui::Ui, response: &egui::Response| {
        response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
    };

    ui.horizontal(|ui| {
        ui.label("Width");
        let response = ui.add(
            egui::TextEdit::singleline(inspector.width_field())
                .desired_width(56.0)
                .horizontal_align(egui::Align::RIGHT),
        );
        ui.label("mm");
        if response.changed() {
            inspector.width_edited();
        }
        if entered(ui, &response) {
            request = Some(Request::Apply);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Height");
        let response = ui.add(
            egui::TextEdit::singleline(inspector.height_field())
                .desired_width(56.0)
                .horizontal_align(egui::Align::RIGHT),
        );
        ui.label("mm");
        if response.changed() {
            inspector.height_edited();
        }
        if entered(ui, &response) {
            request = Some(Request::Apply);
        }
    });

    let mut locked = inspector.lock_aspect();
    if ui
        .checkbox(&mut locked, "Keep the current proportions")
        .changed()
    {
        inspector.set_lock_aspect(locked);
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Apply").clicked() {
            request = Some(Request::Apply);
        }
        // Offered only when there is a measurement to go back to and the
        // rectangle has been moved away from it — a screen nothing could
        // measure has nothing to reset *to* (`inspector.rs`).
        if ui
            .add_enabled(facts.resettable(), egui::Button::new("Use detected size"))
            .clicked()
        {
            request = Some(Request::Reset);
        }
    });
    request
}

/// Paint the bottom status line: freshness and peer state, whatever
/// validation has to say, and the last save's outcome.
pub fn draw_status_bar(ui: &mut egui::Ui, session: &EditorSession, chrome: Chrome<'_>) {
    ui.horizontal_wrapped(|ui| {
        ui.add_space(6.0);
        ui.label(status_line(session));
        if let Some(model) = session.model() {
            if let Some(phrase) = snapping_phrase(model) {
                ui.separator();
                ui.label(phrase);
            }
            if let Some((severity, message)) = model.diagnostics().headline() {
                ui.separator();
                ui.colored_label(severity_color(severity, ui.visuals()), message);
            }
        }
        if let Some((text, failed)) = chrome.status {
            ui.separator();
            let color = if failed {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().text_color()
            };
            ui.colored_label(color, text);
        }
    });
}

/// What the drag currently in progress has snapped to, if anything —
/// named, because a rectangle that jumped a few units should say why.
fn snapping_phrase(model: &Model) -> Option<String> {
    let drag = model.drag()?;
    let guides = drag.guides();
    if guides.is_empty() {
        return None;
    }
    let mut kinds: Vec<&str> = Vec::new();
    for guide in guides {
        let phrase = guide.kind.describe();
        if !kinds.contains(&phrase) {
            kinds.push(phrase);
        }
    }
    let machine = model
        .groups()
        .find(|group| group.device == drag.device())
        .map_or("this machine", |group| group.name.as_str());
    Some(format!("Snapping {machine}: {}", kinds.join(", ")))
}

/// Blocking borrows the style's error color, a warning its warning color —
/// both already chosen for the current light or dark visuals.
fn severity_color(severity: Severity, visuals: &egui::Visuals) -> Color32 {
    match severity {
        Severity::Blocking => visuals.error_fg_color,
        Severity::Warning => visuals.warn_fg_color,
    }
}

/// The unsaved-changes dialog: a real modal, so the window cannot be
/// closed out from under the question it is asking.
///
/// ADR 0018 makes the config file the only way an edit reaches the worker,
/// which means an editor closed with unsaved changes silently discards
/// work that looks, on screen, exactly like work that was saved. Hence the
/// interception.
fn draw_close_confirm(context: &egui::Context) -> Option<CloseChoice> {
    let mut choice = None;
    egui::Modal::new(egui::Id::new("crossover-layout-unsaved")).show(context, |ui| {
        ui.heading("Save this arrangement before closing?");
        ui.add_space(8.0);
        ui.label(
            "The arrangement you drew has not been written to the config file, so the \
             worker is still using the one it already had.",
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Save and close").clicked() {
                choice = Some(CloseChoice::Save);
            }
            if ui.button("Discard").clicked() {
                choice = Some(CloseChoice::Discard);
            }
            if ui.button("Cancel").clicked() {
                choice = Some(CloseChoice::Cancel);
            }
        });
    });
    choice
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
fn draw_waiting_for_peer(ui: &mut egui::Ui, model: &mut Model) {
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
/// available area — and the drag that rearranges them.
fn draw_scene(ui: &mut egui::Ui, model: &mut Model) {
    if let Some(reason) = &model.rejected_layout {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.small(format!(
                "The saved arrangement could not be used ({reason}) — this is a starting guess instead. Drag it into shape and save."
            ));
        });
    } else if model.seeded {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.small(
                "This is a starting guess, not a saved arrangement — drag the machines together and save it.",
            );
        });
    }

    let canvas_size = ui.available_size();
    let (response, painter) = ui.allocate_painter(canvas_size, egui::Sense::click_and_drag());
    let canvas_rect = response.rect;

    // A drag keeps the transform it started with: refitting mid-drag would
    // rescale the picture under the pointer, which moves the group again
    // (see `Drag::viewport`).
    let viewport = model.drag().map_or_else(
        || {
            Viewport::fit(
                model.bounds(),
                (canvas_rect.width(), canvas_rect.height()),
                CANVAS_PADDING,
            )
        },
        crate::model::Drag::viewport,
    );

    apply_pointer(model, &response, viewport, canvas_rect.min);

    let local_hue = base_hsva(ui.visuals());
    let peer_hue = rotated(local_hue);
    let authoritative_scene = !model.seeded;
    let selected = model.selected();

    paint_group(
        &painter,
        canvas_rect.min,
        &viewport,
        &model.local,
        local_hue,
        authoritative_scene,
        model.diagnostics(),
        selected,
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
            model.diagnostics(),
            selected,
            ui,
        );
    }
    if let Some(drag) = model.drag() {
        paint_guides(&painter, canvas_rect.min, &viewport, drag.guides(), ui);
    }
}

/// Turn this frame's pointer state into the model's pure drag operations.
///
/// The whole interaction is three calls and no state of its own: the model
/// owns what is being dragged and from where, so nothing here can disagree
/// with it about a drag in progress.
fn apply_pointer(
    model: &mut Model,
    response: &egui::Response,
    viewport: Viewport,
    canvas_origin: egui::Pos2,
) {
    let point = |position: egui::Pos2| {
        viewport.to_layout((position.x - canvas_origin.x, position.y - canvas_origin.y))
    };
    if response.clicked() {
        // A press that never became a drag: the user is pointing at a
        // screen rather than moving one, which is what the inspector
        // follows. A click on empty canvas clears the selection, so
        // "never mind" needs no separate gesture.
        if let Some(position) = response.interact_pointer_pos() {
            model.select_at(point(position));
        }
    } else if response.drag_started() {
        if let Some(position) = response.interact_pointer_pos() {
            let grabbed = point(position);
            if let Some(target) = model.monitor_at(grabbed) {
                model.begin_drag(&target, grabbed, viewport);
            }
        }
    } else if response.dragged() {
        if let Some(position) = response.interact_pointer_pos() {
            model.drag_to(point(position));
        }
    } else if response.drag_stopped() {
        model.end_drag();
    }
}

/// The lines a snap produced, drawn across both rectangles that agreed.
fn paint_guides(
    painter: &egui::Painter,
    canvas_origin: egui::Pos2,
    viewport: &Viewport,
    guides: &[Guide],
    ui: &egui::Ui,
) {
    let color = ui.visuals().strong_text_color();
    for guide in guides {
        let (from, to) = match guide.axis {
            Axis::X => (
                (guide.position, guide.span.0),
                (guide.position, guide.span.1),
            ),
            Axis::Y => (
                (guide.span.0, guide.position),
                (guide.span.1, guide.position),
            ),
        };
        // An abutment is the snap that creates a crossing, so it draws
        // heavier than a line-up that only tidies the picture.
        let width = if guide.kind == crate::snap::SnapKind::Abut {
            2.0
        } else {
            1.0
        };
        painter.line_segment(
            [
                to_absolute(viewport, canvas_origin, from),
                to_absolute(viewport, canvas_origin, to),
            ],
            egui::Stroke::new(width, color),
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
    diagnostics: &Diagnostics,
    selected: Option<&MonitorKey>,
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

    // One call per machine, before anything is painted: the duplicate rule
    // is a property of the group, so it cannot be decided a rectangle at a
    // time. Positionally aligned with `group.monitors`.
    let captions = crate::caption::captions(&group.caption_inputs());

    for (monitor, caption) in group.monitors.iter().zip(&captions) {
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
        let offending = diagnostics.offends(group.device, &monitor.id);
        let (fill, stroke) = if offending {
            // A blocking diagnostic outranks every other cue: this is the
            // rectangle standing between the user and a save.
            (
                fill_color,
                egui::Stroke::new(OFFENDER_STROKE_WIDTH, ui.visuals().error_fg_color),
            )
        } else if unplaced {
            (ghost_fill, egui::Stroke::new(1.0, ghost_stroke))
        } else {
            (fill_color, egui::Stroke::new(1.5, stroke_color))
        };
        painter.rect(rect, 2.0, fill, stroke, egui::StrokeKind::Inside);

        // The selection ring sits *outside* the rectangle, in the style's
        // own selection colour: what the inspector is about to talk about
        // is the rectangle's size, so nothing about selecting it may change
        // the size or the colour of the shape itself.
        if selected.is_some_and(|key| key.device == group.device && key.id == monitor.id) {
            painter.rect_stroke(
                rect.expand(SELECTION_RING_GAP),
                4.0,
                egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                egui::StrokeKind::Outside,
            );
        }

        if rect.width() >= MIN_LABEL_DIMENSION && rect.height() >= MIN_LABEL_DIMENSION {
            let mut label = caption.clone();
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

fn to_absolute(viewport: &Viewport, origin: egui::Pos2, point: (f64, f64)) -> egui::Pos2 {
    let (x, y) = viewport.to_screen(point);
    egui::pos2(origin.x + x, origin.y + y)
}

#[cfg(test)]
mod tests {
    use super::Chrome;
    use crate::model::Model;
    use crate::session::{EditorSession, Freshness};
    use crate::test_support::{
        LOCAL_DEVICE, PEER_DEVICE, arranged_document, document, live_monitor, monitor_key, paint,
        paint_settled, painted_text, peer_state, unit_viewport,
    };
    use crossover_topology::{DeviceId, MonitorId};

    fn editing(model: Model) -> EditorSession {
        EditorSession::Editing {
            model,
            staleness: Freshness::Fresh,
            peer_connected: true,
        }
    }

    fn key(device: DeviceId) -> crossover_topology::MonitorKey {
        monitor_key(device, r"\\.\DISPLAY1")
    }

    /// The saved side-by-side arrangement, with the peer's group picked up
    /// and nudged: the nudge is inside the snap threshold, so it settles
    /// back onto the seam and the guides that say so are live.
    fn mid_snapped_drag() -> Model {
        let mut model = Model::from_state(&arranged_document(0));
        model.begin_drag(&key(PEER_DEVICE), (2_000.0, 100.0), unit_viewport());
        model.drag_to((2_006.0, 100.0));
        assert!(
            !model.drag().expect("dragging").guides().is_empty(),
            "the fixture must actually snap"
        );
        model
    }

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

    /// The caption a user actually sees: the product name where there is
    /// one, the device string where there is not, duplicates numbered — the
    /// whole rule painted, not merely computed.
    #[test]
    fn a_monitor_is_captioned_by_its_product_name_and_falls_back_to_its_id() {
        use crate::test_support::{labelled_monitor, live_monitor};

        let mut state = document(None, 0);
        state.local.monitors = vec![
            labelled_monitor("A", "DELL U2720Q"),
            labelled_monitor("B", "DELL U2720Q"),
            live_monitor(r"\\.\DISPLAY9"),
        ];
        // The monitors are seeded side by side, so give each one room to
        // carry its caption rather than being dropped as too small.
        for (index, monitor) in state.local.monitors.iter_mut().enumerate() {
            monitor.rect.x = i32::try_from(index).unwrap() * 1920;
        }

        let session = EditorSession::WaitingForPeer {
            model: Model::from_state(&state),
            staleness: Freshness::Fresh,
        };
        let painted = painted_text(&session);

        assert!(painted.contains("DELL U2720Q (1)"), "{painted}");
        assert!(painted.contains("DELL U2720Q (2)"), "{painted}");
        assert!(painted.contains(r"\\.\DISPLAY9"), "{painted}");
        // The bare, un-numbered name never appears on its own line: both
        // copies are numbered, so a user cannot mistake one for "the" one.
        assert!(
            !painted.lines().any(|line| line.ends_with("· DELL U2720Q")),
            "{painted}"
        );
        // And the secondary information the editor always showed survives.
        assert!(painted.contains("1920×1080"), "{painted}");
    }

    /// The machine boundary, painted rather than only computed: both desks
    /// own a `DELL U2720Q`, and neither is numbered — they are drawn in
    /// separate groups under their own machine names, so numbering across
    /// the pair would group screens the user has no reason to see as a set.
    #[test]
    fn the_same_model_on_both_machines_is_not_numbered_across_the_pair() {
        use crate::test_support::labelled_monitor;

        let mut peer = peer_state(true);
        peer.monitors = vec![labelled_monitor(r"\\.\DISPLAY1", "DELL U2720Q")];
        let mut state = document(Some(peer), 0);
        state.local.monitors = vec![labelled_monitor(r"\\.\DISPLAY1", "DELL U2720Q")];

        let session = editing(Model::from_state(&state));
        let painted = painted_text(&session);

        assert!(painted.contains("DELL U2720Q"), "{painted}");
        assert!(
            !painted.contains("DELL U2720Q ("),
            "one machine's screen was numbered against the other's: {painted}"
        );
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

    /// A rectangle whose size nobody could measure says so on the canvas,
    /// and one that was measured does not — the badge is the *only* thing
    /// that tells the two apart, since a seeded proportion looks exactly as
    /// confident as a measured one.
    #[test]
    fn a_screen_that_could_not_be_measured_paints_its_badge() {
        use crate::test_support::live_monitor;
        use crossover_topology::{LiveMonitor, PhysicalSizeMm};

        let measured = |id: &str| LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(597, 336).unwrap()),
            ..live_monitor(id)
        };

        // Neither machine could measure anything: every rectangle is
        // seeded from pixels, exactly as before sizes existed, and a badge
        // on all of them would mark no difference at all.
        let state = document(Some(peer_state(true)), 0);
        let painted = painted_text(&editing(Model::from_state(&state)));
        assert!(!painted.contains("(size estimated)"), "{painted}");

        // One machine measured and the other did not: now there *is* a
        // difference between the rectangles, and the guesses say so.
        let mut state = document(Some(peer_state(true)), 0);
        state.peer.as_mut().unwrap().monitors = vec![measured(r"\\.\DISPLAY1")];
        let painted = painted_text(&editing(Model::from_state(&state)));
        assert!(painted.contains("(size estimated)"), "{painted}");

        // Both measured: nothing is badged, because nothing is a guess.
        let mut state = document(Some(peer_state(true)), 0);
        state.local.monitors = vec![measured(r"\\.\DISPLAY1")];
        state.peer.as_mut().unwrap().monitors = vec![measured(r"\\.\DISPLAY1")];
        let painted = painted_text(&editing(Model::from_state(&state)));
        assert!(
            painted.contains(r"\\.\DISPLAY1"),
            "the screens are still drawn and captioned: {painted}"
        );
        assert!(!painted.contains("(size estimated)"), "{painted}");
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

    /// A drag that snapped draws its guides and names what it snapped to.
    /// The line count is compared against the same frame with no drag at
    /// all, because separators are line segments too — the difference is
    /// what the guides added.
    #[test]
    fn a_snapped_drag_paints_its_guides_and_names_them() {
        let still = paint(
            &editing(Model::from_state(&arranged_document(0))),
            Chrome::default(),
        );
        let model = mid_snapped_drag();
        let guides = model.drag().expect("dragging").guides().len();
        let dragging = paint(&editing(model), Chrome::default());

        assert!(
            dragging.line_segments >= still.line_segments + guides,
            "{} lines with {guides} guides vs {} without",
            dragging.line_segments,
            still.line_segments
        );
        assert!(dragging.says("Snapping"), "{}", dragging.text);
        assert!(dragging.says("edges meet"), "{}", dragging.text);
        // The machine being dragged is named, not "something moved".
        assert!(dragging.says("laptop"), "{}", dragging.text);
    }

    /// A blocking diagnostic says what is wrong and that it cannot be
    /// saved — the toolbar and the status bar agree, and the offending
    /// screens are outlined (asserted through the model, which is what
    /// decides the stroke).
    #[test]
    fn an_overlap_is_reported_and_refuses_the_save() {
        let mut model = Model::from_state(&arranged_document(0));
        model.begin_drag(&key(LOCAL_DEVICE), (100.0, 100.0), unit_viewport());
        model.drag_to((600.0, 100.0));
        model.end_drag();
        assert!(!model.can_save());
        assert!(
            model
                .diagnostics()
                .offends(LOCAL_DEVICE, &MonitorId::new(r"\\.\DISPLAY1").unwrap())
        );

        let painted = paint(&editing(model), Chrome::default());
        assert!(painted.says("Cannot be saved yet"), "{}", painted.text);
        assert!(painted.says("overlap"), "{}", painted.text);
        assert!(painted.says("Save"), "{}", painted.text);
    }

    /// A warning is shown in its own right, and the save stays offered.
    #[test]
    fn a_disconnected_arrangement_warns_and_still_offers_the_save() {
        let mut model = Model::from_state(&arranged_document(0));
        model.begin_drag(&key(LOCAL_DEVICE), (100.0, 100.0), unit_viewport());
        model.drag_to((100.0, 6_000.0));
        model.end_drag();
        assert!(model.can_save());

        let painted = paint(&editing(model), Chrome::default());
        assert!(painted.says("do not touch"), "{}", painted.text);
        assert!(painted.says("Unsaved changes"), "{}", painted.text);
    }

    /// The clean, saved state says what to do rather than nothing.
    #[test]
    fn an_unedited_arrangement_invites_a_drag() {
        let painted = paint(
            &editing(Model::from_state(&arranged_document(0))),
            Chrome::default(),
        );
        assert!(painted.says("Drag a machine's screens"), "{}", painted.text);
    }

    /// The unsaved-changes dialog offers all three answers, and says what
    /// discarding would cost. Painted through `paint_settled`, because a
    /// modal is an `Area` and egui sizes a new one invisibly on its first
    /// frame — see that function's docs.
    #[test]
    fn the_close_confirmation_offers_save_discard_and_cancel() {
        let painted = paint_settled(
            &editing(Model::from_state(&arranged_document(0))),
            Chrome {
                confirming_close: true,
                ..Chrome::default()
            },
        );
        assert!(
            painted.says("Save this arrangement before closing?"),
            "{}",
            painted.text
        );
        assert!(painted.says("Save and close"), "{}", painted.text);
        assert!(painted.says("Discard"), "{}", painted.text);
        assert!(painted.says("Cancel"), "{}", painted.text);
        assert!(painted.says("worker is still using"), "{}", painted.text);
    }

    /// The inspector's own empty state: with nothing selected it says what
    /// to do, in the manner of the editor's other empty screens, rather
    /// than showing two blank fields about no screen in particular.
    #[test]
    fn the_size_panel_asks_for_a_selection_before_it_says_anything() {
        let painted = paint(
            &editing(Model::from_state(&arranged_document(0))),
            Chrome::default(),
        );
        assert!(painted.says("Screen size"), "{}", painted.text);
        assert!(painted.says("Click a screen"), "{}", painted.text);
        assert!(!painted.says("Use detected size"), "{}", painted.text);
    }

    /// A selected screen is named the way the canvas names it, described by
    /// its pixels, and offered in **millimetres** — the drawn rectangle,
    /// divided out, which is what the user is being asked to correct.
    #[test]
    fn the_size_panel_offers_the_selected_screens_size_in_millimetres() {
        use crossover_topology::{LiveMonitor, PhysicalSizeMm};

        let mut state = document(Some(peer_state(true)), 0);
        state.local.monitors = vec![LiveMonitor {
            physical_size: Some(PhysicalSizeMm::new(597, 336).unwrap()),
            ..crate::test_support::labelled_monitor(r"\\.\DISPLAY1", "DELL U2720Q")
        }];
        let mut model = Model::from_state(&state);
        model.select(Some(&key(LOCAL_DEVICE)));

        let mut inspector = crate::inspector::Inspector::new();
        let painted =
            crate::test_support::paint_with(&editing(model), Chrome::default(), &mut inspector);

        assert!(painted.says("DELL U2720Q"), "{}", painted.text);
        assert!(painted.says("1920×1080 pixels"), "{}", painted.text);
        assert!(painted.says("Width"), "{}", painted.text);
        assert!(painted.says("Height"), "{}", painted.text);
        assert!(painted.says("mm"), "{}", painted.text);
        assert!(
            painted.says("597"),
            "the drawn width in mm: {}",
            painted.text
        );
        assert!(painted.says("336"), "{}", painted.text);
        assert!(painted.says("Apply"), "{}", painted.text);
        // Measured, and drawn at what was measured, so there is nothing to
        // go back to — the control is painted but disabled.
        assert!(painted.says("Use detected size"), "{}", painted.text);
        assert!(
            painted.says("proportions"),
            "the aspect lock: {}",
            painted.text
        );
    }

    /// A refused entry reaches the user *in the panel*, beside the fields
    /// it refused, rather than in the status bar with the save diagnostics
    /// — and nothing is drawn at the refused size.
    #[test]
    fn a_refused_size_is_reported_in_the_panel_that_refused_it() {
        let mut model = Model::from_state(&arranged_document(0));
        model.select(Some(&key(LOCAL_DEVICE)));
        let mut inspector = crate::inspector::Inspector::new();
        inspector.sync(Some(&model));
        inspector.typed("9000", "5000");
        inspector.act(&mut model, crate::inspector::Request::Apply);
        assert!(!model.is_dirty(), "nothing was drawn at that size");

        let painted =
            crate::test_support::paint_with(&editing(model), Chrome::default(), &mut inspector);
        assert!(painted.says("3000 mm"), "{}", painted.text);
        assert!(painted.says("not a size to draw from"), "{}", painted.text);
    }

    /// A screen nobody could measure says so where it is being corrected,
    /// not only on the canvas — the panel is where the user is looking when
    /// they are about to fix it.
    #[test]
    fn the_size_panel_explains_an_estimated_rectangle() {
        // The peer measured itself and the local machine did not, so the
        // local rectangle is a guess with something to be a guess *against*
        // — which is the only condition the badge is painted under.
        let mut state = document(Some(peer_state(true)), 0);
        state.peer.as_mut().unwrap().monitors = vec![crossover_topology::LiveMonitor {
            physical_size: Some(crossover_topology::PhysicalSizeMm::new(286, 179).unwrap()),
            ..live_monitor(r"\\.\DISPLAY1")
        }];
        let mut model = Model::from_state(&state);
        model.select(Some(&key(LOCAL_DEVICE)));
        assert!(model.local.monitors[0].size_estimated);

        let mut inspector = crate::inspector::Inspector::new();
        let painted =
            crate::test_support::paint_with(&editing(model), Chrome::default(), &mut inspector);
        assert!(painted.says("(size estimated)"), "{}", painted.text);
        assert!(painted.says("could not measure"), "{}", painted.text);
    }

    /// The selected rectangle is ringed on the canvas, so the panel and the
    /// picture are visibly about the same screen. Counted against the same
    /// frame with nothing selected, because a monitor is a rectangle too —
    /// the difference is the ring.
    #[test]
    fn the_selected_screen_is_ringed_on_the_canvas() {
        let plain =
            crate::test_support::paint_canvas(&editing(Model::from_state(&arranged_document(0))));
        let mut model = Model::from_state(&arranged_document(0));
        model.select(Some(&key(LOCAL_DEVICE)));
        let ringed = crate::test_support::paint_canvas(&editing(model));
        assert_eq!(
            ringed.rects,
            plain.rects + 1,
            "exactly one ring, around exactly one screen"
        );
    }

    /// A save's outcome — success or the whole error chain — reaches the
    /// status bar, which is the only place a windowed process can report
    /// one (NFR-3).
    #[test]
    fn the_status_bar_carries_the_last_saves_outcome() {
        let session = editing(Model::from_state(&arranged_document(0)));
        let saved = paint(
            &session,
            Chrome {
                status: Some(("Saved (revision 5). The worker picks it up shortly.", false)),
                ..Chrome::default()
            },
        );
        assert!(saved.says("Saved (revision 5)"), "{}", saved.text);

        let failed = paint(
            &session,
            Chrome {
                status: Some((
                    "Not saved — writing the config file failed: the existing config file \
                     is not valid TOML; refusing to overwrite it",
                    true,
                )),
                ..Chrome::default()
            },
        );
        assert!(failed.says("Not saved"), "{}", failed.text);
        assert!(failed.says("not valid TOML"), "{}", failed.text);
    }
}
