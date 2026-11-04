//! The gamepad's left stick, as a pan of the sheet.
//!
//! A stick is a *position*, not a queue of readings: there is nothing to wait on, and the frame loop
//! already has a clock. [`Gamepads::pan`] is therefore read by the housekeeping pump, and answers
//! with the distance the sheet should move since the last call — the same kind of number the wheel
//! gives (see `NoteApp::on_wheel`).
//!
//! `gilrs` is a cross-platform library, like GPUI Kit, so this lives above the platform seam: one
//! implementation reads an Xbox pad on Windows and a DualSense on a Mac. No gamepad is not an error;
//! the stick simply asks for nothing.

use std::time::{Duration, Instant};

use gilrs::{Axis, Gilrs};

/// How fast a fully deflected stick pans, in logical pixels per second.
///
/// About a wheel spun at ten notches a second (10 × 3 lines × 32 px), because a stick is held rather
/// than flicked: it has to cross a sheet in about the time a hand expects.
const GAMEPAD_PAN_SPEED: f32 = 1_000.0;

/// The longest wake the stick is paid for.
///
/// A laptop that slept for a minute would otherwise jump the sheet by a minute's worth of pan the
/// first time the stick was read again.
const GAMEPAD_LONGEST_STEP: Duration = Duration::from_millis(100);

/// The gamepads the app can see, and when the stick was last read.
pub struct Gamepads {
    /// `None` when gamepad support could not be opened; the stick is then always still.
    gilrs: Option<Gilrs>,
    /// When [`Self::pan`] was last called, so a position becomes a distance.
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

    /// How far the left stick asks the sheet to move since the last call, in logical pixels.
    ///
    /// The direction is the view's: pushing the stick right looks further right, so the sheet moves
    /// left, and pushing it up looks earlier in the page, so the sheet moves down — the rule a wheel
    /// follows and a document viewer has. The events are drained because that is what keeps
    /// `gilrs`'s cached state current; what they say is not otherwise this app's business. `gilrs`
    /// has already applied the pad's dead zone, so a stick at rest reads as zero rather than as drift.
    pub fn pan(&mut self) -> (f32, f32) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;

        let Some(gilrs) = self.gilrs.as_mut() else {
            return (0.0, 0.0);
        };

        while gilrs.next_event().is_some() {}

        // The first pad holding its stick is the one that pans: two hands on two pads is not a
        // gesture this app has, and adding them together would double the speed.
        let (x, y) = gilrs
            .gamepads()
            .map(|(_, pad)| (pad.value(Axis::LeftStickX), pad.value(Axis::LeftStickY)))
            .find(|(x, y)| *x != 0.0 || *y != 0.0)
            .unwrap_or((0.0, 0.0));

        stick_pan(x, y, elapsed)
    }
}

/// The distance the sheet moves for a stick at `(x, y)` held for `elapsed`, in logical pixels.
///
/// `gilrs` reports `y` positive when the stick is pushed away from the reader, which is earlier in
/// the page — the way a wheel turned away pans. `x` is the other way round because the stick moves
/// the *view*: right is later in the page, and later is the sheet moving left.
fn stick_pan(x: f32, y: f32, elapsed: Duration) -> (f32, f32) {
    let distance = GAMEPAD_PAN_SPEED * elapsed.min(GAMEPAD_LONGEST_STEP).as_secs_f32();
    (-x * distance, y * distance)
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
}
