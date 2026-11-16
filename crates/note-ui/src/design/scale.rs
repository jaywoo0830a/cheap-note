//! The allowed interface scales, and the integer conversion from design units to logical pixels.
//!
//! The scale is an enum rather than a number so that an unsupported scale cannot reach the layout.
//! It is also why the arithmetic is so small: every allowed percent is a multiple of 25, and every
//! design token is a multiple of four, so the two together always produce a whole pixel.

use super::tokens::{DesignUnit, GRID};

/// The interface scales a person may choose.
///
/// The names are the percentages. The range is deliberately closed and quarter-stepped: a 5% step
/// would need either fractional logical pixels or rounding, and this module exists to avoid both.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ScaleStep {
    P50,
    P75,
    P100,
    P125,
    P150,
    P175,
    P200,
}

impl ScaleStep {
    /// Every step, in increasing order.
    pub const ALL: [ScaleStep; 7] = [
        ScaleStep::P50,
        ScaleStep::P75,
        ScaleStep::P100,
        ScaleStep::P125,
        ScaleStep::P150,
        ScaleStep::P175,
        ScaleStep::P200,
    ];

    /// The step the interface starts at: the shipped design, 100%.
    pub const DEFAULT: ScaleStep = ScaleStep::P100;

    /// The percentage this step names.
    pub const fn percent(self) -> u16 {
        match self {
            ScaleStep::P50 => 50,
            ScaleStep::P75 => 75,
            ScaleStep::P100 => 100,
            ScaleStep::P125 => 125,
            ScaleStep::P150 => 150,
            ScaleStep::P175 => 175,
            ScaleStep::P200 => 200,
        }
    }

    /// The step's position in [`Self::ALL`].
    ///
    /// This is the number to persist in a preference file: it is stable, small, and cannot name a
    /// scale the interface does not have.
    pub const fn index(self) -> usize {
        match self {
            ScaleStep::P50 => 0,
            ScaleStep::P75 => 1,
            ScaleStep::P100 => 2,
            ScaleStep::P125 => 3,
            ScaleStep::P150 => 4,
            ScaleStep::P175 => 5,
            ScaleStep::P200 => 6,
        }
    }

    /// The step at a persisted index, if there is one.
    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(ScaleStep::P50),
            1 => Some(ScaleStep::P75),
            2 => Some(ScaleStep::P100),
            3 => Some(ScaleStep::P125),
            4 => Some(ScaleStep::P150),
            5 => Some(ScaleStep::P175),
            6 => Some(ScaleStep::P200),
            _ => None,
        }
    }
}

impl Default for ScaleStep {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A whole number of logical pixels, after the interface scale has been applied.
///
/// The newtype is the boundary marker: design units are what the design system owns, and these are
/// what a window can lay out. Keeping them distinct makes it impossible to pass an unscaled token to
/// a framework API by accident.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LayoutPx(u32);

impl LayoutPx {
    /// No length at all.
    pub const ZERO: LayoutPx = LayoutPx(0);

    /// A number of already-scaled logical pixels.
    pub const fn new(px: u32) -> Self {
        LayoutPx(px)
    }

    /// The number of logical pixels.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One design length at one interface scale, as a whole number of logical pixels.
///
/// The arithmetic divides the design unit into four-unit grid cells first, then multiplies by the
/// number of pixels one grid cell occupies at this scale. A 25% step makes that multiplier an
/// integer (two at 50%, three at 75%, four at 100%, five at 125%…), so the result is exact. There is
/// no floating-point value anywhere in the path, and therefore no difference between platforms.
pub const fn px(design: DesignUnit, scale: ScaleStep) -> LayoutPx {
    let quantum = GRID as u32 * scale.percent() as u32 / 100;
    LayoutPx(design.grids() as u32 * quantum)
}
