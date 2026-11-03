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
//! ## Why a cursor that is only *set* is not enough
//!
//! `NSCursor.set` makes the receiver the current cursor, and AppKit replaces it the next time it applies a
//! **cursor rect**: a view's answer to "what should the pointer be here", installed with
//! `addCursorRect:cursor:` and applied on a `cursorUpdate:` event. GPUI installs one over its whole view
//! (`resetCursorRects` in `gpui-pre-macos`) and invalidates it whenever its own cursor style changes, so a
//! `set` is only the cursor until the next update — which is how an arrow (or a pointing hand) comes back
//! over a window whose ghost says the pointer is the pen's.
//!
//! The answer is the one Windows gives its window's `WM_SETCURSOR` (see
//! `apps/windows/src/platform/system_cursor.rs`): answer the platform's own question. A **local monitor**
//! for `NSEventTypeCursorUpdate` sees the update before the view does and, while the pen has the pointer,
//! sets the transparent cursor and swallows the update — so the rect is not applied for it. That is what
//! makes the seam's `true` a *state* rather than one call: the monitor is what keeps it true between the
//! view's calls.
//!
//! The cursor is also set again per reading, because AppKit puts the arrow back whenever the interface
//! handles an event and the pen's readings are the moments that count — a `set` on a cached cursor, which
//! is not the `resetCursorRects` path the strategy rejects.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use note_ui::platform::SystemPointer;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::{NSCursor, NSEvent, NSEventMask, NSImage};
use objc2_foundation::{NSPoint, NSSize};

/// The system pointer, invisible exactly while the pen has a mark of its own.
pub struct SystemCursor {
    /// A cursor that draws nothing: what the arrow is replaced by.
    transparent: Option<Retained<NSCursor>>,
    /// Whether the pointer is ours to give back.
    ///
    /// Shared with the cursor-update monitor, which runs while this is set and is the one place AppKit's
    /// own answer can be pre-empted (see the module docs).
    hidden: Arc<AtomicBool>,
    /// The object `addLocalMonitorForEventsMatchingMask:handler:` returned, removed in [`Drop`].
    monitor: Option<Retained<AnyObject>>,
}

impl SystemCursor {
    /// Installs the pointer, and the monitor that answers AppKit's cursor updates for it.
    pub fn install() -> Option<Self> {
        let transparent = transparent();
        let hidden = Arc::new(AtomicBool::new(false));
        let monitor = watch_cursor_updates(Arc::clone(&hidden), transparent.clone());

        Some(SystemCursor {
            transparent,
            hidden,
            monitor,
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

/// Answers AppKit's cursor updates with the transparent cursor while the pen has the pointer.
///
/// This is the macOS half of what `WM_SETCURSOR` is on Windows: not a `set` that hopes to be last, but an
/// answer to the question the platform is about to ask. A local monitor runs before the event reaches the
/// view, on this application's own events and on the main thread — the thread every one of these calls has
/// to be on — so the answer is given first and the cursor rect is left unapplied.
///
/// The mask is `CursorUpdate` alone, so every event this sees *is* that question: while the pen has the
/// pointer it is answered with nothing, and when the pen has given it back the event goes through as it
/// came. `None` means AppKit refused a monitor, and the pointer then falls back to the per-reading `set`
/// alone.
fn watch_cursor_updates(
    hidden: Arc<AtomicBool>,
    transparent: Option<Retained<NSCursor>>,
) -> Option<Retained<AnyObject>> {
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        if hidden.load(Ordering::Relaxed) {
            if let Some(cursor) = &transparent {
                cursor.set();
            }

            // Returning null from a local monitor is how an event is consumed: the cursor rect is not
            // applied for this update, so the arrow — or a button's pointing hand — cannot come back
            // over a window whose ghost says the pointer is the pen's.
            return std::ptr::null_mut();
        }

        // Not ours: handed back as it came, so AppKit applies the rect as it would have.
        event.as_ptr()
    });

    // SAFETY: the block is `'static` (it owns an `Arc` and a retained cursor), and AppKit copies it.
    unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::CursorUpdate, &block)
    }
}

impl SystemPointer for SystemCursor {
    fn follow(&mut self, pen_has_its_own_cursor: bool) {
        if pen_has_its_own_cursor {
            if let Some(transparent) = &self.transparent {
                transparent.set();
            }

            if !self.hidden.swap(true, Ordering::Relaxed)
                && std::env::var_os("CHEAP_NOTE_TRACE").is_some()
            {
                eprintln!("pointer: the pen has its own cursor — arrow replaced");
            }

            return;
        }

        if !self.hidden.swap(false, Ordering::Relaxed) {
            return;
        }

        NSCursor::arrowCursor().set();

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            eprintln!("pointer: the arrow is the cursor again");
        }
    }
}

impl Drop for SystemCursor {
    /// Removes the monitor, then gives the pointer back if it was taken.
    ///
    /// Both orders matter: the monitor reads `hidden`, so it goes before the flag is handed back, and the
    /// arrow is restored here rather than left to a cursor update that may never come — a window that
    /// closes mid-stroke must not leave the desktop with an invisible arrow.
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            // SAFETY: this is the object the monitor registration returned, and it is still installed.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }

        if self.hidden.swap(false, Ordering::Relaxed) {
            NSCursor::arrowCursor().set();
        }
    }
}
