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

use crate::render;
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

/// Everything the editor knows: its current screen (with the grace-period
/// bookkeeping `SessionTracker` adds), and when it last read the state file
/// to get there.
struct LayoutEditor {
    session: SessionTracker,
    last_poll: Instant,
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
        };
        editor.poll();
        editor
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Shown first so it claims its strip of the window before the
        // central panel takes the rest — window resize is handled entirely
        // by `render.rs`'s `Viewport::fit`, recomputed from whatever area
        // is left every frame, so there is nothing else to react to here.
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            render::draw_status_bar(ui, self.session.session());
        });
        egui::CentralPanel::default().show(ui, |ui| {
            render::draw_central(ui, self.session.session());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::POLL_INTERVAL;
    use crate::session::EditorSession;
    use crate::test_support::painted_text;

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
