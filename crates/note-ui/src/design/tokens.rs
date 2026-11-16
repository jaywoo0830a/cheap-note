//! The interface's design tokens, in design units at 100%.
//!
//! Every token is a multiple of [`GRID`]. That is not a cosmetic convention: [`crate::design::px`]
//! turns a token into a whole logical pixel at every allowed scale by dividing it into grid cells,
//! so a value off the grid would not have an exact answer at a quarter-step.
//!
//! [`DesignUnit::new`] is the validating constructor for numbers that come from somewhere else. The
//! constants below are written directly on the grid. A value off it would be a design error rather
//! than a runtime state, so the tests walk every token and fail on a typo before it reaches layout.

/// The design grid, in design units.
///
/// Four units is the smallest spacing that can be scaled by 25% and stay a whole pixel. The choice
/// is what couples the two otherwise independent decisions — a four-unit grid and quarter-step
/// scales — into one exact arithmetic.
pub const GRID: u16 = 4;

/// A length on the design grid, measured at 100%.
///
/// The wrapped value is a number of design units, not a number of window pixels. The distinction
/// matters: a design unit is always the same number in the source, and only [`crate::design::px`]
/// turns it into a pixel for a particular scale.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DesignUnit(u16);

impl DesignUnit {
    /// No length at all.
    pub const ZERO: DesignUnit = DesignUnit(0);

    /// A design length, if it lies on the grid.
    ///
    /// A value that is not a multiple of [`GRID`] is refused rather than rounded: rounding here
    /// would hide the mistake until two call sites rounded differently.
    pub const fn new(units: u16) -> Option<Self> {
        if units % GRID == 0 {
            Some(DesignUnit(units))
        } else {
            None
        }
    }

    /// The number of design units.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The number of whole grid cells in this length.
    ///
    /// This is the form the scaler uses. It is exact for every value that can be constructed
    /// through [`Self::new`] or read from this module's constants.
    pub const fn grids(self) -> u16 {
        self.0 / GRID
    }
}

/// The spacing ramp: four units, then multiples of it.
pub const SPACING: [DesignUnit; 8] = [
    SPACE_1, SPACE_2, SPACE_3, SPACE_4, SPACE_6, SPACE_8, SPACE_12, SPACE_16,
];

pub const SPACE_1: DesignUnit = DesignUnit(4);
pub const SPACE_2: DesignUnit = DesignUnit(8);
pub const SPACE_3: DesignUnit = DesignUnit(12);
pub const SPACE_4: DesignUnit = DesignUnit(16);
pub const SPACE_6: DesignUnit = DesignUnit(24);
pub const SPACE_8: DesignUnit = DesignUnit(32);
pub const SPACE_12: DesignUnit = DesignUnit(48);
pub const SPACE_16: DesignUnit = DesignUnit(64);

/// The three control heights the interface uses.
pub const CONTROLS: [DesignUnit; 3] = [CONTROL_SMALL, CONTROL_MEDIUM, CONTROL_LARGE];

pub const CONTROL_SMALL: DesignUnit = DesignUnit(24);
pub const CONTROL_MEDIUM: DesignUnit = DesignUnit(32);
pub const CONTROL_LARGE: DesignUnit = DesignUnit(40);
pub const CONTROL_GAP: DesignUnit = DesignUnit(8);

/// The large lengths that belong to the window's furniture rather than to an individual control.
pub const LAYOUT: [DesignUnit; 7] = [
    BAR_HEIGHT,
    BAR_MARGIN,
    PAGE_MARGIN,
    SCREEN_MARGIN,
    FOOT_BAND,
    FOOT_SLACK,
    STATUS_DROP,
];

/// The height of the floating bar, used as the estimate before its first measurement.
pub const BAR_HEIGHT: DesignUnit = DesignUnit(148);

/// The air between the floating bar and the window's edges.
pub const BAR_MARGIN: DesignUnit = DesignUnit(12);

/// The air between the sheet and the window's edges.
pub const PAGE_MARGIN: DesignUnit = DesignUnit(24);

/// The air between a full screen's content and the window's edges.
///
/// The home, bookmarks and contents screens are pages of their own rather than furniture over the
/// sheet, so their margin is wider than [`PAGE_MARGIN`]: it is the width that makes a card on the
/// desk read as a page rather than as a panel.
pub const SCREEN_MARGIN: DesignUnit = DesignUnit(56);

/// The height of the desk's row before a pill has been measured.
pub const FOOT_BAND: DesignUnit = DesignUnit(72);

/// The distance past a pill's edge that a pen reading still belongs to the pill.
pub const FOOT_SLACK: DesignUnit = DesignUnit(12);

/// How far below the bar's bottom edge the status pill hangs.
pub const STATUS_DROP: DesignUnit = DesignUnit(12);

/// The radii the interface uses, in design units.
pub const RADII: [DesignUnit; 3] = [RADIUS_SMALL, RADIUS_MEDIUM, RADIUS_LARGE];

pub const RADIUS_SMALL: DesignUnit = DesignUnit(4);
pub const RADIUS_MEDIUM: DesignUnit = DesignUnit(8);
pub const RADIUS_LARGE: DesignUnit = DesignUnit(16);
