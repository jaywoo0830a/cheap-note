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
//! ## What it cannot do
//!
//! GPUI gives no way to turn the system pointer off: `CursorStyle` has a variant for every arrow it
//! can show and none that hides it, and there is no hook for a custom bitmap. So this ghost is
//! drawn *in addition* to whatever the platform puts under it. Where Windows suppresses the mouse
//! pointer while the pen is in range it reads as the cursor; where it does not, it reads as a nib
//! marker beside one.
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

use pen_windows::{PenPhase, PenSample, Tilt};

/// How long the drawn body is, in logical pixels, when the pen is laid flat.
///
/// A real pen is about 140 mm, which at this sheet's scale is longer than a hand wants on screen,
/// so the length is chosen for the eye and only the lean scales it.
const BODY_LENGTH: f32 = 120.0;

/// The least lean that draws a body, in degrees.
///
/// Below this the direction is dominated by the digitizer's own noise, and a stub of a pen that
/// swings around the nib is worse than no body at all: the nib mark already says where the ink is.
const MIN_LEAN_DEGREES: f32 = 4.0;

/// How wide the drawn body is at the nib, in logical pixels.
const BODY_NIB_WIDTH: f32 = 1.5;

/// How many times wider the body is at its far end than at the nib.
///
/// The taper is what makes the shape read as a pen rather than as a smudge, and it is also what
/// carries the direction: the narrow end is the nib, so the ghost cannot be read the wrong way
/// round.
const BODY_TAPER: f32 = 3.0;

/// The radius of the nib mark, in logical pixels.
///
/// A fixed size rather than one that follows the stroke width: a mark the size of the stroke would
/// cover the very ink it is pointing at, and the ink already shows its own width. What the mark is
/// for is the moment before the nib touches down.
pub const NIB_RADIUS: f32 = 2.5;

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

    /// The tapered quad of the pen's body, or `None` when the pen is too upright to show one.
    ///
    /// Four points, returned as an array rather than as a path, so that building it costs no
    /// allocation: this is rebuilt whenever the pen moves.
    pub fn body_outline(self) -> Option<[[f32; 2]; 4]> {
        let along = self.lean_direction()?;
        let length = self.body_length();
        if length <= 0.0 {
            return None;
        }

        let across = [-along[1], along[0]];
        let nib = [self.x, self.y];
        let tail = [nib[0] + along[0] * length, nib[1] + along[1] * length];

        let half_nib = BODY_NIB_WIDTH / 2.0;
        let half_tail = BODY_NIB_WIDTH * BODY_TAPER / 2.0;

        Some([
            [nib[0] + across[0] * half_nib, nib[1] + across[1] * half_nib],
            [tail[0] + across[0] * half_tail, tail[1] + across[1] * half_tail],
            [tail[0] - across[0] * half_tail, tail[1] - across[1] * half_tail],
            [nib[0] - across[0] * half_nib, nib[1] - across[1] * half_nib],
        ])
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
        assert!((length - 1.0).abs() < 1e-4, "the direction is a unit vector");
    }

    /// A pen standing straight up has no body to draw: an upright stick projects to a point.
    #[test]
    fn an_upright_pen_shows_no_body() {
        for tilt in [(0.0, 0.0), (2.0, -1.0)] {
            let cursor = cursor(PenPhase::Hover, Some(tilt)).expect("a hovering pen has a cursor");

            assert!(
                cursor.body_outline().is_none(),
                "a lean of {tilt:?} is too upright to draw"
            );
            assert_eq!(cursor.body_length(), 0.0);
        }
    }

    /// A pen that reports no tilt at all still gets a cursor: the nib mark does not depend on it.
    #[test]
    fn a_pen_without_a_tilt_sensor_still_has_a_cursor() {
        let cursor = cursor(PenPhase::Hover, None).expect("a hovering pen has a cursor");

        assert!(cursor.tilt().is_none());
        assert_eq!(cursor.lean_degrees(), 0.0, "nothing measured is a zero lean");
        assert!(cursor.body_outline().is_none());
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

    /// The quad starts at the nib and widens away from it, so the direction cannot be misread.
    #[test]
    fn the_body_is_a_taper_away_from_the_nib() {
        let cursor = cursor(PenPhase::Hover, Some((60.0, 0.0))).expect("a hovering pen has one");
        let outline = cursor.body_outline().expect("a leaning pen has a body");

        // Points 0 and 3 straddle the nib; points 1 and 2 straddle the far end.
        let nib_span = distance(outline[0], outline[3]);
        let tail_span = distance(outline[1], outline[2]);

        assert!(
            (nib_span - BODY_NIB_WIDTH).abs() < 1e-4,
            "the narrow end is the nib"
        );
        assert!(tail_span > nib_span, "the body widens away from the nib");
        assert!(
            (tail_span / nib_span - BODY_TAPER).abs() < 1e-4,
            "the taper is the one the shape was drawn with"
        );

        // With a lean straight to the right, the far end is to the right of the nib.
        assert!(
            outline[1][0] > outline[0][0],
            "the body extends toward the lean, not away from it"
        );
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
}
