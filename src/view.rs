//! The view: how large the sheet is drawn, and where it sits.
//!
//! ## Why zoom is not the sheet's size
//!
//! `Settings::page_display_width` is *paper*: choosing A5 means half a sheet of A4, and a PDF page
//! brings its own shape whatever it is drawn at. Zoom is not paper — it is how close the reader is
//! standing to it. Keeping them apart is what lets Fit Width be pressed, the paper chosen
//! afterwards, and the paper's own size still be what it claims to be.
//!
//! ## The two rules the layout follows
//!
//! * A sheet that **fits** on an axis is centred on it, wherever it was last dragged to. A sheet
//!   smaller than the window has nothing to reveal by being off-centre, and centring is what makes
//!   Fit Width and Fit Height look like they did what they say.
//! * A sheet that **does not fit** is panned, bounded so that its far edge can be brought to the
//!   window's edge and no further. Zooming in therefore always leaves a way back to every corner.
//!
//! ## Why zooming is anchored
//!
//! A pinch or a wheel zoom is a gesture *at a place*: the sheet should grow around the point under
//! the fingers, or the thing being looked at slides away as it grows. [`Viewport::zoom_around`]
//! does that arithmetic, and it is the only reason the pan is stored rather than recomputed from a
//! centre each frame.

/// How much of the sheet's size one step of the zoom buttons adds or removes.
///
/// A quarter of the way, so four presses roughly double the size: fine enough to land on a
/// comfortable reading size, coarse enough that reaching 4x does not take a dozen clicks.
pub const ZOOM_STEP: f32 = 1.25;

/// The closest the sheet can be drawn, relative to its own size.
pub const MIN_ZOOM: f32 = 0.05;

/// The furthest the sheet can be drawn, relative to its own size.
///
/// Sixteen times is already past the point where a page's own bitmap is being magnified rather
/// than resolved, and beyond it the numbers stop being worth the arithmetic.
pub const MAX_ZOOM: f32 = 16.0;

/// Which axis a fit is against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// The sheet's width is made to fit the window's.
    Width,
    /// The sheet's height is made to fit the window's.
    Height,
}

impl Fit {
    /// Both fits, in the order the toolbar offers them.
    pub const ALL: [Fit; 2] = [Fit::Width, Fit::Height];

    /// The label on this fit's button.
    pub fn label(self) -> &'static str {
        match self {
            Fit::Width => "Fit W",
            Fit::Height => "Fit H",
        }
    }

    /// The stable element id of this fit's button.
    ///
    /// Stable and unique per fit: GPUI keeps hover and press state against an element id, so an
    /// id derived from a position in the list would move that state onto a neighbour.
    pub fn button_id(self) -> &'static str {
        match self {
            Fit::Width => "fit-width",
            Fit::Height => "fit-height",
        }
    }

    /// The axis this fit measures, from a `(width, height)` pair.
    fn axis(self, size: (f32, f32)) -> f32 {
        match self {
            Fit::Width => size.0,
            Fit::Height => size.1,
        }
    }
}

/// The zoom the sheet is drawn at, and how far it has been panned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// How large the sheet is drawn: `1.0` is the size the paper asks for.
    zoom: f32,
    /// How far the sheet has been panned from its resting place, in logical pixels.
    ///
    /// Only meaningful on an axis the sheet does not fit on: see the module docs.
    offset: (f32, f32),
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            zoom: 1.0,
            offset: (0.0, 0.0),
        }
    }
}

impl Viewport {
    /// A viewport at the given zoom, with the sheet at its resting place.
    ///
    /// The zoom is clamped, so a settings file that says `1e9` cannot make the app draw a sheet a
    /// kilometre wide.
    pub fn new(zoom: f32) -> Self {
        Viewport {
            zoom: clamp_zoom(zoom),
            offset: (0.0, 0.0),
        }
    }

    /// The zoom the sheet is drawn at.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// A sheet size in paper units, as the size it is drawn at.
    pub fn drawn(&self, sheet: (f32, f32)) -> (f32, f32) {
        (sheet.0 * self.zoom, sheet.1 * self.zoom)
    }

    /// Sets the zoom, reporting whether it changed.
    pub fn set_zoom(&mut self, zoom: f32) -> bool {
        let zoom = clamp_zoom(zoom);
        if zoom == self.zoom {
            return false;
        }

        self.zoom = zoom;
        true
    }

    /// Multiplies the zoom, reporting whether it changed.
    pub fn zoom_by(&mut self, factor: f32) -> bool {
        if !factor.is_finite() || factor <= 0.0 {
            return false;
        }

        self.set_zoom(self.zoom * factor)
    }

    /// One step closer.
    pub fn zoom_in(&mut self) -> bool {
        self.zoom_by(ZOOM_STEP)
    }

    /// One step further away.
    pub fn zoom_out(&mut self) -> bool {
        self.zoom_by(1.0 / ZOOM_STEP)
    }

    /// Puts the sheet back at its resting place on both axes.
    pub fn reset_pan(&mut self) -> bool {
        let was = self.offset;
        self.offset = (0.0, 0.0);
        was != self.offset
    }

    /// Pans by a delta, reporting whether it changed anything.
    pub fn pan_by(&mut self, delta: (f32, f32)) -> bool {
        if !delta.0.is_finite() || !delta.1.is_finite() {
            return false;
        }

        self.offset = (self.offset.0 + delta.0, self.offset.1 + delta.1);
        true
    }

    /// Where the sheet's top-left corner is drawn, in window logical pixels.
    pub fn origin(&self, sheet: (f32, f32), window: (f32, f32), margin: f32) -> (f32, f32) {
        let drawn = self.drawn(sheet);

        (
            axis_origin(drawn.0, window.0, margin, self.offset.0),
            axis_origin(drawn.1, window.1, margin, self.offset.1),
        )
    }

    /// One step of a *continuous* gesture, about the point under the pointer.
    ///
    /// `pointer` is in window logical pixels, `sheet` is the sheet's size in paper units, `window`
    /// is the space it is drawn into, and `margin` is the strip kept clear at its edges. The pan is
    /// adjusted so that the sheet coordinate under the pointer does not move — which is what makes
    /// a pinch feel like pulling the page rather than pushing it away.
    pub fn zoom_around(
        &mut self,
        factor: f32,
        pointer: (f32, f32),
        sheet: (f32, f32),
        window: (f32, f32),
        margin: f32,
    ) -> bool {
        let before = self.origin(sheet, window, margin);
        let was = self.zoom;
        if !self.zoom_by(factor) {
            return false;
        }

        // Where the pointer was, in the sheet's own coordinates. The zoom does not change it,
        // which is the whole point of anchoring the zoom to it.
        let anchor = ((pointer.0 - before.0) / was, (pointer.1 - before.1) / was);

        // What the pan has to be for that same sheet coordinate to still be under the pointer.
        let drawn = self.drawn(sheet);
        self.offset = (
            pan_for(pointer.0 - anchor.0 * self.zoom, drawn.0, window.0, margin),
            pan_for(pointer.1 - anchor.1 * self.zoom, drawn.1, window.1, margin),
        );

        true
    }

    /// Makes the sheet fit the window on one axis, and puts it back at its resting place.
    pub fn fit(&mut self, which: Fit, sheet: (f32, f32), window: (f32, f32), margin: f32) -> bool {
        let wanted = fit_zoom(which.axis(sheet), which.axis(window), margin);
        // Both are evaluated before the `|`: a fit that only re-centred a sheet already at the
        // right zoom still has something to report.
        self.set_zoom(wanted) | self.reset_pan()
    }
}

/// The zoom that makes a sheet `sheet` wide fit inside `window`, less a margin at each edge.
///
/// A window with no room for the margins at all falls back to the minimum, so an absurd window
/// still yields a usable number rather than a negative or infinite one.
fn fit_zoom(sheet: f32, window: f32, margin: f32) -> f32 {
    let room = window - margin * 2.0;
    if sheet <= 0.0 || room <= 0.0 {
        return MIN_ZOOM;
    }

    clamp_zoom(room / sheet)
}

/// The zoom, brought into range.
fn clamp_zoom(zoom: f32) -> f32 {
    if !zoom.is_finite() || zoom <= 0.0 {
        return 1.0;
    }

    zoom.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Where a sheet drawn `drawn` wide is placed on one axis, from a pan offset.
fn axis_origin(drawn: f32, window: f32, margin: f32, offset: f32) -> f32 {
    // It fits: centred, whatever the pan says. This is also why a sheet that fits is never
    // dragged: there is nothing off-screen to drag into view.
    if drawn + margin * 2.0 <= window {
        return (window - drawn) / 2.0;
    }

    // It does not fit: the pan moves it between "its near edge at the margin" and "its far edge at
    // the window's edge", which is exactly the set of positions from which the whole sheet is
    // reachable.
    (margin + offset).clamp(window - drawn - margin, margin)
}

/// The pan that puts the sheet's origin as close to `wanted` as the layout allows.
///
/// `drawn` is the sheet's size *after* the zoom, which is what the clamp bounds are about; `wanted`
/// is the origin the gesture asked for, which already carries the zoom.
///
/// The clamp is why this is not simply `wanted - margin`: a gesture that asked for an
/// out-of-range origin would otherwise leave the pan outside its own bounds, and the sheet would
/// jump the next time an unrelated change let that clamp take effect.
fn pan_for(wanted: f32, drawn: f32, window: f32, margin: f32) -> f32 {
    // An axis the sheet fits on is centred, so there is no pan to store.
    if drawn + margin * 2.0 <= window {
        return 0.0;
    }

    wanted.clamp(window - drawn - margin, margin) - margin
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A4 at 720 logical pixels wide, as the app draws it.
    const SHEET: (f32, f32) = (720.0, 1018.0);
    /// The window the app's own default opens at.
    const WINDOW: (f32, f32) = (1280.0, 900.0);
    const MARGIN: f32 = 24.0;

    /// Zoom is clamped at both ends, and nonsense is not zoom at all.
    #[test]
    fn zoom_is_clamped_and_nonsense_is_ignored() {
        let mut view = Viewport::default();

        assert!(view.set_zoom(2.0));
        assert_eq!(view.zoom(), 2.0);
        assert!(!view.set_zoom(2.0), "the same zoom is not a change");

        view.set_zoom(1e9);
        assert_eq!(view.zoom(), MAX_ZOOM);
        view.set_zoom(1e-9);
        assert_eq!(view.zoom(), MIN_ZOOM);

        // A factor that is not a number would poison every rectangle computed from it, so it is
        // refused rather than clamped: clamping NaN yields NaN.
        let before = view;
        assert!(!view.zoom_by(f32::NAN));
        assert!(!view.zoom_by(0.0));
        assert!(!view.zoom_by(-1.0));
        assert_eq!(view, before, "a refused factor changes nothing");
    }

    /// One step in and one step out is back where it started.
    #[test]
    fn stepping_in_and_out_returns_to_the_same_zoom() {
        let mut view = Viewport::default();

        assert!(view.zoom_in());
        assert!(view.zoom() > 1.0, "in is closer");
        assert!(view.zoom_out());
        assert!(
            (view.zoom() - 1.0).abs() < 1e-6,
            "and out undoes it exactly enough: {}",
            view.zoom()
        );
    }

    /// Fit Width fills the window's width; Fit Height fills its height.
    #[test]
    fn a_fit_fills_its_own_axis() {
        let mut view = Viewport::default();

        assert!(view.fit(Fit::Width, SHEET, WINDOW, MARGIN));
        assert!(
            (view.drawn(SHEET).0 - (WINDOW.0 - MARGIN * 2.0)).abs() < 0.01,
            "the sheet is as wide as the room for it"
        );

        assert!(view.fit(Fit::Height, SHEET, WINDOW, MARGIN));
        assert!(
            (view.drawn(SHEET).1 - (WINDOW.1 - MARGIN * 2.0)).abs() < 0.01,
            "and as tall as the room for it"
        );
    }

    /// A sheet that fits is centred; a sheet that does not fit starts at the margin, so its
    /// top-left corner is the first thing seen.
    #[test]
    fn a_sheet_that_fits_is_centred_and_one_that_does_not_is_not() {
        let view = Viewport::default();

        let roomy = view.origin(SHEET, (1600.0, 1200.0), MARGIN);
        assert!(
            (roomy.0 - (1600.0 - 720.0) / 2.0).abs() < 0.01,
            "a sheet with room either side is centred: {roomy:?}"
        );

        let crowded = view.origin(SHEET, (700.0, 600.0), MARGIN);
        assert_eq!(
            crowded.0, MARGIN,
            "a sheet wider than the window starts at the margin"
        );
        assert_eq!(crowded.1, MARGIN, "and so does a sheet taller than it");
    }

    /// Panning a sheet that does not fit moves it, and stops at its own edges.
    #[test]
    fn panning_stops_at_the_sheet_edges() {
        let mut view = Viewport::new(2.0);
        let drawn = view.drawn(SHEET);

        // Toward the far corner, far past the end of the sheet.
        view.pan_by((-1.0e6, -1.0e6));
        let far = view.origin(SHEET, WINDOW, MARGIN);
        assert!(
            (far.0 - (WINDOW.0 - drawn.0 - MARGIN)).abs() < 0.01,
            "the far edge stops at the window's edge: {far:?}"
        );
        assert!((far.1 - (WINDOW.1 - drawn.1 - MARGIN)).abs() < 0.01);

        // And back past the other end.
        view.pan_by((1.0e6, 1.0e6));
        assert_eq!(
            view.origin(SHEET, WINDOW, MARGIN),
            (MARGIN, MARGIN),
            "the near edge stops at the margin"
        );
    }

    /// A sheet that fits is not panned at all, however hard it is pushed at.
    #[test]
    fn a_sheet_that_fits_cannot_be_pushed_off_centre() {
        let mut view = Viewport::default();
        view.pan_by((500.0, -900.0));

        assert_eq!(
            view.origin((100.0, 100.0), WINDOW, MARGIN),
            ((WINDOW.0 - 100.0) / 2.0, (WINDOW.1 - 100.0) / 2.0),
            "there is nothing off-screen to drag into view"
        );
    }

    /// The point under the pointer stays under the pointer, which is what makes a pinch feel like
    /// pulling the page rather than pushing it away.
    #[test]
    fn a_zoom_keeps_the_point_under_the_pointer() {
        let mut view = Viewport::new(2.0);
        let pointer = (900.0, 300.0);

        let before = view.origin(SHEET, WINDOW, MARGIN);
        let sheet_point = (
            (pointer.0 - before.0) / view.zoom(),
            (pointer.1 - before.1) / view.zoom(),
        );

        assert!(view.zoom_around(1.3, pointer, SHEET, WINDOW, MARGIN));

        let after = view.origin(SHEET, WINDOW, MARGIN);
        assert!(
            (after.0 + sheet_point.0 * view.zoom() - pointer.0).abs() < 0.01
                && (after.1 + sheet_point.1 * view.zoom() - pointer.1).abs() < 0.01,
            "the anchored sheet point moved: {before:?} -> {after:?}"
        );
    }

    /// Zooming about a point on a sheet that fits keeps it centred: there is no pan to anchor
    /// with, and centring is what a reader expects of a sheet with room around it.
    #[test]
    fn zooming_a_sheet_that_fits_leaves_it_centred() {
        let mut view = Viewport::default();
        let window = (1600.0, 1200.0);

        assert!(view.zoom_around(0.5, (10.0, 10.0), SHEET, window, MARGIN));

        assert_eq!(
            view.origin(SHEET, window, MARGIN).0,
            (window.0 - view.drawn(SHEET).0) / 2.0,
            "still centred"
        );
    }

    /// A fit puts the sheet back at its resting place, so pressing it after panning is a way back
    /// to the whole page.
    #[test]
    fn a_fit_resets_the_pan() {
        let mut view = Viewport::new(4.0);
        view.pan_by((-2000.0, -2000.0));

        assert!(view.fit(Fit::Width, SHEET, WINDOW, MARGIN));
        assert_eq!(
            view.origin(SHEET, WINDOW, MARGIN),
            (MARGIN, MARGIN),
            "the sheet is back at its corner"
        );
    }

    /// An absurd window yields a usable zoom rather than a negative or infinite one.
    #[test]
    fn a_window_with_no_room_still_yields_a_zoom() {
        for window in [(0.0, 0.0), (10.0, 10.0), (-5.0, -5.0)] {
            let mut view = Viewport::default();
            view.fit(Fit::Width, SHEET, window, MARGIN);

            assert_eq!(view.zoom(), MIN_ZOOM, "window {window:?}");
        }

        let mut view = Viewport::default();
        view.fit(Fit::Height, (0.0, 0.0), WINDOW, MARGIN);
        assert_eq!(
            view.zoom(),
            MIN_ZOOM,
            "a sheet with no size cannot be fitted"
        );
    }

    /// Every fit has its own button id, because GPUI keeps hover state against one.
    #[test]
    fn every_fit_has_its_own_element_id() {
        let ids: Vec<&str> = Fit::ALL.iter().map(|fit| fit.button_id()).collect();

        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }
}
