//! The editor application: its window, its state, and its drawing.
//!
//! Split from `main.rs` so the entry point stays about *starting a process*
//! (version reporting, exit codes) and this module about *being an editor*.
//! [`LayoutEditor`] owns one [`SessionTracker`] (`session.rs`) and ticks its
//! ~1 s re-read of the worker's state file (`state_file.rs`); every screen
//! it can be in is painted by `render.rs`, and the transform between the
//! layout model (`model.rs`) and the screen is `viewport.rs`'s.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::inspector::Inspector;
use crate::render::{self, Chrome, CloseChoice};
use crate::save;
use crate::session::{SessionEvent, SessionTracker};
use crate::state_file;

/// How often the app re-reads the state file. ADR 0018/0019 call for
/// "~1 s"; this is the exact figure everything else (the poll deadline,
/// the test below) is defined against.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The editor's text, embedded rather than borrowed from the machine: the two
/// Go fonts (Bigelow & Holmes, BSD-3-Clause — `assets/fonts/`). egui bundles
/// its own faces, but under OFL-1.1 and Ubuntu-font-1.0, which this tree's
/// dependency policy does not allow, so `default_fonts` is off and these take
/// their place (ADR 0019). Latin, Greek, and Cyrillic; a character outside
/// that coverage renders as a box rather than as another font's glyph.
const PROPORTIONAL_FONT: &[u8] = include_bytes!("../../../assets/fonts/Go-Regular.ttf");
const MONOSPACE_FONT: &[u8] = include_bytes!("../../../assets/fonts/Go-Mono.ttf");

/// Open the editor window and run until the user closes it.
///
/// # Errors
///
/// When no window can be created — no display server, no usable OpenGL
/// context. `main` reports it; there is no fallback, because an editor
/// without a window has nothing to offer.
pub fn run(title: &str) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            // Roomy enough to draw two desks' worth of monitors side by side,
            // and freely resizable below that.
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([480.0, 320.0]),
        // Explicit rather than inferred: the wgpu backend is compiled out
        // (ADR 0019), and naming the renderer here keeps the manifest's
        // feature set and this call from drifting apart silently.
        renderer: eframe::Renderer::Glow,
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(|creation| {
            install_fonts(&creation.egui_ctx);
            Ok(Box::new(LayoutEditor::new()))
        }),
    )
}

/// Give egui the only fonts it has. With `default_fonts` off there is no
/// fallback stack behind these: an empty `FontDefinitions` renders nothing at
/// all, so this runs before the first frame.
///
/// `pub(crate)` rather than private: `test_support.rs`'s shared headless
/// harness installs these same, real fonts rather than keeping a second
/// `include_bytes!` copy of its own (a font set that failed to load renders
/// *nothing*, which would make every text assertion in the crate vacuously
/// true against a stand-in).
pub(crate) fn install_fonts(context: &egui::Context) {
    let mut fonts = egui::FontDefinitions::empty();
    fonts.font_data.insert(
        "go-regular".to_owned(),
        Arc::new(egui::FontData::from_static(PROPORTIONAL_FONT)),
    );
    fonts.font_data.insert(
        "go-mono".to_owned(),
        Arc::new(egui::FontData::from_static(MONOSPACE_FONT)),
    );
    // Monitor device strings (`\\.\DISPLAY1`) are the text this editor shows
    // most of, and they read far better fixed-width — so the monospace family
    // is a real monospace face, not the proportional one wearing its name.
    fonts.families.insert(
        egui::FontFamily::Proportional,
        vec!["go-regular".to_owned()],
    );
    fonts
        .families
        .insert(egui::FontFamily::Monospace, vec!["go-mono".to_owned()]);
    context.set_fonts(fonts);
}

/// The last save attempt's outcome, as the status bar says it.
struct SaveStatus {
    text: String,
    failed: bool,
}

/// Why [`LayoutEditor::save`] wrote nothing.
enum NotSaved {
    /// There is no arrangement that has been changed — a scene nobody has
    /// dragged, or one already written. Not a failure: nothing was asked
    /// for that has not already happened.
    NothingToSave,
    /// A write was attempted and refused, or failed, with this chain.
    Failed(String),
}

/// Where the window is in answering a close it intercepted.
///
/// One value rather than two booleans, because the pair had a fourth,
/// meaningless combination (confirming *and* closing) that nothing
/// prevented and every branch had to be read twice to rule out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseState {
    /// No close is outstanding — the ordinary state.
    Open,
    /// A close was intercepted and the unsaved-changes dialog is up,
    /// waiting on an answer.
    Confirming,
    /// A close has been agreed to and re-issued. Set *before* re-issuing,
    /// so the interception does not catch its own answer and ask again —
    /// the loop a naive "always intercept" would produce.
    Closing,
}

/// Everything the editor knows: its current screen (with the grace-period
/// bookkeeping `SessionTracker` adds), when it last read the state file to
/// get there, what the last save did, and where a close has got to.
struct LayoutEditor {
    session: SessionTracker,
    last_poll: Instant,
    status: Option<SaveStatus>,
    close_state: CloseState,
    /// The size inspector's two fields and its aspect lock
    /// (`inspector.rs`). Here rather than in the model because a model is
    /// rebuilt from the state file once a second and a half-typed number is
    /// not something to reconcile; the *selection* it follows does live in
    /// the model, because a monitor can stop being drawn.
    inspector: Inspector,
}

impl LayoutEditor {
    /// A fresh editor, having already done its first read — so the very
    /// first frame shows real facts (or the real absence of them) rather
    /// than a `Loading` flash nobody would see anyway, since eframe does
    /// not call `logic` before the first `ui`.
    fn new() -> Self {
        let mut editor = Self {
            session: SessionTracker::new(),
            last_poll: Instant::now(),
            status: None,
            close_state: CloseState::Open,
            inspector: Inspector::new(),
        };
        editor.poll();
        editor
    }

    /// Whether there is work that exists nowhere but this window — the
    /// question a close has to ask.
    ///
    /// Exactly `SessionTracker`'s own answer, which is the same predicate
    /// the state-file poll reconciles against (`session.rs`): an edit that
    /// has not been written, a gesture still in the user's hand, or an edit
    /// being held while the worker's empty state is on screen. A close
    /// asking a *different* question than the poll answers is how one of
    /// them ends up discarding what the other was protecting.
    fn has_unsaved_work(&self) -> bool {
        self.session.has_unsaved_work()
    }

    /// Write the arrangement to the config file, and report what happened
    /// in the status bar. `true` when it landed.
    ///
    /// Success clears the dirty flag and opens the post-save hold
    /// (`session.rs`); nothing else. The worker's own ~2 s config re-read
    /// is what makes the edit live, and the state file this editor polls
    /// will report the new revision when it does — so the loop closes
    /// through the two files, with no rendezvous between the processes
    /// (ADR 0018/0019).
    ///
    /// **A scene nobody has changed is not written.** The Save button is
    /// already disabled for that, but the close dialog's "Save and close"
    /// is not the same path, and a dialog that came up for a drag that
    /// ended where it started would otherwise persist a *seed* — an
    /// arrangement this editor invented and the user never accepted — as
    /// though it had been drawn.
    fn save(&mut self) -> bool {
        // Settle any gesture first. A save can be asked for mid-drag (the
        // close dialog answers a window closed without letting go), and an
        // undropped drag is an arrangement that has not been committed.
        if let Some(model) = self.session.savable_mut() {
            model.end_drag();
        }
        let outcome = match self.session.savable_mut() {
            Some(model) if !model.is_dirty() => Err(NotSaved::NothingToSave),
            Some(model) => match save::save(model) {
                Ok(revision) => {
                    model.mark_saved(revision);
                    Ok(revision)
                }
                Err(error) => Err(NotSaved::Failed(error.chain())),
            },
            None => Err(NotSaved::Failed(
                "there is no arrangement to save".to_owned(),
            )),
        };
        match outcome {
            Ok(revision) => {
                self.session.note_saved(revision);
                tracing::info!(revision, "saved the drawn arrangement to the config file");
                self.status = Some(SaveStatus {
                    text: format!("Saved (revision {revision}). The worker picks it up shortly."),
                    failed: false,
                });
                true
            }
            Err(NotSaved::NothingToSave) => {
                self.status = Some(SaveStatus {
                    text: "Nothing to save — the arrangement has not been changed.".to_owned(),
                    failed: false,
                });
                false
            }
            Err(NotSaved::Failed(reason)) => {
                tracing::warn!(reason = %reason, "the drawn arrangement could not be saved");
                self.status = Some(SaveStatus {
                    text: format!("Not saved — {reason}"),
                    failed: true,
                });
                false
            }
        }
    }

    /// Let the close that was intercepted proceed.
    fn close(&mut self, context: &egui::Context) {
        self.close_state = CloseState::Closing;
        context.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Act on what the frame reported the user clicked.
    fn apply(&mut self, outcome: render::FrameOutcome, context: &egui::Context) {
        if outcome.save_requested {
            self.save();
        }
        match outcome.close_choice {
            Some(CloseChoice::Save) => {
                // A *failed* save keeps the dialog up with the reason in
                // the status bar: closing anyway would discard the work the
                // user just asked to keep. A save that wrote nothing
                // because there was nothing to write leaves nothing to
                // lose, so that close proceeds.
                if self.save() || !self.has_unsaved_work() {
                    self.close(context);
                }
            }
            Some(CloseChoice::Discard) => self.close(context),
            Some(CloseChoice::Cancel) => self.close_state = CloseState::Open,
            None => {}
        }
    }

    /// Re-check the close dialog's own premise, because the poll can
    /// remove it: a re-pair discards the edit the dialog is asking about
    /// (`session.rs`), and a question about work that no longer exists has
    /// no honest answer.
    ///
    /// It dismisses itself rather than closing the window. The close was
    /// already cancelled, and quietly closing now would hide the fact that
    /// the edit went away — the user asked to close a window with work in
    /// it, and what is in front of them no longer is that window.
    fn dismiss_a_dialog_with_nothing_left_to_ask(&mut self) {
        if self.close_state == CloseState::Confirming && !self.has_unsaved_work() {
            self.close_state = CloseState::Open;
        }
    }

    /// Re-read the state file, advance the session, and log exactly the
    /// transitions `SessionTracker` reports — never once per poll (NFR-3):
    /// a state file stuck unreadable would otherwise print the same line
    /// once a second forever.
    fn poll(&mut self) {
        let status = state_file::read_state_file();
        match self.session.on_read(status) {
            SessionEvent::Unchanged => {}
            SessionEvent::Unreadable(reason) => {
                tracing::warn!(reason = %reason, "the worker's state file could not be used");
            }
            SessionEvent::Demoted(reason) => {
                tracing::warn!(
                    reason = %reason,
                    "no drawn arrangement survived the read-failure grace period; showing the empty state"
                );
            }
            SessionEvent::Recovered => {
                tracing::info!("the worker's state file is readable again");
            }
        }
    }
}

impl eframe::App for LayoutEditor {
    /// Runs before painting, every frame eframe decides to paint one.
    /// `request_repaint_after` is computed against the *original* poll
    /// deadline rather than reset to a fresh `POLL_INTERVAL` on every call,
    /// so a frame painted early for some other reason (an input event, a
    /// resize) cannot keep pushing the next poll further out — sparse
    /// input stretching the effective cadence toward 2x is exactly the
    /// failure a fixed deadline avoids. No background thread: the ~1 s
    /// cadence ADR 0018/0019 call for is entirely this timer.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let elapsed = self.last_poll.elapsed();
        if elapsed >= POLL_INTERVAL {
            self.last_poll = Instant::now();
            self.poll();
            ctx.request_repaint_after(POLL_INTERVAL);
        } else {
            ctx.request_repaint_after(POLL_INTERVAL.saturating_sub(elapsed));
        }

        self.dismiss_a_dialog_with_nothing_left_to_ask();

        // An unsaved arrangement is work that exists nowhere but this
        // window: the config file is the only way an edit reaches the
        // worker (ADR 0018), so closing without writing discards it
        // silently. Cancel the close, ask, and let `apply` re-issue it.
        // The test is the same one the poll reconciles against, so a close
        // *mid-drag* — before the drop that would mark the scene dirty —
        // is intercepted too.
        if self.close_state != CloseState::Closing
            && self.has_unsaved_work()
            && ctx.input(|input| input.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_state = CloseState::Confirming;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // `self.status` and `self.session` are disjoint fields, so the
        // chrome may borrow one while the frame mutates the other — no
        // per-frame copy of the status text is needed to satisfy anything.
        let chrome = Chrome {
            status: self
                .status
                .as_ref()
                .map(|status| (status.text.as_str(), status.failed)),
            confirming_close: self.close_state == CloseState::Confirming,
        };
        let outcome =
            render::draw_frame(ui, self.session.session_mut(), &mut self.inspector, chrome);
        let context = ui.ctx().clone();
        self.apply(outcome, &context);
    }
}

#[cfg(test)]
impl LayoutEditor {
    /// An editor around an already-prepared session, with **no state-file
    /// read**.
    ///
    /// `new` polls, and a poll reads the real `~/.crossover`; a unit test
    /// must neither depend on what is there nor be affected by it. The
    /// tests below drive the close and save decisions, all of which are
    /// answered before any filesystem is touched.
    fn around(session: SessionTracker) -> Self {
        Self {
            session,
            last_poll: Instant::now(),
            status: None,
            close_state: CloseState::Open,
            inspector: Inspector::new(),
        }
    }

    /// What the status bar currently says, if anything.
    fn status_text(&self) -> Option<&str> {
        self.status.as_ref().map(|status| status.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseState, LayoutEditor, POLL_INTERVAL};
    use crate::render::{CloseChoice, FrameOutcome};
    use crate::session::{EditorSession, SessionTracker};
    use crate::state_file::StateFileStatus;
    use crate::test_support::{
        LOCAL_DEVICE, arranged_document, document, drag_until_dirty, monitor_key, painted_text,
        peer_state, unit_viewport,
    };
    use crossover_topology::DeviceId;
    use eframe::egui;

    /// An editor showing the fixture arrangement, having read it once.
    fn editor() -> LayoutEditor {
        let mut session = SessionTracker::new();
        let _ = session.on_read(StateFileStatus::Fresh(arranged_document(0)));
        LayoutEditor::around(session)
    }

    /// A context to hand `apply`. Nothing here opens a window: an
    /// `egui::Context` is a value, and a viewport command sent to one with
    /// no viewport is simply queued and never read.
    fn context() -> egui::Context {
        egui::Context::default()
    }

    /// A close mid-drag must be intercepted. The dirty flag is only set by
    /// the *drop*, so an interception that asked about dirtiness alone
    /// would let a window close silently over a gesture in the user's hand
    /// — the same predicate mistake `session.rs`'s poll had.
    #[test]
    fn a_close_mid_drag_still_has_something_to_ask_about() {
        let mut editor = editor();
        assert!(!editor.has_unsaved_work(), "the fixture starts clean");

        let model = editor.session.savable_mut().expect("a scene");
        model.begin_drag(
            &monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY1"),
            (10.0, 10.0),
            unit_viewport(),
        );
        model.drag_to((10.0, 3_010.0));
        assert!(!model.is_dirty(), "a drag in flight is not yet dirty");
        assert!(
            editor.has_unsaved_work(),
            "but the close must still ask about it"
        );
    }

    /// The dialog re-validates its premise: if a poll replaced the model
    /// while it was up — a re-pair, which discards the edit — there is
    /// nothing left to ask about, so it dismisses itself rather than
    /// offering to save work that no longer exists.
    #[test]
    fn the_close_dialog_dismisses_itself_when_a_poll_removes_its_premise() {
        let mut editor = editor();
        drag_until_dirty(editor.session.savable_mut().expect("a scene"));
        editor.close_state = CloseState::Confirming;

        // Still dirty, so the question still stands.
        editor.dismiss_a_dialog_with_nothing_left_to_ask();
        assert_eq!(editor.close_state, CloseState::Confirming);

        // A re-pair: a different machine at the other end discards the
        // drawing (`session.rs`), and with it the dialog's premise.
        let mut stranger = peer_state(true);
        stranger.device = DeviceId::from_bytes([0x77; 16]);
        let _ = editor
            .session
            .on_read(StateFileStatus::Fresh(document(Some(stranger), 1)));
        assert!(!editor.has_unsaved_work());

        editor.dismiss_a_dialog_with_nothing_left_to_ask();
        assert_eq!(
            editor.close_state,
            CloseState::Open,
            "a dialog with nothing to ask must not stay up"
        );
    }

    /// A save with nothing to write **writes nothing**, and says so.
    ///
    /// The Save button is already disabled for an unedited scene, but the
    /// close dialog's "Save and close" is a different path to the same
    /// call. Without this guard it would persist a *seed* — an arrangement
    /// this editor invented as a starting guess and the user never
    /// accepted — into the worker's config file, on the strength of a
    /// dialog that should not have been asking. The refusal happens before
    /// any path is resolved, so this test touches no filesystem.
    #[test]
    fn a_save_with_nothing_to_write_refuses_rather_than_persisting_a_seed() {
        let mut editor = editor();
        let before = editor.session.savable_mut().expect("a scene").seen_revision;

        assert!(!editor.save(), "nothing was written");
        assert_eq!(
            editor.status_text(),
            Some("Nothing to save — the arrangement has not been changed."),
            "the status bar must say why"
        );
        assert_eq!(
            editor.session.savable_mut().expect("a scene").seen_revision,
            before,
            "a refused save records no revision"
        );
    }

    /// "Save and close" on a scene with nothing to write still closes:
    /// there is nothing left to lose, so keeping the window open would be
    /// refusing to do the one thing the user asked for.
    #[test]
    fn save_and_close_with_nothing_to_write_still_closes() {
        let mut editor = editor();
        editor.close_state = CloseState::Confirming;
        editor.apply(
            FrameOutcome {
                save_requested: false,
                close_choice: Some(CloseChoice::Save),
            },
            &context(),
        );
        assert_eq!(editor.close_state, CloseState::Closing);
    }

    /// Cancel puts the window back exactly as it was — the close state is
    /// reset, not left half-set for the next frame's interception to trip
    /// over.
    #[test]
    fn cancelling_the_close_resets_the_state() {
        let mut editor = editor();
        drag_until_dirty(editor.session.savable_mut().expect("a scene"));
        editor.close_state = CloseState::Confirming;
        editor.apply(
            FrameOutcome {
                save_requested: false,
                close_choice: Some(CloseChoice::Cancel),
            },
            &context(),
        );
        assert_eq!(editor.close_state, CloseState::Open);
        assert!(
            editor.has_unsaved_work(),
            "cancelling keeps the work, obviously"
        );
    }

    /// Discard closes without writing, and without asking again.
    #[test]
    fn discarding_closes_without_writing() {
        let mut editor = editor();
        drag_until_dirty(editor.session.savable_mut().expect("a scene"));
        editor.close_state = CloseState::Confirming;
        editor.apply(
            FrameOutcome {
                save_requested: false,
                close_choice: Some(CloseChoice::Discard),
            },
            &context(),
        );
        assert_eq!(editor.close_state, CloseState::Closing);
        assert!(editor.status_text().is_none(), "nothing was written");
    }

    /// The empty state is still the first thing an editor with no worker
    /// says, reached through `EditorSession` and `render.rs` — this is the
    /// integration point the fuller per-screen coverage lives in
    /// `render.rs`'s own tests.
    #[test]
    fn the_empty_state_paints_through_the_app_layer_with_its_own_fonts() {
        let painted = painted_text(&EditorSession::NoWorker { reason: None });
        assert!(
            painted.contains("The Crossover worker is not running"),
            "the empty state painted: {painted}"
        );
        assert!(painted.contains("crossover run"), "painted: {painted}");
        assert!(
            painted.contains("crossover service install"),
            "painted: {painted}"
        );
    }

    #[test]
    fn the_poll_interval_is_about_one_second() {
        assert_eq!(POLL_INTERVAL, std::time::Duration::from_secs(1));
    }
}
