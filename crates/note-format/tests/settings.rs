//! The settings a note keeps, from the outside: how a pen is named and how a width comes out of it.
//!
//! There is no settings *file* in this format — every setting is one `meta` row in one note — so the
//! thing worth testing here is the arithmetic a second implementation has to reproduce exactly: the
//! pen's weight is a multiplier on the nib's own line, and a stroke's width is pressure and weight
//! applied in that order. A build that got the order the other way round would draw the same note
//! with different ink, which is the one thing the format promises not to do.

use note_format::paper::{CanvasSize, CanvasStyle};
use note_format::settings::{PenWeight, Settings};

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
