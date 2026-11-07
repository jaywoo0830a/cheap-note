//! The gamepad's sticks: the sheet's pan and zoom, and the desktop's pointer and scroll.
//!
//! A stick is a *position*, not a queue of readings: there is nothing to wait on, and the frame loop
//! already has a clock. [`Gamepads::read`] is therefore read by the housekeeping pump, and answers
//! with the distance the sheet should move and the factor its zoom should change by since the last
//! call — the same kinds of number the wheel gives (see `NoteApp::on_wheel`).
//!
//! The left stick pans and the right stick's **Y axis** zooms; while another application is in
//! front, the same sticks are the system's pointer and its scroll instead — the right stick's X axis
//! is read for the pointer but never for the zoom, which is the vertical one.
//!
//! `gilrs` is a cross-platform library, like GPUI Kit, so this lives above the platform seam: one
//! implementation reads an Xbox pad on Windows and a DualSense on a Mac. No gamepad is not an error;
//! the sticks simply ask for nothing.

use std::time::{Duration, Instant};

use gilrs::{Axis, Button, Gilrs};

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

/// How fast a fully deflected right stick moves the system pointer, in logical pixels per second.
///
/// Only used while another application is in front: then the right stick is the pointer, and the
/// same position that zooms the note is a velocity here. Much slower than the in-app pan: a cursor
/// is aimed at a button, not swept across a page.
const GAMEPAD_POINTER_SPEED: f32 = 500.0;

/// How far the system pointer's stick has to leave its centre before it moves.
///
/// `gilrs` already applies a 0.1 dead zone; this one is larger because a pointer is a much finer
/// instrument than a page: a worn stick's 0.15 is a drift a person sees, so it is zero here and the
/// rest of the travel is rescaled to make full deflection still full speed.
const GAMEPAD_POINTER_DEADZONE: f32 = 0.2;

/// How sharply the pointer's speed rises with deflection.
///
/// `1.0` is linear; larger values make a gentle push slower without changing the speed at full
/// deflection. The pointer is the one control aimed at a button rather than swept, so the curve is
/// what gives a light touch its precision and a firm push its reach.
const GAMEPAD_POINTER_CURVE: f32 = 1.5;

/// How fast a fully deflected left stick scrolls, in logical pixels per second.
///
/// The left stick's in-app twin is a pan at [`GAMEPAD_PAN_SPEED`]; a scroll is a little slower
/// because it moves the content of whatever window is under the pointer, not a sheet at a known size.
const GAMEPAD_SCROLL_SPEED: f32 = 900.0;

/// How far the scroll's stick has to leave its centre before it moves.
///
/// `gilrs` already applies a 0.1 dead zone; this is the same edge, named here because the curve is
/// applied after it and the rest of the travel is rescaled to make full deflection still full speed.
const GAMEPAD_SCROLL_DEADZONE: f32 = 0.1;

/// How sharply the scroll's speed rises with deflection.
///
/// `1.0` is linear; larger values make a gentle push slower without changing the speed at full
/// deflection. The scroll is a system control like the pointer, so it gets the same shape: a light
/// touch creeps, and a firm push crosses the page.
const GAMEPAD_SCROLL_CURVE: f32 = 1.5;

/// The longest wake the sticks are paid for.
///
/// A laptop that slept for a minute would otherwise jump the sheet by a minute's worth of pan and
/// zoom — or the pointer across the desktop — the first time the sticks were read again.
const GAMEPAD_LONGEST_STEP: Duration = Duration::from_millis(100);

/// What the two sticks, the shoulders and the toggle chord asked for since the last read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sticks {
    /// How far the sheet should move, in logical pixels.
    pub pan: (f32, f32),
    /// The factor the zoom should change by; `1.0` when the right stick is still.
    pub zoom: f32,
    /// How far the system pointer should move, in logical pixels: right and down are positive.
    pub pointer: (f32, f32),
    /// How far the window under the pointer should scroll, in logical pixels: right and up are
    /// positive.
    pub scroll: (f32, f32),
    /// The left shoulder's change since the last read: `Some(true)` pressed, `Some(false)` released.
    pub left: Option<bool>,
    /// The right shoulder's change since the last read.
    pub right: Option<bool>,
    /// Whether Start and Select were pressed together since the last read: the one chord that turns
    /// the desktop mouse mode on and off. A pulse, not a state.
    pub toggle: bool,
}

impl Default for Sticks {
    fn default() -> Self {
        Sticks {
            pan: (0.0, 0.0),
            zoom: 1.0,
            pointer: (0.0, 0.0),
            scroll: (0.0, 0.0),
            left: None,
            right: None,
            toggle: false,
        }
    }
}

/// What one pad is saying, before it is turned into distances.
#[derive(Clone, Copy, Debug, Default)]
struct Input {
    left: (f32, f32),
    right: (f32, f32),
    left_button: bool,
    right_button: bool,
    start: bool,
    select: bool,
}

impl Input {
    /// Whether this pad is the one being held: any stick off its centre, or a button down.
    fn is_held(self) -> bool {
        self.left != (0.0, 0.0)
            || self.right != (0.0, 0.0)
            || self.left_button
            || self.right_button
            || self.start
            || self.select
    }
}

/// The gamepads the app can see, when the sticks were last read, and which shoulders were down.
pub struct Gamepads {
    /// `None` when gamepad support could not be opened; the sticks are then always still.
    gilrs: Option<Gilrs>,
    /// When [`Self::read`] was last called, so a position becomes a distance.
    last: Instant,
    /// The shoulders as the last read saw them, so a read can report the change rather than a state.
    left_down: bool,
    right_down: bool,
    /// Whether Start and Select were both down at the last read, so the chord is one pulse.
    chord_down: bool,
    /// Whether the last read saw any input, so the trace can say when a gesture begins and ends.
    was_held: bool,
    /// When the trace last printed the sticks, so a held stick is one line every quarter second.
    last_input_log: Instant,
}

impl Gamepads {
    /// Opens the platform's gamepad support, if there is any.
    pub fn new() -> Self {
        let gilrs = Gilrs::new().ok();

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            if let Some(gilrs) = gilrs.as_ref() {
                for (id, pad) in gilrs.gamepads() {
                    eprintln!("gamepad {id}: {} connected", pad.name());
                }
            }
        }

        Gamepads {
            gilrs,
            last: Instant::now(),
            left_down: false,
            right_down: false,
            chord_down: false,
            was_held: false,
            last_input_log: Instant::now(),
        }
    }

    /// Whether a pad is connected, for a status line or a trace.
    pub fn is_connected(&self) -> bool {
        self.gilrs
            .as_ref()
            .is_some_and(|gilrs| gilrs.gamepads().next().is_some())
    }

    /// What the sticks and shoulders ask for since the last call.
    ///
    /// The pan's direction is the view's: pushing the left stick right looks further right, so the
    /// sheet moves left, and pushing it up looks earlier in the page, so the sheet moves down — the
    /// rule a wheel follows and a document viewer has. Pushing the right stick away from the reader
    /// zooms in; while another app is in front, that stick is the system's pointer instead, and the
    /// left one is its scroll. The events are drained because that is what keeps `gilrs`'s cached
    /// state current; what they say is not otherwise this app's business. `gilrs` has already
    /// applied each pad's dead zone, so a stick at rest reads as zero rather than as drift.
    pub fn read(&mut self) -> Sticks {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;

        let Some(gilrs) = self.gilrs.as_mut() else {
            return Sticks::default();
        };

        let trace = std::env::var_os("CHEAP_NOTE_TRACE").is_some();

        while let Some(event) = gilrs.next_event() {
            if trace {
                match event.event {
                    gilrs::EventType::Connected => {
                        eprintln!("gamepad {}: connected", event.id);
                    }
                    gilrs::EventType::Disconnected => {
                        eprintln!("gamepad {}: disconnected", event.id);
                    }
                    _ => {}
                }
            }
        }

        // The first pad being held is the one that counts: two hands on two pads is not a gesture
        // this app has, and adding them together would double the speed. `Input::default` is a pad
        // with nothing held, which is also what a released shoulder is measured against.
        let input = gilrs
            .gamepads()
            .map(|(_, pad)| Input {
                left: (pad.value(Axis::LeftStickX), pad.value(Axis::LeftStickY)),
                right: (pad.value(Axis::RightStickX), pad.value(Axis::RightStickY)),
                left_button: pad.is_pressed(Button::LeftTrigger),
                right_button: pad.is_pressed(Button::RightTrigger),
                start: pad.is_pressed(Button::Start),
                select: pad.is_pressed(Button::Select),
            })
            .find(|input| input.is_held())
            .unwrap_or_default();

        let held = input.is_held();
        if trace && held != self.was_held {
            eprintln!(
                "gamepad: {} left=({:.2}, {:.2}) right=({:.2}, {:.2}) L={} R={} start={} select={}",
                if held { "input" } else { "still" },
                input.left.0,
                input.left.1,
                input.right.0,
                input.right.1,
                input.left_button,
                input.right_button,
                input.start,
                input.select
            );
        }
        self.was_held = held;

        if trace && held {
            let logged = Instant::now();
            if logged.saturating_duration_since(self.last_input_log) >= Duration::from_millis(250) {
                eprintln!(
                    "gamepad: left=({:.2}, {:.2}) right=({:.2}, {:.2}) L={} R={} start={} select={}",
                    input.left.0,
                    input.left.1,
                    input.right.0,
                    input.right.1,
                    input.left_button,
                    input.right_button,
                    input.start,
                    input.select
                );
                self.last_input_log = logged;
            }
        }

        Sticks {
            pan: stick_pan(input.left.0, input.left.1, elapsed),
            zoom: stick_zoom(input.right.1, elapsed),
            pointer: stick_pointer(input.right.0, input.right.1, elapsed),
            scroll: stick_scroll(input.left.0, input.left.1, elapsed),
            left: button_edge(input.left_button, &mut self.left_down),
            right: button_edge(input.right_button, &mut self.right_down),
            toggle: chord_edge(input.start && input.select, &mut self.chord_down),
        }
    }
}

/// Reports whether a chord has just been completed: one pulse per press, not one per read.
fn chord_edge(both: bool, was: &mut bool) -> bool {
    let pressed = both && !*was;
    *was = both;
    pressed
}

/// Reports a shoulder's change, if it changed, and remembers the new state.
fn button_edge(pressed: bool, was: &mut bool) -> Option<bool> {
    if *was == pressed {
        return None;
    }

    *was = pressed;
    Some(pressed)
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

/// The distance the system pointer moves for a right stick at `(x, y)` held for `elapsed`.
///
/// Screen coordinates grow downward, so the vertical sign is the one that flips: a stick pushed away
/// from the reader moves the pointer up, and right is right. A pointer dead zone larger than the
/// pad's is applied first, because a cursor shows a drift the sheet would hide.
fn stick_pointer(x: f32, y: f32, elapsed: Duration) -> (f32, f32) {
    let distance = GAMEPAD_POINTER_SPEED * elapsed.min(GAMEPAD_LONGEST_STEP).as_secs_f32();
    (pointer_axis(x) * distance, -pointer_axis(y) * distance)
}

/// One axis of a stick with a dead zone applied, the rest rescaled, and a curve shaping how fast
/// the speed rises with the push.
fn curved_axis(value: f32, deadzone: f32, curve: f32) -> f32 {
    if value.abs() <= deadzone {
        return 0.0;
    }

    let axis = ((value.abs() - deadzone) / (1.0 - deadzone)).clamp(0.0, 1.0);
    value.signum() * axis.powf(curve)
}

/// One axis of the pointer's stick, curved by [`GAMEPAD_POINTER_CURVE`].
fn pointer_axis(value: f32) -> f32 {
    curved_axis(value, GAMEPAD_POINTER_DEADZONE, GAMEPAD_POINTER_CURVE)
}

/// One axis of the scroll's stick, curved by [`GAMEPAD_SCROLL_CURVE`].
fn scroll_axis(value: f32) -> f32 {
    curved_axis(value, GAMEPAD_SCROLL_DEADZONE, GAMEPAD_SCROLL_CURVE)
}

/// The scroll a left stick at `(x, y)` asks for over `elapsed`: a wheel turned away scrolls up, and
/// the push is curved like the pointer's, so a light touch is precise and full deflection is the
/// speed above.
fn stick_scroll(x: f32, y: f32, elapsed: Duration) -> (f32, f32) {
    let distance = GAMEPAD_SCROLL_SPEED * elapsed.min(GAMEPAD_LONGEST_STEP).as_secs_f32();
    (scroll_axis(x) * distance, scroll_axis(y) * distance)
}

#[cfg(test)]
mod tests;
