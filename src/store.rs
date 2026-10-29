//! The note store — the schema, the ink's rows, and the facts a note records about itself.
//!
//! **This module is a re-export, and deliberately nothing more.** What a note is on disk is the
//! format rather than the application, so it lives in the `note-format` crate
//! ([`note_format::store`]), which knows nothing about a window, a pen or a GPU. A second
//! application — on another platform, or a checker that never draws anything — depends on that
//! crate and reads exactly the same notes this one writes.
//!
//! The re-export is what keeps `crate::store::NoteStore` the name the rest of the app already uses,
//! and it is also the boundary made visible: anything this module cannot offer is something the app
//! reaches into the format for, and that should be a deliberate act rather than an accident.
//!
//! The full account of the arrangement — why a database replaced a zip of JSON, what the tables
//! mean, and what a save costs — is the format crate's own module documentation, and
//! `doc/STORE.md` beside it.

pub use note_format::store::*;
