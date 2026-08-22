//! The display boundary and the geometry it speaks (ADR 0009).
//!
//! Seamless control transfer needs two facts from the OS: the size of the
//! desktop the cursor roams, and where the cursor is within it. Both come
//! through this trait, and the geometry *vocabulary* lives here — not in
//! `crossover-core` — for the same reason the input vocabulary does: the
//! trait must speak it and core cannot be a dependency of the trait that
//! describes it (docs/ARCHITECTURE.md §2). The *policy* — which edge
//! links to the peer, how a crossing maps to a fraction of the edge —
//! stays in core's topology model (ADR 0009).
//!
//! The reported region is the whole **virtual desktop** — every monitor,
//! as one rectangle — so the crossing edge is the outer edge of the
//! desktop, not a seam between two monitors (a primary-only region turns
//! the boundary between monitors into a false edge). Coordinates are
//! normalized to the desktop's top-left, so the cursor is always in
//! `0..width`×`0..height` and the topology model needs no origin.
//!
//! Since ADR 0018 a monitor also carries an **identity**: matching a live
//! screen to one the user drew in a layout needs something stabler than a
//! position in an enumeration ([`MonitorInfo`] says why an index would not
//! do).
//!
//! **Identity and geometry are separate queries, deliberately.**
//! [`DisplayInfo::monitors`] is the geometry enumeration and stays the
//! required method; [`DisplayInfo::monitor_layout`] adds identity on top
//! and is *defaulted* to "every rectangle, none of them named". The
//! separation is not tidiness — it is the safety property ADR 0018 states
//! as **an unknown id degrades placement, never geometry**:
//!
//! - **Both are hot.** Which one the edge detector polls every few
//!   milliseconds depends on the arrangement in force: `monitors()` for a
//!   side-model (geometry-only) crossing source, `monitor_layout()` for a
//!   drawn one, which matches live screens to drawn rectangles by device
//!   string (`crossover-core`'s `CrossingSource`). So neither may become
//!   expensive, and — the point of the split — neither may lose a monitor
//!   for want of a *name*. A monitor missing from either list would move
//!   the desktop's outer edge inward, turning an interior seam into a
//!   crossing edge: a false handoff, the release-blocking class of defect.
//!   That is why an unreadable id is `id: None` on a present entry and
//!   never a shorter list.
//! - An unnamed monitor costs only what the ADR says it should: the layout
//!   cannot address it, so a crossing onto it falls back to desktop-bounds
//!   placement with a diagnostic. Control correctness never depended on it.
//!
//! So a backend that can enumerate rectangles but cannot name them is a
//! first-class backend: it implements `monitors()`, inherits the default
//! `monitor_layout()`, and loses the placement nicety and nothing else.
//!
//! **A third query, for the same reason one more time.**
//! [`DisplayInfo::monitor_descriptions`] adds the *human* name — the EDID
//! product name (`DELL U2720Q`) a user reads off the bezel — on top of
//! `monitor_layout()`, and is defaulted the same way. It is a separate
//! method rather than a field on [`MonitorInfo`] because of where each
//! query is called from: `monitor_layout()` is on the edge detector's ~8 ms
//! poll and feeds its equality checks, while a friendly name costs a
//! `QueryDisplayConfig` sweep on Windows — orders of magnitude more
//! expensive, and a value that has no business changing what the detector
//! considers "the same layout". Only the ~1 s topology-sync side asks for
//! descriptions.
//!
//! Since ADR 0018's 2026-08-22 amendment that third query also carries a
//! monitor's **physical size** ([`PhysicalSizeMm`]) — and it rides the same
//! method rather than earning a fourth, because it is acquired by the same
//! sweep at the same cadence and is display-only for the same reason a
//! label is.

use thiserror::Error;

/// The virtual desktop's pixel size — all monitors as one rectangle, its
/// origin normalized to the top-left (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    /// Width in pixels across every monitor.
    pub width: u32,
    /// Height in pixels across every monitor.
    pub height: u32,
}

/// A cursor position in the virtual desktop's pixel space, normalized to
/// its top-left origin (so it lies within [`Screen`]). Signed so a
/// coordinate at or just past an edge is representable without wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPoint {
    /// Rightward pixels from the desktop's left edge.
    pub x: i32,
    /// Downward pixels from the desktop's top edge.
    pub y: i32,
}

/// One monitor's bounds, normalized to the virtual desktop's top-left
/// origin (like [`CursorPoint`]). Crossing maps the edge fraction against
/// the specific monitor on the crossing edge — not the whole bounding-box
/// desktop — so monitors of different resolution, and the dead space
/// between mismatched ones, map correctly (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    /// Left pixel (desktop-relative).
    pub left: i32,
    /// Top pixel (desktop-relative).
    pub top: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// One monitor's bounds together with the identity a drawn layout
/// addresses it by, when the platform could supply one (ADR 0018).
///
/// The id is the **platform-supplied device string** — on Windows,
/// `GetMonitorInfoW`'s `szDevice` (`\\.\DISPLAY1` and friends) — and it is
/// what makes a saved arrangement survive a reboot. An enumeration index is
/// positional: unplug a monitor and index 1 silently becomes a different
/// screen, so a layout drawn against indices would be wrong in the way that
/// is hardest to see. A device string that *does* change leaves the monitor
/// simply unknown, which is observable.
///
/// `id` is an `Option` for exactly that reason, and the `None` case is a
/// state to represent rather than a failure to hide. It means *this
/// rectangle is real and the platform would not name it* — a monitor the
/// user can see, that edge detection must keep using, and that a layout
/// cannot address. Never a fabricated or positional stand-in: a made-up id
/// is worse than none, because after a re-enumeration it would confidently
/// match the wrong screen.
///
/// The id is a plain `String`, unvalidated, because that is the honest
/// shape of "whatever the OS said". The bound and the charset rule
/// (`MAX_MONITOR_ID_BYTES`, printable ASCII) belong to the layout model in
/// `crossover-topology`, which this crate must not depend on: a platform
/// trait that could not report what the OS actually returned would have no
/// way to say that a machine's display configuration is unusable.
///
/// A stable per-monitor identifier is thereby a requirement on the future
/// macOS and Linux backends too (ADR 0018, recorded ahead of Phase 9) — but
/// a *soft* one: a backend without it still serves geometry (see this
/// module's header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// The platform's device string for this monitor, or `None` if it
    /// could not be read.
    pub id: Option<String>,
    /// Its bounds, normalized to the desktop origin, exactly as
    /// [`MonitorRect`] describes.
    pub rect: MonitorRect,
}

/// One monitor's geometry and identity, plus the human-readable name the
/// platform advertises for it when it has one (ADR 0018, amended
/// 2026-08-21).
///
/// The label is the monitor's human-readable name — `DELL U2720Q`, from
/// its EDID, and usually the string the OS's own display settings show for
/// it. A backend may substitute a name of its own where a panel's EDID
/// carries none (a laptop's built-in display is the usual case, and the
/// Windows backend does exactly that). It is **display only**: optional,
/// not unique, and never a key. Two identical screens on one desk report the same label,
/// which is legal and is the editor's problem to caption, not this trait's
/// to disambiguate. Identity remains [`MonitorInfo::id`] and nothing else.
///
/// `label` is a plain `String`, unvalidated, for exactly the reason
/// [`MonitorInfo::id`] is: this is "whatever the OS said". The bound and
/// the character rule (`MAX_MONITOR_LABEL_BYTES`, no control characters)
/// belong to `crossover-topology`, which this crate must not depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDescription {
    /// The geometry and identity [`DisplayInfo::monitor_layout`] reports
    /// for this monitor, unchanged.
    pub info: MonitorInfo,
    /// Its advertised human-readable name, or `None` where the platform
    /// has none, could not read one, or reported an empty one.
    pub label: Option<String>,
    /// How large the panel physically is, or `None` where the platform
    /// could not measure it or does not believe what it measured
    /// ([`PhysicalSizeMm`] says what a backend owes here).
    pub physical_size: Option<PhysicalSizeMm>,
}

/// A monitor's physical panel size in millimetres, as a backend measured it
/// (ADR 0018, amended 2026-08-22).
///
/// **Proportion only, never identity, and never geometry.** It exists so
/// the layout editor can draw a rectangle at a size proportional to the
/// real screen, which is what makes a crossing physically continuous across
/// the seam between two desks: a cursor leaving one machine a third of the
/// way up a bezel should arrive a third of the way up the other's. Nothing
/// about control transfer consults it — the id remains the only key, and
/// the pixel rectangle in [`MonitorRect`] remains the only geometry.
///
/// It is an `Option` on [`MonitorDescription`] because most reasons a
/// platform cannot supply one are ordinary rather than exceptional: a
/// virtual display has no panel to measure, a remote session's screen is
/// somebody else's, and the EDID a real panel carries can be absent,
/// unreadable, or wrong. A backend that declines to measure is a working
/// backend; the editor draws such a monitor the way it drew every monitor
/// before sizes existed.
///
/// **A backend that is unsure reports `None`, not a guess.** This is the
/// one rule the type imposes on its producers, and it is the opposite of
/// the usual best-effort instinct: a size drawn confidently and wrongly
/// misplaces every rectangle beside it, while a size withheld costs only
/// the improvement. Projectors, TVs, and virtual displays all report
/// numbers that are fiction, so the Windows backend applies a plausibility
/// gate before it will claim one at all.
///
/// The fields are plain `u16`, unvalidated, for exactly the reason
/// [`MonitorInfo::id`] and [`MonitorDescription::label`] are plain
/// `String`s: this is "whatever the platform measured". The bound
/// (`MAX_PHYSICAL_SIZE_MM`) belongs to `crossover-topology`, which this
/// crate must not depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalSizeMm {
    /// The panel's width in millimetres.
    pub width_mm: u16,
    /// The panel's height in millimetres.
    pub height_mm: u16,
}

/// Failures from a [`DisplayInfo`] backend.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum DisplayError {
    /// The platform could not report display geometry or the cursor.
    ///
    /// `reason` is diagnostic text for logs (FR-7.3).
    #[error("display query failed: {reason}")]
    Unavailable {
        /// Diagnostic detail.
        reason: String,
    },
}

/// Read-only access to the local virtual-desktop geometry and cursor
/// (ADR 0009).
///
/// The size and the cursor come from the same process and the same
/// (normalized) coordinate space, so edge detection compares like with
/// like. The process is expected to be per-monitor DPI aware (R-3) so the
/// numbers are real pixels; cross-machine mapping never uses these pixels
/// directly — it goes through the fraction in core's topology model.
pub trait DisplayInfo: Send + Sync {
    /// The virtual desktop's pixel size (all monitors as one rectangle).
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot report the
    /// desktop geometry.
    fn desktop_bounds(&self) -> Result<Screen, DisplayError>;

    /// Every monitor's bounds, normalized to the desktop origin, so the
    /// crossing edge can be mapped against the actual monitor on it rather
    /// than the bounding box (ADR 0009). At least one on any real display.
    ///
    /// **Geometry only, and required.** This is the query edge detection
    /// polls, and it must never be able to lose a monitor because the
    /// platform could not *name* one — see this module's header for what a
    /// short list would cost. A backend implements this whether or not it
    /// can identify anything.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot enumerate the
    /// monitors.
    fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError>;

    /// Every monitor, with the device string a drawn layout addresses it
    /// by where the platform supplies one (ADR 0018).
    ///
    /// The list holds **the same rectangles [`DisplayInfo::monitors`]
    /// reports, always** — identity is added per monitor, best effort, and
    /// its absence shows up as `MonitorInfo::id == None` rather than as a
    /// missing entry.
    ///
    /// **This is a hot query whenever a drawn layout is in force**: the
    /// edge detector polls it every few milliseconds to match live screens
    /// to drawn rectangles by device string. It must therefore stay about
    /// as cheap as [`DisplayInfo::monitors`] — anything more expensive
    /// belongs behind [`DisplayInfo::monitor_descriptions`] instead.
    ///
    /// Defaulted to the geometry enumeration with nothing named, so a
    /// backend with no stable per-monitor identifier is still a working
    /// backend: it loses layout-directed cursor placement, which ADR 0018
    /// treats as advisory, and keeps everything else.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot enumerate the
    /// monitors. Failing to *identify* one is not an error.
    fn monitor_layout(&self) -> Result<Vec<MonitorInfo>, DisplayError> {
        Ok(self
            .monitors()?
            .into_iter()
            .map(|rect| MonitorInfo { id: None, rect })
            .collect())
    }

    /// Every monitor, with its identity *and* the descriptive facts the
    /// platform advertises about it: the human-readable name (ADR 0018,
    /// amended 2026-08-21) and the panel's physical size (amended
    /// 2026-08-22).
    ///
    /// The list holds **the same entries [`DisplayInfo::monitor_layout`]
    /// reports, in the same order, always** — a label and a size are added
    /// per monitor, best effort, and the absence of either shows up as a
    /// `None` on a present entry rather than as a missing or reordered one.
    ///
    /// **Not on the hot path, and that is the point.** This is the query
    /// the ~1 s topology poll uses to fill the state file and the peer's
    /// `MonitorTopology`; edge detection keeps calling `monitors()` every
    /// few milliseconds and never comes here. A backend is free to make
    /// this the expensive one (on Windows it is a whole
    /// `QueryDisplayConfig` sweep).
    ///
    /// Defaulted to the identified enumeration with nothing described, so a
    /// backend with no way to ask for product names or panel sizes is still
    /// a working backend: its monitors caption by device string and draw by
    /// pixel count, which is what the editor did before either existed.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot enumerate the
    /// monitors. Failing to *describe* one is not an error.
    fn monitor_descriptions(&self) -> Result<Vec<MonitorDescription>, DisplayError> {
        Ok(self
            .monitor_layout()?
            .into_iter()
            .map(|info| MonitorDescription {
                info,
                label: None,
                physical_size: None,
            })
            .collect())
    }

    /// The cursor's current position, normalized to the virtual desktop's
    /// top-left origin.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Unavailable`] if the platform cannot report the
    /// cursor position.
    fn cursor_position(&self) -> Result<CursorPoint, DisplayError>;
}

#[cfg(test)]
mod tests {
    use super::{CursorPoint, DisplayError, DisplayInfo, MonitorInfo, MonitorRect, Screen};

    /// The minimum a backend can be: geometry and a cursor, no identity, no
    /// labels. Both defaulted methods are inherited, which is the property
    /// under test — a port that can only enumerate rectangles still
    /// compiles and still answers every query.
    struct GeometryOnly;

    fn rect(left: i32) -> MonitorRect {
        MonitorRect {
            left,
            top: 0,
            width: 100,
            height: 100,
        }
    }

    impl DisplayInfo for GeometryOnly {
        fn desktop_bounds(&self) -> Result<Screen, DisplayError> {
            Ok(Screen {
                width: 200,
                height: 100,
            })
        }

        fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError> {
            Ok(vec![rect(0), rect(100)])
        }

        fn cursor_position(&self) -> Result<CursorPoint, DisplayError> {
            Ok(CursorPoint { x: 0, y: 0 })
        }
    }

    /// A backend that names nothing describes nothing — but it never loses
    /// or reorders a monitor doing so. An absent label or size is `None` on
    /// a present entry, exactly as an absent id is.
    #[test]
    fn the_default_description_query_labels_nothing_and_drops_nothing() {
        let display = GeometryOnly;
        let descriptions = display.monitor_descriptions().unwrap();

        assert_eq!(
            descriptions
                .iter()
                .map(|description| description.info.rect)
                .collect::<Vec<_>>(),
            display.monitors().unwrap(),
            "the default description query lost or reordered a monitor"
        );
        assert!(
            descriptions
                .iter()
                .all(|description| description.label.is_none())
        );
        assert!(
            descriptions
                .iter()
                .all(|description| description.physical_size.is_none())
        );
        assert!(
            descriptions
                .iter()
                .all(|description| description.info.id.is_none())
        );
    }

    /// The defaulted layer sits on `monitor_layout()`, so a backend that
    /// *can* identify a monitor keeps that identity in its descriptions
    /// even when it cannot label one.
    #[test]
    fn the_default_description_query_keeps_whatever_identity_exists() {
        struct Identified;

        impl DisplayInfo for Identified {
            fn desktop_bounds(&self) -> Result<Screen, DisplayError> {
                Ok(Screen {
                    width: 100,
                    height: 100,
                })
            }

            fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError> {
                Ok(vec![rect(0)])
            }

            fn monitor_layout(&self) -> Result<Vec<MonitorInfo>, DisplayError> {
                Ok(vec![MonitorInfo {
                    id: Some("SCREEN-1".to_owned()),
                    rect: rect(0),
                }])
            }

            fn cursor_position(&self) -> Result<CursorPoint, DisplayError> {
                Ok(CursorPoint { x: 0, y: 0 })
            }
        }

        let descriptions = Identified.monitor_descriptions().unwrap();
        assert_eq!(descriptions.len(), 1);
        assert_eq!(descriptions[0].info.id.as_deref(), Some("SCREEN-1"));
        assert_eq!(descriptions[0].label, None);
        assert_eq!(descriptions[0].physical_size, None);
    }

    /// An enumeration failure is still a failure: a defaulted method must
    /// propagate it rather than reporting an empty desk.
    #[test]
    fn an_enumeration_failure_propagates_through_the_default() {
        struct Broken;

        impl DisplayInfo for Broken {
            fn desktop_bounds(&self) -> Result<Screen, DisplayError> {
                Err(DisplayError::Unavailable {
                    reason: "no session".to_owned(),
                })
            }

            fn monitors(&self) -> Result<Vec<MonitorRect>, DisplayError> {
                Err(DisplayError::Unavailable {
                    reason: "no session".to_owned(),
                })
            }

            fn cursor_position(&self) -> Result<CursorPoint, DisplayError> {
                Err(DisplayError::Unavailable {
                    reason: "no session".to_owned(),
                })
            }
        }

        assert!(Broken.monitor_descriptions().is_err());
    }
}
