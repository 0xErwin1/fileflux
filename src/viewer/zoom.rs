//! Zoom and pan geometry, free of GPUI so the rules are testable.
//!
//! Coordinates are logical pixels in the image area (the window below the
//! top bar). The pan `offset` is where the image's center sits relative to
//! the area's center, so `(0, 0)` always means "centered".

/// The smallest and largest zoom the viewer allows (2% to 3200%).
pub(crate) const MIN_ZOOM: f32 = 0.02;
pub(crate) const MAX_ZOOM: f32 = 32.0;
/// One keyboard or wheel notch multiplies the zoom by this.
pub(crate) const ZOOM_STEP: f32 = 1.25;

/// A point or size in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Pt {
    pub(crate) x: f32,
    pub(crate) y: f32,
}

impl Pt {
    pub(crate) const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// The scale that shows the whole image inside `area`, never above 100%:
/// small images are not enlarged by "fit".
pub(crate) fn fit_scale(image: Pt, area: Pt) -> f32 {
    if image.x <= 0.0 || image.y <= 0.0 || area.x <= 0.0 || area.y <= 0.0 {
        return 1.0;
    }
    (area.x / image.x).min(area.y / image.y).min(1.0)
}

pub(crate) fn clamp_zoom(scale: f32) -> f32 {
    if scale.is_nan() {
        1.0
    } else {
        scale.clamp(MIN_ZOOM, MAX_ZOOM)
    }
}

/// `steps` notches of zoom from `scale` (negative zooms out).
pub(crate) fn step_zoom(scale: f32, steps: f32) -> f32 {
    clamp_zoom(scale * ZOOM_STEP.powf(steps))
}

/// Keeps the image covering the area: an axis where the image is not
/// larger than the area is centered, otherwise the image edge may not
/// pull away from the area edge.
pub(crate) fn clamp_offset(offset: Pt, shown: Pt, area: Pt) -> Pt {
    let axis = |offset: f32, shown: f32, area: f32| {
        let slack = (shown - area) / 2.0;
        if slack <= 0.0 {
            0.0
        } else {
            offset.clamp(-slack, slack)
        }
    };
    Pt::new(
        axis(offset.x, shown.x, area.x),
        axis(offset.y, shown.y, area.y),
    )
}

/// The offset that keeps the image point under `pointer` fixed while the
/// scale changes from `old` to `new`. Not clamped.
pub(crate) fn zoom_about(offset: Pt, old: f32, new: f32, pointer: Pt, area: Pt) -> Pt {
    if old <= 0.0 {
        return offset;
    }
    let k = new / old;
    let from_center = Pt::new(pointer.x - area.x / 2.0, pointer.y - area.y / 2.0);
    Pt::new(
        from_center.x - k * (from_center.x - offset.x),
        from_center.y - k * (from_center.y - offset.y),
    )
}

/// Where the image's top-left corner lands in the area.
pub(crate) fn top_left(offset: Pt, shown: Pt, area: Pt) -> Pt {
    Pt::new(
        area.x / 2.0 + offset.x - shown.x / 2.0,
        area.y / 2.0 + offset.y - shown.y / 2.0,
    )
}

/// The label for a zoom factor, e.g. `100%`.
pub(crate) fn percent_label(scale: f32) -> String {
    format!("{}%", (scale * 100.0).round())
}

/// Fit-to-window, or a fixed factor chosen by the user.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ZoomMode {
    Fit,
    Custom(f32),
}

/// The zoom and pan state of the shown image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ViewState {
    pub(crate) mode: ZoomMode,
    pub(crate) offset: Pt,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            mode: ZoomMode::Fit,
            offset: Pt::default(),
        }
    }
}

impl ViewState {
    /// The scale in effect for an `image` shown in `area`.
    pub(crate) fn scale(&self, image: Pt, area: Pt) -> f32 {
        match self.mode {
            ZoomMode::Fit => fit_scale(image, area),
            ZoomMode::Custom(scale) => clamp_zoom(scale),
        }
    }

    /// The offset to draw with: the stored one kept inside the image.
    pub(crate) fn offset(&self, image: Pt, area: Pt) -> Pt {
        let scale = self.scale(image, area);
        clamp_offset(self.offset, Pt::new(image.x * scale, image.y * scale), area)
    }

    /// Zooms by `steps` notches around `pointer`.
    pub(crate) fn zoom_at(&mut self, steps: f32, pointer: Pt, image: Pt, area: Pt) {
        let target = step_zoom(self.scale(image, area), steps);
        self.set_scale_at(target, pointer, image, area);
    }

    /// Jumps to an exact factor around `pointer`.
    pub(crate) fn set_scale_at(&mut self, scale: f32, pointer: Pt, image: Pt, area: Pt) {
        let old = self.scale(image, area);
        let new = clamp_zoom(scale);
        let current = self.offset(image, area);
        let moved = zoom_about(current, old, new, pointer, area);
        self.mode = ZoomMode::Custom(new);
        self.offset = clamp_offset(moved, Pt::new(image.x * new, image.y * new), area);
    }

    pub(crate) fn fit(&mut self) {
        *self = Self::default();
    }

    /// Double-click: fit becomes 100% around the pointer, anything else
    /// goes back to fit.
    pub(crate) fn toggle_at(&mut self, pointer: Pt, image: Pt, area: Pt) {
        match self.mode {
            ZoomMode::Fit => self.set_scale_at(1.0, pointer, image, area),
            ZoomMode::Custom(_) => self.fit(),
        }
    }

    /// Drags the image by `delta`, from `start` (the offset when the drag began).
    pub(crate) fn pan_from(&mut self, start: Pt, delta: Pt, image: Pt, area: Pt) {
        let scale = self.scale(image, area);
        let shown = Pt::new(image.x * scale, image.y * scale);
        self.offset = clamp_offset(Pt::new(start.x + delta.x, start.y + delta.y), shown, area);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const AREA: Pt = Pt::new(1000.0, 800.0);

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn fit_shrinks_large_images_to_the_limiting_axis() {
        assert!(near(fit_scale(Pt::new(4000.0, 2000.0), AREA), 0.25));
        assert!(near(fit_scale(Pt::new(1000.0, 4000.0), AREA), 0.2));
    }

    #[test]
    fn fit_never_upscales_small_images() {
        assert!(near(fit_scale(Pt::new(100.0, 50.0), AREA), 1.0));
        assert!(near(fit_scale(Pt::new(1000.0, 800.0), AREA), 1.0));
    }

    #[test]
    fn fit_ignores_degenerate_sizes() {
        assert!(near(fit_scale(Pt::new(0.0, 10.0), AREA), 1.0));
        assert!(near(fit_scale(Pt::new(10.0, 10.0), Pt::new(0.0, 0.0)), 1.0));
    }

    #[test]
    fn zoom_steps_multiply_and_stay_in_range() {
        assert!(near(step_zoom(1.0, 1.0), ZOOM_STEP));
        assert!(near(step_zoom(1.0, -1.0), 1.0 / ZOOM_STEP));
        assert!(near(step_zoom(30.0, 5.0), MAX_ZOOM));
        assert!(near(step_zoom(0.03, -5.0), MIN_ZOOM));
        assert!(near(clamp_zoom(f32::NAN), 1.0));
    }

    #[test]
    fn offset_is_centered_when_the_image_fits_and_bounded_when_it_does_not() {
        let small = clamp_offset(Pt::new(50.0, -20.0), Pt::new(500.0, 400.0), AREA);
        assert_eq!(small, Pt::new(0.0, 0.0));
        let wide = clamp_offset(Pt::new(9999.0, -9999.0), Pt::new(3000.0, 700.0), AREA);
        assert!(near(wide.x, 1000.0));
        assert!(near(wide.y, 0.0));
        let inside = clamp_offset(Pt::new(300.0, 0.0), Pt::new(3000.0, 700.0), AREA);
        assert!(near(inside.x, 300.0));
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer_fixed() {
        let image = Pt::new(2000.0, 1000.0);
        let offset = Pt::new(120.0, -40.0);
        let pointer = Pt::new(700.0, 300.0);
        let (old, new) = (1.0_f32, 2.5_f32);
        let before = top_left(offset, Pt::new(image.x * old, image.y * old), AREA);
        let source = Pt::new((pointer.x - before.x) / old, (pointer.y - before.y) / old);
        let moved = zoom_about(offset, old, new, pointer, AREA);
        let after = top_left(moved, Pt::new(image.x * new, image.y * new), AREA);
        assert!(near((pointer.x - after.x) / new, source.x));
        assert!(near((pointer.y - after.y) / new, source.y));
    }

    #[test]
    fn zooming_at_the_center_does_not_pan() {
        let moved = zoom_about(Pt::default(), 1.0, 2.0, Pt::new(500.0, 400.0), AREA);
        assert_eq!(moved, Pt::default());
    }

    #[test]
    fn percent_label_rounds() {
        assert_eq!(percent_label(1.0), "100%");
        assert_eq!(percent_label(0.254), "25%");
        assert_eq!(percent_label(2.5), "250%");
    }

    #[test]
    fn zooming_from_fit_starts_at_the_fit_scale() {
        let image = Pt::new(4000.0, 2000.0);
        let mut view = ViewState::default();
        view.zoom_at(1.0, Pt::new(500.0, 400.0), image, AREA);
        assert!(matches!(view.mode, ZoomMode::Custom(scale) if near(scale, 0.25 * ZOOM_STEP)));
        assert!(near(view.scale(image, AREA), 0.25 * ZOOM_STEP));
    }

    #[test]
    fn set_scale_and_fit_round_trip() {
        let image = Pt::new(4000.0, 2000.0);
        let mut view = ViewState::default();
        view.set_scale_at(1.0, Pt::new(500.0, 400.0), image, AREA);
        assert!(near(view.scale(image, AREA), 1.0));
        view.fit();
        assert_eq!(view, ViewState::default());
    }

    #[test]
    fn double_click_toggles_between_fit_and_actual_size() {
        let image = Pt::new(4000.0, 2000.0);
        let mut view = ViewState::default();
        view.toggle_at(Pt::new(100.0, 100.0), image, AREA);
        assert!(near(view.scale(image, AREA), 1.0));
        view.toggle_at(Pt::new(100.0, 100.0), image, AREA);
        assert_eq!(view, ViewState::default());
    }

    #[test]
    fn dragging_pans_within_the_image_and_is_a_noop_when_it_fits() {
        let big = Pt::new(4000.0, 2000.0);
        let mut view = ViewState::default();
        view.set_scale_at(1.0, Pt::new(500.0, 400.0), big, AREA);
        let start = view.offset(big, AREA);
        view.pan_from(start, Pt::new(100.0, 50.0), big, AREA);
        assert!(near(view.offset(big, AREA).x, start.x + 100.0));
        view.pan_from(start, Pt::new(99999.0, 0.0), big, AREA);
        assert!(near(view.offset(big, AREA).x, 1500.0));

        let small = Pt::new(100.0, 100.0);
        let mut fit = ViewState::default();
        fit.pan_from(Pt::default(), Pt::new(30.0, 30.0), small, AREA);
        assert_eq!(fit.offset(small, AREA), Pt::default());
    }

    #[test]
    fn an_offset_stored_while_zoomed_is_clamped_after_zooming_out() {
        let image = Pt::new(4000.0, 2000.0);
        let mut view = ViewState::default();
        view.set_scale_at(1.0, Pt::new(0.0, 0.0), image, AREA);
        view.pan_from(Pt::default(), Pt::new(99999.0, 0.0), image, AREA);
        view.set_scale_at(0.1, Pt::new(500.0, 400.0), image, AREA);
        assert_eq!(view.offset(image, AREA), Pt::default());
    }
}
