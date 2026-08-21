//! Screen ↔ layout-space transform (pure), and fitting it to a canvas.
//!
//! The model draws in ADR 0018's unit-agnostic layout space; the canvas
//! draws in screen pixels. [`Viewport`] is the one linear map between them,
//! recomputed by [`Viewport::fit`] whenever the canvas resizes or the
//! model's bounds change — never accumulated or animated, so there is no
//! drift to test for.

/// The bounding box of some layout-space content, in the same
/// unit-agnostic space [`crossover_topology::LayoutRect`] uses. Kept as
/// `f64` here (rather than the model's `i32`/`u32`) because [`Viewport::fit`]
/// does floating-point division to compute a scale, and every corner this
/// module touches goes through that division anyway.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutBounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl LayoutBounds {
    /// The bounds of a single point — the identity for [`LayoutBounds::union`].
    #[must_use]
    pub fn point(x: f64, y: f64) -> Self {
        Self {
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
        }
    }

    /// The smallest bounds containing both.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    #[must_use]
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    #[must_use]
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
}

/// The linear map from layout space to screen space: `screen = layout *
/// scale + offset`, applied identically on both axes (no independent x/y
/// scale — a drawn arrangement must not stretch).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Screen pixels per layout unit. Always positive and finite.
    pub scale: f32,
    /// Screen-space translation applied after scaling.
    pub offset: (f32, f32),
}

impl Viewport {
    /// Layout space to screen space.
    ///
    /// Layout coordinates are drawn, not measured (ADR 0018): every one
    /// this crate produces is bounded well inside `f32`'s exact-integer
    /// range (`crossover_topology::MAX_LAYOUT_COORDINATE` is `2^24`, `f32`'s
    /// mantissa is 24 bits), so narrowing to screen-space `f32` here costs
    /// at most a fraction of a pixel — nothing a person arranging monitors
    /// on a canvas could perceive.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_screen(self, point: (f64, f64)) -> (f32, f32) {
        (
            point.0 as f32 * self.scale + self.offset.0,
            point.1 as f32 * self.scale + self.offset.1,
        )
    }

    /// Screen space to layout space — the exact inverse of [`Self::to_screen`]
    /// for a `scale` that is nonzero and finite, which [`Viewport::fit`]
    /// always produces.
    ///
    /// Unused outside this module's own round-trip proptest today — this
    /// branch is read-and-render only — but it is the exact seam
    /// hit-testing and dragging call in the next branch, and the inverse
    /// half of a transform is worth keeping (and testing) beside its
    /// forward half rather than adding it only once something calls it.
    #[must_use]
    #[allow(dead_code)]
    pub fn to_layout(self, point: (f32, f32)) -> (f64, f64) {
        (
            f64::from((point.0 - self.offset.0) / self.scale),
            f64::from((point.1 - self.offset.1) / self.scale),
        )
    }

    /// The viewport that fits `bounds` inside `canvas` with `padding` screen
    /// pixels of margin on every side, preserving aspect ratio and
    /// centering the content in the remaining space.
    ///
    /// Degenerate inputs (a zero-area canvas, or bounds with no extent —
    /// one monitor, or none) never produce a zero or non-finite scale: both
    /// dimensions are floored so a resize mid-drag or an editor with one
    /// tiny monitor still yields a usable, invertible viewport rather than
    /// a division by zero.
    #[must_use]
    // See `to_screen`: layout-space extents stay well inside `f32`'s exact
    // range, so narrowing here is a display-precision choice, not a
    // correctness one.
    #[allow(clippy::cast_possible_truncation)]
    pub fn fit(bounds: LayoutBounds, canvas: (f32, f32), padding: f32) -> Self {
        let available_w = (canvas.0 - 2.0 * padding).max(1.0);
        let available_h = (canvas.1 - 2.0 * padding).max(1.0);
        let content_w = bounds.width().max(1.0) as f32;
        let content_h = bounds.height().max(1.0) as f32;

        let scale = (available_w / content_w)
            .min(available_h / content_h)
            .max(f32::MIN_POSITIVE);

        let drawn_w = content_w * scale;
        let drawn_h = content_h * scale;
        let offset_x = (canvas.0 - drawn_w) / 2.0 - bounds.min_x as f32 * scale;
        let offset_y = (canvas.1 - drawn_h) / 2.0 - bounds.min_y as f32 * scale;

        Self {
            scale,
            offset: (offset_x, offset_y),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{LayoutBounds, Viewport};

    #[test]
    fn a_square_fits_a_square_canvas_exactly_up_to_padding() {
        let bounds = LayoutBounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 100.0,
            max_y: 100.0,
        };
        let viewport = Viewport::fit(bounds, (200.0, 200.0), 0.0);
        assert!((viewport.scale - 2.0).abs() < 1e-4, "{viewport:?}");

        let top_left = viewport.to_screen((0.0, 0.0));
        let bottom_right = viewport.to_screen((100.0, 100.0));
        assert!((top_left.0 - 0.0).abs() < 1e-2);
        assert!((top_left.1 - 0.0).abs() < 1e-2);
        assert!((bottom_right.0 - 200.0).abs() < 1e-2);
        assert!((bottom_right.1 - 200.0).abs() < 1e-2);
    }

    #[test]
    fn padding_shrinks_the_drawn_content_away_from_the_edge() {
        let bounds = LayoutBounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 100.0,
            max_y: 100.0,
        };
        let viewport = Viewport::fit(bounds, (220.0, 220.0), 10.0);
        let top_left = viewport.to_screen((0.0, 0.0));
        assert!(top_left.0 >= 10.0 - 1e-3, "{top_left:?}");
        assert!(top_left.1 >= 10.0 - 1e-3, "{top_left:?}");
    }

    #[test]
    fn a_wide_arrangement_letterboxes_rather_than_stretching() {
        // 4:1 content in a square canvas: the limiting axis is width, so
        // the vertical scale must equal the horizontal one (no stretch).
        let bounds = LayoutBounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 400.0,
            max_y: 100.0,
        };
        let viewport = Viewport::fit(bounds, (200.0, 200.0), 0.0);
        assert!((viewport.scale - 0.5).abs() < 1e-4, "{viewport:?}");
    }

    #[test]
    fn degenerate_bounds_and_canvases_never_produce_a_broken_scale() {
        for bounds in [
            LayoutBounds::point(0.0, 0.0),
            LayoutBounds {
                min_x: 5.0,
                min_y: 5.0,
                max_x: 5.0,
                max_y: 5.0,
            },
        ] {
            for canvas in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (-5.0, -5.0)] {
                let viewport = Viewport::fit(bounds, canvas, 0.0);
                assert!(
                    viewport.scale.is_finite() && viewport.scale > 0.0,
                    "{viewport:?}"
                );
                assert!(viewport.offset.0.is_finite());
                assert!(viewport.offset.1.is_finite());
            }
        }
    }

    proptest! {
        /// Round trip: any screen point maps to a layout point and back to
        /// (very nearly) the same screen point, for any viewport `fit`
        /// could actually produce.
        #[test]
        fn to_layout_and_back_round_trips(
            min_x in -10_000.0f64..10_000.0, min_y in -10_000.0f64..10_000.0,
            w in 1.0f64..20_000.0, h in 1.0f64..20_000.0,
            canvas_w in 1.0f32..4_000.0, canvas_h in 1.0f32..4_000.0,
            padding in 0.0f32..100.0,
            sx in -5_000.0f32..5_000.0, sy in -5_000.0f32..5_000.0,
        ) {
            let bounds = LayoutBounds { min_x, min_y, max_x: min_x + w, max_y: min_y + h };
            let viewport = Viewport::fit(bounds, (canvas_w, canvas_h), padding);
            let layout = viewport.to_layout((sx, sy));
            let back = viewport.to_screen(layout);
            // A generous fixed tolerance rather than one derived from the
            // viewport's own scale: dividing by a very small `scale` to
            // size the tolerance would itself need the truncating cast
            // this test is trying to avoid depending on.
            let tolerance = 1.0f32;
            prop_assert!((back.0 - sx).abs() <= tolerance, "{back:?} vs {sx}");
            prop_assert!((back.1 - sy).abs() <= tolerance, "{back:?} vs {sy}");
        }

        /// Containment: every corner of the fitted bounds lands inside the
        /// padded canvas rectangle (with a small tolerance for rounding),
        /// for any bounds and canvas `fit` can be asked to reconcile.
        #[test]
        fn fitted_bounds_stay_within_the_padded_canvas(
            min_x in -10_000.0f64..10_000.0, min_y in -10_000.0f64..10_000.0,
            w in 1.0f64..20_000.0, h in 1.0f64..20_000.0,
            canvas_w in 50.0f32..4_000.0, canvas_h in 50.0f32..4_000.0,
            padding in 0.0f32..20.0,
        ) {
            let bounds = LayoutBounds { min_x, min_y, max_x: min_x + w, max_y: min_y + h };
            let viewport = Viewport::fit(bounds, (canvas_w, canvas_h), padding);
            let corners = [
                (bounds.min_x, bounds.min_y),
                (bounds.max_x, bounds.min_y),
                (bounds.min_x, bounds.max_y),
                (bounds.max_x, bounds.max_y),
            ];
            let tolerance = 1.0f32;
            for corner in corners {
                let screen = viewport.to_screen(corner);
                prop_assert!(screen.0 >= padding - tolerance, "{screen:?}");
                prop_assert!(screen.1 >= padding - tolerance, "{screen:?}");
                prop_assert!(screen.0 <= canvas_w - padding + tolerance, "{screen:?}");
                prop_assert!(screen.1 <= canvas_h - padding + tolerance, "{screen:?}");
            }
        }
    }
}
