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

// Twelve bytes a corner: the two that place it, and the four that colour it packed into one word.
//
// The colour was four floats here — sixteen of the twenty-four bytes a vertex took — and every corner of
// every stroke carried its own copy of a colour the whole stroke already agrees on. A page of writing is
// about 1.7 million corners, which was forty megabytes uploaded three times over, on a surface that is
// re-drawn a hundred and forty-four times a second. Four bytes of unorm is 255 steps a channel, which is
// what the display has anyway.
struct Vertex { packed_float2 position; uchar4 colour; };
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
    out.position = place(float2(vertices[id].position) * transform.xy + transform.zw, viewport);
    out.colour = float4(vertices[id].colour) / 255.0;
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
///
/// Twelve bytes rather than twenty-four: the colour is four bytes of unorm rather than four floats, and it is
/// most of what a vertex used to be. A page of writing is ~1.7 million corners, so the floats were forty
/// megabytes of buffer per upload, three slots deep, for colours that 255 steps a channel describe exactly as
/// well as the display renders them (see [`Vertex::new`]).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Vertex {
    x: f32,
    y: f32,
    /// `r`, `g`, `b`, `a` in that order, 8 bits each — the order `uchar4` reads them in.
    colour: [u8; 4],
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
    /// The region of the paper the ink covers, in the paper's units: the visible rectangle grown by
    /// [`CULL_SPAN`] at the moment of the build.
    ///
    /// A frame reuses the ink while the screen still fits inside this (see [`fits`]). A scroll moves the screen
    /// around within it and a zoom inward shrinks the screen inside it, so neither costs a build; a scroll out
    /// of it or a zoom out of it is a build, because both show paper this does not cover.
    ink_covered: [f32; 4],
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
        // waited for. Whether to wait is the note's setting (see `Settings::vsync`), applied here as
        // the layer's default and moved by `Surface::set_vsync` when the switch is thrown. The
        // environment variable is what the trade was measured with before there was a switch, and it
        // still wins over the setting so an experiment does not need a note written to try it.
        let waits = std::env::var_os("CHEAP_NOTE_NO_VSYNC").is_none();
        layer.setDisplaySyncEnabled(waits);

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
            // Nothing built yet: an empty region, so the first frame always does.
            ink_covered: [
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ],
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
    /// Waits for the display's clock before a frame is shown, or stops waiting.
    ///
    /// One property of the layer, which is why this can be answered the moment the switch moves rather
    /// than at the next launch: `displaySyncEnabled` is consulted when a drawable is presented, not when
    /// the layer is made. The environment variable still overrides it — see `install` — because that is
    /// the *experiment*, and an experiment that could be overridden by a note setting would be a
    /// measurement of the note rather than of the toggle.
    pub(super) fn set_vsync(&mut self, on: bool) {
        if std::env::var_os("CHEAP_NOTE_NO_VSYNC").is_some() {
            return;
        }

        self.layer.setDisplaySyncEnabled(on);
    }

    /// Rebuilds the finished ink, when what the triangles are a function of has changed.
    ///
    /// Two reasons to build: the ink *is* different (a stroke added, a zoom rung, a turn of the page — the
    /// [`InkKey`]), or the region it was built for no longer covers what the screen shows ([`fits`]). The
    /// second is what has to be right for a scroll and a zoom to cost nothing, and it is a containment test
    /// rather than a distance for the reason [`CULL_SPAN`] gives.
    ///
    /// `true` when it rebuilt, which is the only thing that makes the uploaded buffers stale. A frame that
    /// reuses the ink must not upload it again: the triangle list has not changed, and copying a page's
    /// worth of it into a buffer three frames in flight — every frame of a pan — is megabytes a frame of
    /// `memcpy` for nothing. The slots keep their validity across such frames, so each of the three is
    /// written once per rebuild rather than once per frame.
    fn refresh_ink(&mut self, canvas: &Canvas) -> bool {
        let key = InkKey::of(canvas);
        let ink = &canvas.ink;

        // A build covers the visible rectangle grown by the margin; every frame after it is free while the
        // screen still fits inside that.
        let covered = fits(ink.visible, self.ink_covered);

        if self.ink_key.as_ref() == Some(&key) && covered {
            return false;
        }

        self.ink.clear();
        self.ink_key = Some(key);
        self.ink_covered = cull_of(ink.visible);

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            report::rebuilt();
        }

        let list = ink.strokes.as_slice();
        let mapping = Mapping::of(canvas);
        let cull = self.ink_covered;
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

        // The ink changed: every slot is stale until its frame uploads it. Said here so the build and the
        // invalidation cannot drift apart — the caller only has to know that a rebuild happened.
        self.ink_slots_valid = [false; 3];

        true
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
    /// A corner at `(x, y)`, in device pixels, in `colour`.
    ///
    /// The colour is packed here rather than carried as four floats, which is the whole of what made a vertex
    /// twelve bytes instead of twenty-four (see [`Vertex`]).
    fn new(x: f32, y: f32, colour: gpui_kit::Hsla) -> Self {
        let rgba = gpui_kit::Rgba::from(colour);
        let byte = |component: f32| (component.clamp(0.0, 1.0) * 255.0).round() as u8;

        Vertex {
            x,
            y,
            colour: [byte(rgba.r), byte(rgba.g), byte(rgba.b), byte(rgba.a)],
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
///
/// ## Why the edges are kept in an active list
///
/// The obvious way to find a band's crossings is to read every edge and keep the ones that reach it, which is
/// what this did first. That is O(n²): a long stroke's outline is two points a reading and a ribbon's ends, so
/// a stroke of a thousand readings is a polygon of two thousand, and the fill then did four million edge tests
/// for triangles that are mostly a few hundred. The bands *and* the edges are both numbers the pen made, and
/// the pen's readings are dense — so the quadratic term was not academic. Measured at the size a long stroke
/// reaches, it was tens of milliseconds a stroke, on the thread that draws.
///
/// So the edges are sorted by their lower end once, and each band is answered from the ones that have started
/// and not yet finished. That is the standard trapezoidal sweep and it is exact — the set of edges crossing a
/// band is the same set, found without re-reading the whole polygon. Same triangles, in every case tried:
/// self-crossing, shared heights, zero-width spikes, repeated points.
fn tessellate(vertices: &mut Vec<Vertex>, polygon: &[(f32, f32)], colour: gpui_kit::Hsla) {
    if polygon.len() < 3 {
        return;
    }

    let mut bands: Vec<f32> = polygon.iter().map(|point| point.1).collect();
    bands.sort_by(|left, right| left.total_cmp(right));
    bands.dedup_by(|left, right| (*left - *right).abs() < f32::EPSILON);

    // Every edge, with the span of heights it covers. A horizontal edge crosses nothing and is left out.
    struct Edge {
        low: f32,
        high: f32,
        from: (f32, f32),
        to: (f32, f32),
        winding: i32,
    }

    let mut edges: Vec<Edge> = Vec::with_capacity(polygon.len());

    for index in 0..polygon.len() {
        let (a, b) = (polygon[index], polygon[(index + 1) % polygon.len()]);

        if a.1 == b.1 {
            continue;
        }

        let (low, high) = if a.1 < b.1 { (a, b) } else { (b, a) };

        edges.push(Edge {
            low: low.1,
            high: high.1,
            from: a,
            to: b,
            winding: if b.1 > a.1 { 1 } else { -1 },
        });
    }

    // By where they start: the sweep takes them in as the bands climb, and never looks back.
    edges.sort_by(|left, right| left.low.total_cmp(&right.low));

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
    let mut active: Vec<usize> = Vec::new();
    let mut next = 0;

    for band in bands.windows(2) {
        let (bottom, top) = (band[0], band[1]);

        // `bands` is sorted, so `top >= bottom` already — the `.abs()` states that a band of no height is
        // what is being skipped, rather than a band that happens to be upside down.
        if (top - bottom).abs() < f32::EPSILON {
            continue;
        }

        // The edges that begin at or below this band's top have started.
        while next < edges.len() && edges[next].low <= top {
            active.push(next);
            next += 1;
        }

        let middle = (bottom + top) * 0.5;
        crossings.clear();

        for &index in active.iter() {
            let edge = &edges[index];

            // A band's ends are vertex heights, so an edge either covers it whole or does not reach it.
            if edge.low > bottom || edge.high < top {
                continue;
            }

            let at = |y: f32| {
                edge.from.0
                    + (edge.to.0 - edge.from.0) * (y - edge.from.1) / (edge.to.1 - edge.from.1)
            };

            crossings.push(Crossing {
                middle: at(middle),
                winding: edge.winding,
                bottom: at(bottom),
                top: at(top),
            });
        }

        // The edges that ended at or below this band's top reach no band above it.
        active.retain(|&index| edges[index].high > top);

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

/// How far the sheet may move before the built ink is thrown away, as a fraction of the screen.
///
/// The model, stated once: the ink is built for a **region of the paper**, and a frame reuses it while the
/// region the screen shows still fits inside the one it was built for. Scrolling and zooming both change the
/// visible region, so both are answered by the same test — *does the new visible rectangle fit in the old
/// culled one* — and only a rectangle that no longer fits pays for a build.
///
/// **The old test was a distance, and it was the wrong shape.** It asked whether any edge of the visible
/// rectangle had moved more than `SCREEN * span`, which is a fine test for a scroll and a bad one for a zoom:
/// zooming does not *move* the edges, it moves some in and some out, and the largest movement is proportional
/// to the screen rather than to the margin. A twenty-percent zoom moved an edge by more than the margin, so a
/// zoom rebuilt every frame — and each rebuild stored a smaller rectangle, so the next one was smaller still.
/// That is the fourteen-fold swing in the vertex count a trace showed: 413,964 to 5,867,880, for a reader who
/// was only zooming.
///
/// A fraction of the screen rather than a distance in pixels, because it is the *screen* the margin is for:
/// room to scroll between builds, in the units the scrolling happens in.
const CULL_SPAN: f32 = 0.5;

/// The region of the paper the ink is built for: the visible rectangle, grown by [`CULL_SPAN`] of itself.
///
/// Grown on every side, so a scroll in any direction — and a zoom around any point — has room inside it. The
/// growth is a fraction of the *visible extent*, which is what makes the same constant right at every zoom: a
/// screen at 400% shows a quarter of the paper and gets a quarter of the margin, and the margin is still half a
/// screen of scrolling in device pixels.
fn cull_of(visible: [f32; 4]) -> [f32; 4] {
    let (width, height) = (visible[2] - visible[0], visible[3] - visible[1]);
    let (across, down) = (width * CULL_SPAN, height * CULL_SPAN);

    [
        visible[0] - across,
        visible[1] - down,
        visible[2] + across,
        visible[3] + down,
    ]
}

/// Whether `visible` still fits inside the region the ink was built for.
///
/// The whole test, and the reason it is containment rather than distance: a rectangle that fits costs nothing,
/// whatever moved to get there, and one that does not is a build. A scroll inside the margin fits; a zoom
/// inward fits; a zoom outward does not, because it shows paper the build never covered.
fn fits(visible: [f32; 4], built_for: [f32; 4]) -> bool {
    visible[0] >= built_for[0]
        && visible[1] >= built_for[1]
        && visible[2] <= built_for[2]
        && visible[3] <= built_for[3]
}

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

        // Only a rebuild makes the uploaded buffers stale, and `refresh_ink` invalidates them itself when
        // it makes one. A pan or a zoom that the ink already covers rebuilds nothing, so there is nothing
        // to upload either: the list is byte for byte what the buffers hold, and copying a page's worth of
        // it again into each of three buffers in flight is the one cost a still page has.
        self.refresh_ink(canvas);
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

    /// A frame that reuses the ink must not upload it again.
    ///
    /// `refresh_ink` returned nothing and the caller invalidated all three slots on every frame, so a pan
    /// — which rebuilds no triangles at all, because a scroll is a [`Transform`] — copied the whole page's
    /// worth of vertex data into each of the three buffers in flight, every frame. At 144 Hz on a dense page
    /// that is the largest cost a still page had. The rule is that the invalidation belongs to the build.
    #[test]
    fn a_reused_ink_is_not_uploaded_again() {
        // The state the draw loop keeps, without a device: valid slots and the key they were built for.
        let mut slots = [true; 3];
        let mut key: Option<InkKey> = None;
        let covered = [0.0, 0.0, 800.0, 600.0];
        let visible = [10.0, 10.0, 400.0, 300.0];

        // The build: a key that is new, so every slot goes stale.
        let refresh = |key: &mut Option<InkKey>, slots: &mut [bool; 3]| {
            if key.is_some() {
                return;
            }
            *key = Some(InkKey::of(&Canvas::default()));
            *slots = [false; 3];
        };

        refresh(&mut key, &mut slots);
        assert_eq!(
            slots, [false; 3],
            "a build left a stale buffer marked valid"
        );

        // Three frames of a pan: the ink is reused, so no slot is invalidated and nothing is uploaded.
        for slot in [0, 1, 2] {
            refresh(&mut key, &mut slots);
            assert_eq!(
                slots, [false; 3],
                "frame {slot} of a pan marked a buffer valid without uploading it"
            );
        }

        // And the ink still covers the screen, which is what lets a pan reuse it.
        assert!(
            fits(visible, covered),
            "a screen inside the covered rectangle asked for a rebuild"
        );
    }

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
        // Built in the initializer rather than field by field: `Canvas`'s `Default` is what a frame starts
        // from, and everything this needs is `scale`, a paper, a zoom and an origin — the rest staying at
        // their defaults is the point, since a transform that depended on them would be a bug.
        let mut canvas = Canvas {
            scale: 2.0,
            ..Canvas::default()
        };
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

    /// A build covers the screen plus a margin, and a frame reuses it while the screen still fits inside.
    ///
    /// The model in one test: the ink is built for a *region*, and the question each frame asks is whether the
    /// visible rectangle is still inside it. A scroll within the margin fits; a zoom inward fits, because the
    /// screen it shows is smaller; a zoom outward does not, because it shows paper the build never covered.
    #[test]
    fn a_build_is_reused_while_the_screen_fits_inside_it() {
        let screen = [100.0, 200.0, 1100.0, 1000.0];
        let covered = cull_of(screen);

        assert!(
            fits(screen, covered),
            "the screen it was built for always fits"
        );

        // A scroll of half the margin, in every direction at once.
        let scrolled = [100.0 + 100.0, 200.0 + 100.0, 1100.0 + 100.0, 1000.0 + 100.0];
        assert!(
            fits(scrolled, covered),
            "a scroll inside the margin is free"
        );

        // Out past the margin: the ink does not cover it.
        let away = [100.0 + 1000.0, 200.0, 1100.0 + 1000.0, 1000.0];
        assert!(!fits(away, covered), "a scroll out of the margin builds");

        // A zoom in, around the middle: a smaller rectangle inside the same place.
        let closer = [400.0, 400.0, 800.0, 800.0];
        assert!(fits(closer, covered), "zooming in fits inside the build");

        // A zoom out, around the middle: smaller numbers on the paper, so a *larger* rectangle.
        let further = [-600.0, -400.0, 1800.0, 1600.0];
        assert!(
            !fits(further, covered),
            "zooming out shows paper the build never covered"
        );
    }

    /// A build always covers the rectangle it was asked for, so a frame never builds twice in a row.
    ///
    /// The bug this rules out is subtle and was real: a build that stored a rectangle *smaller* than the one
    /// it had just been given would fail its own containment test on the very next frame, and rebuild forever.
    #[test]
    fn a_build_covers_the_rectangle_it_was_asked_for() {
        for visible in [
            [0.0, 0.0, 640.0, 906.0],
            [-2000.0, 500.0, -1000.0, 700.0],
            [0.0, 0.0, 0.0, 0.0],
        ] {
            let covered = cull_of(visible);

            assert!(
                fits(visible, covered),
                "{visible:?} is not inside the region built for it, {covered:?}"
            );
            // And the region is larger, never smaller, unless the rectangle had no extent at all.
            if visible[2] > visible[0] && visible[3] > visible[1] {
                assert!(covered[0] < visible[0] && covered[2] > visible[2]);
                assert!(covered[1] < visible[1] && covered[3] > visible[3]);
            }
        }
    }

    /// The margin is a fraction of the screen, so it is the same *screenful* of scrolling at every zoom.
    ///
    /// A distance in device pixels would make the margin a different fraction at every zoom — and the test
    /// that matters is what a zoom does to it. At 400% the paper is four times as large, so the screen shows a
    /// quarter of it if the visible rectangle is in paper units: the margin shrinks by the same factor and
    /// stays half a screen of scrolling.
    #[test]
    fn the_margin_is_a_screenful_at_every_zoom() {
        // The same screen, at two zooms, in the paper's units.
        let near = [0.0, 0.0, 720.0, 1018.0];
        let far = [0.0, 0.0, 180.0, 254.5];

        for visible in [near, far] {
            let covered = cull_of(visible);
            let margin = visible[0] - covered[0];
            let width = visible[2] - visible[0];

            assert!(
                (margin - width * CULL_SPAN).abs() < 1e-3,
                "the margin is a fraction of the screen at every zoom: {margin} against {width}"
            );
        }
    }
}

#[cfg(test)]
mod fill_tests {
    use super::*;

    /// The fill answers the same as the obvious implementation, which reads every edge for every band.
    ///
    /// The sweep keeps an active list of edges and drops the ones that have finished; the obvious way reads
    /// the whole polygon each time. They have to make the *same triangles*: the sweep is only a cheaper way
    /// of finding the same crossings, so any disagreement is a stroke drawn wrongly rather than slowly.
    ///
    /// The shapes are the ones a ribbon's outline actually makes: a square, a stroke that crosses itself, one
    /// with several points at one height, one tall edge beside short ones, a zero-width spike where the pen
    /// turned round, repeated readings, and a star.
    #[test]
    fn the_sweep_finds_the_crossings_the_slow_way_does() {
        let shapes: [(&str, Vec<(f32, f32)>); 7] = [
            (
                "square",
                vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
            ),
            (
                "bowtie, crossing itself",
                vec![(0.0, 0.0), (10.0, 10.0), (10.0, 0.0), (0.0, 10.0)],
            ),
            (
                "several points at one height",
                vec![
                    (0.0, 0.0),
                    (4.0, 0.0),
                    (8.0, 0.0),
                    (8.0, 5.0),
                    (4.0, 5.0),
                    (0.0, 5.0),
                ],
            ),
            (
                "one tall edge beside short ones",
                vec![
                    (0.0, 0.0),
                    (0.0, 100.0),
                    (1.0, 90.0),
                    (2.0, 80.0),
                    (3.0, 70.0),
                    (4.0, 60.0),
                    (5.0, 50.0),
                    (6.0, 40.0),
                    (7.0, 30.0),
                    (8.0, 20.0),
                    (9.0, 10.0),
                ],
            ),
            (
                "a spike where the pen turned round",
                vec![(0.0, 0.0), (5.0, 0.0), (5.0, 20.0), (5.0, 0.0)],
            ),
            (
                "repeated reading",
                vec![(1.0, 1.0), (5.0, 1.0), (5.0, 1.0), (5.0, 9.0), (1.0, 9.0)],
            ),
            (
                "a star, crossing itself three times",
                (0..10)
                    .map(|step| {
                        let angle = step as f32 * std::f32::consts::TAU * 2.0 / 10.0;
                        (50.0 + 40.0 * angle.cos(), 50.0 + 40.0 * angle.sin())
                    })
                    .collect(),
            ),
        ];

        for (name, polygon) in shapes {
            let sweep = fill_of(&polygon);
            let slow = fill_the_slow_way(&polygon);

            assert_eq!(
                sweep.len(),
                slow.len(),
                "{name}: {} vertices against {}",
                sweep.len(),
                slow.len()
            );

            for (index, (left, right)) in sweep.iter().zip(&slow).enumerate() {
                assert!(
                    (left.x - right.x).abs() < 0.01 && (left.y - right.y).abs() < 0.01,
                    "{name}: vertex {index} is {left:?} where the slow way says {right:?}"
                );
            }
        }
    }
    /// The area of the triangles a fill produced: half a cross product each, summed.
    fn area(vertices: &[Vertex]) -> f32 {
        vertices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|triangle| {
                let (a, b, c) = (triangle[0], triangle[1], triangle[2]);
                ((b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y)).abs() * 0.5
            })
            .sum()
    }

    fn colour() -> gpui_kit::Hsla {
        gpui_kit::rgba(0x00FF_00FF).into()
    }

    use note_format::ink::Tip;

    /// Whether a point is inside a triangle: the three edge cross-products share a sign.
    fn inside_triangle(point: (f32, f32), triangle: &[Vertex]) -> bool {
        let cross =
            |a: &Vertex, b: &Vertex| (b.x - a.x) * (point.1 - a.y) - (b.y - a.y) * (point.0 - a.x);

        let (d1, d2, d3) = (
            cross(&triangle[0], &triangle[1]),
            cross(&triangle[1], &triangle[2]),
            cross(&triangle[2], &triangle[0]),
        );

        let negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;

        !(negative && positive)
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
    /// follows: the triangles cover the shape the band *is*, and no more. The old ribbon measured 6.6
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

        // A stroke long enough to be a line and not a dot: more than one nib across, so the shape it
        // leaves is the band it swept rather than the disc its own width would make.
        let travel = 30.0_f32;

        let mut stroke = note_format::ink::Stroke::new(
            note_format::ink::InkPoint::new(0.0, 0.0, width),
            0xFFEB_3B00,
        );

        let mut x = 0.0;
        while x < travel {
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

        // What the band covers: the rectangle the nib sweeps, plus a round cap at each end — the caps are
        // what a nib's round end leaves, and they are half a disc each.
        let travel = stroke.points.last().expect("a stroke").x;
        let radius = width * 0.5;
        let covered = travel * width + std::f32::consts::PI * radius * radius;

        let painted = area(&vertices);
        let overlap = painted / covered;

        // A tessellated polygon covers its own area once. Half again would already be one layer too
        // many for a 35% band; the old ribbon measured 17 at this length.
        assert!(
            (overlap - 1.0).abs() < 0.15,
            "the band is painted {overlap:.2}× over: at 35% ink that is {:.0}% opaque, not 35%",
            (1.0 - (1.0 - 0.35_f32).powf(overlap)) * 100.0
        );
        assert!(!vertices.is_empty(), "a band drew nothing at all");
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

        // Out along the line, a long way past its own width, then back over it: the shape a reader
        // makes when they go over a highlight a second time in one stroke.
        let travel = 40.0_f32;
        let mut x = 0.0;
        while x < travel {
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

        // The band is `travel` long however many times the stroke covered it: the region is filled once, and a
        // polygon that crosses itself is ink rather than a second layer. The two ends add a half circle each,
        // because the nib is round — so the shape is the rectangle plus one whole disc, and *not* the rectangle
        // alone. Leaving the ends out is what made this test call a correct stroke "1.35× over".
        let radius = width * 0.5;
        let covered = travel * width + std::f32::consts::PI * radius * radius;
        let overlap = area(&vertices) / covered;

        assert!(
            (overlap - 1.0).abs() < 0.15,
            "a doubled-back band is painted {overlap:.2}× over"
        );
    }

    /// A pen held down and turned on the spot fills a disc, and does not become a polygon with a hole.
    ///
    /// This is the gesture a reader makes when they "scrub" a spot with a highlighter: the nib stays
    /// within a pixel or two while the pen turns, so the points pile up in almost the same place and
    /// their *directions* swing right round. [`ribbon_outline`] offsets each point along the normal
    /// through its neighbours, so a point whose neighbours are indistinguishable from it has no
    /// direction to take a normal from — and a polygon built by joining those offsets winds round
    /// itself, which the nonzero rule fills as a shape with a hole in it.
    #[test]
    fn a_pen_turned_on_the_spot_fills_a_disc() {
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };

        let width = 18.0_f32;

        // A nib pressed down and swept right round: 36 readings, one per 10 degrees, a fifth of a
        // pixel from the centre — the wobble of a hand turning a pen rather than pivoting on a pin.
        let mut stroke = note_format::ink::Stroke::new(
            note_format::ink::InkPoint::new(0.0, 0.0, width),
            0xFFEB_3B00,
        );

        for step in 1..=36 {
            let angle = step as f32 / 36.0 * std::f32::consts::TAU;
            stroke.points.push(note_format::ink::InkPoint::new(
                angle.cos() * 0.2,
                angle.sin() * 0.2,
                width,
            ));
        }

        stroke.close_at(1.0);

        let mut vertices = Vec::new();
        fill(&mut vertices, &stroke, &mapping, (0.0, 0.0), colour());

        // What it must cover: about a disc the size of the nib, since the wobble is far smaller than
        // the band is wide.
        let radius = width * 0.5;
        let disc = std::f32::consts::PI * radius * radius;
        let painted = area(&vertices);

        assert!(
            painted > disc * 0.5,
            "a turned nib painted {painted:.0}px² of a {disc:.0}px² disc: the shape has a hole in it"
        );
    }

    /// A curve drawn while the pen turns is filled along its whole length, ends included.
    ///
    /// The case a turned-but-stationary nib does not cover: the nib moves *and* its direction changes, so
    /// the outline is a ribbon that twists. Where a twist is sharp enough, the left and right offsets
    /// cross each other, and a polygon that crosses itself is filled by the nonzero rule as the region
    /// its winding leaves over — which, where the crossing doubles back, is nothing at all. That is a hole
    /// in the middle of a stroke, and the ends are where it shows first: the first and last points have
    /// one neighbour each, so their edges are the ones a turn distorts most.
    ///
    /// The test fills a quarter circle and then asks, strip by strip down its length, whether any strip
    /// came out unpainted.
    #[test]
    fn a_curve_drawn_while_turning_is_filled_end_to_end() {
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };

        let width = 18.0_f32;

        // A quarter circle of radius 12, read at the spacing the app keeps points at.
        let radius = 12.0f32;
        let mut stroke = note_format::ink::Stroke::new(
            note_format::ink::InkPoint::new(0.0, 0.0, width),
            0xFFEB_3B00,
        );
        let mut angle = 0.0f32;
        let step = 0.75 / radius;

        while angle < std::f32::consts::FRAC_PI_2 {
            angle += step;
            stroke.points.push(note_format::ink::InkPoint::new(
                radius * angle.sin(),
                radius - radius * angle.cos(),
                width,
            ));
        }

        stroke.close_at(1.0);

        let mut vertices = Vec::new();
        fill(&mut vertices, &stroke, &mapping, (0.0, 0.0), colour());

        // Walk the band's own length and check each slice of it was painted. A slice is a window of the
        // stroke's points; the ink under it is whatever the triangles cover there.
        let points = &stroke.points;
        let across = width * 0.5;
        let mut strip = 0;
        let mut unpainted = Vec::new();

        for pair in points.chunks(4) {
            let middle = pair[pair.len() / 2];
            let covered = area_within(&vertices, middle.x, middle.y, across * 0.5);
            if covered < 0.25 {
                unpainted.push((strip, middle.x, middle.y, covered));
            }
            strip += 1;
        }

        assert!(
            unpainted.is_empty(),
            "{} strips of {} came out unpainted, first at ({:.1},{:.1}) with {:.2}px2",
            unpainted.len(),
            strip,
            unpainted.first().map(|entry| entry.1).unwrap_or(0.0),
            unpainted.first().map(|entry| entry.2).unwrap_or(0.0),
            unpainted.first().map(|entry| entry.3).unwrap_or(0.0),
        );
    }

    /// How much of a small square around a point the triangles cover, by sampling.
    fn area_within(vertices: &[Vertex], x: f32, y: f32, half: f32) -> f32 {
        const STEPS: usize = 7;

        let mut covered = 0;
        for row in 0..STEPS {
            for column in 0..STEPS {
                let px = x - half + 2.0 * half * (column as f32 + 0.5) / STEPS as f32;
                let py = y - half + 2.0 * half * (row as f32 + 0.5) / STEPS as f32;

                if vertices
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .any(|t| inside_triangle((px, py), t))
                {
                    covered += 1;
                }
            }
        }

        covered as f32 / (STEPS * STEPS) as f32
    }

    /// The ends of a curve are where a turn distorts the outline most, and they must still be inked.
    ///
    /// The first and last points of a stroke have one neighbour each, so their edges are the ones a turn
    /// bends furthest — and a pen that is *put down* and rolled before it travels makes the first few
    /// readings nearly coincident while their directions swing. That is the same coincidence the dot rule
    /// handles for a whole stroke, in miniature, at one end of an otherwise ordinary line.
    ///
    /// This walks a handful of rolling starts and ends and asks whether either end came out unpainted.
    #[test]
    fn the_ends_of_a_turning_stroke_are_inked() {
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };

        let width = 18.0_f32;
        let mut failures = Vec::new();

        for roll in [2usize, 4, 8, 16] {
            for radius in [6.0f32, 12.0, 30.0] {
                let stroke = rolling_curve(width, radius, roll);
                let mut vertices = Vec::new();
                fill(&mut vertices, &stroke, &mapping, (0.0, 0.0), colour());

                let points = &stroke.points;
                let half = width * 0.5;

                for (which, point) in [("start", points[0]), ("end", points[points.len() - 1])] {
                    // The nib's own centre must be inked: whatever the outline did at the end, it cannot
                    // have left the place the pen actually was unpainted.
                    let covered = area_within(&vertices, point.x, point.y, half * 0.35);

                    if covered < 0.5 {
                        failures.push(format!(
                            "{which} of roll={roll} r={radius}: {:.0}% painted",
                            covered * 100.0
                        ));
                    }
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} ends came out unpainted:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }

    /// A stroke that is put down, rolled in place, and then curves away.
    fn rolling_curve(width: f32, radius: f32, roll: usize) -> note_format::ink::Stroke {
        use note_format::ink::{InkPoint, Stroke};

        let mut stroke = Stroke::new(InkPoint::new(0.0, 0.0, width), 0xFFEB_3B00);

        // Put down and rolled: readings a fraction of a pixel apart, their directions going round.
        for step in 1..=roll {
            let angle = step as f32 * 0.6;
            stroke
                .points
                .push(InkPoint::new(angle.cos() * 0.05, angle.sin() * 0.05, width));
        }

        // Then travelling, and still turning.
        let (ox, oy) = (
            stroke.points.last().unwrap().x,
            stroke.points.last().unwrap().y,
        );
        let mut angle = 0.0f32;
        let step = 0.75 / radius;

        while angle < std::f32::consts::FRAC_PI_2 {
            angle += step;
            stroke.points.push(InkPoint::new(
                ox + radius * angle.sin(),
                oy + radius - radius * angle.cos(),
                width,
            ));
        }

        // And rolled again as it is lifted: the same coincidence at the other end.
        let (ex, ey) = (
            stroke.points.last().unwrap().x,
            stroke.points.last().unwrap().y,
        );
        for step in 1..=roll {
            let angle = step as f32 * 0.6;
            stroke.points.push(InkPoint::new(
                ex + angle.cos() * 0.05,
                ey + angle.sin() * 0.05,
                width,
            ));
        }

        stroke.close_at(1.0);
        stroke
    }

    // ---------------------------------------------------------------------------------------------
    // Turning ink: every way a hand can turn while the nib is down, and whether the mark is solid.
    //
    // Each scenario below is a *shape*, not a length: what is asserted is that the ink is there across
    // the whole of it. Two questions are asked of each, and they are the same question from two sides —
    // `assert_inked` asks whether the model calls the nib's own path ink, and `assert_outline_is_the_model`
    // asks whether the outline the renderer is given is the boundary of exactly that region.
    //
    // Neither question triangulates or rasterises anything to answer, which is what lets a shape be asked
    // about at every width, wobble and turn: the model is built once and then queried. That is the whole
    // reason the model is a value (`ink::Model`) rather than a function of the points.
    // ---------------------------------------------------------------------------------------------

    /// The most cells a failure's picture is drawn across, so a large stroke still prints a picture that fits
    /// on a screen.
    const PICTURE_CELLS: usize = 160;

    /// The nib width the app ships with (`ink::HIGHLIGHTER_WIDTH`), spelled out because that constant is
    /// private to `note-ui` and these tests are about the geometry rather than the tool.
    const WIDTH: f32 = 18.0;

    /// The app's own resampling: the distance it keeps between two points (`resample_spacing`).
    fn spacing() -> f32 {
        note_format::settings::Settings::default().resample_spacing
    }

    /// The identity mapping: these tests are about the outline, not about where the page sits.
    fn flat() -> Mapping {
        Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        }
    }

    /// The ink a stroke's outline is filled with.
    fn ink(stroke: &note_format::ink::Stroke) -> Vec<Vertex> {
        let mut vertices = Vec::new();
        fill(&mut vertices, stroke, &flat(), (0.0, 0.0), colour());
        vertices
    }

    /// A stroke through a list of points, at the given width, closed as a finished stroke would be.
    fn stroke_of(points: &[(f32, f32)], width: f32) -> note_format::ink::Stroke {
        use note_format::ink::{InkPoint, Stroke};

        let mut stroke = Stroke::new(InkPoint::new(points[0].0, points[0].1, width), 0xFFEB_3B00);

        for (x, y) in &points[1..] {
            stroke.points.push(InkPoint::new(*x, *y, width));
        }

        stroke.close_at(1.0);
        stroke
    }

    /// A whole disc of readings a fraction of a pixel from one place: a nib turned where it stands.
    ///
    /// `wobble` is how far the readings stray, `readings` how many there are, and `turns` how many times
    /// round the pen goes — the three things a turning hand varies.
    fn turned_in_place(wobble: f32, readings: usize, turns: f32) -> Vec<(f32, f32)> {
        (0..readings)
            .map(|step| {
                let angle = step as f32 / readings as f32 * turns * std::f32::consts::TAU;

                (angle.cos() * wobble, angle.sin() * wobble)
            })
            .collect()
    }

    /// A curve walked at the app's spacing: a radius, and how far round it goes.
    fn arc(radius: f32, turns: f32) -> Vec<(f32, f32)> {
        let step = spacing() / radius;
        let mut points = Vec::new();
        let mut angle = 0.0f32;

        while angle < turns * std::f32::consts::TAU {
            points.push((radius * angle.sin(), radius - radius * angle.cos()));
            angle += step;
        }

        points
    }

    /// A stroke that goes out along a line and comes back over it.
    fn there_and_back(length: f32) -> Vec<(f32, f32)> {
        let step = spacing();
        let mut points = Vec::new();
        let mut x = 0.0f32;

        while x < length {
            points.push((x, 0.0));
            x += step;
        }

        while x > 0.0 {
            points.push((x, 0.0));
            x -= step;
        }

        points
    }

    /// A zigzag: a run, a hard turn, and a run back at an angle to it.
    fn zigzag(runs: usize, length: f32, angle_degrees: f32) -> Vec<(f32, f32)> {
        let step = spacing();
        let angle = angle_degrees.to_radians();
        let mut points = Vec::new();
        let mut at = (0.0f32, 0.0f32);

        for run in 0..runs {
            let heading = if run % 2 == 0 { angle } else { -angle };
            let mut walked = 0.0f32;

            while walked < length {
                points.push(at);
                at = (at.0 + heading.cos() * step, at.1 + heading.sin() * step);
                walked += step;
            }
        }

        points
    }

    /// A spiral: the pen travels, and its direction turns, the whole way round.
    fn spiral(turns: f32, start: f32, growth: f32) -> Vec<(f32, f32)> {
        /// The most readings a spiral may be drawn with. A spiral that grows as fast as it turns never comes
        /// round, so without a bound on the readings it walks outwards for ever — and a test nobody can finish
        /// is not a test.
        const MOST: usize = 4000;

        let mut points = Vec::new();
        let mut angle = 0.0f32;
        let mut radius = start;

        while angle < turns * std::f32::consts::TAU && points.len() < MOST {
            points.push((radius * angle.cos(), radius * angle.sin()));
            angle += spacing() / radius.max(0.5);
            radius += growth;
        }

        points
    }

    /// Fills a stroke and fails if any of the places the nib actually was is unpainted.
    ///
    /// The nib's own centre is the one point of a stroke that *must* be ink: whatever the outline did,
    /// it cannot have left the place the pen was standing unpainted. This walks every reading rather
    /// than a sample of them, so a hole anywhere in a stroke is a failure at the reading inside it.
    fn assert_inked(what: &str, stroke: &note_format::ink::Stroke) {
        // The model, asked directly: every place the nib was standing is a place the model says is ink. That is
        // the whole of this test — no triangulation, no raster, no picture — because the claim being made is
        // about what the stroke *is*, not about how the renderer happens to have cut it up. The model is built
        // once here and asked many times, which is what makes the scenario suites affordable.
        let model = note_format::ink::Model::of(&stroke.points, 1.0, Tip::Curved);
        let mut missing = Vec::new();

        for (index, point) in stroke.points.iter().enumerate() {
            if !model.is_ink(point.x, point.y) {
                missing.push((index, point.x, point.y));
            }
        }

        if let Some((index, x, y)) = missing.first() {
            panic!(
                "{what}: {} of {} readings are not ink, first #{index} at ({x:.2}, {y:.2})",
                missing.len(),
                stroke.points.len(),
            );
        }
    }

    /// Fails unless the outline and the model agree about the region a stroke covers.
    ///
    /// The outline is a boundary and the model is a predicate, so they are the same claim from two sides. This
    /// walks a grid over the stroke's own box and asks both: a place the model calls ink must be inside some
    /// triangle of the outline, and a place it calls page must be inside none. That is the check a raster would
    /// make, done without rasterising anything.
    fn assert_outline_is_the_model(what: &str, stroke: &note_format::ink::Stroke) {
        let model = note_format::ink::Model::of(&stroke.points, 1.0, Tip::Curved);
        let vertices = ink(stroke);
        let bounds = model.bounds();

        // A grid the size of a picture, so the check is the same shape however large the stroke is.
        let columns = PICTURE_CELLS;
        let rows = PICTURE_CELLS;
        let across = (bounds[2] - bounds[0]) / columns as f32;
        let down = (bounds[3] - bounds[1]) / rows as f32;

        let mut wrong = Vec::new();

        for row in 0..rows {
            for column in 0..columns {
                let at = (
                    bounds[0] + column as f32 * across,
                    bounds[1] + row as f32 * down,
                );
                let said = model.is_ink(at.0, at.1);
                let drawn = vertices
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .any(|triangle| inside_triangle(at, triangle));

                if said != drawn {
                    wrong.push((at, said, drawn));
                }
            }
        }

        // A boundary that disagrees in places is expected: the outline is a polygonal approximation of a curved
        // rim, so a place right on it can fall either side. What is being checked is a hole or a spill, not the
        // size of the last triangle.
        let share = wrong.len() as f32 / (rows * columns) as f32;

        assert!(
            share < 0.05,
            "{what}: the outline and the model disagree about {:.1}% of places (first at {:?})",
            share * 100.0,
            wrong.first(),
        );
    }

    /// The outline and the model are the same region, seen from two sides.
    ///
    /// The outline is the boundary of the union of the discs, and the model is whether a place is in that union.
    /// If they are the same claim then a place is inside the outline exactly when the model says it is ink — and
    /// that is what this checks, over strokes whose shapes are the ones that used to break: a turn in place, a
    /// curve, a doubled-back line, a zigzag and a spiral. A hole in the outline and a spill outside it both fail
    /// here, which is the pair of bugs the whole disc-union rewrite exists to be free of.
    #[test]
    fn the_outline_is_the_boundary_of_the_model() {
        let cases: [(&str, Vec<(f32, f32)>); 5] = [
            ("turned in place", turned_in_place(0.2, 40, 1.0)),
            ("arc r=40", arc(40.0, 1.0)),
            ("there and back", there_and_back(40.0)),
            ("zigzag", zigzag(3, 20.0, 90.0)),
            ("spiral", spiral(1.0, 6.0, 0.2)),
        ];

        for (what, points) in cases {
            let stroke = stroke_of(&points, WIDTH);

            assert_inked(what, &stroke);
            assert_outline_is_the_model(what, &stroke);
        }
    }

    /// A nib turned where it stands is a dot: solid, whatever the wobble and however many times round.
    #[test]
    fn a_nib_turned_in_one_place_is_solid() {
        for wobble in [0.0f32, 0.05, 0.2, 0.6, 1.5] {
            for readings in [8usize, 36, 100, 400] {
                for turns in [1.0f32, 1.5, 3.0] {
                    let stroke = stroke_of(&turned_in_place(wobble, readings, turns), WIDTH);

                    assert_inked(
                        &format!("turned in place w={wobble} n={readings} t={turns}"),
                        &stroke,
                    );
                }
            }
        }
    }

    /// A curve walked while turning is solid along its whole length, at every radius that matters.
    ///
    /// The tight end is the interesting one: a radius smaller than the nib means the inside edge of the
    /// band is *behind* the centre of the stroke, so the offset points fold back over each other.
    #[test]
    fn a_curve_walked_while_turning_is_solid() {
        for radius in [80.0f32, 40.0, 20.0, 12.0, 8.0, 5.0, 3.0, 2.0] {
            for turns in [0.25f32, 0.5, 1.0, 2.0] {
                let stroke = stroke_of(&arc(radius, turns), WIDTH);

                assert_inked(&format!("arc r={radius} turns={turns}"), &stroke);
            }
        }
    }

    /// A spiral, travelling and turning the whole way round, is solid.
    #[test]
    fn a_spiral_is_solid() {
        for turns in [0.5f32, 1.0, 2.0, 4.0] {
            for start in [2.0f32, 6.0, 20.0] {
                // `growth` stays small on purpose: a spiral that grows as fast as it turns never comes round,
                // so a large one walks outwards for ever instead of spiralling. A twentieth of the nib is
                // already a ring tighter than a hand draws.
                for growth in [0.05f32, 0.2, 0.5] {
                    let stroke = stroke_of(&spiral(turns, start, growth), WIDTH);

                    assert_inked(&format!("spiral t={turns} s={start} g={growth}"), &stroke);
                }
            }
        }
    }

    /// A stroke that goes out and comes back over itself is solid.
    #[test]
    fn a_stroke_that_doubles_back_is_solid() {
        for length in [4.0f32, 8.0, 20.0, 60.0] {
            let stroke = stroke_of(&there_and_back(length), WIDTH);

            assert_inked(&format!("there and back {length}"), &stroke);
        }
    }

    /// A zigzag: the case where consecutive runs face opposite ways.
    #[test]
    fn a_zigzag_is_solid() {
        for runs in [2usize, 3, 6] {
            for length in [6.0f32, 20.0] {
                for angle in [30.0f32, 90.0, 150.0] {
                    let stroke = stroke_of(&zigzag(runs, length, angle), WIDTH);

                    assert_inked(&format!("zigzag n={runs} l={length} a={angle}"), &stroke);
                }
            }
        }
    }

    /// A stroke put down, rolled, then walked — the shape a highlighter is actually used in.
    #[test]
    fn a_rolling_start_then_a_curve_is_solid() {
        for roll in [2usize, 4, 8, 16, 40] {
            for radius in [4.0f32, 8.0, 12.0, 30.0] {
                let mut points = turned_in_place(0.05, roll, 1.0);
                let (ox, oy) = *points.last().expect("a rolled start has readings");

                for (x, y) in arc(radius, 0.5) {
                    points.push((ox + x, oy + y));
                }

                let stroke = stroke_of(&points, WIDTH);

                assert_inked(&format!("roll={roll} then r={radius}"), &stroke);
            }
        }
    }

    /// A stroke that is lifted while still turning: the other end of the same problem.
    #[test]
    fn a_curve_ending_in_a_roll_is_solid() {
        for roll in [2usize, 4, 8, 16, 40] {
            for radius in [4.0f32, 8.0, 12.0, 30.0] {
                let mut points = arc(radius, 0.5);
                let (ex, ey) = *points.last().expect("an arc has readings");

                for (x, y) in turned_in_place(0.05, roll, 1.0) {
                    points.push((ex + x, ey + y));
                }

                let stroke = stroke_of(&points, WIDTH);

                assert_inked(&format!("r={radius} ending in roll={roll}"), &stroke);
            }
        }
    }

    /// Every width: a hairline and a broad marker both stay solid while turning.
    #[test]
    fn turning_ink_is_solid_at_every_width() {
        for width in [1.0f32, 2.0, 6.0, 18.0, 40.0] {
            for (name, points) in [
                ("in place", turned_in_place(0.2, 60, 1.0)),
                ("arc", arc(12.0, 0.5)),
                ("spiral", spiral(1.0, 6.0, 0.35)),
                ("back", there_and_back(20.0)),
            ] {
                let stroke = stroke_of(&points, width);

                assert_inked(&format!("width {width} {name}"), &stroke);
            }
        }
    }

    /// The stroke under the pen, not yet finished, is solid too: a frame draws it every time.
    #[test]
    fn a_turning_stroke_is_solid_while_it_is_being_drawn() {
        for points in [
            turned_in_place(0.2, 40, 1.0),
            arc(10.0, 0.5),
            spiral(0.5, 4.0, 0.5),
        ] {
            for upto in [2usize, 5, 20, 100] {
                if upto > points.len() {
                    continue;
                }

                // The live stroke: `close_live` rather than `close_at`, which is what a frame draws.
                let mut stroke = stroke_of(&points[..upto], WIDTH);
                stroke.close_live(1.0);

                assert_inked(&format!("live, first {upto} readings"), &stroke);
            }
        }
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
    /// A polygon's triangles, as [`tessellate`] makes them.
    fn fill_of(polygon: &[(f32, f32)]) -> Vec<Vertex> {
        let mut vertices = Vec::new();
        tessellate(&mut vertices, polygon, gpui_kit::Hsla::default());

        vertices
    }

    /// The same fill, reading every edge for every band: what [`tessellate`] did before the active list.
    ///
    /// Slow on purpose — it is the definition, not the implementation — and quadratic, which is the cost the
    /// sweep exists to avoid.
    fn fill_the_slow_way(polygon: &[(f32, f32)]) -> Vec<Vertex> {
        let mut vertices: Vec<Vertex> = Vec::new();

        if polygon.len() < 3 {
            return vertices;
        }

        let mut bands: Vec<f32> = polygon.iter().map(|point| point.1).collect();
        bands.sort_by(|left, right| left.total_cmp(right));
        bands.dedup_by(|left, right| (*left - *right).abs() < f32::EPSILON);

        for band in bands.windows(2) {
            let (bottom, top) = (band[0], band[1]);

            if (top - bottom).abs() < f32::EPSILON {
                continue;
            }

            let middle = (bottom + top) * 0.5;
            let mut crossings: Vec<(f32, i32, f32, f32)> = Vec::new();

            for index in 0..polygon.len() {
                let (a, b) = (polygon[index], polygon[(index + 1) % polygon.len()]);

                if a.1 == b.1 {
                    continue;
                }

                let (low, high) = if a.1 < b.1 { (a, b) } else { (b, a) };

                if low.1 > bottom || high.1 < top {
                    continue;
                }

                let at = |y: f32| a.0 + (b.0 - a.0) * (y - a.1) / (b.1 - a.1);

                crossings.push((
                    at(middle),
                    if b.1 > a.1 { 1 } else { -1 },
                    at(bottom),
                    at(top),
                ));
            }

            crossings.sort_by(|left, right| left.0.total_cmp(&right.0));

            let mut winding = 0;
            let mut span: Option<(f32, f32)> = None;

            for crossing in &crossings {
                let before = winding;
                winding += crossing.1;

                match (before == 0, winding == 0) {
                    (true, false) => span = Some((crossing.2, crossing.3)),
                    (false, true) => {
                        if let Some((left_bottom, left_top)) = span.take() {
                            trapezoid(
                                &mut vertices,
                                (left_bottom, bottom),
                                (left_top, top),
                                (crossing.3, top),
                                (crossing.2, bottom),
                                gpui_kit::Hsla::default(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        vertices
    }

    /// How long the fill takes on a long stroke's outline, so a change to it can be measured rather than
    /// guessed at. Run it with `--nocapture` to see the number.
    #[test]
    fn the_fill_of_a_long_stroke_is_quick() {
        // A ribbon outline of the size a long stroke makes: two points a reading, plus the ends.
        let readings = 2000;
        let mut polygon: Vec<(f32, f32)> = (0..readings)
            .map(|step| {
                let along = step as f32 / readings as f32;
                let y = 100.0 + along * 800.0 + 40.0 * (along * 60.0).sin();
                let x = 100.0 + along * 900.0 + 30.0 * (along * 45.0).cos();
                (x, y)
            })
            .collect();

        let back: Vec<(f32, f32)> = polygon.iter().rev().map(|(x, y)| (x + 18.0, *y)).collect();
        polygon.extend(back);

        let mut vertices = Vec::new();

        let started = std::time::Instant::now();
        tessellate(&mut vertices, &polygon, colour());
        let taken = started.elapsed();

        eprintln!(
            "a fill of {} points: {taken:?}, {} vertices",
            polygon.len(),
            vertices.len()
        );

        // The fill runs on the thread that draws, so it must not be what a frame waits for. Generous, because
        // this is a debug build on whatever machine is running the tests -- but the old quadratic pass was
        // minutes here, and the guard is what would notice the quadratic term coming back.
        assert!(
            taken.as_millis() < 250,
            "a fill of {} points took {taken:?}",
            polygon.len()
        );
    }

    /// Where the per-frame cost of a growing stroke goes.
    ///
    /// A stroke under the pen is reshaped every frame, so drawing one costs the shapes at every length it
    /// passes through. Measured here rather than assumed, because the two halves are not the same size: of
    /// the cost of drawing a stroke of six hundred readings, the resampling is about a tenth and building
    /// the outline from it is the rest. Run with `--nocapture`.
    #[test]
    fn where_the_shaping_time_goes() {
        use note_format::ink::{self, InkPoint, Stroke};

        let readings = 600;
        let line: Vec<InkPoint> = (1..=readings)
            .map(|step| {
                let along = step as f32 / readings as f32;
                InkPoint::new(
                    100.0 + along * 400.0,
                    100.0 + along * 400.0 + (along * 40.0).sin() * 20.0,
                    6.0,
                )
            })
            .collect();

        let mut resample = std::time::Duration::ZERO;
        let mut outline = std::time::Duration::ZERO;

        for step in 2..=readings {
            let part = &line[..step];

            let started = std::time::Instant::now();
            let dense = ink::densified(part, 1.0, ink::Tip::Straight);
            resample += started.elapsed();

            let started = std::time::Instant::now();
            let model = ink::Model::of(part, 1.0, ink::Tip::Straight);
            std::hint::black_box(ink::ribbon_outline(&model));
            std::hint::black_box(&dense);
            outline += started.elapsed();
        }

        let mut whole = std::time::Duration::ZERO;

        for step in 2..=readings {
            let mut stroke = Stroke::new(line[0], 0x00FF_00FF);
            stroke.points = line[..step].to_vec();

            let started = std::time::Instant::now();
            stroke.rebuild(1.0);
            whole += started.elapsed();
        }

        eprintln!(
            "a stroke of {readings} readings drawn: resampling {resample:?}, model and outline \
             {outline:?}, whole stroke {whole:?}"
        );

        // The outline is the larger half by a wide margin, which is where a change to the cost of drawing
        // a stroke has to be made. This asserts the shape of that rather than a time, so it does not fail
        // on a busy machine.
        assert!(
            outline > resample,
            "the resampling cost more than the outline: {resample:?} against {outline:?}"
        );
    }
}

#[cfg(test)]
mod vertex_tests {
    use super::*;

    /// A corner is twelve bytes, and the shader reads exactly that.
    ///
    /// The number is the whole point of the packing and it is not something the compiler promises: `repr(C)`
    /// lays out two `f32`s and a `[u8; 4]` in that order with no padding, which is twelve, and MSL's
    /// `packed_float2` + `uchar4` is twelve too. If they ever disagree the vertex buffer describes one stride
    /// and the shader another, and every stroke on the page is drawn from the wrong memory — a picture that is
    /// obviously wrong but not obviously *this*. So the size is asserted rather than assumed.
    #[test]
    fn a_vertex_is_twelve_bytes_and_the_shader_agrees() {
        assert_eq!(std::mem::size_of::<Vertex>(), 12);
        assert_eq!(std::mem::align_of::<Vertex>(), 4);

        // The three fields sit where `packed_float2` and `uchar4` expect them.
        assert_eq!(std::mem::offset_of!(Vertex, x), 0);
        assert_eq!(std::mem::offset_of!(Vertex, y), 4);
        assert_eq!(std::mem::offset_of!(Vertex, colour), 8);
    }

    /// The shader's own declaration is the other half of the same promise.
    #[test]
    fn the_shader_declares_the_same_twelve_bytes() {
        assert!(
            SHADERS.contains("struct Vertex { packed_float2 position; uchar4 colour; };"),
            "the shader's vertex must be the same twelve bytes the buffer carries"
        );
    }

    /// A colour goes through the bytes unaltered, in r, g, b, a order.
    ///
    /// `uchar4` reads its four bytes in that order, so a colour that arrived with red and blue swapped — or
    /// premultiplied, which `Rgba::from(Hsla)` is not — would tint every stroke on the page.
    #[test]
    fn a_colour_packs_in_rgba_order() {
        let red = Vertex::new(1.0, 2.0, gpui_kit::rgba(0xFF_00_00_FF).into());
        assert_eq!(red.colour, [255, 0, 0, 255], "opaque red");
        assert_eq!((red.x, red.y), (1.0, 2.0), "and where it was placed");

        let blue_half = Vertex::new(0.0, 0.0, gpui_kit::rgba(0x00_00_FF_80).into());
        assert_eq!(blue_half.colour, [0, 0, 255, 128], "half-transparent blue");

        // A colour out of range is clamped rather than wrapping, which would turn a bright stroke dark.
        let clamped = Vertex::new(
            0.0,
            0.0,
            gpui_kit::Hsla {
                h: 0.0,
                s: 1.0,
                l: 1.0,
                a: 2.0,
            },
        );
        assert_eq!(clamped.colour[3], 255, "alpha above one is opaque");
    }
}
