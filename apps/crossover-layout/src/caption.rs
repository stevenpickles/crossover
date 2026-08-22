//! What a monitor rectangle is called on screen — pure, and separate from
//! the painting that shows it.
//!
//! The editor's split (see [`crate::render`]) is that egui code is a thin
//! projection over tested pure functions. Captioning earns its own module
//! because it is the one piece of the drawing that has a *rule* rather than
//! a value: the name shown is not a field, it is derived from two fields
//! and from the other monitors on the same machine.
//!
//! # The rule
//!
//! - **A product name if there is one, the device string otherwise.**
//!   `DELL U2720Q` is what the user reads off the bezel; `\\.\DISPLAY1` is
//!   what they can match against a diagnostic. The first is a better
//!   caption and the second is always available, so the fallback chain is
//!   label → id and there is no third case: a monitor always has an id.
//! - **Duplicates are numbered, Windows-Settings style.** Two identical
//!   screens report the same product name, so captioning both `DELL U2720Q`
//!   would leave the user unable to say which rectangle is which — which is
//!   the entire problem labels were added to solve, reintroduced. The
//!   second and subsequent copies therefore become `DELL U2720Q (1)`,
//!   `DELL U2720Q (2)`, … in enumeration order.
//! - **Only within one machine.** Both desks legitimately own a
//!   `DELL U2720Q`, they are drawn in separate groups under their own
//!   machine names, and suffixing across the pair would number screens the
//!   user has no reason to think of as a set.
//! - **Ids are never suffixed.** They are unique within a machine by the
//!   layout model's own rule, so a suffix could only ever be noise.
//!
//! Numbering starts at `(1)` on the *first* duplicate rather than the
//! second, so a pair reads `DELL U2720Q (1)` / `DELL U2720Q (2)` — matching
//! Windows Settings, and avoiding the worse alternative where one screen of
//! an identical pair is unnumbered and looks like the "real" one.

use crossover_topology::{MonitorId, MonitorLabel};

/// Everything captioning needs about one monitor, and nothing else.
///
/// Deliberately not [`crate::model::DrawnMonitor`]: the rule reads four
/// values, and taking exactly those makes the caption tests state their
/// inputs rather than build whole layout rectangles to reach them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionInput<'a> {
    /// Its platform-supplied identity — the fallback, always present.
    pub id: &'a MonitorId,
    /// Its product name, where its owning machine's platform had one.
    pub label: Option<&'a MonitorLabel>,
    /// Its 1-based position within its machine's group.
    pub ordinal: usize,
    /// Its live pixel size, where the machine currently reports it as
    /// attached.
    pub native_size: Option<(u32, u32)>,
}

/// The display names for one machine's monitors, in the order given.
///
/// One call per machine, one output per input, positionally aligned — so
/// the caller never has to match names back to monitors, and a duplicate
/// cannot be resolved differently on two passes.
#[must_use]
pub fn display_names(monitors: &[CaptionInput<'_>]) -> Vec<String> {
    let mut names: Vec<String> = monitors
        .iter()
        .map(|monitor| match monitor.label {
            Some(label) => label.as_str().to_owned(),
            None => monitor.id.as_str().to_owned(),
        })
        .collect();

    // Suffix only names that came from a *label* and repeat. An id is
    // unique within a machine by the layout model's own rule, so a
    // repeated one would be a model violation rather than a caption
    // problem, and numbering it would hide that rather than help.
    for index in 0..names.len() {
        let Some(label) = monitors[index].label else {
            continue;
        };
        let shares = |candidate: &&CaptionInput<'_>| candidate.label == Some(label);
        if monitors.iter().filter(shares).count() < 2 {
            continue;
        }
        // How many monitors before this one carry the same name — so the
        // first copy is `(1)`, not `(0)` and not unnumbered.
        let position = monitors[..index].iter().filter(shares).count() + 1;
        names[index] = format!("{} ({position})", label.as_str());
    }

    names
}

/// The full caption painted inside one monitor's rectangle: its ordinal,
/// its display name, and — where the machine reports it attached — its
/// native pixel size on a second line.
///
/// The ordinal and the resolution are what the editor showed before
/// product names existed and still show: the ordinal because a rectangle
/// too small for any name still has room for a digit, and the resolution
/// because it is how a user tells two same-named screens apart when they
/// are *not* actually identical.
#[must_use]
pub fn caption(monitor: &CaptionInput<'_>, display_name: &str) -> String {
    match monitor.native_size {
        Some((width, height)) => {
            format!("{} · {display_name}\n{width}×{height}", monitor.ordinal)
        }
        None => format!("{} · {display_name}", monitor.ordinal),
    }
}

/// [`display_names`] and [`caption`] together: every monitor's finished
/// caption, in the order given.
#[must_use]
pub fn captions(monitors: &[CaptionInput<'_>]) -> Vec<String> {
    display_names(monitors)
        .iter()
        .zip(monitors)
        .map(|(name, monitor)| caption(monitor, name))
        .collect()
}

#[cfg(test)]
mod tests {
    use crossover_topology::{MonitorId, MonitorLabel};

    use super::{CaptionInput, caption, captions, display_names};

    fn id(text: &str) -> MonitorId {
        MonitorId::new(text).unwrap()
    }

    fn label(text: &str) -> MonitorLabel {
        MonitorLabel::new(text).unwrap()
    }

    fn monitor<'a>(
        id: &'a MonitorId,
        label: Option<&'a MonitorLabel>,
        ordinal: usize,
    ) -> CaptionInput<'a> {
        CaptionInput {
            id,
            label,
            ordinal,
            native_size: Some((1920, 1080)),
        }
    }

    #[test]
    fn a_product_name_wins_over_the_device_string() {
        let one = id(r"\\.\DISPLAY1");
        let name = label("DELL U2720Q");
        assert_eq!(
            display_names(&[monitor(&one, Some(&name), 1)]),
            vec!["DELL U2720Q"]
        );
    }

    #[test]
    fn a_monitor_with_no_product_name_falls_back_to_its_device_string() {
        let one = id(r"\\.\DISPLAY1");
        assert_eq!(
            display_names(&[monitor(&one, None, 1)]),
            vec![r"\\.\DISPLAY1"]
        );
    }

    /// The case the whole module exists for: two identical screens report
    /// one name, and captioning both the same would leave the user unable
    /// to say which rectangle is which.
    #[test]
    fn duplicate_product_names_are_numbered_in_enumeration_order() {
        let (one, two) = (id("A"), id("B"));
        let name = label("DELL U2720Q");
        assert_eq!(
            display_names(&[monitor(&one, Some(&name), 1), monitor(&two, Some(&name), 2),]),
            vec!["DELL U2720Q (1)", "DELL U2720Q (2)"]
        );
    }

    /// Three of them, and the numbering keeps counting rather than
    /// restarting or repeating.
    #[test]
    fn three_identical_screens_number_one_two_three() {
        let (one, two, three) = (id("A"), id("B"), id("C"));
        let name = label("LG ULTRAGEAR");
        assert_eq!(
            display_names(&[
                monitor(&one, Some(&name), 1),
                monitor(&two, Some(&name), 2),
                monitor(&three, Some(&name), 3),
            ]),
            vec!["LG ULTRAGEAR (1)", "LG ULTRAGEAR (2)", "LG ULTRAGEAR (3)"]
        );
    }

    /// A name that appears once is left alone even when *another* name on
    /// the same machine is duplicated — the suffix marks ambiguity, not
    /// the presence of ambiguity somewhere in the group.
    #[test]
    fn a_unique_name_is_never_suffixed() {
        let (one, two, three) = (id("A"), id("B"), id("C"));
        let repeated = label("DELL U2720Q");
        let alone = label("LG ULTRAGEAR");
        assert_eq!(
            display_names(&[
                monitor(&one, Some(&repeated), 1),
                monitor(&two, Some(&alone), 2),
                monitor(&three, Some(&repeated), 3),
            ]),
            vec!["DELL U2720Q (1)", "LG ULTRAGEAR", "DELL U2720Q (2)"]
        );
    }

    /// Device strings are unique within a machine by the layout model's own
    /// rule, so a fallback caption is never suffixed — and a labelled
    /// monitor sitting beside unlabelled ones does not make them ambiguous.
    #[test]
    fn device_string_fallbacks_are_never_numbered() {
        let (one, two) = (id(r"\\.\DISPLAY1"), id(r"\\.\DISPLAY2"));
        let name = label("DELL U2720Q");
        assert_eq!(
            display_names(&[monitor(&one, None, 1), monitor(&two, Some(&name), 2)]),
            vec![r"\\.\DISPLAY1", "DELL U2720Q"]
        );
    }

    /// A label that happens to equal another monitor's *id* is left alone:
    /// the two are different kinds of value, only one of them is a key, and
    /// numbering across them would be numbering a coincidence.
    #[test]
    fn a_label_colliding_with_another_monitors_id_is_left_alone() {
        let (one, two) = (id("SCREEN"), id("OTHER"));
        let name = label("SCREEN");
        assert_eq!(
            display_names(&[monitor(&one, None, 1), monitor(&two, Some(&name), 2)]),
            vec!["SCREEN", "SCREEN"]
        );
    }

    /// Numbering stops at the machine boundary, which is a property of
    /// *how this is called* — once per machine — rather than of the rule
    /// inside it. Worth an automated test anyway: both desks legitimately
    /// own a `DELL U2720Q`, they are drawn in separate groups under their
    /// own machine names, and suffixing across the pair would number
    /// screens the user has no reason to think of as a set.
    ///
    /// A regression here would most likely arrive as "compute captions
    /// once for the whole scene", which reads as a tidy-up and is not one.
    #[test]
    fn identical_screens_on_different_machines_are_not_numbered_together() {
        let (local, peer) = (id("A"), id("B"));
        let name = label("DELL U2720Q");

        // Each machine's group is captioned on its own, which is the
        // contract `render`'s `paint_group` keeps.
        let local_names = display_names(&[monitor(&local, Some(&name), 1)]);
        let peer_names = display_names(&[monitor(&peer, Some(&name), 1)]);

        assert_eq!(local_names, vec!["DELL U2720Q"]);
        assert_eq!(peer_names, vec!["DELL U2720Q"]);
        assert!(
            !local_names[0].contains('('),
            "a lone screen was numbered against the *other* machine's"
        );
    }

    #[test]
    fn an_empty_group_captions_nothing() {
        assert!(display_names(&[]).is_empty());
        assert!(captions(&[]).is_empty());
    }

    /// The secondary information the editor showed before labels existed
    /// is still shown: the ordinal always, the resolution when the machine
    /// reports the monitor attached.
    #[test]
    fn a_caption_keeps_the_ordinal_and_the_resolution() {
        let one = id(r"\\.\DISPLAY1");
        let name = label("DELL U2720Q");
        assert_eq!(
            caption(&monitor(&one, Some(&name), 2), "DELL U2720Q"),
            "2 · DELL U2720Q\n1920×1080"
        );

        // Placed but not currently attached: no resolution to show, and
        // the caption is still complete.
        let unplugged = CaptionInput {
            id: &one,
            label: None,
            ordinal: 3,
            native_size: None,
        };
        assert_eq!(caption(&unplugged, r"\\.\DISPLAY1"), r"3 · \\.\DISPLAY1");
    }

    /// The two halves compose positionally: one caption per monitor, in
    /// the order given, with the suffixes the group rule produced.
    #[test]
    fn captions_pair_each_name_with_its_own_monitor() {
        let (one, two) = (id("A"), id("B"));
        let name = label("DELL U2720Q");
        assert_eq!(
            captions(&[monitor(&one, Some(&name), 1), monitor(&two, Some(&name), 2),]),
            vec![
                "1 · DELL U2720Q (1)\n1920×1080",
                "2 · DELL U2720Q (2)\n1920×1080"
            ]
        );
    }
}
