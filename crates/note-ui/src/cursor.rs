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

use crate::input::{Phase, Reading, Tilt};

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
    pub fn from_sample(sample: Reading, scale: f32) -> Option<Self> {
        match sample.phase {
            Phase::Idle | Phase::Leave | Phase::Cancel => None,
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
mod tests;
