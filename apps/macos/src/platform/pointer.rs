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
//! for `NSEventTypeCursorUpdate` sees the update before the view does and, while the ghost is up, sets the
//! transparent cursor and swallows the update — so the rect is not applied for it. A move, a click or a
//! key is handed back untouched; the cursor update is the only event eaten, and it carries nothing but the
//! cursor question.
//!
//! The cursor is also set again per reading, because AppKit puts the arrow back whenever the interface
//! handles an event and the pen's readings are the moments that count — a `set` on a cached cursor, which
//! is not the `resetCursorRects` path the strategy rejects.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use note_ui::platform::SystemPointer;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::{NSCursor, NSEvent, NSEventMask, NSEventType, NSImage};
use objc2_foundation::{NSPoint, NSSize};

/// The system pointer, invisible exactly while the pen has a mark of its own.
pub struct SystemCursor {
    /// A cursor that draws nothing: what the arrow is replaced by.
    transparent: Option<Retained<NSCursor>>,
    /// Whether the arrow is currently ours to give back.
    ///
    /// Shared with the cursor-update monitor, which runs while this is set and is the one place AppKit's
    /// own answer can be pre-empted (see the module docs).
    hidden: Arc<AtomicBool>,
    /// The object `addLocalMonitorForEventsMatchingMask:handler:` returned, as a raw pointer.
    monitor: Option<usize>,
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

/// Answers AppKit's cursor updates with the transparent cursor while the ghost is up.
///
/// This is the macOS half of what `WM_SETCURSOR` is on Windows: not a `set` that hopes to be last, but an
/// answer to the question the platform is about to ask. A local monitor runs before the event reaches the
/// view, on this application's own events and on the main thread — the thread every one of these calls has
/// to be on — so the answer is given first and the cursor rect is left unapplied.
///
/// Returns the monitor's registration, which [`Drop`] gives back; `None` means AppKit refused one, and the
/// pointer then falls back to the per-reading `set` alone.
fn watch_cursor_updates(
    hidden: Arc<AtomicBool>,
    transparent: Option<Retained<NSCursor>>,
) -> Option<usize> {
    let block = RcBlock::new(move |event: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
        if hidden.load(Ordering::Relaxed) {
            if let Some(cursor) = &transparent {
                cursor.set();
            }

            // SAFETY: AppKit passes a live event for the duration of the call.
            if unsafe { event.as_ref() }.r#type() == NSEventType::CursorUpdate {
                // A cursor update carries nothing but the rect's answer. Swallowing it is what keeps the
                // arrow off for this one; returning null from a local monitor is how an event is consumed.
                return std::ptr::null_mut();
            }
        }

        // Everything else is handed back untouched: the application still needs its moves and clicks.
        event.as_ptr()
    });

    // SAFETY: the block is `'static` (it owns an `Arc` and a retained cursor), and AppKit copies it.
    let token = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::CursorUpdate, &block)
    }?;

    // SAFETY: `into_raw` hands the retain over to us, and `Drop` is what gives it back.
    Some(Retained::into_raw(token) as usize)
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
    /// Gives the pointer back if it was taken, so a window that closes mid-stroke does not leave the
    /// desktop with an invisible arrow, and removes the monitor that answers for it.
    fn drop(&mut self) {
        if let Some(raw) = self.monitor.take() {
            // SAFETY: this pointer came from `Retained::into_raw` at install and has never been released.
            if let Some(token) = unsafe { Retained::<AnyObject>::from_raw(raw as *mut AnyObject) } {
                // SAFETY: it is the object the monitor registration returned.
                unsafe { NSEvent::removeMonitor(&token) };
            }
        }

        if self.hidden.swap(false, Ordering::Relaxed) {
            NSCursor::arrowCursor().set();
        }
    }
}
