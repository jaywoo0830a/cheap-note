//! The seam: what the view needs from whatever machine it is running on.
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
//! | [`SystemStats`] | what this process is costing the machine | the process's own counters |
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

pub use crate::timing::Vitals;

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
    /// The line the ghost is drawn below, in physical client pixels: the bar's own bottom edge, as the
    /// view measured it (`NoteApp::bar_edge`).
    ///
    /// It is the same line the ink is refused above and the system pointer is the cursor on — one line,
    /// three consumers. A ghost drawn below a different line than the ink obeys is a cursor pointing at
    /// a page the pen cannot write on, or an arrow over a place the ghost says is the pen's.
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
    ///
    /// `true` is a **state, not an event**: while it holds, the system pointer has to stay out of the
    /// way until the next `false`. The view calls this on every pen reading and whenever the line the
    /// ghost is drawn below moves (a bar shown or hidden, a window resized), and it does not call again
    /// in between — so a platform whose pointer can be put back by the system itself has to answer
    /// that on its own. Windows answers its window's `WM_SETCURSOR` for as long as the flag is set;
    /// macOS watches `NSEventTypeCursorUpdate`, because AppKit re-applies the cursor rect under the
    /// pointer and a `set` that hopes to be last loses to it. A plain `set` is not enough on either.
    fn follow(&mut self, pen_has_its_own_cursor: bool);
}

/// The machine, as far as this process can see it.
///
/// The fourth thing a platform knows and the view cannot: how much memory and processor the app is
/// actually using. Everything about what those numbers *mean* stays in the view (see
/// [`Vitals`](crate::timing::Vitals)), so this trait is deliberately as small as it can be — one call,
/// one optional answer, no configuration.
///
/// ## Why it is a trait rather than a crate the view depends on
///
/// Because gathering these figures is an operating-system job and `note-ui` may not name one
/// (`doc/MACOS.md` §6). A crate that wraps both platforms would still have to be listed in *this*
/// manifest, which is the thing the rule forbids: the view would be carrying an OS dependency even
/// though it never calls it directly. So the platform answers the question and the view decides what to
/// do with the answer.
///
/// ## Cost, and why the caller must not call it often
///
/// On both platforms this reads the process's own counters, which is cheap but not free — measured at
/// tens to a couple of hundred microseconds depending on the platform's bookkeeping. It is therefore
/// sampled on the once-a-second status refresh (`STATUS_REFRESH_INTERVAL`) and never per frame, and the
/// app does not ask at all while the status line is hidden.
pub trait SystemStats {
    /// What this process is using, or `None` when the platform cannot say.
    ///
    /// `None` is *nothing to report* — a sandbox, a platform without the counters, a process the system
    /// will not describe — rather than an error. A status line with no hardware clause is the correct
    /// response, and it is what a build with no implementation of this trait gets automatically.
    fn vitals(&mut self) -> Option<Vitals>;

    /// How much memory this process has lent the GPU, when the platform can say.
    ///
    /// The one part of a process's footprint that [`Self::vitals`] cannot see: a page bitmap handed to
    /// the compositor is copied into a GPU texture, and that copy is counted against the process without
    /// being in its malloc space. On macOS it is `IOSurface`, and it is the largest single region this
    /// app holds — 111 MiB with a page of a textbook on screen at full detail, against 81 MiB with the
    /// lean memory mode on.
    ///
    /// `None` is *nothing to report*, the same rule [`Self::vitals`] follows: a platform that cannot
    /// measure it leaves the figure out rather than printing a zero that looks like an empty GPU.
    ///
    /// Read only by the memory report (see `crate::memory_report`), which is the caller that exists to
    /// say where the megabytes are. Nothing on the frame path asks, and a build with no implementation
    /// answers `None` and loses one column of a table.
    fn surface_bytes(&mut self) -> Option<u64> {
        None
    }
}

/// The surface the desk, the paper and the ink are painted on: the view says what, this says how.
pub trait InkSurface {
    /// Paints one frame of the canvas.
    ///
    /// `Ok(true)` means a frame was handed to the compositor. `Ok(false)` means the surface had no
    /// drawable ready — a frame to skip rather than a thread to park, so the next wake tries again
    /// (see [`crate::app`]). The error is a sentence rather than a type: it is shown in the status
    /// line, once, and the app keeps running without a canvas rather than failing to start.
    fn draw(&mut self, canvas: &Canvas) -> Result<bool, String>;

    /// Whether frames wait for the display's own clock before they are shown.
    ///
    /// A surface whose platform offers no such choice ignores this rather than failing: the setting is
    /// stored with the note on every platform, and Windows presents at a sync interval of zero for
    /// reasons of its own (see `apps/windows/platform/ink_layer/device.rs`). Called when the setting
    /// moves and once at install, so a surface never has to guess what the note wanted.
    fn set_vsync(&mut self, on: bool);

    /// Whether this surface can honour [`Self::set_vsync`] at all.
    ///
    /// `true` is the default, because a surface that has no opinion keeps the switch visible and
    /// keeps the old behaviour. Windows answers `false`: its canvas presents at a sync interval of
    /// zero on purpose, so a V-Sync switch there would be a control that does nothing. The setting
    /// is still stored with the note — it travels between machines — but the bar only offers it
    /// where pressing it can have an effect (see `NoteApp::honours_vsync`).
    fn honours_vsync(&self) -> bool {
        true
    }
}

/// Where the pen's readings come from.
pub trait PenSource {
    /// The queue the view parks on and drains.
    fn inbox(&self) -> Arc<PenInbox>;
    /// What to tell the user about the pen when it cannot be read; empty when it can.
    ///
    /// The status line shows this only as a *problem* — a capture that attached says nothing — and a
    /// line that names the platform is the one thing a person can act on when the pen is silent.
    fn status(&self) -> &str;
    /// What the capture can report about the digitizer, or `None` when there is no capture.
    ///
    /// Diagnostic rather than data: it is the platform's own report — how the capture is attached, how
    /// coarsely it batches, which axes the digitizer has — and it is read under `CHEAP_NOTE_TRACE`
    /// rather than in the status line, which is the same sentence on every OS without it.
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
    /// How large one of the design's logical pixels is on this platform, as a factor.
    ///
    /// The lengths this crate writes down — the palette's rings and its reach, a stroke's width — are
    /// the logical pixels a **Windows** display measures in (96 to the inch). macOS measures in
    /// *points*, four thirds as large, so a shape that should look the same in the hand is three
    /// quarters of those numbers there; a platform that measures in the design's own unit answers
    /// `1.0`. Every length that has to look the same on both is multiplied by this.
    pub unit_scale: f32,
    /// Whether a line-based scroll — a mouse wheel — moves the sheet.
    ///
    /// The view's rule is that a trackpad reports pixels and a wheel reports lines, and that only
    /// pixels are the distance the sheet already pans in (see `NoteApp::on_scroll`). A platform
    /// whose wheel is the ordinary way to scroll a page answers `true`; the view then reads a line
    /// as `WHEEL_PAN_PER_LINE` logical pixels. A platform whose trackpad is the pointer the rule was
    /// written for answers `false`, and a wheel stays unread.
    ///
    /// A field rather than a `cfg`: the wheel rule is about what this machine's users scroll with,
    /// and `note-ui` may not name an operating system (`doc/MACOS.md` §6).
    pub wheel_pans: bool,
    /// The pen. Always there, because a pen that cannot be read is a pen that draws with the mouse —
    /// a service that says so in its status rather than an absence to handle everywhere.
    pub pen: Box<dyn PenSource>,
    /// What this process is using, when the platform can say.
    ///
    /// `None` is a platform with no implementation of [`SystemStats`], which is a build that shows no
    /// hardware clause rather than one that fails to load. Boxed and optional rather than a required
    /// field so that a new platform is not obliged to write a counter it does not need in order to
    /// compile — the same shape the canvas and the ghost already have.
    pub stats: Option<Box<dyn SystemStats>>,
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
