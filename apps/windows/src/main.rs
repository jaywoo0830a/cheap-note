//! `cheap-note` on Windows.
//!
//! ## What this crate is
//!
//! The *platform* half of the application, and nothing else. Everything a note-taking program does —
//! the bar, the home screen, the sheet, the gestures, the page cache, the ink model, the frame loop —
//! is `crates/note-ui`, written once and portable. What is here is the four things that crate cannot
//! write for another machine:
//!
//! | Module | What it lends the view |
//! |---|---|
//! | [`platform::pen`] | `pen-windows` on a thread of its own: WM_POINTER readings, coalesced |
//! | [`platform::cursor_overlay`] | the nib's ghost, drawn in a layered window above everything |
//! | [`platform::system_cursor`] | `SetCursor`, so the system pointer is out of the way while it is |
//! | [`platform::ink_layer`] | the desk, the paper and the ink: Direct3D 11, Direct2D, DirectComposition |
//!
//! Plus Pdfium's own C entry points, which are `#[cfg(windows)]` in the view's `pdfium` module for
//! now, and `main`, which is the six lines that assemble the lot (see [`platform`]).
//!
//! ## What it means for the next platform
//!
//! A macOS build is this crate, rewritten: the canvas on Metal or CoreGraphics, the pen off `NSEvent`,
//! the ghost drawn inside the frame rather than in a window of its own, and `libpdfium.dylib` behind
//! `dlopen`. Nothing above the seam changes — that is what the seam is for.
//!
//! ## The documents
//!
//! [`doc/ARCHITECTURE.md`](../../doc/ARCHITECTURE.md) is the picture of the whole program: which thread
//! owns what, what crosses a thread boundary, and what may wait for what. [`doc/VIEW.md`](../../doc/VIEW.md)
//! is why the view is the way it is, and [`doc/STORE.md`](../../doc/STORE.md) follows one stroke to the
//! disk and back.
//!
//! Pdfium is loaded at run time from `vendor/lib/pdfium.dll`, which a build copies next to the
//! executable it produces (`build.rs`; `vendor/README.md` says where the library comes from). The app
//! starts and draws without it; only opening a PDF needs it.

// No console beside the window: this is a graphical application, so the linker is told to mark the
// image as a *windows* subsystem one (`/SUBSYSTEM:WINDOWS`). Without it the image is a console
// executable, and Windows opens a console window for one of those whenever the process is started
// with none already attached — double-clicking the executable, a shortcut, a file association, or a
// drag onto it — which is the black window that used to appear next to the app.
//
// Left to the debug and test builds on purpose. A run started *from* a terminal (`cargo run`) shares
// that terminal's console and would show either way, and `cargo test` prints the measurements its
// timing tests take: `cfg(test)` covers the test build of this crate, including `cargo test
// --release`, so those prints survive. What is lost in release is only a diagnostic (the font failing
// to register — `theme`) and a panic message, and only when there is no console to write them to.
#![cfg_attr(
    all(windows, not(debug_assertions), not(test)),
    windows_subsystem = "windows"
)]

mod platform;

fn main() {
    note_ui::run(platform::INSTALLER);
}
