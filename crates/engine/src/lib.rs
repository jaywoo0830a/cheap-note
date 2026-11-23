//! The cheap-note ink engine: what changed, in the paper's own units.
//!
//! `doc/ENGINE.md` is the contract this crate is the portable half of. It has three jobs, and they are
//! the three sections after this one:
//!
//! * **[`place`]** — the one mapping between the paper's own units and the window's: a rotation and a
//!   placement, in the same arithmetic on every platform.
//! * **[`footprint`]** — what a reading changed, as rectangles in the window: the band it swept and the
//!   ink it uncovered. Nothing else in the engine decides what to draw.
//! * **[`journal`]** — the changed bands kept until a pause (or the cap) says the note may have them.
//!
//! Everything here is pure and synchronous. There is no timer, no thread and no device: the platform
//! halves (`cheap-note-engine-metal`, and one day `-d2d`) are *cursors* over what these return, and they
//! own every call that talks to a GPU.

pub mod band;
pub mod engine;
pub mod footprint;
pub mod journal;
pub mod page;
pub mod place;
pub mod surface;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use band::Band;
pub use engine::{Engine, FrameCap, State, StrokeWork};
pub use footprint::{Footprint, Rect};
pub use journal::{Journal, JournalBatch};
pub use page::PageBands;
pub use place::Placement;
pub use surface::{Frame, PresentResult, Surface, SurfaceError};
