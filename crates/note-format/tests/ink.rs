//! The ink, from the outside: a stroke's colour rule, the geometry derived from its points, and the
//! detail ladder a renderer climbs.
//!
//! This is the part of the format a second application reproduces *in its own drawing code*, so the
//! assertions are about facts rather than about pixels: a colour with no top byte is opaque, a closed
//! stroke has bounds and an outline, the eraser hits the line between two points, and turning the zoom
//! up never moves the ink — it only cuts its curves more finely.

use note_format::ink::{
    alpha_of, densified, detail_of, detail_zoom, ribbon_outline, steadied, steps_for, with_alpha,
    InkPoint, Stroke, Tip,
};

/// A straight line from `from` to `to`, as the pen would leave it.
fn line(from: (f32, f32), to: (f32, f32)) -> Stroke {
    let mut stroke = Stroke::new(InkPoint::new(from.0, from.1, 2.0), 0x1B_1B_1F);

    stroke.points.push(InkPoint::new(to.0, to.1, 2.0));
    stroke.close();
    stroke
}

/// A colour with no top byte is opaque; one with a top byte carries its alpha there.
///
/// The exception is the whole rule, and it exists because every stroke ever written in a note is a
/// plain `0xRRGGBB`. Reading the top byte as an alpha unconditionally would make all of that ink
/// invisible — which is why this is the first thing pinned about the format.
#[test]
fn a_colour_with_no_top_byte_is_opaque() {
    assert_eq!(alpha_of(0x1B_1B_1F), 1.0);
    assert_eq!(alpha_of(0x00_FF_FF_FF), 1.0);
    assert_eq!(alpha_of(0xFF_1B_1B_1F), 1.0);
    assert_eq!(alpha_of(0x80_1B_1B_1F), 128.0 / 255.0);

    // A highlighter's colour is stamped with its alpha, and the RGB survives untouched.
    assert_eq!(with_alpha(0x1B_1B_1F, 0.0), 0x00_1B_1B_1F);
    assert_eq!(with_alpha(0x1B_1B_1F, 1.0), 0xFF_1B_1B_1F);
    assert!((alpha_of(with_alpha(0x1B_1B_1F, 0.35)) - 0.35).abs() < 0.01);

    // Clamped rather than wrapped: an alpha of 2 is an opaque stroke, not a different byte.
    assert_eq!(with_alpha(0x1B_1B_1F, 2.0), 0xFF_1B_1B_1F);
    assert_eq!(with_alpha(0x1B_1B_1F, -1.0), 0x00_1B_1B_1F);
}

/// A closed stroke has the geometry a renderer needs, and an empty one has none of it.
#[test]
fn a_closed_stroke_knows_its_bounds_and_its_outline() {
    let stroke = line((10.0, 20.0), (110.0, 20.0));

    assert!(!stroke.is_empty());
    assert_eq!(stroke.bounds[0], 10.0);
    assert_eq!(stroke.bounds[1], 20.0);
    assert_eq!(stroke.bounds[2], 110.0);
    assert_eq!(stroke.bounds[3], 20.0);
    assert!(!stroke.outline.is_empty(), "a line has an outline");

    // A ribbon around a horizontal line stays on the line's own rows; the width is what it spreads by.
    for vertex in &stroke.outline {
        assert!(
            (vertex[1] - 20.0).abs() <= 3.0,
            "{vertex:?} is off the line"
        );
    }

    let mut empty = Stroke::new(InkPoint::new(1.0, 2.0, 2.0), 0x1B_1B_1F);
    empty.points.clear();
    empty.close();

    assert!(empty.is_empty());
    assert_eq!(empty.bounds, [0.0; 4]);
    assert!(empty.outline.is_empty());
}

/// The derived geometry is not the ink: two strokes with the same points and colour are the same ink
/// however they were built, and that is what a history is allowed to rely on.
#[test]
fn the_same_ink_is_the_same_at_every_zoom() {
    // A short line on purpose: a long one is already cut into as many pieces as a stroke is allowed
    // (see `steps_for`), so the two zooms would agree and the test would prove nothing.
    let at_one = line((0.0, 0.0), (8.0, 8.0));

    let mut at_sixteen = at_one.clone();
    at_sixteen.close_at(16.0);

    assert!(
        at_one.same_ink(&at_sixteen),
        "the points moved with the zoom"
    );
    assert_eq!(at_one.points, at_sixteen.points);
    assert!(
        at_sixteen.outline.len() > at_one.outline.len(),
        "a stroke drawn large is cut more finely"
    );

    let mut recoloured = at_one.clone();
    recoloured.color = 0xCC_22_22;
    assert!(!at_one.same_ink(&recoloured), "a colour is ink");
}

/// The eraser tests the *line*, not only the points the resampler kept.
///
/// A fast stroke is two points a hundred pixels apart, and an eraser dragged across the middle of it
/// is nowhere near either. This is the bug the segment test exists for.
#[test]
fn the_eraser_hits_the_line_between_two_points() {
    let stroke = line((10.0, 10.0), (110.0, 10.0));

    assert!(stroke.hits(60.0, 10.0, 1.0), "the middle of the line");
    assert!(stroke.hits(10.0, 10.0, 0.5), "the nib's first point");
    assert!(!stroke.hits(60.0, 50.0, 1.0), "a page away from it");
    assert!(!stroke.hits(200.0, 10.0, 1.0), "past the end of it");
    assert!(stroke.hits(200.0, 10.0, 95.0), "a radius that reaches it");

    let mut empty = Stroke::new(InkPoint::new(0.0, 0.0, 2.0), 0x1B_1B_1F);
    empty.points.clear();
    assert!(!empty.hits(0.0, 0.0, 100.0), "nothing to hit");
}

/// Moving a stroke moves what can be erased, because the geometry is derived from the points.
#[test]
fn a_moved_stroke_hits_where_it_is_now() {
    let mut stroke = line((10.0, 10.0), (110.0, 10.0));
    stroke.translate((0.0, 100.0), detail_zoom(1.0));

    assert!(!stroke.hits(60.0, 10.0, 1.0), "it is not there any more");
    assert!(stroke.hits(60.0, 110.0, 1.0), "it is here");
    assert_eq!(stroke.bounds[1], 110.0);
    assert_eq!(stroke.bounds[3], 110.0);
}

/// Culling answers from the same bounds, and never says yes about a page it is not on.
#[test]
fn a_stroke_is_seen_only_inside_the_rectangle_it_is_in() {
    let stroke = line((100.0, 100.0), (200.0, 100.0));

    assert!(stroke.visible_in([0.0, 0.0, 400.0, 400.0]));
    assert!(!stroke.visible_in([0.0, 0.0, 99.0, 99.0]), "left of it");
    assert!(
        !stroke.visible_in([201.0, 0.0, 400.0, 400.0]),
        "right of it"
    );
    assert!(!stroke.visible_in([0.0, 200.0, 400.0, 400.0]), "below it");
}

/// The detail ladder moves in rungs, and a rung is a fixed point of the ladder.
///
/// A page is rebuilt when the rung changes, not on every frame of a pinch — so a rung handed back to
/// the ladder has to land on itself, or the rebuild happens forever.
#[test]
fn the_detail_ladder_rounds_up_to_a_rung() {
    assert_eq!(detail_zoom(1.0), 1.0);
    assert_eq!(detail_zoom(1.5), 1.5);
    assert_eq!(detail_zoom(0.0), 1.0, "a window with no zoom draws at 1:1");
    assert_eq!(detail_zoom(f32::NAN), 1.0);
    assert!(detail_zoom(1.2) >= 1.4, "rounded up, not down");
    assert_eq!(detail_zoom(1.2), detail_zoom(1.4), "one rung, one detail");

    // The rungs are 1.5 apart, so these are the first few of them.
    for rung in [1.0_f32, 1.5, 2.25, 3.375, 5.0625, 7.593_75] {
        assert!(
            (detail_zoom(rung) - rung).abs() < 1e-3,
            "{rung} is not a fixed point of the ladder"
        );
    }

    assert_eq!(detail_of(None), detail_zoom(1.0));
    assert_eq!(detail_of(Some(2.25)), 2.25);
}

/// A finer detail cuts a segment into more pieces, and never into fewer than one.
#[test]
fn a_finer_detail_cuts_the_line_into_more_pieces() {
    // Four pixels of line: short enough that neither zoom has reached the cap a segment is held to.
    let from = InkPoint::new(0.0, 0.0, 2.0);
    let to = InkPoint::new(4.0, 0.0, 2.0);

    let coarse = steps_for(from, to, detail_zoom(1.0));
    let fine = steps_for(from, to, detail_zoom(16.0));

    assert!(coarse >= 1, "a segment is at least one piece");
    assert!(
        fine > coarse,
        "{fine} pieces at 16x is not more than {coarse} at 1x"
    );

    let points = vec![
        InkPoint::new(0.0, 0.0, 2.0),
        InkPoint::new(2.0, 4.0, 2.0),
        InkPoint::new(4.0, 0.0, 2.0),
    ];

    assert!(steadied(&points).len() >= 3, "the ends are kept");
    assert!(densified(&points, detail_zoom(16.0), Tip::Curved).len() > points.len());
    assert!(!ribbon_outline(&points, detail_zoom(16.0), Tip::Curved).is_empty());
}

/// A stroke the pen has not lifted from draws its newest segment straight.
///
/// A curve through a point needs the point *after* it, and the newest reading of a stroke under the
/// nib has none — drawn from its neighbours the ink would move under the pen as the user writes.
#[test]
fn a_stroke_still_under_the_nib_draws_its_last_segment_straight() {
    let mut live = Stroke::new(InkPoint::new(0.0, 0.0, 2.0), 0x1B_1B_1F);
    for step in 1..8 {
        live.points.push(InkPoint::new(step as f32 * 8.0, 0.0, 2.0));
    }

    live.close_live(1.0);
    let straight = live.outline.clone();

    let mut finished = live.clone();
    finished.close_at(1.0);

    assert!(!straight.is_empty(), "a live stroke is still drawn");
    assert_eq!(
        live.points, finished.points,
        "closing is not a change of ink"
    );
    assert!(
        straight.len() <= finished.outline.len(),
        "a straight last segment needs no more vertices than a curve"
    );

    // And the stroke under the nib is not the stroke it will be: closing it again is what a frame
    // does when the pen lifts.
    finished.close();
    assert!(finished.same_ink(&live));
}
