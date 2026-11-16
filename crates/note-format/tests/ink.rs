//! The ink, from the outside: a stroke's colour rule, the geometry derived from its points, and the
//! detail ladder a renderer climbs.
//!
//! This is the part of the format a second application reproduces *in its own drawing code*, so the
//! assertions are about facts rather than about pixels: a colour with no top byte is opaque, a closed
//! stroke has bounds and an outline, the eraser hits the line between two points, and turning the zoom
//! up never moves the ink — it only cuts its curves more finely.

use note_format::ink::{
    alpha_of, densified, detail_of, detail_zoom, outline_of, steadied, steps_for, with_alpha,
    Bounds, InkPoint, Stroke, Tip,
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
    assert_eq!(
        stroke.bounds,
        Bounds::new((10.0, 20.0), (110.0, 20.0)),
        "the box is the line's own two ends"
    );
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
    assert!(
        empty.bounds.is_empty(),
        "a stroke with no points covers nothing, rather than a box at the origin"
    );
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
    assert_eq!(
        stroke.bounds,
        Bounds::new((10.0, 110.0), (110.0, 110.0)),
        "and its box moved with it"
    );
}

/// Culling answers from the same bounds, and never says yes about a page it is not on.
#[test]
fn a_stroke_is_seen_only_inside_the_rectangle_it_is_in() {
    let stroke = line((100.0, 100.0), (200.0, 100.0));

    assert!(stroke.visible_in(Bounds::new((0.0, 0.0), (400.0, 400.0))));
    assert!(
        !stroke.visible_in(Bounds::new((0.0, 0.0), (99.0, 99.0))),
        "left of it"
    );
    assert!(
        !stroke.visible_in(Bounds::new((201.0, 0.0), (400.0, 400.0))),
        "right of it"
    );
    assert!(
        !stroke.visible_in(Bounds::new((0.0, 200.0), (400.0, 400.0))),
        "below it"
    );
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
    assert!(!outline_of(&points, detail_zoom(16.0), Tip::Curved).is_empty());
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

/// A pen held down and turned on the spot leaves a dot, and not a ring with a hole in it.
///
/// This is the gesture a reader makes when they scrub a spot with a highlighter: the nib stays within
/// a fraction of a pixel of one place while the readings pile up and the pen turns. The outline is
/// built by offsetting each point along the normal through its neighbours, and a point whose
/// neighbours are indistinguishable from it has no direction to take a normal from — so offsetting a
/// whole turning path walks one edge *outside* the band and the other *inside* it. Joined into one
/// polygon that is an annulus, and an annulus is filled by the nonzero rule as a ring with a hole in
/// the middle.
///
/// The assertion is about the shape rather than the pixels: what the outline encloses is a disc the
/// size of the nib, so its area is that disc's and not a ring's.
#[test]
fn a_pen_turned_on_the_spot_leaves_a_dot() {
    let width = 18.0_f32;
    let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, width), 0x59FF_EB3B);

    // One reading every ten degrees, a fifth of a pixel from the centre: a hand turning a pen.
    for step in 1..=36 {
        let angle = step as f32 / 36.0 * std::f32::consts::TAU;
        stroke
            .points
            .push(InkPoint::new(angle.cos() * 0.2, angle.sin() * 0.2, width));
    }

    stroke.close();

    let area = polygon_area(&stroke.outline);
    let radius = width * 0.5;
    let disc = std::f32::consts::PI * radius * radius;

    assert!(
        area > disc * 0.85,
        "a turned nib encloses {area:.0}px2, not the {disc:.0}px2 of its own disc"
    );
}

/// A stroke that covers ground is still a ribbon: the dot rule must not swallow a short line.
#[test]
fn a_line_longer_than_its_nib_is_still_a_band() {
    let width = 18.0_f32;
    let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, width), 0x59FF_EB3B);

    let mut x = 0.0;
    while x < 40.0 {
        x += 0.75;
        stroke.points.push(InkPoint::new(x, 0.0, width));
    }

    stroke.close();

    // A band 40 long and 18 wide, not the disc a dot would be. Its two ends are round, because the nib is,
    // so the shape is the rectangle plus one whole disc — and the disc is most of a millimetre of the mark.
    let area = polygon_area(&stroke.outline);
    let band = 40.0 * width + std::f32::consts::PI * (width * 0.5).powi(2);

    assert!(
        (area / band - 1.0).abs() < 0.15,
        "a 40px band encloses {area:.0}px2, not the {band:.0}px2 it sweeps"
    );
}

/// The area a closed polygon encloses, by the shoelace rule.
fn polygon_area(points: &[[f32; 2]]) -> f32 {
    if points.len() < 3 {
        return 0.0;
    }

    let mut twice = 0.0;
    for index in 0..points.len() {
        let here = points[index];
        let next = points[(index + 1) % points.len()];

        twice += here[0] * next[1] - next[0] * here[1];
    }

    twice.abs() * 0.5
}

/// The straight pieces a stroke is cut into are short enough that the antialiasing hides the curve, and no
/// shorter than that.
///
/// A chord of length `f` standing in for an arc of radius `r` is off by `r(1 - cos(f/2r))`, at most
/// `f²/8r`. The threshold that decides whether that can be seen is the antialiasing: the edge is sampled
/// four times a pixel, so an error under a quarter of a pixel is smaller than the sample grid and cannot
/// resolve. This test states the two bounds the constant was chosen between — under the antialiasing, and
/// coarser than a pixel, because otherwise there was no point raising it at all.
#[test]
fn a_facet_is_short_enough_to_hide_and_no_shorter() {
    // The tightest turn a pen makes, in window pixels: a nib's own radius, and a small one.
    let tightest = 3.0_f32;

    // The error a chord of `FACET` makes against that arc: at most f²/8r.
    let error = |facet: f32| facet * facet / (8.0 * tightest);

    // Four samples a pixel: an error finer than a quarter of one is under the sample grid.
    const RESOLVED: f32 = 0.25;

    let chosen = error(note_format::ink::FACET);
    assert!(
        chosen < RESOLVED,
        "a facet of {} is off by {chosen} px at the tightest turn, and {RESOLVED} px is what can be seen",
        note_format::ink::FACET
    );

    // And coarser than a pixel, which is the whole point of having raised it from the original 1.0.
    let one_pixel = error(1.0);
    assert!(
        chosen > one_pixel,
        "the facet is no coarser than a pixel: {chosen} against {one_pixel}"
    );

    // The next half-pixel step is over the line, so this is the coarsest that stays under it — not an
    // arbitrary value between two others.
    assert!(
        error(note_format::ink::FACET + 0.5) > RESOLVED,
        "a facet of {} would be over the antialiasing's resolution",
        note_format::ink::FACET + 0.5
    );
}

/// Culling is inclusive, which is the safe side of the one mistake it must not make.
///
/// A stroke whose last pixel is exactly the screen's first is *on* the screen. An exclusive test would
/// cull it, and the cost is a line that vanishes at the seam when a reader pans by exactly one stroke
/// width — the kind of bug that shows up once and is never reproducible.
#[test]
fn a_stroke_on_the_edge_of_the_screen_is_on_the_screen() {
    let stroke = line((100.0, 100.0), (200.0, 100.0));

    // A box whose right edge *is* the stroke's left edge, and whose bottom edge *is* the stroke's row.
    assert!(
        stroke.visible_in(Bounds::new((0.0, 0.0), (100.0, 100.0))),
        "a box that touches the stroke draws it"
    );

    // One pixel further left and it is genuinely off screen.
    assert!(
        !stroke.visible_in(Bounds::new((0.0, 0.0), (99.0, 100.0))),
        "a box that stops short of it culls it"
    );

    // The same on the other side.
    assert!(stroke.visible_in(Bounds::new((200.0, 0.0), (300.0, 200.0))));
    assert!(!stroke.visible_in(Bounds::new((201.0, 0.0), (300.0, 200.0))));
}

/// The box that covers nothing overlaps nothing, so folding with it is safe.
///
/// This is what makes [`Bounds::NOTHING`] usable as the start of a fold over strokes: an empty page
/// has to mean "draw nothing", and a box at the origin would mean "the ink is at (0, 0)".
#[test]
fn a_box_that_covers_nothing_overlaps_nothing() {
    let nothing = Bounds::NOTHING;

    assert!(nothing.is_empty());
    assert!(!nothing.overlaps(Bounds::new((0.0, 0.0), (100.0, 100.0))));
    assert!(!nothing.overlaps(nothing), "not even itself");
    assert_eq!(Bounds::default(), nothing, "the default is the empty box");

    // And it is the identity of a union: folding it with a box gives that box.
    let real = Bounds::new((10.0, 20.0), (30.0, 40.0));
    assert_eq!(nothing.union(real), real);
    assert_eq!(real.union(nothing), real);
}

/// [`Bounds::within`] is containment, and a box that shares an edge is contained.
///
/// The ink layer keeps the triangles it built while the screen still fits inside the region they cover.
/// If a screen that shares an edge with that region were judged "outside", the layer would rebuild the
/// page on every frame at exactly the scroll position where the edge lines up — a stutter with no
/// visible cause.
#[test]
fn a_box_sharing_an_edge_is_inside_the_one_it_shares_it_with() {
    let region = Bounds::new((0.0, 0.0), (100.0, 100.0));

    assert!(region.within(region), "a box is inside itself");
    assert!(
        Bounds::new((50.0, 50.0), (100.0, 100.0)).within(region),
        "a corner that shares the far edges is inside"
    );
    assert!(
        !Bounds::new((50.0, 50.0), (101.0, 100.0)).within(region),
        "one unit past the edge is outside"
    );
}

/// The old four-number order survives the type, so nothing that stored a box changed meaning.
#[test]
fn a_box_converts_to_and_from_the_four_numbers_it_used_to_be() {
    let box_ = Bounds::new((1.0, 2.0), (3.0, 4.0));

    assert_eq!(box_.into_array(), [1.0, 2.0, 3.0, 4.0], "min, then max");
    assert_eq!(Bounds::from([1.0, 2.0, 3.0, 4.0]), box_);
    assert_eq!(box_.to_string(), "[1, 2, 3, 4]");
    assert_eq!(box_.size(), (2.0, 2.0));
}

/// A `sum()` of boxes is their union, and an empty page sums to the box that covers nothing.
///
/// Both halves matter. The union half is the reason `Sum` exists — a page's box is one expression rather
/// than a loop with a mutable accumulator. The empty half is the one a careless impl gets wrong, and the
/// obvious careless version is *not* the one it looks like: this type's `Default` already covers nothing,
/// so folding from it happens to be right. The version that is wrong is the one a reader writes when
/// they think "start at zero" — `Bounds::new((0.0, 0.0), (0.0, 0.0))` — which gives an empty page a box
/// in its top-left corner, and every cull test then draws ink that is not there. That is the mistake
/// this pins: the identity has to be the box that covers nothing, not the box at the origin.
#[test]
fn a_sum_of_boxes_is_their_union_and_an_empty_sum_covers_nothing() {
    let boxes = [
        Bounds::new((10.0, 10.0), (20.0, 20.0)),
        Bounds::new((-5.0, 30.0), (0.0, 40.0)),
        Bounds::new((15.0, 0.0), (15.0, 5.0)),
    ];

    assert_eq!(
        boxes.into_iter().sum::<Bounds>(),
        Bounds::new((-5.0, 0.0), (20.0, 40.0)),
        "the union of every box"
    );
    assert_eq!(
        boxes.into_iter().sum::<Bounds>(),
        boxes.iter().copied().fold(Bounds::NOTHING, Bounds::union),
        "and it agrees with a fold from the identity"
    );

    let empty = Vec::<Bounds>::new();
    assert_eq!(
        empty.iter().copied().sum::<Bounds>(),
        Bounds::NOTHING,
        "an empty page covers nothing, not a box at the origin"
    );
    assert!(empty.iter().copied().sum::<Bounds>().is_empty());
}

/// `+=` grows a box, which is what a running total over a page is.
#[test]
fn adding_to_a_box_grows_it_to_cover_the_other() {
    let mut total = Bounds::NOTHING;

    total += Bounds::new((10.0, 10.0), (20.0, 20.0));
    total += Bounds::new((0.0, 25.0), (5.0, 30.0));

    assert_eq!(
        total,
        Bounds::new((0.0, 10.0), (20.0, 30.0)),
        "the running box covers both, from the empty start"
    );
}
