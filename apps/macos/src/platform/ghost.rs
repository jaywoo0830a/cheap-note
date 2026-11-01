//! Where the pen is, and whether it has a mark on screen.
//!
//! The mark itself is drawn by the canvas (`platform::metal`), in the same pass as the ink. A cursor belongs
//! above the ink and **below** the interface, and a window of its own could be neither: it floated over the
//! interface, which is what the strategy in `cursor.md` says not to do. What is left here is the state, and
//! the rules for when there is nothing to show.

use std::sync::Mutex;

use note_ui::cursor::PenCursor;
use note_ui::input::Reading;
use note_ui::platform::{GhostFeed, Screen};
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

/// What the canvas draws for the pen.
#[derive(Clone, Copy)]
pub struct Mark {
    /// Where the nib is, and how the pen leans — in logical window pixels.
    pub cursor: PenCursor,
    /// The ink in hand, `0xRRGGBB`: what the next stroke will be written in.
    pub colour: u32,
    /// The paper's contrast colour, `0xRRGGBB`: the halo, and the body's faint passes.
    pub halo: u32,
    /// The nib's radius, in logical pixels: how thick the next line will be.
    pub nib_radius: f32,
}

/// What the view last said about the screen, and where the pen last was.
struct State {
    /// `None` when there is no pen: out of range, cancelled, or this app is not the one in front.
    cursor: Option<PenCursor>,
    screen: Screen,
}

/// The pen's mark on the screen, as the canvas needs it.
pub struct Ghost {
    state: Mutex<State>,
}

// SAFETY: every method is called on the main thread — `offer` from the pen's tap inside AppKit's own event
// delivery, `set_screen` and `mark` from the view's frame — and the state is a plain `Mutex`.
unsafe impl Send for Ghost {}

impl Ghost {
    /// Makes the pen's state. Nothing to install, now that the mark is the canvas's.
    pub fn install() -> Option<Self> {
        Some(Ghost {
            state: Mutex::new(State {
                cursor: None,
                screen: Screen::default(),
            }),
        })
    }

    /// The mark to draw, or `None` when there is nothing to show.
    ///
    /// Nothing to show means: the pen has not been seen, the view has suppressed it (the home list, a
    /// question on screen), or the nib is above the bar — where a reading belongs to a control, not to the
    /// page.
    pub fn mark(&self) -> Option<Mark> {
        let state = self.state.lock().ok()?;
        let cursor = state.cursor?;
        let screen = state.screen;

        if screen.suppressed {
            return None;
        }

        // The bar's edge is in physical pixels and a cursor is measured in logical ones.
        if cursor.position()[1] <= screen.sheet_top / sane_scale(screen.scale) {
            return None;
        }

        Some(Mark {
            cursor,
            colour: screen.colour & 0x00FF_FFFF,
            halo: screen.halo & 0x00FF_FFFF,
            nib_radius: screen.nib_radius,
        })
    }

    /// Whether this app is the one in front.
    ///
    /// Read on the reading rather than observed from a notification: the answer is needed then anyway, and
    /// a pen left behind in another app's window is a mark on a page that is not ours.
    fn app_is_active() -> bool {
        MainThreadMarker::new()
            .is_some_and(|mtm| NSApplication::sharedApplication(mtm).isActive())
    }
}

impl GhostFeed for Ghost {
    fn offer(&self, readings: &[Reading]) {
        let Some(reading) = readings.last() else {
            return;
        };

        if let Ok(mut state) = self.state.lock() {
            let scale = sane_scale(state.screen.scale);

            state.cursor = if Ghost::app_is_active() {
                PenCursor::from_sample(*reading, scale)
            } else {
                None
            };
        }
    }

    fn set_screen(&self, screen: Screen) {
        if let Ok(mut state) = self.state.lock() {
            state.screen = screen;
        }
    }

    fn is_alive(&self) -> bool {
        true
    }
}

/// A scale of nought is a scale of one.
fn sane_scale(scale: f32) -> f32 {
    if scale > 0.0 {
        scale
    } else {
        1.0
    }
}
