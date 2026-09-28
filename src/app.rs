//! The application view: a command surface over an ink canvas.
//!
//! One `NoteApp` is every screen the app has — the note, the home list, and the two lists of places —
//! and the two pumps that keep them fed. The canvas behind them is drawn by a renderer of the app's
//! own (see [`crate::ink_layer`]), described here and painted elsewhere; what holds the screens
//! together is the element tree that [`Render for NoteApp`] builds.
//!
//! Everything this module decides at length — why the bar floats over the canvas, what the frame loop
//! may and may not wait for, why the status line is not on a clock, what a wheel notch and a
//! rasterised page are worth, and every rule the pen obeys — is written out in `doc/VIEW.md`, which is
//! the map of this module; `doc/ARCHITECTURE.md` is the map of the whole program. What is left here is
//! the line-long "why" a reader wants where they are reading.
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, IndexPath, Sizable as _};
use gpui_kit::*;

use crate::bookmarks::{Bookmarks, MarkedPages, Marks};
use crate::canvas::{
    contrast_color, relative_luminance, CanvasSize, CanvasStyle, Ruling, Swatch, INK_COLORS,
    PAPER_COLORS,
};
use crate::cursor::PenCursor;
use crate::cursor_overlay::{CursorFeed, Screen};
use crate::home::Home;
use crate::ink::{InkTransform, Notes, Stroke, Tool};
use crate::ink_layer::{Canvas, Ink, InkLayer, Page, Rect};
use crate::note::{self, Note, NoteWriter, Report};
use crate::outline::{Outline, OutlinePages};
use crate::pages::Pages;
use crate::pen::{capture_config, BatchTap, PenInbox, PenService};
use crate::pdf::{PageRequest, PdfDocumentView, Progress, RenderedPage};
use crate::recent::{self, Recent};
use crate::settings::{PenWeight, Settings};
use crate::system_cursor::SystemCursor;
use crate::timing::{measure, Measurement, Timings};
use crate::view::{Fit, Viewport};

// The two commands a keyboard reaches that the bar also carries.
//
// They are actions — GPUI's unit of a keyboard command — rather than key listeners, so a *binding* is
// what decides which keys mean them, and the bar's buttons and the keyboard end in the same two
// methods. It matters more here than in a text editor: the bar can be hidden, and a hidden bar with no
// keyboard command would have no way to take a stroke back at all.
actions!(cheap_note, [Undo, Redo]);

/// The bitmap-width multiplier used when rendering a PDF page.
const PDF_RENDER_SCALE: f32 = 2.0;

/// The ladder of bitmap widths a page may be rendered at.
const PDF_PIXEL_WIDTHS: [u32; 4] = [1_024, 1_536, 2_304, 3_456];

/// How much of the sheet's size one notch of a wheel adds.
const WHEEL_ZOOM_STEP: f32 = 1.25;

/// How many *lines* the platform reports for one notch of a wheel.
const WHEEL_LINES_PER_NOTCH: f32 = 3.0;

/// How many logical pixels one *line* of a scroll is worth, for panning.
const WHEEL_LINE_HEIGHT: f32 = 32.0;

/// The margin, in logical pixels, between a sheet and the window's edges.
const PAGE_MARGIN: f32 = 24.0;

/// How far the floating bar and the floating page pill sit from the window's edges.
const BAR_MARGIN: f32 = 12.0;

/// How tall the top bar is, in logical pixels, as an estimate.
const BAR_HEIGHT: f32 = 148.0;

/// The key that starts and stops a measurement session.
const MEASURE_KEY: &str = "Ctrl+M";

/// How much of the window's width the status pill may take, as a fraction of it.
const STATUS_PILL_WIDTH: f32 = 0.6;

/// How far above the desk's bottom row the status pill floats, in logical pixels.
const STATUS_LIFT: f32 = 40.0;

/// How often the housekeeping pump wakes.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(1);

/// How long the pen has to be quiet before a page is rendered for it.
const PDF_QUIET_INTERVAL: Duration = Duration::from_millis(120);

/// How long ink may wait in memory before it is handed to the note.
const BATCH_INTERVAL: Duration = Duration::from_millis(500);

/// How much ink may wait in memory before it is handed over early.
const BATCH_STROKES: usize = 200;

/// How long the pen has to be still before the write-ahead log is folded back into the note.
const CHECKPOINT_QUIET_INTERVAL: Duration = Duration::from_secs(5);

/// The least time between two checkpoints, however quiet the pen has been.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(300);

/// Whether a note was written on the sheet in use.
fn sheet_matches(written: (f32, f32), current: (f32, f32)) -> bool {
    (written.0 - current.0).abs() <= 1.0 && (written.1 - current.1).abs() <= 1.0
}

/// The file name of a path, for a message about it.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// What a note's name can be as far as a *file* name goes.
fn file_stem(title: &str) -> String {
    const STEM_MAX: usize = 64;

    let mut stem = String::with_capacity(title.len());
    let mut last_was_space = false;

    for character in title.chars() {
        let character = match character {
            // The reserved set, plus the control characters a paste can bring along.
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => ' ',
            c => c,
        };

        if character.is_whitespace() {
            if !last_was_space {
                stem.push(' ');
                last_was_space = true;
            }
            continue;
        }

        last_was_space = false;
        stem.push(character);
    }

    let stem = stem.trim().trim_end_matches('.').trim_end();
    let cut: String = stem.chars().take(STEM_MAX).collect();

    cut.trim_end().to_string()
}

/// Whether the pen has been quiet long enough to spend a rasterisation on it.
fn pdf_render_due(last_ink_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_ink_at) >= PDF_QUIET_INTERVAL
}

/// The sheet as the last frame drew it: its size in paper units, and where that was placed.
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
    fn transform(&self, scale: f32, bar: f32) -> InkTransform {
        InkTransform {
            scale,
            zoom: self.zoom,
            origin: self.origin,
            paper: self.paper,
            bar,
        }
    }

    /// The part of the sheet that is on screen, in the sheet's own coordinates.
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
    /// Everything the note in hand remembers: its paper, its pen, its switches, and how it feels.
    settings: Settings,
    /// The ink.
    ink: Notes,
    /// The note being written: its folder, its database, and the document it was written on.
    note: Option<Note>,
    /// The note's writer: its own connection, on its own thread.
    writer: Option<NoteWriter>,
    /// How many strokes of each page have been handed to the writer as *appends*.
    saved: BTreeMap<u64, usize>,
    /// Pages whose stored ink no longer matches the page in memory, waiting to be written again.
    rewritten: BTreeSet<u64>,
    /// When a batch was last handed over, for the 500 ms rule.
    saved_at: Instant,
    /// When the write-ahead log was last folded back into the note file.
    checkpointed_at: Instant,
    /// The pages whose ink is in memory, or known to be empty.
    loaded: BTreeSet<usize>,
    /// What each page of the note shows, in reading order.
    pages: Pages,
    /// The pages of the note that are marked, in reading order.
    bookmarks: Bookmarks,
    /// The list of those marks, drawn in front of the note while it is up.
    marks: Marks,
    /// The document's own table of contents, drawn in front of the note while it is up.
    outline: Outline,
    /// The PDF being annotated, if any.
    pdf: PdfDocumentView,
    /// The pen capture and its queue.
    pen: PenService,
    /// The hook that hides the system pointer while the pen has a cursor of its own.
    system_cursor: Option<SystemCursor>,
    /// The window the pen's ghost cursor is drawn in, outside the frame.
    cursor: Option<CursorFeed>,
    /// The canvas, drawn by a renderer of the app's own rather than by a frame.
    ink_layer: Option<InkLayer>,
    /// Counts the rebuilds of the ink's own outlines.
    ink_revision: u64,
    /// When the canvas layer last presented: the rate the ink reaches the screen at, which is not the frame rate.
    last_present: Option<Instant>,
    /// The canvas as the last frame described it: the desk, the page's shadow, and the sheet.
    canvas: Canvas,
    /// The page being shown.
    page_index: usize,
    /// The window's DPI scale factor, captured each frame.
    scale: f32,
    /// How large the sheet is drawn, and where it sits in the window.
    view: Viewport,
    /// The sheet as the last frame drew it.
    sheet: Sheet,
    /// The bar's bottom edge, in window logical pixels, as the last frame laid it out.
    bar_bottom: Rc<Cell<Option<f32>>>,
    /// What every hot path costs, shared with the canvas's renderer.
    timings: Arc<Timings>,
    /// When the pump last woke, for the gap it reports against its own interval.
    last_pump: Option<Instant>,
    /// The page the view is waiting for, if the cache does not have it yet.
    pending_pdf: Option<PageRequest>,
    /// When the pen last laid ink, for [`pdf_render_due`].
    last_ink_at: Instant,
    /// The rule geometry for the current sheet.
    ruling: Ruling,
    /// The status line as last composed.
    status: String,
    /// When the measurement session was started, if one is running.
    session_at: Option<Instant>,
    /// When the last frame was drawn, for the interval between frames.
    last_frame_at: Option<Instant>,
    /// What the last session measured, held on the line until another one replaces it.
    measured: Option<Measurement>,
    /// The last thing worth telling the user.
    message: String,
    /// What the note on the sheet is called — its own name, or one the list would derive.
    note_title: String,
    /// The window's title as last set, so that it is set when it *changes* rather than every frame.
    window_title: String,
    /// The folder whose name is being typed on the sheet, if any.
    naming: Option<PathBuf>,
    /// Whether a name has been asked for, and the field has not been put up yet.
    naming_asked: bool,
    /// The field the name is typed into on the sheet.
    name_input: Entity<InputState>,
    /// The keyboard the sheet gets back when the field goes away.
    sheet_focus: FocusHandle,
    /// Whether the note screen held the keyboard on the last frame it was in front.
    sheet_has_keyboard: bool,
    /// What the field being typed into says — Enter keeps the name, a click away leaves it as it was.
    _name_events: Subscription,
    /// The bar's paper-size chooser.
    sheet_select: Entity<SelectState<Vec<&'static str>>>,
    /// The bar's ruling chooser. Held for the same reason as [`NoteApp::sheet_select`].
    rule_select: Entity<SelectState<Vec<&'static str>>>,
    /// The bar's pen-weight chooser: the pen the note in hand is written with.
    pen_select: Entity<SelectState<Vec<&'static str>>>,
    /// The screen that offers what was opened recently, and the app starts on.
    home: Home,
}

/// The pen thread's tap on the readings: every batch goes to the ghost cursor's window.
fn cursor_tap(feed: &CursorFeed) -> BatchTap {
    let feed = feed.clone();

    Arc::new(move |samples: &[pen_windows::PenSample]| feed.offer(samples))
}

impl NoteApp {
    /// Builds the view, attaches the pen, and starts the frame loop.
    pub fn new(window: &mut Window, start: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        // Nothing to load and nowhere to load it from — no note is open — so this is the shipped set. It
        // is not a *global* set: whatever is opened below takes it over (see [`crate::settings`]).
        let settings = Settings::default();
        let mut message = String::new();

        // The ghost cursor's window, installed before the capture: the pen thread is handed the tap
        // that feeds it, and that tap has to exist when the thread starts. Failing to install one is
        // not an error — the app keeps the system pointer, and the pen has no ghost of its own.
        let cursor = CursorFeed::install(window);

        // The capture must be attached on the thread that owns the window, which is this one.
        let pen = PenService::attach(window, capture_config(), cursor.as_ref().map(cursor_tap));

        // Installed after the capture, so this hook runs first in the subclass chain and gets to
        // answer `WM_SETCURSOR` before anything else can put a cursor back.
        let system_cursor = SystemCursor::install(window);

        // The canvas's own renderer, which this app requires: a machine that cannot give Direct3D 11
        // a hardware device cannot run this app at all, and saying so here — once, at startup, with
        // the reason — is the whole of the report (see [`crate::ink_layer`]).
        let ink_layer = Some(InkLayer::install(window).expect("a canvas for the window"));

        if message.is_empty() {
            message = pen.status().to_string();
        }

        let view = Viewport::new(settings.zoom);

        // The two choosers are built from the style the app starts on, so the first frame already
        // shows the right one. They are the control the style comes from, and they are put back in
        // step with it on any frame where opening a note has moved it — see [`Self::sync_choosers`].
        let sheet_select = choice(
            &CanvasSize::ALL.map(CanvasSize::label),
            CanvasSize::ALL
                .iter()
                .position(|size| *size == settings.canvas_size),
            window,
            cx,
        );
        let rule_select = choice(
            &CanvasStyle::ALL.map(CanvasStyle::label),
            CanvasStyle::ALL
                .iter()
                .position(|style| *style == settings.canvas_style),
            window,
            cx,
        );
        let pen_select = choice(
            &PenWeight::ALL.map(PenWeight::label),
            PenWeight::ALL
                .iter()
                .position(|weight| *weight == settings.pen_weight),
            window,
            cx,
        );

        // The field a note's name is typed into on the sheet, and the two things the field can say:
        // Enter keeps the name, and losing focus — clicking anywhere else — leaves it as it was.
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("A name"));
        let _name_events = cx.subscribe_in(
            &name_input,
            window,
            |app, _, event: &InputEvent, window, cx| app.note_name_event(event, window, cx),
        );
        let sheet_focus = cx.focus_handle();

        let mut app = NoteApp {
            settings,
            ink: Notes::new(),
            note: None,
            writer: None,
            saved: BTreeMap::new(),
            rewritten: BTreeSet::new(),
            saved_at: Instant::now(),
            checkpointed_at: Instant::now(),
            loaded: BTreeSet::new(),
            pages: Pages::default(),
            bookmarks: Bookmarks::default(),
            marks: Marks::new(window, cx),
            outline: Outline::new(window, cx),
            pdf: PdfDocumentView::empty(),
            pen,
            system_cursor,
            cursor,
            ink_layer,
            ink_revision: 0,
            last_present: None,
            canvas: Canvas::default(),
            page_index: 0,
            scale: window.scale_factor(),
            view,
            sheet: Sheet::default(),
            bar_bottom: Rc::new(Cell::new(None)),
            timings: Arc::new(Timings::default()),
            last_pump: None,
            pending_pdf: None,
            last_ink_at: Instant::now(),
            ruling: Ruling::default(),
            status: String::new(),
            session_at: None,
            last_frame_at: None,
            measured: None,
            message,
            note_title: String::new(),
            window_title: String::new(),
            naming: None,
            naming_asked: false,
            name_input,
            sheet_focus,
            sheet_has_keyboard: false,
            _name_events,
            sheet_select,
            rule_select,
            pen_select,
            home: Home::open(window, cx),
        };

        // What the app was asked to open on the command line, if anything: an argument is a
        // *request*, and a request that fails is reported like any other.
        if let Some(path) = start {
            match app.open_or_place(&path) {
                Ok(opened) => app.message = opened,
                Err(error) => app.message = error.to_string(),
            }
        }

        // What a chooser reports is the label that was showing, so a label is what has to become a
        // setting again. A `Confirm` with no choice behind it — the box has been cleaned — is not
        // one, and is ignored: a sheet always has a size, something printed on it, and a pen.
        cx.subscribe(
            &app.sheet_select,
            |app, _, event: &SelectEvent<Vec<&'static str>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    if let Some(size) = CanvasSize::from_label(label) {
                        app.set_canvas_size(size, cx);
                    }
                }
            },
        )
        .detach();

        cx.subscribe(
            &app.rule_select,
            |app, _, event: &SelectEvent<Vec<&'static str>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    if let Some(style) = CanvasStyle::from_label(label) {
                        app.set_canvas_style(style, cx);
                    }
                }
            },
        )
        .detach();

        cx.subscribe(
            &app.pen_select,
            |app, _, event: &SelectEvent<Vec<&'static str>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    if let Some(weight) = PenWeight::from_label(label) {
                        app.set_pen_weight(weight, cx);
                    }
                }
            },
        )
        .detach();

        // The keyboard's way to the same two commands: *global* handlers, because this window has no
        // focusable node for a binding to reach — a binding would work only on the days something had it.
        let view = cx.weak_entity();
        let undo_view = view.clone();
        App::on_action(cx, move |_: &Undo, cx| {
            undo_view.update(cx, |app, cx| app.undo(cx)).ok();
        });
        App::on_action(cx, move |_: &Redo, cx| {
            view.update(cx, |app, cx| app.redo(cx)).ok();
        });

        // The first status line is composed here, so the first frame already has it and no frame
        // has to render text that is about to be replaced.
        app.touch_status();

        // The scan: what the notes folder holds, and the counts of whatever has changed since the
        // index was written. Off the UI thread, so the first frame is on screen while it runs.
        if app.home_is_open() {
            app.start_scan(cx);
        }

        app.start_pumps(cx);
        app
    }

    /// Starts the two loops that turn readings into frames.
    fn start_pumps(&mut self, cx: &mut Context<Self>) {
        self.start_pen_pump(cx);
        self.start_display_pump(cx);
    }

    /// Drains the pen queue whenever it has something in it, and repaints when the ink changed.
    fn start_pen_pump(&mut self, cx: &mut Context<Self>) {
        let inbox: Arc<PenInbox> = self.pen.inbox();

        cx.spawn(async move |this, cx| loop {
            inbox.wait().await;

            let batch = inbox.take();

            let alive = this.update(cx, |app, cx| {
                // The gap between this wake and the last, recorded against the interval a
                // display-paced pump would have used: the number that says whether a stroke is
                // reaching the screen at the rate the pen reports it.
                let woke = Instant::now();
                if let Some(previous) = app.last_pump.replace(woke) {
                    app.timings
                        .pump_gap
                        .record(woke.saturating_duration_since(previous));
                }

                if batch.samples.is_empty() {
                    return;
                }

                // The pen is captured by the *window* rather than by anything on it, so a reading that
                // reached the ink with a screen in front would be ink on a page nobody is looking at. The
                // capture is left alone and the readings are dropped here, where they become strokes.
                if app.home_is_open() {
                    return;
                }

                // How long the newest reading waited for this wake. Together with the capture's
                // own delay — the digitizer to the window, which nothing here can change — this
                // is the whole path from the pen to the frame that shows it.
                app.timings.pen_latency.record(batch.waited);
                // The other half of what a session measures: the same wait, folded into the session
                // rather than into the running mean, so a stretch of writing can be reported on its own.
                app.timings.session.record_response(batch.waited);

                // Ink is the only part of a reading the frame has: a pen in range but not touching lays
                // none, and its ghost is drawn elsewhere (see [`crate::cursor_overlay`]).
                let laid_ink = app.consume_ink(&batch.samples);
                if laid_ink {
                    // The clock the page render waits on: see `serve_pdf`.
                    app.last_ink_at = woke;
                    // And the note: the ink is handed to the writer in batches rather than written
                    // per stroke, on the clock the design note sets. Nothing waits for it.
                    app.persist(woke, false);
                }

                // The system pointer follows the ghost on every read, so the two can never disagree
                // about whether the pen has a cursor of its own.
                app.follow_pen_with_pointer();

                if !laid_ink {
                    // Nothing on screen changed — a reading the resampler dropped, or a hover that moved
                    // the ghost and nothing else — and repainting an identical scene is what made the top
                    // of the window flicker.
                    return;
                }

                // The canvas draws itself here, on the pen's own wake: the ink is the one part of it that
                // changes between frames, and a frame costs several times what the ink does (see
                // [`Self::draw_canvas`]).
                {
                    app.draw_canvas();
                }

                // The counters have moved too, but the line waits for a change the user made: re-shaping
                // text is work a frame drawn to show ink does not have to carry.
                cx.notify();
            });

            if alive.is_err() {
                // The view is gone; the app is closing.
                break;
            }
        })
        .detach();
    }

    /// Re-describes the ink for the canvas layer, from the sheet the last frame drew.
    fn describe_ink(&mut self) {
        let sheet = self.sheet;
        let open = self.ink.open().cloned().map(|mut stroke| {
            stroke.close_live(sheet.zoom);
            Arc::new(stroke)
        });

        self.canvas.ink = Ink {
            origin: sheet.origin,
            zoom: sheet.zoom,
            strokes: Arc::clone(self.ink.finished()),
            open,
            revision: self.ink_revision,
            visible: sheet.visible(),
        };
    }

    /// Hands the canvas to its layer, and reports a failure without losing the layer.
    fn draw_canvas(&mut self) {
        // Nothing has described a canvas yet: this is a reading that arrived before the first frame,
        // and a layer handed an empty description would paint an empty desk over the window.
        if self.canvas.sheet.is_none() {
            return;
        }

        self.describe_ink();

        // Timed by hand rather than with a guard, which would hold a borrow of the counters across the
        // layer's own borrow of the app. This is what the status line's `canvas` clause counts.
        let started = Instant::now();
        let drawn = match self.ink_layer.as_mut() {
            Some(ink) => ink.draw(&self.canvas),
            None => Ok(()),
        };
        self.timings.canvas.record(started.elapsed());

        match drawn {
            Ok(()) => {
                // Handed over: the compositor shows the newest canvas it is given, so the gap between two
                // of these is the rate the ink reaches the screen at, whichever wake drew it.
                if let Some(previous) = self.last_present.replace(started) {
                    self.timings.present_gap.record(started - previous);
                }
            }
            Err(error) => {
                // Kept rather than dropped: no frame paints the canvas, so a dropped layer is a canvas
                // that is gone, while a kept one holds its last frame and can say what is wrong.
                let message = format!("canvas: {error}");
                if self.message != message {
                    self.message = message;
                }
            }
        }
    }

    /// Keeps the counters and the page being rasterised up to date.
    fn start_display_pump(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(HOUSEKEEPING_INTERVAL).await;

            let alive = this.update(cx, |app, cx| {
                let now = Instant::now();

                // The page the view is waiting for, paid for here rather than inside a frame: it is
                // skipped while the pen is laying ink, so the ink pump keeps taking readings.
                let page_rendered = app.serve_pdf(now);

                // The note's own housekeeping, on the same clock: ink still in memory is handed
                // over when the pen has stopped (see `persist`), and whatever the writer has to say
                // — an export finished, or a write that failed — reaches the status line here.
                app.persist(now, false);
                let reported = app.drain_writer_reports();
                app.checkpoint_if_idle(now);

                // Nothing here rebuilds the status line: it is rebuilt by the change that makes it
                // stale, and the counters that move on their own are read from a session a person
                // stops (see the module docs and [`crate::timing::Session`]).
                if page_rendered || reported {
                    cx.notify();
                }
            });

            if alive.is_err() {
                // The view is gone; the app is closing.
                break;
            }
        })
        .detach();
    }

    /// How far down the window the bar reaches: above it a reading belongs to a control, below it to the
    /// page. `0.0` when there is no bar.
    fn bar_edge(&self) -> f32 {
        if !self.settings.show_toolbar {
            return 0.0;
        }

        self.bar_bottom.get().unwrap_or(0.0)
    }

    /// Reads a batch of readings into the ink model, timed.
    fn consume_ink(&mut self, samples: &[pen_windows::PenSample]) -> bool {
        // The pen is not a pen while something is in front of the sheet: a name being typed, or a screen
        // drawn over a sheet nobody can see. This is why the lists are screens rather than panels — the pen
        // obeys an app state, not a rectangle (see [`crate::bookmarks`]).
        if self.naming.is_some() || self.home_is_open() || self.marks.is_open() || self.outline.is_open()
        {
            return false;
        }

        let transform = self.sheet.transform(self.scale, self.bar_edge());
        let _timed = measure(&self.timings.ink);

        self.ink.consume(samples, &transform, &self.settings)
    }

    /// Hands the ink the pen laid to the note, on the design note's clock.
    fn persist(&mut self, now: Instant, worth_flushing: bool) {
        if self.writer.is_none() {
            // Nothing is open, so the ink on screen has nowhere to go — unless it is not the first
            // stroke of a note this app has not made yet. See `ensure_note`: the ink makes the note,
            // rather than being held in memory until someone opens one.
            if self.ink.is_blank() {
                return;
            }

            if let Err(error) = self.ensure_note() {
                self.report(error.to_string());
                return;
            }
        }

        let page = self.page_index as u64;
        let count = self.ink.finished().len();
        let sent = self.saved.get(&page).copied().unwrap_or(0);

        if count < sent {
            // The page is no longer a longer version of what the note holds.
            self.rewritten.insert(page);
        }

        let waiting = count.saturating_sub(sent);
        let rewrite = self.rewritten.contains(&page);
        let stale = now.saturating_duration_since(self.saved_at) >= BATCH_INTERVAL;
        let due = waiting >= BATCH_STROKES
            || (waiting > 0 && stale)
            || (rewrite && (worth_flushing || stale));

        if !due {
            return;
        }

        if rewrite {
            let ink: Vec<Stroke> = self
                .ink
                .finished()
                .iter()
                .map(|stroke| (**stroke).clone())
                .collect();

            if let Some(writer) = &self.writer {
                writer.rewrite(page, ink);
            }
            self.rewritten.remove(&page);
        } else {
            let ink: Vec<Stroke> = self
                .ink
                .finished()
                .iter()
                .skip(sent)
                .map(|stroke| (**stroke).clone())
                .collect();

            if ink.is_empty() {
                return;
            }

            if let Some(writer) = &self.writer {
                writer.append(page, ink);
            }
        }

        self.saved.insert(page, count);
        self.saved_at = now;
    }

    /// Makes a note for ink that has nowhere to go.
    fn ensure_note(&mut self) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);

        let mut note = Note::open(&note::root().join(format!("blank-{stamp}")))?;

        // What the note remembers about itself: the page list, the sheet, every setting in hand, and
        // which page is open. The settings are the app's live state, so a blank page continues the sheet
        // that was open before it.
        let layout = self.pages.layout().to_vec();
        note.store_mut().set_layout(&layout)?;
        note.store_mut().set_sheet(Some(self.sheet_size()))?;
        note.store_mut().set_settings(&self.settings)?;
        note.store_mut().set_open_page(self.page_index as u64)?;

        // And the marks made before there was a note to keep them in: a blank sheet exists only once
        // something is written on it, and a mark is one of the things that makes it exist.
        for page in self.bookmarks.pages() {
            note.store_mut().set_bookmark(*page as u64, true)?;
        }

        // The writer's connection opens before its thread, as it does for a note that is opened, so a
        // note that cannot be written is reported now rather than discovered later.
        let writer = NoteWriter::spawn(note.dir())?;

        self.note = Some(note);
        self.writer = Some(writer);
        self.loaded.insert(self.page_index);
        self.saved_at = Instant::now();
        self.home.hide();
        self.remember_opened(None);

        self.report(String::from("a new note on a blank sheet — Save writes it out as one file"));
        Ok(())
    }

    /// Folds the write-ahead log back into the note file, when the time is right.
    fn checkpoint_if_idle(&mut self, now: Instant) {
        let quiet = now.saturating_duration_since(self.last_ink_at) >= CHECKPOINT_QUIET_INTERVAL;
        let due = now.saturating_duration_since(self.checkpointed_at) >= CHECKPOINT_INTERVAL;
        if !quiet || !due {
            return;
        }

        if let Some(writer) = &self.writer {
            writer.checkpoint();
            self.checkpointed_at = now;
        }
    }

    /// Takes what the writer has to say, and puts it in the status line.
    fn drain_writer_reports(&mut self) -> bool {
        let Some(writer) = &self.writer else {
            return false;
        };

        let reports = writer.drain();
        if reports.is_empty() {
            return false;
        }

        for report in reports {
            match report {
                Report::Written(path) => {
                    // Where the note was carried to is a *setting*, and settings are the note's: the
                    // directory is written into this note, so the next export of *this* note is offered
                    // there (see [`crate::settings`]).
                    self.settings.export_dir = path.parent().map(|parent| parent.to_path_buf());
                    self.save_note_state();
                    self.message = format!("wrote {}", file_label(&path));
                }
                Report::Failed(message) => self.message = message,
            }
        }

        self.touch_status();
        true
    }

    /// Selects the tool an ordinary nib uses.
    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.ink.set_mode(tool);
        cx.notify();
    }

    /// Changes the sheet's size, and its scale with it.
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

    /// Changes how heavy the pen is.
    fn set_pen_weight(&mut self, weight: PenWeight, cx: &mut Context<Self>) {
        self.settings.pen_weight = weight;
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

    /// Whether the pen has a cursor of its own where it is, so the system pointer should be out of the way.
    fn pen_has_its_own_cursor(&self) -> bool {
        // The ghost is drawn in a window of its own, so this is first a question about that window: with
        // no overlay there is nothing to put in the pointer's place, and hiding the pointer would leave
        // the window with no cursor at all. The same answer goes to the overlay (see
        // [`Self::publish_screen`]), so the two cannot disagree.
        if !self.cursor.as_ref().is_some_and(CursorFeed::is_alive) {
            return false;
        }

        if self.home_is_open() {
            return false;
        }

        self.pen_cursor()
            .is_some_and(|cursor| cursor.position()[1] > BAR_HEIGHT)
    }

    /// Tells the ghost cursor's window what this frame knows about the screen it is drawn on.
    fn publish_screen(&self) {
        let Some(cursor) = &self.cursor else {
            return;
        };

        cursor.set_screen(Screen {
            scale: self.scale,
            // The colour the frame used to draw the ghost in before it moved out: not the ink's, so
            // that it is visible on a sheet of any colour, including one where ink would disappear.
            colour: contrast_color(self.settings.page_color) & 0x00FF_FFFF,
            // The line above which a reading belongs to a control rather than to the page. The same
            // estimate the pointer rule uses ([`Self::pen_has_its_own_cursor`]), in physical pixels.
            sheet_top: BAR_HEIGHT * self.scale,
            suppressed: self.home_is_open() || !self.settings.show_tilt_cursor,
        });
    }

    /// Keeps the system pointer in step with the ghost cursor, hiding it exactly while the ghost replaces
    /// it.
    fn follow_pen_with_pointer(&mut self) {
        let has_its_own = self.pen_has_its_own_cursor();

        if let Some(system_cursor) = &mut self.system_cursor {
            system_cursor.follow(has_its_own);
        }
    }

    /// Persists and repaints after anything changed.
    fn finish_setting(&mut self, cx: &mut Context<Self>) {
        self.save_note_state();
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
            self.persist(Instant::now(), true);
            cx.notify();
        }
    }

    /// Puts back the stroke the last undo took away.
    fn redo(&mut self, cx: &mut Context<Self>) {
        if self.ink.redo() {
            self.persist(Instant::now(), true);
            cx.notify();
        }
    }

    /// Removes every stroke.
    fn clear(&mut self, cx: &mut Context<Self>) {
        if !self.ink.is_blank() {
            self.ink.clear();
            self.persist(Instant::now(), true);
            cx.notify();
        }
    }

    /// Shows the previous page.
    fn previous_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index > 0 {
            self.turn_to(self.page_index - 1);
            cx.notify();
        }
    }

    /// Shows the next page.
    fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index + 1 < self.page_total() {
            self.turn_to(self.page_index + 1);
            cx.notify();
        }
    }

    /// Turns to a page: what is being left is written out, and what is being turned to is read in.
    fn turn_to(&mut self, page: usize) {
        self.close_page();

        self.page_index = page;
        self.ink.go_to(page);

        if let Err(error) = self.load_page_ink(page) {
            self.report(error.to_string());
        }

        self.remember_page();
        // The status line names the page, so it is stale as soon as the page changes.
        self.touch_status();
    }

    /// Writes what a page still owes the note, and folds its ink into chunks: the page is closing.
    fn close_page(&mut self) {
        self.persist(Instant::now(), true);

        let page = self.page_index as u64;
        let touched = self.rewritten.remove(&page)
            || self.saved.get(&page).copied().unwrap_or(0) > 0;

        if touched {
            if let Some(writer) = &self.writer {
                writer.compact(page);
            }
        }
    }

    /// Adds a blank page before or after the one being read, and turns to it.
    fn add_page(&mut self, before: bool, cx: &mut Context<Self>) {
        self.close_page();

        let at = self.pages.insert(self.page_index, before);
        self.ink.insert_at(at);
        // The marks are renamed with the pages, because a mark names a page's *position*: see
        // [`crate::bookmarks::Bookmarks::inserted_at`] for the rule and the note's own shift for the
        // rows that back it.
        self.bookmarks.inserted_at(at);

        // The ink is renamed with its pages — `insert_page` moves every page after the insertion along, and
        // the ink moves with the sheet it was written on — and what has already been handed to the writer
        // is renamed with them, so the next append is still measured against the right page.
        if let Some(note) = &mut self.note {
            if let Err(error) = note.store_mut().insert_page(at as u64) {
                self.report(error.to_string());
            }
        }
        self.rename_pages(at as u64, 1);

        self.page_index = at;
        self.turn_to(at);
        self.save_note_state();

        self.report(format!(
            "added a page {} this one ({} of {})",
            if before { "before" } else { "after" },
            self.page_index + 1,
            self.page_total()
        ));
        cx.notify();
    }

    /// Deletes the page being read, and the ink written on it.
    fn delete_page(&mut self, cx: &mut Context<Self>) {
        // What the page holds is dropped rather than written: it has nowhere to be shown, and
        // writing it would leave ink in the note that no page is about. Everything the page *did*
        // owe — ink still in memory — is handed over first, so the deletion cannot race a batch.
        self.close_page();

        let Some(show) = self.pages.remove(self.page_index) else {
            self.report(String::from("a note keeps at least one page"));
            cx.notify();
            return;
        };

        if let Some(note) = &mut self.note {
            if let Err(error) = note.store_mut().delete_page(self.page_index as u64) {
                self.report(error.to_string());
            }
        }
        self.loaded.remove(&self.page_index);
        self.saved.remove(&(self.page_index as u64));
        self.rewritten.remove(&(self.page_index as u64));
        self.rename_pages(self.page_index as u64 + 1, -1);

        self.ink.remove_at(self.page_index);
        // And the mark, if the page had one: what was deleted is a page, so the mark for the page that
        // used to be at this position is gone with it rather than pointing at its neighbour.
        self.bookmarks.removed_at(self.page_index);
        self.page_index = show;
        self.turn_to(show);
        self.save_note_state();

        self.report(format!(
            "deleted a page ({} left)",
            self.page_total().saturating_sub(1).max(1)
        ));
        cx.notify();
    }

    /// Puts a bookmark on the page in front of the reader, or takes the one there off.
    fn toggle_bookmark(&mut self, cx: &mut Context<Self>) {
        let page = self.marked_page(cx);
        self.toggle_page_bookmark(page, cx);
    }

    /// Puts a bookmark on one page of the note, or takes the one there off.
    pub(crate) fn toggle_page_bookmark(&mut self, page: usize, cx: &mut Context<Self>) {
        self.record_mark(page, !self.bookmarks.contains(page), cx);
    }

    /// Takes the bookmark off a page, asked for by the row that names it.
    pub(crate) fn unmark_page(&mut self, page: usize, cx: &mut Context<Self>) {
        self.record_mark(page, false, cx);
    }

    /// Records that a page is marked or is not: in the list, in the note, and in what the app says.
    fn record_mark(&mut self, page: usize, mark: bool, cx: &mut Context<Self>) {
        let changed = self.bookmarks.set(page, mark);

        if let Some(note) = &mut self.note {
            if let Err(error) = note.store_mut().set_bookmark(page as u64, mark) {
                self.report(error.to_string());
            }
        }

        if changed {
            self.report(format!(
                "{} page {} \u{2014} {} marked in this note",
                if mark {
                    "bookmarked"
                } else {
                    "took the bookmark off"
                },
                page + 1,
                self.bookmarks.len()
            ));
        }

        // A list that is up is drawing the rows that have just changed, and those rows are its own copy:
        // it is told, with the highlight left where it was.
        if self.marks.is_open() {
            let marked = self.marked_pages();
            self.marks.refresh(marked, cx);
        }

        // And the contents screen, if *that* is the list that is up: its rows carry what their menus say
        // they will do, so a mark made from one of them has to reach the rows too.
        if self.outline.is_open() {
            if let Some(pages) = self.outline_pages() {
                self.outline.refresh(pages, cx);
            }
        }

        self.touch_status();
        cx.notify();
    }

    /// The page a bookmark command acts on: the page in front of the reader.
    fn marked_page(&self, cx: &Context<Self>) -> usize {
        if self.outline.is_open() {
            if let Some(page) = self.outline.highlighted(cx) {
                return page;
            }
        } else if self.marks.is_open() {
            if let Some(page) = self.marks.highlighted(cx) {
                return page;
            }
        }

        self.page_index
    }

    /// The note's marked pages, gathered for the list to draw.
    fn marked_pages(&self) -> MarkedPages {
        MarkedPages {
            marked: self.bookmarks.pages().to_vec(),
            here: self.page_index,
            total: self.page_total(),
            pages: self.pages.clone(),
            document: self.pdf.file_name(),
        }
    }

    /// Puts the bookmark list in front of the note.
    pub(crate) fn show_marks(&mut self, cx: &mut Context<Self>) {
        let marked = self.marked_pages();
        // The two screens are the same kind of thing and are never both up: this one is opened from the
        // sheet, and a sheet with two lists in front of it is a sheet with one list too many.
        self.outline.hide();
        self.marks.show(marked, cx);

        self.message = if self.bookmarks.is_empty() {
            String::from("no page is marked yet \u{2014} Ctrl+B marks the page in front")
        } else {
            format!("{} marked in this note", self.bookmarks.len())
        };
        self.touch_status();
        cx.notify();
    }

    /// Takes the bookmark list away: what Escape on it does.
    pub(crate) fn hide_marks(&mut self, cx: &mut Context<Self>) {
        self.marks.hide();
        self.touch_status();
        cx.notify();
    }

    /// Shows a marked page — what opening a row of the list does.
    pub(crate) fn go_to_mark(&mut self, page: usize, cx: &mut Context<Self>) {
        self.marks.hide();

        // A row can only be a page the list was given, so this is a guard rather than a case: a note
        // whose pages were deleted from under a list that is somehow still up.
        let page = page.min(self.page_total().saturating_sub(1));
        self.turn_to(page);
        cx.notify();
    }

    /// Turns to the next marked page after this one.
    fn next_bookmark(&mut self, cx: &mut Context<Self>) {
        match self.bookmarks.next_from(self.page_index) {
            Some(page) => {
                self.turn_to(page);
                cx.notify();
            }
            None => {
                let message = self.no_mark_message("after");
                self.report(message);
                cx.notify();
            }
        }
    }

    /// Turns to the last marked page before this one: the step back.
    fn previous_bookmark(&mut self, cx: &mut Context<Self>) {
        match self.bookmarks.previous_from(self.page_index) {
            Some(page) => {
                self.turn_to(page);
                cx.notify();
            }
            None => {
                let message = self.no_mark_message("before");
                self.report(message);
                cx.notify();
            }
        }
    }

    /// What the two steps say when there is nowhere further to go.
    fn no_mark_message(&self, direction: &str) -> String {
        if self.bookmarks.is_empty() {
            String::from("no page is marked yet \u{2014} Ctrl+B marks the page in front")
        } else {
            format!("no marked page {direction} this one")
        }
    }

    /// The document's contents, gathered for the screen that draws them.
    fn outline_pages(&self) -> Option<OutlinePages> {
        (!self.pdf.outline().is_empty()).then(|| {
            OutlinePages::of(
                self.pdf.outline(),
                &self.pages,
                &self.bookmarks,
                self.page_index,
                self.pdf.file_name(),
            )
        })
    }

    /// Puts the document's contents in front of the note.
    pub(crate) fn show_outline(&mut self, cx: &mut Context<Self>) {
        let Some(pages) = self.outline_pages() else {
            self.report(String::from(
                "this note's document carries no contents of its own",
            ));
            cx.notify();
            return;
        };

        let count = pages.rows.len();
        self.marks.hide();
        self.outline.show(pages, cx);

        self.message = if count == 1 {
            String::from("1 entry in this document's contents")
        } else {
            format!("{count} entries in this document's contents")
        };
        self.touch_status();
        cx.notify();
    }

    /// Takes the contents screen away: what Escape on it does.
    pub(crate) fn hide_outline(&mut self, cx: &mut Context<Self>) {
        self.outline.hide();
        self.touch_status();
        cx.notify();
    }

    /// Shows a page the contents names — what opening a row of it does.
    pub(crate) fn go_to_entry(&mut self, page: usize, cx: &mut Context<Self>) {
        self.outline.hide();

        // A row can only name a page the note had when the rows were built, so this is a guard rather than
        // a case: a note whose pages changed under a list that is somehow still up.
        let page = page.min(self.page_total().saturating_sub(1));
        self.turn_to(page);
        cx.notify();
    }

    /// Renames the pages from `from` on by `by`, in what the app remembers about the note.
    fn rename_pages(&mut self, from: u64, by: i64) {
        let rename = |page: u64| -> u64 {
            if page >= from {
                (page as i64 + by).max(0) as u64
            } else {
                page
            }
        };

        self.loaded = self.loaded.iter().map(|page| rename(*page as u64) as usize).collect();
        self.saved = std::mem::take(&mut self.saved)
            .into_iter()
            .map(|(page, count)| (rename(page), count))
            .collect();
        self.rewritten = std::mem::take(&mut self.rewritten)
            .into_iter()
            .map(rename)
            .collect();
    }

    /// Stores what the note remembers about itself: its page list, its sheet, every setting in hand, and
    /// which page is open.
    fn save_note_state(&mut self) {
        let layout = self.pages.layout().to_vec();
        let sheet = self.sheet_size();

        if let Some(note) = &mut self.note {
            let _ = note.store_mut().set_layout(&layout);
            let _ = note.store_mut().set_sheet(Some(sheet));
            let _ = note.store_mut().set_settings(&self.settings);
            let _ = note.store_mut().set_open_page(self.page_index as u64);
        }
    }

    /// How many pages there are to move between.
    fn page_total(&self) -> usize {
        self.pages.len()
    }

    /// Asks the platform for a PDF, or for a saved note, and opens it.
    pub(crate) fn prompt_for_pdf(&mut self, cx: &mut Context<Self>) {
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
    fn open_any(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match self.open_or_place(&path) {
            Ok(message) => self.message = message,
            Err(error) => self.report(error.to_string()),
        }

        self.touch_status();
        cx.notify();
    }

    /// Opens what was chosen, preferring the note that already exists over placing a new one.
    fn open_or_place(&mut self, path: &Path) -> Result<String> {
        if path.is_file() {
            let folder = note::root().join(note::folder_name(path));
            if folder.join(note::NOTE_DB).is_file() {
                return self.open_folder(&folder, Some(path));
            }
        }

        self.place_note(path)
    }

    /// Places a note in the app's own directory and opens it: the import path.
    fn place_note(&mut self, path: &Path) -> Result<String> {
        let folder = if path.is_dir() {
            path.to_path_buf()
        } else {
            note::root().join(note::folder_name(path))
        };

        let note = Note::placed_from(path, &folder)?;
        self.adopt(note, path.is_file().then_some(path))
    }

    /// Opens the note that is already in `folder`, without placing anything.
    fn open_folder(&mut self, folder: &Path, source: Option<&Path>) -> Result<String> {
        let note = Note::open(folder)?;
        self.adopt(note, source)
    }

    /// Takes over an open note: its writer, its document, its pages, the page that was open, and how it is
    /// written on.
    fn adopt(&mut self, mut note: Note, source: Option<&Path>) -> Result<String> {
        let folder = note.dir().to_path_buf();
        let label = source
            .map(Path::to_path_buf)
            .unwrap_or_else(|| folder.clone());

        // Every setting in hand is replaced by the note's own — read *before* the document is opened and
        // before the viewport is built, because the paper's width is what a page is rasterised at (see
        // [`Self::pdf_render_pixel_width`]) and the zoom is where the reader was.
        //
        // `read_settings` fills in only what the note actually says, so a note that has never been told
        // anything keeps the sheet and the pen already up — which is how a blank page, or a PDF just
        // placed, continues what is in hand. The whole set is then written back, so from here the answers
        // travel with *it* and not with whoever was written in last.
        note.store().read_settings(&mut self.settings)?;
        note.store_mut().set_settings(&self.settings)?;

        // A whole note, not a page turn: the pan is reset with the zoom, because the sheet that was
        // being looked at is not the sheet in hand any more.
        self.view = Viewport::new(self.settings.zoom);

        // The writer's connection is opened here, before its thread runs, so that a note that cannot
        // be written is something the user is told about rather than something they discover the next
        // time they look for their ink.
        let writer = NoteWriter::spawn(note.dir())?;

        let pdf = match note.document()? {
            Some((name, bytes)) => {
                PdfDocumentView::open_bytes(name, bytes, self.pdf_render_pixel_width())?
            }
            // A note without a document was written on a blank sheet, and reopening it puts the app
            // back on a blank sheet: leaving whatever document happened to be open behind it would
            // be putting one sheet's writing on another's.
            None => PdfDocumentView::empty(),
        };

        let layout = note.store().layout()?;
        let written = note.store().pages()?;
        let ink_pages = written.last().map_or(1, |page| *page as usize + 1);
        let mut strokes = 0usize;
        for page in &written {
            strokes += note.store().stroke_count(*page)?;
        }
        let sheet = note.store().sheet()?;
        let marks = note.store().bookmarks()?;
        let open_page = note.store().open_page()?.unwrap_or(0) as usize;

        self.pdf = pdf;
        self.pages = Pages::restore(
            (!layout.is_empty()).then_some(layout),
            self.pdf.page_count(),
            ink_pages,
        );
        self.page_index = self.pages.clamp(open_page);
        // The note's marks, in the note's order: what the list draws, and what the next keystroke
        // steps to. The list itself is put away — a screen left open from the note that was closed
        // would be a list of another note's pages.
        self.bookmarks = Bookmarks::of(marks);
        self.marks.hide();
        // And the contents screen: it belongs to the document that was just closed as much as the marks
        // belonged to the note.
        self.outline.hide();

        // Everything the page turn keeps in memory is reset: the ink that was there belonged to the
        // note that is no longer open.
        self.ink = Notes::new();
        self.loaded.clear();
        self.saved.clear();
        self.rewritten.clear();
        self.ink.go_to(self.page_index);

        self.note = Some(note);
        self.writer = Some(writer);
        self.home.hide();
        self.load_page_ink(self.page_index)?;
        self.remember_page();
        self.remember_opened(source);

        // A note written on a different sheet than the one in use would be drawn at the wrong scale,
        // so that is said out loud rather than silently rescaled.
        let here = self.sheet_size();
        Ok(match sheet {
            Some((width, height)) if !sheet_matches((width, height), here) => format!(
                "opened {} — written on a {width:.0}×{height:.0} sheet, this one is {:.0}×{:.0}",
                file_label(&label),
                here.0,
                here.1
            ),
            _ => format!(
                "opened {} ({strokes} strokes on {} pages)",
                file_label(&label),
                written.len()
            ),
        })
    }

    /// Reads a page out of the note, unless it is already in memory.
    fn load_page_ink(&mut self, page: usize) -> Result<()> {
        if self.loaded.contains(&page) {
            return Ok(());
        }

        let stored = match &self.note {
            Some(note) => note.page_strokes(page as u64)?,
            // Nothing is open: the page in front of the user is a blank sheet, and what is drawn on
            // it belongs to a note that does not exist yet.
            None => Vec::new(),
        };

        // What the note already holds is what has been written as far as the writer is concerned,
        // and it is the number a later append is measured against.
        let written = match &self.note {
            Some(note) => note.store().stroke_count(page as u64)?,
            None => 0,
        };

        self.ink
            .put_page(page, crate::ink::InkDocument::from_strokes(stored));
        self.saved.insert(page as u64, written);
        self.loaded.insert(page);
        Ok(())
    }

    /// Records which page is open, so reopening the note comes back to it.
    fn remember_page(&mut self) {
        if let Some(note) = &mut self.note {
            let _ = note.store_mut().set_open_page(self.page_index as u64);
        }
    }

    /// Whether the home screen is in front of the sheet.
    fn home_is_open(&self) -> bool {
        self.home.is_open()
    }

    /// Gives the keyboard to the screen in front, on the frame that screen changes.
    fn claim_sheet_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let in_front = !self.home_is_open()
            && !self.marks.is_open()
            && !self.outline.is_open()
            && self.naming.is_none();

        if in_front == self.sheet_has_keyboard {
            return;
        }

        self.sheet_has_keyboard = in_front;

        if in_front {
            self.sheet_focus.focus(window, cx);
        }
    }

    /// Shows the home screen: where the app starts, and where Esc goes from a note.
    fn show_home(&mut self, cx: &mut Context<Self>) {
        if self.home.is_open() {
            return;
        }

        self.close_page();

        if let Some(note) = &self.note {
            let folder = note.dir().to_path_buf();
            self.home.reveal(&folder);
        }

        self.home.show();
        self.start_scan(cx);
        self.touch_status();
        cx.notify();
    }

    /// Reads what the notes in the list hold, off the UI thread.
    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.home.is_scanning() {
            return;
        }

        self.home.begin_scan();

        let root = note::root();
        let known: Vec<(PathBuf, Option<recent::Stamp>)> = self
            .home
            .entries()
            .iter()
            .map(|entry| (entry.folder.clone(), entry.stamp))
            .collect();

        cx.spawn(async move |this, cx| {
            let (found, articles) = cx
                .background_executor()
                .spawn(async move { crate::home::scan(&root, &known) })
                .await;

            this.update(cx, |app, cx| {
                let changed = app.home.scanned(&found, &articles);
                if changed {
                    if let Err(error) = app.home.save() {
                        app.message =
                            format!("the list of recent notes could not be written: {error}");
                    }
                }

                // A scan that *adopted* notes — folders it had never been told about — has their
                // counts one pass away, because a scan only reads what the index already lists. One
                // more pass learns them; that pass adopts nothing, so it is the last one.
                if changed && articles.is_empty() {
                    app.start_scan(cx);
                }

                app.touch_status();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The home screen's keyboard: the chords that are the app's.
    pub(crate) fn home_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;

        if keystroke.modifiers.control {
            match keystroke.key.as_str() {
                "n" => self.new_blank_sheet(cx),
                "o" => self.prompt_for_pdf(cx),
                "s" => self.save(cx),
                _ => {}
            }
            return;
        }

        if keystroke.modifiers.alt || keystroke.modifiers.platform {
            return;
        }

        // A rename in progress owns the keyboard: Escape abandons it, and everything else is the
        // field's own business (Enter arrives as the field's event; see [`Self::home_name_event`]).
        if keystroke.key.as_str() == "escape" && self.home.is_renaming(cx) {
            self.home.stop_rename(window, cx);
            self.touch_status();
            cx.notify();
        }
    }

    /// Starts naming the row a menu was opened on.
    pub(crate) fn rename_entry(&mut self, entry: Recent, cx: &mut Context<Self>) {
        self.home.ask_rename(&entry.folder, cx);
        self.message = String::from("type a name, Enter keeps it, Esc leaves it as it was");
        self.touch_status();
        cx.notify();
    }

    /// What the name field says: Enter keeps the name, and clicking away leaves it as it was.
    pub(crate) fn home_name_event(
        &mut self,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.commit_rename(window, cx),
            InputEvent::Blur => {
                if self.home.is_renaming(cx) {
                    self.home.stop_rename(window, cx);
                    cx.notify();
                }
            }
            InputEvent::Change | InputEvent::Focus => {}
        }
    }

    /// Keeps the name that was typed into the list's field: the note is renamed, and the list is told what
    /// it said.
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((folder, name)) = self.home.take_rename(cx) else {
            return;
        };

        self.keep_name(folder, &name, cx);
        // The field goes away with the name, and the keyboard goes back to the list: the row is a row
        // again, and a window with nothing focused is a window whose arrows go nowhere.
        self.home.stop_rename(window, cx);
        self.touch_status();
        cx.notify();
    }

    /// Keeps the name that was typed into the sheet's field: the same thing, from the other screen.
    fn commit_note_name(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.naming.take() else {
            return;
        };

        let name = self.name_input.read(cx).value().to_string();
        self.keep_name(folder, &name, cx);
        // The keyboard goes back to whatever is in front on the next frame, through the one rule that knows
        // which screen that is: a field does not know what is behind it (see [`Self::claim_sheet_keyboard`]).
        self.sheet_has_keyboard = false;
        self.touch_status();
        cx.notify();
    }

    /// Asks for the note on the sheet to be renamed, on the next frame.
    fn ask_note_name(&mut self, cx: &mut Context<Self>) {
        if self.naming.is_some() {
            return;
        }

        if self.note.is_none() {
            self.report(String::from(
                "there is nothing to rename yet \u{2014} the first stroke makes the note",
            ));
            return;
        }

        self.naming_asked = true;
        cx.notify();
    }

    /// Puts the name field in the bar, holding the name the note shows now.
    fn start_note_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(note) = self.note.as_ref() else {
            return;
        };

        let folder = note.dir().to_path_buf();
        let written = self.note_title.clone();

        self.name_input.update(cx, |input, cx| {
            input.set_value(written, window, cx);
            // Selected, so that the first character typed replaces what is showing.
            input.select_all(window, cx);
            input.focus_handle(cx).focus(window, cx);
        });

        self.naming = Some(folder);
        self.message = String::from("type a name, Enter keeps it, Esc leaves it as it was");
        self.touch_status();
        cx.notify();
    }

    /// Leaves the name as it was, and gives the keyboard back to the sheet.
    fn stop_note_name(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.naming.take().is_none() {
            return;
        }

        // As when a name is kept: the keyboard goes back to the screen in front on the next frame, and this
        // is what makes that frame re-claim it (see [`Self::claim_sheet_keyboard`]).
        self.sheet_has_keyboard = false;
        self.touch_status();
        cx.notify();
    }

    /// What the sheet's name field says: Enter keeps the name, and clicking away leaves it as it was.
    pub(crate) fn note_name_event(
        &mut self,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.commit_note_name(window, cx),
            InputEvent::Blur => self.stop_note_name(window, cx),
            InputEvent::Change | InputEvent::Focus => {}
        }
    }

    /// The name a person gave a note, wherever it was typed.
    fn keep_name(&mut self, folder: PathBuf, name: &str, cx: &mut Context<Self>) {
        match note::set_title(&folder, name) {
            Ok(facts) => {
                // What the *note* says, not what the list would show for it: storing the words the list
                // derives would freeze them — a row called "Blank sheet" because that is what it was
                // called the day it was made.
                let named = facts.title.clone().unwrap_or_default();
                let shown = if named.is_empty() {
                    Recent::title_for(&folder)
                } else {
                    named.clone()
                };

                if let Err(error) = self.home.rename(&folder, &named) {
                    self.message =
                        format!("the name was kept, but the list could not be written: {error}");
                } else if named.is_empty() {
                    self.message = format!("{shown} has no name of its own again");
                } else {
                    self.message = format!("named {named}");
                }

                // The note may be the one on the sheet: its name is what the window says, and what the
                // file it is written out as is called.
                if self
                    .note
                    .as_ref()
                    .is_some_and(|note| note.dir() == folder.as_path())
                {
                    self.note_title = shown;
                }
            }
            Err(error) => self.report(format!("{error}")),
        }

        cx.notify();
    }

    /// Takes an entry out of the list. The note is *not* deleted.
    pub(crate) fn forget_entry(&mut self, entry: Recent, cx: &mut Context<Self>) {
        match self.home.forget(&entry.folder) {
            Ok(()) => {
                self.message = format!(
                    "forgot {} \u{2014} the note is still in {}",
                    entry.shown_title(),
                    entry.folder.display()
                );
            }
            Err(error) => self.report(error.to_string()),
        }

        self.touch_status();
        cx.notify();
    }

    /// Opens what the list confirmed.
    pub(crate) fn open_entry(&mut self, entry: Recent, cx: &mut Context<Self>) {
        let opened = if entry.folder.is_dir() {
            self.open_folder(&entry.folder, entry.source.as_deref())
        } else if let Some(source) = &entry.source {
            if source.is_file() {
                self.place_note(source)
            } else {
                Err(anyhow::anyhow!(
                    "{} is gone, and so is the note it was written on",
                    file_label(source)
                ))
            }
        } else {
            Err(anyhow::anyhow!(
                "the note in {} is gone",
                entry.folder.display()
            ))
        };

        match opened {
            Ok(message) => self.message = message,
            Err(error) => self.report(error.to_string()),
        }

        self.touch_status();
        cx.notify();
    }

    /// Starts a blank sheet: a note that does not exist until the first stroke.
    pub(crate) fn new_blank_sheet(&mut self, cx: &mut Context<Self>) {
        self.close_page();

        self.note = None;
        self.writer = None;
        self.pdf = PdfDocumentView::empty();
        self.pages = Pages::default();
        self.bookmarks = Bookmarks::default();
        // A screen over a sheet that is no longer open is a screen about nothing.
        self.marks.hide();
        self.outline.hide();
        self.page_index = 0;
        self.ink = Notes::new();
        self.loaded.clear();
        self.saved.clear();
        self.rewritten.clear();
        self.ink.go_to(0);
        self.loaded.insert(0);
        self.home.hide();
        self.remember_page();

        // And the *name* goes with the note: the bar, the window and `Save`'s file name all come from this
        // one string, and a blank sheet named after the document it no longer has would file its first
        // stroke under that name (see [`Self::save`]).
        self.note_title.clear();

        self.message =
            String::from("a blank sheet — the first stroke makes a note, and Save writes it out");
        self.touch_status();
        cx.notify();
    }

    /// The note screen's keyboard: the way into the home screen, and Save.
    fn note_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;

        if !keystroke.modifiers.control {
            match keystroke.key.as_str() {
                "escape" => {
                    if self.naming.is_some() {
                        self.stop_note_name(window, cx);
                    } else {
                        self.show_home(cx);
                    }
                }
                // The arrows turn the page — the one thing a keyboard is asked for here, and the same
                // two commands the page pill's buttons are. Not while a name is being typed: there the
                // arrows belong to the field, and a caret that turned the page instead would be a trap.
                "left" if self.naming.is_none() => self.previous_page(cx),
                "right" if self.naming.is_none() => self.next_page(cx),
                _ => {}
            }
            return;
        }

        // The bookmark chords first, because the letter is shared: `Ctrl+B` marks the page in front and
        // `Ctrl+Shift+B` opens the list of what is marked — the two halves of one idea, which is why
        // they are the two halves of one key.
        if keystroke.key.as_str() == "b" {
            if keystroke.modifiers.shift {
                self.show_marks(cx);
            } else {
                self.toggle_bookmark(cx);
            }
            return;
        }

        match keystroke.key.as_str() {
            // The document's own contents, under the letter that already means "open a file" — a table of
            // contents is what a document is opened *by*, so the two chords sit on one key as the two
            // bookmark chords do. A document without one is answered in words (see [`Self::show_outline`]).
            "o" if keystroke.modifiers.shift => self.show_outline(cx),
            "n" => self.new_blank_sheet(cx),
            "o" => self.prompt_for_pdf(cx),
            "s" => self.save(cx),
            // The measurement session, on the letter with nothing else to mean: `M` for measure. The
            // sheet's own key, like these, because a session is measured *while* writing — and one key
            // both starts and stops it, which the line in front of the reader says while it runs.
            "m" => self.toggle_measurement(cx),
            // The two steps between marks. The list is not needed for these, which is the point of
            // them: one keystroke per mark, for a reader hopping between the pages they use.
            "down" => self.next_bookmark(cx),
            "up" => self.previous_bookmark(cx),
            _ => {}
        }
    }

    /// The bookmark list's keyboard: the app's own chords, and nothing the list wants.
    pub(crate) fn marks_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.keystroke.modifiers.control {
            return;
        }

        match event.keystroke.key.as_str() {
            "b" if !event.keystroke.modifiers.shift => self.toggle_bookmark(cx),
            "n" => self.new_blank_sheet(cx),
            "o" => self.prompt_for_pdf(cx),
            "s" => self.save(cx),
            _ => {}
        }
    }

    /// The contents screen's keyboard: the app's own chords, and nothing the list wants.
    pub(crate) fn outline_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.keystroke.modifiers.control {
            return;
        }

        match event.keystroke.key.as_str() {
            "b" if !event.keystroke.modifiers.shift => self.toggle_bookmark(cx),
            "n" => self.new_blank_sheet(cx),
            "o" if !event.keystroke.modifiers.shift => self.prompt_for_pdf(cx),
            "s" => self.save(cx),
            _ => {}
        }
    }

    /// Remembers what was just opened, so the home screen offers it next time.
    fn remember_opened(&mut self, source: Option<&Path>) {
        let Some(note) = &self.note else {
            return;
        };

        let folder = note.dir().to_path_buf();
        let document = note.store().document().ok().flatten();
        // A note remembers the file it was made from (see [`crate::store::META_SOURCE`]), so an entry
        // opened by its folder — an adopted one, or one whose list was lost — still says where it came
        // from rather than losing that fact along with the list.
        let source = source
            .map(Path::to_path_buf)
            .or_else(|| note.store().source().ok().flatten().map(PathBuf::from));
        let title = note
            .store()
            .title()
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                document
                    .as_deref()
                    .map(|name| match name.rsplit_once('.') {
                        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
                        _ => name.to_string(),
                    })
                    .unwrap_or_else(|| Recent::title_for(&folder))
            });

        self.note_title = title.clone();
        let entry = Recent::note(folder, source, title, document, recent::now_ms());

        if let Err(error) = self.home.record(entry) {
            self.message = format!("the list of recent notes could not be written: {error}");
        }
    }

    /// Writes the note out as one file, asking where it should go.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.note.is_none() {
            self.report(String::from("there is nothing to write out yet"));
            cx.notify();
            return;
        }

        let suggested = format!("{}.zip", self.note_stem());
        let directory = self
            .settings
            .export_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                this.update(cx, |app, cx| app.write_transfer(path, cx)).ok();
            }
        })
        .detach();
    }

    /// Asks the writer for the file, and says so when it has it.
    fn write_transfer(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.persist(now, true);
        self.close_page();

        match &self.writer {
            Some(writer) => {
                writer.export(path.clone());
                self.message = format!("writing {}", file_label(&path));
            }
            None => self.report(String::from("there is nothing to write out yet")),
        }

        self.touch_status();
        cx.notify();
    }

    /// The name a note about the open document should suggest.
    fn note_stem(&self) -> String {
        let named = file_stem(&self.note_title);
        if !named.is_empty() {
            return named;
        }

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
    /// The bitmap width a page is rendered at, in device pixels.
    fn pdf_render_pixel_width(&self) -> u32 {
        let logical = self.settings.page_display_width * self.view.zoom();
        let wanted = logical * self.scale.max(1.0) * PDF_RENDER_SCALE;

        quantise_width(wanted)
    }

    /// The rendered page for the current index, when a PDF is open.
    fn current_page(&mut self) -> Option<RenderedPage> {
        // The page being read is a page of the *note*; the picture, if it has one, belongs to a page
        // of the document. A blank page has no picture at all, whatever else is open.
        let Some(document_page) = self.pages.document_page(self.page_index) else {
            if self.pdf.rendering() {
                self.pdf.abandon();
            }
            self.pending_pdf = None;
            return None;
        };

        if !self.pdf.is_loaded() {
            return None;
        }

        let wanted = PageRequest::new(document_page, self.pdf_render_pixel_width())
            .grayscale(self.settings.grayscale_pages);

        if let Some(page) = self.pdf.page_for_frame(wanted) {
            self.plan_pdf(wanted);
            return Some(page);
        }

        // Nothing for this page at any rung: pay for the cheapest one now, so the frame has
        // something to draw, then leave the rung the zoom wanted to the pump.
        let preview = PageRequest::new(document_page, PDF_PIXEL_WIDTHS[0])
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

        // Timed like a page render, because it is the same work paid for in pieces: the number that
        // matters is whether a slice runs past its budget, which is a frame's time (see `pdf_slice`).
        let slice = Instant::now();
        let progress = self.pdf.advance(crate::pdf::SLICE_BUDGET);
        self.timings.pdf_slice.record(slice.elapsed());

        match progress {
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
    fn report(&mut self, message: String) {
        if self.message != message {
            self.message = message;
            self.touch_status();
        }
    }

    /// Where the sheet is drawn, in the window the frame is painting.
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

        // The marks, said only when there are any: it is the answer to "which pages did I keep", and a
        // zero on every frame is noise — the same rule the off-the-sheet readings follow below.
        if !self.bookmarks.is_empty() {
            parts.push(format!("{} marked", self.bookmarks.len()));
        }
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

        // The readings that landed beside the page, said only when there are any: it is the answer to
        // "why did that line stop at the edge of the sheet?", and a zero on every frame is noise.
        if ink.off_paper > 0 {
            parts.push(format!("{} off the sheet", ink.off_paper));
        }

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

        // The measurement session, in front of the live meters rather than instead of them: the meters are
        // what is happening now, and this is the stretch that was measured (see [`crate::timing::Session`]).
        if self.session_at.is_some() {
            parts.push(format!("measuring: {MEASURE_KEY} stops it"));
        } else if let Some(measured) = self.measured {
            parts.push(measured.summary());
        }

        parts.push(self.timings.summary());

        if !self.message.is_empty() {
            parts.push(self.message.clone());
        }

        parts.join("   ·   ")
    }

    /// Rebuilds the status line, for a change the user made and expects to see at once.
    fn touch_status(&mut self) {
        self.status = self.compose_status();
    }

    /// Starts or stops the measurement session, and puts what it found on the status line.
    fn toggle_measurement(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();

        match self.session_at.take() {
            // Stopping. The counters are left standing rather than cleared, so the line can be
            // rebuilt for something else and still say what was measured.
            Some(started) => {
                let over = now.saturating_duration_since(started);
                self.measured = Some(self.timings.session.stop(over));
            }
            // Starting. The last result goes, so a session that is running cannot be mistaken for the
            // answer of the one before it.
            None => {
                self.timings.session.start();
                self.measured = None;
                self.session_at = Some(now);
            }
        }

        self.touch_status();
        cx.notify();
    }

    /// The bar: floating, rounded, over the sheet.
    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline) = (theme.title_bar, theme.title_bar_border);

        // The mark below writes through this; the pump reads it later, with no window to ask.
        let bar_bottom = Rc::clone(&self.bar_bottom);

        // Full width from a stretched box with a margin's worth of padding, not from both insets on the bar:
        // an element with a left *and* a right inset is laid out at its content's size here, and a bar at
        // its content's size never wraps its second row — it runs off the edge of the window.
        div()
            .absolute()
            .top(px(BAR_MARGIN))
            .left_0()
            .w_full()
            .px(px(BAR_MARGIN))
            // A zero-height box pinned to the bar's own bottom edge, so it reports the real edge whatever
            // the bar's height turned out to be. It paints nothing, takes no pointer, and is out of the
            // layout: it exists to be measured.
            .child(
                canvas(
                    move |bounds, _, _| bar_bottom.set(Some(f32::from(bounds.bottom()))),
                    |_, _, _, _| {},
                )
                .absolute()
                .bottom_0()
                .h_0(),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p(px(8.0))
                    .w_full()
                    .rounded(theme.radius_lg)
                    .bg(surface)
                    .border_1()
                    .border_color(hairline)
                    // The one place in the interface that casts a renderer shadow: a bar is a *layer*
                    // over the desk. The sheet's shadow is painted with the sheet instead — it is a
                    // path in a paint callback, and a callback cannot put a layer behind itself.
                    .shadow_md()
                    .child(self.command_row(cx))
                    .child(self.sheet_row(cx)),
            )
    }

    /// The bar's first row: where the document is, what the nib does, and what acts on the note.
    fn command_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (hairline, muted) = (theme.title_bar_border, theme.muted_foreground);

        let tool = self.ink.mode();

        let tools = vec![
            tool_button("tool-pen", IconName::Pen, "Pen", tool == Tool::Pen, cx, |app, cx| {
                app.set_tool(Tool::Pen, cx)
            })
            .into_any_element(),
            tool_button(
                "tool-eraser",
                IconName::Eraser,
                "Eraser",
                tool == Tool::Eraser,
                cx,
                |app, cx| app.set_tool(Tool::Eraser, cx),
            )
            .into_any_element(),
        ];

        // Reading order is left to right, so the commands sit in the order they are reached for:
        // take the last stroke back, put it forward again, take everything back, open something
        // else, write it out.
        //
        // Offered only when the page's history says they would do something: a button that sometimes does
        // nothing is what a broken command looks like.
        let actions = vec![
            // The way back to the list, first, because it is the way *out* of everything else: what
            // was opened is where a person starts, and Escape is the keyboard's way to the same
            // place.
            icon_button(
                "home",
                IconName::NotebookPen,
                "What you have been writing (Esc)",
                true,
                cx,
                |app, cx| app.show_home(cx),
            )
            .into_any_element(),
            icon_button(
                "undo",
                IconName::Undo2,
                "Undo (Ctrl+Z)",
                self.ink.can_undo(),
                cx,
                |app, cx| app.undo(cx),
            )
            .into_any_element(),
            icon_button(
                "redo",
                IconName::Redo2,
                "Redo (Ctrl+Y)",
                self.ink.can_redo(),
                cx,
                |app, cx| app.redo(cx),
            )
            .into_any_element(),
            icon_button(
                "clear",
                IconName::Trash,
                "Clear the ink",
                true,
                cx,
                |app, cx| app.clear(cx),
            )
            .into_any_element(),
            icon_button(
                "open-note",
                IconName::FolderOpen,
                "Open a note or a PDF",
                true,
                cx,
                |app, cx| app.prompt_for_pdf(cx),
            )
            .into_any_element(),
            icon_button(
                "save-note",
                IconName::Save,
                "Save the note",
                true,
                cx,
                |app, cx| app.save(cx),
            )
            .into_any_element(),
        ];

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

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .w_full()
            .text_color(muted)
            .child(self.title_chip(cx))
            .child(toolbar_divider(hairline))
            .children(tools)
            .child(div().flex_1())
            .children(actions)
            .child(toolbar_divider(hairline))
            .children(switches)
    }

    /// The note's name, at the left of the bar, where a notebook shows its title.
    fn title_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, foreground) = (theme.muted_foreground, theme.foreground);
        let named = !self.note_title.is_empty();

        let title: AnyElement = if self.naming.is_some() {
            Input::new(&self.name_input)
                .small()
                .w(px(220.0))
                .into_any_element()
        } else if named {
            Label::new(self.note_title.clone()).into_any_element()
        } else {
            Label::new("Untitled note").text_color(muted).into_any_element()
        };

        div()
            .id("note-title")
            .flex()
            .flex_row()
            .items_center()
            .gap_1p5()
            .pl(px(4.0))
            .pr_1()
            // A long name is cut rather than wrapped or allowed to push the commands off the row: the
            // bar has a fixed height, and the name is the one piece of text here whose length the user
            // chooses.
            .max_w(px(260.0))
            .text_color(if named { foreground } else { muted })
            .child(div().text_size(px(15.0)).child(IconName::BookOpen))
            .child(
                div()
                    .text_size(px(13.0))
                    .whitespace_nowrap()
                    .truncate()
                    .child(title),
            )
            .on_click(cx.listener(|app, event: &ClickEvent, _, cx| {
                // Two clicks on a name are a person saying "this word, right here". A single click is
                // nothing: the title is not a control, and a label that acts on one click is a label
                // that acts on every stray click.
                if event.click_count() >= 2 {
                    app.ask_note_name(cx);
                }
            }))
    }

    /// The bar's second row: the page's commands, the sheet's size, its ruling, its two colours, and the
    /// pen's weight.
    fn sheet_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (hairline, muted, accent) = (
            theme.title_bar_border,
            theme.muted_foreground,
            theme.primary,
        );

        // Paper is a square — a page has corners — and ink is a circle, which is what a pen's
        // colour looks like on every writing app there is. Two shapes, one table of colours.
        let mut paper: Vec<AnyElement> = Vec::new();
        for swatch in PAPER_COLORS.iter() {
            paper.push(
                swatch_button(
                    swatch,
                    self.settings.page_color == swatch.color,
                    accent,
                    false,
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
                    true,
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
            .pt_2()
            .border_t_1()
            .border_color(hairline)
            .child(control_label("Page", muted))
            .child(self.page_controls(cx))
            .child(toolbar_divider(hairline))
            .child(control_label("Sheet", muted))
            .child(self.chooser(&self.sheet_select))
            .child(toolbar_divider(hairline))
            .child(control_label("Style", muted))
            .child(self.chooser(&self.rule_select))
            .child(toolbar_divider(hairline))
            .child(control_label("Paper", muted))
            .children(paper)
            .child(toolbar_divider(hairline))
            .child(control_label("Ink", muted))
            .children(ink)
            .child(toolbar_divider(hairline))
            .child(control_label("Weight", muted))
            .child(self.chooser(&self.pen_select))
            .child(toolbar_divider(hairline))
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

    /// One of the row's choosers, at the width the bar gives it.
    fn chooser(&self, state: &Entity<SelectState<Vec<&'static str>>>) -> impl IntoElement {
        div().w(px(112.0)).child(Select::new(state).small())
    }

    /// Keeps the three choosers showing what the note is actually written with.
    fn sync_choosers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (canvas_size, canvas_style, pen_weight) = (
            self.settings.canvas_size,
            self.settings.canvas_style,
            self.settings.pen_weight,
        );
        let boxes = [
            (
                self.sheet_select.clone(),
                chosen_index(&CanvasSize::ALL, canvas_size),
            ),
            (
                self.rule_select.clone(),
                chosen_index(&CanvasStyle::ALL, canvas_style),
            ),
            (
                self.pen_select.clone(),
                chosen_index(&PenWeight::ALL, pen_weight),
            ),
        ];

        for (chooser, chosen) in boxes {
            // Reborrowed per box: a `Select`'s state can only be moved *with* a window, and one
            // window cannot be lent to three closures at once.
            let window = &mut *window;

            chooser.update(cx, |state, cx| {
                if state.selected_index(cx) != chosen {
                    state.set_selected_index(chosen, window, cx);
                    cx.notify();
                }
            });
        }
    }

    /// The desk's own row: the counters at one edge, the page in the middle, the zoom at the other.
    fn bottom_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Full width with a margin's worth of padding, for the reason the bar gives: the width has to
        // come from somewhere, and it cannot come from two insets on the same element.
        let mut row = div()
            .absolute()
            .bottom(px(BAR_MARGIN))
            .left_0()
            .w_full()
            .px(px(BAR_MARGIN))
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .child(self.page_pill(cx));

        // Built only when it is wanted: `compose_status` returns an empty line for a hidden status,
        // and a pill drawn around nothing is still a pill.
        if self.settings.show_status {
            row = row.child(
                div()
                    .absolute()
                    .left_0()
                    // A row higher than the page pill, because the line wraps into a wide box and the
                    // page pill is in the middle of this one: see [`STATUS_LIFT`].
                    .bottom(px(STATUS_LIFT))
                    .child(self.status_pill(cx)),
            );
        }

        // Zoom stays when the bar is hidden, unlike the controls up there: the switch that brings the bar
        // back lives *in* the bar, so a sheet zoomed into a corner with the bar away would have no way
        // out. It is reading rather than writing, which is what the page pill and the counters share.
        row = row.child(
            div()
                .absolute()
                .right_0()
                .bottom_0()
                .child(self.zoom_pill(cx)),
        );

        row
    }

    /// Which page is on the desk, and the way to the others.
    fn page_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline, foreground) = (
            theme.title_bar,
            theme.title_bar_border,
            theme.foreground,
        );

        // One page is still one page: a document with a single page, or no document at all, reads
        // as "1 / 1" rather than as a count of nothing.
        let total = self.page_total().max(1);

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px(px(4.0))
            .py(px(4.0))
            .rounded(theme.radius_lg)
            .bg(surface)
            .border_1()
            .border_color(hairline)
            .shadow_sm()
            .child(icon_button(
                "page-prev",
                IconName::ChevronLeft,
                "Previous page (Left arrow)",
                true,
                cx,
                |app, cx| app.previous_page(cx),
            ))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(foreground)
                    .whitespace_nowrap()
                    .px_1()
                    .child(format!("{} / {total}", self.page_index + 1)),
            )
            .child(icon_button(
                "page-next",
                IconName::ChevronRight,
                "Next page (Right arrow)",
                true,
                cx,
                |app, cx| app.next_page(cx),
            ))
    }

    /// The way the sheet is looked at: two steps, the number they are at, and the two fits.
    fn zoom_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline, accent, radius) = (
            theme.title_bar,
            theme.title_bar_border,
            theme.primary,
            theme.radius_lg,
        );

        // Collected before they are chained, for the reason the page commands give.
        let mut controls: Vec<AnyElement> = Vec::new();
        controls.push(
            icon_button(
                "zoom-out",
                IconName::Minus,
                "Zoom out",
                true,
                cx,
                |app, cx| app.zoom_out(cx),
            )
            .into_any_element(),
        );
        controls.push(
            div()
                .text_size(px(12.0))
                .text_color(accent)
                .whitespace_nowrap()
                .min_w(px(42.0))
                .text_center()
                .child(format!("{:.0}%", self.view.zoom() * 100.0))
                .into_any_element(),
        );
        controls.push(
            icon_button(
                "zoom-in",
                IconName::Plus,
                "Zoom in",
                true,
                cx,
                |app, cx| app.zoom_in(cx),
            )
            .into_any_element(),
        );
        // A hairline between stepping and fitting: one changes the number, the other works it out
        // from the window, and a run of four arrows with no punctuation reads as four steps.
        controls.push(toolbar_divider(hairline).into_any_element());
        for fit in Fit::ALL {
            // A double arrow, pointing the way the sheet is made to fit: the axis a fit is against
            // is the direction its arrow points.
            let icon = match fit {
                Fit::Width => IconName::MoveHorizontal,
                Fit::Height => IconName::MoveVertical,
            };
            controls.push(
                icon_button(fit.button_id(), icon, fit.label(), true, cx, move |app, cx| {
                    app.fit_sheet(fit, cx)
                })
                .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px(px(4.0))
            .py(px(4.0))
            .rounded(radius)
            .bg(surface)
            .border_1()
            .border_color(hairline)
            .shadow_sm()
            .children(controls)
    }

    /// The page commands: insert a page before or after this one, delete this one, and mark it.
    fn page_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let hairline = theme.title_bar_border;
        let marked = self.bookmarks.contains(self.page_index);

        // Collected before they are chained: each `cx.listener` takes a mutable borrow of the
        // context, and one long builder chain would hold them all at once.
        let controls = vec![
            icon_button(
                "page-before",
                IconName::BetweenVerticalStart,
                "Add a page before this one",
                true,
                cx,
                |app, cx| app.add_page(true, cx),
            )
            .into_any_element(),
            icon_button(
                "page-after",
                IconName::BetweenVerticalEnd,
                "Add a page after this one",
                true,
                cx,
                |app, cx| app.add_page(false, cx),
            )
            .into_any_element(),
            toolbar_divider(hairline).into_any_element(),
            icon_button(
                "page-delete",
                IconName::FileX,
                "Delete this page",
                true,
                cx,
                |app, cx| app.delete_page(cx),
            )
            .into_any_element(),
            toolbar_divider(hairline).into_any_element(),
            tool_button(
                "page-mark",
                if marked {
                    IconName::BookmarkCheck
                } else {
                    IconName::Bookmark
                },
                "Bookmark this page (Ctrl+B)",
                marked,
                cx,
                |app, cx| app.toggle_bookmark(cx),
            )
            .into_any_element(),
            icon_button(
                "page-marks",
                IconName::BookMarked,
                "The bookmarked pages (Ctrl+Shift+B)",
                true,
                cx,
                |app, cx| app.show_marks(cx),
            )
            .into_any_element(),
            toolbar_divider(hairline).into_any_element(),
            // The document's own contents: a different subject from the marks — one belongs to the reader,
            // the other to the document — so it stands in its own group, and disabled for a document that
            // has none.
            icon_button(
                "page-contents",
                IconName::ListTree,
                "The document's own contents (Ctrl+Shift+O)",
                self.pdf.has_outline(),
                cx,
                |app, cx| app.show_outline(cx),
            )
            .into_any_element(),
        ];

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .children(controls)
    }

    /// The counters, in a translucent pill on the desk.
    fn status_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.title_bar.opacity(0.92))
            .text_size(px(11.0))
            .text_color(theme.muted_foreground)
            .max_w(relative(STATUS_PILL_WIDTH))
            .child(self.status.clone())
    }

    /// The way back when the bar is hidden.
    fn bar_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .absolute()
            .top(px(BAR_MARGIN))
            .right(px(BAR_MARGIN))
            .flex()
            .flex_row()
            .items_center()
            .px(px(4.0))
            .py(px(4.0))
            .rounded(theme.radius_lg)
            .bg(theme.title_bar)
            .border_1()
            .border_color(theme.title_bar_border)
            .shadow_sm()
            .child(icon_button(
                "show-bar-again",
                IconName::PanelTopOpen,
                "Show the bar",
                true,
                cx,
                |app, cx| app.set_toolbar_shown(true, cx),
            ))
    }
}

impl Render for NoteApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Timed in a scope of its own, and not through `self.timings`: the guard holds a borrow of what it
        // measures, and the rest of this function needs the view mutably. Describing the canvas has a
        // meter of its own (`canvas`).
        let for_render = Arc::clone(&self.timings);
        let _render_timed = measure(&for_render.render);
        // The counters describe one frame, so this frame's are its own.
        self.timings.start_frame();

        // The wait since the last frame, which a frame's own meters cannot see: they measure what a frame
        // *does*, and this is the time between two of them (see [`crate::timing::Session`]). Recorded here
        // because this is the frame clock, and kept whether or not a session is running.
        let now = Instant::now();
        if let Some(previous) = self.last_frame_at.replace(now) {
            self.timings
                .session
                .record_frame(now.saturating_duration_since(previous));
        }

        // A name that was asked for by the bar's button is put up here, on the frame that has a window
        // to put the keyboard in the field with.
        if self.naming_asked {
            self.naming_asked = false;
            self.start_note_name(window, cx);
        }

        // The two sheet choosers are put back in step with the note on the frame that has a window:
        // opening a note changes the paper, the ruling and the zoom without any box being pressed, and
        // the boxes are the only control that changes them back. See [`Self::sync_choosers`].
        self.sync_choosers(window, cx);

        // The keyboard goes with the screen in front, said once here rather than remembered by every
        // transition — opening a note, closing a list, coming back, closing a name field. See
        // [`Self::claim_sheet_keyboard`].
        self.claim_sheet_keyboard(window, cx);

        // The window's own title bar is the one place a person can see, from outside the app, which
        // note they are in — and it is set when it *changes*, not every frame.
        let wanted = if self.home_is_open() || self.note_title.is_empty() {
            String::from("cheap-note")
        } else {
            format!("{} \u{2014} cheap-note", self.note_title)
        };

        if self.window_title != wanted {
            window.set_window_title(&wanted);
            self.window_title = wanted;
        }

        // The scale factor is what turns a physical pen pixel into a logical one, so it is
        // captured before anything that depends on it.
        self.scale = window.scale_factor();

        // The ghost cursor is not in this frame and does not wait for one: what it needs from the
        // frame is this — the screen it is drawn on, published once per frame rather than sampled
        // per reading (see [`crate::cursor_overlay`]).
        self.publish_screen();

        // And the pointer, for the same reason. The pump hands it over on every reading, but the
        // switch that turns the ghost off can be flipped while the pen is away, and a screen can be
        // left while it is away too: no reading will arrive to notice either.
        self.follow_pen_with_pointer();

        let theme = cx.theme();
        let (background, foreground) = (theme.background, theme.foreground);

        let window_size = (
            window.bounds().size.width.into(),
            window.bounds().size.height.into(),
        );

        // The home screen is the whole window while it is up — no sheet, no bar, no counters: a list
        // of what was written is not something to draw ink behind. The sheet's state is left exactly
        // as it is, so a note that is still open is behind the list and one Esc away.
        if self.home_is_open() {
            // Once per frame: the list is handed what the index holds, given the keyboard when the
            // screen has just appeared, and scrolled to the note the app came from.
            self.home.settle(window, cx);
            let home = self.home.view(&self.message, cx);

            return div()
                .relative()
                .size_full()
                .bg(background)
                .text_color(foreground)
                .child(home)
                .into_any_element();
        }

        // The bookmark list is the same kind of thing as the home screen: a screen in front of the note
        // rather than a panel on it, and the sheet's state is left exactly as it is, so Escape from the
        // list puts the note back as it was. See [`crate::bookmarks`] for why it is a screen.
        if self.marks.is_open() {
            // Once per frame: the list is handed the keyboard when the screen has just appeared.
            self.marks.settle(window, cx);
            let marks = self.marks.view(&self.note_title, &self.message, cx);

            return div()
                .relative()
                .size_full()
                .bg(background)
                .text_color(foreground)
                .child(marks)
                .into_any_element();
        }

        // The other list: the document's own table of contents. The same frame, the same kind of screen,
        // and a separate one on purpose — they are two subjects that happen to be drawn alike, and a shared
        // path would tie a change in either to both.
        if self.outline.is_open() {
            self.outline.settle(window, cx);
            let outline = self.outline.view(&self.note_title, &self.message, cx);

            return div()
                .relative()
                .size_full()
                .bg(background)
                .text_color(foreground)
                .child(outline)
                .into_any_element();
        }

        // The canvas's own renderer, called once per frame while the note screen is the one showing
        // (see [`crate::ink_layer`]). The description below is what the layer draws, and the `desk`
        // this element leaves to it is what makes room for it.
        let page = self.current_page();
        let sheet = self.page_layout(window_size, page.as_ref());
        // Stored for the pump, which has no window to ask: a reading has to land on the sheet the
        // user was looking at, and that is this one.
        self.sheet = sheet;

        // The ink follows the sheet it is drawn on: a zoom magnifies the pieces a stroke is made of as well
        // as the stroke, so the outlines are rebuilt when the *detail* rung the zoom asks for moves and not
        // on every frame of a pinch. That also tells the canvas layer its geometry is stale, without any
        // stroke's identity having changed (see [`crate::ink_layer::render`] and `InkDocument::set_zoom`).
        if self.ink.set_zoom(sheet.zoom) {
            self.ink_revision += 1;
        }

        let (page_size, page_origin) = (
            sheet.drawn(),
            point(px(sheet.origin.0), px(sheet.origin.1)),
        );

        let sheet_bounds = Bounds {
            origin: page_origin,
            size: size(px(page_size.0), px(page_size.1)),
        };
        // Built here rather than in a paint callback: a full page of grid lines is several hundred
        // marks, and `Ruling` hands back the same set until the sheet itself changes — which
        // includes its zoom, because the ruling is printed on the paper and grows with it.
        //
        // A PDF page is its own paper, so a blank sheet's ruling has nothing to sit on.
        let rules = page.is_none().then(|| {
            let started = Instant::now();
            let before = self.ruling.rebuilds();
            let rules = self.ruling.rules(
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

            rules
        });

        // The stroke under the pen is closed for the ink description, and where the sheet is: see
        // [`Self::describe_ink`], which the pen's pump calls for the same reason.

        let page_color: Hsla = rgb(self.settings.page_color).into();

        // What the canvas is, described for its own renderer: the desk, the shadow and the sheet, then what
        // is printed on the paper, the document's page, and the ink. The drawing is timed where it happens,
        // in [`Self::draw_canvas`], because the pen's pump draws the canvas too.
        {
            describe_canvas(&mut self.canvas, &sheet, self.scale, page_color, background);

            for rule in rules.iter().flat_map(|rules| rules.iter()) {
                self.canvas.rule(*rule);
            }

            self.canvas.page = page.as_ref().map(|page| Page {
                image: Arc::clone(&page.image),
                rect: Rect {
                    x: sheet.origin.0,
                    y: sheet.origin.1,
                    width: page_size.0,
                    height: page_size.1,
                },
            });

            let visible = sheet.visible();

            // Re-described rather than left to the pump: this is where the sheet it is drawn on was decided.
            self.describe_ink();

            // What the canvas was asked to draw, counted for the status line: the layer draws it and
            // this does not touch a polygon, but the numbers a person reads are the numbers they read
            // before — strokes on the sheet, the outline points they cost, and how many were off it.
            let (mut painted, mut vertices, mut culled) = (0u64, 0u64, 0u64);

            for stroke in self.canvas.ink.strokes.iter() {
                if stroke.visible_in(visible) {
                    painted += 1;
                    vertices += stroke.outline.len() as u64;
                } else {
                    culled += 1;
                }
            }

            if let Some(stroke) = &self.canvas.ink.open {
                painted += 1;
                vertices += stroke.outline.len() as u64;
            }

            self.timings.count_painted(painted, vertices, culled);

            // The same call the pen's pump makes: a frame is what changed the *sheet* — the zoom, the pan,
            // the page, the paper — and the ink is redrawn with it rather than waiting for the next
            // reading (see [`Self::draw_canvas`]).
            self.draw_canvas();
        }

        // The desk belongs to the canvas layer when there is one — those pixels are left to it — and to
        // this element when there is not.
        //
        // Resolved here rather than where the element is built: the theme is a borrow of `cx`, and
        // the bar below is built from `cx` mutably.
        let desk = if self.ink_layer.is_some() {
            theme.transparent
        } else {
            background
        };

        // The pen's ghost cursor is *not* drawn here. It is a position rather than a stroke, and a
        // frame is one frame too late for a position: it has a window of its own, fed straight from
        // the pen thread, and this frame never hears about it — see [`crate::cursor_overlay`].

        // The bar floats over the desk, so pen coordinates need no offset and the ink can run the
        // full height of the window. When it is hidden, the handle that brings it back takes its
        // place — a bar that could be hidden with no way back would be a trap.
        //
        // The bar is not built at all when it is hidden, rather than built and not shown: the status
        // line is the one element whose whole cost is in being built.
        let bar: AnyElement = if self.settings.show_toolbar {
            self.top_bar(cx).into_any_element()
        } else {
            self.bar_handle(cx).into_any_element()
        };

        div()
            .relative()
            .size_full()
            .bg(desk)
            .text_color(foreground)
            // A trackpad's pinch, and a wheel with `Ctrl` held, both arrive here: the canvas is the
            // whole window's only interactive element, so it is what the gestures hit.
            .on_scroll_wheel(
                cx.listener(|app, event: &ScrollWheelEvent, _, cx| app.on_wheel(event, cx)),
            )
            .on_pinch(cx.listener(|app, event: &PinchEvent, _, cx| app.on_pinch(event, cx)))
            // The note's keyboard: Esc for the home screen, the two arrows for the page, and the three
            // commands a person reaches without letting go of the pen. A listener and not a binding,
            // because a *list* of rows wants the same keys (see [`crate::home`]); it is heard because this
            // element is the sheet's, and the sheet holds the keyboard whenever it is the screen in front
            // (see [`Self::claim_sheet_keyboard`]).
            .on_key_down(
                cx.listener(|app, event: &KeyDownEvent, window, cx| {
                    app.note_key_down(event, window, cx)
                }),
            )
            // A name field that has just closed leaves the focus on a handle that is no longer drawn, which
            // is what [`Self::claim_sheet_keyboard`] fixes on the next frame: the handle is tracked here so
            // the focus has somewhere in this element to land.
            .track_focus(&self.sheet_focus)
            // The canvas is not drawn here: it has a surface of its own *behind* this element, and what
            // this element paints is nothing — which is what keeps the desk, the sheet and the ink out of
            // the frame (see [`crate::ink_layer`]).
            .child(bar)
            // The desk's own row: the page in the middle, the counters at the edge. Last, so it
            // paints over the sheet — a page pill *under* the paper would be no pill at all.
            .child(self.bottom_row(cx))
            .into_any_element()
    }
}

/// One of the bar's choosers: a list of options, and the one it starts on.
fn choice(
    labels: &[&'static str],
    selected: Option<usize>,
    window: &mut Window,
    cx: &mut Context<NoteApp>,
) -> Entity<SelectState<Vec<&'static str>>> {
    let items: Vec<&'static str> = labels.to_vec();
    cx.new(|cx| SelectState::new(items, selected.map(IndexPath::new), window, cx))
}

/// Where a value sits in the list a chooser offers it in, as the `Select` wants it.
fn chosen_index<T: PartialEq>(all: &[T], chosen: T) -> Option<IndexPath> {
    all.iter()
        .position(|one| *one == chosen)
        .map(IndexPath::new)
}

/// An icon button that puts a tool in hand.
fn tool_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    active: bool,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, &mut Context<NoteApp>) + 'static,
) -> Button {
    let theme = cx.theme();
    let (wash, accent, idle, hover, pressed, muted) = (
        theme.accent,
        theme.accent_foreground,
        theme.transparent,
        theme.secondary_hover,
        theme.secondary_active,
        theme.muted_foreground,
    );

    let variant = if active {
        ButtonCustomVariant::new(cx)
            .color(wash)
            .hover(wash)
            .active(wash)
            .foreground(accent)
    } else {
        ButtonCustomVariant::new(cx)
            .color(idle)
            .hover(hover)
            .active(pressed)
            .foreground(muted)
    };

    Button::new(id)
        .icon(icon)
        .custom(variant)
        .rounded(px(999.0))
        .compact()
        .selected(active)
        .toggled(active)
        .tooltip(label)
        .on_click(cx.listener(move |app, _, _, cx| handler(app, cx)))
}

/// An icon button for a command: undo, redo, a file, a zoom step.
fn icon_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    enabled: bool,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, &mut Context<NoteApp>) + 'static,
) -> Button {
    Button::new(id)
        .icon(icon)
        .ghost()
        .rounded(px(999.0))
        .compact()
        .disabled(!enabled)
        .accessibility_label(label)
        .tooltip(label)
        .on_click(cx.listener(move |app, _, _, cx| handler(app, cx)))
}

/// A hairline between two groups of controls.
fn toolbar_divider(color: Hsla) -> impl IntoElement {
    div().w(px(1.0)).h_5().bg(color).mx_1()
}

/// A small muted caption in front of a group of controls.
fn control_label(text: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .text_size(px(10.0))
        .text_color(color)
        .mr_1()
        .child(text)
}

/// A switch for one of the bar's two visibility options.
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

/// One colour swatch: a filled shape, ringed when it is the colour in use.
fn swatch_button(
    swatch: &Swatch,
    in_use: bool,
    ring: Hsla,
    circle: bool,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, u32, &mut Context<NoteApp>) + 'static,
) -> impl IntoElement {
    let theme = cx.theme();
    let color = swatch.color;

    // A white or ivory chip on a white bar would have no edge of its own, so a light colour is given
    // a hairline; a dark one is its own edge and is given nothing.
    let edge = if relative_luminance(color) > 0.85 {
        theme.border
    } else {
        theme.transparent
    };

    let mut chip = div()
        .w(px(16.0))
        .h(px(16.0))
        .bg(rgb(color))
        .border_1()
        .border_color(edge);

    let mut button = div()
        .id(swatch.id)
        // A square of colour says nothing to a screen reader, and nothing to anyone who cannot tell
        // "ivory" from "white" by eye. The name is what both need to hear or see.
        .aria_label(swatch.name)
        .p(px(2.0))
        .border_2()
        .border_color(if in_use { ring } else { theme.transparent })
        .cursor_pointer()
        .on_click(cx.listener(move |app, _, _, cx| handler(app, color, cx)));

    if circle {
        chip = chip.rounded_full();
        button = button.rounded_full();
    } else {
        chip = chip.rounded(px(4.0));
        button = button.rounded(px(6.0));
    }

    button.child(chip)
}

#[cfg(test)]
mod tests {
    // Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into
    // scope and shadow the attribute this module needs.
    use super::{
        file_label, file_stem, notch_in_pixels, pdf_render_due, pinch_zoom_factor, quantise_width,
        sheet_matches, wheel_pan, wheel_zoom_factor, PDF_QUIET_INTERVAL, WHEEL_LINE_HEIGHT,
        WHEEL_LINES_PER_NOTCH, WHEEL_ZOOM_STEP,
    };
    use std::time::{Duration, Instant};

    /// One notch of a wheel is one zoom step, however many lines the platform reports it as.
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

    /// Two notches are the step applied twice rather than twice the step, so a fast roll zooms smoothly.
    #[test]
    fn two_notches_are_two_steps_and_not_a_doubled_one() {
        let twice = wheel_zoom_factor(WHEEL_LINES_PER_NOTCH * 2.0, false);

        assert!((twice - WHEEL_ZOOM_STEP * WHEEL_ZOOM_STEP).abs() < 1e-6);
        assert!(twice < WHEEL_ZOOM_STEP * 2.0, "a roll is not a multiplication");
    }

    /// A trackpad reports pixels, and the same distance zooms the same either way it is reported:
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

    /// Panning is a distance in logical pixels, in the direction of the scroll.
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

    /// A page's bitmap width is quantised onto the ladder, so a pixel of zoom does not throw away a
    /// bitmap that cost milliseconds to make.
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

    /// The whole zoom range is covered by a handful of rasterisations, which is the point of the ladder.
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

    /// A name a person gave a note becomes a file name, with what Windows refuses turned into dashes.
    #[test]
    fn a_note_s_name_becomes_a_file_name() {
        assert_eq!(file_stem("3\u{c7a5} \u{c694}\u{c57d}"), "3\u{c7a5} \u{c694}\u{c57d}");
        assert_eq!(file_stem("  chapter   3  "), "chapter 3");
        assert_eq!(file_stem("a/b\\c:d*e?f\"g<h>i|j"), "a-b-c-d-e-f-g-h-i-j");
        assert_eq!(file_stem("report."), "report", "a trailing dot is dropped");
        assert_eq!(file_stem("..."), "", "and a name that was only dots is nothing");
        assert_eq!(
            file_stem("\u{2026}"),
            "\u{2026}",
            "a character a file system is happy with is left alone"
        );
        assert_eq!(file_stem(""), "");
        assert_eq!(
            file_stem(&"\u{c7a5}".repeat(80)).chars().count(),
            64,
            "a long name is cut by characters, so a Korean one is not halved"
        );
    }

    /// A page is rasterised when the pen stops, not while it is writing.
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

}

/// The shadow a sheet casts on the desk: spread, offset beyond the spread, and colour, widest first.
const PAGE_SHADOW: [(f32, f32, u32); 3] = [
    (12.0, 4.0, 0x0000_0008),
    (7.0, 2.5, 0x0000_000C),
    (3.0, 1.0, 0x0000_0014),
];

/// Describes a frame's canvas: the desk, the page's shadow, and the sheet — in the order painted.
fn describe_canvas(canvas: &mut Canvas, sheet: &Sheet, scale: f32, paper: Hsla, desk: Hsla) {
    canvas.clear();
    canvas.desk = desk;
    canvas.scale = scale;

    let (width, height) = sheet.drawn();
    let (x, y) = sheet.origin;

    // The sheet's own rectangle, which is what the layer clips the ink to: the display half of the
    // rule the ink model enforces on every reading (see [`crate::ink::InkTransform::on_paper`]).
    canvas.sheet = Some(Rect {
        x,
        y,
        width,
        height,
    });

    for (spread, drop, tint) in PAGE_SHADOW {
        canvas.fill(
            Rect {
                x: x - spread,
                y: y - spread + drop,
                width: width + spread * 2.0,
                height: height + spread * 2.0,
            },
            rgba(tint).into(),
        );
    }

    canvas.fill(Rect { x, y, width, height }, paper);
}

/// How much a scroll zooms, as a factor to multiply the current zoom by.
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
fn notch_in_pixels() -> f32 {
    WHEEL_LINE_HEIGHT * WHEEL_LINES_PER_NOTCH
}

/// How much a scroll pans, in logical pixels.
fn wheel_pan(delta: (f32, f32), in_pixels: bool) -> (f32, f32) {
    let scale = if in_pixels { 1.0 } else { WHEEL_LINE_HEIGHT };

    (delta.0 * scale, delta.1 * scale)
}

/// How much a pinch zooms, as a factor to multiply the current zoom by.
fn pinch_zoom_factor(delta: f32) -> f32 {
    if !delta.is_finite() {
        return 1.0;
    }

    (1.0 + delta).clamp(0.2, 5.0)
}

/// The smallest width on [`PDF_PIXEL_WIDTHS`] that is at least `wanted`, or the largest one.
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
