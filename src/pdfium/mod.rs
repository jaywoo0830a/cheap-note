//! Pdfium, wrapped down to the part this app uses.
//!
//! ## Why this app declares the API itself
//!
//! `pdfium-render` wraps Pdfium well, and its *synchronous* `render_with_config` would have been
//! enough — except for one thing the app needs: a render that can be **interrupted**. Pdfium
//! exposes that through `FPDF_RenderPageBitmap_Start` / `FPDF_RenderPage_Continue` and an
//! `IFSDK_PAUSE` callback, and the wrapper does not surface any of it. The wrapper also keeps its
//! document and page handles private, so the raw calls cannot be reached from outside it.
//!
//! So this module resolves the handful of functions the app calls from the library directly. There
//! are **thirty-two** of them — documents, pages, bitmaps, the progressive render, the outline walk,
//! and page text — and every one is checked for existence as it is resolved: a Pdfium build without
//! an entry point fails with a message naming the missing symbol rather than crashing the process.
//! Pdfium's README calls the headers in its `public/` directory the stable surface and says nothing
//! outside it may be called; those thirty-two are the whole of this app's use of that surface
//! (<https://pdfium.googlesource.com/pdfium/+/refs/heads/main/README.md>).
//!
//! ## The header that ships with the library is the contract
//!
//! `vendor/include` is the header set that comes with the DLL in `vendor/lib`, and
//! [`api`] declares what those headers declare. That is not a formality: an ABI mismatch can write
//! through a pointer of the wrong width and still compile. The vendored headers are updated with
//! the library (see `vendor/README.md`), and the declarations are checked by the layout test in
//! [`api`] as well as by the compiler.
//!
//! ## The safe surface is the whole of what the rest of the app sees
//!
//! | Type | What it is |
//! |---|---|
//! | [`Document`] | an open PDF, owning the bytes Pdfium reads from |
//! | [`RenderJob`] | one page being rasterised in slices, owning its page and bitmap |
//! | [`Format`] | the colour format the pixels are asked for in |
//! | [`Progress`] | how far a slice got: unfinished, finished, or failed |
//! | [`OutlineEntry`], [`TextChar`] | plain Rust copies of an outline and a page's text |
//!
//! There is no public handle, no lifetime to get wrong, and no function that takes a raw pointer.
//! The private modules do the unsafe work:
//!
//! * [`api`] — the opaque C types, the resolved function table, initialisation, and the library
//!   search;
//! * [`document`] — the document's bytes, pages, outline, and text, with a page keeping its
//!   document alive;
//! * [`render`] — the bitmap, the pause callback, and the progressive render, with every handle
//!   released by `Drop`.
//!
//! ## Threading
//!
//! Pdfium is not thread-safe. [`Document`] and [`RenderJob`] are deliberately `!Send` and `!Sync`
//! (the inner document state carries a marker saying so), which makes using one from another
//! thread a compile error rather than an intermittent crash. The app uses them on the window
//! thread, in the housekeeping pump.
//!
//! ## Where the library comes from
//!
//! The repository does not carry Pdfium: it is downloaded into `vendor/` (see `vendor/README.md`),
//! and `/vendor` is ignored by git. A build copies `vendor/lib/*.dll` next to the executable it
//! produces (`build.rs`), and that is the first place this module looks — so a release build is a
//! *pair* of files that can be started from anywhere, or carried to another machine, and PDFs
//! open. See [`api`] for the rest of the search and what happens when nothing is found.

mod api;
mod document;
mod render;

pub use document::{Document, OutlineEntry, TextChar};
pub use render::{Format, Progress, RenderJob};

#[cfg(test)]
pub(crate) use api::{available, exclusive};
