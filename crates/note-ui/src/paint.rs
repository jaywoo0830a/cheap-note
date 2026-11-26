//! What a surface needs from the view to paint a frame: where the paper is, and what colour the ink is.
//!
//! The canvas description ([`crate::ink_layer`]) is already the view's, and a renderer walks it without
//! help. This module is the far smaller set of *functions* a renderer calls back into because the answer
//! belongs to the view: the one place a paper point becomes a surface point, and the one place an ink
//! colour becomes a paint colour.
//!
//! It exists so that a platform crate depends on **five names** rather than on the ink model. The model
//! — the document, the tools, undo, the eraser — is none of a renderer's business, and a platform that
//! could reach it would eventually reach into it.
//!
//! ## The one conversion, and every caller of it
//!
//! A point exists in three coordinates at once, and this module owns all three of the conversions
//! between them rather than leaving each caller to write its own subtraction:
//!
//! * **Physical pixels** is what a pen reports and what a surface rasterises at.
//! * **Window logical pixels** is what the layout is measured in. [`logical`] is the only division by the
//!   DPI scale, so the rule for "a scale of nought is a scale of one" is written once.
//! * **The paper's own units** is where ink is stored — the page's coordinates, which a turn does not
//!   change, because turning a page moves where it is *drawn* and not where it was written. [`drawn_of_paper`]
//!   and [`paper_of_drawn`] are the two directions of that turn, and [`InkTransform`] composes all of it.
//!
//! Its inverse pair, [`InkTransform::drawn_point`] and [`InkTransform::sheet_point`], is what makes a
//! renderer's geometry derivable: a layer asks for the paper's origin and its two unit steps, and the
//! whole matrix follows (see `ink_matrix` on Windows and `Mapping` on macOS). Nothing else should be
//! subtracting an origin or dividing by a zoom — a second copy of that arithmetic is a second answer to
//! which way up a page is, and the way it shows is ink off the page it was written on.

/// Whether a factor — a DPI scale, a zoom, an interface scale — can be multiplied or divided by.
///
/// `1.0` when it cannot: a window that reports nought, a negative, or a NaN means "no transform", which
/// draws a thing where it is rather than collapsing every point into a corner or poisoning every
/// rectangle derived from it with a NaN.
///
/// The one definition. There were nine copies of this `if` across the two platforms and the view, and the
/// macOS ghost's copy tested `> 0.0` alone, so a NaN scale reached its geometry while every other copy
/// refused it.
pub fn finite_or_one(factor: f32) -> f32 {
    if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    }
}

/// A reading in physical client pixels, as the window's own logical pixels: what the layout and a canvas
/// description are both measured in.
pub fn logical((x, y): (f32, f32), scale: f32) -> (f32, f32) {
    let scale = finite_or_one(scale);

    (x / scale, y / scale)
}

/// Where a point of the sheet is drawn, given the paper's rectangle and the page's turn.
///
/// The renderer's whole geometry is derived from three calls to this — the paper's origin and its two
/// unit steps — because the same function has to be right for both the ink (stored in paper
/// coordinates) and the sheet's own edges.
pub use crate::ink::drawn_of_paper;

/// Where a point of the sheet's rectangle came from on the paper: the inverse of [`drawn_of_paper`].
///
/// What a pen reading's position is turned back through, and the reason a page turned is still written
/// on in the page's own coordinates.
pub use crate::ink::paper_of_drawn;

/// A paper's origin and its two unit steps on the sheet, turned the way the page is turned.
///
/// The geometry a renderer's transform is built from, with no zoom and no placement in it: a caller
/// applies those itself, which is what lets a layer draw a page whose zoom changes without rebuilding a
/// single vertex. [`InkTransform::basis`] is the same three numbers for a transform that has them — this
/// is the version for a renderer holding only a canvas description.
pub fn drawn_basis(
    paper: (f32, f32),
    turns: crate::pages::Quarters,
) -> ((f32, f32), (f32, f32), (f32, f32)) {
    let base = drawn_of_paper((0.0, 0.0), paper, turns);
    let along_x = drawn_of_paper((1.0, 0.0), paper, turns);
    let along_y = drawn_of_paper((0.0, 1.0), paper, turns);

    (
        base,
        (along_x.0 - base.0, along_x.1 - base.1),
        (along_y.0 - base.0, along_y.1 - base.1),
    )
}

/// The paper's points, the window's logical ones, and the pen's physical ones — with the conversions
/// between them.
pub use crate::ink::InkTransform;

/// How much of the page a stroke's colour lets through, in `0.0..=1.0`.
pub use note_format::ink::alpha_of;

/// A colour with an alpha stamped into its top byte: `0xAARRGGBB`.
pub use note_format::ink::with_alpha;

/// How much of the page a highlighter's band lets through: the alpha stamped into its colour.
///
/// The band's *width* is not a constant: it is the note's own answer for the marker in hand
/// (`Settings::highlighter_weight`), and each stroke carries the tip it was laid with.
pub use crate::ink::HIGHLIGHTER_ALPHA;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::Quarters;

    /// The four ways up, so a test says "every turn" rather than listing them.
    const TURNS: [Quarters; 4] = Quarters::ALL;

    /// A factor that cannot be multiplied by is one.
    ///
    /// The nine copies this replaced did not all agree: the macOS ghost's tested `> 0.0` alone and let a
    /// NaN through into its geometry, where every other one refused it.
    #[test]
    fn a_factor_that_cannot_be_used_is_one() {
        assert_eq!(finite_or_one(2.0), 2.0);
        assert_eq!(finite_or_one(0.5), 0.5);

        for bad in [0.0, -0.0, -1.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(finite_or_one(bad), 1.0, "{bad} is not a factor");
        }
    }

    /// Physical pixels become logical ones by the window's scale, once.
    #[test]
    fn logical_pixels_are_divided_by_the_scale_once() {
        assert_eq!(logical((300.0, 150.0), 2.0), (150.0, 75.0));
        assert_eq!(logical((300.0, 150.0), 1.0), (300.0, 150.0));

        // A scale that cannot be divided by leaves the reading where it is rather than collapsing it
        // into the origin.
        assert_eq!(logical((300.0, 150.0), 0.0), (300.0, 150.0));
    }

    /// The steps a renderer builds its transform from reproduce the mapping itself.
    ///
    /// This is what makes `drawn_basis` safe to hand to two renderers: whatever a layer does with the
    /// three numbers, the answer has to be [`drawn_of_paper`]'s, at every point and every turn.
    #[test]
    fn the_steps_reproduce_every_point() {
        let paper = (600.0, 800.0);

        for turns in TURNS {
            let (base, along_x, along_y) = drawn_basis(paper, turns);

            for point in [
                (0.0, 0.0),
                (1.0, 0.0),
                (0.0, 1.0),
                (37.5, 12.25),
                (599.0, 799.0),
            ] {
                let through = (
                    base.0 + point.0 * along_x.0 + point.1 * along_y.0,
                    base.1 + point.0 * along_x.1 + point.1 * along_y.1,
                );
                let mapped = drawn_of_paper(point, paper, turns);

                assert!(
                    (through.0 - mapped.0).abs() < 1e-3 && (through.1 - mapped.1).abs() < 1e-3,
                    "{point:?} at {turns} turns: {through:?} against {mapped:?}"
                );
            }
        }
    }

    /// The steps are a quarter turn's, so each is one unit on one axis and nothing on the other.
    ///
    /// A layer skips the arithmetic entirely when the basis is the identity (see macOS's `is_identity`),
    /// which is only sound because a quarter turn can never produce a fractional step.
    #[test]
    fn a_step_is_one_unit_on_one_axis_and_none_on_the_other() {
        for turns in TURNS {
            let (_, along_x, along_y) = drawn_basis((600.0, 800.0), turns);

            for step in [along_x, along_y] {
                assert!(
                    (step.0.abs() == 1.0 && step.1 == 0.0)
                        || (step.0 == 0.0 && step.1.abs() == 1.0),
                    "{step:?} at {turns} turns is not a quarter turn's step"
                );
            }
        }
    }

    /// An unturned page's basis is the identity: `(0, 0)`, `(1, 0)`, `(0, 1)`.
    #[test]
    fn an_unturned_page_has_the_identity_basis() {
        assert_eq!(
            drawn_basis((600.0, 800.0), Quarters::UPRIGHT),
            ((0.0, 0.0), (1.0, 0.0), (0.0, 1.0))
        );
    }
}
