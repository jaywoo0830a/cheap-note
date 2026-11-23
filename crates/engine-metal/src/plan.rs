//! What one frame *is*, as a list of commands: the pure half of the Metal surface.
//!
//! `doc/ENGINE.md` §4.2 and §6: the order is the contract. For each band, the damage is repaired (cleared
//! to transparent and the base layer composited again), and then the band is filled **once** — mark the
//! pieces into the stencil, colour every marked sample a single time. A pan or a zoom changes only the
//! uniforms, so a plan for one is a plan with the same commands and different numbers.
//!
//! Nothing here touches Metal. It is arithmetic, and its tests read the command list a device would run.

use cheap_note_engine::band::Triangle;
use cheap_note_engine::footprint::Rect;
use cheap_note_engine::surface::{BandDraw, Frame, PresentResult};

/// The surface a plan draws into: its size in logical pixels and the display's scale.
///
/// Logical pixels and a scale *alongside* them, rather than one multiplied into every corner, is the
/// contract of `note_ui`'s canvas (`crates/note-ui/src/ink_layer/canvas.rs`): one multiplication per
/// draw, in the one place a shader applies it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    /// Width in logical pixels.
    pub width: f32,
    /// Height in logical pixels.
    pub height: f32,
    /// Device pixels per logical pixel.
    pub scale: f32,
}

impl Target {
    /// A target of a window's logical size at a display's scale.
    pub const fn new(width: f32, height: f32, scale: f32) -> Self {
        Target {
            width,
            height,
            scale,
        }
    }

    /// The target's size in device pixels, which is what its textures are allocated at.
    pub fn device_size(&self) -> (u32, u32) {
        let scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };

        (
            (self.width * scale).round().max(0.0) as u32,
            (self.height * scale).round().max(0.0) as u32,
        )
    }

    /// A rectangle in device pixels.
    pub fn device_rect(&self, rect: Rect) -> Rect {
        let scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };

        Rect {
            x: rect.x * scale,
            y: rect.y * scale,
            width: rect.width * scale,
            height: rect.height * scale,
        }
    }
}

/// One command a device runs. A plan is these, in order, and nothing else.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Clear this area of the ink layer and composite the base layer under it: what an edit or a new
    /// band uncovered (`doc/ENGINE.md` §4.2).
    Repair(Rect),
    /// Fill one band's triangles as a union: stencil first, colour once (E6).
    Band {
        triangles: Vec<Triangle>,
        colour: u32,
    },
    /// Hand the frame to the compositor. Never waited for (`doc/ENGINE.md` §5.6).
    Present,
}

/// One frame's commands, in the order a device runs them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    steps: Vec<Step>,
}

impl Plan {
    /// The plan for a frame: every repair, then every band, then a present.
    ///
    /// The order is the contract and it is why this is a function rather than a `for` loop at the call
    /// site: repairs must come before the fills that replace them, and the present must come after all
    /// of it. A frame with nothing in it still presents — the *engine* is what decides there is nothing
    /// to present by not handing a frame over at all (E1).
    pub fn of(frame: &Frame) -> Self {
        let mut steps = Vec::with_capacity(frame.ink.repair.len() + frame.ink.bands.len() + 1);

        for rect in &frame.ink.repair {
            steps.push(Step::Repair(*rect));
        }

        for BandDraw {
            triangles, colour, ..
        } in &frame.ink.bands
        {
            steps.push(Step::Band {
                triangles: triangles.clone(),
                colour: *colour,
            });
        }

        steps.push(Step::Present);

        Plan { steps }
    }

    /// The commands, in order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Whether this plan draws any ink. A plan with only a present is a frame that changed nothing on
    /// the ink layer.
    pub fn lays_ink(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, Step::Band { .. }))
    }

    /// The rectangles the plan repairs.
    pub fn repairs(&self) -> impl Iterator<Item = Rect> + '_ {
        self.steps.iter().filter_map(|step| match step {
            Step::Repair(rect) => Some(*rect),
            _ => None,
        })
    }

    /// The total triangle count, which is what a device uploads.
    pub fn triangles(&self) -> usize {
        self.steps
            .iter()
            .map(|step| match step {
                Step::Band { triangles, .. } => triangles.len(),
                _ => 0,
            })
            .sum()
    }
}

/// What the half did with a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ran {
    pub result: PresentResult,
    pub steps: usize,
}
