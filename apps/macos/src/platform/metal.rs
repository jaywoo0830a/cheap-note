//! The canvas, drawn by Metal.
//!
//! The view hands over a description — `Canvas`: the desk, the fills and ruling, the document's page, and
//! the ink — and this turns it into GPU geometry. No bitmap, no Core Graphics, no image hand-over: a fill is
//! two triangles, a page is a textured quad, and a stroke is a fan of triangles over the outline the ink
//! model already computed.
//!
//! Three things follow from that, and they are the reason for the module:
//!
//! * **Nothing is copied per reading.** The finished ink is built once and kept in a vertex buffer; a
//!   reading rebuilds only the stroke under the pen, which is a few hundred vertices.
//! * **Four draw calls.** Desk and paper, the page's texture, the finished ink, the open stroke. The GPU
//!   does in microseconds what Core Graphics did in milliseconds of bitmap work.
//! * **One drawable per present.** `CAMetalLayer` hands the compositor a surface it already owns, instead of
//!   a fifteen-megabyte image for it to copy.
//!
//! The ink is a filled polygon in the model's terms, so it is drawn as a fan of triangles: a stroke that
//! crosses itself is filled once, because every triangle carries the stroke's own colour and the draw is
//! opaque. A highlighter is the one case where that is not exactly true — its ink has an alpha, and a fan
//! darkens where a stroke overlaps itself — which is why the highlighter's strokes are the next thing to
//! give a stencil pass of its own.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use note_ui::ink_layer::canvas::Rect;
use note_ui::ink_layer::{ink_colour, Canvas};
use note_ui::paint::drawn_of_paper;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLLibrary,
    MTLResourceOptions, MTLLoadAction,
    MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSamplerDescriptor,
    MTLSamplerMinMagFilter, MTLSamplerState, MTLSize, MTLStorageMode, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

/// The whole of the shading: place a pixel, and colour it.
const SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Vertex { float2 position; float r; float g; float b; float a; };
struct Varyings { float4 position [[position]]; float4 colour; };

// A pixel of the window in, a corner of the clip volume out. y is turned round here, once: the canvas
// measures downwards from the top and clip space upwards from the middle.
static inline float4 place(float2 position, float2 viewport) {
    return float4(position.x / viewport.x * 2.0 - 1.0,
                  1.0 - position.y / viewport.y * 2.0,
                  0.0, 1.0);
}

vertex Varyings solid_vertex(uint id [[vertex_id]],
                             constant Vertex *vertices [[buffer(0)]],
                             constant float2 &viewport [[buffer(1)]]) {
    Varyings out;
    out.position = place(vertices[id].position, viewport);
    out.colour = float4(vertices[id].r, vertices[id].g, vertices[id].b, vertices[id].a);
    return out;
}

fragment float4 solid_fragment(Varyings in [[stage_in]]) {
    return in.colour;
}

struct PageVertex { float2 position; float2 uv; };
struct PageVaryings { float4 position [[position]]; float2 uv; };

vertex PageVaryings page_vertex(uint id [[vertex_id]],
                                constant PageVertex *vertices [[buffer(0)]],
                                constant float2 &viewport [[buffer(1)]]) {
    PageVaryings out;
    out.position = place(vertices[id].position, viewport);
    out.uv = vertices[id].uv;
    return out;
}

fragment float4 page_fragment(PageVaryings in [[stage_in]],
                              texture2d<float> page [[texture(0)]],
                              sampler sample [[sampler(0)]]) {
    return page.sample(sample, in.uv);
}
"#;

/// One filled or textured corner: where it is, in device pixels, and what colour it carries.
#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    x: f32,
    y: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

/// One corner of the document's page: where it is, and which texel of the page is there.
#[repr(C)]
#[derive(Clone, Copy)]
struct PageVertex {
    x: f32,
    y: f32,
    u: f32,
    v: f32,
}


/// The window's GPU canvas.
pub(super) struct Surface {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Fills, ruling and ink: one vertex per corner, one colour each.
    solid: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The document's page: a textured quad.
    page: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    layer: Retained<CAMetalLayer>,
    /// The finished ink, as triangles. Rebuilt when what it is a function of changes and at no other time:
    /// the strokes, the mapping that places them, the part of the sheet on screen, and the ink in hand.
    ink: Vec<Vertex>,
    /// What `ink` was built from — see [`InkKey`].
    ink_key: Option<InkKey>,
    /// The document's page on the GPU, and the identity of the rasterisation it holds.
    page_texture: Option<(usize, Retained<ProtocolObject<dyn MTLTexture>>)>,
    /// The multisampled target the passes draw into, resolved into the drawable. Made with the size.
    msaa: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    width: usize,
    height: usize,
}

impl Surface {
    /// Gives a view a Metal layer, and the pipelines a canvas is drawn with.
    pub(super) fn install(view: &objc2_app_kit::NSView, scale: f64) -> Option<Self> {
        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;

        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .ok()?;

        let solid = pipeline(&device, &library, "solid_vertex", "solid_fragment")?;
        let page = pipeline(&device, &library, "page_vertex", "page_fragment")?;

        let sampler_descriptor = MTLSamplerDescriptor::new();
        sampler_descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
        sampler_descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
        let sampler = device.newSamplerStateWithDescriptor(&sampler_descriptor)?;

        let layer = CAMetalLayer::layer();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(true);
        layer.setContentsScale(scale);
        // Three in flight: enough for the compositor to hold one while the next is drawn, and no more — a
        // drawable is a window-sized surface.
        layer.setMaximumDrawableCount(3);
        layer.setDisplaySyncEnabled(true);

        view.setWantsLayer(true);
        view.setLayer(Some(&layer));

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            println!("metal: surface ready — pipelines, sampler and layer");
        }

        Some(Surface {
            device,
            queue,
            solid,
            page,
            sampler,
            layer,
            ink: Vec::new(),
            ink_key: None,
            page_texture: None,
            msaa: None,
            width: 1,
            height: 1,
        })
    }

    /// Matches the drawable to the view. `width` and `height` are device pixels.
    pub(super) fn resize(&mut self, width: usize, height: usize, scale: f64) {
        if (self.width, self.height) == (width, height) {
            return;
        }

        self.width = width.max(1);
        self.height = height.max(1);
        self.layer.setContentsScale(scale);
        self.layer.setDrawableSize(objc2_core_foundation::CGSize::new(
            self.width as f64,
            self.height as f64,
        ));

        // The ink's triangles are in pixels, so a resize makes them all stale.
        self.ink_key = None;
        self.msaa = None;
    }
}

/// A pipeline that blends one colour over another, which is what every pass here does.
fn pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    vertex: &str,
    fragment: &str,
) -> Option<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
    let vertex = library.newFunctionWithName(&NSString::from_str(vertex))?;
    let fragment = library.newFunctionWithName(&NSString::from_str(fragment))?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vertex));
    descriptor.setFragmentFunction(Some(&fragment));
    descriptor.setRasterSampleCount(SAMPLES);

    // SAFETY: index 0 is the one attachment every pass here renders into, and the descriptor is alive.
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);

    // Source over destination, in the sRGB values the app's colours are written in — the same arithmetic
    // Core Graphics did, so a highlighter over a page looks the way it looked before.
    unsafe {
        let attachment = &*attachment;
        let _: () = objc2::msg_send![attachment, setBlendingEnabled: true];
        let _: () = objc2::msg_send![attachment, setSourceRGBBlendFactor: 4u64];
        let _: () = objc2::msg_send![attachment, setDestinationRGBBlendFactor: 5u64];
        let _: () = objc2::msg_send![attachment, setSourceAlphaBlendFactor: 4u64];
        let _: () = objc2::msg_send![attachment, setDestinationAlphaBlendFactor: 5u64];
    }

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .ok()
}

impl Surface {
    /// Rebuilds the finished ink, when what the triangles are a function of has changed.
    fn refresh_ink(&mut self, canvas: &Canvas) {
        let key = InkKey::of(canvas);

        if self.ink_key.as_ref() == Some(&key) {
            return;
        }

        self.ink.clear();
        self.ink_key = Some(key);

        let list = canvas.ink.strokes.as_slice();
        let mapping = Mapping::of(canvas);
        let ink = &canvas.ink;
        let in_hand = |index: usize| ink.selected.get(index).copied().unwrap_or(false);

        for (index, stroke) in list.iter().enumerate() {
            if in_hand(index) || !stroke.visible_in(ink.visible) {
                continue;
            }

            fill(&mut self.ink, stroke, &mapping, (0.0, 0.0), ink_colour(stroke.color));
        }

        // The ink in hand, where a drag has taken it, and once more in the selection's colour.
        if ink.selection.is_some() || ink.offset != (0.0, 0.0) {
            for (index, stroke) in list.iter().enumerate() {
                if !in_hand(index) {
                    continue;
                }

                fill(&mut self.ink, stroke, &mapping, ink.offset, ink_colour(stroke.color));

                if let Some(colour) = ink.selection {
                    fill(&mut self.ink, stroke, &mapping, ink.offset, colour);
                }
            }
        }

        // The lasso's loop: the ring the reader is drawing, in the selection's colour.
        if let Some(stroke) = &ink.lasso {
            let colour = ink.selection.unwrap_or_else(|| ink_colour(stroke.color));
            fill(&mut self.ink, stroke, &mapping, (0.0, 0.0), colour);
        }
    }

    /// The multisampled target, made once per size.
    ///
    /// Four samples a pixel, because the ink is a polygon and Metal has no analytic antialiasing to give it
    /// — where Direct2D, on Windows, fills each stroke with *coverage* rather than samples
    /// (`D2D1_ANTIALIAS_MODE_PER_PRIMITIVE`). The texture is private and lives only on the GPU; the pass
    /// resolves it into the drawable, which is the one copy the compositor needs.
    fn multisample(&mut self) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        if let Some(texture) = &self.msaa {
            return Some(texture.clone());
        }

        // SAFETY: a 2D descriptor for the size this surface draws at.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                self.width,
                self.height,
                false,
            )
        };
        descriptor.setTextureType(MTLTextureType::Type2DMultisample);
        // SAFETY: a sample count Metal supports for this format on every Mac that runs this.
        unsafe {
            descriptor.setSampleCount(SAMPLES);
        }
        descriptor.setStorageMode(MTLStorageMode::Private);
        descriptor.setUsage(MTLTextureUsage::RenderTarget);

        self.msaa = self.device.newTextureWithDescriptor(&descriptor);
        self.msaa.clone()
    }

    /// The stroke under the pen, as triangles: the only geometry that is rebuilt per reading.
    fn open_stroke(&self, canvas: &Canvas) -> Vec<Vertex> {
        let mut vertices = Vec::new();

        if let Some(stroke) = &canvas.ink.open {
            let mapping = Mapping::of(canvas);
            ribbon(&mut vertices, stroke, &mapping, (0.0, 0.0), ink_colour(stroke.color));
        }

        vertices
    }

    /// The page's texture, uploaded when the document hands over a new rasterisation of it.
    fn page_texture(&mut self, canvas: &Canvas) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        let page = canvas.page.as_ref()?;
        let key = Arc::as_ptr(&page.image) as usize;

        if self.page_texture.as_ref().map(|(cached, _)| *cached) == Some(key) {
            return self.page_texture.as_ref().map(|(_, texture)| texture.clone());
        }

        let bytes = page.image.as_bytes(0)?;
        let size = page.image.size(0);
        let (width, height) = (size.width.0 as usize, size.height.0 as usize);

        if width == 0 || height == 0 {
            return None;
        }

        // SAFETY: a 2D descriptor for the pixels the page actually has.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                width,
                height,
                false,
            )
        };
        descriptor.setStorageMode(MTLStorageMode::Shared);
        descriptor.setUsage(MTLTextureUsage::ShaderRead);

        let texture = self.device.newTextureWithDescriptor(&descriptor)?;

        // SAFETY: the page's own bytes, top-down BGRA, as many as the texture has pixels.
        unsafe {
            let bytes = NonNull::new(bytes.as_ptr() as *mut c_void)?;
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize { width, height, depth: 1 },
                },
                0,
                bytes,
                width * 4,
            );
        }

        self.page_texture = Some((key, texture));

        self.page_texture.as_ref().map(|(_, texture)| texture.clone())
    }
}

/// A rectangle as two triangles.
fn quad(vertices: &mut Vec<Vertex>, rect: Rect, colour: gpui_kit::Hsla) {
    if rect.is_empty() {
        return;
    }

    let (x, y) = (rect.x, rect.y);
    let (right, bottom) = (rect.x + rect.width, rect.y + rect.height);
    let vertex = |x: f32, y: f32| Vertex::new(x, y, colour);

    vertices.extend_from_slice(&[
        vertex(x, y),
        vertex(right, y),
        vertex(x, bottom),
        vertex(x, bottom),
        vertex(right, y),
        vertex(right, bottom),
    ]);
}

impl Vertex {
    fn new(x: f32, y: f32, colour: gpui_kit::Hsla) -> Self {
        let rgba = gpui_kit::Rgba::from(colour);

        Vertex {
            x,
            y,
            r: rgba.r,
            g: rgba.g,
            b: rgba.b,
            a: rgba.a,
        }
    }
}

/// A finished stroke, filled exactly: its outline, tessellated once and kept.
///
/// The outline is the ribbon's edge, and it is a polygon — one Direct2D fills in a single primitive on
/// Windows, which is why a stroke that crosses itself there is ink and not a hole. Metal has no such fill,
/// so the polygon is cut into triangles here, once per stroke, and the triangles are cached with the rest of
/// the ink (see [`InkKey`]).
fn fill(
    vertices: &mut Vec<Vertex>,
    stroke: &note_format::ink::Stroke,
    mapping: &Mapping,
    shift: (f32, f32),
    colour: gpui_kit::Hsla,
) {
    if stroke.outline.len() < 3 {
        return;
    }

    let polygon: Vec<(f32, f32)> = stroke
        .outline
        .iter()
        .map(|point| mapping.point(*point, shift))
        .collect();

    tessellate(vertices, &polygon, colour);
}

/// Cuts a closed polygon into triangles with the **nonzero winding** rule.
///
/// Nonzero, and not even-odd: a stroke that crosses itself has a region the pen passed through twice, and
/// that region is ink. Filled even-odd it would be a hole — a highlighter would show the page through the
/// wrong places — and the rule Direct2D fills with on Windows is nonzero too.
///
/// The polygon is swept as a stack of trapezoids: every height where it has a vertex bounds a band, no edge
/// crosses a band's top or bottom inside it, and a band's fill is the run of x-intervals where the winding
/// is not zero. That is exact for any single closed loop, self-intersecting or not, and needs no ear to be
/// clipped. One loop is all a stroke's outline ever is: a hole would need a second loop joined to the first
/// by a bridge, which the ink model has no reason to make.
fn tessellate(vertices: &mut Vec<Vertex>, polygon: &[(f32, f32)], colour: gpui_kit::Hsla) {
    if polygon.len() < 3 {
        return;
    }

    let mut bands: Vec<f32> = polygon.iter().map(|point| point.1).collect();
    bands.sort_by(|left, right| left.total_cmp(right));
    bands.dedup_by(|left, right| (*left - *right).abs() < f32::EPSILON);

    // One crossing of a band: where it is at the band's middle (which is what orders two crossings), which
    // way its edge runs, and where it meets the band's own bottom and top — one crossing gives a trapezoid
    // two of its corners.
    struct Crossing {
        middle: f32,
        winding: i32,
        bottom: f32,
        top: f32,
    }

    let mut crossings: Vec<Crossing> = Vec::new();

    for band in bands.windows(2) {
        let (bottom, top) = (band[0], band[1]);

        if top - bottom < f32::EPSILON {
            continue;
        }

        let middle = (bottom + top) * 0.5;
        crossings.clear();

        for index in 0..polygon.len() {
            let (a, b) = (polygon[index], polygon[(index + 1) % polygon.len()]);

            if a.1 == b.1 {
                continue;
            }

            let (low, high) = if a.1 < b.1 { (a, b) } else { (b, a) };

            // A band's ends are vertex heights, so an edge either covers it whole or does not reach it.
            if low.1 > bottom || high.1 < top {
                continue;
            }

            let at = |y: f32| a.0 + (b.0 - a.0) * (y - a.1) / (b.1 - a.1);

            crossings.push(Crossing {
                middle: at(middle),
                winding: if b.1 > a.1 { 1 } else { -1 },
                bottom: at(bottom),
                top: at(top),
            });
        }

        crossings.sort_by(|left, right| left.middle.total_cmp(&right.middle));

        let mut winding = 0;
        let mut span: Option<(f32, f32)> = None;

        for crossing in &crossings {
            let before = winding;
            winding += crossing.winding;

            match (before == 0, winding == 0) {
                (true, false) => span = Some((crossing.bottom, crossing.top)),
                (false, true) => {
                    if let Some((left_bottom, left_top)) = span.take() {
                        trapezoid(
                            vertices,
                            (left_bottom, bottom),
                            (left_top, top),
                            (crossing.top, top),
                            (crossing.bottom, bottom),
                            colour,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

/// One trapezoid of a band, as two triangles.
fn trapezoid(
    vertices: &mut Vec<Vertex>,
    left_bottom: (f32, f32),
    left_top: (f32, f32),
    right_top: (f32, f32),
    right_bottom: (f32, f32),
    colour: gpui_kit::Hsla,
) {
    if (right_bottom.0 - left_bottom.0).abs() < f32::EPSILON
        && (right_top.0 - left_top.0).abs() < f32::EPSILON
    {
        return;
    }

    let vertex = |corner: (f32, f32)| Vertex::new(corner.0, corner.1, colour);

    vertices.extend_from_slice(&[
        vertex(left_bottom),
        vertex(right_bottom),
        vertex(right_top),
        vertex(left_bottom),
        vertex(right_top),
        vertex(left_top),
    ]);
}

/// A stroke as a ribbon: a quad per segment, and a disc at every joint.
///
/// The model's `outline` is the ribbon's edge, and a fan of triangles over it is only correct when that
/// polygon is star-shaped — handwriting is not: a loop or a sharp turn makes a triangle of the fan reach
/// outside the stroke, which is ink where nobody wrote. The points and their widths are what the ribbon was
/// made from, so the ribbon is rebuilt from those instead. Overlap is invisible for opaque ink, which is
/// every stroke but the highlighter's.
fn ribbon(
    vertices: &mut Vec<Vertex>,
    stroke: &note_format::ink::Stroke,
    mapping: &Mapping,
    shift: (f32, f32),
    colour: gpui_kit::Hsla,
) {
    let points = &stroke.points;
    let Some(first) = points.first() else {
        return;
    };

    let at = |point: &note_format::ink::InkPoint| mapping.point([point.x, point.y], shift);
    // The ink's widths are in the paper's own units, and the mapping scales those by the zoom and the
    // display's scale — so a width becomes pixels by the same two factors.
    let pixels = |width: f32| width * 0.5 * mapping.zoom * mapping.scale;

    if points.len() == 1 {
        disc(vertices, at(first), pixels(first.width), colour);
        return;
    }

    for pair in points.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let (pa, pb) = (at(a), at(b));

        let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
        let length = (dx * dx + dy * dy).sqrt();

        if length < f32::EPSILON {
            continue;
        }

        let (ha, hb) = (pixels(a.width), pixels(b.width));
        let (nx, ny) = (-dy / length, dx / length);
        let vertex = |p: (f32, f32)| Vertex::new(p.0, p.1, colour);

        let corners = [
            vertex((pa.0 + nx * ha, pa.1 + ny * ha)),
            vertex((pa.0 - nx * ha, pa.1 - ny * ha)),
            vertex((pb.0 - nx * hb, pb.1 - ny * hb)),
            vertex((pb.0 + nx * hb, pb.1 + ny * hb)),
        ];

        vertices.extend_from_slice(&[
            corners[0], corners[1], corners[2], corners[0], corners[2], corners[3],
        ]);

        // The wedge a turn leaves open on the outside of the bend.
        disc(vertices, pb, hb, colour);
    }

    // The ends: a nib is round, and so is a highlighter's.
    disc(vertices, at(first), pixels(first.width), colour);
}

/// A disc as a fan of triangles: a joint of the ribbon, or a stroke of a single point.
fn disc(vertices: &mut Vec<Vertex>, centre: (f32, f32), radius: f32, colour: gpui_kit::Hsla) {
    const STEPS: usize = 8;

    if radius <= 0.0 {
        return;
    }

    let vertex = |angle: f32| {
        Vertex::new(
            centre.0 + radius * angle.cos(),
            centre.1 + radius * angle.sin(),
            colour,
        )
    };

    for step in 0..STEPS {
        let from = step as f32 / STEPS as f32 * std::f32::consts::TAU;
        let to = (step + 1) as f32 / STEPS as f32 * std::f32::consts::TAU;

        vertices.push(Vertex::new(centre.0, centre.1, colour));
        vertices.push(vertex(from));
        vertices.push(vertex(to));
    }
}


/// Samples per pixel: the ink is a polygon, and a polygon's edge is a staircase without them.
///
/// Four, resolved into the drawable. This is the Mac's answer to what Direct2D does analytically on
/// Windows: Metal has no per-primitive coverage, so the edge is sampled instead.
const SAMPLES: usize = 4;

/// What the finished ink's triangles were built from.
///
/// Every triangle is in **device pixels**, so it is a function of everything that decides where a point of
/// the paper lands on the screen. The stroke list alone is not enough, and that is not a detail: scrolling
/// moves the paper and leaves the ink untouched, so a key of "the same strokes" would keep the triangles
/// where they were — and the page would slide out from under its own handwriting.
#[derive(PartialEq)]
struct InkKey {
    len: usize,
    first: Option<usize>,
    last: Option<usize>,
    revision: u64,
    origin: [u32; 2],
    zoom: u32,
    rotation: u8,
    scale: u32,
    paper: [u32; 2],
    visible: [u32; 4],
    offset: [u32; 2],
    selection: Option<[u32; 4]>,
    lasso: Option<(usize, usize, [u32; 4])>,
}

impl InkKey {
    fn of(canvas: &Canvas) -> Self {
        let list = canvas.ink.strokes.as_slice();
        let ink = &canvas.ink;

        InkKey {
            len: list.len(),
            first: list.first().map(|stroke| Arc::as_ptr(stroke) as usize),
            last: list.last().map(|stroke| Arc::as_ptr(stroke) as usize),
            revision: ink.revision,
            origin: [ink.origin.0.to_bits(), ink.origin.1.to_bits()],
            zoom: ink.zoom.to_bits(),
            rotation: ink.rotation,
            scale: canvas.scale.to_bits(),
            paper: [ink.paper.0.to_bits(), ink.paper.1.to_bits()],
            visible: ink.visible.map(f32::to_bits),
            offset: [ink.offset.0.to_bits(), ink.offset.1.to_bits()],
            selection: ink.selection.map(|colour| {
                [
                    colour.h.to_bits(),
                    colour.s.to_bits(),
                    colour.l.to_bits(),
                    colour.a.to_bits(),
                ]
            }),
            lasso: ink.lasso.as_ref().map(|stroke| {
                (
                    stroke.points.len(),
                    stroke.outline.len(),
                    stroke.bounds.map(f32::to_bits),
                )
            }),
        }
    }
}

/// The ink's coordinates to the screen's: where the paper is drawn, how large, which way up, and how the
/// page is turned.
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

    fn point(&self, [x, y]: [f32; 2], shift: (f32, f32)) -> (f32, f32) {
        let (x, y) = (x + shift.0, y + shift.1);
        let across = self.base.0 + x * self.step_x.0 + y * self.step_y.0;
        let down = self.base.1 + x * self.step_x.1 + y * self.step_y.1;

        (
            (self.origin.0 + across * self.zoom) * self.scale,
            (self.origin.1 + down * self.zoom) * self.scale,
        )
    }
}

/// A canvas rectangle in device pixels.
fn device(rect: Rect, scale: f32) -> Rect {
    Rect {
        x: rect.x * scale,
        y: rect.y * scale,
        width: rect.width * scale,
        height: rect.height * scale,
    }
}

/// The document's page, as the two triangles its texture is stretched over.
fn page_quad(canvas: &Canvas, scale: f32) -> Vec<PageVertex> {
    let Some(page) = &canvas.page else {
        return Vec::new();
    };

    let rect = device(page.rect, scale);
    let (x, y) = (rect.x, rect.y);
    let (right, bottom) = (rect.x + rect.width, rect.y + rect.height);
    let corner = |x: f32, y: f32, u: f32, v: f32| PageVertex { x, y, u, v };

    vec![
        corner(x, y, 0.0, 0.0),
        corner(right, y, 1.0, 0.0),
        corner(x, bottom, 0.0, 1.0),
        corner(x, bottom, 0.0, 1.0),
        corner(right, y, 1.0, 0.0),
        corner(right, bottom, 1.0, 1.0),
    ]
}


impl Surface {
    /// Draws one description of the canvas. `false` when there was no drawable to draw into.
    pub(super) fn draw(&mut self, canvas: &Canvas) -> bool {
        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            report::drawn(self.ink.len(), canvas.ink.open.as_deref().map_or(0, |s| s.outline.len()));
        }

        let scale = if canvas.scale > 0.0 { canvas.scale } else { 1.0 };
        let (width, height) = (self.width as f32, self.height as f32);

        // Behind the page: the desk, the shadow's steps, the sheet and the ruling, in the order painted.
        let mut behind = Vec::with_capacity(8 + canvas.fills.len() * 6 + canvas.rules.len() * 6);
        quad(
            &mut behind,
            Rect {
                x: 0.0,
                y: 0.0,
                width,
                height,
            },
            canvas.desk,
        );

        for mark in canvas.fills.iter().chain(canvas.rules.iter()) {
            quad(&mut behind, device(mark.rect, scale), mark.colour);
        }

        // In front of the ink: the highlighter's preview, the one mark a gesture lays over ink that is
        // already on the page. (A lasso's loop is ink, and is drawn with the ink.)
        let mut front = Vec::new();
        if let Some((colour, bands)) = &canvas.ink.highlight {
            for band in bands {
                quad(&mut front, device(*band, scale), *colour);
            }
        }

        self.refresh_ink(canvas);
        let open = self.open_stroke(canvas);
        let page = self
            .page_texture(canvas)
            .map(|texture| (page_quad(canvas, scale), texture));

        let Some(drawable) = self.layer.nextDrawable() else {
            return false;
        };

        let Some(msaa) = self.multisample() else {
            return false;
        };

        let pass = MTLRenderPassDescriptor::new();

        // SAFETY: index 0 is the one attachment these pipelines were built for.
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&msaa));
        attachment.setResolveTexture(Some(&drawable.texture()));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::MultisampleResolve);

        let Some(buffer) = self.queue.commandBuffer() else {
            return false;
        };

        let Some(encoder) = buffer.renderCommandEncoderWithDescriptor(&pass) else {
            return false;
        };

        // Behind: the desk, the shadow, the sheet, the ruling.
        if let Some(buffer) = self.buffer(&behind) {
            self.solid(&encoder, &buffer, behind.len(), width, height);
        }

        // The document's page.
        if let Some((quad, texture)) = &page {
            if let Some(buffer) = self.buffer(quad) {
                encoder.setRenderPipelineState(&self.page);
                self.bind(&encoder, &buffer, width, height);

                // SAFETY: a texture this surface keeps, and the sampler its pipeline was built with.
                unsafe {
                    encoder.setFragmentTexture_atIndex(Some(texture), 0);
                    encoder.setFragmentSamplerState_atIndex(Some(&self.sampler), 0);
                }

                // SAFETY: six vertices — the two triangles of the page's quad.
                unsafe {
                    encoder.drawPrimitives_vertexStart_vertexCount(
                        MTLPrimitiveType::Triangle,
                        0,
                        quad.len(),
                    );
                }
            }
        }

        // The finished ink, then the highlight, then the stroke under the pen: the order the page was
        // painted in when Core Graphics drew it.
        if let Some(buffer) = self.buffer(&self.ink) {
            self.solid(&encoder, &buffer, self.ink.len(), width, height);
        }

        if let Some(buffer) = self.buffer(&front) {
            self.solid(&encoder, &buffer, front.len(), width, height);
        }

        if let Some(buffer) = self.buffer(&open) {
            self.solid(&encoder, &buffer, open.len(), width, height);
        }

        encoder.endEncoding();

        // SAFETY: `presentDrawable:` is the command buffer's own method for the drawable it was handed.
        unsafe {
            let _: () = objc2::msg_send![&*buffer, presentDrawable: &*drawable];
        }

        buffer.commit();

        true
    }
}


impl Surface {
    /// Uploads one run of vertices, and hands back the buffer to bind.
    ///
    /// One allocation per run per frame — at most four, a few kilobytes each. A pool would save the
    /// allocation and cost a lifetime to track; the measurement this module exists for is the reading's,
    /// and this is not where it goes.
    fn buffer<T>(&self, vertices: &[T]) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
        if vertices.is_empty() {
            return None;
        }

        let bytes = std::mem::size_of_val(vertices);
        let buffer = self
            .device
            .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)?;

        // SAFETY: a buffer of exactly this many bytes, written once before anything reads it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                vertices.as_ptr().cast::<u8>(),
                buffer.contents().as_ptr().cast::<u8>(),
                bytes,
            );
        }

        Some(buffer)
    }

    /// Binds a run of vertices, and the window they are placed in.
    fn bind(
        &self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        buffer: &ProtocolObject<dyn MTLBuffer>,
        width: f32,
        height: f32,
    ) {
        let viewport: [f32; 2] = [width, height];

        // SAFETY: the buffer holds the vertices the draw call reads, and the viewport is eight bytes the
        // encoder copies before this function returns.
        unsafe {
            encoder.setVertexBuffer_offset_atIndex(Some(buffer), 0, 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(viewport.as_ptr() as *mut c_void).unwrap(),
                std::mem::size_of_val(&viewport),
                1,
            );
        }
    }

    /// One run of coloured vertices, in one draw call.
    fn solid(&self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        buffer: &ProtocolObject<dyn MTLBuffer>,
        count: usize,
        width: f32,
        height: f32,
    ) {
        encoder.setRenderPipelineState(&self.solid);
        self.bind(encoder, buffer, width, height);

        // SAFETY: `count` vertices, all of them in the buffer just bound.
        unsafe {
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, count);
        }
    }
}


/// What the canvas drew, once a second, when `CHEAP_NOTE_TRACE` is set.
///
/// Two numbers that matter: how often the canvas reached a drawable — a frame that cannot get one is a
/// frame nobody sees — and how much ink was in it, so that "nothing drew" and "nothing to draw" can be
/// told apart.
mod report {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    static DRAWS: AtomicU64 = AtomicU64::new(0);
    static FINISHED: AtomicU64 = AtomicU64::new(0);
    static OPEN: AtomicU64 = AtomicU64::new(0);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    pub fn drawn(finished: usize, open: usize) {
        DRAWS.fetch_add(1, Ordering::Relaxed);
        FINISHED.store(finished as u64, Ordering::Relaxed);
        OPEN.store(open as u64, Ordering::Relaxed);

        let mut last = LAST.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();

        if last.is_some_and(|at| now.duration_since(at).as_millis() < 1000) {
            return;
        }

        *last = Some(now);

        println!(
            "canvas: {} draws/s · {} finished vertices · {} under the pen",
            DRAWS.swap(0, Ordering::Relaxed),
            FINISHED.load(Ordering::Relaxed),
            OPEN.load(Ordering::Relaxed)
        );
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// A scroll moves the paper and leaves the strokes exactly as they were: the ink's triangles, which are
    /// in pixels, have to be rebuilt — this is the bug that had the page slide out from under its writing.
    #[test]
    fn a_scroll_is_a_new_ink_key() {
        let mut canvas = Canvas::default();
        let before = InkKey::of(&canvas);

        canvas.ink.origin = (0.0, -120.0);
        let after = InkKey::of(&canvas);

        assert!(before != after, "a scroll left the ink's key alone");
    }

    /// A zoom is the same story, and so is a turn of the page.
    #[test]
    fn a_zoom_is_a_new_ink_key() {
        let mut canvas = Canvas::default();
        let before = InkKey::of(&canvas);

        canvas.ink.zoom = 1.5;
        assert!(
            before != InkKey::of(&canvas),
            "a zoom left the ink's key alone"
        );
    }

    /// The other half: a frame that changed nothing is the same key, so the ink is not rebuilt for being
    /// asked twice — the page's triangles are tens of thousands of vertices.
    #[test]
    fn the_same_page_is_the_same_key() {
        let canvas = Canvas::default();

        assert!(InkKey::of(&canvas) == InkKey::of(&canvas));
    }
}


#[cfg(test)]
mod fill_tests {
    use super::*;

    /// The area of the triangles a fill produced: half a cross product each, summed.
    fn area(vertices: &[Vertex]) -> f32 {
        vertices
            .chunks_exact(3)
            .map(|triangle| {
                let (a, b, c) = (triangle[0], triangle[1], triangle[2]);
                ((b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y)).abs() * 0.5
            })
            .sum()
    }

    fn colour() -> gpui_kit::Hsla {
        gpui_kit::rgba(0x00FF_00FF).into()
    }

    /// A square comes out as a square: covered once, and its area is its own.
    #[test]
    fn a_square_is_filled_once() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let mut vertices = Vec::new();

        tessellate(&mut vertices, &square, colour());

        assert!((area(&vertices) - 100.0).abs() < 0.01, "area {}", area(&vertices));
    }

    /// A square traced twice is still one square. This is the highlighter's case — a stroke that passes over
    /// itself is ink, not a hole — and the reason the fill is nonzero rather than even-odd.
    #[test]
    fn a_region_traced_twice_is_still_ink() {
        let one = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let mut twice: Vec<(f32, f32)> = one.to_vec();
        twice.extend_from_slice(&one);

        let mut vertices = Vec::new();
        tessellate(&mut vertices, &twice, colour());

        assert!((area(&vertices) - 100.0).abs() < 0.01, "area {}", area(&vertices));
    }

    /// Every triangle stays inside the shape it came from: a fill that spills is ink where nobody wrote.
    #[test]
    fn nothing_is_filled_outside_the_polygon() {
        let polygon = [(0.0, 0.0), (40.0, 0.0), (40.0, 30.0), (0.0, 30.0)];
        let mut vertices = Vec::new();

        tessellate(&mut vertices, &polygon, colour());

        for vertex in &vertices {
            assert!(
                (-0.01..=40.01).contains(&vertex.x) && (-0.01..=30.01).contains(&vertex.y),
                "a corner at ({}, {}) is outside the polygon",
                vertex.x,
                vertex.y
            );
        }
    }
}

