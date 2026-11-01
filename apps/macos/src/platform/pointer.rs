//! Hiding the system pointer while the pen has a cursor of its own.
//!
//! The view drives this: it asks for `true` exactly while a ghost is over the sheet, and the ghost is the
//! nib's mark the canvas paints (see [`crate::platform::ghost`]), so the two cannot disagree.
//!
//! ## Why an invisible cursor, and not a hidden one
//!
//! `NSCursor.hide()` is a counter, and a process that ends with it positive leaves the desktop without a
//! pointer. It also loses a race: the interface sets the arrow again whenever the pointer moves, so a
//! single `hide` is undone by the next event the pen generates. What is done instead is the ordinary
//! drawing-app trick — the arrow is *replaced* by a cursor with nothing in it, on **every reading**, which
//! is the pen's own rate and faster than the arrow can come back. When the pen leaves the sheet the arrow
//! is put back once.
//!
//! The counter is still used, but only as a fallback for the case where the empty cursor cannot be made.

use note_ui::platform::SystemPointer;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_app_kit::{NSCursor, NSImage};
use objc2_foundation::{NSPoint, NSSize};

/// The system pointer, invisible exactly while the pen has a mark of its own.
pub struct SystemCursor {
    /// A cursor that draws nothing: what the arrow is replaced by.
    invisible: Option<Retained<NSCursor>>,
    /// How many hides this type has taken that it owes an unhide for.
    hides: usize,
}

impl SystemCursor {
    /// Installs the pointer hook. Nothing can fail: an AppKit class method always exists.
    pub fn install() -> Option<Self> {
        // An image with nothing in it, and a cursor made from it: where the arrow was, nothing is drawn.
        let image = NSImage::initWithSize(NSImage::alloc(), NSSize::ZERO);
        let invisible = NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, NSPoint::ZERO);

        Some(SystemCursor {
            invisible: Some(invisible),
            hides: 0,
        })
    }
}

impl SystemPointer for SystemCursor {
    fn follow(&mut self, pen_has_its_own_cursor: bool) {
        if pen_has_its_own_cursor {
            if let Some(invisible) = &self.invisible {
                invisible.set();
            }

            // And the counter too. AppKit counts hides against unhides, and the interface puts the arrow
            // back on the next event it handles: one `hide` is a snapshot, a `hide` per reading is a rule.
            // The count is kept so the pointer can be given back exactly, rather than approximately.
            NSCursor::hide();
            let first = self.hides == 0;
            self.hides += 1;

            if first && std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
                eprintln!("pointer: the pen has its own cursor — arrow hidden");
            }

            return;
        }

        if self.hides == 0 {
            return;
        }

        for _ in 0..self.hides {
            NSCursor::unhide();
        }

        self.hides = 0;
        NSCursor::arrowCursor().set();

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            eprintln!("pointer: the arrow is the cursor again");
        }
    }
}

impl Drop for SystemCursor {
    /// Gives the pointer back if it was taken, so a window that closes mid-stroke does not leave the
    /// desktop without one.
    fn drop(&mut self) {
        for _ in 0..self.hides {
            NSCursor::unhide();
        }

        self.hides = 0;
    }
}
