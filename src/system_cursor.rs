//! Hiding the system pointer while the pen is in range.
//!
//! ## What GPUI cannot do, and why this exists
//!
//! GPUI can choose *which* pointer a window shows, but not whether it shows one: `CursorStyle` has
//! a variant for every arrow and none that hides it. The platform has the machinery — its window
//! procedure answers `WM_SETCURSOR` with a null cursor when its own `cursor_visible` flag is clear
//! — but the flag is private and the only way in is `Platform::hide_cursor_until_mouse_moves`,
//! which the app cannot reach and whose restore condition is a *mouse* message. Pen input
//! synthesises mouse messages (which is why `pen-windows` chains them), so that flag would clear
//! itself in the middle of a stroke.
//!
//! ## The Windows answer
//!
//! `WM_SETCURSOR` is how Windows asks a window what the pointer should look like. Handling it is
//! the documented way to hide the pointer over a client area: set a null cursor and return `TRUE`,
//! which says the question has been answered and stops the chain.
//!
//! [`ShowCursor`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-showcursor)
//! is deliberately *not* used. It maintains a display counter that `SetCursor` ignores, so the
//! platform's next `SetCursor` would bring the pointer back; and it is desktop state rather than
//! window state, so a process that died with the counter negative would leave the whole desktop
//! without a pointer. `SetCursor(NULL)` is transient state owned by the window, and it ends with
//! the window.
//!
//! ## Why a subclass, and why it is safe here
//!
//! The window procedure belongs to GPUI, and its `WM_SETCURSOR` handler would put the cursor back.
//! `SetWindowSubclass` installs a procedure in front of it that answers first — the same comctl32
//! chain `pen-windows` already hooks on this very window for `WM_POINTER`, so two hooks compose by
//! design rather than by luck. The most recently installed procedure runs first, which is this one,
//! because it is installed after the capture.
//!
//! ## What it does not do
//!
//! It does not render anything: the pen's ghost cursor is what replaces the pointer, and it lives
//! in [`crate::cursor`]. This module only takes the platform's pointer out of the way.

use std::sync::atomic::{AtomicBool, Ordering};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    LoadCursorW, SetCursor, HCURSOR, HTCLIENT, IDC_ARROW, WM_SETCURSOR,
};

/// This hook's identifier on the window.
///
/// The subclass chain is keyed by `(procedure, id)`, so the id only has to be unique among the
/// hooks *this* procedure installs — but a value that spells something is easier to recognise in a
/// debugger than an index would be.
const SUBCLASS_ID: usize = 0x6368_6561;

/// Whether a `WM_SETCURSOR` is asking about this window's **client area**.
///
/// The hit-test code in the low word of `lParam` is what tells the client area from the frame: over
/// the frame the pointer has to stay, because a resize border with no resize arrow is a border
/// nobody can find.
fn is_client_area(window: HWND, wparam: WPARAM, lparam: LPARAM) -> bool {
    wparam.0 == window.0 as usize && (lparam.0 as u32 & 0xFFFF) == HTCLIENT
}

/// The window procedure that answers `WM_SETCURSOR` while the pointer is meant to be hidden.
///
/// # Safety
///
/// Called by Windows on the thread that owns the window. `reference` is the address of the
/// [`SystemCursor::hidden`] flag this hook was installed with, and that flag outlives the hook:
/// [`SystemCursor::drop`] removes the hook before the flag is freed.
unsafe extern "system" fn pointer_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    reference: usize,
) -> LRESULT {
    // SAFETY: see this function's own contract. The address came from `Box<AtomicBool>`, which is
    // shared with nothing else and never moved, and `&AtomicBool` is safe to share across threads.
    let hidden = unsafe { &*(reference as *const AtomicBool) };

    if message == WM_SETCURSOR
        && hidden.load(Ordering::Relaxed)
        && is_client_area(window, wparam, lparam)
    {
        // A null cursor draws nothing, and `TRUE` says the question was answered — so the chain
        // stops here and GPUI's own handler never gets to put a cursor back.
        unsafe { SetCursor(None) };
        return LRESULT(1);
    }

    // Everything else belongs to the window's real procedure, exactly as `pen-windows` chains the
    // messages it only observes.
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

/// The pointer GPUI expects a window to have when nothing special is wanted.
///
/// Not the app's current cursor style, because there is no way to ask for it — and there does not
/// need to be: the next mouse movement asks the window again, down the very chain this hook sits
/// in.
fn arrow() -> Option<HCURSOR> {
    unsafe { LoadCursorW(None, IDC_ARROW).ok() }
}

/// Keeps the system pointer hidden for as long as the pen is drawing its own cursor.
///
/// Installing one is optional and failing to install one is not an error: without it the window
/// behaves exactly as it did before, with two cursors instead of one.
pub struct SystemCursor {
    /// The window whose procedure was subclassed, so the hook can be removed.
    window: HWND,
    /// What the window procedure reads. Boxed for an address that does not move, and freed only
    /// after the hook that points at it is gone.
    hidden: Box<AtomicBool>,
    /// Whether the pointer is hidden right now, so that the platform is called on transitions
    /// rather than on every frame.
    is_hidden: bool,
}

impl SystemCursor {
    /// Installs the hook on a window, or returns `None` if it cannot be installed.
    ///
    /// A handle that is not a Win32 one, or a subclass the system refuses, both mean "no hook":
    /// the app draws the pen's ghost cursor either way.
    pub fn install<W: HasWindowHandle>(window: &W) -> Option<Self> {
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return None;
        };

        let window = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
        let hidden = Box::new(AtomicBool::new(false));

        // The address of the boxed flag, which stays put because the box does.
        let reference = (&*hidden as *const AtomicBool) as usize;
        let installed =
            unsafe { SetWindowSubclass(window, Some(pointer_proc), SUBCLASS_ID, reference) };

        installed.as_bool().then_some(SystemCursor {
            window,
            hidden,
            is_hidden: false,
        })
    }

    /// Follows the pen: hides the pointer while the pen has a cursor of its own, and gives the
    /// pointer back when it does not.
    ///
    /// Driven by the ghost cursor rather than by the pen's range on purpose. Hiding the pointer
    /// with nothing drawn in its place would leave the user with no cursor at all, and tying the
    /// two to one switch means the Tilt toggle cannot produce that state.
    ///
    /// The change takes effect immediately rather than waiting for the next `WM_SETCURSOR`, because
    /// Windows only asks when the pointer moves and a pen can enter range without moving anything.
    pub fn follow(&mut self, pen_has_its_own_cursor: bool) {
        if pen_has_its_own_cursor == self.is_hidden {
            return;
        }

        self.is_hidden = pen_has_its_own_cursor;
        self.hidden.store(pen_has_its_own_cursor, Ordering::Relaxed);

        unsafe {
            SetCursor(if pen_has_its_own_cursor { None } else { arrow() });
        }
    }
}

impl Drop for SystemCursor {
    /// Removes the hook, then gives the pointer back.
    ///
    /// Both orders matter. The hook goes before the flag it reads is freed, or the next
    /// `WM_SETCURSOR` would read freed memory; and the pointer is restored here rather than left to
    /// the next `WM_SETCURSOR`, because a window that closes while it was hidden would take the
    /// last chance to restore it with it.
    fn drop(&mut self) {
        unsafe {
            // The result says whether the hook was found. There is nothing to do about a `false`
            // here — this is `Drop`, and the window is going away — but the call is what keeps the
            // chain from pointing at a flag that is about to be freed.
            let _ = RemoveWindowSubclass(self.window, Some(pointer_proc), SUBCLASS_ID);

            if self.is_hidden {
                SetCursor(arrow());
            }
        }
    }
}
