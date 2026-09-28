//! What draws the canvas: Direct2D, bound to the swap chain's current buffer.
//!
//! ## Why Direct2D rather than Direct3D's own drawing calls
//!
//! The ink is a filled polygon per stroke — a ribbon whose width changes along the line — and the
//! work that makes one is *tessellation*: turning an outline into triangles, with antialiasing at
//! the edges. Direct3D offers no such thing, so a renderer built on it directly has to tessellate
//! the outlines itself, keep its own vertex buffers, and run its own multisampled pass. Direct2D
//! does all of that, and — the part that matters here — it keeps the result per *geometry* rather
//! than per frame.
//!
//! GPUI's own renderer does the other thing: lyon tessellates every path into the scene *every
//! frame*, and the geometry is thrown away when the frame ends. A page of handwriting held ~33 fps
//! with `paint` at 9.3 ms because of it, and a cached layer moved `paint` to 0.00 without moving the
//! frame rate, because the engine replays whatever the cache hands it. Here a stroke is turned into
//! geometry once — when it is closed — and every frame after that just draws it.
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
use gpui_kit::{rgb, Hsla, ImageId, Rgba};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_END_CLOSED, D2D1_FILL_MODE_WINDING, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT,
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
    ID2D1PathGeometry1, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct2D::ID2D1GeometrySink;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows_numerics::{Matrix3x2, Vector2};

use crate::ink::Stroke;
use crate::ink_layer::canvas::{Canvas, Fill, Page, Rect};

/// One unit in one pixel, whatever the display's own scale is (see the module docs).
const DOTS_PER_INCH: f32 = 96.0;

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
    /// The document page's bitmap, and the image it was uploaded from.
    page: Option<(ImageId, ID2D1Bitmap1)>,
}

/// The geometry of the finished strokes, index-aligned with the app's list of them.
#[derive(Default)]
struct InkCache {
    /// Each cached stroke, by identity — the address of its `Arc` — and the revision of every
    /// outline it was built at.
    identities: Vec<usize>,
    revision: u64,
    geometry: Vec<ID2D1PathGeometry1>,
}

impl Renderer {
    /// A renderer for the canvas's device.
    pub(crate) fn new(dxgi_device: &IDXGIDevice) -> Result<Self> {
        let factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }
                .context("creating a Direct2D factory")?;
        let device: ID2D1Device = unsafe { factory.CreateDevice(dxgi_device) }
            .context("creating a Direct2D device")?;
        let context = unsafe { device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }
            .context("creating a Direct2D context")?;

        unsafe {
            context.SetDpi(DOTS_PER_INCH, DOTS_PER_INCH);
            context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }

        let brush = unsafe { context.CreateSolidColorBrush(&colour_of(Hsla::default()), None) }
            .context("creating a brush")?;

        Ok(Renderer {
            factory,
            context,
            brush,
            ink: InkCache::default(),
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

        let target: ID2D1Bitmap1 =
            unsafe { self.context.CreateBitmapFromDxgiSurface(&buffer, Some(&properties)) }
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
    /// The finished strokes' geometry is kept between frames and rebuilt only for strokes this
    /// renderer has not seen. The stroke being written is rebuilt every frame, because it is still
    /// moving — and it is one stroke.
    fn draw_ink(&mut self, canvas: &Canvas) -> Result<()> {
        self.refresh_ink(canvas)?;

        unsafe {
            self.context.SetTransform(&ink_transform(canvas));

            for (stroke, geometry) in canvas.ink.strokes.iter().zip(self.ink.geometry.iter()) {
                self.brush.SetColor(&colour_of(rgb(stroke.color).into()));
                self.context.FillGeometry(geometry, &self.brush, None);
            }

            if let Some(stroke) = &canvas.ink.open {
                let geometry = self.geometry_for(stroke)?;
                self.brush.SetColor(&colour_of(rgb(stroke.color).into()));
                self.context.FillGeometry(&geometry, &self.brush, None);
            }

            // The transform is the ink's alone and does not outlive this call: a context keeps it,
            // and the next frame's sheet is placed in the surface's own pixels.
            self.context.SetTransform(&Matrix3x2::identity());
        }

        Ok(())
    }

    /// Brings the cached geometry up to date with the app's strokes.
    fn refresh_ink(&mut self, canvas: &Canvas) -> Result<()> {
        let strokes = &canvas.ink.strokes;

        let keep = keep(
            &self.ink.identities,
            strokes.iter().map(identity),
            self.ink.revision,
            canvas.ink.revision,
        );

        self.ink.identities.truncate(keep);
        self.ink.geometry.truncate(keep);
        self.ink.revision = canvas.ink.revision;

        for stroke in strokes.iter().skip(keep) {
            let geometry = self.geometry_for(stroke)?;
            self.ink.identities.push(identity(stroke));
            self.ink.geometry.push(geometry);
        }

        Ok(())
    }

    /// A stroke's ribbon outline as geometry Direct2D can draw.
    ///
    /// Filled with the **winding** rule, which is what a pen does: an outline that crosses itself —
    /// a loop, a sharp turn, a scribble over its own line — fills everything it winds around, where
    /// the even-odd rule would leave a hole at every crossing. The frame's own renderer set the same
    /// rule on lyon for the same reason, and the measured symptom of getting it wrong was 3,145
    /// pixels of paper inside the ink, in stripes.
    fn geometry_for(&self, stroke: &Stroke) -> Result<ID2D1PathGeometry1> {
        let geometry = unsafe { self.factory.CreatePathGeometry() }
            .context("creating a stroke's geometry")?;

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

/// Where the paper's own units sit on the surface: how large it is drawn, times the display's
/// scale, and where its origin is, likewise.
fn ink_transform(canvas: &Canvas) -> Matrix3x2 {
    let scale = canvas.scale * canvas.ink.zoom;

    Matrix3x2 {
        M11: scale,
        M22: scale,
        M31: canvas.ink.origin.0 * canvas.scale,
        M32: canvas.ink.origin.1 * canvas.scale,
        ..Default::default()
    }
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
}
