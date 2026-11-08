//! The canvas, drawn by Metal.
//!
//! The view hands over a description — `Canvas`: the desk, the fills and ruling, the document's page, and
//! the ink — and this turns it into GPU geometry. No bitmap, no Core Graphics, no image hand-over: a fill is
//! two triangles, a page is a textured quad, and a stroke is a fan of triangles over the outline the ink
//! model already computed.
//!
//! Three things follow from that, and they are the reason for the module:
//!
//! * **Nothing is copied per reading.** The finished ink is built once and kept in a vertex buffer,
//!   uploaded again only when the ink changes; a reading appends only the new segments of the stroke
//!   under the pen. Both buffers are reused across frames, in a ring of three so the CPU never
//!   overwrites one the GPU is still reading.
//! * **Scrolling is a uniform, not a rebuild.** The ink is baked in the paper's own units — the units
//!   `note_ui`'s canvas keeps its geometry in — and where the paper is drawn, how large, and which way up
//!   is one transform the shader applies. A trackpad sends a hundred readings a second; rebuilding a page
//!   of triangles for each of them is what made scrolling stutter, and it is what this does not do.
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

use note_ui::cursor::{
    BODY_ALPHA, BODY_HALO_ALPHA, BODY_HALO_GROW, NIB_ALPHA, NIB_BLOOM_ALPHA, NIB_BLOOM_SPREAD,
    NIB_RADIUS_MAX, NIB_RADIUS_MIN,
};
use note_ui::ink_layer::canvas::Rect;
use note_ui::ink_layer::{ink_colour, Canvas};
use note_ui::paint::drawn_of_paper;

use crate::platform::ghost::Mark;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLLibrary,
    MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState,
    MTLResourceOptions, MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerState, MTLSize,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
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
                             constant float2 &viewport [[buffer(1)]],
                             constant float4 &transform [[buffer(2)]]) {
    // The ink is baked in the paper's own units and placed by `transform` — a scale and a shift, in device
    // pixels. The desk, the page and the pen's mark are already placed, and are drawn through the identity.
    Varyings out;
    out.position = place(vertices[id].position * transform.xy + transform.zw, viewport);
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
    /// The part of the sheet that was on screen when `ink` was built, in the paper's units.
    ///
    /// The one thing that moves while the ink stays valid: a build leaves out every stroke that was off
    /// screen, so scrolling towards one has to bring it back (see [`CULL_SLACK`]).
    ink_visible: [f32; 4],
    /// The open stroke's triangles, appended as the pen adds points.
    open: Vec<Vertex>,
    /// The identity of the stroke `open` was built from.
    open_id: u64,
    /// How many of the open stroke's points `open` already covers.
    open_points: usize,
    /// The colour `open` was built with.
    open_colour: u32,
    /// The paper and turn `open` was built for: a page turn changes the mapping, not the points.
    open_paper: (f32, f32),
    open_rotation: u8,
    /// The finished ink's buffers, one per frame in flight: the CPU writes the next while the GPU
    /// may still be reading the last.
    ink_buffers: [Option<Retained<ProtocolObject<dyn MTLBuffer>>>; 3],
    /// Which of the finished ink's buffers hold the current ink.
    ink_slots_valid: [bool; 3],
    /// The open stroke's buffers, one per frame in flight.
    open_buffers: [Option<Retained<ProtocolObject<dyn MTLBuffer>>>; 3],
    /// Which of the three the next frame writes.
    buffer_slot: usize,
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
        // A drawable that is not ready is a frame to skip, not a thread to park: the pen's pump
        // draws on the main thread, and waiting here would hold the next reading behind the
        // compositor. The next wake tries again (see `NoteApp::present_canvas`).
        layer.setAllowsNextDrawableTimeout(false);
        // The display's clock is the compositor's; a frame that waits for it is a frame the pen
        // waited for. `CHEAP_NOTE_NO_VSYNC=1` is the experiment: present immediately and see
        // whether the tear is worth the latency.
        layer.setDisplaySyncEnabled(std::env::var_os("CHEAP_NOTE_NO_VSYNC").is_none());

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
            ink_visible: [0.0; 4],
            open: Vec::new(),
            open_id: 0,
            open_points: 0,
            open_colour: 0,
            open_paper: (0.0, 0.0),
            open_rotation: 0,
            ink_buffers: [None, None, None],
            ink_slots_valid: [false; 3],
            open_buffers: [None, None, None],
            buffer_slot: 0,
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
        self.layer
            .setDrawableSize(objc2_core_foundation::CGSize::new(
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
    fn refresh_ink(&mut self, canvas: &Canvas, scale: f32) {
        let key = InkKey::of(canvas);
        let ink = &canvas.ink;

        // How far the sheet may scroll before a stroke left out of the build could be back on screen: the
        // culling rectangle is grown by this, and the ink is drawn again once the sheet has moved past it.
        let slack = CULL_SLACK / (ink.zoom * scale).max(f32::MIN_POSITIVE);
        let stale = moved(self.ink_visible, ink.visible) > slack;

        if self.ink_key.as_ref() == Some(&key) && !stale {
            return;
        }

        self.ink.clear();
        self.ink_key = Some(key);
        self.ink_visible = ink.visible;

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            report::rebuilt();
        }

        let list = ink.strokes.as_slice();
        let mapping = Mapping::of(canvas);
        let cull = grow(ink.visible, slack);
        let in_hand = |index: usize| ink.selected.get(index).copied().unwrap_or(false);

        for (index, stroke) in list.iter().enumerate() {
            if in_hand(index) || !stroke.visible_in(cull) {
                continue;
            }

            fill(
                &mut self.ink,
                stroke,
                &mapping,
                (0.0, 0.0),
                ink_colour(stroke.color),
            );
        }

        // The ink in hand, where a drag has taken it, and once more in the selection's colour.
        if ink.selection.is_some() || ink.offset != (0.0, 0.0) {
            for (index, stroke) in list.iter().enumerate() {
                if !in_hand(index) {
                    continue;
                }

                fill(
                    &mut self.ink,
                    stroke,
                    &mapping,
                    ink.offset,
                    ink_colour(stroke.color),
                );

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

        // The ink changed: every slot is stale until its frame uploads it. The slot from three
        // frames ago is the one the compositor has finished with, so a slot is written only when
        // its turn comes.
        self.ink_slots_valid = [false; 3];
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

    /// The stroke under the pen, rebuilt whenever it grew.
    ///
    /// The open stroke is the one piece of ink that moves, and like the finished ink it is drawn as its
    /// *outline* (see [`fill`]). The outline is already in the stroke: the view closes it for the sheet it
    /// is drawing every frame (`Stroke::close_live`, from `describe_ink`), because the edge at a point is
    /// set by the points on either side of it and so the last point's edge changes when the next one
    /// arrives. Appending could only ever draw overlapping pieces, and those pieces compose — a
    /// translucent band laid down that way darkens into an opaque one as the pen moves.
    ///
    /// What is rebuilt here is the tessellation of one stroke, once per frame the pen is down. It is
    /// bounded by the stroke being drawn and by nothing else, and a *still* page never pays for it: the
    /// point count is what the early return below is for.
    fn open_stroke(&mut self, canvas: &Canvas) {
        let Some(stroke) = &canvas.ink.open else {
            self.open.clear();
            self.open_id = 0;
            self.open_points = 0;
            return;
        };

        let points = &stroke.points;
        let colour = ink_colour(stroke.color);
        let mapping = Mapping::of(canvas);

        // A new stroke, a different colour, a page turn, or a stroke that lost points: start over.
        if self.open_id != canvas.ink.open_id
            || self.open_colour != stroke.color
            || self.open_paper != canvas.ink.paper
            || self.open_rotation != canvas.ink.rotation
            || points.len() < self.open_points
        {
            self.open.clear();
            self.open_id = canvas.ink.open_id;
            self.open_colour = stroke.color;
            self.open_paper = canvas.ink.paper;
            self.open_rotation = canvas.ink.rotation;
            self.open_points = 0;
        }

        if points.is_empty() {
            self.open_points = 0;
            return;
        }

        // Nothing to do while the pen has not moved: the triangles already uploaded are still right.
        if self.open_points == points.len() && !self.open.is_empty() {
            return;
        }

        self.open.clear();
        fill(&mut self.open, stroke, &mapping, (0.0, 0.0), colour);
        self.open_points = points.len();
    }

    /// Uploads the finished ink into the current frame's buffer.
    fn upload_ink(&mut self, slot: usize) {
        let buffer = self.ink_buffers[slot].take();
        self.ink_buffers[slot] = Self::upload(&self.device, &self.ink, buffer);
    }

    /// Uploads the open stroke into the current frame's buffer.
    fn upload_open(&mut self, slot: usize) {
        let buffer = self.open_buffers[slot].take();
        self.open_buffers[slot] = Self::upload(&self.device, &self.open, buffer);
    }

    /// Copies `vertices` into `buffer`, growing it when it is too small.
    fn upload<T>(
        device: &ProtocolObject<dyn MTLDevice>,
        vertices: &[T],
        buffer: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    ) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
        if vertices.is_empty() {
            return None;
        }

        let bytes = std::mem::size_of_val(vertices);
        let buffer = match buffer.filter(|buffer| buffer.length() >= bytes) {
            Some(buffer) => buffer,
            None => {
                device.newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)?
            }
        };

        // SAFETY: a buffer of at least this many bytes, written once before anything reads it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                vertices.as_ptr().cast::<u8>(),
                buffer.contents().as_ptr().cast::<u8>(),
                bytes,
            );
        }

        Some(buffer)
    }

    /// The page's texture, uploaded when the document hands over a new rasterisation of it.
    fn page_texture(
        &mut self,
        canvas: &Canvas,
    ) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        let page = canvas.page.as_ref()?;
        let key = Arc::as_ptr(&page.image) as usize;

        if self.page_texture.as_ref().map(|(cached, _)| *cached) == Some(key) {
            return self
                .page_texture
                .as_ref()
                .map(|(_, texture)| texture.clone());
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
                    size: MTLSize {
                        width,
                        height,
                        depth: 1,
                    },
                },
                0,
                bytes,
                width * 4,
            );
        }

        self.page_texture = Some((key, texture));

        self.page_texture
            .as_ref()
            .map(|(_, texture)| texture.clone())
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

/// How far the body's shadow is dropped, in logical pixels — the Windows overlay's own offset, so that the
/// two builds' cursors are one drawing.
const BODY_SHADOW_DROP: [f32; 2] = [3.0, 3.6];

/// How opaque the body's shadow is at full lean: the widest and faintest of the three passes.
const BODY_SHADOW_ALPHA: f32 = 0.26;

/// The pen, where the nib is: the body when it leans, and the nib's own disc.
///
/// The same drawing the Windows overlay makes, from the same geometry (`note_ui::cursor`) and the same
/// constants — so a pen looks the same on both platforms. It is drawn here, in the canvas's pass, rather
/// than in a window of its own: a cursor is above the ink and below the interface, and only this pass is.
fn cursor_vertices(vertices: &mut Vec<Vertex>, mark: &Mark, scale: f32) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let position = mark.cursor.position();
    let nib = (position[0] * scale, position[1] * scale);
    let fade = mark.cursor.body_fade();

    if let Some(body) = mark.cursor.body_shape() {
        // Shadow, soft edge, body: the order and the colours the Windows overlay uses.
        polygon(
            vertices,
            &shifted(body.outline(0.0), BODY_SHADOW_DROP),
            scale,
            mark.halo,
            BODY_SHADOW_ALPHA * fade,
        );
        polygon(
            vertices,
            &body.outline(BODY_HALO_GROW),
            scale,
            mark.halo,
            BODY_HALO_ALPHA * fade,
        );
        polygon(
            vertices,
            &body.outline(0.0),
            scale,
            mark.colour,
            BODY_ALPHA * fade,
        );
    }

    let radius = if mark.nib_radius.is_finite() {
        mark.nib_radius.clamp(NIB_RADIUS_MIN, NIB_RADIUS_MAX)
    } else {
        NIB_RADIUS_MIN
    } * scale;

    // Over a bloom, so that a nib of the paper's own colour can still be found.
    disc(
        vertices,
        nib,
        radius + NIB_BLOOM_SPREAD * scale,
        shaded(mark.halo, NIB_BLOOM_ALPHA),
    );
    disc(vertices, nib, radius, shaded(mark.colour, NIB_ALPHA));
}

/// One of the body's passes: its outline, filled with the nonzero rule like any other polygon.
fn polygon(
    vertices: &mut Vec<Vertex>,
    outline: &[[f32; 2]; 7],
    scale: f32,
    colour: u32,
    alpha: f32,
) {
    if alpha <= 0.0 {
        return;
    }

    let points: Vec<(f32, f32)> = outline
        .iter()
        .map(|corner| (corner[0] * scale, corner[1] * scale))
        .collect();

    tessellate(vertices, &points, shaded(colour, alpha));
}

/// The body's outline, moved by a drop in logical pixels.
fn shifted(outline: [[f32; 2]; 7], by: [f32; 2]) -> [[f32; 2]; 7] {
    let mut moved = outline;

    for corner in &mut moved {
        corner[0] += by[0];
        corner[1] += by[1];
    }

    moved
}

/// A colour and an opacity, as the app's own colour type.
fn shaded(colour: u32, alpha: f32) -> gpui_kit::Hsla {
    let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;

    gpui_kit::rgba(((colour & 0x00FF_FFFF) << 8) | alpha).into()
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

/// How far the sheet may be scrolled before the ink's culling rectangle is drawn again, in device pixels.
///
/// The ink is baked in the paper's units and placed by a transform, so scrolling is not a reason to rebuild
/// anything — but a build leaves out the strokes that were off screen, and scrolling towards one has to
/// bring it back. Growing the culling rectangle by this much is what bounds how far the sheet can move
/// while the ink stays as it is: several screens of scrolling, and a rebuild that lands once in that
/// distance is not a rebuild anybody sees.
const CULL_SLACK: f32 = 4096.0;

/// What the finished ink's triangles were built from.
///
/// Every triangle is in the **paper's own units** — the same numbers at every zoom and every scroll — so the
/// strokes, the detail they were built at and the shape of the page are the whole of it. Where the paper is
/// drawn is *not* here: it is [`Transform`], applied by the shader, which is what makes a scroll a uniform
/// update instead of a page of geometry rebuilt a hundred times a second.
#[derive(PartialEq)]
struct InkKey {
    len: usize,
    first: Option<usize>,
    last: Option<usize>,
    revision: u64,
    rotation: u8,
    paper: [u32; 2],
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
            rotation: ink.rotation,
            paper: [ink.paper.0.to_bits(), ink.paper.1.to_bits()],
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

/// A culling rectangle, grown by `by` on every side.
fn grow(rect: [f32; 4], by: f32) -> [f32; 4] {
    [rect[0] - by, rect[1] - by, rect[2] + by, rect[3] + by]
}

/// How far one rectangle's edges are from another's: the largest movement of any of the four.
///
/// A rectangle of infinities — "everything is visible" — is infinitely far from anything else, which is
/// `NaN` once subtracted; `f32::max` keeps the zero instead, so the ink is not redrawn for a rectangle that
/// cannot be compared.
fn moved(from: [f32; 4], to: [f32; 4]) -> f32 {
    (0..4)
        .map(|edge| (to[edge] - from[edge]).abs())
        .fold(0.0, f32::max)
}

/// The ink's coordinates to the sheet's *drawn* ones: the same numbers at every zoom, turned the way the
/// page is turned.
///
/// This is where the ink is baked, and it is the reason a scroll costs nothing: a stroke's outline is in
/// the paper's own units, so the triangles built from it are the same triangles wherever the paper sits and
/// however large it is drawn. Where it sits is [`Transform`], which the shader applies — see
/// `note_ui::ink_layer::canvas`, whose contract this follows.
struct Mapping {
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
            base,
            step_x: (along_x.0 - base.0, along_x.1 - base.1),
            step_y: (along_y.0 - base.0, along_y.1 - base.1),
        }
    }

    fn point(&self, [x, y]: [f32; 2], shift: (f32, f32)) -> (f32, f32) {
        let (x, y) = (x + shift.0, y + shift.1);
        let across = self.base.0 + x * self.step_x.0 + y * self.step_y.0;
        let down = self.base.1 + x * self.step_x.1 + y * self.step_y.1;

        (across, down)
    }
}

/// Where the sheet's drawn coordinates land in the window: a scale and a shift, in device pixels.
///
/// The one thing a scroll, a zoom or a change of screen changes. `note_ui`'s canvas says where the paper is
/// drawn and how large, and this is that "where" in the form the shader takes it — which is why panning is
/// a uniform and not a page of triangles.
#[repr(C)]
#[derive(Clone, Copy)]
struct Transform {
    scale: [f32; 2],
    shift: [f32; 2],
}

impl Transform {
    /// Window pixels as they are: the desk, the page's quad, the pen's mark — everything already placed.
    const WINDOW: Transform = Transform {
        scale: [1.0, 1.0],
        shift: [0.0, 0.0],
    };

    /// The sheet, where the view has put it and as large as it is drawn.
    fn of(canvas: &Canvas) -> Self {
        let scale = if canvas.scale > 0.0 {
            canvas.scale
        } else {
            1.0
        };
        let ink = &canvas.ink;

        Transform {
            scale: [ink.zoom * scale, ink.zoom * scale],
            shift: [ink.origin.0 * scale, ink.origin.1 * scale],
        }
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
    /// Draws one description of the canvas, and the pen's mark over it. `false` when there was no drawable.
    pub(super) fn draw(&mut self, canvas: &Canvas, mark: Option<&Mark>) -> bool {
        let scale = if canvas.scale > 0.0 {
            canvas.scale
        } else {
            1.0
        };

        // A drawable that is not ready is a frame to skip, not work to do: the pen's pump is on
        // the main thread, and building a frame nobody can see would hold the next reading.
        let Some(drawable) = self.layer.nextDrawable() else {
            return false;
        };

        let Some(msaa) = self.multisample() else {
            return false;
        };

        // The buffer slot this frame writes. It advances only when a frame is actually committed, so
        // the slot from three frames ago is the one the compositor has finished with.
        let slot = self.buffer_slot;

        // The pen's mark: above the ink, below the interface. Built before the trace, so that "no cursor
        // drawn" and "no cursor to draw" can be told apart.
        let mut pointer = Vec::new();
        if let Some(mark) = mark {
            cursor_vertices(&mut pointer, mark, scale);
        }

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            report::drawn(
                self.ink.len(),
                canvas
                    .ink
                    .open
                    .as_deref()
                    .map_or(0, |stroke| stroke.outline.len()),
                pointer.len(),
            );
        }
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

        self.refresh_ink(canvas, scale);
        if !self.ink_slots_valid[slot] {
            self.upload_ink(slot);
            self.ink_slots_valid[slot] = true;
        }
        self.open_stroke(canvas);
        self.upload_open(slot);

        // Where the paper's own units land in the window: the one thing a scroll, a zoom or a change of
        // screen changes, and the reason none of them rebuilds a triangle.
        let sheet = Transform::of(canvas);
        let page = self
            .page_texture(canvas)
            .map(|texture| (page_quad(canvas, scale), texture));

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
            self.solid(
                &encoder,
                &buffer,
                behind.len(),
                width,
                height,
                Transform::WINDOW,
            );
        }

        // The document's page.
        if let Some((quad, texture)) = &page {
            if let Some(buffer) = self.buffer(quad) {
                encoder.setRenderPipelineState(&self.page);
                self.bind(&encoder, &buffer, width, height, Transform::WINDOW);

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

        // The finished ink, then the stroke under the pen: the order the page was painted in when Core
        // Graphics drew it.
        if let Some(buffer) = &self.ink_buffers[slot] {
            self.solid(&encoder, buffer, self.ink.len(), width, height, sheet);
        }

        if let Some(buffer) = &self.open_buffers[slot] {
            self.solid(&encoder, buffer, self.open.len(), width, height, sheet);
        }

        // The pen, over everything the canvas drew: a cursor is the one thing on the sheet that has to be
        // visible above the ink it is about to write.
        if let Some(buffer) = self.buffer(&pointer) {
            self.solid(
                &encoder,
                &buffer,
                pointer.len(),
                width,
                height,
                Transform::WINDOW,
            );
        }

        encoder.endEncoding();

        // SAFETY: `presentDrawable:` is the command buffer's own method for the drawable it was handed.
        unsafe {
            let _: () = objc2::msg_send![&*buffer, presentDrawable: &*drawable];
        }

        buffer.commit();

        self.buffer_slot = (self.buffer_slot + 1) % self.ink_buffers.len();

        true
    }
}

impl Surface {
    /// Uploads one of the small runs — the desk, the page's quad, the highlight, the pen's mark —
    /// and hands back the buffer to bind.
    ///
    /// One allocation per run per frame, a few kilobytes each. The two large runs — the finished ink
    /// and the open stroke — are kept in `ink_buffers`/`open_buffers` instead, because they are the
    /// ones whose size makes an allocation per frame worth avoiding.
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

    /// Binds a run of vertices, the window they are placed in, and how the paper's units land in it.
    fn bind(
        &self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        buffer: &ProtocolObject<dyn MTLBuffer>,
        width: f32,
        height: f32,
        transform: Transform,
    ) {
        let viewport: [f32; 2] = [width, height];
        let transform: [f32; 4] = [
            transform.scale[0],
            transform.scale[1],
            transform.shift[0],
            transform.shift[1],
        ];

        // SAFETY: the buffer holds the vertices the draw call reads, and the two values after it are bytes
        // the encoder copies before this function returns.
        unsafe {
            encoder.setVertexBuffer_offset_atIndex(Some(buffer), 0, 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(viewport.as_ptr() as *mut c_void).unwrap(),
                std::mem::size_of_val(&viewport),
                1,
            );
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(transform.as_ptr() as *mut c_void).unwrap(),
                std::mem::size_of_val(&transform),
                2,
            );
        }
    }

    /// One run of coloured vertices, in one draw call.
    fn solid(
        &self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        buffer: &ProtocolObject<dyn MTLBuffer>,
        count: usize,
        width: f32,
        height: f32,
        transform: Transform,
    ) {
        encoder.setRenderPipelineState(&self.solid);
        self.bind(encoder, buffer, width, height, transform);

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
/// told apart. A third says how often the ink was *built*: a scroll, a zoom or a still frame has no
/// business rebuilding a page of triangles, and a count that moves while the hand does nothing is the
/// regression this module exists to keep out (see [`Transform`]).
mod report {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    static DRAWS: AtomicU64 = AtomicU64::new(0);
    static FINISHED: AtomicU64 = AtomicU64::new(0);
    static OPEN: AtomicU64 = AtomicU64::new(0);
    static POINTER: AtomicU64 = AtomicU64::new(0);
    static REBUILDS: AtomicU64 = AtomicU64::new(0);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    /// One build of the finished ink's triangles.
    pub fn rebuilt() {
        REBUILDS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn drawn(finished: usize, open: usize, pointer: usize) {
        DRAWS.fetch_add(1, Ordering::Relaxed);
        FINISHED.store(finished as u64, Ordering::Relaxed);
        OPEN.store(open as u64, Ordering::Relaxed);
        POINTER.store(pointer as u64, Ordering::Relaxed);

        let mut last = LAST.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();

        if last.is_some_and(|at| now.duration_since(at).as_millis() < 1000) {
            return;
        }

        *last = Some(now);

        println!(
            "canvas: {} draws/s · {} finished vertices · {} under the pen · {} in the cursor · {} rebuilds/s",
            DRAWS.swap(0, Ordering::Relaxed),
            FINISHED.load(Ordering::Relaxed),
            OPEN.load(Ordering::Relaxed),
            POINTER.load(Ordering::Relaxed),
            REBUILDS.swap(0, Ordering::Relaxed)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scroll moves the paper and leaves the strokes exactly as they were, so the ink is *not* drawn
    /// again: the triangles are in the paper's own units and the scroll is a [`Transform`]. That difference
    /// is the whole of why scrolling stutters or does not — the triangles used to be in device pixels, and
    /// every reading of a trackpad rebuilt them.
    #[test]
    fn a_scroll_is_a_transform_and_not_a_new_ink_key() {
        let mut canvas = Canvas::default();
        let before = InkKey::of(&canvas);
        let placed = Transform::of(&canvas);

        canvas.ink.origin = (0.0, -120.0);

        assert!(
            before == InkKey::of(&canvas),
            "a scroll asked for the ink to be drawn again"
        );
        assert!(
            placed.shift != Transform::of(&canvas).shift,
            "a scroll left the sheet where it was"
        );
    }

    /// A zoom is the same story, and so is the screen's own scale: how large the paper is drawn is the
    /// transform. The *detail* a zoom asks for is another matter — `note_ui` says so with the ink's
    /// revision, which is in the key — so a zoom that crosses a rung still draws the ink again.
    #[test]
    fn a_zoom_is_a_transform_and_not_a_new_ink_key() {
        let mut canvas = Canvas::default();
        let before = InkKey::of(&canvas);
        let placed = Transform::of(&canvas);

        canvas.ink.zoom = 1.5;

        assert!(
            before == InkKey::of(&canvas),
            "a zoom asked for the ink to be drawn again"
        );
        assert!(
            placed.scale != Transform::of(&canvas).scale,
            "a zoom left the sheet its own size"
        );

        canvas.ink.revision = 1;
        assert!(
            before != InkKey::of(&canvas),
            "a new detail left the ink's key alone"
        );
    }

    /// A turn of the page *is* a change to the ink: the paper's own units are turned with it, and every
    /// triangle moves.
    #[test]
    fn a_turn_of_the_page_is_a_new_ink_key() {
        let mut canvas = Canvas::default();
        let before = InkKey::of(&canvas);

        canvas.ink.rotation = 1;
        assert!(
            before != InkKey::of(&canvas),
            "a turn left the ink's key alone"
        );
    }

    /// The other half: a frame that changed nothing is the same key, so the ink is not rebuilt for being
    /// asked twice — the page's triangles are tens of thousands of vertices.
    #[test]
    fn the_same_page_is_the_same_key() {
        let canvas = Canvas::default();

        assert!(InkKey::of(&canvas) == InkKey::of(&canvas));
    }

    /// The transform places the paper exactly where the old mapping did: a point of the paper lands at
    /// `(origin + drawn * zoom) * scale`, which is the arithmetic that drew the sheet when the triangles
    /// were in device pixels. Baking them in the paper's units must not move anything.
    #[test]
    fn the_transform_lands_the_paper_where_the_mapping_did() {
        let mut canvas = Canvas::default();
        canvas.scale = 2.0;
        canvas.ink.paper = (720.0, 1018.0);
        canvas.ink.zoom = 1.5;
        canvas.ink.origin = (24.0, -120.0);

        let mapping = Mapping::of(&canvas);
        let transform = Transform::of(&canvas);

        for point in [[0.0, 0.0], [10.0, 20.0], [719.0, 1017.0]] {
            let drawn = mapping.point(point, (0.0, 0.0));
            let placed = (
                drawn.0 * transform.scale[0] + transform.shift[0],
                drawn.1 * transform.scale[1] + transform.shift[1],
            );
            let expected = (
                (canvas.ink.origin.0 + point[0] * canvas.ink.zoom) * canvas.scale,
                (canvas.ink.origin.1 + point[1] * canvas.ink.zoom) * canvas.scale,
            );

            assert!(
                (placed.0 - expected.0).abs() < 0.001 && (placed.1 - expected.1).abs() < 0.001,
                "{point:?} landed at {placed:?}, not {expected:?}"
            );
        }
    }

    /// The culling rectangle is the one thing that moves while the ink stays valid, and the ink is drawn
    /// again once it has moved further than the slack the cull was grown by.
    #[test]
    fn a_scroll_that_leaves_the_culling_behind_asks_for_the_ink_again() {
        assert_eq!(moved([0.0; 4], [0.0; 4]), 0.0);
        assert_eq!(
            moved([0.0, 0.0, 100.0, 100.0], [0.0, -50.0, 100.0, 50.0]),
            50.0
        );

        // "Everything is visible" cannot be compared with itself — the difference of two infinities is
        // `NaN` — and a rectangle that cannot be compared must not ask for a redraw on every frame.
        let everything = [
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::INFINITY,
        ];
        assert_eq!(moved(everything, everything), 0.0);

        assert_eq!(grow([0.0, 0.0, 10.0, 10.0], 5.0), [-5.0, -5.0, 15.0, 15.0]);
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

    /// A stroke is painted **once**, however many pieces its outline was cut into.
    ///
    /// This is the bug that made a highlighter read as a smear. Ink is translucent
    /// (`HIGHLIGHTER_ALPHA`, 35%), and two triangles over the same pixel compose: 35% over 35% is 58%,
    /// three is 72%. The ribbon this used to draw laid a quad per segment *and a disc at every joint*,
    /// and at the sampling the app uses (`resample_spacing`, 0.75 logical px) that was a chain of discs
    /// a few tenths of a pixel apart under a band 18 wide — six to nineteen layers of 35%, which came
    /// out 91-100% opaque. The shorter the stroke, the more the overlap *varied*, which is why a short
    /// mark looked broken where a long one merely looked dark.
    ///
    /// A stroke is now drawn as its outline, tessellated once with the nonzero rule ([`tessellate`]) —
    /// the polygon the model already caches, which cannot overlap itself. This test is the property that
    /// follows: the triangles cover the capsule the band *is*, and no more. The old ribbon measured 6.6
    /// at this length; a band drawn once measures 1.
    #[test]
    fn a_highlighter_band_is_painted_exactly_once() {
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };

        // The width and the spacing the app ships with: the numbers a reader's hand meets. The width is
        // `ink::HIGHLIGHTER_WIDTH` (18 logical px) — spelled out because that constant is private to
        // `note-ui`.
        let width = 18.0_f32;
        let spacing = note_format::settings::Settings::default().resample_spacing;

        // A short mark: a few pixels of travel, which is what "too short" means in practice.
        let mut stroke = note_format::ink::Stroke::new(
            note_format::ink::InkPoint::new(0.0, 0.0, width),
            0xFFEB_3B00,
        );

        let mut x = 0.0;
        while x < 6.0 {
            x += spacing;
            stroke
                .points
                .push(note_format::ink::InkPoint::new(x, 0.0, width));
        }

        let mut vertices = Vec::new();
        // The outline is what a renderer draws, and the app builds it before the layer ever sees the
        // stroke (`describe_ink`), so the test does the same: a stroke with no outline draws nothing.
        stroke.close_at(1.0);
        fill(&mut vertices, &stroke, &mapping, (0.0, 0.0), colour());

        // What the band covers. A highlighter's outline is the *rectangle* the nib sweeps —
        // `ribbon_outline` offsets to either side of the line and joins the two edges, with no round
        // cap — so the area is the travel times the width, and nothing else.
        let travel = stroke.points.last().expect("a stroke").x;
        let covered = travel * width;

        let painted = area(&vertices);
        let overlap = painted / covered;

        // A tessellated polygon covers its own area once. Half again would already be one layer too
        // many for a 35% band; the old ribbon measured 6.6 at this length and 19 over a long stroke.
        assert!(
            (overlap - 1.0).abs() < 0.15,
            "the band is painted {overlap:.2}× over: at 35% ink that is {:.0}% opaque, not 35%",
            (1.0 - (1.0 - 0.35_f32).powf(overlap)) * 100.0
        );
        assert!(!vertices.is_empty(), "a short band drew nothing at all");
    }

    /// The same property for a stroke that doubles back over itself: the overlap is *inside* one
    /// polygon, and the nonzero rule keeps it ink rather than counting it twice.
    #[test]
    fn a_band_that_crosses_itself_is_still_one_layer() {
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };

        let width = 18.0_f32;
        let spacing = note_format::settings::Settings::default().resample_spacing;

        let mut stroke = note_format::ink::Stroke::new(
            note_format::ink::InkPoint::new(0.0, 0.0, width),
            0xFFEB_3B00,
        );

        // Out along the line, then back over it: the shape a reader makes when they go over a
        // highlight a second time in one stroke.
        let mut x = 0.0;
        while x < 8.0 {
            x += spacing;
            stroke
                .points
                .push(note_format::ink::InkPoint::new(x, 0.0, width));
        }
        while x > 0.0 {
            x -= spacing;
            stroke
                .points
                .push(note_format::ink::InkPoint::new(x.max(0.0), 0.0, width));
        }

        let mut vertices = Vec::new();
        stroke.close_at(1.0);
        fill(&mut vertices, &stroke, &mapping, (0.0, 0.0), colour());

        // The band is 8 px of travel wide, however many times the stroke covered it: the nonzero rule
        // fills the region once, and a polygon that crosses itself is ink rather than a hole.
        let covered = 8.0 * width;
        let overlap = area(&vertices) / covered;

        assert!(
            (overlap - 1.0).abs() < 0.15,
            "a doubled-back band is painted {overlap:.2}× over"
        );
    }

    /// A square comes out as a square: covered once, and its area is its own.
    #[test]
    fn a_square_is_filled_once() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let mut vertices = Vec::new();

        tessellate(&mut vertices, &square, colour());

        assert!(
            (area(&vertices) - 100.0).abs() < 0.01,
            "area {}",
            area(&vertices)
        );
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

        assert!(
            (area(&vertices) - 100.0).abs() < 0.01,
            "area {}",
            area(&vertices)
        );
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
