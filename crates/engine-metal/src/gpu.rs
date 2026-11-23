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

    /// Draws one frame whole — base, ink, mark — and hands it over, in one command buffer.
    ///
    /// The order is the contract (`doc/ENGINE.md` §4.2): the base under the ink, the ink in its bands,
    /// the pen's mark over it. A half that can do it in one present does; one that cannot falls back to
    /// [`Self::base`], [`Self::run`] and [`Self::mark`] in that order, at the cost of presents nobody
    /// sees but the power meter.
    fn present(
        &mut self,
        base: &crate::plan::BaseLayer,
        plan: &Plan,
        mark: Option<&crate::plan::Mark>,
    ) -> Result<(), GpuError> {
        self.base(base)?;
        self.run(plan)?;
        self.mark(mark)
    }

    /// Draws the base layer — the desk, the sheet's shadow, the sheet and the ruling — for a frame.
    ///
    /// The engine does not decide what a desk looks like: `note-ui` describes it as rectangles and
    /// colours (`Canvas::fills`, `Canvas::rules`), and this is where they become pixels. Called before
    /// [`Self::run`] in the same command buffer, so a frame is one present rather than two.
    fn base(&mut self, base: &crate::plan::BaseLayer) -> Result<(), GpuError>;

    /// Draws the pen's mark over the ink, in the same pass as the base and the ink.
    ///
    /// A cursor is not ink and is not the interface: it is the one thing on the sheet whose whole
    /// meaning is *now* (`doc/VIEW.md` §5), and it is drawn here rather than in a frame because a frame
    /// is one frame too late for a position.
    fn mark(&mut self, mark: Option<&crate::plan::Mark>) -> Result<(), GpuError>;
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
