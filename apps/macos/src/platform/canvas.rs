//! The canvas, drawn by the platform: Core Graphics into a bitmap, the bitmap as a layer's contents.
//!
//! ## Why the ink is not in the frame
//!
//! The Windows build learned this the hard way — the canvas was drawn inside a GPUI frame at first, and
//! the measurement that killed it is in `apps/windows/src/platform/ink_layer/mod.rs`: a page of 370
//! strokes held about 33 fps with `paint` at 9.3 ms, against 143 fps and 0.0 ms for an empty page. A
//! frame is a *submission*: every element, every path, tessellated and uploaded again, and the cost
//! grows with the ink on the page. With a digitizer reporting a hundred or two readings a second, that
//! is a frame per reading of the most expensive frame there is.
//!
//! So the canvas is a renderer of the platform's own, in a layer of its own, and the frame never hears
//! about a page of handwriting:
//!
//! ```text
//!   pen reading ──▶ InkSurface::draw(&Canvas) ──▶ Core Graphics ──▶ layer.contents
//! ```
//!
//! ## The three things that put it on screen
//!
//! 1. the window is made **transparent** (`Window::set_background_appearance`) so GPUI's own
//!    `CAMetalLayer` is not opaque — otherwise this view sits behind an opaque surface and shows
//!    nothing at all;
//! 2. the root surface is left **unpainted** (`platform::before_window`), the same `RootPlugin` the
//!    Windows build registers, so the theme's background is not a rectangle over it;
//! 3. this view is a subview of the window's content view, ordered **below** GPUI's.
//!
//! ## Two bitmaps, and why
//!
//! * `base` — the desk, the page's shadow, the sheet, the ruling, the PDF page and every **finished**
//!   stroke. It is repainted when the scene changes: a page turn, a zoom, an undo, a closed stroke.
//! * a bitmap the size of **the stroke under the pen**, in a sublayer of its own. A reading clears it,
//!   fills the ribbon into it, and hands it to its layer — a few hundred kilobytes, not the window.
//!   This is the difference between a pen that keeps up and one that does not: `contents` is an image,
//!   and an image of the whole window is nine to eighteen megabytes *per reading*.
//!
//! A frame's own call to `draw` describes the same canvas as the reading before it and returns after one
//! comparison (see [`Scene`]).
//!
//! ## What is left for later
//!
//! The present is a `CGImage` per update, and Core Graphics snapshots copy-on-write: one memcpy of the
//! bitmap that was written. The Windows build pays no such copy because a swap chain is drawn into
//! directly; on a Mac the equivalent is a `CAMetalLayer` over a shared `IOSurface`, and when the
//! measurements ask for it only [`InkCanvas::present_stroke`] changes.

use std::ffi::c_void;
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;

use core_graphics::base::CGFloat;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::data_provider::CGDataProvider;
use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};
use core_graphics::image::CGImage;
use foreign_types_shared::ForeignType;
use note_ui::ink_layer::canvas::Rect;
use note_ui::ink_layer::{ink_colour, Canvas};
use note_ui::paint::drawn_of_paper;
use note_ui::platform::InkSurface;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindowOrderingMode};
use objc2_quartz_core::CALayer;

/// `kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little`: premultiplied BGRA, which is what
/// Core Graphics, GPUI and Pdfium all speak.
const BGRA: u32 = 2 | (2 << 12);

/// The same, for an image with no alpha at all — a rasterised PDF page, which is opaque paper.
const BGRA_OPAQUE: u32 = 6 | (2 << 12);

// Core Graphics' graphics-state stack, which the `core-graphics` crate does not wrap: these two calls are
// the whole of what the drawing here needs that the crate leaves out.
extern "C" {
    fn CGContextSaveGState(context: *mut c_void);
    fn CGContextRestoreGState(context: *mut c_void);
}

/// The ink, drawn by the platform.
pub struct InkCanvas {
    /// The view the canvas is shown in: a subview of the window's content view, below GPUI's.
    view: Retained<NSView>,
    /// The view's own layer, whose `contents` is the bitmap the window shows.
    layer: Retained<CALayer>,
    /// Everything that does not move while a stroke is being drawn.
    base: Bitmap,
    /// `base` as an image: what a damaged rectangle is restored from.
    base_image: Option<CGImage>,
    /// The bitmap the window shows: the base, plus the stroke under the pen.
    shown: Bitmap,
    /// The open stroke's last box, in device pixels: the part of `shown` to put back.
    open: Option<CGRect>,
    /// What the base was drawn from, so a call that changed nothing does not paint it again.
    scene: Option<Scene>,
    /// What the open stroke looked like last time — see [`open_key`].
    open_key: Option<(usize, usize, [u32; 4])>,
    /// The page's pixels, as Core Graphics sees them, kept until the page is rasterised again.
    page: Option<(usize, CGImage)>,
}

/// One bitmap and the context that draws into it.
pub(super) struct Bitmap {
    pub(super) context: CGContext,
    pub(super) width: usize,
    pub(super) height: usize,
}

impl Bitmap {
    pub(super) fn new(width: usize, height: usize) -> Option<Self> {
        let space = CGColorSpace::create_device_rgb();
        let context =
            CGContext::create_bitmap_context(None, width, height, 8, width * 4, &space, BGRA);

        // Core Graphics draws with y up; the canvas describes itself with y down, and so does the screen.
        // One flip at the top of the bitmap makes every coordinate in this module the canvas's own.
        context.concat_ctm(CGAffineTransform {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            tx: 0.0,
            ty: height as CGFloat,
        });

        Some(Bitmap {
            context,
            width,
            height,
        })
    }

    pub(super) fn image(&self) -> Option<CGImage> {
        self.context.create_image()
    }
}

impl InkCanvas {
    /// Installs the canvas in a window that has just been made: a view of ours under GPUI's.
    pub fn install(window: &gpui_kit::Window) -> Option<Self> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };

        // SAFETY: AppKit's own content view, alive for the window's life, used on the main thread.
        let gpui_view = unsafe { appkit.ns_view.as_ptr().cast::<NSView>().as_ref() }?;

        // SAFETY: the view is in a window — it is the one GPUI draws the interface into — and this is the
        // main thread. The parent is the window's content view, which outlives the canvas.
        let parent = unsafe { gpui_view.superview() }?;

        let scale = f64::from(window.scale_factor());
        let frame = gpui_view.frame();

        let view = NSView::new(MainThreadMarker::new()?);
        view.setFrame(frame);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.setWantsLayer(true);

        let layer = view.layer()?;
        layer.setContentsScale(scale);

        // Below GPUI's view: the interface is painted over the ink, never the other way round.
        parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, Some(gpui_view));

        let (width, height) = (device(frame.size.width, scale), device(frame.size.height, scale));

        Some(InkCanvas {
            view,
            layer,
            base: Bitmap::new(width, height)?,
            base_image: None,
            shown: Bitmap::new(width, height)?,
            open: None,
            scene: None,
            open_key: None,
            page: None,
        })
    }

    /// Hands the bitmap to the layer. This is the one present, and it is a whole window.
    fn present(&mut self) {
        let Some(image) = self.shown.image() else {
            return;
        };

        // SAFETY: a `CGImage` is an object, and `setContents` is the layer's own method for one.
        unsafe {
            let object = &*(image.as_ptr() as *const AnyObject);
            self.layer.setContents(Some(object));
        }
    }

    /// Puts the whole base into the shown bitmap: a new scene, a resize, a page turn.
    fn copy_all(&mut self) {
        let full = CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(self.shown.width as f64, self.shown.height as f64),
        );

        self.restore(full);
    }

    /// Puts one rectangle of the base back into the shown bitmap.
    ///
    /// Nothing about a stroke's box is converted to a layer's coordinates here: the base and the shown
    /// bitmap are the same size and share one mapping, so a point of the canvas is the same pixel in both
    /// — which is what makes the stroke under the pen land exactly where the nib was.
    fn restore(&mut self, damaged: CGRect) {
        if self.base_image.is_none() {
            self.base_image = self.base.image();
        }

        let Some(base) = self.base_image.as_ref() else {
            return;
        };

        let context = &self.shown.context;
        let full = CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(self.shown.width as f64, self.shown.height as f64),
        );

        save(context);
        context.clip_to_rect(damaged);
        draw_image(context, full, base);
        restore(context);
    }

    /// Matches the bitmaps to the view, when the window changed size or screen. `true` when they did.
    fn resize(&mut self, scale: f64) -> bool {
        let frame = self.view.frame();
        let (width, height) = (device(frame.size.width, scale), device(frame.size.height, scale));

        if self.shown.width == width && self.shown.height == height {
            return false;
        }

        let (Some(shown), Some(base)) = (Bitmap::new(width, height), Bitmap::new(width, height))
        else {
            return false;
        };

        self.shown = shown;
        self.base = base;
        self.base_image = None;
        self.scene = None;
        self.open = None;
        self.open_key = None;
        self.layer.setContentsScale(scale);

        true
    }
}

/// A length in logical pixels, in device pixels: at least one, because a bitmap of none is no bitmap.
fn device(length: CGFloat, scale: f64) -> usize {
    ((length * scale).round() as usize).max(1)
}

/// What the canvas was asked to do, printed once a second when `CHEAP_NOTE_TRACE` is set.
///
/// Two numbers, and the question they answer: *which* of the two presents is running per reading. The
/// base costs a whole window and must happen once per scene; the stroke's layer costs a few hundred
/// kilobytes and happens once per reading. A base count in the hundreds per second is a scene key that
/// changes when nothing did.
mod count {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    static BASE: AtomicU64 = AtomicU64::new(0);
    static STROKE: AtomicU64 = AtomicU64::new(0);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    pub fn base() {
        BASE.fetch_add(1, Ordering::Relaxed);
        report();
    }

    pub fn stroke() {
        STROKE.fetch_add(1, Ordering::Relaxed);
        report();
    }

    fn report() {
        let mut last = LAST.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();

        if last.is_some_and(|at| now.duration_since(at).as_millis() < 1000) {
            return;
        }

        *last = Some(now);

        let base = BASE.swap(0, Ordering::Relaxed);
        let stroke = STROKE.swap(0, Ordering::Relaxed);

        println!("canvas: {base} base/s · {stroke} stroke/s");
    }
}

/// The graphics-state stack, for the two calls the crate does not wrap.
fn save(context: &CGContext) {
    // SAFETY: a live context, from the caller.
    unsafe { CGContextSaveGState(context.as_ptr().cast()) }
}

fn restore(context: &CGContext) {
    // SAFETY: a live context, and one save per restore.
    unsafe { CGContextRestoreGState(context.as_ptr().cast()) }
}


impl InkSurface for InkCanvas {
    /// Draws one description of the canvas.
    ///
    /// Called twice in a frame's life: once per pen batch, by the pump, and once per UI frame, by the
    /// view. The second is why the first thing here is a comparison — a frame that changed nothing costs
    /// one [`Scene`] comparison and returns, instead of repainting a page nobody touched.
    fn draw(&mut self, canvas: &Canvas) -> Result<(), String> {
        let scale = if canvas.scale > 0.0 {
            f64::from(canvas.scale)
        } else {
            1.0
        };

        let resized = self.resize(scale);

        // Nothing has described a canvas yet: a reading that arrived before the first frame.
        if canvas.sheet.is_none() {
            return Ok(());
        }

        let scene = Scene::of(canvas);

        if resized || self.scene.as_ref() != Some(&scene) {
            self.paint_base(canvas, scale);
            self.base_image = self.base.image();
            self.copy_all();
            self.present();

            if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
                count::base();
                dump(&mut self.shown, "canvas");
            }
            self.scene = Some(scene);
            self.open = None;
            self.open_key = None;
        }

        // The stroke under the pen, drawn into the same bitmap as the page: one mapping, one size, so a
        // point of the canvas is the same pixel in both and the stroke lands where the nib was. What the
        // last reading covered is put back from the base first, so the stroke is redrawn and not smeared.
        let key = canvas.ink.open.as_ref().map(|stroke| open_key(stroke));

        if self.open_key != key {
            let mapping = Mapping::of(canvas);
            let open = canvas
                .ink
                .open
                .as_ref()
                .and_then(|stroke| stroke_box(stroke, &mapping));

            if let Some(damaged) = damaged(&[self.open, open]) {
                self.restore(damaged);

                if let Some(stroke) = &canvas.ink.open {
                    fill_stroke(
                        &self.shown.context,
                        stroke,
                        &mapping,
                        (0.0, 0.0),
                        ink_colour(stroke.color),
                    );
                }
            }

            self.open = open;
            self.open_key = key;
            self.present();

            if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
                count::stroke();
            }
        }

        Ok(())
    }
}

impl InkCanvas {
    /// Paints everything that does not change while a stroke is being drawn: the desk, the page's shadow,
    /// the sheet, the ruling, the document's page, the finished ink, and the marks a gesture has left.
    fn paint_base(&mut self, canvas: &Canvas, scale: f64) {
        let page = canvas
            .page
            .as_ref()
            .and_then(|page| page_image(&mut self.page, &page.image).map(|image| (image, page.rect)));

        let context = &self.base.context;
        let (width, height) = (self.base.width as f64, self.base.height as f64);

        context.set_should_antialias(true);

        // The desk, which is what the window shows where nothing else covers it.
        let (r, g, b, a) = rgba(canvas.desk);
        context.set_rgb_fill_color(r, g, b, a);
        context.fill_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(width, height),
        ));

        // The shadow's steps, the sheet and the ruling — in the order they are painted.
        for mark in canvas.fills.iter().chain(canvas.rules.iter()) {
            if mark.rect.is_empty() {
                continue;
            }

            let (r, g, b, a) = rgba(mark.colour);
            context.set_rgb_fill_color(r, g, b, a);
            context.fill_rect(device_rect(mark.rect, scale));
        }

        if let Some((image, rect)) = page {
            draw_image(context, device_rect(rect, scale), image);
        }

        let mapping = Mapping::of(canvas);
        let ink = &canvas.ink;
        let in_hand = |index: usize| ink.selected.get(index).copied().unwrap_or(false);

        // Everything not in hand, where it was written.
        for (index, stroke) in ink.strokes.iter().enumerate() {
            if in_hand(index) || !stroke.visible_in(ink.visible) {
                continue;
            }

            fill_stroke(
                context,
                stroke,
                &mapping,
                (0.0, 0.0),
                ink_colour(stroke.color),
            );
        }

        // The ink in hand, where a drag has taken it, and once more in the selection's colour.
        if ink.selection.is_some() || ink.offset != (0.0, 0.0) {
            let culled = [
                ink.visible[0] - ink.offset.0,
                ink.visible[1] - ink.offset.1,
                ink.visible[2] - ink.offset.0,
                ink.visible[3] - ink.offset.1,
            ];

            for (index, stroke) in ink.strokes.iter().enumerate() {
                if !in_hand(index) || !stroke.visible_in(culled) {
                    continue;
                }

                fill_stroke(context, stroke, &mapping, ink.offset, ink_colour(stroke.color));

                if let Some(colour) = ink.selection {
                    fill_stroke(context, stroke, &mapping, ink.offset, colour);
                }
            }
        }

        // The text a highlighter has hold of, before the lasso's loop so that the mark being swept stays
        // on top of everything.
        if let Some((colour, bands)) = &ink.highlight {
            for band in bands {
                fill_band(context, *band, &mapping, *colour);
            }
        }

        if let Some(stroke) = &ink.lasso {
            let colour = ink.selection.unwrap_or_else(|| ink_colour(stroke.color));
            fill_stroke(context, stroke, &mapping, (0.0, 0.0), colour);
        }
    }
}

/// Where a point of the paper is drawn, in device pixels: the canvas's own mapping, scaled.
///
/// The same calls to [`drawn_of_paper`] the Windows renderer builds its transform from, so a turned page
/// goes the same way on both machines.
struct Mapping {
    origin: (f32, f32),
    zoom: f32,
    scale: f32,
    base: (f32, f32),
    step_x: (f32, f32),
    step_y: (f32, f32),
}

impl Mapping {
    fn of(canvas: &Canvas) -> Self {
        let paper = canvas.ink.paper;
        let turns = canvas.ink.rotation;
        let base = drawn_of_paper((0.0, 0.0), paper, turns);
        let along_x = drawn_of_paper((1.0, 0.0), paper, turns);
        let along_y = drawn_of_paper((0.0, 1.0), paper, turns);

        Mapping {
            origin: canvas.ink.origin,
            zoom: canvas.ink.zoom,
            scale: canvas.scale,
            base,
            step_x: (along_x.0 - base.0, along_x.1 - base.1),
            step_y: (along_y.0 - base.0, along_y.1 - base.1),
        }
    }

    fn point(&self, [x, y]: [f32; 2], shift: (f32, f32)) -> CGPoint {
        let (x, y) = (x + shift.0, y + shift.1);
        let across = self.base.0 + x * self.step_x.0 + y * self.step_y.0;
        let down = self.base.1 + x * self.step_x.1 + y * self.step_y.1;

        CGPoint::new(
            f64::from((self.origin.0 + across * self.zoom) * self.scale),
            f64::from((self.origin.1 + down * self.zoom) * self.scale),
        )
    }
}

/// The colour Core Graphics takes: red, green, blue and alpha in `0.0..=1.0`.
fn rgba(colour: gpui_kit::Hsla) -> (CGFloat, CGFloat, CGFloat, CGFloat) {
    let rgba = gpui_kit::Rgba::from(colour);
    (
        f64::from(rgba.r),
        f64::from(rgba.g),
        f64::from(rgba.b),
        f64::from(rgba.a),
    )
}

/// A canvas rectangle in device pixels.
fn device_rect(rect: Rect, scale: f64) -> CGRect {
    CGRect::new(
        &CGPoint::new(rect.x as f64 * scale, rect.y as f64 * scale),
        &CGSize::new(rect.width as f64 * scale, rect.height as f64 * scale),
    )
}

/// Draws an image the right way up in a context whose y grows downwards.
fn draw_image(context: &CGContext, rect: CGRect, image: &CGImage) {
    save(context);
    context.concat_ctm(CGAffineTransform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: -1.0,
        tx: rect.origin.x,
        ty: rect.origin.y + rect.size.height,
    });
    context.draw_image(CGRect::new(&CGPoint::new(0.0, 0.0), &rect.size), image);
    restore(context);
}

/// A stroke's ribbon as one filled path.
fn fill_stroke(
    context: &CGContext,
    stroke: &note_format::ink::Stroke,
    mapping: &Mapping,
    shift: (f32, f32),
    colour: gpui_kit::Hsla,
) {
    if stroke.outline.len() < 3 {
        return;
    }

    let (r, g, b, a) = rgba(colour);
    context.set_rgb_fill_color(r, g, b, a);
    context.begin_path();

    let first = mapping.point(stroke.outline[0], shift);
    context.move_to_point(first.x, first.y);

    for vertex in &stroke.outline[1..] {
        let point = mapping.point(*vertex, shift);
        context.add_line_to_point(point.x, point.y);
    }

    context.close_path();
    context.fill_path();
}

/// One band of a highlighter's preview, through the same mapping — so a band on a turned page is turned
/// with it.
fn fill_band(context: &CGContext, band: Rect, mapping: &Mapping, colour: gpui_kit::Hsla) {
    if band.is_empty() {
        return;
    }

    let (r, g, b, a) = rgba(colour);
    context.set_rgb_fill_color(r, g, b, a);
    context.begin_path();

    let corners = [
        [band.x, band.y],
        [band.x + band.width, band.y],
        [band.x + band.width, band.y + band.height],
        [band.x, band.y + band.height],
    ];

    let first = mapping.point(corners[0], (0.0, 0.0));
    context.move_to_point(first.x, first.y);

    for corner in &corners[1..] {
        let point = mapping.point(*corner, (0.0, 0.0));
        context.add_line_to_point(point.x, point.y);
    }

    context.close_path();
    context.fill_path();
}

/// The box a stroke covers on screen, in device pixels: all four corners of its bounds, mapped, then made
/// a rectangle — because a turned page turns the box with it.
fn stroke_box(stroke: &note_format::ink::Stroke, mapping: &Mapping) -> Option<CGRect> {
    let [min_x, min_y, max_x, max_y] = stroke.bounds;
    if !min_x.is_finite() || !min_y.is_finite() {
        return None;
    }

    let corners = [
        [min_x, min_y],
        [max_x, min_y],
        [max_x, max_y],
        [min_x, max_y],
    ];

    let mut points = corners.iter().map(|corner| mapping.point(*corner, (0.0, 0.0)));
    let first = points.next()?;
    let (mut left, mut top, mut right, mut bottom) = (first.x, first.y, first.x, first.y);

    for point in points {
        left = left.min(point.x);
        top = top.min(point.y);
        right = right.max(point.x);
        bottom = bottom.max(point.y);
    }

    Some(CGRect::new(
        &CGPoint::new(left, top),
        &CGSize::new((right - left).max(1.0), (bottom - top).max(1.0)),
    ))
}

/// What identifies an open stroke between two calls.
///
/// The app rebuilds the stroke's `Arc` on every frame, so its address says nothing; the number of points
/// it holds and the box they cover do, and a stroke that grew is a stroke whose count changed.
fn open_key(stroke: &note_format::ink::Stroke) -> (usize, usize, [u32; 4]) {
    (
        stroke.points.len(),
        stroke.outline.len(),
        stroke.bounds.map(f32::to_bits),
    )
}

/// What has to be put back before the stroke is drawn again: the boxes it covered before
/// and the ones they cover now, with a pixel of slack for the antialiased edge.
fn damaged(boxes: &[Option<CGRect>]) -> Option<CGRect> {
    let region = boxes.iter().flatten().copied().reduce(union)?;

    Some(CGRect::new(
        &CGPoint::new(region.origin.x - 1.0, region.origin.y - 1.0),
        &CGSize::new(region.size.width + 2.0, region.size.height + 2.0),
    ))
}

fn union(a: CGRect, b: CGRect) -> CGRect {
    let left = a.origin.x.min(b.origin.x);
    let top = a.origin.y.min(b.origin.y);
    let right = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);

    CGRect::new(
        &CGPoint::new(left, top),
        &CGSize::new(right - left, bottom - top),
    )
}


/// The page's pixels as Core Graphics sees them, prepared once per rasterised page.
///
/// The bytes are the view's own (`RenderImage` is BGRA, top-down) and are copied once, here: an image that
/// borrowed them would have to keep the app's buffer alive, and a page is rasterised once per zoom rather
/// than once per frame.
fn page_image<'a>(
    cache: &'a mut Option<(usize, CGImage)>,
    image: &Arc<gpui_kit::RenderImage>,
) -> Option<&'a CGImage> {
    let key = Arc::as_ptr(image) as usize;

    if cache.as_ref().map(|(cached, _)| *cached) != Some(key) {
        let bytes = image.as_bytes(0)?;
        let size = image.size(0);
        let (width, height) = (size.width.0 as usize, size.height.0 as usize);

        if width == 0 || height == 0 {
            return None;
        }

        let provider = CGDataProvider::from_buffer(Arc::new(bytes.to_vec()));
        let space = CGColorSpace::create_device_rgb();
        let prepared = CGImage::new(
            width, height, 8, 32, width * 4, &space, BGRA_OPAQUE, &provider, true, 0,
        );

        *cache = Some((key, prepared));
    }

    cache.as_ref().map(|(_, prepared)| prepared)
}

/// What the base bitmap was painted from.
///
/// A frame's own call to `draw` describes the same canvas as the one before it, and repainting a page of
/// handwriting because a frame happened is the cost this module exists to remove. So the description is
/// compared instead of repainted: the sheet, the fills' count and paper colour, the ruling's count, the
/// page's identity, the finished strokes' identity and revision, and where the paper is drawn.
#[derive(Clone, PartialEq)]
struct Scene {
    sheet: Option<Rect>,
    fills: usize,
    paper: u32,
    rules: usize,
    page: Option<usize>,
    strokes: usize,
    revision: u64,
    zoom: u32,
    origin: (u32, u32),
    rotation: u8,
    offset: (u32, u32),
    selection: Option<u32>,
    marks: (bool, bool),
}

impl Scene {
    fn of(canvas: &Canvas) -> Self {
        Scene {
            sheet: canvas.sheet,
            fills: canvas.fills.len(),
            paper: canvas.fills.first().map_or(0, |fill| fill.colour.h.to_bits()),
            rules: canvas.rules.len(),
            page: canvas
                .page
                .as_ref()
                .map(|page| Arc::as_ptr(&page.image) as usize),
            strokes: Arc::as_ptr(&canvas.ink.strokes) as usize,
            revision: canvas.ink.revision,
            zoom: canvas.ink.zoom.to_bits(),
            origin: (canvas.ink.origin.0.to_bits(), canvas.ink.origin.1.to_bits()),
            rotation: canvas.ink.rotation,
            offset: (canvas.ink.offset.0.to_bits(), canvas.ink.offset.1.to_bits()),
            selection: canvas.ink.selection.map(|colour| colour.a.to_bits()),
            marks: (canvas.ink.highlight.is_some(), canvas.ink.lasso.is_some()),
        }
    }
}

/// Writes a bitmap to `/tmp/cheap-note-<name>.png`, three times at most, when `CHEAP_NOTE_TRACE` is set.
///
/// A screen-recording permission is not something this app asks for, and the canvas is the thing that has
/// to be right — so the bitmap itself is the evidence, and a PNG of it can be looked at.
fn dump(bitmap: &mut Bitmap, name: &str) {
    use std::sync::atomic::{AtomicU8, Ordering};

    static DUMPS: AtomicU8 = AtomicU8::new(0);

    if DUMPS.fetch_add(1, Ordering::Relaxed) >= 3 {
        return;
    }

    let path = format!("/tmp/cheap-note-{name}.png");
    let (width, height) = (bitmap.width, bitmap.height);
    let data = bitmap.context.data();

    let Ok(file) = File::create(&path) else {
        return;
    };

    let mut encoder = png::Encoder::new(BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);

    let Ok(mut writer) = encoder.write_header() else {
        return;
    };

    // BGRA in memory, RGBA in a PNG.
    let mut rgba = Vec::with_capacity(width * height * 4);
    for pixel in data.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }

    if writer.write_image_data(&rgba).is_ok() {
        println!("trace: canvas written to {path}");
    }
}
