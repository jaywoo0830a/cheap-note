//! The macOS half of the cheap-note engine.
//!
//! `doc/ENGINE.md` §2.1 and §5.1: the engine is written twice behind one seam, because the canvas is a
//! surface of the app's own in a window whose interface GPUI owns — a `CAMetalLayer` under GPUI's view
//! here, a second DirectComposition target on Windows. This crate is the Mac half, and it has the same
//! shape either side of the device:
//!
//! * **[`plan`]** is pure arithmetic with no device in it: a [`cheap_note_engine::Frame`] becomes a list
//!   of draw commands — repairs, then bands, then a present — in the order `doc/ENGINE.md` §4.2 says. A
//!   test reads the list; a device executes it.
//! * **[`gpu`]** is the trait a device implements (`Gpu`), so the plan can be checked without Metal and
//!   the Metal implementation can be checked without a window.
//! * **[`metal`]** is the device itself: `MTLDevice`, the command queue, the two pipelines (the stencil
//!   pass that makes a band the union of its pieces, and the colour pass that fills each marked sample
//!   once), and the drawable.
//!
//! The one thing the half must never do is decide *what* changed: that is the engine's, in the paper's
//! units. Here the plan is executed, the uniforms are uploaded, and the frame is handed over — never
//! waited for (`doc/ENGINE.md` §5.6).

pub mod gpu;
pub mod metal;
pub mod plan;

pub use cheap_note_engine::surface::{Frame, PresentResult, Surface, SurfaceError};
pub use gpu::{Gpu, GpuError};
pub use metal::{Device, MetalSurface};
pub use plan::{BaseLayer, Mark, Plan, RectFill, Step, Target};
