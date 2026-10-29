//! The view: everything this application *does*, and the seam a platform plugs into.
//!
//! ## What this crate is
//!
//! A window you can write in with a pen, with a PDF page behind the ink — written once, for whatever
//! machine it is running on:
//!
//! * the interface is built with GPUI Kit (theme tokens, components, a GPU-accelerated scene), in the
//!   app's own palette and its own font: see [`theme`];
//! * readings become strokes here — edges, resampling, width, erasing — and the stroke is the
//!   *format's* ([`ink::Stroke`] is `note_format::ink::Stroke`, re-exported), so what is written on
//!   the page is what any other application reading the note will find;
//! * PDF pages are rasterised by Pdfium through this crate's own C entry points, so that a render can
//!   be sliced across frames and the budget can bound it (see [`pdfium`]);
//! * the writing loop is *not* paced against the display: the pump parks on the pen's queue and draws
//!   a frame per batch, so the ink reaches the screen at the rate the pen reports it. Nothing else is
//!   limited by the display either — the housekeeping loop that rasterises pages and rebuilds the
//!   counters runs on a fixed interval of its own.
//!
//! ## Module map
//!
//! | Module      | Responsibility                                                     |
//! | ----------- | ------------------------------------------------------------------ |
//! | [`input`]   | one pen reading in the app's own terms, and the queue it arrives through |
//! | [`platform`]| the seam: the four things a platform lends the view                |
//! | `keys`      | the chords, and how this platform writes them (`Ctrl+Z`, `⌘Z`)     |
//! | [`ink`]     | readings to strokes: edges, resampling, width, erasing             |
//! | [`canvas`]  | the sheet's size, colour and ruling                                |
//! | [`ink_layer`] | the canvas *description*: what the surface is asked to paint     |
//! | [`view`]    | zoom, fit, and where the sheet sits in the window                  |
//! | [`cursor`]  | the pen's ghost cursor: where it is and how it leans               |
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
//! ## Two boundaries, and they face opposite ways
//!
//! [`pages`], [`store`], [`note`] and [`settings`] are re-exports of the `note-format` crate: what a
//! note *is* on disk is not the application's, and nothing in that crate knows about a window, a pen
//! or a GPU. This crate depends on the format; the format depends on nothing here.
//!
//! Below this crate is a platform, and it depends on this crate rather than the other way round. What
//! a platform lends is in [`platform`] — a pen to read, a ghost cursor to follow the nib, a pointer to
//! hide, and a surface to paint the desk and the ink on — and `apps/windows` is one implementation of
//! those four. A macOS build would be another, and would leave everything above the seam alone.
//!
//! ## The documents
//!
//! [`doc/ARCHITECTURE.md`](../../doc/ARCHITECTURE.md) is the picture of the whole program: which
//! thread owns what, what crosses a thread boundary, and what may wait for what.
//! [`doc/VIEW.md`](../../doc/VIEW.md) is why the view is the way it is, and
//! [`doc/STORE.md`](../../doc/STORE.md) follows one stroke to the disk and back.
//!
//! A note is a folder under the user's application data — a SQLite file, the document, and anything
//! attached to it — and it is written as the pen moves; `Save` writes the single file a person carries
//! to another machine. **`doc/STORE.md` explains that arrangement in full.**
//!
//! The window opens on a list of what has already been written in — the [`home`] screen — and one
//! Escape from any note returns to it. The list is a *cache* beside the notes, which a launch reads
//! with a `stat` per note rather than a database read (see [`recent`]). A path on the command line
//! opens that instead, and skips the list.

pub mod cursor;
pub mod error;
pub mod ink_layer;
pub mod input;
pub mod paint;
pub mod platform;

mod app;
mod bookmarks;
mod canvas;
mod history;
mod home;
mod ink;
mod keys;
mod note;
mod outline;
mod pages;
mod pdf;
mod pdfium;
mod recent;
mod settings;
mod store;
mod theme;
mod timing;
mod view;

use std::path::PathBuf;

use gpui_kit::component::Root;
use gpui_kit::*;

use crate::app::{Redo, Undo};
use crate::platform::Installer;

/// Starts the application: one window, and the platform's four services inside it.
///
/// The whole of what a platform's `main` has to do, and the reason it can be six lines: everything a
/// window needs that is not about this machine — the toolkit's own initialization, the app's look, the
/// keys, the window's shape, and the view itself — happens here.
pub fn run(installer: Installer) {
    // The icons: the toolbar draws pen, eraser, page and zoom marks, and the *full* Lucide catalog is
    // what carries those. The default bundle is the component library's own set — a hundred or so
    // marks that describe a generic application and include none of the ones a notebook needs.
    let application = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);

    application.run(move |cx| {
        // GPUI Kit must be initialized before any of its components or theme tokens are used.
        gpui_kit::init(cx);

        // And, immediately after that, the one thing the canvas needs from the *application*: a
        // surface with no background of its own, so that the desk the canvas paints is visible through
        // it. Registered here — before the window — because a window's plugins are read when it is
        // created (see [`platform::Installer`]).
        (installer.before_window)(cx);

        // The app's own look, before the first element is built: the bundled font and the palette.
        // After `init`, because that is what creates the theme this writes into, and before the
        // window, because the first frame is laid out with the font.
        theme::install(cx);

        // The keyboard's way to undo and redo, which the bar's buttons are the other way to reach (see
        // the `actions!` declaration in `app`). Three bindings for two commands, because Windows
        // applications reach redo with `Ctrl+Y` while everything else reaches it with `⇧⌘Z` — and
        // there is no text field anywhere in this app for either to collide with.
        //
        // The chords say `secondary` rather than `ctrl`: that is the modifier the *platform* uses for
        // commands, and `gpui` resolves it per target (⌘ on macOS, Ctrl elsewhere). The specs come
        // from `keys`, which is also what the interface names them from, so a binding and the tooltip
        // that mentions it cannot disagree.
        //
        // Everything else a key does — the home screen's list, Escape, the new/open/save chords —
        // is handled by the screen that is showing, as a listener the painted element registers. That
        // is what lets a key reach a window with nothing focused, and lets a *character* reach the
        // list's filter without a text field to type it into. See [`home`].
        //
        // Registered on the application rather than on the view: a binding belongs to the window that
        // will receive the keystroke, and the view does not exist yet at this point.
        cx.bind_keys([
            KeyBinding::new(keys::chord::UNDO, Undo, None),
            KeyBinding::new(keys::chord::REDO, Redo, None),
            KeyBinding::new(keys::chord::REDO_ALT, Redo, None),
        ]);

        // What the app was asked to open: a path on the command line, which is what a file
        // association, a shortcut, or a drag onto the executable becomes. With one, the home screen is
        // never seen — the note itself is the first frame.
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
                // renderer of its own, composed *behind* what GPUI draws, and the only way to see it
                // through is to leave the pixels GPUI does not paint transparent. What this costs is
                // subpixel text rendering, which a window with a background it does not own cannot
                // have — the interface is drawn in grayscale antialiasing instead.
                //
                // A platform whose canvas cannot be composed under the window paints the desk itself
                // and says so from its own `InkSurface`: this is a hint about *this* window, not a
                // promise about the seam.
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            };

            cx.open_window(options, |window, cx| {
                let platform = (installer.install)(window);
                let view = cx.new(|cx| app::NoteApp::new(window, start, platform, cx));
                // The first element in a window must be a `Root`: it owns the overlay layers the
                // styled components draw into.
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
