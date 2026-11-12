use super::*;
use crate::input::{Phase, Point, Reading, Tilt};

/// A reading at a position with the given phase and lean.
fn reading(phase: Phase, tilt: Option<(f32, f32)>) -> Reading {
    Reading {
        phase,
        id: 1,
        pixel: Point::new(100.0, 200.0),
        tilt: tilt.map(|(x, y)| Tilt::new(x, y)),
        ..Reading::default()
    }
}

/// The cursor for a reading.
fn cursor(phase: Phase, tilt: Option<(f32, f32)>) -> Option<PenCursor> {
    PenCursor::from_sample(reading(phase, tilt), 1.0)
}

/// The distance between two points.
fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// A cursor left behind where the pen was last seen would be a lie, so the phases that end a
/// visit draw nothing.
#[test]
fn a_cursor_is_only_drawn_while_the_pen_is_here() {
    for phase in [
        Phase::Enter,
        Phase::Hover,
        Phase::Down,
        Phase::Move,
        Phase::Up,
    ] {
        assert!(
            cursor(phase, None).is_some(),
            "{phase:?} is a pen to draw a cursor for"
        );
    }

    for phase in [Phase::Idle, Phase::Leave, Phase::Cancel] {
        assert!(
            cursor(phase, None).is_none(),
            "{phase:?} leaves no cursor behind"
        );
    }
}

/// The body lies in the direction the pen leans, in window coordinates.
///
/// The signs are the convention the whole feature rests on: `tilt_x` positive is a lean to the
/// right, and `tilt_y` positive is a lean toward the user, which in window coordinates is
/// *down*.
#[test]
fn the_body_lies_where_the_pen_leans() {
    let cases = [
        ((45.0, 0.0), [1.0, 0.0]),
        ((-45.0, 0.0), [-1.0, 0.0]),
        ((0.0, 45.0), [0.0, 1.0]),
        ((0.0, -45.0), [0.0, -1.0]),
    ];

    for (tilt, expected) in cases {
        let direction = cursor(Phase::Hover, Some(tilt))
            .expect("a hovering pen has a cursor")
            .lean_direction()
            .expect("a leaning pen has a direction");

        assert!(
            (direction[0] - expected[0]).abs() < 1e-4 && (direction[1] - expected[1]).abs() < 1e-4,
            "a lean of {tilt:?} points {direction:?}, not {expected:?}"
        );
    }
}

/// A lean of 45 degrees on both axes lies between the two, and is a unit vector.
#[test]
fn a_lean_on_both_axes_points_between_them() {
    let direction = cursor(Phase::Hover, Some((45.0, 45.0)))
        .expect("a hovering pen has a cursor")
        .lean_direction()
        .expect("a leaning pen has a direction");

    let diagonal = 1.0 / 2.0_f32.sqrt();
    assert!((direction[0] - diagonal).abs() < 1e-4);
    assert!((direction[1] - diagonal).abs() < 1e-4);

    let length = (direction[0] * direction[0] + direction[1] * direction[1]).sqrt();
    assert!(
        (length - 1.0).abs() < 1e-4,
        "the direction is a unit vector"
    );
}

/// A pen standing straight up has no body to draw: an upright stick projects to a point.
#[test]
fn an_upright_pen_shows_no_body() {
    for tilt in [(0.0, 0.0), (2.0, -1.0)] {
        let cursor = cursor(Phase::Hover, Some(tilt)).expect("a hovering pen has a cursor");

        assert!(
            cursor.body_shape().is_none(),
            "a lean of {tilt:?} is too upright to draw"
        );
        assert_eq!(cursor.body_length(), 0.0);
        assert_eq!(
            cursor.body_fade(),
            0.0,
            "and an upright pen's body has no weight to fade in from"
        );
    }
}

/// A pen that reports no tilt at all still gets a cursor: the nib mark does not depend on it.
#[test]
fn a_pen_without_a_tilt_sensor_still_has_a_cursor() {
    let cursor = cursor(Phase::Hover, None).expect("a hovering pen has a cursor");

    assert!(cursor.tilt().is_none());
    assert_eq!(
        cursor.lean_degrees(),
        0.0,
        "nothing measured is a zero lean"
    );
    assert!(cursor.body_shape().is_none());
    assert_eq!(cursor.position(), [100.0, 200.0]);
}

/// The drawn length is the lean: longer the flatter the pen lies, and never longer than a pen.
#[test]
fn the_body_grows_as_the_pen_lies_down() {
    let lengths: Vec<f32> = [10.0, 30.0, 60.0, 85.0]
        .into_iter()
        .map(|lean| {
            cursor(Phase::Hover, Some((lean, 0.0)))
                .expect("a hovering pen has a cursor")
                .body_length()
        })
        .collect();

    for pair in lengths.windows(2) {
        assert!(
            pair[1] > pair[0],
            "a flatter pen draws a longer body: {lengths:?}"
        );
    }

    assert!(
        lengths[3] <= BODY_LENGTH,
        "the body never exceeds the pen's own length"
    );
}

/// Where the drawn shape actually ends: a quadratic comes halfway to its control point at the
/// middle of itself, which is the apex of the cap.
fn apex(outline: &[[f32; 2]; 7], far: [f32; 2]) -> [f32; 2] {
    [
        far[0] + (outline[3][0] - far[0]) / 2.0,
        far[1] + (outline[3][1] - far[1]) / 2.0,
    ]
}

/// The body starts at the nib and widens away from it, and its far end is round rather than cut
/// off: the direction cannot be misread, and there is no corner left to catch the eye.
#[test]
fn the_body_is_a_taper_with_a_round_far_end() {
    let cursor = cursor(Phase::Hover, Some((60.0, 0.0))).expect("a hovering pen has one");
    let body = cursor.body_shape().expect("a leaning pen has a body");
    let outline = body.outline(0.0);
    let far = body.far();

    // Points 0 and 6 straddle the nib; points 2 and 4 straddle the far end.
    let nib_span = distance(outline[0], outline[6]);
    let tail_span = distance(outline[2], outline[4]);

    assert!(
        (nib_span - BODY_NIB_WIDTH).abs() < 1e-4,
        "the narrow end is the nib"
    );
    assert!(
        (tail_span - BODY_TAIL_WIDTH).abs() < 1e-4,
        "the far end is the width the shape is drawn at"
    );
    assert!(tail_span > nib_span, "the body widens away from the nib");

    // The flanks are curves through a control point out at the far end's width, most of the way
    // along: the edge leaves the nib almost straight and swells toward the far end, so the curve
    // passes outside the straight edge between the two ends. The control point alone would not
    // say that — a control exactly on that line still draws a straight edge — so what is pinned
    // is the side of the line the control fell on.
    let straight_mid = (outline[0][1] + outline[2][1]) / 2.0;
    assert!(
        outline[1][1] > straight_mid,
        "the flank bows outward rather than running straight"
    );

    // With a lean straight to the right, the body extends toward the lean, and the cap reaches
    // past that far end — the end is round, not cut off.
    assert!(
        outline[2][0] > outline[0][0],
        "the body extends toward the lean, not away from it"
    );
    assert!(
        apex(&outline, far)[0] > far[0],
        "the round end reaches past the far end instead of stopping at it"
    );
}

/// The soft edge is the same outline, a pixel wider all round, so the two shapes painted
/// together are one shape with a soft edge rather than two shapes.
#[test]
fn the_soft_edge_wraps_the_body() {
    let body = cursor(Phase::Hover, Some((50.0, 0.0)))
        .expect("a hovering pen has one")
        .body_shape()
        .expect("a leaning pen has a body");

    let tight = body.outline(0.0);
    let soft = body.outline(BODY_HALO_GROW);
    let far = body.far();

    let nib_grown = distance(soft[0], soft[6]) - distance(tight[0], tight[6]);
    let tail_grown = distance(soft[2], soft[4]) - distance(tight[2], tight[4]);
    let reached = distance(apex(&soft, far), apex(&tight, far));

    assert!(
        (nib_grown - BODY_HALO_GROW * 2.0).abs() < 1e-4,
        "the nib end moves out by the width of the soft edge"
    );
    assert!(
        (tail_grown - BODY_HALO_GROW * 2.0).abs() < 1e-4,
        "and so does the far end"
    );
    assert!(
        (reached - BODY_HALO_GROW).abs() < 1e-4,
        "the round end reaches that much further as well"
    );
}

/// The body arrives with the lean rather than popping into being at the threshold.
#[test]
fn the_body_arrives_as_its_lean_grows() {
    let fade = |lean: f32| {
        cursor(Phase::Hover, Some((lean, 0.0)))
            .expect("a hovering pen has one")
            .body_fade()
    };

    // The lean is measured through `tan`/`atan`, so the ends of the ramp are compared within a
    // hair rather than exactly: what is pinned is the ramp, not the last bit of a float.
    assert!(fade(MIN_LEAN_DEGREES) < 1e-4, "a body not drawn yet");
    assert!(
        (fade(FULL_LEAN_DEGREES) - 1.0).abs() < 1e-4,
        "and one fully drawn"
    );
    assert_eq!(fade(80.0), 1.0, "a pen laid flat is no stronger than that");

    let steps: Vec<f32> = [5.0, 8.0, 11.0, 14.0].into_iter().map(fade).collect();
    assert!(
        steps.windows(2).all(|pair| pair[1] > pair[0]),
        "the weight grows with the lean: {steps:?}"
    );
}

/// The soft layers are wider and fainter than what they sit under, and the nib mark stays the
/// strongest thing drawn: this is what keeps the cursor a hint rather than a blot.
///
/// Asserted against local bindings rather than against the constants directly, because these are
/// *constants*: `assert!(NIB_BLOOM_SPREAD > 0.0)` is a comparison the compiler folds before clippy ever
/// sees it, and a fold that came out false would stop the crate compiling rather than fail a test. Reading
/// them into `let`s keeps the relations as checks on values a test can actually be run over — and it is
/// the relations that matter, not the numbers.
#[test]
fn the_soft_layers_are_fainter_than_what_they_sit_under() {
    let (bloom_spread, bloom_alpha) = (NIB_BLOOM_SPREAD, NIB_BLOOM_ALPHA);
    let (halo_grow, halo_alpha) = (BODY_HALO_GROW, BODY_HALO_ALPHA);
    let (nib_alpha, body_alpha) = (NIB_ALPHA, BODY_ALPHA);
    let (radius_min, radius_max) = (NIB_RADIUS_MIN, NIB_RADIUS_MAX);

    assert!(bloom_spread > 0.0, "the bloom reaches past the mark");
    assert!(bloom_alpha < nib_alpha, "and is the fainter copy");
    assert!(halo_grow > 0.0, "the body's edge reaches outward");
    assert!(halo_alpha < body_alpha, "and is fainter than the body");
    assert!(
        body_alpha < nib_alpha,
        "the body never competes with the nib"
    );
    assert!(
        radius_min < radius_max,
        "the mark has a range of sizes to move in"
    );

    for alpha in [nib_alpha, bloom_alpha, body_alpha, halo_alpha] {
        assert!(
            (0.0..1.0).contains(&alpha),
            "{alpha} is not an opacity a shape can be painted with"
        );
    }
}

/// Whatever the window's DPI, the ghost lands on the ink.
#[test]
fn the_dpi_scale_is_applied_once() {
    let physical = Reading {
        phase: Phase::Hover,
        pixel: Point::new(200.0, 400.0),
        ..Reading::default()
    };

    let doubled = PenCursor::from_sample(physical, 2.0).expect("a hovering pen has one");
    assert_eq!(doubled.position(), [100.0, 200.0], "physical pixels halve");

    let nonsense = PenCursor::from_sample(physical, 0.0).expect("a hovering pen has one");
    assert_eq!(
        nonsense.position(),
        [200.0, 400.0],
        "a scale that makes no sense is 1:1 rather than a division by zero"
    );
}

/// The nib mark is half the line in hand, held between the two sizes the shape declares: the dot is the pen's
/// weight made visible, and no setting can make it invisible or fill the window.
#[test]
fn the_mark_is_half_the_line_in_hand() {
    assert!(
        (nib_radius_for_width(4.5) - 2.25).abs() < 1e-6,
        "half of the widest press the shipped pen has"
    );
    assert!(
        nib_radius_for_width(18.0) > nib_radius_for_width(4.5),
        "a heavier pen has a heavier dot"
    );
    assert_eq!(
        nib_radius_for_width(18.0),
        NIB_RADIUS_MAX,
        "and a highlighter's mark is half its own band"
    );
    assert_eq!(
        nib_radius_for_width(0.1),
        NIB_RADIUS_MIN,
        "a pen of nothing still has a dot"
    );
    assert_eq!(
        nib_radius_for_width(f32::NAN),
        NIB_RADIUS_MIN,
        "and a width that arrived as nonsense is not a hole in the window"
    );
}
