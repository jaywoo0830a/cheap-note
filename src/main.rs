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
//! * the interface is built with GPUI Kit (theme tokens, components, a GPU-accelerated scene);
//! * PDF pages are rasterised by Pdfium through `pdfium-render`;
//! * the writing loop is paced against the display's frame rate: the monitor's mode is read
//!   (`EnumDisplaySettingsW`), the frames this app paints are measured, and the pump interval
//!   follows whichever of the two describes what the eye sees.
//!
//! ## Module map
//!
//! | Module      | Responsibility                                             |
//! | ----------- | ---------------------------------------------------------- |
//! | [`refresh`] | supported refresh rates, the monitor's mode, and the measured frame rate |
//! | [`pen`]     | the capture, its worker thread, and the hand-off queue     |
//! | [`ink`]     | readings to strokes: edges, resampling, width, erasing     |
//! | [`canvas`]  | the sheet's size, colour and ruling                        |
//! | [`view`]    | zoom, fit, and where the sheet sits in the window          |
//! | [`cursor`]  | the pen's ghost cursor: where it is and how it leans       |
//! | [`system_cursor`] | hiding the system pointer while the pen is in range |
//! | [`pdf`]     | the Pdfium document, page rendering, and the page cache    |
//! | [`bundle`]  | a saved note: the original PDF and the ink, in one zip     |
//! | [`app`]     | the view: toolbar, canvas painting, and the pen pump       |
//! | [`timing`]  | what every hot path costs, measured rather than guessed    |
//! | [`settings`]| the user's tuning, serialised as JSON                      |
//! | [`error`]   | the error type every fallible boundary returns             |
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
mod pen;
mod refresh;
mod settings;
mod system_cursor;
mod timing;
mod view;

use gpui_kit::component::Root;
use gpui_kit::*;

fn main() {
    let application = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    application.run(move |cx| {
        // GPUI Kit must be initialized before any of its components or theme tokens are used.
        gpui_kit::init(cx);

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
