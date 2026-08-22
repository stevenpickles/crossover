//! Correcting one monitor's **size** by hand — pure, and separate from the
//! panel that shows it (ADR 0018, addendum 2026-08-22, feature/161).
//!
//! # Why an override exists
//!
//! The seeding rule ([`crate::seeding`]) draws a panel from its own
//! millimetres where the machine could read them and from its pixels where
//! it could not. Both arms can be wrong about a real screen: an EDID can be
//! absent (a virtual display, a remote session, a KVM that does not pass one
//! through), it can be cached from a *different* panel, and a size outside
//! the believable range is deliberately discarded rather than drawn. In
//! every one of those cases the user is looking at the screen and knows what
//! it is; nothing else in the system does.
//!
//! # An override is a resize, and nothing else
//!
//! ADR 0018 makes the drawn rectangle the size: crossings map
//! proportionally along drawn edges, so the proportions in the drawing *are*
//! the crossing mapping. Correcting a screen is therefore resizing its
//! rectangle — [`crate::model::Model::set_size_mm`] — and it needs no new
//! persistence, no schema change, and no protocol change, because a saved
//! arrangement already carries every rectangle's extent and reads it back as
//! authoritative. It dirties the scene exactly as a drag does, and the Save
//! button, the revision, and the sync behave exactly as they always did.
//!
//! # What this module decides
//!
//! Everything except the widgets: what the panel says about the selected
//! monitor ([`MonitorFacts`]), what the two text fields hold and when they
//! are refilled, how the aspect lock fills one from the other, which
//! entries are refused and in what words, and whether "use detected size"
//! has anything to go back to. `render.rs` draws the result and reports
//! clicks; it holds no rule of its own.
//!
//! # Refusing rather than clamping
//!
//! An entry outside the shared plausible range
//! (`crossover_topology::MIN_PLAUSIBLE_PHYSICAL_MM`..=`MAX_…`, the same
//! 50–3000 mm the EDID reader and the seeder apply) is **refused with a
//! message**, not quietly clamped. A clamp would draw a rectangle other
//! than the one the user typed and say nothing about it — which is how a
//! typo becomes a crossing seam in the wrong place — while the range itself
//! is worth enforcing for exactly the reason the seeder enforces it: a size
//! is a proportion, so one absurd number does not misdraw one rectangle, it
//! misdraws the desk.

use crossover_topology::{MAX_PLAUSIBLE_PHYSICAL_MM, MIN_PLAUSIBLE_PHYSICAL_MM, MonitorKey};

use crate::caption;
use crate::model::Model;
use crate::seeding;

/// Everything the inspector says about one monitor, derived from the scene
/// rather than stored: the sizes are read off the drawn rectangle every
/// frame, so a rectangle that changed for any reason (a poll, a reset, an
/// override) describes itself correctly with no state to keep in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorFacts {
    /// The machine the monitor is attached to, by name.
    pub machine: String,
    /// Its caption: the product name where its machine advertised one, its
    /// device string otherwise, numbered where two identical screens on one
    /// machine would otherwise read alike ([`crate::caption`], the same
    /// rule and the same call the canvas paints with — an inspector that
    /// named a rectangle differently from the rectangle would be worse than
    /// one that named nothing).
    pub caption: String,
    /// Its 1-based position in its machine's group.
    pub ordinal: usize,
    /// Its live pixel size, or `None` for a monitor an arrangement places
    /// but the machine no longer reports.
    pub native_size: Option<(u32, u32)>,
    /// The **drawn** size in millimetres — the rectangle through
    /// [`crate::seeding::mm_for_units`], which is what the fields are
    /// pre-filled with, and the granularity every question about whether a
    /// size *changed* is asked at.
    pub drawn_mm: (u32, u32),
    /// The drawn rectangle itself, in layout units — carried so the aspect
    /// lock can fill one field from the other at the proportion actually on
    /// screen rather than at the proportion of the two rounded millimetre
    /// figures beside it. Rounding the ratio's inputs once per correction is
    /// how a 16:9 screen slowly becomes a 1.775:1 one over a few of them.
    pub drawn_units: (u32, u32),
    /// What the machine says the panel measures, where it says anything
    /// believable ([`crate::model::DrawnMonitor::detected_size_mm`]).
    pub detected_mm: Option<(u32, u32)>,
    /// Whether the drawn size is still the seeder's guess.
    pub estimated: bool,
}

impl MonitorFacts {
    /// Whether "use detected size" has anywhere to go: a believable
    /// measurement exists, and the rectangle is not already drawn at it.
    ///
    /// A screen nothing could measure — the `(size estimated)` case — has no
    /// reset, because there is nothing to reset *to*.
    #[must_use]
    pub fn resettable(&self) -> bool {
        self.detected_mm
            .is_some_and(|detected| detected != self.drawn_mm)
    }
}

/// What the inspector says about `target`, or `None` when the scene no
/// longer draws it.
#[must_use]
pub fn facts(model: &Model, target: &MonitorKey) -> Option<MonitorFacts> {
    let (group, monitor) = model.find(target)?;
    let captions = caption::display_names(&group.caption_inputs());
    let caption = group
        .monitors
        .iter()
        .position(|drawn| drawn.id == monitor.id)
        .and_then(|index| captions.get(index).cloned())
        .unwrap_or_else(|| monitor.id.as_str().to_owned());
    Some(MonitorFacts {
        machine: group.name.clone(),
        caption,
        ordinal: monitor.ordinal,
        native_size: monitor.native_size,
        drawn_mm: (
            seeding::mm_for_units(monitor.rect.width),
            seeding::mm_for_units(monitor.rect.height),
        ),
        drawn_units: (monitor.rect.width, monitor.rect.height),
        detected_mm: monitor.detected_size_mm,
        estimated: monitor.size_estimated,
    })
}

/// One line for the user, and whether it is a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// What to say.
    pub text: String,
    /// `true` when the entry was refused — the panel's cue to use the
    /// style's error colour, as the status bar does for a blocked save.
    pub refused: bool,
}

/// What the user asked the inspector to do this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Draw the selected monitor at the millimetres in the fields.
    Apply,
    /// Draw it at the size its machine says the panel measures.
    Reset,
}

/// The two text fields, the aspect lock, and the last thing said about
/// them — the only editor state that is neither a fact from the worker nor
/// part of the drawn scene.
///
/// It lives in `app.rs` beside the session rather than inside [`Model`] on
/// purpose: a model is rebuilt from the state file once a second, and a
/// half-typed number is not something to rebuild or to reconcile. What the
/// model does keep is the *selection*, because that is a property of the
/// scene (a monitor can stop being drawn) and the canvas is what sets it.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspector {
    /// The monitor the fields describe, so a changed selection refills
    /// them.
    target: Option<MonitorKey>,
    /// The drawn size the fields were last filled from, in millimetres —
    /// the test for "the rectangle changed underneath the fields", which is
    /// what a reset, an applied override, or a re-seeded poll all look like
    /// from here. In millimetres because that is what the fields show: a
    /// rectangle that moved by a quarter of one has changed nothing the
    /// user could read, and refilling for it would only be a chance to
    /// overwrite something half-typed.
    filled_from: (u32, u32),
    /// The width field's text, in millimetres.
    width: String,
    /// The height field's text, in millimetres.
    height: String,
    /// Whether editing one dimension fills the other from the rectangle's
    /// current aspect. **On by default**: a panel's proportions are the
    /// part of a size the crossing mapping actually reads, a user correcting
    /// a screen almost always knows its diagonal or one edge rather than
    /// both edges, and a lock that has to be switched on is a lock nobody
    /// discovers.
    lock_aspect: bool,
    /// The aspect the lock fills by — width ÷ height of the **rectangle**
    /// the fields were last filled from, so it follows the drawing rather
    /// than whatever half-typed pair is in the fields, and does not drift
    /// through the rounding on the way to and from them.
    aspect: f64,
    /// The last thing said about an entry, until the selection moves.
    message: Option<Message>,
}

impl Default for Inspector {
    fn default() -> Self {
        Self::new()
    }
}

impl Inspector {
    /// A fresh inspector: nothing selected, the aspect lock on.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            target: None,
            filled_from: (0, 0),
            width: String::new(),
            height: String::new(),
            lock_aspect: true,
            aspect: 1.0,
            message: None,
        }
    }

    /// Point the fields at whatever the scene currently has selected.
    ///
    /// Refills them when the selection moves **or** when the selected
    /// rectangle's drawn size changes underneath them — an applied
    /// override, a reset, a poll that re-seeded it — so what is in the
    /// fields is always what is on the canvas. It does **not** refill while
    /// only the text has changed, which is what makes typing possible at
    /// all.
    ///
    /// The message is cleared only when the *selection* moves. An applied
    /// override changes the rectangle in the same gesture that produced the
    /// message, and clearing it here would wipe the confirmation before the
    /// user could read it.
    pub fn sync(&mut self, model: Option<&Model>) {
        let selection = model.and_then(Model::selected).cloned();
        let current = selection
            .as_ref()
            .and_then(|target| model.and_then(|model| facts(model, target)));
        let Some((target, facts)) = selection.zip(current) else {
            self.target = None;
            self.message = None;
            self.width.clear();
            self.height.clear();
            return;
        };
        let moved = self.target.as_ref() != Some(&target);
        if moved {
            self.message = None;
        }
        if moved || self.filled_from != facts.drawn_mm {
            self.target = Some(target);
            self.filled_from = facts.drawn_mm;
            self.width = facts.drawn_mm.0.to_string();
            self.height = facts.drawn_mm.1.to_string();
            self.aspect = aspect_of(facts.drawn_units);
        }
    }

    /// The monitor the fields describe, if any.
    #[must_use]
    pub const fn target(&self) -> Option<&MonitorKey> {
        self.target.as_ref()
    }

    /// The width field's text, for the widget to edit in place.
    pub const fn width_field(&mut self) -> &mut String {
        &mut self.width
    }

    /// The height field's text, for the widget to edit in place.
    pub const fn height_field(&mut self) -> &mut String {
        &mut self.height
    }

    /// Whether the aspect lock is on — the toggle's state.
    #[must_use]
    pub const fn lock_aspect(&self) -> bool {
        self.lock_aspect
    }

    /// Turn the aspect lock on or off. Switching it on does not disturb
    /// what is already typed: it applies from the next edit, so a user who
    /// has just entered a deliberate pair does not watch one of them jump.
    pub const fn set_lock_aspect(&mut self, locked: bool) {
        self.lock_aspect = locked;
    }

    /// The last thing said about an entry.
    #[must_use]
    pub const fn message(&self) -> Option<&Message> {
        self.message.as_ref()
    }

    /// The width field was edited: fill the height from the aspect, when
    /// the lock is on.
    pub fn width_edited(&mut self) {
        if let Some(width) = self.lock_aspect.then(|| parse(&self.width)).flatten() {
            self.height = scaled(width, 1.0 / self.aspect).to_string();
        }
    }

    /// The height field was edited: fill the width from the aspect, when
    /// the lock is on.
    pub fn height_edited(&mut self) {
        if let Some(height) = self.lock_aspect.then(|| parse(&self.height)).flatten() {
            self.width = scaled(height, self.aspect).to_string();
        }
    }

    /// Act on `request` against the scene, and record what to say about it.
    ///
    /// The whole of the inspector's behaviour, so `render.rs` can be a
    /// button that calls this and a label that prints
    /// [`Inspector::message`].
    pub fn act(&mut self, model: &mut Model, request: Request) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.message = Some(match request {
            Request::Apply => match self.entered() {
                Err(refusal) => refusal,
                Ok((width_mm, height_mm)) => {
                    if model.set_size_mm(&target, width_mm, height_mm) {
                        said(format!("Drawn at {width_mm} × {height_mm} mm."))
                    } else {
                        said("Already drawn at that size.".to_owned())
                    }
                }
            },
            Request::Reset => match facts(model, &target).and_then(|facts| facts.detected_mm) {
                None => said(
                    "This machine could not measure this panel, so there is no detected size \
                     to return to."
                        .to_owned(),
                ),
                Some((width_mm, height_mm)) if model.reset_to_detected_size(&target) => said(
                    format!("Back to the size this machine detected: {width_mm} × {height_mm} mm."),
                ),
                Some(_) => said("Already drawn at the detected size.".to_owned()),
            },
        });
    }

    /// The pair in the fields, or the refusal to show instead.
    ///
    /// # Errors
    ///
    /// When either field is not a whole number, or names a size no panel
    /// could be — the shared plausible range, refused rather than clamped
    /// (module doc).
    fn entered(&self) -> Result<(u32, u32), Message> {
        let read = |text: &str, axis: &str| -> Result<u32, Message> {
            let Some(value) = parse(text) else {
                return Err(refused(format!(
                    "Enter the panel's {axis} as a whole number of millimetres."
                )));
            };
            if !(u32::from(MIN_PLAUSIBLE_PHYSICAL_MM)..=u32::from(MAX_PLAUSIBLE_PHYSICAL_MM))
                .contains(&value)
            {
                return Err(refused(format!(
                    "A panel is between {MIN_PLAUSIBLE_PHYSICAL_MM} mm and \
                     {MAX_PLAUSIBLE_PHYSICAL_MM} mm on a side, so {value} mm is not a size to \
                     draw from."
                )));
            }
            Ok(value)
        };
        Ok((read(&self.width, "width")?, read(&self.height, "height")?))
    }
}

/// The aspect the lock fills by, taken from the **drawn rectangle** rather
/// than from the millimetres shown beside it — see
/// [`MonitorFacts::drawn_units`] — and defended against the degenerate pair
/// no drawn rectangle actually has (every extent is at least 1).
fn aspect_of((width, height): (u32, u32)) -> f64 {
    if width == 0 || height == 0 {
        return 1.0;
    }
    f64::from(width) / f64::from(height)
}

/// `value` millimetres through `ratio`, rounded, never zero, and never past
/// what a `u32` field can show — total for any ratio the aspect can hold.
fn scaled(value: u32, ratio: f64) -> u32 {
    let product = f64::from(value) * ratio;
    if !product.is_finite() {
        return value;
    }
    let clamped = product.round().clamp(1.0, f64::from(u16::MAX));
    // `clamp` has already put this inside `1.0..=65535.0`, which `u32`
    // represents exactly.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let narrowed = clamped as u32;
    narrowed
}

/// A field's whole number of millimetres, or `None` when it is not one.
/// Surrounding space is forgiven; anything else is not, because a silently
/// ignored character is a silently different size.
fn parse(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok()
}

fn said(text: String) -> Message {
    Message {
        text,
        refused: false,
    }
}

fn refused(text: String) -> Message {
    Message {
        text,
        refused: true,
    }
}

#[cfg(test)]
impl Inspector {
    /// Put `width` and `height` in the fields, as typing them would — the
    /// one way a test stands in for a keyboard, since the widgets
    /// themselves are `render.rs`'s.
    pub(crate) fn typed(&mut self, width: &str, height: &str) {
        self.width = width.to_owned();
        self.height = height.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::{Inspector, Request, facts};
    use crate::model::Model;
    use crate::seeding::UNITS_PER_MM;
    use crate::test_support::{
        LOCAL_DEVICE, arranged_document, document, live_monitor, monitor_key, peer_state,
    };
    use crossover_topology::{LiveMonitor, PhysicalSizeMm};

    /// A seeded scene whose local machine has two screens: one that
    /// measured itself and one that did not (so it is badged, and has no
    /// detected size to reset to).
    fn scene() -> Model {
        let mut state = document(Some(peer_state(true)), 0);
        state.local.monitors = vec![
            LiveMonitor {
                physical_size: Some(PhysicalSizeMm::new(597, 336).unwrap()),
                ..live_monitor(r"\\.\DISPLAY1")
            },
            LiveMonitor {
                rect: crossover_topology::LayoutRect {
                    x: 1920,
                    ..live_monitor(r"\\.\DISPLAY2").rect
                },
                ..live_monitor(r"\\.\DISPLAY2")
            },
        ];
        Model::from_state(&state)
    }

    fn select(model: &mut Model, id: &str) {
        model.select(Some(&monitor_key(LOCAL_DEVICE, id)));
    }

    fn inspecting(model: &Model) -> Inspector {
        let mut inspector = Inspector::new();
        inspector.sync(Some(model));
        inspector
    }

    #[test]
    fn the_fields_are_prefilled_with_the_rectangle_as_it_is_drawn() {
        let mut model = scene();
        select(&mut model, r"\\.\DISPLAY1");
        let inspector = inspecting(&model);

        assert_eq!(inspector.width, "597", "the measured panel, in millimetres");
        assert_eq!(inspector.height, "336");
        let facts = facts(&model, inspector.target().unwrap()).unwrap();
        assert_eq!(facts.native_size, Some((1920, 1080)), "the pixels, too");
        assert!(!facts.estimated);
        assert!(!facts.resettable(), "it is already drawn at its own size");
    }

    /// The headline gesture: type the real size of a screen the machine got
    /// wrong, and the rectangle is drawn at it — dirty, badge cleared, and
    /// with no persistence of its own (the rectangle *is* the size).
    #[test]
    fn applying_a_size_resizes_the_rectangle_and_dirties_the_scene() {
        let mut model = scene();
        select(&mut model, r"\\.\DISPLAY2");
        let mut inspector = inspecting(&model);
        assert!(
            facts(&model, inspector.target().unwrap())
                .unwrap()
                .estimated,
            "the fixture's second screen is a guess"
        );

        inspector.width.clear();
        inspector.width.push_str("300");
        inspector.height.clear();
        inspector.height.push_str("200");
        inspector.act(&mut model, Request::Apply);

        let (_, drawn) = model
            .find(&monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY2"))
            .unwrap();
        assert_eq!(drawn.rect.width, 300 * UNITS_PER_MM);
        assert_eq!(drawn.rect.height, 200 * UNITS_PER_MM);
        assert!(!drawn.size_estimated, "the user stated it; it is no guess");
        assert!(drawn.size_edited);
        assert!(model.is_dirty(), "an override dirties like a drag");
        assert!(!inspector.message().unwrap().refused);
    }

    /// A number no panel could be is **refused with a reason**, not clamped
    /// into range behind the user's back: a clamp would draw something
    /// other than what was typed and say nothing about it.
    #[test]
    fn a_size_outside_the_plausible_range_is_refused_and_nothing_is_drawn() {
        let mut model = scene();
        select(&mut model, r"\\.\DISPLAY2");
        let mut inspector = inspecting(&model);
        let before = model
            .find(&monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY2"))
            .unwrap()
            .1
            .rect;

        for (width, height) in [("49", "300"), ("3001", "300"), ("0", "0"), ("597", "4000")] {
            inspector.width = width.to_owned();
            inspector.height = height.to_owned();
            inspector.act(&mut model, Request::Apply);

            let message = inspector.message().expect("a refusal says why");
            assert!(message.refused, "{width}x{height}: {message:?}");
            assert!(message.text.contains("3000 mm"), "{message:?}");
            assert_eq!(
                model
                    .find(&monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY2"))
                    .unwrap()
                    .1
                    .rect,
                before,
                "a refused entry draws nothing"
            );
            assert!(!model.is_dirty(), "and dirties nothing");
        }
    }

    #[test]
    fn a_field_that_is_not_a_number_is_refused_in_its_own_words() {
        let mut model = scene();
        select(&mut model, r"\\.\DISPLAY2");
        let mut inspector = inspecting(&model);
        inspector.width = "about a foot".to_owned();
        inspector.act(&mut model, Request::Apply);

        let message = inspector.message().unwrap();
        assert!(message.refused);
        assert!(message.text.contains("whole number"), "{message:?}");
        assert!(!model.is_dirty());
    }

    /// The aspect lock, which is on by default: correcting the width fills
    /// the height from the rectangle's own proportions, and switching the
    /// lock off leaves both fields alone.
    #[test]
    fn the_aspect_lock_fills_the_other_dimension_and_can_be_turned_off() {
        let mut model = scene();
        select(&mut model, r"\\.\DISPLAY1");
        let mut inspector = inspecting(&model);
        assert!(inspector.lock_aspect(), "on by default");

        // 597 × 336 is 16:9; half the width is half the height.
        inspector.width = "300".to_owned();
        inspector.width_edited();
        assert_eq!(inspector.height, "169", "336 × 300/597 rounds to 169");

        // And the other way round.
        inspector.height = "336".to_owned();
        inspector.height_edited();
        assert_eq!(inspector.width, "597");

        inspector.set_lock_aspect(false);
        inspector.width = "300".to_owned();
        inspector.width_edited();
        assert_eq!(inspector.height, "336", "free entry leaves it alone");
    }

    /// "Use detected size" is offered exactly when there is a measurement
    /// to go back to and the rectangle has been moved away from it — and it
    /// puts the rectangle back on the machine's own numbers.
    #[test]
    fn the_reset_is_offered_only_when_there_is_a_detected_size_to_return_to() {
        let mut model = scene();

        // A screen nothing could measure has nothing to reset to, however
        // it is drawn.
        select(&mut model, r"\\.\DISPLAY2");
        let mut inspector = inspecting(&model);
        assert!(
            !facts(&model, inspector.target().unwrap())
                .unwrap()
                .resettable()
        );
        inspector.width = "300".to_owned();
        inspector.height = "200".to_owned();
        inspector.act(&mut model, Request::Apply);
        inspector.sync(Some(&model));
        assert!(
            !facts(&model, inspector.target().unwrap())
                .unwrap()
                .resettable(),
            "an override does not invent a measurement to go back to"
        );

        // A measured screen the user has overridden does.
        select(&mut model, r"\\.\DISPLAY1");
        let mut inspector = inspecting(&model);
        inspector.width = "500".to_owned();
        inspector.height = "281".to_owned();
        inspector.act(&mut model, Request::Apply);
        inspector.sync(Some(&model));
        let facts = facts(&model, inspector.target().unwrap()).unwrap();
        assert_eq!(facts.drawn_mm, (500, 281));
        assert!(facts.resettable());

        inspector.act(&mut model, Request::Reset);
        let (_, drawn) = model
            .find(&monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY1"))
            .unwrap();
        assert_eq!(drawn.rect.width, 597 * UNITS_PER_MM);
        assert_eq!(drawn.rect.height, 336 * UNITS_PER_MM);
        assert!(inspector.message().unwrap().text.contains("597 × 336"));
        assert!(
            drawn.size_edited,
            "a reset is the user's statement too, and the poll must keep it"
        );
    }

    /// The fields follow the selection, and follow the rectangle when it
    /// changes underneath them — but they do **not** overwrite what is
    /// being typed, which is the one thing that would make the panel
    /// unusable.
    #[test]
    fn the_fields_follow_the_selection_without_overwriting_what_is_typed() {
        let mut model = scene();
        select(&mut model, r"\\.\DISPLAY1");
        let mut inspector = inspecting(&model);
        assert_eq!(inspector.width, "597");

        // Mid-entry: a sync (which happens every frame, poll or no poll)
        // leaves the half-typed number alone.
        inspector.width = "5".to_owned();
        inspector.sync(Some(&model));
        assert_eq!(inspector.width, "5");

        // A different screen refills both fields and drops the message.
        inspector.act(&mut model, Request::Apply);
        assert!(inspector.message().is_some());
        select(&mut model, r"\\.\DISPLAY2");
        inspector.sync(Some(&model));
        assert_eq!(
            inspector.target(),
            Some(&monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY2"))
        );
        assert!(inspector.message().is_none(), "a new screen, a clean slate");
        assert_ne!(inspector.width, "5");

        // And a scene with nothing selected empties it.
        model.select(None);
        inspector.sync(Some(&model));
        assert!(inspector.target().is_none());
        assert!(inspector.width.is_empty());
    }

    /// **Apply on untouched fields must do nothing.** The fields hold whole
    /// millimetres and a drawn rectangle is quarter-millimetres, so a
    /// rectangle whose extent is not a multiple of four — every rectangle
    /// the DIP fallback seeds — does not survive a round trip through them
    /// unless "already drawn at that size" is asked in *millimetres*. Asked
    /// in units it would answer no, and an accidental Enter would nudge the
    /// geometry, dirty the scene, shift the row, and pin the unconditional
    /// transplant hold on a size the user never stated.
    #[test]
    fn pressing_apply_on_untouched_fields_changes_nothing() {
        // 1366 units wide reads as 341.5 mm, which the field rounds to 342
        // and converts back to 1368. Nothing anywhere is measured, so this
        // is the plain DIP seeding and the rectangle is the pixel count.
        let mut state = document(Some(peer_state(true)), 0);
        state.local.monitors = vec![LiveMonitor {
            rect: crossover_topology::LayoutRect {
                x: 0,
                y: 0,
                width: 1366,
                height: 768,
            },
            ..live_monitor(r"\\.\DISPLAY1")
        }];
        let mut model = Model::from_state(&state);
        select(&mut model, r"\\.\DISPLAY1");
        let mut inspector = inspecting(&model);
        assert_eq!(inspector.width, "342", "the millimetre it reads as");

        inspector.act(&mut model, Request::Apply);

        let (_, drawn) = model
            .find(&monitor_key(LOCAL_DEVICE, r"\\.\DISPLAY1"))
            .unwrap();
        assert_eq!(drawn.rect.width, 1366, "the rectangle did not move");
        assert!(!drawn.size_edited, "and nothing was pinned as stated");
        assert!(!model.is_dirty(), "and the Save button did not light");
        assert_eq!(
            inspector.message().map(|message| message.text.as_str()),
            Some("Already drawn at that size.")
        );

        // A millimetre either way *is* expressible, and does change it.
        inspector.typed("343", "192");
        inspector.act(&mut model, Request::Apply);
        assert!(model.is_dirty());
    }

    /// The same rule where the extent came from a *saved* arrangement,
    /// which is the other way a rectangle acquires a size nobody typed.
    #[test]
    fn re_entering_a_saved_rectangles_own_size_changes_nothing() {
        let mut model = Model::from_state(&arranged_document(0));
        select(&mut model, r"\\.\DISPLAY1");
        let mut inspector = inspecting(&model);
        let drawn = inspector.width.clone();

        inspector.act(&mut model, Request::Apply);
        assert!(!model.is_dirty(), "{drawn} mm was already what was drawn");
        assert!(!inspector.message().unwrap().refused);
    }

    /// The caption the inspector shows is the caption on the rectangle,
    /// duplicate numbering included — one rule, one call.
    #[test]
    fn the_inspector_names_a_screen_the_way_the_canvas_does() {
        use crate::test_support::labelled_monitor;

        let mut state = document(Some(peer_state(true)), 0);
        state.local.monitors = vec![
            labelled_monitor(r"\\.\DISPLAY1", "DELL U2720Q"),
            LiveMonitor {
                rect: crossover_topology::LayoutRect {
                    x: 1920,
                    ..live_monitor(r"\\.\DISPLAY2").rect
                },
                ..labelled_monitor(r"\\.\DISPLAY2", "DELL U2720Q")
            },
        ];
        let mut model = Model::from_state(&state);
        select(&mut model, r"\\.\DISPLAY2");

        let facts = facts(&model, model.selected().unwrap()).unwrap();
        assert_eq!(facts.caption, "DELL U2720Q (2)");
        assert_eq!(facts.machine, "workstation-left");
    }
}
