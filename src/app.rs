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
//! The overlay costs the pen one number: where the bar ends (see [`NoteApp::bar_edge`]). A reading
//! that lands on the bar is a press on a control rather than ink, so that line has to be the bar's
//! *real* bottom edge — observed from the frame, since only the toolkit knows how the bar wrapped —
//! while no ink point is ever *moved* by the bar's existence, which is the offset this design avoids.
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
//! "Something new" is one thing: ink that changed. The pen's ghost cursor used to be the other, and
//! it has left the frame altogether — a pen held in range without touching lays no ink and draws its
//! cursor in a window of its own, fed from the pen thread rather than from here (see
//! [`crate::cursor_overlay`]). So a batch that laid no ink has nothing to show, and no frame is
//! drawn for it.
//!
//! The wake draws the canvas itself rather than waiting for a frame to: the ink is the one part of a
//! canvas that changes between frames, and a frame on this stack is redrawn on every vblank and
//! carries the whole interface with it. The pump hands the canvas to its layer and the compositor
//! paces the present (see [`crate::ink_layer`]), so the line lands at the display's rate rather than
//! at the interface's, and a frame is left to redraw the chrome and the sheet's own geometry.
//!
//! A second task ([`NoteApp::start_display_pump`]) does the work that is not the ink — rasterising
//! the page, rebuilding the counters — on a fixed [`HOUSEKEEPING_INTERVAL`] that owes nothing to
//! the display, so that a rasterisation can never delay a stroke.
//!
//! ## The top bar, and why it can be turned off
//!
//! The status line holds counters that move on every reading, and text that changes is text GPUI has to
//! re-shape and re-lay-out. Rebuilding it on every frame is what made the top of the window flicker
//! while writing, so it is rebuilt when the user changes something — a key, a tool, a page — and never
//! on a clock of its own. What needs a clock to be readable is a measurement, and that is a session a
//! person starts and stops by hand (see [`crate::timing::Session`]): the fastest, the slowest and the
//! mean of the wait between frames, over exactly the stretch they chose. The line can be switched off
//! entirely — as can the whole bar, which floats over the canvas.
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
/// Used for two decisions: whether the pen is over the bar, where a reading is a press on a control
/// rather than ink — and where the ghost cursor hides, since a ghost drawn over the controls would be
/// a ghost over a control. The window it is drawn in hides it and cuts the body's lean at this line,
/// so a pen just below the bar does not lean a body across it. See [`NoteApp::bar_edge`], which is
/// the *measured* edge the ink rule uses; this is the estimate the two cursor rules share.
///
/// The estimate is the bar at its tallest — its margin, its padding and two rows of controls, the
/// second of which may have wrapped — and it errs upward deliberately: being wrong upward leaves a
/// strip of sheet still showing the pointer (a small surprise), while being wrong downward would
/// leave part of the bar with no cursor at all, and the pen is how those controls get clicked.
///
/// Ink is not asked that favour, which is why it does not use this number: a reading refused in a
/// strip of page *below* the bar is a pen that does not write where the user can see it. The rule
/// that keeps ink off the bar uses the bar's own edge, as the frame laid it out — see
/// [`NoteApp::bar_edge`].
const BAR_HEIGHT: f32 = 148.0;

/// The key that starts and stops a measurement session.
///
/// Named once for the one place it is *said* — the line that reports on a session, which tells the
/// reader how to stop it. The chord this app listens for is the same one and is matched by its letter in
/// [`NoteApp::note_key_down`], where the control modifier is what separates it from the sheet's own
/// unmodified keys.
const MEASURE_KEY: &str = "Ctrl+M";

/// How much of the window's width the status pill may take, as a fraction of it.
///
/// A fraction rather than a width, because the line is longer than any pill at any size: the pill
/// wraps, and what it is given decides how many lines it wraps into. Not the whole window, because a
/// readout that runs the width of the desk is a bar rather than a pill — see
/// [`NoteApp::status_pill`] for the arithmetic, and [`STATUS_LIFT`] for where it sits.
const STATUS_PILL_WIDTH: f32 = 0.6;

/// How far above the desk's bottom row the status pill floats, in logical pixels.
///
/// The pill is wide, and the page pill sits in the middle of that row: a readout drawn over the
/// control a person is reaching for is worse than one floating a row higher. The number is the page
/// pill's own height plus [`BAR_MARGIN`], which is what "clear of it" comes to.
const STATUS_LIFT: f32 = 40.0;

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
/// A free function for the same reason the other decisions in this file are: it is a decision, and
/// decisions that can be tested without a window are worth testing without one.
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
    /// `bar` travels with it because it answers the same question asked of the interface: it is where
    /// the bar ends, and a reading above it is a press on a control rather than ink (see
    /// [`InkTransform::on_bar`]).
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
    /// The pages of the note that are marked, in reading order.
    ///
    /// Held here rather than read from the note wherever it is wanted, because the status line, the
    /// bar's button and the list all ask about it on paths that have no error to report — and because
    /// the marks are one of the few things the app *writes* while it runs: every mark is a row in the
    /// note (see [`crate::bookmarks`]).
    bookmarks: Bookmarks,
    /// The list of those marks, drawn in front of the note while it is up.
    marks: Marks,
    /// The document's own table of contents, drawn in front of the note while it is up.
    ///
    /// The twin of [`Self::marks`] — a screen over the sheet rather than a panel on it, for the reason
    /// [`crate::bookmarks`] gives — and the other half of "how do I get there": the marks are the reader's
    /// own list of places, and this is the document's.
    outline: Outline,
    /// The PDF being annotated, if any.
    pdf: PdfDocumentView,
    /// The pen capture and its queue.
    pen: PenService,
    /// The hook that hides the system pointer while the pen has a cursor of its own.
    ///
    /// `None` when the platform would not give one, which costs nothing but a second cursor.
    system_cursor: Option<SystemCursor>,
    /// The window the pen's ghost cursor is drawn in, outside the frame.
    ///
    /// `None` when the platform would not give one, which costs the pen its ghost and nothing else:
    /// the system pointer stays, and the app is what it was before the overlay existed. See
    /// [`crate::cursor_overlay`] for why the ghost is not part of a frame.
    cursor: Option<CursorFeed>,
    /// The canvas, drawn by a renderer of the app's own rather than by a frame.
    ///
    /// `None` when it could not be installed — a machine whose graphics processor will not give
    /// Direct3D 11 a device — in which case the app says so once and draws the canvas the way it
    /// did before this existed. See [`crate::ink_layer`].
    ink_layer: Option<InkLayer>,
    /// Counts the rebuilds of the ink's own outlines.
    ///
    /// The ink model rebuilds every stroke's ribbon when the zoom crosses into another detail rung
    /// (`InkDocument::set_zoom`), which changes what the canvas layer has to draw without changing
    /// any stroke's identity. This is how the layer is told: it is part of the canvas it is handed
    /// (see [`crate::ink_layer::canvas::Ink`]).
    ink_revision: u64,
    /// When the canvas layer last presented, for the gap between two presentations: the rate the ink
    /// reaches the screen at, which is not the frame rate and is the one the writing is judged by
    /// (see [`crate::timing::Timings::present_gap`]).
    last_present: Option<Instant>,
    /// The canvas as the last frame described it: the desk, the page's shadow, and the sheet.
    ///
    /// Described by the app and drawn by the layer — or, when there is no layer, painted by the
    /// frame from this same description. Held rather than built per frame for the memory's sake,
    /// and because the two renderers must be handed the same rectangles (see
    /// [`crate::ink_layer::canvas`]).
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
    ///
    /// A measurement rather than a constant — unlike [`BAR_HEIGHT`], which is the same edge
    /// *estimated* for the cursor's sake — because the ink rule cannot be given the benefit of the
    /// doubt that the estimate takes: refusing a reading in a strip of page below the bar is a pen
    /// that does not write where the user can see it. The bar's real height is the toolkit's
    /// business, not this file's: its second row wraps when the window is narrow, so it is a
    /// different number in a different window.
    ///
    /// Written during prepaint by the one element that knows where the bar ended (see
    /// [`Self::top_bar`]) into a cell shared with that closure, and read by the pump, which has no
    /// window to ask — the same arrangement, and for the same reason, as [`Self::sheet`].
    ///
    /// `None` until a frame has laid the bar out: nothing is refused on the strength of a
    /// measurement that has not been made.
    bar_bottom: Rc<Cell<Option<f32>>>,
    /// What every hot path costs, shared with the canvas's renderer.
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
    /// Held here rather than rebuilt every frame: a frame must not do geometry work, and a grid
    /// over a full page is several hundred marks.
    ruling: Ruling,
    /// The status line as last composed.
    ///
    /// Cached so that most frames draw the *same* text. A line rebuilt every frame is a line the text
    /// system has to shape and lay out every frame, which is what flickers — so it is rebuilt when the
    /// user changes something that the line reports, and never on a clock (see the module docs).
    status: String,
    /// When the measurement session was started, if one is running.
    ///
    /// The app owns the clock rather than the counters, because "how long did I measure for" is wall
    /// clock rather than accumulated nanoseconds — and because a session that is stopped and started
    /// again has nothing to keep in the counters at all. `None` means idle or stopped.
    session_at: Option<Instant>,
    /// When the last frame was drawn, for the interval between frames.
    ///
    /// A frame's own duration is measured already (`ink`, `render`, `paint`), and what those leave out
    /// is where a stutter lives: the wait between one frame ending and the next arriving, which is the
    /// toolkit's scene, its upload and its present. That interval is what a session measures — see
    /// [`crate::timing::Session`].
    last_frame_at: Option<Instant>,
    /// What the last session measured, held on the line until another one replaces it.
    ///
    /// Held rather than shown once: a result that vanished on the next key press would be a number
    /// nobody could read, and reading it afterwards is the whole point of stopping the session.
    measured: Option<Measurement>,
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
    /// The note screen's keys are caught by the element being *painted* (see [`Self::note_key_down`]), and
    /// GPUI dispatches a keystroke along the path from the window's root to whatever is **focused** — so
    /// the sheet hears its own keys only while this handle holds the focus. What gives it back is the rule
    /// in [`Self::claim_sheet_keyboard`].
    sheet_focus: FocusHandle,
    /// Whether the note screen held the keyboard on the last frame it was in front.
    ///
    /// The rule's memory, and the whole of why it is a field: the keyboard is claimed when a *screen*
    /// changes, not on every frame — a list that is up must keep the focus its own arrow keys need.
    sheet_has_keyboard: bool,
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

/// The pen thread's tap on the readings: every batch goes to the ghost cursor's window.
///
/// A named function rather than a closure inline, because there is nothing for it to capture: the
/// feed carries what the frame publishes — the scale, the colour, the sheet's edge — and the pen
/// thread only says *when* something arrived. See [`crate::pen::BatchTap`].
fn cursor_tap(feed: &CursorFeed) -> BatchTap {
    let feed = feed.clone();

    Arc::new(move |samples: &[pen_windows::PenSample]| feed.offer(samples))
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
                // The other half of what a session measures: the same wait, folded into the session
                // rather than into the running mean, so a stretch of writing can be reported on its own.
                app.timings.session.record_response(batch.waited);

                // The ink is the frame's business, and it is now the only part of a reading that is:
                // a pen in range but not touching lays none, and the ghost cursor follows it without
                // a frame at all (see [`crate::cursor_overlay`]). `consume` is the whole of what a
                // batch has to say here.
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
                    // Nothing on screen changed: a reading the resampler dropped, or a hover that
                    // moved the ghost and nothing else. Repainting an identical scene on each of
                    // those is what made the top of the window look like it was flickering — and the
                    // ghost, which is drawn elsewhere, has already moved by now.
                    return;
                }

                // The canvas draws itself here, on the pen's own wake rather than in a frame: this
                // app describes the canvas and its layer draws it, and the ink is the one part of a
                // canvas that changes between frames. What only a frame can give it is the display's
                // own rate — a frame is redrawn on every vblank and carries the interface with it, at
                // several times the cost of the ink, so ink that waited for one would reach the screen
                // at a fraction of the rate the display can show (see [`Self::draw_canvas`]).
                {
                    app.draw_canvas();
                }

                // The counters have moved too, but the line is not rebuilt for them: it waits for a
                // change the user made, and the counters a person actually reads are the ones a session
                // reports. Re-shaping text is still work a frame drawn to show ink does not have to
                // carry (see the module docs).
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
    ///
    /// The stroke under the pen is closed here, for the sheet as it is drawn now: an in-progress one
    /// has no cached ribbon outline yet. Its newest segment is left straight — the reading after the
    /// tip has not arrived, and a curve drawn without it would move ink the user has already seen,
    /// under the nib, as they write (see `Stroke::close_live`).
    ///
    /// Called by a frame, which is where the sheet's own geometry is decided, and by the pen's pump
    /// before it draws the canvas itself. Both, because the ink is the one part of a canvas that
    /// changes between frames: the desk, the paper, its ruling and the document's page change only
    /// when the user changes them, and ink that waited for a frame would reach the screen at the
    /// frame rate rather than at the display's (see [`Self::draw_canvas`]).
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

    /// Hands the canvas to its layer, and forgets the layer if it fails.
    ///
    /// Called by the pen's pump — where the ink is newest — and by a frame, which is what draws the
    /// chrome and what knows where the sheet is. A layer that fails is reported rather than drawn
    /// around: it is dropped, the desk below comes back to the frame, and the canvas keeps its last
    /// frame. Both callers go through here so that is one path.
    fn draw_canvas(&mut self) {
        // Nothing has described a canvas yet: this is a reading that arrived before the first frame,
        // and a layer handed an empty description would paint an empty desk over the window.
        if self.canvas.sheet.is_none() {
            return;
        }

        self.describe_ink();

        // Timed by hand rather than with a guard: a guard would hold a borrow of the counters across
        // the layer's own borrow of the app. This is the draw, and what the status line's `canvas`
        // clause is about (see [`crate::timing`]); the description above it is arithmetic on a few
        // rectangles and one `Arc` per frame.
        let started = Instant::now();
        let drawn = match self.ink_layer.as_mut() {
            Some(ink) => ink.draw(&self.canvas),
            None => Ok(false),
        };
        self.timings.canvas.record(started.elapsed());

        match drawn {
            Ok(true) => {
                // The ink reached the screen. The gap between two of those is the rate it reaches it
                // at — the ink's own rate, drawn by whichever wake asked for it — and it is the number
                // a session that reads only frames cannot show (see [`crate::timing`]).
                if let Some(previous) = self.last_present.replace(started) {
                    self.timings.present_gap.record(started - previous);
                }
            }
            Ok(false) => {}
            Err(error) => {
                self.ink_layer = None;
                self.message = format!("canvas layer: {error}");
            }
        }
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


    /// How far down the window the bar reaches, in logical pixels: the line above which a reading
    /// belongs to a control and below which it belongs to the page.
    ///
    /// `0.0` — a line that refuses nothing — when the bar is hidden, and before any frame has laid
    /// it out: a window with no bar in front of the page has no such line, and neither has one whose
    /// bar is still only a plan.
    fn bar_edge(&self) -> f32 {
        if !self.settings.show_toolbar {
            return 0.0;
        }

        self.bar_bottom.get().unwrap_or(0.0)
    }

    /// Reads a batch of readings into the ink model, timed.
    ///
    /// The transform comes from the last frame's sheet rather than from the window: a reading
    /// belongs on the sheet the user was looking at when the nib moved. The bar's edge travels with
    /// it for the same reason, and it is what keeps a tap on the bar from landing on the page behind
    /// it (see [`Self::bar_edge`] and [`InkTransform::on_bar`]).
    fn consume_ink(&mut self, samples: &[pen_windows::PenSample]) -> bool {
        // The pen is not a pen while something is in front of the sheet: a name being typed must not
        // leave a line across the page behind the field, and the two *screens* — the list of notes, and
        // the list of this note's bookmarks — are drawn over a sheet nobody can see. A stroke laid there
        // would go into a note the user is not looking at, and behind a screen it would not even be
        // visible. This is why both are screens rather than panels floating on the sheet: see
        // [`crate::bookmarks`].
        if self.naming.is_some() || self.home_is_open() || self.marks.is_open() || self.outline.is_open()
        {
            return false;
        }

        let transform = self.sheet.transform(self.scale, self.bar_edge());
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

        // And the marks that were made on the sheet before there was a note to keep them in: a blank
        // sheet is a note that does not exist until something is written on it, and a bookmark made on
        // one is one of the things that makes it exist. Until here they were only in memory, which is
        // where a setting chosen on a blank sheet lives too (see [`crate::settings`]).
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
        // The ghost is drawn in a window of its own now, so this is first a question about that
        // window: with no overlay there is nothing to put in the pointer's place, and hiding the
        // pointer would leave the window with no cursor at all. The frame publishes the same answer
        // to the overlay — see [`Self::publish_screen`] — so the two can never disagree.
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
    ///
    /// Four things, and each of them is something a reading cannot say: the scale its pixels are in,
    /// the colour it is drawn in on this paper, where the bar ends, and whether this screen wants a
    /// ghost at all. Publishing is what makes a switch take effect at once — the Tilt switch, the
    /// home list — rather than at the next reading.
    ///
    /// With no overlay this is nothing at all, which is why the caller does not check for one.
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
        // The marks are renamed with the pages, because a mark names a page's *position*: see
        // [`crate::bookmarks::Bookmarks::inserted_at`] for the rule and the note's own shift for the
        // rows that back it.
        self.bookmarks.inserted_at(at);

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
    ///
    /// The same act as [`Self::toggle_bookmark`], with the page *named* rather than worked out from what is
    /// in front of the reader. What a row of the contents screen wants: a row's page is a page the reader
    /// picked, not the one they happen to be looking at — and a right-clicked row is not necessarily the
    /// highlighted one.
    pub(crate) fn toggle_page_bookmark(&mut self, page: usize, cx: &mut Context<Self>) {
        self.record_mark(page, !self.bookmarks.contains(page), cx);
    }

    /// Takes the bookmark off a page, asked for by the row that names it.
    ///
    /// Not a toggle: the menu says "take the bookmark off", and a command that says what it does has to
    /// do that rather than the opposite of what it finds there.
    pub(crate) fn unmark_page(&mut self, page: usize, cx: &mut Context<Self>) {
        self.record_mark(page, false, cx);
    }

    /// Records that a page is marked or is not: in the list, in the note, and in what the app says.
    ///
    /// One place, because its callers are all the same act told differently — the toggle key, the bar's
    /// button, the list's menu — and because a mark is the one thing the app writes that is neither ink
    /// nor a setting: a single `INSERT` or `DELETE` in a file that is already open, done here rather than
    /// on the writer's thread, which exists for batches of ink.
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
    ///
    /// Which page that is depends on what they are looking at. With the sheet in front it is the page on
    /// the desk; with a *list* in front it is the page of the row that list is pointing at — the marks
    /// list points at a marked page, the contents at the page of an entry — because a person pointing at a
    /// list of places is pointing at a row, and marking the page behind the screen is the one thing the key
    /// would be understood not to do. A row that names no page (an entry that leaves the document) leaves
    /// the reader with the page on the desk, which is the only page left.
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
    ///
    /// The page being read is *not* closed first, unlike a page turn: nothing about the note changes by
    /// looking at a list of its marks, and the ink still in memory is written on the batch's own clock
    /// (see [`Self::persist`]) rather than on this command. What the screen needs is its rows, which are
    /// gathered here rather than kept in step for as long as it is up.
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
    ///
    /// The screen goes with it. It has done its job: the page the reader asked for is the page in front
    /// of them now, and a list left over the answer would be a list covering it.
    pub(crate) fn go_to_mark(&mut self, page: usize, cx: &mut Context<Self>) {
        self.marks.hide();

        // A row can only be a page the list was given, so this is a guard rather than a case: a note
        // whose pages were deleted from under a list that is somehow still up.
        let page = page.min(self.page_total().saturating_sub(1));
        self.turn_to(page);
        cx.notify();
    }

    /// Turns to the next marked page after this one.
    ///
    /// The keyboard's step, for a reader who knows where they are going: no list, no highlight, one
    /// keystroke per mark. A note with nothing marked ahead says so rather than doing nothing, because a
    /// key that quietly stops working is a key a person presses again.
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
    ///
    /// `None` when there is nothing to show: a note written on a blank sheet has no document, and most
    /// documents carry no contents of their own. Both cases are answered the same way — there is no list to
    /// put in front of the note — and the caller is the one that says which it was, because only it knows
    /// whether a key was pressed or a button was greyed out.
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
    ///
    /// Refused, in words, for a note whose document has none: the bar's button is disabled in that case, and
    /// a *key* cannot be disabled, so this is where the two part company. Nothing is written and no page is
    /// closed — the sheet waits exactly as it does behind the bookmark list — and the screen is handed rows
    /// that already know where in this note they land.
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
    ///
    /// The screen goes with it, as the bookmark list does when one of its rows is opened: the page the
    /// reader asked for is the page in front of them now, and a list left over it would cover the answer.
    pub(crate) fn go_to_entry(&mut self, page: usize, cx: &mut Context<Self>) {
        self.outline.hide();

        // A row can only name a page the note had when the rows were built, so this is a guard rather than
        // a case: a note whose pages changed under a list that is somehow still up.
        let page = page.min(self.page_total().saturating_sub(1));
        self.turn_to(page);
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

    /// Gives the keyboard to the screen in front, on the frame that screen changes.
    ///
    /// ## Why this is needed at all
    ///
    /// A keystroke in GPUI is dispatched along one path: the window's root, down to whatever element has
    /// the **focus** (see `dispatch_path` in the framework's key dispatch). Every screen here catches its
    /// own keys with a listener on the element it paints, so a screen hears a key only if the focus is
    /// inside it. The lists — the home screen, the bookmarks, the contents — focus themselves when they
    /// appear, which is why they always hear their arrows. The note screen does not: it is the *fallback*
    /// screen, the one that is left when every list is closed, so nothing hands it the keyboard — and a
    /// window whose focus is left on a screen that has just gone away resolves that focus to *no node at
    /// all*, which leaves the path as the window's root and the note screen's keys **swallowed**: no
    /// arrows, no `Ctrl+S`, no `Escape`, from opening a note until something else gives the sheet the
    /// keyboard back (a rename does, which is what made this look like it depended on writing).
    ///
    /// ## What it does
    ///
    /// Focuses the sheet when the *screen in front* changes to the note screen. Not every frame: a list
    /// that is up needs the focus for its own arrow keys, and taking it away once per frame would break
    /// exactly the screens this fixes the fallback from. The flag is what remembers the last answer, so
    /// this costs a comparison per frame and a focus only on a transition — the same arrangement as
    /// [`crate::bookmarks::Marks::settle`], from the other direction.
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
    fn commit_note_name(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.naming.take() else {
            return;
        };

        let name = self.name_input.read(cx).value().to_string();
        self.keep_name(folder, &name, cx);
        // The field goes away with the name, and the keyboard goes back to whatever is in front — the sheet,
        // or a list that is up — on the next frame, through the one rule that knows which screen that is.
        // Handed back rather than focused here: this is reached from the field's own event, and a field does
        // not know what is behind it. See [`Self::claim_sheet_keyboard`].
        self.sheet_has_keyboard = false;
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

        // And the *name* goes with the note: the bar's title, the window's title and what `Save` writes the
        // export out as all come from this one string (see [`Self::save`]), and a blank sheet that still
        // says the name of the note that was open would be a sheet named after a document it does not
        // have — and, on its first stroke, a new note filed under that name.
        self.note_title.clear();

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
    ///
    /// Everything a list needs the keyboard for — arrows to move, Enter to open the row, Escape to come
    /// back — belongs to the list, which has the focus, and this screen takes none of it away. What is
    /// left is what the list cannot know: `Ctrl+B`, which marks the row in front of the reader (the same
    /// act as marking the page in front of them on the sheet, see [`Self::marked_page`]), and the three
    /// chords that are the app's wherever it is — a new sheet, opening a file, saving one — which is the
    /// same set the home screen keeps.
    ///
    /// The steps between marks are deliberately not bound here: the arrows are the list's own, and two
    /// meanings on one key would be one meaning too many.
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
    ///
    /// The twin of [`Self::marks_key_down`], and for the same reasons: the arrows, Enter and Escape belong
    /// to the list, and what is left is what the list cannot know — `Ctrl+B`, which marks the page of the
    /// row in front (an entry of the contents is a page like any other), and the three chords that are the
    /// app's wherever it is.
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
    ///
    /// An error has to reach the user as soon as it happens rather than waiting for the next change they
    /// make — the line is not on a clock, so this is the only thing that would otherwise hold it back.
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
    /// Composed from the current state rather than cached here: caching it is what
    /// [`Self::touch_status`] does, and keeping the two apart means a caller can put a line together
    /// — a test, or a screen — without it being the line the bar is showing.
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

        // The measurement session, while one is running or once it has been stopped, and in front of the
        // live meters rather than instead of them: the meters answer "what is happening now", and this is
        // the stretch of time that was actually measured. The two together are what makes a stutter
        // arguable rather than a matter of opinion — see [`crate::timing::Session`].
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
    ///
    /// There is no clock behind this any more, and that is the point: a line that moves on its own is a
    /// line nobody can read, so it is rebuilt by the change that made it stale and by the session a
    /// person starts and stops.
    fn touch_status(&mut self) {
        self.status = self.compose_status();
    }

    /// Starts or stops the measurement session, and puts what it found on the status line.
    ///
    /// One key for both, because a session is a stopwatch: starting it and stopping it are the same
    /// gesture a moment apart, and a person reading a number off the screen should not have to remember
    /// which of two keys they pressed. What is measured is the wait between frames and the pen's own
    /// wait (see [`crate::timing::Session`]); what is *shown* while it runs is only that it is running,
    /// because a number that changes while it is being measured is the thing this replaced.
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
    ///
    /// Two rows and one surface. The first is what the reader is *doing* — the document, the tool in
    /// hand, and the commands that act on the note — and the second is what is being written *on*:
    /// the sheet's size, its ruling, and the two colours. The bar floats for the reason the module
    /// docs give (an overlay needs no offset arithmetic for the pen), and it is rounded and lifted
    /// off the desk because that is what makes the sheet underneath read as paper.
    ///
    /// It also *reports where it ended*, which is the one number the overlay costs the pen: a mark
    /// pinned to its bottom edge, whose bounds are the bar's own footprint as this frame laid it out.
    /// See [`Self::bar_edge`].
    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline) = (theme.title_bar, theme.title_bar_border);

        // The mark below writes through this; the pump reads it later, with no window to ask.
        let bar_bottom = Rc::clone(&self.bar_bottom);

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
            // A zero-height box on the bar's own bottom edge — the edge is what it is pinned to, so
            // its bounds are right whatever the bar's height turned out to be, and a bar that wraps
            // into three rows reports the third row's bottom rather than a guess at it. Paints
            // nothing, takes no pointer, and is out of the layout: it exists to be measured.
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
                    // A row higher than the page pill, because the line wraps into a wide box and the
                    // page pill is in the middle of this one: see [`STATUS_LIFT`].
                    .bottom(px(STATUS_LIFT))
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

    /// The page commands: insert a page before or after this one, delete this one, and mark it.
    ///
    /// The first group in the bar's second row, because a page *is* the sheet and these are the only
    /// controls in the app that change how much of it there is. They stand bare on the bar rather
    /// than in a pill: in the bar, every group does. The pill they used to sit in is still there —
    /// the zoom has it now (see [`NoteApp::zoom_pill`]).
    ///
    /// The bookmark sits with them because it is a fact about *this page* and not about the view: the
    /// page pill and the zoom pill are how the sheet is read, and this is something the page keeps. Its
    /// button says which way round the page is — a filled mark for a marked page — and the list of what
    /// is marked is the button beside it, because the two are the same subject.
    ///
    /// The last group is the *document's* own table of contents — see [`crate::outline`] — which is a
    /// different subject again: the marks belong to the reader, the contents to the document they are
    /// annotating. It is disabled for a document that has none, which is most of them.
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
            // The document's own contents: a different subject from the marks — one belongs to the reader, the
            // other to the document — so it stands in its own group. Disabled for a document that has none,
            // which is most of them, because a button that opens a screen saying "there is nothing here" is a
            // button that should not have been offered.
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
    ///
    /// Out of the bar and over the desk, because it is a readout rather than a control, and because
    /// the line changes four times a second: text in the bar would re-lay-out the bar, while text in
    /// a pill of its own re-lays-out the pill.
    ///
    /// It **wraps**, and it is allowed most of the window's width, because of what the line has grown
    /// into: the pen's readings, the ink's counts, the page cache's hit ratio, the zoom and the frame's
    /// own timings are some three hundred characters, which is more than fifteen hundred logical
    /// pixels at this size — several times any window. Set on one nowrap line of 460 pixels (which is
    /// what this pill used to do) the timings were the part that fell off the end of it, and the
    /// timings are the whole reason to read the line while something is being measured. A pill that
    /// wraps costs a strip of the desk along the bottom of the sheet and shows all of it, and it grows
    /// *upward* from the corner it is anchored to rather than down off the edge of the window.
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
        // the element tree is built. Describing the canvas, which happens at the end of this
        // function, has a meter of its own (`canvas`).
        let for_render = Arc::clone(&self.timings);
        let _render_timed = measure(&for_render.render);
        // The counters describe one frame, so this frame's are its own.
        self.timings.start_frame();

        // The wait since the last frame, for the session a person may be running: the interval a
        // frame's own meters cannot see, because they measure what a frame *does* and this is the time
        // between two of them (see [`crate::timing::Session`]). Recorded here because this is the frame
        // clock — `render` runs once per frame and not for a frame the toolkit decided not to draw. The
        // clock is kept whether or not a session is running, so the first interval of one is a frame
        // interval rather than the whole time since the last session.
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

        // The keyboard goes with the screen in front. Done here, before any of the screens is built,
        // because this is the one place every transition passes through — opening a note, closing a list,
        // coming back from the home screen, closing a name field — and the arrangement it replaces was one
        // that every transition had to remember. See [`Self::claim_sheet_keyboard`].
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

        // The ink in front of the reader follows the sheet it is drawn on: a zoom magnifies the pieces
        // a stroke is made of as well as the stroke, so the outlines a frame walks are rebuilt when
        // the *detail* the zoom asks for moves, rather than on every frame a pinch is on. This keeps a
        // cache in step and edits nothing — the strokes themselves are what they were (see
        // `InkDocument::set_zoom`).
        // The outlines themselves are rebuilt when the zoom crosses into another detail rung, and the
        // canvas layer has to hear about that: every stroke's geometry is stale, without any stroke's
        // *identity* having changed (see [`crate::ink_layer::render`]).
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

        // What the canvas is, described for its own renderer (see [`crate::ink_layer::canvas`]): the
        // desk, the page's shadow and the sheet; then what is printed on the paper, the document's
        // page, and the ink. The drawing itself is timed where it happens — in [`Self::draw_canvas`]
        // — because the pen's pump draws the canvas too, and a number that only counted the frame's
        // half of the draws would be half a number (see [`crate::timing`]).
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

            // The ink, from the same description the pen's pump draws with: a frame re-describes it
            // rather than leaving it to the pump, because this is where the sheet it is drawn on was
            // decided.
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

            // The canvas draws itself: the same call the pen's pump makes, on the same thread, with
            // the same description (see [`Self::draw_canvas`]). A frame draws it because a frame is
            // what changed the *sheet* — the zoom, the pan, the page, the paper — and the ink is
            // redrawn along with it rather than waiting for the next reading.
            self.draw_canvas();
        }

        // The desk belongs to the canvas layer when it is there: this element leaves those pixels to
        // it, and the layer's own background shows through them. With no layer the desk is this
        // element's again, which is what the app looked like before the layer existed.
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
            // The note's keyboard, on the screen being painted: Esc for the home screen, the two
            // arrows for the page, and the three commands a person expects to reach without letting
            // go of the pen. Registered here rather than as bindings because a *list* of rows wants
            // the same keys for itself (see [`crate::home`]), and because a listener reads next to
            // what it does. What makes it heard is the focus: this element is the sheet's, and the
            // sheet holds the keyboard whenever it is the screen in front — see
            // [`Self::claim_sheet_keyboard`], which is the rule that hands it over, and without which
            // these keys are swallowed (they were, from opening a note until a rename).
            .on_key_down(
                cx.listener(|app, event: &KeyDownEvent, window, cx| {
                    app.note_key_down(event, window, cx)
                }),
            )
            // The sheet is what the keyboard goes back to when a field goes away — and this is the element
            // that *is* the sheet's keyboard, so a name field that has just closed leaves the focus on a
            // handle that is no longer drawn, which is what [`Self::claim_sheet_keyboard`] fixes on the next
            // frame. The handle is tracked here so that the focus has somewhere in this element to land.
            .track_focus(&self.sheet_focus)
            // The canvas is not drawn here. It is drawn by a renderer of its own, in a surface of
            // its own *behind* this element, and what this element paints is nothing — which is what
            // lets the desk, the sheet and the ink be drawn without a frame hearing about them (see
            // [`crate::ink_layer`]).
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
        sheet_matches, wheel_pan, wheel_zoom_factor, PDF_QUIET_INTERVAL, WHEEL_LINE_HEIGHT,
        WHEEL_LINES_PER_NOTCH, WHEEL_ZOOM_STEP,
    };
    use std::time::{Duration, Instant};

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

}

/// The shadow a sheet casts on the desk: spread, vertical offset beyond the spread, and colour —
/// widest and faintest first.
///
/// Three translucent rectangles, each a little wider than the last and a little fainter, painted in
/// that order: they accumulate into one soft edge. The renderer's own shadows are for *elements* —
/// they cost a layer, and they are painted behind an element's own background, which a rectangle on
/// the desk does not have. A page is drawn by the canvas layer, so its shadow is three rectangles in
/// the canvas it is handed, and three steps read as one blurred edge at the sizes a page is drawn
/// at.
///
/// The sheet is lifted *and* offset downward, the way a sheet of paper lies on a desk: a shadow
/// centred on the paper reads as a glow, and one that is only offset reads as a hard edge.
const PAGE_SHADOW: [(f32, f32, u32); 3] = [
    (12.0, 4.0, 0x0000_0008),
    (7.0, 2.5, 0x0000_000C),
    (3.0, 1.0, 0x0000_0014),
];

/// Describes a frame's canvas: the desk, the page's shadow, and the sheet — in the order painted.
///
/// The canvas layer draws it and nothing else does (see [`crate::ink_layer`]): a shadow is three
/// rectangles in the description, and the renderer is the only thing that knows how to paint one.
///
/// In logical window pixels, which is what the app has: the display's scale travels with the
/// description and the layer applies it (see [`crate::ink_layer::canvas`]).
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
