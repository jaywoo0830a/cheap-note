use super::*;

/// A4 at 720 logical pixels wide, as the app draws it.
const SHEET: (f32, f32) = (720.0, 1018.0);
/// The window the app's own default opens at.
const WINDOW: (f32, f32) = (1280.0, 900.0);
const MARGIN: f32 = 24.0;

/// The end of the sheet is an end: a wheel turned past it leaves no debt to unwind.
///
/// The pan used to accumulate while the drawing clamped it, so a scroll past the bottom of a page
/// built up an invisible offset and the next scroll *up* was spent paying it back — which is a page
/// that sits still under a hand that is moving.
#[test]
fn a_pan_held_at_the_end_of_the_sheet_owes_nothing_back() {
    // Twice A4, so it fits on neither axis and the pan is what decides what is on screen.
    let sheet = (SHEET.0 * 2.0, SHEET.1 * 2.0);
    let mut view = Viewport::new(2.0);

    // Far past the bottom of the sheet, and held there.
    view.pan_by((0.0, -1.0e6));
    assert!(view.hold_pan(sheet, WINDOW, MARGIN));

    let at_the_end = view.origin(sheet, WINDOW, MARGIN);

    // One notch back up moves the sheet at once.
    view.pan_by((0.0, 40.0));
    view.hold_pan(sheet, WINDOW, MARGIN);

    assert!(
        view.origin(sheet, WINDOW, MARGIN).1 > at_the_end.1,
        "the sheet stayed still after a pan back from the end: {:?} → {:?}",
        at_the_end,
        view.origin(sheet, WINDOW, MARGIN)
    );
}

/// A sheet that fits is centred, so there is no pan to hold — and no offset kept either.
#[test]
fn a_sheet_that_fits_has_no_pan_to_hold() {
    // Half an A4, which the default window has room for on both axes.
    let sheet = (SHEET.0 * 0.5, SHEET.1 * 0.5);
    let mut view = Viewport::new(1.0);

    view.pan_by((120.0, -240.0));

    assert!(view.hold_pan(sheet, WINDOW, MARGIN));
    assert_eq!(view.pan(), (0.0, 0.0), "a sheet that fits kept a pan");
    assert_eq!(
        view.origin(sheet, WINDOW, MARGIN),
        ((WINDOW.0 - sheet.0) / 2.0, (WINDOW.1 - sheet.1) / 2.0)
    );
}

/// A page opens at its own top, and comes back to the place it was left at.
///
/// The two halves of what a page turn does with the sheet: a pan of nothing is the sheet's top-left corner at the
/// margin — the top of the page, wherever the reader has zoomed to — and a pan that was remembered is the place to
/// come back to (`NoteApp::turn_to` keeps one per page for as long as the note is open, which is why a page nobody
/// has looked at opens at its top rather than where the page before it was scrolled to).
#[test]
fn a_page_opens_at_its_top_and_comes_back_where_it_was() {
    // Twice A4, so it fits on neither axis and the pan is what decides what is on screen.
    let sheet = (SHEET.0 * 2.0, SHEET.1 * 2.0);
    let mut view = Viewport::new(2.0);

    assert_eq!(
        view.origin(sheet, WINDOW, MARGIN),
        (MARGIN, MARGIN),
        "a page nobody has panned is drawn from its top-left corner"
    );

    // Read part-way down the page, and left there.
    view.pan_by((0.0, -900.0));
    let place = view.pan();

    assert!(place.1 < 0.0, "the sheet was pushed up: {place:?}");

    // Another page: its top. Then back to this one: where it was.
    view.reset_pan();
    assert_eq!(view.origin(sheet, WINDOW, MARGIN), (MARGIN, MARGIN));

    assert!(view.set_pan(place), "the place came back");
    assert_eq!(view.pan(), place);
    assert_eq!(
        view.origin(sheet, WINDOW, MARGIN),
        (MARGIN, MARGIN - 900.0),
        "and the sheet with it"
    );
}

/// A pan that is not two numbers is refused rather than stored: a `NaN` would poison every rectangle drawn from
/// it, and the one place a pan comes from outside this module is a page's remembered place.
#[test]
fn a_pan_that_is_not_a_place_is_refused() {
    let mut view = Viewport::new(2.0);
    view.pan_by((10.0, 10.0));

    for nonsense in [(f32::NAN, 0.0), (0.0, f32::INFINITY), (f32::NAN, f32::NAN)] {
        assert!(!view.set_pan(nonsense), "{nonsense:?} is not a place");
        assert_eq!(view.pan(), (10.0, 10.0), "and the sheet did not move");
    }
}

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
