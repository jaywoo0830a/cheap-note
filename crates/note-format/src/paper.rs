//! The paper a note is written on: its size, what is printed on it, and the app's own millimetre.
//!
//! ## Why the sizes are physical
//!
//! A canvas size *is* a paper size, and paper is specified in millimetres. Storing the physical
//! dimensions once and deriving both the drawn aspect ratio and the drawn width from a single
//! pixels-per-millimetre constant keeps the sizes comparable: A5 is really half of A4 on screen,
//! not merely a different shape that happens to be about as big. Picking a size therefore sets
//! both the shape and the scale, which is what a person choosing "A5" expects.
//!
//! ## Why this is here rather than in the view
//!
//! A note *remembers* the sheet it was written on — `sheet_width` and `sheet_height` in its `meta`
//! table, and the size and style in its settings (see [`crate::store`] and [`crate::settings`]) —
//! so the paper is part of the format, not of the window that happens to be drawing it. The
//! numbers are logical pixels at [`PIXELS_PER_MM`], which is a resolution-independent unit: a build
//! on a screen of another density draws the same sheet at its own scale and the ink still lands in
//! the same place on the page.
//!
//! What is *not* here is anything about pixels on a screen: the ruling geometry, the paper's
//! colour, the swatches. Those are the view's (see the app's `canvas.rs`).

use serde::{Deserialize, Serialize};

/// How many logical pixels one millimetre of paper is drawn at, and therefore the unit a pen's
/// thickness is measured in.
///
/// Chosen so the default sheet, A4, comes out 720 logical pixels wide — a comfortable reading size
/// in a 1280-pixel window. Every canvas size is drawn at this scale (`display_width` is its own
/// width in millimetres times this), so a width in logical pixels is the same number of millimetres
/// on A5 as on A4, which is what makes "0.6 mm" a fact about the pen and not about the page.
///
/// Public because the settings spell a pen's thickness out in millimetres (see
/// [`crate::settings::PenWeight::describe`]), and because the number is part of what two
/// applications have to agree on: it is the unit the ink is stored in.
pub const PIXELS_PER_MM: f32 = 720.0 / 210.0;

/// The shape and scale of the sheet written on when no PDF is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasSize {
    /// ISO A4, 210 by 297 mm: the default.
    A4,
    /// ISO A5, 148 by 210 mm: half a sheet of A4, for small notes.
    A5,
    /// US Letter, 8.5 by 11 in.
    Letter,
    /// US Legal, 8.5 by 14 in.
    Legal,
    /// A square sheet, for diagrams rather than prose.
    Square,
    /// A4 turned on its side, for wide material.
    Wide,
}

impl CanvasSize {
    /// Every size the toolbar offers, in the order it offers them.
    pub const ALL: [CanvasSize; 6] = [
        CanvasSize::A4,
        CanvasSize::A5,
        CanvasSize::Letter,
        CanvasSize::Legal,
        CanvasSize::Square,
        CanvasSize::Wide,
    ];

    /// The label on this size's button.
    pub fn label(self) -> &'static str {
        match self {
            CanvasSize::A4 => "A4",
            CanvasSize::A5 => "A5",
            CanvasSize::Letter => "Letter",
            CanvasSize::Legal => "Legal",
            CanvasSize::Square => "Square",
            CanvasSize::Wide => "Wide",
        }
    }

    /// The size a label names.
    ///
    /// The toolbar's chooser carries a choice back as the text that was showing in it — the
    /// options it holds are strings, not sizes — so the label is what has to become a size again.
    /// It is looked up by name rather than by position on purpose: the position is the one thing
    /// about the list that may change, and a size inserted in the middle must not rename the
    /// choice that was already made.
    pub fn from_label(label: &str) -> Option<CanvasSize> {
        CanvasSize::ALL
            .iter()
            .copied()
            .find(|size| size.label() == label)
    }

    /// The sheet's width and height in millimetres.
    pub fn millimetres(self) -> (f32, f32) {
        match self {
            CanvasSize::A4 => (210.0, 297.0),
            CanvasSize::A5 => (148.0, 210.0),
            CanvasSize::Letter => (215.9, 279.4),
            CanvasSize::Legal => (215.9, 355.6),
            CanvasSize::Square => (200.0, 200.0),
            CanvasSize::Wide => (297.0, 210.0),
        }
    }

    /// The height divided by the width.
    pub fn aspect(self) -> f32 {
        let (width, height) = self.millimetres();
        height / width
    }

    /// The width, in logical pixels, this size is drawn at when it is selected.
    pub fn display_width(self) -> f32 {
        self.millimetres().0 * PIXELS_PER_MM
    }

    /// The sheet's drawn size at the given width, in logical pixels.
    pub fn display_size(self, width: f32) -> (f32, f32) {
        (width, width * self.aspect())
    }
}

/// What is printed on the sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasStyle {
    /// A blank sheet.
    Plain,
    /// Horizontal rules, for prose.
    Ruled,
    /// A square grid, for diagrams and sketching.
    Grid,
    /// A dot grid: the least ink that still guides a line.
    Dots,
}

impl CanvasStyle {
    /// Every style the toolbar offers, in the order it offers them.
    pub const ALL: [CanvasStyle; 4] = [
        CanvasStyle::Plain,
        CanvasStyle::Ruled,
        CanvasStyle::Grid,
        CanvasStyle::Dots,
    ];

    /// The label on this style's button.
    pub fn label(self) -> &'static str {
        match self {
            CanvasStyle::Plain => "Plain",
            CanvasStyle::Ruled => "Ruled",
            CanvasStyle::Grid => "Grid",
            CanvasStyle::Dots => "Dots",
        }
    }

    /// The style a label names. See [`CanvasSize::from_label`] for why the lookup is by name.
    pub fn from_label(label: &str) -> Option<CanvasStyle> {
        CanvasStyle::ALL
            .iter()
            .copied()
            .find(|style| style.label() == label)
    }

    /// The distance between two rules, in logical pixels, or `None` for a blank sheet.
    ///
    /// The spacings differ by style on purpose: prose wants a taller line than a diagram wants a
    /// square, so a single number would leave one of the two cramped.
    pub fn spacing(self) -> Option<f32> {
        match self {
            CanvasStyle::Plain => None,
            CanvasStyle::Ruled => Some(34.0),
            CanvasStyle::Grid => Some(28.0),
            CanvasStyle::Dots => Some(28.0),
        }
    }
}
