//! Platform-neutral input model (FR-4.x, docs/SPECIFICATION.md §4.4).
//!
//! [`InputState`] tracks *what this machine believes is held down on the
//! destination* — pointer buttons and, from Phase 4, keyboard keys — and
//! synthesizes the releases for all of it: [`InputState::release_all`]
//! for buttons, [`InputState::release_all_keys`] for keys. This is the
//! mechanism FR-4.4 requires, and the one that keeps a disconnect from
//! leaving a stuck button *or a stuck key/modifier* (ADR 0008).
//!
//! Buttons are a fixed set of five, so they live in an array; keys are a
//! sparse slice of the USB HID usage namespace, so held keys live in a
//! `BTreeSet` — sorted, which keeps release order deterministic (NFR-2).
//!
//! Motion is **relative** (ADR 0007: Raw Input reports unaccelerated,
//! unclamped deltas), which makes coalescing exact rather than lossy:
//! merging two movements is addition, where merging absolute positions
//! would mean discarding one. Key transitions are never coalesced
//! (FR-4.2).
//!
//! The event *vocabulary* ([`PointerEvent`], [`PointerButton`],
//! [`KeyEvent`]) lives in `crossover-platform`, because the HAL traits
//! must speak it and cannot depend on this crate (docs/ARCHITECTURE.md
//! §2). What lives here is *policy*: what is believed held, and what may
//! be merged.

use std::collections::BTreeSet;

pub use crossover_platform::{
    InputEvent, KeyEvent, PointerButton, PointerEvent, SCROLL_UNITS_PER_DETENT, hid,
};

/// What this machine believes is currently held down on the destination.
///
/// "Believes" is the operative word (FR-4.3): the state is maintained
/// from what was *sent*, so that when a session dies mid-gesture there
/// is a record of what needs releasing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputState {
    buttons: [bool; 5],
    /// Held keys, by USB HID usage. A set, not per-event: only the usage
    /// determines what must be released, so a repeat is idempotent and a
    /// release simply removes. Sorted iteration gives deterministic
    /// release order (NFR-2).
    keys: BTreeSet<u16>,
}

impl InputState {
    /// Nothing held.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update from an event that is being sent to the destination.
    pub fn apply(&mut self, event: PointerEvent) {
        if let PointerEvent::Button { button, pressed } = event {
            self.buttons[button.index()] = pressed;
        }
    }

    /// Update from a whole sequence.
    pub fn apply_all(&mut self, events: &[PointerEvent]) {
        for event in events {
            self.apply(*event);
        }
    }

    /// Is `button` believed held?
    #[must_use]
    pub fn is_pressed(&self, button: PointerButton) -> bool {
        self.buttons[button.index()]
    }

    /// Buttons believed held, in [`PointerButton::ALL`] order.
    pub fn pressed(&self) -> impl Iterator<Item = PointerButton> + '_ {
        PointerButton::ALL
            .into_iter()
            .filter(|button| self.buttons[button.index()])
    }

    /// Update from a key event being sent to the destination. A press (or
    /// its auto-repeat) marks the key held; a release clears it. Only the
    /// usage matters, so `text` and `repeat` do not affect the belief.
    pub fn apply_key(&mut self, event: &KeyEvent) {
        if event.pressed {
            self.keys.insert(event.key);
        } else {
            self.keys.remove(&event.key);
        }
    }

    /// Update from a whole sequence of key events.
    pub fn apply_keys(&mut self, events: &[KeyEvent]) {
        for event in events {
            self.apply_key(event);
        }
    }

    /// Update from one event of either kind.
    pub fn apply_input(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Pointer(pointer) => self.apply(*pointer),
            InputEvent::Key(key) => self.apply_key(key),
        }
    }

    /// Update from a whole sequence of mixed events.
    pub fn apply_inputs(&mut self, events: &[InputEvent]) {
        for event in events {
            self.apply_input(event);
        }
    }

    /// Is the key with USB HID usage `key` believed held?
    #[must_use]
    pub fn is_key_held(&self, key: u16) -> bool {
        self.keys.contains(&key)
    }

    /// Keys believed held, by usage, in ascending (deterministic) order.
    pub fn keys_held(&self) -> impl Iterator<Item = u16> + '_ {
        self.keys.iter().copied()
    }

    /// Nothing is held — no button and no key. The state a session may
    /// safely end in.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        !self.buttons.iter().any(|down| *down) && self.keys.is_empty()
    }

    /// `ReleaseAllInput` for buttons (FR-4.4): the release events needed
    /// to leave the destination holding no button, and clear that belief.
    ///
    /// Called on disconnect, session termination, fatal protocol failure,
    /// and control reset. A stuck button after any of those is a
    /// release-blocking defect, so this is deliberately total: it emits a
    /// release for every button believed held, and afterwards no button
    /// is held.
    pub fn release_all(&mut self) -> Vec<PointerEvent> {
        let releases: Vec<PointerEvent> = self
            .pressed()
            .map(|button| PointerEvent::Button {
                button,
                pressed: false,
            })
            .collect();
        self.buttons = [false; 5];
        releases
    }

    /// `ReleaseAllInput` for keys (FR-4.4): a release for every key
    /// believed held, in deterministic usage order (NFR-2), clearing the
    /// belief. A stuck key or modifier after a disconnect is the same
    /// release-blocking defect class as a stuck button (ADR 0008), so
    /// this is equally total: afterwards no key is held.
    pub fn release_all_keys(&mut self) -> Vec<KeyEvent> {
        let releases: Vec<KeyEvent> = self.keys_held().map(KeyEvent::release).collect();
        self.keys.clear();
        releases
    }
}

/// Merge adjacent coalescable events, preserving order everywhere else
/// (FR-4.2).
///
/// Motion and scroll accumulate; a button transition is a barrier. That
/// barrier is not fussiness: merging motion *across* a press would move
/// the click, changing where the user clicked. Merging only adjacent
/// runs keeps every button transition at the position it happened.
#[must_use]
pub fn coalesce(events: &[PointerEvent]) -> Vec<PointerEvent> {
    let mut out: Vec<PointerEvent> = Vec::with_capacity(events.len());
    for event in events.iter().copied() {
        let merged = out.last_mut().is_some_and(|last| merge_into(last, event));
        if !merged {
            out.push(event);
        }
    }
    // A run that cancels out (moved right then left) leaves a zero
    // delta worth nothing on the wire.
    out.retain(|event| {
        !matches!(
            event,
            PointerEvent::Motion { dx: 0, dy: 0 } | PointerEvent::Scroll { dx: 0, dy: 0 }
        )
    });
    out
}

/// Accumulate `next` into `target` when both are the same coalescable
/// kind; `false` means they must stay separate events.
fn merge_into(target: &mut PointerEvent, next: PointerEvent) -> bool {
    match (target, next) {
        (PointerEvent::Motion { dx, dy }, PointerEvent::Motion { dx: ndx, dy: ndy })
        | (PointerEvent::Scroll { dx, dy }, PointerEvent::Scroll { dx: ndx, dy: ndy }) => {
            *dx = dx.saturating_add(ndx);
            *dy = dy.saturating_add(ndy);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{InputState, KeyEvent, PointerButton, PointerEvent, coalesce, hid};

    fn motion(dx: i32, dy: i32) -> PointerEvent {
        PointerEvent::Motion { dx, dy }
    }

    fn press(button: PointerButton) -> PointerEvent {
        PointerEvent::Button {
            button,
            pressed: true,
        }
    }

    fn release(button: PointerButton) -> PointerEvent {
        PointerEvent::Button {
            button,
            pressed: false,
        }
    }

    #[test]
    fn state_tracks_presses_and_releases() {
        let mut state = InputState::new();
        assert!(state.is_clear());

        state.apply(press(PointerButton::Left));
        state.apply(press(PointerButton::X2));
        assert!(state.is_pressed(PointerButton::Left));
        assert!(state.is_pressed(PointerButton::X2));
        assert!(!state.is_pressed(PointerButton::Right));
        assert!(!state.is_clear());

        state.apply(release(PointerButton::Left));
        assert!(!state.is_pressed(PointerButton::Left));
        assert!(!state.is_clear());

        state.apply(release(PointerButton::X2));
        assert!(state.is_clear());
    }

    #[test]
    fn motion_and_scroll_do_not_disturb_button_state() {
        let mut state = InputState::new();
        state.apply(press(PointerButton::Middle));
        state.apply(motion(40, -20));
        state.apply(PointerEvent::Scroll { dx: 0, dy: 120 });
        assert!(state.is_pressed(PointerButton::Middle));
    }

    /// FR-4.4 for the disconnect-mid-drag case: the exact scenario the
    /// spec calls release-blocking.
    #[test]
    fn release_all_clears_a_drag_in_progress() {
        let mut state = InputState::new();
        state.apply(press(PointerButton::Left));
        state.apply(motion(100, 100));

        let releases = state.release_all();
        assert_eq!(releases, vec![release(PointerButton::Left)]);
        assert!(state.is_clear());
    }

    #[test]
    fn release_all_is_ordered_and_idempotent() {
        let mut state = InputState::new();
        for button in PointerButton::ALL {
            state.apply(press(button));
        }
        let releases = state.release_all();
        assert_eq!(
            releases,
            PointerButton::ALL.map(release).to_vec(),
            "releases must follow the declared order (NFR-2)"
        );
        // Nothing left to release, and calling again is harmless.
        assert!(state.release_all().is_empty());
        assert!(state.is_clear());
    }

    #[test]
    fn adjacent_motion_merges_by_addition() {
        let merged = coalesce(&[motion(3, 4), motion(5, -2), motion(1, 1)]);
        assert_eq!(merged, vec![motion(9, 3)]);
    }

    /// The barrier that matters: merging across a press would move the
    /// click to somewhere the user never clicked.
    #[test]
    fn motion_never_merges_across_a_button_transition() {
        let events = [
            motion(10, 0),
            press(PointerButton::Left),
            motion(20, 0),
            release(PointerButton::Left),
        ];
        assert_eq!(coalesce(&events), events.to_vec());
    }

    #[test]
    fn cancelling_motion_is_dropped_entirely() {
        assert!(coalesce(&[motion(5, 0), motion(-5, 0)]).is_empty());
        // But a click between them survives, because it happened.
        let events = [motion(5, 0), press(PointerButton::Left), motion(-5, 0)];
        assert_eq!(coalesce(&events), events.to_vec());
    }

    #[test]
    fn scroll_merges_separately_from_motion() {
        let events = [
            PointerEvent::Scroll { dx: 0, dy: 120 },
            PointerEvent::Scroll { dx: 0, dy: 120 },
            motion(1, 1),
            PointerEvent::Scroll { dx: 0, dy: -120 },
        ];
        assert_eq!(
            coalesce(&events),
            vec![
                PointerEvent::Scroll { dx: 0, dy: 240 },
                motion(1, 1),
                PointerEvent::Scroll { dx: 0, dy: -120 },
            ]
        );
    }

    fn any_event() -> impl Strategy<Value = PointerEvent> {
        prop_oneof![
            (-50i32..50, -50i32..50).prop_map(|(dx, dy)| PointerEvent::Motion { dx, dy }),
            (-50i32..50, -50i32..50).prop_map(|(dx, dy)| PointerEvent::Scroll { dx, dy }),
            (0usize..5, any::<bool>()).prop_map(|(i, pressed)| PointerEvent::Button {
                button: PointerButton::ALL[i],
                pressed,
            }),
        ]
    }

    proptest! {
        /// The invariant FR-4.4 exists for: whatever happened, releasing
        /// leaves nothing held. No event sequence can defeat it.
        #[test]
        fn release_all_always_clears(events in proptest::collection::vec(any_event(), 0..40)) {
            let mut state = InputState::new();
            state.apply_all(&events);
            let _ = state.release_all();
            prop_assert!(state.is_clear());
            prop_assert_eq!(state.pressed().count(), 0);
        }

        /// Coalescing is lossless in the only sense that matters: total
        /// displacement is preserved, so the remote pointer lands in the
        /// same place whether or not events were merged.
        #[test]
        fn coalescing_preserves_total_displacement(
            events in proptest::collection::vec(any_event(), 0..40),
        ) {
            let sum = |list: &[PointerEvent]| {
                list.iter().fold((0i64, 0i64), |(x, y), event| match event {
                    PointerEvent::Motion { dx, dy } => (x + i64::from(*dx), y + i64::from(*dy)),
                    _ => (x, y),
                })
            };
            prop_assert_eq!(sum(&events), sum(&coalesce(&events)));
        }

        /// Coalescing never reorders or drops a button transition: the
        /// destination sees the same gesture, merely with fewer motion
        /// events.
        #[test]
        fn coalescing_preserves_button_sequence(
            events in proptest::collection::vec(any_event(), 0..40),
        ) {
            let buttons = |list: &[PointerEvent]| {
                list.iter()
                    .filter(|event| matches!(event, PointerEvent::Button { .. }))
                    .copied()
                    .collect::<Vec<_>>()
            };
            prop_assert_eq!(buttons(&events), buttons(&coalesce(&events)));
        }

        /// Coalescing never makes a batch bigger.
        #[test]
        fn coalescing_never_grows(events in proptest::collection::vec(any_event(), 0..40)) {
            prop_assert!(coalesce(&events).len() <= events.len());
        }

        /// Applying a sequence then its coalesced form leaves identical
        /// button state — the destination cannot tell the difference.
        #[test]
        fn coalescing_preserves_final_button_state(
            events in proptest::collection::vec(any_event(), 0..40),
        ) {
            let mut direct = InputState::new();
            direct.apply_all(&events);
            let mut merged = InputState::new();
            merged.apply_all(&coalesce(&events));
            prop_assert_eq!(direct, merged);
        }
    }

    // ---- keyboard key-state (FR-4.3, FR-4.4; ADR 0008) ----

    #[test]
    fn state_tracks_key_presses_and_releases() {
        let mut state = InputState::new();
        assert!(state.is_clear());

        state.apply_key(&KeyEvent::press(hid::A));
        state.apply_key(&KeyEvent::press(hid::LEFT_CONTROL));
        assert!(state.is_key_held(hid::A));
        assert!(state.is_key_held(hid::LEFT_CONTROL));
        assert!(!state.is_key_held(hid::ENTER));
        assert!(!state.is_clear());

        state.apply_key(&KeyEvent::release(hid::A));
        assert!(!state.is_key_held(hid::A));
        assert!(!state.is_clear()); // Control still held

        state.apply_key(&KeyEvent::release(hid::LEFT_CONTROL));
        assert!(state.is_clear());
    }

    #[test]
    fn auto_repeat_does_not_double_track_a_held_key() {
        let mut state = InputState::new();
        state.apply_key(&KeyEvent::press(hid::A));
        // Several OS auto-repeats of the same held key.
        for _ in 0..5 {
            state.apply_key(&KeyEvent {
                key: hid::A,
                pressed: true,
                repeat: true,
                text: Some("a".to_owned()),
            });
        }
        // One release still fully clears it — a repeat is not a new press.
        let releases = state.release_all_keys();
        assert_eq!(releases, vec![KeyEvent::release(hid::A)]);
        assert!(state.is_clear());
    }

    #[test]
    fn text_and_repeat_do_not_affect_the_belief() {
        // Only the usage determines what is held; a press carrying text is
        // the same held key as one without.
        let mut with_text = InputState::new();
        with_text.apply_key(&KeyEvent {
            key: hid::A,
            pressed: true,
            repeat: false,
            text: Some("á".to_owned()),
        });
        let mut without = InputState::new();
        without.apply_key(&KeyEvent::press(hid::A));
        assert_eq!(with_text, without);
    }

    #[test]
    fn release_all_keys_is_ordered_by_usage_and_idempotent() {
        let mut state = InputState::new();
        // Press out of usage order; releases must still come out sorted.
        for key in [hid::LEFT_GUI, hid::A, hid::ENTER, hid::LEFT_SHIFT] {
            state.apply_key(&KeyEvent::press(key));
        }
        let releases = state.release_all_keys();
        assert_eq!(
            releases,
            vec![
                KeyEvent::release(hid::A),          // 0x04
                KeyEvent::release(hid::ENTER),      // 0x28
                KeyEvent::release(hid::LEFT_SHIFT), // 0xE1
                KeyEvent::release(hid::LEFT_GUI),   // 0xE3
            ],
            "releases must follow ascending usage order (NFR-2)"
        );
        // Nothing left; calling again is harmless.
        assert!(state.release_all_keys().is_empty());
        assert!(state.is_clear());
    }

    #[test]
    fn key_and_button_state_are_independent_and_both_gate_is_clear() {
        let mut state = InputState::new();
        state.apply(press(PointerButton::Left));
        state.apply_key(&KeyEvent::press(hid::LEFT_ALT));
        assert!(!state.is_clear());

        // Releasing only buttons leaves the key held (and vice versa).
        let _ = state.release_all();
        assert!(!state.is_pressed(PointerButton::Left));
        assert!(state.is_key_held(hid::LEFT_ALT));
        assert!(
            !state.is_clear(),
            "a held key must keep the state non-clear"
        );

        let _ = state.release_all_keys();
        assert!(state.is_clear());
    }

    fn any_key_event() -> impl Strategy<Value = KeyEvent> {
        // A small pool of usages (so presses and releases actually
        // collide), an arbitrary press/release, repeat, and optional text.
        (
            prop::sample::select(vec![
                hid::A,
                hid::ENTER,
                hid::ESCAPE,
                hid::LEFT_CONTROL,
                hid::LEFT_SHIFT,
                hid::RIGHT_ALT,
            ]),
            any::<bool>(),
            any::<bool>(),
            prop::option::of(prop::string::string_regex("[a-z]").unwrap()),
        )
            .prop_map(|(key, pressed, repeat, text)| KeyEvent {
                key,
                pressed,
                repeat,
                text,
            })
    }

    proptest! {
        /// FR-4.4 for the keyboard: whatever sequence of key transitions
        /// occurred, releasing leaves nothing held — no stuck key or
        /// modifier can survive a disconnect.
        #[test]
        fn release_all_keys_always_clears(
            events in proptest::collection::vec(any_key_event(), 0..40),
        ) {
            let mut state = InputState::new();
            state.apply_keys(&events);
            let releases = state.release_all_keys();
            prop_assert!(state.is_clear());
            prop_assert_eq!(state.keys_held().count(), 0);
            // Every release is a release (never a press), and unique.
            prop_assert!(releases.iter().all(|e| !e.pressed && e.text.is_none()));
            let mut usages: Vec<u16> = releases.iter().map(|e| e.key).collect();
            let len = usages.len();
            usages.dedup();
            prop_assert_eq!(usages.len(), len, "a key was released more than once");
        }

        /// Release output is exactly the set of keys whose last transition
        /// was a press — the belief matches the transitions applied.
        #[test]
        fn held_keys_are_those_last_pressed(
            events in proptest::collection::vec(any_key_event(), 0..40),
        ) {
            use std::collections::BTreeSet;
            let mut expected: BTreeSet<u16> = BTreeSet::new();
            for event in &events {
                if event.pressed {
                    expected.insert(event.key);
                } else {
                    expected.remove(&event.key);
                }
            }
            let mut state = InputState::new();
            state.apply_keys(&events);
            let held: BTreeSet<u16> = state.keys_held().collect();
            prop_assert_eq!(held, expected);
        }
    }
}
