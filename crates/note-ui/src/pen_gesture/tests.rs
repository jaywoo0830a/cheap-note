//! The pen's own gestures: what a held key makes the nib do.
//!
//! The keys are the operating system's to answer, so what is tested here is the *rule* — which gesture a pair
//! of held keys asks for, and how far a given travel zooms — rather than the reading of a keyboard, which is
//! `device_query`'s business and not this crate's.

use crate::pen_gesture::{
    is_option, pen_zoom_factor, Gesture, Held, PEN_ZOOM_CORRECTION, PEN_ZOOM_TRAVEL,
};

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

/// The key names are the *platform's*, and the wrong ones would be a gesture that never fires.
///
/// `device_query` spells the choice modifier three ways and produces only one of them per platform: macOS
/// maps its Option key to `LOption`/`ROption` and never emits `LAlt`, `RAlt`, `LMeta` or `RMeta` at all. So
/// the four that mean "option" are checked here against the names that really exist — the test is what keeps
/// a future edit from "simplifying" this to `LAlt` alone and quietly breaking every Mac.
///
/// `Command` is deliberately absent, and this is the half that matters most: on a Mac it is the key every
/// application's own commands live under, so accepting it would fire a zoom while a reader was reaching for
/// `⌘C`.
#[test]
fn the_option_key_is_the_one_the_platform_reports() {
    use device_query::Keycode;

    // The names macOS actually produces.
    let mac = [Keycode::LOption, Keycode::ROption];
    for key in mac {
        assert!(is_option(key), "{key:?} is what macOS calls the option key");
    }

    // The names Windows and Linux produce, for the same source to be right there too.
    let elsewhere = [Keycode::LAlt, Keycode::RAlt];
    for key in elsewhere {
        assert!(is_option(key), "{key:?} is the option key off macOS");
    }

    // Not the option key: the Mac's command key, and its meta aliases, which macOS never emits.
    for key in [
        Keycode::Command,
        Keycode::RCommand,
        Keycode::LMeta,
        Keycode::RMeta,
        Keycode::Space,
        Keycode::A,
    ] {
        assert!(!is_option(key), "{key:?} must not zoom");
    }
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
    assert_eq!(pen_zoom_factor(0.0, 1.0), 1.0, "a nib that has not moved");

    let up = pen_zoom_factor(100.0, 1.0);
    let down = pen_zoom_factor(-100.0, 1.0);

    assert!(up > 1.0, "up is closer: {up}");
    assert!(down < 1.0, "down is further away: {down}");
    assert!(
        (up * down - 1.0).abs() < 1e-4,
        "the same distance either way undoes itself: {up} * {down}"
    );
}

/// The zoom is an exponent, so the same distance is the same multiple from the same starting zoom.
#[test]
fn the_pen_zoom_is_an_exponent() {
    let one = pen_zoom_factor(50.0, 1.0);
    let two = pen_zoom_factor(100.0, 1.0);

    assert!(
        (two - one * one).abs() < 1e-5,
        "twice the travel is the step applied twice: {one} squared vs {two}"
    );
}

/// A doubling takes the documented travel at 100%, and it takes more as the zoom grows — which is why the
/// travel is not fixed.
///
/// This is the complaint that started it: at a high zoom the same hand movement flew across the page, so the
/// sheet could only be nudged a pixel at a time. The rate is now `PEN_ZOOM_TRAVEL * zoom^0.25` pixels to a
/// doubling, so 400% takes a little more travel than 100% does and the gesture feels calmer in the hand.
#[test]
fn a_doubling_takes_more_travel_at_a_higher_zoom() {
    let travel = |zoom: f32| PEN_ZOOM_TRAVEL * zoom.powf(PEN_ZOOM_CORRECTION);

    // At the sheet's own size, the documented travel is exactly one doubling.
    let at_one = pen_zoom_factor(travel(1.0), 1.0);
    assert!(
        (at_one - 2.0).abs() < 1e-3,
        "100% doubles in {PEN_ZOOM_TRAVEL} px of travel: got {at_one}"
    );

    // And at every other zoom, its own travel is one doubling too.
    for zoom in [0.25, 0.5, 2.0, 4.0, 8.0] {
        let at = pen_zoom_factor(travel(zoom), zoom);
        assert!(
            (at - 2.0).abs() < 1e-3,
            "{zoom} doubles in {} px: got {at}",
            travel(zoom)
        );
    }

    // The point of the correction: a *fixed* travel would do more at a high zoom than at a low one, which is
    // exactly the bug.
    let same = 100.0;
    assert!(
        pen_zoom_factor(same, 4.0) < pen_zoom_factor(same, 1.0),
        "a fixed travel is too quick at a high zoom"
    );

    // And the correction is mild enough that a high zoom is still nowhere near immune to the hand: 50 pixels
    // must move it by more than a tenth, at every zoom.
    for zoom in [0.25, 1.0, 4.0, 8.0] {
        let factor = pen_zoom_factor(50.0, zoom);
        assert!(
            factor > 1.1,
            "at {zoom} a 50 px stroke moved the sheet by only {factor}"
        );
    }
}

/// A wild reading from a driver, or a hand that left the surface and came back, is clamped.
#[test]
fn a_wild_pen_travel_is_clamped() {
    assert_eq!(pen_zoom_factor(100_000.0, 1.0), 5.0);
    assert_eq!(pen_zoom_factor(-100_000.0, 1.0), 0.2);
    assert_eq!(pen_zoom_factor(f32::NAN, 1.0), 1.0);
    assert_eq!(pen_zoom_factor(f32::INFINITY, 1.0), 1.0);
    assert_eq!(pen_zoom_factor(100.0, f32::NAN), 1.0);
    assert_eq!(pen_zoom_factor(100.0, 0.0), 1.0);
    assert_eq!(pen_zoom_factor(100.0, -1.0), 1.0);
}

/// The pen is quicker than a wheel, because a hand on a pen has far finer control than a wheel being spun.
///
/// A wheel is 1.25 a notch of 96 pixels, which is a doubling every 298; the pen is 200. The earlier rate was a
/// fixed 300, which sat closer to the wheel than to the hand and is what felt sluggish.
#[test]
fn the_pen_zooms_more_quickly_than_a_wheel() {
    let wheel_travel = 96.0 / 1.25_f32.log2();

    assert!(
        PEN_ZOOM_TRAVEL < wheel_travel,
        "the pen doubles in {PEN_ZOOM_TRAVEL} px and a wheel in {wheel_travel}"
    );
    // The number the complaint was about: under the old fixed rate, 300 pixels of travel was one doubling. The
    // same 200 pixels now does more than that old 300 did, which is exactly the increase in responsiveness the
    // complaint asked for.
    let old_rate = 1.002_313_2_f32; // the fixed per-pixel base this replaced: a doubling every 300 px
    let old_at_two_hundred = old_rate.powf(PEN_ZOOM_TRAVEL);
    let now_at_two_hundred = pen_zoom_factor(PEN_ZOOM_TRAVEL, 1.0);

    assert!(
        old_at_two_hundred < 2.0,
        "the old rate needed 300 px to double, so 200 px fell short: {old_at_two_hundred}"
    );
    assert!(
        now_at_two_hundred > old_at_two_hundred,
        "200 px now does more than it used to: {now_at_two_hundred} vs {old_at_two_hundred}"
    );
}
