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

use gpui_kit::Hsla;

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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fill {
    pub rect: Rect,
    pub colour: Hsla,
}

/// One frame of canvas: the desk, and everything painted over it in order.
#[derive(Clone, Debug, Default)]
pub struct Canvas {
    /// The colour of the desk, which is also what the parts of the surface nothing covers are.
    pub desk: Hsla,
    /// Physical pixels per logical pixel, as the window is drawn at. The layer scales by this; the
    /// app never does (see the module docs).
    pub scale: f32,
    /// What is painted over the desk, in the order it is painted: the page's shadow, then the sheet.
    pub fills: Vec<Fill>,
}

impl Canvas {
    /// Empties the canvas, keeping the memory it was using.
    ///
    /// Called once per frame rather than rebuilding the description: a frame is a few rectangles,
    /// but "a few" is not a reason to allocate them anew sixty times a second.
    pub fn clear(&mut self) {
        self.fills.clear();
    }

    /// Adds a rectangle to the canvas, on top of what is already described.
    pub fn fill(&mut self, rect: Rect, colour: Hsla) {
        if !rect.is_empty() {
            self.fills.push(Fill { rect, colour });
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
            fills: Vec::new(),
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
