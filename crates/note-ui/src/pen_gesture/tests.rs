//! The pen's own gestures: what a held key makes the nib do.
//!
//! The keys are the operating system's to answer, so what is tested here is the *rule* — which gesture a pair
//! of held keys asks for, and how far a given travel zooms — rather than the reading of a keyboard, which is
//! `device_query`'s business and not this crate's.

use crate::pen_gesture::{pen_zoom_factor, Gesture, Held, PEN_ZOOM_PER_PIXEL};

/// Nothing held is writing, so the pen is a pen.
#[test]
fn no_key_held_lays_ink() {
    assert_eq!(Gesture::of(Held::default(), true), Gesture::Ink);
}

/// Space held makes the nib move the sheet, and nothing else.
#[test]
fn space_makes_the_pen_pan() {
    let held = Held {
        space: true,
        ..Held::default()
    };

    assert_eq!(Gesture::of(held, true), Gesture::Pan);
}

/// Option held makes the nib zoom, and nothing else.
#[test]
fn option_makes_the_pen_zoom() {
    let held = Held {
        option: true,
        ..Held::default()
    };

    assert_eq!(Gesture::of(held, true), Gesture::Zoom);
}

/// Space wins when both are held: a hand that is panning may rest on option without meaning to zoom.
#[test]
fn space_wins_over_option() {
    let both = Held {
        space: true,
        option: true,
    };

    assert_eq!(
        Gesture::of(both, true),
        Gesture::Pan,
        "a gesture that began as a pan stays one"
    );
}

/// A key held while something other than the sheet has the pen means nothing at all.
#[test]
fn no_gesture_while_a_screen_is_in_front() {
    let both = Held {
        space: true,
        option: true,
    };

    assert_eq!(Gesture::of(both, false), Gesture::Ink);
}

/// Up is closer and down is further away, and a nib that has not moved zooms nothing.
#[test]
fn the_nib_zooms_the_way_it_moves() {
    assert_eq!(pen_zoom_factor(0.0), 1.0, "a nib that has not moved");
    assert!(
        pen_zoom_factor(100.0) > 1.0,
        "up is closer: {}",
        pen_zoom_factor(100.0)
    );
    assert!(
        pen_zoom_factor(-100.0) < 1.0,
        "down is further away: {}",
        pen_zoom_factor(-100.0)
    );
    assert!(
        (pen_zoom_factor(100.0) * pen_zoom_factor(-100.0) - 1.0).abs() < 1e-4,
        "the same distance either way undoes itself"
    );
}

/// The zoom is an exponent, so the same distance is the same multiple wherever the gesture began.
#[test]
fn the_pen_zoom_is_an_exponent() {
    let one = pen_zoom_factor(50.0);
    let two = pen_zoom_factor(100.0);

    assert!(
        (two - one * one).abs() < 1e-5,
        "twice the travel is the step applied twice: {one} squared vs {two}"
    );
}

/// A wild reading from a driver, or a hand that left the surface and came back, is clamped.
#[test]
fn a_wild_pen_travel_is_clamped() {
    assert_eq!(pen_zoom_factor(100_000.0), 5.0);
    assert_eq!(pen_zoom_factor(-100_000.0), 0.2);
    assert_eq!(pen_zoom_factor(f32::NAN), 1.0);
    assert_eq!(pen_zoom_factor(f32::INFINITY), 1.0);
}

/// The rate is gentler than a wheel's, because a hand moving a pen is not a wheel being spun.
#[test]
fn the_pen_zooms_more_slowly_than_a_wheel() {
    // A wheel's own per-pixel rate: 1.25 a notch of 96 pixels.
    let wheel_per_pixel = 1.25_f32.powf(1.0 / 96.0);

    assert!(
        PEN_ZOOM_PER_PIXEL < wheel_per_pixel,
        "the pen is the gentler of the two: {PEN_ZOOM_PER_PIXEL} vs {wheel_per_pixel}"
    );
}
