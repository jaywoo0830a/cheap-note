//! Hiding the system pointer while the pen has a cursor of its own.
//!
//! The view drives this: it asks for `true` exactly while a ghost is over the sheet, and the ghost is not
//! written yet (see `platform::install`), so today this is installed and told `false` on every frame — the
//! window keeps its arrow, which is a cursor.
//!
//! `NSCursor.hide()` and `unhide()` are a **counter**, not a flag: every `hide` must be paired with an
//! `unhide`, and a process that ends with the counter still positive leaves the desktop without a pointer.
//! So this keeps the state the counter should be in, moves it only when the answer changes, and gives the
//! pointer back when it is dropped.

use note_ui::platform::SystemPointer;
use objc2_app_kit::NSCursor;

/// The system pointer, hidden exactly while the pen has a cursor of its own.
pub struct SystemCursor {
    /// Whether this type has hidden the pointer and owes the system an `unhide`.
    hidden: bool,
}

impl SystemCursor {
    /// Installs the pointer hook. Nothing can fail: an AppKit class method always exists.
    pub fn install() -> Option<Self> {
        Some(SystemCursor { hidden: false })
    }
}

impl SystemPointer for SystemCursor {
    fn follow(&mut self, pen_has_its_own_cursor: bool) {
        // Only a change moves the counter: AppKit counts, and a `hide` per frame would take a hundred
        // `unhide`s to undo.
        if pen_has_its_own_cursor == self.hidden {
            return;
        }

        if pen_has_its_own_cursor {
            NSCursor::hide();
        } else {
            NSCursor::unhide();
        }

        self.hidden = pen_has_its_own_cursor;
    }
}

impl Drop for SystemCursor {
    /// Gives the pointer back if it was taken, so a window that closes mid-stroke does not take the
    /// desktop's cursor with it.
    fn drop(&mut self) {
        if self.hidden {
            NSCursor::unhide();
            self.hidden = false;
        }
    }
}
