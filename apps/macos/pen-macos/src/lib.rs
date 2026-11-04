//! Pen input on macOS, in three numbers: where the nib is, how hard it is pressed, and how it leans.
//!
//! ## What this crate is
//!
//! A local `NSEvent` monitor, and a closure that is offered each reading from inside AppKit's own event
//! delivery — on the main thread, before the event reaches anything else. That is the whole of it: no
//! window of its own, no thread, no queue, no ink engine, no smoothing. A caller decides what a reading
//! means.
//!
//! ## The three values, and the traps each one hides
//!
//! | Value | Where it comes from | The trap |
//! |---|---|---|
//! | `x`, `y` | `NSEvent.locationInWindow`, converted by the view | AppKit's window coordinates have the origin at the **bottom left** and are in points; a client pixel's origin is the **top left**, and a Retina display has two of them per point |
//! | `pressure` | `NSEvent.pressure` | a **mouse** reports `0` or `1` here, which is a button and not a force; a tablet's is `0.0..=1.0` |
//! | `tilt` | `NSEvent.tilt` | it is **normalized to -1..1, not degrees**, and it is only valid for tablet events |
//!
//! Two more things that make a working pen look broken:
//!
//! * A pen is not only a `NSEventTypeTabletPoint` event — it is usually a **mouse event carrying the
//!   `.tabletPoint` subtype**. Both are read here, and a report that arrives twice (a driver may send
//!   both) is dropped by its timestamp so the ink is not laid down twice.
//! * A device that cannot report an axis sends zeros, and the only place that says so is the
//!   **proximity** event's `capabilityMask` (`NX_TABLET_CAPABILITY_TILTXMASK`, `PRESSUREMASK`, … in
//!   `<IOKit/hidsystem/IOLLEvent.h>`). Until a proximity report arrives the mask is unknown, and this
//!   crate gives the device the benefit of the doubt — a pen that says nothing about its mask almost
//!   always has the sensors.
//!
//! ## What it does not do
//!
//! It does not change `NSEvent.mouseCoalescingEnabled`. Turning AppKit's coalescing off multiplies the
//! readings several times over, and whether that is a good trade is a decision for the application that
//! has to draw them — not something a capture should decide on the way past.
//!
//! It also does not predict: macOS has no equivalent of iOS's `predictedTouches`, which is why a line
//! here trails the nib by whatever the system and the frame cost.

use std::cell::Cell;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSEvent, NSEventButtonMask, NSEventMask, NSEventSubtype, NSEventType, NSPointingDeviceType,
    NSView,
};
use objc2_foundation::NSProcessInfo;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// What a caller does with each reading.
///
/// Called on the main thread, once per event, in order. It must not block: whatever it does is time the
/// next event spends waiting.
pub type ReadingTap = Arc<dyn Fn(Reading) + Send + Sync>;

/// Which part of the pen's visit this reading belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Nothing has been seen: no pen in range.
    #[default]
    Idle,
    /// The pen came into range.
    Enter,
    /// In range and not touching the surface.
    Hover,
    /// The nib touched the surface. One per stroke: a stroke begins here.
    Down,
    /// The nib is on the surface and moving.
    Move,
    /// The nib left the surface. One per stroke: a stroke ends here.
    Up,
    /// The pen left detection range.
    Leave,
    /// The system took the pointer away; a stroke should end where it was last seen.
    Cancel,
}

/// How the pen leans, in degrees: `x` positive to the right, `y` positive toward the user.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tilt {
    /// Lean along the x axis, in degrees.
    pub x: f32,
    /// Lean along the y axis, in degrees.
    pub y: f32,
}

impl Tilt {
    /// A lean of the given two angles.
    pub const fn new(x: f32, y: f32) -> Self {
        Tilt { x, y }
    }
}

/// Degrees per unit of `NSEvent.tilt`, which is normalized to `-1.0..=1.0` — *not* degrees.
///
/// The header says only "range is -1 to 1 for both axes". The range is the whole of the conversion that
/// is documented, so this is the linear reading of it: `±1` is `±90°`, a pen on its side. If a device
/// turns out to report the *sine* of the angle instead, this constant is the one line to change.
pub const TILT_DEGREES_PER_UNIT: f32 = 90.0;

/// One reading: where the nib is, how hard it is pressed, how it leans, and when.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reading {
    /// Which part of the pen's visit this is.
    pub phase: Phase,
    /// The tablet this came from, or `0` for the mouse.
    pub device: u32,
    /// The nib's horizontal position, in **physical client pixels**.
    pub x: f32,
    /// The nib's vertical position, in physical client pixels, growing downwards.
    pub y: f32,
    /// Nib force, `0.0..=1.0`, or `None` when the device has no pressure axis — and always `None` for
    /// the mouse, whose `NSEvent.pressure` is a button rather than a force.
    pub pressure: Option<f32>,
    /// Lean from the surface normal, or `None` when the device reports no tilt.
    pub tilt: Option<Tilt>,
    /// What the device says it can report, from its proximity report: the `NX_TABLET_CAPABILITY_*`
    /// bits of `capabilityMask`.
    ///
    /// `0` means no proximity report has been seen yet, which is *not* the same as "reports nothing".
    /// This is the field that answers "why is tilt always zero": a device without
    /// `NX_TABLET_CAPABILITY_TILTXMASK` and `TILTYMASK` sends zeros for `tilt`, and only this says so.
    pub capabilities: u64,
    /// Whether the eraser end is toward the screen.
    pub eraser: bool,
    /// When the reading was taken, in microseconds on AppKit's clock (boot-relative).
    ///
    /// Only differences are ever useful, so the origin is the platform's business.
    pub at_us: u64,
    /// How long the reading waited between AppKit's stamp and this process, in milliseconds.
    ///
    /// The part of the latency nothing in this process can remove. `0` when the two clocks cannot be
    /// compared — a stamp from the future, or from before a sleep, is not a delay.
    pub delay_ms: u16,
}

impl fmt::Display for Reading {
    /// The line the example prints: `Down 512.0,388.5 p=0.42 tilt=+12.0/-4.0 3ms`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:.1},{:.1}", self.phase, self.x, self.y)?;

        if let Some(pressure) = self.pressure {
            write!(f, " p={pressure:.2}")?;
        }

        if let Some(tilt) = self.tilt {
            write!(f, " tilt={:+.1}/{:+.1}", tilt.x, tilt.y)?;
        }

        if self.capabilities != 0 {
            write!(f, " caps=0x{:04x}", self.capabilities)?;
        }

        if self.eraser {
            write!(f, " eraser")?;
        }

        write!(f, " {}ms", self.delay_ms)
    }
}

/// The pen, for as long as this value lives.
///
/// Dropping it removes the monitor; nothing else has to be torn down.
pub struct Pen {
    /// The object `addLocalMonitorForEventsMatchingMask:handler:` returned, as a raw pointer.
    ///
    /// Kept as a `usize` so that this type is `Send` without an `unsafe impl`: the token is opaque, and
    /// [`Drop`] is the only place that touches it.
    monitor: Option<usize>,
}

impl Pen {
    /// Watches a window's pen, through whatever raw handle it reports.
    ///
    /// This is the constructor for an application whose window comes from a toolkit: the handle is an
    /// AppKit one, and the view inside it is where coordinates are converted.
    pub fn attach<W: HasWindowHandle>(window: &W, tap: ReadingTap) -> Option<Self> {
        let handle = HasWindowHandle::window_handle(window).ok()?;

        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };

        // SAFETY: AppKit hands out its own content view, which lives as long as the window, and this is
        // the main thread.
        let view = unsafe { appkit.ns_view.as_ptr().cast::<NSView>().as_ref() }?;

        Pen::attach_view(view, tap)
    }

    /// Watches a window's pen, given the view the readings are placed in.
    ///
    /// Must be called on the thread that owns the view: the monitor's block is AppKit's to call, on that
    /// thread, and it converts coordinates with this view.
    pub fn attach_view(view: &NSView, tap: ReadingTap) -> Option<Self> {
        let window = view.window()?;
        let window_number = window.windowNumber();

        // A pointer's approach is only reported to a window that asked for it, and a pen in range but
        // not touching is worth watching: it is where a `Hover` reading comes from.
        window.setAcceptsMouseMovedEvents(true);

        let view = std::ptr::from_ref(view) as usize;
        let anchor = Instant::now();
        // AppKit's stamps and this process's clock are two clocks; one `systemUptime` at attach is what
        // lets every reading after it be compared with the stamp the system put on it.
        let anchor_uptime = NSProcessInfo::processInfo().systemUptime();

        let last_stamp = Cell::new(f64::NAN);
        let last_phase = Cell::new(Phase::Idle);
        let capabilities = Cell::new(0u64);

        let block = RcBlock::new(move |event: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit passes a live event for the duration of the call.
            let event = unsafe { event.as_ref() };

            // A local monitor sees every window this application owns; only this one's readings are ours.
            if event.windowNumber() == window_number {
                let stamp = event.timestamp();

                // A pen reports through two paths — mouse events with a tablet subtype, and tablet events
                // of their own — and a driver may use both. The same report must not arrive twice.
                if stamp != last_stamp.get() {
                    last_stamp.set(stamp);

                    if is_proximity(event) {
                        let mask = event.capabilityMask() as u64;
                        if mask != 0 {
                            capabilities.set(mask);
                        }
                    }

                    let now = anchor_uptime + anchor.elapsed().as_secs_f64();

                    // SAFETY: the view is alive and this is the main thread.
                    let reading = unsafe {
                        reading_of(
                            event,
                            view as *const NSView,
                            capabilities.get(),
                            last_phase.get(),
                            stamp,
                            now,
                        )
                    };

                    if let Some(reading) = reading {
                        last_phase.set(reading.phase);
                        tap(reading);
                    }
                }
            }

            // Handed back untouched: this monitor reads the event, it does not consume it. Returning null
            // here would swallow the click the application is waiting for.
            event as *const NSEvent as *mut NSEvent
        });

        let mask = NSEventMask::LeftMouseDown
            | NSEventMask::LeftMouseUp
            | NSEventMask::LeftMouseDragged
            | NSEventMask::MouseMoved
            | NSEventMask::MouseEntered
            | NSEventMask::MouseExited
            | NSEventMask::TabletPoint
            | NSEventMask::TabletProximity;

        // SAFETY: the block is `'static`, AppKit copies it, and it holds only pointers that outlive the
        // monitor: the view belongs to the window, and the window to the application.
        let token = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }?;

        // SAFETY: `into_raw` hands the retain over to us, and `Drop` is what gives it back.
        let monitor = Retained::into_raw(token) as usize;

        Some(Pen {
            monitor: Some(monitor),
        })
    }
}

impl Drop for Pen {
    /// Removes the monitor, so a closed window leaves no callback behind.
    fn drop(&mut self) {
        if let Some(raw) = self.monitor.take() {
            // SAFETY: this pointer came from `Retained::into_raw` at attach and has never been released.
            if let Some(token) = unsafe { Retained::<AnyObject>::from_raw(raw as *mut AnyObject) } {
                // SAFETY: it is the object the monitor registration returned.
                unsafe { NSEvent::removeMonitor(&token) };
            }
        }
    }
}

/// `NX_TABLET_CAPABILITY_*` bits (`<IOKit/hidsystem/IOLLEvent.h>`): which axes the device can report.
const CAPABILITY_TILT_X: u64 = 0x0080;
const CAPABILITY_TILT_Y: u64 = 0x0100;
const CAPABILITY_PRESSURE: u64 = 0x0400;

/// Whether an event is a proximity report, in either of the two ways AppKit delivers one.
fn is_proximity(event: &NSEvent) -> bool {
    event.r#type() == NSEventType::TabletProximity
        || subtype_of(event) == Some(NSEventSubtype::TabletProximity)
}

/// The subtype of an event that has one, and `None` for an event that does not.
///
/// `-[NSEvent subtype]` **raises** `NSInternalInconsistencyException` — "Invalid message sent to event" —
/// for the kinds of event that have no subtype field, and an Objective-C exception cannot unwind through
/// this callback: the release profile aborts and all that is left is "panic in a function that cannot
/// unwind". A `MouseEntered` did exactly that, on a plain mouse move across the window.
///
/// So the question is asked only of the kinds that carry one: the four AppKit gives subtypes to, and the
/// mouse reports a pen sends its tablet data through. `MouseEntered` and `MouseExited` are deliberately
/// absent — they cannot be a pen's report, and asking them was the crash.
fn subtype_of(event: &NSEvent) -> Option<NSEventSubtype> {
    let carries_one = matches!(
        event.r#type(),
        NSEventType::AppKitDefined
            | NSEventType::SystemDefined
            | NSEventType::ApplicationDefined
            | NSEventType::Periodic
            | NSEventType::LeftMouseDown
            | NSEventType::LeftMouseUp
            | NSEventType::LeftMouseDragged
            | NSEventType::MouseMoved
    );

    carries_one.then(|| event.subtype())
}

/// One reading from one event, or `None` when the event is not about the pointer at all.
///
/// # Safety
///
/// `view` must be the live content view, and this must run on the main thread.
unsafe fn reading_of(
    event: &NSEvent,
    view: *const NSView,
    capabilities: u64,
    last_phase: Phase,
    stamp: f64,
    now: f64,
) -> Option<Reading> {
    let kind = event.r#type();
    let subtype = subtype_of(event);

    // A pen arrives as a mouse event carrying the tablet subtype, or as a tablet event of its own — and
    // a driver is free to send both.
    let tablet = kind == NSEventType::TabletPoint || subtype == Some(NSEventSubtype::TabletPoint);
    let proximity =
        kind == NSEventType::TabletProximity || subtype == Some(NSEventSubtype::TabletProximity);
    let tip_down = tablet && event.buttonMask().contains(NSEventButtonMask::PenTip);

    // `isEnteringProximity` is a proximity event's own property, and asking any other kind raises
    // `NSInternalInconsistencyException` ("Invalid message sent to event") — which cannot unwind through
    // this callback: the release profile aborts. So it is asked only where it means something, and the
    // other questions below are guarded the same way.
    let entering = proximity && event.isEnteringProximity();

    let phase = phase_of(kind, subtype, entering, tip_down, last_phase)?;

    // One fact on macOS: which end of the pen is down.
    let eraser =
        (tablet || proximity) && event.pointingDeviceType() == NSPointingDeviceType::Eraser;

    // SAFETY: the caller's contract.
    let (x, y) = unsafe { pixel_of(event, view) };

    Some(Reading {
        phase,
        device: if tablet { event.deviceID() as u32 } else { 0 },
        x,
        y,
        pressure: pressure_of(event, capabilities, tablet),
        tilt: tilt_of(event, capabilities, tablet),
        capabilities,
        eraser,
        at_us: (stamp * 1_000_000.0) as u64,
        delay_ms: behind_ms(stamp, now),
    })
}

/// The phase an event reports.
///
/// The edges come from the mouse stream, which is what a stylus generates whatever else it sends: a
/// down, a drag, an up. A legacy `TabletPoint` event has no edge of its own, so its tip bit is turned
/// into one against the phase that came before it — otherwise a driver that sent only those could never
/// start or finish a stroke.
fn phase_of(
    kind: NSEventType,
    subtype: Option<NSEventSubtype>,
    entering_proximity: bool,
    tip_down: bool,
    last: Phase,
) -> Option<Phase> {
    // Proximity is its own event in both spellings, and it is the only place AppKit says the pen has
    // arrived or left rather than moved.
    if kind == NSEventType::TabletProximity || subtype == Some(NSEventSubtype::TabletProximity) {
        return Some(if entering_proximity {
            Phase::Enter
        } else {
            Phase::Leave
        });
    }

    if kind == NSEventType::LeftMouseDown {
        return Some(Phase::Down);
    }
    if kind == NSEventType::LeftMouseDragged {
        return Some(Phase::Move);
    }
    if kind == NSEventType::LeftMouseUp {
        return Some(Phase::Up);
    }
    if kind == NSEventType::MouseMoved {
        return Some(Phase::Hover);
    }
    if kind == NSEventType::MouseEntered {
        return Some(Phase::Enter);
    }
    // A pointer that leaves the window mid-stroke ends where it was last seen, and that is what
    // `Cancel` means: not the next place it appears, which would be a line across the window.
    if kind == NSEventType::MouseExited {
        return Some(Phase::Cancel);
    }
    if kind == NSEventType::TabletPoint {
        let touching = matches!(last, Phase::Down | Phase::Move);

        return Some(match (tip_down, touching) {
            (true, true) => Phase::Move,
            (true, false) => Phase::Down,
            (false, true) => Phase::Up,
            (false, false) => Phase::Hover,
        });
    }

    None
}

/// The force a reading carries, or `None` when the device has no pressure axis.
///
/// A mouse's `NSEvent.pressure` is `0` or `1` — a button, not a force — and a light touch and a full
/// press are drawn at different widths, so the two must not be confused. For a tablet the capability
/// mask decides; a driver that reports no mask is given the benefit of the doubt.
fn pressure_of(event: &NSEvent, capabilities: u64, tablet: bool) -> Option<f32> {
    if !tablet {
        return None;
    }

    if capabilities != 0 && capabilities & CAPABILITY_PRESSURE == 0 {
        return None;
    }

    Some(event.pressure().clamp(0.0, 1.0))
}

/// The lean a reading carries, in degrees, or `None` when the device reports no tilt.
fn tilt_of(event: &NSEvent, capabilities: u64, tablet: bool) -> Option<Tilt> {
    if !tablet {
        return None;
    }

    let tilts = CAPABILITY_TILT_X | CAPABILITY_TILT_Y;
    if capabilities != 0 && capabilities & tilts != tilts {
        return None;
    }

    let tilt = event.tilt();

    Some(Tilt::new(tilt_degrees(tilt.x), tilt_degrees(tilt.y)))
}

/// One axis of `NSEvent.tilt`, from its `-1.0..=1.0` to degrees.
fn tilt_degrees(unit: f64) -> f32 {
    (unit * f64::from(TILT_DEGREES_PER_UNIT)) as f32
}

/// Where the nib is, in physical client pixels with the origin at the top left.
///
/// # Safety
///
/// `view` must be the live content view, and this must run on the main thread.
///
/// AppKit reports `locationInWindow` from the window's **bottom**-left corner and in points; the view is
/// what knows both the height to flip with and the backing scale factor to multiply by. The flip happens
/// only when the view is not already flipped itself, so the answer is the same whichever way the
/// application's view is built.
unsafe fn pixel_of(event: &NSEvent, view: *const NSView) -> (f32, f32) {
    // SAFETY: the caller's contract.
    let view = unsafe { &*view };
    let mut point = view.convertPoint_fromView(event.locationInWindow(), None);

    if !view.isFlipped() {
        point.y = view.bounds().size.height - point.y;
    }

    let backing = view.convertPointToBacking(point);

    (backing.x as f32, backing.y as f32)
}

/// How long a reading waited between AppKit's stamp and this process, in milliseconds.
///
/// Both are boot-relative seconds, so the subtraction is the delay — except when it is not: a stamp that
/// is ahead of the clock, a process that slept, a clock that disagrees. Those are reported as `0`, which
/// is the honest "not known", rather than as a number that would pin a worst case forever.
pub fn behind_ms(stamp: f64, now: f64) -> u16 {
    let behind = now - stamp;

    // Two seconds is already longer than any event waits, so anything outside that is a disagreement
    // between the clocks rather than a delay.
    if !behind.is_finite() || !(0.0..2.0).contains(&behind) {
        return 0;
    }

    (behind * 1_000.0).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mouse path: an edge per button, a hover for a move, and nothing for an event that is not
    /// about the pointer.
    #[test]
    fn the_mouse_reports_the_edges_a_stroke_needs() {
        let plain = Some(NSEventSubtype::MouseEvent);
        let idle = Phase::Idle;

        assert_eq!(
            phase_of(NSEventType::LeftMouseDown, plain, false, false, idle),
            Some(Phase::Down)
        );
        assert_eq!(
            phase_of(NSEventType::LeftMouseDragged, plain, false, false, idle),
            Some(Phase::Move)
        );
        assert_eq!(
            phase_of(NSEventType::LeftMouseUp, plain, false, false, idle),
            Some(Phase::Up)
        );
        assert_eq!(
            phase_of(NSEventType::MouseMoved, plain, false, false, idle),
            Some(Phase::Hover)
        );
        assert_eq!(
            phase_of(NSEventType::MouseExited, plain, false, false, idle),
            Some(Phase::Cancel)
        );
        assert_eq!(
            phase_of(NSEventType::KeyDown, plain, false, false, idle),
            None,
            "a key is not a reading"
        );
    }

    /// Proximity is the only place the pen says it has arrived or left, in both its spellings.
    #[test]
    fn proximity_is_the_pens_own_edge() {
        let idle = Phase::Idle;

        assert_eq!(
            phase_of(
                NSEventType::TabletProximity,
                Some(NSEventSubtype::TabletProximity),
                true,
                false,
                idle
            ),
            Some(Phase::Enter)
        );
        assert_eq!(
            phase_of(
                NSEventType::TabletProximity,
                Some(NSEventSubtype::TabletProximity),
                false,
                false,
                idle
            ),
            Some(Phase::Leave)
        );
    }

    /// A driver that sends only legacy `TabletPoint` events still gets a stroke with two ends: the tip
    /// bit is turned into an edge against the phase before it.
    #[test]
    fn a_tablet_point_event_gets_its_edges_from_the_tip_bit() {
        let point = NSEventType::TabletPoint;
        let tablet = Some(NSEventSubtype::TabletPoint);

        assert_eq!(
            phase_of(point, tablet, false, true, Phase::Hover),
            Some(Phase::Down)
        );
        assert_eq!(
            phase_of(point, tablet, false, true, Phase::Down),
            Some(Phase::Move)
        );
        assert_eq!(
            phase_of(point, tablet, false, false, Phase::Move),
            Some(Phase::Up)
        );
        assert_eq!(
            phase_of(point, tablet, false, false, Phase::Up),
            Some(Phase::Hover)
        );
    }

    /// Tilt is normalized, and the conversion is the one place that says so.
    #[test]
    fn tilt_is_normalized_and_becomes_degrees() {
        assert_eq!(tilt_degrees(0.0), 0.0);
        assert_eq!(tilt_degrees(1.0), 90.0);
        assert_eq!(tilt_degrees(-1.0), -90.0);
        assert_eq!(tilt_degrees(0.5), 45.0);
    }

    /// The delay is a subtraction of two boot-relative clocks — and anything that is not a delay is
    /// reported as none rather than as a number nobody should see.
    #[test]
    fn a_delay_is_a_delay_and_a_broken_clock_is_zero() {
        assert_eq!(behind_ms(10.000, 10.001), 1);
        assert_eq!(behind_ms(10.0, 10.012), 12);
        assert_eq!(behind_ms(10.0, 10.0), 0);

        assert_eq!(
            behind_ms(10.5, 10.0),
            0,
            "a stamp from the future is not a delay"
        );
        assert_eq!(behind_ms(10.0, 4_000.0), 0, "a sleep is not a delay");
        assert_eq!(behind_ms(f64::NAN, 10.0), 0);
        assert_eq!(behind_ms(10.0, f64::INFINITY), 0);
    }
}
