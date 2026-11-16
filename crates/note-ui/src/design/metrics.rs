//! The layout's design tokens resolved once, at one interface scale.
//!
//! Callers should ask a [`LayoutMetrics`] for a length rather than call [`crate::design::px`] at
//! every use. The snapshot is the single place where a scale becomes pixels: every value in it is
//! computed from the same [`ScaleStep`], so a control cannot accidentally use one scale and its
//! neighbour another. It is also cheap to copy, which lets a render pass hold it without borrowing
//! the application.

use super::scale::{px, LayoutPx, ScaleStep};
use super::tokens;

/// Every layout length the interface's furniture needs, at one interface scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutMetrics {
    /// The scale that produced these values.
    pub scale: ScaleStep,
    /// The estimated height of the floating bar.
    pub bar_height: LayoutPx,
    /// The air between the floating bar and the window's edges.
    pub bar_margin: LayoutPx,
    /// The air between the sheet and the window's edges.
    pub page_margin: LayoutPx,
    /// The estimated height of the desk's row.
    pub foot_band: LayoutPx,
    /// How far past a pill's edge a pen reading still belongs to the pill.
    pub foot_slack: LayoutPx,
    /// How far below the bar's bottom edge the status pill hangs.
    pub status_drop: LayoutPx,
    /// The height of a small control.
    pub control_small: LayoutPx,
    /// The height of a medium control.
    pub control_medium: LayoutPx,
    /// The height of a large control.
    pub control_large: LayoutPx,
    /// The space between adjacent controls.
    pub control_gap: LayoutPx,
    /// The small corner radius.
    pub radius_small: LayoutPx,
    /// The medium corner radius.
    pub radius_medium: LayoutPx,
    /// The large corner radius.
    pub radius_large: LayoutPx,
}

impl LayoutMetrics {
    /// Resolves every layout token at `scale`.
    ///
    /// This is the only conversion in the module that a renderer has to call. Each field is the
    /// same integer expression the tests check against the scale's percentage, so the snapshot
    /// cannot drift from the tokens.
    pub const fn at(scale: ScaleStep) -> Self {
        LayoutMetrics {
            scale,
            bar_height: px(tokens::BAR_HEIGHT, scale),
            bar_margin: px(tokens::BAR_MARGIN, scale),
            page_margin: px(tokens::PAGE_MARGIN, scale),
            foot_band: px(tokens::FOOT_BAND, scale),
            foot_slack: px(tokens::FOOT_SLACK, scale),
            status_drop: px(tokens::STATUS_DROP, scale),
            control_small: px(tokens::CONTROL_SMALL, scale),
            control_medium: px(tokens::CONTROL_MEDIUM, scale),
            control_large: px(tokens::CONTROL_LARGE, scale),
            control_gap: px(tokens::CONTROL_GAP, scale),
            radius_small: px(tokens::RADIUS_SMALL, scale),
            radius_medium: px(tokens::RADIUS_MEDIUM, scale),
            radius_large: px(tokens::RADIUS_LARGE, scale),
        }
    }
}

impl Default for LayoutMetrics {
    /// The shipped design, at 100%.
    fn default() -> Self {
        Self::at(ScaleStep::DEFAULT)
    }
}
