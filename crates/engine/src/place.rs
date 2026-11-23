//! The placement: the paper's own units to the window's, and the same arithmetic on every platform.
//!
//! Contract: `doc/ENGINE.md` §4.5 — *"geometry is baked in the paper's own units; where the paper is
//! drawn, and how large, is a uniform."* This module is that uniform, resolved into a point function so
//! the tests can hold it to the paper's mapping without a shader.

use note_format::pages::Quarters;

/// Where the paper is drawn, how large, and which way up — the whole of what a pan, a zoom or a turn
/// changes.
///
/// The paper's units are what a stroke is stored in: the same numbers at every zoom and every placement
/// (`doc/VIEW.md` §7). This type is the *only* place those numbers become a window's, and it does so in a
/// single affine map — which is what makes a scroll a uniform rather than a rebuild.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// The paper's own size in its own units, unturned: what [`Quarters`] turns.
    paper: (f32, f32),
    /// How the page has been turned, in quarter turns clockwise.
    turns: Quarters,
    /// How much larger than its own units the paper is drawn (zoom × display scale).
    scale: f32,
    /// Where the paper's drawn origin lands, in window pixels.
    origin: (f32, f32),
}

/// Where a paper point lands *within the drawn sheet*, before the placement moves it: the paper's own
/// units, turned.
///
/// The four quarter turns are written out, exactly as `note_ui`'s `drawn_of_paper` writes them, because a
/// quarter turn here is not a rotation matrix: it also swaps the rectangle's sides, so the arithmetic is
/// the mapping and not a matrix times a vector. The engine depends on the format crate and not on the
/// view, so this is the same function with the same cases rather than a re-export — and
/// `the_turn_matches_the_views_own` below is what holds the two together.
fn turned((x, y): (f32, f32), (width, height): (f32, f32), turns: Quarters) -> (f32, f32) {
    match turns.turns() {
        1 => (height - y, x),
        2 => (width - x, height - y),
        3 => (y, width - x),
        _ => (x, y),
    }
}

impl Placement {
    /// A placement: the paper's size, how it is turned, how large it is drawn, and where it lands.
    pub const fn new(paper: (f32, f32), turns: Quarters, scale: f32, origin: (f32, f32)) -> Self {
        Placement {
            paper,
            turns,
            scale,
            origin,
        }
    }

    /// The paper's own point, as a window pixel.
    ///
    /// Two steps, and the order is the contract: the paper's units are turned into the drawn sheet's,
    /// and the drawn sheet is scaled and moved into the window. Turning first is what makes a turned
    /// page *the same ink* — the strokes never move, only where their coordinates are drawn.
    pub fn to_window(&self, point: (f32, f32)) -> (f32, f32) {
        let (x, y) = turned(point, self.paper, self.turns);
        let scale = if self.scale.is_finite() && self.scale != 0.0 {
            self.scale
        } else {
            // A placement with no scale draws the sheet at its own size: the same answer
            // `Transform::of` gives a canvas whose display scale is not a number.
            1.0
        };

        (self.origin.0 + x * scale, self.origin.1 + y * scale)
    }

    /// The paper's own point, given as the two numbers a [`Piece`] holds, as a window pixel.
    ///
    /// The model's pieces carry corners as `[f32; 2]`; every caller that turns them into geometry has
    /// them in that shape, so the conversion lives here rather than at each of them.
    pub fn window_point(&self, [x, y]: [f32; 2]) -> (f32, f32) {
        self.to_window((x, y))
    }

    /// Whether the map leaves every corner where it is, so a caller may skip the arithmetic.
    ///
    /// The common page is unturned, unmoved and at 1:1 — `authored` says whether this placement was
    /// *built* as the identity, which is the cheaper answer than comparing four floats per corner.
    pub fn is_identity(&self) -> bool {
        self.turns == Quarters::UPRIGHT && self.scale == 1.0 && self.origin == (0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page 600 wide and 800 tall, drawn at 1:1 at the window's origin, upright.
    fn upright() -> Placement {
        Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0))
    }

    /// An unturned sheet drawn at 1:1 at the origin puts every paper point where it says.
    ///
    /// The common case, and the one a fast path may skip the arithmetic for: `is_identity` says so, and
    /// a placement that answered "not identity" here would make every corner of every stroke pay for a
    /// multiply it did not need (`apps/macos/src/platform/metal.rs`, `Mapping::is_identity`).
    #[test]
    fn an_upright_sheet_at_one_to_one_is_the_identity() {
        let place = upright();

        assert!(
            place.is_identity(),
            "an unturned 1:1 sheet was not the identity"
        );

        for point in [(0.0, 0.0), (1.0, 2.0), (599.0, 799.0), (-3.0, 4.5)] {
            assert_eq!(
                place.to_window(point),
                point,
                "the identity moved {point:?}"
            );
        }
    }

    /// A quarter turn clockwise takes the paper's top-left corner to the drawn sheet's top-right, and
    /// the drawn sheet's width becomes the paper's height — the same four cases `note_ui`'s
    /// `drawn_of_paper` has.
    ///
    /// This is the test that keeps the engine's own copy of the mapping honest: the engine cannot depend
    /// on the view, so the arithmetic is written twice, and the two must agree. The expected values
    /// below are the view's, read off its table for a 600×800 page.
    #[test]
    fn a_quarter_turn_is_the_views_own() {
        let (width, height) = (600.0, 800.0);

        for (turns, expected) in [
            (Quarters::from_turns(0), (0.0, 0.0)),
            (Quarters::from_turns(1), (height, 0.0)),
            (Quarters::from_turns(2), (width, height)),
            (Quarters::from_turns(3), (0.0, width)),
        ] {
            let place = Placement::new((width, height), turns, 1.0, (0.0, 0.0));

            assert_eq!(
                place.to_window((0.0, 0.0)),
                expected,
                "the paper's origin was drawn in the wrong place for {} turns",
                turns.turns()
            );
        }
    }

    /// A page turned back is the page it was: four quarter turns are the identity.
    #[test]
    fn four_turns_are_the_identity() {
        let paper = (600.0, 800.0);
        let turn = Placement::new(paper, Quarters::from_turns(1), 1.0, (0.0, 0.0));
        let mut point = (123.0, 456.0);

        for _ in 0..4 {
            point = turn.to_window(point);
        }

        assert_eq!(point, (123.0, 456.0), "four quarter turns moved the ink");
    }

    /// A zoom scales about the drawn origin, and a pan moves it: the two things a gesture changes, and
    /// neither of which is allowed to be a rebuild (E5).
    #[test]
    fn a_zoom_scales_and_a_pan_moves() {
        let zoomed = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 2.0, (0.0, 0.0));
        assert_eq!(
            zoomed.to_window((10.0, 10.0)),
            (20.0, 20.0),
            "a 2x sheet did not double a paper point"
        );

        let panned = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (-40.0, 15.0));
        assert_eq!(
            panned.to_window((10.0, 10.0)),
            (-30.0, 25.0),
            "a pan did not move the drawn sheet"
        );

        // Neither is the identity, which is what makes a caller pay for the arithmetic only when a
        // gesture has actually changed something.
        assert!(
            !zoomed.is_identity(),
            "a 2x sheet claimed to be the identity"
        );
        assert!(
            !panned.is_identity(),
            "a panned sheet claimed to be the identity"
        );
    }

    /// A placement with no scale draws the sheet at its own size rather than collapsing it to a point.
    #[test]
    fn no_scale_draws_the_sheet_at_its_own_size() {
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 0.0, (0.0, 0.0));

        assert_eq!(
            place.to_window((10.0, 10.0)),
            (10.0, 10.0),
            "a zero scale collapsed the sheet"
        );
    }
}
