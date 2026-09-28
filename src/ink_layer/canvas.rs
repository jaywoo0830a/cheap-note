//! What the app hands the canvas, and what comes back: rectangles, colours, and — from the ink on
//! — geometry, all in *logical* window pixels.
//!
//! ## Why logical pixels
//!
//! The app works in logical pixels: a pen reading arrives in physical ones and is converted at the
//! boundary (see [`crate::ink::InkTransform`]), a layout number is a logical one, and the sheet's
//! geometry is the same number whatever display it is on. So the canvas is described in logical
//! pixels too, and the scale factor travels *with* the description — [`Canvas::scale`] — rather
//! than being multiplied into every rectangle. The layer applies it once, as the transform of the
//! surface it draws into, which is also where the ink's own coordinates are scaled. One
//! multiplication per frame instead of one per rectangle, and one place to get it right.
//!
//! ## Why the app describes the canvas and the layer draws it
//!
//! The shadow's steps, the paper's colour and the sheet's geometry are the app's design and stay
//! where they are; the layer knows how to put rectangles on a surface and nothing about what a page
//! looks like. That is also what keeps the two renderers — the layer, and the frame's own painting
//! when there is no layer — from drifting apart: both are handed the same rectangles.

use std::sync::Arc;

use gpui_kit::{Hsla, RenderImage};

use crate::ink::Stroke;
use crate::pages::Quarters;

/// A rectangle in logical window pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    /// Whether there is anything to draw.
    ///
    /// A rectangle with no area is not a fill: a sheet scrolled off the surface, a window narrower
    /// than its own margin, a shadow step of nothing. They are all the same to a renderer and none
    /// of them is worth a command.
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// A rectangle to fill, and the colour to fill it with.
///
/// Also the shape of one mark of a sheet's ruling: a line on paper is a rectangle and its colour, a
/// dot is that with its corners rounded, and nothing else about either is the renderer's business.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fill {
    pub rect: Rect,
    pub colour: Hsla,
    /// The corners' radius, in logical pixels: nought for a rectangle, and half the short side for
    /// the dot of a dot grid.
    pub radius: f32,
}

impl Fill {
    /// A rectangle, with square corners.
    pub fn new(rect: Rect, colour: Hsla) -> Self {
        Fill {
            rect,
            colour,
            radius: 0.0,
        }
    }

    /// The same fill, with its corners rounded by `radius`.
    pub fn rounded(self, radius: f32) -> Self {
        Fill { radius, ..self }
    }
}

/// The document's page, as the canvas draws it: the pixels Pdfium rendered, and where on the desk
/// they are placed.
#[derive(Clone, Debug)]
pub struct Page {
    /// The bitmap, in BGRA rows ordered top-down. Uploaded when [`RenderImage::id`] changes and kept
    /// until it does — a page is rasterised when the zoom asks for a different width, not per frame.
    pub image: Arc<RenderImage>,
    /// Where the page is drawn, in logical window pixels.
    pub rect: Rect,
}

/// The ink in front of the reader.
///
/// ## Why the geometry is not in these numbers
///
/// A stroke's outline is in the *paper's* units — the same numbers at every zoom — and where the
/// paper sits and how large it is drawn are these three fields. That split is the whole reason the
/// canvas can pan and zoom without rebuilding anything: the layer keeps each stroke's geometry and
/// moves it, and geometry is built only when the ink changes or the *detail* the zoom asks for
/// moves (see [`Canvas::ink`]).
#[derive(Clone, Debug)]
pub struct Ink {
    /// Where the paper's own origin is drawn, in logical window pixels.
    pub origin: (f32, f32),
    /// How much larger than its own units the paper is drawn.
    pub zoom: f32,
    /// The finished strokes, shared rather than copied: the layer reads their outlines and compares
    /// their identities, and a frame that copies three hundred outlines would be the cost this
    /// exists to remove.
    ///
    /// The `Arc` is also the *identity* of the page of ink: a page turn, an undo or an erase
    /// replaces it, and a replaced one is a cache the layer drops.
    pub strokes: Arc<Vec<Arc<Stroke>>>,
    /// The stroke under the pen, closed at the zoom in hand. Rebuilt by the app for every frame it
    /// is drawn in, because it is the one piece of ink that is still moving.
    pub open: Option<Arc<Stroke>>,
    /// Counts the rebuilds of the outlines themselves: a zoom that crosses into another detail rung
    /// changes every stroke's outline without changing any of their identities, and this is what
    /// says so.
    pub revision: u64,
    /// The part of the sheet that is on screen, in the paper's own units: what a frame culls against
    /// (see [`Stroke::visible_in`]).
    ///
    /// The ink is *placed* in the paper's units and drawn through the transform, so a stroke the
    /// reader has scrolled past is a draw call the device does not need — and when zoomed in, most
    /// of a page is off screen. Everything is visible in the default, because a caller with no
    /// window has nothing honest to say and skipping a stroke that *is* on screen is the one mistake
    /// this can make.
    pub visible: [f32; 4],
    /// How far the page has been turned, in quarter turns clockwise, and the paper's own size in its
    /// own units.
    ///
    /// Both are here because the *transform* needs them: turning a page is not a property of any stroke
    /// (the ink never moves), it is a property of the mapping between the paper and the screen, and the
    /// layer is what draws through that mapping (see [`crate::ink::drawn_of_paper`]).
    pub rotation: Quarters,
    /// The paper's own size, in the paper's units — the un-turned shape, which is what `rotation`
    /// turns. `(0.0, 0.0)` means "no paper": see [`crate::ink::InkTransform::has_paper`].
    pub paper: (f32, f32),
}

impl Default for Ink {
    fn default() -> Self {
        Ink {
            origin: (0.0, 0.0),
            zoom: 0.0,
            strokes: Arc::new(Vec::new()),
            open: None,
            revision: 0,
            visible: Ink::ALL_VISIBLE,
            rotation: 0,
            paper: (0.0, 0.0),
        }
    }
}

impl Ink {
    /// The sheet in full, as a culling rectangle.
    ///
    /// Written down because the two infinities have to be: a rectangle of zeroes — what a derived
    /// `Default` would leave here — is the one value that means "nothing on this sheet is on screen"
    /// (see [`Ink::visible`]).
    const ALL_VISIBLE: [f32; 4] = [
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::INFINITY,
    ];
}

/// One frame of canvas: the desk, what is printed on it, and the ink in front of it.
#[derive(Clone, Debug, Default)]
pub struct Canvas {
    /// The colour of the desk, which is also what the parts of the surface nothing covers are.
    pub desk: Hsla,
    /// Physical pixels per logical pixel, as the window is drawn at. The layer scales by this; the
    /// app never does (see the module docs).
    pub scale: f32,
    /// The page's shadow and the sheet, in the order they are painted.
    pub fills: Vec<Fill>,
    /// The sheet's own rectangle, in logical window pixels: what the ink is clipped to.
    ///
    /// A ribbon at the paper's edge spills a couple of pixels past it, and ink written before the
    /// model refused off-paper readings can lie well outside it; both are ink the paper cannot hold
    /// (see [`crate::ink::InkTransform::on_paper`]).
    pub sheet: Option<Rect>,
    /// The ruling, printed on the sheet.
    pub rules: Vec<Fill>,
    /// The document's page, when the sheet is one.
    pub page: Option<Page>,
    /// The ink, in the paper's own coordinates.
    pub ink: Ink,
}

impl Canvas {
    /// Empties the canvas, keeping the memory it was using.
    ///
    /// Called once per frame rather than rebuilding the description: a frame is a few rectangles
    /// and a shared list of strokes, but "a few" is not a reason to allocate them anew sixty times a
    /// second.
    pub fn clear(&mut self) {
        self.fills.clear();
        self.rules.clear();
        self.sheet = None;
        self.page = None;
        self.ink = Ink::default();
    }

    /// Adds a rectangle to the canvas, on top of what is already described.
    pub fn fill(&mut self, rect: Rect, colour: Hsla) {
        if !rect.is_empty() {
            self.fills.push(Fill::new(rect, colour));
        }
    }

    /// Adds a mark to the sheet's ruling.
    pub fn rule(&mut self, fill: Fill) {
        if !fill.rect.is_empty() {
            self.rules.push(fill);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::rgba;

    /// A rectangle with no area is not added: a shadow step of zero, a sheet off the surface, a
    /// window narrower than its own border. They are all the same to a fill and none of them is
    /// worth a command.
    #[test]
    fn a_rectangle_with_no_area_is_not_painted() {
        let mut canvas = Canvas::default();
        canvas.fill(
            Rect {
                x: 1.0,
                y: 1.0,
                width: 0.0,
                height: 10.0,
            },
            rgba(0xFF00_00FF).into(),
        );

        assert!(canvas.fills.is_empty());
    }

    /// Clearing empties the canvas without forgetting the desk or the scale: a frame's fills are
    /// its own, the desk it is painted on and the display it is drawn for belong to the window.
    #[test]
    fn clearing_forgets_the_fills_and_keeps_the_desk() {
        let desk = Hsla::from(rgba(0x8080_80FF));
        let mut canvas = Canvas {
            desk,
            scale: 1.5,
            ..Default::default()
        };
        canvas.fill(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            rgba(0xFFFF_FFFF).into(),
        );

        canvas.clear();

        assert!(canvas.fills.is_empty());
        assert_eq!(canvas.desk, desk);
        assert_eq!(canvas.scale, 1.5);
    }
}
