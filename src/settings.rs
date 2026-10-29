//! The settings a *note* remembers — re-exported from the format crate.
//!
//! There is no application-wide settings file: every setting belongs to a note, and is one `meta`
//! row in that note's database, so a note carried to another machine arrives with the pen, the
//! paper and the view it was written with. That is why the list lives here rather than in the app —
//! see [`note_format::settings`] — and why a second application reading a note finds the same
//! settings this one wrote.

pub use note_format::settings::*;
