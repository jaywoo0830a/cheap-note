//! The interface's type sizes at one interface scale.
//!
//! ## Why this is not in [`LayoutMetrics`](crate::design::LayoutMetrics)
//!
//! Type is not a four-unit length. Font sizes are authored as a base plus a few roles, and the
//! values do not need to divide by four (`doc/DESIGN.md` §7.6). The two systems share
//! [`ScaleStep`], not arithmetic: layout is exact whole pixels, type is a deliberate size at a
//! given step.
//!
//! ## Why the base is the one number that matters
//!
//! GPUI Kit sets the window's rem size from its theme's font size, and Tailwind-style utilities
//! (`text_sm`, `text_lg`, component paddings and icons) are rem multiples. Setting the base is
//! therefore enough to make every rem-based string and control follow the interface scale. The
//! other roles exist only for the few places that spell a size in pixels — a control's name, the
//! status pill, the floating bar's title.
//!
//! `base` is also the body size: the name is kept because it is what the theme is told.

use super::ScaleStep;

/// The sizes the interface's text roles take at one scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeRamp {
    /// The base size, and the size of body text. This is the theme's font size, and therefore the
    /// window's rem size.
    pub base: f32,
    /// The smallest label: the name of a control in the bar.
    pub micro: f32,
    /// The status line and its pill.
    pub caption: f32,
    /// The floating bar's title.
    pub title: f32,
}

impl TypeRamp {
    /// Resolves the type roles at `step`.
    ///
    /// The shipped design is 13px body, 10px micro, 11px caption and 15px title. The quarter-steps
    /// keep every value exact in binary floating point, so a role at 150% is exactly 1.5 times its
    /// 100% value rather than a rounded approximation.
    pub fn at(step: ScaleStep) -> Self {
        let factor = step.factor();

        TypeRamp {
            base: 13.0 * factor,
            micro: 10.0 * factor,
            caption: 11.0 * factor,
            title: 15.0 * factor,
        }
    }
}

impl Default for TypeRamp {
    fn default() -> Self {
        Self::at(ScaleStep::DEFAULT)
    }
}
