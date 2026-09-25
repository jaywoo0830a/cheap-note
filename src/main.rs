//! `cheap-note` — a minimal, high-performance handwriting note prototype.
//!
//! ## What it is
//!
//! A window you can write in with a pen, with a PDF page behind the ink:
//!
//! * pen input comes from `pen-windows` (WM_POINTER: pressure, tilt, coalesced batches);
//! * the pen's tilt is drawn as a ghost cursor — a nib mark with the pen's body leaning away from
//!   it — because a system cursor cannot be rotated, and the system pointer is hidden while it is
//!   drawn;
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
//! | [`system_cursor`] | hiding the system pointer while the pen is in range          |
//! | [`pdf`]     | the Pdfium document, page rendering, and the page cache            |
//! | [`pages`]   | what a note's pages are, and what each one shows                   |
//! | [`pdfium`]  | Pdfium's own C API: documents, pages, and the sliced render        |
//! | [`store`]   | the note's SQLite file: the schema, the batch write, the read path |
//! | [`chunk`]   | a page of ink as one blob: SoA, varints, zstd, CRC32               |
//! | [`note`]    | a note as a folder, the writer thread, and the file it is carried in |
//! | [`recent`]  | what has been opened, and the index of it the app keeps             |
//! | [`home`]    | the screen that offers what was opened, and what it does            |
//! | [`app`]     | the view: the bar and the pills, canvas painting, the two pumps     |
//! | [`timing`]  | what every hot path costs, measured rather than guessed            |
//! | [`settings`]| the user's tuning, serialised as JSON                              |
//! | [`theme`]   | the palette, the bundled font, and the icons' asset source         |
//! | [`error`]   | the error type every fallible boundary returns                     |
//!
//! ## Running
//!
//! ```text
//! cargo run --release
//! ```
//!
//! Pdfium is loaded at run time from `vendor/lib/pdfium.dll` (or next to the executable). The
//! app starts and draws without it; only opening a PDF needs it.
//!
//! A note is a folder under `%LOCALAPPDATA%\cheap-note\notes` — a SQLite file, the document, and
//! anything attached to it — and it is written as the pen moves; `Save` writes the single file a
//! person carries to another machine. **`doc/STORE.md` explains that arrangement in full.**
//!
//! The window opens on a list of what has already been written in — the [`home`] screen — and one
//! Escape from any note returns to it. The list is built from `%LOCALAPPDATA%\cheap-note\recent.json`
//! (see [`recent`]): a *cache* beside the notes, which a launch reads with a `stat` per note rather
//! than a database read. A path on the command line opens that instead, and skips the list.

mod app;
mod canvas;
mod chunk;
mod cursor;
mod error;
mod home;
mod ink;
mod note;
mod pdf;
mod pdfium;
mod pen;
mod pages;
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
