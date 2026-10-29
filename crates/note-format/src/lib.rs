//! A cheap-note note, as a file: the whole of the format, and nothing about how it is drawn.
//!
//! ## Why this is its own crate
//!
//! The app is a Windows program — a Direct2D canvas, a WM_POINTER pen capture, a layered cursor
//! window (see the app's own `doc/ARCHITECTURE.md`) — and none of that is portable. The *note* is:
//! a folder holding a SQLite database, a source PDF and attachments, carried between machines as a
//! zip of the same. That half is what a note-taking application on another platform can share, and
//! sharing it is only honest if the sharing is enforced by the compiler rather than by intent.
//!
//! So the format is this crate, and the rule it is built on is one sentence long:
//!
//! > **Nothing here may depend on a window, a pen, a GPU, or a PDF renderer.**
//!
//! The app depends on this crate; this crate depends on nothing of the app's. That is what makes
//! it possible to build a second application — on macOS, on Linux, or as a checker that never draws
//! anything — that reads and writes exactly the same notes.
//!
//! ## What crosses the line
//!
//! | Here (the format) | There (the app) |
//! |---|---|
//! | [`store`]: the schema, the ink's rows, the facts a note records | when a batch is written, and on which thread |
//! | [`chunk`]: a page's strokes as one compressed, checksummed blob | how a stroke is drawn |
//! | [`ink`]: what a stroke *is* — points, widths, a colour, derived geometry | the document, the tools, the eraser, undo |
//! | [`history`]: an edit as a value that can be reverted | when the reader is offered one |
//! | [`pages`]: the page list, in reading order, with each page's rotation | what a page looks like |
//! | [`settings`]: the settings a *note* remembers, one `meta` row each | the ones that belong to the reader |
//! | [`paper`]: the sheet sizes and styles a note can be written on | the sheet on screen |
//! | [`note`]: the folder, the zip it is carried in, the writer thread's job queue | the bar's *Save* |
//! | [`error`]: what the format refuses, and why | what the status line says |
//!
//! The documents that go with each half are in the repository: `doc/STORE.md` is one stroke's
//! journey to the disk and back, which is this crate; `doc/VIEW.md` is the frame loop, the sheet and
//! the pen's rules, which are the app's.
//!
//! ## Two test suites, and why both
//!
//! * **Inside each module** (`#[cfg(test)] mod tests`) are the tests that need to see a module's own
//!   insides — a private helper, a cursor, the exact SQL one statement runs. They are the tests a
//!   change to that module has to satisfy.
//! * **In `tests/`**, one file per module and named after it, are the tests that use *nothing but the
//!   public API* — and that open a note with SQLite, a zip reader or a JSON parser rather than with
//!   this crate. They are the contract a second implementation owes: the schema version, the names
//!   inside a folder, the bytes of a chunk, the refusals. A change that passes the first and fails the
//!   second has changed the *format* rather than the code, which is the thing worth being told about.
//!
//! `cargo test --workspace` runs both, along with the app's.
//!
//! ## The one thing this crate deliberately does not promise
//!
//! **Two applications will not draw the same page the same way.** The format stores a stroke as
//! positions, a width per position and a colour ([`ink::Stroke`]); how that becomes pixels — a
//! ribbon outline, a spline, a quad — is the drawing application's business, and a macOS build using
//! CoreGraphics will not produce the same pixels as this one using Direct2D. What is guaranteed is
//! that it will be *the same ink*: the same points, in the same colour, in the same place on the
//! same page.

pub mod chunk;
pub mod error;
pub mod history;
pub mod ink;
pub mod note;
pub mod pages;
pub mod paper;
pub mod settings;
pub mod store;

pub use error::{Error, Result};
