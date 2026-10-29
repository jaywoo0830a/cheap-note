//! The paper, from the outside: the sizes and styles a note can be written on, and the names they
//! are stored under.
//!
//! What this file is really pinning is that a sheet size is a *physical* fact. A second
//! implementation reads `"A4"` out of a note's settings and has to arrive at the same rectangle on
//! screen — so the millimetres are asserted as millimetres, not as "some numbers", and the way a
//! size becomes pixels is asserted against the one constant both sides have to agree on.

use note_format::paper::{CanvasSize, CanvasStyle, PIXELS_PER_MM};

/// Every size the bar offers, and every one comes back from its own label.
///
/// The label is what a note stores, so this is the round trip a note does on every open: the row
/// says "A5", the chooser has to put A5 in hand rather than the nearest thing or nothing at all.
#[test]
fn every_size_has_a_label_and_comes_back_from_it() {
    for size in CanvasSize::ALL {
        let label = size.label();

        assert!(!label.is_empty(), "{size:?} has no label");
        assert_eq!(
            CanvasSize::from_label(label),
            Some(size),
            "{label} does not name {size:?}"
        );
    }
}

/// No two sizes share a label, and a label that is not one of them names nothing.
///
/// The lookup is by name rather than by position for exactly this reason: a size inserted in the
/// middle of the list must not rename a choice that was already made, and a stored label must never
/// resolve to the wrong sheet.
#[test]
fn a_label_names_one_size_or_none() {
    for (index, size) in CanvasSize::ALL.iter().enumerate() {
        for other in &CanvasSize::ALL[index + 1..] {
            assert_ne!(
                size.label(),
                other.label(),
                "{size:?} and {other:?} share a label"
            );
        }
    }

    assert_eq!(CanvasSize::from_label("A0"), None);
    assert_eq!(CanvasSize::from_label(""), None);
    assert_eq!(CanvasSize::from_label("a4"), None, "labels are exact");
}

/// The sizes are paper: the numbers are millimetres, and they are the ones ISO and US Letter say.
///
/// Pinned as *values*, because they are the format's meaning rather than this build's preference. A
/// note written on A4 is about 210 by 297 millimetres of paper, and a second application that
/// decided 200 by 280 would put the same ink on a different sheet.
#[test]
fn a_size_is_a_physical_measurement() {
    assert_eq!(CanvasSize::A4.millimetres(), (210.0, 297.0));
    assert_eq!(CanvasSize::A5.millimetres(), (148.0, 210.0));
    assert_eq!(CanvasSize::Letter.millimetres(), (215.9, 279.4));
    assert_eq!(CanvasSize::Legal.millimetres(), (215.9, 355.6));
    assert_eq!(CanvasSize::Square.millimetres(), (200.0, 200.0));

    let (a4_width, a4_height) = CanvasSize::A4.millimetres();
    assert_eq!(
        CanvasSize::Wide.millimetres(),
        (a4_height, a4_width),
        "Wide is A4 on its side"
    );

    let (square_width, square_height) = CanvasSize::Square.millimetres();
    assert_eq!(square_width, square_height);
}

/// A size becomes pixels one way: the drawn width is its millimetres at the app's own scale.
///
/// This is the number the ink is stored against — a stroke's width in logical pixels is a width in
/// millimetres only because of it — so it is asserted rather than assumed, and the aspect ratio is
/// asserted to follow from the same measurement.
#[test]
fn a_size_becomes_pixels_at_one_scale() {
    for size in CanvasSize::ALL {
        let (width_mm, height_mm) = size.millimetres();

        assert_eq!(
            size.display_width(),
            width_mm * PIXELS_PER_MM,
            "{size:?}'s drawn width"
        );
        assert!(
            (size.aspect() - height_mm / width_mm).abs() < 1e-6,
            "{size:?}'s aspect"
        );
        assert_eq!(size.display_size(600.0), (600.0, 600.0 * size.aspect()));
    }

    // A4 at this scale is the 720-pixel sheet the default window was chosen around.
    assert!((CanvasSize::A4.display_width() - 720.0).abs() < 1e-3);
}

/// What is printed on the sheet: a label that comes back, and a spacing only where there is one.
#[test]
fn every_style_has_a_label_and_a_spacing() {
    for style in CanvasStyle::ALL {
        assert_eq!(
            CanvasStyle::from_label(style.label()),
            Some(style),
            "{} does not name {style:?}",
            style.label()
        );

        match style {
            CanvasStyle::Plain => assert_eq!(style.spacing(), None, "a blank sheet has no rules"),
            _ => {
                let spacing = style.spacing().expect("a ruled sheet has a spacing");
                assert!(spacing > 0.0, "{style:?} has a spacing of {spacing}");
            }
        }
    }

    assert_eq!(CanvasStyle::from_label("graph"), None);
}

/// The stored form is the variant's own name, and reading it back gives the same paper.
///
/// A note's settings already hold a page list written as JSON (see [`note_format::pages`]), and the
/// sheet is stored the same way: what is pinned here is the *spelling*, because that is what another
/// implementation has to parse.
#[test]
fn a_sheet_survives_its_json() {
    for size in CanvasSize::ALL {
        let json = serde_json::to_string(&size).expect("a size as JSON");
        assert_eq!(json, format!("\"{}\"", size.label()));
        assert_eq!(serde_json::from_str::<CanvasSize>(&json).ok(), Some(size));
    }

    for style in CanvasStyle::ALL {
        let json = serde_json::to_string(&style).expect("a style as JSON");
        assert_eq!(json, format!("\"{}\"", style.label()));
        assert_eq!(serde_json::from_str::<CanvasStyle>(&json).ok(), Some(style));
    }
}
