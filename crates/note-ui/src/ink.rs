//! The ink model: readings in, strokes out.
//!
//! ## What this module owns, and what it borrows
//!
//! What a stroke *is* — its points, its widths, its colour, and the geometry derived from them —
//! is the note's, and lives in the format crate ([`note_format::ink`]); it is re-exported here so
//! that every path in the app still reads `crate::ink::Stroke`. This module owns the half that is
//! about a reader rather than a note: the tools, the eraser, the lasso, the selection, undo, and
//! the page a reader turns to.
//!
//! ## What this module owns
//!
//! A pen reading is a *point in time*; a stroke is a *line the user drew*. Turning the first
//! into the second is the application's job (the `pen-windows` crate deliberately stops at the
//! reading), and it is where the writing *feels* right or wrong:
//!
//! * **Edges decide the stroke.** `Down` begins one, `Up` ends it, and `Cancel`/`Leave` end it
//!   without adding the position they carry — that position is where the pointer was last
//!   seen, not ink the user drew.
//! * **Readings belong to a pointer.** A tablet can report a finger and a pen at once, and a
//!   reading from the wrong pointer must not extend the open stroke.
//! * **Resampling removes work, not ink.** A slow hand produces points a fraction of a pixel
//!   apart; keeping them all costs render time and changes nothing the eye can see.
//! * **Width comes from force when the digitizer reports it**, and from a constant when it
//!   cannot. `applied_pressure()` is `None` for a pen with no sensor, and drawing that as zero
//!   width is the classic "my pen looks broken" bug.
//!
//! ## Why the finished strokes are behind an `Arc`
//!
//! A frame hands the canvas layer the *list* of finished strokes rather than a copy of them, and
//! the layer reads their outlines where they are and keeps geometry of its own (see
//! [`crate::ink_layer::render`]). Cloning a `Vec<Stroke>` every frame would be O(strokes) per frame
//! at the display's rate; cloning an `Arc` is a pointer copy. The strokes are behind an `Arc` of
//! their own for the same reason one level down: a vector of pointers can be appended to,
//! undone and filtered without touching the strokes themselves.
//!
//! ## The tools
//!
//! Four, and each is a different thing to do to a page rather than a different setting of one pen:
//!
//! * **The pen** writes lines, in the colour and weight in hand (see [`crate::settings`]).
//! * **The highlighter** lays a band: one fixed width, and a colour with an alpha in it, so that what it marks stays
//!   readable through it (see [`HIGHLIGHTER_ALPHA`]). It is not a pen in disguise — it is the *same* stroke with
//!   translucent ink, which is why a highlight can be erased, dragged, undone and stored like anything else, and
//!   why nothing outside this module has to know what a highlighter is.
//! * **The eraser** removes the strokes the nib passes near, whatever they are.
//! * **The lasso** takes the ink a loop encloses into the hand, to drag or delete.
//!
//! ## What undo is
//!
//! Every change to a page is an [`Edit`] — a value that knows how to put itself back — and undo is one edit at a
//! time, per page. The model is [`crate::history`]'s, and why it is built this way is written down there; the
//! rules a hand notices are these:
//!
//! * **The stroke under the nib is not history.** It has no closed geometry and the pen is still on
//!   the paper, so undo leaves it exactly where it is: taking it away would remove a line that was
//!   never finished, and would leave the model thinking the pen was up while it is down.
//! * **Any new edit ends the redo branch.** Anything done after an undo — a stroke, an erase, a
//!   drag — makes the drawing a *new* one, so what was taken back is no longer something to put
//!   forward. This is the rule every editor has, and the reason redo can be trusted.
//! * **Everything else can be taken back**, which is what the rewrite from a list of strokes to edits
//!   bought: the eraser holds the strokes it removed, a lasso's drag holds what it moved, and *Clear*
//!   holds the whole page. Undo is not "give me the last stroke back"; it is "put back what I did".
//! * **A page keeps its history while it can still be redone**, even with nothing drawn on it, so
//!   turning the page and turning back does not quietly throw the redo away.
//! * **The history outlives the session.** A page's edits are written down beside its ink, in one
//!   transaction (see [`crate::store`]), so a note opened tomorrow can still take back what was
//!   written today — and *before* that, the deepest undo of a long session is dropped rather than
//!   kept ([`crate::history::HISTORY_EDITS`]).
//!
//! ## The lasso
//!
//! The third tool neither adds ink nor removes it: it takes ink *in hand*. A loop is swept around the
//! writing the reader wants, and what the loop encloses becomes the **selection** — which can be dragged,
//! and which a command can delete. The rules are the ones a hand expects, and the exceptions are again
//! what a reader notices:
//!
//! * **A selection belongs to its page**, like the ink does: turn away and back, and the same strokes are
//!   still in hand, because the selection is a fact about that page's writing and not about the view.
//! * **The ink does not move while it is being dragged.** A drag changes where the selected strokes are
//!   *drawn* and nothing else — one offset for the whole frame, the same idea as a page's rotation one
//!   level down — and the points are moved once, when the drag is put down. Dragging a page of
//!   handwriting is therefore as cheap as dragging one stroke, and a cancelled drag costs nothing at all.
//! * **Whole strokes, by the loop.** A stroke the loop encloses is taken as one line, and a stroke the
//!   loop merely crosses is taken if *any* of its points is inside — the prototype's trade-off, which is
//!   the one the eraser makes too (see [`InkDocument::erase_at`]). Its points are resampled about a pixel
//!   apart, so a crossing is caught; what is never produced is a fragment of a stroke, which nothing
//!   could pick up again.
//! * **Moving and deleting are edits like any other**, so undo puts them back: a drag reverses exactly, and a
//!   deleted selection comes back where it was. What a move also does is leave the selection in hand, so it can be
//!   dragged back by hand as well — and it is counted by the page, because it is the one edit a *stroke count*
//!   cannot see (see `NoteApp::persist`).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::input::{Phase, Reading};
// Strokes are independent work, so a rung rebuild spreads them over the pool (see
// `InkDocument::build_detail`). The same crate `note-format` seals chunks with.
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::history::{Edit, History};
use crate::pages::Quarters;
use crate::settings::Settings;

/// The ink itself, from the format crate — and the geometry that is arithmetic over it.
///
/// A stroke is what a *note* holds, so it is defined where the note's encoding is (see
/// [`note_format::ink`]) and re-exported here rather than being defined twice. The detail ladder
/// (`detail_zoom`, `Tip`, `densified`) is re-exported for the same reason: a page's outlines are
/// built at a zoom rung, and the ladder is the same arithmetic for the note's own readers — the app
/// model ([`InkDocument`]), the ink layer, and any other application that draws one of these notes.
pub use note_format::ink::{
    alpha_of, detail_of, detail_zoom, editable, with_alpha, InkPoint, Stroke, Tip,
};

/// The rest of the detail ladder: the pieces the tests measure the ink's geometry with, and the
/// names this module's own documentation refers to.
///
/// Not called by the app's paths — a stroke is closed through [`Stroke::close`], and a page is
/// rebuilt from [`detail_zoom`] — but a test that asks *what the outline is* has to be able to
/// build one, and it is the same arithmetic the app is drawing.
#[allow(unused_imports)]
pub use note_format::ink::{densified, outline_of, ribbon_outline, steadied, steps_for, FACET};

/// Where the pen is, in the sheet's own coordinates.
///
/// Three coordinate systems meet here, and this is the one place they are brought together:
/// `pen-windows` reports **physical** client pixels, GPUI lays out and paints in **logical** ones,
/// and the sheet is drawn inside the window at a zoom and an offset. Storing ink in window
/// coordinates instead would tie the pen's line to the window rather than to the paper — zoom, and
/// the note slides off the page it was written on.
///
/// The zoom is *not* applied to a point's width: widths stay in paper units, so a stroke keeps its
/// weight relative to the page it was written on, and zooming in enlarges it along with everything
/// else printed there.
///
/// The **paper's own size** is carried too, because "on the paper" has to be answerable here rather
/// than guessed at downstream: the canvas is the whole window, with the sheet a rectangle inside it,
/// so the desk beside the page is paintable — and a reading that lands on the desk is not ink.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InkTransform {
    /// Physical pixels to logical pixels: the window's DPI scale factor.
    pub scale: f32,
    /// How large the sheet is drawn, relative to its own size.
    pub zoom: f32,
    /// The sheet's top-left corner, in window logical pixels.
    pub origin: (f32, f32),
    /// The paper's own size, in sheet units. `(0.0, 0.0)` means "no paper": see [`Self::has_paper`].
    pub paper: (f32, f32),
    /// Where the interface's top bar ends, in window logical pixels: everything above it is *in
    /// front of* the sheet, and a reading that lands there is a press on a control rather than ink.
    /// `0.0` when nothing is in front of the page — no bar, or one that has not been laid out yet.
    ///
    /// The bar floats over the canvas rather than taking part in the layout, which is what keeps a
    /// pen's coordinates free of offsets (see [`crate::app`]); this is the price of that. It is
    /// carried with the rest of the window's geometry because it answers, with the paper's edges,
    /// the one question this model asks of every reading: is it *on the page the user is writing
    /// on* — and the top of the page behind a bar is not somewhere a nib can write.
    pub bar: f32,
    /// How far the page has been turned, in quarter turns clockwise.
    ///
    /// A reading is turned back by it — see [`paper_of_drawn`] — so ink written on a page the reader
    /// has turned is stored the way it will be read: turn the page back and the writing turns with it,
    /// because both are in the page's own coordinates and neither was ever anything else.
    pub rotation: Quarters,
}

/// Where the paper's own point `paper` is drawn inside the sheet's rectangle, in the sheet's units.
///
/// The whole of what a page's rotation is: a rectangle of paper `size` turned `turns` quarter turns
/// *clockwise* — its top-left corner goes to the top-right, its own rightwards direction points down the
/// screen — and this is where each of its points lands. A page turned twice is not the same as a page
/// turned none: `(pw - x, ph - y)` is a different mapping from `(x, y)`, and the difference is exactly
/// what a reader sees.
///
/// The layer draws the ink through the same mapping — its transform is *built* from this function (see
/// [`crate::ink_layer::render::ink_matrix`]) — so there is one convention and not two, and no way for the
/// layer and the ink model to disagree about which way is clockwise.
pub fn drawn_of_paper(
    (x, y): (f32, f32),
    (width, height): (f32, f32),
    turns: Quarters,
) -> (f32, f32) {
    match turns % 4 {
        1 => (height - y, x),
        2 => (width - x, height - y),
        3 => (y, width - x),
        _ => (x, y),
    }
}

/// Where a point of the sheet's rectangle came from on the paper: the inverse of [`drawn_of_paper`].
///
/// What a pen reading is put through, and the reason a page turned is still written on in the page's own
/// coordinates.
///
/// The four are written out rather than taken from [`drawn_of_paper`], because a quarter turn is not a
/// rotation matrix here: it *also* swaps the rectangle's sides, so the three-quarter turn of the same
/// function is not this one — for a 600×800 page, `paper_of_drawn` of `(800, 0)` is `(0, 0)`, while the
/// three-quarter turn of the forward mapping is `(0, -200)`. `the_two_mappings_are_inverses` is what
/// holds the two tables together, and it is the test to keep when either changes.
pub fn paper_of_drawn(
    (x, y): (f32, f32),
    (width, height): (f32, f32),
    turns: Quarters,
) -> (f32, f32) {
    match turns % 4 {
        1 => (y, height - x),
        2 => (width - x, height - y),
        3 => (width - y, x),
        _ => (x, y),
    }
}

impl Default for InkTransform {
    fn default() -> Self {
        InkTransform::identity()
    }
}

impl InkTransform {
    /// The reading's own coordinates: what a test with no window wants.
    ///
    /// It has no paper — see [`Self::has_paper`] — so every reading is on the sheet by construction.
    /// A test that wants the paper's edges sets them itself.
    pub const fn identity() -> Self {
        InkTransform {
            scale: 1.0,
            zoom: 1.0,
            origin: (0.0, 0.0),
            paper: (0.0, 0.0),
            bar: 0.0,
            rotation: 0,
        }
    }

    /// Where a reading in physical client pixels lands on the sheet.
    pub fn sheet_point(&self, pixel: (f32, f32)) -> (f32, f32) {
        // A window that reports a zero scale or a zero zoom would divide every point into a
        // corner, and a NaN would poison every rectangle derived from it. Both mean "no
        // transform", which draws the ink where the pen is.
        let scale = finite_or_one(self.scale);
        let zoom = finite_or_one(self.zoom);

        let drawn = (
            (pixel.0 / scale - self.origin.0) / zoom,
            (pixel.1 / scale - self.origin.1) / zoom,
        );

        // Turned back into the page's own coordinates: with no paper there is no page to be turned,
        // and a rotation of a shape with no size would be a mapping of nothing into itself.
        if !self.has_paper() {
            return drawn;
        }

        paper_of_drawn(drawn, self.paper, self.rotation)
    }

    /// Whether a point in the sheet's own coordinates is on the paper.
    ///
    /// A point that is nowhere — a NaN — is not on it either.
    pub fn on_paper(&self, (x, y): (f32, f32)) -> bool {
        if !self.has_paper() {
            return true;
        }

        (0.0..=self.paper.0).contains(&x) && (0.0..=self.paper.1).contains(&y)
    }

    /// The nearest point *on* the paper to a point that is off it.
    ///
    /// What a line that runs off the page gets: pulled back to the border rather than dropped, which
    /// is what clipping to a page looks like — the ink stops at the edge and runs along it, rather
    /// than jumping from where the pen left the paper to wherever it came back.
    pub fn onto_paper(&self, (x, y): (f32, f32)) -> (f32, f32) {
        if !self.has_paper() {
            return (x, y);
        }

        (x.clamp(0.0, self.paper.0), y.clamp(0.0, self.paper.1))
    }

    /// Whether a reading, at a vertical position in **physical client** pixels, landed on the bar.
    ///
    /// The bar's line is the *window's*, not the sheet's: a page panned up out from under the bar is
    /// still covered where the two overlap, so this is asked of the reading's own pixels — which is
    /// also why the DPI scale divides here, exactly as [`Self::sheet_point`] divides it.
    ///
    /// A bar of no height refuses nothing, and a reading exactly on the line is the bar's: the line
    /// is the bar's last pixel, and one pixel of ink behind a button is a dot on the page that the
    /// user never saw themselves write.
    pub fn on_bar(&self, pixel_y: f32) -> bool {
        self.bar > 0.0 && pixel_y / finite_or_one(self.scale) <= self.bar
    }

    /// Whether there is a paper to be off.
    ///
    /// A sheet with no size is the transform for a test with no window (see [`Self::identity`]): it
    /// has no edges, so nothing is refused and nothing is pulled back.
    fn has_paper(&self) -> bool {
        self.paper.0 > 0.0 && self.paper.1 > 0.0
    }
}

/// `value` when it is a usable divisor, and `1.0` when it is not.
fn finite_or_one(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0
    }
}

/// What a stroke does to the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    /// Draw ink.
    Pen,
    /// Lay down a wide, translucent band: what a highlighter is, and what it is *only* by being a stroke with a
    /// see-through colour (see [`Stroke::color`] and [`alpha_of`]).
    Highlighter,
    /// Remove the strokes the nib passes near.
    Eraser,
    /// Take the ink a loop encloses into the hand, to drag or delete.
    Lasso,
}

/// How wide the highlighter's nib is, in logical pixels.
///
/// Not a setting, and deliberately: a marker has one tip, and everything a highlighter is for comes from the two
/// numbers being *fixed* — a band wide enough to sit under a line of writing, and a colour light enough to read
/// through. The pen's widths are the reader's because a hand varies; this one is the tool's.
pub const HIGHLIGHTER_WIDTH: f32 = 18.0;

/// How much of the page a highlighter's stroke lets through: the alpha its colour is stamped with.
///
/// Where the number comes from: high enough that the band is unmistakable over the writing it marks, and low
/// enough that the writing — and the rules of the paper under it — stay readable through it. At this value a
/// yellow band over black ink reads as a highlighter and not as a smear, and two overlapping strokes show as one
/// darker band rather than as two.
pub const HIGHLIGHTER_ALPHA: f32 = 0.35;

/// Counters for the status bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InkStats {
    /// Readings offered to the model.
    pub readings: u64,
    /// Points that became ink.
    pub kept_points: u64,
    /// Points dropped because they were too close to the previous kept point.
    pub resampled: u64,
    /// Readings that were not on the paper: a nib that came down beside the sheet, or a line pulled
    /// back to its edge. Zero for every drawing that stayed on the page.
    pub off_paper: u64,
    /// Strokes the eraser removed.
    pub erased_strokes: u64,
}

impl InkStats {
    /// The fraction of readings the resampler dropped.
    pub fn resample_ratio(&self) -> f32 {
        if self.readings == 0 {
            0.0
        } else {
            self.resampled as f32 / self.readings as f32
        }
    }
}

/// How thick the loop a lasso is drawing is drawn, in logical pixels.
///
/// Thin, because it is drawn *over* the ink it is being swept around: a fat ring hides the very writing
/// the reader is choosing between. Not a setting — it is the app's mark rather than the reader's ink, and
/// a person who wanted a different one would have nothing to point at.
const LASSO_WIDTH: f32 = 1.5;

/// How near the selected ink a reading has to be to take hold of it, in logical pixels.
///
/// About a nib's width. The reader presses *on* the writing they mean to move, and a line of handwriting
/// is a fraction of a millimetre wide, so a bare hit-test would make taking hold of a word a matter of
/// aim — while a radius much wider than this would reach the next line of a page written small.
const GRAB_RADIUS: f32 = 10.0;

/// What the lasso's nib is doing.
///
/// One field with two cases rather than two flags, because the gestures are exclusive: a reading that is
/// sweeping a loop cannot be dragging a selection, and the state that says which is also the state the
/// reading is applied to.
#[derive(Debug)]
enum Lasso {
    /// A loop being swept around the ink to take.
    Sweeping(Stroke),
    /// The selection being dragged: where the reading that took hold of it went down, and how far the
    /// hand has taken it since.
    ///
    /// An offset and not a moved stroke: the ink is not touched until the drag is put down (see the module
    /// docs), so this is the whole of what a drag changes.
    Dragging { from: (f32, f32), by: (f32, f32) },
}

/// The page's ink: the strokes that are finished, and the one being drawn.
///
/// ## One consumer, one open stroke
///
/// `pen-windows` hands readings over in the order the digitizer produced them, and this type
/// walks them in that order. The pointer id on each reading is what decides whether it may
/// extend the open stroke: a second stylus, a finger, or a pen that came back after losing its
/// lift must not be glued onto the line the first pen is drawing.
#[derive(Debug)]
pub struct InkDocument {
    /// The strokes the pen has finished, behind an `Arc` so a frame can snapshot it cheaply.
    ///
    /// The strokes are behind an `Arc` of their own, so that the vector can be *changed* without
    /// copying the ink: appending a stroke, undoing one, or erasing one rebuilds a vector of
    /// pointers rather than a vector of strokes. With the strokes inline, ending a stroke on a page
    /// that already held a thousand of them deep-copied all thousand — and the eraser, which
    /// touches this per reading, copied the page several hundred times a second.
    finished: Arc<Vec<Arc<Stroke>>>,
    /// The edits this page's ink has been through, and the ones taken back.
    ///
    /// Replaced a list of strokes that undo popped and redo pushed back: a list can only be *shortened*, which
    /// is why the eraser, a lasso's drag, and a cleared page could not be undone (see [`crate::history`]).
    history: History,
    /// The stroke currently being drawn, if a pen nib is down.
    open: Option<Stroke>,
    /// Counts the strokes this page has opened, so a layer can tell a longer stroke from a new one.
    ///
    /// The open stroke's points grow while the pen is down; a layer that appends to the geometry it
    /// already built needs to know when the stroke it is looking at is a *different* one. The id
    /// changes when a new stroke begins and at no other time.
    open_id: u64,
    /// The firmest press this open stroke has been laid at, in `0.0..=1.0`.
    ///
    /// **A stroke's ink is decided by this, not by the reading that began it.** A nib arriving on the paper is
    /// barely pressing — that is what arriving is — so the first reading of every stroke is one of its lightest.
    /// Deciding the alpha there made every line pale no matter how hard the hand pressed afterwards, which is
    /// exactly backwards: a reader pressing down had done everything right and the ink did not answer.
    ///
    /// It only ever rises. Taking the peak rather than the latest reading means a stroke does not flicker paler
    /// when the hand eases between letters, and that ink already laid is never taken *away* — the line can only
    /// darken into what the hand has already shown it means. The width still follows every reading, because a
    /// width is a shape and this is a colour.
    ///
    /// A stroke that never reported pressure at all — a pen with no sensor, a lasso's loop — keeps a reading of
    /// `None` here, which [`Settings::alpha_for_pressure`] answers with the weight's own alpha. That is the
    /// difference between "no hand to answer" and "a hand pressing nothing", and it must not be confused: reading
    /// a sensorless pen as zero would make every line it drew pale.
    peak_pressure: Option<f32>,
    /// The pointer that owns the turn: the pen or eraser that is currently down.
    active_pointer: Option<u32>,
    /// Which tool [`Self::active_pointer`] is using.
    active_tool: Tool,
    /// The tool the toolbar has selected: what an ordinary nib lays.
    ///
    /// The **reader's**, and not the page's — like the zoom, and unlike the ink, the selection and the history, which
    /// are what the page was *written* with. The home of the fact while a note is open is [`Notes::mode`], which hands
    /// this document the reader's answer on every page turn ([`Notes::show`]); it is read from here because this is
    /// where the pen's path needs it (see [`Self::consume`]), and written from there so that the two cannot disagree.
    ///
    /// This is the fallback when the pen reports nothing about itself: a pen whose eraser end
    /// is toward the screen always erases, and this decides what an ordinary nib does.
    mode: Tool,
    /// The last reading consumed, for the time step a smoother needs.
    last_sample: Option<Reading>,
    /// Where the eraser last removed ink, so a slow drag is not rescanned per reading.
    last_erase: Option<(f32, f32)>,
    /// The detail the finished strokes' outlines were built for, if they have been built.
    ///
    /// A stroke's outline depends on the zoom it is drawn at — not its shape, but how finely the
    /// curves in it are cut (see [`FACET`]) — and a page of ink is far too much to re-derive per
    /// frame. So it is rebuilt when the *detail* moves rather than when the zoom does: the zoom is
    /// rounded up to a rung first, and a frame whose zoom falls in the rung the page is already drawn
    /// for pays nothing at all. `None` until something has asked (see [`Self::set_zoom`]).
    detailed_at: Option<f32>,
    /// The detail a frame has asked for, which `detailed_at` becomes once the build is given time.
    ///
    /// Separate from `detailed_at` because the two are different questions: a page drawn for one rung
    /// while it has been asked for another is the *whole point* of the split — the drawing is what the
    /// reader sees now, and the build is what the pump owes it (see [`Self::request_detail`]).
    wanted_at: Option<f32>,
    /// What the model has done since the app started.
    stats: InkStats,
    /// Which strokes are in hand, one flag per stroke in [`Self::finished`], in the same order.
    ///
    /// Behind an `Arc` for the same reason the strokes are: a frame is handed the flags rather than a copy
    /// of them, and the layer draws the ink in hand in the selection's colour (see [`crate::ink_layer`]).
    /// A *mask* rather than a list of indices, because an index that still points at *something* after the
    /// stroke it named has been removed is how the wrong line gets deleted — and every edit that renumbers
    /// the strokes would have to remember to renumber the selection with them.
    selected: Arc<Vec<bool>>,
    /// What the lasso's nib is doing, if it is down.
    lasso: Option<Lasso>,
    /// How many times this page's ink has been *shifted*: moved without gaining or losing a stroke.
    ///
    /// The one edit the note cannot see from its own bookkeeping. A write is an append while a page only
    /// gains strokes and a rewrite when it loses them (see `NoteApp::persist`), and a move does neither —
    /// the count is exactly what it was, and every stroke it names has changed. So the page keeps this
    /// count and the app remembers the number it last wrote, which is what makes a dragged selection reach
    /// the file at all.
    shifted: u64,
}

impl Default for InkDocument {
    fn default() -> Self {
        InkDocument {
            finished: Arc::new(Vec::new()),
            history: History::new(),
            open: None,
            open_id: 0,
            peak_pressure: None,
            active_pointer: None,
            active_tool: Tool::Pen,
            mode: Tool::Pen,
            last_sample: None,
            last_erase: None,
            detailed_at: None,
            wanted_at: None,
            stats: InkStats::default(),
            selected: Arc::new(Vec::new()),
            lasso: None,
            shifted: 0,
        }
    }
}

impl InkDocument {
    /// The colour a pen stroke is laid in, for a hand pressing `pressure`.
    ///
    /// The one place the pen's ink is decided, so that the reading that begins a stroke and the ones that raise
    /// its press cannot disagree about it. An opaque result is the plain `0xRRGGBB` with no top byte written on
    /// it — see [`alpha_of`] for why that is the encoding's way of saying "opaque" — and a faded one carries its
    /// alpha in the top byte, the way a highlighter's does.
    fn pen_colour(settings: &Settings, pressure: Option<f32>) -> u32 {
        let alpha = settings.alpha_for_pressure(pressure);

        if alpha >= 1.0 {
            settings.ink_color
        } else {
            with_alpha(settings.ink_color, alpha)
        }
    }

    /// An empty page.
    ///
    /// Named rather than derived because reading a page that is not in the note yet is a *blank*
    /// page, and saying `default()` at that call site says nothing about why.
    pub fn blank() -> Self {
        InkDocument::default()
    }

    /// The finished strokes, cheaply shareable with a frame.
    pub fn finished(&self) -> &Arc<Vec<Arc<Stroke>>> {
        &self.finished
    }

    /// The stroke being drawn right now, if any.
    pub fn open(&self) -> Option<&Stroke> {
        self.open.as_ref()
    }

    /// The identity of the stroke being drawn: it changes when a new stroke begins, and not as the
    /// stroke grows. A layer uses it to append to the open stroke's geometry instead of rebuilding it.
    pub fn open_id(&self) -> u64 {
        self.open_id
    }

    /// Selects the tool an ordinary nib uses.
    ///
    /// The document's own copy of [`Notes::mode`], and a real setter for the tests that drive a single page:
    /// [`Notes::set_mode`] writes both, and that is the one the app uses.
    ///
    /// The pen's own state still wins: a pen whose eraser end is toward the screen erases even
    /// while the toolbar says `Pen`, because that is what the user is physically doing.
    pub fn set_mode(&mut self, mode: Tool) {
        self.mode = mode;
    }

    /// The counters behind the status bar.
    pub fn stats(&self) -> InkStats {
        self.stats
    }

    /// Whether the page has no ink at all.
    pub fn is_blank(&self) -> bool {
        self.finished.is_empty() && self.open.is_none()
    }

    /// How many strokes the page holds, finished or open.
    pub fn stroke_count(&self) -> usize {
        self.finished.len() + usize::from(self.open.is_some())
    }

    /// Takes the most recent edit back: one command, whatever kind of edit it was.
    ///
    /// Everything a command does to a page is an [`Edit`], so this is the whole of undo and there is nothing
    /// here that knows what it is taking back: a stroke written, ink erased, a selection dragged, a page cleared
    /// — all of them reverse the same way (see [`crate::history`]).
    ///
    /// The stroke the pen is still drawing is deliberately **not** touched: it is not history yet — it has no
    /// closed geometry and it is not what a save would write — and the pen is still on the paper, so taking it
    /// away would both lose a line the user meant to draw and leave the model believing the pen was lifted. It is
    /// finished, or lifted, on its own.
    ///
    /// Returns whether anything was taken back. A caller uses that to decide whether to repaint, and the
    /// interface uses [`Self::can_undo`] to say so *before* it is asked.
    pub fn undo(&mut self) -> bool {
        let mut page = editable(
            &mut self.finished,
            &mut self.selected,
            &mut self.shifted,
            self.detailed_at,
        );

        if !self.history.undo(&mut page) {
            return false;
        }

        self.forget_erase();
        true
    }

    /// Puts the most recently taken-back edit forward again.
    ///
    /// Nothing is *copied* by undoing and nothing is invented by redoing: the ink comes back exactly as it was,
    /// because an edit holds the strokes themselves (see [`Edit`]). A redo branch is ended by any new edit, so
    /// this can never put ink back into a drawing it does not belong to.
    pub fn redo(&mut self) -> bool {
        let mut page = editable(
            &mut self.finished,
            &mut self.selected,
            &mut self.shifted,
            self.detailed_at,
        );

        if !self.history.redo(&mut page) {
            return false;
        }

        self.forget_erase();
        true
    }

    /// Whether [`Self::undo`] would do anything.
    ///
    /// The interface asks this rather than pressing the button to find out: a command that is offered and then
    /// does nothing is indistinguishable from one that is broken. A page read out of the note answers with the
    /// history the note holds, so a note opened after a night away still has a way back (see [`crate::store`]).
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Whether [`Self::redo`] would do anything.
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// How many edits this page could take back, and how many it could put forward again.
    ///
    /// What the status line says (`history 12 back, 3 forward` — see `NoteApp::compose_status`), and the shape of a
    /// history a session can see: a count out of *memory*, which the note's own log may reach further back than.
    pub fn history_depth(&self) -> (usize, usize) {
        (self.history.depth(), self.history.forward())
    }

    /// The page's history, mutably: the write path takes its news, and a *deeper* edit comes out of the note.
    pub fn history_mut(&mut self) -> &mut History {
        &mut self.history
    }

    /// Puts a history back on a page read out of the note.
    ///
    /// The history is the note's, not the app's: a page opened on Monday arrives with the edits it went through
    /// last week, which is the whole of what makes undo survive a restart.
    pub fn set_history(&mut self, history: History) {
        self.history = history;
    }

    /// What to tell the note about this page's history, for a write that is about to go out.
    pub fn history_update(&mut self, count: usize) -> crate::history::HistoryUpdate {
        self.history.take_update(count)
    }

    /// How many of this page's applied edits the note already holds (see [`History::in_note`]).
    pub fn history_in_note(&self) -> usize {
        self.history.in_note()
    }

    /// Removes every stroke: an edit like any other, so undo puts the page back.
    ///
    /// This used to end both directions — "the one command that says *none of this page*" — because a history
    /// that could only shorten had no way to hold a page's worth of ink. An [`Edit::Erased`] holds it, so a
    /// cleared page is one command away from being back, and the edits *before* the clear are still behind it.
    pub fn clear(&mut self) {
        let removed: Vec<(usize, Arc<Stroke>)> = self
            .finished
            .iter()
            .enumerate()
            .map(|(index, stroke)| (index, Arc::clone(stroke)))
            .collect();

        if removed.is_empty() {
            return;
        }

        self.record(Edit::Erased { removed });
        self.open = None;
        self.active_pointer = None;
        self.last_erase = None;
        // A loop being swept is not ink, and a cleared page is not a page to be selecting on.
        self.lasso = None;
    }

    /// Records an edit the page has just been given: what every command ends in.
    ///
    /// The edit is *applied* here and then filed, in that order and through one piece of code: a command that
    /// changes the page runs the same [`Edit::apply`] a redo runs, so the two cannot come to disagree about what
    /// an edit does. An edit that changes nothing is not history.
    fn record(&mut self, edit: Edit) -> bool {
        let applied = {
            let mut page = editable(
                &mut self.finished,
                &mut self.selected,
                &mut self.shifted,
                self.detailed_at,
            );

            edit.apply(&mut page)
        };

        if applied {
            self.history.record(edit);
        }

        applied
    }

    /// Forgets where the eraser last removed ink, because the ink under it has just changed.
    ///
    /// The eraser's cheap path skips a rescan when the nib has barely moved; that path is only sound while the
    /// strokes under the nib are the ones it scanned, and an undo or a redo changes them.
    fn forget_erase(&mut self) {
        self.last_erase = None;
    }

    /// A page holding strokes that came from somewhere else — a saved note, or the clipboard of a
    /// future version — with the caches [`Stroke::close`] computes rebuilt.
    ///
    /// Rebuilding is not optional: a stroke's bounds and outline are `#[serde(skip)]`, because they
    /// are derivable from its points, and they are what the eraser hit-tests against and what a
    /// frame culls by. A page loaded without them would draw, and would refuse to be erased.
    ///
    /// The **selection mask** is rebuilt with them, one flag per stroke, all of them clear. It is a *view* of the
    /// ink — which strokes a lasso has in hand — and nothing about it is stored, so a page read out of the note has
    /// nothing selected. Leaving it as an empty vector is what made the first edit on a freshly opened page insert
    /// flags past the end of a list that had a stroke for every one of them (see [`crate::history`], whose `insert`
    /// is where that was fatal).
    pub fn from_strokes(strokes: Vec<Stroke>) -> Self {
        let finished = strokes
            .into_iter()
            .map(|mut stroke| {
                stroke.close();
                Arc::new(stroke)
            })
            .collect::<Vec<_>>();

        InkDocument {
            selected: Arc::new(vec![false; finished.len()]),
            finished: Arc::new(finished),
            // Built at 1:1, which is what [`Stroke::close`] details a stroke for. A sheet drawn larger
            // refines them, and the frame that draws it is what knows how large that is: see
            // [`Self::set_zoom`].
            detailed_at: Some(detail_zoom(1.0)),
            wanted_at: Some(detail_zoom(1.0)),
            ..InkDocument::default()
        }
    }

    /// The detail this page is drawn for, and the detail it has been asked for.
    ///
    /// The pair is what tells "drawn at the rung the zoom wants" from "drawn at the rung it had while
    /// the pump is told what to build" — see [`Self::request_detail`].
    ///
    /// Read by this module's tests, which is why the library build sees it as unused.
    #[allow(dead_code)]
    pub fn detail(&self) -> (Option<f32>, Option<f32>) {
        (self.detailed_at, self.wanted_at)
    }

    /// Asks for the derived geometry to be rebuilt for a sheet drawn at `zoom`, returning whether that
    /// is a rebuild the page does not already have.
    ///
    /// The detail a page is built for is [`detail_zoom`]'s, from the rung the zoom falls in.
    ///
    /// A stroke's outline depends on the zoom it is being drawn at: not its *shape*, which depends on
    /// its points alone, but how finely the curves in it are cut (see [`FACET`]). A page of ink is far
    /// too much to re-derive per frame — and a zoom *is* per frame, while a pinch is on — so what is
    /// tracked is the detail rather than the zoom: it moves in rungs, and a gesture across the whole
    /// range asks for a handful of rebuilds.
    ///
    /// **Nothing is rebuilt here** — that is [`Self::build_detail`], and the split is what keeps a
    /// pinch smooth. Crossing a rung costs 23.7 ms on a page of 8,182 strokes (measured, `cargo test
    /// --release -- --nocapture measures_the_rung_costs`), which is three and a half frames at 144 Hz:
    /// done here, in the frame that renders, it is a hitch every rung. Done after, the page keeps the
    /// rung it has until the pump can afford the new one, and the only thing that changes in between
    /// is how finely a curve is cut — under a quarter of a pixel, by [`FACET`]'s arithmetic.
    ///
    /// What it does not touch is the ink. The points, the colours, and what the eraser hit-tests are
    /// exactly what was written; the outline and the bounds are derived from them.
    ///
    /// The stroke under the nib is not this page's business: a frame closes the stroke it is drawing
    /// itself, at the zoom it is drawing (see [`Stroke::close_live`]).
    pub fn request_detail(&mut self, zoom: f32) -> bool {
        let detail = detail_zoom(zoom);

        if self.wanted_at == Some(detail) {
            return false;
        }

        self.wanted_at = Some(detail);
        self.detailed_at != Some(detail)
    }

    /// Rebuilds the derived geometry for the detail [`Self::request_detail`] asked for, returning
    /// whether anything was rebuilt.
    ///
    /// The expensive half, and deliberately not called from a frame: its caller is the housekeeping
    /// pump, which spends a window rather than a frame budget.
    ///
    /// ## Why it is spread across the cores
    ///
    /// Every stroke's outline is derived from that stroke alone — [`Stroke::freeze`] reads the points,
    /// writes the bounds and the outline, and looks at nothing else — so the strokes are independent
    /// work and the loop over them is the one place in this crate where the machine's other cores can
    /// be put to use. Measured on the same page of 8,182 strokes: 23.6 ms in a `for` loop, **5.0 ms**
    /// over the pool on a twelve-core M6 (`cargo test --release -- --nocapture
    /// measures_the_rung_costs`). It is not 12× because the work per stroke is small and irregular:
    /// the pool's own scheduling, and the `Arc::make_mut` copy a shared stroke costs, are a real share
    /// of a stroke that takes three microseconds. What the 4.7× buys is that the rebuild is now well
    /// under a frame at 144 Hz rather than three and a half of them, which matters because the pump
    /// that calls this still shares the main thread with the frame loop.
    ///
    /// The strokes are `Arc<Stroke>` because frames share them, and `Arc::make_mut` is what lets a
    /// rebuild happen in place when this page is the only holder. Under a parallel iteration each
    /// element is `&mut Arc<Stroke>`, so the same call works — it copies the stroke exactly where the
    /// serial loop copied it and nowhere else, so a page whose strokes a canvas is still drawing is
    /// copied once either way. What parallelism changes is only which core does the deriving.
    ///
    /// The order of `finished` is untouched: strokes are visited in place, so a page's ink stays in the
    /// order it was written, which is the order the canvas draws it in.
    pub fn build_detail(&mut self) -> bool {
        let Some(detail) = self.wanted_at else {
            return false;
        };

        if self.detailed_at == Some(detail) {
            return false;
        }

        Arc::make_mut(&mut self.finished)
            .par_iter_mut()
            .for_each(|stroke| {
                Arc::make_mut(stroke).freeze(detail, Tip::Curved);
            });

        self.detailed_at = Some(detail);
        true
    }

    /// Rebuilds for `zoom` at once, which is what a page read or turned to does before anything draws
    /// it: there is no pinch to keep smooth, and a page nobody has seen yet has no earlier detail worth
    /// keeping.
    pub fn set_zoom(&mut self, zoom: f32) -> bool {
        self.request_detail(zoom);
        self.build_detail()
    }

    /// Feeds a batch of pen readings to the model.
    ///
    /// `transform` is how a physical reading becomes a place on the sheet: the window's DPI scale,
    /// the sheet's zoom, where the sheet is drawn, and how large the paper is. All of them are
    /// applied here, once, so that nothing downstream has to know about any of them — the ink, the
    /// eraser and the geometry all work in the sheet's own coordinates, and everything they produce
    /// is *on the paper*, because a reading that is not on it is not ink. Where the bar is counts as
    /// part of that same question, one layer nearer the eye: a reading that lands on the bar is a
    /// press on a control (see [`InkTransform::on_bar`]).
    ///
    /// Returns whether anything changed, which is what the caller uses to decide whether a
    /// repaint is worth scheduling.
    pub fn consume(
        &mut self,
        samples: &[Reading],
        transform: &InkTransform,
        settings: &Settings,
    ) -> bool {
        let mut changed = false;

        for sample in samples {
            self.stats.readings += 1;

            // Where the reading lands on the sheet, and whether that is on the *paper* inside it.
            // The canvas is the whole window, so a reading beside the page lands on the desk — and
            // before this was asked, a nib that came down there drew on the desk and *saved* it:
            // ink at coordinates the page has no room for and no reader would ever see.
            let mapped = transform.sheet_point((sample.pixel.x, sample.pixel.y));
            let on_paper = transform.on_paper(mapped);
            // A line that leaves the paper is pulled back to its edge rather than dropped: that is
            // the part of the line the page can hold, and stopping at the last point *on* it would
            // leave the stroke to jump across the page when the pen came back.
            let (x, y) = transform.onto_paper(mapped);

            match sample.phase {
                Phase::Down => {
                    // A `Down` closes whatever the previous pointer left open: the ink the user
                    // drew is kept, and an `Up` that never arrived is no reason to lose it. A lasso's
                    // gesture ends with it, for the same reason: a reading that never came is no reason to
                    // keep a half-swept loop — or a half-moved selection — hanging on the page.
                    self.finish_open();
                    self.lasso = None;

                    if transform.on_bar(sample.pixel.y) {
                        // A nib that comes down on the bar opens nothing either: the bar is in
                        // front of the page and its buttons are pressed *with the pen*, so the
                        // reading is a press on a control and not a line. A tap that opened a
                        // stroke here would leave a dot on the page behind the bar — ink the user
                        // never saw themselves write, saved with the note and hidden there until
                        // the note was opened with the bar switched off. Nothing opens, so the
                        // readings that follow the `Down` have no stroke to extend: a tap on the
                        // bar leaves the page exactly as it was.
                        //
                        // Only the `Down` is refused. A line that began on the page and ran up
                        // behind the bar keeps the ink it has, for the reason the paper's own edge
                        // is a clamp rather than a cut: the user cannot see the line a cut would
                        // be made on.
                    } else if !on_paper {
                        // Clipping cannot help a nib that comes down on the desk: there is no line
                        // to clip yet, and starting one would put a dot on the nearest edge of the
                        // paper — ink at a place nobody wrote.
                        self.stats.off_paper += 1;
                        changed = true;
                    } else {
                        // The eraser end of a stylus always erases, whatever the bar has selected;
                        // everything else is the bar's answer (see [`Tool`]).
                        let tool = if sample.eraser || sample.inverted {
                            Tool::Eraser
                        } else {
                            self.mode
                        };
                        self.active_pointer = Some(sample.id);
                        self.active_tool = tool;

                        match tool {
                            Tool::Eraser => {
                                self.last_erase = None;
                                self.erase_at(x, y, settings);
                            }
                            Tool::Pen => {
                                let pressure = sample.applied_pressure();
                                let width = settings.width_for_pressure(pressure);
                                // The pen in hand is stamped into the stroke: it is chosen at the
                                // moment the nib goes down, and it stays with that line for good.
                                //
                                // Its *weight* is stamped in the same way, and in all three of the ways
                                // a weight changes a line: the width above, the pencil's own alpha
                                // (`PenWeight::alpha`), and — when `pressure_fades` is on — how pale
                                // this particular press leaves it.
                                //
                                // An opaque stroke's colour is left *exactly* the `0xRRGGBB` it was set
                                // to, with no top byte written on it. `with_alpha` would stamp `0xFF`
                                // there, which draws the same but is not the same number: it would make
                                // every new stroke unequal to the colour the reader chose, and rewrite
                                // the bytes of every plain pen stroke in the note (see `alpha_of` for
                                // why no top byte is the encoding's way of saying opaque).
                                self.open_id = self.open_id.wrapping_add(1);
                                self.peak_pressure = pressure;

                                let colour = Self::pen_colour(settings, pressure);

                                self.open = Some(Stroke::new(InkPoint::new(x, y, width), colour));
                            }
                            // A highlighter is a pen stroke whose ink lets the page through: one fixed width, and a
                            // colour with an alpha in it. Both are stamped in at the same moment the pen's are, so the
                            // line carries what it was drawn with and nothing about the tool has to be remembered
                            // afterwards — the *stroke* is wide, not the tool (see [`alpha_of`] and [`add_point`]).
                            // It is freehand wherever it is drawn: a band follows the nib exactly as a line does.
                            Tool::Highlighter => {
                                self.open_id = self.open_id.wrapping_add(1);
                                self.open = Some(Stroke::new(
                                    InkPoint::new(x, y, HIGHLIGHTER_WIDTH),
                                    with_alpha(settings.highlighter_color, HIGHLIGHTER_ALPHA),
                                ));
                            }
                            // The lasso takes hold of the ink already in hand when the reading is *on* it,
                            // and sweeps a new loop around whatever else it is pressed on.
                            Tool::Lasso => self.begin_lasso(x, y),
                        }

                        changed = true;
                    }
                }

                Phase::Move => {
                    if self.active_pointer == Some(sample.id) {
                        if !on_paper {
                            self.stats.off_paper += 1;
                        }
                        match self.active_tool {
                            Tool::Eraser => self.erase_at(x, y, settings),
                            Tool::Pen | Tool::Highlighter => {
                                self.push_point(x, y, sample, settings, false)
                            }
                            Tool::Lasso => self.sweep_lasso(x, y, sample, settings),
                        }
                        changed = true;
                    }
                }

                Phase::Up => {
                    if self.active_pointer == Some(sample.id) {
                        if !on_paper {
                            self.stats.off_paper += 1;
                        }

                        match self.active_tool {
                            // The lift is where the pen left the paper, so it is the stroke's last point
                            // regardless of what the resampler would prefer. A highlighter's band ends the
                            // same way: it is one stroke written as the nib moved, not a span of characters.
                            Tool::Pen | Tool::Highlighter => {
                                self.push_point(x, y, sample, settings, true);
                            }
                            // The loop ends where the pen lifted, and a drag is put down there.
                            Tool::Lasso => self.end_lasso(Some((x, y)), sample, settings),
                            Tool::Eraser => {}
                        }

                        self.finish_open();
                        changed = true;
                    }
                }

                Phase::Cancel | Phase::Leave => {
                    if self.active_pointer == Some(sample.id) {
                        // The position these phases carry is not ink: the stroke ends wherever
                        // it actually got to. A lasso's gesture ends the same way — a loop selects what
                        // it enclosed and a drag is put down — because a lift that never arrived is no
                        // reason to lose what the reader did.
                        if self.active_tool == Tool::Lasso {
                            self.end_lasso(None, sample, settings);
                        }

                        self.finish_open();
                        changed = true;
                    }
                }

                // Hover readings move a preview cursor but never lay ink.
                Phase::Idle | Phase::Enter | Phase::Hover => {}
            }

            self.last_sample = Some(*sample);
        }

        // The flags and the strokes are one list in two halves, and a mismatch is a selection pointing at
        // the wrong lines — which nothing else here would ever notice.
        debug_assert_eq!(
            self.selected.len(),
            self.finished.len(),
            "the selection and the stroke list must be the same length"
        );

        changed
    }

    /// Ends the open stroke, keeping it if it has any ink.
    ///
    /// Called for the lift, for a cancel or leave, and when a new stroke begins on a pointer
    /// whose previous lift never arrived.
    fn finish_open(&mut self) {
        if let Some(mut stroke) = self.open.take() {
            if !stroke.is_empty() {
                // Detailed for the sheet as it is being drawn now rather than for 1:1: a stroke
                // finished while the reader is zoomed in has to be as smooth as the ones around it,
                // and the page's detail is whatever the last frame asked for (see [`Self::set_zoom`]).
                stroke.freeze(detail_of(self.detailed_at), Tip::Curved);

                // One edit, and the only one a pen ever makes: the line it has just finished, written where the
                // page ends. Recording it *is* the page change — the stroke is inserted by the edit's own
                // `apply`, so a stroke written now and a stroke put back by a redo travel the same path.
                let at = self.finished.len();
                self.record(Edit::Written {
                    at,
                    strokes: vec![Arc::new(stroke)],
                });
            }
        }
        self.active_pointer = None;
    }

    /// The smoothing factor for this reading, in `0.0..=1.0`.
    ///
    /// `dt / (dt + tau)` is the standard first-order low-pass: the marker moves most of the way
    /// to a reading that arrived after a long gap, and a small fraction of the way to one that
    /// arrived in the same instant as the last. A repeated or out-of-order timestamp has no
    /// step to measure, so the raw position is used rather than a stalled one.
    fn alpha(&self, sample: &Reading, settings: &Settings) -> f32 {
        if settings.smoothing_ms <= 0.0 {
            return 1.0;
        }

        match self.last_sample {
            Some(previous) => {
                let dt_ms = sample.dt_ms(&previous);
                if dt_ms <= 0.0 {
                    1.0
                } else {
                    dt_ms / (dt_ms + settings.smoothing_ms)
                }
            }
            None => 1.0,
        }
    }

    /// Adds a point to the stroke being drawn, after smoothing and resampling.
    fn push_point(&mut self, x: f32, y: f32, sample: &Reading, settings: &Settings, force: bool) {
        let alpha = self.alpha(sample, settings);

        // The firmest press so far, raised before the point is added so that the colour and the shape of this
        // reading agree with each other. `None` means the pen reported no force at all, which is not the same as
        // no force being applied: a hovering pen and a sensorless one both stay `None`, and a stroke that only
        // ever saw `None` keeps its weight's own alpha (see [`Self::peak_pressure`]).
        if let Some(pressure) = sample.applied_pressure() {
            self.peak_pressure = Some(
                self.peak_pressure
                    .map_or(pressure, |peak| peak.max(pressure)),
            );
        }

        // Only the pen's own ink follows the hand. A highlighter is laid at one fixed alpha and its band must not
        // be recoloured here, and a lasso's loop is not ink at all — both come through this same walk, so the
        // question has to be asked of the tool rather than assumed from which caller reached it.
        let pen = self.active_tool == Tool::Pen;

        if let Some(stroke) = self.open.as_mut() {
            add_point(
                stroke,
                &mut self.stats,
                alpha,
                x,
                y,
                sample,
                settings,
                force,
            );

            // The ink follows the hand as it presses harder. One colour for the whole stroke, so this recolours
            // all of it rather than appending bands of different darkness — which is what keeps a stroke a
            // single polygon and therefore free of the seams that translucent segments would show where they
            // meet. It is the *same* stroke being laid darker, not a second one.
            if pen && !stroke.points.is_empty() {
                stroke.color = Self::pen_colour(settings, self.peak_pressure);
            }
        }
    }

    /// Removes the finished strokes the eraser nib passes over.
    ///
    /// Erasing at stroke granularity (rather than splitting strokes) is the prototype's
    /// trade-off: it is predictable, cheap, and never leaves stray fragments.
    fn erase_at(&mut self, x: f32, y: f32, settings: &Settings) {
        // A slow drag revisits the same few pixels for many readings; rescanning every stroke
        // for each of them is wasted work.
        if let Some((last_x, last_y)) = self.last_erase {
            let dx = x - last_x;
            let dy = y - last_y;
            let step = settings.erase_radius * 0.5;
            if dx * dx + dy * dy < step * step {
                return;
            }
        }
        self.last_erase = Some((x, y));

        // One pass. The hit test is not free — a stroke that overlaps the nib is *walked*, reading by reading,
        // until one of them is close enough — so asking it twice for every stroke the eraser touches is half the
        // work of the drag done again. The old shape of this was two passes: `any` to find out whether there was
        // anything to do, then `filter` to collect it. That bought nothing, because a stroke the `any` stopped at
        // was retested by the `filter` from the beginning, and a stroke the `any` found early was walked twice.
        //
        // The bounds test that makes the `any` cheap is inside `hits` itself, so the common case — a drag over
        // blank paper — still costs four comparisons a stroke and no allocation. The `Vec` is only ever built
        // from strokes that were actually hit, which is the same set the second pass used to find.
        //
        // What the eraser took, with the indices it took them from: an edit has to be able to put them back where
        // they were, so the strokes are *held* rather than dropped — which is the whole difference between an
        // eraser that can be undone and one that cannot.
        let mut removed: Vec<(usize, Arc<Stroke>)> = Vec::new();

        for (index, stroke) in self.finished.iter().enumerate() {
            if stroke.hits(x, y, settings.erase_radius) {
                removed.push((index, Arc::clone(stroke)));
            }
        }

        if removed.is_empty() {
            return;
        }

        self.stats.erased_strokes += removed.len() as u64;
        // One edit for the whole sweep of the nib, and the redo branch ends with it — the rule every edit
        // follows. The flags that say which strokes are in hand are the *edit's* business: removing a stroke
        // removes its flag (see [`Edit::apply`]), which is what used to be three lines of keeping two lists in
        // step by hand.
        self.record(Edit::Erased { removed });
    }

    /// Which strokes are in hand: one flag per finished stroke, in the same order.
    ///
    /// Handed to a frame as it stands — an `Arc`, so a frame shares the flags rather than copying them —
    /// and read by the layer to draw the ink in hand in the selection's colour.
    pub fn selected(&self) -> &Arc<Vec<bool>> {
        &self.selected
    }

    /// How many strokes are in hand.
    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|flag| **flag).count()
    }

    /// Whether anything is in hand.
    pub fn has_selection(&self) -> bool {
        self.selected.iter().any(|flag| *flag)
    }

    /// The loop being swept, if the lasso is down and drawing one.
    ///
    /// What a frame draws as the ring: the same stroke machinery as the line under the pen, so a lasso
    /// looks like something the reader drew rather than like an overlay the system painted.
    pub fn lasso(&self) -> Option<&Stroke> {
        match &self.lasso {
            Some(Lasso::Sweeping(loop_stroke)) => Some(loop_stroke),
            _ => None,
        }
    }

    /// How far the selection is being dragged, in the paper's units. `(0.0, 0.0)` when it is not.
    pub fn drag_offset(&self) -> (f32, f32) {
        match &self.lasso {
            Some(Lasso::Dragging { by, .. }) => *by,
            _ => (0.0, 0.0),
        }
    }

    /// How many times this page's ink has been shifted in place: see [`Self::shifted`].
    pub fn shifted(&self) -> u64 {
        self.shifted
    }

    /// Lets the selection go, answering whether there was one.
    pub fn deselect(&mut self) -> bool {
        if !self.has_selection() {
            return false;
        }

        self.selected = Arc::new(vec![false; self.finished.len()]);
        true
    }

    /// Deletes the strokes in hand, answering how many went.
    ///
    /// The same edit an eraser makes — a removal, with the strokes and the places they came from — so undo puts
    /// the selection back exactly where it was. "Not undoable" is a thing of the past: what is filed is the
    /// removal, and that is all undo needs.
    pub fn delete_selected(&mut self) -> usize {
        let removed: Vec<(usize, Arc<Stroke>)> = self
            .finished
            .iter()
            .enumerate()
            .filter(|(index, _)| self.selected.get(*index).copied().unwrap_or(false))
            .map(|(index, stroke)| (index, Arc::clone(stroke)))
            .collect();

        if removed.is_empty() {
            return 0;
        }

        let count = removed.len();
        self.record(Edit::Erased { removed });
        count
    }

    /// Moves the strokes in hand by `by`, in the paper's units: what putting a drag down does.
    ///
    /// One edit, and the only one that changes a page without adding or removing a stroke — which is why the
    /// page counts its shifts (see [`Self::shifted`]) and why undo of a move has to count one too.
    pub fn move_selected(&mut self, by: (f32, f32)) -> bool {
        if by.0 == 0.0 && by.1 == 0.0 {
            return false;
        }

        let strokes: Vec<usize> = self
            .selected
            .iter()
            .enumerate()
            .filter(|(_, in_hand)| **in_hand)
            .map(|(index, _)| index)
            .collect();

        if strokes.is_empty() {
            return false;
        }

        self.record(Edit::Shifted { strokes, by })
    }

    /// Begins what the lasso's nib is doing: taking hold of the ink in hand, or sweeping a new loop.
    ///
    /// A press *on* the selected ink takes hold of it — the gesture every notebook has — and a press
    /// anywhere else starts a new loop, which is also how a selection is let go: a reader pressing beside
    /// the ink is drawing a region, not asking for the old one.
    fn begin_lasso(&mut self, x: f32, y: f32) {
        if self.takes_hold_at(x, y) {
            self.lasso = Some(Lasso::Dragging {
                from: (x, y),
                by: (0.0, 0.0),
            });
            return;
        }

        self.selected = Arc::new(vec![false; self.finished.len()]);
        self.lasso = Some(Lasso::Sweeping(Stroke::new(
            InkPoint::new(x, y, LASSO_WIDTH),
            0,
        )));
    }

    /// Whether a reading is on the ink in hand: what takes hold of a selection rather than sweeping a loop.
    fn takes_hold_at(&self, x: f32, y: f32) -> bool {
        self.finished.iter().enumerate().any(|(index, stroke)| {
            self.selected.get(index).copied().unwrap_or(false) && stroke.hits(x, y, GRAB_RADIUS)
        })
    }

    /// Keeps up with what the lasso's nib is doing: a loop grows, and a drag follows the hand.
    fn sweep_lasso(&mut self, x: f32, y: f32, sample: &Reading, settings: &Settings) {
        let alpha = self.alpha(sample, settings);

        match self.lasso.as_mut() {
            Some(Lasso::Sweeping(loop_stroke)) => {
                add_point(
                    loop_stroke,
                    &mut self.stats,
                    alpha,
                    x,
                    y,
                    sample,
                    settings,
                    false,
                );
            }
            Some(Lasso::Dragging { from, by }) => *by = (x - from.0, y - from.1),
            None => {}
        }
    }

    /// Ends what the lasso's nib was doing, at the position it ended at.
    ///
    /// A sweep *selects*: the loop is the region, and the strokes inside it are in hand. A drag is *put
    /// down*: the ink moves once, and stays in hand, so it can be taken somewhere else.
    fn end_lasso(&mut self, at: Option<(f32, f32)>, sample: &Reading, settings: &Settings) {
        let Some(lasso) = self.lasso.take() else {
            return;
        };

        match lasso {
            Lasso::Sweeping(mut loop_stroke) => {
                if let Some((x, y)) = at {
                    let alpha = self.alpha(sample, settings);
                    add_point(
                        &mut loop_stroke,
                        &mut self.stats,
                        alpha,
                        x,
                        y,
                        sample,
                        settings,
                        true,
                    );
                }

                self.select_inside(&loop_stroke);
            }
            Lasso::Dragging { from, mut by } => {
                if let Some((x, y)) = at {
                    by = (x - from.0, y - from.1);
                }

                self.move_selected(by);
            }
        }
    }

    /// Takes the strokes the loop encloses, and lets go of the ones it does not.
    ///
    /// The loop's own bounds answer most strokes in four comparisons — the same cheap first test the eraser makes
    /// (`Stroke::visible_in`, which is a bounds-overlap test) — and the survivors are read point by point against
    /// a [`Loop`], which is the loop prepared once for all of them rather than walked afresh for every reading.
    fn select_inside(&mut self, region: &Stroke) {
        let loop_bounds = region.bounds;
        let lasso = Loop::of(&region.points);
        let mut selected = Vec::with_capacity(self.finished.len());

        for stroke in self.finished.iter() {
            selected.push(
                stroke.visible_in(loop_bounds)
                    && stroke
                        .points
                        .iter()
                        .any(|point| lasso.contains((point.x, point.y))),
            );
        }

        self.selected = Arc::new(selected);
    }

    /// Drops whatever the nib was in the middle of: a page turn ends a gesture rather than carrying it over.
    ///
    /// A lasso's loop is not ink, and a drag has not moved anything yet (see [`Lasso`]) — so there is nothing
    /// to *finish*: the page being left keeps its selection exactly as it was, and the page being turned to
    /// starts with none of somebody else's half-done gesture on it.
    fn forget_gesture(&mut self) {
        self.lasso = None;
        self.active_pointer = None;
    }
}

/// Adds a point to a stroke being drawn, after smoothing and resampling.
///
/// A free function rather than a method, because two strokes are drawn this way: the line under the pen and
/// the loop the lasso is sweeping. What differs between them is only which stroke it is.
///
/// `force` bypasses the resampler, and is used for the end of a stroke — the lift, or the last reading of a
/// loop: a distance filter that applied its own rule to the final point would shorten every stroke by up to
/// one spacing.
///
/// The counters it updates count *readings* rather than ink: a lasso's loop is read by the same walk, so the
/// status line says what the digitizer sent — which is what a person measuring a pen wants to know, whatever
/// the readings are being turned into.
fn add_point(
    stroke: &mut Stroke,
    stats: &mut InkStats,
    alpha: f32,
    x: f32,
    y: f32,
    sample: &Reading,
    settings: &Settings,
    force: bool,
) {
    let (target_x, target_y) = match (stroke.points.last(), alpha < 1.0) {
        (Some(last), true) => (last.x + (x - last.x) * alpha, last.y + (y - last.y) * alpha),
        _ => (x, y),
    };

    if !force {
        if let Some(last) = stroke.points.last() {
            let dx = last.x - target_x;
            let dy = last.y - target_y;
            if dx * dx + dy * dy < settings.resample_spacing * settings.resample_spacing {
                stats.resampled += 1;
                return;
            }
        }
    }

    // The lift reports no applied force (`applied_pressure()` is `None` off the surface), so an existing
    // point's width is reused rather than letting the stroke change width in its last pixel.
    //
    // A highlighter never follows the pressure at all: a marker has one tip, and a translucent band that thinned
    // out under a light touch would show the writing through in stripes. Which of the two this is comes from the
    // *stroke* rather than from the tool in hand, because a stroke knows what it was drawn with and the tool may
    // have been changed since.
    //
    // Being translucent is *not* what makes it a highlighter. A fine pen is translucent too — it is the pencil,
    // and a pencil that stopped answering pressure would be a thin marker (see `PenWeight::alpha`). What a
    // highlighter has and a pen does not is a **fixed width**: its points are all laid at the one tip's
    // thickness, which is the tool's rather than the hand's. So the question asked here is whether every point so
    // far is at the highlighter's width, and a stroke whose width has ever varied is one the pressure is moving.
    let fixed = alpha_of(stroke.color) < 1.0
        && stroke
            .points
            .iter()
            .all(|point| (point.width - HIGHLIGHTER_WIDTH).abs() < 1e-3);

    let width = if fixed {
        stroke
            .points
            .last()
            .map(|point| point.width)
            .unwrap_or(HIGHLIGHTER_WIDTH)
    } else {
        match sample.applied_pressure() {
            Some(_) => settings.width_for_pressure(sample.applied_pressure()),
            None => stroke
                .points
                .last()
                .map(|point| point.width)
                .unwrap_or_else(|| settings.width_for_pressure(None)),
        }
    };

    stroke.points.push(InkPoint::new(target_x, target_y, width));

    // Kept up to date as the stroke grows, rather than only when it closes: a frame asks whether the
    // stroke being drawn is on screen before it has ever been closed.
    stroke.bounds[0] = stroke.bounds[0].min(target_x);
    stroke.bounds[1] = stroke.bounds[1].min(target_y);
    stroke.bounds[2] = stroke.bounds[2].max(target_x);
    stroke.bounds[3] = stroke.bounds[3].max(target_y);

    stats.kept_points += 1;
}

/// A reader's lasso loop, prepared for being asked about many points: the edges sorted by where they start.
///
/// The loop is walked once, when it is made, and every question after that skips straight to the edges that can
/// cross the point's own row. A raw walk visits every edge of the loop for every reading of every stroke on the
/// page — a lasso drawn round a word, over a page of a few hundred strokes, asks the same three hundred edges
/// several hundred thousand times. Sorting them by their lower end turns each of those asks into a binary search
/// and the handful of edges that actually reach that row.
///
/// The rule is the same even-odd ray cast [`inside_loop`] uses; only the walking is cheaper.
pub(crate) struct Loop {
    /// The edges, lowest end first: `(low_y, high_y, x_at_low, x_at_high)`.
    edges: Vec<(f32, f32, f32, f32)>,
    /// The lowest `low_y` of any edge, so a point below every edge is answered without searching.
    lowest: f32,
}

impl Loop {
    /// Prepares a closed loop of readings. Fewer than three readings enclose nothing.
    pub(crate) fn of(points: &[InkPoint]) -> Self {
        if points.len() < 3 {
            return Loop {
                edges: Vec::new(),
                lowest: 0.0,
            };
        }

        let mut edges = Vec::with_capacity(points.len());
        let mut previous = points[points.len() - 1];

        for current in points {
            let (low, high) = if previous.y <= current.y {
                (previous, *current)
            } else {
                (*current, previous)
            };

            edges.push((low.y, high.y, low.x, high.x));
            previous = *current;
        }

        edges.sort_by(|left, right| left.0.total_cmp(&right.0));

        let lowest = edges.first().map_or(0.0, |edge| edge.0);

        Loop { edges, lowest }
    }

    /// How many edges of the loop a question at `y` has to look at: the ones whose lowest end is at or above it.
    ///
    /// This is the loop's whole saving, exposed so a test can assert the *mechanism* rather than time it. A
    /// question near the bottom of a loop looks at few edges and one near the top at nearly all of them, so the
    /// saving is real but not uniform — which is worth knowing when reading a profile of a lasso sweep.
    pub(crate) fn reachable(&self, y: f32) -> usize {
        self.edges.partition_point(|edge| edge.0 <= y)
    }

    /// Whether a point is inside the loop, by the even-odd rule.
    pub(crate) fn contains(&self, point: (f32, f32)) -> bool {
        if self.edges.is_empty() || point.1 < self.lowest {
            return false;
        }

        // Every edge that starts above the point cannot reach its row, and the edges are sorted by where they
        // start — so the search only has to look at the ones before that.
        let reachable = self.reachable(point.1);

        let mut inside = false;

        for edge in &self.edges[..reachable] {
            let (low_y, high_y, low_x, high_x) = *edge;

            // The y comparison is half-open, so a point level with a vertex is counted once rather than twice.
            if high_y > point.1 {
                let crossing = low_x + (point.1 - low_y) / (high_y - low_y) * (high_x - low_x);

                if point.0 < crossing {
                    inside = !inside;
                }
            }
        }

        inside
    }
}

/// Every page's ink, and which page is being written on.
///
/// ## Why the ink is per page
///
/// A stroke belongs to the sheet it was drawn on. The app used to hold a single [`InkDocument`] and
/// simply not move it when the user turned the page, so the ink of the page written on last was
/// still there on the next one — and worse, drawing on page two appended to page one's ink.
///
/// ## Why only one page is hot
///
/// The page being written on is the one the pen appends to several hundred times a second, so it is
/// kept inline; the others are moved in and out of a map on a page turn, which happens at the speed
/// of a hand, not a digitizer. A page whose ink is empty is not kept at all, so the map holds
/// exactly the pages that have been written on and `written_pages` is the truth about the note.
///
/// `Notes` dereferences to the current page, so the writing path reads `notes.consume(..)` and
/// `notes.finished()` and never has to name a page at all: there is only ever one page the pen can
/// be writing on, and this type owns which.
///
/// ## What the page owns, and what the reader carries
///
/// The split matters more than it looks, and getting it wrong is silent. The **page** owns the ink written on it, the
/// selection a lasso has of it, and the history of the edits it went through — all of which travel with it through
/// `taken`, and none of which a reader would expect another page to inherit. The **reader** owns the tool in hand and
/// the zoom: turning a page is not reaching for a different pen, and it is not standing closer to the paper either.
/// Every way a document comes to the front therefore goes through [`Self::show`], which is where the reader's state is
/// carried across.
#[derive(Debug)]
pub struct Notes {
    /// Which page [`Self::current`] is the ink of.
    page: usize,
    /// The ink of the page being written on.
    current: InkDocument,
    /// The ink of every other page that has been written on.
    taken: BTreeMap<usize, InkDocument>,
    /// The zoom the sheet is drawn at, as the last frame drew it.
    ///
    /// Kept here rather than asked of each page, because the pages in `taken` are not being drawn and
    /// a page turned *to* has to arrive with outlines detailed for the sheet it is about to be drawn
    /// on. Only the page in front of the reader is detailed as the zoom moves (see [`Self::set_zoom`]
    /// and [`InkDocument::set_zoom`]).
    zoom: f32,
    /// The tool in the reader's hand: which one an ordinary nib lays.
    ///
    /// Beside the zoom on purpose, because the two are the same kind of fact — the reader's, and carried from page to
    /// page — rather than the page's, like the ink, the selection and the history. The page's document keeps a copy
    /// because that is where the pen's path reads it ([`InkDocument::consume`]), and [`Self::show`] writes that copy
    /// from here on every page turn. See [`Self::mode`].
    mode: Tool,
}

impl Default for Notes {
    /// A note with one blank page, drawn at the size its paper asks for.
    fn default() -> Self {
        Notes {
            page: 0,
            current: InkDocument::blank(),
            taken: BTreeMap::new(),
            zoom: 1.0,
            mode: Tool::Pen,
        }
    }
}

impl std::ops::Deref for Notes {
    type Target = InkDocument;

    fn deref(&self) -> &InkDocument {
        &self.current
    }
}

impl std::ops::DerefMut for Notes {
    fn deref_mut(&mut self) -> &mut InkDocument {
        &mut self.current
    }
}

impl Notes {
    /// A note with one blank page.
    pub fn new() -> Self {
        Notes::default()
    }

    /// Which page the ink in hand belongs to: the page a write is *about*.
    ///
    /// The app asks this rather than its own idea of where the view is, and the two are the same
    /// number everywhere except in the middle of the two commands that move the page list itself —
    /// an inserted page and a deleted one. Those close the page they are leaving *before* the list
    /// moves (see `NoteApp::add_page`), so for one moment the view's index is a page nobody is on,
    /// and a write that read the index there is how the page being left once filed its whole ink
    /// under the page that took its place.
    pub fn page(&self) -> usize {
        self.page
    }

    /// The tool in the reader's hand.
    ///
    /// This is the home of the fact while a note is open — see [`Notes`] for why it is the reader's and not the page's.
    /// The page's own document keeps a copy for the pen's path ([`InkDocument::consume`]), and it is written by
    /// [`Self::show`] and [`Self::set_mode`] alone, so the two agree.
    pub fn mode(&self) -> Tool {
        self.mode
    }

    /// Puts a tool in the reader's hand.
    ///
    /// The pen's own state still wins over it: a pen whose eraser end is toward the screen erases even while the bar
    /// says `Pen`, because that is what the user is physically doing (see [`InkDocument::consume`]).
    pub fn set_mode(&mut self, mode: Tool) {
        self.mode = mode;
        self.current.set_mode(mode);
    }

    /// Tells the note what the sheet is drawn at, so the page in front of the reader is detailed for
    /// it — and so is the ink of a page turned to afterwards.
    ///
    /// Returns whether the page being drawn had to be rebuilt, which is what a caller uses to decide
    /// whether a frame is worth scheduling. The detail moves in rungs (see
    /// [`InkDocument::set_zoom`]), so a pinch or a wheel across the whole zoom range costs a handful
    /// of rebuilds rather than one per frame — and none at all while the zoom stays in one rung.
    /// Asks for the page to be detailed for `zoom`, and says whether that asked for a rebuild it has
    /// not done yet.
    ///
    /// **The rebuild is not done here.** Detail is a rung (see [`detail_zoom`]), and crossing one means
    /// re-deriving every stroke on the page: measured at 23.7 ms on a page of 8,182 strokes, which is
    /// three and a half 144 Hz frames. This is called from `render`, so doing the work here put all of
    /// it inside one frame — the frame that crossed the rung — and that is the hitch a pinch shows: the
    /// zoom itself is continuous, and the *detail* steps. The page is drawn at the detail it already has
    /// until [`Self::build_detail`] is given the time, which is at most one rung's worth of slightly
    /// coarser or finer pieces and never visible as a pause.
    ///
    /// `true` means the page is now drawn for a rung other than the one asked for and wants
    /// [`Self::build_detail`]. `false` means the rung did not move (or moved back to what is already
    /// built), so there is nothing to borrow a frame for.
    pub fn set_zoom(&mut self, zoom: f32) -> bool {
        self.zoom = zoom;
        self.current.request_detail(zoom)
    }

    /// Builds the detail [`Self::set_zoom`] asked for, if it is not the detail the page already has.
    ///
    /// Returns whether anything was rebuilt. This is the expensive half, deliberately outside a frame:
    /// the caller is the housekeeping pump, which has a window to spend and can be told to wait when
    /// the hand is in the middle of a gesture.
    pub fn build_detail(&mut self) -> bool {
        self.current.build_detail()
    }

    /// Moves to a page, taking the ink of the page being left behind with it.
    ///
    /// The page being moved to keeps its own ink, which is the whole point: what was drawn there is
    /// still there on the way back.
    pub fn go_to(&mut self, page: usize) {
        if page == self.page {
            return;
        }

        let mut leaving = std::mem::take(&mut self.current);
        // Whatever the nib was in the middle of ends with the page: a half-swept loop belongs to the paper it
        // was drawn on, and the page being turned to must not inherit a gesture nobody finished (see
        // [`InkDocument::forget_gesture`]). The *selection* stays — it is a fact about the page's writing.
        leaving.forget_gesture();
        // A page left with nothing on it is normally dropped — an empty document costs nothing to
        // make again — but one that can still be *redone* is not empty in the sense that matters
        // here: the strokes are gone from the page and still in the model, and a page turn is not an
        // edit. Keeping it is what makes undo, turn the page, turn back, redo work.
        if !leaving.is_blank() || leaving.can_redo() {
            self.taken.insert(self.page, leaving);
        }

        let arriving = self.taken.remove(&page).unwrap_or_else(InkDocument::blank);

        // The tool in hand is *not* part of what went into `taken`: it is the reader's, and they take it with them.
        self.show(arriving, page);
    }

    /// Puts one page's ink into the note: what reading a page out of the file gives.
    ///
    /// A page read from the store is the page the pen is about to write on or one it has just turned
    /// to, so this is the counterpart of [`Self::go_to`]: that one takes a page out, this one puts a
    /// page back. A page with nothing on it is not kept, for the reason `go_to` does not keep one —
    /// an empty document costs nothing to make again, and a map of them would only be holes.
    ///
    /// A page read out of a file arrives detailed for 1:1 (see [`InkDocument::from_strokes`]), which is
    /// right if the sheet is about to be drawn at the size its paper asks for and a page of
    /// straightened pieces if it is not. It is detailed here instead, for the zoom in front of the
    /// reader, in one pass over the stroke list.
    pub fn put_page(&mut self, page: usize, mut ink: InkDocument) {
        ink.set_zoom(self.zoom);

        if page == self.page {
            self.show(ink, page);
            return;
        }

        if ink.is_blank() {
            self.taken.remove(&page);
        } else {
            self.taken.insert(page, ink);
        }
    }

    /// Makes room at `page` for a page being inserted there: every page from it onwards becomes the
    /// page after it, and the ink moves with the name.
    ///
    /// The page the pen is on moves too. That is the half of this that is easy to forget and
    /// impossible to notice: leave it behind and the next stroke lands on the page before the one
    /// on screen.
    pub fn insert_at(&mut self, page: usize) {
        self.taken = std::mem::take(&mut self.taken)
            .into_iter()
            .map(|(index, ink)| (if index >= page { index + 1 } else { index }, ink))
            .collect();

        if self.page >= page {
            self.page += 1;
        }
    }

    /// Removes the page at `page` and the ink written on it, moving everything after it up one.
    ///
    /// The ink goes with the page: a deleted page's writing has nowhere to be shown, and keeping it
    /// would need an identity for "the page that used to be here" that no later page could be
    /// confused with. Undo is one stroke at a time (see [`InkDocument::undo`]), so nothing here is
    /// expected to be reversible.
    pub fn remove_at(&mut self, page: usize) {
        self.taken.remove(&page);

        if page == self.page {
            // The page in front of the reader is the one that followed the deleted page, or a blank
            // sheet when it was the last.
            let arriving = self
                .taken
                .remove(&(page + 1))
                .unwrap_or_else(InkDocument::blank);

            // The tool in hand comes across with the reader: the page that follows a deleted one is written on with
            // the pen that was already in hand, not with whatever the note's default is.
            self.show(arriving, page);
        } else if page < self.page {
            self.page -= 1;
        }

        self.taken = std::mem::take(&mut self.taken)
            .into_iter()
            .map(|(index, ink)| (if index > page { index - 1 } else { index }, ink))
            .collect();
    }

    /// Puts a document in front of the reader, with the reader's tool still in hand.
    ///
    /// The ink, the selection and the history belong to the **page**, and travel with it through `taken`; the tool in
    /// hand and the zoom are the **reader's**, because turning a page is not reaching for a different pen. So every way
    /// a document comes to the front goes through here, and both of the reader's two facts are handed to the arriving
    /// document from [`Self::mode`] and [`Self::zoom`].
    ///
    /// Getting this wrong is invisible and compound: the bar would quietly show the tool some *other* page was last
    /// written with, and the next stroke would be laid with it, so one page's marker would write on the next page. It is
    /// also why the tool is carried from `self.mode` rather than read off the document being left: by the time a turn
    /// has taken that document away, what is left in its place holds nothing.
    fn show(&mut self, mut ink: InkDocument, page: usize) {
        ink.set_mode(self.mode);
        // A page turned back to holds the ink it did, and its outlines were detailed for the sheet as
        // it was drawn when the reader left it. One pass, and only when the detail has moved since.
        ink.set_zoom(self.zoom);
        self.current = ink;
        self.page = page;
    }
}

#[cfg(test)]
mod tests;
