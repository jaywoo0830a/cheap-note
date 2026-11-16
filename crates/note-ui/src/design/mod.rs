//! The interface's one scale, expressed as design units and integer arithmetic.
//!
//! A design length is written in **design units** at 100%: the number the layout was drawn at when
//! the app shipped. The person's interface scale multiplies that number, and the result is a whole
//! number of logical pixels. There is no platform factor and no display density in this module: the
//! windowing system speaks logical pixels, and a platform's own scaling is the windowing system's
//! business. The same scale therefore produces the same *logical* layout on every machine; the
//! operating system owns the physical density beneath it.
//!
//! ## Why the grid is four units and the steps are quarters
//!
//! Design units are multiples of [`GRID`] (4), and the allowed scales are [`ScaleStep::P50`]
//! through [`ScaleStep::P200`] in 25% steps. Four units at a quarter-step is always a whole pixel:
//!
//! ```text
//! 4 du × 75%  = 3 px
//! 4 du × 125% = 5 px
//! 12 du × 175% = 21 px
//! ```
//!
//! That is the whole of the arithmetic. [`px`] divides the design unit by the grid and multiplies by
//! the grid-sized quantum, so every result is exact — no rounding, no floating point, and no chance
//! of two call sites disagreeing by a pixel.
//!
//! ## What is deliberately absent
//!
//! This module knows nothing about GPUI, a window, a theme, a note, or a platform. An adapter that
//! turns [`LayoutPx`] into a framework's pixels or rems belongs outside it. Keeping the core pure is
//! what makes the tests exhaustive: every token can be checked at every allowed scale without
//! opening a window.
//!
//! Typography is deliberately separate: [`TypeRamp`] shares [`ScaleStep`] with the layout, but its
//! sizes are authored for text rather than derived from the four-unit grid. Its base is what GPUI
//! Kit uses as the window's rem size, so it also carries every rem-based text style and control.

mod metrics;
mod scale;
mod tokens;
mod type_ramp;

#[cfg(test)]
mod tests;

pub use metrics::LayoutMetrics;
pub use scale::{px, LayoutPx, ScaleStep};
pub use tokens::*;
pub use type_ramp::TypeRamp;
