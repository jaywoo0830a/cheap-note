//! `cheap-note` — a minimal, high-performance handwriting note prototype.
//!
//! ## What it is
//!
//! A window you can write in with a pen, with a PDF page behind the ink:
//!
//! * pen input comes from `pen-windows` (WM_POINTER: pressure, tilt, coalesced batches);
//! * the pen's tilt is drawn as a ghost cursor — a nib mark with the pen's body leaning away from
//!   it — because a system cursor cannot be rotated, and the system pointer is hidden while it is
//!   drawn. It is drawn *outside* the frame, in a window of its own, because a cursor is a position
//!   and a frame is one frame too late for one (see [`cursor_overlay`]);
//! * the interface is built with GPUI Kit (theme tokens, components, a GPU-accelerated scene), in
//!   the app's own palette and its own font: see [`theme`];
//! * PDF pages are rasterised by Pdfium, which the app drives through its own C entry points so
//!   that a render can be sliced across frames and the budget can bound it (see [`pdfium`]);
//! * the writing loop is *not* paced against the display: the pump parks on the pen's queue and
//!   draws a frame per batch, so the ink reaches the screen at the rate the pen reports it. Nothing
//!   else is limited by the display either — the housekeeping loop that rasterises pages and
//!   rebuilds the counters runs on a fixed interval of its own.
//!
//! ## Module map
//!
//! | Module      | Responsibility                                                     |
//! | ----------- | ------------------------------------------------------------------ |
//! | [`pen`]     | the capture, its worker thread, and the hand-off queue             |
//! | [`ink`]     | readings to strokes: edges, resampling, width, erasing             |
//! | [`canvas`]  | the sheet's size, colour and ruling                                |
//! | [`view`]    | zoom, fit, and where the sheet sits in the window                  |
//! | [`cursor`]  | the pen's ghost cursor: where it is and how it leans               |
//! | [`cursor_overlay`] | the ghost's own window, drawn without waiting for a frame     |
//! | [`system_cursor`] | hiding the system pointer while the pen is in range          |
//! | [`ink_layer`] | the canvas's own renderer: the desk, the paper and the ink, outside the frame |
//! | [`pdf`]     | the Pdfium document, page rendering, and the page cache            |
//! | [`pages`]   | what a note's pages are, and what each one shows (the format's)     |
//! | [`bookmarks`] | the pages a note has marked, and the screen that lists them      |
//! | [`outline`] | the document's own table of contents, and the screen that lists it  |
//! | [`pdfium`]  | Pdfium's own C API: documents, pages, and the sliced render        |
//! | [`store`]   | the note's SQLite file: the schema, the batch write, the read path (the format's) |
//! | [`note`]    | a note as a folder, the writer thread, and the file it is carried in (the format's) |
//! | [`recent`]  | what has been opened, and the index of it the app keeps             |
//! | [`home`]    | the screen that offers what was opened, and what it does            |
//! | [`app`]     | the view: the bar and the pills, canvas painting, the two pumps     |
//! | [`timing`]  | what every hot path costs, measured rather than guessed            |
//! | [`settings`]| what a note remembers, one row at a time (the format's)             |
//! | [`theme`]   | the palette, the bundled font, and the icons' asset source         |
//! | [`error`]   | the error type every fallible boundary returns                     |
//!
//! Four of those — [`pages`], [`store`], [`note`] and [`settings`] — are re-exports. What a note
//! *is* on disk is not the application's, so it lives in the `note-format` crate, together with the
//! chunk codec (a page of ink as one compressed, checksummed blob) and the stroke itself
//! ([`ink::Stroke`]). Nothing in that crate knows about a window, a pen or a GPU, and that is the
//! point of it: a second application, on another platform, can be built against the same format and
//! read and write the same notes. The app depends on the format; the format depends on nothing here.
//!
//! ## The documents
//!
//! [`doc/ARCHITECTURE.md`](../doc/ARCHITECTURE.md) is the picture of the whole program: which thread
//! owns what, what crosses a thread boundary, and what may wait for what. [`doc/VIEW.md`](../doc/VIEW.md)
//! is why the view is the way it is, and [`doc/STORE.md`](../doc/STORE.md) follows one stroke to the
//! disk and back.
//!
//! ## Running
//!
//! ```text
//! cargo run --release
//! ```
//!
//! A release build is the window and nothing else: no console appears beside it, because the
//! executable is linked as a *windows* subsystem program rather than as a console one (see the crate
//! attribute below). Windows creates a console window for a console-subsystem executable whenever it
//! is started *without* one already attached — which is what double-clicking it does — and this is an
//! application with a window of its own, not a command that prints. The cost is that the diagnostics
//! a run can print (see [`theme`]: the bundled font failing to register) go nowhere in release: with
//! no console attached, Rust discards what is written to `stderr`. A debug build, and every test
//! build, keeps the console — the measurements the tests print are the point of them.
//!
//! Pdfium is loaded at run time from `vendor/lib/pdfium.dll`, which a build copies next to the
//! executable it produces (`build.rs`; `vendor/README.md` says where the library comes from). The app
//! starts and draws without it; only opening a PDF needs it.
//!
//! A note is a folder under `%LOCALAPPDATA%\cheap-note\notes` — a SQLite file, the document, and
//! anything attached to it — and it is written as the pen moves; `Save` writes the single file a
//! person carries to another machine. **`doc/STORE.md` explains that arrangement in full.**
//!
//! The window opens on a list of what has already been written in — the [`home`] screen — and one
//! Escape from any note returns to it. The list is built from `%LOCALAPPDATA%\cheap-note\recent.json`
//! (see [`recent`]): a *cache* beside the notes, which a launch reads with a `stat` per note rather
//! than a database read. A path on the command line opens that instead, and skips the list.

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

mod app;
mod bookmarks;
mod canvas;
mod cursor;
mod cursor_overlay;
mod error;
mod history;
mod home;
mod ink;
mod ink_layer;
mod note;
mod outline;
mod pages;
mod pdf;
mod pdfium;
mod pen;
mod recent;
mod settings;
mod store;
mod system_cursor;
mod theme;
mod timing;
mod view;

use gpui_kit::component::Root;
use gpui_kit::*;

use std::path::PathBuf;

use crate::app::{Redo, Undo};

fn main() {
    // The icons: the toolbar draws pen, eraser, page and zoom marks, and the *full* Lucide catalog
    // is what carries those. The default bundle is the component library's own set — a hundred or so
    // marks that describe a generic application and include none of the ones a notebook needs.
    let application = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);

    application.run(move |cx| {
        // GPUI Kit must be initialized before any of its components or theme tokens are used.
        gpui_kit::init(cx);

        // And, immediately after that, the one thing the canvas needs from the window itself: a
        // surface with no background of its own, so that the desk the canvas paints is visible
        // through it. Registered here — before the window — because a window's plugins are read
        // when it is created (see [`ink_layer`]).
        ink_layer::leave_the_surface_unpainted(cx);

        // The app's own look, before the first element is built: the bundled font and the palette.
        // After `init`, because that is what creates the theme this writes into, and before the
        // window, because the first frame is laid out with the font.
        theme::install(cx);

        // The keyboard's way to undo and redo, which the bar's buttons are the other way to reach
        // (see the `actions!` declaration in `app`). Three bindings for two commands, because Windows
        // applications reach redo with `Ctrl+Y` while everything else reaches it with `Ctrl+Shift+Z`,
        // and there is no text field anywhere in this app for either to collide with.
        //
        // Everything else a key does — the home screen's list, Escape, `Ctrl+N`, `Ctrl+O`, `Ctrl+S` —
        // is handled by the screen that is showing, as a listener the painted element registers. That
        // is what lets a key reach a window with nothing focused, and lets a *character* reach the
        // list's filter without a text field to type it into. See [`home`].
        //
        // Registered on the application rather than on the view: a binding belongs to the window
        // that will receive the keystroke, and the view does not exist yet at this point.
        cx.bind_keys([
            KeyBinding::new("ctrl-z", Undo, None),
            KeyBinding::new("ctrl-y", Redo, None),
            KeyBinding::new("ctrl-shift-z", Redo, None),
        ]);

        // What the app was asked to open: a path on the command line, which is what a file
        // association, a shortcut, or a drag onto the executable becomes. With one, the home screen
        // is never seen — the note itself is the first frame.
        let start = std::env::args_os().nth(1).map(PathBuf::from);

        cx.spawn(async move |cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(80.0), px(80.0)),
                    size: size(px(1280.0), px(900.0)),
                })),
                window_min_size: Some(size(px(760.0), px(520.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("cheap-note".into()),
                    ..Default::default()
                }),
                // The window is not opaque, because the desk is not painted by it: the canvas is a
                // renderer of its own, composed *behind* what GPUI draws (see [`ink_layer`]), and
                // the only way to see it through is to leave the pixels GPUI does not paint
                // transparent. What this costs is subpixel text rendering, which a window with a
                // background it does not own cannot have — the interface is drawn in grayscale
                // antialiasing instead.
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            };

            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| app::NoteApp::new(window, start, cx));
                // The first element in a window must be a `Root`: it owns the overlay layers
                // the styled components draw into.
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open the cheap-note window");
        })
        .detach();

        // Closing the only window ends the application.
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}
