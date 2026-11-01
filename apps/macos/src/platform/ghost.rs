//! The pen's ghost cursor: the nib's mark and the pen's lean, in a window of its own.
//!
//! ## What it is
//!
//! A disc at the nib in the ink's own colour, with a ring under it in the paper's contrast colour so that
//! white ink on white paper is still findable — plus, when the device reports tilt, the **body** of the pen
//! leaning the way the hand holds it. The disc's radius is half the line in hand, so the mark is a
//! *preview* of the stroke rather than a dot.
//!
//! The shapes are [`PenCursor`]'s, the same ones the Windows overlay draws: `body_shape`, `Body::outline`
//! and the alphas in [`note_ui::cursor`] are platform-free, so both builds lean by the same rules. What is
//! written here is only how Core Graphics draws them.
//!
//! ## Why a window of its own
//!
//! The Windows build puts its ghost in a layered window above the interface, and this is the same shape: a
//! borderless window that is *never* key, ignores the mouse entirely, and holds nothing but the mark. It
//! is above every window of the app — the canvas, the palette, the UI — so the nib is never covered by the
//! thing it is drawing on, and the app's frame has no part in moving it.
//!
//! ## How moving it stays cheap
//!
//! The pen's tap (AppKit's own event delivery, on the main thread) moves the window directly: a bitmap the
//! size of the mark and its body, one `contents`, one frame origin. No part of the page is touched, and no
//! frame is asked for. The position is converted from the reading's physical client pixels with
//! `NSWindow.convertPointToScreen`, so a second screen or a rotation is AppKit's arithmetic, not ours.
//!
//! Two things a first version gets wrong, both invisible until the pen is in hand:
//!
//! * a layer **animates** `frame` and `contents` for a quarter of a second by default, so the ghost eases
//!   towards the nib and reads as lag — hence `CATransaction.setDisableActions(true)`;
//! * a reading is in the window's **client pixels from the top left**, while AppKit's window and screen
//!   coordinates grow upwards from the bottom left: flip, then `convertPointToScreen`.
//!
//! What the frame contributes is only what a reading cannot say — the scale, the colours, the bar's edge —
//! through [`GhostFeed::set_screen`].

use std::sync::Mutex;

use core_graphics::context::CGContext;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use foreign_types_shared::ForeignType;
use note_ui::cursor::{
    PenCursor, BODY_ALPHA, BODY_HALO_ALPHA, BODY_HALO_GROW, NIB_ALPHA, NIB_BLOOM_ALPHA,
    NIB_BLOOM_SPREAD, NIB_RADIUS_MAX, NIB_RADIUS_MIN,
};
use note_ui::input::Reading;
use note_ui::platform::{GhostFeed, Screen};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSFloatingWindowLevel,
};
use objc2_foundation::{NSRect, NSPoint, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::platform::bitmap::Bitmap;

/// How far the body's shadow is dropped, in logical pixels — the Windows overlay's own offset, so that the
/// two builds' ghosts are one drawing.
const BODY_SHADOW_DROP: [f32; 2] = [3.0, 3.6];

/// How opaque the body's shadow is, at full lean: the widest and faintest of the three passes.
const BODY_SHADOW_ALPHA: f64 = 0.26;

/// What the view last said about the screen, and where the pen last was.
struct State {
    /// Where the pen is and how it leans, in logical window pixels — `None` when there is no pen: out of
    /// range, cancelled, or this app is not the one in front.
    cursor: Option<PenCursor>,
    screen: Screen,
    /// What the panel is showing, so that a reading which changed nothing costs nothing.
    painted: Option<Key>,
}

impl Default for State {
    fn default() -> Self {
        State {
            cursor: None,
            screen: Screen::default(),
            painted: None,
        }
    }
}

/// Everything the drawing depends on, so that "the same mark twice" is one comparison.
#[derive(Clone, Copy, PartialEq)]
struct Key {
    cursor: PenCursor,
    colour: u32,
    halo: u32,
    nib_radius: f32,
    scale: f32,
}

/// The ghost's window, and the state that decides what it shows.
pub struct Ghost {
    /// The app's window: a reading is in its client pixels, and the panel has to be placed on the screen.
    anchor: Retained<NSWindow>,
    /// The mark's window: borderless, click-through, and never key.
    panel: Retained<NSWindow>,
    /// The panel's layer, whose `contents` is the drawing.
    layer: Retained<CALayer>,
    state: Mutex<State>,
}

// SAFETY: every method here is called on the main thread — `offer` from the pen's tap inside AppKit's own
// event delivery, `set_screen` and `is_alive` from the view's frame — and the panel is touched nowhere
// else. The bounds are the view's, which states them for the Windows overlay that lives on a thread; this
// type keeps the same contract without needing the thread.
unsafe impl Send for Ghost {}

impl Ghost {
    /// Makes the mark's window, and hands back the ghost that moves it.
    ///
    /// Must be called on the main thread: it makes an `NSWindow`, which is AppKit's to own there.
    pub fn install(window: &gpui_kit::Window) -> Option<Self> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };

        // SAFETY: AppKit's own view, alive for the app's life, used on the main thread.
        let view = unsafe { appkit.ns_view.as_ptr().cast::<NSView>().as_ref() }?;

        // The window the view is in: the one a reading's client pixels belong to.
        let anchor = view.window()?;

        let mtm = MainThreadMarker::new()?;

        // A window with nothing in it: no title bar, no shadow, no fill. It is the size of the drawing and
        // follows it, and it is not a place a person can click or type — a borderless `NSWindow` cannot
        // become key, which is what keeps the pen's window the one in front.
        let panel = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };

        // SAFETY: the panel is alive and owned by the value this builds, and this is the main thread.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setIgnoresMouseEvents(true);
        panel.setMovable(false);
        panel.setCanHide(false);

        // Above the app's own windows — the canvas, the palette, the UI — because a cursor that the thing
        // it draws on can cover is not a cursor. `Transient` keeps it out of Exposé and window cycling.
        panel.setLevel(NSFloatingWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle,
        );

        let content = NSView::new(mtm);
        content.setWantsLayer(true);
        panel.setContentView(Some(&content));

        let layer = content.layer()?;
        layer.setContentsScale(f64::from(window.scale_factor()));

        Some(Ghost {
            anchor,
            panel,
            layer,
            state: Mutex::new(State::default()),
        })
    }

    /// Whether this app is the one in front.
    ///
    /// Read here rather than observed from a notification: the answer is needed on every reading anyway,
    /// and a reading is when it can matter.
    fn app_is_active(&self) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };

        NSApplication::sharedApplication(mtm).isActive()
    }

    /// The pen to draw, from what the view last said and where the pen last was.
    ///
    /// `None` means there is nothing to show: the pen has not been seen, the view has suppressed it (the
    /// home list, a question on screen), or the nib is above the bar — where a reading belongs to a
    /// control, not to the page.
    fn pen(state: &State) -> Option<PenCursor> {
        let cursor = state.cursor?;
        let screen = state.screen;

        if screen.suppressed {
            return None;
        }

        // The bar's edge is in physical pixels and a cursor is measured in logical ones.
        if cursor.position()[1] <= screen.sheet_top / sane_scale(screen.scale) {
            return None;
        }

        Some(cursor)
    }
}


impl Ghost {
    /// Draws the mark and the lean, and puts the panel where they belong — or takes it away.
    fn show(&self, state: &mut State, pen: Option<PenCursor>) {
        let screen = state.screen;
        let key = pen.map(|cursor| Key {
            cursor,
            colour: screen.colour,
            halo: screen.halo,
            nib_radius: screen.nib_radius,
            scale: screen.scale,
        });

        if state.painted == key {
            return;
        }

        state.painted = key;

        let Some(cursor) = pen else {
            self.panel.orderOut(None);
            return;
        };

        let scale = f64::from(sane_scale(screen.scale));
        let nib = cursor.position();
        let radius = sane_radius(screen.nib_radius);
        let body = cursor.body_shape();

        // The area the drawing covers, in logical pixels: the nib's bloom, and — when the pen leans — the
        // body with its own soft edge and its shadow, which are the parts that reach furthest.
        let mut area = Area::around(nib, radius + NIB_BLOOM_SPREAD);

        if let Some(body) = &body {
            area = area.covering(&body.outline(BODY_HALO_GROW));
            area = area.covering(&shifted(&body.outline(0.0), BODY_SHADOW_DROP));
        }

        // A pixel of slack, because every edge here is antialiased.
        let area = area.grown(1.0);

        let (width, height) = (
            (f64::from(area.width) * scale).ceil().max(1.0) as usize,
            (f64::from(area.height) * scale).ceil().max(1.0) as usize,
        );

        let Some(bitmap) = Bitmap::new(width, height) else {
            return;
        };

        // Logical window pixels in, this bitmap's own pixels out: the nib is where the page says it is.
        let map = |p: [f32; 2]| {
            (
                f64::from(p[0] - area.x) * scale,
                f64::from(p[1] - area.y) * scale,
            )
        };

        // A layer animates whatever is set on it, and the default animation is a quarter of a second:
        // without this the ghost *eases* towards the nib, which is the whole of what "lag" feels like.
        CATransaction::begin();
        CATransaction::setDisableActions(true);

        // The body, in three passes — shadow, soft edge, body — which is the order the Windows overlay uses
        // and the same colours: the two fainter ones in the paper's contrast colour, so a rod is findable
        // on paper of any colour, and the body itself in the ink's.
        if let Some(body) = &body {
            let fade = f64::from(cursor.body_fade());

            fill_outline(
                &bitmap.context,
                &shifted(&body.outline(0.0), BODY_SHADOW_DROP),
                &map,
                screen.halo,
                BODY_SHADOW_ALPHA * fade,
            );
            fill_outline(
                &bitmap.context,
                &body.outline(BODY_HALO_GROW),
                &map,
                screen.halo,
                f64::from(BODY_HALO_ALPHA) * fade,
            );
            fill_outline(
                &bitmap.context,
                &body.outline(0.0),
                &map,
                screen.colour,
                f64::from(BODY_ALPHA) * fade,
            );
        }

        // The nib: a disc as wide as the line in hand, over a bloom, drawn last so that it is the one thing
        // nothing else can cover — it says where the ink will land, and how thick it will be.
        let at = map(nib);
        disc(
            &bitmap.context,
            at,
            (f64::from(radius) + f64::from(NIB_BLOOM_SPREAD)) * scale,
            screen.halo,
            f64::from(NIB_BLOOM_ALPHA),
        );
        disc(
            &bitmap.context,
            at,
            f64::from(radius) * scale,
            screen.colour,
            f64::from(NIB_ALPHA),
        );

        if let Some(image) = bitmap.image() {
            // SAFETY: a `CGImage` is an object, and `setContents` is the layer's own method for one.
            unsafe {
                let object = &*(image.as_ptr() as *const AnyObject);
                self.layer.setContents(Some(object));
            }
        }

        // AppKit's window coordinates grow upwards from the bottom left, so the area's *top* edge is what
        // becomes the window's y; the screen's own coordinates are AppKit's to work out from there.
        let height_of_window = self
            .anchor
            .contentView()

            .map_or(0.0, |view| view.frame().size.height);

        let corner = NSPoint::new(
            f64::from(area.x),
            height_of_window - f64::from(area.y + area.height),
        );
        let on_screen = self.anchor.convertPointToScreen(corner);

        self.panel.setFrame_display(
            NSRect::new(
                on_screen,
                NSSize::new(f64::from(area.width), f64::from(area.height)),
            ),
            false,
        );

        CATransaction::commit();

        self.panel.orderFrontRegardless();
    }
}

unsafe impl Sync for Ghost {}

impl GhostFeed for Ghost {
    /// The newest reading's position and lean, from the pen's tap: the ghost follows the pen, not the frame.
    fn offer(&self, readings: &[Reading]) {
        let Some(reading) = readings.last() else {
            return;
        };

        // A floating window is above *every* app's windows, so a ghost left behind while another app is in
        // front would be a mark on a page that is not ours. The pen's readings stop when the event is not
        // ours; this is the other half of that rule.
        let active = self.app_is_active();

        if let Ok(mut state) = self.state.lock() {
            let scale = sane_scale(state.screen.scale);
            state.cursor = if active {
                PenCursor::from_sample(*reading, scale)
            } else {
                None
            };

            let pen = Ghost::pen(&state);
            self.show(&mut state, pen);
        }
    }

    /// What the frame knows and a reading cannot say: the scale, the colours, the bar's edge.
    fn set_screen(&self, screen: Screen) {
        if let Ok(mut state) = self.state.lock() {
            state.screen = screen;

            let pen = Ghost::pen(&state);
            self.show(&mut state, pen);
        }
    }

    /// Whether there is a mark to show. The panel is the app's for its whole life, so this is true until
    /// the app ends — and the view turns it into "hide the arrow" on its own.
    fn is_alive(&self) -> bool {
        true
    }
}

/// A rectangle in logical window pixels: the part of the screen the drawing needs.
#[derive(Clone, Copy)]
struct Area {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Area {
    /// The square around a point, which is the whole of a pen held straight up.
    fn around(point: [f32; 2], radius: f32) -> Self {
        Area {
            x: point[0] - radius,
            y: point[1] - radius,
            width: radius * 2.0,
            height: radius * 2.0,
        }
    }

    /// This area grown to hold an outline: what the body needs, over the nib's own square.
    fn covering(self, points: &[[f32; 2]; 7]) -> Self {
        let mut left = self.x;
        let mut top = self.y;
        let mut right = self.x + self.width;
        let mut bottom = self.y + self.height;

        for point in points {
            left = left.min(point[0]);
            top = top.min(point[1]);
            right = right.max(point[0]);
            bottom = bottom.max(point[1]);
        }

        Area {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
    }

    /// This area, a little bigger on every side.
    fn grown(self, by: f32) -> Self {
        Area {
            x: self.x - by,
            y: self.y - by,
            width: self.width + by * 2.0,
            height: self.height + by * 2.0,
        }
    }
}

/// An outline moved by an offset: the body's shadow is the body, dropped.
fn shifted(points: &[[f32; 2]; 7], by: [f32; 2]) -> [[f32; 2]; 7] {
    let mut moved = *points;

    for point in &mut moved {
        point[0] += by[0];
        point[1] += by[1];
    }

    moved
}

/// The window's scale factor, or `1` for an answer that is not a scale — the same rule the ink uses.
fn sane_scale(scale: f32) -> f32 {
    if scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// The nib mark's radius, kept within the bounds the geometry sets: a mark too small to find, or one that
/// would fill the window, is not the app's to ask for.
fn sane_radius(radius: f32) -> f32 {
    if radius.is_finite() {
        radius.clamp(NIB_RADIUS_MIN, NIB_RADIUS_MAX)
    } else {
        NIB_RADIUS_MIN
    }
}

fn fill_colour(context: &CGContext, colour: u32, alpha: f64) {
    let red = f64::from((colour >> 16) & 0xff) / 255.0;
    let green = f64::from((colour >> 8) & 0xff) / 255.0;
    let blue = f64::from(colour & 0xff) / 255.0;

    context.set_rgb_fill_color(red, green, blue, alpha);
}

/// Fills a [`Body`](note_ui::cursor::Body) outline: three quadratic curves from the nib's left flank, round
/// the far end, and back down the right — the order `outline` promises.
fn fill_outline(
    context: &CGContext,
    points: &[[f32; 2]; 7],
    map: &impl Fn([f32; 2]) -> (f64, f64),
    colour: u32,
    alpha: f64,
) {
    let (x, y) = map(points[0]);
    context.move_to_point(x, y);

    for (control, to) in [(1, 2), (3, 4), (5, 6)] {
        let (control_x, control_y) = map(points[control]);
        let (to_x, to_y) = map(points[to]);

        context.add_quad_curve_to_point(control_x, control_y, to_x, to_y);
    }

    context.close_path();
    fill_colour(context, colour, alpha);
    context.fill_path();
}

/// A filled disc, in the ink's colour or the halo's.
fn disc(context: &CGContext, at: (f64, f64), radius: f64, colour: u32, alpha: f64) {
    fill_colour(context, colour, alpha);

    context.fill_ellipse_in_rect(CGRect::new(
        &CGPoint::new(at.0 - radius, at.1 - radius),
        &CGSize::new(radius * 2.0, radius * 2.0),
    ));
}
