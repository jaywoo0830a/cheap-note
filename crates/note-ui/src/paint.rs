//! What a surface needs from the view to paint a frame: where the paper is, and what colour the ink is.
//!
//! The canvas description ([`crate::ink_layer`]) is already the view's, and a renderer walks it without
//! help. This module is the far smaller set of *functions* a renderer calls back into because the answer
//! belongs to the view: the one place a paper point becomes a surface point, and the one place an ink
//! colour becomes a paint colour.
//!
//! It exists so that a platform crate depends on **four names** rather than on the ink model. The model
//! — the document, the tools, undo, the eraser — is none of a renderer's business, and a platform that
//! could reach it would eventually reach into it.

/// Where a point of the sheet is drawn, given the paper's rectangle and the page's turn.
///
/// The renderer's whole geometry is derived from three calls to this — the paper's origin and its two
/// unit steps — because the same function has to be right for both the ink (stored in paper
/// coordinates) and the sheet's own edges.
pub use crate::ink::drawn_of_paper;

/// How much of the page a stroke's colour lets through, in `0.0..=1.0`.
pub use note_format::ink::alpha_of;

/// A colour with an alpha stamped into its top byte: `0xAARRGGBB`.
pub use note_format::ink::with_alpha;

/// How wide the highlighter's band is, in logical pixels: the tool's own number, not a setting.
pub use crate::ink::HIGHLIGHTER_ALPHA;
