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
//! task (see [`NoteApp::start_pen_pump`]) drains it on a timer derived from the display's *measured*
//! frame rate, feeds the ink model, and calls `cx.notify()` — so a frame is scheduled exactly when
//! there is something new to show, and the app is idle otherwise. "Something new" is two things: ink
//! that changed, and a cursor that moved. A pen held in range without touching lays no
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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::Button;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Sizable as _};
use gpui_kit::*;

use crate::canvas::{contrast_color, CanvasSize, CanvasStyle, Ruling, Swatch, INK_COLORS, PAPER_COLORS};
use crate::cursor::{PenCursor, NIB_RADIUS};
use crate::ink::{InkTransform, Notes, Stroke, Tool};
use crate::pen::{capture_config, PenInbox, PenService};
use crate::pdf::{PageRequest, PdfDocumentView, Progress, RenderedPage};
use crate::refresh::{Cadence, DisplayRefresh};
use crate::settings::Settings;
use crate::system_cursor::SystemCursor;
use crate::timing::{measure, Timings};
use crate::view::{Fit, Viewport};

/// The bitmap-width multiplier used when rendering a PDF page.
///
/// Rendering at twice the logical width keeps page text crisp when the window is scaled and on
/// a high-DPI panel, and the cost is paid once per page rather than once per frame.
const PDF_RENDER_SCALE: f32 = 2.0;

/// The ladder of bitmap widths a page may be rendered at.
///
/// ## Why the width is quantised instead of exact
///
/// A page is rasterised whenever the requested width *changes*, and rasterising is the one part of
/// a frame that costs milliseconds — measured on a blank A4 page: 1.0 ms at 720 pixels wide,
/// 3.4 ms at 1440, 13.4 ms at 2880 and 54.5 ms at 5760, with the bitmap itself reaching 178 MB at
/// the last of those. A zoom gesture asks for a new width on *every event*, sixty times a second,
/// so an exact width means a pinch across a PDF re-rasterises the page hundreds of times and never
/// finishes a frame.
///
/// Quantising the request onto a ladder turns that into a handful of rasterisations per gesture.
/// Nothing needs to be exact: GPUI scales the bitmap to the bounds it is painted into, so a page
/// rendered at a width the reader is not quite at is simply slightly softer than it could be.
///
/// ## Why the rungs grow geometrically
///
/// Because the cost is quadratic in the width: each rung is half again as wide as the last, so the
/// memory and the time each grow by a factor of about two and a quarter. Four rungs cover 5% to
/// 1600% of zoom — every zoom this app allows — with the worst rung at 67 MB and about 27 ms, and
/// the top rung is where it stops growing: past it a page is magnified rather than resolved, which
/// is the same trade-off any viewer makes when it stops rendering at the image's own resolution.
const PDF_PIXEL_WIDTHS: [u32; 4] = [1_024, 1_536, 2_304, 3_456];

/// How much of the sheet's size one notch of a wheel adds.
///
/// The same step as the toolbar's own zoom buttons, because both are asking the same question and
/// should answer it at the same rate.
const WHEEL_ZOOM_STEP: f32 = 1.25;

/// How many *lines* the platform reports for one notch of a wheel.
///
/// GPUI scales a wheel notch by the system's "lines to scroll per notch" setting — three on a
/// machine at its defaults — so one notch of a real wheel reaches this app as three lines. Assuming
/// that here is what makes one notch zoom like one press of the toolbar's button. A machine set to
/// scroll a different number of lines per notch zooms proportionally faster or slower per notch,
/// which is a small, self-consistent difference; measuring it would mean reading a system setting
/// this app has no API for. A trackpad reports pixels and never uses this.
const WHEEL_LINES_PER_NOTCH: f32 = 3.0;

/// How many logical pixels one *line* of a scroll is worth, for panning.
///
/// The platform does not say; Windows' own notion of a line is a fraction of a "page" whose size
/// comes from the mouse settings, and 32 logical pixels lands within a few pixels of it on a
/// machine at its defaults. A trackpad reports pixels rather than lines and never uses this.
const WHEEL_LINE_HEIGHT: f32 = 32.0;

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

/// How often the monitor's mode is re-read.
///
/// A mode changes when a person changes it: a display is unplugged, a window is dragged to another
/// panel, a laptop's panel is switched to another rate. Two seconds is fast enough that none of
/// those outlives a stroke, and slow enough that a user-mode display query per frame is not worth
/// thinking about. The rate the app is actually *served* at is not this: that is measured from the
/// frames themselves, on every frame.
const DISPLAY_INTERVAL: Duration = Duration::from_secs(2);

/// Whether the monitor's mode is due to be read again.
///
/// A free function for the same reason [`status_due`] is one: the pacing can be tested without a
/// window or a display.
fn display_due(probed_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(probed_at) >= DISPLAY_INTERVAL
}

/// How long the pen has to be quiet before a page is rendered for it.
///
/// A slice of a render blocks the thread that runs the pump, so while a stroke is being laid the
/// rung already on screen is the right one; the sharp one can wait for the hand to stop. An eighth
/// of a second is short enough that a person pausing to think sees it land, and long enough that the
/// pause between two letters of a word is not mistaken for one.
const PDF_QUIET_INTERVAL: Duration = Duration::from_millis(120);

/// Whether a note was written on the sheet in use.
///
/// Compared with a tolerance rather than exactly: the numbers travel through JSON as `f32`, and a
/// note written on the same sheet must not be reported as a different one because of a rounding
/// step. One logical pixel is far below what a person could notice and far above what the round
/// trip can introduce.
fn sheet_matches(written: (f32, f32), current: (f32, f32)) -> bool {
    (written.0 - current.0).abs() <= 1.0 && (written.1 - current.1).abs() <= 1.0
}

/// The file name of a path, for a message about it.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Whether the pen has been quiet long enough to spend a rasterisation on it.
///
/// A free function for the same reason [`status_due`] and [`display_due`] are: it is a decision,
/// and decisions that can be tested without a window are worth testing without one.
fn pdf_render_due(last_ink_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_ink_at) >= PDF_QUIET_INTERVAL
}

/// The sheet as the last frame drew it: its size in paper units, and where that was placed.
///
/// The pump has no window to ask, and does not need one: a reading has to land on the sheet the
/// user was *looking at*, which is the last frame's — not one computed from a window that may have
/// been resized, zoomed or panned since the reading was produced. Cached here rather than read back
/// out of a layout callback, because the app computes this geometry itself.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sheet {
    /// The size the paper asks for, in logical pixels at 1:1.
    paper: (f32, f32),
    /// Where the sheet's top-left corner was drawn, in window logical pixels.
    origin: (f32, f32),
    /// The window it was drawn into.
    window: (f32, f32),
    /// The zoom it was drawn at.
    zoom: f32,
}

impl Sheet {
    /// The size it was drawn at, after the zoom.
    fn drawn(&self) -> (f32, f32) {
        (self.paper.0 * self.zoom, self.paper.1 * self.zoom)
    }

    /// Where a reading in physical client pixels lands on the sheet.
    fn transform(&self, scale: f32) -> InkTransform {
        InkTransform {
            scale,
            zoom: self.zoom,
            origin: self.origin,
        }
    }

    /// Where a point on the sheet is drawn, in window logical pixels.
    fn place(&self, x: f32, y: f32) -> Point<Pixels> {
        point(
            px(self.origin.0 + x * self.zoom),
            px(self.origin.1 + y * self.zoom),
        )
    }

    /// The part of the sheet that is on screen, in the sheet's own coordinates.
    ///
    /// This is what a frame culls against: ink outside it costs nothing to skip and a great deal
    /// to draw, and the difference is the whole point of zooming in.
    fn visible(&self) -> [f32; 4] {
        let zoom = if self.zoom.is_finite() && self.zoom > 0.0 {
            self.zoom
        } else {
            1.0
        };

        [
            (0.0 - self.origin.0) / zoom,
            (0.0 - self.origin.1) / zoom,
            (self.window.0 - self.origin.0) / zoom,
            (self.window.1 - self.origin.1) / zoom,
        ]
    }
}

/// The application view.
pub struct NoteApp {
    /// The user's tuning, persisted between runs.
    settings: Settings,
    /// Where [`Self::settings`] is written.
    settings_path: PathBuf,
    /// The display probe and the rate the app paces itself at.
    refresh: DisplayRefresh,
    /// When the monitor's mode was last re-read, for [`display_due`].
    display_at: Instant,
    /// What the display's frame time is, measured from the frames this app painted.
    ///
    /// Shared with the paint callback, which is the only place that knows a frame reached the
    /// screen: the callback runs after the element tree is built and cannot borrow the view.
    cadence: Arc<Cadence>,
    /// The page's ink.
    ink: Notes,
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
    /// How large the sheet is drawn, and where it sits in the window.
    view: Viewport,
    /// The sheet as the last frame drew it.
    sheet: Sheet,
    /// What every hot path costs, shared with the paint callback.
    timings: Arc<Timings>,
    /// When the pump last woke, for the gap it reports against its own interval.
    last_pump: Option<Instant>,
    /// The page the view is waiting for, if the cache does not have it yet.
    ///
    /// One slot, always the current page: the renderer has a single job, so there is nothing to
    /// prioritise and nothing to reorder — a request the view has moved past is simply replaced.
    pending_pdf: Option<PageRequest>,
    /// When the pen last laid ink, for [`pdf_render_due`].
    last_ink_at: Instant,
    /// The pump's current interval in microseconds, shared with the pump task.
    ///
    /// A frame measured faster than the mode it read has to change how often the queue is drained,
    /// and the pump task is already running, so the interval lives in an atomic the task re-reads
    /// rather than in a captured local it could not see a change to.
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

        // The monitor the window is on, not the primary one: a mode is a property of the display
        // the ink is on screen on, and the frames that follow measure the rest.
        let refresh = DisplayRefresh::probe(window);

        // The capture must be attached on the thread that owns the window, which is this one.
        let pen = PenService::attach(window, capture_config());

        // Installed after the capture, so this hook runs first in the subclass chain and gets to
        // answer `WM_SETCURSOR` before anything else can put a cursor back.
        let system_cursor = SystemCursor::install(window);

        if message.is_empty() {
            message = pen.status().to_string();
        }

        let view = Viewport::new(settings.zoom);

        let mut app = NoteApp {
            settings,
            settings_path,
            refresh,
            display_at: Instant::now(),
            cadence: Arc::new(Cadence::new()),
            ink: Notes::new(),
            pdf: PdfDocumentView::empty(),
            pen,
            system_cursor,
            page_index: 0,
            scale: window.scale_factor(),
            view,
            sheet: Sheet::default(),
            timings: Arc::new(Timings::default()),
            last_pump: None,
            pending_pdf: None,
            last_ink_at: Instant::now(),
            pump_interval_micros: Arc::new(AtomicU64::new(
                refresh.pump_interval().as_micros() as u64
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
    /// The loop parks on a timer rather than spinning: the interval comes from the display's frame
    /// time — measured from the frames this app painted, so a laptop panel running at 165 Hz is
    /// pumped for at 165 Hz rather than for whichever step it was snapped to — and is half a frame,
    /// clamped. The task ends when the view is dropped — `update` returns `Err` once the entity is
    /// gone.
    fn start_pen_pump(&mut self, cx: &mut Context<Self>) {
        let inbox: Arc<PenInbox> = self.pen.inbox();
        let interval_micros = Arc::clone(&self.pump_interval_micros);

        cx.spawn(async move |this, cx| loop {
            let interval =
                Duration::from_micros(interval_micros.load(Ordering::Relaxed).max(500));
            cx.background_executor().timer(interval).await;

            let batch = inbox.take();

            let alive = this.update(cx, |app, cx| {
                // The gap between this wake and the last, against the interval it asked for: a
                // timer that oversleeps puts a floor under the latency of everything else.
                let woke = Instant::now();
                if let Some(previous) = app.last_pump.replace(woke) {
                    app.timings
                        .pump_gap
                        .record(woke.saturating_duration_since(previous));
                }

                // What the frames have measured since the last wake, before anything is painted
                // with it: the pump interval has to be the display's, not a guess about it.
                //
                // Read before the pen is looked at, because the display is measured whether or not
                // a pen is in hand — a resize, a zoom, or the first frames of a session are all
                // repaints its rate can be read from, and an app that only counted frames while a
                // pen was down would report "measured —" at every other moment.
                let display_moved = app.follow_display_cadence(woke);

                // The page the view is waiting for, paid for here rather than inside a frame. It
                // is skipped while the pen is laying ink, so the queue keeps draining at the pace
                // the display has.
                let page_rendered = app.serve_pdf(woke);

                if batch.samples.is_empty() {
                    // Nothing from the pen: the display's numbers and the page are the only things
                    // that can have moved, and the page only needs a frame once it has landed.
                    if display_moved || page_rendered {
                        app.refresh_status(woke);
                        cx.notify();
                    }
                    return;
                }

                // How long the newest reading waited for this wake. Together with the capture's
                // own delay — the digitizer to the window, which nothing here can change — this
                // is the whole path from the pen to the frame that shows it.
                app.timings.pen_latency.record(batch.waited);

                // The cursor is read on both sides of the batch because a pen in range but not
                // touching lays no ink and still has to be followed around the window: `consume`
                // reports the ink, and the comparison reports the cursor. A frame is scheduled
                // when either of them moved.
                let cursor = app.pen_cursor();
                let laid_ink = app.consume_ink(&batch.samples);
                let cursor_moved = app.pen_cursor() != cursor;
                if laid_ink {
                    // The clock the page render waits on: see `serve_pdf`.
                    app.last_ink_at = woke;
                }

                // The system pointer follows the ghost on every read, so the two can never disagree
                // about whether the pen has a cursor of its own.
                app.follow_pen_with_pointer();

                if !laid_ink && !cursor_moved && !display_moved {
                    // Nothing on screen changed: a reading the resampler and the pointer gate both
                    // dropped, a hover that moved nothing, or a display whose numbers have not
                    // moved since the last wake. Repainting an identical scene on each of those is
                    // what made the top of the window look like it was flickering.
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

    /// Reads a batch of readings into the ink model, timed.
    ///
    /// The transform comes from the last frame's sheet rather than from the window: a reading
    /// belongs on the sheet the user was looking at when the nib moved.
    fn consume_ink(&mut self, samples: &[pen_windows::PenSample]) -> bool {
        let transform = self.sheet.transform(self.scale);
        let _timed = measure(&self.timings.ink);

        self.ink.consume(samples, &transform, &self.settings)
    }

    /// Selects the tool an ordinary nib uses.
    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.ink.set_mode(tool);
        cx.notify();
    }

    /// Folds in what the frames have measured, and says whether it changed on screen.
    ///
    /// This is where the display is *measured* rather than asked about: the cadence is fed by the
    /// paint callback on every frame, and a change in it moves the pump interval — the one number
    /// that decides how long a reading waits before it is on screen — without a restart or a probe.
    fn follow_display_cadence(&mut self, now: Instant) -> bool {
        let measured = self.cadence.frame_interval();
        if !self.refresh.observe(measured.map(|interval| interval.as_micros() as u64)) {
            return false;
        }

        self.store_pump_interval();
        // The line names the measurement, so it is stale the moment the measurement changes. It is
        // rebuilt through the paced path rather than forced: at most the clock is what it was
        // already waiting for.
        self.refresh_status(now);
        true
    }

    /// Re-reads the monitor's mode, and says whether it changed on screen.
    ///
    /// Called from the frame, on a slow clock, because that is the only place the window is in
    /// hand. A *changed* reading is also the one piece of evidence that the frames measured before
    /// it came from another display, so the measurement is thrown away with it: the app is paced by
    /// the new monitor's reported rate until the new monitor's frames say otherwise.
    fn reprobe_display(&mut self, window: &Window) -> bool {
        if !display_due(self.display_at, Instant::now()) {
            return false;
        }

        self.display_at = Instant::now();
        if !self.refresh.reprobe(window) {
            return false;
        }

        self.cadence.reset();
        self.store_pump_interval();
        self.touch_status();
        true
    }

    /// Publishes the interval the pump should be waking at.
    ///
    /// One place, so no path can change the rate in force without the running pump task seeing it.
    fn store_pump_interval(&mut self) {
        self.pump_interval_micros.store(
            self.refresh.pump_interval().as_micros() as u64,
            Ordering::Relaxed,
        );
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

    /// Renders PDF pages in grayscale, or back in colour.
    ///
    /// Nothing has to be invalidated by hand: the colour is a field of the cache key, so the
    /// bitmaps the toggle changed simply are not the bitmaps the cache holds, and the next frame
    /// asks for the ones it now wants.
    fn set_pdf_grayscale(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.grayscale_pages = on;
        self.finish_setting(cx);
    }

    /// One step closer.
    fn zoom_in(&mut self, cx: &mut Context<Self>) {
        if self.view.zoom_in() {
            self.finish_zoom(cx);
        }
    }

    /// One step further away.
    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        if self.view.zoom_out() {
            self.finish_zoom(cx);
        }
    }

    /// Makes the sheet fill the window on one axis.
    ///
    /// The sheet it is fitting is the one the last frame drew, so the answer is about the window
    /// the user is looking at rather than one that has been resized since.
    fn fit_sheet(&mut self, which: Fit, cx: &mut Context<Self>) {
        let sheet = self.sheet;

        if self
            .view
            .fit(which, sheet.paper, sheet.window, PAGE_MARGIN)
        {
            self.finish_zoom(cx);
        }
    }

    /// Keeps the settings and the status line in step with a zoom the view already accepted.
    fn finish_zoom(&mut self, cx: &mut Context<Self>) {
        self.settings.zoom = self.view.zoom();
        self.finish_setting(cx);
    }

    /// Zooms or pans with the wheel — which is also where a trackpad's two-finger gesture arrives.
    ///
    /// With `Ctrl` held it zooms about the pointer, which is what every viewer does and what a
    /// trackpad's pinch is delivered as when the platform sends it as a wheel rather than as a
    /// gesture. Without it, a scroll pans the sheet, which is what a two-finger drag is asking for.
    fn on_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let pointer = (event.position.x.into(), event.position.y.into());

        if event.modifiers.control {
            let (amount, in_pixels) = match event.delta {
                ScrollDelta::Lines(delta) => (delta.y, false),
                ScrollDelta::Pixels(delta) => (delta.y.into(), true),
            };
            let factor = wheel_zoom_factor(amount, in_pixels);
            let sheet = self.sheet;

            if self.view.zoom_around(
                factor,
                pointer,
                sheet.paper,
                sheet.window,
                PAGE_MARGIN,
            ) {
                self.finish_zoom(cx);
            }
            return;
        }

        let (dx, dy) = match event.delta {
            ScrollDelta::Lines(delta) => (delta.x, delta.y),
            ScrollDelta::Pixels(delta) => (delta.x.into(), delta.y.into()),
        };
        let in_pixels = matches!(event.delta, ScrollDelta::Pixels(_));

        if self.view.pan_by(wheel_pan((dx, dy), in_pixels)) {
            cx.notify();
        }
    }

    /// Zooms with a trackpad's pinch, about the point between the fingers.
    ///
    /// The delta is already a fraction — `0.1` is ten percent — so the factor is simply one more
    /// than it. A pinch also cancels any pan that has just been applied by the same gesture being
    /// reported as a wheel as well, by replacing the offset with the anchor rather than adding to it.
    fn on_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        let pointer = (event.position.x.into(), event.position.y.into());
        let factor = pinch_zoom_factor(event.delta);
        let sheet = self.sheet;

        if self
            .view
            .zoom_around(factor, pointer, sheet.paper, sheet.window, PAGE_MARGIN)
        {
            self.finish_zoom(cx);
        }
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
    ///
    /// The ink moves with the page — `go_to` takes the page being left behind with it — which is
    /// what keeps a note on the sheet it was written on rather than on whichever sheet is shown
    /// next.
    fn previous_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index > 0 {
            self.page_index -= 1;
            self.ink.go_to(self.page_index);
            // The status line names the page, so it is stale as soon as the page changes.
            self.touch_status();
            cx.notify();
        }
    }

    /// Shows the next page.
    ///
    /// The page count is the document's when there is one, and the note's otherwise: a note written
    /// on blank sheets has pages of its own, and without this the sheet it was written on beyond the
    /// first could never be reached again.
    fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index + 1 < self.page_total() {
            self.page_index += 1;
            self.ink.go_to(self.page_index);
            self.touch_status();
            cx.notify();
        }
    }

    /// How many pages there are to move between.
    fn page_total(&self) -> usize {
        self.pdf.page_count().max(self.ink.page_count())
    }

    /// Asks the platform for a PDF, or for a saved note, and opens it.
    fn prompt_for_pdf(&mut self, cx: &mut Context<Self>) {
        let options = PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open a PDF, or a note saved from this app".into()),
        };
        let receiver = cx.prompt_for_paths(options);

        cx.spawn(async move |this, cx| {
            // The platform relays the choice through a oneshot channel; a cancelled prompt and
            // a platform error both mean "nothing was chosen".
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |app, cx| app.open_any(path, cx)).ok();
                }
            }
        })
        .detach();
    }

    /// Opens whatever the user chose: a saved note, or a bare PDF to write on.
    ///
    /// Told apart by the file rather than by a menu of two commands: a note *is* a zip, and making
    /// the user remember which of two dialogs to pick would be asking them to keep track of this
    /// app's internals for it.
    fn open_any(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let is_note = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));

        if is_note {
            self.open_bundle(path, cx);
        } else {
            self.open_pdf(path, cx);
        }
    }

    /// Opens a saved note: the document it holds, and the ink over it.
    ///
    /// A note without a document was written on a blank sheet, and reopening it puts the app back
    /// on a blank sheet — leaving whatever document happened to be open behind it would be putting
    /// one sheet's writing on another's.
    fn open_bundle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let bundle = match crate::bundle::read(&path) {
            Ok(bundle) => bundle,
            Err(error) => {
                self.report(error.to_string());
                cx.notify();
                return;
            }
        };

        let pixel_width = self.pdf_render_pixel_width();
        let mut failed = false;

        self.pdf = match bundle.document {
            Some(document) => match PdfDocumentView::open_bytes(
                document.name.clone(),
                document.bytes,
                pixel_width,
            ) {
                Ok(view) => view,
                Err(error) => {
                    self.report(format!(
                        "the document in {} could not be opened: {error}",
                        file_label(&path)
                    ));
                    failed = true;
                    PdfDocumentView::empty()
                }
            },
            None => PdfDocumentView::empty(),
        };

        if failed {
            cx.notify();
            return;
        }

        let strokes: usize = bundle.pages.iter().map(|(_, ink)| ink.stroke_count()).sum();
        self.ink.replace(bundle.pages, bundle.page);
        self.page_index = bundle.page;
        self.settings.bundle_path = Some(path.clone());
        self.save_settings();

        // A note written on a different sheet than the one in use would be drawn at the wrong
        // scale, so that is said out loud rather than silently rescaled.
        let sheet = self.sheet_size();
        self.message = match bundle.sheet {
            Some((width, height)) if !sheet_matches((width, height), sheet) => format!(
                "opened {} — written on a {width:.0}×{height:.0} sheet, this one is {:.0}×{:.0}",
                file_label(&path),
                sheet.0,
                sheet.1
            ),
            _ => format!(
                "opened {} ({strokes} strokes on {} pages)",
                file_label(&path),
                self.ink.written_pages().len()
            ),
        };

        self.touch_status();
        cx.notify();
    }

    /// Opens a PDF and renders its first page.
    fn open_pdf(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let pixel_width = self.pdf_render_pixel_width();

        match PdfDocumentView::open(&path, pixel_width) {
            Ok(view) => {
                self.pdf = view;
                self.page_index = 0;
                // A new document is a new note: the ink that was on screen belonged to the sheet
                // that is no longer there, and keeping it would put one document's writing on
                // another's page.
                self.ink = Notes::new();
                self.message = format!("opened {}", self.pdf.file_name());
            }
            Err(error) => {
                self.message = format!("could not open {}: {error}", path.display());
            }
        }

        self.touch_status();
        cx.notify();
    }

    /// Asks the platform where to write the note, then writes it.
    fn prompt_to_save(&mut self, cx: &mut Context<Self>) {
        if !self.pdf.is_loaded() && self.ink.is_blank() {
            self.report(String::from("there is nothing to save yet"));
            cx.notify();
            return;
        }

        // The suggestion is the document's name with the extension changed: a note about
        // `chapter-3.pdf` is one the user will look for as `chapter-3`.
        let suggested = format!("{}.zip", self.note_stem());
        let directory = self
            .settings
            .bundle_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                this.update(cx, |app, cx| app.save_bundle(path, cx)).ok();
            }
        })
        .detach();
    }

    /// Writes the note back to where it came from, asking for a place the first time.
    fn save(&mut self, cx: &mut Context<Self>) {
        match self.settings.bundle_path.clone() {
            Some(path) => self.save_bundle(path, cx),
            None => self.prompt_to_save(cx),
        }
    }

    /// Writes the note now: the document, and every page that was written on.
    fn save_bundle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // The document is optional: a blank sheet can be written on with nothing open, and what is
        // written then has to be saveable too.
        let document = if self.pdf.is_loaded() {
            match self.pdf.document_bytes() {
                Ok(bytes) => Some(crate::bundle::Document {
                    name: self.pdf.file_name(),
                    bytes,
                }),
                Err(error) => {
                    self.report(error.to_string());
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };

        // Each page's ink is copied out of the model, because the model keeps only one page *hot*
        // and the bundle is written from the map: this is the page-turn code path run in reverse,
        // and it happens once per save.
        let pages: Vec<(usize, crate::ink::InkDocument)> = self
            .ink
            .written_pages()
            .into_iter()
            .filter_map(|page| {
                let strokes = self
                    .ink
                    .page_ink(page)?
                    .finished()
                    .iter()
                    .map(|stroke| (**stroke).clone())
                    .collect();

                Some((page, crate::ink::InkDocument::from_strokes(strokes)))
            })
            .collect();

        let strokes: usize = pages.iter().map(|(_, ink)| ink.stroke_count()).sum();
        let bundle = crate::bundle::Bundle {
            document,
            pages,
            page: self.page_index,
            sheet: Some(self.sheet_size()),
        };

        match crate::bundle::write(&path, &bundle) {
            Ok(()) => {
                self.settings.bundle_path = Some(path.clone());
                self.save_settings();
                self.message = format!("saved {} ({strokes} strokes)", file_label(&path));
            }
            Err(error) => self.message = error.to_string(),
        }

        self.touch_status();
        cx.notify();
    }

    /// The name a note about the open document should suggest.
    fn note_stem(&self) -> String {
        if !self.pdf.is_loaded() {
            return String::from("note");
        }

        let name = self.pdf.file_name();
        match name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_string(),
            _ => name,
        }
    }

    /// The sheet the ink is written on, in logical pixels.
    ///
    /// With a document open the sheet *is* the page, so its own aspect ratio decides the height;
    /// without one it is the chosen canvas size. This is the space every stroke's coordinates are
    /// in, which is why it is what a note records about itself.
    fn sheet_size(&self) -> (f32, f32) {
        let width = self.settings.page_display_width;

        self.pdf
            .page_point_size(self.page_index)
            .map(|(point_width, point_height)| {
                let scale = width / point_width.max(1.0);
                (width, point_height * scale)
            })
            .unwrap_or_else(|| self.settings.canvas_size.display_size(width))
    }
    fn save_settings(&mut self) {
        if let Err(error) = self.settings.save(&self.settings_path) {
            self.message = format!("settings could not be saved: {error}");
        }
    }

    /// The bitmap width a page is rendered at, in device pixels.
    ///
    /// The displayed width is the paper's own width times the zoom, in logical pixels; device
    /// pixels add the window's scale factor, and [`PDF_RENDER_SCALE`] adds the crispness. The
    /// result is then quantised onto [`PDF_PIXEL_WIDTHS`], which is what keeps a zoom gesture from
    /// re-rasterising the page on every event.
    fn pdf_render_pixel_width(&self) -> u32 {
        let logical = self.settings.page_display_width * self.view.zoom();
        let wanted = logical * self.scale.max(1.0) * PDF_RENDER_SCALE;

        quantise_width(wanted)
    }

    /// The rendered page for the current index, when a PDF is open.
    ///
    /// A frame must never wait for Pdfium, so this *asks the cache only*: the exact rung if it is
    /// there, and otherwise whatever rung of the same page is (a soft page for a frame or two beats
    /// a blank one for twenty milliseconds). If the cache has nothing at all for the page — a page
    /// turn, the first frame of a document — the cheapest rung is rendered here, because it costs
    /// about a millisecond and showing nothing is worse.
    ///
    /// What the zoom actually asked for, and could not have, becomes [`Self::pending_pdf`]: the
    /// pump pays for it when the pen is quiet.
    fn current_page(&mut self) -> Option<RenderedPage> {
        if !self.pdf.is_loaded() {
            return None;
        }

        let wanted = PageRequest::new(self.page_index, self.pdf_render_pixel_width())
            .grayscale(self.settings.grayscale_pages);

        if let Some(page) = self.pdf.page_for_frame(wanted) {
            self.plan_pdf(wanted);
            return Some(page);
        }

        // Nothing for this page at any rung: pay for the cheapest one now, so the frame has
        // something to draw, then leave the rung the zoom wanted to the pump.
        let preview = PageRequest::new(self.page_index, PDF_PIXEL_WIDTHS[0])
            .grayscale(self.settings.grayscale_pages);
        let page = match self.render_now(preview) {
            Ok(page) => Some(page),
            Err(error) => {
                self.report(error.to_string());
                None
            }
        };

        self.plan_pdf(wanted);
        page
    }

    /// Records what the view is waiting for, and counts a request that was dropped for it.
    ///
    /// This is one half of the app's cancellation: a request that the view has moved on from
    /// *before* its render started is replaced here, and the replacement is what the counter counts.
    /// The other half belongs to the pump, which abandons a render that was already in flight — see
    /// [`Self::serve_pdf`].
    fn plan_pdf(&mut self, wanted: PageRequest) {
        if self.pending_pdf.is_some_and(|pending| pending != wanted) {
            self.pdf.count_cancelled();
        }

        self.pending_pdf = self.pdf.plan(wanted);
    }

    /// Rasterises one page here and now, timed.
    fn render_now(&mut self, request: PageRequest) -> Result<RenderedPage> {
        let started = Instant::now();
        let before = self.pdf.rasterised();

        let page = self.pdf.render_page(request.page, request.pixels)?;

        self.timings.count_rasterised(self.pdf.rasterised() - before);
        if self.pdf.rasterised() != before {
            self.timings.pdf.record(started.elapsed());
        }

        Ok(page)
    }

    /// Pays for the page the view is waiting for, one slice at a time.
    ///
    /// The render happens *here*, in the pump, and not in the frame: a rasterisation blocks this
    /// thread either way, but the pump is between frames rather than inside one, so the cost lands
    /// on how soon the next reading is consumed instead of on whether a frame is drawn at all. The
    /// slice budget is what keeps even that small — Pdfium stops when the budget is spent, and the
    /// next wake carries on where it left off.
    ///
    /// It waits for the pen to stop because that is the trade the user feels: while a stroke is
    /// being laid the rung already on screen is the right one, and the sharp one can wait for the
    /// hand to stop.
    ///
    /// Returns whether a page became available, which is what the caller turns into a frame.
    fn serve_pdf(&mut self, now: Instant) -> bool {
        let Some(request) = self.pending_pdf else {
            // Nothing is owed. A job that is still in flight belongs to a request the view has
            // moved past — `plan_pdf` replaces the pending request every frame — so it is dropped
            // here rather than finished for nobody.
            if self.pdf.rendering() {
                self.pdf.abandon();
            }
            return false;
        };

        if !pdf_render_due(self.last_ink_at, now) {
            return false;
        }

        // A job for another page or another rung is work for a view that is gone.
        if self.pdf.rendering() && self.pdf.job_request() != Some(request) {
            self.pdf.abandon();
        }

        if !self.pdf.rendering() {
            match self.pdf.begin(request) {
                // Started, and then left alone for this wake: starting a job allocates the bitmap
                // and writes the paper into it, which is the one part of a render a budget cannot
                // slice — so it gets a wake of its own and the rendering starts in the next one.
                Ok(()) => return false,
                Err(error) => {
                    // A page that cannot even be started is not retried every wake: the request is
                    // dropped, and the frame keeps the rung it has.
                    self.pending_pdf = None;
                    self.report(error.to_string());
                    self.touch_status();
                    return true;
                }
            }
        }

        match self.pdf.advance(crate::pdf::SLICE_BUDGET) {
            Progress::Finished => {
                self.pending_pdf = None;
                true
            }
            Progress::Unfinished => false,
            Progress::Failed(message) => {
                self.pending_pdf = None;
                self.report(message);
                self.touch_status();
                true
            }
        }
    }

    /// Puts a message in the status line, rebuilding it at once.
    ///
    /// An error has to reach the user as soon as it happens rather than on the status clock.
    fn report(&mut self, message: String) {
        if self.message != message {
            self.message = message;
            self.touch_status();
        }
    }

    /// Where the sheet is drawn, in the window the frame is painting.
    ///
    /// The *paper* size is what the size setting says; the zoom and the pan belong to the view, and
    /// the two are combined here and nowhere else. The result is stored on the way past, because the
    /// pump needs it and has no window to ask.
    fn page_layout(
        &self,
        window: (f32, f32),
        page: Option<&RenderedPage>,
    ) -> Sheet {
        let paper = match page {
            // A PDF brings its own shape; only how wide it is drawn is the app's choice.
            Some(page) => page.display_size(self.settings.page_display_width),
            // The blank sheet takes both its shape and its scale from the chosen canvas size.
            None => self
                .settings
                .canvas_size
                .display_size(self.settings.page_display_width),
        };

        Sheet {
            paper,
            origin: self.view.origin(paper, window, PAGE_MARGIN),
            window,
            zoom: self.view.zoom(),
        }
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
            self.page_total().max(1)
        ));
        parts.push(format!(
            "{}  {:.2} ms/frame",
            self.refresh.summary(),
            self.refresh.frame_interval().as_secs_f64() * 1000.0
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

        // What the page cache has done, when there is a page to cache. The counters are the whole
        // reason the cache can be argued about rather than guessed at: a hit ratio of 0% and one of
        // 99% look identical from the outside, and the placeholders are what the eye actually saw.
        if self.pdf.is_loaded() {
            parts.push(self.pdf.stats().summary());
        }

        // Where the reading goes and what it costs to put it there. The zoom is the one piece of
        // view state the sheet's own controls cannot show a number for, and the rest is the
        // measurement this app runs on itself.
        parts.push(format!("view {:.0}%", self.view.zoom() * 100.0));
        parts.push(self.timings.summary(self.refresh.pump_interval()));

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

    /// The command surface: tools, actions, pages, and the three visibility switches.
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
            // Saves over the note this session has already written, and asks where to put it the
            // first time — so writing repeatedly is one click, and the file is still the user's to
            // choose.
            action_button("save-note", "Save", false, cx, |app, cx| app.save(cx))
                .into_any_element(),
        );
        actions.push(
            action_button("open-note", "Open…", false, cx, |app, cx| {
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

        let mut zooms: Vec<AnyElement> = Vec::new();
        zooms.push(
            action_button("zoom-out", "−", false, cx, |app, cx| app.zoom_out(cx)).into_any_element(),
        );
        zooms.push(
            action_button("zoom-in", "+", false, cx, |app, cx| app.zoom_in(cx)).into_any_element(),
        );
        for fit in Fit::ALL {
            zooms.push(
                action_button(fit.button_id(), fit.label(), false, cx, move |app, cx| {
                    app.fit_sheet(fit, cx)
                })
                .into_any_element(),
            );
        }

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
            .child(control_label("Zoom", muted_foreground))
            .children(zooms)
            // What the zoom *is*, next to the controls that change it: a percentage is the only
            // thing that says whether Fit Width has already been pressed.
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(accent)
                    .whitespace_nowrap()
                    .child(format!("{:.0}%", self.view.zoom() * 100.0)),
            )
            .child(toolbar_divider(border_color))
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
            .child(toolbar_divider(border_color))
            // A render option for the *page*, next to the colours it overrides: this row wraps, so
            // one more control here cannot push the others off the edge.
            .child(visibility_switch(
                "gray-pages",
                "Gray",
                self.settings.grayscale_pages,
                cx,
                |app: &mut NoteApp, on: bool, cx: &mut Context<NoteApp>| {
                    app.set_pdf_grayscale(on, cx)
                },
            ))
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
        // Timed in a scope of its own, and *not* through `self.timings`: this guard holds a borrow
        // of what it measures, and the rest of this function needs the view mutably. It ends when
        // the element tree is built — the paint callback, which runs later, is measured separately.
        let for_render = Arc::clone(&self.timings);
        let _render_timed = measure(&for_render.render);
        // The counters describe one frame, so this frame's are its own.
        self.timings.start_frame();

        // The monitor's mode, re-read on a slow clock. It costs a user-mode query at most once
        // every couple of seconds, it cannot happen anywhere but here — this is the only place the
        // window is in hand — and a change in it resets the measurement, so the frames that follow
        // are the new display's.
        self.reprobe_display(window);

        // The scale factor is what turns a physical pen pixel into a logical one, so it is
        // captured before anything that depends on it.
        self.scale = window.scale_factor();

        let theme = cx.theme();
        let (background, foreground) = (theme.background, theme.foreground);

        let window_size = (
            window.bounds().size.width.into(),
            window.bounds().size.height.into(),
        );
        let page = self.current_page();
        let sheet = self.page_layout(window_size, page.as_ref());
        // Stored for the pump, which has no window to ask: a reading has to land on the sheet the
        // user was looking at, and that is this one.
        self.sheet = sheet;

        let (page_size, page_origin) = (
            sheet.drawn(),
            point(px(sheet.origin.0), px(sheet.origin.1)),
        );

        let sheet_bounds = Bounds {
            origin: page_origin,
            size: size(px(page_size.0), px(page_size.1)),
        };
        // Built here rather than in the paint callback: the callback runs once per frame, and a
        // full page of grid lines is several hundred quads. `Ruling` hands back the same set
        // until the sheet itself changes — which now includes its zoom, because the ruling is
        // printed on the paper and grows with it.
        //
        // A PDF page is its own paper, so a blank sheet's ruling has nothing to sit on.
        let ruling = page.is_none().then(|| {
            let started = Instant::now();
            let before = self.ruling.rebuilds();
            let quads = self.ruling.quads(
                sheet_bounds,
                self.settings.canvas_style,
                self.settings.page_color,
                sheet.zoom,
            );

            // A cache hit is not a build, and recording one as a build would hide the cost of the
            // builds that do happen behind the many frames that do not.
            if self.ruling.rebuilds() != before {
                self.timings.ruling.record(started.elapsed());
                self.timings.count_ruling();
            }

            quads
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
        let timings = Arc::clone(&self.timings);
        // The same hand-off for the frame clock: the paint callback is where a frame is known to
        // have reached the screen, so it is where the display's real rate is measured.
        let cadence = Arc::clone(&self.cadence);

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
            // A trackpad's pinch, and a wheel with `Ctrl` held, both arrive here: the canvas is the
            // whole window's only interactive element, so it is what the gestures hit.
            .on_scroll_wheel(
                cx.listener(|app, event: &ScrollWheelEvent, _, cx| app.on_wheel(event, cx)),
            )
            .on_pinch(cx.listener(|app, event: &PinchEvent, _, cx| app.on_pinch(event, cx)))
            .child(
                canvas(
                    |_, _, _| (),
                    move |_bounds, _, window: &mut Window, _cx: &mut App| {
                        let _timed = measure(&timings.paint);

                        // The one place in the app that knows a frame reached the screen, and so
                        // the one place the display's rate can be measured rather than asked about.
                        // Two atomic adds; the pump reads the answer on its next wake.
                        cadence.record(Instant::now());

                        // The sheet: a filled rectangle, then its ruling, then the page image.
                        paint_rect(window, page_origin, page_size.0, page_size.1, page_color);

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
                                size: size(px(page_size.0), px(page_size.1)),
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

                        // Ink that is off the sheet is skipped before a polygon is built for it.
                        // This is where zooming in pays for itself: a page and a half of ink can
                        // be off screen, and building a path per stroke only to have the renderer
                        // discard it is the most expensive thing an immediate-mode canvas does.
                        let visible = sheet.visible();
                        let mut scratch: Vec<Point<Pixels>> = Vec::new();
                        let mut painted = 0u64;
                        let mut vertices = 0u64;
                        let mut culled = 0u64;

                        for stroke in finished.iter() {
                            if !stroke.visible_in(visible) {
                                culled += 1;
                                continue;
                            }

                            painted += 1;
                            vertices += stroke.outline.len() as u64;
                            paint_stroke(window, stroke, &sheet, ink_color, &mut scratch);
                        }

                        // The stroke being drawn is never culled: it is by definition under the
                        // pen, and a stroke that vanished for a frame would read as a glitch.
                        if let Some(stroke) = &open {
                            painted += 1;
                            vertices += stroke.outline.len() as u64;
                            paint_stroke(window, stroke, &sheet, ink_color, &mut scratch);
                        }

                        timings.count_painted(painted, vertices, culled);

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
    use super::{
        display_due, file_label, notch_in_pixels, pdf_render_due, pinch_zoom_factor, quantise_width,
        sheet_matches, solid_path, status_due, wheel_pan, wheel_zoom_factor, DISPLAY_INTERVAL,
        PDF_QUIET_INTERVAL, STATUS_INTERVAL, WHEEL_LINE_HEIGHT, WHEEL_LINES_PER_NOTCH,
        WHEEL_ZOOM_STEP,
    };
    use crate::ink::{InkPoint, Stroke};
    use gpui_kit::{point, px, Path, PathBuilder, Pixels, Point};
    use std::time::{Duration, Instant};

    /// How many triangles of a built path cover a point.
    ///
    /// A path's vertices are a triangle list — the renderer draws them as `TRIANGLELIST` — so this
    /// is the coverage that would be accumulated at that pixel. Zero means the paper shows through.
    fn coverage(path: &Path<Pixels>, x: f32, y: f32) -> usize {
        let cross = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
            (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
        };

        path.vertices
            .chunks_exact(3)
            .filter(|triangle| {
                let corner = |index: usize| {
                    let vertex = triangle[index].xy_position;
                    (f32::from(vertex.x), f32::from(vertex.y))
                };
                let (a, b, c) = (corner(0), corner(1), corner(2));

                let (d1, d2, d3) = (cross(a, b, (x, y)), cross(b, c, (x, y)), cross(c, a, (x, y)));

                // Inside whichever way the triangle is wound: the list holds both orientations.
                (d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0) || (d1 <= 0.0 && d2 <= 0.0 && d3 <= 0.0)
            })
            .count()
    }

    /// A stroke whose ribbon crosses itself: a circle drawn all the way round and a fifth of the
    /// way past where it started, which is the shape a cursive loop or a scribble-over makes.
    fn self_crossing_stroke() -> Stroke {
        let mut stroke = Stroke::new(InkPoint::new(60.0, 0.0, 24.0));

        for step in 1..=190 {
            // A full turn and a fifth of the way past it: `TAU` and not `PI`, or the "circle" is
            // half of one and never crosses itself.
            let angle = step as f32 / 180.0 * std::f32::consts::TAU;
            stroke.points.push(InkPoint::new(
                60.0 * angle.cos(),
                60.0 * angle.sin(),
                24.0,
            ));
        }

        stroke.close();
        stroke
    }

    /// A stroke that crosses itself must be painted solid.
    ///
    /// This is the bug the fill rule in [`solid_path`] fixes, pinned from both sides: the default
    /// *even-odd* rule leaves the crossing empty — a white diamond where two lines cross, and a
    /// striped mesh where a scribble doubles back — and *non-zero* fills it. The last assertion is
    /// the other half: neither rule loses the ordinary, uncrossed part of the ribbon, so the fix
    /// costs nothing anywhere else.
    #[test]
    fn a_crossing_stroke_is_filled_and_not_holed() {
        let stroke = self_crossing_stroke();
        let outline: Vec<Point<Pixels>> = stroke
            .outline
            .iter()
            .map(|[x, y]| point(px(*x), px(*y)))
            .collect();

        // What the app used to paint with: the builder's own default options.
        let mut default_builder = PathBuilder::fill();
        default_builder.add_polygon(&outline, true);
        let even_odd = default_builder.build().expect("a path");

        // What it paints with now.
        let mut ink_builder = solid_path();
        ink_builder.add_polygon(&outline, true);
        let non_zero = ink_builder.build().expect("a path");

        // The loop closes over its own start, so the overlap is the strip just inside the circle
        // between where it began and where it came back round to.
        let (crossing_x, crossing_y) = (57.0, 10.0);
        assert_eq!(
            coverage(&even_odd, crossing_x, crossing_y),
            0,
            "even-odd leaves the crossing empty: that is the white diamond"
        );
        assert!(
            coverage(&non_zero, crossing_x, crossing_y) > 0,
            "non-zero fills the crossing"
        );

        // The top of the circle, which no part of the stroke crosses.
        let (plain_x, plain_y) = (0.0, -60.0);
        assert!(
            coverage(&even_odd, plain_x, plain_y) > 0,
            "even-odd fills the ordinary part of the ribbon"
        );
        assert!(
            coverage(&non_zero, plain_x, plain_y) > 0,
            "and so does non-zero"
        );
    }

    /// One notch of a wheel is one zoom step — and a notch arrives as several lines, which is the
    /// part that is easy to get wrong: it made a notch of a real wheel zoom three times too fast
    /// the first time this was written.
    #[test]
    fn one_wheel_notch_is_one_zoom_step() {
        assert_eq!(
            wheel_zoom_factor(WHEEL_LINES_PER_NOTCH, false),
            WHEEL_ZOOM_STEP,
            "a notch is what a notch of a wheel reports"
        );
        assert_eq!(wheel_zoom_factor(0.0, false), 1.0);
        assert!(wheel_zoom_factor(-WHEEL_LINES_PER_NOTCH, false) < 1.0);
        assert!(
            (wheel_zoom_factor(-WHEEL_LINES_PER_NOTCH, false) * WHEEL_ZOOM_STEP - 1.0).abs() < 1e-6,
            "and undoes a notch of the other way"
        );
    }

    /// Two notches are the step applied twice rather than twice the step: a fast roll has to zoom
    /// smoothly instead of in jumps.
    #[test]
    fn two_notches_are_two_steps_and_not_a_doubled_one() {
        let twice = wheel_zoom_factor(WHEEL_LINES_PER_NOTCH * 2.0, false);

        assert!((twice - WHEEL_ZOOM_STEP * WHEEL_ZOOM_STEP).abs() < 1e-6);
        assert!(twice < WHEEL_ZOOM_STEP * 2.0, "a roll is not a multiplication");
    }

    /// A trackpad reports pixels, and the same distance zooms the same either way it is reported:
    /// a notch of lines and a notch's worth of pixels are one gesture.
    #[test]
    fn a_scroll_of_pixels_matches_a_scroll_of_lines() {
        let by_lines = wheel_zoom_factor(WHEEL_LINES_PER_NOTCH, false);
        let by_pixels = wheel_zoom_factor(notch_in_pixels(), true);

        assert!(
            (by_lines - by_pixels).abs() < 1e-6,
            "a notch of lines and a notch of pixels are the same gesture: {by_lines} vs {by_pixels}"
        );
        assert_eq!(by_lines, WHEEL_ZOOM_STEP, "and both are one step");
    }

    /// One enormous delta from a driver must not take the sheet from 100% to 1600%.
    #[test]
    fn a_wild_delta_is_clamped() {
        assert_eq!(wheel_zoom_factor(10_000.0, false), 5.0);
        assert_eq!(wheel_zoom_factor(-10_000.0, false), 0.2);
        assert_eq!(wheel_zoom_factor(f32::NAN, false), 1.0);
        assert_eq!(wheel_zoom_factor(f32::INFINITY, true), 1.0);
    }

    /// Panning is a distance in logical pixels, in the direction of the scroll: up is earlier in
    /// the page, which puts the sheet lower in the window.
    #[test]
    fn a_scroll_pans_in_the_direction_it_points() {
        assert_eq!(wheel_pan((0.0, 1.0), false), (0.0, WHEEL_LINE_HEIGHT));
        assert_eq!(
            wheel_pan((0.0, 3.0), true),
            (0.0, 3.0),
            "pixels are already pixels"
        );
        assert_eq!(wheel_pan((-1.0, 0.0), true), (-1.0, 0.0));
    }

    /// A pinch reports a fraction of the current size, and a nonsense one is refused.
    #[test]
    fn a_pinch_reports_a_fraction_of_the_size() {
        assert!((pinch_zoom_factor(0.1) - 1.1).abs() < 1e-6, "ten percent closer");
        assert!((pinch_zoom_factor(-0.1) - 0.9).abs() < 1e-6, "and ten percent back");
        assert_eq!(pinch_zoom_factor(0.0), 1.0, "three fingers held still");
        assert_eq!(pinch_zoom_factor(100.0), 5.0, "clamped");
        assert_eq!(pinch_zoom_factor(f32::NAN), 1.0, "refused");
    }

    /// A page's bitmap width is quantised onto the ladder, so that a zoom which changes the width
    /// by a pixel does not invalidate a bitmap that cost milliseconds to make.
    #[test]
    fn a_page_is_rendered_on_the_ladder() {
        // The rungs themselves, and everything between them rounded up to the next one.
        assert_eq!(quantise_width(100.0), 1_024);
        assert_eq!(quantise_width(1_024.0), 1_024);
        assert_eq!(quantise_width(1_025.0), 1_536, "just past a rung is the next rung");
        assert_eq!(quantise_width(1_500.0), 1_536);
        assert_eq!(quantise_width(2_000.0), 2_304);
        assert_eq!(quantise_width(3_000.0), 3_456);

        // Past the top rung it stops growing: the page is magnified rather than resolved.
        assert_eq!(quantise_width(9_999.0), 3_456);

        // A window that reports nonsense still asks for something renderable.
        assert_eq!(quantise_width(0.0), 1_024);
        assert_eq!(quantise_width(-10.0), 1_024);
        assert_eq!(quantise_width(f32::NAN), 1_024);
    }

    /// The whole zoom range is covered by a handful of rasterisations, which is the point of the
    /// ladder: a pinch across a PDF must not re-render the page on every event.
    #[test]
    fn a_whole_pinch_needs_only_a_few_rasterisations() {
        let paper_width = 720.0;
        let mut widths = Vec::new();

        // 5% to 1600%, in the steps a gesture would actually produce.
        let mut zoom = 0.05f32;
        while zoom <= 16.0 {
            let width = quantise_width(paper_width * zoom * 2.0);
            if widths.last() != Some(&width) {
                widths.push(width);
            }
            zoom *= 1.25f32.powf(0.1);
        }

        assert_eq!(
            widths,
            vec![1_024, 1_536, 2_304, 3_456],
            "one rasterisation per rung, and no more"
        );
    }

    /// The monitor's mode is re-read on a clock of its own: rarely enough to be free, often enough
    /// that a display change does not outlive a stroke.
    #[test]
    fn the_monitor_is_not_re_read_every_frame() {
        let now = Instant::now();

        assert!(!display_due(now, now));
        assert!(!display_due(now, now + DISPLAY_INTERVAL - Duration::from_millis(1)));
        assert!(display_due(now, now + DISPLAY_INTERVAL));

        // A clock that goes backwards must not force a probe on every frame either.
        assert!(!display_due(now + Duration::from_secs(1), now));
    }

    /// A note written on the same sheet is not reported as a different one.
    #[test]
    fn a_note_on_the_same_sheet_matches() {
        let sheet = (794.0, 1123.0);

        assert!(sheet_matches(sheet, sheet), "the same numbers");
        assert!(
            sheet_matches(sheet, (794.4, 1122.6)),
            "a rounding step through JSON is not a different sheet"
        );
        assert!(!sheet_matches(sheet, (595.0, 842.0)), "A5 is not A4");
        assert!(
            !sheet_matches(sheet, (794.0, 1123.0 + 8.0)),
            "a taller sheet is a different sheet"
        );
    }

    /// A message about a file names the file, not its whole path.
    #[test]
    fn a_file_is_labelled_by_its_name() {
        // `Path` is spelled out because the test module also has GPUI's own `Path` in scope.
        assert_eq!(
            file_label(std::path::Path::new(r"C:\notes\chapter-3.zip")),
            "chapter-3.zip"
        );
        assert_eq!(file_label(std::path::Path::new("note.zip")), "note.zip");
    }

    /// A page is rasterised when the pen stops, not while it is writing.
    ///
    /// The render blocks the thread that runs the pump, so this clock is the difference between a
    /// sharp page and a queue that drains late. It is a decision, so it is pinned down.
    #[test]
    fn a_page_render_waits_for_the_pen_to_stop() {
        let now = Instant::now();

        assert!(!pdf_render_due(now, now), "the pen has just laid ink");
        assert!(
            !pdf_render_due(now, now + PDF_QUIET_INTERVAL - Duration::from_millis(1)),
            "a pause inside a word is not the end of writing"
        );
        assert!(
            pdf_render_due(now, now + PDF_QUIET_INTERVAL),
            "and a hand that has stopped is one to spend a render on"
        );
        assert!(
            !pdf_render_due(now + Duration::from_secs(1), now),
            "a clock that goes backwards must not force a render either"
        );
    }

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

/// The path builder every solid shape the canvas draws is filled with: the ink, and the ghost
/// cursor's body.
///
/// ## Why the fill rule is set, when the default is one line shorter
///
/// `PathBuilder::fill()` uses lyon's default fill options, and lyon's default *fill rule* is
/// **even-odd**: a pixel the outline crosses an even number of times is left empty. That is the
/// right rule for a glyph with a counter in it — the hole in an "o" — and the wrong one for a pen.
///
/// A stroke is a ribbon filled as one outline, and that outline crosses *itself* wherever the pen
/// doubles back: a loop, a sharp turn, a scribble over its own line, or a single stroke that
/// crosses itself. Every one of those places was left empty — a white diamond at a crossing, and a
/// striped mesh wherever the user scribbled back and forth. Measured against a real drawing
/// captured from the screen: 3,145 pixels of paper enclosed inside the ink, in stripes.
///
/// Non-zero fills everything the outline winds around, crossing or not, which is what a pen does.
/// The tessellation is otherwise the same work — the same vertices, the same cost — so this is one
/// option and nothing else changes.
///
/// [`tests::a_crossing_stroke_is_filled_and_not_holed`] pins both halves of that: that the default
/// rule really does leave a hole in a self-crossing stroke, and that this rule does not.
fn solid_path() -> PathBuilder {
    PathBuilder::fill().with_style(PathStyle::Fill(
        FillOptions::default().with_fill_rule(FillRule::NonZero),
    ))
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

        // The same rule the ink uses. This quad is convex, so the default rule would do — but a
        // filled shape that can show a hole is one degenerate tilt away from being a bug.
        let mut builder = solid_path();
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
///
/// The outline is in the sheet's coordinates, so it is scaled and placed on the way out, and the
/// `points` scratch buffer is handed in rather than allocated per stroke: a frame with three
/// hundred strokes on it would otherwise make — and free — three hundred vectors.
fn paint_stroke(
    window: &mut Window,
    stroke: &Stroke,
    sheet: &Sheet,
    color: Hsla,
    points: &mut Vec<Point<Pixels>>,
) {
    // Fewer than three points cannot enclose an area.
    if stroke.outline.len() < 3 {
        return;
    }

    points.clear();
    points.extend(
        stroke
            .outline
            .iter()
            .map(|[x, y]| sheet.place(*x, *y)),
    );

    let mut builder = solid_path();
    builder.add_polygon(points, true);

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// How much a scroll zooms, as a factor to multiply the current zoom by.
///
/// A wheel reports notches in *lines* and a trackpad reports pixels, and both become notches here.
/// One notch is [`WHEEL_ZOOM_STEP`] of the size and two are its square, so a fast roll zooms
/// smoothly rather than in jumps. The result is clamped, because one enormous delta from a driver
/// should not take the sheet from 100% to 1600% in a single event.
fn wheel_zoom_factor(delta: f32, in_pixels: bool) -> f32 {
    if !delta.is_finite() {
        return 1.0;
    }

    let notches = if in_pixels {
        delta / notch_in_pixels()
    } else {
        delta / WHEEL_LINES_PER_NOTCH
    };

    WHEEL_ZOOM_STEP.powf(notches).clamp(0.2, 5.0)
}

/// How far a trackpad reports a scroll for one notch's worth of zoom.
///
/// Derived from the wheel's own two measurements rather than picked: a notch is
/// [`WHEEL_LINES_PER_NOTCH`] lines and a line is [`WHEEL_LINE_HEIGHT`] pixels, so a trackpad that
/// has moved that many pixels has asked for the same thing a wheel's notch does. Without this the
/// pixel path would zoom three times faster per gesture than the line path.
fn notch_in_pixels() -> f32 {
    WHEEL_LINE_HEIGHT * WHEEL_LINES_PER_NOTCH
}

/// How much a scroll pans, in logical pixels.
///
/// Up is earlier in the page, so a wheel turned away from the user moves the sheet *down*: the
/// same rule a document viewer follows, applied to both axes.
fn wheel_pan(delta: (f32, f32), in_pixels: bool) -> (f32, f32) {
    let scale = if in_pixels { 1.0 } else { WHEEL_LINE_HEIGHT };

    (delta.0 * scale, delta.1 * scale)
}

/// How much a pinch zooms, as a factor to multiply the current zoom by.
///
/// A pinch's delta is already a fraction of the current size, so the factor is one more than it —
/// and it is clamped to a factor rather than to a fraction, so a nonsense delta cannot invert the
/// sheet.
fn pinch_zoom_factor(delta: f32) -> f32 {
    if !delta.is_finite() {
        return 1.0;
    }

    (1.0 + delta).clamp(0.2, 5.0)
}

/// The smallest width on [`PDF_PIXEL_WIDTHS`] that is at least `wanted`, or the largest one.
///
/// The *smallest* rung that covers the request, rather than the nearest: a page rendered a little
/// too large is sharp, and one rendered a little too small is soft, and only one of those is
/// visible.
fn quantise_width(wanted: f32) -> u32 {
    if !wanted.is_finite() || wanted <= 0.0 {
        return PDF_PIXEL_WIDTHS[0];
    }

    let wanted = wanted.round() as u32;
    PDF_PIXEL_WIDTHS
        .iter()
        .copied()
        .find(|width| *width >= wanted)
        .unwrap_or(PDF_PIXEL_WIDTHS[PDF_PIXEL_WIDTHS.len() - 1])
}
