//! The band: what one reading added to an open stroke, as geometry and as damage.
//!
//! Contract: `doc/ENGINE.md` §4.2 — *"the band between the previous disc and this one becomes ink"*, the
//! pieces the band covers are replayed once, and nothing else on the layer changes (E2, E6).
//!
//! The band is built from the ink model's own two families (`note_format::ink::Piece`): the trapezoid
//! between two readings' outer tangents, and the fan over the disc a reading left. Those families *are*
//! the region — the union of the discs the nib left (`doc/VIEW.md` §7) — so there is nothing to sort and
//! no turning case to get wrong.
//!
//! Everything here answers in **window pixels**: the paper's coordinates go through a
//! [`Placement`](crate::place::Placement) once, and the layer's damage is the shape on the screen.

use note_format::ink::Piece;

use crate::footprint::{Footprint, Rect};
use crate::place::Placement;

/// One reading's worth of new ink: the pieces to fill, and the area they touch.
#[derive(Clone, Debug, Default)]
pub struct Band {
    /// The triangles to fill, in the window's coordinates: a trapezoid for the span between two
    /// readings, a wedge for the disc each reading left.
    ///
    /// They overlap by construction, so a renderer must mark them and colour once — the union, not the
    /// composite (E6, `doc/ENGINE.md` §4.1).
    pieces: Vec<Triangle>,
    /// The band's bounds as its corners were added: the smallest box every corner lies in.
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// A triangle in window pixels, ready to upload: three corners, wound the same way every time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Triangle {
    pub a: (f32, f32),
    pub b: (f32, f32),
    pub c: (f32, f32),
}

/// How far past a band's tangents the antialiased edge of the stroke spills, in logical pixels.
///
/// A band's trapezoid is bounded by the tangent line, which is where the stroke's *edge* is rather than
/// where its coverage ends: the coverage reaches one more pixel and a fraction, which is what an
/// antialiased fill draws. One pixel of slack is enough for a four-sample pixel (`doc/VIEW.md` §12's
/// `SAMPLES`) and costs a strip of damage the composite would have paid anyway.
pub const EDGE_SLACK: f32 = 1.0;

impl Band {
    /// An empty band: a reading that laid no ink (E1).
    pub fn empty() -> Self {
        Band {
            pieces: Vec::new(),
            left: f32::INFINITY,
            top: f32::INFINITY,
            right: f32::NEG_INFINITY,
            bottom: f32::NEG_INFINITY,
        }
    }

    /// The band one reading laid, from the pieces that reading added.
    ///
    /// `pieces` is what the model says changed since the last reading — the new span and the new disc
    /// — and `place` is where the paper is drawn. The pieces are in the paper's own units; the band is
    /// in the window's.
    pub fn of(pieces: &[Piece], place: &Placement) -> Self {
        let mut band = Band::empty();

        for piece in pieces {
            match *piece {
                Piece::Quad {
                    left_from,
                    left_to,
                    right_to,
                    right_from,
                } => {
                    let (a, b, c, d) = (
                        place.window_point(left_from),
                        place.window_point(left_to),
                        place.window_point(right_to),
                        place.window_point(right_from),
                    );

                    // Two triangles for the trapezoid, wound the way the boundary runs: left forward,
                    // right back.
                    band.pieces.push(Triangle { a, b, c });
                    band.pieces.push(Triangle { a, b: c, c: d });
                    band.cover(&[a, b, c, d]);
                }
                Piece::Fan { centre, from, to } => {
                    let (o, a, b) = (
                        place.window_point(centre),
                        place.window_point(from),
                        place.window_point(to),
                    );

                    band.pieces.push(Triangle { a: o, b: a, c: b });
                    band.cover(&[o, a, b]);
                }
            }
        }

        band
    }

    /// A band from a stroke's whole geometry: its pieces when it has any, and its outline when it is a
    /// single disc.
    ///
    /// A pen that came down and lifted without moving leaves one disc, and the model answers that with
    /// an **outline** rather than pieces — there is no tangent to cut between one reading and itself
    /// (`note_format::ink::ribbon`). Drawing that as nothing is a dot nobody sees; drawing it as a fan
    /// over the disc's rim is the mark (E6: the same union as every other stroke).
    pub fn of_stroke(stroke: &note_format::ink::Stroke, place: &Placement) -> Self {
        if !stroke.pieces.is_empty() {
            return Band::of(&stroke.pieces, place);
        }

        if stroke.points.is_empty() {
            return Band::empty();
        }

        // The disc the model left: where the readings are, and the nib's own half-width.
        let mut centre = (0.0_f32, 0.0_f32);
        for point in &stroke.points {
            centre.0 += point.x;
            centre.1 += point.y;
        }

        let count = stroke.points.len() as f32;
        centre = (centre.0 / count, centre.1 / count);

        // A fan round the rim: the boundary the model already computed, as triangles from the centre.
        let outline: Vec<(f32, f32)> = stroke
            .outline
            .iter()
            .map(|point| place.window_point(*point))
            .collect();

        let centre = place.to_window(centre);
        let mut band = Band::empty();

        for pair in outline.windows(2) {
            band.pieces.push(Triangle {
                a: centre,
                b: pair[0],
                c: pair[1],
            });
            band.cover(&[centre, pair[0], pair[1]]);
        }

        band
    }

    /// Grows the band's own bounds over `corners`; the damage is added once, when the band is finished.
    ///
    /// One rectangle per band rather than one per corner: a corner is a point, and a damage of points
    /// would be eight rectangles that no layer could scissor to, and whose area would not be the band's
    /// (E2). The band *is* the box its corners make, grown by [`EDGE_SLACK`].
    fn cover(&mut self, corners: &[(f32, f32)]) {
        for &(x, y) in corners {
            self.left = self.left.min(x);
            self.top = self.top.min(y);
            self.right = self.right.max(x);
            self.bottom = self.bottom.max(y);
        }
    }

    /// The band's bounds as it stands, or nothing when no corner has been seen.
    fn boxed(&self) -> Option<Rect> {
        if self.left > self.right {
            return None;
        }

        Some(Rect::from_corners(
            (self.left, self.top),
            (self.right, self.bottom),
        ))
    }

    /// The triangles to fill.
    pub fn pieces(&self) -> &[Triangle] {
        &self.pieces
    }

    /// The area the layer has to repair for this band: the box its corners make, grown by the edge.
    ///
    /// One rectangle, because a band's ink is one connected run: two readings and the span between them
    /// leave a box with nothing of their own outside it, and a layer scissors to it in one pass.
    pub fn damage(&self) -> Footprint {
        let mut damage = Footprint::empty();

        if let Some(bounds) = self.boxed() {
            damage.add(bounds.grown(EDGE_SLACK));
        }

        damage
    }

    /// Whether this reading laid no ink at all, which is the answer that draws nothing (E1).
    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// The area, in square pixels: the number E2 bounds.
    pub fn area(&self) -> f32 {
        self.damage().area()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use note_format::pages::Quarters;

    /// The paper drawn where its own units are the window's, so a test can read the geometry directly.
    fn identity() -> Placement {
        Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0))
    }

    /// The span between two readings is a trapezoid: two triangles, and a damage the size of the band.
    ///
    /// This is E2's arithmetic in its smallest form — one reading, one span, and the area it costs is
    /// the band's rather than the page's.
    #[test]
    fn one_span_is_one_trapezoid() {
        let span = Piece::Quad {
            left_from: [0.0, 0.0],
            left_to: [10.0, 0.0],
            right_to: [10.0, 4.0],
            right_from: [0.0, 4.0],
        };

        let band = Band::of(&[span], &identity());

        assert_eq!(band.pieces().len(), 2, "a trapezoid was not two triangles");

        // The band is ten long and four wide, grown by the edge slack on every side.
        let damage = band.damage().bounds();
        assert_eq!((damage.x, damage.y), (-EDGE_SLACK, -EDGE_SLACK));
        assert_eq!(
            (damage.width, damage.height),
            (10.0 + 2.0 * EDGE_SLACK, 4.0 + 2.0 * EDGE_SLACK),
            "the band's damage was not the band it covers"
        );
    }

    /// A disc is one wedge: a fan is a single triangle its centre joins.
    #[test]
    fn one_disc_is_one_wedge() {
        let fan = Piece::Fan {
            centre: [5.0, 5.0],
            from: [6.0, 4.0],
            to: [6.0, 6.0],
        };

        let band = Band::of(&[fan], &identity());

        assert_eq!(band.pieces().len(), 1, "a wedge was not one triangle");
        assert_eq!(band.pieces()[0].a, (5.0, 5.0), "the wedge lost its centre");
    }

    /// E1: a reading that added no piece lays no ink, and a layer handed this draws nothing.
    #[test]
    fn a_reading_with_no_pieces_lays_no_ink() {
        let band = Band::of(&[], &identity());

        assert!(band.is_empty(), "an empty band claimed to have ink");
        assert!(band.area() == 0.0, "an empty band claimed an area");
        assert!(
            band.damage().is_empty(),
            "an empty band damaged a rectangle"
        );
    }

    /// E2: the band is bounded by its own span, whatever the page is — the whole design exists to keep
    /// a reading from costing the page.
    #[test]
    fn a_band_costs_its_span_and_not_the_page() {
        let page_area = 600.0 * 800.0;
        let span = Piece::Quad {
            left_from: [100.0, 100.0],
            left_to: [108.0, 100.0],
            right_to: [108.0, 108.0],
            right_from: [100.0, 108.0],
        };

        let band = Band::of(&[span], &identity());

        assert!(
            band.area() < page_area / 100.0,
            "one reading cost more than a hundredth of the page"
        );
    }

    /// E6, the case the offset-edges model got wrong four ways: a stroke that doubles back is still one
    /// region, and its band is drawn once. The pieces of a doubling-back span still make one trapezoid,
    /// and the damage is the union of where the nib was — never a slit through the middle.
    #[test]
    fn a_band_that_doubles_back_is_still_one_region() {
        // Out along the top and back along the bottom, overlapping the same strip.
        let out = Piece::Quad {
            left_from: [0.0, 0.0],
            left_to: [20.0, 0.0],
            right_to: [20.0, 6.0],
            right_from: [0.0, 6.0],
        };
        let back = Piece::Quad {
            left_from: [20.0, 3.0],
            left_to: [0.0, 3.0],
            right_to: [0.0, 9.0],
            right_from: [20.0, 9.0],
        };

        let band = Band::of(&[out, back], &identity());

        // The damage covers the whole area the nib passed over: from y = 0 to y = 9, x = 0 to 20, grown
        // by the edge slack — one rectangle, because the two bands overlap.
        let damage = band.damage().bounds();
        assert_eq!(damage.y, -EDGE_SLACK);
        assert_eq!(damage.height, 9.0 + 2.0 * EDGE_SLACK);
        assert_eq!(
            band.damage().rects().len(),
            1,
            "a band that doubled back was damaged twice"
        );
    }

    /// The band is in the window's pixels, not the paper's: a turned, zoomed sheet moves every corner
    /// through the placement before it becomes geometry.
    #[test]
    fn a_band_is_in_window_pixels() {
        let span = Piece::Quad {
            left_from: [0.0, 0.0],
            left_to: [10.0, 0.0],
            right_to: [10.0, 4.0],
            right_from: [0.0, 4.0],
        };
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 2.0, (100.0, 50.0));

        let band = Band::of(&[span], &place);

        // The first corner is the paper's (0,0) at 2x, moved by the origin.
        assert_eq!(band.pieces()[0].a, (100.0, 50.0));
        // The span is ten long at 2x, so its far corner is twenty along.
        assert_eq!(band.pieces()[0].b, (120.0, 50.0));
    }

    /// A whole page is a run of bands: one per span between readings, for every stroke on it.
    ///
    /// This is the *static* path — a page read, a page turned to, the first frame of a document — where
    /// the engine is handed a stroke list rather than a reading. It is the same geometry as the
    /// reading-by-reading path (E6): a stroke's pieces turned into triangles, and each stroke's band
    /// filled as one union.
    #[test]
    fn a_whole_stroke_is_a_run_of_bands() {
        use note_format::ink::{ribbon, InkPoint, Model, Tip};

        let points = [
            InkPoint::new(10.0, 10.0, 4.0),
            InkPoint::new(14.0, 10.0, 4.0),
            InkPoint::new(18.0, 12.0, 4.0),
        ];
        let pieces = ribbon(&Model::of(&points, 1.0, Tip::Curved)).pieces;
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0));

        let band = Band::of(&pieces, &place);

        // A stroke of three readings has a span between each pair, and a wedge at each end: the same
        // pieces the model walked, as triangles.
        assert!(
            band.pieces().len() >= 4,
            "a stroke of three readings drew {} triangles",
            band.pieces().len()
        );
        assert!(!band.is_empty(), "a whole stroke's band came out empty");
        // The band is the stroke's own box, grown by the edge slack — not the page's.
        let damage = band.damage().bounds();
        assert!(damage.width < 60.0, "a stroke's band was page-wide");
        assert!(
            damage.y >= 10.0 - crate::band::EDGE_SLACK - 3.0,
            "a stroke's band began above the stroke"
        );
    }

    /// A point repeated is still a disc, not nothing: a nib that came down and lifted without moving
    /// lays a dot (the model's own rule, `doc/VIEW.md` §7).
    #[test]
    fn a_stroke_of_one_reading_is_a_disc() {
        use note_format::ink::{InkPoint, Stroke, Tip};

        let mut dot = Stroke::new(InkPoint::new(50.0, 50.0, 4.0), Stroke::DEFAULT_COLOR);
        dot.freeze(1.0, Tip::Curved);

        assert!(
            dot.pieces.is_empty(),
            "the model gave a one-reading stroke pieces, so this is not testing the disc"
        );

        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0));
        let band = Band::of_stroke(&dot, &place);

        assert!(!band.is_empty(), "a dot laid no ink");
        assert!(
            band.pieces().len() >= 2,
            "a disc was not a fan of triangles"
        );
        assert!(
            band.damage().bounds().width <= 2.0 * 2.5 + 2.0 * crate::band::EDGE_SLACK + 0.5,
            "a dot's damage was wider than its own nib: {}",
            band.damage().bounds().width
        );
    }

    /// E2 on a real stroke: one reading extends an open stroke by its band, and the band is the span
    /// between the two discs — never the stroke, never the page.
    ///
    /// The pieces come from the ink model itself (`note_format::ink`), so this is the one test that
    /// proves the engine's band *is* the model's region rather than a second implementation of it.
    #[test]
    fn a_reading_extends_a_real_stroke_by_its_band() {
        use note_format::ink::{ribbon, InkPoint, Model, Tip};

        let anchored = [
            InkPoint::new(100.0, 100.0, 4.0),
            InkPoint::new(104.0, 100.0, 4.0),
        ];
        let extended = [
            InkPoint::new(100.0, 100.0, 4.0),
            InkPoint::new(104.0, 100.0, 4.0),
            InkPoint::new(108.0, 100.0, 4.0),
        ];

        let before = ribbon(&Model::of(&anchored, 1.0, Tip::Curved)).pieces;
        let after = ribbon(&Model::of(&extended, 1.0, Tip::Curved)).pieces;

        assert!(
            after.len() > before.len(),
            "a new reading added no piece at all"
        );

        // What the new reading laid is the *last quad*: the span between the two newest readings. The
        // stroke's end arcs move as the line grows, so a set difference would sweep the whole stroke —
        // which is exactly the cost the engine exists to avoid. The band is the span, and that is what
        // `Band::of` is given.
        let span = *after
            .last()
            .expect("a stroke of three readings has no pieces");
        assert!(
            matches!(span, note_format::ink::Piece::Quad { .. }),
            "the newest piece of a growing stroke was not a span"
        );

        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0));
        let band = Band::of(&[span], &place);

        // The band lies between the two newest readings: four long, nothing like the stroke's eight.
        let damage = band.damage().bounds();
        assert!(
            damage.width <= 4.0 + 2.0 * crate::band::EDGE_SLACK,
            "a reading's band was wider than the span it laid: {}",
            damage.width
        );
        assert!(
            damage.area() < 200.0,
            "a reading cost more than a few hundred pixels"
        );
    }
}
