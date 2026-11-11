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
/// `device_query` has three spellings for the choice modifier — `Alt`, `Option` and `Meta` — and **only one of
/// them exists on any given platform**. On macOS its own table maps `readkey::Keycode::Option` to `LOption`
/// and `Keycode::Command` to `Command`, so the left and right Option keys arrive as `LOption`/`ROption` and
/// `LAlt`/`RAlt`/`LMeta`/`RMeta` are never produced at all. On Windows it is the reverse: `LAlt`/`RAlt` are
/// the modifier and the `Option`/`Meta` spellings do not appear.
///
/// So all four names are accepted rather than one, because the same source has to be right on both — and
/// **`Command` is deliberately not among them.** On a Mac that is the key next to the space bar that every
/// other application uses for its own commands, and a pen gesture that fired under it would be a gesture that
/// fired while the reader was reaching for `⌘C`.
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
            option: keys.iter().copied().any(is_option),
        }
    }
}

/// Whether a keycode is one of the platform's names for the option key. See [`Held`] for why the list is
/// four names long and why `Command` is not in it.
fn is_option(key: Keycode) -> bool {
    matches!(
        key,
        Keycode::LOption | Keycode::ROption | Keycode::LAlt | Keycode::RAlt
    )
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
/// `rise` is how far up the nib has moved since the gesture began, in logical pixels, and `from` is the zoom
/// the gesture started at: up is closer, so a positive rise is a factor above one. It is an *exponent* rather
/// than a ratio — the same rule the wheel and the pinch follow — so a gesture that goes a long way zooms
/// smoothly instead of in jumps.
///
/// **The travel a doubling takes grows with the zoom.** At 100% the hand crosses `PEN_ZOOM_TRAVEL` pixels to
/// double the sheet; at 400% it must cross twice that. That looks backwards until the screen is considered:
/// a gesture is a distance in *window* pixels, and at 400% a window pixel is a quarter of a sheet pixel, so
/// the same finger travel is already four times as much of the page. Without the correction, a zoom held at
/// 400% would fly off the page on the same stroke that felt right at 100% — which is exactly what a reader
/// described as having to "barely touch" the sheet to make it move.
///
/// The correction is the *fourth root* of the zoom rather than the zoom itself, or its square root. Full
/// proportionality would make a high zoom almost immune to the hand; no correction at all would make the same
/// stroke fly four times as far at 400% as at 100%. In between there are two candidates, and the measurements
/// decide it: 50 pixels of travel at a fixed rate moved the sheet by `1.19` at every zoom, which is the
/// target shape; the square root gave `1.41` at 25% and only `1.06` at 800% — gentler than the fixed rate it
/// replaced, which is the opposite of the complaint. The fourth root gives `1.28` at 25% and `1.11` at 800%:
/// always at least as responsive as before, and calmer as the sheet gets larger.
///
/// So a doubling takes 200 pixels at 100%, 283 at 400%, 336 at 800% and 141 at 25% — every one a comfortable
/// travel for a wrist, and none of them a twitch.
///
/// The result is clamped like every other zoom: a stray reading from a driver, or a hand that left the
/// surface and came back, should not take the sheet from nothing to everything in one step.
pub fn pen_zoom_factor(rise: f32, from: f32) -> f32 {
    if !rise.is_finite() || !from.is_finite() || from <= 0.0 {
        return 1.0;
    }

    // How many window pixels this zoom needs for a doubling, and the per-pixel rate that follows from it.
    let travel = PEN_ZOOM_TRAVEL * from.max(0.0625).powf(PEN_ZOOM_CORRECTION);
    let per_pixel = 2.0_f32.powf(1.0 / travel);

    (per_pixel.powf(rise)).clamp(0.2, 5.0)
}

/// How strongly the travel grows with the zoom: `travel = PEN_ZOOM_TRAVEL * zoom^this`.
///
/// The fourth root, and the numbers behind it are in [`pen_zoom_factor`]'s own note. Zero would be a fixed
/// rate, which is too quick at a high zoom; a half is the square root, which is too slow there; and a quarter
/// is the one that stays responsive without flying.
pub const PEN_ZOOM_CORRECTION: f32 = 0.25;

/// How far the nib crosses, in window logical pixels, to double the sheet when the sheet is at its own size.
///
/// Two hundred pixels is about a hand's width on a laptop trackpad, and a little over half a finger's travel
/// on a tablet: a stroke that is a deliberate gesture rather than a twitch, and short enough that a reader
/// need not lift their hand to find out what happened. A wheel's own rate is a doubling every 298 pixels, so
/// the pen is the quicker of the two — deliberately, because a hand moving a pen has far finer control than a
/// wheel being spun, and the earlier 300 sat closer to the wheel than to the hand.
pub const PEN_ZOOM_TRAVEL: f32 = 200.0;

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
