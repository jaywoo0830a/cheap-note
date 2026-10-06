//! The on-screen cursor: where the pen is, and the way it leans.
//!
//! ## Why the application draws the cursor
//!
//! A Windows cursor is a fixed bitmap. The system can choose *which* arrow to show; it cannot
//! choose the angle, and there is no arrow that means "a pen held at 40 degrees". The pen is not a
//! point — it is a stick held at an angle, and on a digitizer that reports tilt that angle is real
//! data — so the only way to show it is to draw it. That is what this module describes: a ghost of
//! the pen's body, drawn from the nib in the direction the pen leans, with a length that is the
//! lean.
//!
//! ## What the platform does about it
//!
//! A Windows cursor is a fixed bitmap: the system can choose *which* cursor to show, never which
//! angle, so a rotated cursor has to be drawn by the application — which is what this module is
//! for. The system pointer itself is taken out of the way by [`crate::system_cursor`] while this
//! ghost is drawn, so the ghost *is* the cursor rather than a marker beside one.
//!
//! ## The convention, which is the whole design
//!
//! `pen-windows` says what each axis means: `tilt_x` is positive to the right and `tilt_y` is
//! positive toward the user, and the pen's axis is `(tan tilt_x, tan tilt_y, 1)`. So the body of
//! the pen extends from the nib along
//!
//! ```text
//!     direction = normalize(tan(tilt_x), tan(tilt_y))
//! ```
//!
//! in **window** coordinates — where x grows to the right and y grows *down*, toward the user,
//! which is the same way round as the reading. There is no sign flip and no mirror: lean the pen
//! to the right and the ghost extends to the right of the nib, which is where the hand is.
//!
//! The lean from the vertical is `Tilt::from_normal` — the tangents, not a sum of angles — and the
//! drawn length is a pen's projection onto the screen, `length * sin(lean)`. Standing straight up,
//! no body shows at all; laid flat, the body is at its longest. So the *direction* of the ghost is
//! the compass bearing of the lean, and its *length* is the angle.
//!
//! ## How it is drawn
//!
//! Not by a frame: the ghost is drawn in a window of its own, straight from the pen thread, by
//! [`crate::cursor_overlay`] — a cursor is a *position*, and a frame is one frame too late for one.
//! What this module owns is the shape, and it is the same shape either way.
//!
//! Two shapes, each with a soft edge: the pen's body — a slender spindle that swells away from the
//! nib and ends in a round cap, drawn as three quadratic curves — and a mark at the nib that is as
//! wide as the line in hand. The body is painted three times — a cast shadow, then its soft edge,
//! then the body itself, widest and faintest first — and the mark twice, its bloom under it, so the
//! ghost sits *in* the page rather than on top of it. That is the same trick the sheet's own shadow
//! uses, in fewer steps, because this shape is small and is rebuilt on every reading the hand moves.
//!
//! The two shapes say different things, and are weighted for it: the nib mark is exact and nearly
//! opaque, because it is where the ink will land *and how thick that ink will be* — it is drawn at
//! the pen's own weight, so a heavier pen has a heavier dot (see [`nib_radius_for_width`]) — while
//! the body is a hint about the angle, drawn faintly and no more strongly than the lean it is
//! reporting. Its shadow is what makes a faint rod findable on paper that is nearly the colour of
//! the rod itself.

use pen_windows::{PenPhase, PenSample, Tilt};

/// How long the drawn body is, in logical pixels, when the pen is laid flat.
///
/// A real pen is about 140 mm, which at this sheet's scale is longer than a hand wants on screen,
/// so the length is chosen for the eye and only the lean scales it. It is deliberately shorter than
/// a pen *and* fainter than the nib mark: the body is a hint about the angle, and a long dark one
/// competes with the page it is drawn on.
/// Public because the ghost cursor is no longer drawn by the frame: the window it is drawn in has
/// to be big enough for the body, and that is the longest this number can be (see
/// [`crate::cursor_overlay`]).
pub const BODY_LENGTH: f32 = 88.0;

/// The least lean that draws a body, in degrees.
///
/// Below this the direction is dominated by the digitizer's own noise, and a stub of a pen that
/// swings around the nib is worse than no body at all: the nib mark already says where the ink is.
const MIN_LEAN_DEGREES: f32 = 4.0;

/// The lean at which the body is drawn at full strength, in degrees.
///
/// Between [`MIN_LEAN_DEGREES`] and this the body gains its weight as it gains its length, so there
/// is no lean at which a shape appears out of nothing: it arrives as the faintest of stubs and
/// settles into a pen. See [`PenCursor::body_fade`].
const FULL_LEAN_DEGREES: f32 = 16.0;

/// How wide the drawn body is at the nib, in logical pixels.
const BODY_NIB_WIDTH: f32 = 1.2;

/// How wide the drawn body is at its far end, in logical pixels.
///
/// The taper is what makes the shape read as a pen rather than as a smudge, and it is also what
/// carries the direction: the narrow end is the nib, so the ghost cannot be read the wrong way
/// round.
const BODY_TAIL_WIDTH: f32 = 3.6;

/// Where the body's edge swells, as a fraction of its length from the nib.
///
/// The flanks are curves through a control point here, rather than straight edges between the nib
/// and the far end. That is the whole difference between a body that rounds outward toward its far
/// end and a wedge: a wedge has an apex pointing at the nib, and an apex is the one shape a pen is
/// not.
const BODY_BULGE: f32 = 0.72;

/// The least and the most the nib mark is ever drawn at, in logical pixels.
///
/// The mark's size is the width of the line in hand — see [`nib_radius_for_width`] — so the number that matters is the
/// app's, and these two are what keeps an answer the app had no business sending from drawing a mark nobody can find,
/// or one that fills the window. The ceiling is half a highlighter's band, because a highlighter's mark *is* the band
/// it is about to lay, and the floor keeps the thinnest pen's mark a dot.
pub const NIB_RADIUS_MIN: f32 = 1.2;
pub const NIB_RADIUS_MAX: f32 = 9.0;

/// The radius of the nib mark for a line of the given width, in logical pixels.
///
/// Half the width, because a width is a *diameter*: the mark is drawn as wide as the line the tool in hand lays, so a
/// heavier pen's dot is visibly heavier and a reader can see the weight they chose without touching the page. The app
/// asks this once per frame and publishes the answer to the window the ghost is drawn in
/// ([`crate::cursor_overlay::Screen::nib_radius`]).
///
/// A width that is not finite — a setting that arrived as nonsense — is answered with the smallest mark rather than
/// with a hole in the window.
pub fn nib_radius_for_width(width: f32) -> f32 {
    if !width.is_finite() {
        return NIB_RADIUS_MIN;
    }

    (width * 0.5).clamp(NIB_RADIUS_MIN, NIB_RADIUS_MAX)
}

/// How far the nib mark's bloom reaches beyond the mark, in logical pixels.
///
/// A *spread* rather than a multiple of the mark: the mark can be as wide as a highlighter's band, and a bloom that
/// grew with it would be a fog rather than an edge. A fixed spread keeps the edge the same thickness whatever is in
/// hand, which is what makes it read as an edge.
pub const NIB_BLOOM_SPREAD: f32 = 4.0;

/// How opaque the nib mark's bloom is.
///
/// The renderer antialiases the mark's own edge, but a hard edge of a hard grey on white paper is
/// still a sticker on the page. One wider, much fainter copy underneath reads as the mark sitting
/// *in* the paper — the same trick the page's own shadow uses, in one step rather than three,
/// because a cursor is small and is rebuilt on every frame the hand moves.
pub const NIB_BLOOM_ALPHA: f32 = 0.10;

/// How opaque the nib mark itself is.
///
/// Not quite opaque: the mark is a marker rather than ink, and a solid dot reads as a blot on the
/// page it is pointing at.
pub const NIB_ALPHA: f32 = 0.85;

/// How far the body's soft edge reaches beyond the body itself, in logical pixels.
pub const BODY_HALO_GROW: f32 = 1.4;

/// How opaque the body's soft edge is. It is painted under the body, so the two read as one shape
/// with a soft edge rather than as a shape with an outline.
///
/// Twice what it was, because the body's shadow changed what this one is for: the edge used to be the only thing
/// standing between a faint rod and the paper, and now it is the second of two — a rim around the shadow rather than
/// a rescue, and the two together are what an eye finds a rod by on a page of writing.
pub const BODY_HALO_ALPHA: f32 = 0.20;

/// How opaque the body is, at the lean [`FULL_LEAN_DEGREES`] and beyond.
///
/// Fainter than the nib mark by a wide margin, because the two say different things: the nib mark
/// says where the ink will land, and the body only says which way the pen is held.
pub const BODY_ALPHA: f32 = 0.34;

/// Where the pen is and how it leans: everything needed to draw a cursor for it.
///
/// `Copy` and `PartialEq` so the frame loop can ask whether the cursor moved without asking the
/// ink model to report it. A pen in range but not touching lays no ink, and yet its cursor still
/// has to follow it around the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenCursor {
    /// The nib's position, in logical window pixels.
    x: f32,
    y: f32,
    /// The lean, or `None` when the device reports no tilt.
    tilt: Option<Tilt>,
}

impl PenCursor {
    /// The cursor a reading asks for, or `None` when there is no pen to draw one for.
    ///
    /// `scale` is the window's DPI scale factor, because `pen-windows` reports **physical** client
    /// pixels and GPUI paints in **logical** ones. This is the same division ink does in
    /// `InkDocument::consume`, and for the same reason: if the two ever differed, the ghost would
    /// float away from the ink it is meant to describe.
    ///
    /// The phases that end a visit yield `None`. The pen has left detection range, or the system
    /// has taken the pointer away, and a cursor left behind at the last position it was seen would
    /// be a lie about where the pen is.
    pub fn from_sample(sample: PenSample, scale: f32) -> Option<Self> {
        match sample.phase {
            PenPhase::Idle | PenPhase::Leave | PenPhase::Cancel => None,
            _ => {
                let scale = if scale > 0.0 { scale } else { 1.0 };

                Some(PenCursor {
                    x: sample.pixel.x / scale,
                    y: sample.pixel.y / scale,
                    tilt: sample.tilt,
                })
            }
        }
    }

    /// The nib's position, in logical window pixels.
    pub fn position(self) -> [f32; 2] {
        [self.x, self.y]
    }

    /// The lean, or `None` when the device reports no tilt.
    pub fn tilt(self) -> Option<Tilt> {
        self.tilt
    }

    /// The angle between the pen and the surface normal, in degrees.
    ///
    /// `0` is straight up and `90` is flat on the paper. A pen that reports no tilt is reported as
    /// straight up, which is the only honest answer: nothing was measured.
    pub fn lean_degrees(self) -> f32 {
        self.tilt.map(Tilt::from_normal).unwrap_or(0.0)
    }

    /// The direction the pen's body lies in, as a unit vector in window coordinates.
    pub fn lean_direction(self) -> Option<[f32; 2]> {
        let tilt = self.tilt?;
        let (x, y) = (tilt.x.to_radians().tan(), tilt.y.to_radians().tan());
        let length = (x * x + y * y).sqrt();

        // A pen reported as perfectly upright has no direction to point in.
        (length > 0.0).then(|| [x / length, y / length])
    }

    /// How long the drawn body is, in logical pixels.
    pub fn body_length(self) -> f32 {
        let lean = self.lean_degrees();
        if lean < MIN_LEAN_DEGREES {
            return 0.0;
        }

        // A pen of length L leaning by `lean` from the normal projects to `L * sin(lean)` on the
        // screen, so the drawn length *is* the angle rather than a decoration of it.
        BODY_LENGTH * lean.to_radians().sin()
    }

    /// How strongly the body is drawn, from nothing at [`MIN_LEAN_DEGREES`] to full at
    /// [`FULL_LEAN_DEGREES`].
    ///
    /// The drawn length already ramps with the lean, but a length of eight pixels is still a shape
    /// that was not there a moment ago. Ramping the weight too means the body arrives instead of
    /// appearing: a caller multiplies its alphas by this.
    pub fn body_fade(self) -> f32 {
        let lean = self.lean_degrees();

        ((lean - MIN_LEAN_DEGREES) / (FULL_LEAN_DEGREES - MIN_LEAN_DEGREES)).clamp(0.0, 1.0)
    }

    /// The pen's body as a shape to draw, or `None` when the pen is too upright to show one.
    ///
    /// `Body` is `Copy` and its outline is a fixed array rather than a path, so building it costs
    /// no allocation: this is rebuilt whenever the pen moves.
    pub fn body_shape(self) -> Option<Body> {
        let along = self.lean_direction()?;
        let length = self.body_length();
        if length <= 0.0 {
            return None;
        }

        Some(Body {
            nib: [self.x, self.y],
            along,
            across: [-along[1], along[0]],
            length,
            nib_half: BODY_NIB_WIDTH / 2.0,
            tail_half: BODY_TAIL_WIDTH / 2.0,
        })
    }
}

/// The pen's body: a slender spindle from the nib, swelling to a round far end.
///
/// Seven points rather than four, because the flanks are curves and the end is a cap. A quad cannot
/// be rotated and pointing somewhere is the entire point of the shape, but a quad *drawn as*
/// straight edges leaves two corners at the far end and a narrower one at the nib — a wedge. Three
/// quadratic curves swell the flanks and round the far end, and the shape then reads as a body seen
/// at an angle rather than as an arrowhead painted over the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Body {
    /// The centre of the nib end.
    nib: [f32; 2],
    /// A unit vector from the nib end toward the far end.
    along: [f32; 2],
    /// A unit vector across the body, so that `+1` is one flank and `-1` the other.
    across: [f32; 2],
    /// The distance from the nib end to the far end's centre, in logical pixels.
    length: f32,
    /// Half the body's width at the nib end.
    nib_half: f32,
    /// Half the body's width at the far end.
    tail_half: f32,
}

impl Body {
    /// The seven points the outline is drawn through, each edge pushed out by `grow` pixels.
    ///
    /// The order is the order they are drawn in, and it is the whole contract: nib-left, flank
    /// control, far-left, cap control, far-right, flank control, nib-right. Every control point is
    /// the single one of a quadratic, so a caller draws three curves and closes the shape.
    ///
    /// `grow` is what makes the soft edge: the same outline, a pixel wider all round, painted first
    /// and fainter — one step of the trick the page's shadow uses. It grows the shape *outward*, so
    /// the far end reaches `grow` further while the nib end stays where the pen is: the halo sits
    /// behind the nib mark, and a soft edge that pushed the tip of the body out by twice as much as
    /// its flanks would be a shape being stretched rather than enlarged.
    pub fn outline(self, grow: f32) -> [[f32; 2]; 7] {
        let grow = grow.max(0.0);
        let (along, across) = (self.along, self.across);
        let nib_half = self.nib_half + grow;
        let tail_half = self.tail_half + grow;
        let far = self.far();

        // A point beside `base`: `side` is which flank, and the width is measured from the body's
        // own axis at that place.
        let beside = |base: [f32; 2], half: f32, side: f32| {
            [
                base[0] + across[0] * half * side,
                base[1] + across[1] * half * side,
            ]
        };

        // Where the flank swells: most of the way along, already at the far end's width. The curve
        // through it leaves the nib almost straight and bulges near the far end, which is what a
        // body seen looking down its own length does.
        let swell = [
            self.nib[0] + along[0] * self.length * BODY_BULGE,
            self.nib[1] + along[1] * self.length * BODY_BULGE,
        ];

        // A quadratic reaches half of the distance to its control point, so a control twice the
        // cap's radius puts the drawn end exactly one radius beyond the far end.
        let cap = [
            far[0] + along[0] * tail_half * 2.0,
            far[1] + along[1] * tail_half * 2.0,
        ];

        [
            beside(self.nib, nib_half, 1.0),
            beside(swell, tail_half, 1.0),
            beside(far, tail_half, 1.0),
            cap,
            beside(far, tail_half, -1.0),
            beside(swell, tail_half, -1.0),
            beside(self.nib, nib_half, -1.0),
        ]
    }

    /// The centre of the round far end.
    pub fn far(self) -> [f32; 2] {
        [
            self.nib[0] + self.along[0] * self.length,
            self.nib[1] + self.along[1] * self.length,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pen_windows::Point;

    /// A reading at a position with the given phase and lean.
    fn reading(phase: PenPhase, tilt: Option<(f32, f32)>) -> PenSample {
        PenSample {
            phase,
            id: 1,
            pixel: Point::new(100.0, 200.0),
            tilt: tilt.map(|(x, y)| Tilt::new(x, y)),
            ..PenSample::default()
        }
    }

    /// The cursor for a reading.
    fn cursor(phase: PenPhase, tilt: Option<(f32, f32)>) -> Option<PenCursor> {
        PenCursor::from_sample(reading(phase, tilt), 1.0)
    }

    /// The distance between two points.
    fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
    }

    /// A cursor left behind where the pen was last seen would be a lie, so the phases that end a
    /// visit draw nothing.
    #[test]
    fn a_cursor_is_only_drawn_while_the_pen_is_here() {
        for phase in [
            PenPhase::Enter,
            PenPhase::Hover,
            PenPhase::Down,
            PenPhase::Move,
            PenPhase::Up,
        ] {
            assert!(
                cursor(phase, None).is_some(),
                "{phase:?} is a pen to draw a cursor for"
            );
        }

        for phase in [PenPhase::Idle, PenPhase::Leave, PenPhase::Cancel] {
            assert!(
                cursor(phase, None).is_none(),
                "{phase:?} leaves no cursor behind"
            );
        }
    }

    /// The body lies in the direction the pen leans, in window coordinates.
    ///
    /// The signs are the convention the whole feature rests on: `tilt_x` positive is a lean to the
    /// right, and `tilt_y` positive is a lean toward the user, which in window coordinates is
    /// *down*.
    #[test]
    fn the_body_lies_where_the_pen_leans() {
        let cases = [
            ((45.0, 0.0), [1.0, 0.0]),
            ((-45.0, 0.0), [-1.0, 0.0]),
            ((0.0, 45.0), [0.0, 1.0]),
            ((0.0, -45.0), [0.0, -1.0]),
        ];

        for (tilt, expected) in cases {
            let direction = cursor(PenPhase::Hover, Some(tilt))
                .expect("a hovering pen has a cursor")
                .lean_direction()
                .expect("a leaning pen has a direction");

            assert!(
                (direction[0] - expected[0]).abs() < 1e-4
                    && (direction[1] - expected[1]).abs() < 1e-4,
                "a lean of {tilt:?} points {direction:?}, not {expected:?}"
            );
        }
    }

    /// A lean of 45 degrees on both axes lies between the two, and is a unit vector.
    #[test]
    fn a_lean_on_both_axes_points_between_them() {
        let direction = cursor(PenPhase::Hover, Some((45.0, 45.0)))
            .expect("a hovering pen has a cursor")
            .lean_direction()
            .expect("a leaning pen has a direction");

        let diagonal = 1.0 / 2.0_f32.sqrt();
        assert!((direction[0] - diagonal).abs() < 1e-4);
        assert!((direction[1] - diagonal).abs() < 1e-4);

        let length = (direction[0] * direction[0] + direction[1] * direction[1]).sqrt();
        assert!(
            (length - 1.0).abs() < 1e-4,
            "the direction is a unit vector"
        );
    }

    /// A pen standing straight up has no body to draw: an upright stick projects to a point.
    #[test]
    fn an_upright_pen_shows_no_body() {
        for tilt in [(0.0, 0.0), (2.0, -1.0)] {
            let cursor = cursor(PenPhase::Hover, Some(tilt)).expect("a hovering pen has a cursor");

            assert!(
                cursor.body_shape().is_none(),
                "a lean of {tilt:?} is too upright to draw"
            );
            assert_eq!(cursor.body_length(), 0.0);
            assert_eq!(
                cursor.body_fade(),
                0.0,
                "and an upright pen's body has no weight to fade in from"
            );
        }
    }

    /// A pen that reports no tilt at all still gets a cursor: the nib mark does not depend on it.
    #[test]
    fn a_pen_without_a_tilt_sensor_still_has_a_cursor() {
        let cursor = cursor(PenPhase::Hover, None).expect("a hovering pen has a cursor");

        assert!(cursor.tilt().is_none());
        assert_eq!(
            cursor.lean_degrees(),
            0.0,
            "nothing measured is a zero lean"
        );
        assert!(cursor.body_shape().is_none());
        assert_eq!(cursor.position(), [100.0, 200.0]);
    }

    /// The drawn length is the lean: longer the flatter the pen lies, and never longer than a pen.
    #[test]
    fn the_body_grows_as_the_pen_lies_down() {
        let lengths: Vec<f32> = [10.0, 30.0, 60.0, 85.0]
            .into_iter()
            .map(|lean| {
                cursor(PenPhase::Hover, Some((lean, 0.0)))
                    .expect("a hovering pen has a cursor")
                    .body_length()
            })
            .collect();

        for pair in lengths.windows(2) {
            assert!(
                pair[1] > pair[0],
                "a flatter pen draws a longer body: {lengths:?}"
            );
        }

        assert!(
            lengths[3] <= BODY_LENGTH,
            "the body never exceeds the pen's own length"
        );
    }

    /// Where the drawn shape actually ends: a quadratic comes halfway to its control point at the
    /// middle of itself, which is the apex of the cap.
    fn apex(outline: &[[f32; 2]; 7], far: [f32; 2]) -> [f32; 2] {
        [
            far[0] + (outline[3][0] - far[0]) / 2.0,
            far[1] + (outline[3][1] - far[1]) / 2.0,
        ]
    }

    /// The body starts at the nib and widens away from it, and its far end is round rather than cut
    /// off: the direction cannot be misread, and there is no corner left to catch the eye.
    #[test]
    fn the_body_is_a_taper_with_a_round_far_end() {
        let cursor = cursor(PenPhase::Hover, Some((60.0, 0.0))).expect("a hovering pen has one");
        let body = cursor.body_shape().expect("a leaning pen has a body");
        let outline = body.outline(0.0);
        let far = body.far();

        // Points 0 and 6 straddle the nib; points 2 and 4 straddle the far end.
        let nib_span = distance(outline[0], outline[6]);
        let tail_span = distance(outline[2], outline[4]);

        assert!(
            (nib_span - BODY_NIB_WIDTH).abs() < 1e-4,
            "the narrow end is the nib"
        );
        assert!(
            (tail_span - BODY_TAIL_WIDTH).abs() < 1e-4,
            "the far end is the width the shape is drawn at"
        );
        assert!(tail_span > nib_span, "the body widens away from the nib");

        // The flanks are curves through a control point out at the far end's width, most of the way
        // along: the edge leaves the nib almost straight and swells toward the far end, so the curve
        // passes outside the straight edge between the two ends. The control point alone would not
        // say that — a control exactly on that line still draws a straight edge — so what is pinned
        // is the side of the line the control fell on.
        let straight_mid = (outline[0][1] + outline[2][1]) / 2.0;
        assert!(
            outline[1][1] > straight_mid,
            "the flank bows outward rather than running straight"
        );

        // With a lean straight to the right, the body extends toward the lean, and the cap reaches
        // past that far end — the end is round, not cut off.
        assert!(
            outline[2][0] > outline[0][0],
            "the body extends toward the lean, not away from it"
        );
        assert!(
            apex(&outline, far)[0] > far[0],
            "the round end reaches past the far end instead of stopping at it"
        );
    }

    /// The soft edge is the same outline, a pixel wider all round, so the two shapes painted
    /// together are one shape with a soft edge rather than two shapes.
    #[test]
    fn the_soft_edge_wraps_the_body() {
        let body = cursor(PenPhase::Hover, Some((50.0, 0.0)))
            .expect("a hovering pen has one")
            .body_shape()
            .expect("a leaning pen has a body");

        let tight = body.outline(0.0);
        let soft = body.outline(BODY_HALO_GROW);
        let far = body.far();

        let nib_grown = distance(soft[0], soft[6]) - distance(tight[0], tight[6]);
        let tail_grown = distance(soft[2], soft[4]) - distance(tight[2], tight[4]);
        let reached = distance(apex(&soft, far), apex(&tight, far));

        assert!(
            (nib_grown - BODY_HALO_GROW * 2.0).abs() < 1e-4,
            "the nib end moves out by the width of the soft edge"
        );
        assert!(
            (tail_grown - BODY_HALO_GROW * 2.0).abs() < 1e-4,
            "and so does the far end"
        );
        assert!(
            (reached - BODY_HALO_GROW).abs() < 1e-4,
            "the round end reaches that much further as well"
        );
    }

    /// The body arrives with the lean rather than popping into being at the threshold.
    #[test]
    fn the_body_arrives_as_its_lean_grows() {
        let fade = |lean: f32| {
            cursor(PenPhase::Hover, Some((lean, 0.0)))
                .expect("a hovering pen has one")
                .body_fade()
        };

        // The lean is measured through `tan`/`atan`, so the ends of the ramp are compared within a
        // hair rather than exactly: what is pinned is the ramp, not the last bit of a float.
        assert!(fade(MIN_LEAN_DEGREES) < 1e-4, "a body not drawn yet");
        assert!(
            (fade(FULL_LEAN_DEGREES) - 1.0).abs() < 1e-4,
            "and one fully drawn"
        );
        assert_eq!(fade(80.0), 1.0, "a pen laid flat is no stronger than that");

        let steps: Vec<f32> = [5.0, 8.0, 11.0, 14.0].into_iter().map(fade).collect();
        assert!(
            steps.windows(2).all(|pair| pair[1] > pair[0]),
            "the weight grows with the lean: {steps:?}"
        );
    }

    /// The soft layers are wider and fainter than what they sit under, and the nib mark stays the
    /// strongest thing drawn: this is what keeps the cursor a hint rather than a blot.
    #[test]
    fn the_soft_layers_are_fainter_than_what_they_sit_under() {
        assert!(NIB_BLOOM_SPREAD > 0.0, "the bloom reaches past the mark");
        assert!(NIB_BLOOM_ALPHA < NIB_ALPHA, "and is the fainter copy");
        assert!(BODY_HALO_GROW > 0.0, "the body's edge reaches outward");
        assert!(BODY_HALO_ALPHA < BODY_ALPHA, "and is fainter than the body");
        assert!(
            BODY_ALPHA < NIB_ALPHA,
            "the body never competes with the nib"
        );
        assert!(
            NIB_RADIUS_MIN < NIB_RADIUS_MAX,
            "the mark has a range of sizes to move in"
        );

        for alpha in [NIB_ALPHA, NIB_BLOOM_ALPHA, BODY_ALPHA, BODY_HALO_ALPHA] {
            assert!(
                (0.0..1.0).contains(&alpha),
                "{alpha} is not an opacity a shape can be painted with"
            );
        }
    }

    /// Whatever the window's DPI, the ghost lands on the ink.
    #[test]
    fn the_dpi_scale_is_applied_once() {
        let physical = PenSample {
            phase: PenPhase::Hover,
            pixel: Point::new(200.0, 400.0),
            ..PenSample::default()
        };

        let doubled = PenCursor::from_sample(physical, 2.0).expect("a hovering pen has one");
        assert_eq!(doubled.position(), [100.0, 200.0], "physical pixels halve");

        let nonsense = PenCursor::from_sample(physical, 0.0).expect("a hovering pen has one");
        assert_eq!(
            nonsense.position(),
            [200.0, 400.0],
            "a scale that makes no sense is 1:1 rather than a division by zero"
        );
    }

    /// The nib mark is half the line in hand, held between the two sizes the shape declares: the dot is the pen's
    /// weight made visible, and no setting can make it invisible or fill the window.
    #[test]
    fn the_mark_is_half_the_line_in_hand() {
        assert!(
            (nib_radius_for_width(4.5) - 2.25).abs() < 1e-6,
            "half of the widest press the shipped pen has"
        );
        assert!(
            nib_radius_for_width(18.0) > nib_radius_for_width(4.5),
            "a heavier pen has a heavier dot"
        );
        assert_eq!(
            nib_radius_for_width(18.0),
            NIB_RADIUS_MAX,
            "and a highlighter's mark is half its own band"
        );
        assert_eq!(
            nib_radius_for_width(0.1),
            NIB_RADIUS_MIN,
            "a pen of nothing still has a dot"
        );
        assert_eq!(
            nib_radius_for_width(f32::NAN),
            NIB_RADIUS_MIN,
            "and a width that arrived as nonsense is not a hole in the window"
        );
    }
}
