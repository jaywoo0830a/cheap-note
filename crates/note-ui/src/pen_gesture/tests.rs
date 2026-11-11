//! The pen's own gestures: what a held key makes the nib do.
//!
//! The keys are the operating system's to answer, so what is tested here is the *rule* — which gesture a pair
//! of held keys asks for, and how far a given travel zooms — rather than the reading of a keyboard, which is
//! `device_query`'s business and not this crate's.

use crate::pen_gesture::{is_option, pen_zoom_factor, Gesture, Held, PEN_ZOOM_TRAVEL};

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
    assert_eq!(pen_zoom_factor(0.0), 1.0, "a nib that has not moved");

    let up = pen_zoom_factor(100.0);
    let down = pen_zoom_factor(-100.0);

    assert!(up > 1.0, "up is closer: {up}");
    assert!(down < 1.0, "down is further away: {down}");
    assert!(
        (up * down - 1.0).abs() < 1e-4,
        "the same distance either way undoes itself: {up} * {down}"
    );
}

/// The rule a reader has to learn is one sentence long: this much travel is this much zoom.
///
/// The documented travel doubles the sheet, and it does so **whatever the zoom is**. That is the point of
/// dropping the zoom-dependent correction: a gesture measured in window pixels is already absolute, and the
/// hand knows how far it moved — so the only thing left to explain is the distance, which is one number.
#[test]
fn the_travel_to_double_is_the_same_at_every_zoom() {
    let at_travel = pen_zoom_factor(PEN_ZOOM_TRAVEL);
    assert!(
        (at_travel - 2.0).abs() < 1e-3,
        "{PEN_ZOOM_TRAVEL} px doubles the sheet: {at_travel}"
    );

    // Half the travel is the square root of a doubling, twice it is two doublings — and neither depends on
    // where the sheet happens to be zoomed to, because nothing here asks.
    let half = pen_zoom_factor(PEN_ZOOM_TRAVEL / 2.0);
    assert!(
        (half * half - 2.0).abs() < 1e-3,
        "half the travel, applied twice, is a doubling: {half}"
    );
    let twice = pen_zoom_factor(PEN_ZOOM_TRAVEL * 2.0);
    assert!(
        (twice - 4.0).abs() < 1e-3,
        "twice the travel is four times the size: {twice}"
    );
}

/// The zoom is an exponent, so the same travel is always the same multiple.
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

/// The pen is quicker than a wheel, because a hand on a pen has far finer control than a wheel being spun.
///
/// A wheel is 1.25 a notch of 96 pixels, which is a doubling every 298; the pen is 200.
#[test]
fn the_pen_zooms_more_quickly_than_a_wheel() {
    let wheel_travel = 96.0 / 1.25_f32.log2();

    assert!(
        PEN_ZOOM_TRAVEL < wheel_travel,
        "the pen doubles in {PEN_ZOOM_TRAVEL} px and a wheel in {wheel_travel}"
    );

    // What the complaint was about: the old fixed per-pixel base needed 300 px for a doubling, so 200 px used
    // to fall short of one. It is now exactly one.
    let old_rate = 1.002_313_2_f32;
    let old_at_two_hundred = old_rate.powf(PEN_ZOOM_TRAVEL);
    assert!(
        old_at_two_hundred < 2.0,
        "the old rate needed 300 px to double: {old_at_two_hundred}"
    );
    assert!(
        (pen_zoom_factor(PEN_ZOOM_TRAVEL) - 2.0).abs() < 1e-3,
        "and 200 px now does it in one"
    );
}
