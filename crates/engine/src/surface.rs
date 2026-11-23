//! The seam the platform halves implement (`doc/ENGINE.md` §5.4).
//!
//! The engine's head decides *what* changed; a half decides *how pixels are made*. Everything here is a
//! description in logical window pixels — no device, no handle, no platform type — so the declaration is
//! portable even though its implementations are not.

use crate::band::Triangle;
use crate::footprint::Rect;

/// How a fill resolves the places its triangles overlap.
///
/// `NonZero` is the house rule: a stroke is one closed region, so a band that doubles back is still
/// ink and never a hole (`doc/ARCHITECTURE.md`, invariant 17). The two halves implement it
/// differently — Metal with the stencil, Direct2D with one realization — and the contract is the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
}

/// One band to fill: the triangles, the colour they are written in (`0xAARRGGBB`, alpha in the top
/// byte), and the rule.
#[derive(Clone, Debug, PartialEq)]
pub struct BandDraw {
    pub triangles: Vec<Triangle>,
    pub colour: u32,
    pub rule: FillRule,
}

/// A rectangle to fill: the desk, the sheet's shadow, or a rule of the ruling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FillDraw {
    pub rect: Rect,
    pub colour: u32,
}

/// What a half must do to draw the ink layer, in the order it must do it.
///
/// `repair` comes first for every band: the damage is cleared back to transparent and the base layer
/// under it is composited again, so an edit that uncovered pixels leaves no ink behind (`doc/ENGINE.md`
/// §4.2). Then the bands are filled, each once, in the union's rule.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkDraw {
    pub repair: Vec<Rect>,
    pub bands: Vec<BandDraw>,
}

/// What a half must do to draw the base layer: the desk, the sheet, and the page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BaseDraw {
    pub fills: Vec<FillDraw>,
}

/// One frame's work, as the engine hands it to a half.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub base: BaseDraw,
    pub ink: InkDraw,
}

/// Whether a present happened, or why it did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentResult {
    /// The frame is with the compositor.
    Presented,
    /// A drawable is not ready. Not an error: the canvas stays dirty and the next wake tries again
    /// (`doc/ENGINE.md` §5.6).
    NotReady,
}

/// A platform's canvas, as the engine uses it.
///
/// One implementation per platform (`cheap-note-engine-metal`, and one day `-d2d`). Nothing here
/// may block: a `present` that waited for the compositor is the one wait this design measured to be
/// worse than a dropped frame (`doc/VIEW.md` §2).
pub trait Surface {
    /// The surface's size, in logical pixels.
    fn size(&self) -> (f32, f32);

    /// The surface was resized: the layers are the window's, so both are re-laid.
    fn resize(&mut self, width: f32, height: f32);

    /// Draws one frame. `Ok(NotReady)` is a wait the caller does not take; `Err` is a half that has
    /// lost its device and is a canvas that is gone (`doc/ENGINE.md` §4.3).
    fn draw(&mut self, frame: &Frame) -> Result<PresentResult, SurfaceError>;

    /// Whether this half can honour vertical sync at all. Windows cannot (`Present(0, …)`), so its
    /// switch is left out of the bar; Metal can (`displaySyncEnabled`).
    fn honours_vsync(&self) -> bool {
        true
    }
}

/// Why a half could not draw. A sentence, because a platform's own error is its own
/// (`note_ui::error::AppError` names the boundary the same way).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceError {
    pub reason: String,
}

impl SurfaceError {
    /// A failure named by what the half was trying to do.
    pub fn new(reason: impl Into<String>) -> Self {
        SurfaceError {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the canvas could not be drawn: {}", self.reason)
    }
}

impl std::error::Error for SurfaceError {}
