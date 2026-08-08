//! Platform-neutral input model (FR-4.x, docs/SPECIFICATION.md §4.4).
//!
//! Phase 3 covers the pointer; the keyboard joins in Phase 4. The
//! structure anticipates it: [`InputState`] tracks *what this machine
//! believes is held down on the destination*, and
//! [`InputState::release_all`] synthesizes the releases for all of it —
//! the mechanism FR-4.4 requires and the one that keeps a disconnect
//! from leaving a stuck button.
//!
//! Motion is **relative** (ADR 0007: Raw Input reports unaccelerated,
//! unclamped deltas), which makes coalescing exact rather than lossy:
//! merging two movements is addition, where merging absolute positions
//! would mean discarding one.
//!
//! The event *vocabulary* ([`PointerEvent`], [`PointerButton`]) lives in
//! `crossover-platform`, because the HAL traits must speak it and cannot
//! depend on this crate (docs/ARCHITECTURE.md §2). What lives here is
//! *policy*: what is believed held, and what may be merged.

pub use crossover_platform::{PointerButton, PointerEvent, SCROLL_UNITS_PER_DETENT};

/// What this machine believes is currently held down on the destination.
///
/// "Believes" is the operative word (FR-4.3): the state is maintained
/// from what was *sent*, so that when a session dies mid-gesture there
/// is a record of what needs releasing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputState {
    buttons: [bool; 5],
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

    /// Nothing is held — the state a session may safely end in.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        !self.buttons.iter().any(|down| *down)
    }

    /// `ReleaseAllInput` (FR-4.4): the release events needed to leave the
    /// destination holding nothing, and clear the belief.
    ///
    /// Called on disconnect, session termination, fatal protocol failure,
    /// and control reset. A stuck button after any of those is a
    /// release-blocking defect, so this is deliberately total: it emits a
    /// release for everything believed held, and afterwards
    /// [`InputState::is_clear`] is unconditionally true.
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

    use super::{InputState, PointerButton, PointerEvent, coalesce};

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
}
