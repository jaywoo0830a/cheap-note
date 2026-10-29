//! A note as a folder, and the single file it is carried in — re-exported from the format crate.
//!
//! See [`note_format::note`] for the arrangement itself: the working copy the app writes into, the
//! zip a person carries between machines, and the writer thread's job queue. The application keeps
//! only what is *its* — which thread runs the queue (see [`NoteWriter`]), and when the bar's *Save*
//! asks for an export.

pub use note_format::note::*;
