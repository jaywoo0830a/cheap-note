//! What the canvas is asked to paint: the desk, the paper, the page's picture, and the ink.
//!
//! This module is the *description* and nothing else — a list of rectangles and one list of strokes,
//! built by the view every frame and handed to whatever surface the platform installed (see
//! [`crate::platform::InkSurface`]). Painting it is the platform's job, and the reason the split is
//! here rather than further up is that a description is portable and a renderer is not: Windows
//! draws it with Direct2D under GPUI's surface, and a macOS build would draw the same description onto
//! a texture — with no change to the view that filled it in.
//!
//! The colours are [`Hsla`](gpui_kit::Hsla) because the view computes them from the theme; they are
//! converted to whatever the renderer wants at the far end of the seam.

pub mod canvas;

pub use canvas::{Canvas, Ink, Page, Rect};

use gpui_kit::{rgb, rgba, Hsla};

/// An ink colour — `0xRRGGBB`, or `0xAARRGGBB` for a highlighter — as a colour the canvas can paint.
///
/// The one conversion from the *format's* colour to the *view's*, and it lives here rather than in a
/// renderer because two things need the same answer: the surface that draws the ink, and the app's own
/// preview of a highlighter's band ([`crate::app`]). Two conversions would be two colours, and a
/// preview that did not match the ink it previews is the kind of bug nobody reports and everybody
/// notices.
pub fn ink_colour(color: u32) -> Hsla {
    let alpha = note_format::ink::alpha_of(color);
    if alpha >= 1.0 {
        return rgb(color).into();
    }

    let byte = (alpha * 255.0).round() as u32;
    let (r, g, b) = ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF);

    rgba((r << 24) | (g << 16) | (b << 8) | byte).into()
}
