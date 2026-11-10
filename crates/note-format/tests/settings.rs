//! The settings a note keeps, from the outside: how a pen is named and how a width comes out of it.
//!
//! There is no settings *file* in this format — every setting is one `meta` row in one note — so the
//! thing worth testing here is the arithmetic a second implementation has to reproduce exactly: the
//! pen's weight is a multiplier on the nib's own line, and a stroke's width is pressure and weight
//! applied in that order. A build that got the order the other way round would draw the same note
//! with different ink, which is the one thing the format promises not to do.

use note_format::paper::{CanvasSize, CanvasStyle};
use note_format::settings::{PenWeight, Settings, FADE_CEILING};

/// The defaults are a note that has never been told anything — and they are a *sane* set.
///
/// Nothing here pins a preference: the numbers are this build's starting point and may be tuned. What
/// is asserted is what makes them a usable default at all — a plain colour rather than one with an
/// alpha in it (see [`note_format::ink::alpha_of`]), a pen whose thin end is not wider than its thick
/// end, and a sheet that exists.
#[test]
fn the_defaults_are_a_note_that_has_never_been_told_anything() {
    let settings = Settings::default();

    for (name, color) in [
        ("ink", settings.ink_color),
        ("highlighter", settings.highlighter_color),
        ("page", settings.page_color),
    ] {
        assert!(
            color <= 0x00FF_FFFF,
            "the default {name} colour {color:#08x} has an alpha in it"
        );
    }

    assert!(
        settings.min_width > 0.0 && settings.min_width <= settings.max_width,
        "the pen's widths are {}..{}",
        settings.min_width,
        settings.max_width
    );
    assert!(
        settings.no_pressure_width > 0.0,
        "a pen with no sensor would draw nothing"
    );
    assert!(settings.erase_radius > 0.0, "the eraser would miss");
    assert!(settings.resample_spacing >= 0.0);
    assert!(settings.smoothing_ms >= 0.0);
    assert!(settings.page_display_width > 0.0);
    assert!(settings.zoom > 0.0);

    assert!(CanvasSize::ALL.contains(&settings.canvas_size));
    assert!(CanvasStyle::ALL.contains(&settings.canvas_style));
}

/// A weight is named by its own multiplier, and by nothing else.
///
/// The note stores a *number* for the pen — a multiplier, meaningful whatever the nib is — so this is
/// the row's round trip. A row that is not one of the five is not the nearest pen: it is a row this
/// build does not understand, and answering with a guess is how a note comes back written in a pen
/// nobody chose.
#[test]
fn a_weight_is_named_by_its_own_multiplier() {
    for weight in PenWeight::ALL {
        assert_eq!(
            PenWeight::from_scale(&weight.scale().to_string()),
            Some(weight),
            "{} is not {}",
            weight.scale(),
            weight.scale()
        );
    }

    assert_eq!(PenWeight::from_scale("0"), None);
    assert_eq!(PenWeight::from_scale("3.5"), None);
    assert_eq!(PenWeight::from_scale("marker"), None);
    assert_eq!(PenWeight::from_scale(""), None);
}

/// The pens are ordered, and the spread is wide enough to be five tools rather than one.
///
/// A range that did not reach both a fine liner and a marker would make the setting a matter of taste
/// rather than a choice of instrument, so the direction and the span are both asserted.
#[test]
fn the_pens_are_ordered_by_how_heavy_they_are() {
    for pair in PenWeight::ALL.windows(2) {
        assert!(
            pair[0].scale() < pair[1].scale(),
            "{:?} is not lighter than {:?}",
            pair[0],
            pair[1]
        );
    }

    let lightest = PenWeight::ALL[0].scale();
    let heaviest = PenWeight::ALL[PenWeight::ALL.len() - 1].scale();
    assert!(
        heaviest / lightest >= 4.0,
        "the range is only {lightest}..{heaviest}"
    );
}

/// A pen is described in millimetres, and the label names the pen back.
///
/// The label is a measurement rather than a name — "0.58 mm", not "Fine" — so a chooser is a row of
/// numbers, and the number is read back the way it was written. The nib is the note's own because the
/// number is: the same pen is a different line on a note with a different nib.
#[test]
fn a_pen_is_described_in_millimetres_and_the_label_names_it_back() {
    let nib = Settings::default().no_pressure_width;

    for weight in PenWeight::ALL {
        let label = weight.describe(nib);

        assert!(label.ends_with(" mm"), "{label} is not a measurement");
        assert_eq!(
            PenWeight::from_label(&label, nib),
            Some(weight),
            "{label} does not name {weight:?}"
        );
        assert!(
            (weight.millimetres(nib) - weight.scale() * nib / note_format::paper::PIXELS_PER_MM)
                .abs()
                < 1e-6,
            "{weight:?} is measured on another millimetre"
        );
    }

    assert_eq!(
        PenWeight::from_label("Marker", nib),
        None,
        "a word is not a width"
    );
    assert_eq!(
        PenWeight::from_label("0.58", nib),
        None,
        "a unit is required"
    );
}

/// A pen with no pressure sensor draws at a constant width — not at zero.
///
/// The classic bug this pins: `None` pressure read as `0.0` collapses the line to nothing and makes a
/// perfectly good pen look broken. `None` is a *different* answer from "no pressure", and the weight
/// still applies to it, because a marker is a fat pen whichever hand holds it.
#[test]
fn a_pen_with_no_sensor_draws_at_its_flat_width() {
    for weight in PenWeight::ALL {
        let settings = Settings {
            pen_weight: weight,
            ..Settings::default()
        };

        assert_eq!(
            settings.width_for_pressure(None),
            settings.no_pressure_width * weight.scale()
        );
        assert!(
            settings.width_for_pressure(None) > 0.0,
            "{weight:?} with no sensor draws nothing"
        );
    }
}

/// Pressure moves the line between the two widths, and the weight is applied after it.
///
/// The order is the whole reason a weight is a multiplier: `min` and `max` are where the line starts
/// and ends, and the weight says how heavy the pen doing it is. Reversed, a marker would be thin under
/// a hard press — which is what this asserts cannot happen.
#[test]
fn pressure_moves_the_line_and_the_weight_multiplies_it() {
    let settings = Settings {
        pen_weight: PenWeight::Normal,
        ..Settings::default()
    };

    assert!((settings.width_for_pressure(Some(0.0)) - settings.min_width).abs() < 1e-6);
    assert!((settings.width_for_pressure(Some(1.0)) - settings.max_width).abs() < 1e-6);

    // Out of range is clamped rather than extrapolated: a driver that reports 1.2 is a happy pen,
    // not a pen twice as thick.
    assert_eq!(
        settings.width_for_pressure(Some(1.2)),
        settings.width_for_pressure(Some(1.0))
    );
    assert_eq!(
        settings.width_for_pressure(Some(-0.3)),
        settings.width_for_pressure(Some(0.0))
    );

    for weight in PenWeight::ALL {
        let heavy = Settings {
            pen_weight: weight,
            ..Settings::default()
        };

        assert!(
            heavy.width_for_pressure(Some(1.0)) >= heavy.width_for_pressure(Some(0.0)),
            "{weight:?} draws thinner under a harder press"
        );
        assert!(
            (heavy.width_for_pressure(Some(0.5))
                - (settings.width_for_pressure(Some(0.5)) * weight.scale()))
            .abs()
                < 1e-6
        );
    }
}

/// A locked zoom holds the gestures off, and nothing else.
#[test]
fn a_locked_zoom_holds_off_the_gestures() {
    let locked = Settings {
        zoom_locked: true,
        ..Settings::default()
    };

    assert!(!locked.gestures_zoom());
    assert!(Settings::default().gestures_zoom());
}

/// A light hand draws a *thin* line, not a middling one: the pressure curve is the pencil's.
///
/// This is the difference between a pencil and a pen that happens to be thin. Laid on a straight run between the
/// nib's two widths, a third of full pressure would draw at `2.05 px` of the `1.0..4.5` on offer — more than half
/// way, so a hand that is barely touching the paper writes as heavily as one pressing half as hard. A pencil does
/// not do that: the nib only has to graze the paper to mark it, and the rest of the press is what thickens the
/// line. Measured here as the shape of the road, since that is the thing that changed.
#[test]
fn a_light_hand_draws_a_thin_line() {
    let settings = Settings {
        pen_weight: PenWeight::Normal,
        ..Settings::default()
    };

    let third = settings.width_for_pressure(Some(1.0 / 3.0));
    let half = settings.width_for_pressure(Some(0.5));
    let (least, most) = (settings.min_width, settings.max_width);
    let straight = least + (most - least) / 3.0;

    assert!(
        third < straight * 0.75,
        "a third of the pressure draws {}px, against {}px on a straight run",
        third,
        straight
    );
    assert!(
        half < least + (most - least) / 2.0,
        "half pressure is below the midpoint: {half}px"
    );

    // The ends of the road do not move. What the curve changes is the shape between them, so full and no
    // pressure still land exactly where the two widths say — a pencil pressed hard is as thick as the nib is,
    // and the lightest graze is as thin.
    assert!((settings.width_for_pressure(Some(0.0)) - least).abs() < 1e-6);
    assert!((settings.width_for_pressure(Some(1.0)) - most).abs() < 1e-6);

    // And it only ever rises: no pressure is ever thinner than more pressure. A curve that crossed itself would
    // draw a line that thinned out as the hand pressed harder.
    let mut last = 0.0;

    for step in 0..=20 {
        let width = settings.width_for_pressure(Some(step as f32 / 20.0));

        assert!(width >= last, "the line thinned as the hand pressed harder");
        last = width;
    }
}

/// Soft ink: pressure decides how much of the colour reaches the page, and the switch decides whether it does.
///
/// The three things that have to hold at once. A soft hand writes paler than a firm one; a firm one reaches the
/// colour exactly, because there is no ink darker than the colour chosen; and with the switch off nothing fades at
/// all, whatever the hand did — which is what makes it a switch rather than a slider.
#[test]
fn soft_ink_fades_a_light_touch_and_only_when_asked() {
    let on = Settings {
        pen_weight: PenWeight::Normal,
        pressure_fades: true,
        ..Settings::default()
    };
    let off = Settings {
        pressure_fades: false,
        ..on.clone()
    };

    // A nib drifting on the paper, well below the threshold. The value matters: `0.2` is the threshold itself now,
    // so the pale end of the range is the play above a resting nib rather than anything a reader writes at.
    let (soft, firm) = (
        on.alpha_for_pressure(Some(0.05)),
        on.alpha_for_pressure(Some(1.0)),
    );

    assert!(soft < 0.9, "a light hand writes pale: {soft}");
    assert_eq!(firm, 1.0, "a firm one reaches the colour exactly");
    assert!(soft < firm);

    // The switch is the whole difference: the same readings, and no fade at all.
    assert_eq!(off.alpha_for_pressure(Some(0.05)), 1.0);
    assert_eq!(off.alpha_for_pressure(Some(1.0)), 1.0);

    // A pen with no sensor reports nothing for this to fade, so it keeps its weight's own alpha — a flat pen's
    // line must not wobble because a fade was asked for.
    assert_eq!(on.alpha_for_pressure(None), PenWeight::Normal.alpha());
}

/// Pressing past the threshold makes the line solid, and it stays solid however much harder the hand presses.
///
/// This is the shape the fade is meant to have: ordinary writing sits below the threshold and is pale, and a
/// deliberate press is *fully* opaque rather than almost. A curve that climbed to one only at the very top of the
/// range would spend its upper reach in shades nobody can tell apart, and the ink would look as though it never
/// quite arrived.
#[test]
fn pressing_past_the_threshold_makes_the_line_solid() {
    let settings = Settings {
        pen_weight: PenWeight::Normal,
        pressure_fades: true,
        ..Settings::default()
    };

    // Everything below it fades, and it is a fade rather than a step.
    let mut pressure = 0.0;

    while pressure < FADE_CEILING {
        let alpha = settings.alpha_for_pressure(Some(pressure));

        assert!(
            alpha < 1.0,
            "a press at {pressure} should still be pale, not {alpha}"
        );
        pressure += 0.01;
    }

    // At it, and every harder press above it, is solid — exactly one, not nearly one.
    pressure = FADE_CEILING;

    while pressure <= 1.0 {
        assert_eq!(
            settings.alpha_for_pressure(Some(pressure)),
            1.0,
            "a press at {pressure} is past the threshold and should be solid"
        );
        pressure += 0.01;
    }
}

/// The threshold is inside the range a hand actually presses, which is the whole reason it works.
///
/// `NSEvent.pressure` is normalised and Apple is clear that it does not measure weight. Two thresholds were tried
/// and both were reported from use as never getting solid: `0.7` left the hardest press on `0.96`, and `0.55` was
/// still above where a hand writes. The scale is not a description of a hand's range, so the threshold has to sit
/// low on it rather than near the top.
///
/// So this pins the *reachability*, and against a value far below the scale's ceiling: a press anywhere near what
/// a reader produces has to be solid, not by a hair. Checked when the file is compiled, because both sides are
/// constants and there is nothing to wait for a test run to find out.
#[test]
fn a_press_a_reader_makes_is_solid() {
    let settings = Settings {
        pen_weight: PenWeight::Normal,
        pressure_fades: true,
        ..Settings::default()
    };

    const _: () = assert!(
        FADE_CEILING <= 0.3,
        "the threshold is above where a hand presses, so nobody would ever reach it"
    );

    // At the threshold and every press above it, a reader's press is solid.
    for pressure in [FADE_CEILING, 0.4, 0.5, 0.7, 1.0] {
        assert_eq!(
            settings.alpha_for_pressure(Some(pressure)),
            1.0,
            "a press at {pressure} should be solid"
        );
    }

    // And the band below the threshold is the play in a nib, not a band a reader writes in: half of it is still
    // visibly short of the colour.
    let resting = settings.alpha_for_pressure(Some(FADE_CEILING / 2.0));

    assert!(
        resting < 0.95,
        "a nib barely touching the paper drew {resting}, which is not a fade"
    );
}

/// Below the threshold the fade is proportional to the hand, not curved.
///
/// A reader pressing half as hard gets half the fade, so the ink answers the hand predictably. Measured as the
/// midpoint between the faintest touch and the threshold landing half-way up the climb.
#[test]
fn below_the_threshold_the_fade_is_proportional() {
    let settings = Settings {
        pen_weight: PenWeight::Normal,
        pressure_fades: true,
        ..Settings::default()
    };

    let (none, half, full) = (
        settings.alpha_for_pressure(Some(0.0)),
        settings.alpha_for_pressure(Some(FADE_CEILING / 2.0)),
        settings.alpha_for_pressure(Some(FADE_CEILING)),
    );

    assert_eq!(full, 1.0, "the threshold itself is solid");

    let middle = (none + full) / 2.0;

    assert!(
        (half - middle).abs() < 0.02,
        "half the way to the threshold should be half the fade: {half} against {middle}"
    );

    // And it rises with the hand rather than jumping about.
    let mut last = none;
    let mut pressure = 0.0;

    while pressure < FADE_CEILING {
        let alpha = settings.alpha_for_pressure(Some(pressure));

        assert!(alpha >= last - 1e-6, "the fade went backwards");
        last = alpha;
        pressure += 0.01;
    }
}

/// The fade only ever takes the line *below* what its weight allows, never above it.
///
/// The fine pen is already a pencil, so soft ink must not make a light press on it darker than the pencil's own
/// alpha — and a full press on any pen must not exceed one, because there is no ink more opaque than opaque.
#[test]
fn soft_ink_never_goes_above_the_weight() {
    for weight in PenWeight::ALL {
        let settings = Settings {
            pen_weight: weight,
            pressure_fades: true,
            ..Settings::default()
        };
        let ceiling = weight.alpha();

        for step in 0..=20 {
            let alpha = settings.alpha_for_pressure(Some(step as f32 / 20.0));

            assert!(
                alpha <= ceiling + 1e-6,
                "{weight:?} at pressure {} drew at {alpha}, above its own {ceiling}",
                step as f32 / 20.0
            );
            assert!(alpha > 0.0, "{weight:?} drew nothing at all");
        }

        assert_eq!(
            settings.alpha_for_pressure(Some(1.0)),
            ceiling,
            "{weight:?} at full pressure is its own alpha"
        );
    }
}

/// A graze still leaves a mark.
///
/// The floor's whole job: at no pressure at all the line is faint but present, because a stroke nobody can see is
/// not a light stroke. Pinned against the pencil's own alpha so that the floor cannot rise above it unnoticed.
#[test]
fn the_lightest_touch_still_leaves_a_mark() {
    let settings = Settings {
        pen_weight: PenWeight::Normal,
        pressure_fades: true,
        ..Settings::default()
    };

    let graze = settings.alpha_for_pressure(Some(0.0));

    assert!(graze > 0.0, "the lightest touch drew nothing");
    assert!(
        graze >= 0.4,
        "a graze at {graze} is too faint to read as a line"
    );
    assert!(graze < 1.0, "a graze should be fainter than a press");
}

/// The fine pen is the pencil: its ink is the colour chosen, laid down at less than full strength.
///
/// Every other pen is opaque, and the *number* it is stored as keeps no top byte — which is what
/// [`alpha_of`] reads as fully opaque, and what every stroke written before this existed is. Soft ink is a
/// ceiling on top of this rather than a replacement for it, so a weight's own alpha is still the most the line can
/// reach.
#[test]
fn the_fine_pen_is_the_pencil_and_the_rest_are_opaque() {
    assert!(
        PenWeight::Fine.alpha() < 1.0,
        "the fine pen should read as graphite, not ink"
    );

    // Lighter than a pen, and still dark enough to write a margin note in — a hair over four fifths of the
    // colour reaching the page leaves a soft grey rather than a ghost.
    assert!(
        PenWeight::Fine.alpha() >= 0.75,
        "a fine pen at {} is too pale to read",
        PenWeight::Fine.alpha()
    );

    for weight in PenWeight::ALL {
        if weight == PenWeight::Fine {
            continue;
        }

        assert_eq!(
            weight.alpha(),
            1.0,
            "{weight:?} should be opaque: it is a pen, not a pencil"
        );
    }
}
