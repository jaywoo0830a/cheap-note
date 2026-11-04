//! A window, a pen, and a line that follows the nib: the smallest thing that shows what this crate reads.
//!
//! ```text
//! cargo run -p pen-macos --example watch
//! ```
//!
//! Write in the window with the stylus. The line is drawn from the readings alone — position for where,
//! **pressure for how thick** — so if the line follows the nib and thickens where the nib is pressed,
//! the values this crate promises are real. Every reading also prints to the terminal, with tilt in
//! degrees and the device's own capability mask:
//!
//! ```text
//! Down 89.6,434.9 p=0.04 tilt=+27.0/+2.0 caps=0x05c7 7ms
//! ```
//!
//! What to look for:
//!
//! * `Down` once at the start of a stroke and `Up` once at the end, with `Move` between them;
//! * `p=0.00 … p=1.00` changing as the nib is pressed — and the line's width with it;
//! * `tilt=+.. / +..` changing as the pen is leaned, in degrees (`±90` is flat on the desk);
//! * `caps=`: the axes the device says it can report; `0x05c7` includes tilt X, tilt Y and pressure;
//! * the milliseconds at the end: AppKit's own stamp against this process's clock, which is the part of
//!   the latency nothing here can remove.
//!
//! A mouse works too — press and drag — but it has no force and no lean, so those two are absent.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_graphics::base::CGFloat;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::{CGContext, CGLineCap};
use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};
use foreign_types_shared::ForeignType;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint as WindowPoint, CGRect as WindowRect, CGSize as WindowSize};
use objc2_foundation::NSString;
use objc2_quartz_core::CALayer;

use pen_macos::{Pen, Phase, Reading};

/// `kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little`.
const BGRA: u32 = 2 | (2 << 12);

/// How often the pad is handed to the window, at most: sixty times a second is more than a screen shows
/// and far fewer than a pen reports, and this example is not trying to be clever about it.
const PRESENT_EVERY: Duration = Duration::from_millis(16);

/// The paper: a bitmap the readings are drawn into, and the layer that shows it.
struct Pad {
    layer: Retained<CALayer>,
    context: CGContext,
    /// Where the last reading was, while a stroke is in progress.
    last: Option<(f32, f32)>,
    presented: Instant,
}

impl Pad {
    fn install(view: &NSView) -> Option<Pad> {
        let bounds = view.bounds();
        let scale = view
            .window()
            .map_or(1.0, |window| window.backingScaleFactor());
        let width = ((bounds.size.width * scale).round() as usize).max(1);
        let height = ((bounds.size.height * scale).round() as usize).max(1);

        let space = CGColorSpace::create_device_rgb();
        let context =
            CGContext::create_bitmap_context(None, width, height, 8, width * 4, &space, BGRA);

        // Core Graphics draws with y up, and a reading's y grows downwards.
        context.concat_ctm(CGAffineTransform {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            tx: 0.0,
            ty: height as CGFloat,
        });

        // Paper.
        context.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
        context.fill_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(width as CGFloat, height as CGFloat),
        ));

        view.setWantsLayer(true);
        let layer = view.layer()?;
        layer.setContentsScale(scale);

        let mut pad = Pad {
            layer,
            context,
            last: None,
            presented: Instant::now(),
        };

        pad.present();

        Some(pad)
    }

    /// Draws one reading: a segment from the last position, as thick as the nib is pressed.
    fn ink(&mut self, reading: &Reading) {
        match reading.phase {
            Phase::Down => {
                // A tap with no movement is still a mark.
                self.segment(reading.x, reading.y, reading.x, reading.y, reading.pressure);
                self.last = Some((reading.x, reading.y));
            }
            Phase::Move => {
                if let Some((x, y)) = self.last {
                    self.segment(x, y, reading.x, reading.y, reading.pressure);
                }

                self.last = Some((reading.x, reading.y));
            }
            _ => self.last = None,
        }

        if self.presented.elapsed() >= PRESENT_EVERY {
            self.present();
        }
    }

    fn segment(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, pressure: Option<f32>) {
        // A point and a half to eight: a mouse sits in the middle, a pressed nib at the top.
        let width = 1.5 + pressure.unwrap_or(0.6) * 7.0;

        let context = &self.context;
        context.set_rgb_stroke_color(0.10, 0.10, 0.12, 1.0);
        context.set_line_width(CGFloat::from(width));
        context.set_line_cap(CGLineCap::CGLineCapRound);
        context.begin_path();
        context.move_to_point(CGFloat::from(x0), CGFloat::from(y0));
        context.add_line_to_point(CGFloat::from(x1), CGFloat::from(y1));
        context.stroke_path();
    }

    /// Hands the bitmap to the layer. The layer keeps it; the image may be dropped here.
    fn present(&mut self) {
        if let Some(image) = self.context.create_image() {
            // SAFETY: a `CGImage` is an object, and `setContents` is the layer's own method for one.
            unsafe {
                let object = &*(image.as_ptr() as *const AnyObject);
                self.layer.setContents(Some(object));
            }
        }

        self.presented = Instant::now();
    }
}

thread_local! {
    /// The pad, where the pen's tap can reach it: everything here is AppKit and Core Graphics, all of it
    /// on the main thread, and a `Send` handle for it would be a lie.
    static PAD: RefCell<Option<Pad>> = const { RefCell::new(None) };
}

fn main() {
    let mtm = MainThreadMarker::new().expect("the main thread");

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let frame = WindowRect::new(WindowPoint::new(80.0, 80.0), WindowSize::new(900.0, 700.0));

    // SAFETY: the main thread, and AppKit's designated initialiser for a window.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };

    window.setTitle(&NSString::from_str("pen-macos — write in this window"));
    window.makeKeyAndOrderFront(None);
    app.activate();

    let view = window.contentView().expect("a content view");

    PAD.with(|pad| *pad.borrow_mut() = Pad::install(&view));

    let pen = Pen::attach_view(
        &view,
        Arc::new(|reading| {
            println!("{reading}");

            PAD.with(|pad| {
                if let Some(pad) = pad.borrow_mut().as_mut() {
                    pad.ink(&reading);
                }
            });
        }),
    )
    .expect("a monitor");

    println!(
        "pen-macos: write in the window. The line follows the nib, and thickens with pressure."
    );

    app.run();

    drop(pen);
}
