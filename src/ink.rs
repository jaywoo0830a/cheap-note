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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InkTransform {
    /// Physical pixels to logical pixels: the window's DPI scale factor.
    pub scale: f32,
    /// How large the sheet is drawn, relative to its own size.
    pub zoom: f32,
    /// The sheet's top-left corner, in window logical pixels.
    pub origin: (f32, f32),
}

impl Default for InkTransform {
    fn default() -> Self {
        InkTransform::identity()
    }
}

impl InkTransform {
    /// The reading's own coordinates: what a test with no window wants.
    pub const fn identity() -> Self {
        InkTransform {
            scale: 1.0,
            zoom: 1.0,
            origin: (0.0, 0.0),
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
/// The width is baked in when the point is created, so changing the stroke-width setting does
/// not retroactively redraw the ink the user already laid down.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    /// The positions, oldest first.
    pub points: Vec<InkPoint>,
    /// The colour this stroke was written in, as `0xRRGGBB`.
    ///
    /// Per stroke, not per page: the palette is a set of pens, and picking up a different one must
    /// not repaint what the others wrote. A note written before this field existed loads as
    /// [`Stroke::DEFAULT_COLOR`] — the ink colour those notes were drawn in, since there was only
    /// ever one.
    #[serde(default = "Stroke::default_color")]
    pub color: u32,
    /// The ribbon outline, in logical pixels, cached when the stroke is closed.
    ///
    /// Recomputing the outline every frame is pure waste: the points never change once the
    /// stroke is finished. Not serialised, because it is derivable from `points`.
    #[serde(skip, default)]
    pub outline: Vec<[f32; 2]>,
    /// The axis-aligned bounds as `[min_x, min_y, max_x, max_y]`, for culling and hit-testing.
    #[serde(skip, default)]
    pub bounds: [f32; 4],
}

impl Stroke {
    /// The colour strokes from a note without one are given.
    ///
    /// The near-black the app drew everything in before a stroke could carry its own colour: an old
    /// note therefore opens looking exactly as it did.
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

    /// The default for the field's `serde` attribute, which wants a path it can call.
    fn default_color() -> u32 {
        Self::DEFAULT_COLOR
    }

    /// Whether this stroke has nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Freezes the stroke: computes its bounds and its cached ribbon outline.
    ///
    /// Called once, when the pen lifts, so the render path only has to walk the outline.
    pub fn close(&mut self) {
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
        self.outline = ribbon_outline(&self.points);
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

/// Builds the filled ribbon outline of a polyline with a per-point width.
///
/// The outline walks the line on both sides — the left offset side forward, the right offset
/// side backward — and closes the polygon. Filling that polygon is what gives a stroke a
/// width that varies with the pen's force; a constant-width stroke would need only a polyline.
fn ribbon_outline(points: &[InkPoint]) -> Vec<[f32; 2]> {
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

    let last = points.len() - 1;
    let mut left = Vec::with_capacity(points.len());
    let mut right = Vec::with_capacity(points.len());

    for index in 0..points.len() {
        // The direction through a point is its neighbours' difference, which is what keeps a
        // sharp corner from pinching the ribbon.
        let previous = points[index.saturating_sub(1)];
        let next = points[(index + 1).min(last)];

        let mut dx = next.x - previous.x;
        let mut dy = next.y - previous.y;
        let length = (dx * dx + dy * dy).sqrt();
        if length <= f32::EPSILON {
            dx = 1.0;
            dy = 0.0;
        } else {
            dx /= length;
            dy /= length;
        }

        // The unit normal of the direction: the direction across the line.
        let normal_x = -dy;
        let normal_y = dx;
        let half_width = (points[index].width * 0.5).max(MIN_HALF_WIDTH);

        left.push([
            points[index].x + normal_x * half_width,
            points[index].y + normal_y * half_width,
        ]);
        right.push([
            points[index].x - normal_x * half_width,
            points[index].y - normal_y * half_width,
        ]);
    }

    left.extend(right.into_iter().rev());
    left
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
    /// What the model has done since the app started.
    stats: InkStats,
}

impl Default for InkDocument {
    fn default() -> Self {
        InkDocument {
            finished: Arc::new(Vec::new()),
            open: None,
            active_pointer: None,
            active_tool: Tool::Pen,
            mode: Tool::Pen,
            last_sample: None,
            last_erase: None,
            stats: InkStats::default(),
        }
    }
}

impl InkDocument {
    /// An empty page.
    pub fn new() -> Self {
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

    /// Removes the most recent stroke.
    pub fn undo(&mut self) -> bool {
        if let Some(open) = self.open.take() {
            // An in-progress stroke is the most recent thing the user drew.
            self.active_pointer = None;
            return !open.is_empty();
        }

        if self.finished.is_empty() {
            return false;
        }

        Arc::make_mut(&mut self.finished).pop();
        self.last_erase = None;
        true
    }

    /// Removes every stroke.
    pub fn clear(&mut self) {
        self.finished = Arc::new(Vec::new());
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
            ..InkDocument::default()
        }
    }

    /// Feeds a batch of pen readings to the model.
    ///
    /// `transform` is how a physical reading becomes a place on the sheet: the window's DPI scale,
    /// the sheet's zoom and where the sheet is drawn. All three are applied here, once, so that
    /// nothing downstream has to know about any of them — the ink, the eraser and the geometry all
    /// work in the sheet's own coordinates.
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

            let (x, y) = transform.sheet_point((sample.pixel.x, sample.pixel.y));

            match sample.phase {
                PenPhase::Down => {
                    // A `Down` closes whatever the previous pointer left open: the ink the user
                    // drew is kept, and an `Up` that never arrived is no reason to lose it.
                    self.finish_open();

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
                            self.open =
                                Some(Stroke::new(InkPoint::new(x, y, width), settings.ink_color));
                        }
                    }
                    changed = true;
                }

                PenPhase::Move => {
                    if self.active_pointer == Some(sample.id) {
                        match self.active_tool {
                            Tool::Eraser => self.erase_at(x, y, settings),
                            Tool::Pen => self.push_point(x, y, sample, settings, false),
                        }
                        changed = true;
                    }
                }

                PenPhase::Up => {
                    if self.active_pointer == Some(sample.id) {
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
                stroke.close();
                // `make_mut` reuses the existing vector when no frame is holding a snapshot, and
                // copies it when one is — and that copy is of pointers, not of ink.
                Arc::make_mut(&mut self.finished).push(Arc::new(stroke));
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

        self.stats.erased_strokes += (before - self.finished.len()) as u64;
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
#[derive(Debug, Default)]
pub struct Notes {
    /// Which page [`Self::current`] is the ink of.
    page: usize,
    /// The ink of the page being written on.
    current: InkDocument,
    /// The ink of every other page that has been written on.
    taken: BTreeMap<usize, InkDocument>,
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

    /// Moves to a page, taking the ink of the page being left behind with it.
    ///
    /// The page being moved to keeps its own ink, which is the whole point: what was drawn there is
    /// still there on the way back.
    pub fn go_to(&mut self, page: usize) {
        if page == self.page {
            return;
        }

        let leaving = std::mem::take(&mut self.current);
        if !leaving.is_blank() {
            self.taken.insert(self.page, leaving);
        }

        self.current = self.taken.remove(&page).unwrap_or_default();
        self.page = page;
    }

    /// The ink of any page, whether or not it has been written on.
    pub fn page_ink(&self, page: usize) -> Option<&InkDocument> {
        if page == self.page {
            return Some(&self.current);
        }

        self.taken.get(&page).filter(|ink| !ink.is_blank())
    }

    /// The pages that hold ink, in page order: what a save writes out.
    pub fn written_pages(&self) -> Vec<usize> {
        let mut pages: Vec<usize> = self
            .taken
            .iter()
            .filter(|(_, ink)| !ink.is_blank())
            .map(|(page, _)| *page)
            .collect();

        if !self.current.is_blank() {
            pages.push(self.page);
        }

        pages.sort_unstable();
        pages.dedup();
        pages
    }

    /// How many pages this note has, for turning between them.
    ///
    /// A note written on a blank sheet has no document to ask, so its own pages are the count:
    /// the last page written on, plus one. It never reports zero, because there is always the page
    /// in front of the user.
    pub fn page_count(&self) -> usize {
        self.written_pages().last().map_or(1, |page| page + 1)
    }

    /// Replaces every page's ink, moving to `page` — what opening a saved note does.
    pub fn replace(&mut self, pages: Vec<(usize, InkDocument)>, page: usize) {
        self.taken.clear();
        self.page = page;

        let mut current = InkDocument::new();
        for (index, ink) in pages {
            if index == page {
                current = ink;
            } else if !ink.is_blank() {
                self.taken.insert(index, ink);
            }
        }

        self.current = current;
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
            self.current = self.taken.remove(&(page + 1)).unwrap_or_default();
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

    /// The edges are what make a stroke: a down, positions, and an up.
    #[test]
    fn a_down_and_up_make_one_stroke() {
        let mut ink = InkDocument::new();
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

    /// Each stroke keeps the pen it was written with.
    ///
    /// The palette is a set of pens, not a page setting: choosing a different colour has to leave
    /// what the previous one wrote exactly as it was, which is only true if the colour is stamped
    /// into the stroke when the nib goes down.
    #[test]
    fn a_stroke_keeps_the_colour_it_was_written_in() {
        let mut ink = InkDocument::new();
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

    /// A note written before a stroke carried a colour of its own loads as the colour it was drawn
    /// in: there was only ever one, and it was this.
    #[test]
    fn a_stroke_without_a_colour_loads_as_the_one_ink_used_to_be() {
        let old = r#"{"points":[{"x":1.0,"y":2.0,"width":3.0}]}"#;
        let stroke: Stroke = serde_json::from_str(old).expect("an older stroke still parses");

        assert_eq!(stroke.color, Stroke::DEFAULT_COLOR);
        assert_eq!(stroke.points.len(), 1);
    }

    /// A cancel ends the stroke but does not add the position it carries.
    #[test]
    fn a_cancel_ends_the_stroke_without_its_position() {
        let mut ink = InkDocument::new();
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
        let mut ink = InkDocument::new();
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
        let mut ink = InkDocument::new();
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

    /// A pen with no pressure sensor draws at the constant width, not at zero.
    #[test]
    fn a_pen_with_no_sensor_draws_a_constant_width() {
        let mut ink = InkDocument::new();
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
        let mut ink = InkDocument::new();
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
        let mut ink = InkDocument::new();
        let s = settings();

        for index in 0..3 {
            let x = index as f32 * 100.0;
            ink.consume(&[reading(7, PenPhase::Down, x, 0.0, Some(0.5))], &id(), &s);
            ink.consume(&[reading(7, PenPhase::Up, x + 10.0, 0.0, None)], &id(), &s);
        }
        assert_eq!(ink.stroke_count(), 3);

        assert!(ink.undo());
        assert_eq!(ink.stroke_count(), 2);
        assert!(ink.undo());
        assert!(ink.undo());
        assert!(!ink.undo(), "there is nothing left to undo");
        assert!(ink.is_blank());
    }

    /// Physical pixels are divided by the window's scale exactly once.
    #[test]
    fn the_dpi_scale_is_applied_once() {
        let mut ink = InkDocument::new();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 150.0, 90.0, Some(0.5))], &scaled(1.5), &s);
        ink.consume(&[reading(7, PenPhase::Up, 300.0, 180.0, None)], &scaled(1.5), &s);

        assert_eq!(ink.finished()[0].points[0].x, 100.0);
        assert_eq!(ink.finished()[0].points[1].y, 120.0);
    }

    /// The eraser removes the strokes it passes over and leaves the rest alone.
    #[test]
    fn the_eraser_removes_only_what_it_touches() {
        let mut ink = InkDocument::new();
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
        let mut ink = InkDocument::new();
        let s = settings();

        // A sheet drawn at 2x, its top-left corner 100 px into the window.
        let sheet = zoomed(2.0, (100.0, 60.0));
        ink.consume(&[reading(7, PenPhase::Down, 300.0, 160.0, Some(0.5))], &sheet, &s);

        let point = ink.open().expect("a stroke").points[0];
        assert_eq!((point.x, point.y), (100.0, 50.0), "(300-100)/2, (160-60)/2");

        // The same reading on a sheet at its own size, drawn at the origin, is the reading.
        let mut flat = InkDocument::new();
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
        };

        assert_eq!(broken.sheet_point((30.0, 30.0)), (20.0, 20.0));
    }

    /// A stroke's bounds follow it as it grows, so a frame can ask whether the stroke being drawn
    /// is on screen before the stroke has ever been closed.
    #[test]
    fn a_growing_stroke_knows_where_it_is() {
        let mut ink = InkDocument::new();
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
        let mut ink = InkDocument::new();
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
        assert_eq!(notes.written_pages(), vec![0, 1], "and both pages are written on");

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
        assert_eq!(notes.written_pages(), vec![3], "only the page with ink");
        assert_eq!(notes.stroke_count(), 1, "and its ink came back with it");
        assert_eq!(notes.page_count(), 4, "pages 0..=3 exist for turning");
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
        assert_eq!(notes.written_pages(), vec![0, 2]);

        // A page is inserted where page 1 was, and the reader turns to it.
        notes.insert_at(1);
        notes.go_to(1);

        assert!(notes.is_blank(), "the inserted page has no ink");
        assert_eq!(
            notes.written_pages(),
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
        assert_eq!(notes.written_pages(), vec![0, 1, 2]);

        // The page being shown is the one deleted: the reader lands on what followed it.
        notes.remove_at(1);

        assert_eq!(notes.written_pages(), vec![0, 1]);
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

    /// Opening a note puts the ink where it was written, on the page that was open.
    #[test]
    fn replacing_a_note_restores_its_pages() {
        let mut notes = Notes::new();
        let s = settings();
        notes.consume(&[reading(7, PenPhase::Down, 1.0, 1.0, Some(0.5))], &id(), &s);
        notes.consume(&[reading(7, PenPhase::Up, 2.0, 2.0, None)], &id(), &s);

        let pages = vec![(2, page_with(3)), (5, page_with(4))];
        notes.replace(pages, 5);

        assert_eq!(notes.stroke_count(), 4, "the page that was open is the one on screen");
        assert_eq!(notes.written_pages(), vec![2, 5]);
        notes.go_to(2);
        assert_eq!(notes.stroke_count(), 3, "and the other page is where it was saved");
    }

    /// A page of `count` two-point strokes, each 10 px apart, for the tests above.
    fn page_with(count: usize) -> InkDocument {
        let mut ink = InkDocument::new();
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
        let mut ink = InkDocument::new();

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
        let mut ink = InkDocument::new();
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
