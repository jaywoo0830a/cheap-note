//! The services the view asks for, on Windows, and the assembly of them.
//!
//! [`INSTALLER`] is the whole of what this crate hands the view: two functions — one that runs before
//! the window exists, one that runs when it does — and what comes back is a [`Platform`]: a pen to
//! read, a ghost cursor to follow the nib, a pointer hook, and a surface to paint the desk and the ink
//! on.
//!
//! The order is the one the view's constructor used to do inline, and it still matters:
//!
//! 1. **The ghost's window first**, because the pen thread is handed the tap that feeds it, and that
//!    tap has to exist when the thread starts;
//! 2. **then the capture**, on this thread, because attaching to a window is a call the window's own
//!    thread has to make;
//! 3. **then the pointer hook**, installed *after* the capture so that this hook runs first in the
//!    subclass chain and gets to answer `WM_SETCURSOR` before anything else puts a cursor back;
//! 4. **then the canvas**, which the app requires: a machine that cannot give Direct3D 11 a hardware
//!    device cannot run this app at all, and saying so once, at startup, with the reason, is the whole
//!    of the report.

pub mod bench;
pub mod cursor_overlay;
pub mod ink_layer;
pub mod input;
pub mod pen;
pub mod system_cursor;
pub mod vitals;

use std::sync::Arc;

use gpui_kit::Window;
use note_ui::input::{BatchTap, Reading};
use note_ui::platform::{GhostFeed, Installer, Platform};

use crate::platform::cursor_overlay::CursorFeed;
use crate::platform::ink_layer::InkLayer;
use crate::platform::pen::PenService;
use crate::platform::system_cursor::SystemCursor;
use crate::platform::vitals::ProcessStats;

/// What this platform hands the view.
pub static INSTALLER: Installer = Installer {
    before_window: ink_layer::leave_the_surface_unpainted,
    install,
};

/// Installs the services on a window that has just been made.
fn install(window: &mut Window) -> Platform {
    // The ghost cursor's window, installed before the capture: the pen thread is handed the tap that
    // feeds it, and that tap has to exist when the thread starts. Failing to install one is not an
    // error — the app keeps the system pointer, and the pen has no ghost of its own.
    //
    // The feed is made into the seam's trait object here rather than later, because the tap the pen
    // thread is given holds exactly that: one `Arc<dyn GhostFeed>` and nothing about windows.
    let ghost = CursorFeed::install(window).map(|feed| Arc::new(feed) as Arc<dyn GhostFeed>);

    // The capture must be attached on the thread that owns the window, which is this one.
    let pen = PenService::attach(
        window,
        pen::capture_config(),
        ghost.as_ref().map(cursor_tap),
    );

    // Installed after the capture, so this hook runs first in the subclass chain.
    let pointer = SystemCursor::install(window).map(|cursor| Box::new(cursor) as Box<_>);

    let ink = Some(Box::new(InkLayer::install(window).expect("a canvas for the window")) as Box<_>);

    Platform {
        ink,
        ghost,
        pointer,
        // The design's numbers are this platform's own: a Windows logical pixel is a 96-DPI one.
        unit_scale: 1.0,
        // A desktop Windows machine scrolls with a wheel, and a wheel reports lines. Reading them
        // is what keeps a plugged-in mouse from being unable to move the sheet at all.
        wheel_pans: true,
        pen: Box::new(pen),
        // What the note costs this machine. The counters are the process's own, so no permission and no
        // failure mode beyond a system that will not describe it.
        stats: ProcessStats::install().map(|stats| Box::new(stats) as Box<_>),
    }
}

/// The pen thread's tap on the readings: every batch goes straight to the ghost cursor's window.
fn cursor_tap(feed: &Arc<dyn GhostFeed>) -> BatchTap {
    let feed = Arc::clone(feed);

    Arc::new(move |readings: &[Reading]| feed.offer(readings))
}
