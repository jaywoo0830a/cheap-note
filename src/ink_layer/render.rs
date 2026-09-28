//! What draws the canvas: Direct2D, bound to the swap chain's current buffer.
//!
//! ## Why Direct2D rather than Direct3D's own drawing calls
//!
//! The ink is a filled polygon per stroke — a ribbon whose width changes along the line — and the
//! work that makes one is *tessellation*: turning an outline into triangles, with antialiasing at
//! the edges. Direct3D offers no such thing, so a renderer built on it directly has to tessellate
//! the outlines itself, keep its own vertex buffers, and run its own multisampled pass. Direct2D
//! does all of that, keeps the result per geometry rather than per frame, and antialiases
//! analytically rather than by supersampling.
//!
//! GPUI's own renderer does the other thing: lyon tessellates every path into the scene *every
//! frame*, and the geometry is thrown away when the frame ends. That is what this exists not to do.
//!
//! ## Dots per inch, and why they are pinned
//!
//! A Direct2D context scales every coordinate by its own DPI, and its default is the desktop's: on
//! a 150% display, a rectangle asked for in physical pixels would be drawn one and a half times too
//! large. This canvas works in physical pixels and nothing else, so the context is pinned to 96 —
//! which makes one unit one pixel, on every display.

use anyhow::{Context, Result};
use gpui_kit::{Hsla, Rgba};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
    ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};

use crate::ink_layer::canvas::{Canvas, Rect};

/// One unit in one pixel, whatever the display's own scale is (see the module docs).
const DOTS_PER_INCH: f32 = 96.0;

/// A Direct2D device context, on the device the canvas was made with, and the brush a frame's
/// rectangles are filled with.
///
/// One brush rather than one per colour: its colour is set before each fill, and a page's shadow is
/// three rectangles that differ in nothing but their colour.
pub(crate) struct Renderer {
    context: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
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

        Ok(Renderer { context, brush })
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

        // Between `BeginDraw` and `EndDraw` Direct2D is a recorder: the calls below add to a batch
        // and report nothing, and the frame's own result is the one `EndDraw` answers with. That is
        // the model this renderer is built on, so the per-call results are deliberately not read.
        unsafe {
            self.context.SetTarget(&target);
            self.context.BeginDraw();
            self.context
                .Clear(Some(std::ptr::from_ref(&colour_of(canvas.desk))));
            for fill in &canvas.fills {
                self.brush.SetColor(&colour_of(fill.colour));
                self.context.FillRectangle(
                    std::ptr::from_ref(&rect_of(fill.rect, canvas.scale)),
                    &self.brush,
                );
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
}

/// A rectangle as Direct2D takes it: two corners, in that order, in the surface's own pixels.
///
/// The scale is applied here — one multiplication per side — rather than as a transform of the
/// surface, because the transform would have to be set and cleared around the batch and this is the
/// one place a canvas coordinate becomes a surface one.
fn rect_of(rect: Rect, scale: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.x * scale,
        top: rect.y * scale,
        right: (rect.x + rect.width) * scale,
        bottom: (rect.y + rect.height) * scale,
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
    use gpui_kit::rgba;

    /// The colour conversion, against the one mistake worth catching: a channel swap. The surface
    /// is BGRA and Direct2D's colour is not — passing the surface's byte order through here would
    /// turn the desk blue.
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
}
