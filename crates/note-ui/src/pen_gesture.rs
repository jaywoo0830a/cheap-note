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
/// `rise` is how far up the nib has moved since the gesture began, in logical pixels: up is closer, so a
/// positive rise is a factor above one. The rate is one number, `PEN_ZOOM_TRAVEL`, and it does not depend on
/// the zoom: a nib that travels 200 pixels doubles the sheet whether the sheet is at 25% or at 800%.
///
/// That is deliberate, and it is the simpler of the two rules. The scroll wheel scales its own step with the
/// zoom, which is right for a wheel — a wheel *is* a series of detents, and there is no way to spin one by a
/// distance. A nib is not: the hand knows exactly how far it moved, and the only thing a reader has to learn is
/// "this much travel is this much zoom". Scaling that by the current zoom means the same gesture is a different
/// size depending on where you are, which is a rule nobody can hold in their head — and the two corrections
/// that were tried here (the square root, the fourth root) were each a guess about which way to bend it.
///
/// The gesture is still absolute: it is measured from where the nib went down, so a hand that goes up and comes
/// back returns the sheet to the zoom it started at. The clamp is the one guard, for a stray reading or a hand
/// that left the surface and came back.
pub fn pen_zoom_factor(rise: f32) -> f32 {
    if !rise.is_finite() {
        return 1.0;
    }

    (2.0_f32.powf(rise / PEN_ZOOM_TRAVEL)).clamp(0.2, 5.0)
}

/// How far the nib travels, in logical pixels, to double the sheet — at any zoom.
///
/// Two hundred pixels is a little over half a finger's movement on a tablet. That is shorter than a wheel's
/// doubling (298 pixels), deliberately: a hand on a pen has far more control than a wheel being spun, and the
/// pen should reward that rather than make the reader spin.
pub const PEN_ZOOM_TRAVEL: f32 = 200.0;

/// How far the sheet moves for each pixel the nib travels while a pan is in play.
///
/// **Two**, so the paper moves twice as far as the hand. A pen can only travel as far as the screen allows —
/// a hand that starts near the bottom of a tablet and pushes up has one screen's height to work with, and the
/// sheet it is dragging is often taller than that. A gain of one makes a long page unreachable without lifting
/// and re-placing the pen, which is a gesture a reader should never have to make; two lets one comfortable
/// sweep cover twice the page.
///
/// It is a *gain*, not a scale on a unit — the nib's own distance is already in the same logical pixels the
/// sheet is placed in, so one would be an exact 1:1 and this simply asks for more than the hand gave. Which
/// way the paper moves is unchanged: the nib going right still shows what is further left, exactly as a
/// two-finger drag does, because the paper follows the hand rather than the hand following the paper.
pub const PEN_PAN_GAIN: f32 = 2.0;

/// The smallest movement of the nib, in logical pixels, that a gesture treats as the hand's.
///
/// A hand resting on a tablet is not still: the digitizer reports the nib wandering by a fraction of a pixel
/// from one reading to the next, and a machine with a spinning fan reports rather more of it. During a pan
/// that wandering is multiplied by [`PEN_PAN_GAIN`] and lands on the sheet, so a page being held still visibly
/// shivers — and during a zoom, where the factor is an exponent of the travel, a tenth of a pixel of jitter
/// is a tenth of a pixel of breathing in the sheet's size.
///
/// **Two tenths of a pixel** is the number this was tuned to. It is far below what a hand can aim at — a nib
/// is about a millimetre wide, and a fifth of a pixel is a tiny fraction of that on the glass — so nothing a
/// reader means to do is lost, while the reading-to-reading noise a fan puts into a still hand sits under it.
/// It is a *threshold on the reading*, not a dead zone on the gesture: the nib is compared with where the
/// hand has settled rather than with the last raw reading, so a slow deliberate drift is followed exactly and
/// only the chatter is held back (see [`PenJitter`]).
pub const PEN_JITTER_SPAN: f32 = 0.2;

/// What a pen's position is reported as, with the hand's own trembling taken out.
///
/// The digitizer's readings carry noise — the hand is never perfectly still, and a spinning fan shakes the
/// whole tablet — and a gesture that follows every one of them shows the noise as movement. The two gestures
/// make that worse in different ways: a pan multiplies it by [`PEN_PAN_GAIN`], and a zoom turns it into a
/// change of the sheet's size, which is the most visible thing on the screen.
///
/// ## Why this is not a low-pass filter
///
/// A filter that averaged the last few readings would smooth the tremor, and it would also **lag**: the nib
/// would trail the hand by however many readings were averaged. A pen is the one input where that is
/// unacceptable — the whole point of writing on a screen is that the ink is under the nib — and a gesture is
/// judged by the same rule. So this holds the hand's *settled* position instead: readings within
/// [`PEN_JITTER_SPAN`] of it are the tremor and are ignored, and a reading beyond that is the hand moving and
/// is followed at once. The output is a staircase rather than a ramp, which is exactly what a step of the
/// hand should look like — and it costs no latency on the readings that mean something.
///
/// The one thing it must not do is *accumulate*: a hand creeping steadily in steps below the span would be
/// held at the settled point forever. So the settle point moves with the reading once a reading has been
/// beyond the span (see [`Self::at`]), and the hand's slow drift is followed within one span of it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PenJitter {
    /// Where the hand is taken to be, once the tremor is set aside.
    settled: Option<(f32, f32)>,
}

impl PenJitter {
    /// A filter with nothing in it yet: the first reading it is given is where the hand is.
    pub fn new() -> Self {
        Self { settled: None }
    }

    /// Where the nib is, as far as the gesture is concerned, given a reading at `at`.
    ///
    /// The first reading settles immediately — there is no tremor to weigh it against yet, and a gesture's
    /// origin is where the nib went down. After that a reading within [`PEN_JITTER_SPAN`] of the settled
    /// point is the tremor and answers the settled point; one beyond it is the hand and settles there.
    ///
    /// The distance is measured the way the gestures measure it, in logical window pixels, so the span means
    /// the same thing to a pan and to a zoom.
    ///
    /// A reading that is not a number settles where it is rather than poisoning every answer after it: a
    /// driver that reported one `NaN` would otherwise hold the filter for the rest of the gesture.
    pub fn at(&mut self, at: (f32, f32)) -> (f32, f32) {
        let Some(settled) = self.settled else {
            self.settled = Some(at);
            return at;
        };

        if !at.0.is_finite() || !at.1.is_finite() {
            return settled;
        }

        let (dx, dy) = (at.0 - settled.0, at.1 - settled.1);

        if dx.hypot(dy) >= PEN_JITTER_SPAN {
            self.settled = Some(at);
            return at;
        }

        settled
    }
}

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
