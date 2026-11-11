//! What a stroke is: the ink itself, and the geometry derived from it.
//!
//! ## What is stored, and what is not
//!
//! A stroke is a list of [`InkPoint`]s — a position and the **width the pen drew it at** — and one
//! colour. Nothing else is written down, and that is the format's most important decision:
//!
//! * **No pressure, no tilt, no timing.** A reading's pressure is turned into a width the moment the
//!   point is made (`InkPoint::width`), so the note records the *line* rather than the sensor that
//!   drew it. Two applications with different digitizers, different drivers and different smoothing
//!   still read the same page as the same line.
//! * **No outline, no bounds.** The [`Stroke::outline`] a renderer draws and the
//!   [`Stroke::bounds`] it culls with are *derived* from the points, at the zoom the page is drawn
//!   at, and thrown away when that zoom changes. A note that stored them would be a note whose file
//!   changes when it is merely looked at.
//! * **Positions in logical pixels.** Resolution-independent, so a page written on a 1:1 screen
//!   lands in the same place on the sheet as one written on a 2:1 screen (see [`crate::paper`]).
//!
//! The colour is `0xRRGGBB`, or `0xAARRGGBB` for a highlighter, and [`alpha_of`] is the whole rule
//! for telling them apart: a colour with no top byte is opaque. It is written that way because it
//! always has been, and because every stroke ever drawn is read through it.
//!
//! ## Why the derived geometry is here too
//!
//! Because it is arithmetic over the points and nothing else — no window, no GPU, no pen. The app's
//! model (`InkDocument`) builds a page's outlines once per zoom rung and the ink layer draws them;
//! both of them are asking this module a question, and so is an application on another platform
//! that wants to draw the same page its own way.
//!
//! What is *not* here: the document, the tools, the eraser, the lasso, undo. Those are the app's
//! (see its `ink.rs`), because they are about a reader in front of a window rather than about what a
//! note is.

use std::sync::Arc;

use serde::Serialize;

use crate::history;

/// A colour with an alpha stamped into its top byte: `0xAARRGGBB`.
///
/// What a highlighter's stroke is written in, and what a pen's is *not*: see [`alpha_of`] for the rule that keeps
/// a pen stroke's colour the `0xRRGGBB` it has always been.
pub fn with_alpha(color: u32, alpha: f32) -> u32 {
    let byte = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    (byte << 24) | (color & 0x00FF_FFFF)
}

/// How much of the page a stroke's colour lets through, in `0.0..=1.0`.
///
/// **A colour with no top byte is opaque**, and that is the whole of the rule. A pen's colour is written as
/// `0xRRGGBB` — in the settings, in a note's `meta`, and in every stroke ever drawn — so reading the top byte as
/// an alpha *without* that exception would make every line of ink that was ever written invisible.
pub fn alpha_of(color: u32) -> f32 {
    match color >> 24 {
        0 => 1.0,
        byte => byte as f32 / 255.0,
    }
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
    /// The colour this stroke was written in, as `0xRRGGBB` — or `0xAARRGGBB` when it is a highlighter, whose ink
    /// lets the page through.
    ///
    /// The colour is stamped in when the nib goes down and never changes: it is what makes a page of ink a record
    /// of what was drawn with what. [`alpha_of`] is the whole of how the two shapes are told apart, and a stroke
    /// with no top byte is opaque.
    ///
    /// Per stroke, not per page: the palette is a set of pens, and picking up a different one must
    /// not repaint what the others wrote.
    pub color: u32,
    /// The ribbon outline, in logical pixels, cached when the stroke is closed.
    ///
    /// Recomputing the outline every frame is pure waste: the points never change once the
    /// stroke is finished. Not serialised, because it is derivable from `points` — and *only* from
    /// them plus the zoom it was built for, which is why it is thrown away and built again when the
    /// sheet is drawn at another size (see [`Self::close_at`], which is what the app's `InkDocument::set_zoom` calls).
    #[serde(skip)]
    pub outline: Vec<[f32; 2]>,
    /// The axis-aligned bounds as `[min_x, min_y, max_x, max_y]`, for culling and hit-testing.
    #[serde(skip)]
    pub bounds: [f32; 4],
}

impl Stroke {
    /// The near-black the app drew everything in before a stroke could carry a colour of its own.
    ///
    /// Not a default an application has to use — the pen's colour is a setting a note keeps (see
    /// [`crate::settings::Settings::ink_color`]) — but a named colour for a caller that has to
    /// build a stroke without being given one: a test, a migration, or a reader drawing a page it
    /// was handed with no settings at all.
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
    /// [`Self::close_at`], which is what the app's `InkDocument::set_zoom` calls).
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
    pub fn freeze(&mut self, detail: f32, tip: Tip) {
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
        self.outline = ribbon_outline(&Model::of(&self.points, detail, tip));
    }

    /// Rebuilds the derived geometry of a stroke for `detail`: its bounds and its outline.
    ///
    /// What a stroke read out of a note — or out of a logged *edit* — needs before it can be drawn or erased: the
    /// derived geometry is not stored anywhere, so a stroke that arrives without a rebuild would draw as nothing
    /// and refuse to be erased (see [`crate::chunk`] and [`crate::store`]).
    pub fn rebuild(&mut self, detail: f32) {
        self.freeze(detail, Tip::Curved);
    }

    /// Moves the whole line by `by`, and rebuilds what a move invalidates.
    ///
    /// The points are the ink; the bounds and the outline are *derived* from them, and a stroke whose points had
    /// moved while one of those had not would refuse to be erased where it is and be erased where it used to be
    /// (see [`Self::hits`] and the app's ink layer).
    pub fn translate(&mut self, by: (f32, f32), detail: f32) {
        for point in &mut self.points {
            point.x += by.0;
            point.y += by.1;
        }

        self.rebuild(detail);
    }

    /// Whether this stroke is the same *ink* as another: the same line, in the same colour.
    ///
    /// Identity for a history, and deliberately not pointer equality: an edit is replayed against a page that may
    /// have been read back out of a note — the same strokes, freshly decoded — so what an edit can check is that
    /// the line it recorded is the line that is there (see [`crate::history::Edit::apply`]). The derived geometry
    /// is not part of it: bounds and outlines are built for the sheet a page is drawn at, and the same ink at two
    /// zooms is the same ink.
    pub fn same_ink(&self, other: &Stroke) -> bool {
        self.color == other.color && self.points == other.points
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
/// (see [`crate::settings::Settings::resample_spacing`]), and between two of the points that survive, the ink is a
/// straight edge. At 1:1 that is invisible — the spacing is under a pixel — but a zoom magnifies the
/// line *and* the corner at the end of every piece, so a stroke of curves read at 8x is a chain of
/// straight pieces. That is what "the ink wobbles when I zoom in" is made of, and cutting the pieces
/// down to this length is what removes it.
///
/// Measured in *window* pixels rather than the sheet's, because the zoom is what makes a piece
/// visible: the same ink written at 1:1 and examined at 16x needs pieces sixteen times shorter in
/// sheet coordinates to look the same.
///
/// **Two, and the error it costs is the reason.** A chord of length `f` standing in for an arc of radius
/// `r` is off by `r(1 - cos(f/2r))`, which is at most `f²/8r` — for the tightest turn a pen makes, a
/// three-pixel radius, that is 0.04 pixels at 1.0, 0.09 at 1.5, and 0.17 at 2.0. The edge is sampled four
/// times a pixel, so an error under a quarter of a pixel cannot be resolved by the antialiasing that draws
/// it: all three are invisible, and 2.0 is the coarsest that stays under. What it buys is not small — a
/// page of writing is millions of corners and every one of them is shaded by the GPU on every frame: the
/// same traced page is 6.98 million vertices at 1.0, 4.65 at 1.5, and 3.49 at 2.0, so 80 MB of buffer
/// became 40.
///
/// A pen's own curvature is far gentler than the bound this is checked against. The tightest turn in real
/// handwriting is the reversal at a stroke's tip, which the model draws as a disc rather than as a chain of
/// chords (see `Tip` and the arcs in `arc_between`), so the straight pieces are almost always standing in
/// for a curve of a far larger radius — where the error falls as `1/r` and 2.0 is off by a hundredth of a
/// pixel.
pub const FACET: f32 = 2.0;

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
pub fn detail_zoom(zoom: f32) -> f32 {
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

/// The view an edit acts on, built out of a page's own fields.
///
/// A free function over the fields rather than a method on the page, and that is the whole point of it: a method
/// would borrow the *page*, and the page's history has to be borrowed at the same time as the ink it is about
/// (see the app's `InkDocument::record`). Through here an edit can reach the ink, the flags over it, and the shift count
/// — and nothing else.
pub fn editable<'a>(
    finished: &'a mut Arc<Vec<Arc<Stroke>>>,
    selected: &'a mut Arc<Vec<bool>>,
    shifted: &'a mut u64,
    detailed_at: Option<f32>,
) -> history::Page<'a> {
    history::Page {
        strokes: finished,
        selected,
        shifts: shifted,
        detail: detail_of(detailed_at),
    }
}

/// The detail a page's outlines are built for, from the rung it was last detailed at.
///
/// A free function rather than a method, because an edit's view of a page is built out of the page's own fields —
/// the ink and the page's *history* are borrowed at the same time — and reading one field through `&self` would
/// borrow the whole page (see the app's `InkDocument::editable`).
pub fn detail_of(detailed_at: Option<f32>) -> f32 {
    detailed_at.unwrap_or_else(|| detail_zoom(1.0))
}

/// Whether the newest segment of a stroke is drawn as a curve or as the line its points describe.
///
/// A curve through a point needs the point *after* it, and the newest reading of a stroke under the
/// nib has none: drawn from the tip's neighbours alone, the ink would move under the pen as the user
/// writes. So the newest segment of a stroke being drawn is left straight — it is one resample
/// spacing long, usually under a pixel, and its corner with the segment before it is interpolated
/// like every other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tip {
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
/// redraw: see `STEADY_WITHIN`. A stroke written at reading size is made of points under a pixel
/// apart, and every interior point of it is steadied; a flick is made of points twenty pixels apart,
/// and none of it is.
pub fn steadied(points: &[InkPoint]) -> Vec<InkPoint> {
    if STEADYING == 0 || points.len() < 3 {
        return points.to_vec();
    }

    let mut line = points.to_vec();
    // The distance between two readings, which is what decides whether they are close enough for a
    // pass to be a steadying rather than a redraw.
    let spacing =
        |from: InkPoint, to: InkPoint| ((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt();

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
/// positions than a stroke keeps (see [`crate::settings::Settings::resample_spacing`]) — and what is left of them was
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
/// Each segment is cut into pieces no longer than [`FACET`] window pixels — at most `MAX_STEPS` of
/// them — and the width is interpolated along the pieces. A segment short enough to need one piece is
/// *copied* rather than sampled, so at 1:1 the geometry is exactly the geometry the stored points
/// describe, and writing at reading size costs nothing for any of this.
pub fn densified(points: &[InkPoint], detail: f32, tip: Tip) -> Vec<InkPoint> {
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
            line.push(InkPoint::new(
                x,
                y,
                from.width + (to.width - from.width) * t,
            ));
        }

        // The end of a segment is the point itself rather than the curve's value at the end of it:
        // those differ by rounding, and where a stroke's ink ends is not a place to be a few
        // floating-point steps away from where the pen was.
        line.push(to);
    }

    line
}

/// Whether a place on the page is ink, according to the model: **the union of the discs the nib left**.
///
/// This is the model written as the predicate it is, rather than as the boundary [`ribbon_outline`] derives
/// from it. The two are the same claim from two sides — the outline is where a point stops being ink, and this
/// is whether it is — so a test that asks this can check the model's *meaning* without triangulating anything,
/// and a test that compares the two checks that the outline is the boundary of exactly this region.
///
/// A place is ink when it is within the nib's radius of any reading, or under the tangent between two
/// readings that are further apart than that. The tangent is the straight side of the band: between two discs
/// the outside is bounded by the common outer tangent, and the strip between the tangent, the two radii and the
/// line of centres is inside the union.
///
/// `detail` is the zoom the page is drawn at, and the points are resampled at it, so the answer is the answer
/// for the stroke as it would be drawn.
/// One disc of the model: where the nib was, and how far it reached.
#[derive(Clone, Copy, Debug)]
pub struct Disc {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
}

impl Disc {
    /// Whether a place is inside this disc.
    pub fn covers(&self, x: f32, y: f32) -> bool {
        let (dx, dy) = (x - self.x, y - self.y);

        dx * dx + dy * dy <= self.radius * self.radius
    }
}

/// Two discs and the tangent between them: the straight side of a band.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub from: Disc,
    pub to: Disc,
}

impl Span {
    /// Whether a place is under the tangent between these two discs — the straight run of the band.
    ///
    /// The outside of the union of two discs is their two arcs and the common tangents, so between the two
    /// centres and closer to their line than the nib is wide is ink.
    pub fn covers(&self, x: f32, y: f32) -> bool {
        let (dx, dy) = (self.to.x - self.from.x, self.to.y - self.from.y);
        let length = (dx * dx + dy * dy).sqrt();

        if length <= 1e-6 {
            return false;
        }

        let along = (dx / length, dy / length);
        let across = (-along.1, along.0);
        let (px, py) = (x - self.from.x, y - self.from.y);

        let forward = px * along.0 + py * along.1;
        let sideways = (px * across.0 + py * across.1).abs();

        forward >= 0.0 && forward <= length && sideways <= self.from.radius.max(self.to.radius)
    }
}

/// A stroke's ink, worked out once: **the union of the discs the nib left behind.**
///
/// This is the model, computed rather than described. The points are resampled and steadied exactly once, when
/// the model is built, and every question after that — is this place ink, what is the boundary, where are its
/// bounds — is answered from the discs. That split is what makes the model usable in a test: the expensive part
/// is the *making* of a stroke's model, not the asking about it, and a scenario suite that asks a thousand
/// questions asks them all against one model.
///
/// The claim it encodes has no cases. A stroke is the discs its nib left, so:
///
/// * a nib turned where it stands is a disc, because every disc is at the same place;
/// * a stroke that doubles back is still one region, because a union has no order;
/// * an end is round, because the disc at the end is round;
/// * a corner is filled, because the disc at the corner reaches past it.
///
/// None of those are special cases in the code below, and none of them can be got wrong in it. They were four
/// separate bugs in the outline this replaced, in which the band was described by two offset *edges* — each
/// with a normal that had to be kept in step with its neighbours, and each of which could be flipped by a turn.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// The discs the nib left, in order. Order is kept only for drawing the boundary.
    pub discs: Vec<Disc>,
    /// The spans between consecutive discs: the straight runs of the band.
    pub spans: Vec<Span>,
}

impl Model {
    /// The model of a stroke's points, resampled at the zoom the page is drawn at.
    ///
    /// This is the only expensive call: it steadies, resamples and curves the points. Everything else on
    /// `Model` is arithmetic over what this returns.
    pub fn of(points: &[InkPoint], detail: f32, tip: Tip) -> Self {
        /// The thinnest a stroke is drawn, so a hairline reading still leaves a mark.
        const MIN_HALF_WIDTH: f32 = 0.35;

        if points.is_empty() {
            return Model::default();
        }

        let line = densified(points, detail, tip);
        let discs: Vec<Disc> = line
            .iter()
            .map(|point| Disc {
                x: point.x,
                y: point.y,
                radius: (point.width * 0.5).max(MIN_HALF_WIDTH),
            })
            .collect();

        let spans = discs
            .windows(2)
            .map(|pair| Span {
                from: pair[0],
                to: pair[1],
            })
            .collect();

        Model { discs, spans }
    }

    /// Whether a place on the page is ink.
    ///
    /// The whole model in one predicate: inside any disc, or under the tangent between two of them. A place the
    /// nib covered is ink; there is nothing else to be.
    pub fn is_ink(&self, x: f32, y: f32) -> bool {
        self.discs.iter().any(|disc| disc.covers(x, y))
            || self.spans.iter().any(|span| span.covers(x, y))
    }

    /// The box the ink occupies: the discs' own bounds, made a little wider than the points.
    pub fn bounds(&self) -> [f32; 4] {
        if self.discs.is_empty() {
            return [0.0, 0.0, 0.0, 0.0];
        }

        let mut low = (f32::MAX, f32::MAX);
        let mut high = (f32::MIN, f32::MIN);

        for disc in &self.discs {
            low = (
                low.0.min(disc.x - disc.radius),
                low.1.min(disc.y - disc.radius),
            );
            high = (
                high.0.max(disc.x + disc.radius),
                high.1.max(disc.y + disc.radius),
            );
        }

        [low.0, low.1, high.0, high.1]
    }

    /// How many of a set of places the model calls ink, out of how many were asked.
    ///
    /// A whole scenario in one number, which is what a test wants to assert on: `(1.0, 1.0)` is a stroke whose
    /// readings are all inked.
    pub fn inked_share(&self, places: impl IntoIterator<Item = (f32, f32)>) -> (usize, usize) {
        let mut inked = 0;
        let mut total = 0;

        for (x, y) in places {
            total += 1;

            if self.is_ink(x, y) {
                inked += 1;
            }
        }

        (inked, total)
    }
}

/// The boundary of a stroke's ink, from the model that was already worked out.
///
/// The model is one sentence — a stroke is the union of the discs its nib left, one disc per reading — and
/// the drawing is that sentence turned into points a fill can use. There is no direction to splay a nib
/// along (a nib is round), no normal to flip at a corner (a union has no orientation), no cap to add at an
/// end and no ring to close: a turned pen, a stroke that doubles back, and one that ends where it began are
/// all unions of discs that come out right without being mentioned.
///
/// ## The shape of the boundary
///
/// The outline is the edge of the union of the discs, so it is derived from [`Model`] rather than from the
/// points: the resampling happens once, in [`Model::of`], and this walks the discs that came out of it. Between
/// two discs the edge is the common outer tangent, and round each disc it is the arc no other disc covers —
/// which is a half circle at an end and the outside of the turn at a corner.
///
/// ## The shape of the boundary
///
/// Between two readings the nib draws two discs of radius `r` each, and the outside of their union is one of two
/// figures:
///
/// * **overlapping** (`d < 2r`, the usual case) — two arcs and the two *outer* common tangents, which meet
///   inside the pair. This is what makes a turn solid: the arcs are the outside of each disc, so the wedge on
///   the outside of a corner is covered by the disc at the corner and is never left as page.
/// * **apart** (`d >= 2r`) — two arcs and the two tangents, which are parallel: a straight band with round ends.
///
/// Walking the discs in order and taking the outer tangent on each side gives one closed loop for the whole
/// stroke, whatever the readings do.
///
/// ## What is not here any more
///
/// The cases this replaces were four: a disc for a pen that did not move, an annulus guard for one that turned,
/// a close test for a stroke that met itself, and a half circle at each end. Each was a bug found and fixed one
/// at a time, and the fourth was still leaving a reading unpainted at every corner. They were all symptoms of
/// describing the band by its *edges* — two offset curves with a normal to keep in step — instead of by the
/// discs that made it. The discs have no edges to keep in step.
pub fn ribbon_outline(model: &Model) -> Vec<[f32; 2]> {
    /// How many points a round end and a turned reading are drawn with.
    const ARC_STEPS: usize = 16;

    let line = &model.discs;

    if line.is_empty() {
        return Vec::new();
    }

    if line.len() == 1 {
        return arc_points(line[0], 0.0, std::f32::consts::TAU, ARC_STEPS);
    }

    let last = line.len() - 1;

    // A nib that never left the place it was put is a disc, and nothing else: every reading is at the same
    // point, so every tangent is undefined and the union of the discs is the one disc. This is the shape a
    // reader makes scrubbing a spot, and it is the model's own answer rather than a case invented for it.
    //
    // The disc is the *enclosing* one: centred where the readings are, and wide enough to reach the furthest
    // reading plus its own nib. A hand that wobbles as it scrubs pushed the ink that far out, so a disc drawn at
    // the nib's plain width would be smaller than the model says the mark is.
    let mut low = (f32::MAX, f32::MAX);
    let mut high = (f32::MIN, f32::MIN);

    for disc in line {
        low = (low.0.min(disc.x), low.1.min(disc.y));
        high = (high.0.max(disc.x), high.1.max(disc.y));
    }

    let widest = line
        .iter()
        .fold(0.0f32, |widest, disc| widest.max(disc.radius * 2.0));

    if (high.0 - low.0).max(high.1 - low.1) <= widest * 0.5 {
        let total = line.iter().fold((0.0, 0.0), |total, disc| {
            (total.0 + disc.x, total.1 + disc.y)
        });
        let count = line.len() as f32;
        let centre = (total.0 / count, total.1 / count);

        // How far the furthest reading is from that centre, on top of its own reach.
        let reach = line.iter().fold(0.0f32, |reach, disc| {
            let (dx, dy) = (disc.x - centre.0, disc.y - centre.1);

            reach.max((dx * dx + dy * dy).sqrt() + disc.radius)
        });

        return arc_points(
            Disc {
                x: centre.0,
                y: centre.1,
                radius: reach,
            },
            0.0,
            std::f32::consts::TAU,
            ARC_STEPS,
        );
    }

    // The two edges, walked forwards: each disc contributes the point its tangents to the discs either side
    // meet at, which is the outside of the disc there.
    let mut left: Vec<[f32; 2]> = Vec::with_capacity(line.len());
    let mut right: Vec<[f32; 2]> = Vec::with_capacity(line.len());

    for index in 0..line.len() {
        let point = line[index];
        let previous = line[index.saturating_sub(1)];
        let next = line[(index + 1).min(last)];

        left.push(edge_point(previous, point, next, Side::Left));
        right.push(edge_point(previous, point, next, Side::Right));
    }

    // The loop: the left edge forwards, round the far disc, the right edge back, and round the near one.
    let mut outline: Vec<[f32; 2]> = Vec::with_capacity(line.len() * 2 + ARC_STEPS * 2);

    // The left edge, with the disc's own arc wherever the band turns hard enough that the two straight
    // pieces at a reading meet outside the disc. That arc is not a special case either: it is the part of that
    // reading's disc that no other disc covers, which on a sharp corner is the outside of the turn — and where
    // the pen turned round and came back, it is the whole tip.
    outline.push(left[0]);

    for index in 1..line.len() {
        let point = line[index];

        outline.extend(arc_between(point, left[index - 1], left[index], ARC_STEPS));
        outline.push(left[index]);
    }

    outline.extend(end_arc(
        line[last],
        left[last],
        right[last],
        [
            line[last.saturating_sub(1)].x,
            line[last.saturating_sub(1)].y,
        ],
        ARC_STEPS,
    ));

    for index in (0..line.len()).rev() {
        let point = line[index];

        outline.push(right[index]);

        if index > 0 {
            outline.extend(arc_between(
                point,
                right[index],
                right[index - 1],
                ARC_STEPS,
            ));
        }
    }

    outline.extend(end_arc(
        line[0],
        right[0],
        left[0],
        [line[(1).min(last)].x, line[(1).min(last)].y],
        ARC_STEPS,
    ));

    outline
}

/// Which of the two outer tangents of a pair of discs a point is taken on.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

/// Where the band's edge is at a reading: on the tangents to the discs either side of it.
///
/// The two tangents are averaged rather than one being chosen, because at a corner that average *is* the
/// corner: extending both straight pieces of edge until they meet. A disc with a neighbour on one side only
/// takes that tangent, which is a reading at either end of the line.
fn edge_point(previous: Disc, point: Disc, next: Disc, side: Side) -> [f32; 2] {
    match (
        outer_tangent(previous, point, side),
        outer_tangent(point, next, side),
    ) {
        (Some(from), Some(to)) => {
            // The two tangents meet at the corner, and their average is the meeting point. Where they cancel —
            // the pen turned round and came back along its own line, so its next reading is the one before it —
            // the average is the reading itself, and the band would pinch to nothing there. That pinch is not a
            // point: it is the whole disc at the turn, and the disc is drawn by the arc this reading gets. So the
            // tangent to the reading it *came from* is the edge through it, and the arc round the turn is the
            // outside.
            let average = [(from[0] + to[0]) * 0.5, (from[1] + to[1]) * 0.5];
            let (ax, ay) = (average[0] - point.x, average[1] - point.y);
            let (fx, fy) = (from[0] - point.x, from[1] - point.y);

            if (ax * ax + ay * ay).sqrt() < (fx * fx + fy * fy).sqrt() * 0.25 {
                from
            } else {
                average
            }
        }
        (Some(point), None) | (None, Some(point)) => point,
        // Neither neighbour has a direction: the pen went down and lifted again in one place.
        (None, None) => {
            let angle = match side {
                Side::Left => -std::f32::consts::FRAC_PI_2,
                Side::Right => std::f32::consts::FRAC_PI_2,
            };

            [
                point.x + point.radius * angle.cos(),
                point.y + point.radius * angle.sin(),
            ]
        }
    }
}

/// Where the outer tangent from `from` touches `to`, on `side`.
///
/// For two circles the outer tangents are the ones crossing neither circle. Their touch points differ by one
/// vector on both circles, and that vector has the direction it has because the tangent meets both radii at
/// a right angle: along the centres by `r_to - r_from`, and across them by `sqrt(d^2 - (r_to - r_from)^2)`.
/// With equal radii — which is what one nib draws — the along part is nothing and the across part is the
/// whole distance, so the touch point is simply a radius off the line of centres. Unequal ones tilt it, which
/// is what a stroke that thins as it travels needs.
///
/// `None` where the two discs are the same disc, because there is no tangent out of a place to nowhere.
fn outer_tangent(from: Disc, to: Disc, side: Side) -> Option<[f32; 2]> {
    /// Nearer than this and the two discs are one disc.
    const FLOOR: f32 = 1e-6;

    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let distance = (dx * dx + dy * dy).sqrt();

    if distance <= FLOOR {
        return None;
    }

    let along = (dx / distance, dy / distance);
    let across = match side {
        Side::Left => (-along.1, along.0),
        Side::Right => (along.1, -along.0),
    };

    let difference = to.radius - from.radius;
    let reach = (distance * distance - difference * difference)
        .max(0.0)
        .sqrt();

    // The direction the touch points are offset from the centres, which the radius then scales.
    let direction = (
        along.0 * difference + across.0 * reach,
        along.1 * difference + across.1 * reach,
    );

    Some([
        from.x + direction.0 * from.radius / distance,
        from.y + direction.1 * from.radius / distance,
    ])
}

/// `steps` points along a disc's edge, starting at `start` and turning through `sweep` radians.
fn arc_points(centre: Disc, start: f32, sweep: f32, steps: usize) -> Vec<[f32; 2]> {
    let mut points = Vec::with_capacity(steps);

    for step in 0..steps {
        let angle = start + sweep * step as f32 / steps as f32;

        points.push([
            centre.x + centre.radius * angle.cos(),
            centre.y + centre.radius * angle.sin(),
        ]);
    }

    points
}

/// The boundary of a stroke's ink, from its points. A convenience over [`Model::of`] and [`ribbon_outline`].
pub fn outline_of(points: &[InkPoint], detail: f32, tip: Tip) -> Vec<[f32; 2]> {
    ribbon_outline(&Model::of(points, detail, tip))
}

///
/// The edge of a band is straight between two readings, so at each reading it has to get from the tangent it
/// came in on to the tangent it goes out on. Where those two meet *inside* the disc there is nothing to draw —
/// the corner is covered. Where they meet outside it, the edge has to follow the disc's own rim round the
/// outside, which is this arc. On a corner sharper than the nib is wide it is a long arc; at a tip, where the
/// pen turned round and came back, it is nearly a whole circle.
///
/// `outwards` is the reading the edge came from; the sweep kept is the one going away from it.
fn arc_between(end: Disc, from: [f32; 2], to: [f32; 2], steps: usize) -> Vec<[f32; 2]> {
    // Both points are on the disc's rim by construction, so their angles round it are the angles of the arc.
    // Measuring them from the centre is only safe if they really are on the rim: a point that floated inside
    // would swing through a wild angle and draw an arc that is not there. Where one is not on the rim the edge
    // runs straight between them and there is no arc to draw.
    let on_rim = |point: [f32; 2]| {
        let (dx, dy) = (point[0] - end.x, point[1] - end.y);
        let reach = (dx * dx + dy * dy).sqrt();

        (reach - end.radius).abs() <= end.radius * 0.5 + 1e-3
    };

    if !on_rim(from) || !on_rim(to) {
        return Vec::new();
    }

    let start = (from[1] - end.y).atan2(from[0] - end.x);
    let finish = (to[1] - end.y).atan2(to[0] - end.x);

    let mut sweep = finish - start;

    while sweep > std::f32::consts::PI {
        sweep -= std::f32::consts::TAU;
    }
    while sweep < -std::f32::consts::PI {
        sweep += std::f32::consts::TAU;
    }

    // A turn of nearly nothing is a straight run, and an arc there would double the edge over the straight
    // piece it is already drawn as — which is the difference between a band painted once and painted twice
    // over. The arc is only boundary where the corner is sharper than the straight pieces can cover.
    if sweep.abs() < TURN {
        return Vec::new();
    }

    arc_points(end, start, sweep, steps)
}

/// How sharp a turn has to be before its disc contributes an arc: about 20°, which is a corner a reader can
/// see. Below it the two straight pieces already meet inside the disc, so the edge is those pieces and an arc
/// added over them would be the same stretch of band drawn twice.
const TURN: f32 = 0.35;

///
/// This is the round cap, and it is not added to the model — it is what the end disc already is. It runs from
/// one edge point to the other, and of the two ways round the disc it takes the one that passes through the
/// point furthest from the reading behind: that is the tip of the nib, and the half beyond it is the half no
/// part of the stroke lies in. Going the other way would sweep across the band's own width, and the tip of the
/// stroke would be the part left unpainted.
fn end_arc(
    end: Disc,
    from: [f32; 2],
    to: [f32; 2],
    behind: [f32; 2],
    steps: usize,
) -> Vec<[f32; 2]> {
    let start = (from[1] - end.y).atan2(from[0] - end.x);
    let finish = (to[1] - end.y).atan2(to[0] - end.x);

    // Where the way out of the disc points, as an angle: directly away from the reading behind.
    let out = (end.y - behind[1]).atan2(end.x - behind[0]);

    // The sweep that goes through the tip: the one whose midpoint is the way out. Of the two candidates, the
    // one nearer `out` is it.
    let mut sweep = finish - start;

    while sweep > std::f32::consts::TAU {
        sweep -= std::f32::consts::TAU;
    }
    while sweep < -std::f32::consts::TAU {
        sweep += std::f32::consts::TAU;
    }

    let normalise = |angle: f32| {
        let mut angle = angle;

        while angle > std::f32::consts::PI {
            angle -= std::f32::consts::TAU;
        }
        while angle < -std::f32::consts::PI {
            angle += std::f32::consts::TAU;
        }

        angle
    };

    let forward = normalise(start + sweep * 0.5 - out).abs();
    let backward =
        normalise(start + (sweep - std::f32::consts::TAU * sweep.signum()) * 0.5 - out).abs();

    if backward < forward {
        sweep -= std::f32::consts::TAU * sweep.signum();
    }

    arc_points(end, start, sweep, steps)
}

/// How many straight pieces one segment of a stroke is drawn as.
///
/// The segment is measured *after* the zoom, because that is what decides whether a piece of it is
/// visible, and rounded up so that a piece is never longer than [`FACET`] — with a bound at each end:
/// never fewer than one, because a segment is a piece of the line at the very least, and never more
/// than `MAX_STEPS`.
pub fn steps_for(from: InkPoint, to: InkPoint, detail: f32) -> usize {
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
