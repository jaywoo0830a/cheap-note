//! The page list — re-exported from the format crate.
//!
//! Which pages a note has, in reading order, and how far each one has been turned, is a fact the
//! note records about itself (one `meta` row of JSON), so it belongs to the format rather than to
//! the view. See [`note_format::pages`]; what the app keeps is the toolbar's business of turning a
//! page and the sheet it is drawn on (see the app's `canvas` and `view`).

pub use note_format::pages::*;
