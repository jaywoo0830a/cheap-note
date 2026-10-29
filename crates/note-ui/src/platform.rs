//! The seam: the four things the view needs from whatever machine it is running on.
//!
//! ## What is on each side
//!
//! Everything this crate does is written once — the bar, the home screen, the sheet, the gestures, the
//! page cache, the ink model, the frame loop. What it cannot write once is the operating system, and
//! there is very little of that which the view actually needs:
//!
//! | Trait | What the view asks | What Windows answers with |
//! |---|---|---|
//! | [`PenSource`] | what the pen has read, and how well | `pen-windows`, on a thread of its own |
//! | [`GhostFeed`] | draw the nib's mark at the pen's angle, now | a layered window above everything |
//! | [`SystemPointer`] | get the system pointer out of the way, or back | `SetCursor` in a subclass |
//! | [`InkSurface`] | paint the desk, the paper and the ink | a Direct2D surface under GPUI's |
//!
//! A platform is one implementation of each, assembled by [`Platform`] and handed to
//! [`crate::run`]. That is the whole of what a second platform has to write: no element tree, no
//! gesture, no page, no stroke.
//!
//! ## Why the traits are declared here rather than in a crate of their own
//!
//! Because two of them name types this crate owns — [`InkSurface`] is handed a
//! [`Canvas`](crate::ink_layer::Canvas), which is what the view *built*, and [`GhostFeed`] is offered
//! [`Reading`](crate::input::Reading)s, which is what the view's model reads. A separate trait crate
//! would have to depend on this one to name them, which is the same dependency with an extra step.
//! What matters is the direction, and it is one way: a platform crate depends on the view, and the
//! view never depends on a platform.
//!
//! ## What a platform may not assume
//!
//! **That it draws behind the window.** Windows can compose a second renderer under GPUI's surface,
//! which is why the canvas here is fast; macOS has no equivalent, so a macOS canvas will hand GPUI a
//! texture instead. The view does not care — it says what to draw — and that difference is exactly
//! what the seam is for.

use std::sync::Arc;

use gpui_kit::{App, Window};

use crate::ink_layer::Canvas;
use crate::input::{PenInbox, PenStats, Reading};

/// What the frame publishes about the screen the ghost is drawn on: everything a reading cannot say.
///
/// These five numbers are the whole of what the view knows and the overlay does not. `suppressed`
/// means "there is nothing to draw a ghost for here" — the home list is up, or the app is asking a
/// question the pen must not write under — and it is published rather than inferred because only the
/// frame knows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    /// Physical pixels per logical pixel, as the frame laid the window out.
    pub scale: f32,
    /// The colour the ghost's mark is drawn in, as `0xRRGGBB`.
    pub colour: u32,
    /// The colour of the soft edge under it, and of the nib's bloom.
    ///
    /// The frame's own answer for the paper in front: the ghost's mark is the *ink's* colour, which
    /// can be the colour of the paper it is drawn on, and this is what keeps a white pen or a
    /// highlighter's pale yellow findable.
    pub halo: u32,
    /// How wide the ghost's nib mark is, in logical pixels: half the width of the line in hand.
    ///
    /// Published for the same reason the colour is — the view is the only thing that knows the pen's
    /// weight — and it is what makes the mark a preview of the ink rather than a point.
    pub nib_radius: f32,
    /// The line the ghost is drawn below, in physical client pixels: the bar's bottom edge.
    pub sheet_top: f32,
    /// Whether there is nothing to draw a ghost for on this screen.
    pub suppressed: bool,
}

impl Default for Screen {
    /// The state before the first frame has said anything: no ghost, which is the honest answer for a
    /// screen nobody has described yet. The mark is the smallest one, because nothing has said what is
    /// in hand — and it is not drawn at all while `suppressed` is set, which is what this is.
    fn default() -> Self {
        Screen {
            scale: 1.0,
            colour: 0x00_00_00,
            halo: 0x00_00_00,
            nib_radius: crate::cursor::NIB_RADIUS_MIN,
            sheet_top: 0.0,
            suppressed: true,
        }
    }
}

/// The pen's mark at the nib: drawn as fast as the pen reports, and in a window of its own.
///
/// The one part of the view that does **not** wait for a frame. A ghost that arrived with the next
/// painted frame would lag the nib by up to a frame, which is the difference between a cursor and a
/// trailer; so the pen thread offers every batch straight to this, and the frame only says what the
/// screen looks like ([`Screen`]).
pub trait GhostFeed: Send + Sync {
    /// Offers the newest batch, from the pen thread.
    fn offer(&self, readings: &[Reading]);
    /// Publishes what the frame knows about the screen: its scale, its colours, the bar's edge.
    fn set_screen(&self, screen: Screen);
    /// Whether the ghost's own window is still there to draw into.
    fn is_alive(&self) -> bool;
}

/// Hides the system pointer while the pen has a cursor of its own, and gives it back when it does not.
///
/// Driven by the ghost rather than by the pen's range on purpose: hiding the pointer with nothing
/// drawn in its place would leave the window with no cursor at all.
pub trait SystemPointer {
    /// Follows the pen: `true` while a ghost is on screen, `false` when the pointer is the cursor again.
    fn follow(&mut self, pen_has_its_own_cursor: bool);
}

/// The surface the desk, the paper and the ink are painted on: the view says what, this says how.
pub trait InkSurface {
    /// Paints one frame of the canvas.
    ///
    /// The error is a sentence rather than a type: it is shown in the status line, once, and the app
    /// keeps running without a canvas rather than failing to start.
    fn draw(&mut self, canvas: &Canvas) -> Result<(), String>;
}

/// Where the pen's readings come from.
pub trait PenSource {
    /// The queue the view parks on and drains.
    fn inbox(&self) -> Arc<PenInbox>;
    /// The line the status bar shows about the capture.
    fn status(&self) -> &str;
    /// What the capture can report about the digitizer, or `None` when there is no capture.
    fn stats(&self) -> Option<PenStats>;
}

/// Everything the platform gave the view, in one place: what the window is built with.
///
/// Assembled by an [`Installer`], held by the app, and dropped with it — which is what stops the pen
/// thread and closes the ghost's window.
pub struct Platform {
    /// The canvas the ink is drawn on, when one could be installed.
    ///
    /// `None` is a build that has no canvas of its own: the window then draws the sheet itself, which
    /// is slower and correct, rather than the app failing to start.
    pub ink: Option<Box<dyn InkSurface>>,
    /// The pen's ghost cursor, when one could be installed.
    pub ghost: Option<Arc<dyn GhostFeed>>,
    /// The pointer hook, when one could be installed.
    pub pointer: Option<Box<dyn SystemPointer>>,
    /// The pen. Always there, because a pen that cannot be read is a pen that draws with the mouse —
    /// a service that says so in its status rather than an absence to handle everywhere.
    pub pen: Box<dyn PenSource>,
}

/// The platform's two halves: what has to happen before a window exists, and what has to happen once
/// it does.
///
/// Two functions rather than one, because the canvas registers a window plugin and a window's plugins
/// are read when it is created: there is no window to install anything on yet, and the surface has to
/// be told to leave its background unpainted before there is one to paint.
#[derive(Clone, Copy)]
pub struct Installer {
    /// Called once, on the application, before the window is opened.
    pub before_window: fn(&mut App),
    /// Called once, on the window's own thread, when the view is built.
    pub install: fn(&mut Window) -> Platform,
}
