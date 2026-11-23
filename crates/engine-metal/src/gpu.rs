//! The device seam: the one trait a test substitutes for Metal.
//!
//! `doc/ENGINE.md` §5.4's `Surface` is what the engine sees; this is what the *half* sees of its device.
//! Between them, every promise the engine makes is provable without a GPU: a test plan says what would
//! be drawn, and a fake device says what the half asked it to do. [`metal`](crate::metal) is the one
//! real implementation, and it is never compiled on a machine without Metal — see its module docs.

use crate::plan::Plan;

/// A surface the half can hand a frame to: a drawable to draw into, a present to hand it over, and the
/// size to allocate the two layers at.
pub trait Gpu {
    /// The target's device size, which changes only on a resize or a display change.
    fn size(&self) -> (u32, u32);

    /// Whether a drawable is available *now*. A `false` is not an error: the plan is not run, the canvas
    /// stays dirty, and the next wake tries again (`doc/ENGINE.md` §5.6).
    fn drawable(&self) -> bool;

    /// Runs a plan into the current drawable.
    ///
    /// The call must not wait for the GPU: a present is handed over and the compositor shows the newest
    /// frame it is given (`doc/ENGINE.md` §4.3). What a command does is the device's; what a command
    /// *is* is [`crate::plan`]'s.
    fn run(&mut self, plan: &Plan) -> Result<(), GpuError>;
}

/// Why a device could not run a plan. A sentence rather than a wrapped OS error, because what a driver
/// says is its own — the same choice `note_ui::error::AppError` makes at its boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuError {
    pub reason: String,
}

impl GpuError {
    /// A failure named by what the half was doing.
    pub fn new(reason: impl Into<String>) -> Self {
        GpuError {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the Metal device could not draw: {}", self.reason)
    }
}

impl std::error::Error for GpuError {}
