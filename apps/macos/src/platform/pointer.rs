//! The system pointer, got out of the way.
//!
//! The strategy (`cursor.md`): never ask AppKit to draw a pen — take the arrow away with a **transparent
//! cursor** and let the app draw the pen itself. What is left here is the taking-away.
//!
//! ## What this does not do, and why
//!
//! `NSCursor.hide()` is not used at all. It is a counter — a process that ends with it positive leaves the
//! desktop without a pointer — and it takes the hardware cursor plane out of play, which is the path that
//! costs a frame while the cursor changes. A transparent cursor keeps the plane active and drawing nothing,
//! which is the whole of what "hidden" is supposed to mean.
//!
//! The cursor is made once. It is set again per reading, because AppKit puts the arrow back whenever the
//! interface handles an event and the pen's readings are the moments that count — a `set` on a cached cursor,
//! which is not the `resetCursorRects` path the strategy rejects.

use note_ui::platform::SystemPointer;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_app_kit::{NSCursor, NSImage};
use objc2_foundation::{NSPoint, NSSize};

/// The system pointer, invisible exactly while the pen has a mark of its own.
pub struct SystemCursor {
    /// A cursor that draws nothing: what the arrow is replaced by.
    transparent: Option<Retained<NSCursor>>,
    /// Whether the arrow is currently ours to give back.
    hidden: bool,
}

impl SystemCursor {
    /// Installs the pointer. Nothing can fail: an AppKit class method always exists.
    pub fn install() -> Option<Self> {
        Some(SystemCursor {
            transparent: transparent(),
            hidden: false,
        })
    }
}

/// A 1×1 image with nothing in it, as a cursor.
///
/// One pixel and not none: a cursor needs a size to be a cursor, and the strategy's whole point is that the
/// arrow is replaced by something *valid* that draws nothing — not removed, which is what costs a frame.
fn transparent() -> Option<Retained<NSCursor>> {
    let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(1.0, 1.0));

    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &image,
        NSPoint::ZERO,
    ))
}

impl SystemPointer for SystemCursor {
    fn follow(&mut self, pen_has_its_own_cursor: bool) {
        if pen_has_its_own_cursor {
            if let Some(transparent) = &self.transparent {
                transparent.set();
            }

            if !self.hidden {
                self.hidden = true;

                if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
                    eprintln!("pointer: the pen has its own cursor — arrow replaced");
                }
            }

            return;
        }

        if !self.hidden {
            return;
        }

        self.hidden = false;
        NSCursor::arrowCursor().set();

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            eprintln!("pointer: the arrow is the cursor again");
        }
    }
}

impl Drop for SystemCursor {
    /// Gives the pointer back if it was taken, so a window that closes mid-stroke does not leave the
    /// desktop with an invisible arrow.
    fn drop(&mut self) {
        if self.hidden {
            self.hidden = false;
            NSCursor::arrowCursor().set();
        }
    }
}
