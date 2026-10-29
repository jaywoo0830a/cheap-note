//! A page's history — re-exported from the format crate.
//!
//! An edit as a value that can be reverted is what the note stores (one row per edit, in the order
//! made: see [`note_format::store`]), so the shape of an edit is the format's. See
//! [`note_format::history`] for why an edit is a value rather than a list of strokes; what the app
//! keeps is only when the reader is offered one, and how deep the session's own stack is.

pub use note_format::history::*;
