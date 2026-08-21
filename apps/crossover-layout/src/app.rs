//! The editor application: its window, its state, and its drawing.
//!
//! Split from `main.rs` so the entry point stays about *starting a process*
//! (version reporting, exit codes) and this module about *being an editor*.
//! Today it holds one screen — the empty state below — and the canvas, the
//! state-file reader, and the config writer grow into it from here.

use std::sync::Arc;

use eframe::egui;

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
            // Roomy enough to draw two desks' worth of monitors side by side
            // once the canvas lands, and freely resizable below that.
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
            Ok(Box::new(LayoutEditor))
        }),
    )
}

/// Give egui the only fonts it has. With `default_fonts` off there is no
/// fallback stack behind these: an empty `FontDefinitions` renders nothing at
/// all, so this runs before the first frame.
fn install_fonts(context: &egui::Context) {
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

/// Everything the editor knows. Nothing, for now: this branch establishes the
/// crate, the window, and the packaging; reading the worker's state file is
/// the canvas branch's first job (ADR 0018's worker→editor contract).
struct LayoutEditor;

impl eframe::App for LayoutEditor {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The `Ui` eframe hands over has no margin or background of its own; a
        // central panel is what gives the window the theme's surface to draw
        // on, and it is the container the canvas will fill.
        egui::CentralPanel::default().show(ui, |ui| {
            draw_worker_absent(ui);
        });
    }
}

/// The empty state: what the editor shows when it has no live facts to draw.
///
/// ADR 0018 makes this a first-class screen rather than a blank canvas. The
/// state file carries a heartbeat precisely so the editor can say *the worker
/// is not running* instead of presenting stale monitors as current — and until
/// the reader lands, this is the only thing the editor honestly knows.
fn draw_worker_absent(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 3.0);
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
    });
}

#[cfg(test)]
mod tests {
    use super::{draw_worker_absent, install_fonts};
    use eframe::egui;

    /// Drive one headless pass of the real drawing code and read back what it
    /// painted.
    ///
    /// egui needs no window to lay out and rasterize a frame, which is what
    /// makes the editor's screens testable on all three CI OSes — none of
    /// which has a display server (docs/TESTING.md §2).
    fn painted_text() -> String {
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
                egui::CentralPanel::default().show(ui, draw_worker_absent);
            },
        );
        // There is no renderer here to upload the font atlas to, and epaint
        // treats an unapplied texture delta as a defect rather than letting it
        // drop quietly — so a headless pass has to say it meant it.
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

    /// The empty state is the only thing this binary says today, and it says
    /// it with fonts it embeds itself — with `default_fonts` off (ADR 0019),
    /// a font set that failed to load renders *nothing* rather than failing
    /// loudly, so painting no text is the failure this asserts against.
    #[test]
    fn the_empty_state_paints_what_it_promises() {
        let painted = painted_text();
        assert!(
            painted.contains("The Crossover worker is not running"),
            "the empty state painted: {painted}"
        );
        // The command names are the actionable half, and they are the only
        // text here that goes through the *monospace* family — so one
        // assertion covers both embedded faces, and an empty family for
        // either one fails this test rather than passing silently.
        assert!(painted.contains("crossover run"), "painted: {painted}");
        assert!(
            painted.contains("crossover service install"),
            "painted: {painted}"
        );
    }
}
