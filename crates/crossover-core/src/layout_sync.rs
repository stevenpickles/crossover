//! Which of two competing arrangements a session converges on
//! ([ADR 0018](../../../docs/adr/0018-drawn-display-topology.md)'s "one
//! arrangement, two machines: sync and conflict").
//!
//! One layout describes both machines and either desk can edit it, so
//! ownership is not modelled at all — convergence is. This module is the
//! whole of that rule, as a pure function: [`resolve`] takes the layout
//! this machine currently holds and the one that just arrived and answers
//! which survives. Everything that *acts* on the answer — persisting,
//! publishing, reporting, the diagnostics — lives in the worker, so the
//! rule itself can be tested exhaustively without a session, a filesystem,
//! or a clock.
//!
//! # The rule
//!
//! - **Newest revision wins**, the ordering key being `(revision, origin)`
//!   compared lexicographically, `origin` as its sixteen raw bytes
//!   ([`crossover_topology::DeviceId`]'s own `Ord`). The origin breaks the
//!   tie between two edits that independently claimed the same revision,
//!   which is what a simultaneous edit at both desks produces.
//! - **Equal key, different content** should be impossible and is decided
//!   anyway: the layout with the lower **SHA-256** of its canonical
//!   encoding ([`canonical_bytes`]) wins. Both machines hash identical
//!   bytes because the monitor list is sorted by `(device, id)` first.
//!   ADR 0018 asks for that anomaly to be *logged*, and this module
//!   deliberately does not log it: this is a pure function on a frame
//!   path, and a peer able to make it fire once could make it fire
//!   forever. [`keys_tie`] lets the caller — which has a session to latch
//!   a warning against — say it once instead.
//! - **Equal key, equal content is [`Resolution::Identical`]** — not a
//!   win for either side, because there is nothing to adopt and nothing to
//!   supersede. That is the arm that keeps two synced machines silent:
//!   content-equality is a no-op everywhere, so neither answers a
//!   `LayoutSync` it already agrees with and no echo loop can start.
//!
//! # Why the answer is a three-way enum
//!
//! `bool` would collapse `KeepLocal` and `Identical`, and the worker needs
//! them apart: losing means *answering* with our own arrangement so the
//! peer adopts it, and agreeing means saying nothing at all. A run that
//! answered an identical layout would talk to a peer that would answer
//! back — the ping-pong ADR 0018's content-equality no-op exists to
//! prevent.
//!
//! # Saturation, not wrapping
//!
//! A revision is a bare `u64` here and the ordering is total over the whole
//! range, `u64::MAX` included. A peer asserting the ceiling pins both sides
//! there, after which the `(revision, origin)` tiebreak is fixed and one
//! machine's edits stop winning — visible, logged, and reachable only from
//! a peer already sending nonsense (ADR 0018's consequences). Nothing here
//! adds to a revision, so nothing here can wrap one.

use sha2::{Digest, Sha256};

use crossover_topology::{Layout, PlacedMonitor};

/// What [`resolve`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The arrangement that arrived wins: persist it, publish it, report
    /// it (ADR 0018's persist-publish-report order).
    AdoptReceived,
    /// The arrangement this machine holds wins. The worker answers with
    /// its own `LayoutSync` so the peer adopts, and logs the supersession
    /// naming both revisions and both origins (NFR-3).
    KeepLocal,
    /// The two are the same arrangement. Nothing happens — no write, no
    /// publication, no answer.
    Identical,
}

impl Resolution {
    /// A one-word form for a log field, so every diagnostic about a
    /// resolution names the outcome the same way.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AdoptReceived => "adopt_received",
            Self::KeepLocal => "keep_local",
            Self::Identical => "identical",
        }
    }
}

/// The lexicographic ordering key ADR 0018 fixes: the revision, then the
/// origin's sixteen raw bytes.
///
/// Returned as a tuple rather than compared inline so the two comparisons
/// in [`resolve`] cannot drift apart, and so a test can state the key
/// directly.
#[must_use]
fn ordering_key(layout: &Layout) -> (u64, [u8; crossover_topology::DEVICE_ID_BYTES]) {
    (layout.revision(), layout.origin().to_bytes())
}

/// The bytes both machines hash when the ordering key ties and the content
/// does not: the postcard encoding of the monitor list **sorted by
/// `(device, id)`**.
///
/// The sort is the whole point. Two machines that reached the same
/// arrangement by different edit histories can hold the same monitors in
/// different orders, and a hash over the wire order would then make the
/// tiebreak depend on which desk happened to list a screen first — a
/// tiebreak that disagrees with itself is worse than none. Sorted, the
/// bytes are a property of the arrangement rather than of its history.
///
/// The revision and the origin are deliberately **not** hashed: this is
/// only ever consulted when they are already equal, and ADR 0018 defines
/// the hash over the monitor list.
///
/// Infallible in practice — `postcard` serialization of a validated
/// [`Layout`]'s own monitors cannot fail — and an empty encoding on the
/// impossible failure, which hashes deterministically like anything else
/// and so keeps the comparison total rather than introducing a `Result`
/// that no caller could act on.
#[must_use]
pub fn canonical_bytes(layout: &Layout) -> Vec<u8> {
    let mut monitors: Vec<&PlacedMonitor> = layout.monitors().iter().collect();
    monitors.sort_by(|first, second| {
        first
            .device
            .cmp(&second.device)
            .then_with(|| first.id.cmp(&second.id))
    });
    postcard::to_stdvec(&monitors).unwrap_or_default()
}

/// The SHA-256 of [`canonical_bytes`] — the deterministic tiebreak of last
/// resort, and the value a collision diagnostic names.
#[must_use]
pub fn canonical_hash(layout: &Layout) -> [u8; 32] {
    hash(&canonical_bytes(layout))
}

/// The one place this crate says "SHA-256 of these bytes", so
/// [`canonical_hash`] and [`resolve_tied`] cannot come to disagree about
/// what the tiebreak is a hash *of*.
fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Do these two arrangements tie on the ordering key — same revision *and*
/// same origin?
///
/// Only ever true of a pair one machine could not have produced, which is
/// why it is worth a diagnostic. [`resolve`] does not emit that diagnostic
/// (see the module docs); this predicate is what lets a caller with a
/// session to latch against emit it once. Cheap: one tuple comparison, no
/// hashing.
#[must_use]
pub fn keys_tie(local: &Layout, received: &Layout) -> bool {
    ordering_key(local) == ordering_key(received)
}

/// Decide between the arrangement this machine holds and the one that
/// arrived (ADR 0018).
///
/// `local` is `None` when this machine holds no explicit layout at all, in
/// which case anything the peer drew wins: there is nothing to supersede,
/// and refusing would leave the pair permanently disagreed with no way for
/// either desk to fix it.
///
/// Pure and total: every pair of validated layouts produces an answer, with
/// no allocation on the common paths (the key comparison decides all but an
/// exact tie) and no panic on any input.
#[must_use]
pub fn resolve(local: Option<&Layout>, received: &Layout) -> Resolution {
    let Some(local) = local else {
        return Resolution::AdoptReceived;
    };
    match ordering_key(local).cmp(&ordering_key(received)) {
        std::cmp::Ordering::Greater => Resolution::KeepLocal,
        std::cmp::Ordering::Less => Resolution::AdoptReceived,
        std::cmp::Ordering::Equal => resolve_tied(local, received),
    }
}

/// The equal-key case: identical content is [`Resolution::Identical`],
/// and anything else is settled by the lower hash.
fn resolve_tied(local: &Layout, received: &Layout) -> Resolution {
    let local_bytes = canonical_bytes(local);
    let received_bytes = canonical_bytes(received);
    if local_bytes == received_bytes {
        return Resolution::Identical;
    }
    // ADR 0018 calls this the anomaly it is: two edits that claimed the
    // same revision *and* the same origin describe different desks, which
    // one machine cannot produce. It is still decided rather than left
    // open, because an undecided conflict is a pair that never converges.
    // Saying so is the caller's job — see the module docs and [`keys_tie`].
    if hash(&received_bytes) < hash(&local_bytes) {
        Resolution::AdoptReceived
    } else {
        // Equal hashes with unequal bytes is a SHA-256 collision — not
        // reachable, and keeping what we hold is the non-adopting
        // direction, which is the safe one to be wrong in.
        Resolution::KeepLocal
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crossover_topology::{DeviceId, DevicePair, Layout, LayoutRect, MonitorId, PlacedMonitor};

    use super::{Resolution, canonical_bytes, canonical_hash, keys_tie, resolve};

    const A: DeviceId = DeviceId::from_bytes([0x11; 16]);
    const B: DeviceId = DeviceId::from_bytes([0x22; 16]);

    fn pair() -> DevicePair {
        DevicePair::new(A, B).unwrap()
    }

    fn placed(device: DeviceId, id: &str, x: i32) -> PlacedMonitor {
        PlacedMonitor {
            device,
            id: MonitorId::new(id).unwrap(),
            rect: LayoutRect {
                x,
                y: 0,
                width: 100,
                height: 100,
            },
        }
    }

    /// An ordinary two-screen arrangement at `revision`, drawn by `origin`,
    /// with the peer's screen `gap` units to the right — so `gap` is the
    /// knob that makes two layouts differ in *content* alone.
    fn layout(revision: u64, origin: DeviceId, gap: i32) -> Layout {
        Layout::new(
            revision,
            origin,
            vec![placed(A, "A", 0), placed(B, "B", 100 + gap)],
            &pair(),
        )
        .unwrap()
    }

    #[test]
    fn nothing_local_adopts_whatever_arrived() {
        assert_eq!(
            resolve(None, &layout(0, A, 0)),
            Resolution::AdoptReceived,
            "a machine with no arrangement has nothing to supersede"
        );
        // Even revision 0, the lowest there is.
        assert_eq!(resolve(None, &layout(0, B, 0)), Resolution::AdoptReceived);
    }

    #[test]
    fn the_newer_revision_wins_whichever_side_holds_it() {
        assert_eq!(
            resolve(Some(&layout(3, A, 0)), &layout(5, B, 0)),
            Resolution::AdoptReceived
        );
        assert_eq!(
            resolve(Some(&layout(5, A, 0)), &layout(3, B, 0)),
            Resolution::KeepLocal
        );
        // The revision dominates the origin: a *lower* origin at a higher
        // revision still wins.
        assert_eq!(
            resolve(Some(&layout(5, B, 0)), &layout(6, A, 0)),
            Resolution::AdoptReceived
        );
    }

    #[test]
    fn an_equal_revision_is_broken_by_the_origin_bytes() {
        // A (0x11…) < B (0x22…), so B's edit wins the tie.
        assert_eq!(
            resolve(Some(&layout(4, A, 0)), &layout(4, B, 0)),
            Resolution::AdoptReceived
        );
        assert_eq!(
            resolve(Some(&layout(4, B, 0)), &layout(4, A, 0)),
            Resolution::KeepLocal
        );
    }

    /// The arm the whole no-echo property rests on.
    #[test]
    fn the_same_arrangement_from_both_ends_is_idle() {
        assert_eq!(
            resolve(Some(&layout(9, A, 0)), &layout(9, A, 0)),
            Resolution::Identical
        );
        // And it is content equality, not list order: the same monitors
        // listed the other way round are the same arrangement.
        let forwards = layout(9, A, 0);
        let backwards =
            Layout::new(9, A, vec![placed(B, "B", 100), placed(A, "A", 0)], &pair()).unwrap();
        assert_ne!(
            forwards, backwards,
            "the fixture no longer differs in order"
        );
        assert_eq!(
            resolve(Some(&forwards), &backwards),
            Resolution::Identical,
            "list order changed the answer; the canonical sort is not doing its job"
        );
    }

    /// Equal key, genuinely different content: decided by the hash, and
    /// both machines reach the same decision.
    #[test]
    fn an_equal_key_with_different_content_is_decided_by_the_lower_hash() {
        let first = layout(7, A, 0);
        let second = layout(7, A, 40);
        let lower_is_received = canonical_hash(&second) < canonical_hash(&first);

        let forwards = resolve(Some(&first), &second);
        let backwards = resolve(Some(&second), &first);
        assert_eq!(
            forwards,
            if lower_is_received {
                Resolution::AdoptReceived
            } else {
                Resolution::KeepLocal
            }
        );
        // Whichever way round it is asked, exactly one side adopts.
        assert_ne!(forwards, backwards);
        assert!(matches!(
            (forwards, backwards),
            (Resolution::AdoptReceived, Resolution::KeepLocal)
                | (Resolution::KeepLocal, Resolution::AdoptReceived)
        ));
    }

    /// The canonical bytes are a property of the arrangement, not of the
    /// order its monitors happen to be listed in — which is what lets two
    /// machines hash the same thing.
    #[test]
    fn the_canonical_encoding_is_order_independent() {
        let forwards = layout(1, A, 0);
        let backwards =
            Layout::new(1, A, vec![placed(B, "B", 100), placed(A, "A", 0)], &pair()).unwrap();
        assert_eq!(canonical_bytes(&forwards), canonical_bytes(&backwards));
        assert_eq!(canonical_hash(&forwards), canonical_hash(&backwards));
        // And it still separates arrangements that really differ.
        assert_ne!(canonical_hash(&forwards), canonical_hash(&layout(1, A, 40)));
    }

    /// The ceiling: `u64::MAX` is an ordinary revision to compare, and the
    /// origin tiebreak still resolves at it (ADR 0018's saturating-edit
    /// consequence).
    #[test]
    fn the_revision_ceiling_is_still_totally_ordered() {
        assert_eq!(
            resolve(Some(&layout(u64::MAX - 1, A, 0)), &layout(u64::MAX, A, 0)),
            Resolution::AdoptReceived
        );
        assert_eq!(
            resolve(Some(&layout(u64::MAX, A, 0)), &layout(u64::MAX - 1, B, 0)),
            Resolution::KeepLocal
        );
        // Both pinned at the ceiling: the origin decides, deterministically.
        assert_eq!(
            resolve(Some(&layout(u64::MAX, A, 0)), &layout(u64::MAX, B, 0)),
            Resolution::AdoptReceived
        );
        assert_eq!(
            resolve(Some(&layout(u64::MAX, B, 0)), &layout(u64::MAX, A, 0)),
            Resolution::KeepLocal
        );
        // And identical at the ceiling is still idle, not a conflict.
        assert_eq!(
            resolve(Some(&layout(u64::MAX, A, 0)), &layout(u64::MAX, A, 0)),
            Resolution::Identical
        );
    }

    /// [`keys_tie`] is the predicate the *caller* logs the anomaly from,
    /// so it has to answer exactly the question `resolve` asks before it
    /// reaches for a hash — and never one frame wider.
    #[test]
    fn keys_tie_is_true_for_exactly_the_hash_path() {
        // Same revision and same origin: tied, whatever the content.
        assert!(keys_tie(&layout(7, A, 0), &layout(7, A, 40)));
        assert!(keys_tie(&layout(7, A, 0), &layout(7, A, 0)));
        // A different revision or a different origin is not a tie.
        assert!(!keys_tie(&layout(7, A, 0), &layout(8, A, 0)));
        assert!(!keys_tie(&layout(7, A, 0), &layout(7, B, 0)));
    }

    #[test]
    fn every_outcome_has_a_log_label() {
        for resolution in [
            Resolution::AdoptReceived,
            Resolution::KeepLocal,
            Resolution::Identical,
        ] {
            assert!(!resolution.label().is_empty());
        }
    }

    proptest! {
        /// **Antisymmetry**: asked from both ends, the two machines agree
        /// on one winner. Never both adopting (which would swap the pair's
        /// arrangements forever) and never both keeping (which would leave
        /// them permanently disagreed).
        #[test]
        fn both_ends_agree_on_one_winner(
            first_revision in prop_oneof![Just(0u64), Just(u64::MAX), Just(u64::MAX - 1), any::<u64>()],
            second_revision in prop_oneof![Just(0u64), Just(u64::MAX), Just(u64::MAX - 1), any::<u64>()],
            first_origin in prop::sample::select(vec![A, B]),
            second_origin in prop::sample::select(vec![A, B]),
            first_gap in 0i32..500,
            second_gap in 0i32..500,
        ) {
            let first = layout(first_revision, first_origin, first_gap);
            let second = layout(second_revision, second_origin, second_gap);
            let forwards = resolve(Some(&first), &second);
            let backwards = resolve(Some(&second), &first);
            match forwards {
                Resolution::Identical => prop_assert_eq!(backwards, Resolution::Identical),
                Resolution::AdoptReceived => prop_assert_eq!(backwards, Resolution::KeepLocal),
                Resolution::KeepLocal => prop_assert_eq!(backwards, Resolution::AdoptReceived),
            }
        }

        /// **Idempotence**: a layout never supersedes itself, however
        /// extreme its revision — so re-receiving what is already in force
        /// is silent rather than a fresh adoption (and a fresh disk write).
        #[test]
        fn a_layout_never_supersedes_itself(
            revision in prop_oneof![Just(0u64), Just(u64::MAX), any::<u64>()],
            origin in prop::sample::select(vec![A, B]),
            gap in 0i32..500,
        ) {
            let same = layout(revision, origin, gap);
            prop_assert_eq!(resolve(Some(&same), &same), Resolution::Identical);
        }

        /// **Totality**: the answer is the ordering key's, exactly — no
        /// input reaches the hash path unless the keys tie.
        #[test]
        fn the_key_decides_whenever_it_differs(
            first_revision in any::<u64>(),
            second_revision in any::<u64>(),
            first_origin in prop::sample::select(vec![A, B]),
            second_origin in prop::sample::select(vec![A, B]),
        ) {
            let first = layout(first_revision, first_origin, 0);
            let second = layout(second_revision, second_origin, 60);
            let first_key = (first_revision, first_origin.to_bytes());
            let second_key = (second_revision, second_origin.to_bytes());
            prop_assume!(first_key != second_key);
            let expected = if first_key < second_key {
                Resolution::AdoptReceived
            } else {
                Resolution::KeepLocal
            };
            prop_assert_eq!(resolve(Some(&first), &second), expected);
        }
    }
}
