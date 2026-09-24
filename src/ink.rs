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
//! the display's rate; cloning an `Arc` is a pointer copy. A new `Arc` is built only when the
//! ink actually changes (a stroke ends, undo, clear), which happens once per stroke.

use std::sync::Arc;

use pen_windows::{PenPhase, PenSample};
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

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
    /// A stroke beginning at one point.
    pub fn new(point: InkPoint) -> Self {
        Stroke {
            points: vec![point],
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
    finished: Arc<Vec<Stroke>>,
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
    pub fn finished(&self) -> &Arc<Vec<Stroke>> {
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

        let mut finished = (*self.finished).clone();
        finished.pop();
        self.finished = Arc::new(finished);
        true
    }

    /// Removes every stroke.
    pub fn clear(&mut self) {
        self.finished = Arc::new(Vec::new());
        self.open = None;
        self.active_pointer = None;
        self.last_erase = None;
    }

    /// Feeds a batch of pen readings to the model.
    ///
    /// `scale` is the window's DPI scale factor: `pen-windows` reports **physical** client
    /// pixels, while GPUI lays out and paints in **logical** ones, so the two must be brought
    /// together exactly once, here.
    ///
    /// Returns whether anything changed, which is what the caller uses to decide whether a
    /// repaint is worth scheduling.
    pub fn consume(&mut self, samples: &[PenSample], scale: f32, settings: &Settings) -> bool {
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let mut changed = false;

        for sample in samples {
            self.stats.readings += 1;

            let x = sample.pixel.x / scale;
            let y = sample.pixel.y / scale;

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
                            self.open = Some(Stroke::new(InkPoint::new(x, y, width)));
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
                let mut finished = (*self.finished).clone();
                finished.push(stroke);
                self.finished = Arc::new(finished);
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

        let before = self.finished.len();
        let kept: Vec<Stroke> = self
            .finished
            .iter()
            .filter(|stroke| !stroke.hits(x, y, settings.erase_radius))
            .cloned()
            .collect();

        if kept.len() != before {
            self.stats.erased_strokes += (before - kept.len()) as u64;
            self.finished = Arc::new(kept);
        }
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

    /// The edges are what make a stroke: a down, positions, and an up.
    #[test]
    fn a_down_and_up_make_one_stroke() {
        let mut ink = InkDocument::new();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Move, 14.0, 10.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Up, 18.0, 10.0, None)], 1.0, &s);

        assert_eq!(ink.stroke_count(), 1);
        assert!(ink.open().is_none(), "the lift closed the stroke");
        let stroke = &ink.finished()[0];
        assert_eq!(stroke.points.len(), 3, "down, move and the lift");
        assert_eq!(stroke.points[2].x, 18.0, "the lift is the last point");
    }

    /// A cancel ends the stroke but does not add the position it carries.
    #[test]
    fn a_cancel_ends_the_stroke_without_its_position() {
        let mut ink = InkDocument::new();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Move, 20.0, 10.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Cancel, 999.0, 999.0, None)], 1.0, &s);

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

        ink.consume(&[reading(7, PenPhase::Down, 10.0, 10.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(9, PenPhase::Move, 500.0, 10.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Up, 20.0, 10.0, None)], 1.0, &s);

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
            1.0,
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

        ink.consume(&[reading(7, PenPhase::Down, 0.0, 0.0, None)], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Move, 20.0, 0.0, None)], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Up, 40.0, 0.0, None)], 1.0, &s);

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

        ink.consume(&[reading(7, PenPhase::Down, 0.0, 0.0, Some(0.5))], 1.0, &s);
        for step in 1..=9 {
            // Each reading is 1 px from the last: all of them are below the spacing.
            ink.consume(
                &[reading(7, PenPhase::Move, step as f32, 0.0, Some(0.5))],
                1.0,
                &s,
            );
        }
        ink.consume(&[reading(7, PenPhase::Up, 9.5, 0.0, None)], 1.0, &s);

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
            ink.consume(&[reading(7, PenPhase::Down, x, 0.0, Some(0.5))], 1.0, &s);
            ink.consume(&[reading(7, PenPhase::Up, x + 10.0, 0.0, None)], 1.0, &s);
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

        ink.consume(&[reading(7, PenPhase::Down, 150.0, 90.0, Some(0.5))], 1.5, &s);
        ink.consume(&[reading(7, PenPhase::Up, 300.0, 180.0, None)], 1.5, &s);

        assert_eq!(ink.finished()[0].points[0].x, 100.0);
        assert_eq!(ink.finished()[0].points[1].y, 120.0);
    }

    /// The eraser removes the strokes it passes over and leaves the rest alone.
    #[test]
    fn the_eraser_removes_only_what_it_touches() {
        let mut ink = InkDocument::new();
        let s = settings();

        ink.consume(&[reading(7, PenPhase::Down, 0.0, 0.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Up, 40.0, 0.0, None)], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Down, 500.0, 0.0, Some(0.5))], 1.0, &s);
        ink.consume(&[reading(7, PenPhase::Up, 540.0, 0.0, None)], 1.0, &s);
        assert_eq!(ink.stroke_count(), 2);

        // The eraser nib touches the first stroke only.
        let mut eraser = reading(7, PenPhase::Down, 20.0, 0.0, None);
        eraser.eraser = true;
        ink.consume(&[eraser], 1.0, &s);

        assert_eq!(ink.stroke_count(), 1);
        assert_eq!(ink.finished()[0].points[0].x, 500.0);
        assert_eq!(ink.stats().erased_strokes, 1);
    }
}
