//! The application view: a command surface over an ink canvas.
//!
//! ## The layout, and why it is an overlay
//!
//! The ink canvas fills the whole content area and the toolbar is drawn *over* it. That is not
//! only a visual choice: `pen-windows` reports the pen in client-window coordinates, and GPUI
//! paints the canvas in window coordinates, so an ink point needs no layout arithmetic at all.
//! A toolbar that took part in the layout would sit at the top of the content area and every
//! stroke would have to be offset by its height — and that offset would have to be captured
//! from a layout callback, which is exactly the kind of coupling this avoids.
//!
//! ## The frame loop
//!
//! Nothing here polls for pen input. The pen thread pushes readings into a queue, and an async
//! task (see [`NoteApp::start_pen_pump`]) drains it on a timer derived from the display's
//! refresh rate, feeds the ink model, and calls `cx.notify()` — so a frame is scheduled exactly
//! when there is new ink, and the app is idle otherwise.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::Button;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use crate::ink::{InkDocument, Stroke, Tool};
use crate::pen::{capture_config, PenInbox, PenService};
use crate::pdf::{PdfDocumentView, RenderedPage};
use crate::refresh::{DisplayRefresh, RefreshMode, RefreshRate};
use crate::settings::Settings;

/// The bitmap-width multiplier used when rendering a PDF page.
///
/// Rendering at twice the logical width keeps page text crisp when the window is scaled and on
/// a high-DPI panel, and the cost is paid once per page rather than once per frame.
const PDF_RENDER_SCALE: f32 = 2.0;

/// The margin, in logical pixels, between a page and the window's edges.
const PAGE_MARGIN: f32 = 24.0;

/// The application view.
pub struct NoteApp {
    /// The user's tuning, persisted between runs.
    settings: Settings,
    /// Where [`Self::settings`] is written.
    settings_path: PathBuf,
    /// The display probe and the rate the app paces itself at.
    refresh: DisplayRefresh,
    /// The page's ink.
    ink: InkDocument,
    /// The PDF being annotated, if any.
    pdf: PdfDocumentView,
    /// The pen capture and its queue.
    pen: PenService,
    /// The page being shown.
    page_index: usize,
    /// The window's DPI scale factor, captured each frame.
    scale: f32,
    /// The pump's current interval in microseconds, shared with the pump task.
    ///
    /// Changing the refresh rate has to change how often the queue is drained, and the pump
    /// task is already running, so the interval lives in an atomic the task re-reads rather
    /// than in a captured local it could not see a change to.
    pump_interval_micros: Arc<AtomicU64>,
    /// The last thing worth telling the user.
    message: String,
}

impl NoteApp {
    /// Builds the view, attaches the pen, and starts the frame loop.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings_path = Settings::default_path();

        let (settings, mut message) = match Settings::load(&settings_path) {
            Ok(settings) => (settings, String::new()),
            Err(error) => (Settings::default(), format!("using default settings ({error})")),
        };

        let refresh = DisplayRefresh::probe(settings.refresh);

        // The capture must be attached on the thread that owns the window, which is this one.
        let pen = PenService::attach(window, capture_config());

        if message.is_empty() {
            message = pen.status().to_string();
        }

        let mut app = NoteApp {
            settings,
            settings_path,
            refresh,
            ink: InkDocument::new(),
            pdf: PdfDocumentView::empty(),
            pen,
            page_index: 0,
            scale: window.scale_factor(),
            pump_interval_micros: Arc::new(AtomicU64::new(
                refresh.effective.pump_interval().as_micros() as u64
            )),
            message,
        };

        app.start_pen_pump(cx);
        app
    }

    /// Drains the pen queue on a timer and repaints when the ink changed.
    ///
    /// The loop parks on a timer rather than spinning: the interval comes from the refresh
    /// rate (half a frame, clamped), so a 240 Hz panel gets readings onto the screen in about
    /// 2 ms and a 60 Hz panel does not wake up more often than it needs to. The task ends when
    /// the view is dropped — `update` returns `Err` once the entity is gone.
    fn start_pen_pump(&mut self, cx: &mut Context<Self>) {
        let inbox: Arc<PenInbox> = self.pen.inbox();
        let interval_micros = Arc::clone(&self.pump_interval_micros);

        cx.spawn(async move |this, cx| loop {
            let interval =
                Duration::from_micros(interval_micros.load(Ordering::Relaxed).max(500));
            cx.background_executor().timer(interval).await;

            let samples = inbox.take();
            if samples.is_empty() {
                continue;
            }

            let alive = this.update(cx, |app, cx| {
                app.ink.consume(&samples, app.scale, &app.settings);
                cx.notify();
            });

            if alive.is_err() {
                // The view is gone; the app is closing.
                break;
            }
        })
        .detach();
    }

    /// Selects the tool an ordinary nib uses.
    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.ink.set_mode(tool);
        cx.notify();
    }

    /// Hands the app back to the display, or pins it to one supported rate.
    ///
    /// The pump interval is updated through the shared atomic, so the running pump task picks
    /// the new rate up on its next wake without a restart.
    fn set_refresh_mode(&mut self, mode: RefreshMode, cx: &mut Context<Self>) {
        self.settings.refresh = mode;
        self.refresh = DisplayRefresh::probe(mode);
        self.pump_interval_micros.store(
            self.refresh.effective.pump_interval().as_micros() as u64,
            Ordering::Relaxed,
        );
        self.message = self.refresh.summary();
        self.save_settings();
        cx.notify();
    }

    /// Removes the most recent stroke.
    fn undo(&mut self, cx: &mut Context<Self>) {
        if self.ink.undo() {
            cx.notify();
        }
    }

    /// Removes every stroke.
    fn clear(&mut self, cx: &mut Context<Self>) {
        if !self.ink.is_blank() {
            self.ink.clear();
            cx.notify();
        }
    }

    /// Shows the previous page.
    fn previous_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index > 0 {
            self.page_index -= 1;
            cx.notify();
        }
    }

    /// Shows the next page.
    fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index + 1 < self.pdf.page_count() {
            self.page_index += 1;
            cx.notify();
        }
    }

    /// Asks the platform for a PDF and opens it.
    fn prompt_for_pdf(&mut self, cx: &mut Context<Self>) {
        let options = PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open a PDF to annotate".into()),
        };
        let receiver = cx.prompt_for_paths(options);

        cx.spawn(async move |this, cx| {
            // The platform relays the choice through a oneshot channel; a cancelled prompt and
            // a platform error both mean "nothing was chosen".
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |app, cx| app.open_pdf(path, cx)).ok();
                }
            }
        })
        .detach();
    }

    /// Opens a PDF and renders its first page.
    fn open_pdf(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let pixel_width = self.pdf_render_pixel_width();

        match PdfDocumentView::open(&path, pixel_width) {
            Ok(view) => {
                self.pdf = view;
                self.page_index = 0;
                self.message = format!("opened {}", self.pdf.file_name());
            }
            Err(error) => {
                self.message = format!("could not open {}: {error}", path.display());
            }
        }

        cx.notify();
    }

    /// Writes the settings out, reporting a failure rather than hiding it.
    fn save_settings(&mut self) {
        if let Err(error) = self.settings.save(&self.settings_path) {
            self.message = format!("settings could not be saved: {error}");
        }
    }

    /// The bitmap width a page is rendered at, in device pixels.
    fn pdf_render_pixel_width(&self) -> u32 {
        (self.settings.page_display_width * self.scale.max(1.0) * PDF_RENDER_SCALE).round() as u32
    }

    /// The rendered page for the current index, when a PDF is open.
    ///
    /// [`PdfDocumentView`] caches rendered pages, so on the common path this is a map lookup
    /// and an `Arc` clone — no rasterisation in the frame.
    fn current_page(&mut self) -> Option<RenderedPage> {
        if !self.pdf.is_loaded() {
            return None;
        }

        let pixel_width = self.pdf_render_pixel_width();
        match self.pdf.render_page(self.page_index, pixel_width) {
            Ok(page) => Some(page),
            Err(error) => {
                let message = error.to_string();
                if self.message != message {
                    self.message = message;
                }
                None
            }
        }
    }

    /// Where the page is drawn: its size in logical pixels and its top-left corner.
    fn page_layout(
        &self,
        window_width: f32,
        page: Option<&RenderedPage>,
    ) -> ([f32; 2], Point<Pixels>) {
        let (width, height) = match page {
            Some(page) => page.display_size(self.settings.page_display_width),
            // With no PDF open the app still needs a sheet to write on. US Letter proportions
            // (8.5 by 11 inches) are the least surprising default.
            None => (
                self.settings.page_display_width,
                self.settings.page_display_width * 11.0 / 8.5,
            ),
        };

        let x = ((window_width - width) / 2.0).max(PAGE_MARGIN);
        ([width, height], point(px(x), px(PAGE_MARGIN)))
    }

    /// The one line that says what the app is doing.
    fn status_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        parts.push(format!(
            "{}  page {}/{}",
            self.pdf.file_name(),
            self.page_index + 1,
            self.pdf.page_count().max(1)
        ));
        parts.push(format!(
            "{}  {:.2} ms/frame",
            self.refresh.summary(),
            self.refresh.effective.frame_interval().as_secs_f64() * 1000.0
        ));

        match self.pen.stats() {
            Some(stats) => {
                parts.push(format!("{stats}  {:.1} readings/message", stats.mean_batch()));

                // What the digitizer can report is worth showing: a pen whose mask never
                // mentions pressure is a pen to judge by its line, not by its force.
                let axes: Vec<&str> = stats.mask.names().collect();
                parts.push(format!(
                    "pen: {}",
                    if axes.is_empty() {
                        String::from("position only")
                    } else {
                        axes.join("+")
                    }
                ));
            }
            None => parts.push(self.pen.status().to_string()),
        }

        let ink = self.ink.stats();
        parts.push(format!(
            "ink {} strokes, {} points ({} resampled, {:.0}%)",
            self.ink.stroke_count(),
            ink.kept_points,
            ink.resampled,
            ink.resample_ratio() * 100.0
        ));

        if !self.message.is_empty() {
            parts.push(self.message.clone());
        }

        parts.join("   ·   ")
    }

    /// The command surface: tools, actions, pages, refresh rate, and status.
    ///
    /// The elements are collected into owned vectors before they are chained onto the row.
    /// That is deliberate: each `cx.listener` takes a mutable borrow of the context, and a
    /// single long builder chain would hold them all at once.
    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (title_bar, border_color, muted_foreground) = (
            theme.title_bar,
            theme.title_bar_border,
            theme.muted_foreground,
        );

        let current_tool = self.ink.mode();

        let mut tools: Vec<AnyElement> = Vec::new();
        tools.push(
            action_button("tool-pen", "Pen", current_tool == Tool::Pen, cx, |app, cx| {
                app.set_tool(Tool::Pen, cx)
            })
            .into_any_element(),
        );
        tools.push(
            action_button(
                "tool-eraser",
                "Eraser",
                current_tool == Tool::Eraser,
                cx,
                |app, cx| app.set_tool(Tool::Eraser, cx),
            )
            .into_any_element(),
        );

        let mut actions: Vec<AnyElement> = Vec::new();
        actions.push(
            action_button("undo", "Undo", false, cx, |app, cx| app.undo(cx)).into_any_element(),
        );
        actions.push(
            action_button("clear", "Clear", false, cx, |app, cx| app.clear(cx)).into_any_element(),
        );
        actions.push(
            action_button("open-pdf", "Open PDF…", false, cx, |app, cx| {
                app.prompt_for_pdf(cx)
            })
            .into_any_element(),
        );

        let mut pages: Vec<AnyElement> = Vec::new();
        pages.push(
            action_button("page-prev", "Prev", false, cx, |app, cx| {
                app.previous_page(cx)
            })
            .into_any_element(),
        );
        pages.push(
            action_button("page-next", "Next", false, cx, |app, cx| app.next_page(cx))
                .into_any_element(),
        );

        let mut rates: Vec<AnyElement> = Vec::new();
        rates.push(
            action_button(
                "rate-auto",
                "Auto",
                self.settings.refresh == RefreshMode::Auto,
                cx,
                |app, cx| app.set_refresh_mode(RefreshMode::Auto, cx),
            )
            .into_any_element(),
        );
        for rate in RefreshRate::ALL {
            rates.push(
                action_button(
                    refresh_button_id(rate),
                    rate.label(),
                    self.settings.refresh == RefreshMode::Fixed(rate),
                    cx,
                    move |app, cx| app.set_refresh_mode(RefreshMode::Fixed(rate), cx),
                )
                .into_any_element(),
            );
        }

        let status = self.status_line();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .w_full()
            .px_3()
            .py_2()
            .bg(title_bar)
            .border_b_1()
            .border_color(border_color)
            .children(tools)
            .child(toolbar_divider(border_color))
            .children(actions)
            .child(toolbar_divider(border_color))
            .children(pages)
            .child(toolbar_divider(border_color))
            .children(rates)
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(muted_foreground)
                    .overflow_hidden()
                    .child(status),
            )
    }
}

impl Render for NoteApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The scale factor is what turns a physical pen pixel into a logical one, so it is
        // captured before anything that depends on it.
        self.scale = window.scale_factor();

        let theme = cx.theme();
        let (background, foreground) = (theme.background, theme.foreground);

        let window_width: f32 = window.bounds().size.width.into();
        let page = self.current_page();
        let (page_size, page_origin) = self.page_layout(window_width, page.as_ref());

        // Everything the paint callback needs is owned by the time the callback is built: it
        // runs later, during the paint phase, and it must not borrow the view.
        let finished = Arc::clone(self.ink.finished());
        let open = self.ink.open().cloned().map(|mut stroke| {
            // An in-progress stroke has no cached ribbon outline yet; computing it here keeps
            // the paint callback free of geometry work.
            stroke.close();
            stroke
        });
        let page_image = page.as_ref().map(|page| Arc::clone(&page.image));
        let ink_color: Hsla = rgb(self.settings.ink_color).into();
        let page_color: Hsla = rgb(self.settings.page_color).into();

        let toolbar = self.toolbar(cx);

        div()
            .relative()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(
                canvas(
                    |_, _, _| (),
                    move |_bounds, _, window: &mut Window, _cx: &mut App| {
                        // The page: a filled sheet, then the rendered bitmap over it.
                        paint_rect(window, page_origin, page_size[0], page_size[1], page_color);

                        if let Some(image) = page_image {
                            let image_bounds = Bounds {
                                origin: page_origin,
                                size: size(px(page_size[0]), px(page_size[1])),
                            };
                            window
                                .paint_image(
                                    image_bounds,
                                    image_bounds,
                                    Corners::default(),
                                    image,
                                    0,
                                    false,
                                )
                                .ok();
                        }

                        for stroke in finished.iter() {
                            paint_stroke(window, stroke, ink_color);
                        }
                        if let Some(stroke) = &open {
                            paint_stroke(window, stroke, ink_color);
                        }
                    },
                )
                .size_full(),
            )
            .child(
                // The command surface floats over the canvas, so pen coordinates need no
                // offset and the ink can run the full height of the window.
                div().absolute().top_0().left_0().w_full().child(toolbar),
            )
    }
}

/// A stable element id for each refresh rate.
///
/// Element ids must be stable across repaints, otherwise GPUI cannot keep focus, hover or
/// scroll state attached to the control — and a reorderable or looping control that used its
/// index would silently shift that state onto a neighbour.
fn refresh_button_id(rate: RefreshRate) -> &'static str {
    match rate {
        RefreshRate::Hz60 => "rate-60",
        RefreshRate::Hz120 => "rate-120",
        RefreshRate::Hz180 => "rate-180",
        RefreshRate::Hz240 => "rate-240",
    }
}

/// A compact toolbar button that mutates the view.
///
/// `active` is the control's selected state: a tool that is in force has to look different
/// from one that is merely available, or the toolbar lies about what the nib will do.
fn action_button(
    id: &'static str,
    label: &'static str,
    active: bool,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, &mut Context<NoteApp>) + 'static,
) -> Button {
    Button::new(id)
        .label(label)
        .compact()
        .toggled(active)
        .on_click(cx.listener(move |app, _, _, cx| handler(app, cx)))
}

/// A thin vertical rule between toolbar groups.
fn toolbar_divider(color: Hsla) -> impl IntoElement {
    div().w_1().h_4().bg(color).mx_1()
}

/// Fills a rectangle given in window coordinates.
///
/// A rectangle drawn as a path — rather than as a component — is what lets the page sheet and
/// the ink share one paint pass, in one coordinate system, with no layout involved.
fn paint_rect(window: &mut Window, origin: Point<Pixels>, width: f32, height: f32, color: Hsla) {
    if width <= 0.0 || height <= 0.0 {
        return;
    }

    let mut builder = PathBuilder::fill();
    builder.move_to(origin);
    builder.line_to(point(origin.x + px(width), origin.y));
    builder.line_to(point(origin.x + px(width), origin.y + px(height)));
    builder.line_to(point(origin.x, origin.y + px(height)));
    builder.close();

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Fills a stroke's ribbon outline.
///
/// A stroke is a filled polygon rather than a stroked polyline, because that is the only shape
/// that can carry a width that changes along the line: the pen's force is baked into the
/// outline's two edges, and a single stroke-width would flatten it.
fn paint_stroke(window: &mut Window, stroke: &Stroke, color: Hsla) {
    // Fewer than three points cannot enclose an area.
    if stroke.outline.len() < 3 {
        return;
    }

    let points: Vec<Point<Pixels>> = stroke
        .outline
        .iter()
        .map(|[x, y]| point(px(*x), px(*y)))
        .collect();

    let mut builder = PathBuilder::fill();
    builder.add_polygon(&points, true);

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}
