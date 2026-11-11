//! What a pen does when a key is held: the two gestures that need the nib, and the keys they answer to.
//!
//! ## Why the keys are asked of the operating system
//!
//! A gesture here is *"space is down while the nib is on the paper"*, and the nib is on the paper for as
//! long as the hand is writing — which is exactly when this window is not necessarily the one the keyboard
//! is talking to. A window hears keys only while it has focus, and a reader who pressed space and then put
//! the pen down would see the first frames of their gesture missing. So the held keys are read from
//! [`device_query`], which asks the OS for the current state rather than waiting for an event.
//!
//! It is a *query*, not a listener: no thread, no event loop, no system-wide tap. That is the whole reason
//! it is affordable to do this per pen batch — one call, a handful of nanoseconds, on a thread that is
//! already awake.
//!
//! ## Why the gestures are decided away from the pen
//!
//! [`Gesture::of`] takes the held keys and the pen's own position and answers which gesture is in play, and
//! it is pure. The parts that need a window — moving the sheet, zooming the view — are the app's; the part
//! that is a rule lives here, where it can be read in one place and tested without one.

use device_query::{DeviceState, Keycode};

/// The keys the pen's gestures answer to, as the platform reports them.
///
/// The variants are per-platform aliases in `device_query`'s own list: `Space` is one key everywhere, while
/// the option key is `LAlt`/`RAlt` on some layouts, `LOption`/`ROption` on others and `LMeta`/`RMeta` on a
/// third. Any of them means "the option key is held", because a reader pressing the key their keyboard
/// calls Option should not have to know which name the crate gave it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    /// Space is down: a drag with the nib pans the sheet.
    pub space: bool,
    /// The option key is down: a drag with the nib zooms.
    pub option: bool,
}

impl Held {
    /// Which of the gesture keys are down right now.
    ///
    /// A machine without the permission `device_query` needs on macOS answers an empty list, which reads as
    /// "nothing is held" — the behaviour this app had before the gestures existed, rather than an error.
    pub fn now(state: &DeviceState) -> Self {
        let keys = state.query_keymap();

        Self {
            space: keys.contains(&Keycode::Space),
            option: keys.iter().any(|key| {
                matches!(
                    key,
                    Keycode::LAlt
                        | Keycode::RAlt
                        | Keycode::LOption
                        | Keycode::ROption
                        | Keycode::LMeta
                        | Keycode::RMeta
                )
            }),
        }
    }
}

/// What the pen is doing, given the keys its hand is not holding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    /// Ordinary writing: the nib lays ink.
    Ink,
    /// Space is down: the nib moves the sheet instead of writing on it.
    Pan,
    /// Option is down: the nib's vertical travel zooms, up closer and down further away.
    Zoom,
}

impl Gesture {
    /// Which gesture a pen reading belongs to.
    ///
    /// Space wins over option, because a reader who is panning may put their other hand on option without
    /// meaning to change what the pen is doing; and neither key means anything while something other than
    /// the sheet has the pen (see [`crate::app`]), which is why `sheet_is_front` is asked here rather than
    /// by the caller.
    pub fn of(held: Held, sheet_is_front: bool) -> Self {
        if !sheet_is_front {
            return Self::Ink;
        }

        if held.space {
            Self::Pan
        } else if held.option {
            Self::Zoom
        } else {
            Self::Ink
        }
    }
}

/// The zoom a pen's vertical travel asks for, as a factor to multiply the sheet's zoom by.
///
/// `rise` is how far up the nib has moved since the gesture began, in logical pixels: up is closer, so a
/// positive rise is a factor above one. It is an *exponent* rather than a ratio — the same rule the wheel
/// and the pinch follow — so a gesture that goes a long way zooms smoothly instead of in jumps, and the
/// same distance zooms the same amount whatever the sheet started at.
///
/// The result is clamped like every other zoom: a stray reading from a driver, or a hand that left the
/// surface and came back, should not take the sheet from nothing to everything in one step.
pub fn pen_zoom_factor(rise: f32) -> f32 {
    if !rise.is_finite() {
        return 1.0;
    }

    (PEN_ZOOM_PER_PIXEL.powf(rise)).clamp(0.2, 5.0)
}

/// How much one logical pixel of vertical pen travel multiplies the zoom by, as an exponent's base.
///
/// Three hundred pixels of travel — about twice a hand's width on a sheet at 100% — is one doubling. That is
/// deliberately *gentler* than a wheel: `WHEEL_ZOOM_STEP` is 1.25 a notch of 96 pixels, which is a doubling
/// every 298 pixels, so the two are close by construction and the pen is the slower of them. A hand moving a
/// pen wants to see the sheet move with it, not to be flung across a page by a small correction.
pub const PEN_ZOOM_PER_PIXEL: f32 = 1.002_313_2;

/// Where a gesture's keys are read from, for as long as the app is running.
///
/// `None` is a machine where the state could not be created at all, which is the same as "no key is held":
/// the pen then writes exactly as it did before there were gestures. The value is not `Send`, which is why
/// it lives on the app rather than being shared with the pen's own thread — the keys are read on the UI
/// thread, in the same wake that spends the reading.
pub fn keys() -> Option<DeviceState> {
    DeviceState::checked_new()
}

#[cfg(test)]
mod tests;
