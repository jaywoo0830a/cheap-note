//! `cheap-note` — a minimal, high-performance handwriting note prototype.
//!
//! ## What it is
//!
//! A window you can write in with a pen, with a PDF page behind the ink:
//!
//! * pen input comes from `pen-windows` (WM_POINTER: pressure, tilt, coalesced batches);
//! * the interface is built with GPUI Kit (theme tokens, components, a GPU-accelerated scene);
//! * PDF pages are rasterised by Pdfium through `pdfium-render`;
//! * the writing loop is paced against the display's refresh rate (60, 120, 180 or 240 Hz).
//!
//! ## Module map
//!
//! | Module      | Responsibility                                             |
//! | ----------- | ---------------------------------------------------------- |
//! | [`refresh`] | supported refresh rates, detection, and frame pacing       |
//! | [`pen`]     | the capture, its worker thread, and the hand-off queue     |
//! | [`ink`]     | readings to strokes: edges, resampling, width, erasing     |
//! | [`pdf`]     | the Pdfium document, page rendering, and the page cache    |
//! | [`app`]     | the view: toolbar, canvas painting, and the pen pump       |
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
mod error;
mod ink;
mod pdf;
mod pen;
mod refresh;
mod settings;

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
