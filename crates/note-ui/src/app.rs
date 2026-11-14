//! The application view: a command surface over an ink canvas.
//!
//! One `NoteApp` is every screen the app has — the note, the home list, and the two lists of places —
//! and the two pumps that keep them fed. The canvas behind them is drawn by a renderer of the app's
//! own (see [`crate::ink_layer`]), described here and painted elsewhere; what holds the screens
//! together is the element tree that [`Render for NoteApp`] builds.
//!
//! Everything this module decides at length — why the bar floats over the canvas, what the frame loop
//! may and may not wait for, why the status line is not on a clock, what a rung of detail and a
//! rasterised page are worth, and every rule the pen obeys — is written out in `doc/VIEW.md`, which is
//! the map of this module; `doc/ARCHITECTURE.md` is the map of the whole program. What is left here is
//! the line-long "why" a reader wants where they are reading.
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
use gpui_kit::component::theme::Theme;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath, Sizable as _};
use gpui_kit::*;

use crate::bookmarks::{Bookmarks, MarkedPages, Marks};
use crate::canvas::{
    contrast_color, relative_luminance, CanvasSize, CanvasStyle, FrameCap, Ruling, RulingSheet,
    Swatch, HIGHLIGHTER_COLORS, INK_COLORS, PAPER_COLORS,
};
use crate::cursor::{nib_radius_for_width, PenCursor};
use crate::home::Home;
use crate::ink::{paper_of_drawn, InkTransform, Notes, Stroke, Tool, HIGHLIGHTER_WIDTH};
use crate::ink_layer::{Canvas, Ink, Page, Rect};
use crate::input::PenInbox;
use crate::input::Reading;
use crate::keys;
use crate::note::{self, Note, NoteWriter, Report};
use crate::outline::{Outline, OutlinePages};
use crate::pages::{swaps_axes, Pages, Quarters};
use crate::pdf::{PageRequest, PdfDocumentView, Progress, RenderedPage};
use crate::platform::{GhostFeed, InkSurface, PenSource, Platform, Screen, SystemPointer};
use crate::recent::{self, Recent};
use crate::settings::{PenWeight, Settings};
use crate::status_log::StatusLog;
use crate::timing::{measure, Measurement, Timings};
use crate::view::{Fit, Viewport};
use note_format::paper::MemoryMode;

// The two commands a keyboard reaches that the bar also carries.
//
// They are actions — GPUI's unit of a keyboard command — rather than key listeners, so a *binding* is
// what decides which keys mean them, and the bar's buttons and the keyboard end in the same two
// methods. It matters more here than in a text editor: the bar can be hidden, and a hidden bar with no
// keyboard command would have no way to take a stroke back at all.
actions!(cheap_note, [Undo, Redo]);

/// The ladder of bitmap widths a page may be rendered at.
///
/// The rungs a page is quantised *to*; the ceiling of a rung is the note's [`MemoryMode`], which decides
/// how far up this ladder the app may go (see [`MemoryMode::pixel_ceiling`]).
const PDF_PIXEL_WIDTHS: [u32; 4] = [1_024, 1_536, 2_304, 3_456];

/// How far in from a window's side edge a horizontally-swiped trackpad gesture still counts as an edge swipe.
///
/// A trackpad reports where a gesture *began* on macOS, so this band is measured once per gesture
/// rather than per frame. Windows reports its system cursor instead, which may not be the gesture's
/// start — the remaining unreliability in `doc/WINDOWS.md` §3.1. A hundred and twenty logical pixels
/// is wide enough to catch a swipe that starts at the rim without asking a hand to aim, and narrow
/// enough that a scroll started over the page is never mistaken for one — the paper fills the window,
/// so a start further in than this is a reader panning what they are reading.
const EDGE_SWIPE_BAND: f32 = 120.0;

/// How far sideways an edge gesture must travel before it turns the page.
///
/// A trackpad sends a gesture as many small deltas, and the first of them is noise as often as intent: a hand
/// resting near the edge drifts a few pixels. Eighty is about two centimetres, which is more than a resting hand
/// drifts and less than a deliberate swipe. The travel is signed and accumulated, so a swipe that goes out and
/// comes back does not add up to one, and only the axis that was meant counts.
const EDGE_SWIPE_TRAVEL: f32 = 80.0;

/// How much more sideways than vertical an edge gesture has to be before it is taken for a swipe.
///
/// A reader scrolling down the page with a thumb near the window's edge is the case this rules out: that gesture
/// is mostly vertical with a little sideways drift, and a page turn from it would be a page turn nobody asked
/// for. Two-to-one is a generous margin — a diagonal flick still counts — and it is checked against the same
/// accumulated travel the threshold uses.
const EDGE_SWIPE_SLOPE: f32 = 2.0;

/// How far one line of a mouse wheel's scroll moves the sheet, in logical pixels.
///
/// A line is not a distance until someone says how tall it is, and this is that decision: 32 px at
/// 96 DPI, a comfortable line of text and its leading. GPUI has already applied the system's
/// lines-per-notch setting (a notch is three lines by default), so this multiplies what Windows
/// meant rather than guessing at it; the remaining guess is how tall a line is. A platform whose
/// wheel is not how its readers scroll answers [`crate::platform::Platform::wheel_pans`] `false`,
/// and this number is never reached there.
const WHEEL_PAN_PER_LINE: f32 = 32.0;

/// The margin, in logical pixels, between a sheet and the window's edges.
const PAGE_MARGIN: f32 = 24.0;

/// How far the floating bar and the floating page pill sit from the window's edges.
const BAR_MARGIN: f32 = 12.0;

/// How tall the top bar is, in logical pixels, as an estimate.
///
/// Used only by [`bar_line`], and there only while the bar is up and no frame has measured it yet: the
/// edge the last frame laid out is the real line, and everything that asks where the bar ends — the
/// ink, the ghost cursor, the system pointer — asks it there.
const BAR_HEIGHT: f32 = 148.0;

// The two chords the status line names are `keys::chord::MEASURE` and `keys::chord::PALETTE`, written
// out where they are shown by `keys::label` — never typed here, because the words for them are the
// platform's (`Ctrl+M` on Windows, `⌘M` on macOS).

/// How far from the palette's centre a colour is placed, in logical pixels.
///
/// Two rings in two zones around one hub: the tools the hand is reaching for sit in the well inside, and
/// the colours the palette was opened for are on the ring outside. The distances follow from what a tap
/// has to miss — a disc's own radius plus the slack a hand needs ([`PALETTE_REACH`]) — and the two rings
/// are far enough apart that no point on the palette is nearer two controls than one (there is a test for
/// that, and for the gaps within a ring).
const PALETTE_RING_RADIUS: f32 = 104.0;

/// How far from the palette's centre the inner ring's tool chips are placed, in logical pixels.
const PALETTE_TOOL_RADIUS: f32 = 54.0;

/// The diameter of one colour disc, in logical pixels.
const PALETTE_DISC: f32 = 30.0;

/// The diameter of the chip one tool is drawn in, in logical pixels.
///
/// A chip rather than a bare glyph: the palette is read at a glance and at arm's length, and a tool with
/// nothing behind it is a smudge on a white field — which is what the bar's own buttons would be without
/// the bar under them.
const PALETTE_TOOL_DISC: f32 = 38.0;

/// The size of the icon inside a tool chip, in logical pixels.
///
/// A little over half the chip: the proportions the bar's own tool buttons are drawn at, so a tool looks
/// the same in both places.
const PALETTE_TOOL_ICON: f32 = 20.0;

/// The size of the box one colour disc draws in: the disc, and the ring around it.
///
/// What a placement is measured in, because the ring is a border on the box rather than a disc under it
/// (see [`palette_disc`]): placing by the disc's own size would put every ringed disc a hair off centre.
/// The eight is that ring and the white gap inside it, two pixels each.
const PALETTE_DISC_BOX: f32 = PALETTE_DISC + 8.0;

/// The diameter of the hub: the colour the next stroke is written in, at the middle of the palette.
const PALETTE_HUB: f32 = 44.0;

/// How wide the ring of white the hub's colour is set in is, in logical pixels.
const PALETTE_HUB_RING: f32 = 4.0;

/// The least gap between the well's rim and the outermost tool chip in it, in logical pixels.
const PALETTE_WELL_PAD: f32 = 6.0;

/// The diameter of the well the hub and the tools sit in, in logical pixels.
///
/// What makes the palette two zones rather than a dozen discs on one white field: the colours are on the
/// ring and the tools are in the middle, and the well is what says so — the trick the bar plays when it
/// boxes a row of controls instead of leaving them to float.
const PALETTE_WELL: f32 = (PALETTE_TOOL_RADIUS + PALETTE_TOOL_DISC / 2.0 + PALETTE_WELL_PAD) * 2.0;

/// The least gap between the palette and the window's edge, in logical pixels.
const PALETTE_MARGIN: f32 = 8.0;

/// How near a point has to come to a control's centre to be that control, in logical pixels.
///
/// Wider than a disc, because a pen lands where a hand put it rather than on a pixel: the reach is a
/// disc's own radius plus the slack a hand needs, and the gaps between discs are wider than twice this,
/// so a tap between two colours belongs to neither rather than to the nearer of them by a hair.
const PALETTE_REACH: f32 = 24.0;

/// The white the ring of colours is given inside the palette, in logical pixels.
const PALETTE_PAD: f32 = 14.0;

/// The radius of the square the whole palette fits in: the outer ring, half a disc, and that white.
const PALETTE_RADIUS: f32 = PALETTE_RING_RADIUS + PALETTE_DISC / 2.0 + PALETTE_PAD;

/// The palette's measurements in the logical pixels of the screen it is drawn on.
///
/// Every number above is the *design's*, in the 96-DPI logical pixels a Windows display measures in.
/// macOS measures in points, four thirds of one, so the same palette there has to be three quarters of
/// those numbers to be the same size in the hand — at the design's numbers it is a third too large,
/// which is what a 266-pixel ring drawn in points looks like.
///
/// One factor applied to every length is what makes this a *size* rather than a different shape: the
/// rings, the discs, the well and the reach of a tap all move together, so the gaps the palette's own
/// tests pin stay proportional at any unit. The factor comes from the platform
/// ([`crate::platform::Platform::unit_scale`]) because only the platform knows how its logical pixels
/// are measured.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaletteGeometry {
    /// How large one of the design's logical pixels is here: `1.0` on Windows, `0.75` on macOS.
    unit: f32,
}

impl PaletteGeometry {
    /// The geometry for a platform whose logical pixel is `unit` of the design's.
    ///
    /// A unit that is not a positive number is the design's own: a platform that cannot say how its
    /// pixels are measured gets the sizes the tests and the eye were tuned at, not a palette of none.
    fn of(unit: f32) -> Self {
        PaletteGeometry {
            unit: if unit.is_finite() && unit > 0.0 {
                unit
            } else {
                1.0
            },
        }
    }

    /// The half-width of the square the whole palette fits in.
    fn radius(self) -> f32 {
        PALETTE_RADIUS * self.unit
    }

    /// How far out the ring of colours sits.
    fn ring(self) -> f32 {
        PALETTE_RING_RADIUS * self.unit
    }

    /// How far out the ring of tools sits.
    fn tool_ring(self) -> f32 {
        PALETTE_TOOL_RADIUS * self.unit
    }

    /// The diameter of one colour disc.
    fn disc(self) -> f32 {
        PALETTE_DISC * self.unit
    }

    /// The box one colour disc draws in: the disc, plus the ring and the white gap around it.
    ///
    /// The gap and the ring are a hairline's width and do not shrink with the palette — they are the
    /// two pixels each side that [`PALETTE_DISC_BOX`] adds — so this is the disc at this unit plus
    /// those, which is the size the element actually comes out at.
    fn disc_box(self) -> f32 {
        self.disc() + (PALETTE_DISC_BOX - PALETTE_DISC)
    }

    /// The diameter of the chip one tool is drawn in.
    fn tool_disc(self) -> f32 {
        PALETTE_TOOL_DISC * self.unit
    }

    /// The size of the icon inside a tool chip.
    fn tool_icon(self) -> f32 {
        PALETTE_TOOL_ICON * self.unit
    }

    /// The diameter of the hub at the middle.
    fn hub(self) -> f32 {
        PALETTE_HUB * self.unit
    }

    /// How wide the ring of white around the hub's colour is.
    fn hub_ring(self) -> f32 {
        PALETTE_HUB_RING * self.unit
    }

    /// The diameter of the well the hub and the tools sit in.
    fn well(self) -> f32 {
        PALETTE_WELL * self.unit
    }

    /// The least gap between the palette and the window's edge.
    fn margin(self) -> f32 {
        PALETTE_MARGIN * self.unit
    }

    /// How near a tap has to come to a control's centre to be that control.
    fn reach(self) -> f32 {
        PALETTE_REACH * self.unit
    }
}

/// How much of the window's width the status pill may take, as a fraction of it.
const STATUS_PILL_WIDTH: f32 = 0.6;

/// How far below the bar's bottom edge the status pill hangs, in logical pixels.
///
/// The pill belongs to the *top* edge, with the bar, because both say what is open and what it holds;
/// the bottom edge is left to navigation — the page pill and the zoom — so each edge has one job. The
/// gap is `BAR_MARGIN`, the same air the bar itself keeps from the window's top: the pill is drawn by
/// the same hand and reads as part of the same strip.
const STATUS_DROP: f32 = BAR_MARGIN;

/// How much of the selection's colour is laid over the ink a lasso has hold of.
///
/// A wash rather than a coat: the ink keeps its own colour underneath, because a reader who has selected a
/// word to move it still has to be able to *read* the word. Enough to be unmistakable at a glance, which is
/// the whole of what a selection is for — and shared with the loop, so the ring the reader is drawing and the
/// ink it takes are the same colour.
const SELECTION_TINT: f32 = 0.45;

/// How often the housekeeping pump wakes.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(1);

/// How often the counters on the status line are rebuilt while a note is in front.
///
/// The rest of the line changes when the reader changes something; the meters move on their own,
/// and a person watching a stroke should see them move. One second is a readout's pace, not a
/// frame's: rebuilding the line on every wake would make the interface compete with the pen.
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// How long a continuous zoom gesture has to be quiet before it is taken to have ended.
///
/// A pinch delivers a stream of events with no final one, so the end has to be inferred. Long
/// enough that no gap inside a real gesture reaches it — a pinch at 120 Hz sends one every 8 ms — and
/// short enough that the number a reader typed is committed before they look
/// away. A tenth of a second is both: more than ten times the slowest event of a live gesture, and well under
/// the time a hand takes to leave the trackpad.
const GESTURE_SETTLE_INTERVAL: Duration = Duration::from_millis(100);

/// How long a zoom has to be quiet before the detail rung it asked for is built.
///
/// Shorter than [`GESTURE_SETTLE_INTERVAL`], because the two answer different questions. That one waits
/// for a gesture to *end* so a note is written once; this one waits for a gap between two events, so a
/// pinch that crossed a rung stops asking and the build lands in the pause rather than between two
/// readings. Two frames is a real gap at any pointer rate — a 120 Hz digitizer sends one every 8 ms — and
/// a pen that is still moving keeps the old rung, which differs from the new one by under a quarter of a
/// pixel (see `FACET`).
const DETAIL_SETTLE_INTERVAL: Duration = Duration::from_millis(32);

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

/// The page a write belongs to, given the ink in hand: the ink's own page, never the view's index.
///
/// The two numbers are the same everywhere except in the middle of the two commands that move the
/// page list itself — an inserted page and a deleted one. Those close the page they are leaving
/// *before* the list moves, so for a moment [`NoteApp::page_index`] is a page nobody is on, while
/// the ink is still on the page it was written on. A write that read the index there filed that
/// page's whole ink under the page that took its place, which the reader met as the writing of the
/// page before appearing on the new one — sometimes the next time the note was opened (see
/// [`crate::ink::Notes::page`]).
fn page_of_write(ink: &Notes) -> u64 {
    ink.page() as u64
}

/// Whether a note's copy of a page is out of date in a way an append cannot express.
///
/// Two ways, and the second is the one a stroke *count* cannot see:
///
/// * the page has **fewer** strokes than the note holds, which is what an undo, an erase, or a deleted
///   selection leaves behind — the page is no longer a longer version of what is on disk;
/// * the page has been **shifted** — the same strokes, moved — since the number the app last wrote. A lasso
///   dragging a selection changes every point it holds and leaves the count exactly as it was, so a page moved
///   after it was saved would otherwise never be written out at all.
///
/// Nothing else needs a rewrite. A page that has only gained strokes is a longer version of the one on disk,
/// and appending to it is the cheap way to say so (see [STORE.md](../../doc/STORE.md)).
fn page_owes_a_rewrite(count: usize, sent: usize, shift: u64, written: u64) -> bool {
    count < sent || shift > written
}

/// The sheet as the last frame drew it: its size in paper units, and where that was placed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sheet {
    /// The size the paper asks for, in logical pixels at 1:1 — *as it is drawn*, so a page the reader
    /// has turned is the other way round here.
    paper: (f32, f32),
    /// Where the sheet's top-left corner was drawn, in window logical pixels.
    origin: (f32, f32),
    /// The window it was drawn into.
    window: (f32, f32),
    /// The zoom it was drawn at.
    zoom: f32,
    /// How far the page has been turned, in quarter turns clockwise.
    rotation: Quarters,
}

impl Sheet {
    /// The size it was drawn at, after the zoom.
    fn drawn(&self) -> (f32, f32) {
        (self.paper.0 * self.zoom, self.paper.1 * self.zoom)
    }

    /// The paper's own size, in the paper's units: what the ink is written in.
    ///
    /// The drawn size turned back, because the ink is in the page's coordinates and not the screen's:
    /// a page turned a quarter is drawn `(h, w)` where its own paper is `(w, h)`, and the ink written
    /// on it is measured on the paper. It is also the box a reading is checked against — see
    /// [`InkTransform::on_paper`] — which is why getting it the wrong way round would put ink on the
    /// desk at the ends of a turned page.
    fn ink_paper(&self) -> (f32, f32) {
        if swaps_axes(self.rotation) {
            (self.paper.1, self.paper.0)
        } else {
            self.paper
        }
    }

    /// Where a reading in physical client pixels lands on the sheet.
    fn transform(&self, scale: f32, bar: f32) -> InkTransform {
        InkTransform {
            scale,
            zoom: self.zoom,
            origin: self.origin,
            paper: self.ink_paper(),
            bar,
            rotation: self.rotation,
        }
    }

    /// The part of the sheet that is on screen, in the paper's own coordinates.
    ///
    /// A *rectangle* of the paper, because that is what the culling test takes — and it is the box
    /// around the window's four corners turned back onto the paper, because a page the reader has
    /// turned puts the screen's rectangle at an angle to the paper's axes. A box and not a polygon:
    /// what this decides is whether a stroke is worth *drawing*, and including a stroke that is only
    /// near the screen costs one draw call while excluding one that is on it costs a missing line.
    fn visible(&self) -> [f32; 4] {
        let zoom = if self.zoom.is_finite() && self.zoom > 0.0 {
            self.zoom
        } else {
            1.0
        };

        let paper = self.ink_paper();
        let corner = |window: (f32, f32)| {
            paper_of_drawn(
                (
                    (window.0 - self.origin.0) / zoom,
                    (window.1 - self.origin.1) / zoom,
                ),
                paper,
                self.rotation,
            )
        };

        let corners = [
            corner((0.0, 0.0)),
            corner((self.window.0, 0.0)),
            corner((0.0, self.window.1)),
            corner(self.window),
        ];

        [
            corners.iter().fold(f32::MAX, |low, at| low.min(at.0)),
            corners.iter().fold(f32::MAX, |low, at| low.min(at.1)),
            corners.iter().fold(f32::MIN, |high, at| high.max(at.0)),
            corners.iter().fold(f32::MIN, |high, at| high.max(at.1)),
        ]
    }
}

/// A command that cannot be taken back, waiting to be confirmed.
///
/// The two commands a note has no undo for: clearing a page's ink, and deleting the page itself. Both are *only*
/// reachable through the box the app puts up (see [`confirmed`]) — a button that acted on the first click is how a
/// page of handwriting disappears by accident, and a note is not something a reader expects to have to be careful
/// with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confirmation {
    /// Empty the page in front: every stroke on it, and its history with them.
    ClearInk,
    /// Delete the page in front, and everything on it.
    DeletePage,
}

impl Confirmation {
    /// What the box says: the heading, the line under it, and the two words on its buttons.
    ///
    /// Two strings rather than one with a line break in it, because a heading and its explanation are two elements:
    /// the heading is what a reader reads, and the line under it is there for the reader who wants to know exactly
    /// what goes.
    fn words(self) -> Words {
        match self {
            Confirmation::ClearInk => Words {
                heading: "Clear the ink on this page?",
                detail:
                    "Every stroke on it goes, and its history with them. This cannot be taken back.",
                agree: "Clear the page",
                refuse: "Keep it",
            },
            Confirmation::DeletePage => Words {
                heading: "Delete this page?",
                detail: "Everything on it goes, and the page itself. This cannot be taken back.",
                agree: "Delete the page",
                refuse: "Keep it",
            },
        }
    }
}

/// What a box says: the question, and the two answers it offers.
///
/// "Keep it" rather than "Cancel" on the refusing side: a reader asked about their own handwriting should be able to
/// answer without working out what cancel means here, and the word that says what happens to the page is the word
/// that gets read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Words {
    heading: &'static str,
    detail: &'static str,
    agree: &'static str,
    refuse: &'static str,
}

/// What an answer to a confirmation means: the command to run, or nothing at all.
///
/// A free function, and the whole of the state machine — which is why it is here rather than inside the app: it is
/// the one place "yes" becomes an action, so the rule that a destructive command is reachable *only* through an
/// answer is checkable without a window, and a command that could be reached another way would have to be written
/// into this match to be reachable at all.
fn confirmed(what: Confirmation, yes: bool) -> Option<Confirmation> {
    yes.then_some(what)
}

/// The palette that opens at the pen: where it is, and what the keyboard is on.
///
/// Where it *is* is fixed where it opened, and never follows the pen afterwards: a ring that moved
/// with the hand is a ring the hand cannot aim at, and the pen keeps reporting while it is up (see
/// [`NoteApp::palette_from_pen`]). What it *holds* is deliberately not stored here — the ring is the
/// palette of the tool in hand ([`palette_colours`]), so a tool taken from the inner ring changes the
/// colours around it without the palette being rebuilt.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Palette {
    /// The middle of the palette, in logical window pixels.
    centre: [f32; 2],
    /// The colour the keyboard is on: what Enter takes, and what the ring's own ring marks.
    ///
    /// It starts on the colour in hand, the way a chooser opens on the value it holds, and the arrows
    /// walk it around the ring (see [`spun`]).
    chosen: usize,
}

/// The tools the palette's inner ring offers, in the order a clock face reads them.
///
/// Three, and not the four the bar carries: the lasso is a *sweep* — what it takes is what a loop
/// encloses — and a ring of buttons a tap is aimed at has no room for a gesture. It stays where a
/// sweep belongs, on the bar.
const PALETTE_TOOLS: [Tool; 3] = [Tool::Pen, Tool::Highlighter, Tool::Eraser];

/// What a press on the palette landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PalettePart {
    /// The colour at this place on the outer ring.
    Colour(usize),
    /// The tool at this place on the inner ring.
    Tool(usize),
}

/// Where one control of a ring sits, as the point its *centre* goes at, in logical pixels.
///
/// The angle is a clock's: place 0 is at twelve o'clock and the ring runs clockwise, so a table's
/// first colour is the one at the top and the order a person reads the ring in is the order the table
/// is written in. `radius` is how far out from `centre` the ring is drawn.
fn palette_point(centre: [f32; 2], radius: f32, place: usize, count: usize) -> [f32; 2] {
    let step = std::f32::consts::TAU / count.max(1) as f32;
    let angle = -std::f32::consts::FRAC_PI_2 + step * place as f32;

    [
        centre[0] + radius * angle.cos(),
        centre[1] + radius * angle.sin(),
    ]
}

/// Which control of the palette a point is, if it is one at all.
///
/// Distance rather than the element under the pointer, because the pen is captured by the *window* and
/// never reaches the element tree (see [`crate::pen`]): a tap has to be answered by where it landed.
/// The same rule answers a mouse's click and the keyboard's place on the ring, so all three reach a
/// colour the same way. The outer ring is asked first, because a colour is what a hand reaching into
/// this palette is reaching for; a point within reach of nothing is the palette's own background, and
/// putting the palette away is what a press on that means (see [`NoteApp::palette_overlay`]).
fn palette_at(
    geometry: PaletteGeometry,
    centre: [f32; 2],
    point: [f32; 2],
    colours: usize,
) -> Option<PalettePart> {
    let reach = geometry.reach();
    let nearest = |radius: f32, count: usize| {
        (0..count)
            .map(|place| {
                let at = palette_point(centre, radius, place, count);
                let (dx, dy) = (at[0] - point[0], at[1] - point[1]);

                (place, (dx * dx + dy * dy).sqrt())
            })
            .filter(|(_, away)| *away <= reach)
            .min_by(|one, other| one.1.total_cmp(&other.1))
            .map(|(place, _)| place)
    };

    nearest(geometry.ring(), colours)
        .map(PalettePart::Colour)
        .or_else(|| nearest(geometry.tool_ring(), PALETTE_TOOLS.len()).map(PalettePart::Tool))
}

/// Where the palette may be centred and still be on the screen.
///
/// The pen is at the window's edge as often as it is in the middle — the last line of a page, a note
/// written down the margin — and a ring half off the screen is a ring with half its colours. A window
/// too small to hold the palette puts it in the middle and lets it overhang, which is the one case
/// where there is no better answer.
fn palette_centre(geometry: PaletteGeometry, near: [f32; 2], window: [f32; 2]) -> [f32; 2] {
    let (radius, margin) = (geometry.radius(), geometry.margin());
    let keep = |at: f32, extent: f32| {
        let (low, high) = (radius + margin, extent - radius - margin);

        if low > high {
            extent / 2.0
        } else {
            at.clamp(low, high)
        }
    };

    [keep(near[0], window[0]), keep(near[1], window[1])]
}

/// The next place on a ring of `count` controls, `step` controls along.
///
/// Wrapping at both ends: a ring has no end, so no arrow is a key that does nothing (see
/// [`NoteApp::palette_spin`]).
fn spun(from: usize, step: isize, count: usize) -> usize {
    (from as isize + step).rem_euclid(count.max(1) as isize) as usize
}

/// The palette the tool in hand writes with: the pen's twelve, or the marker's four.
///
/// The two tables the bar's Ink row draws, because a colour means one thing in this app — and the
/// *pen's* for the tools that write no colour at all: an eraser has no palette of its own, and a
/// colour taken from behind one is a colour to write in, so taking it puts the pen back in hand (see
/// [`NoteApp::palette_colour`]).
fn palette_colours(tool: Tool) -> &'static [Swatch] {
    match tool {
        Tool::Highlighter => &HIGHLIGHTER_COLORS,
        Tool::Pen | Tool::Eraser | Tool::Lasso => &INK_COLORS[..],
    }
}

/// Where a colour sits in a palette, or the first place when the palette does not hold it.
///
/// The colour in hand is in the ring for every tool whose palette the ring is (see [`palette_colours`]),
/// so the second answer is the one no tool leaves: the keyboard has to be *somewhere* for the arrows
/// to walk from, and the first place is the one a reader can find without being told.
fn palette_in_hand(colours: &[Swatch], colour: u32) -> usize {
    colours
        .iter()
        .position(|swatch| swatch.color == colour)
        .unwrap_or(0)
}

/// What an inner-ring button carries: the id its element draws with, its icon, and the words for it.
///
/// `None` for a tool the ring does not offer (see [`PALETTE_TOOLS`]), which is a fact about this ring
/// rather than a mistake: what a tap can take is what has a face here.
fn palette_face(tool: Tool) -> Option<(&'static str, IconName, &'static str)> {
    match tool {
        Tool::Pen => Some(("palette-tool-pen", IconName::Pen, "Pen")),
        Tool::Highlighter => Some((
            "palette-tool-highlighter",
            IconName::Highlighter,
            "Highlighter",
        )),
        Tool::Eraser => Some(("palette-tool-eraser", IconName::Eraser, "Eraser")),
        Tool::Lasso => None,
    }
}

/// The element id one of the palette's colour discs draws with.
///
/// An id of its own rather than the bar's swatch id for the same colour: element ids address the whole
/// element tree, and the palette floats *over* the bar, so the two are drawn at the same moment —
/// sharing an id would silently share their hover and press state. The bar's ids come from the colour
/// tables for the same reason (see [`crate::canvas::Swatch`]).
fn palette_id(swatch: &Swatch) -> ElementId {
    ElementId::Name(format!("palette-{}", swatch.id).into())
}

/// The pen gesture in play, and where it began.
///
/// Kept beside the field that holds it because it is the *state* of a gesture rather than a thing the app
/// owns: the mode, the point the nib went down at, and — for both gestures — the view's own numbers as they
/// were when the gesture began. Storing the starting values rather than accumulating a delta reading by
/// reading is what makes a gesture absolute: a hand that moves up and back down returns the sheet to the zoom
/// and the place it started at, because each reading is asked about against the first.
#[derive(Clone, Copy, Debug)]
struct PenGesture {
    /// What the pen is doing.
    mode: crate::pen_gesture::Gesture,
    /// Where the nib first touched, in logical window pixels.
    origin: (f32, f32),
    /// The sheet's zoom when the gesture began.
    zoom: f32,
    /// The sheet's pan when the gesture began, for a gesture that moves it.
    ///
    /// A pan is *set* from here rather than added to, because [`Viewport::pan_by`] accumulates: a nib that
    /// travels 50 pixels would otherwise be added to the sheet on every reading, so a hand moving at a steady
    /// pace would drag the page faster and faster (see [`NoteApp::pan_by_pen`]).
    pan: (f32, f32),
    /// Where the hand is taken to be, with the digitizer's own trembling set aside.
    ///
    /// The gesture is measured from the nib's *settled* position rather than from the raw readings, so a
    /// fan shaking the tablet does not show up as the sheet shivering. See [`crate::pen_gesture::PenJitter`],
    /// and [`Settings::pen_jitter`] for the switch that turns it off.
    jitter: crate::pen_gesture::PenJitter,
    /// Whether this gesture sets the tremor aside at all — the note's answer when the nib went down.
    ///
    /// Read once rather than per reading so that flipping the switch mid-gesture cannot move the sheet under
    /// a hand that has not: the filter's settle point is a *state*, and changing whether it is consulted
    /// halfway through would jump the pan by however far the tremor had been held back.
    smoothed: bool,
}

impl PenGesture {
    /// The pan this gesture asks for while the nib is at `at`, in window logical pixels.
    ///
    /// The gain is applied because a hand only has one screen's travel in it and a page is often longer than
    /// that (see [`crate::pen_gesture::PEN_PAN_GAIN`]).
    ///
    /// **Out of range included, deliberately.** The answer is what the hand asked for, not what the layout can
    /// show: the clamp belongs to [`Viewport::origin`], which applies it when it draws. Clamping here as well
    /// throws away where the hand actually is, and no later reading can bring it back — each one is measured
    /// from the gesture's origin rather than from the stored value — so a hand that took the sheet past its
    /// edge and came back would watch the paper sit still through the whole return and then jump. A pure
    /// function so that this is testable without a window: the gesture path itself needs one.
    fn panned_to(&self, at: (f32, f32)) -> (f32, f32) {
        let delta = (at.0 - self.origin.0, at.1 - self.origin.1);

        (
            self.pan.0 + delta.0 * crate::pen_gesture::PEN_PAN_GAIN,
            self.pan.1 + delta.1 * crate::pen_gesture::PEN_PAN_GAIN,
        )
    }

    /// Where the hand is, given the reading at `at`, with the digitizer's trembling set aside.
    ///
    /// This is the one place a gesture's readings pass through the filter, so both of them see the same
    /// position: a pan measures from it and a zoom measures the rise from it. Off ([`Self::smoothed`] false)
    /// the reading is the hand, exactly as it was before the filter existed.
    fn settled_at(&mut self, at: (f32, f32)) -> (f32, f32) {
        if !self.smoothed {
            return at;
        }

        self.jitter.at(at)
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
    /// How many times each page had been *shifted* when it was last written: see [`page_owes_a_rewrite`].
    ///
    /// Kept beside `saved` because the two are the same fact about two different edits: what the note holds,
    /// and whether what it holds is still what is on disk. A move is the edit that only this one can see.
    shifted: BTreeMap<u64, u64>,
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
    pen: Box<dyn PenSource>,
    /// Which keys the operating system says are held, asked once per pen batch (see [`crate::pen_gesture`]).
    ///
    /// `None` is a machine where that could not be asked at all, which is the same as nothing being held:
    /// the pen then writes exactly as it did before there were gestures.
    held_keys: Option<device_query::DeviceState>,
    /// The pen gesture in play, and what it has moved since it began.
    ///
    /// A gesture is a *mode* the nib is in rather than a sequence of events: the pen goes down with space
    /// held and stays a pan until the nib lifts or the key does. Both directions are watched, because a
    /// reader who lets go of the key mid-stroke means to start writing, and a reader who presses it mid-
    /// stroke means to start moving — neither should have to lift the pen to be obeyed.
    pen_mode: Option<PenGesture>,
    /// The hook that hides the system pointer while the pen has a cursor of its own.
    system_cursor: Option<Box<dyn SystemPointer>>,
    /// The window the pen's ghost cursor is drawn in, outside the frame.
    cursor: Option<Arc<dyn GhostFeed>>,
    /// The canvas, drawn by a renderer of the app's own rather than by a frame.
    ink_layer: Option<Box<dyn InkSurface>>,
    /// Counts the rebuilds of the ink's own outlines.
    ink_revision: u64,
    /// When the canvas layer last presented: the rate the ink reaches the screen at, which is not the frame rate.
    last_present: Option<Instant>,
    /// Whether the canvas has changed since the last frame the layer presented.
    ///
    /// A drawable that is not ready is a frame to skip, not a thread to park: the pen's pump leaves
    /// this set and the display pump tries again (see [`Self::present_canvas`]).
    canvas_dirty: bool,
    /// When the status line was last rebuilt on the clock, so its counters move while the reader
    /// is only watching (see [`STATUS_REFRESH_INTERVAL`]).
    status_refreshed_at: Instant,
    /// The status line's log file: one line a second while a note is in front.
    status_log: StatusLog,
    /// The machine's own counters, when this platform can read them (see [`crate::platform::SystemStats`]).
    ///
    /// `None` on a platform with no implementation, which is a status line without a hardware clause
    /// rather than an app that cannot start.
    system_stats: Option<Box<dyn crate::platform::SystemStats>>,
    /// What the last sample of those counters said, and the clause it came to.
    ///
    /// Held rather than asked for where the line is composed, because composing happens far more often
    /// than sampling may (see [`STATUS_REFRESH_INTERVAL`]): the clause is the *sample*, and the line
    /// repeats it until the next one. Empty when the status line is hidden, so a hidden line costs no
    /// sampling at all.
    vitals: String,
    /// The canvas as the last frame described it: the desk, the page's shadow, and the sheet.
    canvas: Canvas,
    /// The colour the ink a lasso has hold of is shown in: the wash over the ink, and the loop's own mark.
    ///
    /// Kept rather than read where it is used, because it *is* used outside a frame: the pen's pump draws the
    /// canvas too, and has no theme to read (see [`Self::describe_ink`]). The theme's accent, because a
    /// selection is the one thing on the page that is the app's mark rather than the reader's ink.
    selection_colour: Hsla,
    /// The page being shown.
    page_index: usize,
    /// The window's DPI scale factor, captured each frame.
    scale: f32,
    /// How large one of the design's logical pixels is on this platform, as a factor: what the palette
    /// is drawn at (see [`crate::platform::Platform::unit_scale`]).
    unit_scale: f32,
    /// Whether a wheel's line-based scroll moves the sheet (see [`crate::platform::Platform::wheel_pans`]).
    ///
    /// A wheel reports lines and a trackpad reports pixels; the view pans in pixels, so a platform
    /// that answers `true` here gets its wheel's lines converted at [`WHEEL_PAN_PER_LINE`]. The
    /// answer is the platform's because it is about what the machine's readers scroll with, and the
    /// view may not name an operating system.
    wheel_pans: bool,
    /// How large the sheet is drawn, and where it sits in the window.
    view: Viewport,
    /// The desk colour the canvas was last described with.
    ///
    /// Kept because the description is now written from two places — the frame, which reads the theme, and the
    /// pump, which has no `cx` to ask — and both have to write the *same* desk or the pump's frame would paint
    /// the window in a colour the theme no longer chose.
    desk: Hsla,
    /// The scroll readings the trackpad has left for the frame that will show them, in logical pixels.
    ///
    /// A trackpad reports its movement faster than the screen takes frames — a hundred or so readings a
    /// second against sixty — and a reading spent on its own is a pan nobody sees and a canvas asked to
    /// draw a place the hand has already left. Collected here and spent once per frame, the sheet moves by
    /// exactly the distance the hand did, and the canvas is asked for one frame's worth of it.
    scroll_waiting: (f32, f32),
    /// Whether a frame has been asked for to spend [`Self::scroll_waiting`].
    scroll_asked: bool,
    /// The edge swipe in flight, or `None` when no gesture is being read as one.
    ///
    /// A page turn is a *gesture*, not a reading, so it needs somewhere to keep the gesture's own state between
    /// events: where it began, how far it has travelled, and whether it has already turned a page. See
    /// [`EdgeSwipe`] for the rules, and [`Self::on_scroll`] for the one place it is fed.
    edge_swipe: Option<EdgeSwipe>,
    /// When the last scroll reading arrived, which is the only end an edge swipe has.
    ///
    /// The platform says where a gesture is, never when it stopped, so the end is a gap in the readings — the
    /// same gap [`GESTURE_SETTLE_INTERVAL`] measures for a pinch, and the pump reads both.
    scrolled_at: Option<Instant>,
    /// Whether the sheet was moved since the last frame, by a gesture rather than by ink.
    ///
    /// The canvas draws itself on the pen's own wake, so a stroke needs no frame; a *pan* does, because the
    /// bar and the page pill describe where the sheet is and are drawn by the frame. Set by the two paths that
    /// move the view and cleared by the frame that spends them, so a gesture draw one frame per changed wake
    /// rather than one per reading.
    view_moved: bool,
    /// Whether the page in front has been asked for a detail rung it is not drawn at yet.
    ///
    /// A zoom asks for one from `render` and the pump builds it between frames (see the note at the
    /// `set_zoom` call): a rung is 23.7 ms on a page of thousands of strokes, and a frame cannot pay it.
    /// Cleared when the build happens, so a still page asks for nothing.
    detail_owed: bool,
    /// When the last continuous zoom event arrived, or `None` when no zoom gesture is in flight.
    ///
    /// A pinch is a *stream*: the events say where the zoom is now, and the platform does not say
    /// when they have stopped. So the end is a gap — the pump commits the zoom to the note once a whole interval
    /// has passed with nothing new, which is the write [`Self::finish_zoom`] does and the one thing a stream of
    /// events must not do per event.
    zoom_settles_at: Option<Instant>,
    /// Where each page was being read, for as long as the note in hand is open.
    ///
    /// The *page's* place rather than the reader's: turning to a page added at the end of a note opens that page at
    /// its top rather than at whatever part of the page before it the reader had scrolled to, and turning back to a
    /// page one was in the middle of comes back to the middle (see [`Self::turn_to`]).
    ///
    /// Emptied when a note is opened and when the pages are renumbered — a place remembered for page 4 belongs to the
    /// page that is now 3 when one in front of it is deleted — because what it holds is a position in a list rather
    /// than anything the note knows about itself.
    pans: HashMap<usize, (f32, f32)>,
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
    /// The pen's most recent reading, whatever consumed it.
    ///
    /// The ghost is fed every batch on the pen thread, so the view's own cursor rules have to see the
    /// same reading even when the batch went to a control instead of to the page: the palette takes
    /// readings before they reach the ink, and a cursor fed from the ink would freeze at the moment the
    /// palette opened.
    last_pen: Option<Reading>,
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
    /// What a destructive command is waiting for an answer to, if anything is on screen.
    ///
    /// The two commands a note cannot undo ask first, and this is the question being asked: while it is here, the
    /// pen has no cursor and lays no ink, and the keyboard belongs to the box (see [`Confirmation`]).
    confirm: Option<Confirmation>,
    /// The palette at the pen, while it is up.
    ///
    /// Like a question, it is a state the *pen* obeys: while it is here the nib lays no ink and a
    /// reading that lands on a colour is a choice rather than a stroke (see [`Self::consume_ink`] and
    /// [`Self::palette_from_pen`]).
    palette: Option<Palette>,
    /// The field the name is typed into on the sheet.
    name_input: Entity<InputState>,
    /// The keyboard the sheet gets back when the field goes away.
    sheet_focus: FocusHandle,
    /// Whether the note screen held the keyboard on the last frame it was in front.
    sheet_has_keyboard: bool,
    /// What the field being typed into says — Enter keeps the name, a click away leaves it as it was.
    _name_events: Subscription,
    /// The bar's paper-size chooser.
    sheet_select: Entity<SelectState<Vec<String>>>,
    /// The bar's ruling chooser. Held for the same reason as [`NoteApp::sheet_select`].
    rule_select: Entity<SelectState<Vec<String>>>,
    /// The bar's pen-weight chooser: the pen the note in hand is written with.
    ///
    /// Its labels are *built* rather than spelled out, because each one is the pen's thickness in millimetres
    /// ([`PenWeight::describe`]) and that belongs to the note in hand: hence `String` rather than `&'static str`.
    pen_select: Entity<SelectState<Vec<String>>>,
    /// The bar's frame-cap chooser: the most frames a second the canvas will draw.
    ///
    /// Not a display mode — see [`FrameCap`] — so it sits with the render options rather than with the
    /// paper: what it caps is the canvas's own work.
    frame_select: Entity<SelectState<Vec<String>>>,
    /// The bar's memory-mode chooser: whether a document may spend memory freely or has to be lean.
    ///
    /// Beside the frame cap because it is the same kind of answer — an allocation the reader is choosing
    /// how much of to spend — and a chooser rather than a switch because there are two named modes with
    /// different costs, not one thing that is on or off. See [`MemoryMode`].
    memory_select: Entity<SelectState<Vec<String>>>,
    /// Whether one of the bar's choosers has its list open.
    ///
    /// The list is a popup that hangs *below* the box, over the page — so the place the reader is aiming
    /// at is page, by [`Self::bar_edge`]'s measure, while the thing they are aiming at is a control. That
    /// is the palette's case over again (see [`Self::ghost_suppressed`]): the ghost goes and the system
    /// pointer is the cursor, or a list of options is a list with no cursor on it.
    ///
    /// The chooser's own state cannot answer this — `SelectState` keeps its `open` flag private — so it is
    /// tracked here from every way a list is opened or closed: the click on the box, a press anywhere else,
    /// a confirmed choice, and Escape. See [`Self::chooser_toggled`] and [`Self::chooser`].
    chooser_open: bool,
    /// The field the zoom is typed into, between the two steps.
    ///
    /// A field rather than a third pair of buttons, because the zoom a person wants is usually a *number* they already
    /// have in mind — the size they read at, or the number the page was laid out at — and stepping to 60% is a dozen
    /// presses. What it shows is the truth about the view ([`Self::sync_choosers`]), whether the view changed by a
    /// press, a pinch, a fit or a note being opened.
    zoom_input: Entity<InputState>,
    /// What the zoom field says: Enter takes the number, a click away leaves the view's own.
    _zoom_events: Subscription,
    /// The screen that offers what was opened recently, and the app starts on.
    home: Home,
}

impl NoteApp {
    /// Builds the view around what the platform lent it, and starts the frame loop.
    ///
    /// `platform` is the whole of what a machine hands over — a pen, a ghost cursor, a pointer hook and
    /// a surface (see [`Platform`]) — and this constructor keeps all four for as long as the app lives.
    /// That is what stops the pen thread and closes the ghost's window when the app goes away, and it is
    /// the reason the view never names a platform: what it holds are the traits.
    pub fn new(
        window: &mut Window,
        start: Option<PathBuf>,
        platform: Platform,
        cx: &mut Context<Self>,
    ) -> Self {
        // Nothing to load and nowhere to load it from — no note is open — so this is the shipped set. It
        // is not a *global* set: whatever is opened below takes it over (see [`crate::settings`]).
        let settings = Settings::default();
        let mut message = String::new();

        // The services, taken apart again. The pen's thread and the ghost's window are already
        // running by the time this returns, because installing them is the platform's job (see
        // `apps/windows/src/platform`); what is kept here is the handle on each.
        let Platform {
            ink: ink_layer,
            ghost: cursor,
            pointer: system_cursor,
            unit_scale,
            wheel_pans,
            pen,
            stats: system_stats,
        } = platform;

        // What the pen says about itself, or the reason it could not be read — a pen that cannot be
        // read is a pen that draws with the mouse, and the status line is where a person is told so.
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
        let pen_select = choice_of(
            // Each box carries the pen's thickness in millimetres rather than a name: a weight is a *word*, and what
            // a person choosing a pen wants to know is how thick the line will be (see [`PenWeight::describe`]).
            PenWeight::ALL
                .iter()
                .map(|weight| weight.describe(settings.no_pressure_width))
                .collect(),
            PenWeight::ALL
                .iter()
                .position(|weight| *weight == settings.pen_weight),
            window,
            cx,
        );

        // The frame cap's labels are their own names — `60 Hz` and the rest — because the number *is* what
        // a person choosing one wants to know.
        let frame_select = choice_of(
            FrameCap::ALL
                .iter()
                .map(|cap| cap.label().to_string())
                .collect(),
            FrameCap::ALL
                .iter()
                .position(|cap| *cap == settings.frame_cap),
            window,
            cx,
        );

        // The memory mode's labels are its own names — `Room` and `Lean` — and the list order is
        // `MemoryMode::ALL`, which starts at `Room` because that is the default a fresh note gets.
        let memory_select = choice_of(
            MemoryMode::ALL
                .iter()
                .map(|mode| mode.label().to_string())
                .collect(),
            MemoryMode::ALL
                .iter()
                .position(|mode| *mode == settings.memory_mode),
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

        // The field the zoom is typed into, and the two things it can say: Enter takes the number, and losing focus —
        // clicking anywhere else — puts the view's own back. It is filled from the view rather than from the settings,
        // because the view is what has been clamped and fitted.
        let zoom_input = cx.new(|cx| InputState::new(window, cx).placeholder(ZOOM_PLACEHOLDER));
        zoom_input.update(cx, |input, cx| {
            input.set_value(zoom_text(view.zoom()), window, cx);
        });
        let _zoom_events = cx.subscribe_in(
            &zoom_input,
            window,
            |app, _, event: &InputEvent, window, cx| app.zoom_event(event, window, cx),
        );
        let sheet_focus = cx.focus_handle();

        let mut app = NoteApp {
            settings,
            ink: Notes::new(),
            note: None,
            writer: None,
            saved: BTreeMap::new(),
            shifted: BTreeMap::new(),
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
            held_keys: crate::pen_gesture::keys(),
            pen_mode: None,
            system_cursor,
            cursor,
            ink_layer,
            ink_revision: 0,
            last_present: None,
            canvas_dirty: false,
            status_refreshed_at: Instant::now(),
            status_log: StatusLog::open(),
            system_stats,
            vitals: String::new(),
            canvas: Canvas::default(),
            selection_colour: cx.theme().accent.opacity(SELECTION_TINT),
            page_index: 0,
            scale: window.scale_factor(),
            unit_scale,
            wheel_pans,
            view,
            desk: cx.theme().background,
            scroll_waiting: (0.0, 0.0),
            scroll_asked: false,
            edge_swipe: None,
            scrolled_at: None,
            view_moved: false,
            detail_owed: false,
            zoom_settles_at: None,
            pans: HashMap::new(),
            sheet: Sheet::default(),
            bar_bottom: Rc::new(Cell::new(None)),
            timings: Arc::new(Timings::default()),
            last_pump: None,
            pending_pdf: None,
            last_ink_at: Instant::now(),
            last_pen: None,
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
            confirm: None,
            palette: None,
            name_input,
            sheet_focus,
            sheet_has_keyboard: false,
            _name_events,
            zoom_input,
            _zoom_events,
            sheet_select,
            rule_select,
            pen_select,
            frame_select,
            memory_select,
            chooser_open: false,
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
            |app, _, event: &SelectEvent<Vec<String>>, cx| {
                // A confirmed choice closes the list, and that is the cursor's answer too: no press
                // outside the box arrives to say so (see [`NoteApp::chooser_toggled`]).
                app.chooser_toggled(false, cx);

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
            |app, _, event: &SelectEvent<Vec<String>>, cx| {
                app.chooser_toggled(false, cx);

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
            |app, _, event: &SelectEvent<Vec<String>>, cx| {
                app.chooser_toggled(false, cx);

                if let SelectEvent::Confirm(Some(label)) = event {
                    if let Some(weight) =
                        PenWeight::from_label(label, app.settings.no_pressure_width)
                    {
                        app.set_pen_weight(weight, cx);
                    }
                }
            },
        )
        .detach();

        cx.subscribe(
            &app.frame_select,
            |app, _, event: &SelectEvent<Vec<String>>, cx| {
                app.chooser_toggled(false, cx);

                if let SelectEvent::Confirm(Some(label)) = event {
                    if let Some(cap) = FrameCap::from_label(label) {
                        app.set_frame_cap(cap, cx);
                    }
                }
            },
        )
        .detach();

        cx.subscribe(
            &app.memory_select,
            |app, _, event: &SelectEvent<Vec<String>>, cx| {
                app.chooser_toggled(false, cx);

                // A label with no mode behind it — the box has been cleaned — is not a choice, and is
                // ignored: a note always has a memory mode, as it always has a paper size.
                if let SelectEvent::Confirm(Some(label)) = event {
                    if let Some(mode) = MemoryMode::from_label(label) {
                        app.set_memory_mode(mode, cx);
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

                if batch.readings.is_empty() {
                    return;
                }

                // Before anything consumes it: the ghost has already been offered this batch on the pen
                // thread, and the pointer rule asks where the pen is — whether the batch becomes ink, a
                // palette choice, or nothing at all.
                app.last_pen = batch.readings.last().copied();

                // The pen is captured by the *window* rather than by anything on it, so a reading that
                // reached the ink with a screen in front would be ink on a page nobody is looking at. The
                // capture is left alone and the readings are dropped here, where they become strokes — and a
                // reading that arrives under a confirmation box is dropped for the same reason: nothing is
                // written while a question is waiting for an answer.
                if app.home_is_open() || app.asking() {
                    // The ghost is suppressed with the screen up, so the pointer has to be the system's —
                    // and it has to know the pen moved while readings are being dropped here.
                    app.follow_pen_with_pointer();
                    return;
                }

                // The palette at the pen: with it up a reading is a *choice* rather than a stroke —
                // this is the one control the pen is aimed at instead of written on — and either way
                // no ink is laid while it is there (see [`NoteApp::palette_from_pen`]).
                if app.palette_is_open() {
                    app.palette_from_pen(&batch.readings, cx);
                    // The ghost is suppressed while the palette is up and the *system* pointer is the
                    // cursor over the ring (see [`NoteApp::ghost_suppressed`] and the way a question is
                    // handled below), so a reading here only has to keep the pointer's own answer in
                    // step — it lays no ink and draws no ghost, so the canvas needs no repaint.
                    app.follow_pen_with_pointer();
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
                let laid_ink = app.consume_ink(&batch.readings, cx);
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

                // The canvas draws itself here, on the pen's own wake: it is the one part of the window that
                // changes between readings, and a frame costs several times what the ink does (see
                // [`Self::draw_canvas`]).
                //
                // A reading that laid no ink still moved the pen — a hover, the mouse crossing the sheet —
                // and the pen's mark is drawn by this canvas now: a hover that is not drawn is a cursor
                // that does not move. What the early return this replaces was protecting is the
                // *interface*, and that is still protected: the counters are only worth a frame when ink
                // was laid.
                app.draw_canvas();

                // A frame is only for the *interface*, and only when the interface changed. The canvas was
                // drawn above, on this wake, and the compositor shows the newest canvas it is given — so a
                // notify here would redraw the whole window, canvas included, a second time, which is the one
                // thing a stroke cannot afford.
                //
                // A gesture that moved the sheet is the other case that needs a frame: the sheet's *place* is
                // drawn by the canvas but the page's own geometry is not, and a pan that never asked for a
                // frame would leave the bar and the page pill describing a place the sheet had left. The flags
                // are read here because they are set by the two paths that move the view — the pen's gesture
                // and the trackpad — and cleared by the frame that spends them.
                if app.view_moved {
                    app.view_moved = false;
                    cx.notify();
                } else if batch.readings.iter().any(|reading| {
                    matches!(
                        reading.phase,
                        crate::input::Phase::Up | crate::input::Phase::Cancel
                    )
                }) {
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

    /// Re-describes the ink for the canvas layer, from the sheet the last frame drew.
    fn describe_ink(&mut self) {
        let sheet = self.sheet;
        let open = self.ink.open().cloned().map(|mut stroke| {
            stroke.close_live(sheet.zoom);
            Arc::new(stroke)
        });

        // The loop a lasso is sweeping, and whether there is anything to show in the selection's colour: the
        // ink in hand, or the loop being drawn around it. Detailed for the sheet it is drawn at, exactly as the
        // stroke under the pen is: the layer draws a stroke's *cached* outline, and a loop nobody has detailed
        // is a ring with nothing in it.
        let lasso = self.ink.lasso().cloned().map(|mut stroke| {
            stroke.close_live(sheet.zoom);
            Arc::new(stroke)
        });
        let shown = lasso.is_some() || self.ink.has_selection();

        self.canvas.ink = Ink {
            origin: sheet.origin,
            zoom: sheet.zoom,
            strokes: Arc::clone(self.ink.finished()),
            open,
            open_id: self.ink.open_id(),
            revision: self.ink_revision,
            visible: sheet.visible(),
            // Which way up the page is, and the paper's own shape: the layer turns the ink with the
            // page, because the ink is written *on* the page (see [`crate::ink::drawn_of_paper`]).
            rotation: sheet.rotation,
            paper: sheet.ink_paper(),
            // The lasso's own: which strokes are in hand, where a drag has taken them, and the loop being
            // drawn. The colour is `None` when there is nothing to show, which is what leaves a page nobody
            // has lassoed drawing exactly as it did before there was a lasso.
            selected: Arc::clone(self.ink.selected()),
            offset: self.ink.drag_offset(),
            selection: shown.then_some(self.selection_colour),
            lasso,
        };
    }

    /// Describes the sheet — desk, paper, ruling, page and ink — for the canvas layer, and asks for the frame
    /// that draws it.
    ///
    /// This is the whole of "the canvas now shows where the sheet is", and it is written from two places for
    /// the same reason [`Self::describe_ink`] is: the frame, which knows the window and the theme, and the
    /// pump, which draws the canvas on its own wake. A pinch is the case that made it a method — a trackpad
    /// sends pinch events but nothing wakes the pump, so a zoom that only moved `self.sheet` was a zoom whose
    /// canvas was not described until the gesture ended and the settle wrote the note.
    fn describe_sheet(&mut self, sheet: &Sheet, desk: Hsla, page: Option<&RenderedPage>) {
        let page_color: Hsla = rgb(self.settings.page_color).into();
        self.desk = desk;

        describe_canvas(&mut self.canvas, sheet, self.scale, page_color, desk);

        // The ruling is printed on the paper and grows with it, so it is handed the sheet's own rectangle. A
        // PDF page is its own paper, and a blank sheet's ruling has nothing to sit on, which is why the page
        // arriving is the one reason not to build it.
        let (ink_width, ink_height) = sheet.ink_paper();
        let paper_bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(ink_width * sheet.zoom), px(ink_height * sheet.zoom)),
        };

        if page.is_none() {
            let started = Instant::now();
            let before = self.ruling.rebuilds();
            let rules = self.ruling.rules(
                RulingSheet {
                    paper: paper_bounds,
                    turns: sheet.rotation,
                },
                self.settings.canvas_style,
                self.settings.page_color,
                sheet.zoom,
            );

            // A cache hit is not a build, and recording one as a build would hide the cost of the builds
            // that do happen behind the many frames that do not.
            if self.ruling.rebuilds() != before {
                self.timings.ruling.record(started.elapsed());
                self.timings.count_ruling();
            }

            for rule in Ruling::rules_at(&rules, sheet.origin) {
                self.canvas.rule(rule);
            }
        }

        let (page_width, page_height) = sheet.drawn();
        self.canvas.page = page.map(|page| Page {
            image: Arc::clone(&page.image),
            rect: Rect {
                x: sheet.origin.0,
                y: sheet.origin.1,
                width: page_width,
                height: page_height,
            },
        });

        // Described after the desk and the page, because it is the ink's placement that depends on the sheet
        // this call was given.
        self.describe_ink();

        // What the canvas was asked to draw, counted for the status line: the layer draws it and this does
        // not touch a polygon, but the numbers a person reads are the numbers they read before — strokes on
        // the sheet, the outline points they cost, and how many were off it.
        let visible = sheet.visible();
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

        // The same call the pen's pump makes: a frame is what changed the *sheet* — the zoom, the pan, the
        // page, the paper — and the ink is redrawn with it rather than waiting for the next reading.
        self.draw_canvas();
    }

    /// Describes the ink for the canvas, then hands the canvas to its layer.
    fn draw_canvas(&mut self) {
        // Nothing has described a canvas yet: this is a reading that arrived before the first frame,
        // and a layer handed an empty description would paint an empty desk over the window.
        if self.canvas.sheet.is_none() {
            self.canvas_dirty = false;
            return;
        }

        // The cap the reader chose, against the time since the last frame was handed over. A frame that
        // arrives early is *dropped*, not deferred: the newest description is what the next one draws
        // anyway, so waiting would only add latency to a drawing that is already on its way. The canvas is
        // marked dirty so the pump tries again, which is where the next frame comes from when nothing else
        // is moving — the pen's own wake is faster than any cap and is not relied on here.
        if held_by_cap(
            self.settings.frame_cap,
            self.last_present.map(|last| last.elapsed()),
        ) {
            self.canvas_dirty = true;
            return;
        }

        self.describe_ink();
        self.present_canvas();
    }

    /// Hands the canvas to its layer, and reports a failure without losing the layer.
    ///
    /// A layer that has no drawable ready is not a failure: the canvas stays dirty and the next
    /// wake tries again. That is what keeps the pen's pump from waiting for the compositor — the
    /// one wait a reading cannot afford (see [`crate::platform::InkSurface`]).
    fn present_canvas(&mut self) {
        if self.canvas.sheet.is_none() {
            self.canvas_dirty = false;
            return;
        }

        // Timed by hand rather than with a guard, which would hold a borrow of the counters across the
        // layer's own borrow of the app. This is what the status line's `canvas` clause counts.
        let started = Instant::now();
        let drawn = match self.ink_layer.as_mut() {
            Some(ink) => ink.draw(&self.canvas),
            None => Ok(true),
        };

        match drawn {
            Ok(true) => {
                self.timings.canvas.record(started.elapsed());
                // Handed over: the compositor shows the newest canvas it is given, so the gap between two
                // of these is the rate the ink reaches the screen at, whichever wake drew it.
                if let Some(previous) = self.last_present.replace(started) {
                    self.timings.present_gap.record(started - previous);
                }
                self.canvas_dirty = false;
            }
            Ok(false) => {
                // No drawable was ready. Nothing was presented, so nothing is recorded as one; the
                // display pump will try again, and the last reading is not left unpresented.
                self.canvas_dirty = true;
            }
            Err(error) => {
                self.timings.canvas.record(started.elapsed());
                // Kept rather than dropped: no frame paints the canvas, so a dropped layer is a canvas
                // that is gone, while a kept one holds its last frame and can say what is wrong.
                self.canvas_dirty = false;
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

                // The detail a zoom asked for, built between frames rather than in one. A rung is 23.7 ms
                // on a page of 8,182 strokes — three and a half frames at 144 Hz (see
                // `measures_the_rung_costs`) — so the frame that crossed it records the request and this
                // pays for it. Waited for until the gesture has stopped talking, because a pinch still in
                // flight would ask for the next rung before this one was finished with; the page draws at
                // the rung it already has until then, which differs by under a quarter of a pixel.
                if app.detail_owed
                    && app.zoom_settles_at.is_none_or(|last| {
                        now.saturating_duration_since(last) >= DETAIL_SETTLE_INTERVAL
                    })
                {
                    app.detail_owed = !app.ink.build_detail();
                    if !app.detail_owed {
                        app.ink_revision += 1;
                        app.canvas_dirty = true;
                        cx.notify();
                    }
                }

                // A zoom gesture that has stopped talking is a zoom that has finished: commit it to the note
                // and ask for the frame that shows the new number. This is the write a pinch must not do per
                // event (see [`Self::zoom_settles_at`]), and the gap is the only end a pinch has — the
                // platform reports where a gesture is, not when it stopped.
                if app.zoom_settles_at.is_some_and(|last| {
                    now.saturating_duration_since(last) >= GESTURE_SETTLE_INTERVAL
                }) {
                    app.zoom_settles_at = None;
                    app.finish_zoom(cx);
                }

                // An edge swipe that has stopped talking is over, whether or not it turned a page: its state is
                // only meaningful within one gesture, and a gesture nobody is still making must not be resumed by
                // the next scroll — a swipe that turned a page and a scroll a minute later are not one motion.
                if app.edge_swipe.is_some()
                    && app.scrolled_at.is_none_or(|last| {
                        now.saturating_duration_since(last) >= GESTURE_SETTLE_INTERVAL
                    })
                {
                    app.end_edge_swipe();
                }

                // The counters are the one part of the line that moves on its own. Rebuild it
                // once a second while a note is in front, so a person watching a stroke sees the
                // numbers move; the rest of the line still waits for a change the reader made.
                if !app.home_is_open()
                    && now.saturating_duration_since(app.status_refreshed_at)
                        >= STATUS_REFRESH_INTERVAL
                {
                    app.status_refreshed_at = now;
                    // The machine's own counters, sampled here and nowhere else: this is the one wake
                    // that is already about numbers moving on their own (see [`Self::sample_vitals`]).
                    app.sample_vitals();
                    let before = app.status.clone();
                    app.touch_status();
                    app.status_log.write(&app.status);
                    if app.status != before {
                        cx.notify();
                    }
                }

                // The page the view is waiting for, paid for here rather than inside a frame: it is
                // skipped while the pen is laying ink, so the ink pump keeps taking readings.
                let page_rendered = app.serve_pdf(now);

                // A canvas that could not get a drawable — or that the reader's frame cap held back — is
                // still the newest canvas: try again on this wake rather than leaving the last reading
                // unpresented. `nextDrawable` does not block (see `metal.rs`), so a retry is a cheap check.
                //
                // Through `draw_canvas` and not `present_canvas`, so that the cap is checked here too: the
                // pump is the *only* thing drawing when the page is still, and a retry that skipped the cap
                // would be a cap that does not hold at rest.
                if app.canvas_dirty {
                    app.draw_canvas();
                }

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
    ///
    /// This is the one line the pen is judged by: ink refuses a `Down` above it, the ghost cursor is not
    /// drawn above it, and the system pointer is the cursor there (see
    /// [`Self::pen_has_its_own_cursor`] and [`Self::publish_screen`]). They have to be one line — a pen
    /// that can write where its own cursor has given way to the arrow is a pen writing through the
    /// interface (see [`bar_line`]).
    fn bar_edge(&self) -> f32 {
        bar_line(self.settings.show_toolbar, self.bar_bottom.get())
    }

    /// Reads a batch of readings into the ink model, timed — or spends them as a gesture when a key is held.
    fn consume_ink(&mut self, samples: &[Reading], cx: &mut Context<Self>) -> bool {
        // The pen is not a pen while something is in front of the sheet: a name being typed, or a screen
        // drawn over a sheet nobody can see. This is why the lists are screens rather than panels — the pen
        // obeys an app state, not a rectangle (see [`crate::bookmarks`]).
        let sheet_is_front = self.naming.is_none()
            && !self.home_is_open()
            && !self.asking()
            && !self.marks.is_open()
            && !self.outline.is_open()
            && !self.palette_is_open();

        if !sheet_is_front {
            self.pen_mode = None;
            return false;
        }

        // Space and option turn the pen into something else for as long as they are held: space moves the
        // sheet, option zooms it (see [`crate::pen_gesture`]). Asked of the OS rather than of the window's
        // keyboard, because the key is held *while the nib is on the paper*, which is not the same as this
        // window having focus.
        let held = self
            .held_keys
            .as_ref()
            .map(crate::pen_gesture::Held::now)
            .unwrap_or_default();

        if self.consume_pen_gesture(samples, held, cx) {
            return false;
        }

        let transform = self.sheet.transform(self.scale, self.bar_edge());
        let _timed = measure(&self.timings.ink);

        self.ink.consume(samples, &transform, &self.settings)
    }

    /// Lets the pen gesture go, committing what it did if it was a zoom.
    ///
    /// A pan is already written down: it *is* the view's own pan, which the note's settings do not hold. A
    /// zoom is a setting, and the gesture has been *showing* it without writing it (see [`Self::step_zoom`]) —
    /// so this is where the note learns it, once, however many readings it took to get there.
    ///
    /// A pan gesture also settles its stored pan back inside the layout's bounds here, once, at the end. During
    /// the gesture the pan is kept exactly where the hand put it, because that is what lets the sheet come back
    /// the moment the hand does (see [`Self::pan_by_pen`]); but a pan left far outside its bounds would mean a
    /// later scroll — or the next gesture's own start — unwinding travel nobody can see. Settling at the end
    /// keeps both: the sheet follows the hand while the hand is moving, and the end of a sheet is an end once
    /// it stops.
    fn end_pen_gesture(&mut self, cx: &mut Context<Self>) {
        let mode = self.pen_mode;
        let zoomed = mode.is_some_and(|mode| mode.mode == crate::pen_gesture::Gesture::Zoom);

        self.pen_mode = None;

        if zoomed {
            self.finish_zoom(cx);
        }

        if mode.is_some_and(|mode| mode.mode == crate::pen_gesture::Gesture::Pan) {
            self.settle_pan();
        }
    }

    /// Puts the stored pan back inside the bounds the layout draws it in, once, and shows the result.
    ///
    /// The end of a pan gesture, and the only place a pan is held now that [`Self::pan_by_pen`] keeps the hand's
    /// own number. Nothing to do on a sheet that fits — there is no pan on that axis — and `hold_pan` already
    /// answers `false` when nothing moved.
    fn settle_pan(&mut self) {
        let sheet = self.sheet;
        if sheet.window.0 <= 0.0 || sheet.window.1 <= 0.0 {
            return;
        }

        if self.view.hold_pan(sheet.paper, sheet.window, PAGE_MARGIN) {
            self.settle_view();
        }
    }

    /// Spends a batch as a pen gesture — a pan or a zoom — when a key is asking for one.
    ///
    /// Returns whether the readings were spent, which is what tells [`Self::consume_ink`] that they are not
    /// ink. Three things decide it, in this order:
    ///
    /// 1. **Which gesture the keys ask for** ([`crate::pen_gesture::Gesture::of`]). Space wins over option,
    ///    because a hand that is panning may rest on option without meaning to zoom.
    /// 2. **Whether the gesture has begun.** A gesture starts on the nib's `Down` and ends on its `Up`,
    ///    `Cancel`, or on a key being let go — a reader who lets go mid-stroke means to start writing, and
    ///    one who presses the key mid-stroke means to start moving. Neither has to lift the pen.
    /// 3. **Where the nib is now against where it began.** Every reading is measured against the gesture's
    ///    origin rather than the one before it, so the gesture is absolute: a hand that goes up and comes
    ///    back returns the sheet to where it started.
    ///
    /// Ink already on the paper is *not* disturbed: a gesture never opens a stroke, so nothing is written,
    /// and nothing that was written is taken back.
    fn consume_pen_gesture(
        &mut self,
        samples: &[Reading],
        held: crate::pen_gesture::Held,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::input::Phase;
        use crate::pen_gesture::Gesture;

        let Some(last) = samples.last() else {
            return false;
        };

        let gesture = Gesture::of(held, true);

        if gesture == Gesture::Ink {
            // The keys are up. A gesture that was in play has ended, and the pen goes back to writing from
            // this reading — the nib is already where the reader wants the next stroke to start.
            self.end_pen_gesture(cx);
            return false;
        }

        let at = (last.pixel.x / self.scale, last.pixel.y / self.scale);
        let ended = matches!(last.phase, Phase::Up | Phase::Cancel);
        // The nib is on the paper. A pen that is merely *above* the display is not gesturing: a hand resting
        // over the screen with a key held would otherwise drag the sheet around while the nib touched nothing,
        // which is the one thing a pen that hovers cannot do. See [`touches_paper`].
        let touching = touches_paper(last.phase);

        if self.pen_mode.is_none() {
            // A gesture starts on a nib that is touching, below the bar: a key held while the pen hovers is not
            // a gesture at all, and one held over the toolbar is a press on a control.
            if !touching || at.1 < self.bar_edge() {
                return false;
            }
        } else if self.pen_mode.is_some_and(|mode| mode.mode != gesture) {
            // The key changed mid-stroke: let the old gesture go and begin the new one at the nib's place
            // now, rather than at a place from the gesture before.
            self.end_pen_gesture(cx);
            return true;
        } else if !touching && !ended {
            // A gesture already in play, and the nib has come off the paper without a `Up` — a hover that
            // followed a touch. The gesture is over; the ink is not, because none was laid.
            self.end_pen_gesture(cx);
            return false;
        }

        let smoothed = self.settings.pen_jitter;
        let mode = *self.pen_mode.get_or_insert(PenGesture {
            mode: gesture,
            origin: at,
            zoom: self.view.zoom(),
            pan: self.view.pan(),
            // Seeded with where the nib went down, so the first reading after it is weighed against the
            // place the gesture started rather than against nothing.
            jitter: {
                let mut jitter = crate::pen_gesture::PenJitter::new();
                if smoothed {
                    jitter.at(at);
                }
                jitter
            },
            smoothed,
        });

        if ended {
            self.end_pen_gesture(cx);
            return true;
        }

        match gesture {
            Gesture::Pan => {
                self.pan_by_pen(mode, at, cx);
                // The reading moved the filter's settle point, and it has to outlive this reading: the next
                // one is weighed against it, which is what makes the tremor a tremor rather than a drift.
                self.pen_mode = Some(mode);
            }
            Gesture::Zoom => {
                self.zoom_by_pen(mode, at, cx);
                self.pen_mode = Some(mode);
            }
            Gesture::Ink => unreachable!("handled above"),
        }

        true
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

        // The page this ink belongs to, asked of the ink and never of `page_index`: `add_page` and
        // `delete_page` move the page list before the view follows it, and an index read here is
        // how the page being left once wrote its whole ink under the page that took its place. The
        // two numbers agree everywhere else, which is exactly why it went unnoticed.
        let page = page_of_write(&self.ink);
        let count = self.ink.finished().len();
        let sent = self.saved.get(&page).copied().unwrap_or(0);
        let shift = self.ink.shifted();
        let written = self.shifted.get(&page).copied().unwrap_or(0);

        if page_owes_a_rewrite(count, sent, shift, written) {
            // The page is not simply a longer version of what the note holds: something was taken back,
            // removed, or moved.
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

            // What the page's history did, taken here rather than at the top of this function: the update *is* the
            // note being told, so it is asked for at the moment it is handed over.
            let history = self.ink.history_update(count);

            if let Some(writer) = &self.writer {
                writer.rewrite(page, ink, history);
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

            let history = self.ink.history_update(count);

            if let Some(writer) = &self.writer {
                writer.append(page, ink, history);
            }
        }

        self.saved.insert(page, count);
        // The page as it now stands on disk, twice over: how many strokes the note holds, and how many times
        // it had been moved when that was written.
        self.shifted.insert(page, shift);
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

        self.report(String::from(
            "a new note on a blank sheet — Save writes it out as one file",
        ));
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

    /// Deletes the ink a lasso has hold of, and says so.
    ///
    /// One of the two edits a note notices by *counting*: the page it leaves has fewer strokes than the note
    /// holds, which is what makes [`Self::persist`] write the page out again rather than append to it. Not
    /// undoable, for the reason erasing is not (see [`crate::ink`]).
    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let removed = self.ink.delete_selected();
        if removed == 0 {
            return;
        }

        // Written out at once rather than on the batch clock: this is a deliberate command, and leaving it in
        // memory for half a second is how it is lost if the app is closed in that half-second — the same
        // reason an undo is written at once.
        self.persist(Instant::now(), true);

        self.report(format!(
            "deleted {removed} stroke{}",
            if removed == 1 { "" } else { "s" }
        ));
        cx.notify();
    }

    /// Lets the lasso's selection go: what Escape does before it leaves the note.
    ///
    /// Nothing is written: a selection is not ink and the note has never heard of it (see [`crate::ink`]) — it
    /// is the page's own idea of what is in hand, for as long as the note is open.
    fn deselect(&mut self, cx: &mut Context<Self>) {
        if self.ink.deselect() {
            self.touch_status();
            cx.notify();
        }
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

    /// Caps how many frames a second the canvas draws, whatever the display can show.
    ///
    /// Nothing about the display changes: the cap is the app's own ceiling, checked in
    /// [`Self::draw_canvas`] against the time since the last frame was handed over. A higher cap than
    /// the display can show simply never binds, which is why `Unlimited` is safe as the default.
    fn set_frame_cap(&mut self, cap: FrameCap, cx: &mut Context<Self>) {
        self.settings.frame_cap = cap;
        // A frame drawn a moment ago must not hold back the first one after the cap moves *down*, and a
        // cap moved *up* should not have to wait for the old interval to run out: the clock starts again.
        self.last_present = None;
        self.draw_canvas();
        self.finish_setting(cx);
    }

    /// Waits for the display's clock before showing a frame, or stops waiting.
    ///
    /// Handed to the surface rather than kept here: this is a property of the layer that presents, and a
    /// platform whose layer has no such property ignores it (see [`InkSurface::set_vsync`]). Nothing is
    /// redrawn — the next frame that is already coming is the one that waits, or does not.
    fn set_vsync(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.vsync = on;
        self.apply_vsync();
        self.finish_setting(cx);
    }

    /// Tells the canvas whether to wait for the display, if there is a canvas to tell.
    ///
    /// Called at install and whenever the switch moves, so a surface never has to guess: the note's own
    /// answer is the only one it is ever given.
    fn apply_vsync(&mut self) {
        if let Some(ink) = self.ink_layer.as_mut() {
            ink.set_vsync(self.settings.vsync);
        }
    }

    /// Whether the canvas can honour the note's V-Sync setting at all.
    ///
    /// The setting is stored with the note on every platform, because it travels between machines;
    /// the *switch* is only offered where pressing it can have an effect. Windows presents at a sync
    /// interval of zero on purpose (see `apps/windows/platform/ink_layer/device.rs`), so its surface
    /// answers `false` and the bar leaves the control out rather than showing one that does nothing.
    fn honours_vsync(&self) -> bool {
        self.ink_layer
            .as_ref()
            .is_none_or(|ink| ink.honours_vsync())
    }

    /// Hands the note's memory mode to the cache, which is the one part of the app that holds memory in
    /// bulk. See [`MemoryMode`].
    ///
    /// Separate from [`Self::set_memory_mode`] because it runs on the *loading* path, where the choice has
    /// not changed — it has just been read from a note — and where there is no context to notify. The
    /// method that changes it calls this and then does the things a change needs.
    fn apply_memory_mode(&mut self) {
        self.pdf
            .set_budget(self.settings.memory_mode.cache_budget_bytes());
    }

    /// Turns the pen gesture's own tremor correction on or off. See [`Settings::pen_jitter`].
    ///
    /// Nothing is redrawn and no gesture in flight is disturbed: the answer is read when the nib goes down,
    /// so what this changes is the *next* gesture. A settle point that moved halfway through one would jump
    /// the sheet by however far the tremor had been held back.
    fn set_pen_jitter(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.pen_jitter = on;
        self.finish_setting(cx);
    }

    /// Renders PDF pages in grayscale, or back in colour.
    fn set_pdf_grayscale(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.grayscale_pages = on;
        self.finish_setting(cx);
    }

    /// Spends what the sheet is worth, or as little as the app can manage. See [`MemoryMode`].
    ///
    /// Two things happen here beyond storing the choice. The cache's budget moves with the mode, and
    /// whatever the old budget allowed but the new one does not is **dropped now** rather than at the
    /// next page turn — turning the mode on to stop a machine swapping and then keeping every bitmap
    /// until the reader happens to turn a page is not the mode working.
    ///
    /// The bitmaps that survive are dropped too, by the page being invalidated below: they were drawn at
    /// the old mode's scale, and a page rendered at 1.0 shown on a 2.0 screen is visibly soft. Rendering
    /// it again is the price of the change, paid once.
    fn set_memory_mode(&mut self, mode: MemoryMode, cx: &mut Context<Self>) {
        if self.settings.memory_mode == mode {
            return;
        }

        self.settings.memory_mode = mode;
        self.apply_memory_mode();

        // The rung on screen was sized for the other mode. Clearing the plan is what asks for the new
        // one: `serve_pdf` plans whatever the current request is on its next wake, which is the same
        // rule a zoom follows, and until it arrives the page keeps drawing at the detail it has.
        if self.pdf.is_loaded() {
            self.pdf.abandon();
            self.pending_pdf = None;
        }

        self.finish_setting(cx);
    }

    /// Turns the pencil's fade on or off. See [`Settings::pressure_fades`].
    ///
    /// Like every other pen setting, it says what the *next* stroke is laid with and changes nothing already
    /// written: the palette's colours are stamped into the strokes drawn with them, and so is this.
    fn set_pressure_fades(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.pressure_fades = on;
        self.finish_setting(cx);
    }

    /// Locks or unlocks the zoom gestures. See [`Settings::zoom_locked`].
    ///
    /// Locking does not move the sheet and does not change the number: it says what the *next* pinch or fit
    /// does, which is nothing.
    fn set_zoom_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.settings.zoom_locked = locked;
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
        // A fit is a gesture: it *derives* the zoom from the window rather than being told it, which is the thing a
        // locked view is protected from (see [`Settings::zoom_locked`]). Said out loud, because a button that does
        // nothing is worse than one that is disabled — and this one cannot be disabled, since the lock is what
        // disables it.
        if !self.settings.gestures_zoom() {
            self.report(String::from(
                "the zoom is locked: unlock it to fit the sheet",
            ));
            cx.notify();
            return;
        }

        let sheet = self.sheet;

        if self.view.fit(which, sheet.paper, sheet.window, PAGE_MARGIN) {
            self.finish_zoom(cx);
        }
    }

    /// Keeps the settings and the status line in step with a zoom the view already accepted.
    ///
    /// The *committing* half of a zoom. It writes the note's settings and asks for a frame, both of which are
    /// far too expensive to do per reading of a continuous gesture — a pinch sends dozens of events
    /// a second, and each one was doing a four-table SQLite transaction and a whole-window redraw. A gesture in
    /// flight calls [`Self::step_zoom`] instead, and this once when it ends.
    ///
    /// A zoom also moves the pan — that is what [`Viewport::zoom_around`] does, so the point under the fingers
    /// stays under them — and that pan is kept out of range while the gesture is in flight, for the reason
    /// [`Self::pan_by_pen`] gives. The end of the gesture is where it settles inside its bounds, once, so a
    /// page left of its own edge comes back to it rather than waiting for an unrelated change to clamp it.
    fn finish_zoom(&mut self, cx: &mut Context<Self>) {
        self.settings.zoom = self.view.zoom();
        self.settle_pan();
        self.finish_setting(cx);
        // The zoom field *is* the readout of the zoom, so it has to be redrawn after any change to it — by a button,
        // a pinch or a fit ([`Self::sync_choosers`] puts the number in it, this is what asks for the frame).
        cx.notify();
    }

    /// Shows a zoom the view has just taken, without committing it to the note.
    ///
    /// The *gesture* half of a zoom, and the reason it exists: a pinch delivers an event per frame, and a
    /// note's settings are not a thing written per event. The numbers the reader
    /// sees are updated here so the field and the canvas agree while the hand is still moving; the write and the
    /// frame wait for [`Self::finish_zoom`], which the end of the gesture calls once.
    ///
    /// The canvas needs no frame of its own: it is drawn by the pen's wake or by the next frame the gesture
    /// asks for, and its geometry is read from `self.sheet`, which [`Self::settle_view`] moves.
    fn step_zoom(&mut self) {
        self.settings.zoom = self.view.zoom();
        self.settle_view();
    }

    /// Moves `self.sheet` to where the view has just put the sheet, without a frame to lay it out again.
    ///
    /// The sheet is normally rebuilt by the frame, which is the only place the window's size is known. A pan or
    /// a zoom changes nothing about what the sheet *is* — the paper, the rotation, the window it is drawn into
    /// are all unchanged — so the two numbers the view owns can be moved here and the canvas, which draws on
    /// the pen's own wake, gets the right place instead of trailing the hand by one frame.
    ///
    /// The zoom comes with it because the view holds it too: a pinch that set a new zoom and left the sheet's
    /// old one behind would draw the paper at the size it used to be. What is *not* done here is the ink's own
    /// rebuild — the outlines a stroke is made of are detailed for a zoom rung, and doing that per reading
    /// would rebuild every stroke in the note hundreds of times a second. That waits for the frame the settled
    /// gesture asks for (see [`crate::ink_layer`]).
    fn settle_view(&mut self) {
        let before = self.sheet;
        self.settle_sheet();

        if self.sheet.origin != before.origin || self.sheet.zoom != before.zoom {
            self.view_moved = true;
        }
    }

    /// What the zoom field says: Enter takes the number that was typed, and a click away puts the view's own back.
    fn zoom_event(&mut self, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            InputEvent::PressEnter { .. } => {
                let typed = self.zoom_input.read(cx).value().to_string();
                self.set_zoom_from_text(&typed, window, cx);
            }
            // A half-typed number is not a zoom: leaving the field — which is what a click on the sheet means — puts
            // back the number the sheet is actually drawn at.
            InputEvent::Blur => self.show_zoom(window, cx),
            InputEvent::Change | InputEvent::Focus => {}
        }
    }

    /// Applies a typed zoom, and shows what the view made of it.
    ///
    /// Written back in both directions, because the field is the only place the zoom is shown: a number the view
    /// clamped to its range, and a number it refused, both have to leave the field saying what the sheet is drawn at
    /// rather than what was typed (see [`Self::sync_choosers`]).
    fn set_zoom_from_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        match parse_zoom(text) {
            Some(zoom) => {
                if self.view.set_zoom(zoom) {
                    self.finish_zoom(cx);
                }
            }
            None => self.report(format!("'{}' is not a zoom", text.trim())),
        }

        self.show_zoom(window, cx);
    }

    /// Puts the view's own zoom into the field.
    fn show_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let showing = zoom_text(self.view.zoom());

        self.zoom_input.update(cx, |input, cx| {
            if input.value().as_ref() != showing {
                input.set_value(showing.clone(), window, cx);
            }
        });
    }

    /// Pans with a trackpad's two-finger scroll, and turns the page from a swipe at either side.
    ///
    /// The view's rule is that a trackpad reports pixels and a wheel reports lines, and that only
    /// pixels are the distance the sheet already pans in. A platform whose wheel is the ordinary
    /// way to scroll a page (`Platform::wheel_pans`) is the exception: its lines are read as
    /// `WHEEL_PAN_PER_LINE` logical pixels each. GPUI has already turned a notch into the system's
    /// own line count, so this is not a guess at the wheel's setting — only at how tall a line is.
    ///
    /// A wheel is never an edge swipe. It has no gesture to follow and no origin to measure, and a
    /// vertical notch at the window's edge is a scroll rather than a page turn, so line-based
    /// readings are kept out of [`EdgeSwipe`] entirely.
    ///
    /// Zooming is the pinch ([`Self::on_pinch`]) and only the pinch. A two-finger scroll pans, always, and
    /// the modifier keys do not turn it into something else.
    ///
    /// A swipe that *began* at a window's side is the exception, and it is the pane turn: rightwards from the
    /// left edge is the previous page, leftwards from the right edge is the next one. The gesture is followed
    /// across readings by [`EdgeSwipe`] rather than decided on one, and while it is in flight it does not pan —
    /// a page turn that dragged the sheet sideways first would be two motions for one intention.
    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(scroll) = scroll_of(&event.delta, self.wheel_pans) else {
            return;
        };

        let distance = scroll.distance;

        if !distance.0.is_finite() || !distance.1.is_finite() {
            return;
        }

        // Only a pixel reading is a gesture that can turn a page. A wheel's lines are a sequence of
        // notches with no origin and no hand to follow, and a vertical notch at the window's edge is
        // a scroll — so a wheel is kept out of the edge machinery entirely.
        if scroll.gesture {
            // An edge swipe is begun from where the gesture *started*. macOS reports that position
            // with every reading; Windows reports its system cursor, which is not necessarily the
            // gesture's start (see `doc/WINDOWS.md` §3.1), so the band is only as reliable as that
            // platform's answer.
            if self.edge_swipe.is_none() {
                self.scrolled_at = Some(Instant::now());
                if let Some(edge) = Edge::at(f32::from(event.position.x), self.sheet.window.0) {
                    self.edge_swipe = Some(EdgeSwipe::new(edge));
                }
            }

            if let Some(swipe) = self.edge_swipe.as_mut() {
                swipe.moved(distance);
                self.scrolled_at = Some(Instant::now());

                if let Swipe::Turn(turn) = swipe.decided() {
                    swipe.turned();
                    match turn {
                        Turn::Previous => self.previous_page(cx),
                        Turn::Next => self.next_page(cx),
                    }
                    // The pan the swipe had accumulated is dropped rather than spent: the gesture was a page turn,
                    // and moving the sheet as well would leave the new page scrolled by a swipe that never meant to
                    // scroll.
                    self.scroll_waiting = (0.0, 0.0);
                    return;
                }

                // A gesture that has not decided neither pans nor ends here: it is followed until a reading says it
                // has stopped (see [`Self::end_edge_swipe`], which the pump calls on the same quiet gap a zoom uses).
                return;
            }
        }

        // Left for the frame that shows it, not spent here: the readings arrive faster than frames do, and a
        // pan the compositor never draws is a canvas asked to move somewhere the hand has already left (see
        // [`Self::scroll_waiting`]).
        self.scroll_waiting = (
            self.scroll_waiting.0 + distance.0,
            self.scroll_waiting.1 + distance.1,
        );

        if !self.scroll_asked {
            self.scroll_asked = true;
            cx.on_next_frame(window, |app, window, cx| app.spend_scroll(window, cx));
        }
    }

    /// Ends an edge swipe the readings have stopped arriving for.
    ///
    /// The platform reports where a gesture is, not when it stopped, so the end is a gap — the same gap
    /// [`GESTURE_SETTLE_INTERVAL`] measures for a pinch, read by the same pump. A gesture that has ended without
    /// turning a page is simply forgotten: it was a pan near the edge, and it panned nothing because a swipe in
    /// flight is not spent as one.
    fn end_edge_swipe(&mut self) {
        self.edge_swipe = None;
        self.scrolled_at = None;
    }

    /// Pans the sheet by everything the trackpad has left, once, in the frame that shows it.
    fn spend_scroll(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_asked = false;

        let distance = std::mem::take(&mut self.scroll_waiting);
        if distance == (0.0, 0.0) {
            return;
        }

        // The trace is how a scroll can be watched from outside: one line per *frame*, not per reading,
        // which is what the coalescing above is for — and the canvas's own line beside it says how many
        // times the ink was rebuilt, which while scrolling should be none.
        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            eprintln!("pan: ({:.1}, {:.1})", distance.0, distance.1);
        }

        // Held where the layout can show it, so that a scroll taken past the end of the sheet leaves
        // no debt for the next one to pay off (see [`Viewport::hold_pan`]).
        self.pan_sheet(distance, cx);
    }

    /// Pans the sheet by a distance in logical pixels, kept where the layout can show it, and asks for the
    /// frame that shows it when the pan actually changed — a stick held against the end of the sheet is a
    /// position that has nowhere to go, not a reason to redraw.
    fn pan_sheet(&mut self, distance: (f32, f32), cx: &mut Context<Self>) {
        if distance == (0.0, 0.0) {
            return;
        }

        let sheet = self.sheet;
        let was = self.view.pan();
        self.view.pan_by(distance);

        // A sheet no frame has drawn yet has no bounds to be held inside — the pan is left alone
        // rather than clamped into a window of nothing (see [`Viewport::hold_pan`]).
        if sheet.window.0 > 0.0 && sheet.window.1 > 0.0 {
            self.view.hold_pan(sheet.paper, sheet.window, PAGE_MARGIN);
        }

        if self.view.pan() != was {
            // Already once per frame — the trackpad's readings are accumulated and spent together (see
            // [`Self::scroll_waiting`]) — so this is not the per-reading notify a pan cannot afford. The
            // sheet is settled as well, because the pen's wake may draw the canvas before the frame lands.
            self.settle_sheet();
            cx.notify();
        }
    }

    /// Moves `self.sheet` to match the view, without asking for a frame.
    ///
    /// [`Self::settle_view`] does the same and *sets* the flag that asks for one; this is for callers that are
    /// about to ask anyway.
    fn settle_sheet(&mut self) {
        let sheet = self.sheet;
        if sheet.window.0 <= 0.0 || sheet.window.1 <= 0.0 {
            return;
        }

        let zoom = self.view.zoom();
        let origin = self.view.origin(sheet.paper, sheet.window, PAGE_MARGIN);

        self.sheet = Sheet {
            zoom,
            origin,
            ..sheet
        };
    }

    /// Pans the sheet so the point the nib touched stays under it.
    ///
    /// `mode` is the gesture as it began and `at` is where the nib is now, both in window logical pixels. The
    /// sheet is *placed* at the pan the gesture started from plus the distance travelled: set, not added.
    ///
    /// That distinction is the gesture. [`Viewport::pan_by`] accumulates, so calling it once per reading with
    /// the whole distance from the origin would add the same travel again and again — a hand moving at a steady
    /// pace would drag the page faster and faster, and a nib held still would keep going. Measured from the
    /// origin and *set*, the paper follows the hand exactly, and a nib that comes back leaves the sheet there.
    ///
    /// **The pan is stored as the hand asked it, out of range included**, and clamped only where it is drawn
    /// ([`Viewport::origin`]). Holding it inside its bounds here as well looks harmless and is not: a clamp
    /// throws away where the hand actually is, and no later reading can bring it back, because each one is
    /// measured from the gesture's origin rather than from the stored value. The sheet then sits still while
    /// the hand travels back through the distance it was clamped by — measured at 500 px of nothing moving, on
    /// a sheet drawn twice the window's height — and then jumps past it. Keeping the number whole is what makes
    /// the paper come back with the hand; the end of the gesture settles it once ([`Self::settle_pan`]).
    fn pan_by_pen(&mut self, mut mode: PenGesture, at: (f32, f32), _cx: &mut Context<Self>) {
        // The hand's position with the digitizer's own trembling set aside (see [`PenGesture::settled_at`]).
        // Measured here rather than on the raw reading, because a pan multiplies whatever it is given by two
        // and a fan shaking the tablet would otherwise be two pixels of shiver on the sheet.
        let at = mode.settled_at(at);

        // Where the sheet belongs: the pan the gesture began at, moved by the whole distance so far. Out of
        // range included — see [`PenGesture::panned_to`] for why that is not an oversight, and what a clamp
        // here cost (500 px of hand travel with the paper sitting still, then a jump).
        let wanted = mode.panned_to(at);

        // Stored as it is, out of range included: the clamp belongs to [`Viewport::origin`], which applies it
        // when it draws. See [`PenGesture::panned_to`].
        if !self.view.set_pan(wanted) {
            return;
        }

        // The canvas draws itself on this wake, from `self.sheet`, which is only rebuilt by a frame — so the
        // place is moved here as well. See [`Self::settle_view`].
        self.settle_view();
    }

    /// Zooms the sheet from the gesture's own starting zoom, by how far up the nib has come.
    ///
    /// Set rather than multiplied: the factor is worked out from the whole distance travelled since the nib
    /// went down, and applied to the zoom the gesture started from. Multiplying a factor per reading
    /// — which a pinch does, correctly, because a pinch reports the change it made — would compound a hand's
    /// slow drag into a runaway zoom and leave nothing to come back to.
    fn zoom_by_pen(&mut self, mut mode: PenGesture, at: (f32, f32), _cx: &mut Context<Self>) {
        // The rise is measured to the hand's settled position, for the same reason a pan is: the zoom's factor
        // is an exponent of the travel, so a reading's worth of tremor would be a visible change of size.
        let at = mode.settled_at(at);
        let rise = mode.origin.1 - at.1;
        let wanted = mode.zoom * crate::pen_gesture::pen_zoom_factor(rise);

        if self.view.set_zoom(wanted) {
            // A pen sends readings far faster than a screen takes frames, and the note is not written per
            // reading: this shows the zoom and leaves the write to the end of the gesture, when the nib lifts
            // or the key is let go (see [`Self::consume_pen_gesture`]).
            self.step_zoom();

            // The pump wakes on the reading and draws the canvas, but it draws the description it is given:
            // without this the sheet has a new zoom and the canvas still shows the old one.
            let sheet = self.sheet;
            let page = self.current_page();
            self.describe_sheet(&sheet, self.desk, page.as_ref());
        }
    }

    /// Zooms with a trackpad's pinch, about the point between the fingers.
    fn on_pinch(&mut self, event: &PinchEvent, _cx: &mut Context<Self>) {
        // A pinch is the likeliest of the accidents a lock is for: a palm settling onto a trackpad is a pinch as far as
        // the platform is concerned, and it happens while a hand is writing (see [`Settings::zoom_locked`]).
        if !self.settings.gestures_zoom() {
            return;
        }

        let pointer = (event.position.x.into(), event.position.y.into());
        let factor = pinch_zoom_factor(event.delta);
        let sheet = self.sheet;

        if self
            .view
            .zoom_around(factor, pointer, sheet.paper, sheet.window, PAGE_MARGIN)
        {
            // The note is written once the pinch has gone quiet (`zoom_settles_at`, below). The *drawing* is
            // not deferred: a trackpad wakes no pump the way a pen does, so a zoom that only moved the sheet
            // was a zoom nobody saw until the settle asked for a frame — the paper jumped to its final size
            // with the whole gesture drawn nowhere. Describing it here is what shows the frames in between.
            self.step_zoom();
            self.zoom_settles_at = Some(Instant::now());

            let sheet = self.sheet;
            let page = self.current_page();
            self.describe_sheet(&sheet, self.desk, page.as_ref());
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

    /// Chooses the colour the highlighter lays down.
    fn set_highlighter_color(&mut self, color: u32, cx: &mut Context<Self>) {
        self.settings.highlighter_color = color;
        self.finish_setting(cx);
    }

    /// Changes how heavy the pen is.
    fn set_pen_weight(&mut self, weight: PenWeight, cx: &mut Context<Self>) {
        self.settings.pen_weight = weight;
        self.finish_setting(cx);
    }

    /// Shows or hides the whole top bar.
    ///
    /// The line the ghost is drawn below and the pointer gives way above is the bar's, so the two are
    /// told *now* rather than at the frame that redraws the bar: the switch that hides the bar is inside
    /// the strip it is about to give back to the page, and a frame's delay there is the system arrow
    /// appearing under the hand that hid it (see [`Self::bar_edge`]).
    fn set_toolbar_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.settings.show_toolbar = shown;
        self.publish_screen();
        self.follow_pen_with_pointer();
        self.finish_setting(cx);
    }

    /// Shows or hides the live status line inside the bar.
    fn set_status_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.settings.show_status = shown;
        // Sampled here as well as on the clock, so switching the line on fills its hardware clause at
        // once: `finish_setting` rebuilds the line below, and a clause sampled a second later would leave
        // the reader looking at a line that was missing the very thing they just asked for.
        self.sample_vitals();
        self.finish_setting(cx);
    }

    /// The cursor the pen's most recent reading describes.
    ///
    /// The reading is the app's own ([`Self::last_pen`]) rather than the ink's: the ghost is fed every
    /// batch on the pen thread, so a batch that became a palette choice rather than a stroke still moved
    /// the pen — and the cursor rules have to see the same place the ghost does.
    fn last_cursor(&self) -> Option<PenCursor> {
        self.last_pen
            .and_then(|sample| PenCursor::from_sample(sample, self.scale))
    }

    /// The cursor to draw.
    ///
    /// There is no switch: the ghost *is* the cursor. A Windows pointer is a fixed bitmap, so the system
    /// can say which cursor to show and never which angle — and a pen held at forty degrees has no arrow
    /// that means it. Turning the ghost off would leave a pen with no cursor of its own at all, which is
    /// a state nobody asked for. The two switches that remain are about the bar, and are the bar's own
    /// business (see [`Self::command_row`]).
    fn pen_cursor(&self) -> Option<PenCursor> {
        self.last_cursor()
    }

    /// Whether something on screen means the ghost is not drawn: the home list, a question waiting for
    /// an answer, the palette at the pen, or a chooser's list hanging over the page.
    ///
    /// One answer for the ghost *and* the pointer ([`Self::pen_has_its_own_cursor`]): with the ghost
    /// suppressed the pointer has to be the system's, or the app would be showing no cursor at all on a
    /// screen that has buttons to press. The palette is one of those screens: its controls are aimed at
    /// with the system pointer, so the ghost goes while it is up and the system cursor is what the hand
    /// sees over the ring. A chooser's list is the same case with a different shape — the popup is drawn
    /// *below* its box, over the page, so the one place the reader has to aim at is a place the bar's own
    /// line calls page (see [`Self::chooser_open`]).
    fn ghost_suppressed(&self) -> bool {
        self.home_is_open() || self.asking() || self.palette_is_open() || self.chooser_open
    }

    /// Whether the pen has a cursor of its own where it is, so the system pointer should be out of the way.
    ///
    /// This is the platform-agnostic half of "is the ghost on screen": the ghost is handed the same
    /// answer (see [`Self::publish_screen`]), because a pointer taken away with nothing drawn in its
    /// place is a window with no cursor at all. There is no ghost without a platform to draw one, with
    /// something over the sheet, or above the bar's own edge — so the pointer is the system's in exactly
    /// those places.
    fn pen_has_its_own_cursor(&self) -> bool {
        if !self.cursor.as_ref().is_some_and(|feed| feed.is_alive()) {
            return false;
        }

        if self.ghost_suppressed() {
            return false;
        }

        // Below the bar's own edge — the measured one, and a bar that is hidden has no edge at all, so
        // the strip its switch sat in is page again (see [`Self::bar_edge`]). The ghost's line is the
        // same one; a pointer that followed a different line would leave the arrow under a ghost, or a
        // ghost under the arrow.
        self.pen_cursor()
            .is_some_and(|cursor| cursor.position()[1] > self.bar_edge())
    }

    /// Tells the ghost cursor's window what this frame knows about the screen it is drawn on.
    fn publish_screen(&self) {
        let Some(cursor) = &self.cursor else {
            return;
        };

        cursor.set_screen(Screen {
            scale: self.scale,
            // The ghost's mark is the colour in hand: the dot says what the next stroke will be
            // written in, which is what a reader looks at it for. The *halo* is the app's answer for
            // the paper, so a mark that would vanish on it — white ink, a highlighter's pale yellow —
            // still has an edge to be found by (see [`Screen::halo`]).
            colour: self.ink_colour_in_hand(),
            halo: contrast_color(self.settings.page_color) & 0x00FF_FFFF,
            // And how thick that stroke will be, for the same reason: the mark is a preview of the
            // ink, so a heavy pen's dot is heavy (see [`Self::nib_width`]).
            nib_radius: nib_radius_for_width(self.nib_width()),
            // The line above which a reading belongs to a control rather than to the page: the bar's
            // own edge, as the ink and the pointer rule ask for it ([`Self::bar_edge`]), in physical
            // pixels. One line, three consumers — the ghost that is drawn below it, the ink that is
            // refused above it, and the system pointer that is the cursor there.
            sheet_top: self.bar_edge() * self.scale,
            suppressed: self.ghost_suppressed(),
        });
    }

    /// How thick the line in hand is, in logical pixels: what the ghost cursor's nib mark is sized by.
    ///
    /// The pen's *widest* line rather than the pressure of the moment — the weight is a setting and a pressure is a
    /// measurement, and a mark that grew and shrank as the hand pressed and eased would be a mark that never meant the
    /// same thing twice. It is the weight that makes it interesting: switching from `Fine` to `Heavy` grows the dot,
    /// which is the one way the pen's thickness can be seen without putting ink on the page.
    ///
    /// A marker lays one width and ignores the pen's, so it is asked for its own band; the eraser and the lasso lay
    /// nothing, and are shown the pen's mark, because it is the pen whose weight the reader set — a dot the size of
    /// what a lasso encloses would be a smear rather than a cursor.
    fn nib_width(&self) -> f32 {
        match self.ink.mode() {
            Tool::Highlighter => HIGHLIGHTER_WIDTH,
            Tool::Pen | Tool::Eraser | Tool::Lasso => self.settings.width_for_pressure(Some(1.0)),
        }
    }

    /// The colour the next stroke will be written in: what the ghost cursor's mark is drawn in.
    ///
    /// The pen's ink or the marker's, whichever is in hand — the ghost is a ghost of the *next* stroke, and that is
    /// what a reader looks at it to know. The eraser and the lasso write nothing, so they keep the paper's own
    /// contrast colour, which is what a mark on that paper can be seen in at all.
    fn ink_colour_in_hand(&self) -> u32 {
        let colour = match self.ink.mode() {
            Tool::Pen => self.settings.ink_color,
            Tool::Highlighter => self.settings.highlighter_color,
            Tool::Eraser | Tool::Lasso => contrast_color(self.settings.page_color),
        };

        colour & 0x00FF_FFFF
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
        cx.notify();
    }

    /// Removes the most recent edit, whatever kind of edit it was, and says so.
    ///
    /// A page taken back is not a *shorter* page, so an undo is always a rewrite: the strokes that survive are in a
    /// different order than the ones on disk — a stroke put back in the middle takes its place in the middle — and
    /// an append can only add to the end (see [STORE.md](../../doc/STORE.md), §6).
    fn undo(&mut self, cx: &mut Context<Self>) {
        if !self.ink.can_undo() {
            // The session's own history has run out. The *note* keeps a deeper one, and an undo the reader asks for
            // is not the moment to explain the difference between the two: one more edit is read in and taken back
            // (see [`Self::deepen`]).
            self.deepen();
        }

        if self.ink.undo() {
            self.rewritten.insert(page_of_write(&self.ink));
            self.persist(Instant::now(), true);
            self.touch_status();
            cx.notify();
        }
    }

    /// Puts back the edit the last undo took away.
    fn redo(&mut self, cx: &mut Context<Self>) {
        if self.ink.redo() {
            // A redo is the mirror of an undo, for the same reason: it changes the *order* of the page as much as
            // its length.
            self.rewritten.insert(page_of_write(&self.ink));
            self.persist(Instant::now(), true);
            self.touch_status();
            cx.notify();
        }
    }

    /// Reads one more edit of the page's history out of the note, so that an undo can go deeper than the session's
    /// memory does.
    ///
    /// The edits below a page's cursor are all *applied* — the note's log is the history of the page as it stands —
    /// so nothing here changes the page: it puts back an undo that memory had let go of, which is what makes a note
    /// opened on Friday able to take back what was written on Monday.
    ///
    /// Answers whether the note had one. What it does *not* do is say so: the bar's buttons are offered by
    /// [`InkDocument::can_undo`], which asks the session's history — and a reader who asks for an undo that neither
    /// memory nor the note can give is asking about a page that has nothing to take back.
    fn deepen(&mut self) -> bool {
        let page = page_of_write(&self.ink);
        let skip = self.ink.history_in_note();

        let older = match &self.note {
            Some(note) => note.store().older_history(page, skip),
            // Nothing is open, so there is no deeper history to read: the ink on screen belongs to a note that does
            // not exist yet.
            None => Ok(None),
        };

        match older {
            Ok(Some(edit)) => self.ink.history_mut().deepened(edit),
            Ok(None) => false,
            // A log that cannot be read is a history that reaches only as far as memory does, and saying so is the
            // whole of what can be done about it.
            Err(error) => {
                self.report(error.to_string());
                false
            }
        }
    }

    /// Removes every stroke.
    /// Asks before doing something the note cannot take back.
    ///
    /// Nothing happens until [`Self::answer`] is called: asking is what makes a destructive command reachable only
    /// through an answer (see [`confirmed`]). The pen is told as well — with the box up it draws no ghost and lays no
    /// ink — because a stroke arriving under a question would be ink nobody asked for.
    fn ask(&mut self, what: Confirmation, cx: &mut Context<Self>) {
        self.confirm = Some(what);
        self.follow_pen_with_pointer();
        self.publish_screen();
        cx.notify();
    }

    /// Answers the question on screen: `yes` runs what it asked about, and any other answer drops it.
    fn answer(&mut self, yes: bool, cx: &mut Context<Self>) {
        let Some(what) = self.confirm.take() else {
            return;
        };

        // The one place an answer becomes an action. Every arm is spelled out rather than wildcarded: a new
        // destructive command has to be given a meaning here, or it cannot be run at all.
        match confirmed(what, yes) {
            Some(Confirmation::ClearInk) => self.clear(cx),
            Some(Confirmation::DeletePage) => self.delete_page(cx),
            None => {}
        }

        self.follow_pen_with_pointer();
        self.publish_screen();
        cx.notify();
    }

    /// Whether a question is waiting for an answer.
    fn asking(&self) -> bool {
        self.confirm.is_some()
    }

    /// Whether the palette at the pen is up.
    fn palette_is_open(&self) -> bool {
        self.palette.is_some()
    }

    /// Puts the palette at the pen, on the colour in hand.
    ///
    /// *Where the pen is*, not where the pointer is: the hand is over the page holding a pen, and this
    /// is a control for that hand — the app knows the pen's position to the last reading (see
    /// [`Self::last_cursor`]), so the palette arrives under the nib, where the hand already is. With no
    /// pen in range the pointer's own position is the second answer, and with neither — a window that
    /// has just opened — the middle of it, which is where the sheet is, so the palette never lands half
    /// off the window ([`palette_centre`]).
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A question, a name being typed, a list in front: each is already what the pen and the
        // keyboard obey, and a palette opened over one would be a second answer to the same question
        // (see [`Self::consume_ink`]).
        if self.asking()
            || self.naming.is_some()
            || self.home_is_open()
            || self.marks.is_open()
            || self.outline.is_open()
        {
            return;
        }

        let near = self
            .last_cursor()
            .map(PenCursor::position)
            .unwrap_or_else(|| {
                let at = window.mouse_position();

                [f32::from(at.x), f32::from(at.y)]
            });

        let size = window.bounds().size;

        self.palette = Some(Palette {
            centre: palette_centre(
                PaletteGeometry::of(self.unit_scale),
                near,
                [size.width.into(), size.height.into()],
            ),
            chosen: palette_in_hand(palette_colours(self.ink.mode()), self.ink_colour_in_hand()),
        });

        // The ring with the ghost gone and the system pointer in its place: with the palette up a
        // reading is a choice, not a stroke, and the hand aims the ring with the pointer rather than
        // writing on the page — so the pen's own cursor goes and the system's is what the hand sees
        // (see [`Self::ghost_suppressed`]). Published here, once, the same way a question does it
        // ([`Self::ask`]): the suppression is a state of the screen, not of a reading.
        self.follow_pen_with_pointer();
        self.publish_screen();
        self.touch_status();
        cx.notify();
    }

    /// Records that a chooser's list was opened or closed, and lets the cursor follow.
    ///
    /// The off answer is sent whether or not it was asked for: a press outside the box dismisses a list that
    /// was never open (the press was on the page), and saying so is cheaper than asking the chooser. What
    /// does *not* happen is a frame — a press that changed nothing about the list changes nothing on screen,
    /// and a cursor rule that has already been followed is not worth one.
    fn chooser_toggled(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.chooser_open == open {
            return;
        }

        self.chooser_open = open;

        // The list is a screen over the page for as long as it is up, so the ghost and the pointer swap over
        // exactly as they do for the palette (see [`Self::ghost_suppressed`]) — and the canvas has to be told,
        // or a ghost drawn a moment ago stays drawn.
        self.follow_pen_with_pointer();
        self.publish_screen();
        self.draw_canvas();
        cx.notify();
    }

    /// Takes the palette away.
    fn close_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            // And the ghost comes back with it, exactly as an answer brings it back (see [`Self::answer`]).
            self.follow_pen_with_pointer();
            self.publish_screen();
            self.touch_status();
            cx.notify();
        }
    }

    /// Takes what a press on the palette landed on.
    fn palette_take(&mut self, part: PalettePart, cx: &mut Context<Self>) {
        match part {
            // A tool *stays*: what it changes is the ring of colours around it, and a hand that has
            // just picked up the marker means to pick a marker's colour next — the two presses the bar
            // asks for, made as one gesture.
            PalettePart::Tool(place) => {
                if let Some(tool) = PALETTE_TOOLS.get(place).copied() {
                    self.palette_tool(tool, cx);
                }
            }
            // A colour is the answer the palette was opened for, so the palette goes with it.
            PalettePart::Colour(place) => self.palette_colour(place, cx),
        }
    }

    /// Writes in the colour at `place` on the ring, and puts the palette away.
    fn palette_colour(&mut self, place: usize, cx: &mut Context<Self>) {
        let highlighter = self.ink.mode() == Tool::Highlighter;

        let Some(swatch) = palette_colours(self.ink.mode()).get(place) else {
            return;
        };

        let (color, name) = (swatch.color, swatch.name);

        self.close_palette(cx);

        if highlighter {
            self.set_highlighter_color(color, cx);
        } else {
            // The pen's palette is the ring's for every tool that writes no colour of its own (see
            // [`palette_colours`]), so a colour taken from it is one the *pen* is about to write in, and
            // the eraser or the lasso goes out of hand with it.
            self.ink.set_mode(Tool::Pen);
            self.set_ink_color(color, cx);
        }

        // Said in words as well as in colour: the bar's swatch — the other place this is readable —
        // may be hidden, and the ring that was just pressed goes away with the answer.
        self.report(format!("writing in {name}"));
    }

    /// Puts a tool in hand from the palette, and leaves the palette up.
    fn palette_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.set_tool(tool, cx);

        // The keyboard goes to the colour in hand *of the ring it is now looking at*: a place on a ring
        // of four is not the place it was on a ring of twelve, and the colour in hand is the one answer
        // both palettes have (see [`palette_in_hand`]). Read before the palette is borrowed, because the
        // colour in hand is asked of the app.
        let home = palette_in_hand(palette_colours(tool), self.ink_colour_in_hand());

        if let Some(palette) = &mut self.palette {
            palette.chosen = home;
        }

        cx.notify();
    }

    /// Walks the keyboard's own place around the ring.
    fn palette_spin(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = palette_colours(self.ink.mode()).len();

        if let Some(palette) = &mut self.palette {
            palette.chosen = spun(palette.chosen, step, count);
            cx.notify();
        }
    }

    /// Reads a batch of readings as a *choice*, while the palette is up.
    ///
    /// Called from the pen's pump in place of the ink, because a tap is what a pen has instead of a
    /// click and this is the one control a pen is aimed at rather than written on. A landing point is
    /// converted exactly as the ghost cursor converts its own (see [`PenCursor::from_sample`]): a palette
    /// that answered a different point than the ghost is drawn at would take a colour the hand never
    /// touched.
    fn palette_from_pen(&mut self, samples: &[Reading], cx: &mut Context<Self>) {
        let Some(palette) = self.palette else {
            return;
        };

        let colours = palette_colours(self.ink.mode()).len();
        let geometry = PaletteGeometry::of(self.unit_scale);
        let landed = samples
            .iter()
            .filter(|sample| sample.phase == crate::input::Phase::Down)
            .find_map(|sample| {
                let at = PenCursor::from_sample(*sample, self.scale)?.position();

                palette_at(geometry, palette.centre, at, colours)
            });

        if let Some(part) = landed {
            self.palette_take(part, cx);
        }
    }

    /// Empties the page in front: every stroke on it, and its history with them.
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
        // Where the reader was, before the page is left: a page keeps the place it was reading at for as long as the
        // note is open, so turning back to it comes back to the line one was on. A page nobody has looked at yet has
        // no place to come back to, and opens at its own top (`Viewport::set_pan` of nothing).
        self.pans.insert(self.page_index, self.view.pan());

        self.close_page();
        self.show_page(page);

        let remembered = self.pans.get(&page).copied();
        self.view.set_pan(remembered.unwrap_or((0.0, 0.0)));

        self.remember_page();
        // The status line names the page, so it is stale as soon as the page changes.
        self.touch_status();
    }

    /// Shows a page and reads its ink in: the half of a turn that closes nothing.
    ///
    /// Split from [`Self::turn_to`] because the two commands that move the *page list* itself —
    /// `add_page` and `delete_page` — close the page they are leaving **before** the pages are
    /// renamed, which is the order the design states (see [VIEW.md](../../doc/VIEW.md), "Turning a
    /// page"). A close that ran after the rename closed a page nobody was on: it compacted the page
    /// that took the place of the one being left, and — while a write was filed under the view's
    /// index rather than the ink's page — it wrote the ink of the page being left under that page's
    /// name, which is what put one page's writing on the next page's sheet.
    fn show_page(&mut self, page: usize) {
        self.page_index = page;
        self.ink.go_to(page);

        if let Err(error) = self.load_page_ink(page) {
            self.report(error.to_string());
        }
    }

    /// Writes what a page still owes the note, and folds its ink into chunks: the page is closing.
    fn close_page(&mut self) {
        self.persist(Instant::now(), true);

        // The page being closed is the one the ink is on, for the reason [`Self::persist`] reads it
        // from there: an index that had already moved would compact a page nobody is leaving.
        let page = page_of_write(&self.ink);
        let touched =
            self.rewritten.remove(&page) || self.saved.get(&page).copied().unwrap_or(0) > 0;

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
        // The pages after the insertion are not the pages they were: a place remembered for the page that is now one
        // along belongs to the wrong one (`Self::turn_to` is where those places are kept, and this is why it is a map
        // of positions rather than anything the note stores).
        self.pans.clear();
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

        // The page left behind was closed above, *before* the pages were renamed — that is the order
        // the design asks for — so the turn here is only the half that shows the page that was made.
        self.show_page(at);
        // And a page nobody has read opens at its own **top**: the sheet in the view was panned to wherever the page
        // before it was being read, and carrying that over is how a page added at the end starts half-way down (see
        // [`Self::turn_to`], which is the same rule for a page turn proper).
        self.view.reset_pan();
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

        // Every page after the one that has gone has moved along, so the places remembered for them mean nothing now
        // (see [`Self::turn_to`]).
        self.pans.clear();

        if let Some(note) = &mut self.note {
            if let Err(error) = note.store_mut().delete_page(self.page_index as u64) {
                self.report(error.to_string());
            }
        }
        self.loaded.remove(&self.page_index);
        self.saved.remove(&(self.page_index as u64));
        self.shifted.remove(&(self.page_index as u64));
        self.rewritten.remove(&(self.page_index as u64));
        self.rename_pages(self.page_index as u64 + 1, -1);

        self.ink.remove_at(self.page_index);
        // And the mark, if the page had one: what was deleted is a page, so the mark for the page that
        // used to be at this position is gone with it rather than pointing at its neighbour.
        self.bookmarks.removed_at(self.page_index);
        // As in `add_page`: the page being left was closed before the pages were renamed, so this is
        // the showing half of a turn rather than another close of a page nobody is on.
        self.show_page(show);
        // The page shown is not the one that was panned — the view still holds the place of the page that has just
        // gone — and every place remembered for the pages after it means nothing now (see [`Self::turn_to`]).
        self.view.reset_pan();
        self.save_note_state();

        self.report(format!(
            "deleted a page ({} left)",
            self.page_total().saturating_sub(1).max(1)
        ));
        cx.notify();
    }

    /// Turns the page being read, clockwise for `turns > 0`.
    ///
    /// The ink does not move: a rotation belongs to the *page* (see [`crate::pages::Quarters`]), so what
    /// changes is where the page's own coordinates are drawn — the writing that was on the page before
    /// the turn reads the same way after it, because it was always in the page's coordinates and the page
    /// is what turned. Nothing is written to the note's ink for this: the pages are the note's own list
    /// (see [`crate::pages::PageEntry`]), and the turn is a field of one row of it.
    fn turn_page(&mut self, turns: i32, cx: &mut Context<Self>) {
        self.pages.turn(self.page_index, turns);

        // The sheet, the page's bitmap and the ink's transform all follow from the list on the next
        // frame, so nothing here has to describe them again — but the note has to keep the turn, and the
        // reader has to be told what it came to.
        self.save_note_state();
        self.report(self.turn_message(turns, false));
        cx.notify();
    }

    /// Turns every page of the note, clockwise for `turns > 0`.
    ///
    /// One command over the whole list rather than a rotation of the window: each page keeps its own
    /// turn, so turning the note round and then turning one page the other way leaves that page where
    /// the reader put it — and turning the note back is the same command the other way.
    fn turn_every_page(&mut self, turns: i32, cx: &mut Context<Self>) {
        self.pages.turn_all(turns);
        self.save_note_state();
        self.report(self.turn_message(turns, true));
        cx.notify();
    }

    /// What a turn says on the status line: which way, and how far the page in front ended up.
    fn turn_message(&self, turns: i32, every: bool) -> String {
        format!(
            "turned {} {} — the page in front is {}° from upright",
            if every { "every page" } else { "this page" },
            if turns > 0 {
                "clockwise"
            } else {
                "counter-clockwise"
            },
            self.pages.rotation(self.page_index) as u32 * 90
        )
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
            format!(
                "no page is marked yet \u{2014} {} marks the page in front",
                keys::label(keys::chord::MARK)
            )
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
            format!(
                "no page is marked yet \u{2014} {} marks the page in front",
                keys::label(keys::chord::MARK)
            )
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

        self.loaded = self
            .loaded
            .iter()
            .map(|page| rename(*page as u64) as usize)
            .collect();
        self.saved = std::mem::take(&mut self.saved)
            .into_iter()
            .map(|(page, count)| (rename(page), count))
            .collect();
        self.shifted = std::mem::take(&mut self.shifted)
            .into_iter()
            .map(|(page, shift)| (rename(page), shift))
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

        // The note's own answer for vertical sync, handed to the surface now: the layer that presents
        // is the platform's, and a note opened asking for something the surface already offers stops
        // offering it at the next frame.
        self.apply_vsync();

        // A whole note, not a page turn: the pan is reset with the zoom, because the sheet that was
        // being looked at is not the sheet in hand any more.
        self.view = Viewport::new(self.settings.zoom);
        // And the pages of the note that has just gone are not the pages of this one: a place remembered for *their*
        // page 4 means nothing on this note's page 4, which nobody has looked at yet.
        self.pans.clear();

        // The writer's connection is opened here, before its thread runs, so that a note that cannot
        // be written is something the user is told about rather than something they discover the next
        // time they look for their ink.
        let writer = NoteWriter::spawn(note.dir())?;

        let pdf = match note.document()? {
            Some((name, bytes)) => PdfDocumentView::open_bytes(
                name,
                bytes,
                self.pdf_render_pixel_width(),
                self.settings.memory_mode.cache_budget_bytes(),
            )?,
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
            Some(note) => note
                .store()
                .load_page(page as u64, crate::history::HISTORY_EDITS)?,
            // Nothing is open: the page in front of the user is a blank sheet, and what is drawn on
            // it belongs to a note that does not exist yet.
            None => crate::store::LoadedPage {
                strokes: Vec::new(),
                history: crate::history::History::new(),
            },
        };

        // What the note already holds is what has been written as far as the writer is concerned,
        // and it is the number a later append is measured against.
        let written = match &self.note {
            Some(note) => note.store().stroke_count(page as u64)?,
            None => 0,
        };

        // The page's *history* comes back with its ink: a page opened on Friday arrives with the edits it went
        // through on Monday, which is the whole of what makes undo outlive a session.
        let mut ink = crate::ink::InkDocument::from_strokes(stored.strokes);
        ink.set_history(stored.history);

        self.ink.put_page(page, ink);
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
            && !self.asking()
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

        if keys::is_primary(keystroke.modifiers, cx) {
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
    fn note_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        // The palette at the pen takes the keyboard while it is up: the arrows walk the ring, Enter
        // takes the colour the ring marks, Escape puts it away — and nothing else the keyboard means
        // reaches the sheet or the bar under it. That *is* the palette's modality: like a confirmation
        // box, it is a state the app is in rather than a panel beside what it covers (see
        // [`Self::consume_ink`]), so there is no press for a stroke to arrive on and no command to be
        // issued under it.
        if let Some(palette) = self.palette {
            match keystroke.key.as_str() {
                "escape" => self.close_palette(cx),
                "enter" | "space" => self.palette_take(PalettePart::Colour(palette.chosen), cx),
                "left" | "up" => self.palette_spin(-1, cx),
                "right" | "down" => self.palette_spin(1, cx),
                // The chord that put it up takes it away again; a press on the palette's own
                // background is the other way out (see [`Self::palette_overlay`]).
                "c" if keys::is_primary(keystroke.modifiers, cx) && keystroke.modifiers.shift => {
                    self.close_palette(cx)
                }
                _ => {}
            }

            return;
        }

        // The palette at the pen: `C` for colour, on the one chord every drawing app spells it with —
        // and on *both* modifiers, so that the plain `Ctrl+C` a text field would expect stays
        // where it is.
        if keys::is_primary(keystroke.modifiers, cx)
            && keystroke.modifiers.shift
            && keystroke.key.as_str() == "c"
        {
            self.open_palette(window, cx);
            return;
        }

        if !keys::is_primary(keystroke.modifiers, cx) {
            match keystroke.key.as_str() {
                "escape" => {
                    if self.asking() {
                        // A question is answered before anything else Escape means: the box is in front of
                        // everything on screen, and "no" is what this key says to it.
                        self.answer(false, cx);
                    } else if self.naming.is_some() {
                        self.stop_note_name(window, cx);
                    } else if self.chooser_open {
                        // A chooser's list is dismissed by Escape too, and the app has to hear it: the flag is
                        // what keeps the pointer the system's while the list is up (see [`Self::ghost_suppressed`]).
                        self.chooser_toggled(false, cx);
                    } else if self.ink.has_selection() {
                        // A selection is let go of *before* the note is left: a reader who has just lassoed
                        // something meant to put it down, not to go home — the same precedence the name being
                        // typed follows one line up.
                        self.deselect(cx);
                    } else {
                        self.show_home(cx);
                    }
                }
                // What a selection is for besides dragging: exactly what is in hand, removed. Only while a name
                // is not being typed, because there the keys belong to the field.
                "delete" | "backspace" if self.naming.is_none() => self.delete_selection(cx),
                // The arrows turn the page — the one thing a keyboard is asked for here, and the same
                // two commands the page pill's buttons are. Not while a name is being typed: there the
                // arrows belong to the field, and a caret that turned the page instead would be a trap.
                "left" if self.naming.is_none() => self.previous_page(cx),
                "right" if self.naming.is_none() => self.next_page(cx),
                _ => {}
            }
            return;
        }

        // The bookmark chords first, because the letter is shared: the command chord with `B` marks the
        // page in front and with `Shift+B` opens the list of what is marked (`Ctrl+B` and `Ctrl+Shift+B`
        // on Windows, `⌘B` and `⇧⌘B` on macOS) — the two halves of one idea, which is why they are the
        // two halves of one key.
        if keystroke.key.as_str() == "b" {
            if keystroke.modifiers.shift {
                self.show_marks(cx);
            } else {
                self.toggle_bookmark(cx);
            }
            return;
        }

        // Turning a page: the key alone turns the page in front clockwise, Shift turns that page the
        // other way, and Alt turns the same two for every page of the note. Four commands, one key — the
        // same shape as the two bookmark chords, and the same four commands the bar's buttons are.
        if keystroke.key.as_str() == "r" {
            match (keystroke.modifiers.alt, keystroke.modifiers.shift) {
                (false, false) => self.turn_page(1, cx),
                (false, true) => self.turn_page(-1, cx),
                (true, false) => self.turn_every_page(1, cx),
                (true, true) => self.turn_every_page(-1, cx),
            }
            return;
        }

        match keystroke.key.as_str() {
            // The document's own contents, under the letter that already means "open a file" — a table of
            // contents is what a document is opened *by*, so the two chords sit on one key as the two
            // bookmark chords do. A document without one is answered in words (see [`Self::show_outline`]).
            "o" if keystroke.modifiers.shift => self.show_outline(cx),
            // The bar, on the key the two platforms already agree means "show or hide a strip of the
            // interface": `⌘\` on a Mac and `Ctrl+\` on Windows (see [`keys::chord::BAR`]). The *whole*
            // toggle, not just the flag — hiding the bar gives the page the strip back, and the pen's
            // ghost and the system pointer have to be told now rather than on the next frame (see
            // [`Self::set_toolbar_shown`]).
            "\\" if !keystroke.modifiers.shift => {
                self.set_toolbar_shown(!self.settings.show_toolbar, cx)
            }
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
        if !keys::is_primary(event.keystroke.modifiers, cx) {
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
        if !keys::is_primary(event.keystroke.modifiers, cx) {
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
        let title = note.store().title().ok().flatten().unwrap_or_else(|| {
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
    ///
    /// Three things decide it, in this order: the sheet's width times the zoom (what the page is drawn
    /// at), the screen's scale (how many device pixels each logical pixel is), and the note's
    /// [`MemoryMode`] (whether the app may spend those pixels). The mode supplies the last multiplier
    /// rather than a constant here, because it is the one the reader chose — see [`MemoryMode::render_scale`],
    /// which is 2.0 in `Room` and 1.0 in `Lean`.
    ///
    /// The result is then clamped to the mode's own ceiling ([`MemoryMode::pixel_ceiling`]), because a
    /// zoomed-in page would otherwise ask for a bitmap the lean cache could not hold even one of.
    fn pdf_render_pixel_width(&self) -> u32 {
        let logical = self.settings.page_display_width * self.view.zoom();
        let wanted = logical * self.scale.max(1.0) * self.settings.memory_mode.render_scale();

        quantise_width(wanted).min(self.settings.memory_mode.pixel_ceiling())
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

        // The reader's turn is part of the request: it changes the pixels, so it belongs in the cache
        // key and in the size the page is rasterised at, and a page turned after it was read once must
        // not be served the bitmap from before (see [`crate::pdf::PageKey`]).
        let turns = self.pages.rotation(self.page_index);
        let wanted = PageRequest::new(document_page, self.pdf_render_pixel_width())
            .grayscale(self.settings.grayscale_pages)
            .turned(turns);

        if let Some(page) = self.pdf.page_for_frame(wanted) {
            self.plan_pdf(wanted);
            return Some(page);
        }

        // Nothing for this page at any rung: pay for the cheapest one now, so the frame has
        // something to draw, then leave the rung the zoom wanted to the pump.
        let preview = PageRequest::new(document_page, PDF_PIXEL_WIDTHS[0])
            .grayscale(self.settings.grayscale_pages)
            .turned(turns);
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

        let page = self.pdf.render_page(request)?;

        self.timings
            .count_rasterised(self.pdf.rasterised() - before);
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
    fn page_layout(&self, window: (f32, f32), page: Option<&RenderedPage>) -> Sheet {
        // Which way up the page is: the *note's* answer, and not the document's — a PDF page arrives
        // with its own `/Rotate`, and that is already in the size below, because Pdfium reports a page
        // the way it draws it.
        let turns = self.pages.rotation(self.page_index);

        let paper = match page {
            // A PDF brings its own shape; only how wide it is drawn is the app's choice. A page the
            // reader has turned comes back with the two swapped, which is the shape drawn.
            Some(page) => page.display_size(self.settings.page_display_width),
            // The blank sheet takes both its shape and its scale from the chosen canvas size — and a
            // page the reader has turned is that shape on its side, because the paper in hand is the
            // same sheet either way: it is the *screen* that shows it the other way round.
            None => {
                let (width, height) = self
                    .settings
                    .canvas_size
                    .display_size(self.settings.page_display_width);

                if swaps_axes(turns) {
                    (height, width)
                } else {
                    (width, height)
                }
            }
        };

        Sheet {
            paper,
            origin: self.view.origin(paper, window, PAGE_MARGIN),
            window,
            zoom: self.view.zoom(),
            rotation: turns,
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

        // Which way up the page in front is, said only when the reader has turned it: the right way up is
        // the ordinary case, and a number on every page would be noise — the same rule the marks follow.
        let turns = self.pages.rotation(self.page_index);
        if turns > 0 {
            parts.push(format!("turned {}°", turns as u32 * 90));
        }

        // What a lasso has hold of, said only when it has hold of something: it is the answer to "what would
        // Delete take?", and a zero on every frame is noise — the same rule the marks above follow.
        if self.ink.has_selection() {
            parts.push(format!("{} selected", self.ink.selected_count()));
        }

        // How far the page can be taken back, and how far forward: a count out of the *session's* memory, which the
        // note's own log may reach deeper than. Said only while there is a history to speak of, and asked of the
        // page because it is a fact about the page.
        let (back, forward) = self.ink.history_depth();
        if back > 0 || forward > 0 {
            parts.push(format!("history {back} back, {forward} forward"));
        }

        // The marks, said only when there are any: it is the answer to "which pages did I keep", and a
        // zero on every frame is noise — the same rule the off-the-sheet readings follow below.
        if !self.bookmarks.is_empty() {
            parts.push(format!("{} marked", self.bookmarks.len()));
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
        // measurement this app runs on itself. A locked zoom says so here as well as in the pill: the
        // lock is the reason a pinch or a Fit did nothing, and a person who pressed Fit wants to know
        // that it was not a bug.
        parts.push(format!(
            "view {:.0}%{}",
            self.view.zoom() * 100.0,
            if self.settings.zoom_locked {
                " (locked)"
            } else {
                ""
            }
        ));

        // The memory mode, said only when it is the lean one: `Room` is what the app always did and
        // naming it every second would be noise. `Lean` is a state a reader chose for a reason and may
        // need to be reminded of — it is why a page looks soft at 200% and why `MiB` stopped climbing,
        // and both of those are questions this line is the answer to (see [`MemoryMode`]).
        if self.settings.memory_mode.is_lean() {
            parts.push(format!(
                "memory {} ({:.0} MiB cache, {} px)",
                self.settings.memory_mode.label(),
                self.settings.memory_mode.cache_budget_bytes() as f64 / (1024.0 * 1024.0),
                self.settings.memory_mode.pixel_ceiling()
            ));
        }

        // The measurement session, in front of the live meters rather than instead of them: the meters are
        // what is happening now, and this is the stretch that was measured (see [`crate::timing::Session`]).
        if self.session_at.is_some() {
            parts.push(format!(
                "measuring: {} stops it",
                keys::label(keys::chord::MEASURE)
            ));
        } else if let Some(measured) = self.measured {
            parts.push(measured.summary());
        }

        // The palette at the pen says how to leave it, the way a running session says how to stop it:
        // the ring is on screen but the *key* is not on it, and this line is where the app names the
        // states a reader is in (see [`keys::chord::PALETTE`]).
        if self.palette_is_open() {
            parts.push(format!(
                "palette: {} or Esc puts it away",
                keys::label(keys::chord::PALETTE)
            ));
        }

        parts.push(self.timings.summary());

        // What this process is costing the machine, when the platform can say. Put after the timing
        // meters because it answers a question those cannot: a meter says what one path took and this
        // says what the *process* is holding and burning, which is the figure a reader compares against
        // the rest of their machine. Empty while hidden and empty on a platform with no counters, so
        // the line keeps its shape on a build that cannot measure itself.
        if !self.vitals.is_empty() {
            parts.push(self.vitals.clone());
        }

        // What the platform's own capture says about itself — how it is attached, how coarsely it
        // batches readings, which axes the digitizer can report, whether the pointer hook took, and
        // which way the pen leans — is diagnostic rather than data. None of it is a fact about the
        // note, and it is the one part of this line that is spelled by the platform, so leaving it out
        // is what makes the line the same sentence on every OS. `CHEAP_NOTE_TRACE` is where it is read
        // (the canvas and the pointer keep their own lines there too).
        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            match self.pen.stats() {
                Some(stats) => {
                    parts.push(format!(
                        "{}  {:.1} readings/message",
                        stats.summary, stats.mean_batch
                    ));
                    parts.push(format!(
                        "pen: {}",
                        if stats.axes.is_empty() {
                            String::from("position only")
                        } else {
                            stats.axes.join("+")
                        }
                    ));
                }
                None => parts.push(self.pen.status().to_string()),
            }

            // Worth saying only when it is missing. A window whose pointer could not be hooked still
            // works — it simply shows two cursors — and this is the only way to know which of the two
            // it is (see [`crate::platform::SystemPointer`]).
            if self.system_cursor.is_none() {
                parts.push(String::from("no pointer hook"));
            }

            // The live lean, when the pen is here to have one: the number the ghost cursor is drawing,
            // and where a person checks that the cursor tells the truth about which way the pen leans.
            if let Some(tilt) = self.last_cursor().and_then(PenCursor::tilt) {
                parts.push(format!("lean {tilt}"));
            }
        }

        // A log that could not be opened is worth saying once, in the line it was meant to record:
        // the app still works, and this is the only place the failure would otherwise be seen.
        if !self.status_log.is_on() {
            parts.push(String::from("log: off"));
        }

        if !self.message.is_empty() {
            parts.push(self.message.clone());
        }

        parts.join("   ·   ")
    }

    /// Rebuilds the status line, for a change the user made and expects to see at once.
    fn touch_status(&mut self) {
        self.status = self.compose_status();
    }

    /// Samples the machine's own counters, and keeps the clause they came to.
    ///
    /// Called on the once-a-second status refresh and from nowhere else. Two reasons it is not part of
    /// [`Self::touch_status`]: a reader changing a switch should not pay for a `/proc` walk or a
    /// `task_info` call, and the numbers must not move on their own — a memory figure that changed
    /// every time an unrelated switch was thrown would be a figure nobody could read.
    ///
    /// Asking is skipped entirely while the status line is hidden, which is the one case where a
    /// reader has said outright that they are not reading numbers.
    fn sample_vitals(&mut self) {
        if !samples_vitals(self.settings.show_status) {
            // Cleared rather than left stale: a line that was shown, hidden and shown again must not
            // print a figure from a minute ago before the first new sample lands.
            self.vitals.clear();
            return;
        }

        let Some(stats) = self.system_stats.as_mut() else {
            return;
        };

        self.vitals = match stats.vitals() {
            Some(vitals) => vitals.summary(),
            // A platform that cannot say leaves the clause empty. It is not an error and it is not a
            // zero: the line simply has one fewer thing in it, which is the same rule the counters
            // follow for a note that has no marks, no history and no document.
            None => String::new(),
        };
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
            tool_button(
                "tool-pen",
                IconName::Pen,
                "Pen",
                tool == Tool::Pen,
                cx,
                |app, cx| app.set_tool(Tool::Pen, cx),
            )
            .into_any_element(),
            // The marker sits with the pen, because it is a second way of *writing*: it lays down a band rather
            // than a line, and the band is a stroke like any other — wide, and a colour the page shows through, so
            // what it marks stays readable. The tooltip is the whole of the difference.
            tool_button(
                "tool-highlighter",
                IconName::Highlighter,
                "Highlighter: a wide, translucent band over what you mark",
                tool == Tool::Highlighter,
                cx,
                |app, cx| app.set_tool(Tool::Highlighter, cx),
            )
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
            // The third tool takes ink rather than adding or removing it: sweep the writing that is wanted,
            // then drag it somewhere or press Delete. The tooltip says the whole of it, because a lasso has no
            // other way to be learned.
            tool_button(
                "tool-lasso",
                IconName::Lasso,
                "Lasso: sweep around the ink to take it, then drag it or press Delete",
                tool == Tool::Lasso,
                cx,
                |app, cx| app.set_tool(Tool::Lasso, cx),
            )
            .into_any_element(),
        ];

        // Reading order is left to right, so the commands sit in the order they are reached for:
        // take the last stroke back, put it forward again, take everything back, open something
        // else, write it out.
        //
        // Offered only when the page's history says they would do something: a button that sometimes does
        // nothing is what a broken command looks like.
        let mut actions = vec![
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
                keys::tooltip::UNDO.as_str(),
                self.ink.can_undo(),
                cx,
                |app, cx| app.undo(cx),
            )
            .into_any_element(),
            icon_button(
                "redo",
                IconName::Redo2,
                keys::tooltip::REDO.as_str(),
                self.ink.can_redo(),
                cx,
                |app, cx| app.redo(cx),
            )
            .into_any_element(),
            icon_button(
                "clear",
                IconName::Trash,
                "Clear the ink (asks first)",
                true,
                cx,
                |app, cx| app.ask(Confirmation::ClearInk, cx),
            )
            .into_any_element(),
        ];

        // The command that acts on a selection exists only while there is one — the rule the undo buttons
        // follow, and the reason it is the *element* that comes and goes rather than a button that is disabled:
        // a command with nothing to act on is what a broken command looks like. It is worth a button of its
        // own because the hand holding the pen has no Delete key, and `Scissors` rather than a second `Trash`,
        // which the bar already spells "clear the page".
        if self.ink.has_selection() {
            actions.push(
                icon_button(
                    "selection-delete",
                    IconName::Scissors,
                    "Delete the selected ink (Delete)",
                    true,
                    cx,
                    |app, cx| app.delete_selection(cx),
                )
                .into_any_element(),
            );
        }

        // And the file: what was opened, and what is written out. Pushed after the page's own commands, which
        // is where they sit in the row.
        actions.push(
            icon_button(
                "open-note",
                IconName::FolderOpen,
                "Open a note or a PDF",
                true,
                cx,
                |app, cx| app.prompt_for_pdf(cx),
            )
            .into_any_element(),
        );
        actions.push(
            icon_button(
                "save-note",
                IconName::Save,
                "Save the note",
                true,
                cx,
                |app, cx| app.save(cx),
            )
            .into_any_element(),
        );

        // The bar's own switches: what is shown of the *interface*, and nothing about the writer. The pen's
        // ghost is deliberately not one of them — it is the cursor, and a pen with none is not a state
        // anybody chose (see [`Self::pen_cursor`]).
        let switches = vec![
            // The one switch with a chord, so the one that names it on hover: `⌘\` or `Ctrl+\`, the panel
            // toggle both platforms already know (see [`keys::chord::BAR`]).
            chorded_switch(
                "show-bar",
                "Bar",
                keys::tooltip::BAR.as_str(),
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
            Label::new("Untitled note")
                .text_color(muted)
                .into_any_element()
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

    /// The box a destructive command is confirmed in.
    ///
    /// Over everything and centred: it is a question about the page in front, so it stands in front of it rather than
    /// beside it. Two answers, and no way out that is not one of them — a click off the box and Escape both *refuse*
    /// (see [`Self::note_key_down`]) — because a question that can be got rid of without answering is a question
    /// nobody reads.
    fn confirm_box(&self, what: Confirmation, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline, muted) = (
            theme.title_bar,
            theme.title_bar_border,
            theme.muted_foreground,
        );
        let words = what.words();

        // The two answers, in the bar's own style: the one that carries the command out is the accent, and the one
        // that refuses is the quieter of the two — a reader glancing at the box should be able to tell which is which
        // without reading both. The verb is on the button ("Clear the page"), not "OK": the last thing read before a
        // page goes should say what is about to happen.
        let keep = Button::new("confirm-keep")
            .label(words.refuse)
            .ghost()
            .on_click(cx.listener(|app, _, _, cx| app.answer(false, cx)));

        let agree = Button::new("confirm-agree")
            .label(words.agree)
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(theme.accent)
                    .hover(theme.accent)
                    .active(theme.accent)
                    .foreground(theme.accent_foreground),
            )
            .on_click(cx.listener(|app, _, _, cx| app.answer(true, cx)));

        div()
            .id("confirm-backdrop")
            .absolute()
            .inset_0()
            // The desk behind the question, dimmed: the page is still there, and it is what the question is about.
            .bg(theme.overlay)
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|app, _, _, cx| app.answer(false, cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p(px(16.0))
                    .w(px(360.0))
                    .rounded(theme.radius_lg)
                    .bg(surface)
                    .border_1()
                    .border_color(hairline)
                    .shadow_md()
                    // The box is not the backdrop: a click *on* it is not an answer either way, and without this the
                    // backdrop's own handler would hear it and refuse.
                    .occlude()
                    .child(div().child(words.heading))
                    .child(div().text_sm().text_color(muted).child(words.detail))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(keep)
                            .child(agree),
                    ),
            )
    }

    /// The palette at the pen: a ring of the tool's colours around the tool in hand, on a disc of its
    /// own (see [`Self::open_palette`]).
    ///
    /// An *overlay* rather than a screen, because the page is still in front of the reader and what is
    /// being chosen is a colour *for* it — and because the pen obeys an app state rather than a
    /// rectangle (see [`Self::consume_ink`]): one state holds the ink off and takes a tap as a choice,
    /// which a panel covering part of the window could not do.
    ///
    /// Every control is placed by [`palette_point`] from the palette's own middle, and the whole palette
    /// is put where the pen was ([`palette_centre`]) — so the same numbers answer the drawing, a mouse's
    /// click, a pen's tap *and* the keyboard ([`palette_at`]).
    fn palette_overlay(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline, accent, overlay, well_fill) = (
            theme.popover,
            theme.border,
            theme.primary,
            theme.overlay,
            // The fill of the well the tools sit in: the theme's quiet grey, which is the one colour the
            // app uses for a surface *behind* controls rather than for a control.
            theme.muted,
        );

        let mode = self.ink.mode();
        let colours = palette_colours(mode);
        // The palette's own size on this platform: the design's numbers, in the logical pixels the
        // screen it is drawn on measures in (see [`PaletteGeometry`]).
        let geometry = PaletteGeometry::of(self.unit_scale);

        // Where the ring is measured from *inside* this palette's own square: its middle, which is one
        // radius in from each side. Every placement below is in the box's coordinates, so the box is put
        // where the pen was and nothing else has to know about that.
        let middle = [geometry.radius(), geometry.radius()];

        // Collected before they are chained, for the reason the bar collects its own: each
        // `cx.listener` takes the context mutably (see [`Self::top_bar`]).
        let mut discs: Vec<AnyElement> = Vec::new();
        for (place, swatch) in colours.iter().enumerate() {
            let at = palette_point(middle, geometry.ring(), place, colours.len());

            discs.push(
                placed(
                    at,
                    geometry.disc_box(),
                    palette_disc(swatch, place, palette.chosen == place, accent, geometry, cx),
                )
                .into_any_element(),
            );
        }

        let mut tools: Vec<AnyElement> = Vec::new();
        for (place, tool) in PALETTE_TOOLS.iter().enumerate() {
            let Some((id, icon, label)) = palette_face(*tool) else {
                continue;
            };

            let at = palette_point(middle, geometry.tool_ring(), place, PALETTE_TOOLS.len());

            tools.push(
                placed(
                    at,
                    geometry.tool_disc(),
                    palette_tool_chip(
                        id,
                        icon,
                        label,
                        *tool == mode,
                        geometry,
                        cx,
                        move |app, cx| app.palette_take(PalettePart::Tool(place), cx),
                    ),
                )
                .into_any_element(),
            );
        }

        // The middle is the tools' own zone rather than more white. The well is what makes the ring of
        // colours and the three tools beside the hub read as two things instead of thirteen discs on one
        // field, and it is the first child so that everything else sits *in* it.
        let well = div()
            .absolute()
            .left(px(middle[0] - geometry.well() / 2.0))
            .top(px(middle[1] - geometry.well() / 2.0))
            .w(px(geometry.well()))
            .h(px(geometry.well()))
            .rounded_full()
            .bg(well_fill);

        // The hub: the colour the next stroke is written in, which is what a palette is opened to *see* —
        // the answer at the centre, rather than hunted for among eleven others. It is set in a ring of
        // white, so that the one thing in the middle that is not a control does not look like one.
        let in_hand = self.ink_colour_in_hand();

        let hub = div()
            .absolute()
            .left(px(middle[0] - geometry.hub() / 2.0))
            .top(px(middle[1] - geometry.hub() / 2.0))
            .w(px(geometry.hub()))
            .h(px(geometry.hub()))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(surface)
            .shadow_sm()
            .child(colour_chip(
                in_hand,
                geometry.hub() - geometry.hub_ring() * 2.0,
                colour_edge(in_hand, cx.theme()),
                true,
            ));

        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            // Dimmed like a question's backdrop, and for the same reason: while the palette is up the pen
            // writes nothing, and a page still fully lit would say otherwise.
            .bg(overlay)
            .on_click(cx.listener(|app, _, _, cx| app.close_palette(cx)))
            .child(
                div()
                    .id("palette")
                    .absolute()
                    .left(px(palette.centre[0] - geometry.radius()))
                    .top(px(palette.centre[1] - geometry.radius()))
                    .w(px(geometry.radius() * 2.0))
                    .h(px(geometry.radius() * 2.0))
                    .rounded_full()
                    .bg(surface)
                    .border_1()
                    .border_color(hairline)
                    // The widest shadow the theme has: the palette is the only thing on the screen while
                    // it is up and the page behind it is dimmed, so it can afford to be seen to float.
                    .shadow_lg()
                    // The palette is not the backdrop: a press *on* it is a press on a control, and
                    // without this the backdrop's own handler would hear it and put the palette away.
                    .occlude()
                    .child(well)
                    .child(hub)
                    .children(tools)
                    .children(discs),
            )
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

        // The palette the ink circle shows follows the *tool*: a pen's colours and a highlighter's are two
        // different sets of answers to "what does the next stroke look like" (see `canvas::HIGHLIGHTER_COLORS`), and
        // a reader who has just picked up the marker should not have to pick a colour out of the pen's row to get
        // the marker they meant.
        let mut ink: Vec<AnyElement> = Vec::new();
        if self.ink.mode() == Tool::Highlighter {
            for swatch in HIGHLIGHTER_COLORS.iter() {
                ink.push(
                    swatch_button(
                        swatch,
                        self.settings.highlighter_color == swatch.color,
                        accent,
                        true,
                        cx,
                        |app, color, cx| app.set_highlighter_color(color, cx),
                    )
                    .into_any_element(),
                );
            }
        } else {
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
        }

        // The display's clock is a switch only where the surface can honour it: a reader whose
        // platform cannot make a frame wait is not offered a control that does nothing. The divider
        // stays either way, so hiding the switch is not a hole in the row.
        let vsync: Vec<AnyElement> = if self.honours_vsync() {
            vec![
                toolbar_divider(hairline).into_any_element(),
                visibility_switch(
                    "vsync",
                    "V-Sync",
                    self.settings.vsync,
                    cx,
                    |app: &mut NoteApp, on: bool, cx: &mut Context<NoteApp>| app.set_vsync(on, cx),
                )
                .into_any_element(),
                toolbar_divider(hairline).into_any_element(),
            ]
        } else {
            vec![toolbar_divider(hairline).into_any_element()]
        };

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
            .child(self.chooser("chooser-sheet", &self.sheet_select, cx))
            .child(toolbar_divider(hairline))
            .child(control_label("Style", muted))
            .child(self.chooser("chooser-rule", &self.rule_select, cx))
            .child(toolbar_divider(hairline))
            .child(control_label("Paper", muted))
            .children(paper)
            .child(toolbar_divider(hairline))
            .child(control_label("Ink", muted))
            .children(ink)
            .child(toolbar_divider(hairline))
            .child(control_label("Weight", muted))
            .child(self.chooser("chooser-pen", &self.pen_select, cx))
            .child(toolbar_divider(hairline))
            // The canvas's own ceiling, beside the render option it is a sibling of: what it paces is the
            // canvas's work, not the display's. See [`FrameCap`].
            .child(control_label("Frames", muted))
            .child(self.chooser("chooser-frames", &self.frame_select, cx))
            // The display's clock, beside the canvas's own ceiling, where the surface can honour it:
            // the two are the whole of what paces a frame (see [`Settings::vsync`]).
            .children(vsync)
            // The pen's own steadiness, beside the two things that pace a frame: what it corrects is the
            // gesture that moves the sheet — the pan and the zoom a held modifier makes of the nib — and it is
            // the one setting here that is about the *hardware*. Named for the gesture rather than for the pen,
            // because writing is not corrected at all: only a move of the sheet is.
            .child(visibility_switch(
                "pen-jitter",
                "Steady move",
                self.settings.pen_jitter,
                cx,
                |app: &mut NoteApp, on: bool, cx: &mut Context<NoteApp>| app.set_pen_jitter(on, cx),
            ))
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
            // The memory mode, beside the page's other render option because it is the same kind of
            // thing: an answer about *this document's* pictures rather than about the pen. A chooser
            // rather than a switch because both answers have a name and a cost — `Room` spends what the
            // sheet is worth, `Lean` a quarter of it (see [`MemoryMode`]).
            .child(toolbar_divider(hairline))
            .child(control_label("Memory", muted))
            .child(self.chooser("chooser-memory", &self.memory_select, cx))
            // The pen's own half of that row: a light hand writes pale as well as thin. Beside the weights it
            // belongs to, and after the page's options so that the pen's controls stay together at the end.
            .child(visibility_switch(
                "pressure-fades",
                "Soft ink",
                self.settings.pressure_fades,
                cx,
                |app: &mut NoteApp, on: bool, cx: &mut Context<NoteApp>| {
                    app.set_pressure_fades(on, cx)
                },
            ))
    }

    /// One of the row's choosers, at the width the bar gives it.
    ///
    /// The wrapper is what lets the app know the list is up: the chooser keeps that to itself, and the two
    /// presses that decide it are both seen here — the click on the box, which is the chooser's own toggle,
    /// and a press anywhere else, which is what dismisses the list (see [`Self::chooser_toggled`]). Neither
    /// listener consumes the event: the box still opens and closes the list it is the box for.
    ///
    /// `id` is the wrapper's own, because a press listener is a thing an identified element has: it is what
    /// makes the click and the dismiss answerable at all.
    fn chooser(
        &self,
        id: &'static str,
        state: &Entity<SelectState<Vec<String>>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .w(px(112.0))
            .child(Select::new(state).small())
            // The box opens the list; it does not close it. `Select` attaches its own toggle *only* while the
            // list is shut (`allow_open`), so a second press on the box is not a press that closes it — and a
            // wrapper that flipped on every click would go out of step with the list the moment one landed.
            // Closing is the list's own business, and both ways it happens are read below.
            .on_click(cx.listener(|app, _: &ClickEvent, _, cx| app.chooser_toggled(true, cx)))
            // A press outside the box is what dismisses the list (`Select` closes on exactly this), so it is
            // also the answer for the cursor. Nothing is stopped here: the press belongs to whatever it landed
            // on, ink included.
            .on_mouse_down_out(cx.listener(|app, _, _, cx| app.chooser_toggled(false, cx)))
    }

    /// Keeps the four choosers showing what the note is actually written with.
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
            (
                self.frame_select.clone(),
                chosen_index(&FrameCap::ALL, self.settings.frame_cap),
            ),
            (
                self.memory_select.clone(),
                chosen_index(&MemoryMode::ALL, self.settings.memory_mode),
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

        // The zoom field is the same kind of readout, and needs the same treatment: the view changes the number under
        // it — a pinch, a fit, a note being opened — and a field left showing the number that was typed last
        // would be showing a zoom the sheet is not drawn at. Left alone while it has the keyboard, because then the
        // field is a number being *typed* and [`Self::zoom_event`] is what writes it back.
        let typing = self.zoom_input.read(cx).focus_handle(cx).is_focused(window);
        if !typing {
            self.show_zoom(window, cx);
        }
    }

    /// The desk's own row: the page in the middle, the zoom at the other edge.
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

        // Neither of the row's other two children is the counters: the status pill hangs from the bar at
        // the top of the window now (see [`Self::status_row`]), because what a document *is* and what it
        // holds are one subject and belong at one edge, which leaves the bottom edge to navigation.

        // Zoom stays when the bar is hidden, unlike the controls up there: the switch that brings the bar
        // back lives *in* the bar, so a sheet zoomed into a corner with the bar away would have no way
        // out. It is reading rather than writing, which is what the page pill is too.
        //
        // The right margin is written here rather than left to the row's own padding: an absolutely
        // positioned child is placed against its parent's padding box, not its content, so the `px`
        // above reaches the page pill and not this one. `BAR_MARGIN` is what the pill has below it —
        // the row itself sits that far from the bottom edge — so the two margins are the same.
        row = row.child(
            div()
                .absolute()
                .right(px(BAR_MARGIN))
                .bottom_0()
                .child(self.zoom_pill(cx)),
        );

        row
    }

    /// Which page is on the desk, and the way to the others.
    fn page_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (surface, hairline, foreground) =
            (theme.title_bar, theme.title_bar_border, theme.foreground);

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
        let (surface, hairline, radius) =
            (theme.title_bar, theme.title_bar_border, theme.radius_lg);

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
                .w(px(68.0))
                .child(Input::new(&self.zoom_input).small())
                .into_any_element(),
        );
        controls.push(
            icon_button("zoom-in", IconName::Plus, "Zoom in", true, cx, |app, cx| {
                app.zoom_in(cx)
            })
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
                icon_button(
                    fit.button_id(),
                    icon,
                    fit.label(),
                    true,
                    cx,
                    move |app, cx| app.fit_sheet(fit, cx),
                )
                .into_any_element(),
            );
        }

        // The lock, after a hairline of its own: it is not a way of *moving* the zoom but a statement about the three
        // that do it by gesture, so it reads as the last thing in the pill rather than as another step.
        controls.push(toolbar_divider(hairline).into_any_element());
        let locked = self.settings.zoom_locked;
        controls.push(
            tool_button(
                "zoom-lock",
                if locked {
                    IconName::Lock
                } else {
                    IconName::LockOpen
                },
                if locked {
                    "Zoom locked: a pinch and Fit leave it alone"
                } else {
                    "Lock the zoom, so a pinch or Fit cannot move it"
                },
                locked,
                cx,
                move |app, cx| app.set_zoom_locked(!locked, cx),
            )
            .into_any_element(),
        );

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
                "Delete this page (asks first)",
                true,
                cx,
                |app, cx| app.ask(Confirmation::DeletePage, cx),
            )
            .into_any_element(),
            toolbar_divider(hairline).into_any_element(),
            // Turning the page: two buttons for the page in front and two for the whole note. The icons
            // are the same arrows twice, a page's worth and a sheet's worth, because that is the whole of
            // the difference between them — and the tooltips say the chords the same commands answer to.
            icon_button(
                "page-turn-ccw",
                IconName::RotateCcw,
                keys::tooltip::TURN_PAGE_CCW.as_str(),
                true,
                cx,
                |app, cx| app.turn_page(-1, cx),
            )
            .into_any_element(),
            icon_button(
                "page-turn-cw",
                IconName::RotateCw,
                keys::tooltip::TURN_PAGE_CW.as_str(),
                true,
                cx,
                |app, cx| app.turn_page(1, cx),
            )
            .into_any_element(),
            toolbar_divider(hairline).into_any_element(),
            icon_button(
                "pages-turn-ccw",
                IconName::RotateCcwSquare,
                keys::tooltip::TURN_PAGES_CCW.as_str(),
                true,
                cx,
                |app, cx| app.turn_every_page(-1, cx),
            )
            .into_any_element(),
            icon_button(
                "pages-turn-cw",
                IconName::RotateCwSquare,
                keys::tooltip::TURN_PAGES_CW.as_str(),
                true,
                cx,
                |app, cx| app.turn_every_page(1, cx),
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
                keys::tooltip::MARK.as_str(),
                marked,
                cx,
                |app, cx| app.toggle_bookmark(cx),
            )
            .into_any_element(),
            icon_button(
                "page-marks",
                IconName::BookMarked,
                keys::tooltip::MARKED.as_str(),
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
                keys::tooltip::CONTENTS.as_str(),
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

    /// The counters, hanging under the bar at the top-left of the window.
    ///
    /// The top edge is where a document's identity lives: the bar says *which* note is open, and the pill
    /// says what that note and its page are made of. Read together they are one subject, so they sit at
    /// one edge. The bottom edge is left to navigation — the page pill and the zoom — and that split is
    /// what keeps the pill from being drawn over the corner a person writes in.
    ///
    /// Offset by the bar's *measured* bottom edge rather than a constant, because the bar's height
    /// depends on what it is carrying (a second row appears with the sheet's settings) and because the
    /// bar can be switched off, in which case the pill hangs from the window's own top margin. Both
    /// come from [`Self::bar_edge`], the one line the pen is judged by, so the pill starts where the
    /// page starts.
    fn status_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let top = self.bar_edge().max(BAR_MARGIN) + STATUS_DROP;

        div()
            .absolute()
            .top(px(top))
            .left(px(BAR_MARGIN))
            // No width of its own: the pill inside carries `STATUS_PILL_WIDTH`, and a second bound
            // here would be two answers to one question.
            .child(self.status_pill(cx))
    }

    /// The counters' pill: a readout of text, so it is given the air a row of buttons would get from
    /// their own padding rather than the tighter box the page and zoom pills wear.
    fn status_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .px(px(10.0))
            .py(px(6.0))
            .rounded(theme.radius)
            .bg(theme.title_bar.opacity(0.92))
            .text_size(px(11.0))
            .text_color(theme.muted_foreground)
            .max_w(relative(STATUS_PILL_WIDTH))
            // A layer over the desk, like the bar: the pill is read *across* a page, and a page is ink of
            // its own, so the pill has to be seen to sit above it rather than in it. `shadow_md` is what
            // this interface spends on that, and the hairline is what keeps the edge legible where the
            // shadow falls on something already dark.
            .border_1()
            .border_color(theme.title_bar_border)
            .shadow_md()
            // The line is the whole pill, so the pill is a block that holds text and nothing else: no
            // row, no trailing button, and no inner box to squeeze the text to a width of its own.
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
        //
        // What is asked for here and what is *built* here are different things on purpose: a rung costs
        // 23.7 ms on the page this was measured against, which is three and a half frames at 144 Hz, and
        // paying it inside the frame that renders is the hitch a pinch shows. The request is recorded, the
        // page keeps drawing at the detail it already has, and the pump builds the new rung between frames
        // (`housekeeping`, via `InkDocument::build_detail`).
        if self.ink.set_zoom(sheet.zoom) {
            self.detail_owed = true;
        }

        // The ruling is built inside `describe_sheet`, which owns the sheet's rectangle for the same reason.

        // The stroke under the pen is closed for the ink description, and where the sheet is: see
        // [`Self::describe_ink`], which the pen's pump calls for the same reason.

        // What the canvas is, described for its own renderer: the desk, the shadow and the sheet, then what
        // is printed on the paper, the document's page, and the ink. The drawing is timed where it happens,
        // in [`Self::draw_canvas`], which this calls last — the pen's pump draws the canvas too.
        self.describe_sheet(&sheet, background, page.as_ref());

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

        let mut screen =
            div()
                .relative()
                .size_full()
                .bg(desk)
                .text_color(foreground)
                // A trackpad's two-finger scroll pans. A pinch arrives on its own event (`on_pinch` below).
                .on_scroll(cx.listener(|app, event: &ScrollEvent, window, cx| {
                    app.on_scroll(event, window, cx)
                }))
                .on_pinch(cx.listener(|app, event: &PinchEvent, _, cx| app.on_pinch(event, cx)))
                // The note's keyboard: Esc for the home screen, the two arrows for the page, and the three
                // commands a person reaches without letting go of the pen. A listener and not a binding,
                // because a *list* of rows wants the same keys (see [`crate::home`]); it is heard because this
                // element is the sheet's, and the sheet holds the keyboard whenever it is the screen in front
                // (see [`Self::claim_sheet_keyboard`]).
                .on_key_down(cx.listener(|app, event: &KeyDownEvent, window, cx| {
                    app.note_key_down(event, window, cx)
                }))
                // A name field that has just closed leaves the focus on a handle that is no longer drawn, which
                // is what [`Self::claim_sheet_keyboard`] fixes on the next frame: the handle is tracked here so
                // the focus has somewhere in this element to land.
                .track_focus(&self.sheet_focus)
                // The canvas is not drawn here: it has a surface of its own *behind* this element, and what
                // this element paints is nothing — which is what keeps the desk, the sheet and the ink out of
                // the frame (see [`crate::ink_layer`]).
                .child(bar)
                // The counters, hanging under the bar at the top-left: what the open document is and
                // what it holds, read at the edge where the document itself is named. Drawn after the
                // bar so it is never buried by it.
                .children(
                    self.settings
                        .show_status
                        .then(|| self.status_row(cx).into_any_element()),
                )
                // The desk's own row: the page in the middle, the zoom at the edge. Last, so it
                // paints over the sheet — a page pill *under* the paper would be no pill at all.
                .child(self.bottom_row(cx));

        // The palette at the pen, over the sheet: what the hand is reaching for is drawn in front of
        // what it is writing on, and under the question below, which is about the page (see
        // [`Self::palette_overlay`]).
        if let Some(palette) = self.palette {
            screen = screen.child(self.palette_overlay(palette, cx));
        }

        // The confirmation box, over everything: a question about the page in front stands in front of it, and while
        // it is there the box is what answers (see [`Self::confirm_box`]).
        if let Some(what) = self.confirm {
            screen = screen.child(self.confirm_box(what, cx));
        }

        screen.into_any_element()
    }
}

/// One of the bar's choosers: a list of options, and the one it starts on.
fn choice(
    labels: &[&'static str],
    selected: Option<usize>,
    window: &mut Window,
    cx: &mut Context<NoteApp>,
) -> Entity<SelectState<Vec<String>>> {
    choice_of(
        labels.iter().map(|label| label.to_string()).collect(),
        selected,
        window,
        cx,
    )
}

/// A chooser over labels that are *built* rather than spelled out in the source.
///
/// The pen's chooser needs this because each of its labels carries a number that belongs to the note in hand (see
/// [`PenWeight::describe`]), so the strings are owned rather than `&'static`. Every chooser is built this way —
/// the state's item type is one thing rather than one per label flavour — so a fixed-label chooser simply hands
/// its labels through [`choice`].
fn choice_of(
    items: Vec<String>,
    selected: Option<usize>,
    window: &mut Window,
    cx: &mut Context<NoteApp>,
) -> Entity<SelectState<Vec<String>>> {
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

/// A visibility switch that also names its chord, for the one of them a key can reach.
///
/// The bar is the only switch here with a chord, so it is the only one that needs a tooltip: the others are
/// pointers-only, and a tooltip that says nothing but their own label would be noise on hover. The text comes
/// from [`keys::tooltip`], so the words and the key are written in the platform's own spelling.
fn chorded_switch(
    id: &'static str,
    label: &'static str,
    tooltip: &'static str,
    on: bool,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, bool, &mut Context<NoteApp>) + 'static,
) -> Switch {
    Switch::new(id)
        .checked(on)
        .label(label)
        .small()
        .tooltip(tooltip)
        .on_change(cx.listener(move |app, checked: &bool, _, cx| handler(app, *checked, cx)))
}

/// A box placed at one point of the palette, in logical pixels.
///
/// The point is the box's *centre*, so its own size is taken off it before it is placed. The palette's
/// controls are placed by hand rather than laid out in a row or a grid, because a ring is neither:
/// there is no arrangement of flex children that puts twelve things on a circle.
fn placed(at: [f32; 2], size: f32, child: impl IntoElement) -> Div {
    div()
        .absolute()
        .left(px(at[0] - size / 2.0))
        .top(px(at[1] - size / 2.0))
        .child(child)
}

/// The edge a colour is given where it is drawn.
///
/// A white or ivory chip on a white surface would have no edge of its own, so a light colour is given a
/// hairline; a dark one is its own edge and is given nothing. Read off the colour's own luminance rather
/// than decided per palette, so wherever a colour is drawn it is drawn the same way.
fn colour_edge(color: u32, theme: &Theme) -> Hsla {
    if relative_luminance(color) > 0.85 {
        theme.border
    } else {
        theme.transparent
    }
}

/// A colour, drawn: a filled shape of `size`, with the hairline a light colour needs to have an edge.
///
/// Shared by the bar's swatches and the palette's discs, so that one colour cannot come out two colours
/// depending on where it is drawn — and so the shape a colour has is chosen in one place: a page is a
/// square, because a page has corners, and a pen's colour is a circle.
fn colour_chip(color: u32, size: f32, edge: Hsla, circle: bool) -> Div {
    let chip = div()
        .w(px(size))
        .h(px(size))
        .bg(rgb(color))
        .border_1()
        .border_color(edge);

    if circle {
        chip.rounded_full()
    } else {
        chip.rounded(px(4.0))
    }
}

/// One colour of the palette at the pen: a disc, ringed when it is the place the keyboard is on.
///
/// Its id is its own rather than the bar's swatch id for the same colour (see [`palette_id`]), and what a
/// press reports is its *place* on the ring rather than its colour: the ring is a list, and its places
/// are what the keyboard walks and what the palette is looked up by ([`NoteApp::palette_spin`]).
fn palette_disc(
    swatch: &Swatch,
    place: usize,
    chosen: bool,
    ring: Hsla,
    geometry: PaletteGeometry,
    cx: &mut Context<NoteApp>,
) -> impl IntoElement {
    let theme = cx.theme();
    let (edge, none) = (colour_edge(swatch.color, theme), theme.transparent);

    div()
        .id(palette_id(swatch))
        // A disc of colour says nothing to a screen reader, and nothing to anyone who cannot tell
        // "ivory" from "white" by eye: the name is what both need to hear or see.
        .aria_label(swatch.name)
        .p(px(2.0))
        .border_2()
        .border_color(if chosen { ring } else { none })
        .rounded_full()
        .cursor_pointer()
        .on_click(
            cx.listener(move |app, _, _, cx| app.palette_take(PalettePart::Colour(place), cx)),
        )
        .child(colour_chip(swatch.color, geometry.disc(), edge, true))
}

/// One tool of the palette at the pen: a chip with the tool's icon in it, washed in the accent when the
/// tool is the one in hand.
///
/// Built here rather than borrowed from the bar's [`tool_button`] because the two are drawn at different
/// sizes — a chip is a circle of a fixed diameter with an icon of a fixed size inside it, and the button's
/// icon grows with its box — and because the ring places its own controls by hand anyway (see `placed`).
/// What the two *do* share is the state the palette and the bar agree on: the same accent wash for the
/// tool in hand, and a tooltip naming the tool.
fn palette_tool_chip(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    active: bool,
    geometry: PaletteGeometry,
    cx: &mut Context<NoteApp>,
    handler: impl Fn(&mut NoteApp, &mut Context<NoteApp>) + 'static,
) -> impl IntoElement {
    let theme = cx.theme();
    let (wash, accent, surface, hairline, idle) = (
        // The wash a tool in hand is drawn in, which is the bar's own answer to the same question.
        theme.accent,
        theme.accent_foreground,
        // An idle chip is cut out of the palette's own surface: the white of a button on a grey well.
        theme.popover,
        theme.border,
        // Darker than the bar's muted grey, because a chip is a smaller thing to read: the icon carries
        // the whole of what the chip says.
        theme.secondary_foreground,
    );

    let chip = div()
        .id(id)
        // An icon says nothing to a screen reader: the name is what it needs to hear ([`palette_face`]).
        .aria_label(label)
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .w(px(geometry.tool_disc()))
        .h(px(geometry.tool_disc()))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .on_click(cx.listener(move |app, _, _, cx| handler(app, cx)));

    // A tool in hand is washed in the accent, as it is on the bar; the others are chips of the palette's
    // own white with the hairline a white chip needs to have an edge of its own.
    let chip = if active {
        chip.bg(wash)
    } else {
        chip.bg(surface).border_1().border_color(hairline)
    };

    chip.child(
        Icon::new(icon)
            .with_size(px(geometry.tool_icon()))
            .text_color(if active { accent } else { idle }),
    )
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
    // The edge a light colour is given, and the ring's own absence: both read off the theme here,
    // because the listener below takes the context ([`colour_edge`] says why a colour needs either).
    let (edge, none) = (colour_edge(color, theme), theme.transparent);

    let mut button = div()
        .id(swatch.id)
        // A square of colour says nothing to a screen reader, and nothing to anyone who cannot tell
        // "ivory" from "white" by eye. The name is what both need to hear or see.
        .aria_label(swatch.name)
        .p(px(2.0))
        .border_2()
        .border_color(if in_use { ring } else { none })
        .cursor_pointer()
        .on_click(cx.listener(move |app, _, _, cx| handler(app, color, cx)));

    button = if circle {
        button.rounded_full()
    } else {
        button.rounded(px(6.0))
    };

    // The chip is the shared one: the same colour, drawn by the same rule the palette's discs are.
    button.child(colour_chip(color, 16.0, edge, circle))
}

#[cfg(test)]
mod tests;

/// The shadow a sheet casts on the desk: spread, offset beyond the spread, and colour, widest first.
const PAGE_SHADOW: [(f32, f32, u32); 3] = [
    (12.0, 4.0, 0x0000_0008),
    (7.0, 2.5, 0x0000_000C),
    (3.0, 1.0, 0x0000_0014),
];

/// The line the bar's own bottom edge is on, in logical window pixels: `0.0` when there is no bar.
///
/// The measured edge is the real line, and it is one line for everything about the pen — ink refuses a
/// `Down` above it, the ghost is not drawn above it, and the system pointer is the cursor there (see
/// [`NoteApp::bar_edge`]). A hidden bar has no edge at all, even if it was measured a moment ago: the
/// strip it covered is page again, and the switch that hid it was *inside* that strip — the one place
/// where keeping the old edge would put the system arrow under the hand that had just hidden it.
///
/// [`BAR_HEIGHT`] is only the estimate for the frame before the bar has been measured. It is not a
/// second answer: once a frame has laid the bar out, the measured edge wins, so a bar that wrapped
/// taller than the estimate hides the ghost across all of itself and a short one leaves the page under
/// it as page. A measurement of zero is no measurement — a bar that is up has some height — so it falls
/// back to the estimate too.
fn bar_line(shown: bool, measured: Option<f32>) -> f32 {
    if !shown {
        return 0.0;
    }

    measured.filter(|edge| *edge > 0.0).unwrap_or(BAR_HEIGHT)
}

/// Whether the machine's counters should be read at all.
///
/// A free function because the decision is the part worth testing, and it is the whole of what the switch
/// does: a hidden status line asks the platform nothing, so hiding the line is also how a reader stops
/// paying for the sample (see [`NoteApp::sample_vitals`]).
fn samples_vitals(status_shown: bool) -> bool {
    status_shown
}

/// Whether the reader's frame cap holds back a frame, given how long ago the last one was handed over.
///
/// `since` is `None` for a canvas that has never drawn, which is a frame that must go out: the cap paces a
/// stream, it does not delay the first one. The comparison is `<` rather than `<=` so that a cap of exactly
/// one interval's worth of time draws — a display at 60 Hz and a cap of 60 Hz have to agree rather than each
/// waiting for the other to move first.
///
/// A free function because the decision is the part worth testing: it is one comparison, and it is the whole
/// of what the setting does.
fn held_by_cap(cap: FrameCap, since: Option<std::time::Duration>) -> bool {
    let (Some(interval), Some(since)) = (cap.interval(), since) else {
        return false;
    };

    since < interval
}

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

    canvas.fill(
        Rect {
            x,
            y,
            width,
            height,
        },
        paper,
    );
}

/// What the zoom field says while it is empty, which is only until the first frame fills it.
const ZOOM_PLACEHOLDER: &str = "100%";

/// The zoom a typed number asks for, as the fraction the view takes.
///
/// A number is a **percentage** — "60" is 60%, and "150%" is the same thing with the sign spelled out — because that
/// is what the field shows and what a number read off a page means. Nothing else is a zoom: a word, a zero, a negative
/// number and an empty field are all `None`, and the caller puts the view's own number back rather than guessing at
/// what was meant.
fn parse_zoom(text: &str) -> Option<f32> {
    let number: f32 = text
        .trim()
        .strip_suffix('%')
        .unwrap_or(text)
        .trim()
        .parse()
        .ok()?;

    if !number.is_finite() || number <= 0.0 {
        return None;
    }

    Some(number / 100.0)
}

/// What the zoom field shows: the zoom as a whole percentage.
///
/// Whole, because this is a number to read and a zoom is not measured in fractions of a percent: the view's own steps
/// are a quarter of the way at a time, and the two fits land wherever the window puts them.
fn zoom_text(zoom: f32) -> String {
    format!("{:.0}%", zoom * 100.0)
}

/// Whether a reading's phase means the nib is *on* the paper rather than above it.
///
/// The one distinction the pen's gestures rest on. A digitizer reports a pen that is hovering, and a hover is
/// where the ghost cursor is drawn and nothing else: a hand resting over the screen with a key held must not
/// drag the sheet, or the sheet would follow a nib that is touching nothing.
///
/// `Down` is the first reading with the nib down and `Move` is every one after it (see [`Phase`]); `Up` and
/// `Cancel` end a reading rather than begin one, and are answered before this is asked.
fn touches_paper(phase: crate::input::Phase) -> bool {
    use crate::input::Phase;

    matches!(phase, Phase::Down | Phase::Move)
}

/// An edge swipe in flight: a two-finger gesture that began at a window's side and may turn the page.
///
/// The gesture is the unit, not the reading. A trackpad delivers a swipe as dozens of small deltas, so the
/// decision cannot be made on any one of them — a single delta is thirty pixels at most and says nothing about
/// where the hand is going. What this accumulates instead is the total travel, signed, and it answers one
/// question when enough has built up: was this a sideways swipe that began at the edge it is moving away from?
///
/// The two rules that keep it from firing by accident are the band (where the gesture *began*) and the slope
/// (that it is more sideways than vertical). A reader panning the page with a thumb near the edge begins outside
/// the band; a reader scrolling down the page from inside it is largely vertical. See [`EDGE_SWIPE_BAND`],
/// [`EDGE_SWIPE_TRAVEL`] and [`EDGE_SWIPE_SLOPE`].
///
/// **It fires once.** A swipe that keeps going — a fast flick, or a hand that carries on after the page turns —
/// is still one gesture, and turning four pages from it would be the gesture misread as four. `turned` is what
/// says the page has already been turned, and it is never cleared while the gesture lasts.
#[derive(Debug, Clone, Copy)]
struct EdgeSwipe {
    /// Which side the gesture began at.
    edge: Edge,
    /// How far the gesture has moved sideways since it began, signed: positive is rightwards.
    travel: f32,
    /// The same for the vertical axis, so a scroll can be told from a swipe. Only its magnitude is ever read.
    rise: f32,
    /// Whether this gesture has already turned a page.
    turned: bool,
}

/// Which window edge an [`EdgeSwipe`] began at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// The window's left side, where a rightward swipe is the previous page.
    Left,
    /// The window's right side, where a leftward swipe is the next page.
    Right,
}

/// What an edge gesture has come to, once it has travelled far enough to mean something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swipe {
    /// The gesture is not a page turn: too short, too vertical, or pointing back into the sheet.
    None,
    /// It is a turn, the way this names.
    Turn(Turn),
}

/// Which way an edge swipe turns the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    Previous,
    Next,
}

impl Edge {
    /// The edge a gesture began at, if `x` is inside [`EDGE_SWIPE_BAND`] of either side of a window `width`.
    ///
    /// `None` is a gesture that began over the page, which is the common case and is never a page turn: the paper
    /// fills the window, so anything further in than the band is a reader moving the page they are reading.
    fn at(x: f32, width: f32) -> Option<Self> {
        // Written as an explicit `is_nan` rather than as `!(width > 0.0)`: the negation is how a `NaN`
        // width is caught (every comparison against a `NaN` is false), but a reader has to know that to
        // see it, and clippy cannot tell this from a comparison written the wrong way round.
        if width.is_nan() || width <= 0.0 || !x.is_finite() {
            return None;
        }

        if x < EDGE_SWIPE_BAND {
            Some(Edge::Left)
        } else if x > width - EDGE_SWIPE_BAND {
            Some(Edge::Right)
        } else {
            None
        }
    }
}

impl EdgeSwipe {
    /// Begins a gesture that started at `edge`.
    fn new(edge: Edge) -> Self {
        Self {
            edge,
            travel: 0.0,
            rise: 0.0,
            turned: false,
        }
    }

    /// Adds one reading's worth of movement.
    fn moved(&mut self, delta: (f32, f32)) {
        self.travel += delta.0;
        self.rise += delta.1;
    }

    /// What the gesture has come to.
    ///
    /// A swipe turns the page once it has travelled [`EDGE_SWIPE_TRAVEL`] *away from* the edge it began at, and
    /// only if it is [`EDGE_SWIPE_SLOPE`] times more sideways than vertical. Both are read from the accumulated
    /// travel, so a gesture that drifts out and back is not a swipe, and a swipe that has already turned one page
    /// reports `None` ever after — see [`EdgeSwipe`] on why it fires once.
    fn decided(&self) -> Swipe {
        if self.turned {
            return Swipe::None;
        }

        // Away from the edge, which is the direction each side turns. A swipe that pushes *into* the page from
        // the rim is a pan, and the sign is what says so.
        let away = match self.edge {
            Edge::Left => self.travel,
            Edge::Right => -self.travel,
        };

        if away < EDGE_SWIPE_TRAVEL || away < self.rise.abs() * EDGE_SWIPE_SLOPE {
            return Swipe::None;
        }

        Swipe::Turn(match self.edge {
            Edge::Left => Turn::Previous,
            Edge::Right => Turn::Next,
        })
    }

    /// Marks the page as turned, so the rest of this gesture cannot turn another.
    fn turned(&mut self) {
        self.turned = true;
    }
}
/// One scroll reading, in the units the sheet pans in, and whether it is a gesture that may turn a
/// page.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ScrollRead {
    /// The distance to add to the sheet's pan, in logical pixels.
    distance: (f32, f32),
    /// Whether this reading belongs to a gesture that can arm and feed an [`EdgeSwipe`].
    ///
    /// A trackpad's pixel readings are such a gesture; a wheel's lines are not (see
    /// [`NoteApp::on_scroll`]).
    gesture: bool,
}

/// Reads one scroll into the distance the sheet should move, or `None` when this platform does not
/// take that kind of scroll.
///
/// GPUI reports a trackpad in pixels and a wheel in lines. Pixels are already the units the view
/// pans in; a line is turned into pixels by [`WHEEL_PAN_PER_LINE`], and only where the platform
/// answered [`crate::platform::Platform::wheel_pans`] `true`. A reading this function returns
/// `None` for is not a scroll this machine asked the app to follow.
fn scroll_of(delta: &ScrollDelta, wheel_pans: bool) -> Option<ScrollRead> {
    match delta {
        ScrollDelta::Pixels(delta) => Some(ScrollRead {
            distance: (f32::from(delta.x), f32::from(delta.y)),
            gesture: true,
        }),
        ScrollDelta::Lines(lines) if wheel_pans => Some(ScrollRead {
            distance: (lines.x * WHEEL_PAN_PER_LINE, lines.y * WHEEL_PAN_PER_LINE),
            gesture: false,
        }),
        _ => None,
    }
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
