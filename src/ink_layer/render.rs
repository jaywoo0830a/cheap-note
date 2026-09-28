//! What draws the canvas: Direct2D, bound to the swap chain's current buffer.
//!
//! ## Why Direct2D rather than Direct3D's own drawing calls
//!
//! The ink is a filled polygon per stroke — a ribbon whose width changes along the line — and the
//! work that makes one is *tessellation*: turning an outline into triangles, with antialiasing at
//! the edges. Direct3D offers no such thing, so a renderer built on it directly has to tessellate
//! the outlines itself, keep its own vertex buffers, and run its own multisampled pass. Direct2D
//! does all of that, and — the part that matters here — it can keep the result: a *realization* is
//! the shape an outline was flattened and filled into, held on the device, and drawing one outlines
//! nothing again (see [`Renderer::realization_for`]).
//!
//! GPUI's own renderer does the other thing: lyon tessellates every path into the scene *every
//! frame*, and the geometry is thrown away when the frame ends. A page of handwriting held ~33 fps
//! with `paint` at 9.3 ms because of it, and a cached layer moved `paint` to 0.00 without moving the
//! frame rate, because the engine replays whatever the cache hands it. Here a stroke is turned into
//! geometry once — when it is closed — and baked once: measured on this app, a page of 29 strokes
//! rendered in 1.69 ms and one of 316 in 7.87, which is 21 µs a stroke, and every one of those
//! microseconds was a path geometry being outlined all over again.
//!
//! ## What a stroke's geometry is kept under
//!
//! A stroke's outline is in the paper's own units, which is what lets the canvas pan and zoom
//! without rebuilding anything: the transform moves the geometry and the numbers stay as they were.
//! Only two things invalidate it — a stroke the layer has not seen, and the app's `revision`, which
//! counts the rebuilds the *detail* of a zoom forces. Appending a stroke builds one geometry; a zoom
//! crossing into another detail rung rebuilds all of them, which is the same handful of rebuilds
//! the ink model itself does (see `InkDocument::set_zoom`).
//!
//! ## Dots per inch, and why they are pinned
//!
//! A Direct2D context scales every coordinate by its own DPI, and its default is the desktop's: on a
//! 150% display, a rectangle asked for in physical pixels would be drawn one and a half times too
//! large. This canvas works in the surface's own pixels and nothing else, so the context is pinned
//! to 96 — one unit, one pixel — and the window's own scale is a number the canvas carries.

use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{Context, Result};
use gpui_kit::{rgb, rgba, Hsla, ImageId, Rgba};
// `cast`: asking a context for the later interface whose method bakes a geometry into a realization.
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_END_CLOSED, D2D1_FILL_MODE_WINDING, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::ID2D1GeometrySink;
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1DeviceContext1,
    ID2D1Factory1, ID2D1Geometry, ID2D1GeometryRealization, ID2D1Image, ID2D1PathGeometry1,
    ID2D1SolidColorBrush, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows_numerics::{Matrix3x2, Vector2};

use crate::ink::Stroke;
use crate::ink_layer::canvas::{Canvas, Fill, Page, Rect};
use crate::pages::Quarters;

/// One unit in one pixel, whatever the display's own scale is (see the module docs).
const DOTS_PER_INCH: f32 = 96.0;

/// Direct2D's own default flattening tolerance, in the geometry's units.
///
/// `D2D1_DEFAULT_FLATTENING_TOLERANCE`, which the headers define and the bindings do not: it is the
/// coarsest a curve may be turned into straight edges without the difference being visible, and the
/// smallest tolerance `CreateFilledGeometryRealization` documents taking (see
/// [`flattening_tolerance`]).
const DEFAULT_FLATTENING: f32 = 0.25;

/// How many bytes one pixel of a page takes: BGRA.
const BYTES_PER_PIXEL: u32 = 4;

/// A Direct2D device context on the canvas's device, the brush its shapes are filled with, and the
/// geometry that is kept between frames.
pub(crate) struct Renderer {
    factory: ID2D1Factory1,
    context: ID2D1DeviceContext,
    /// One brush, recoloured before each fill: a page's shadow is three rectangles that differ in
    /// nothing but their colour, and a brush per colour would be an object per palette entry.
    brush: ID2D1SolidColorBrush,
    /// The finished strokes' geometry, and what it was built from.
    ink: InkCache,
    /// The same context, asked for the one interface that can bake a geometry into a realization.
    ///
    /// The same object as [`Renderer::context`] — Direct2D's device context answers for 1.1 on any
    /// system this app runs on — but a separate handle, because the method that draws a realization
    /// is declared on the later interface and the ones that place and fill everything else are not.
    context1: ID2D1DeviceContext1,
    /// The document page's bitmap, and the image it was uploaded from.
    page: Option<(ImageId, ID2D1Bitmap1)>,
}

/// The finished strokes' geometry, index-aligned with the app's list of them.
#[derive(Default)]
struct InkCache {
    /// Each cached stroke, by identity — the address of its `Arc` — and the revision of every
    /// outline it was built at.
    identities: Vec<usize>,
    revision: u64,
    /// One baked geometry per stroke: the outline flattened, filled and antialiased *once*, at the
    /// zoom it was built for (see [`Renderer::realization_for`]).
    realizations: Vec<ID2D1GeometryRealization>,
}

impl Renderer {
    /// A renderer for the canvas's device.
    pub(crate) fn new(dxgi_device: &IDXGIDevice) -> Result<Self> {
        let factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }
                .context("creating a Direct2D factory")?;
        let device: ID2D1Device =
            unsafe { factory.CreateDevice(dxgi_device) }.context("creating a Direct2D device")?;
        let context = unsafe { device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }
            .context("creating a Direct2D context")?;

        unsafe {
            context.SetDpi(DOTS_PER_INCH, DOTS_PER_INCH);
            context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }

        let brush = unsafe { context.CreateSolidColorBrush(&colour_of(Hsla::default()), None) }
            .context("creating a brush")?;
        // A second handle rather than a second object: what a realization needs is a method on
        // Direct2D 1.1, and everything else this renderer calls is on the interface it already has.
        let context1: ID2D1DeviceContext1 = context
            .cast()
            .context("asking the context for Direct2D 1.1")?;

        Ok(Renderer {
            factory,
            context,
            brush,
            ink: InkCache::default(),
            context1,
            page: None,
        })
    }

    /// Draws a frame into the buffer the surface is.
    ///
    /// The target is made for this buffer and released again at the end of the frame, rather than
    /// kept: a swap chain with two buffers hands over a different one each time, and a Direct2D
    /// target that outlives its buffer is the reason DXGI refuses to resize.
    pub(crate) fn draw(&mut self, buffer: IDXGISurface, canvas: &Canvas) -> Result<()> {
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: DOTS_PER_INCH,
            dpiY: DOTS_PER_INCH,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };

        let target: ID2D1Bitmap1 = unsafe {
            self.context
                .CreateBitmapFromDxgiSurface(&buffer, Some(&properties))
        }
        .context("binding the swap chain's buffer")?;

        // The page's pixels are uploaded before the batch opens: an upload is work on the device,
        // and doing it between `BeginDraw` and `EndDraw` would be work inside a recording.
        let page = match &canvas.page {
            Some(page) => self.page_bitmap(page)?,
            None => None,
        };

        // Between `BeginDraw` and `EndDraw` Direct2D is a recorder: the calls below add to a batch
        // and report nothing, and the frame's own result is the one `EndDraw` answers with. That is
        // the model this renderer is built on, so the per-call results are deliberately not read.
        unsafe {
            self.context.SetTarget(&target);
            self.context.BeginDraw();

            // Nothing but the ink is drawn through a transform (see [`Renderer::draw_ink`]): the
            // desk, the sheet, the ruling and the page are placed in the surface's own pixels, and
            // `rect_of` is the one place a canvas coordinate becomes a surface one. Scaling them
            // here *as well* would place the sheet at its origin times the scale squared — which is
            // a sheet pushed off centre, further the larger the scale and the further from the
            // window's corner the paper sits, and ink that no longer lands on it.
            //
            // Set rather than assumed: a context keeps its transform, so this clears the one the
            // frame before left.
            self.context.SetTransform(&Matrix3x2::identity());
            self.context
                .Clear(Some(std::ptr::from_ref(&colour_of(canvas.desk))));

            for fill in canvas.fills.iter().chain(canvas.rules.iter()) {
                self.fill(fill, canvas.scale);
            }

            if let (Some(page), Some(bitmap)) = (&canvas.page, page) {
                self.context.DrawBitmap(
                    &bitmap,
                    Some(std::ptr::from_ref(&rect_of(page.rect, canvas.scale))),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            }

            // The ink is clipped to the sheet, which is the display half of the rule the ink model
            // enforces: a reading off the paper is not ink, so ink off the paper is not *drawn*
            // either. The clip is pushed before the ink's transform is set, and so is in the
            // surface's own pixels like everything else here.
            if let Some(sheet) = canvas.sheet {
                self.context.PushAxisAlignedClip(
                    std::ptr::from_ref(&rect_of(sheet, canvas.scale)),
                    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                );
                self.draw_ink(canvas)?;
                self.context.PopAxisAlignedClip();
            } else {
                self.draw_ink(canvas)?;
            }
        }

        let result = unsafe { self.context.EndDraw(None, None) };

        // The target goes before the frame ends, whatever the frame did: the next one is a
        // different buffer, and the buffer this one holds cannot be resized while it is bound.
        unsafe {
            self.context.SetTarget(None::<&ID2D1Image>);
        }

        result.context("drawing the canvas")
    }

    /// Fills one rectangle, with the brush set to its colour first.
    fn fill(&self, fill: &Fill, scale: f32) {
        unsafe {
            self.brush.SetColor(&colour_of(fill.colour));

            let rect = rect_of(fill.rect, scale);

            // A dot of a dot grid is a square whose corners are rounded by half its side, which is
            // what a circle is: the ruling is built as rectangles long before it reaches here, and
            // this is the one shape Direct2D needs told about.
            if fill.radius > 0.0 {
                let radius = fill.radius * scale;
                self.context.FillRoundedRectangle(
                    std::ptr::from_ref(&D2D1_ROUNDED_RECT {
                        rect,
                        radiusX: radius,
                        radiusY: radius,
                    }),
                    &self.brush,
                );
            } else {
                self.context
                    .FillRectangle(std::ptr::from_ref(&rect), &self.brush);
            }
        }
    }
}

impl Renderer {
    /// Draws the ink: the finished strokes, then the one under the pen.
    ///
    /// A finished stroke is a realization — a shape its outline was flattened and filled into once —
    /// so the device draws it from a cache rather than outlining it again (see
    /// [`Renderer::realization_for`]). The stroke being written is still moving, so it is a plain
    /// path geometry built for this frame; it is one stroke, and the frame after it will not be.
    fn draw_ink(&mut self, canvas: &Canvas) -> Result<()> {
        self.refresh_ink(canvas)?;

        // What is off the sheet's visible part is skipped, not drawn: it is the same test the app counts
        // its `culled` figure with, so the number on the status line is what happened, and it is what makes
        // zooming in on a page of handwriting cheap (see [`Stroke::visible_in`]).
        let visible = canvas.ink.visible;
        let selected = &canvas.ink.selected;
        let in_hand = |index: usize| selected.get(index).copied().unwrap_or(false);
        let offset = canvas.ink.offset;
        let drawn = canvas.scale * canvas.ink.zoom;

        unsafe {
            self.context.SetTransform(&ink_transform(canvas));

            for (index, (stroke, realization)) in canvas
                .ink
                .strokes
                .iter()
                .zip(self.ink.realizations.iter())
                .enumerate()
            {
                // The ink in hand is drawn in the pass below, where the drag has taken it: drawing it here
                // as well would leave a ghost of it behind at every step of a drag.
                if in_hand(index) || !stroke.visible_in(visible) {
                    continue;
                }

                self.brush.SetColor(&colour_of(ink_colour(stroke.color)));
                self.context1
                    .DrawGeometryRealization(realization, &self.brush);
            }

            // The ink *in hand*, where a drag has taken it, and once more in the selection's colour. The same
            // realization twice rather than a second geometry: a highlight built as its own outline would be
            // another shape to keep in step with the ink it is meant to be showing.
            if canvas.ink.selection.is_some() || offset != (0.0, 0.0) {
                self.context
                    .SetTransform(&shifted(ink_transform(canvas), offset, drawn));

                // The window, in the coordinates the *drawing* is in: a drag moves the ink, so the part of
                // the paper that is on screen is that much the other way when the ink is asked about itself.
                let culled = [
                    visible[0] - offset.0,
                    visible[1] - offset.1,
                    visible[2] - offset.0,
                    visible[3] - offset.1,
                ];

                for (index, (stroke, realization)) in canvas
                    .ink
                    .strokes
                    .iter()
                    .zip(self.ink.realizations.iter())
                    .enumerate()
                {
                    if !in_hand(index) || !stroke.visible_in(culled) {
                        continue;
                    }

                    self.brush.SetColor(&colour_of(ink_colour(stroke.color)));
                    self.context1
                        .DrawGeometryRealization(realization, &self.brush);

                    if let Some(colour) = canvas.ink.selection {
                        self.brush.SetColor(&colour_of(colour));
                        self.context1
                            .DrawGeometryRealization(realization, &self.brush);
                    }
                }
            }

            if let Some(stroke) = &canvas.ink.open {
                let outline = self.outline_geometry(stroke)?;
                // The live stroke is drawn in the colour it will keep: a highlighter marks as it goes, with the
                // page showing through it, which is exactly what the reader is deciding about.
                self.brush.SetColor(&colour_of(ink_colour(stroke.color)));
                self.context.FillGeometry(&outline, &self.brush, None);
            }

            // The text a highlighter has hold of, drawn through the ink's own transform because that is where the
            // text is: the bands are in the paper's units, exactly as the strokes they are about to become. Drawn
            // before the lasso's loop, so that the mark being swept stays on top of everything.
            if let Some((colour, bands)) = &canvas.ink.highlight {
                self.context.SetTransform(&ink_transform(canvas));
                self.brush.SetColor(&colour_of(*colour));

                for band in bands {
                    if band.is_empty() {
                        continue;
                    }

                    self.context
                        .FillRectangle(&rect_of(*band, 1.0), &self.brush);
                }
            }

            // And the loop a lasso is sweeping, over everything: it is the mark the reader is making *now*,
            // and the one thing on the sheet that has to be visible over the ink it is choosing between.
            if let Some(loop_stroke) = &canvas.ink.lasso {
                self.context.SetTransform(&ink_transform(canvas));
                let outline = self.outline_geometry(loop_stroke)?;
                let colour = canvas
                    .ink
                    .selection
                    .unwrap_or_else(|| ink_colour(loop_stroke.color));

                self.brush.SetColor(&colour_of(colour));
                self.context.FillGeometry(&outline, &self.brush, None);
            }

            // The transform is the ink's alone and does not outlive this call: a context keeps it,
            // and the next frame's sheet is placed in the surface's own pixels.
            self.context.SetTransform(&Matrix3x2::identity());
        }

        Ok(())
    }

    /// Brings the cached geometry up to date with the app's strokes.
    ///
    /// The zoom travels with the strokes because a realization is flattened for a size, not for a
    /// shape: every outline it is built from was rebuilt when the zoom crossed into another detail
    /// rung, and the flattening has to be as well (see [`Renderer::realization_for`]).
    fn refresh_ink(&mut self, canvas: &Canvas) -> Result<()> {
        let strokes = &canvas.ink.strokes;

        let keep = keep(
            &self.ink.identities,
            strokes.iter().map(identity),
            self.ink.revision,
            canvas.ink.revision,
        );

        self.ink.identities.truncate(keep);
        self.ink.realizations.truncate(keep);
        self.ink.revision = canvas.ink.revision;

        for stroke in strokes.iter().skip(keep) {
            let realization = self.realization_for(stroke, canvas.ink.zoom)?;
            self.ink.identities.push(identity(stroke));
            self.ink.realizations.push(realization);
        }

        Ok(())
    }

    /// A stroke's ribbon outline as Direct2D geometry: the shape a realization is made from, and the
    /// shape the stroke under the pen is filled from, frame by frame.
    ///
    /// Filled with the **winding** rule, which is what a pen does: an outline that crosses itself —
    /// a loop, a sharp turn, a scribble over its own line — fills everything it winds around, where
    /// the even-odd rule would leave a hole at every crossing. The frame's own renderer set the same
    /// rule on lyon for the same reason, and the measured symptom of getting it wrong was 3,145
    /// pixels of paper inside the ink, in stripes. A realization keeps it: the fill rule is the
    /// geometry's, and the API that bakes one takes no other.
    fn outline_geometry(&self, stroke: &Stroke) -> Result<ID2D1PathGeometry1> {
        let geometry =
            unsafe { self.factory.CreatePathGeometry() }.context("creating a stroke's geometry")?;

        let Some((first, rest)) = stroke.outline.split_first() else {
            return Ok(geometry);
        };

        let sink: ID2D1GeometrySink =
            unsafe { geometry.Open() }.context("opening a stroke's geometry")?;
        let points: Vec<Vector2> = rest.iter().map(|point| point_of(*point)).collect();

        // The sink's own calls report nothing to this renderer: Direct2D answers for a geometry when
        // it is drawn, not while it is being built (see [`Renderer::draw`]).
        unsafe {
            sink.SetFillMode(D2D1_FILL_MODE_WINDING);
            sink.BeginFigure(point_of(*first), D2D1_FIGURE_BEGIN_FILLED);
            if !points.is_empty() {
                sink.AddLines(&points);
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            // The last call of the four, and the one with a result worth keeping: a sink that could
            // not be closed left the geometry unfinished, and it would be drawn as what it has.
            sink.Close().ok();
        }

        Ok(geometry)
    }

    /// A stroke baked into a shape the device keeps triangles for.
    ///
    /// A path geometry is not that shape: filling one flattens every curve and triangulates the fill
    /// on the way to the frame, so a page of handwriting pays for all of its outlines again every
    /// time the screen refreshes. That is the last thing this canvas's whole design was for, and the
    /// measurements say so — a page of 29 strokes rendered in 1.69 ms and a page of 316 in 7.87,
    /// which is 21 µs a stroke, and nothing in this renderer touches a stroke per frame but this.
    /// A realization is what Direct2D 1.1 offers for static geometry: the flattening, the
    /// triangulation and the antialiasing done once, kept on the device, and drawn again and again.
    ///
    /// It is flattened for a *size*, not for a shape (see [`flattening_tolerance`]), which is why
    /// one is built per stroke per detail rung rather than per stroke: the outlines it came from are
    /// the rung's too, and both are rebuilt together when the zoom crosses into another (see
    /// [`Renderer::refresh_ink`]).
    fn realization_for(&self, stroke: &Stroke, zoom: f32) -> Result<ID2D1GeometryRealization> {
        let outline = self.outline_geometry(stroke)?;
        // The geometry as the interface every geometry has: a realization is made *from* a geometry,
        // whatever kind it was built as.
        let geometry: ID2D1Geometry = outline.cast().context("a stroke's geometry, as geometry")?;

        unsafe {
            self.context1
                .CreateFilledGeometryRealization(&geometry, flattening_tolerance(zoom))
        }
        .context("baking a stroke's geometry")
    }

    /// Lets go of the frame's target, so that the swap chain can be resized.
    ///
    /// DXGI will not resize a chain while anything holds its back buffers, and a device context that
    /// has drawn into one is holding it (see `IDXGISwapChain::ResizeBuffers`). The next frame binds a
    /// target of its own, which is a frame the resize could not have been drawn into (see
    /// [`Device::resize`]).
    pub(crate) fn release_target(&self) {
        unsafe { self.context.SetTarget(None) };
    }

    /// The document page's bitmap, uploaded when the image it comes from changes.
    ///
    /// Kept rather than made per frame: a page's pixels are tens of megabytes, and the point of the
    /// app's render cache is that they are made when the zoom asks for another width and not once
    /// per frame (see [`crate::pdf`]).
    fn page_bitmap(&mut self, page: &Page) -> Result<Option<ID2D1Bitmap1>> {
        if let Some((id, bitmap)) = &self.page {
            if *id == page.image.id {
                return Ok(Some(bitmap.clone()));
            }
        }

        let Some(pixels) = page.image.as_bytes(0) else {
            return Ok(None);
        };

        let size = page.image.size(0);
        let (width, height) = (size.width.0.max(0) as u32, size.height.0.max(0) as u32);

        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                // Straight alpha rather than premultiplied: Pdfium hands over pixels whose
                // components are independent of the alpha (see [`crate::pdfium`]), and a page is
                // opaque in any case.
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: DOTS_PER_INCH,
            dpiY: DOTS_PER_INCH,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            ..Default::default()
        };

        let bitmap = unsafe {
            self.context.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(pixels.as_ptr() as *const c_void),
                width * BYTES_PER_PIXEL,
                &properties,
            )
        }
        .context("uploading the page")?;

        self.page = Some((page.image.id, bitmap.clone()));

        Ok(Some(bitmap))
    }
}

/// A stroke's identity for the geometry cache: the address of its own `Arc`.
///
/// An address rather than an index, because an index is what changes when a stroke is inserted or
/// removed and an address is what stays: a stroke the layer has built geometry for is the *same*
/// stroke until the app hands it a different one.
fn identity(stroke: &Arc<Stroke>) -> usize {
    Arc::as_ptr(stroke) as usize
}

/// How many of the cached geometries are still the right ones.
///
/// A pure function of the identities, in order, and of the two revisions — which is what makes
/// "appending a stroke builds one geometry and nothing else" a fact that a test can pin rather than
/// a hope about a loop (see [`Renderer::refresh_ink`]). Everything after the count it returns is
/// built again: a stroke that changed, or every stroke, when the outlines themselves were rebuilt
/// at another detail rung.
fn keep(
    cached: &[usize],
    now: impl Iterator<Item = usize>,
    revision: u64,
    revision_now: u64,
) -> usize {
    if revision != revision_now {
        return 0;
    }

    cached
        .iter()
        .zip(now)
        .take_while(|(cached, now)| *cached == now)
        .count()
}

/// How coarsely a stroke's curves may be turned into straight edges, in the paper's own units.
///
/// What the screen wants is a fraction of a *pixel* of error, and a geometry's numbers are in paper
/// units, so the two are one zoom apart: at 1:1 Direct2D's own default — a quarter of a unit — is a
/// quarter of a pixel, and the case worth being coarser than that on is the paper drawn *smaller*
/// than its own size, where a paper unit is less than the pixel showing it.
///
/// Finer is not possible: the default is documented as the smallest tolerance
/// `CreateFilledGeometryRealization` takes, so a paper drawn larger than its own size is flattened at
/// the same quarter of a paper unit — a pixel of chord error on a curve at 4x, which is where the
/// difference stops showing on the width of a pen.
fn flattening_tolerance(zoom: f32) -> f32 {
    let zoom = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    };

    (DEFAULT_FLATTENING / zoom.min(1.0)).max(DEFAULT_FLATTENING)
}

/// Where the paper's own units sit on the surface: how large it is drawn, times the display's scale,
/// where its origin is, and which way up the page is.
///
/// The rotation is the *mapping's* and not any stroke's: the ink never moves when a page is turned, so
/// what changes is where the paper's coordinates land on the screen.
///
/// The matrix is derived from [`crate::ink::drawn_of_paper`] — the paper's origin and its two unit steps,
/// mapped and scaled — rather than written out case by case. It is the same function the pen's readings
/// are turned back through, so the layer and the ink model cannot come to disagree about which way is
/// clockwise; a table here would be a second opinion about that, and the only way it would ever be
/// noticed is ink that sits off the page it was written on.
fn ink_matrix(
    scale: f32,
    zoom: f32,
    origin: (f32, f32),
    paper: (f32, f32),
    turns: Quarters,
) -> Matrix3x2 {
    let drawn = scale * zoom;
    let (ox, oy) = crate::ink::drawn_of_paper((0.0, 0.0), paper, turns);
    let (xx, xy) = crate::ink::drawn_of_paper((1.0, 0.0), paper, turns);
    let (yx, yy) = crate::ink::drawn_of_paper((0.0, 1.0), paper, turns);

    Matrix3x2 {
        M11: (xx - ox) * drawn,
        M12: (xy - oy) * drawn,
        M21: (yx - ox) * drawn,
        M22: (yy - oy) * drawn,
        M31: origin.0 * scale + ox * drawn,
        M32: origin.1 * scale + oy * drawn,
        ..Default::default()
    }
}

/// The same transform, moved by a drag: where the ink in hand is drawn while the reader is taking it
/// somewhere.
///
/// Added to the translation rather than composed as a second matrix: Direct2D's transform is a row-vector
/// one — `x' = x*M11 + y*M21 + M31` — so "the same place, so much further along" is exactly the two
/// `M31`/`M32` fields, and composing would be the same arithmetic with a matrix to allocate.
fn shifted(transform: Matrix3x2, offset: (f32, f32), drawn: f32) -> Matrix3x2 {
    Matrix3x2 {
        M31: transform.M31 + offset.0 * drawn,
        M32: transform.M32 + offset.1 * drawn,
        ..transform
    }
}

/// The transform the ink is drawn through, from what the app described.
fn ink_transform(canvas: &Canvas) -> Matrix3x2 {
    ink_matrix(
        canvas.scale,
        canvas.ink.zoom,
        canvas.ink.origin,
        canvas.ink.paper,
        canvas.ink.rotation,
    )
}

/// A rectangle as Direct2D takes it: two corners, in that order, in the surface's own pixels.
///
/// The scale is applied here — one multiplication per side — rather than as a transform of the
/// surface: the transform is what the ink uses, and this is the one place a canvas coordinate
/// becomes a surface one for everything else.
fn rect_of(rect: Rect, scale: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.x * scale,
        top: rect.y * scale,
        right: (rect.x + rect.width) * scale,
        bottom: (rect.y + rect.height) * scale,
    }
}

/// A point of a stroke's outline, as Direct2D takes one.
fn point_of(point: [f32; 2]) -> Vector2 {
    Vector2 {
        X: point[0],
        Y: point[1],
    }
}

/// A colour as Direct2D takes it.
///
/// The paint colour of a stroke: the colour it was written in, with the alpha it was written with.
///
/// A highlighter is drawn exactly as a pen stroke is — the same ribbon, the same geometry realization, the same
/// brush — and the whole of the difference is here. `rgba` takes `0xRRGGBBAA` while a stroke's colour carries its
/// alpha in the *top* byte (`0xAARRGGBB`), so the two have to be moved to where Direct2D's brush reads them; and a
/// colour with no alpha byte is opaque, which is what every pen stroke is (see [`crate::ink::alpha_of`]).
pub(crate) fn ink_colour(color: u32) -> Hsla {
    let alpha = crate::ink::alpha_of(color);
    if alpha >= 1.0 {
        return rgb(color).into();
    }

    let byte = (alpha * 255.0).round() as u32;
    let (r, g, b) = ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF);

    rgba((r << 24) | (g << 16) | (b << 8) | byte).into()
}

/// Straight (not premultiplied) red, green, blue, alpha in `0..=1`. Not the byte order of a BGRA
/// *pixel* — that is the surface's business, and Direct2D is the one that writes it.
fn colour_of(hsla: Hsla) -> D2D1_COLOR_F {
    let rgba = Rgba::from(hsla);

    D2D1_COLOR_F {
        r: rgba.r,
        g: rgba.g,
        b: rgba.b,
        a: rgba.a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink_layer::canvas::Ink;
    use gpui_kit::rgba;

    /// The colour conversion, against the one mistake worth catching: a channel swap. The surface is
    /// BGRA and Direct2D's colour is not — passing the surface's byte order through here would turn
    /// the desk blue.
    #[test]
    fn a_colour_keeps_its_channels() {
        let red = colour_of(rgba(0xFF00_00FF).into());

        assert_eq!((red.r, red.g, red.b, red.a), (1.0, 0.0, 0.0, 1.0));
    }

    /// A highlighter's ink is drawn with its alpha, and a pen's is drawn opaque: the two are one path.
    #[test]
    fn a_highlighter_keeps_its_alpha() {
        let alpha = crate::ink::HIGHLIGHTER_ALPHA;
        let band = colour_of(ink_colour(crate::ink::with_alpha(0xFF_EB_3B, alpha)));
        let pen = colour_of(ink_colour(0x1C_1C_1E));

        assert!(
            (band.a - alpha).abs() < 0.01,
            "the band lets the page through it: {}",
            band.a
        );
        assert_eq!(pen.a, 1.0, "a pen's line is opaque");
        assert!(
            (band.r - 1.0).abs() < 0.02
                && (band.g - 0.92).abs() < 0.06
                && (band.b - 0.23).abs() < 0.06,
            "and the colour is the one the stroke was stamped with: {band:?}"
        );
    }

    /// A rectangle is passed on by its own four numbers, as two corners, scaled for the display.
    #[test]
    fn a_rectangle_becomes_two_corners() {
        let rect = rect_of(
            Rect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            },
            2.0,
        );

        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (20.0, 40.0, 80.0, 120.0)
        );
    }

    /// The ink's transform: the paper's own units, magnified by the zoom and the display's scale,
    /// with the paper's origin placed in the surface's pixels.
    #[test]
    fn the_ink_is_scaled_and_placed_by_its_transform() {
        let canvas = Canvas {
            scale: 2.0,
            ink: Ink {
                origin: (100.0, 50.0),
                zoom: 1.5,
                ..Default::default()
            },
            ..Default::default()
        };

        let transform = ink_transform(&canvas);

        assert_eq!(transform.M11, 3.0);
        assert_eq!(transform.M22, 3.0);
        assert_eq!(transform.M31, 200.0);
        assert_eq!(transform.M32, 100.0);
    }

    /// The layer's four transforms put the paper where the reader sees it.
    ///
    /// Written as literals rather than compared with the function the matrix is built from — a test of a
    /// function against itself proves nothing. The convention: a page turned clockwise takes its own
    /// top-left corner to the sheet's top-right, and every corner stays inside the sheet it is drawn in.
    /// The ink model is turned back through the same function (see
    /// [`crate::ink::paper_of_drawn`], and the round trip the ink tests check), so this is the half that
    /// is pinned here.
    #[test]
    fn a_turned_page_is_drawn_where_the_reader_sees_it() {
        let (scale, zoom) = (1.5f32, 2.0f32);
        let origin = (37.0f32, 11.0f32);
        let paper = (600.0f32, 800.0f32);
        let drawn = scale * zoom;

        // Where the paper's own origin lands in the sheet: the right way up, a quarter clockwise, upside
        // down, and the other quarter — top-left, top-right, bottom-right, bottom-left.
        let expected = [(0.0, 0.0), (800.0, 0.0), (600.0, 800.0), (0.0, 600.0)];

        for (turns, (dx, dy)) in expected.iter().enumerate() {
            let turns = turns as u8;
            let matrix = ink_matrix(scale, zoom, origin, paper, turns);
            let at = |point: (f32, f32)| {
                (
                    point.0 * matrix.M11 + point.1 * matrix.M21 + matrix.M31,
                    point.0 * matrix.M12 + point.1 * matrix.M22 + matrix.M32,
                )
            };

            let want = (origin.0 * scale + dx * drawn, origin.1 * scale + dy * drawn);
            let got = at((0.0, 0.0));
            assert!(
                (got.0 - want.0).abs() < 0.01 && (got.1 - want.1).abs() < 0.01,
                "turn {turns}: the paper's origin is drawn at {got:?}, not {want:?}"
            );

            // Every corner of the paper is inside the rectangle the sheet is drawn in — the whole of what
            // the mapping has to get right, because ink drawn outside it is ink on the desk.
            let (sheet_width, sheet_height) = if turns % 2 == 1 {
                (paper.1 * drawn, paper.0 * drawn)
            } else {
                (paper.0 * drawn, paper.1 * drawn)
            };

            for corner in [(0.0, 0.0), (600.0, 0.0), (0.0, 800.0), (600.0, 800.0)] {
                let (x, y) = at(corner);
                assert!(
                    x >= origin.0 * scale - 0.01
                        && y >= origin.1 * scale - 0.01
                        && x <= origin.0 * scale + sheet_width + 0.01
                        && y <= origin.1 * scale + sheet_height + 0.01,
                    "turn {turns}: {corner:?} is drawn outside the sheet at {x}, {y}"
                );
            }
        }
    }

    /// A turn the whole way round comes back to the page it started as, in the matrix as in the model.
    #[test]
    fn four_turns_of_the_transform_are_the_identity() {
        let paper = (600.0f32, 800.0f32);
        let plain = ink_matrix(1.0, 1.0, (0.0, 0.0), paper, 0);
        let round = ink_matrix(1.0, 1.0, (0.0, 0.0), paper, 4);

        assert_eq!(
            (round.M11, round.M12, round.M21, round.M22, round.M31, round.M32),
            (plain.M11, plain.M12, plain.M21, plain.M22, plain.M31, plain.M32),
            "a page turned four times is the page it was"
        );
    }

    /// A dragged selection is drawn where the hand has taken it, and nothing else about it changes.
    #[test]
    fn a_dragged_selection_is_drawn_where_it_has_been_taken() {
        let (scale, zoom) = (1.5f32, 2.0f32);
        let drawn = scale * zoom;
        let paper = (600.0f32, 800.0f32);
        let origin = (37.0f32, 11.0f32);
        let plain = ink_matrix(scale, zoom, origin, paper, 0);
        let moved = shifted(plain, (10.0, -4.0), drawn);

        let at = |matrix: &Matrix3x2, point: (f32, f32)| {
            (
                point.0 * matrix.M11 + point.1 * matrix.M21 + matrix.M31,
                point.0 * matrix.M12 + point.1 * matrix.M22 + matrix.M32,
            )
        };

        let (x0, y0) = at(&plain, (0.0, 0.0));
        let (x1, y1) = at(&moved, (0.0, 0.0));
        assert!(
            (x1 - x0 - 10.0 * drawn).abs() < 0.01 && (y1 - y0 + 4.0 * drawn).abs() < 0.01,
            "a drag of the paper's own units, drawn: {x0}, {y0} -> {x1}, {y1}"
        );
        assert_eq!(
            (moved.M11, moved.M12, moved.M21, moved.M22),
            (plain.M11, plain.M12, plain.M21, plain.M22),
            "and a drag scales and turns nothing"
        );
    }

    /// The cache's first rule, which is the one that decides whether a page of handwriting costs
    /// anything per frame: a stroke appended keeps every geometry that was already built.
    #[test]
    fn appending_a_stroke_keeps_every_geometry() {
        assert_eq!(keep(&[1, 2, 3], [1, 2, 3, 4].into_iter(), 7, 7), 3);
    }

    /// An erase or an undo changes a stroke in the middle of the list, and everything from there on
    /// is built again — the strokes before it are untouched.
    #[test]
    fn a_changed_stroke_costs_the_ones_after_it() {
        assert_eq!(keep(&[1, 2, 3], [1, 9, 3].into_iter(), 7, 7), 1);
        assert_eq!(keep(&[1, 2, 3], [1, 2].into_iter(), 7, 7), 2);
    }

    /// A zoom that crosses into another detail rung rebuilds every outline without changing any
    /// stroke's identity, and the revision is what says so.
    #[test]
    fn another_revision_costs_every_geometry() {
        assert_eq!(keep(&[1, 2, 3], [1, 2, 3].into_iter(), 7, 8), 0);
    }

    /// The flattening tolerance is in the paper's units, coarser than the default only where a paper
    /// unit is smaller than the pixel showing it — and never finer, which the API does not take.
    #[test]
    fn the_flattening_tolerance_follows_the_zoom_down_to_the_api_floor() {
        assert_eq!(
            flattening_tolerance(1.0),
            DEFAULT_FLATTENING,
            "1:1 is the default"
        );
        assert_eq!(
            flattening_tolerance(4.0),
            DEFAULT_FLATTENING,
            "and nothing finer than the default is asked for"
        );
        assert_eq!(
            flattening_tolerance(0.25),
            DEFAULT_FLATTENING * 4.0,
            "zoomed out, four times as coarse in paper units is the same on screen"
        );

        // A zoom that makes no sense is not a reason to ask Direct2D for a tolerance it refuses.
        for zoom in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                flattening_tolerance(zoom),
                DEFAULT_FLATTENING,
                "zoom {zoom}"
            );
        }
    }
}
