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
//!
//! ## Why there is a floor
//!
//! The laid-out lengths in [`LayoutMetrics`](crate::design::LayoutMetrics) can shrink to 50% and
//! still be a layout: a 74-pixel bar is unusual, not broken. Text is different. A proportional
//! 50% would put the smallest label at 5 px, which is not a small label but an illegible one, and
//! the person who chose 50% is the person least able to read it. Each role therefore has a
//! [`MINIMUM`] it never goes below, and the floor bites only at the steps that would otherwise
//! fall through it — [`ScaleStep::P75`] and [`ScaleStep::P50`] today. Above the floor the ramp is
//! still the exact proportion `doc/DESIGN.md` §7.6 promises, so a 150% role is still exactly 1.5
//! times its 100% value.
//!
//! This is the one place the module departs from "every length follows the step", and it is the
//! same distinction §7.5 draws for hairlines, the pen's nib mark and selection rings: a thing that
//! has a floor of its own does not scale below it, because what it is does not survive being
//! smaller.

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
    /// The shipped sizes at 100%, which every role is a proportion of.
    pub const SHIPPED: TypeRamp = TypeRamp {
        base: 13.0,
        micro: 10.0,
        caption: 11.0,
        title: 15.0,
    };

    /// The smallest each role may become, whatever step is chosen.
    ///
    /// The floor is the shipped `micro` size for the body roles and one pixel under it for the
    /// smallest label: at 100% the design already calls 10 px "the smallest label", so a smaller
    /// step may not go below what the design itself treats as the floor of legibility. `micro` gets
    /// 9 px rather than 10 so that a person who lowers the scale still sees *some* decrease in the
    /// small print rather than a ramp frozen entirely.
    ///
    /// These numbers bite at [`ScaleStep::P75`] (`micro` would be 7.5) and [`ScaleStep::P50`]
    /// (`base` would be 6.5); at [`ScaleStep::P100`] and above the ramp is the plain proportion.
    pub const MINIMUM: TypeRamp = TypeRamp {
        base: 10.0,
        micro: 9.0,
        caption: 10.0,
        title: 11.0,
    };

    /// Resolves the type roles at `step`, never going below [`Self::MINIMUM`].
    ///
    /// The shipped design is 13px body, 10px micro, 11px caption and 15px title. The quarter-steps
    /// keep every value exact in binary floating point, so a role at 150% is exactly 1.5 times its
    /// 100% value. Below 100% each role is clamped to its floor (see the module docs): the ramp is
    /// a proportion where that proportion stays readable, and a floor where it would not.
    pub fn at(step: ScaleStep) -> Self {
        let factor = step.factor();
        let floor = Self::MINIMUM;

        TypeRamp {
            base: (Self::SHIPPED.base * factor).max(floor.base),
            micro: (Self::SHIPPED.micro * factor).max(floor.micro),
            caption: (Self::SHIPPED.caption * factor).max(floor.caption),
            title: (Self::SHIPPED.title * factor).max(floor.title),
        }
    }

    /// Whether this ramp is the plain proportion of the shipped sizes rather than a clamped one.
    ///
    /// The view does not need this to draw; it exists so a test can say *where* the floor bites
    /// without repeating the arithmetic that decides it.
    pub fn is_proportional(self, step: ScaleStep) -> bool {
        self == TypeRamp::proportional(step)
    }

    /// The proportion of the shipped sizes at `step`, ignoring the floor.
    fn proportional(step: ScaleStep) -> Self {
        let factor = step.factor();

        TypeRamp {
            base: Self::SHIPPED.base * factor,
            micro: Self::SHIPPED.micro * factor,
            caption: Self::SHIPPED.caption * factor,
            title: Self::SHIPPED.title * factor,
        }
    }
}

impl Default for TypeRamp {
    fn default() -> Self {
        Self::at(ScaleStep::DEFAULT)
    }
}
