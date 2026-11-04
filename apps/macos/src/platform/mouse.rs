//! The system pointer, driven by the gamepad while another application is in front.
//!
//! ## What this is
//!
//! Cheap-note's gamepad has two jobs. While the app is in front, its sticks pan and zoom the note.
//! While another app is in front, the same sticks are that app's mouse: the right one moves the
//! pointer, the left one scrolls, and the shoulders click. The view decides *when* and *how much*
//! (see `gamepad.rs`); this file is the macOS half — it posts the events.
//!
//! ## Why it needs a permission
//!
//! Synthetic mouse events are not a link-time capability: `CGEventPost` into the HID stream is
//! exactly how a program drives the whole desktop, so macOS gates it behind **Accessibility**
//! (System Settings → Privacy & Security → Accessibility). Without the grant the function returns
//! and nothing moves, which is why [`SystemMouse::is_ready`] is asked before a gesture is spent —
//! the app can say why the sticks are doing nothing instead of looking broken. The prompt is asked
//! for by the first gesture, not by startup: a person who never uses a gamepad never sees it.
//!
//! The grant is tied to the signed application. An ad-hoc build (what `bundle.sh` makes) changes its
//! code identity when it is rebuilt, so macOS may ask again after a new bundle; a Developer ID
//! signature is what keeps one grant across releases.
//!
//! ## What the events are
//!
//! * a move is a `MouseMoved` at the current location plus the stick's distance — or a
//!   `LeftMouseDragged`/`RightMouseDragged` while a shoulder is held, so the shoulder can drag;
//! * a scroll is a pixel-unit wheel event, with a fractional accumulator because a stick at a
//!   quarter deflection moves less than a pixel per wake;
//! * a click is a down or an up at the current location, which is what makes a held shoulder a
//!   button held rather than a burst of clicks.

use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::CFString;
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use note_ui::platform::{AppFocus, MouseButton, SystemMouse};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSActivityOptions, NSObjectProtocol, NSProcessInfo, NSString};

// The Accessibility calls, from HIServices (ApplicationServices).
//
// `Boolean` is an `unsigned char` in C, so the return is read as `u8` rather than `bool`.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    // Whether this process may post synthetic events.
    fn AXIsProcessTrusted() -> u8;
    // The same question, with the option to show the system's permission prompt.
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
}

/// The app's own answer to "am I the one in front".
pub struct AppActive;

impl AppFocus for AppActive {
    fn is_active(&self) -> bool {
        // AppKit's answer belongs to the main thread, which is where the display pump runs. A call
        // from anywhere else is answered `true`: the in-app job is the harmless default, and posting
        // system events from a thread that cannot know is not.
        let Some(mtm) = MainThreadMarker::new() else {
            return true;
        };

        NSApplication::sharedApplication(mtm).isActive()
    }
}

/// The system pointer, as Core Graphics events.
struct SystemMouseService {
    /// Where events come from; `None` if Core Graphics would not give a source, which is a service
    /// that cannot post rather than a reason for the app not to start.
    source: Option<CGEventSource>,
    /// The fractional scroll the integer wheel has not been given yet.
    scroll: (f32, f32),
    /// The buttons this service is holding down.
    buttons: (bool, bool),
    /// Whether the Accessibility prompt has already been asked for in this process.
    requested: bool,
    /// When the trace last printed a pointer movement, so a held stick is one line a quarter second.
    last_log: Instant,
    /// The App Nap exemption, held for as long as the service exists.
    ///
    /// A background app is exactly what App Nap slows down, and a mouse that answers on App Nap's
    /// clock is not a mouse. `UserInitiatedAllowingIdleSystemSleep` keeps the housekeeping pump at
    /// its 1 ms while still letting the machine sleep.
    activity: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl SystemMouseService {
    /// Opens the service; the Accessibility prompt waits until a controller is actually used.
    fn install() -> Box<dyn SystemMouse> {
        let activity = Some(
            NSProcessInfo::processInfo().beginActivityWithOptions_reason(
                NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
                    | NSActivityOptions::LatencyCritical,
                &NSString::from_str("cheap-note is the gamepad's system pointer"),
            ),
        );

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            // SAFETY: a process-wide TCC read with no arguments.
            let trusted = unsafe { AXIsProcessTrusted() != 0 };
            eprintln!("system mouse: installed, Accessibility={trusted}");
        }

        Box::new(SystemMouseService {
            source: CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok(),
            scroll: (0.0, 0.0),
            buttons: (false, false),
            requested: false,
            last_log: Instant::now(),
            activity,
        })
    }

    /// Where the pointer is, in Core Graphics' global display coordinates.
    fn cursor(&self) -> Option<CGPoint> {
        let source = self.source.as_ref()?.clone();
        CGEvent::new(source).ok().map(|event| event.location())
    }

    /// A mouse event of one kind at one place, from this service's source.
    fn event(&self, kind: CGEventType, at: CGPoint, button: CGMouseButton) -> Option<CGEvent> {
        let source = self.source.as_ref()?.clone();
        CGEvent::new_mouse_event(source, kind, at, button).ok()
    }

    /// Posts an event into the HID stream; a failed event is nothing to post.
    fn post(event: Option<CGEvent>) {
        if let Some(event) = event {
            event.post(CGEventTapLocation::HID);
        }
    }
}

impl SystemMouse for SystemMouseService {
    fn is_ready(&self) -> bool {
        // SAFETY: the function reads a process-wide TCC state and has no arguments to get wrong.
        self.source.is_some() && unsafe { AXIsProcessTrusted() != 0 }
    }

    fn request(&mut self) {
        if self.requested {
            return;
        }

        self.requested = true;

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            eprintln!("system mouse: asking for Accessibility");
        }

        request_accessibility();
    }

    fn move_by(&mut self, distance: (f32, f32)) {
        if distance == (0.0, 0.0) {
            return;
        }

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            let now = Instant::now();
            if now.saturating_duration_since(self.last_log) >= Duration::from_millis(250) {
                eprintln!("system mouse: move ({:.1}, {:.1})", distance.0, distance.1);
                self.last_log = now;
            }
        }

        let Some(at) = self.cursor() else {
            return;
        };

        let to = CGPoint::new(at.x + f64::from(distance.0), at.y + f64::from(distance.1));

        // A held shoulder makes this a drag: a `MouseMoved` while the button is down would move the
        // pointer without dragging what it has hold of.
        let (kind, button) = if self.buttons.0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons.1 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };

        Self::post(self.event(kind, to, button));
    }

    fn scroll(&mut self, distance: (f32, f32)) {
        self.scroll.0 += distance.0;
        self.scroll.1 += distance.1;

        // `CGEventCreateScrollWheelEvent2` takes whole pixels, and a quarter-deflected stick moves
        // less than one a wake — the remainder waits here for the next one.
        let horizontal = self.scroll.0.trunc();
        let vertical = self.scroll.1.trunc();
        self.scroll = (self.scroll.0 - horizontal, self.scroll.1 - vertical);

        if horizontal == 0.0 && vertical == 0.0 {
            return;
        }

        let Some(source) = self.source.as_ref().cloned() else {
            return;
        };

        // The first wheel is vertical and the second horizontal; both are pixel units so the
        // distance the stick asked for is the distance the window scrolls.
        if let Ok(event) = CGEvent::new_scroll_event(
            source,
            ScrollEventUnit::PIXEL,
            2,
            vertical as i32,
            horizontal as i32,
            0,
        ) {
            event.post(CGEventTapLocation::HID);
        }
    }

    fn button(&mut self, button: MouseButton, pressed: bool) {
        let Some(at) = self.cursor() else {
            return;
        };

        let (kind, core_button) = match (button, pressed) {
            (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
        };

        Self::post(self.event(kind, at, core_button));

        match button {
            MouseButton::Left => self.buttons.0 = pressed,
            MouseButton::Right => self.buttons.1 = pressed,
        }
    }

    fn release_all(&mut self) {
        if self.buttons.0 {
            self.button(MouseButton::Left, false);
        }

        if self.buttons.1 {
            self.button(MouseButton::Right, false);
        }
    }
}

impl Drop for SystemMouseService {
    /// A controller held when the app closes must not leave the desktop mid-drag, and the App Nap
    /// exemption ends with the service that asked for it.
    fn drop(&mut self) {
        self.release_all();

        if let Some(activity) = self.activity.take() {
            // SAFETY: the token is the one `beginActivityWithOptions:reason:` returned, and it is
            // ended once.
            unsafe {
                NSProcessInfo::processInfo().endActivity(&activity);
            }
        }
    }
}

/// Asks macOS for Accessibility once, showing its prompt if this process does not have it.
///
/// The prompt is shown by the system, not by this app, and it is the only way to explain the grant
/// without sending a person into System Settings by hand. If it is declined, `is_ready` is false and
/// the app says so rather than posting events into nothing.
fn request_accessibility() {
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);

    // SAFETY: `options` is a dictionary of the two Core Foundation types the function documents,
    // and it outlives the call.
    unsafe {
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
    }
}

/// Installs the macOS system pointer service.
pub fn install() -> Option<Box<dyn SystemMouse>> {
    Some(SystemMouseService::install())
}
