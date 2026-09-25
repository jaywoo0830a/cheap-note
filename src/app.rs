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
//! Nothing here polls for pen input. The pen thread pushes readings into a queue, and an async task
//! (see [`NoteApp::start_pen_pump`]) *waits on that queue* — not on a timer — takes every batch the
//! instant it lands, feeds the ink model, and calls `cx.notify()`. A frame is therefore scheduled
//! exactly when there is something new to show, and at the rate the pen reports it: 133 Hz, 240 Hz,
//! whatever the digitizer sends, with no ceiling taken from any display. The app is idle, and
//! spends nothing, otherwise.
//!
//! "Something new" is two things: ink that changed, and a cursor that moved. A pen held in range
//! without touching lays no ink at all, and it is the ghost cursor (see [`crate::cursor`]) that has
//! to follow it.
//!
//! A second task ([`NoteApp::start_display_pump`]) does the work that is not the ink — rasterising
//! the page, rebuilding the counters — on a fixed [`HOUSEKEEPING_INTERVAL`] that owes nothing to
//! the display, so that a rasterisation can never delay a stroke.
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
//! printed on it. A PDF page overrides all three, because a PDF page is its own paper. All of it —
//! the size, the colours, the ruling, the ink's colour, the pen's weight, and the zoom — belongs to
//! the *note* and is kept in it (see [`crate::settings`]), so opening a note opens the sheet it was
//! written on, written with the pen it was written with.
//!
//! ## Where the ink goes
//!
//! Nothing waits for a *Save*. A note is a folder with a SQLite file in it — see [`crate::note`] and
//! [`crate::store`] — and ink is written into it in batches while the pen is moving: 200 strokes or
//! half a second, whichever comes first, on a thread of its own, so a commit never delays a stroke
//! and a crash costs at most the last half-second of ink. The first stroke of a session makes the
//! note if one is not open; the page being turned away from is folded into compressed chunks as it
//! closes; the write-ahead log is folded back into the file when the pen has been still for a while;
//! and `Save` writes out the single file a person carries to another machine.
//!
//! Reading is the other half of the same arrangement. A note is opened *without* being read: the app
//! loads the page that was open and the note's page list, and every other page is read the moment it
//! is turned to. A thousand-page note opens as fast as a one-page note.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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

use crate::canvas::{
    contrast_color, relative_luminance, CanvasSize, CanvasStyle, Ruling, Swatch, INK_COLORS,
    PAPER_COLORS,
};
use crate::cursor::{
    PenCursor, BODY_ALPHA, BODY_HALO_ALPHA, BODY_HALO_GROW, NIB_ALPHA, NIB_BLOOM_ALPHA,
    NIB_BLOOM_RADIUS, NIB_RADIUS,
};
use crate::home::Home;
use crate::ink::{InkTransform, Notes, Stroke, Tool};
use crate::note::{self, Note, NoteWriter, Report};
use crate::pages::Pages;
use crate::pen::{capture_config, PenInbox, PenService};
use crate::pdf::{PageRequest, PdfDocumentView, Progress, RenderedPage};
use crate::recent::{self, Recent};
use crate::settings::{PenWeight, Settings};
use crate::system_cursor::SystemCursor;
use crate::timing::{measure, Timings};
use crate::view::{Fit, Viewport};

// The two commands a keyboard reaches that the bar also carries.
//
// They are actions — GPUI's unit of a keyboard command — rather than key listeners, because a
// *binding* is then what decides which keys mean them (see the `KeyBinding`s in `main`) and this app
// never has to parse a keystroke itself. The bar's buttons and the keyboard both end in the same two
// methods, so there is one implementation of undo and one of redo and no way for the two paths to
// disagree.
//
// This matters more here than it would in a text editor: the bar can be hidden, and without a
// keyboard command an app with the bar off would have no way to take a stroke back at all.
actions!(cheap_note, [Undo, Redo]);

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

/// How far the floating bar and the floating page pill sit from the window's edges.
///
/// The desk shows in that gap, which is what makes the sheet read as paper on a desk rather than as
/// a rectangle filling a window — and the bar floats for a second reason too: an overlay takes no
/// part in the layout, so a pen reading needs no offset arithmetic (see the module docs).
const BAR_MARGIN: f32 = 12.0;

/// How tall the top bar is, in logical pixels, as an estimate.
///
/// Used for exactly one decision: whether the pen is over the bar, where the ghost cursor is drawn
/// behind the bar's opaque background and the system pointer therefore has to stay. The estimate is
/// the bar at its tallest — its margin, its padding and two rows of controls, the second of which
/// may have wrapped — and it errs upward deliberately: being wrong upward leaves a strip of sheet
/// still showing the pointer (a small surprise), while being wrong downward would leave part of the
/// bar with no cursor at all, and the pen is how those controls get clicked.
const BAR_HEIGHT: f32 = 148.0;

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

/// How often the housekeeping pump wakes.
///
/// A fixed floor rather than a rate taken from the display: nothing in the loop reads the monitor or
/// paces the ink any more, so there is no clock to follow and no reason for the interval to move.
/// One millisecond is the shortest wait that still parks the task rather than spinning it, and the
/// work the loop does is skipped while the pen is laying ink, so waking this often costs a timer and
/// a comparison when there is nothing to do.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(1);

/// How long the pen has to be quiet before a page is rendered for it.
///
/// A slice of a render blocks the thread that runs the pump, so while a stroke is being laid the
/// rung already on screen is the right one; the sharp one can wait for the hand to stop. An eighth
/// of a second is short enough that a person pausing to think sees it land, and long enough that the
/// pause between two letters of a word is not mistaken for one.
const PDF_QUIET_INTERVAL: Duration = Duration::from_millis(120);

/// How long ink may wait in memory before it is handed to the note.
///
/// This is the design note's clock: the pen's path never writes a file, it *batches*, and half a
/// second is both the most ink a crash can cost and short enough that a person who closes the lid
/// has lost nothing they would notice.
const BATCH_INTERVAL: Duration = Duration::from_millis(500);

/// How much ink may wait in memory before it is handed over early.
///
/// The other half of the same rule: a fast hand lays more than a page's worth of ink in half a
/// second, and a batch that grew without a bound would be a transaction that takes longer to commit
/// than the moment it was meant to save.
const BATCH_STROKES: usize = 200;

/// How long the pen has to be still before the write-ahead log is folded back into the note.
///
/// A checkpoint is a write of its own, and the one moment a log must not be truncated is while the
/// pen is moving: that log *is* the ink that has not reached the file yet.
const CHECKPOINT_QUIET_INTERVAL: Duration = Duration::from_secs(5);

/// The least time between two checkpoints, however quiet the pen has been.
///
/// The design note's five minutes: long enough that a session's worth of strokes is folded back in
/// one write, short enough that a note does not sit next to a log of itself for a whole day.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(300);

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

/// What a note's name can be as far as a *file* name goes.
///
/// A name is free text — a person may type a slash, a colon, a question mark, or nothing at all — and a
/// file name is not: the characters Windows refuses become dashes, a run of whitespace becomes one
/// space, and a dot or a space at either end goes, because Windows quietly drops those itself and a name
/// that comes back different from the one that was typed is worse than one that is visibly tidied.
///
/// Long names are cut to 64 *characters*, not bytes: a Korean name is three bytes a syllable, and
/// cutting by bytes would halve it and split the last one.
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
///
/// A free function for the same reason [`status_due`] is: it is a decision, and decisions that can
/// be tested without a window are worth testing without one.
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
    ///
    /// The paper travels with it: the pump has to be able to refuse a reading that landed on the desk
    /// beside the page, and the paper's edges are what "beside" means (see [`InkTransform::on_paper`]).
    fn transform(&self, scale: f32) -> InkTransform {
        InkTransform {
            scale,
            zoom: self.zoom,
            origin: self.origin,
            paper: self.paper,
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
    /// Everything the note in hand remembers, in memory: its paper, its pen, its switches, and how the
    /// pen feels.
    ///
    /// There is no global half — this is the whole of it, it belongs to the note that is open, and it is
    /// read out of that note when one is opened and written back into it when anything changes (see
    /// [`crate::settings`] and [`Self::save_note_state`]). With no note open it is the shipped set: what
    /// a blank sheet starts as, and what the first note made from one is told.
    settings: Settings,
    /// The ink.
    ink: Notes,
    /// The note being written: its folder, its database, and the document it was written on.
    ///
    /// `None` until something is opened or made: the app starts on a blank sheet, which is a note
    /// that does not exist until it is saved.
    note: Option<Note>,
    /// The note's writer: its own connection, on its own thread.
    ///
    /// Held beside [`Self::note`] rather than inside it, because the two are two connections to one
    /// file — a reader and a writer, which is what WAL is for (see [`crate::note`]).
    writer: Option<NoteWriter>,
    /// How many strokes of each page have been handed to the writer as *appends*.
    ///
    /// This is what makes a save incremental: the ink not yet sent is `finished()[sent..]`, and a
    /// count that went *down* means the page was undone, erased or cleared — a rewrite, not an
    /// append.
    saved: BTreeMap<u64, usize>,
    /// Pages whose stored ink no longer matches the page in memory, waiting to be written again.
    rewritten: BTreeSet<u64>,
    /// When a batch was last handed over, for the 500 ms rule.
    saved_at: Instant,
    /// When the write-ahead log was last folded back into the note file.
    checkpointed_at: Instant,
    /// The pages whose ink is in memory, or known to be empty.
    ///
    /// What keeps a page from being read from the note twice, and what makes it safe for a turn to
    /// drop a blank page: a page that is blank in memory is blank in the note.
    loaded: BTreeSet<usize>,
    /// What each page of the note shows, in reading order.
    ///
    /// The page the reader is on is [`NoteApp::page_index`], which indexes *this* list rather than
    /// any document's pages: a page can be inserted or deleted, and after that the two are not the
    /// same thing. See [`crate::pages`].
    pages: Pages,
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
    /// What the note on the sheet is called — its own name, or one the list would derive.
    ///
    /// Kept rather than asked for every frame: the note's name lives in the database (see
    /// [`crate::store::META_TITLE`]), and a read per frame is a read per frame. It is set when a note
    /// is opened and when one is renamed.
    note_title: String,
    /// The window's title as last set, so that it is set when it *changes* rather than every frame.
    window_title: String,
    /// The folder whose name is being typed on the sheet, if any.
    ///
    /// The *folder* rather than the note, for the same reason the list keeps the folder: it is what the
    /// note is called in its own database (see [`crate::store::META_TITLE`]).
    naming: Option<PathBuf>,
    /// Whether a name has been asked for, and the field has not been put up yet.
    ///
    /// The bar's button has no window to focus a field with, so the ask is kept and carried out on the
    /// frame that has one — the same arrangement the list uses for its own field.
    naming_asked: bool,
    /// The field the name is typed into on the sheet.
    name_input: Entity<InputState>,
    /// The keyboard the sheet gets back when the field goes away.
    ///
    /// A screen with nothing focused still hears the keys it paints (see [`Self::note_key_down`]), but a
    /// *field* that has just gone away is a window that thinks something is still focused — so finishing
    /// a name puts the focus somewhere real.
    sheet_focus: FocusHandle,
    /// What the field being typed into says — Enter keeps the name, a click away leaves it as it was.
    ///
    /// Held rather than dropped: dropping a subscription is how a listener stops listening.
    _name_events: Subscription,
    /// The bar's paper-size chooser.
    ///
    /// A `Select` rather than a row of buttons, and it is the *only* thing that changes
    /// [`crate::settings::Settings::canvas_size`]: the choice comes back as the label that was
    /// showing, which [`CanvasSize::from_label`] turns into a size, so the box and the style cannot
    /// drift apart. Opening a note *does* move this box, because the paper travels with the note —
    /// see [`Self::sync_choosers`].
    sheet_select: Entity<SelectState<Vec<&'static str>>>,
    /// The bar's ruling chooser. Held for the same reason as [`NoteApp::sheet_select`].
    rule_select: Entity<SelectState<Vec<&'static str>>>,
    /// The bar's pen-weight chooser: the pen the note in hand is written with.
    ///
    /// A chooser rather than a row of buttons for the same reason the size and the ruling are: the
    /// alternatives have names, and a row of marks a person has to compare by eye is a row a person
    /// has to guess at. It is also the control that moved the ink's *width* where the ink's colour
    /// already was — into the note (see [`crate::settings`]).
    pen_select: Entity<SelectState<Vec<&'static str>>>,
    /// The screen that offers what was opened recently, and the app starts on.
    ///
    /// Held here rather than as a view of its own because the window *is* the app: a second screen
    /// is a state of this one, and swapping a whole root entity out of a window would take the pen
    /// capture and the pumps with it. See [`crate::home`].
    home: Home,
}

impl NoteApp {
    /// Builds the view, attaches the pen, and starts the frame loop.
    ///
    /// `start` is a path the app was asked to open — a file argument, which is what a file
    /// association or a drag onto the executable becomes. Opening it here rather than on the first
    /// frame means the very first frame already shows it, and the home screen is never seen.
    pub fn new(window: &mut Window, start: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        // There is nothing to load, and nowhere to load it from: every setting a person can change
        // belongs to a note, and no note is open yet. This is the shipped set — and it is not a
        // *global* set: whatever is opened or made below takes it over, and from then on these answers
        // live in that note (see [`crate::settings`] and [`Self::adopt`]).
        let settings = Settings::default();
        let mut message = String::new();

        // The capture must be attached on the thread that owns the window, which is this one.
        let pen = PenService::attach(window, capture_config());

        // Installed after the capture, so this hook runs first in the subclass chain and gets to
        // answer `WM_SETCURSOR` before anything else can put a cursor back.
        let system_cursor = SystemCursor::install(window);

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
            ruling: Ruling::default(),
            status: String::new(),
            status_at: Instant::now(),
            message,
            note_title: String::new(),
            window_title: String::new(),
            naming: None,
            naming_asked: false,
            name_input,
            sheet_focus,
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

        // The keyboard's way to the same two commands the bar's buttons run. Registered as *global*
        // action handlers, which GPUI runs after the focused element's own handlers: the app has no
        // focusable canvas and no text field, so a keystroke has no other node to go to, and a
        // binding that only worked while some element happened to hold focus would be a command that
        // works on some days.
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
    ///
    /// ## Why two
    ///
    /// They answer different questions at different rates, and separating them is what takes the
    /// ceiling off the first:
    ///
    /// * the **ink pump** parks on the pen's queue and wakes the instant a batch lands, so there is a
    ///   frame per batch — the pen's own rate, 133 or 240 Hz or whatever it reports — and nothing at
    ///   all is spent while the pen is away. This is the loop the hand feels: its wake is where a
    ///   reading becomes a frame, and it is what "unlimited" means here.
    /// * the **housekeeping pump** runs on a fixed [`HOUSEKEEPING_INTERVAL`] and does everything that
    ///   is *not* the ink: rasterising the page the view is waiting for, and rebuilding the counters.
    ///   A rasterisation blocks whichever thread runs it, and running it here rather than in the ink
    ///   pump is what keeps a sharp page from ever delaying a stroke.
    ///
    /// Both end when the view is dropped — `update` returns `Err` once the entity is gone.
    fn start_pumps(&mut self, cx: &mut Context<Self>) {
        self.start_pen_pump(cx);
        self.start_display_pump(cx);
    }

    /// Drains the pen queue whenever it has something in it, and repaints when the ink changed.
    ///
    /// There is no timer: `PenInbox::wait` parks this task on the queue itself, so the pump is woken
    /// by readings rather than by a clock. A display-paced pump put a ceiling on how often a reading
    /// could reach the screen, and the ceiling was invisible from the outside — it looked like the
    /// pen's own rate — so it is gone rather than made configurable.
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

                // The home screen is in front of the sheet, and the pen is captured by the
                // *window* rather than by anything on it: a reading that reached the ink while a list
                // of notes was showing would be ink written on a page nobody is looking at. The
                // capture itself is left alone — nothing else can own the pen while the app runs —
                // and the readings are dropped here, where they would otherwise become strokes.
                if app.home_is_open() {
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
                    // And the note: the ink is handed to the writer in batches rather than written
                    // per stroke, on the clock the design note sets. Nothing waits for it.
                    app.persist(woke, false);
                }

                // The system pointer follows the ghost on every read, so the two can never disagree
                // about whether the pen has a cursor of its own.
                app.follow_pen_with_pointer();

                if !laid_ink && !cursor_moved {
                    // Nothing on screen changed: a reading the resampler and the pointer gate both
                    // dropped, or a hover that moved nothing. Repainting an identical scene on each
                    // of those is what made the top of the window look like it was flickering.
                    return;
                }

                // The counters have moved too, but the line is rebuilt on a slow clock by the
                // housekeeping pump: re-shaping text is the reader-visible half of the same problem,
                // and a frame drawn to show ink does not have to carry it.
                cx.notify();
            });

            if alive.is_err() {
                // The view is gone; the app is closing.
                break;
            }
        })
        .detach();
    }

    /// Keeps the counters and the page being rasterised up to date.
    ///
    /// It runs on a fixed [`HOUSEKEEPING_INTERVAL`] rather than on the display's clock: nothing here
    /// reads the monitor or paces the ink, so the interval is a constant that never moves. It
    /// repaints only when one of the things it watches actually changed — a frame of its own is
    /// cheap, a frame that draws an identical scene is not.
    fn start_display_pump(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(HOUSEKEEPING_INTERVAL).await;

            let alive = this.update(cx, |app, cx| {
                let now = Instant::now();

                // The page the view is waiting for, paid for here rather than inside a frame: it is
                // skipped while the pen is laying ink, so the ink pump keeps taking readings.
                let page_rendered = app.serve_pdf(now);

                // The status counters have moved; the *line* is rebuilt only when it is due, and
                // the answer says whether it was.
                let status_rebuilt = app.refresh_status(now);

                // The note's own housekeeping, on the same clock: ink still in memory is handed
                // over when the pen has stopped (see `persist`), and whatever the writer has to say
                // — an export finished, or a write that failed — reaches the status line here.
                app.persist(now, false);
                let reported = app.drain_writer_reports();
                app.checkpoint_if_idle(now);

                if page_rendered || status_rebuilt || reported {
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


    /// Reads a batch of readings into the ink model, timed.
    ///
    /// The transform comes from the last frame's sheet rather than from the window: a reading
    /// belongs on the sheet the user was looking at when the nib moved.
    fn consume_ink(&mut self, samples: &[pen_windows::PenSample]) -> bool {
        // The pen is not a pen while something is in front of the sheet: a name being typed must not
        // leave a line across the page behind the field, and the list of recent notes is drawn over a
        // sheet nobody can see — a stroke laid there would go into a note the user is not looking at.
        if self.naming.is_some() || self.home_is_open() {
            return false;
        }

        let transform = self.sheet.transform(self.scale);
        let _timed = measure(&self.timings.ink);

        self.ink.consume(samples, &transform, &self.settings)
    }

    /// Hands the ink the pen laid to the note, on the design note's clock.
    ///
    /// Three rules, and each of them is a page of the design note:
    ///
    /// * **A batch, not a write per stroke.** The ink goes out when [`BATCH_STROKES`] strokes are
    ///   waiting or [`BATCH_INTERVAL`] has passed, whichever comes first — one transaction, one BLOB
    ///   a stroke, and nothing that already written is rewritten.
    /// * **A page whose ink went *down* is written again, not appended to.** An undo, the eraser and
    ///   `clear` all shorten a page, and an append can only describe ink being *added*: those pages
    ///   are marked and written whole instead.
    /// * **Nothing waits.** The write happens on the writer's thread (see [`NoteWriter`]), so a
    ///   half-second batch commit is invisible while the pen is moving.
    ///
    /// `worth_flushing` is for the moments the ink has to be in the note *now* rather than on the
    /// clock: a page being closed, or a note being written out as a file.
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
    ///
    /// The app starts on a blank sheet, which is not a note yet — a note is a folder with a database
    /// in it, and there is no reason to make one for a session that never writes anything. The first
    /// stroke is what makes one, and from then on the ink is written as it is laid, so nothing is
    /// ever held *only* in memory: `Save` writes out a file that has been the note all along.
    ///
    /// The note has no document, which is what a note written on a blank sheet is, and its folder is
    /// named after the moment it was made so that two blank-sheet notes in one session — a note that
    /// was cleared and written on again, say — are two notes and not one that overwrites the other.
    fn ensure_note(&mut self) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);

        let mut note = Note::open(&note::root().join(format!("blank-{stamp}")))?;

        // What the note remembers about itself: the page list, the sheet the ink is written on, every
        // setting in hand, and which page is open — the same facts a note with a document carries. The
        // settings come from the app's live state, which is why a blank page continues the sheet that was
        // open before it.
        let layout = self.pages.layout().to_vec();
        note.store_mut().set_layout(&layout)?;
        note.store_mut().set_sheet(Some(self.sheet_size()))?;
        note.store_mut().set_settings(&self.settings)?;
        note.store_mut().set_open_page(self.page_index as u64)?;

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
    ///
    /// The design note's periodic `wal_checkpoint(TRUNCATE)`. WAL is what keeps a write from blocking
    /// a read, at the cost of a log beside the note, and the log is worth folding back when nothing
    /// is happening — never while the pen is moving, because that log *is* the ink that has not
    /// reached the file yet. The job goes to the writer's thread like any other, so even this write
    /// happens off the UI thread.
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
    ///
    /// Drained on every wake of the housekeeping pump rather than waited for: the writer reports an
    /// export when it has finished one, and a failure whenever it has one — and a writer that failed
    /// silently would be the worst of both worlds, ink that is not being saved and an app that looks
    /// like it is.
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

    /// Changes how heavy the pen is.
    ///
    /// What is already on the page is untouched: a stroke keeps the width it was drawn at, exactly as
    /// it keeps its colour, because the width is stamped into every point as the nib moves (see
    /// [`crate::ink`]). So this is what the *next* line is laid with, and choosing a marker on a page
    /// of fine writing changes nothing about the page.
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
    /// where it cannot be seen. Never on the home screen, which has no sheet to point at and is
    /// meant to be tapped: a hidden system pointer with nothing drawn in its place is a list no
    /// pen can click.
    fn pen_has_its_own_cursor(&self) -> bool {
        if self.home_is_open() {
            return false;
        }

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

    /// Persists and repaints after anything changed.
    ///
    /// One write covers all of it, because there is only one place for it to go: every setting — the
    /// paper, the pen, the switches, and how the pen *feels* — belongs to the note that is open, and
    /// [`Self::save_note_state`] puts them there (see [`crate::settings`]).
    ///
    /// The ruling is keyed on the sheet's rectangle, its style and its paper colour, so it
    /// discards itself on the next frame without being told. Only the status line has to be
    /// rebuilt, and it is rebuilt here rather than on the clock because the user is looking for
    /// the change they just made.
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
    ///
    /// The page's history is what decides whether there is anything to take back — see
    /// [`crate::ink::InkDocument::undo`] — and the bar asks the same question before it offers the
    /// button, so a stroke is only ever taken back by a command that said it could. The note is told
    /// at once rather than on the batch clock: an undo is a deliberate act, and leaving it in memory
    /// for half a second is how it is lost if the app is closed in that half-second.
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
    ///
    /// The ink moves with the page — `go_to` takes the page being left behind with it, and the page
    /// being turned to is read out of the note if it is not in memory yet — which is what keeps a
    /// note on the sheet it was written on rather than on whichever sheet is shown next.
    fn previous_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index > 0 {
            self.turn_to(self.page_index - 1);
            cx.notify();
        }
    }

    /// Shows the next page.
    ///
    /// The page count is the note's own list — see [`crate::pages`] — which for a note written on a
    /// document is the document's pages and for one written on blank sheets is as many as have been
    /// made. Either way an inserted page is a page like any other.
    fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.page_index + 1 < self.page_total() {
            self.turn_to(self.page_index + 1);
            cx.notify();
        }
    }

    /// Turns to a page: what is being left is written out, and what is being turned to is read in.
    ///
    /// The order is the whole of it. The page being left is closed first — its outstanding ink is
    /// handed to the writer and folded into chunks, which is the design note's "compaction at page
    /// close" — and only then is the page being turned to read, so a page's ink is never in two
    /// places at once.
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
    ///
    /// Nothing waits for any of it. The batch is handed to the writer's thread and the compaction is
    /// queued behind it, in that order, so the ink is in the note before the chunks that follow it.
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
    ///
    /// A blank page rather than a copy of the neighbouring one: an inserted page is paper to write
    /// on, and duplicating a page is a different command (which this app does not have). Turning to
    /// it is not a courtesy — a page that was just made is the page that is about to be written on,
    /// and leaving the reader on the old one would make the button look like it did nothing.
    fn add_page(&mut self, before: bool, cx: &mut Context<Self>) {
        self.close_page();

        let at = self.pages.insert(self.page_index, before);
        self.ink.insert_at(at);

        // The note's ink is renamed with its pages: `insert_page` moves every page after the
        // insertion along, and the ink moves with the sheet it was written on. What has been handed
        // to the writer is renamed with them, so the next append is still measured against the right
        // page.
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
    ///
    /// The ink goes with the page: there is nowhere to show it afterwards, and keeping it would
    /// mean keeping an identity for "the page that used to be here" that no later page could be
    /// confused with. On a note about a document the page leaves the *note*, not the file — this app
    /// has no PDF writer — and that is stated in [`crate::pages`] rather than left to be discovered.
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
        self.page_index = show;
        self.turn_to(show);
        self.save_note_state();

        self.report(format!(
            "deleted a page ({} left)",
            self.page_total().saturating_sub(1).max(1)
        ));
        cx.notify();
    }

    /// Renames the pages from `from` on by `by`, in what the app remembers about the note.
    ///
    /// The note's own database does this itself (see [`crate::store::NoteStore::insert_page`]); what
    /// is mirrored here is the app's side of the same bookkeeping — which pages have been read, and
    /// how much of each has been written — because a page that was read before an insertion is no
    /// longer the page it was read as.
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

    /// Stores what the note remembers about itself: its page list, its sheet, every setting in hand,
    /// and which page is open.
    ///
    /// Written on the commands that change any of them — an insert, a delete, a paper choice, a colour,
    /// a pen, a zoom, a switch — rather than on a clock: these are the answers the next session reads
    /// back, and they change at the speed of a hand.
    ///
    /// This is where the app's live state becomes the note's stored state, and the only place it does:
    /// [`crate::settings::Settings`] is the whole of what a note remembers, and it is handed over whole
    /// (see [`crate::store::NoteStore::set_settings`]).
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
    ///
    /// Told apart by the file rather than by a menu of two commands: a note is a zip, and making the
    /// user remember which of two dialogs to pick would be asking them to keep track of this app's
    /// internals for it. A folder is opened where it stands.
    fn open_any(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match self.open_or_place(&path) {
            Ok(message) => self.message = message,
            Err(error) => self.report(error.to_string()),
        }

        self.touch_status();
        cx.notify();
    }

    /// Opens what was chosen, preferring the note that already exists over placing a new one.
    ///
    /// **This is the difference between opening a PDF again and losing a note.** Placing a PDF starts
    /// a note on that document — a *new* one, written over the working copy of the same name (see
    /// [`Note::placed_from`]) — so someone who opens the PDF they have been writing on twice would,
    /// without this, find their ink gone the second time. A file whose note is already there opens
    /// *that note*, and the document inside it is the one it was made from.
    ///
    /// The other half of that choice — writing a second, fresh note on the same document — is not
    /// offered yet: the message says which note was opened, so it is not a silent substitution.
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
    ///
    /// The ink is deliberately *not* all read here. A note is opened by reading the page that was
    /// open and the note's own page list; every other page is read when it is turned to, which is
    /// what the chunked store is for — see [`crate::store`]. A thousand-page note opens as fast as a
    /// one-page note.
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
    ///
    /// `source` is the file the note was made from, when the caller knows it: it is what the app
    /// says this note is, and what is remembered about where it came from. Nothing about the ink
    /// depends on it — the document is inside the folder.
    fn open_folder(&mut self, folder: &Path, source: Option<&Path>) -> Result<String> {
        let note = Note::open(folder)?;
        self.adopt(note, source)
    }

    /// Takes over an open note: its writer, its document, its pages, the page that was open, and how
    /// the note is written on.
    fn adopt(&mut self, mut note: Note, source: Option<&Path>) -> Result<String> {
        let folder = note.dir().to_path_buf();
        let label = source
            .map(Path::to_path_buf)
            .unwrap_or_else(|| folder.clone());

        // Every setting in hand is replaced by the note's own — read *before* the document is opened and
        // before the viewport is built, because the paper's width is what a page is rasterised at (see
        // [`Self::pdf_render_pixel_width`]) and the zoom is where the reader was.
        //
        // [`crate::store::NoteStore::read_settings`] fills in only what the note actually says, so a note
        // that has never been told anything keeps the sheet and the pen already up: that is how a blank
        // page, or a PDF just placed, continues what is in hand instead of snapping back to the shipped
        // set. The whole set is then written straight back, so the note is told once and for all — from
        // here on these answers travel with *it* and not with whoever was written in last.
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
        let open_page = note.store().open_page()?.unwrap_or(0) as usize;

        self.pdf = pdf;
        self.pages = Pages::restore(
            (!layout.is_empty()).then_some(layout),
            self.pdf.page_count(),
            ink_pages,
        );
        self.page_index = self.pages.clamp(open_page);

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
    ///
    /// The page is put where the model keeps a page — the current one if it is the current one, in
    /// the map otherwise — and marked loaded, so turning back to it is free. A page that is blank in
    /// memory is blank in the note, which is what makes it safe for a page turn to drop one.
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
    ///
    /// Written through the app's own connection rather than the writer's: this is one small update on
    /// a page turn, not a stream of ink, and WAL is what makes two connections to one note safe.
    fn remember_page(&mut self) {
        if let Some(note) = &mut self.note {
            let _ = note.store_mut().set_open_page(self.page_index as u64);
        }
    }

    /// Whether the home screen is in front of the sheet.
    fn home_is_open(&self) -> bool {
        self.home.is_open()
    }

    /// Shows the home screen: where the app starts, and where Esc goes from a note.
    ///
    /// Nothing can be lost by leaving a note — the ink is in it as it is laid (see [`crate::store`])
    /// — so this is a page close and nothing more: the outstanding ink goes to the writer, the page
    /// is folded into chunks, and the sheet stays where it is behind the list. The note being left is
    /// where the list starts: the way back to what was open is the first thing a person looks for.
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
    ///
    /// A scan costs a `stat` per note and a database read only for the notes that have changed since
    /// the list was written (see [`crate::home::scan`]) — and it runs on a worker thread, so the
    /// first frame is on screen while it happens.
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
    ///
    /// The list is this screen's keyboard — arrows, Enter, Escape, and typing into its search box —
    /// and it has the focus, which [`Home::settle`] gives it on the frame after the screen appears.
    /// What is left here is what the list cannot know: the commands that belong to the app wherever it
    /// is. Everything a *row* can do beyond being opened is on the row's right-click menu, which is
    /// where a mouse reaches it and where the operations that touch the list are named.
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
    ///
    /// A name half typed is not a name, and a row left holding a field that nothing focuses is a row
    /// that has to be clicked before it can be used again — so both endings are endings.
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

    /// Keeps the name that was typed into the list's field: the note is renamed, and the list is told
    /// what the note said.
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
    ///
    /// A note opened by a double click is the note a person is *in*, and naming it is the same act
    /// whether the list is in front of it or not — which is why both fields end here.
    fn commit_note_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.naming.take() else {
            return;
        };

        let name = self.name_input.read(cx).value().to_string();
        self.keep_name(folder, &name, cx);
        // The field goes away with the name; the keyboard has to go somewhere, and on the sheet that is
        // the sheet itself.
        self.sheet_focus.focus(window, cx);
        self.touch_status();
        cx.notify();
    }

    /// Asks for the note on the sheet to be renamed, on the next frame.
    ///
    /// Asked for, rather than done: focusing a field needs the window, and this is reached from a click
    /// — which has a context and no window. A rename that is *already* being typed is left alone, so
    /// that a second click on the title cannot clear a half-typed name.
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
    ///
    /// What it holds is what the bar says the note is called, which may be the name derived from the
    /// folder ("Blank sheet", "chapter-3") — a person renaming a note is editing the words they can see,
    /// not an empty box they have to recreate them in.
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
    fn stop_note_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming.take().is_none() {
            return;
        }

        self.sheet_focus.focus(window, cx);
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
    ///
    /// The note is renamed *first*, and the list is told what the note answered — never the other way
    /// round. The index is a cache of what the notes say, and a cache that runs ahead of what it
    /// describes is how a name goes missing from a note that has it.
    fn keep_name(&mut self, folder: PathBuf, name: &str, cx: &mut Context<Self>) {
        match note::set_title(&folder, name) {
            Ok(facts) => {
                // What the *note* says, not what the list would show for it: an unnamed note is written
                // to the index as unnamed, and the list derives the words at the moment it draws them.
                // Storing the derived words would freeze them — a row that says "Blank sheet" because
                // that is what it was called the day it was made, not because that is what it is.
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
    ///
    /// The message says where the note still is, because the words "forgot" and "deleted" are one
    /// keystroke apart in meaning and the app means the first: the entry is a line in a list, and the
    /// note is a folder with the writing in it.
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
    ///
    /// The *folder* is opened when it is there, never the source file: placing a file again is how a
    /// note is lost (see [`Self::open_or_place`]). A note whose folder has gone but whose file is
    /// still there is placed afresh — which is what the row says out loud, because what was written
    /// in the old note is not in the file.
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
    ///
    /// This is the state the app used to start in, and it is still the state a note that was cleared
    /// and left alone should end in — see [`Self::ensure_note`], which makes the note the moment
    /// there is ink to put in it.
    pub(crate) fn new_blank_sheet(&mut self, cx: &mut Context<Self>) {
        self.close_page();

        self.note = None;
        self.writer = None;
        self.pdf = PdfDocumentView::empty();
        self.pages = Pages::default();
        self.page_index = 0;
        self.ink = Notes::new();
        self.loaded.clear();
        self.saved.clear();
        self.rewritten.clear();
        self.ink.go_to(0);
        self.loaded.insert(0);
        self.home.hide();
        self.remember_page();

        self.message =
            String::from("a blank sheet — the first stroke makes a note, and Save writes it out");
        self.touch_status();
        cx.notify();
    }

    /// The note screen's keyboard: the way into the home screen, and Save.
    ///
    /// `Escape` leaves the note rather than the window: nothing can be lost by walking away from a
    /// note — the ink is in it as it is laid — and a screen that lists what you were writing is the
    /// one thing an app like this should be one keystroke away from. While a name is being typed,
    /// Escape is spent on that instead: an open field that Escape did not close would leave a person
    /// typing into a box they cannot leave.
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

        match keystroke.key.as_str() {
            "n" => self.new_blank_sheet(cx),
            "o" => self.prompt_for_pdf(cx),
            "s" => self.save(cx),
            _ => {}
        }
    }

    /// Remembers what was just opened, so the home screen offers it next time.
    ///
    /// The title is the name a person gave the note if they gave it one (see
    /// [`crate::store::META_TITLE`]), then the document's own name, then the note's folder turned back
    /// into words — a note made on a blank sheet is named after the moment it was made. The *folder*
    /// is what an entry opens; the source path is a convenience, and no ink depends on it. See
    /// [`crate::recent`].
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
    ///
    /// The note is already saved — it is a folder the app writes into as the pen moves — so this is
    /// the *export*: the single file a person carries to another machine. That is why it asks, every
    /// time, and why the suggestion is the document's name with `.zip` on it: the file is the note's
    /// public shape, and the working copy in the app's own directory is not something anyone should
    /// have to find.
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
    ///
    /// Everything the pen has laid goes first, and the page being closed is folded into chunks
    /// before the export is queued: the writer works in order, so the file that is written holds the
    /// ink that is on screen. The answer comes back through the writer's reports — see
    /// [`Self::drain_writer_reports`] — because the export happens on the writer's thread.
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
    ///
    /// The name a person gave the note comes first, because that is the name the note is *called*: a
    /// file written out of it is the note's public shape, and a person who named a note "3장 요약" is
    /// looking for `3장 요약.zip`. With no name it falls back to the document's own, and with neither —
    /// a blank sheet nobody has named — to the word "note".
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
        parts.push(self.timings.summary(HOUSEKEEPING_INTERVAL));

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

    /// Rebuilds the status line if enough time has passed, and says whether it did.
    ///
    /// Called on every wake of the housekeeping pump, which is up to 240 times a second. The clock is
    /// what keeps the text identical across those wakes, and the answer is what keeps the *frames*
    /// off them: a wake that rebuilt nothing has nothing new for a frame to draw.
    fn refresh_status(&mut self, now: Instant) -> bool {
        if status_due(self.status_at, now) {
            self.touch_status();
            return true;
        }

        false
    }

    /// The bar: floating, rounded, over the sheet.
    ///
    /// Two rows and one surface. The first is what the reader is *doing* — the document, the tool in
    /// hand, and the commands that act on the note — and the second is what is being written *on*:
    /// the sheet's size, its ruling, and the two colours. The bar floats for the reason the module
    /// docs give (an overlay needs no offset arithmetic for the pen), and it is rounded and lifted
    /// off the desk because that is what makes the sheet underneath read as paper.
    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline) = (theme.title_bar, theme.title_bar_border);

        // The width comes from a full-width box with a margin's worth of padding, not from setting
        // both insets on the bar itself: an absolutely positioned element with a left *and* a right
        // inset is laid out at its content's size here rather than stretched between the two, and a
        // bar at its content's size never wraps its second row — it runs off the edge of the window.
        div()
            .absolute()
            .top(px(BAR_MARGIN))
            .left_0()
            .w_full()
            .px(px(BAR_MARGIN))
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
    ///
    /// The elements are collected into owned vectors before they are chained onto the row: each
    /// `cx.listener` takes a mutable borrow of the context, and a single long builder chain would
    /// hold them all at once.
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
        // Undo and redo are offered only when the page's history says they would do something. A
        // button that is always there and sometimes does nothing is the one thing a user cannot
        // tell apart from a broken command, and this app has no way to say "nothing to undo" other
        // than by looking like it.
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
    ///
    /// This is the *title*: the name a person gave the note, or the one the app derived from the folder
    /// while it has none ("chapter-3", "Blank sheet"). The file the note was written on is still in the
    /// status line, because a note that was renamed still came from somewhere.
    ///
    /// Double-clicking it is the whole of the way in — there is no button beside it, because there is no
    /// second gesture to offer: every notebook turns its own title into a field when it is double-clicked,
    /// and a control that does what the title already does is a control nobody uses. While a name is being
    /// typed the field is *here*, in place: a name is edited where it is read.
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

    /// The bar's second row: the page's commands, the sheet's size, what is printed on it, its two
    /// colours, and the pen's weight.
    ///
    /// This row wraps and the first does not. The size, the ruling and the pen are *choosers* — each one
    /// a single box showing what is in use, with the alternatives in a list under it — while the twelve
    /// colours stay a row of swatches, which is what a palette is. Six sizes, four rulings and five pens
    /// as buttons was more than a narrow window holds, and the one that was current had to be found
    /// among its neighbours rather than read off the control.
    ///
    /// Each group is captioned because without the words it would be a guess which of the two runs of
    /// squares is the paper and which the ink, and which of the three boxes is the size, which the
    /// ruling, and which the pen. The page's three commands lead the row because a page *is* the sheet —
    /// they are the only controls here that change how much of it there is — and the zoom they traded
    /// places with is at the bottom of the desk, on the pill beside the page it acts on (see
    /// [`NoteApp::zoom_pill`]).
    ///
    /// The pen's weight sits beside the ink's colour rather than up with the tools, because the two are
    /// one answer: the colour is *which* pen, the weight is *how heavy* it is, and both travel with the
    /// note (see [`crate::settings`]). Its caption is a property and not a thing, like
    /// `Gray` — `Ink` is already the caption of the colours, and a second caption of `Pen` beside it
    /// would leave a person guessing which of the two was the pen.
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
    ///
    /// The width is set here rather than on the select itself: a `Select` fills its parent by design
    /// — it is a form field, and a form field is as wide as the field it is on — so in a row it would
    /// take the whole bar. Wide enough for its longest label (`Square`, `Letter`) so that the box does
    /// not resize as the choice changes, which would move the controls beside it.
    fn chooser(&self, state: &Entity<SelectState<Vec<&'static str>>>) -> impl IntoElement {
        div().w(px(112.0)).child(Select::new(state).small())
    }

    /// Keeps the three choosers showing what the note is actually written with.
    ///
    /// What the style *is* and what a box *draws* are two things — the style decides, `Select` draws —
    /// and they are kept in step here, the same way [`crate::home`] keeps its list's highlight on the
    /// row Enter would open. Three reads a frame, and a write only on the frames where they disagree.
    ///
    /// Not cosmetic: these boxes are the only control that changes the paper, the ruling and the pen,
    /// and a box showing A4 while the note is A5 is a press that asks for A4 and reports A4 — the one
    /// choice the note is already on. It moved from cosmetic to necessary when the style became the
    /// *note's*: opening a note now changes all three without any box being pressed.
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
    ///
    /// Floating pills rather than one bar, because they are read at different distances: the page
    /// number is reached for constantly and sits in the middle of the desk where the hand already is,
    /// the zoom is reached for when the page's own shape is in the way and sits at the edge that hand
    /// falls to, and the counters are glanced at and stay out of the way at the other edge.
    ///
    /// The page is centred by *this* row and the rest are taken out of the flow, so a counter growing
    /// by a digit cannot shove the page off centre — a page number that moved whenever a number
    /// changed would be worse than no page number.
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
                    .bottom_0()
                    .child(self.status_pill(cx)),
            );
        }

        // The zoom stays when the bar is hidden, and it is the one control that has to: the switch that
        // brings the bar back lives *in* the bar, so a sheet zoomed into a corner of itself with the
        // bar away would have no way out. It is reading rather than writing — it changes nothing about
        // the note — which is what the page pill and the counters have in common with it.
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
    ///
    /// A pill of its own, floating in the middle of the bottom edge: the page is what a reader
    /// navigates most, and it is the one control that belongs under the sheet rather than above it.
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
    ///
    /// In the pill the page commands left behind, and deliberately in their place: zoom is *reading*,
    /// not writing — it changes nothing about the note — so it belongs on the desk beside the page
    /// number rather than up on the bar with the controls that change the sheet.
    ///
    /// The percentage sits between the two steps and is a readout rather than a button: it is the one
    /// thing that says whether Fit Width has already been pressed. It is given a fixed width so that
    /// stepping from 99% to 100% to 101% does not shuffle the buttons either side of it.
    ///
    /// It does *not* hide with the bar, unlike the controls up there: zoom is how the sheet is *read*,
    /// and a person who turned the bar off is the one with the least way back — Fit Width is what
    /// recovers a view that has been zoomed into a corner of the page. The page pill and the counters
    /// stay for the same reason.
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

    /// The page commands: insert a page before or after this one, or delete this one.
    ///
    /// The first group in the bar's second row, because a page *is* the sheet and these are the only
    /// controls in the app that change how much of it there is. They stand bare on the bar rather
    /// than in a pill: in the bar, every group does. The pill they used to sit in is still there —
    /// the zoom has it now (see [`NoteApp::zoom_pill`]).
    fn page_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let hairline = theme.title_bar_border;

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
        ];

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .children(controls)
    }

    /// The counters, in a translucent pill on the desk.
    ///
    /// Out of the bar and over the desk, because it is a readout rather than a control, and because
    /// the line changes four times a second: text in the bar would re-lay-out the bar, while text in
    /// a pill of its own re-lays-out the pill.
    fn status_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.title_bar.opacity(0.92))
            .text_size(px(11.0))
            .text_color(theme.muted_foreground)
            .whitespace_nowrap()
            .truncate()
            .max_w(px(460.0))
            .child(self.status.clone())
    }

    /// The way back when the bar is hidden.
    ///
    /// A bar that can be hidden must never be hidden *permanently*: the switch that brings it back
    /// lives inside the bar, so hiding the bar would take the way back with it. This handle is drawn
    /// over the desk whenever the bar is away — the same pill the page is in, with one button in it,
    /// at the top right of the window.
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
        // Timed in a scope of its own, and *not* through `self.timings`: this guard holds a borrow
        // of what it measures, and the rest of this function needs the view mutably. It ends when
        // the element tree is built — the paint callback, which runs later, is measured separately.
        let for_render = Arc::clone(&self.timings);
        let _render_timed = measure(&for_render.render);
        // The counters describe one frame, so this frame's are its own.
        self.timings.start_frame();

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
        let page_color: Hsla = rgb(self.settings.page_color).into();
        let timings = Arc::clone(&self.timings);

        // The ghost cursor: a mark at the nib with the pen's body leaning away from it. Drawn last,
        // because a cursor belongs on top of everything, and only while the pen is in range — the
        // ink model drops the cursor the moment it hears the pen leave.
        let cursor = self.pen_cursor();
        // Its own colour rather than the ink's: it is not ink, and it has to be visible on a sheet
        // of any colour, including one where the ink would disappear.
        let cursor_color: Hsla = rgb(contrast_color(self.settings.page_color)).into();

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
            .bg(background)
            .text_color(foreground)
            // A trackpad's pinch, and a wheel with `Ctrl` held, both arrive here: the canvas is the
            // whole window's only interactive element, so it is what the gestures hit.
            .on_scroll_wheel(
                cx.listener(|app, event: &ScrollWheelEvent, _, cx| app.on_wheel(event, cx)),
            )
            .on_pinch(cx.listener(|app, event: &PinchEvent, _, cx| app.on_pinch(event, cx)))
            // The note's keyboard, on the screen being painted: Esc for the home screen, the two
            // arrows for the page, and the three commands a person expects to reach without letting
            // go of the pen. Registered here rather than as bindings because a screen with nothing
            // focused is a screen whose keys have to be caught as they are painted — see
            // [`crate::home`].
            .on_key_down(
                cx.listener(|app, event: &KeyDownEvent, window, cx| {
                    app.note_key_down(event, window, cx)
                }),
            )
            // The sheet is what the keyboard goes back to when a field goes away: a name field that has
            // just closed is a window that still thinks something is focused, and keys would go to a
            // box that is no longer drawn.
            .track_focus(&self.sheet_focus)
            .child(
                canvas(
                    |_, _, _| (),
                    move |_bounds, _, window: &mut Window, _cx: &mut App| {
                        let _timed = measure(&timings.paint);

                        // The sheet's shadow first, on the desk, then the sheet over it: a paint
                        // callback cannot put a layer behind what it draws, so the shadow is a few
                        // translucent rectangles rather than a renderer shadow (see the helper).
                        paint_page_shadow(window, page_origin, page_size.0, page_size.1);

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

                        // The ink is clipped to the sheet, which is the display half of the rule the
                        // ink model enforces: a reading off the paper is not ink (see
                        // [`InkTransform::on_paper`]), and ink off the paper is not *drawn* either. The
                        // clip is here rather than in the model because it also has to hide ink written
                        // before that rule existed, and because one rectangle for the whole page is
                        // cheaper than a mask per stroke.
                        window.with_content_mask(
                            Some(ContentMask { bounds: sheet_bounds }),
                            |window| {
                                for stroke in finished.iter() {
                                    if !stroke.visible_in(visible) {
                                        culled += 1;
                                        continue;
                                    }

                                    painted += 1;
                                    vertices += stroke.outline.len() as u64;
                                    paint_stroke(window, stroke, &sheet, &mut scratch);
                                }

                                // The stroke being drawn is never culled: it is by definition under
                                // the pen, and a stroke that vanished for a frame would read as a
                                // glitch.
                                if let Some(stroke) = &open {
                                    painted += 1;
                                    vertices += stroke.outline.len() as u64;
                                    paint_stroke(window, stroke, &sheet, &mut scratch);
                                }
                            },
                        );

                        timings.count_painted(painted, vertices, culled);

                        if let Some(cursor) = cursor {
                            paint_cursor(window, cursor, cursor_color);
                        }
                    },
                )
                .size_full(),
            )
            .child(bar)
            // The desk's own row: the page in the middle, the counters at the edge. Last, so it
            // paints over the sheet — a page pill *under* the paper would be no pill at all.
            .child(self.bottom_row(cx))
            .into_any_element()
    }
}

/// One of the bar's choosers: a list of options, and the one it starts on.
///
/// A component rather than a row of buttons, because these are *choosers* and not toggles: six paper
/// sizes, four rulings and five pens do not fit across a narrow window, and the set that is current is
/// read off one control instead of being picked out of a row that wraps. The items are the labels and
/// nothing else — a `Select` reports its choice as the text it was showing — which is why
/// [`CanvasSize::from_label`] and its two siblings exist to turn that text back into a setting.
///
/// `None` leaves the box empty, and is only reachable for a value the labels cannot name; the size,
/// the ruling and the pen always have one.
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
///
/// A chooser holds its choice by *position* and the style holds the choice itself, so the two are
/// married here: [`NoteApp::sync_choosers`] is the only caller, and it is the place a note's own
/// answer is put back into the boxes that were showing another note's.
fn chosen_index<T: PartialEq>(all: &[T], chosen: T) -> Option<IndexPath> {
    all.iter()
        .position(|one| *one == chosen)
        .map(IndexPath::new)
}

/// An icon button that puts a tool in hand.
///
/// The tool that *is* in hand is drawn in the accent colour, on a wash of it; every other tool is
/// muted grey on nothing. That is not styling layered over a component — it is which of two variants
/// the button is built with, because the variant is what decides the colours of the states the
/// pointer moves through, and a tool that is blue while idle and grey while hovered would be a tool
/// that changes what it is saying.
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
///
/// No visible label, so the words that name it are still set — as the tooltip a person reads and as
/// the name a screen reader is given. The variant is a ghost: nothing at rest, a small tint under
/// the pointer, and the icon in the foreground colour, which is what keeps a dozen of these from
/// reading as a form.
///
/// `enabled` is passed in rather than assumed, because a command that cannot act has to *say* so:
/// a button drawn disabled refuses the click, and so cannot be mistaken for a command that is
/// broken. The bar is rebuilt whenever the view renders, so the answer is never stale.
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
///
/// One logical pixel wide and tall enough to span a row of buttons: the line is punctuation between
/// groups, and anything heavier turns a toolbar into a table.
fn toolbar_divider(color: Hsla) -> impl IntoElement {
    div().w(px(1.0)).h_5().bg(color).mx_1()
}

/// A small muted caption in front of a group of controls.
///
/// The caption is what makes the second row readable: four of its controls are paper sizes and
/// twelve are colours, and without the words it would be a guess which is which.
fn control_label(text: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .text_size(px(10.0))
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

/// One colour swatch: a filled shape, ringed when it is the colour in use.
///
/// The ring is the accent colour rather than a neutral frame, because a swatch can be *any* colour —
/// including the one a frame would be drawn in — and a page-coloured square in a white bar would
/// otherwise have no edge at all. Ink is a circle, because a colour is what a pen is; paper keeps its
/// corners, because a page has them.
///
/// The ring is a border on this element's own box rather than a second element behind it: a border
/// takes part in the layout, so the swatch is the same size selected or not, and the row of them
/// never shuffles under the pointer.
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
        sheet_matches, solid_path, status_due, wheel_pan, wheel_zoom_factor, PDF_QUIET_INTERVAL,
        STATUS_INTERVAL, WHEEL_LINE_HEIGHT, WHEEL_LINES_PER_NOTCH, WHEEL_ZOOM_STEP,
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
        let mut stroke = Stroke::new(InkPoint::new(60.0, 0.0, 24.0), Stroke::DEFAULT_COLOR);

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

    /// A name a person gave a note becomes a file name: what Windows refuses becomes a dash, a run of
    /// whitespace becomes one space, and what a file system would quietly drop is dropped *visibly*.
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

/// The shadow a sheet casts on the desk.
///
/// Three translucent rectangles, each a little wider than the last and a little fainter, the largest
/// first: painted in that order they accumulate into one soft edge. The renderer's own shadows are
/// for *elements* — they cost a layer, and they are painted behind an element's own background,
/// which a rectangle drawn inside a paint callback does not have. A page is a path in a callback, so
/// its shadow is drawn as a path too; three steps read as one blurred edge at the sizes a page is
/// drawn at, and they cost three quads.
///
/// The sheet is lifted *and* offset downward, the way a sheet of paper lies on a desk: a shadow
/// centred on the paper reads as a glow, and one that is only offset reads as a hard edge.
fn paint_page_shadow(window: &mut Window, origin: Point<Pixels>, width: f32, height: f32) {
    /// Spread, vertical offset beyond the spread, and colour — widest and faintest first.
    const STEPS: [(f32, f32, u32); 3] = [
        (12.0, 4.0, 0x0000_0008),
        (7.0, 2.5, 0x0000_000C),
        (3.0, 1.0, 0x0000_0014),
    ];

    for (spread, drop, color) in STEPS {
        let corner = point(
            px(f32::from(origin.x) - spread),
            px(f32::from(origin.y) - spread + drop),
        );

        paint_rect(
            window,
            corner,
            width + spread * 2.0,
            height + spread * 2.0,
            rgba(color).into(),
        );
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

/// Draws the pen's ghost cursor: a soft mark at the nib, and the pen's body leaning away from it.
///
/// The body is curves rather than a quad — a quad cannot be rotated, and pointing somewhere is the
/// entire point of the shape, but a quad *drawn as* straight edges is a wedge. Three quadratic
/// curves swell its flanks and round its far end, which is what makes it read as a body seen at an
/// angle rather than as an arrowhead painted over the page.
///
/// Each shape is painted twice: a copy pushed out by a pixel or two, faint, and the shape itself
/// over it. That is the trick the page's shadow uses, in one step rather than three, and it is what
/// stops the cursor reading as a sticker on the sheet. The nib mark is the exception to how faint
/// it all is, because it is the one part that says where the ink will land.
///
/// It is rebuilt whenever the pen moves — there is no way to draw something whose position and
/// angle are both new each frame — which is affordable because it is three curves, not a stroke.
fn paint_cursor(window: &mut Window, cursor: PenCursor, color: Hsla) {
    let [x, y] = cursor.position();
    let here = |p: [f32; 2]| point(px(p[0]), px(p[1]));

    // The body is absent for a pen with no tilt sensor, and for one held straight up. It fades in
    // as the lean grows, so the threshold is not a shape appearing out of nothing.
    if let Some(body) = cursor.body_shape() {
        let fade = cursor.body_fade();

        // The soft edge first, then the body over it: widest and faintest first, which is the order
        // the page's shadow is painted in.
        for (grow, alpha) in [
            (BODY_HALO_GROW, BODY_HALO_ALPHA * fade),
            (0.0, BODY_ALPHA * fade),
        ] {
            let [nib_left, flank_left, far_left, cap, far_right, flank_right, nib_right] =
                body.outline(grow);

            // The same rule the ink uses. This outline is convex, so the default rule would do —
            // but a filled shape that can show a hole is one degenerate tilt away from being a bug.
            let mut builder = solid_path();
            builder.move_to(here(nib_left));
            builder.curve_to(here(far_left), here(flank_left));
            builder.curve_to(here(far_right), here(cap));
            builder.curve_to(here(nib_right), here(flank_right));
            builder.close();

            if let Ok(path) = builder.build() {
                window.paint_path(path, color.opacity(alpha));
            }
        }
    }

    // The nib: a small circle, always, so there is a fixed point that says exactly where the ink
    // will land. The faint bloom around it is what lets it sit in the page rather than on it.
    for (radius, alpha) in [(NIB_BLOOM_RADIUS, NIB_BLOOM_ALPHA), (NIB_RADIUS, NIB_ALPHA)] {
        paint_dot(window, point(px(x), px(y)), radius, color.opacity(alpha));
    }
}

/// Fills a circle centred on a point: a square rounded by half its own side.
///
/// A quad rather than a path, because a quad is a handful of numbers the renderer places directly —
/// there is nothing here to build and nothing to curve.
fn paint_dot(window: &mut Window, centre: Point<Pixels>, radius: f32, color: Hsla) {
    let radius = px(radius);
    let corner = point(centre.x - radius, centre.y - radius);

    window.paint_quad(
        fill(
            Bounds {
                origin: corner,
                size: size(radius * 2.0, radius * 2.0),
            },
            color,
        )
        .corner_radii(Corners {
            top_left: radius,
            top_right: radius,
            bottom_right: radius,
            bottom_left: radius,
        }),
    );
}

/// Fills a stroke's ribbon outline.
///
/// A stroke is a filled polygon rather than a stroked polyline, because that is the only shape
/// that can carry a width that changes along the line: the pen's force is baked into the
/// outline's two edges, and a single stroke-width would flatten it.
///
/// The colour comes from the stroke itself and not from the palette: a page can hold ink written
/// with several pens, and a frame has no business knowing which one is in hand.
///
/// The outline is in the sheet's coordinates, so it is scaled and placed on the way out, and the
/// `points` scratch buffer is handed in rather than allocated per stroke: a frame with three
/// hundred strokes on it would otherwise make — and free — three hundred vectors.
fn paint_stroke(
    window: &mut Window,
    stroke: &Stroke,
    sheet: &Sheet,
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
        let color: Hsla = rgb(stroke.color).into();
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
