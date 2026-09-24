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
//! when there is something new to show, and the app is idle otherwise. "Something new" is two
//! things: ink that changed, and a cursor that moved. A pen held in range without touching lays no
//! ink at all, and it is the ghost cursor (see [`crate::cursor`]) that has to follow it.
//!
//! ## The top bar, and why it can be turned off
//!
//! The status line holds counters that move on every reading, and text that changes is text GPUI
//! has to re-shape and re-lay-out. Rebuilding it on every frame is what made the top of the
//! window flicker while writing, so it is rebuilt on a slow clock instead (see [`status_due`]) and
//! can be switched off entirely — as can the whole bar, which floats over the canvas.
//!
//! ## The sheet
//!
//! What the user writes on is described by [`crate::canvas`]: its size, its colour, and what is
//! printed on it. A PDF page overrides all three, because a PDF page is its own paper.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::Button;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Sizable as _};
use gpui_kit::*;

use crate::canvas::{contrast_color, CanvasSize, CanvasStyle, Ruling, Swatch, INK_COLORS, PAPER_COLORS};
use crate::cursor::{PenCursor, NIB_RADIUS};
use crate::ink::{InkDocument, Stroke, Tool};
use crate::pen::{capture_config, PenInbox, PenService};
use crate::pdf::{PdfDocumentView, RenderedPage};
use crate::refresh::{DisplayRefresh, RefreshMode, RefreshRate};
use crate::settings::Settings;
use crate::system_cursor::SystemCursor;

/// The bitmap-width multiplier used when rendering a PDF page.
///
/// Rendering at twice the logical width keeps page text crisp when the window is scaled and on
/// a high-DPI panel, and the cost is paid once per page rather than once per frame.
const PDF_RENDER_SCALE: f32 = 2.0;

/// The margin, in logical pixels, between a sheet and the window's edges.
///
/// The top bar — one or two rows of controls — floats over the sheet, so it covers this strip and
/// a little more. That is the price of the overlay: ink needs no offset arithmetic to be painted,
/// and in exchange the top of the page sits under the bar until the bar is switched off.
const PAGE_MARGIN: f32 = 24.0;

/// How tall the top bar is, in logical pixels, as an estimate.
///
/// Used for exactly one decision: whether the pen is over the bar, where the ghost cursor is drawn
/// behind the bar's opaque background and the system pointer therefore has to stay. The estimate
/// errs upward deliberately — being wrong upward leaves a strip of sheet still showing the pointer
/// (a small surprise), while being wrong downward would leave part of the toolbar with no cursor at
/// all, and the pen is how those controls get clicked.
const BAR_HEIGHT: f32 = 96.0;

/// How often the status line is rebuilt at most.
///
/// Four times a second: often enough that the numbers look live to a person reading them, rare
/// enough that the text is identical across most frames and therefore costs nothing to draw. The
/// line is the only text in the interface that changes on its own.
const STATUS_INTERVAL: Duration = Duration::from_millis(250);

/// Whether the status line is due to be rebuilt.
///
/// A free function rather than a method so the pacing can be tested without a window: this clock
/// is the difference between a calm top bar and one that flickers.
fn status_due(built_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(built_at) >= STATUS_INTERVAL
}

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
    /// The hook that hides the system pointer while the pen has a cursor of its own.
    ///
    /// `None` when the platform would not give one, which costs nothing but a second cursor.
    system_cursor: Option<SystemCursor>,
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
    /// The rule geometry for the current sheet.
    ///
    /// Held here rather than rebuilt in the paint callback: the callback runs once per frame and
    /// must not do geometry work.
    ruling: Ruling,
    /// The status line as last composed, and when.
    ///
    /// Cached so that most frames draw the *same* text. A line rebuilt every frame is a line the
    /// text system has to shape and lay out every frame, which is what flickers.
    status: String,
    /// When [`Self::status`] was last composed, for [`status_due`].
    status_at: Instant,
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

        // Installed after the capture, so this hook runs first in the subclass chain and gets to
        // answer `WM_SETCURSOR` before anything else can put a cursor back.
        let system_cursor = SystemCursor::install(window);

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
            system_cursor,
            page_index: 0,
            scale: window.scale_factor(),
            pump_interval_micros: Arc::new(AtomicU64::new(
                refresh.effective.pump_interval().as_micros() as u64
            )),
            ruling: Ruling::default(),
            status: String::new(),
            status_at: Instant::now(),
            message,
        };

        // The first status line is composed here, so the first frame already has it and no frame
        // has to render text that is about to be replaced.
        app.touch_status();

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
                // The cursor is read on both sides of the batch because a pen in range but not
                // touching lays no ink and still has to be followed around the window: `consume`
                // reports the ink, and the comparison reports the cursor. A frame is scheduled
                // when either of them moved.
                let cursor = app.pen_cursor();
                let laid_ink = app.ink.consume(&samples, app.scale, &app.settings);
                let cursor_moved = app.pen_cursor() != cursor;

                // The system pointer follows the ghost on every read, so the two can never disagree
                // about whether the pen has a cursor of its own.
                app.follow_pen_with_pointer();

                if !laid_ink && !cursor_moved {
                    // Nothing on screen changed: a reading the resampler and the pointer gate both
                    // dropped, or a hover that moved nothing. Repainting an identical scene on each
                    // of those is what made the top of the window look like it was flickering.
                    return;
                }

                // The status counters have moved, but the line is rebuilt only when it is due:
                // rebuilding it here would re-shape the text on every frame, which is the other
                // half of the same problem.
                app.refresh_status(Instant::now());
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
        self.finish_setting(cx);
    }

    /// Changes the sheet's size, and its scale with it.
    ///
    /// A paper size is a physical size, so choosing one sets the drawing scale too: a person
    /// picking A5 expects half a sheet of A4, not the same sheet under another name.
    fn set_canvas_size(&mut self, size: CanvasSize, cx: &mut Context<Self>) {
        self.settings.canvas_size = size;
        self.settings.page_display_width = size.display_width();
        self.finish_setting(cx);
    }

    /// Changes what is printed on the sheet.
    fn set_canvas_style(&mut self, style: CanvasStyle, cx: &mut Context<Self>) {
        self.settings.canvas_style = style;
        self.finish_setting(cx);
    }

    /// Changes the sheet's colour.
    fn set_paper_color(&mut self, color: u32, cx: &mut Context<Self>) {
        self.settings.page_color = color;
        self.finish_setting(cx);
    }

    /// Changes the ink's colour.
    fn set_ink_color(&mut self, color: u32, cx: &mut Context<Self>) {
        self.settings.ink_color = color;
        self.finish_setting(cx);
    }

    /// Shows or hides the whole top bar.
    fn set_toolbar_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.settings.show_toolbar = shown;
        self.finish_setting(cx);
    }

    /// Shows or hides the live status line inside the bar.
    fn set_status_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.settings.show_status = shown;
        self.finish_setting(cx);
    }

    /// Shows or hides the pen's ghost cursor.
    fn set_tilt_cursor_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.settings.show_tilt_cursor = shown;
        self.finish_setting(cx);
    }

    /// The cursor the pen's most recent reading describes, whatever the ghost is set to.
    ///
    /// Reading it from the ink model rather than from a batch means the cursor survives the batches
    /// that lay nothing — which is most of them, while the pen is merely held over the window —
    /// and disappears only when the pen does.
    fn last_cursor(&self) -> Option<PenCursor> {
        self.ink
            .last_sample()
            .and_then(|sample| PenCursor::from_sample(sample, self.scale))
    }

    /// The cursor to draw, if the ghost cursor is switched on.
    fn pen_cursor(&self) -> Option<PenCursor> {
        if !self.settings.show_tilt_cursor {
            return None;
        }

        self.last_cursor()
    }

    /// Whether the pen has a cursor of its own where it is, so the system pointer should be out of
    /// the way.
    ///
    /// True only *below the bar*: over the bar the ghost is drawn behind an opaque background,
    /// where it cannot be seen.
    fn pen_has_its_own_cursor(&self) -> bool {
        self.pen_cursor()
            .is_some_and(|cursor| cursor.position()[1] > BAR_HEIGHT)
    }

    /// Keeps the system pointer in step with the ghost cursor, hiding it exactly while the ghost
    /// replaces it.
    ///
    /// Driven from the ghost and not from the pen's range, because the two states have to be
    /// impossible to separate: a hidden pointer with nothing drawn in its place is a window with no
    /// cursor at all.
    fn follow_pen_with_pointer(&mut self) {
        let has_its_own = self.pen_has_its_own_cursor();

        if let Some(system_cursor) = &mut self.system_cursor {
            system_cursor.follow(has_its_own);
        }
    }

    /// Persists and repaints after a setting changed.
    ///
    /// The ruling is keyed on the sheet's rectangle, its style and its paper colour, so it
    /// discards itself on the next frame without being told. Only the status line has to be
    /// rebuilt, and it is rebuilt here rather than on the clock because the user is looking for
    /// the change they just made.
    fn finish_setting(&mut self, cx: &mut Context<Self>) {
        self.save_settings();
        self.touch_status();
        // A setting can change whether the pen has a cursor of its own — the Tilt switch does — and
        // the pump may not wake for a while if the pen is away. Handing the pointer back here means
        // the switch takes effect the moment it is flipped.
        self.follow_pen_with_pointer();
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
            // The status line names the page, so it is stale as soon as the page changes.
            self.touch_status();
            cx.notify();
        }
    }

    /// Shows the next page.
    fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index + 1 < self.pdf.page_count() {
            self.page_index += 1;
            self.touch_status();
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

        self.touch_status();
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
                    // The message is part of the status line, which is now stale. The line is
                    // rebuilt in the same frame rather than on the clock, because an error has to
                    // reach the user as soon as it happens.
                    self.touch_status();
                }
                None
            }
        }
    }

    /// Where the sheet is drawn: its size in logical pixels and its top-left corner.
    fn page_layout(
        &self,
        window_width: f32,
        page: Option<&RenderedPage>,
    ) -> ([f32; 2], Point<Pixels>) {
        let (width, height) = match page {
            // A PDF brings its own shape; only how wide it is drawn is the app's choice.
            Some(page) => page.display_size(self.settings.page_display_width),
            // The blank sheet takes both its shape and its scale from the chosen canvas size.
            None => self
                .settings
                .canvas_size
                .display_size(self.settings.page_display_width),
        };

        let x = ((window_width - width) / 2.0).max(PAGE_MARGIN);
        ([width, height], point(px(x), px(PAGE_MARGIN)))
    }

    /// The one line that says what the app is doing.
    ///
    /// Composed from the current state rather than cached by the caller: the caching is
    /// [`Self::refresh_status`]'s job, and keeping the two apart means a caller can force the
    /// line up to date without knowing how it is paced.
    fn compose_status(&self) -> String {
        if !self.settings.show_status {
            // Not built at all, not built and hidden: composing a string the user asked not to
            // see would be work done on every wake of the pump for nothing.
            return String::new();
        }

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

        // Worth saying only when it is missing. A window whose pointer could not be hooked still
        // works — it simply shows two cursors — and this line is the only way to know which of the
        // two is happening.
        if self.system_cursor.is_none() {
            parts.push(String::from("no pointer hook"));
        }

        // The live lean, when the pen is here to have one. This is the number the ghost cursor is
        // drawing, so it is where a person checks that the cursor is telling the truth about which
        // way the pen leans — worth reporting whether or not the ghost itself is drawn.
        if let Some(tilt) = self.last_cursor().and_then(PenCursor::tilt) {
            parts.push(format!("lean {tilt}"));
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

    /// Rebuilds the status line, for a change the user made and expects to see at once.
    fn touch_status(&mut self) {
        self.status = self.compose_status();
        self.status_at = Instant::now();
    }

    /// Rebuilds the status line if enough time has passed.
    ///
    /// Called on every wake of the pump, which is up to 240 times a second while writing. The
    /// clock is what keeps the text identical across those frames.
    fn refresh_status(&mut self, now: Instant) {
        if status_due(self.status_at, now) {
            self.touch_status();
        }
    }

    /// The command surface: tools, actions, pages, refresh rate, and the two visibility switches.
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

        let switches = vec![
            visibility_switch(
                "show-bar",
                "Bar",
                self.settings.show_toolbar,
                cx,
                |app: &mut NoteApp, on: bool, cx: &mut Context<NoteApp>| {
                    app.set_toolbar_shown(on, cx)
                },
            ),
            visibility_switch(
                "show-status",
                "Status",
                self.settings.show_status,
                cx,
                |app: &mut NoteApp, on: bool, cx: &mut Context<NoteApp>| {
                    app.set_status_shown(on, cx)
                },
            ),
            visibility_switch(
                "show-tilt",
                "Tilt",
                self.settings.show_tilt_cursor,
                cx,
                |app: &mut NoteApp, on: bool, cx: &mut Context<NoteApp>| {
                    app.set_tilt_cursor_shown(on, cx)
                },
            ),
        ];

        // The live status line is the only text here that changes on its own, so it is built only
        // when it is wanted. `whitespace_nowrap` matters: text that may wrap is text the layout
        // has to re-measure whenever it changes, and this line changes more than any other.
        let status = self.settings.show_status.then(|| {
            div()
                .flex_shrink_1()
                .text_size(px(12.0))
                .text_color(muted_foreground)
                .whitespace_nowrap()
                .overflow_hidden()
                .child(self.status.clone())
                .into_any_element()
        });

        div()
            .flex()
            .flex_col()
            .w_full()
            .bg(title_bar)
            .border_b_1()
            .border_color(border_color)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .w_full()
                    .px_3()
                    .py_2()
                    .children(tools)
                    .child(toolbar_divider(border_color))
                    .children(actions)
                    .child(toolbar_divider(border_color))
                    .children(pages)
                    .child(toolbar_divider(border_color))
                    .children(rates)
                    .child(div().flex_1())
                    .children(status)
                    .child(toolbar_divider(border_color))
                    .children(switches),
            )
            .child(self.canvas_row(cx))
    }

    /// The canvas controls: the sheet's size, what is printed on it, and the two colours.
    ///
    /// A row of its own rather than more of the first: the two rows answer different questions —
    /// "what does the nib do" and "what am I writing on" — and this one wraps, so a narrow window
    /// moves its controls onto another line instead of hiding them.
    fn canvas_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border_color, muted_foreground, accent) =
            (theme.title_bar_border, theme.muted_foreground, theme.primary);

        let mut sizes: Vec<AnyElement> = Vec::new();
        for size in CanvasSize::ALL {
            sizes.push(
                action_button(
                    size.button_id(),
                    size.label(),
                    self.settings.canvas_size == size,
                    cx,
                    move |app, cx| app.set_canvas_size(size, cx),
                )
                .into_any_element(),
            );
        }

        let mut styles: Vec<AnyElement> = Vec::new();
        for style in CanvasStyle::ALL {
            styles.push(
                action_button(
                    style.button_id(),
                    style.label(),
                    self.settings.canvas_style == style,
                    cx,
                    move |app, cx| app.set_canvas_style(style, cx),
                )
                .into_any_element(),
            );
        }

        let mut paper: Vec<AnyElement> = Vec::new();
        for swatch in PAPER_COLORS.iter() {
            paper.push(
                swatch_button(
                    swatch,
                    self.settings.page_color == swatch.color,
                    accent,
                    cx,
                    |app, color, cx| app.set_paper_color(color, cx),
                )
                .into_any_element(),
            );
        }

        let mut ink: Vec<AnyElement> = Vec::new();
        for swatch in INK_COLORS.iter() {
            ink.push(
                swatch_button(
                    swatch,
                    self.settings.ink_color == swatch.color,
                    accent,
                    cx,
                    |app, color, cx| app.set_ink_color(color, cx),
                )
                .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1()
            .w_full()
            .px_3()
            .pb_2()
            .child(control_label("Sheet", muted_foreground))
            .children(sizes)
            .child(toolbar_divider(border_color))
            .child(control_label("Style", muted_foreground))
            .children(styles)
            .child(toolbar_divider(border_color))
            .child(control_label("Paper", muted_foreground))
            .children(paper)
            .child(toolbar_divider(border_color))
            .child(control_label("Ink", muted_foreground))
            .children(ink)
    }

    /// The way back when the bar is hidden.
    ///
    /// A bar that can be hidden must never be hidden *permanently*: the switch that brings it
    /// back lives inside the bar, so hiding the bar would take the way back with it. This handle
    /// is drawn over the canvas whenever the bar is away.
    fn bar_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().absolute().top_0().left_0().p_1().child(action_button(
            "show-bar-again",
            "Bar",
            false,
            cx,
            |app, cx| app.set_toolbar_shown(true, cx),
        ))
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

        let sheet = Bounds {
            origin: page_origin,
            size: size(px(page_size[0]), px(page_size[1])),
        };
        // Built here rather than in the paint callback: the callback runs once per frame, and a
        // full page of grid lines is several hundred quads. `Ruling` hands back the same set
        // until the sheet itself changes.
        //
        // A PDF page is its own paper, so a blank sheet's ruling has nothing to sit on.
        let ruling = page.is_none().then(|| {
            self.ruling
                .quads(sheet, self.settings.canvas_style, self.settings.page_color)
        });

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

        // The ghost cursor: a mark at the nib with the pen's body leaning away from it. Drawn last,
        // because a cursor belongs on top of everything, and only while the pen is in range — the
        // ink model drops the cursor the moment it hears the pen leave.
        let cursor = self.pen_cursor();
        // Its own colour rather than the ink's: it is not ink, and it has to be visible on a sheet
        // of any colour, including one where the ink would disappear.
        let cursor_color: Hsla = rgb(contrast_color(self.settings.page_color)).into();

        // The bar floats over the canvas, so pen coordinates need no offset and the ink can run
        // the full height of the window. When it is hidden, the handle that brings it back takes
        // its place — a bar that could be hidden with no way back would be a trap.
        //
        // The bar is not built at all when it is hidden, rather than built and not shown: the
        // status line is the one element whose whole cost is in being built.
        let bar: AnyElement = if self.settings.show_toolbar {
            div()
                .absolute()
                .top_0()
                .left_0()
                .w_full()
                .child(self.toolbar(cx))
                .into_any_element()
        } else {
            self.bar_handle(cx).into_any_element()
        };

        div()
            .relative()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(
                canvas(
                    |_, _, _| (),
                    move |_bounds, _, window: &mut Window, _cx: &mut App| {
                        // The sheet: a filled rectangle, then its ruling, then the page image.
                        paint_rect(window, page_origin, page_size[0], page_size[1], page_color);

                        // Quad by quad, cloned: a `PaintQuad` is a handful of plain old data, so
                        // this is a memcpy per rule and needs no geometry work at all.
                        if let Some(quads) = &ruling {
                            for quad in quads.iter() {
                                window.paint_quad(quad.clone());
                            }
                        }

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

                        if let Some(cursor) = cursor {
                            paint_cursor(window, cursor, cursor_color);
                        }
                    },
                )
                .size_full(),
            )
            .child(bar)
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

/// A small muted caption in front of a group of controls.
///
/// The caption is what makes the canvas row readable: six of its controls are paper sizes and
/// twelve are colours, and without the words it would be a guess which is which.
fn control_label(text: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(color)
        .mr_1()
        .child(text)
}

/// A switch for one of the bar's two visibility options.
///
/// A switch rather than a button, because this is a state and not a command: the control has to
/// say what *is*, and a switch that is off reads as clearly as one that is on, which a button
/// that merely is not highlighted does not.
fn visibility_switch(
    id: &'static str,
    label: &'static str,
    on: bool,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, bool, &mut Context<NoteApp>) + 'static,
) -> Switch {
    Switch::new(id)
        .checked(on)
        .label(label)
        .small()
        .on_change(cx.listener(move |app, checked: &bool, _, cx| handler(app, *checked, cx)))
}

/// One colour swatch: a filled square, framed when it is the colour in use.
///
/// The frame is this element's own background rather than a border, so the selection reads
/// against a swatch of *any* colour — including one that is the same colour as a border would
/// be — and the control stays a square of colour with a little padding around it.
fn swatch_button(
    swatch: &Swatch,
    in_use: bool,
    frame: Hsla,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, u32, &mut Context<NoteApp>) + 'static,
) -> impl IntoElement {
    let color = swatch.color;

    div()
        .id(swatch.id)
        // A colour square says nothing to a screen reader, and nothing to anyone who cannot
        // tell "ivory" from "white" by eye. The name is what both need to hear or see.
        .aria_label(swatch.name)
        .p(px(1.5))
        .rounded_sm()
        .bg(if in_use { frame } else { transparent_black() })
        .cursor_pointer()
        .on_click(cx.listener(move |app, _, _, cx| handler(app, color, cx)))
        .child(div().w(px(16.0)).h(px(16.0)).rounded_xs().bg(rgb(color)))
}

#[cfg(test)]
mod tests {
    // Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into
    // scope and shadow the attribute this module needs.
    use super::{status_due, STATUS_INTERVAL};
    use std::time::{Duration, Instant};

    /// The status line is rebuilt on a clock, not on every frame.
    ///
    /// This is the whole anti-flicker fix, so it is pinned down: the pump wakes up to 240 times a
    /// second while writing, and a line rebuilt on each of those wakes is a line the text system
    /// has to shape and the bar has to lay out on each of them.
    #[test]
    fn the_status_line_is_not_rebuilt_every_frame() {
        let built = Instant::now();

        assert!(!status_due(built, built), "a line just built is not rebuilt");
        assert!(
            !status_due(built, built + Duration::from_millis(4)),
            "not even on a 240 Hz panel"
        );
        assert!(
            !status_due(built, built + STATUS_INTERVAL - Duration::from_millis(1)),
            "not just before the interval is up"
        );
        assert!(
            status_due(built, built + STATUS_INTERVAL),
            "due once the interval has passed"
        );
        assert!(
            status_due(built, built + Duration::from_secs(5)),
            "and still due long after"
        );
    }

    /// A clock that appears to go backwards must not make the line rebuilt on every wake.
    ///
    /// `Instant` is monotonic, so this cannot happen in practice — but the guard is one call and
    /// the failure it prevents is the flicker this pacing exists to remove.
    #[test]
    fn a_clock_that_goes_backwards_does_not_force_rebuilds() {
        let now = Instant::now();

        assert!(!status_due(now + Duration::from_secs(1), now));
    }
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

/// Draws the pen's ghost cursor: the nib, and the pen's body leaning away from it.
///
/// The body is a filled polygon rather than a quad, because a quad cannot be rotated and pointing
/// somewhere is the entire point of the shape. It is built fresh on every frame the pen moves —
/// there is no way to draw something whose position and angle are both new each frame — which is
/// affordable because it is four points, not a stroke.
fn paint_cursor(window: &mut Window, cursor: PenCursor, color: Hsla) {
    let [x, y] = cursor.position();

    // The body is absent for a pen with no tilt sensor, and for one held straight up.
    if let Some([a, b, c, d]) = cursor.body_outline() {
        let corners = [
            point(px(a[0]), px(a[1])),
            point(px(b[0]), px(b[1])),
            point(px(c[0]), px(c[1])),
            point(px(d[0]), px(d[1])),
        ];

        let mut builder = PathBuilder::fill();
        builder.add_polygon(&corners, true);

        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    }

    // The nib: a small circle, always, so there is a fixed point that says exactly where the ink
    // will land. A square rounded by half its own side is a circle.
    let radius = NIB_RADIUS;
    let corner = px(radius);
    window.paint_quad(
        fill(
            Bounds {
                origin: point(px(x - radius), px(y - radius)),
                size: size(px(radius * 2.0), px(radius * 2.0)),
            },
            color,
        )
        .corner_radii(Corners {
            top_left: corner,
            top_right: corner,
            bottom_right: corner,
            bottom_left: corner,
        }),
    );
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
