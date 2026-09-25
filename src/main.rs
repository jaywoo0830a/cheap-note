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
//!   draws a frame per batch, so the ink reaches the screen at the rate the pen reports it. The
//!   monitor's mode is still read (`EnumDisplaySettingsW`) and the frames this app paints are still
//!   measured, and both now pace the *housekeeping* loop — the monitor probe, the counters, and the
//!   PDF page being rasterised — rather than the writing itself.
//!
//! ## Module map
//!
//! | Module      | Responsibility                                                     |
//! | ----------- | ------------------------------------------------------------------ |
//! | [`refresh`] | supported refresh rates, the monitor's mode, and the measured frame rate |
//! | [`pen`]     | the capture, its worker thread, and the hand-off queue             |
//! | [`ink`]     | readings to strokes: edges, resampling, width, erasing             |
//! | [`canvas`]  | the sheet's size, colour and ruling                                |
//! | [`view`]    | zoom, fit, and where the sheet sits in the window                  |
//! | [`cursor`]  | the pen's ghost cursor: where it is and how it leans               |
//! | [`system_cursor`] | hiding the system pointer while the pen is in range          |
//! | [`pdf`]     | the Pdfium document, page rendering, and the page cache            |
//! | [`pages`]   | what a note's pages are, and what each one shows                   |
//! | [`pdfium`]  | Pdfium's own C API: documents, pages, and the sliced render        |
//! | [`bundle`]  | a saved note: the original PDF and the ink, in one zip             |
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

mod app;
mod bundle;
mod canvas;
mod cursor;
mod error;
mod ink;
mod pdf;
mod pdfium;
mod pen;
mod pages;
mod refresh;
mod settings;
mod system_cursor;
mod theme;
mod timing;
mod view;

use gpui_kit::component::Root;
use gpui_kit::*;

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
        // Registered on the application rather than on the view: a binding belongs to the window
        // that will receive the keystroke, and the view does not exist yet at this point.
        cx.bind_keys([
            KeyBinding::new("ctrl-z", Undo, None),
            KeyBinding::new("ctrl-y", Redo, None),
            KeyBinding::new("ctrl-shift-z", Redo, None),
        ]);

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
                let view = cx.new(|cx| app::NoteApp::new(window, cx));
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
