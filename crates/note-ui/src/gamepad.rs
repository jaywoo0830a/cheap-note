//! The gamepad's sticks, as a pan and a zoom.
//!
//! A stick is a *position*, not a queue of readings: there is nothing to wait on, and the frame loop
//! already has a clock. [`Gamepads::read`] is therefore read by the housekeeping pump, and answers
//! with the distance the sheet should move and the factor its zoom should change by since the last
//! call — the same kinds of number the wheel gives (see `NoteApp::on_wheel`).
//!
//! The left stick pans and the right stick's **Y axis** zooms; the right stick's X axis is not read,
//! because this app has one use for that stick and it is the vertical one.
//!
//! `gilrs` is a cross-platform library, like GPUI Kit, so this lives above the platform seam: one
//! implementation reads an Xbox pad on Windows and a DualSense on a Mac. No gamepad is not an error;
//! the sticks simply ask for nothing.

use std::time::{Duration, Instant};

use gilrs::{Axis, Gilrs};

/// How fast a fully deflected left stick pans, in logical pixels per second.
///
/// About a wheel spun at ten notches a second (10 × 3 lines × 32 px), because a stick is held rather
/// than flicked: it has to cross a sheet in about the time a hand expects.
const GAMEPAD_PAN_SPEED: f32 = 1_000.0;

/// How fast a fully deflected right stick zooms, in doublings per second.
///
/// A second at full deflection doubles the sheet; half deflection is the square root of that. The
/// zoom is a *rate* rather than a step for the same reason the pan is a speed: a stick is held.
const GAMEPAD_ZOOM_RATE: f32 = 1.0;

/// The longest wake the sticks are paid for.
///
/// A laptop that slept for a minute would otherwise jump the sheet by a minute's worth of pan and
/// zoom the first time the sticks were read again.
const GAMEPAD_LONGEST_STEP: Duration = Duration::from_millis(100);

/// What the two sticks asked for since the last read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sticks {
    /// How far the sheet should move, in logical pixels.
    pub pan: (f32, f32),
    /// The factor the zoom should change by; `1.0` when the right stick is still.
    pub zoom: f32,
}

impl Default for Sticks {
    fn default() -> Self {
        Sticks {
            pan: (0.0, 0.0),
            zoom: 1.0,
        }
    }
}

/// The gamepads the app can see, and when the sticks were last read.
pub struct Gamepads {
    /// `None` when gamepad support could not be opened; the sticks are then always still.
    gilrs: Option<Gilrs>,
    /// When [`Self::read`] was last called, so a position becomes a distance.
    last: Instant,
}

impl Gamepads {
    /// Opens the platform's gamepad support, if there is any.
    pub fn new() -> Self {
        Gamepads {
            gilrs: Gilrs::new().ok(),
            last: Instant::now(),
        }
    }

    /// What the sticks ask for since the last call: a pan in logical pixels and a zoom factor.
    ///
    /// The pan's direction is the view's: pushing the left stick right looks further right, so the
    /// sheet moves left, and pushing it up looks earlier in the page, so the sheet moves down — the
    /// rule a wheel follows and a document viewer has. Pushing the right stick away from the reader
    /// zooms in. The events are drained because that is what keeps `gilrs`'s cached state current;
    /// what they say is not otherwise this app's business. `gilrs` has already applied each pad's
    /// dead zone, so a stick at rest reads as zero rather than as drift.
    pub fn read(&mut self) -> Sticks {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;

        let Some(gilrs) = self.gilrs.as_mut() else {
            return Sticks::default();
        };

        while gilrs.next_event().is_some() {}

        // The first pad with a stick off its centre is the one being held: two hands on two pads is
        // not a gesture this app has, and adding them together would double the speed.
        let ((x, y), zoom) = gilrs
            .gamepads()
            .map(|(_, pad)| {
                (
                    (pad.value(Axis::LeftStickX), pad.value(Axis::LeftStickY)),
                    pad.value(Axis::RightStickY),
                )
            })
            .find(|((x, y), zoom)| *x != 0.0 || *y != 0.0 || *zoom != 0.0)
            .unwrap_or(((0.0, 0.0), 0.0));

        Sticks {
            pan: stick_pan(x, y, elapsed),
            zoom: stick_zoom(zoom, elapsed),
        }
    }
}

/// The distance the sheet moves for a left stick at `(x, y)` held for `elapsed`, in logical pixels.
///
/// `gilrs` reports `y` positive when the stick is pushed away from the reader, which is earlier in
/// the page — the way a wheel turned away pans. `x` is the other way round because the stick moves
/// the *view*: right is later in the page, and later is the sheet moving left.
fn stick_pan(x: f32, y: f32, elapsed: Duration) -> (f32, f32) {
    let distance = GAMEPAD_PAN_SPEED * elapsed.min(GAMEPAD_LONGEST_STEP).as_secs_f32();
    (-x * distance, y * distance)
}

/// The factor the zoom changes by for a right stick at `y` held for `elapsed`.
///
/// The factor is an *exponent* rather than a sum, so that one second at half deflection and half a
/// second at full are the same zoom, and pushing the stick back to where it was undoes it exactly.
fn stick_zoom(y: f32, elapsed: Duration) -> f32 {
    let seconds = elapsed.min(GAMEPAD_LONGEST_STEP).as_secs_f32();
    2.0f32.powf(y * GAMEPAD_ZOOM_RATE * seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stick held still asks for nothing, and a second at full tilt is the speed, not a leap.
    #[test]
    fn a_stick_moves_the_sheet_at_its_speed() {
        assert_eq!(stick_pan(0.0, 0.0, Duration::from_millis(16)), (0.0, 0.0));
        assert_eq!(
            stick_pan(1.0, 1.0, Duration::from_millis(100)),
            (-100.0, 100.0),
            "a tenth of a second at a thousand pixels a second"
        );
    }

    /// The stick moves the view: right looks further right, up looks earlier in the page.
    #[test]
    fn the_stick_points_the_way_the_view_looks() {
        let (right, up) = stick_pan(1.0, 1.0, Duration::from_millis(100));
        assert!(right < 0.0, "the sheet moves left when the view goes right");
        assert!(up > 0.0, "the sheet moves down when the view goes up");

        let (left, down) = stick_pan(-1.0, -1.0, Duration::from_millis(100));
        assert!(left > 0.0 && down < 0.0);
    }

    /// A machine that slept does not wake to the sheet a minute away.
    #[test]
    fn a_long_sleep_is_only_one_step() {
        assert_eq!(
            stick_pan(1.0, 0.0, Duration::from_secs(60)),
            (-100.0, 0.0),
            "clamped to the longest step, not multiplied by a minute"
        );
    }

    /// The right stick's Y axis zooms: away from the reader in, toward them out.
    #[test]
    fn the_right_stick_zooms_in_the_direction_it_points() {
        assert_eq!(stick_zoom(0.0, Duration::from_millis(100)), 1.0);

        let closer = stick_zoom(1.0, Duration::from_millis(100));
        let further = stick_zoom(-1.0, Duration::from_millis(100));

        assert!(closer > 1.0, "away from the reader is closer");
        assert!(further < 1.0, "toward the reader is further away");
        assert!(
            (closer * further - 1.0).abs() < 1e-6,
            "the two directions undo each other"
        );
    }

    /// Full deflection for a second doubles the sheet, however the second is divided into wakes.
    #[test]
    fn a_second_at_full_tilt_doubles_the_zoom() {
        let mut factor = 1.0;
        for _ in 0..10 {
            factor *= stick_zoom(1.0, Duration::from_millis(100));
        }

        assert!((factor - 2.0).abs() < 1e-5, "two, not a stack of steps");
    }

    /// A machine that slept does not wake to a zoom a minute away either.
    #[test]
    fn a_long_sleep_zooms_once() {
        assert_eq!(
            stick_zoom(1.0, Duration::from_secs(60)),
            stick_zoom(1.0, GAMEPAD_LONGEST_STEP)
        );
    }
}
