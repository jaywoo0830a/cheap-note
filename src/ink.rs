//! The ink model: readings in, strokes out.
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
//! GPUI's canvas paint callback is `FnOnce`, so each frame hands the renderer an owned
//! snapshot of the ink. Cloning a `Vec<Stroke>` every frame would be O(strokes) per frame at
//! the display's rate; cloning an `Arc` is a pointer copy. The strokes are behind an `Arc` of
//! their own for the same reason one level down: a vector of pointers can be appended to,
//! undone and filtered without touching the strokes themselves.
//!
//! ## What undo is, and what it is not
//!
//! Undo is one *finished* stroke at a time, per page, and redo is that same list walked the other
//! way. The rules are the ones a hand expects, and they are written down because the exceptions are
//! what a user notices:
//!
//! * **The stroke under the nib is not history.** It has no closed geometry and the pen is still on
//!   the paper, so undo leaves it exactly where it is: taking it away would remove a line that was
//!   never finished, and would leave the model thinking the pen was up while it is down.
//! * **Any new ink ends the redo branch.** A stroke finished after an undo — or an erase that
//!   removed something — is a new drawing, so what was taken back is no longer something to put
//!   forward. This is the rule every editor has, and the reason redo can be trusted.
//! * **Erasing is not undoable.** The eraser removes whole strokes as its nib passes over them (see
//!   [`InkDocument::erase_at`]), so undo afterwards takes the most recent *surviving* stroke of the
//!   page, which need not be one the eraser touched.
//! * **Clearing is not undoable either.** It is the one command that says "none of this page", and
//!   it ends both directions rather than leaving a way back that the command itself did not mean.
//! * **A page keeps its history while it can still be redone**, even with nothing drawn on it, so
//!   turning the page and turning back does not quietly throw the redo away.

use std::collections::BTreeMap;
use std::sync::Arc;

use pen_windows::{PenPhase, PenSample};
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

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
        }
    }

    /// Where a reading in physical client pixels lands on the sheet.
    pub fn sheet_point(&self, pixel: (f32, f32)) -> (f32, f32) {
        // A window that reports a zero scale or a zero zoom would divide every point into a
        // corner, and a NaN would poison every rectangle derived from it. Both mean "no
        // transform", which draws the ink where the pen is.
        let scale = finite_or_one(self.scale);
        let zoom = finite_or_one(self.zoom);

        (
            (pixel.0 / scale - self.origin.0) / zoom,
            (pixel.1 / scale - self.origin.1) / zoom,
        )
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
    /// Remove the strokes the nib passes near.
    Eraser,
}

/// One point of a stroke: a position and the width the pen drew it at.
///
/// The width is baked in when the point is created, so changing the pen — its colour or its weight
/// ([`crate::settings`]) — does not retroactively redraw the ink already laid down.
///
/// `Serialize` and no `Deserialize`: the only reader of ink is [`crate::chunk`], which has its own
/// encoding, and the one thing that ever *parsed* a stroke was the reader for the old note format.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct InkPoint {
    /// Horizontal position, in logical (DPI-independent) pixels.
    pub x: f32,
    /// Vertical position, in logical (DPI-independent) pixels.
    pub y: f32,
    /// The full stroke width at this point, in logical pixels.
    pub width: f32,
}

impl InkPoint {
    /// A point at the given position and width.
    pub const fn new(x: f32, y: f32, width: f32) -> Self {
        InkPoint { x, y, width }
    }
}

/// A finished or in-progress line of ink.
///
/// Written out only by tests, which measure the chunk encoding against JSON (see [`crate::chunk`]);
/// nothing in the app serialises a stroke, and nothing reads one back as anything but a chunk.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stroke {
    /// The positions, oldest first.
    pub points: Vec<InkPoint>,
    /// The colour this stroke was written in, as `0xRRGGBB`.
    ///
    /// Per stroke, not per page: the palette is a set of pens, and picking up a different one must
    /// not repaint what the others wrote.
    pub color: u32,
    /// The ribbon outline, in logical pixels, cached when the stroke is closed.
    ///
    /// Recomputing the outline every frame is pure waste: the points never change once the
    /// stroke is finished. Not serialised, because it is derivable from `points` — and *only* from
    /// them plus the zoom it was built for, which is why it is thrown away and built again when the
    /// sheet is drawn at another size (see [`Self::close_at`] and [`InkDocument::set_zoom`]).
    #[serde(skip)]
    pub outline: Vec<[f32; 2]>,
    /// The axis-aligned bounds as `[min_x, min_y, max_x, max_y]`, for culling and hit-testing.
    #[serde(skip)]
    pub bounds: [f32; 4],
}

impl Stroke {
    /// The near-black the app drew everything in before a stroke could carry a colour of its own.
    ///
    /// Test-only, and named here rather than repeated in every test module: the app's own paths write
    /// ink in [`crate::settings::Settings::ink_color`], and a page of ink in a test still has to be
    /// written in *some* colour.
    #[cfg(test)]
    pub const DEFAULT_COLOR: u32 = 0x1B_1B_1F;

    /// The colour a stroke beginning at one point is written in.
    pub fn new(point: InkPoint, color: u32) -> Self {
        Stroke {
            points: vec![point],
            color,
            outline: Vec::new(),
            bounds: [point.x, point.y, point.x, point.y],
        }
    }

    /// Whether this stroke has nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Freezes the stroke: computes its bounds and its cached ribbon outline.
    ///
    /// Called once, when the pen lifts, so the render path only has to walk the outline. The detail
    /// is 1:1, which is the size a stroke asks to be drawn at; a sheet drawn larger refines it (see
    /// [`Self::close_at`] and [`InkDocument::set_zoom`]).
    pub fn close(&mut self) {
        self.close_at(1.0);
    }

    /// Freezes the stroke for a sheet drawn at `zoom`: its bounds, and the outline of the whole line.
    ///
    /// The detail follows the zoom because the *pieces* a stroke is drawn from are its geometry: the
    /// larger the sheet is drawn, the shorter a piece has to be for the ink to read as a curve rather
    /// than as a chain of straight edges (see [`FACET`]). This is the outline of a stroke the pen has
    /// left, and every segment of it is interpolated, the last one included.
    pub fn close_at(&mut self, zoom: f32) {
        self.freeze(detail_zoom(zoom), Tip::Curved);
    }

    /// Freezes the stroke for the frame drawing it *while the pen is still down*.
    ///
    /// [`Self::close_at`] with one exception: the newest segment is left as the straight line between
    /// its points. A curve through the tip needs the reading *after* it, and one drawn from the tip's
    /// neighbours alone would move ink the user has already seen, under the nib, as they write. See
    /// [`Tip`].
    pub fn close_live(&mut self, zoom: f32) {
        self.freeze(detail_zoom(zoom), Tip::Straight);
    }

    /// What all of the above do, given the detail rather than the zoom.
    ///
    /// The bounds are the *points'*, and the interpolated line cannot wander far from them (see
    /// [`densified`]) — which is what makes them still the right numbers for culling a frame and for
    /// the eraser's first, cheap test.
    fn freeze(&mut self, detail: f32, tip: Tip) {
        if self.points.is_empty() {
            self.bounds = [0.0; 4];
            self.outline.clear();
            return;
        }

        let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for point in &self.points {
            bounds[0] = bounds[0].min(point.x);
            bounds[1] = bounds[1].min(point.y);
            bounds[2] = bounds[2].max(point.x);
            bounds[3] = bounds[3].max(point.y);
        }
        self.bounds = bounds;
        self.outline = ribbon_outline(&self.points, detail, tip);
    }

    /// Whether the eraser at `(x, y)` with the given radius touches this stroke.
    ///
    /// The test is against the stroke's *segments*, not only its stored points. A fast, straight
    /// stroke is resampled down to a handful of points, so an eraser dragged across the middle
    /// of the line can be centimetres from any of them — a points-only test would leave half a
    /// word behind while cutting through it.
    ///
    /// The bounds test rejects most strokes in a couple of comparisons; only the survivors pay
    /// for the segment scan.
    pub fn hits(&self, x: f32, y: f32, radius: f32) -> bool {
        if self.points.is_empty() {
            return false;
        }
        if x < self.bounds[0] - radius
            || x > self.bounds[2] + radius
            || y < self.bounds[1] - radius
            || y > self.bounds[3] + radius
        {
            return false;
        }

        let radius_squared = radius * radius;

        if self.points.len() == 1 {
            let only = self.points[0];
            return squared_distance(x, y, only.x, only.y) <= radius_squared;
        }

        self.points
            .windows(2)
            .any(|pair| distance_to_segment_squared(x, y, pair[0], pair[1]) <= radius_squared)
    }

    /// Whether any of this stroke falls inside a rectangle of the sheet, as `[min_x, min_y,
    /// max_x, max_y]`.
    ///
    /// This is how a frame leaves the ink that is off screen out of the picture. The bounds are
    /// already kept for hit-testing, so the test is four comparisons — against building a polygon
    /// for every stroke on the page and letting the renderer discover that most of them are
    /// outside the window, which is the most expensive thing an immediate-mode canvas can be
    /// asked to do. It matters most when zoomed in, where most of a page is off screen.
    pub fn visible_in(&self, rect: [f32; 4]) -> bool {
        if self.points.is_empty() {
            return false;
        }

        self.bounds[0] <= rect[2]
            && self.bounds[2] >= rect[0]
            && self.bounds[1] <= rect[3]
            && self.bounds[3] >= rect[1]
    }
}

/// The squared distance between two positions.
///
/// Squared, because comparing squared distances is a `sqrt` cheaper per test and the ordering
/// is the same.
fn squared_distance(x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let dx = x1 - x0;
    let dy = y1 - y0;
    dx * dx + dy * dy
}

/// The squared distance from a position to the line *segment* between two stroke points.
fn distance_to_segment_squared(x: f32, y: f32, from: InkPoint, to: InkPoint) -> f32 {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let length_squared = dx * dx + dy * dy;

    if length_squared <= f32::EPSILON {
        return squared_distance(x, y, from.x, from.y);
    }

    // The position on the segment closest to the query, as a fraction along it. Clamped, so
    // the nearest point is on the segment rather than on the infinite line.
    let t = (((x - from.x) * dx + (y - from.y) * dy) / length_squared).clamp(0.0, 1.0);
    squared_distance(x, y, from.x + t * dx, from.y + t * dy)
}

/// The longest piece of a stroke that may be drawn as one straight edge, in window pixels.
///
/// A stroke is a polyline: the pen's readings are dropped until they are `resample_spacing` apart
/// (see [`Settings::resample_spacing`]), and between two of the points that survive, the ink is a
/// straight edge. At 1:1 that is invisible — the spacing is under a pixel — but a zoom magnifies the
/// line *and* the corner at the end of every piece, so a stroke of curves read at 8x is a chain of
/// straight pieces. That is what "the ink wobbles when I zoom in" is made of, and cutting the pieces
/// down to this length is what removes it: a straight edge shorter than a pixel cannot be told from
/// the curve it stands in for.
///
/// Measured in *window* pixels rather than the sheet's, because the zoom is what makes a piece
/// visible: the same ink written at 1:1 and examined at 16x needs pieces sixteen times shorter in
/// sheet coordinates to look the same.
const FACET: f32 = 1.0;

/// The most pieces one segment of a stroke may be cut into.
///
/// A fast hand leaves points tens of pixels apart, and one of those at 16x would be hundreds of
/// window pixels long — hundreds of pieces, for one segment, in an outline the renderer
/// re-tessellates every frame. The bound keeps a stroke's derived geometry proportional to its
/// readings rather than to the zoom, and it costs less than it looks: a segment long enough to reach
/// the bound was drawn at speed, and a fast hand draws rounded lines, so the corner between two long
/// segments is a small turn of direction and a coarse piece of one is not what the eye notices.
const MAX_STEPS: usize = 16;

/// The zoom a stroke's outline is detailed for: `zoom`, rounded up to the next rung.
///
/// The detail a stroke needs is a function of the zoom, so a page's derived geometry has to be
/// rebuilt when the reader zooms — but rebuilding a page of ink on *every* frame of a pinch, to show
/// a difference nobody can see, is not a trade worth making. Rounding up to a geometric rung (1.5x at
/// a time, the way the page renders' own rungs grow — see `crate::app`) makes the rebuild happen at a
/// handful of zooms across the whole range, and it is also the right number to draw for: at the top
/// of a rung the pieces are exactly [`FACET`] long, and lower in it they are shorter.
fn detail_zoom(zoom: f32) -> f32 {
    /// The factor between rungs.
    const RUNG: f32 = 1.5;

    let zoom = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        // A window that reports no zoom is drawing the sheet at the size it asks for, which is what
        // an outline is built for when nothing has said otherwise.
        1.0
    };

    // The nudge is not decoration: a rung handed back to this function has to land on *itself*, and
    // the logarithm of `1.5^n` is not exactly `n` in floating point. Without it a rung would round up
    // to the next one and rebuild a page that was already drawn for it.
    RUNG.powf((zoom.log(RUNG) - 1e-4).ceil())
}

/// Whether the newest segment of a stroke is drawn as a curve or as the line its points describe.
///
/// A curve through a point needs the point *after* it, and the newest reading of a stroke under the
/// nib has none: drawn from the tip's neighbours alone, the ink would move under the pen as the user
/// writes. So the newest segment of a stroke being drawn is left straight — it is one resample
/// spacing long, usually under a pixel, and its corner with the segment before it is interpolated
/// like every other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tip {
    /// Every segment is a curve. A finished stroke is drawn this way, its last segment included.
    Curved,
    /// The newest segment is the straight line between its two points: the pen is still down.
    Straight,
}

/// How many passes of the steadier take the digitizer's own noise out of a stroke's positions.
///
/// One, and the number is worth writing down. A panel reports its readings a fifth of a pixel or so
/// off the line the hand meant; at 1:1 that is invisible — the eye does not resolve a fifth of a pixel
/// — and a zoom magnifies it: at 16x that same fifth of a pixel is several pixels of wander along the
/// edge of a stroke, which is what a reader sees when they zoom into their own handwriting. Measured
/// with this code (`tests::the_wobble_a_zoom_shows`): 4.0 pixels of wander at 16x as the readings came,
/// 2.5 with one pass. A second pass buys little more (2.2) while moving the line as much again, and
/// every pass moves the ink away from where it was written.
const STEADYING: usize = 1;

/// The widest spacing at which a point is steadied at all, in logical pixels.
///
/// Below this the three points a pass averages are within a couple of pixels of each other, so the
/// average can move the line by a fraction of a pixel whichever way they lie — which is the size of the
/// noise being taken out, and below what an eye resolves at the size a note is written at. Above it the
/// spacing is the *hand's own speed*: a flick leaves points twenty pixels apart, and three-point
/// averaging over that moves the line five pixels — it stops being a steadier and starts being a
/// redraw, rounding corners the pen actually turned.
///
/// This is also the honest edge of what any of this can do. A fast hand's readings are all there is to
/// know about where its line went; nothing here pretends to know where it "really" was.
const STEADY_WITHIN: f32 = 2.0;

/// The positions a stroke is drawn from: the readings, with a share of the digitizer's noise taken out
/// of the ones in the middle of the stroke.
///
/// ## Why this is not the low-pass `Settings::smoothing_ms` is
///
/// That one is *causal*: it can only use readings that have already arrived, so it steadies a line by
/// putting it behind the nib — and the app ships with it off for exactly that reason. This one is
/// symmetric, and it is applied to the *derived* geometry, where the reading after a point is in hand:
/// it steadies the line without any lag at all. The reading the nib is at is not one of the ones it
/// touches (there is no reading after it yet), so the tip is exactly where the pen is — and the point
/// behind it, which *is* steadied once the next reading arrives, moves by less than the fifth of a
/// pixel the filter is taking out.
///
/// ## What it costs, and why it is a constant rather than a setting
///
/// The line no longer passes exactly through every reading — which is the one thing this file's
/// interpolation is careful *not* to do — so it is worth being exact about the size of that: a point
/// moves by at most what the panel's own noise is, and a point that moves less than a fifth of a pixel
/// is not a point anybody can see. What the *ends* do is stay put: the first and last readings of a
/// stroke are drawn where they were written, because where a line starts and stops is the part of it a
/// reader is most likely to be looking at.
///
/// The one reader who would want this off is one writing deliberately at the scale of a fraction of a
/// pixel — deep zoom, drawing detail a fifth of a pixel wide — and the answer for them is to turn the
/// number above to zero rather than to add a row of settings for it.
///
/// ## Where it applies
///
/// Only where the readings are close enough together for the average to be a *steadying* rather than a
/// redraw: see [`STEADY_WITHIN`]. A stroke written at reading size is made of points under a pixel
/// apart, and every interior point of it is steadied; a flick is made of points twenty pixels apart,
/// and none of it is.
fn steadied(points: &[InkPoint]) -> Vec<InkPoint> {
    if STEADYING == 0 || points.len() < 3 {
        return points.to_vec();
    }

    let mut line = points.to_vec();
    // The distance between two readings, which is what decides whether they are close enough for a
    // pass to be a steadying rather than a redraw.
    let spacing = |from: InkPoint, to: InkPoint| {
        ((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt()
    };

    for _ in 0..STEADYING {
        let mut pass = line.clone();
        // Every point that has a reading on either side of it: a stroke's ends are its own.
        for index in 1..line.len() - 1 {
            let (before, here, after) = (line[index - 1], line[index], line[index + 1]);

            if spacing(before, here).max(spacing(here, after)) > STEADY_WITHIN {
                continue;
            }

            pass[index] = InkPoint::new(
                (before.x + here.x * 2.0 + after.x) * 0.25,
                (before.y + here.y * 2.0 + after.y) * 0.25,
                (before.width + here.width * 2.0 + after.width) * 0.25,
            );
        }
        line = pass;
    }

    line
}

/// The line a stroke is drawn as: the points it holds, with a curve between each pair of them.
///
/// The stored points are a *sampling* of the line the pen drew — the digitizer reports far more
/// positions than a stroke keeps (see [`Settings::resample_spacing`]) — and what is left of them was
/// joined with straight edges. At 1:1 the corners between those edges are sub-pixel and invisible,
/// and a zoom magnifies the edges *and* the corners: this is where the line the pen actually drew is
/// put back together from the samples of it that were kept.
///
/// The curve is a **centripetal** Catmull-Rom, and both halves of that matter:
///
/// * It **passes through** every point it is given, so the curve itself moves nothing: the line is
///   reconstructed *between* the points, and where a point is drawn is where it is. (The points it is
///   given are the readings with a share of the digitizer's noise taken out of the ones in the middle
///   of the stroke — see [`steadied`], which is the one place in this path that moves ink at all, and
///   by less than a fifth of a pixel.)
/// * Its knots are the **square root** of the distance between two points. A uniform
///   parameterisation loops and overshoots where the spacing changes, and a stroke's spacing *is* the
///   pen's speed — so a hand that accelerated mid-stroke would be drawn with a curl in it.
///
/// Between the points the line is *rounded*, and at a sharp corner that rounding leaves the corner by
/// a fraction of the spacing between the points there: a pen that turned a corner at speed has its
/// corner softened, which is what a fast hand looks like, and at the pace a note is written the
/// spacing is under a pixel so the softening is too.
///
/// Each segment is cut into pieces no longer than [`FACET`] window pixels — at most [`MAX_STEPS`] of
/// them — and the width is interpolated along the pieces. A segment short enough to need one piece is
/// *copied* rather than sampled, so at 1:1 the geometry is exactly the geometry the stored points
/// describe, and writing at reading size costs nothing for any of this.
fn densified(points: &[InkPoint], detail: f32, tip: Tip) -> Vec<InkPoint> {
    if points.len() < 2 {
        return points.to_vec();
    }

    let points = &steadied(points);
    let last = points.len() - 1;
    let mut line = Vec::with_capacity(points.len());
    line.push(points[0]);

    for index in 0..last {
        let from = points[index];
        let to = points[index + 1];

        // The one exception, and only while the pen is down.
        let curved = tip == Tip::Curved || index + 1 < last;

        // A Catmull-Rom draws a segment from the points on either side of it, and the first and last
        // segments have none on one of them. The end point is doubled instead, which halves the
        // tangent there: such a segment is drawn as the line it is, which is the honest thing for a
        // nib that began or stopped at that very point.
        let before = points[index.saturating_sub(1)];
        let after = points[(index + 2).min(last)];

        let steps = steps_for(from, to, detail);
        if steps == 1 {
            line.push(to);
            continue;
        }

        for step in 1..steps {
            let t = step as f32 / steps as f32;
            let (x, y) = if curved {
                catmull_rom(before, from, to, after, t)
            } else {
                (from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t)
            };
            line.push(InkPoint::new(x, y, from.width + (to.width - from.width) * t));
        }

        // The end of a segment is the point itself rather than the curve's value at the end of it:
        // those differ by rounding, and where a stroke's ink ends is not a place to be a few
        // floating-point steps away from where the pen was.
        line.push(to);
    }

    line
}

/// Builds the filled ribbon outline of a line with a per-point width.
///
/// The outline walks the line on both sides — the left offset side forward, the right offset
/// side backward — and closes the polygon. Filling that polygon is what gives a stroke a
/// width that varies with the pen's force; a constant-width stroke would need only a polyline.
///
/// The line walked is the *drawn* one rather than the stored one: [`densified`] has already put a
/// curve through the points that were kept and cut it into pieces short enough for `detail` — the
/// zoom — to be drawn from. The direction through a point is the difference of the points on either
/// side of it, which is what keeps a sharp corner from pinching the ribbon, and on an interpolated
/// line those points are a fraction of a pixel away: the two edges follow the curve the pen drew
/// instead of the corners of the sampling of it.
fn ribbon_outline(points: &[InkPoint], detail: f32, tip: Tip) -> Vec<[f32; 2]> {
    /// The thinnest a stroke is drawn, so a hairline reading still leaves a mark.
    const MIN_HALF_WIDTH: f32 = 0.35;

    if points.is_empty() {
        return Vec::new();
    }

    if points.len() == 1 {
        // A dot: a small polygon standing in for a filled circle.
        const SEGMENTS: usize = 12;
        let point = points[0];
        let radius = (point.width * 0.5).max(MIN_HALF_WIDTH);
        return (0..SEGMENTS)
            .map(|step| {
                let angle = step as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
                [point.x + radius * angle.cos(), point.y + radius * angle.sin()]
            })
            .collect();
    }

    let line = densified(points, detail, tip);
    let last = line.len() - 1;
    let mut left = Vec::with_capacity(line.len());
    let mut right = Vec::with_capacity(line.len());

    for index in 0..line.len() {
        let point = line[index];
        let previous = line[index.saturating_sub(1)];
        let next = line[(index + 1).min(last)];

        let (across_x, across_y) = normal_through(previous, point, next);
        let half_width = (point.width * 0.5).max(MIN_HALF_WIDTH);

        left.push([
            point.x + across_x * half_width,
            point.y + across_y * half_width,
        ]);
        right.push([
            point.x - across_x * half_width,
            point.y - across_y * half_width,
        ]);
    }

    left.extend(right.into_iter().rev());
    left
}

/// The unit normal of the line as it passes through a point: the direction *across* it, from the
/// points on either side.
///
/// A point with no direction of its own — a pen that came back to the place it was, or a reading
/// doubled by a lift — is the case worth spelling out, because the cheap answer is wrong in a way
/// that shows. Offsetting across an axis picked out of the air lays the two edges of the ribbon
/// *along* the line instead of across it, pinching the stroke to nothing exactly where the user
/// doubled back over their own ink. The direction the ink was drawn in is the honest one, so such a
/// point is offset across the segment it arrived on, and a point whose whole neighbourhood is one
/// place gets a horizontal line across it, which is as good as any.
fn normal_through(previous: InkPoint, point: InkPoint, next: InkPoint) -> (f32, f32) {
    /// The shortest direction that is a direction at all, in logical pixels.
    const FLOOR: f32 = 1e-6;

    // The unit normal of a direction: the direction across the line.
    let across = |dx: f32, dy: f32| -> Option<(f32, f32)> {
        let length = (dx * dx + dy * dy).sqrt();
        if length <= FLOOR {
            None
        } else {
            Some((-dy / length, dx / length))
        }
    };

    across(next.x - previous.x, next.y - previous.y)
        .or_else(|| across(point.x - previous.x, point.y - previous.y))
        .or_else(|| across(next.x - point.x, next.y - point.y))
        .unwrap_or((0.0, 1.0))
}

/// How many straight pieces one segment of a stroke is drawn as.
///
/// The segment is measured *after* the zoom, because that is what decides whether a piece of it is
/// visible, and rounded up so that a piece is never longer than [`FACET`] — with a bound at each end:
/// never fewer than one, because a segment is a piece of the line at the very least, and never more
/// than [`MAX_STEPS`].
fn steps_for(from: InkPoint, to: InkPoint, detail: f32) -> usize {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let window_pixels = (dx * dx + dy * dy).sqrt() * detail;

    let steps = (window_pixels / FACET).ceil();
    if !steps.is_finite() || steps < 1.0 {
        1
    } else {
        (steps as usize).min(MAX_STEPS)
    }
}

/// The knot spacing between two points: the square root of the distance between them.
///
/// The square root is the whole of the "centripetal" in the curve this file draws: the curve is
/// parameterised by the *root* of the distance rather than by the distance itself (chordal) or by
/// nothing at all (uniform), which is what keeps it from looping where a hand changes speed. The
/// floor keeps the arithmetic finite where two readings landed on the same place — a pen held still,
/// or the doubled point of a lift — because a knot of zero is a knot to divide by.
fn knot(from: InkPoint, to: InkPoint) -> f32 {
    /// The shortest knot spacing, in logical pixels.
    const FLOOR: f32 = 0.01;

    let dx = to.x - from.x;
    let dy = to.y - from.y;
    (dx * dx + dy * dy).sqrt().sqrt().max(FLOOR)
}

/// The point at `t` (`0.0` at `from`, `1.0` at `to`) of the centripetal Catmull-Rom curve through
/// four consecutive points.
///
/// Evaluated by the recursive form — each step is the weighted average of the two points it is drawn
/// between, over the span their own knots describe — which is the form that takes an uneven knot
/// spacing without being rewritten for it. A span can extrapolate where the spacing changes sharply,
/// and that is precisely what a Catmull-Rom's rounded corner is: see [`densified`].
fn catmull_rom(
    before: InkPoint,
    from: InkPoint,
    to: InkPoint,
    after: InkPoint,
    t: f32,
) -> (f32, f32) {
    let t0 = 0.0;
    let t1 = t0 + knot(before, from);
    let t2 = t1 + knot(from, to);
    let t3 = t2 + knot(to, after);

    // Where in the middle span the sample falls.
    let t = t1 + (t2 - t1) * t;

    // The average of two points over the span between two knots.
    let average = |a: (f32, f32), b: (f32, f32), span: (f32, f32)| {
        let forward = (t - span.0) / (span.1 - span.0);
        (a.0 + (b.0 - a.0) * forward, a.1 + (b.1 - a.1) * forward)
    };
    let at = |point: InkPoint| (point.x, point.y);

    let first = average(at(before), at(from), (t0, t1));
    let middle = average(at(from), at(to), (t1, t2));
    let last = average(at(to), at(after), (t2, t3));

    let low = average(first, middle, (t0, t2));
    let high = average(middle, last, (t1, t3));

    average(low, high, (t1, t2))
}

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
    /// The strokes taken back by [`Self::undo`], newest last, waiting for [`Self::redo`].
    ///
    /// It holds strokes by pointer, like [`Self::finished`] does, so the two lists are one ink
    /// between them and undoing a page of a thousand strokes copies no strokes at all. It is emptied
    /// by anything that makes the drawing a *new* one rather than a shortened one: a finished
    /// stroke, an erase that removed something, and a clear.
    undone: Vec<Arc<Stroke>>,
    /// The stroke currently being drawn, if a pen nib is down.
    open: Option<Stroke>,
    /// The pointer that owns the turn: the pen or eraser that is currently down.
    active_pointer: Option<u32>,
    /// Which tool [`Self::active_pointer`] is using.
    active_tool: Tool,
    /// The tool the toolbar has selected.
    ///
    /// This is the fallback when the pen reports nothing about itself: a pen whose eraser end
    /// is toward the screen always erases, and this decides what an ordinary nib does.
    mode: Tool,
    /// The last reading consumed, for the time step a smoother needs.
    last_sample: Option<PenSample>,
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
    /// What the model has done since the app started.
    stats: InkStats,
}

impl Default for InkDocument {
    fn default() -> Self {
        InkDocument {
            finished: Arc::new(Vec::new()),
            undone: Vec::new(),
            open: None,
            active_pointer: None,
            active_tool: Tool::Pen,
            mode: Tool::Pen,
            last_sample: None,
            last_erase: None,
            detailed_at: None,
            stats: InkStats::default(),
        }
    }
}

impl InkDocument {
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

    /// The tool the toolbar has selected.
    pub fn mode(&self) -> Tool {
        self.mode
    }

    /// Selects the tool an ordinary nib uses.
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

    /// The last reading consumed, whatever it did.
    ///
    /// This is the reading *as the pen reported it* — physical pixels, raw tilt — and it is kept
    /// for anything that has to follow the pen rather than draw with it, such as the ghost cursor.
    /// Unlike the ink, it is also updated by the phases that lay nothing: hovering, entering, and
    /// leaving are all positions the cursor has to know about.
    pub fn last_sample(&self) -> Option<PenSample> {
        self.last_sample
    }

    /// Whether the page has no ink at all.
    pub fn is_blank(&self) -> bool {
        self.finished.is_empty() && self.open.is_none()
    }

    /// How many strokes the page holds, finished or open.
    pub fn stroke_count(&self) -> usize {
        self.finished.len() + usize::from(self.open.is_some())
    }

    /// Removes the most recent finished stroke, and nothing else.
    ///
    /// The stroke the pen is still drawing is deliberately **not** touched: it is not history yet —
    /// it has no closed geometry and it is not what a save would write — and the pen is still on the
    /// paper, so taking it away would both lose a line the user meant to draw and leave the model
    /// believing the pen was lifted. It is finished, or lifted, on its own.
    ///
    /// Returns whether there was a stroke to take back. A caller uses that to decide whether to
    /// repaint, and the interface uses [`Self::can_undo`] to say so *before* it is asked.
    pub fn undo(&mut self) -> bool {
        let Some(stroke) = Arc::make_mut(&mut self.finished).pop() else {
            return false;
        };

        self.undone.push(stroke);
        // The eraser's cheap path skips a rescan when the nib has barely moved, and the strokes
        // under it have just changed. Forgetting the last position costs one rescan.
        self.last_erase = None;
        true
    }

    /// Puts back the stroke the most recent [`Self::undo`] took away.
    ///
    /// Nothing is undone by undoing: the stroke comes back exactly as it was drawn, because undo
    /// moved a pointer rather than copying ink. The redo list is emptied by any new ink, so this can
    /// never put a stroke back into a drawing it does not belong to.
    pub fn redo(&mut self) -> bool {
        let Some(stroke) = self.undone.pop() else {
            return false;
        };

        Arc::make_mut(&mut self.finished).push(stroke);
        self.last_erase = None;
        true
    }

    /// Whether [`Self::undo`] would do anything.
    ///
    /// The interface asks this rather than pressing the button to find out: a command that is
    /// offered and then does nothing is indistinguishable from one that is broken.
    pub fn can_undo(&self) -> bool {
        !self.finished.is_empty()
    }

    /// Whether [`Self::redo`] would do anything.
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Removes every stroke.
    ///
    /// Both directions are cleared: this is the command that says the page holds nothing, and a redo
    /// list left behind it would be able to put ink back onto a page that was just emptied.
    pub fn clear(&mut self) {
        self.finished = Arc::new(Vec::new());
        self.undone.clear();
        self.open = None;
        self.active_pointer = None;
        self.last_erase = None;
    }

    /// A page holding strokes that came from somewhere else — a saved note, or the clipboard of a
    /// future version — with the caches [`Stroke::close`] computes rebuilt.
    ///
    /// Rebuilding is not optional: a stroke's bounds and outline are `#[serde(skip)]`, because they
    /// are derivable from its points, and they are what the eraser hit-tests against and what a
    /// frame culls by. A page loaded without them would draw, and would refuse to be erased.
    pub fn from_strokes(strokes: Vec<Stroke>) -> Self {
        let finished = strokes
            .into_iter()
            .map(|mut stroke| {
                stroke.close();
                Arc::new(stroke)
            })
            .collect::<Vec<_>>();

        InkDocument {
            finished: Arc::new(finished),
            // Built at 1:1, which is what [`Stroke::close`] details a stroke for. A sheet drawn larger
            // refines them, and the frame that draws it is what knows how large that is: see
            // [`Self::set_zoom`].
            detailed_at: Some(detail_zoom(1.0)),
            ..InkDocument::default()
        }
    }

    /// The detail this page's outlines are built for, for a page nothing has detailed yet.
    ///
    /// 1:1 is the answer for a page whose reader has not said otherwise, and it is also what an
    /// outline built outside a window — by a load, or by a test — is detailed for.
    fn detail(&self) -> f32 {
        self.detailed_at.unwrap_or_else(|| detail_zoom(1.0))
    }

    /// Rebuilds the derived geometry of the page for a sheet drawn at `zoom`, returning whether
    /// anything was rebuilt.
    ///
    /// A stroke's outline depends on the zoom it is being drawn at: not its *shape*, which depends on
    /// its points alone, but how finely the curves in it are cut (see [`FACET`]). A page of ink is far
    /// too much to re-derive per frame — and a zoom *is* per frame, while a pinch is on — so what is
    /// tracked is the detail rather than the zoom: it moves in rungs (see [`detail_zoom`]), so a
    /// gesture across the whole range costs a handful of rebuilds, and each one happens on the frame
    /// the rung changes rather than on every frame in between.
    ///
    /// What it does not touch is the ink. The points, the colours, and what the eraser hit-tests are
    /// exactly what was written; the outline and the bounds are derived from them, and derived again
    /// whenever the detail they were built for stops being the one on screen.
    ///
    /// The stroke under the nib is not this page's business: a frame closes the stroke it is drawing
    /// itself, at the zoom it is drawing (see [`Stroke::close_live`]).
    pub fn set_zoom(&mut self, zoom: f32) -> bool {
        let detail = detail_zoom(zoom);
        if self.detailed_at == Some(detail) {
            return false;
        }

        for stroke in Arc::make_mut(&mut self.finished) {
            Arc::make_mut(stroke).freeze(detail, Tip::Curved);
        }

        self.detailed_at = Some(detail);
        true
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
        samples: &[PenSample],
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
                PenPhase::Down => {
                    // A `Down` closes whatever the previous pointer left open: the ink the user
                    // drew is kept, and an `Up` that never arrived is no reason to lose it.
                    self.finish_open();

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
                        let tool = if self.mode == Tool::Eraser || sample.eraser || sample.inverted {
                            Tool::Eraser
                        } else {
                            Tool::Pen
                        };
                        self.active_pointer = Some(sample.id);
                        self.active_tool = tool;

                        match tool {
                            Tool::Eraser => {
                                self.last_erase = None;
                                self.erase_at(x, y, settings);
                            }
                            Tool::Pen => {
                                let width = settings.width_for_pressure(sample.applied_pressure());
                                // The pen in hand is stamped into the stroke: it is chosen at the
                                // moment the nib goes down, and it stays with that line for good.
                                self.open = Some(Stroke::new(
                                    InkPoint::new(x, y, width),
                                    settings.ink_color,
                                ));
                            }
                        }

                        changed = true;
                    }
                }

                PenPhase::Move => {
                    if self.active_pointer == Some(sample.id) {
                        if !on_paper {
                            self.stats.off_paper += 1;
                        }
                        match self.active_tool {
                            Tool::Eraser => self.erase_at(x, y, settings),
                            Tool::Pen => self.push_point(x, y, sample, settings, false),
                        }
                        changed = true;
                    }
                }

                PenPhase::Up => {
                    if self.active_pointer == Some(sample.id) {
                        if !on_paper {
                            self.stats.off_paper += 1;
                        }
                        if self.active_tool == Tool::Pen {
                            // The lift is where the pen left the paper, so it is the stroke's
                            // last point regardless of what the resampler would prefer.
                            self.push_point(x, y, sample, settings, true);
                        }
                        self.finish_open();
                        changed = true;
                    }
                }

                PenPhase::Cancel | PenPhase::Leave => {
                    if self.active_pointer == Some(sample.id) {
                        // The position these phases carry is not ink: the stroke ends wherever
                        // it actually got to.
                        self.finish_open();
                        changed = true;
                    }
                }

                // Hover readings move a preview cursor but never lay ink.
                PenPhase::Idle | PenPhase::Enter | PenPhase::Hover => {}
            }

            self.last_sample = Some(*sample);
        }

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
                stroke.freeze(self.detail(), Tip::Curved);
                // `make_mut` reuses the existing vector when no frame is holding a snapshot, and
                // copies it when one is — and that copy is of pointers, not of ink.
                Arc::make_mut(&mut self.finished).push(Arc::new(stroke));

                // A stroke drawn after an undo makes the drawing a new one rather than a shortened
                // one, so what was taken back is no longer something to put forward. Emptied here
                // rather than when the pen went down: a `Down` whose stroke never became a line is
                // not an edit at all.
                self.undone.clear();
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
    fn alpha(&self, sample: &PenSample, settings: &Settings) -> f32 {
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

    /// Adds a point to the open stroke, after smoothing and resampling.
    ///
    /// `force` bypasses the resampler and is used for the lift, which is the stroke's end: a
    /// distance filter that applied its own rule to the last point would shorten every stroke
    /// by up to one spacing.
    fn push_point(
        &mut self,
        x: f32,
        y: f32,
        sample: &PenSample,
        settings: &Settings,
        force: bool,
    ) {
        let alpha = self.alpha(sample, settings);

        let Some(stroke) = self.open.as_mut() else {
            return;
        };

        let (target_x, target_y) = match (stroke.points.last(), alpha < 1.0) {
            (Some(last), true) => (
                last.x + (x - last.x) * alpha,
                last.y + (y - last.y) * alpha,
            ),
            _ => (x, y),
        };

        if !force {
            if let Some(last) = stroke.points.last() {
                let dx = last.x - target_x;
                let dy = last.y - target_y;
                if dx * dx + dy * dy < settings.resample_spacing * settings.resample_spacing {
                    self.stats.resampled += 1;
                    return;
                }
            }
        }

        // The lift reports no applied force (`applied_pressure()` is `None` off the surface),
        // so an existing point's width is reused rather than letting the stroke change width
        // in its last pixel.
        let width = match sample.applied_pressure() {
            Some(_) => settings.width_for_pressure(sample.applied_pressure()),
            None => stroke
                .points
                .last()
                .map(|point| point.width)
                .unwrap_or_else(|| settings.width_for_pressure(None)),
        };

        stroke.points.push(InkPoint::new(target_x, target_y, width));

        // Kept up to date as the stroke grows, rather than only when it closes: a frame asks
        // whether the stroke being drawn is on screen before it has ever been closed.
        stroke.bounds[0] = stroke.bounds[0].min(target_x);
        stroke.bounds[1] = stroke.bounds[1].min(target_y);
        stroke.bounds[2] = stroke.bounds[2].max(target_x);
        stroke.bounds[3] = stroke.bounds[3].max(target_y);

        self.stats.kept_points += 1;
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

        // Two passes, and the order is the whole point of them. The first is a bounds test per
        // stroke — a handful of comparisons — and it answers the question the drag asks most of the
        // time: *nothing* is under the nib. Only when something is does the second pass run at all,
        // so the common case costs no copying.
        if !self
            .finished
            .iter()
            .any(|stroke| stroke.hits(x, y, settings.erase_radius))
        {
            return;
        }

        let before = self.finished.len();
        Arc::make_mut(&mut self.finished).retain(|stroke| !stroke.hits(x, y, settings.erase_radius));
        let erased = before - self.finished.len();

        if erased > 0 {
            self.stats.erased_strokes += erased as u64;
            // Erasing is a change of its own — the page it leaves is not the page undo takes back
            // to — so it ends the redo branch exactly as new ink does.
            self.undone.clear();
        }
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
}

impl Default for Notes {
    /// A note with one blank page, drawn at the size its paper asks for.
    fn default() -> Self {
        Notes {
            page: 0,
            current: InkDocument::blank(),
            taken: BTreeMap::new(),
            zoom: 1.0,
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

    /// Tells the note what the sheet is drawn at, so the page in front of the reader is detailed for
    /// it — and so is the ink of a page turned to afterwards.
    ///
    /// Returns whether the page being drawn had to be rebuilt, which is what a caller uses to decide
    /// whether a frame is worth scheduling. The detail moves in rungs (see
    /// [`InkDocument::set_zoom`]), so a pinch or a wheel across the whole zoom range costs a handful
    /// of rebuilds rather than one per frame — and none at all while the zoom stays in one rung.
    pub fn set_zoom(&mut self, zoom: f32) -> bool {
        self.zoom = zoom;
        self.current.set_zoom(zoom)
    }

    /// Moves to a page, taking the ink of the page being left behind with it.
    ///
    /// The page being moved to keeps its own ink, which is the whole point: what was drawn there is
    /// still there on the way back.
    pub fn go_to(&mut self, page: usize) {
        if page == self.page {
            return;
        }

        let leaving = std::mem::take(&mut self.current);
        // A page left with nothing on it is normally dropped — an empty document costs nothing to
        // make again — but one that can still be *redone* is not empty in the sense that matters
        // here: the strokes are gone from the page and still in the model, and a page turn is not an
        // edit. Keeping it is what makes undo, turn the page, turn back, redo work.
        if !leaving.is_blank() || leaving.can_redo() {
            self.taken.insert(self.page, leaving);
        }

        self.current = self.taken.remove(&page).unwrap_or_else(InkDocument::blank);
        // A page turned back to holds the ink it did, and its outlines were detailed for the sheet as
        // it was drawn when the reader left it. One pass, and only when the detail has moved since.
        self.current.set_zoom(self.zoom);
        self.page = page;
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
            self.current = ink;
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
            self.current = self
                .taken
                .remove(&(page + 1))
                .unwrap_or_else(InkDocument::blank);
            // The page that follows a deleted one was detailed for the sheet as it was drawn when the
            // reader last had it, which need not be the size it is drawn at now.
            self.current.set_zoom(self.zoom);
        } else if page < self.page {
            self.page -= 1;
        }

        self.taken = std::mem::take(&mut self.taken)
            .into_iter()
            .map(|(index, ink)| (if index > page { index - 1 } else { index }, ink))
            .collect();
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use pen_windows::Point;

    /// A reading at a position, in physical client pixels, with the given phase.
    fn reading(id: u32, phase: PenPhase, x: f32, y: f32, pressure: Option<f32>) -> PenSample {
        PenSample {
            phase,
            id,
            pixel: Point::new(x, y),
            pressure,
            ..PenSample::default()
        }
    }

    /// A settings value with no smoothing, so positions are exactly the readings'.
    fn settings() -> Settings {
        Settings {
            smoothing_ms: 0.0,
            ..Settings::default()
        }
    }

    /// The transform for a test with no window: the reading's own pixels.
    fn id() -> InkTransform {
        InkTransform::identity()
    }

    /// A transform with a DPI scale and no zoom, for the tests about the conversion.
    fn scaled(scale: f32) -> InkTransform {
        InkTransform {
            scale,
            ..InkTransform::identity()
        }
    }

    /// A transform with a zoom and an offset: a sheet drawn inside a window.
    fn zoomed(zoom: f32, origin: (f32, f32)) -> InkTransform {
        InkTransform {
            zoom,
            origin,
            ..InkTransform::identity()
        }
    }

    /// A transform for a sheet that is *paper*: A4, at 1:1, with its edges where the page's are.
    fn papered() -> InkTransform {
        InkTransform {
            paper: (595.0, 842.0),
            ..InkTransform::identity()
        }
    }

    /// That paper with the bar in front of it: the bar reaches down `bar` logical pixels of the
    /// window, so the page it hides is from the top of the window to that line.
    fn barred(bar: f32) -> InkTransform {
        InkTransform {
            bar,
            ..papered()
        }
    }

    /// A curved stroke as the resampler leaves one: points a little under a pixel apart, walking a
    /// quarter of a circle.
    ///
    /// A hand writing at reading size produces exactly this — the digitizer's readings are dropped
    /// until they are `resample_spacing` apart, and what survives follows the curve. It is the case a
    /// zoom is unkind to: three quarters of a logical pixel of straight edge is invisible at 1:1 and
    /// twelve pixels of it at 16x, with a corner at each end of every piece.
    fn curve() -> Stroke {
        /// The radius of the arc, in logical pixels.
        const RADIUS: f32 = 60.0;
        /// How far apart the points are: the resampler's default spacing.
        const SPACING: f32 = 0.75;
        /// How many points to walk.
        const POINTS: usize = 120;

        let step = SPACING / RADIUS;
        let mut stroke = Stroke::new(InkPoint::new(RADIUS, 0.0, 2.0), Stroke::DEFAULT_COLOR);
        for index in 1..POINTS {
            let angle = index as f32 * step;
            stroke
                .points
                .push(InkPoint::new(RADIUS * angle.cos(), RADIUS * angle.sin(), 2.0));
        }
        stroke
    }

    /// A stroke drawn fast: points twenty logical pixels apart, which is what a flick leaves when the
    /// hand outruns the readings.
    fn flick() -> Stroke {
        let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, 2.0), Stroke::DEFAULT_COLOR);
        for index in 1..24 {
            let along = index as f32 * 20.0;
            stroke.points.push(InkPoint::new(
                along,
                along * 0.4 + (along * 0.2).sin() * 6.0,
                2.0,
            ));
        }
        stroke
    }

    /// A stroke that comes back down over itself: the case where the readings on either side of a
    /// point are the same place, and the point therefore has no direction of its own.
    fn doubled_back() -> Stroke {
        let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, 4.0), Stroke::DEFAULT_COLOR);
        for index in 1..=20 {
            stroke.points.push(InkPoint::new(0.0, index as f32, 4.0));
        }
        for index in 1..=20 {
            stroke.points.push(InkPoint::new(0.0, 20.0 - index as f32, 4.0));
        }
        stroke
    }

    /// Two long segments meeting at a right angle, so the curve through the last of them bulges where
    /// the straight line between its points does not.
    fn turned() -> Stroke {
        let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, 2.0), Stroke::DEFAULT_COLOR);
        stroke.points.push(InkPoint::new(40.0, 0.0, 2.0));
        stroke.points.push(InkPoint::new(40.0, 40.0, 2.0));
        stroke
    }

    /// The outline a frame draws for this stroke at this zoom.
    fn drawn(stroke: &Stroke, zoom: f32) -> Vec<[f32; 2]> {
        let mut copy = stroke.clone();
        copy.close_at(zoom);
        copy.outline
    }

    /// The outline a frame drew *before* the pieces were interpolated: one straight edge per segment,
    /// which is what a zoom magnifies into a chain of straight pieces.
    ///
    /// The detail of `0.0` is what asks for exactly one piece a segment (see [`steps_for`]), so this is
    /// the geometry this change is measured against — built by the same code that draws the new one,
    /// rather than by a copy of the code it replaced.
    fn drawn_flat(stroke: &Stroke) -> Vec<[f32; 2]> {
        ribbon_outline(&stroke.points, 0.0, Tip::Curved)
    }

    /// The arc [`curve`] walks, written by a hand whose digitizer is a little noisy: every reading is
    /// off the line by up to a fifth of a pixel, in a direction of its own.
    ///
    /// This is what nearly every panel does, and at 1:1 it is invisible — a fifth of a pixel is not a
    /// place an eye can see — while at 16x it is three pixels of wander along the edge of a line.
    fn wandering() -> Stroke {
        let mut stroke = curve();
        let mut seed = 0x2545_F491u32;
        let mut jitter = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (((seed >> 8) & 0xFFFF) as f32 / 65_535.0 - 0.5) * 0.4
        };

        for point in &mut stroke.points {
            point.x += jitter();
            point.y += jitter();
        }
        stroke
    }

    /// The farthest any of these points is from the arc of this radius about the origin, times the
    /// zoom: how far the ink wanders from the line the hand meant, in window pixels on screen.
    fn wander(points: &[InkPoint], radius: f32, zoom: f32) -> f32 {
        points
            .iter()
            .map(|point| ((point.x * point.x + point.y * point.y).sqrt() - radius).abs())
            .fold(0.0, f32::max)
            * zoom
    }

    /// The longest edge of one side of an outline, in the outline's own (sheet) units.
    fn longest_edge(side: &[[f32; 2]]) -> f32 {
        (0..side.len().saturating_sub(1))
            .map(|index| {
                let [x0, y0] = side[index];
                let [x1, y1] = side[index + 1];
                ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt()
            })
            .fold(0.0, f32::max)
    }

    /// The sharpest turn between two consecutive edges of one side, in degrees — the corner a piece
    /// leaves at the end of the piece before it, which is what the eye reads as a wobble.
    fn sharpest_turn(side: &[[f32; 2]]) -> f32 {
        let mut sharpest: f32 = 0.0;
        for index in 1..side.len().saturating_sub(1) {
            let along =
                |from: usize, to: usize| (side[to][0] - side[from][0], side[to][1] - side[from][1]);
            let (before, after) = (along(index - 1, index), along(index, index + 1));
            let turn = (before.0 * after.1 - before.1 * after.0)
                .atan2(before.0 * after.0 + before.1 * after.1)
                .to_degrees()
                .abs();
            sharpest = sharpest.max(turn);
        }
        sharpest
    }

    /// The distance from a point to the nearest edge of an outline.
    fn distance_to_outline(point: [f32; 2], outline: &[[f32; 2]]) -> f32 {
        let mut nearest = f32::MAX;
        for index in 0..outline.len() {
            let from = outline[index];
            let to = outline[(index + 1) % outline.len()];
            let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
            let length_squared = dx * dx + dy * dy;
            let along = if length_squared <= f32::EPSILON {
                0.0
            } else {
                (((point[0] - from[0]) * dx + (point[1] - from[1]) * dy) / length_squared)
                    .clamp(0.0, 1.0)
            };
            let (x, y) = (from[0] + along * dx, from[1] + along * dy);
            nearest = nearest.min(((point[0] - x).powi(2) + (point[1] - y).powi(2)).sqrt());
        }
        nearest
    }

    /// How far the middle of a run of points leaves the straight line between its two ends.
    fn bulge(from: InkPoint, to: InkPoint, run: &[InkPoint]) -> f32 {
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        let length = (dx * dx + dy * dy).sqrt();

        run.iter()
            .map(|point| ((point.x - from.x) * dy - (point.y - from.y) * dx).abs() / length)
            .fold(0.0, f32::max)
    }

    /// The side of the edge from `(x0, y0)` to `(x1, y1)` that `(x, y)` is on: positive is the left.
    fn side_of(x0: f32, y0: f32, x1: f32, y1: f32, x: f32, y: f32) -> f32 {
        (x1 - x0) * (y - y0) - (x - x0) * (y1 - y0)
    }

    /// Whether a filled outline covers a point — the nonzero rule the fill uses, which is the rule the
    /// ribbon was drawn for (see `crate::app::solid_path`).
    ///
    /// A winding count rather than a crossing count: the ribbon crosses itself wherever the pen did,
    /// and two crossings of the same direction are ink rather than a hole.
    fn covers(outline: &[[f32; 2]], x: f32, y: f32) -> bool {
        let mut winding = 0i32;
        for index in 0..outline.len() {
            let [x0, y0] = outline[index];
            let [x1, y1] = outline[(index + 1) % outline.len()];

            if y0 <= y {
                if y1 > y && side_of(x0, y0, x1, y1, x, y) > 0.0 {
                    winding += 1;
                }
            } else if y1 <= y && side_of(x0, y0, x1, y1, x, y) < 0.0 {
                winding -= 1;
            }
        }

        winding != 0
    }

    /// The detail a stroke is drawn with moves in rungs, so a pinch across the whole range rebuilds a
    /// page a handful of times rather than once per frame — and a rung is stable when it is handed
    /// back to itself, which is what keeps a rebuild from happening on every frame of a gesture.
    #[test]
    fn the_detail_moves_in_rungs() {
        assert_eq!(detail_zoom(1.0), 1.0);
        assert_eq!(detail_zoom(1.5), 1.5);

        // A zoom inside a rung is drawn at the top of the rung: never worse than the zoom asks for, and
        // the same detail for every zoom in it.
        assert_eq!(detail_zoom(1.2), detail_zoom(1.4), "one rung, one detail");
        assert!(detail_zoom(1.2) >= 1.4, "rounded up, not down");

        for rung in [1.0f32, 1.5, 2.25, 5.0625, 11.390_625, 17.085_938] {
            assert!(
                (detail_zoom(rung) - rung).abs() < 1e-3,
                "a rung is its own detail: {rung} -> {}",
                detail_zoom(rung)
            );
        }

        assert_eq!(detail_zoom(0.0), 1.0, "a window with no zoom draws at 1:1");
        assert_eq!(detail_zoom(f32::NAN), 1.0);
    }

    /// At 1:1 nothing is interpolated: a stroke written at reading size is drawn from exactly the
    /// points it came from, which is what keeps writing at reading size as cheap as it ever was — and
    /// its geometry what the pen produced.
    #[test]
    fn the_detail_costs_nothing_at_one_to_one() {
        let mut stroke = curve();
        let points = stroke.points.len();

        stroke.close_at(1.0);

        assert_eq!(
            stroke.outline.len(),
            points * 2,
            "a vertex a side and no pieces added: a point is still one straight edge"
        );
    }

    /// A zoom buys detail and not shape: the same line, cut finer. Every piece gets shorter and no
    /// vertex moves to anywhere it was not already.
    #[test]
    fn a_zoom_buys_detail_and_not_shape() {
        let stroke = curve();
        let coarse = drawn(&stroke, 1.0);
        let fine = drawn(&stroke, 16.0);

        assert!(
            fine.len() > coarse.len() * 3,
            "16x is cut much finer: {} vertices against {}",
            fine.len(),
            coarse.len()
        );

        for vertex in &coarse {
            let moved = distance_to_outline(*vertex, &fine);
            assert!(moved <= 0.25, "the line moved: {vertex:?} is {moved} away");
        }

        let longest = longest_edge(&fine[..fine.len() / 2]);
        assert!(
            longest <= FACET * 1.25 / 16.0,
            "the longest piece of a 16x line is {longest} logical pixels"
        );
    }

    /// The newest segment of a stroke under the nib is the line between its points and not a curve: a
    /// curve through the tip needs the reading *after* it, and one drawn without it would move ink the
    /// user has already seen, under the pen, as they write. The stroke the pen has left is curved to
    /// its very end.
    #[test]
    fn a_live_stroke_is_straight_at_its_tip() {
        let stroke = turned();
        let detail = detail_zoom(16.0);
        // The line as it is drawn: the points the geometry is built from, which is the readings with
        // the steadier's pass over them (see [`steadied`]).
        let points = steadied(&stroke.points);
        let (from, to) = (points[1], points[2]);

        let steps = steps_for(from, to, detail);
        assert!(steps > 4, "the test needs a segment cut into pieces: {steps}");

        let live = densified(&points, detail, Tip::Straight);
        let finished = densified(&points, detail, Tip::Curved);
        let live_tip = &live[live.len() - steps..];
        let finished_tip = &finished[finished.len() - steps..];

        let straight = bulge(from, to, live_tip);
        assert!(straight < 1e-3, "the live tip is the straight line: {straight}");

        let curved = bulge(from, to, finished_tip);
        assert!(curved > 0.5, "and the finished one is a curve: {curved}");

        // Both end on the point the pen was at, exactly: where a stroke's ink stops is not a place to
        // be a floating-point step away from the reading.
        assert_eq!(live.last().unwrap().y, to.y);
        assert_eq!(finished.last().unwrap().x, to.x);
    }

    /// A point with no direction of its own — a pen doubling back over its own ink — keeps the stroke
    /// its width. The direction it is offset across is the one the ink was drawn in; a direction
    /// picked out of the air lays the two edges of the ribbon *along* the line and pinches the stroke
    /// away exactly where the user doubled back.
    #[test]
    fn a_point_that_doubles_back_keeps_the_stroke_its_width() {
        let stroke = doubled_back();
        let turn = 20usize;
        // The line as it is drawn: the readings after the steadier's pass (see [`steadied`]).
        let points = steadied(&stroke.points);
        let point = points[turn];
        let half = point.width * 0.5;

        let mut copy = stroke.clone();
        copy.close_at(1.0);
        let count = copy.points.len();

        // The two ends of the ribbon at the turn are a half width either side of it — *across* the
        // line, which runs vertically here.
        for index in [turn, 2 * count - 1 - turn] {
            let [x, y] = copy.outline[index];
            assert!(
                (y - point.y).abs() < 1e-4,
                "the edge at the turn left the line: {y} against {}",
                point.y
            );
            assert!(
                ((x - point.x).abs() - half).abs() < 1e-4,
                "the ribbon is not a half width wide there: {} against {half}",
                (x - point.x).abs()
            );
        }
    }

    /// The edges are what make a stroke: a down, positions, and an up.
    #[test]
    fn a_down_and_up_make_one_stroke() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Move, 14.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 18.0, 10.0, None)], &id(), &s);

        assert_eq!(ink.stroke_count(), 1);
        assert!(ink.open().is_none(), "the lift closed the stroke");
        let stroke = &ink.finished()[0];
        assert_eq!(stroke.points.len(), 3, "down, move and the lift");
        assert_eq!(stroke.points[2].x, 18.0, "the lift is the last point");
    }

    /// A page is re-detailed when the detail moves and not when it does not: that is what makes a
    /// pinch affordable, and what makes it correct — a page drawn at 16x has to be cut for 16x.
    #[test]
    fn a_page_is_re_detailed_when_the_detail_moves() {
        let mut page = InkDocument::from_strokes(vec![curve()]);
        let at_one = page.finished()[0].outline.len();

        assert!(!page.set_zoom(1.0), "1:1 is where a loaded page already is");
        assert_eq!(page.finished()[0].outline.len(), at_one);

        assert!(page.set_zoom(8.0), "8x is a new rung");
        let at_eight = page.finished()[0].outline.len();
        assert!(at_eight > at_one, "{at_eight} vertices against {at_one}");

        assert!(!page.set_zoom(8.3), "inside the same rung there is nothing to build");
        assert_eq!(page.finished()[0].outline.len(), at_eight);

        // Nothing was edited: the page's ink is exactly the ink it holds.
        assert_eq!(page.finished()[0].points.len(), curve().points.len());
        assert_eq!(page.finished()[0].color, Stroke::DEFAULT_COLOR);
    }

    /// A page turned to is detailed for the sheet in front of the reader rather than for the 1:1 its
    /// file was read at: otherwise a reader zoomed in would be shown a page of straightened pieces on
    /// every page they turned to, for as long as it took them to touch the zoom.
    #[test]
    fn a_page_turned_to_is_detailed_at_the_zoom_in_hand() {
        let mut notes = Notes::new();
        notes.set_zoom(16.0);
        notes.put_page(1, InkDocument::from_strokes(vec![curve()]));
        notes.go_to(1);

        let turned_to = notes.finished()[0].outline.len();
        let plain = InkDocument::from_strokes(vec![curve()]);

        assert!(
            turned_to > plain.finished()[0].outline.len(),
            "the page turned to is cut for the zoom in hand: {turned_to} vertices"
        );
    }

    /// Each stroke keeps the pen it was written with.
    ///
    /// The palette is a set of pens, not a page setting: choosing a different colour has to leave
    /// what the previous one wrote exactly as it was, which is only true if the colour is stamped
    /// into the stroke when the nib goes down.
    #[test]
    fn a_stroke_keeps_the_colour_it_was_written_in() {
        let mut ink = InkDocument::default();
        let mut s = settings();

        s.ink_color = 0xDC_26_26;
        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 18.0, 10.0, None)], &id(), &s);

        s.ink_color = 0x1D_4E_D8;
        ink.consume(&[reading(7, PenPhase::Down, 10.0, 40.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 18.0, 40.0, None)], &id(), &s);

        assert_eq!(ink.finished().len(), 2);
        assert_eq!(ink.finished()[0].color, 0xDC_26_26, "the red line stays red");
        assert_eq!(ink.finished()[1].color, 0x1D_4E_D8, "the blue line is blue");
    }

    /// The steadier takes the panel's noise out and leaves the line where it was written: the ends of
    /// a stroke are exactly the readings, and nothing in the middle moves by more than the noise that
    /// was taken out of it. That is the whole trade the filter makes, and this is its size.
    #[test]
    fn the_steadier_leaves_the_line_where_it_was_written() {
        let written = wandering();
        let points = &written.points;
        let line = steadied(points);

        assert_eq!(line.len(), points.len(), "a pass moves points, never adds one");
        assert_eq!(line[0], points[0], "where a stroke starts is where it was written");
        assert_eq!(
            line[points.len() - 1],
            points[points.len() - 1],
            "and where it stops"
        );

        for (before, after) in points.iter().zip(&line) {
            let moved = ((after.x - before.x).powi(2) + (after.y - before.y).powi(2)).sqrt();
            assert!(
                moved <= 0.25,
                "a point moved {moved} of a pixel: the noise it is taking out is all it may move"
            );
        }
    }

    /// The ink under a magnifier, printed: a stroke's pieces as they are drawn for a sheet at 1:1 and
    /// at 16x, beside the geometry that was drawn for 1:1 whatever the zoom was.
    ///
    /// Run `cargo test --release -- --nocapture the_magnifier`.
    ///
    /// This is the test the complaint "the ink wobbles when I zoom in" is about. A stroke is a
    /// polyline: readings are dropped until they are `resample_spacing` apart, and what is left is
    /// joined with straight edges — invisible at 1:1, and a chain of straight pieces with a corner at
    /// each end once the sheet is drawn sixteen times larger. One character is one *window* pixel, so
    /// the picture is of what the reader sees rather than of what the sheet holds.
    #[test]
    fn the_magnifier() {
        /// The window shown, in window pixels, one character each.
        const WIDTH: usize = 72;
        const HEIGHT: usize = 36;

        eprintln!("\n── the ink under a magnifier ─────────────────────────────────");

        for (name, stroke, focus) in [
            ("a slow arc: points 0.75 px apart", curve(), 0usize),
            ("a flick: points 20 px apart", flick(), 8usize),
        ] {
            eprintln!("  {name}");

            for zoom in [1.0f32, 16.0] {
                let as_it_was = drawn_flat(&stroke);
                let as_it_is = drawn(&stroke, zoom);
                let centre = stroke.points[focus];
                let (before, after) = (
                    grid(&as_it_was, centre, zoom, WIDTH, HEIGHT),
                    grid(&as_it_is, centre, zoom, WIDTH, HEIGHT),
                );

                for (label, outline) in
                    [("as it was:", &as_it_was), ("as it is: ", &as_it_is)]
                {
                    let side = &outline[..outline.len() / 2];
                    eprintln!(
                        "    {zoom:>4.0}x {label} {:>5} vertices · longest piece {:>5.1} px · \
                         sharpest turn {:>4.1}°",
                        outline.len(),
                        longest_edge(side) * zoom,
                        sharpest_turn(side)
                    );
                }

                for row in 0..HEIGHT {
                    eprintln!("    {}  |  {}", before[row], after[row]);
                }
                eprintln!();
            }
        }

        eprintln!("───────────────────────────────────────────────────────────────\n");

        // The numbers the picture is about: at reading size there is nothing to interpolate, and at
        // 16x a piece of ink is a pixel long on screen rather than twelve.
        let stroke = curve();
        let at_one = drawn(&stroke, 1.0);
        let reading_piece = longest_edge(&at_one[..at_one.len() / 2]);
        assert!(
            reading_piece <= FACET,
            "1:1 needs no pieces cut: {reading_piece} logical pixels is a piece"
        );

        let at_sixteen = drawn(&stroke, 16.0);
        let piece = longest_edge(&at_sixteen[..at_sixteen.len() / 2]);
        assert!(
            piece <= FACET * 1.25 / 16.0,
            "16x is drawn from pieces {piece} px long in the sheet, {} on screen",
            piece * 16.0
        );
    }

    /// The wobble a zoom shows: a hand whose panel is a little noisy, at three zooms, as its readings
    /// came and as this code draws them.
    ///
    /// Run `cargo test --release -- --nocapture the_wobble`.
    ///
    /// This is the other half of the magnifier, and the half a *slow* hand is about. Interpolating the
    /// pieces takes the corners out of a stroke, but a corner is not what a reader sees when they zoom
    /// into their own handwriting: what they see is that the line is not quite where they meant it to
    /// be. That is the digitizer's own noise — a fifth of a pixel, invisible at 1:1 and several pixels
    /// of wander at 16x — and nothing that *passes through* the readings can take it out. The steadier
    /// (see [`steadied`]) can, and this is what it buys against what it costs.
    #[test]
    fn the_wobble_a_zoom_shows() {
        /// The radius of the arc the wandering hand was trying to draw.
        const RADIUS: f32 = 60.0;
        /// The window shown, in window pixels, one character each.
        const WIDTH: usize = 72;
        const HEIGHT: usize = 36;

        let written = wandering();
        let once = steadied(&written.points);
        let twice = steadied(&once);

        eprintln!("\n── what a zoom shows of a wandering hand ──────────────────────");
        eprintln!("  the farthest the ink is from the line the hand meant, in window pixels:");

        for zoom in [1.0f32, 4.0, 16.0] {
            eprintln!(
                "    {zoom:>4.0}x   as written {:>5.1}   as drawn {:>5.1}   two passes {:>5.1}",
                wander(&written.points, RADIUS, zoom),
                wander(&once, RADIUS, zoom),
                wander(&twice, RADIUS, zoom)
            );
        }

        // The picture at the zoom the complaint is about: the readings, one straight edge each, against
        // the line this code draws from them.
        let centre = written.points[0];
        let as_written = grid(&drawn_flat(&written), centre, 16.0, WIDTH, HEIGHT);
        let as_drawn = grid(&drawn(&written, 16.0), centre, 16.0, WIDTH, HEIGHT);

        eprintln!("    16x      as written  |  as drawn");
        for row in 0..HEIGHT {
            eprintln!("    {}  |  {}", as_written[row], as_drawn[row]);
        }
        eprintln!("───────────────────────────────────────────────────────────────\n");

        // The numbers, pinned: a noisy panel wanders by pixels of screen at 16x, the line this code
        // draws wanders by less, and at reading size there was never anything to see.
        let as_written = wander(&written.points, RADIUS, 16.0);
        assert!(
            as_written > 2.0,
            "a noisy digitizer wanders by more than a pixel at 16x: {as_written}"
        );

        let as_drawn = wander(
            &densified(&written.points, detail_zoom(16.0), Tip::Curved),
            RADIUS,
            16.0,
        );
        assert!(
            as_drawn < as_written * 0.8,
            "the line this code draws is steadier: {as_drawn} against {as_written}"
        );
        assert!(
            wander(&written.points, RADIUS, 1.0) < 0.5,
            "and at 1:1 the readings were always within half a pixel: {}",
            wander(&written.points, RADIUS, 1.0)
        );
    }

    /// A window of the sheet as characters: one *window* pixel to a character, the ink as `#`.
    fn grid(
        outline: &[[f32; 2]],
        centre: InkPoint,
        zoom: f32,
        width: usize,
        height: usize,
    ) -> Vec<String> {
        let mut rows = Vec::with_capacity(height);
        for row in 0..height {
            let mut line = String::with_capacity(width);
            for column in 0..width {
                // The window pixel this character stands for, in the sheet's own coordinates.
                let x = centre.x + (column as f32 + 0.5 - width as f32 / 2.0) / zoom;
                let y = centre.y + (row as f32 + 0.5 - height as f32 / 2.0) / zoom;
                line.push(if covers(outline, x, y) { '#' } else { '.' });
            }
            rows.push(line);
        }
        rows
    }

    /// A cancel ends the stroke but does not add the position it carries.
    #[test]
    fn a_cancel_ends_the_stroke_without_its_position() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Move, 20.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Cancel, 999.0, 999.0, None)], &id(), &s);

        assert_eq!(ink.stroke_count(), 1);
        assert_eq!(ink.finished()[0].points.len(), 2);
        assert_eq!(
            ink.finished()[0].points.last().unwrap().x,
            20.0,
            "the cancel position is not ink"
        );
    }

    /// A reading from another pointer does not extend the open stroke.
    #[test]
    fn a_second_pointer_does_not_extend_the_open_stroke() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(9, PenPhase::Move, 500.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 20.0, 10.0, None)], &id(), &s);

        assert_eq!(ink.stroke_count(), 1);
        let xs: Vec<f32> = ink.finished()[0].points.iter().map(|p| p.x).collect();
        assert_eq!(xs, [10.0, 20.0], "the second pointer's reading is not ink here");
    }

    /// Hover readings move a cursor but never lay ink, and a stray lift closes nothing.
    #[test]
    fn hovering_lays_no_ink() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(
            &[
                reading(7, PenPhase::Enter, 0.0, 0.0, None),
                reading(7, PenPhase::Hover, 10.0, 0.0, None),
                reading(7, PenPhase::Up, 20.0, 0.0, None),
            ],
            &id(),
            &s,
        );

        assert_eq!(ink.stroke_count(), 0);
        assert!(ink.is_blank());
    }

    /// The paper's edges are the edges of the ink: a reading outside them is off the paper, and the
    /// nearest point that *is* on it is the border.
    #[test]
    fn the_paper_has_edges() {
        let t = papered();

        assert!(t.on_paper((0.0, 0.0)), "the corner is on the paper");
        assert!(t.on_paper((595.0, 842.0)), "so is the far one");
        assert!(!t.on_paper((-0.5, 10.0)), "a point past the left edge is not");
        assert!(!t.on_paper((10.0, 842.5)), "nor one past the bottom");
        assert!(!t.on_paper((f32::NAN, 10.0)), "and a point that is nowhere is not");

        assert_eq!(
            t.onto_paper((-50.0, 900.0)),
            (0.0, 842.0),
            "the nearest point on the paper is a corner"
        );
        assert_eq!(
            t.onto_paper((300.0, 400.0)),
            (300.0, 400.0),
            "a point that is already on it does not move"
        );

        // A transform with no paper — the one a test without a window uses — has no edges at all.
        let none = InkTransform::identity();
        assert!(none.on_paper((f32::MAX, f32::MIN)));
        assert_eq!(none.onto_paper((-10.0, -10.0)), (-10.0, -10.0));
    }

    /// A nib that comes down beside the page draws nothing.
    ///
    /// The canvas is the window and the sheet is a rectangle inside it, so the desk is paintable:
    /// without this, a tap beside the page became a stroke — and the ink went into the note, at
    /// coordinates the page has no room for and no reader would ever see.
    #[test]
    fn a_nib_that_comes_down_off_the_paper_draws_nothing() {
        let mut ink = InkDocument::default();
        let s = settings();
        let t = papered();

        // Down on the desk and then dragged across the page: no stroke was opened, so the drag across
        // it is not one either.
        ink.consume(&[reading(7, PenPhase::Down, 900.0, 400.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Move, 300.0, 400.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Up, 300.0, 400.0, None)], &t, &s);

        assert_eq!(ink.stroke_count(), 0, "the desk is not paper");
        assert!(ink.open().is_none());
        assert_eq!(ink.stats().off_paper, 1, "and the nib that landed there is counted");
    }

    /// A line that leaves the paper is pulled back to its edge, rather than drawn beside it.
    #[test]
    fn a_line_that_leaves_the_paper_stops_at_its_edge() {
        let mut ink = InkDocument::default();
        let s = settings();
        let t = papered();

        ink.consume(&[reading(7, PenPhase::Down, 100.0, 100.0, Some(0.5))], &t, &s);
        // Past the right edge by 300 px, then back on the page at the same height.
        ink.consume(&[reading(7, PenPhase::Move, 900.0, 100.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Move, 400.0, 100.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Up, 400.0, 100.0, None)], &t, &s);

        let strokes = ink.finished();
        assert_eq!(strokes.len(), 1);
        let xs: Vec<f32> = strokes[0].points.iter().map(|p| p.x).collect();

        assert!(
            strokes[0].points.iter().all(|p| t.on_paper((p.x, p.y))),
            "every point of it is on the paper: {xs:?}"
        );
        assert!(
            xs.contains(&t.paper.0),
            "the reading past the edge came back as the edge itself: {xs:?}"
        );
        assert_eq!(
            ink.stats().off_paper,
            1,
            "the one reading past the edge is counted"
        );
    }

    /// The bar is not paper either: a nib that comes down on it opens nothing, because the pen
    /// there is pressing a control rather than writing on the page behind it.
    #[test]
    fn the_bar_is_not_paper() {
        let mut ink = InkDocument::default();
        let s = settings();
        let t = barred(100.0);

        // A tap on a control: down and up, both on the bar.
        ink.consume(&[reading(7, PenPhase::Down, 200.0, 40.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Up, 200.0, 40.0, None)], &t, &s);

        assert_eq!(ink.stroke_count(), 0, "the tap leaves no dot behind the bar");
        assert!(ink.is_blank());
        assert_eq!(
            ink.stats().off_paper,
            0,
            "it is the bar and not the desk: nothing was off the page"
        );

        // The same nib, below the bar, is a pen again.
        ink.consume(&[reading(7, PenPhase::Down, 200.0, 140.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Up, 200.0, 140.0, None)], &t, &s);

        assert_eq!(ink.stroke_count(), 1, "the page below the bar still takes ink");
    }

    /// A line that runs up behind the bar keeps the ink it has: only a `Down` on the bar is refused,
    /// so no stroke is ever cut in two at a line the user cannot see.
    #[test]
    fn a_line_that_runs_up_behind_the_bar_keeps_its_ink() {
        let mut ink = InkDocument::default();
        let s = settings();
        let t = barred(100.0);

        ink.consume(&[reading(7, PenPhase::Down, 200.0, 140.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Move, 200.0, 40.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Up, 200.0, 20.0, None)], &t, &s);

        assert_eq!(ink.stroke_count(), 1);
        let stroke = &ink.finished()[0];
        assert!(
            stroke.bounds[1] < 100.0,
            "the reading behind the bar is part of the line: {:?}",
            stroke.bounds
        );
    }

    /// The eraser is refused by the bar too: erasing a page behind a button is erasing a page the
    /// user is not looking at.
    #[test]
    fn the_eraser_is_refused_by_the_bar_too() {
        let mut ink = InkDocument::default();
        let s = settings();
        let t = barred(100.0);

        // A line written up behind the bar, where a stroke to be erased has to be.
        ink.consume(&[reading(7, PenPhase::Down, 200.0, 140.0, Some(0.5))], &t, &s);
        ink.consume(&[reading(7, PenPhase::Up, 200.0, 20.0, None)], &t, &s);
        assert_eq!(ink.stroke_count(), 1);

        // The eraser nib comes down on the bar, over the line the bar is hiding.
        let mut eraser = reading(7, PenPhase::Down, 200.0, 60.0, None);
        eraser.eraser = true;
        ink.consume(&[eraser], &t, &s);

        assert_eq!(ink.stroke_count(), 1, "the stroke behind the bar is still there");
        assert_eq!(ink.stats().erased_strokes, 0);
    }

    /// The bar's line is the window's, in logical pixels: a reading is measured against it whatever
    /// the panel's scale factor is, and a window with no bar refuses nothing at all.
    #[test]
    fn the_bar_is_a_line_in_the_window() {
        let scaled = InkTransform {
            scale: 2.0,
            ..barred(100.0)
        };

        assert!(scaled.on_bar(200.0), "200 physical pixels is 100 logical");
        assert!(!scaled.on_bar(210.0), "and the pixel below the line is not");
        assert!(
            !InkTransform::identity().on_bar(0.0),
            "no bar refuses nothing, not even the top pixel"
        );
    }

    /// A pen with no pressure sensor draws at the constant width, not at zero.
    #[test]
    fn a_pen_with_no_sensor_draws_a_constant_width() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 0.0, 0.0, None)], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Move, 20.0, 0.0, None)], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 40.0, 0.0, None)], &id(), &s);

        for point in &ink.finished()[0].points {
            assert_eq!(point.width, s.no_pressure_width);
        }
    }

    /// The resampler drops points that are too close, but never the lift.
    #[test]
    fn the_resampler_keeps_the_lift_and_drops_the_clutter() {
        let mut ink = InkDocument::default();
        let s = Settings {
            resample_spacing: 10.0,
            smoothing_ms: 0.0,
            ..Settings::default()
        };

        ink.consume(&[reading(7, PenPhase::Down, 0.0, 0.0, Some(0.5))], &id(), &s);
        for step in 1..=9 {
            // Each reading is 1 px from the last: all of them are below the spacing.
            ink.consume(
                &[reading(7, PenPhase::Move, step as f32, 0.0, Some(0.5))],
                &id(),
                &s,
            );
        }
        ink.consume(&[reading(7, PenPhase::Up, 9.5, 0.0, None)], &id(), &s);

        let stroke = &ink.finished()[0];
        assert_eq!(stroke.points.len(), 2, "the down and the lift");
        assert_eq!(
            stroke.points[1].x, 9.5,
            "the stroke ends where the pen lifted"
        );
        assert!(
            ink.stats().resampled >= 8,
            "the clutter was counted, not drawn"
        );
    }

    /// Undo removes the most recent stroke and nothing else.
    #[test]
    fn undo_removes_the_most_recent_stroke() {
        let mut ink = page_with(3);
        assert_eq!(ink.stroke_count(), 3);

        assert!(ink.can_undo(), "there is something to take back");
        assert!(ink.undo());
        assert_eq!(ink.stroke_count(), 2);
        assert!(ink.undo());
        assert!(ink.undo());
        assert!(!ink.undo(), "there is nothing left to undo");
        assert!(ink.is_blank());
        assert!(!ink.can_undo(), "and the interface can say so beforehand");
    }

    /// An edit made while a snapshot is held produces a *new* list, and leaves the snapshot alone.
    ///
    /// This is the property the note screen's ink layer is built on: the layer keeps the page's `Arc`
    /// between frames, and the model is what guarantees that the next stroke, undo or redo cannot write
    /// through it — which is also what keeps a frame that is drawing the previous scene from showing a
    /// half-edited page. [`InkDocument::finish_open`] states the mechanism in a comment ("`make_mut`
    /// reuses the existing vector when no frame is holding a snapshot, and copies it when one is");
    /// this is the half a frame depends on, asserted.
    #[test]
    fn an_edit_while_a_snapshot_is_held_leaves_the_snapshot_alone() {
        let mut ink = page_with(2);
        let held = Arc::clone(ink.finished());

        assert!(ink.undo(), "the last stroke comes off");
        assert!(
            !Arc::ptr_eq(&held, ink.finished()),
            "and the list it came off is a new one, not the one being held"
        );
        assert_eq!(held.len(), 2, "the snapshot still says what it said");
        assert_eq!(ink.finished().len(), 1);

        assert!(ink.redo(), "and the stroke goes back on");
        assert_eq!(held.len(), 2, "with the snapshot still untouched");
        assert_eq!(ink.finished().len(), 2);
    }

    /// Redo puts back exactly what undo took away, in the order they went, and never doubles back
    /// further than the undos reached.
    #[test]
    fn redo_puts_back_what_undo_took_away() {
        let mut ink = page_with(3);
        assert!(!ink.can_redo(), "nothing has been taken back yet");

        ink.undo();
        ink.undo();
        assert_eq!(ink.stroke_count(), 1);
        assert!(ink.can_redo());

        assert!(ink.redo());
        assert_eq!(ink.stroke_count(), 2);
        assert_eq!(
            ink.finished()[1].points[0].x,
            10.0,
            "the stroke that came back is the one that went"
        );

        assert!(ink.redo());
        assert_eq!(ink.stroke_count(), 3);
        assert!(!ink.redo(), "and there is nothing left to put forward");
        assert!(!ink.can_redo());
    }

    /// A stroke drawn after an undo is a new drawing: what was taken back is not put forward again.
    #[test]
    fn a_stroke_drawn_after_an_undo_ends_the_branch() {
        let mut ink = page_with(2);
        let s = settings();

        ink.undo();
        assert!(ink.can_redo());

        ink.consume(&[reading(7, PenPhase::Down, 900.0, 0.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 940.0, 0.0, None)], &id(), &s);

        assert!(
            !ink.can_redo(),
            "the new stroke ended the branch the undo left"
        );
        assert!(!ink.redo(), "and there is nothing to put forward");
        assert_eq!(ink.stroke_count(), 2, "the page is what was drawn on it");
    }

    /// The stroke under the nib is not history: undo takes the stroke before it and leaves the
    /// pen's own line — and the pen — exactly as they were.
    #[test]
    fn undo_leaves_the_line_under_the_nib_alone() {
        let mut ink = page_with(1);
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 500.0, 0.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Move, 600.0, 0.0, Some(0.5))], &id(), &s);
        assert!(ink.open().is_some(), "the pen is drawing");

        assert!(ink.undo());
        assert_eq!(ink.stroke_count(), 1, "the finished stroke was taken back");
        assert_eq!(
            ink.open().map(|open| open.points.len()),
            Some(2),
            "the line being drawn was not touched"
        );

        // The stroke keeps growing: an undo must not leave the model thinking the pen was lifted.
        ink.consume(&[reading(7, PenPhase::Move, 700.0, 0.0, Some(0.5))], &id(), &s);
        assert_eq!(
            ink.open().map(|open| open.points.len()),
            Some(3),
            "the pen went on drawing"
        );

        ink.consume(&[reading(7, PenPhase::Up, 700.0, 0.0, None)], &id(), &s);
        assert_eq!(
            ink.stroke_count(),
            1,
            "the stroke the pen was drawing was finished on the page"
        );
        assert!(ink.open().is_none());
    }

    /// With nothing finished on the page there is nothing to take back, even while the pen is down.
    #[test]
    fn a_page_with_no_finished_stroke_has_nothing_to_undo() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);

        assert!(!ink.can_undo());
        assert!(!ink.undo(), "the pen's own line is not history");
        assert!(ink.open().is_some(), "and it is still there");
    }

    /// Erasing is a change of its own, so it ends the branch an undo left open.
    #[test]
    fn erasing_ends_the_branch() {
        let mut ink = page_with(2);
        let s = settings();

        ink.undo();
        assert!(ink.can_redo());
        assert_eq!(ink.stroke_count(), 1);

        // The eraser nib passes over the stroke that is still there.
        let mut eraser = reading(7, PenPhase::Down, 0.0, 0.0, None);
        eraser.eraser = true;
        ink.consume(&[eraser], &id(), &s);

        assert_eq!(ink.stroke_count(), 0, "the stroke the eraser touched is gone");
        assert!(!ink.can_redo(), "and the eraser ended the branch");
    }

    /// Clearing says the page holds nothing, in both directions.
    #[test]
    fn clearing_ends_both_directions() {
        let mut ink = page_with(3);

        ink.undo();
        assert!(ink.can_undo() && ink.can_redo());

        ink.clear();
        assert!(ink.is_blank());
        assert!(!ink.can_undo());
        assert!(
            !ink.can_redo(),
            "nothing can be put back onto an emptied page"
        );
    }

    /// Turning the page and turning back keeps a page's redo, even though the page itself is empty:
    /// a page turn is not an edit.
    #[test]
    fn a_page_turn_keeps_what_can_be_redone() {
        let mut notes = Notes::new();
        let s = settings();

        notes.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);
        notes.consume(&[reading(7, PenPhase::Up, 30.0, 30.0, None)], &id(), &s);
        notes.undo();
        assert!(notes.is_blank(), "the page it was drawn on is empty again");

        notes.go_to(1);
        notes.go_to(0);

        assert!(notes.can_redo(), "the way back survived the page turn");
        assert!(notes.redo());
        assert_eq!(notes.stroke_count(), 1);
        assert_eq!(
            inked(&notes),
            vec![0],
            "and the page counts as written on again"
        );
    }

    /// Physical pixels are divided by the window's scale exactly once.
    #[test]
    fn the_dpi_scale_is_applied_once() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 150.0, 90.0, Some(0.5))], &scaled(1.5), &s);
        ink.consume(&[reading(7, PenPhase::Up, 300.0, 180.0, None)], &scaled(1.5), &s);

        assert_eq!(ink.finished()[0].points[0].x, 100.0);
        assert_eq!(ink.finished()[0].points[1].y, 120.0);
    }

    /// The eraser removes the strokes it passes over and leaves the rest alone.
    #[test]
    fn the_eraser_removes_only_what_it_touches() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 0.0, 0.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 40.0, 0.0, None)], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Down, 500.0, 0.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Up, 540.0, 0.0, None)], &id(), &s);
        assert_eq!(ink.stroke_count(), 2);

        // The eraser nib touches the first stroke only.
        let mut eraser = reading(7, PenPhase::Down, 20.0, 0.0, None);
        eraser.eraser = true;
        ink.consume(&[eraser], &id(), &s);

        assert_eq!(ink.stroke_count(), 1);
        assert_eq!(ink.finished()[0].points[0].x, 500.0);
        assert_eq!(ink.stats().erased_strokes, 1);
    }

    /// Ink belongs to the paper, not to the window: a reading becomes the place on the sheet that
    /// the reader sees the nib at, so zooming moves the ink with the page it was written on.
    #[test]
    fn ink_lands_where_the_sheet_is_drawn() {
        let mut ink = InkDocument::default();
        let s = settings();

        // A sheet drawn at 2x, its top-left corner 100 px into the window.
        let sheet = zoomed(2.0, (100.0, 60.0));
        ink.consume(&[reading(7, PenPhase::Down, 300.0, 160.0, Some(0.5))], &sheet, &s);

        let point = ink.open().expect("a stroke").points[0];
        assert_eq!((point.x, point.y), (100.0, 50.0), "(300-100)/2, (160-60)/2");

        // The same reading on a sheet at its own size, drawn at the origin, is the reading.
        let mut flat = InkDocument::default();
        flat.consume(&[reading(7, PenPhase::Down, 300.0, 160.0, Some(0.5))], &id(), &s);
        let point = flat.open().expect("a stroke").points[0];
        assert_eq!((point.x, point.y), (300.0, 160.0));
    }

    /// A transform that cannot divide draws the ink where the pen is rather than nowhere.
    #[test]
    fn a_broken_transform_is_the_identity() {
        let broken = InkTransform {
            scale: 0.0,
            zoom: f32::NAN,
            origin: (10.0, 10.0),
            ..InkTransform::identity()
        };

        assert_eq!(broken.sheet_point((30.0, 30.0)), (20.0, 20.0));
    }

    /// A stroke's bounds follow it as it grows, so a frame can ask whether the stroke being drawn
    /// is on screen before the stroke has ever been closed.
    #[test]
    fn a_growing_stroke_knows_where_it_is() {
        let mut ink = InkDocument::default();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);
        ink.consume(&[reading(7, PenPhase::Move, 90.0, 70.0, Some(0.5))], &id(), &s);

        let open = ink.open().expect("a stroke");
        assert_eq!(open.bounds, [10.0, 10.0, 90.0, 70.0]);
        assert!(open.visible_in([0.0, 0.0, 50.0, 50.0]), "the corner overlaps");
        assert!(!open.visible_in([200.0, 200.0, 300.0, 300.0]), "and away does not");
    }

    /// Off-screen ink is rejected by four comparisons rather than turned into a polygon.
    #[test]
    fn a_stroke_outside_the_view_is_not_visible() {
        let mut stroke = Stroke::new(InkPoint::new(500.0, 500.0, 2.0), Stroke::DEFAULT_COLOR);
        stroke.points.push(InkPoint::new(540.0, 520.0, 2.0));
        stroke.close();

        assert!(stroke.visible_in([400.0, 400.0, 600.0, 600.0]));
        assert!(stroke.visible_in([520.0, 480.0, 700.0, 700.0]), "overlapping counts");
        assert!(!stroke.visible_in([0.0, 0.0, 100.0, 100.0]));
        assert!(!stroke.visible_in([600.0, 0.0, 700.0, 100.0]), "beside it");
    }

    /// Ending a stroke must not copy the page's ink.
    ///
    /// The strokes are behind their own `Arc`s precisely so that growing the vector of them is a
    /// pointer copy; with the strokes inline, a page holding a thousand of them deep-copied all
    /// thousand at every lift. The pointers are the assertion: a copy would move them.
    #[test]
    fn ending_a_stroke_does_not_copy_the_page() {
        let mut ink = InkDocument::default();
        let s = settings();

        let mut pointers: Vec<*const Stroke> = Vec::new();
        for index in 0..64 {
            let x = index as f32 * 10.0;
            ink.consume(&[reading(7, PenPhase::Down, x, 0.0, Some(0.5))], &id(), &s);
            ink.consume(&[reading(7, PenPhase::Up, x + 4.0, 0.0, None)], &id(), &s);

            if index < 63 {
                pointers = ink.finished().iter().map(Arc::as_ptr).collect();
            }
        }

        let after: Vec<*const Stroke> = ink.finished().iter().map(Arc::as_ptr).collect();
        assert_eq!(after.len(), 64);
        assert_eq!(
            &after[..63],
            &pointers[..],
            "the first sixty-three strokes are the same allocations they were"
        );
    }

    /// A reading that erases nothing must not copy the page — and it is the common one: a drag
    /// spends most of its readings over blank paper.
    #[test]
    fn erasing_blank_paper_does_not_copy_the_page() {
        let mut ink = page_with(200);
        let s = settings();

        // A frame holding a snapshot, exactly as the render path does.
        let frame = Arc::clone(ink.finished());
        assert_eq!(Arc::strong_count(&frame), 2);

        let mut eraser = reading(7, PenPhase::Move, 0.0, 0.0, None);
        eraser.eraser = true;
        ink.consume(&[eraser], &id(), &s);

        assert_eq!(
            Arc::strong_count(&frame),
            2,
            "the page was copied for a reading that touched nothing"
        );
        assert_eq!(ink.finished().len(), 200);
    }

    /// Ink stays on the page it was written on.
    ///
    /// This is the bug the per-page model exists for: with one document for the whole note, writing
    /// on page one and turning to page two left the writing on screen — laid over a page it was
    /// never drawn on, and appended to by the pen.
    #[test]
    fn ink_stays_on_the_page_it_was_written_on() {
        let mut notes = Notes::new();
        let s = settings();

        notes.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], &id(), &s);
        notes.consume(&[reading(7, PenPhase::Up, 30.0, 30.0, None)], &id(), &s);

        assert_eq!(notes.stroke_count(), 1, "page one has the stroke");

        notes.go_to(1);
        assert!(notes.is_blank(), "page two is empty");
        assert_eq!(notes.stroke_count(), 0);

        notes.consume(&[reading(7, PenPhase::Down, 50.0, 50.0, Some(0.5))], &id(), &s);
        notes.consume(&[reading(7, PenPhase::Up, 70.0, 70.0, None)], &id(), &s);
        assert_eq!(notes.stroke_count(), 1, "page two has its own stroke");

        notes.go_to(0);
        assert_eq!(notes.stroke_count(), 1, "page one still has exactly its own");
        assert_eq!(inked(&notes), vec![0, 1], "and both pages are written on");

        // The stroke on page two is the one that starts at (50, 50): erasing where page one's ink
        // is must leave page two alone, and vice versa.
        let first = notes.finished()[0].points[0];
        assert_eq!((first.x, first.y), (10.0, 10.0), "page one's stroke is its own");
    }

    /// A page that was written on and left keeps its ink; a page that was never touched stays out
    /// of the note.
    #[test]
    fn a_page_keeps_its_ink_and_a_blank_page_is_not_kept() {
        let mut notes = Notes::new();
        let s = settings();

        notes.go_to(3);
        assert!(notes.is_blank());
        notes.consume(&[reading(7, PenPhase::Down, 5.0, 5.0, Some(0.5))], &id(), &s);
        notes.consume(&[reading(7, PenPhase::Up, 9.0, 9.0, None)], &id(), &s);

        // Visiting pages that are not written on must not invent pages.
        notes.go_to(1);
        notes.go_to(2);
        notes.go_to(3);
        assert_eq!(inked(&notes), vec![3], "only the page with ink");
        assert_eq!(notes.stroke_count(), 1, "and its ink came back with it");
        assert_eq!(
            inked(&notes).last().map_or(1, |page| page + 1),
            4,
            "pages 0..=3 exist for turning"
        );
    }

    /// One stroke written on the page the notes are on, starting at `x`: how a test says which page
    /// holds what.
    fn write(notes: &mut Notes, s: &Settings, x: f32) {
        notes.consume(&[reading(7, PenPhase::Down, x, x, Some(0.5))], &id(), s);
        notes.consume(&[reading(7, PenPhase::Up, x + 4.0, x, None)], &id(), s);
    }

    /// Inserting a page renames the pages after it, and the ink moves with the names.
    #[test]
    fn inserting_a_page_moves_the_ink_with_the_names() {
        let mut notes = Notes::new();
        let s = settings();

        // Page 0 and page 2 written on, page 1 left alone.
        write(&mut notes, &s, 1.0);
        notes.go_to(2);
        write(&mut notes, &s, 50.0);
        assert_eq!(inked(&notes), vec![0, 2]);

        // A page is inserted where page 1 was, and the reader turns to it.
        notes.insert_at(1);
        notes.go_to(1);

        assert!(notes.is_blank(), "the inserted page has no ink");
        assert_eq!(
            inked(&notes),
            vec![0, 3],
            "the page that was at 2 is now at 3"
        );

        notes.go_to(3);
        assert_eq!(notes.stroke_count(), 1, "and its ink moved with it");
        assert_eq!(notes.finished()[0].points[0].x, 50.0);
    }

    /// Deleting a page takes its ink with it, and shows the page that followed.
    #[test]
    fn deleting_a_page_takes_its_ink_and_shows_the_next_one() {
        let mut notes = Notes::new();
        let s = settings();

        write(&mut notes, &s, 1.0);
        notes.go_to(1);
        write(&mut notes, &s, 20.0);
        notes.go_to(2);
        write(&mut notes, &s, 40.0);
        assert_eq!(inked(&notes), vec![0, 1, 2]);

        // The page being shown is the one deleted: the reader lands on what followed it.
        notes.remove_at(1);

        assert_eq!(inked(&notes), vec![0, 1]);
        assert_eq!(notes.stroke_count(), 1, "the page that followed is on screen");
        assert_eq!(
            notes.finished()[0].points[0].x,
            40.0,
            "and it is the page that followed, not the one deleted"
        );

        // Deleting a page *before* the one being read keeps the reader on the same ink.
        notes.go_to(1);
        notes.remove_at(0);
        assert_eq!(notes.stroke_count(), 1);
        assert_eq!(notes.finished()[0].points[0].x, 40.0);
    }

    /// Reading a note back puts each page's ink where it was written, and the page that was open is
    /// the page on screen.
    ///
    /// This is what the app does when it opens a note: pages are *put* into the model one at a time
    /// as they are turned to, rather than the whole note being loaded at once — see
    /// `NoteApp::load_page_ink` — so the model has to put a page where a page belongs.
    #[test]
    fn putting_pages_back_restores_a_note() {
        let mut notes = Notes::new();

        // Two pages read out of the note, and the pen is turned to the second of them.
        notes.go_to(5);
        notes.put_page(5, page_with(4));
        notes.put_page(2, page_with(3));

        assert_eq!(
            notes.stroke_count(),
            4,
            "the page that was open is the one on screen"
        );
        assert_eq!(inked(&notes), vec![2, 5]);

        notes.go_to(2);
        assert_eq!(
            notes.stroke_count(),
            3,
            "and the other page is where it was read from"
        );
    }

    /// The pages that hold ink, in page order: what a save would write out.
    ///
    /// The model used to answer this itself — `Notes::written_pages` — and the *note* answers it now,
    /// out of its database (see [`crate::store::NoteStore::pages`]), so the tests read the model's own
    /// map directly: this module is the model, and what it is holding is what is being tested.
    fn inked(notes: &Notes) -> Vec<usize> {
        let mut pages: Vec<usize> = notes
            .taken
            .iter()
            .filter(|(_, ink)| !ink.is_blank())
            .map(|(page, _)| *page)
            .collect();

        if !notes.current.is_blank() {
            pages.push(notes.page);
        }

        pages.sort_unstable();
        pages.dedup();
        pages
    }

    /// A page of `count` two-point strokes, each 10 px apart, for the tests above.
    fn page_with(count: usize) -> InkDocument {
        let mut ink = InkDocument::default();
        let s = settings();

        for index in 0..count {
            let x = (index % 40) as f32 * 10.0;
            let y = (index / 40) as f32 * 10.0;
            ink.consume(&[reading(7, PenPhase::Down, x, y, Some(0.5))], &id(), &s);
            ink.consume(&[reading(7, PenPhase::Up, x + 4.0, y, None)], &id(), &s);
        }

        ink
    }

    /// A page of `count` strokes of 40 points each: enough ink to look like a written page.
    fn written_page(count: usize) -> InkDocument {
        let mut ink = InkDocument::default();

        for index in 0..count {
            let mut samples = Vec::with_capacity(41);
            let x = (index % 20) as f32 * 40.0;
            let y = (index / 20) as f32 * 60.0;
            samples.push(reading(7, PenPhase::Down, x, y, Some(0.5)));

            for step in 1..40 {
                // Two pixels apart: above the resampler's spacing, so every one is kept.
                samples.push(reading(
                    7,
                    PenPhase::Move,
                    x + step as f32 * 2.0,
                    y + (step % 7) as f32,
                    Some(0.5),
                ));
            }
            samples.push(reading(7, PenPhase::Up, x + 80.0, y, None));
            ink.consume(&samples, &id(), &settings());
        }

        ink
    }

    /// `count` readings along a wave, 2 px apart, as one batch.
    fn a_wave(count: usize) -> Vec<pen_windows::PenSample> {
        let mut samples = Vec::with_capacity(count + 2);
        samples.push(reading(7, PenPhase::Down, 0.0, 0.0, Some(0.5)));

        for step in 1..count {
            let angle = step as f32 * 0.05;
            samples.push(reading(
                7,
                PenPhase::Move,
                step as f32 * 2.0,
                angle.sin() * 20.0,
                Some(0.5),
            ));
        }

        samples.push(reading(7, PenPhase::Up, count as f32 * 2.0, 0.0, None));
        samples
    }

    /// What the ink model costs, measured rather than guessed.
    ///
    /// The budgets here are deliberately loose — an order of magnitude above what this machine
    /// measures, and they have to hold in a debug build too. They are not a speed target; they
    /// guard the *shape* of the hot paths. A copy that used to be a move, or a rebuild that used
    /// to be a cache hit, changes the order of magnitude and fails here rather than in somebody's
    /// hand.
    ///
    /// Run `cargo test --release -- --nocapture measures_the_ink_costs` for the numbers.
    #[test]
    fn measures_the_ink_costs() {
        let s = settings();
        let call = |elapsed: std::time::Duration, calls: u32| {
            elapsed.as_secs_f64() * 1_000_000.0 / f64::from(calls)
        };

        // ── Reading a batch into ink: one pump wake of the writing loop ──────
        let batch = a_wave(240);
        let mut ink = InkDocument::default();
        let rounds = 200;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            ink.clear();
            ink.consume(&batch, &id(), &s);
        }
        let consumed = started.elapsed();

        // ── Closing a stroke: the ribbon geometry ────────────────────────────
        let mut closings = Vec::new();
        for points in [100usize, 1_000, 3_000] {
            let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, 2.0), Stroke::DEFAULT_COLOR);
            for step in 1..points {
                stroke
                    .points
                    .push(InkPoint::new(step as f32 * 2.0, (step % 11) as f32, 2.0));
            }

            let rounds = 20;
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                stroke.close();
            }
            closings.push((points, started.elapsed() / rounds));
        }

        // ── The detail a zoom asks for: what a frame pays at 16x ─────────────
        //
        // The stroke under the nib is closed on *every* frame, and a zoomed sheet is what asks for the
        // most pieces (see `FACET` and `MAX_STEPS`) — so this is the number that says whether writing
        // while zoomed in still fits in a frame. A shape of the same size as a note's own writing: a
        // thousand points three quarters of a pixel apart.
        let mut zoomed = Vec::new();
        for zoom in [1.0f32, 4.0, 16.0] {
            let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, 2.0), Stroke::DEFAULT_COLOR);
            for step in 1..1_000 {
                let angle = step as f32 * 0.0125;
                stroke
                    .points
                    .push(InkPoint::new(60.0 * angle.cos(), 60.0 * angle.sin(), 2.0));
            }

            let rounds = 20;
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                stroke.close_at(zoom);
            }
            zoomed.push((zoom, stroke.outline.len(), started.elapsed() / rounds));
        }

        // ── Ending a stroke on a page that is already full ───────────────────
        let mut page = written_page(2_000);
        let rounds = 100;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            page.consume(&[reading(7, PenPhase::Down, 5.0, 5.0, Some(0.5))], &id(), &s);
            page.consume(&[reading(7, PenPhase::Up, 9.0, 5.0, None)], &id(), &s);
        }
        let appended = started.elapsed() / rounds;

        // ── An eraser reading that actually erases something ────────────────
        let mut hit_page = written_page(2_000);
        let mut eraser = reading(7, PenPhase::Move, 0.0, 0.0, None);
        eraser.eraser = true;
        let stroke_point = hit_page.finished()[1].points[0];
        let hits = 100u32;
        let started = std::time::Instant::now();
        for _ in 0..hits {
            eraser.pixel = pen_windows::Point::new(stroke_point.x, stroke_point.y);
            hit_page.consume(&[eraser], &id(), &s);
        }
        let erased_hit = started.elapsed();

        // ── What the eraser used to cost, kept as the reason it does not ─────
        //
        // Every reading — hit or miss — used to deep-copy the whole page: one `Stroke` clone per
        // stroke on it, points and ribbon outline and all. Nothing calls this path any more; it is
        // measured so that the reason the strokes are behind their own `Arc`s is a number rather
        // than a memory.
        let page = written_page(2_000);
        let rounds = 50;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            let deep: Vec<Stroke> = page
                .finished()
                .iter()
                .map(|stroke| (**stroke).clone())
                .collect();
            std::hint::black_box(deep);
        }
        let used_to_be = started.elapsed() / rounds;

        // ── An eraser dragged over blank paper, which is most of a drag ──────
        let mut page = written_page(2_000);
        let mut eraser = reading(7, PenPhase::Move, 0.0, 0.0, None);
        eraser.eraser = true;
        let drags = 500u32;
        let started = std::time::Instant::now();
        for step in 0..drags {
            eraser.pixel = pen_windows::Point::new(-500.0 - step as f32, -500.0);
            page.consume(&[eraser], &id(), &s);
        }
        let erased = started.elapsed();

        // ── Culling: asking a full page whether each stroke is on screen ─────
        let rect = [0.0, 0.0, 800.0, 600.0];
        let rounds = 200;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            for stroke in page.finished().iter() {
                std::hint::black_box(stroke.visible_in(rect));
            }
        }
        let culled = started.elapsed() / rounds;

        eprintln!("\n── ink, measured ──────────────────────────────────────────────");
        eprintln!(
            "  240 readings, one pump wake          {:>9.1} us",
            call(consumed / rounds, 1)
        );
        for (points, elapsed) in &closings {
            eprintln!("  close a {points:>4}-point stroke         {elapsed:>9.1?}");
        }
        for (zoom, vertices, elapsed) in &zoomed {
            eprintln!(
                "  close 1000 points for a {zoom:>4.0}x sheet {elapsed:>9.1?}   ({} vertices)",
                vertices / 2
            );
        }
        eprintln!("  end a stroke on a 2000-stroke page   {appended:>9.1?}");
        eprintln!(
            "  one erase reading, blank paper       {:>9.1} us",
            call(erased, drags)
        );
        eprintln!("  cull a 2000-stroke page              {culled:>9.1?}");
        eprintln!(
            "  one erase reading, hitting ink       {:>9.1} us",
            call(erased_hit, hits)
        );
        eprintln!(
            "  ...the copy that used to happen      {used_to_be:>9.1?}   (per reading, hit or miss)"
        );
        eprintln!("───────────────────────────────────────────────────────────────\n");

        // ── The budgets ──────────────────────────────────────────────────────
        assert!(
            call(consumed / rounds, 1) < 20_000.0,
            "240 readings taking over 20 ms is not a real-time ink model"
        );
        for (points, elapsed) in &closings {
            assert!(
                elapsed.as_millis() < 200,
                "closing a {points}-point stroke took {elapsed:?}"
            );
        }
        for (zoom, _, elapsed) in &zoomed {
            assert!(
                elapsed.as_millis() < 20,
                "closing a stroke for a {zoom}x sheet took {elapsed:?}, which is not a frame"
            );
        }
        assert!(
            appended.as_millis() < 50,
            "ending a stroke on a full page took {appended:?}"
        );
        assert!(
            call(erased, drags) < 500.0,
            "an erase reading that touched nothing took {:?}",
            erased / drags
        );
        assert!(
            culled.as_micros() < 2_000,
            "culling a full page took {culled:?}"
        );
    }
}
