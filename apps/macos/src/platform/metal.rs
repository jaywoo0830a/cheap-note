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
//! The ink's region is taken apart by the model, not by this module: `note_format::ink::pieces_of` hands
//! over a trapezoid per pair of readings and a disc per reading, because a ribbon is a curve grown by a
//! radius and those two families *are* that region (see its documentation). A stroke that crosses itself
//! can therefore be filled from its pieces with nothing to sort and nothing to cut up.
//!
//! Those pieces overlap each other — a disc reaches into its neighbour's trapezoid — so drawing them
//! straight would composite a translucent pen more than once wherever they meet, and would leave a
//! hairline at every shared triangle edge under multisampling. The ink is therefore drawn in two passes:
//! one marks the stencil wherever the pieces reach, the other colours each marked sample once. That is
//! what makes a highlighter read at its own 35% however many pieces met under it.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use note_format::ink::Bounds;
use note_ui::cursor::{
    BODY_HALO_ALPHA, BODY_HALO_GROW, NIB_ALPHA, NIB_BLOOM_ALPHA, NIB_BLOOM_SPREAD, NIB_RADIUS_MAX,
    NIB_RADIUS_MIN,
};
use note_ui::ink::{InkChange, StrokeId};
use note_ui::ink_layer::canvas::{Ink, Rect};
use note_ui::ink_layer::{ink_colour, Canvas};
use note_ui::paint::finite_or_one;

use crate::platform::ghost::Mark;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCompareFunction,
    MTLDepthStencilDescriptor, MTLDepthStencilState, MTLDevice, MTLLibrary, MTLLoadAction,
    MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState,
    MTLResourceOptions, MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerState, MTLSize,
    MTLStencilDescriptor, MTLStencilOperation, MTLStorageMode, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
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

// The same triangles, marking instead of colouring.
//
// A translucent stroke is filled from pieces that overlap by construction — a disc at every reading and a
// trapezoid between neighbours (see `note_format::ink::pieces_of`). Laid down one after another, a 35%
// highlighter composes to `1 - 0.65^n`, which for a point inside ten of them is 99%: the writing it was
// meant to mark is hidden. Marking first and colouring once is what keeps one stroke one layer.
//
// The fragment returns no colour at all. The pass is answered by whether a sample was written, and a
// fragment that returned one would be blended into the page — the opposite of marking without drawing.
struct Stencil { float4 position [[position]]; };

vertex Stencil stencil_vertex(uint id [[vertex_id]],
                              constant Vertex *vertices [[buffer(0)]],
                              constant float2 &viewport [[buffer(1)]],
                              constant float4 &transform [[buffer(2)]]) {
    Stencil out;
    out.position = place(float2(vertices[id].position) * transform.xy + transform.zw, viewport);
    return out;
}

fragment void stencil_fragment(Stencil in [[stage_in]]) {
    (void) in;
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
///
/// Public rather than crate-only: the measurement adapter (`src/bin/adapter.rs`) draws through the same
/// surface the window and the bench do, which is the whole point of it being one type.
pub struct Surface {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Fills, ruling and ink: one vertex per corner, one colour each.
    solid: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The document's page: a textured quad.
    page: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The ink's mark pass: the same vertices, counting instead of colouring.
    stencil: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// What the mark pass does to a sample a piece reaches.
    counting: Retained<ProtocolObject<dyn MTLDepthStencilState>>,
    /// What the colour pass does: keep a sample this stroke counted and no earlier one did.
    counted: Retained<ProtocolObject<dyn MTLDepthStencilState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    layer: Retained<CAMetalLayer>,
    /// The finished ink, as triangles. Rebuilt when what it is a function of changes and at no other time:
    /// the strokes, the mapping that places them, the part of the sheet on screen, and the ink in hand.
    ink: Vec<Vertex>,
    /// The region of the paper the ink covers, in the paper's units: the visible box grown by
    /// [`CULL_SPAN`] at the moment of the build.
    ///
    /// A frame reuses the ink while the screen still fits inside this (see [`fits`]). A scroll moves the screen
    /// around within it and a zoom inward shrinks the screen inside it, so neither costs a build; a scroll out
    /// of it or a zoom out of it is a build, because both show paper this does not cover.
    ink_covered: Bounds,
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
    /// Where each translucent stroke's vertices begin in [`Self::ink`], one entry a stroke, in the order
    /// they were filled — the ranges the stencil pass needs to keep one stroke one layer (see
    /// [`Self::ink_pass`]).
    ///
    /// Opaque ink is not in here: the same colour over itself is the same colour, so a pen costs nothing to
    /// draw the way it always has.
    translucent_ink: Vec<usize>,
    /// The same for [`Self::open`].
    translucent_open: Vec<usize>,
    /// The finished ink's buffers, one per frame in flight: the CPU writes the next while the GPU
    /// may still be reading the last.
    ink_buffers: [Option<Retained<ProtocolObject<dyn MTLBuffer>>>; 3],
    /// How many of [`Self::ink`]'s vertices each buffer holds — the front of the list, from zero — in
    /// order.
    ///
    /// The explicit-rebuild design's per-slot state, and the only one: a rebuild makes every buffer
    /// hold nothing (`filled` goes to zero) and a frame refills its slot with the whole list; an append
    /// leaves the buffers holding the vertices they had and the next frame copies only the tail past
    /// this. That is what turns an append's upload from a page into a stroke, without asking a buffer
    /// of unknown contents to vouch for the front of itself.
    ///
    /// A slot holds the current ink when — and only when — its figure equals [`Self::ink`]'s length.
    /// That is a comparison rather than a third flag, so the two can never disagree: the slots used to
    /// carry a validity boolean beside this count, and an append that set the count *forward* for every
    /// slot, then marked the slot it wrote valid, left the other two claiming a front they had only
    /// partly been given. The next frame to use one copied `120..130` of `[120, 100, 0]` and drew a
    /// stroke with a hundred corners of garbage before it.
    filled: [usize; 3],
    /// Time spent copying the finished ink into a frame's buffer, and how much was copied.
    ///
    /// Instrumentation for the bench's breakdown (`CHEAP_NOTE_BENCH_BREAKDOWN`): a rebuild's wall time
    /// is the fill plus this copy, and which of the two it is changes what to do about it. Reset by
    /// [`Self::take_stages`].
    stage_uploaded: std::time::Duration,
    uploaded_bytes: u64,
    /// Time spent turning strokes into vertices: `refresh_ink`'s fill, without the copy that follows.
    stage_refresh: std::time::Duration,
    /// How many of those uploads had to make a new buffer rather than refill one already big enough.
    ///
    /// A page that grows past a buffer's length allocates again, and an allocation of 137 MB is not
    /// free — worth telling apart from the copy itself.
    upload_count: u32,
    /// How many times this surface has built the finished ink's triangles.
    ///
    /// The process-wide count in [`report`] is what the status line prints, and it is every surface's; this
    /// is *this* one's, which is what a test can hold a figure about. Counted where `refresh_ink` decides a
    /// rebuild — a reuse, an append and a build the surface did before the test started are not counted —
    /// so "the page was built exactly twice" is a fact about the sequence rather than about a return value.
    builds: u64,
    /// The open stroke's buffers, one per frame in flight.
    open_buffers: [Option<Retained<ProtocolObject<dyn MTLBuffer>>>; 3],
    /// Which of the three the next frame writes.
    buffer_slot: usize,
    /// The polygon and sweep a fill reuses, stroke after stroke.
    ///
    /// Both are buffers whose capacity a long stroke earns and the next one spends; keeping them on the
    /// surface is what stops a rebuild from making and freeing one set per stroke (`OPT-MAC.md`).
    scratch: Vec<(f32, f32)>,
    sweep: Sweep,
    /// What the surface knows about the page it drew: the revision it was built from, how many of the
    /// page's strokes it covers, and the rung and turn they were baked for.
    ///
    /// The explicit-rebuild state — the whole point being that it answers "is this the page below" without
    /// walking the page. `covered` is the number of strokes at the *front* that the triangles below already
    /// are: appending lengthens that prefix, and an edit anywhere else shortens it.
    ///
    /// `None` until a page has been built, which is what makes the first frame a build rather than an
    /// append onto triangles that do not exist.
    delta: Option<DeltaState>,
    /// The strokes in hand, as of the last build: which of the page's strokes the caller draws itself.
    ///
    /// Kept on the surface rather than passed down, because the fill has to know which strokes to skip and
    /// the surface is the only thing that knows the page it is filling.
    ink_in_hand: Arc<Vec<bool>>,
    /// What the last build baked *about the selection*: the mask it read, the colour it tinted the ink in
    /// hand with, and where a drag had taken that ink.
    ///
    /// The selection is not an edit. A loop swept around a stroke leaves the page's revision alone, and the
    /// ink in hand is drawn a second time in the selection's colour by [`Surface::rebuild_ink`] — so a
    /// selection that changed is a page whose triangles are stale without the page having changed, which is
    /// the one thing the revision cannot say. This is the state that says it instead, and it is a *state*
    /// rather than a flag because a frame after the first one has to be able to say "that is what I drew"
    /// as well as "that is not" (see [`Paint`]).
    ///
    /// Remembered as the build read it rather than as the description arrived, because those are two
    /// different things in a page nobody has selected anything on: an empty mask, whatever `Vec` it came in,
    /// is no selection.
    ink_paint: Paint,
    /// The document's page on the GPU, and the identity of the rasterisation it holds.
    page_texture: Option<(usize, Retained<ProtocolObject<dyn MTLTexture>>)>,
    /// The multisampled target the passes draw into, resolved into the drawable. Made with the size.
    msaa: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// The stencil the ink's two passes share, made once per size.
    stencil_texture: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// The command buffer committed by the most recent `draw`, kept so that a benchmark can wait for it.
    ///
    /// `None` until the first frame is committed. The app never reads it: it exists for
    /// [`Surface::settle`], which is the only caller that needs a frame's cost to have been paid.
    last_commit: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    width: usize,
    height: usize,
}

impl Surface {
    /// Gives a view a Metal layer, and the pipelines a canvas is drawn with.
    pub fn install(view: &objc2_app_kit::NSView, scale: f64) -> Option<Self> {
        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;

        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .ok()?;

        let solid = pipeline(&device, &library, "solid_vertex", "solid_fragment")?;
        let page = pipeline(&device, &library, "page_vertex", "page_fragment")?;
        // The ink's stencil pass: the same vertices through a shader that marks instead of colouring, with a
        // fragment shader of its own because it has no colour to hand on.
        let stencil = pipeline(&device, &library, "stencil_vertex", "stencil_fragment")?;
        let stenciling = stenciling(&device)?;
        let stencilled = stencilled(&device)?;

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
            stencil,
            counting: stenciling,
            counted: stencilled,
            sampler,
            layer,
            ink: Vec::new(),
            delta: None,
            ink_in_hand: Arc::new(Vec::new()),
            ink_paint: Paint::nothing(),
            // Nothing built yet: a region that covers nothing, so the first frame always builds.
            ink_covered: Bounds::NOTHING,
            open: Vec::new(),
            open_id: 0,
            open_points: 0,
            open_colour: 0,
            open_paper: (0.0, 0.0),
            open_rotation: 0,
            translucent_ink: Vec::new(),
            translucent_open: Vec::new(),
            ink_buffers: [None, None, None],
            filled: [0; 3],
            stage_uploaded: std::time::Duration::ZERO,
            uploaded_bytes: 0,
            upload_count: 0,
            builds: 0,
            stage_refresh: std::time::Duration::ZERO,
            open_buffers: [None, None, None],
            buffer_slot: 0,
            scratch: Vec::new(),
            sweep: Sweep::default(),
            page_texture: None,
            msaa: None,
            stencil_texture: None,
            last_commit: None,
            width: 1,
            height: 1,
        })
    }

    /// Matches the drawable to the view. `width` and `height` are device pixels.
    pub fn resize(&mut self, width: usize, height: usize, scale: f64) {
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
        self.delta = None;
        self.msaa = None;
        self.stencil_texture = None;
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
    // The ink's passes share a stencil (see [`STENCIL`]); the desk and the page ignore it.
    descriptor.setStencilAttachmentPixelFormat(STENCIL);

    // SAFETY: index 0 is the one attachment every pass here renders into, and the descriptor is alive.
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);

    // Source over destination, in the sRGB values the app's colours are written in — the same arithmetic
    // Core Graphics did, so a highlighter over a page looks the way it looked before.
    //
    // SAFETY: the attachment came from the descriptor above, which is alive for the length of this
    // function, and `objectAtIndexedSubscript(0)` returned a borrowed reference to it rather than a
    // new object — so `&*attachment` is that live reference, not a dangling one, and the six
    // `msg_send!` writes go to the attachment the pass will render into. Each selector is one the
    // `MTLRenderPipelineColorAttachmentDescriptor` protocol declares, and the values are the
    // documented enum ordinals for source-over.
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

/// What the **mark** pass does: count, per sample, the strokes whose pieces have reached it.
///
/// `Increment` rather than a set: the count is what tells a stroke from an earlier one (see
/// [`Surface::ink_pass`]), and it is idempotent for the purpose that matters — a sample covered by ten of
/// one stroke's pieces is counted once, because the count advances per *pass*, not per triangle.
///
/// The front and back faces are treated alike: a triangle here is the shape being filled rather than an edge
/// of it, and a stroke that crosses itself has pieces wound both ways.
/// What the **mark** pass does: mark the samples a stroke's pieces reach, so the colour pass leaves them.
///
/// The comparison is `Always` and every sample a fragment touches is overwritten with [`STENCIL_MARKED`].
/// Because the write is a `Replace` rather than a count, it is idempotent — a point inside the band that
/// thirty-odd pieces cover ends up marked exactly as a point one piece grazes, which is the property that
/// makes one stroke one layer.
///
/// The front and back faces are treated alike: a triangle here is the shape being filled rather than an edge
/// of it, and a stroke that crosses itself has pieces wound both ways.
fn stenciling(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Option<Retained<ProtocolObject<dyn MTLDepthStencilState>>> {
    let counts = MTLStencilDescriptor::new();
    counts.setStencilCompareFunction(MTLCompareFunction::Always);
    counts.setStencilFailureOperation(MTLStencilOperation::Keep);
    counts.setDepthFailureOperation(MTLStencilOperation::Keep);
    counts.setDepthStencilPassOperation(MTLStencilOperation::Replace);
    counts.setReadMask(u32::MAX);
    counts.setWriteMask(u32::MAX);

    let state = MTLDepthStencilDescriptor::new();
    state.setFrontFaceStencil(Some(&counts));
    state.setBackFaceStencil(Some(&counts));
    // The ink is one flat layer: nothing is behind anything, and depth is not written.
    state.setDepthCompareFunction(MTLCompareFunction::Always);
    state.setDepthWriteEnabled(false);

    device.newDepthStencilStateWithDescriptor(&state)
}

/// What the **colour** pass does: colour the samples the stroke covers, once each.
///
/// `Equal` against the reference, which is [`STENCIL_MARKED`] — what the mark pass wrote over every sample the
/// stroke's pieces reach. The first fragment to reach a sample colours it and writes [`STENCIL_EMPTY`] back,
/// so the thirty-odd fragments that follow fail the same test and leave it alone. That single fact is what
/// makes one stroke one layer of its alpha.
///
/// Every outcome writes, so a sample a fragment merely passes over is cleared of its mark too — which is
/// harmless: it was not the stroke's to colour.
fn stencilled(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Option<Retained<ProtocolObject<dyn MTLDepthStencilState>>> {
    let counted = MTLStencilDescriptor::new();
    counted.setStencilCompareFunction(MTLCompareFunction::Equal);
    // A sample this pass colours loses its mark, so no later piece of the stroke can colour it again.
    counted.setStencilFailureOperation(MTLStencilOperation::Zero);
    counted.setDepthFailureOperation(MTLStencilOperation::Zero);
    counted.setDepthStencilPassOperation(MTLStencilOperation::Zero);
    counted.setReadMask(u32::MAX);
    counted.setWriteMask(u32::MAX);

    let state = MTLDepthStencilDescriptor::new();
    state.setFrontFaceStencil(Some(&counted));
    state.setBackFaceStencil(Some(&counted));
    state.setDepthCompareFunction(MTLCompareFunction::Always);
    state.setDepthWriteEnabled(false);

    device.newDepthStencilStateWithDescriptor(&state)
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
    /// Brings the triangles below up to the page the caller is describing.
    ///
    /// There are exactly three answers, and which one is right is decided from numbers the surface
    /// already holds — the page's revision, how many of its strokes are covered, the rung and turn they
    /// were baked for — plus the change the model published. Nothing here walks the page:
    ///
    /// * **A reuse** ([`InkPlan::Reuse`]). The page is the one below. A pan, a frame while the hand is
    ///   still, a second draw from one edit.
    /// * **An append** ([`InkPlan::Append`]). One stroke was added at the end (see
    ///   [`Self::append_ink`]). The triangles before it are kept and only the tail is filled.
    /// * **A rebuild**, with the reason recorded ([`InkPlan::Rebuild`]): a rung crossed, the page
    ///   turned, a change that went by unread, an edit that was not an append, a selection.
    ///
    /// Which of the three is [`decide`]'s to say, so a test asks the same function this path runs: a
    /// decision a test read differently from the frame that shipped it is how a middle erase's undo came
    /// to be drawn out of order.
    ///
    /// `true` when triangles were made, which is the only thing that makes the uploaded buffers stale. A
    /// frame that reuses the ink must not upload it again: the triangle list has not changed, and copying
    /// a page's worth of it into a buffer three frames in flight — every frame of a pan — is megabytes a
    /// frame of `memcpy` for nothing. The slots keep their validity across such frames, so each of the
    /// three is written once per rebuild rather than once per frame.
    fn refresh_ink(&mut self, canvas: &Canvas) -> bool {
        // What the edit was, decided from the page's own revision and the change the model published —
        // never from a walk of the page. A page that is the one below, at the same rung and the same way
        // up, whose screen still fits, and whose selection is the one baked below is nothing to do: what
        // the triangles are drawn *through* is a `Transform` the shader applies, and the tint a selection
        // and a drag bake in are re-baked only when they move (they name `Paint`).
        //
        // No state at all is not the same as a state that matches: a surface that has never built this
        // page has no drawing of it to compare a selection against, and says a rebuild (see `Demotion`
        // `Cold`) — which is where `ink_paint` starts, though The empty bake is a selection nobody has made,
        // so a cold surface asked for no selection with nothing below it reads a reuse and draws nothing.
        let plan = match self.delta {
            Some(state) => decide(canvas, &state, self.ink_covered, &self.ink_paint),
            None => InkPlan::Rebuild(Demotion::Cold),
        };

        match plan {
            InkPlan::Reuse => false,
            InkPlan::Append => {
                // The page only got longer, so the vertices it already had are still right and the only
                // new triangles are the appended stroke's. The selection this frame found is what every
                // slot is then given as it catches up, so it is the one baked.
                let started = std::time::Instant::now();
                self.append_ink(canvas);
                self.stage_refresh += started.elapsed();
                true
            }
            InkPlan::Rebuild(reason) => {
                self.rebuild_ink(canvas, reason);
                true
            }
        }
    }

    /// Throws the triangles below away and builds the page again, recording `reason`.
    fn rebuild_ink(&mut self, canvas: &Canvas, reason: Demotion) {
        let ink = &canvas.ink;

        // Every rebuild names why it had to happen — see `Demotion`. "It rebuilt" is never an answer
        // without a reason, and the trace is where the reason is readable for a run that went wrong.
        let started = std::time::Instant::now();

        self.ink.clear();
        self.ink_covered = cull_of(ink.visible);

        // The page this rebuild *is*, not the one it replaced: a rung change or a page turn arrives with
        // the new revision already on the page, and recording the old one would make the frame after this
        // read as a gap and rebuild again for nothing.
        self.delta = Some(DeltaState {
            revision: ink.edit,
            covered: ink.strokes.len(),
            detail: detail_of(ink),
            rotation: ink.rotation.into(),
            paper: paper_of(canvas),
        });

        if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
            report::trace(reason);
        }
        self.builds += 1;
        // Counted whether or not the trace is on: the status line reads this the same way, and "how many times
        // did this page rebuild" is not a fact that should need an environment variable to exist. The *print*
        // is what the variable turns on, not the count.
        report::rebuilt();

        let list = ink.strokes.as_slice();
        let mapping = Mapping::of(canvas);
        let cull = self.ink_covered;

        // Whose strokes are the caller's to draw: the fill skips them, and the loop below lays them down
        // where the drag has taken them.
        self.ink_in_hand = Arc::clone(&ink.selected);

        // What the triangles below are about to bake about the selection, recorded before the bake: the
        // next frame compares against this, and a selection left out of it would be a selection the surface
        // would bake again on every frame it is still in hand.
        self.ink_paint = Paint::of(ink);

        // The page itself: every finished stroke that can be seen, in the order it was written.
        self.fill_ink(list, &mapping, cull);

        // The ink in hand, where a drag has taken it, and once more in the selection's colour.
        if ink.selection.is_some() || ink.offset != (0.0, 0.0) {
            for (index, stroke) in list.iter().enumerate() {
                if !ink.selected.get(index).copied().unwrap_or(false) {
                    continue;
                }

                let colour = ink_colour(stroke.color);
                let Self {
                    ink: vertices,
                    scratch,
                    sweep,
                    translucent_ink,
                    ..
                } = self;

                open(translucent_ink, vertices.len(), colour);
                fill(
                    vertices, scratch, sweep, stroke, &mapping, ink.offset, colour,
                );

                if let Some(colour) = ink.selection {
                    open(translucent_ink, vertices.len(), colour);
                    fill(
                        vertices, scratch, sweep, stroke, &mapping, ink.offset, colour,
                    );
                }
            }
        }

        // The lasso's loop: the ring the reader is drawing, in the selection's colour.
        if let Some(stroke) = &ink.lasso {
            let colour = ink.selection.unwrap_or_else(|| ink_colour(stroke.color));
            let Self {
                ink: vertices,
                scratch,
                sweep,
                translucent_ink,
                ..
            } = self;

            open(translucent_ink, vertices.len(), colour);
            fill(
                vertices,
                scratch,
                sweep,
                stroke,
                &mapping,
                (0.0, 0.0),
                colour,
            );
        }

        // The ink changed: every slot now holds nothing of the list (see `fill_ink`). Said here so the
        // build and the invalidation cannot drift apart — the caller only has to know that a rebuild
        // happened, and a slot holds the current ink exactly when its figure equals the list's length.
        self.filled = [0; 3];

        self.stage_refresh += started.elapsed();
    }

    /// How many corners the last build made: what the upload copies and the draw reads.
    pub fn vertices(&self) -> usize {
        self.ink.len()
    }

    /// How many times this surface has decided to rebuild the finished ink's triangles.
    ///
    /// A reuse and an append are not rebuilds, so a page held still on screen reads the same figure however
    /// many frames it draws — which is the point. `pub(super)` for the tests in this module: the app has the
    /// status line's process-wide count (see [`report`]), and nothing in a frame needs this one.
    #[cfg(test)]
    pub(super) fn builds(&self) -> u64 {
        self.builds
    }

    /// How many finished-ink buffers are in flight — the frames a measurement has to give a turn to
    /// before a change's uploads are all paid.
    ///
    /// The app draws every frame, so a change reaches every slot by the third one on its own. A
    /// measurement that draws one frame per operation has to finish the ring itself, and this is the
    /// ring's length rather than a number a caller guesses (see the adapter's `frame`).
    pub fn slots(&self) -> usize {
        self.ink_buffers.len()
    }

    /// Where the finished ink's corners begin for a frame's slot, and whether that frame has to copy.
    ///
    /// The code this check replaced was a boolean beside the count and an append that
    /// converged their positions for every slot — both of which can be wrong at once; this cannot.
    fn ink_starts_at(&self, slot: usize) -> Option<usize> {
        ink_upload(self.filled[slot], self.ink.len())
    }

    /// Re-fills the page's own strokes into `self.ink`, from `list`.
    ///
    /// The one loop that turns finished ink into triangles, in the order the ink was written, and the
    /// only place a stroke is culled. Both the whole-page build and an append end here, which is what
    /// keeps "the page, drawn" one definition rather than two that can drift.
    ///
    /// A stroke in hand is skipped: it is drawn where the drag has taken it, in the selection's colour,
    /// by the caller — and drawing it here as well would lay it down twice.
    fn fill_ink(
        &mut self,
        list: &[Arc<note_format::ink::Stroke>],
        mapping: &Mapping,
        cull: Bounds,
    ) {
        let in_hand = Arc::clone(&self.ink_in_hand);

        let Self {
            ink: vertices,
            scratch,
            sweep,
            translucent_ink,
            ..
        } = self;

        // A stroke that is not opaque is filled through the stencil, one stroke at a time, so the draw needs
        // to know where each one's pieces begin. Only those are recorded: opaque ink is drawn as it always
        // was, and a page of pen pays nothing for a feature it does not use.
        translucent_ink.clear();

        for (index, stroke) in list.iter().enumerate() {
            // A mask that is short is not an error — a frame describes what it has — and a stroke the
            // caller draws itself is skipped wherever its flag says so.
            let held = index < in_hand.len() && in_hand[index];
            if held || !stroke.visible_in(cull) {
                continue;
            }

            let colour = ink_colour(stroke.color);
            open(translucent_ink, vertices.len(), colour);
            fill(
                vertices,
                scratch,
                sweep,
                stroke,
                mapping,
                (0.0, 0.0),
                colour,
            );
        }
    }

    /// Extends the triangles below with the page's last stroke, which is all an append adds.
    ///
    /// The one path the explicit-rebuild design exists for. It fills the appended stroke alone — a
    /// hundred-odd corners — and leaves every vertex before it untouched, so what the next upload copies
    /// is still the whole list but the *work* is one stroke's. Everything before it is reused as it stands.
    ///
    /// Only called for [`InkPlan::Append`], whose proof is exactly what this relies on: the one new stroke
    /// is the page's last, and every stroke before it is the one already drawn.
    fn append_ink(&mut self, canvas: &Canvas) {
        let ink = &canvas.ink;
        let list = ink.strokes.as_slice();
        let mapping = Mapping::of(canvas);
        let cull = self.ink_covered;

        // The number of strokes the triangles below already are: `decide` proved they run from the
        // front of the page, so the one that is new is at this index.
        let drawn = self.delta.map_or(0, |state| state.covered);

        if let Some(stroke) = list.get(drawn) {
            if stroke.visible_in(cull) {
                let colour = ink_colour(stroke.color);

                let Self {
                    ink: vertices,
                    scratch,
                    sweep,
                    translucent_ink,
                    ..
                } = self;

                open(translucent_ink, vertices.len(), colour);
                fill(
                    vertices,
                    scratch,
                    sweep,
                    stroke,
                    &mapping,
                    (0.0, 0.0),
                    colour,
                );
            }
        }

        // A stroke's vertices are *added* to a buffer, so an append copies the new stroke's bytes on
        // top of what a slot already holds — and nothing before it is written again (see `upload_at`).
        //
        // Each slot's `filled` is left exactly where it is. A slot that was at `[120, 100, 0]` before
        // the stroke that took the list to 130 still says so, and the frames that use the other two
        // copy `100..130` and `0..130` — the corners they are missing, not `120..130`. Advancing every
        // slot to the old length here is the bug this line replaced: it claimed a front for a buffer
        // that had never been given one, and the missing corners were the ones nobody copied again.
        // `upload_ink` raises a slot's figure, and only once its copy has landed.
        self.delta = Some(DeltaState {
            revision: ink.edit,
            covered: list.len(),
            detail: detail_of(ink),
            rotation: ink.rotation.into(),
            paper: paper_of(canvas),
        });
    }

    /// Clears the stages and totals, so a measurement starts from nothing.
    ///
    /// The tuple is a `Duration` spent filling, a `Duration` spent copying, the bytes copied, and the number
    /// of copies — the four numbers a frame's cost is read from.
    pub fn take_stages(&mut self) -> (std::time::Duration, std::time::Duration, u64, u32) {
        let taken = (
            self.stage_refresh,
            self.stage_uploaded,
            self.uploaded_bytes,
            self.upload_count,
        );

        self.stage_refresh = std::time::Duration::ZERO;
        self.stage_uploaded = std::time::Duration::ZERO;
        self.uploaded_bytes = 0;
        self.upload_count = 0;

        taken
    }

    /// The multisampled target, made once per size.
    ///
    /// Four samples a pixel, because the ink is a polygon and Metal has no analytic antialiasing to give it
    /// — where Direct2D, on Windows, fills each stroke with *coverage* rather than samples
    /// (`D2D1_ANTIALIAS_MODE_PER_PRIMITIVE`). Only the resolved drawable survives the pass. Apple GPUs can keep the
    /// multisample attachment in tile memory.
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
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
        if self
            .device
            .supportsFamily(objc2_metal::MTLGPUFamily::Apple1)
        {
            descriptor.setStorageMode(MTLStorageMode::Memoryless);
            if let Some(texture) = self.device.newTextureWithDescriptor(&descriptor) {
                self.msaa = Some(texture.clone());
                return Some(texture);
            }
        }
        descriptor.setStorageMode(MTLStorageMode::Private);

        self.msaa = self.device.newTextureWithDescriptor(&descriptor);
        self.msaa.clone()
    }

    /// The stencil the ink's two passes share, made once per size.
    ///
    /// Multisampled to match [`Self::multisample`], as Metal requires of attachments in one pass — and as
    /// the question needs: a band's edge is a fraction of a pixel, so marking whole pixels would undo the
    /// coverage `SAMPLES` exists to give.
    ///
    /// `None` when the device refuses it, in which case [`Self::ink_pass`] draws the ink without one: a
    /// translucent band that reads as opaque is better than no ink at all.
    fn stencil(&mut self) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        if let Some(texture) = &self.stencil_texture {
            return Some(texture.clone());
        }

        // SAFETY: a 2D descriptor for the size this surface draws at.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                STENCIL,
                self.width,
                self.height,
                false,
            )
        };
        descriptor.setTextureType(MTLTextureType::Type2DMultisample);
        // SAFETY: the sample count the colour target was made with, which this must equal.
        unsafe {
            descriptor.setSampleCount(SAMPLES);
        }
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
        if self
            .device
            .supportsFamily(objc2_metal::MTLGPUFamily::Apple1)
        {
            descriptor.setStorageMode(MTLStorageMode::Memoryless);
            if let Some(texture) = self.device.newTextureWithDescriptor(&descriptor) {
                self.stencil_texture = Some(texture.clone());
                return Some(texture);
            }
        }
        descriptor.setStorageMode(MTLStorageMode::Private);

        self.stencil_texture = self.device.newTextureWithDescriptor(&descriptor);
        self.stencil_texture.clone()
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
            self.translucent_open.clear();
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
            || self.open_rotation != canvas.ink.rotation.turns()
            || points.len() < self.open_points
        {
            self.open.clear();
            self.open_id = canvas.ink.open_id;
            self.open_colour = stroke.color;
            self.open_paper = canvas.ink.paper;
            self.open_rotation = canvas.ink.rotation.turns();
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
        self.translucent_open.clear();
        open(&mut self.translucent_open, 0, colour);
        fill(
            &mut self.open,
            &mut self.scratch,
            &mut self.sweep,
            stroke,
            &mapping,
            (0.0, 0.0),
            colour,
        );
        if self.translucent_open.first() == Some(&self.open.len()) {
            // `fill` drew nothing, so there is nothing to mark.
            self.translucent_open.pop();
        }
        self.open_points = points.len();
    }

    /// Uploads the finished ink into the current frame's buffer.
    ///
    /// One buffer holds the whole list because that is what the draw reads — a page of ink is one draw
    /// call — so the copy is whatever of the list the slot is missing, and the explicit-rebuild design
    /// is what makes that "whatever" small: an append leaves everything before the new stroke in the
    /// buffer, so the copy is one stroke; a rebuild empties every slot, so the copy is the page.
    ///
    /// The stage most of a rebuild's wall time goes into at page size, and the one with a surprise in
    /// it: `StorageModeShared` is memory both the CPU and the GPU read, and on Apple silicon writing it
    /// is not a `memcpy` — the 137 MB a page of 8 182 strokes makes costs several times what copying
    /// the same bytes in ordinary heap does (measured; see `OPT-MAC.md`). So the copy is worth *not*
    /// doing when the slot already has the list, which is what the caller's `filled[slot] == ink.len()`
    /// check is for.
    fn upload_ink(&mut self, slot: usize) {
        let started = std::time::Instant::now();

        // What this slot already holds decides what has to be copied: a rebuild emptied it, and an
        // append left everything before the new stroke in it. Either way the tail is what is missing. A
        // slot the draw loop has already found current — `ink_starts_at` says `None` — is not copied for.
        let Some(from) = self.ink_starts_at(slot) else {
            return;
        };
        let buffer = self.ink_buffers[slot].take();
        let reused = buffer
            .as_ref()
            .is_some_and(|buffer| buffer.length() >= std::mem::size_of_val(&self.ink[..]));

        let (buffer, copied) = Self::upload_at(&self.device, &self.ink, Some(from), buffer);
        self.ink_buffers[slot] = buffer;
        let Some(copied) = copied else {
            // The device refused the buffer: the slot keeps whatever it had, which is not the ink, and
            // its figure stays below the list's length so the next frame using it tries again.
            return;
        };

        // The figure moves only now, past the copy: a slot's `filled` is never ahead of what its buffer
        // was actually given. `upload_at` copied from `from` to the end, so the slot holds all of it.
        self.filled[slot] = self.ink.len();
        self.stage_uploaded += started.elapsed();
        self.uploaded_bytes += copied as u64;
        if !reused {
            self.upload_count += 1;
        }
    }

    /// Uploads the open stroke into the current frame's buffer.
    fn upload_open(&mut self, slot: usize) {
        let buffer = self.open_buffers[slot].take();
        self.open_buffers[slot] = Self::upload(&self.device, &self.open, buffer).0;
    }

    /// Copies `vertices` into `buffer`, growing it when it is too small: a buffer that is made is
    /// copied from its first vertex, which is what the open stroke always wants.
    fn upload(
        device: &ProtocolObject<dyn MTLDevice>,
        vertices: &[Vertex],
        buffer: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    ) -> (
        Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
        Option<usize>,
    ) {
        Self::upload_at(device, vertices, None, buffer)
    }

    /// Copies `vertices` into `buffer`, growing it when it is too small, and says how many bytes were
    /// copied.
    ///
    /// `from` is `None` when the buffer holds nothing of `vertices` — a stroke being uploaded from its
    /// first corner — and `Some(n)` for a slot that already holds the first `n` corners: only the tail
    /// past them is copied, which is what makes an append's upload a stroke. A buffer that had to be
    /// made holds nothing, whatever `from` said, so it is copied into from zero: copying only the tail
    /// into a fresh buffer would leave the front the draw reads uninitialised.
    ///
    /// `(None, None)` when the device would not give a buffer: nothing was written, so nothing is
    /// claimed. The second half is the copy's size in bytes, `0` for a list with nothing to copy.
    fn upload_at(
        device: &ProtocolObject<dyn MTLDevice>,
        vertices: &[Vertex],
        from: Option<usize>,
        buffer: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    ) -> (
        Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
        Option<usize>,
    ) {
        let bytes = std::mem::size_of_val(vertices);
        let mut reused = false;
        let buffer = match buffer.filter(|buffer| buffer.length() >= bytes) {
            Some(buffer) => {
                reused = true;
                buffer
            }
            None => match device
                .newBufferWithLength_options(bytes.max(1), MTLResourceOptions::StorageModeShared)
            {
                Some(buffer) => buffer,
                None => return (None, None),
            },
        };

        // A slot never claims a front its buffer was not given: `ink_write` is the one place the window is
        // decided, from what the slot's figure claims and whether the device kept the buffer.
        let (start, end) = ink_write(from.unwrap_or(0), vertices.len(), reused)
            .map_or((vertices.len(), vertices.len()), |range| {
                (range.start, range.end)
            });
        let tail = &vertices[start..end];
        let copied = std::mem::size_of_val(tail);
        if copied == 0 {
            return (Some(buffer), Some(0));
        }

        let offset = start * std::mem::size_of::<Vertex>();
        // SAFETY: a buffer of at least `bytes` — which is at least `offset + copied` — and this range of
        // it is written once here before anything reads it. The bytes before `offset` belong to vertices
        // an earlier frame copied.
        unsafe {
            std::ptr::copy_nonoverlapping(
                tail.as_ptr().cast::<u8>(),
                buffer.contents().as_ptr().cast::<u8>().add(offset),
                copied,
            );
        }

        (Some(buffer), Some(copied))
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

/// A surface the tests can make without a window.
///
/// Built without a view, a layer or a drawable. `refresh_ink`'s *fill* needs the device's shaders, so
/// this cannot build a page; it can decide — `decide` and `append_ink` are the bookkeeping around the
/// fill — and that is what the slot tests ask: what an append does to the figures the next frame's copy
/// reads, and they get there through the same functions the draw loop and `upload_ink` use.
#[cfg(test)]
impl Surface {
    /// A surface with a device and its pipelines, and nothing that needs a window.
    ///
    /// `None` on a machine with no Metal device, which is a machine that cannot draw ink at all.
    fn with_device() -> Option<Self> {
        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue().expect("a queue");
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .expect("the shaders compile");
        let solid = pipeline(&device, &library, "solid_vertex", "solid_fragment")
            .expect("the solid pipeline");
        let page =
            pipeline(&device, &library, "page_vertex", "page_fragment").expect("the page pipeline");
        let stencil = pipeline(&device, &library, "stencil_vertex", "stencil_fragment")
            .expect("the stencil pipeline");
        let counting = stenciling(&device).expect("a counting stencil");
        let counted = stencilled(&device).expect("a counted stencil");

        let sampler_descriptor = MTLSamplerDescriptor::new();
        let sampler = device
            .newSamplerStateWithDescriptor(&sampler_descriptor)
            .expect("a sampler");

        Some(Surface {
            device,
            queue,
            solid,
            page,
            stencil,
            counting,
            counted,
            sampler,
            layer: CAMetalLayer::layer(),
            ink: Vec::new(),
            delta: None,
            ink_in_hand: Arc::new(Vec::new()),
            ink_paint: Paint::nothing(),
            ink_covered: Bounds::NOTHING,
            open: Vec::new(),
            open_id: 0,
            open_points: 0,
            open_colour: 0,
            open_paper: (0.0, 0.0),
            open_rotation: 0,
            translucent_ink: Vec::new(),
            translucent_open: Vec::new(),
            ink_buffers: [None, None, None],
            filled: [0; 3],
            stage_uploaded: std::time::Duration::ZERO,
            uploaded_bytes: 0,
            upload_count: 0,
            builds: 0,
            stage_refresh: std::time::Duration::ZERO,
            open_buffers: [None, None, None],
            buffer_slot: 0,
            scratch: Vec::new(),
            sweep: Sweep::default(),
            page_texture: None,
            msaa: None,
            stencil_texture: None,
            last_commit: None,
            width: 1,
            height: 1,
        })
    }

    /// Puts slot `slot` in the state a frame that was given the first `corners` vertices of the page
    /// leaves it in: a buffer with room for `room` vertices holding exactly those, and the figure.
    ///
    /// `room` is the buffer's whole length in vertices, which is what decides whether the next frame keeps
    /// it or makes another — so a test says which of the two states it is starting from instead of leaving
    /// it to whatever size the page happens to be. The front past `corners` is uninitialised, honestly:
    /// nothing has copied it, and the frames a test drives are what writes it.
    fn give_front(&mut self, slot: usize, corners: usize, room: usize) {
        let corners = corners.min(self.ink.len());
        let buffer = self
            .device
            .newBufferWithLength_options(
                (room * std::mem::size_of::<Vertex>()).max(1),
                MTLResourceOptions::StorageModeShared,
            )
            .expect("a buffer");
        let bytes = corners * std::mem::size_of::<Vertex>();

        // SAFETY: the buffer has room for `room` vertices, which is at least `corners`, so at least
        // `bytes`; the front is `bytes`, written once before anything reads.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.ink.as_ptr().cast::<u8>(),
                buffer.contents().as_ptr().cast::<u8>(),
                bytes,
            );
        }

        self.ink_buffers[slot] = Some(buffer);
        self.filled[slot] = corners;
    }

    /// Gives every slot the front `fronts` says, with room for the whole list and its next stroke: what a
    /// rebuild followed by some frames leaves behind, and what keeps an append's upload a tail.
    fn stage_fronts(&mut self, fronts: [usize; 3]) {
        let room = (self.ink.len() * 2).max(self.ink.len() + 1);

        for (slot, corners) in fronts.into_iter().enumerate() {
            self.give_front(slot, corners, room);
        }
    }

    /// The bytes of slot `slot`'s `MTLBuffer`: the page the GPU would draw from it.
    ///
    /// A blit this surface's own queue runs, because a device's buffer may be `StorageModePrivate` and its
    /// `contents()` is then not to be read. The copy is into a shared buffer of the same length, on the GPU,
    /// with a wait before a byte is read — which is the same round trip a frame's present makes.
    fn contents(&self, slot: usize) -> Vec<u8> {
        // The trait that declares `blitCommandEncoder`, imported here because only this helper asks the
        // device for one: the app's frames encode render passes, never blits.
        use objc2_metal::MTLBlitCommandEncoder;

        let buffer = self.ink_buffers[slot].as_ref().expect("a slot's buffer");
        let bytes = buffer.length();
        let read = self
            .device
            .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
            .expect("a buffer to read back into");

        let commands = self.queue.commandBuffer().expect("a command buffer");
        let blit = commands.blitCommandEncoder().expect("a blit encoder");
        // SAFETY: two buffers of `bytes` each, copied whole.
        unsafe {
            blit.copyFromBuffer_sourceOffset_toBuffer_destinationOffset_size(
                buffer, 0, &read, 0, bytes,
            );
        }
        blit.endEncoding();
        commands.commit();
        commands.waitUntilCompleted();

        // SAFETY: the shared buffer's own memory, `bytes` long, written by the blit above before it
        // completed.
        (0..bytes)
            .map(|byte| {
                // SAFETY: `byte` is inside the buffer, which is `bytes` long and aligned for a `u8`.
                unsafe { *read.contents().as_ptr().cast::<u8>().add(byte) }
            })
            .collect()
    }
}

/// Notes that the vertices after `at` are one translucent stroke's pieces.
///
/// The stencil pass is what keeps a stroke one layer, and it needs a stroke at a time: mark this stroke's
/// pieces, colour the marked area once, then clear the stencil for the next. Two strokes therefore stay two
/// layers, which is what the reader drew — `1 − 0.65²` where a second band crosses the first — while the
/// pieces *within* one stroke stay one.
///
/// A stroke that filled nothing has nothing to mark, and one that is opaque is drawn without the stencil
/// (`fill` already made its colour opaque), so neither is recorded.
fn open(translucent: &mut Vec<usize>, at: usize, colour: gpui_kit::Hsla) {
    if colour.a >= 1.0 {
        return;
    }

    if at > translucent.last().copied().unwrap_or(0) {
        translucent.push(at);
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
        Vertex {
            x,
            y,
            colour: Self::rgb(colour),
        }
    }

    /// The colour half of [`Vertex::new`], on its own.
    ///
    /// A stroke's corners all carry the same colour, so packing it belongs to the stroke rather than to
    /// each corner: `fill` calls this once and hands the four bytes to every corner it makes.
    fn rgb(colour: gpui_kit::Hsla) -> [u8; 4] {
        let rgba = gpui_kit::Rgba::from(colour);
        let byte = |component: f32| (component.clamp(0.0, 1.0) * 255.0).round() as u8;

        [byte(rgba.r), byte(rgba.g), byte(rgba.b), byte(rgba.a)]
    }
}

/// A finished stroke, filled exactly: its ribbon, triangulated once and kept.
///
/// The outline is the ribbon's edge, and it is a polygon — one Direct2D fills in a single primitive on
/// Windows, which is why a stroke that crosses itself there is ink and not a hole. Metal has no such fill,
/// so the band is cut into triangles here, once per stroke, and the triangles are cached with the rest of
/// the ink (see [`InkKey`]).
fn fill(
    vertices: &mut Vec<Vertex>,
    scratch: &mut Vec<(f32, f32)>,
    sweep: &mut Sweep,
    stroke: &note_format::ink::Stroke,
    mapping: &Mapping,
    shift: (f32, f32),
    colour: gpui_kit::Hsla,
) {
    if stroke.outline.len() < 3 {
        return;
    }

    // The ribbon's pieces are what the band is filled from: the trapezoid between two readings and the
    // disc at each of them (see `note_format::ink::pieces_of`). They come from the ribbon's own definition
    // — every reading's disc is inside it, and two readings are joined by the trapezoid their outer
    // tangents cut — so there is nothing to sort and no case to get wrong.
    //
    // The pieces are a *cache*, and a page that was given up to save memory has none (see
    // `Stroke::shed_pieces`). `pieces_at_detail` is what builds the ones this frame needs from the points —
    // a stroke is a few hundred corners, and the strokes being drawn are the ones on screen — so the band
    // is filled from polygons however the page arrived.
    //
    // The pieces overlap by construction (a disc reaches into its neighbour's trapezoid), and the stencil
    // pass is what makes that one layer of ink: it marks which samples the pieces reach and colours each
    // of them once (`Surface::ink_pass`). A stroke with no pieces at all — an older page, a test that made
    // its own outline — falls back to the sweep over the boundary, which fills by the nonzero rule.
    let colour = Vertex::rgb(colour);
    let polygons = stroke.pieces_at_detail();

    if !polygons.is_empty() {
        pieces(vertices, &polygons, mapping, shift, colour);
        return;
    }

    scratch.clear();
    scratch.extend(
        stroke
            .outline
            .iter()
            .map(|point| mapping.point(*point, shift)),
    );

    sweep.run(vertices, scratch, colour);
}

/// Fills a ribbon's pieces: two triangles a quad, a triangle a fan.
///
/// The sweep over the boundary finds the boundary's edges again, sorts them, and walks a band at a time;
/// the pieces already say what they are, so there is nothing to find and nothing to sort. Measured on a
/// page of 8,182 strokes: 395 trapezoids a stroke became 125 quads, and the fill's share of a rebuild fell
/// with it (`OPT-MAC.md`).
///
/// A quad here is the trapezoid between two readings, whose four corners are that pair's outer tangent
/// points — always convex, and always wound the same way as the pen. A fan is a wedge of a disc.
fn pieces(
    vertices: &mut Vec<Vertex>,
    list: &[note_format::ink::Piece],
    mapping: &Mapping,
    shift: (f32, f32),
    colour: [u8; 4],
) {
    // The unturned page asks for nothing: every corner is already where it goes, and the arithmetic
    // below would be a multiply and an add per axis per corner for no change (see `is_identity`). The
    // shift is the one thing that can still move a corner, so it is what the fast path checks.
    if mapping.is_identity() && shift == (0.0, 0.0) {
        let corner = |[x, y]: [f32; 2]| Vertex { x, y, colour };

        for piece in list {
            match *piece {
                note_format::ink::Piece::Quad {
                    left_from,
                    left_to,
                    right_to,
                    right_from,
                } => {
                    let (a, b, c, d) = (
                        corner(left_from),
                        corner(left_to),
                        corner(right_to),
                        corner(right_from),
                    );

                    vertices.extend_from_slice(&[a, b, c, a, c, d]);
                }
                note_format::ink::Piece::Fan { centre, from, to } => {
                    let (o, a, b) = (corner(centre), corner(from), corner(to));

                    vertices.extend_from_slice(&[o, a, b]);
                }
            }
        }

        return;
    }

    let vertex = |corner: [f32; 2]| {
        let (x, y) = mapping.point(corner, shift);

        Vertex { x, y, colour }
    };

    for piece in list {
        match *piece {
            note_format::ink::Piece::Quad {
                left_from,
                left_to,
                right_to,
                right_from,
            } => {
                let (a, b, c, d) = (
                    vertex(left_from),
                    vertex(left_to),
                    vertex(right_to),
                    vertex(right_from),
                );

                vertices.extend_from_slice(&[a, b, c, a, c, d]);
            }
            note_format::ink::Piece::Fan { centre, from, to } => {
                let (o, a, b) = (vertex(centre), vertex(from), vertex(to));

                vertices.extend_from_slice(&[o, a, b]);
            }
        }
    }
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
    Sweep::default().run(vertices, polygon, Vertex::rgb(colour));
}

/// The buffers one tessellation sweeps through, kept so a page's worth of strokes shares one set.
///
/// A rebuild fills a polygon per stroke — thousands on a page — and each call used to make and then free
/// four `Vec`s of its own. Nothing here depends on which polygon it is, so the surface keeps one set and
/// every stroke in a rebuild spends it (`OPT-MAC.md`).
#[derive(Default)]
struct Sweep {
    bands: Vec<f32>,
    edges: Vec<Edge>,
    crossings: Vec<Crossing>,
    active: Vec<usize>,
}

/// Every edge, with the span of heights it covers. A horizontal edge crosses nothing and is left out.
struct Edge {
    low: f32,
    high: f32,
    from: (f32, f32),
    to: (f32, f32),
    winding: i32,
}

/// One crossing of a band: where it is at the band's middle (which is what orders two crossings), which
/// way its edge runs, and where it meets the band's own bottom and top — one crossing gives a trapezoid
/// two of its corners.
struct Crossing {
    middle: f32,
    winding: i32,
    bottom: f32,
    top: f32,
}

impl Sweep {
    fn run(&mut self, vertices: &mut Vec<Vertex>, polygon: &[(f32, f32)], colour: [u8; 4]) {
        if polygon.len() < 3 {
            return;
        }

        let Sweep {
            bands,
            edges,
            crossings,
            active,
        } = self;

        bands.clear();
        bands.extend(polygon.iter().map(|point| point.1));
        bands.sort_by(|left, right| left.total_cmp(right));
        bands.dedup_by(|left, right| (*left - *right).abs() < f32::EPSILON);

        edges.clear();

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

        active.clear();
        let mut next = 0;

        for band in bands.windows(2) {
            let (bottom, top) = (band[0], band[1]);

            // `bands` is sorted, so `top >= bottom` already — the `.abs()` states that a band of no height
            // is what is being skipped, rather than a band that happens to be upside down.
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

            for crossing in crossings.iter() {
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
}

/// One trapezoid of a band, as two triangles.
fn trapezoid(
    vertices: &mut Vec<Vertex>,
    left_bottom: (f32, f32),
    left_top: (f32, f32),
    right_top: (f32, f32),
    right_bottom: (f32, f32),
    colour: [u8; 4],
) {
    if (right_bottom.0 - left_bottom.0).abs() < f32::EPSILON
        && (right_top.0 - left_top.0).abs() < f32::EPSILON
    {
        return;
    }

    // The packed colour, shared by all six corners: the trapezoid's shape is the only thing that differs
    // between one band and the next, so nothing here converts a colour.
    let vertex = |corner: (f32, f32)| Vertex {
        x: corner.0,
        y: corner.1,
        colour,
    };

    vertices.extend_from_slice(&[
        vertex(left_bottom),
        vertex(right_bottom),
        vertex(right_top),
        vertex(left_bottom),
        vertex(right_top),
        vertex(left_top),
    ]);
}

/// How far the body's shadow is dropped, in logical pixels at 100% — the Windows overlay's own offset, so
/// that the two builds' cursors are one drawing. The interface scale multiplies it with the body.
const BODY_SHADOW_DROP: [f32; 2] = [3.0, 3.6];

/// How opaque the body's shadow is at full lean: the widest and faintest of the three passes.
const BODY_SHADOW_ALPHA: f32 = 0.26;

/// The pen, where the nib is: the body when it leans, and the nib's own disc.
///
/// The same drawing the Windows overlay makes, from the same geometry (`note_ui::cursor`) and the same
/// constants — so a pen looks the same on both platforms. It is drawn here, in the canvas's pass, rather
/// than in a window of its own: a cursor is above the ink and below the interface, and only this pass is.
fn cursor_vertices(vertices: &mut Vec<Vertex>, mark: &Mark, scale: f32) {
    let scale = finite_or_one(scale);
    let unit = finite_or_one(mark.unit);
    let position = mark.cursor.position();
    let nib = (position[0] * scale, position[1] * scale);
    let fade = mark.cursor.body_fade();
    // The state's weight under the lean's own: faint in the air, heavier on the paper. This is the whole of what
    // says the pen has landed, because the nib's dot cannot — it is the ink's own colour in both states.
    let weight = mark.cursor.body_alpha();

    if let Some(body) = mark.cursor.body_shape_at(unit) {
        // Shadow, soft edge, body: the order and the colours the Windows overlay uses. The body
        // already carries the interface scale; the passes around it carry it here.
        let drop = [BODY_SHADOW_DROP[0] * unit, BODY_SHADOW_DROP[1] * unit];
        polygon(
            vertices,
            &shifted(body.outline(0.0), drop),
            scale,
            mark.halo,
            BODY_SHADOW_ALPHA * weight * fade,
        );
        polygon(
            vertices,
            &body.outline(BODY_HALO_GROW * unit),
            scale,
            mark.halo,
            BODY_HALO_ALPHA * weight * fade,
        );
        polygon(
            vertices,
            &body.outline(0.0),
            scale,
            mark.colour,
            weight * fade,
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
        radius + NIB_BLOOM_SPREAD * unit * scale,
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

/// The stencil the ink's two passes share: eight bits, and no depth at all.
///
/// The ink needs an *order*, not a distance — the first piece of a stroke to reach a sample takes it — so
/// the stencil is the whole of what the two passes share. Eight bits is one layer per sample rather than a
/// counter, which is what makes it safe: any piece that reaches a sample sets it, so how many pieces overlap
/// cannot overflow it.
const STENCIL: MTLPixelFormat = MTLPixelFormat::Stencil8;

/// The value the stencil holds where nothing has been coloured: what the frame clears to.
const STENCIL_EMPTY: u32 = 0;

/// The value written where a sample has been coloured, and so must not be coloured again.
///
/// `MTLStencilOperation::Replace` writes the encoder's **reference value**, not a value in the descriptor, so
/// the two passes take turns setting it (see [`ink_pass`]): the mark pass runs with this, marking every sample
/// a stroke's pieces reach; the colour pass runs with [`STENCIL_EMPTY`], keeps only the samples nothing has
/// reached, and writes this over each one it colours.
const STENCIL_MARKED: u32 = 1;

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

/// The region of the paper the ink is built for: the visible box, grown by [`CULL_SPAN`] of itself.
///
/// Grown on every side, so a scroll in any direction — and a zoom around any point — has room inside it. The
/// growth is a fraction of the *visible extent*, which is what makes the same constant right at every zoom: a
/// screen at 400% shows a quarter of the paper and gets a quarter of the margin, and the margin is still half a
/// screen of scrolling in device pixels.
fn cull_of(visible: Bounds) -> Bounds {
    let (width, height) = visible.size();

    visible.grown(width * CULL_SPAN, height * CULL_SPAN)
}

/// Whether `visible` still fits inside the region the ink was built for.
///
/// The whole test, and the reason it is containment rather than distance: a box that fits costs nothing,
/// whatever moved to get there, and one that does not is a build. A scroll inside the margin fits; a zoom
/// inward fits; a zoom outward does not, because it shows paper the build never covered.
fn fits(visible: Bounds, built_for: Bounds) -> bool {
    visible.within(built_for)
}

/// Where a slot's finished-ink copy begins: `None` when the slot already holds all `longest` corners,
/// `Some(n)` when it holds the first `n` and the rest has to be copied.
///
/// The whole of what a slot's figure means. A rebuild empties every slot (`0`), and an append leaves the
/// figures where they are, so a slot that was at `100` of a list that grew to `130` begins at `100` — not
/// at `120`, which is where the front of the *last change* began. A figure that has run past the list (a
/// page undone to a shorter one) begins at the list's end and copies nothing, which is the same answer as
/// a slot that holds it — the draw reads `filled[slot] == ink.len()`, so the next append copies the whole
/// difference.
fn ink_upload(filled: usize, longest: usize) -> Option<usize> {
    if filled == longest {
        None
    } else {
        Some(filled.min(longest))
    }
}

/// The range of `vertices` a copy writes: from where a slot's buffer was given vertices to the list's end.
///
/// The one place the window is decided, so what a slot's figure claims and what its buffer was actually
/// given cannot be two decisions. `None` when the slot already holds the list — the frame's whole point is
/// that it then copies nothing — and otherwise from where the buffer's own contents end: a reused buffer
/// past those `filled` vertices, a made one from its first.
///
/// A figure past the list's end (a page undone to a shorter one) is the list's end: an empty copy, which
/// is what a slot that holds everything gets too.
fn ink_write(filled: usize, vertices: usize, reused: bool) -> Option<std::ops::Range<usize>> {
    ink_upload(filled, vertices).map(|from| {
        let start = ink_copy_start(from, reused);

        start..vertices
    })
}

/// Where a copy begins, given the figure the slot claims and whether the device gave back the buffer it
/// was handed.
///
/// A new buffer holds nothing, so it is copied into from zero however far the slot's figure had got; a
/// buffer the device kept holds the first `filled` corners, so only the tail past them is copied. This is
/// the rule `Surface::upload_at` applies, stated once so that what a slot claims and what its buffer was
/// given cannot be decided differently in two places.
fn ink_copy_start(filled: usize, reused: bool) -> usize {
    if reused {
        filled
    } else {
        0
    }
}

/// What a build baked about the selection: the mask it read, the colour it tinted the ink in hand with, and
/// where a drag had taken it.
///
/// The page's revision says what the *ink* is; this says what was drawn over it. A selection is not an edit
/// — a loop swept around a stroke, Escape letting it go, a colour the theme changed — so none of those
/// moves [`Ink::edit`], and a renderer comparing only the revision would keep drawing the ink in the
/// colour of a selection that is no longer there. This is the state [`decide`] compares instead.
///
/// Two ways of holding "no selection", and both are needed:
///
/// * [`Paint::nothing`] — no mask, no tint. The configured value for a surface that has baked nothing, and
///   what a page nobody has selected anything on reduces to whatever `Vec` its empty mask came in: a
///   description that carries `Arc::new(Vec::new())` one frame and `vec![false; n]` the next with nothing in
///   hand is the same frame, and comparing the `Arc`s would rebuild for it.
/// * a mask with no flags set — what [`note_ui::ink::InkDocument::deselect`] hands a frame. It is *not* the
///   same as `NOTHING`, because a tint over a mask with nothing in it draws nothing: a surface that baked a
///   colour with no ink in hand and one that baked none at all are the same picture, so both reduce here.
///
/// The remaining fields are compared only while the mask is a selection, which is the case a tint is drawn
/// in. That is why a drag's offset is part of this and not of [`DeltaState`]: a drag moves the ink in hand
/// and the page has not been touched, so the offset has to make a frame a `Paint` rebuild on its own.
#[derive(Debug)]
struct Paint {
    /// The strokes in hand. Compared by `Arc::ptr_eq` and then by content, so a mask that was not rebuilt is
    /// O(1) to compare and one that was is compared only until it differs — a handful of flags in practice.
    mask: Arc<Vec<bool>>,
    /// The colour the ink in hand is drawn in, and the lasso's loop, which share one number. `None` is a
    /// frame with nothing to show; see [`Paint::of`] for why that is decided from the mask rather than from
    /// the colour alone.
    tint: Option<[u32; 4]>,
    /// How far a drag has taken the ink in hand, as the bits of the two numbers — `f32` has no `Eq`, and a
    /// NaN in a drag would make a comparison that never matches. `+0.0` and `-0.0` are one offset; the app
    /// drags in paper units and the one it uses for "not dragging" is `(0.0, 0.0)`.
    offset: [u32; 2],
}

impl Paint {
    /// A surface that has baked no selection: no mask, no tint, no drag.
    ///
    /// A function rather than a `const`, because an empty mask is an `Arc` and a `const` may not allocate. It
    /// is made where a surface is made and nowhere per frame, so the allocation is a surface's start.
    fn nothing() -> Paint {
        Paint {
            mask: Arc::new(Vec::new()),
            tint: None,
            offset: [0, 0],
        }
    }

    /// What baking `ink` records: no selection when there is nothing in hand, and otherwise the mask, the
    /// colour in hand and the drag.
    ///
    /// The mask is shared rather than copied — the description's `Arc` is the page's, and rebuilding it would
    /// turn this bookkeeping into a page-sized walk once per build.
    ///
    /// A colour with no ink in hand and no loop is no selection: the bake draws the tinted copy only for the
    /// flags that are set and the loop only while one is being swept, so a mask with nothing in it draws
    /// exactly what a frame that carries no colour does. That is the pair of cases
    /// [`crate::ink::InkDocument::deselect`] leaves behind — its mask is one flag per stroke, all clear, and
    /// the app still names the selection colour — and reading them as the same picture is what keeps a
    /// deselected page from rebuilding on every frame after it.
    fn of(ink: &Ink) -> Self {
        let tint = ink.selection.map(colour_key);
        let shown =
            tint.is_some() && (ink.selected.iter().any(|flag| *flag) || ink.lasso.is_some());

        if !shown {
            return Paint::nothing();
        }

        Paint {
            mask: Arc::clone(&ink.selected),
            tint,
            offset: [ink.offset.0.to_bits(), ink.offset.1.to_bits()],
        }
    }

    /// Whether this is the `other` bake: the same strokes in hand, in the same colour, where the drag has
    /// taken them.
    ///
    /// O(1) while the mask is the same `Arc`, which is the still selection this whole type exists for: a
    /// selection held across a pan is one pointer compare per frame, not a walk of a page's flags.
    fn is(&self, other: &Paint) -> bool {
        if self.tint != other.tint || self.offset != other.offset {
            return false;
        }
        if Arc::ptr_eq(&self.mask, &other.mask) {
            return true;
        }

        // A mask that was rebuilt with the same flags — a lasso swept round the same strokes again — is the
        // same picture. Its length is compared first, so the empty masks that are not one `Arc` (a cold
        // surface's, a description's, a page's) cost one compare rather than a walk of a page's flags.
        self.mask.len() == other.mask.len() && self.mask.iter().eq(other.mask.iter())
    }
}

/// A selection colour as the bits a comparison can hold.
///
/// Taken from the four numbers the shader is handed rather than from `Hsla` itself, so that the comparison
/// is on what is drawn and a NaN — which no caller makes, the theme's colours being constants — cannot make
/// a bake that never matches itself. That is the one failure mode this must not have: a comparison that
/// cannot settle is a rebuild on every frame of a selection that is standing still.
fn colour_key(colour: gpui_kit::Hsla) -> [u32; 4] {
    [colour.h, colour.s, colour.l, colour.a]
        .map(|number| if number.is_nan() { f32::NAN } else { number }.to_bits())
}

/// What a frame's page needs: the triangles below as they are, one stroke more, or a rebuild.
///
/// `refresh_ink` and the tests share this one answer, which is the point: reuse, append and rebuild
/// used to be two decisions — a short-circuit at the top of `refresh_ink` and a `None`/`Some` from
/// `decide` — so a test that read `None` could not say whether the frame would have reused the ink or
/// appended to it, and a condition the test did not share was a condition the test could not check.
#[derive(Clone, Copy, Debug, PartialEq)]
enum InkPlan {
    /// The page is the one below, at the same rung, the same way up, and still inside the build.
    Reuse,
    /// The page is the one below plus one stroke at its end ([`Surface::append_ink`]).
    Append,
    /// Every triangle has to be made again, and this names why ([`Demotion`]).
    Rebuild(Demotion),
}

/// Decides what a frame whose page may not be the one below has to do: reuse, append, or rebuild.
///
/// `Reuse` means the triangles below are still the page the caller is describing, so nothing is
/// rebuilt and nothing uploaded. `Append` means the page is the one that was drawn plus one stroke at
/// its end (`Append { at, id }`, where `at` is the length *before* the change and `id` is that stroke),
/// and every triangle already there is still right — [`Surface::append_ink`] can extend it.
/// `Rebuild(reason)` names the condition that decided it, so a measurement never has to say "it
/// rebuilt, for some reason".
///
/// Everything here is read from numbers the surface already holds: the page's revision, how much of it
/// is covered, what the change *said*, and what the last build baked about the selection. Nothing walks
/// the strokes, which is what makes an append's bookkeeping O(1) rather than O(page) — the one stroke it
/// reads is the page's last, to prove the append named the stroke that is actually there, and not one put
/// back in the middle. The mask the selection comparison reads is the page's own `Arc`, compared by
/// pointer first, so a selection standing still costs one compare and not a walk of the page's flags. A
/// free function rather than a method so that it can be tested without a Metal device.
fn decide(canvas: &Canvas, state: &DeltaState, built_for: Bounds, painted: &Paint) -> InkPlan {
    let ink = &canvas.ink;

    // The detail or the turn moved: every triangle is stale, whatever the edit was.
    if state.detail != detail_of(ink) {
        return InkPlan::Rebuild(Demotion::Rung);
    }
    if state.rotation != u8::from(ink.rotation) {
        return InkPlan::Rebuild(Demotion::Rotation);
    }
    if state.paper != paper_of(canvas) {
        return InkPlan::Rebuild(Demotion::Paper);
    }

    // A selection, a drag or a lasso changes what is baked, not only what the change appended — and it is
    // the *difference* from the last bake that changes it, not the presence. A selected stroke is drawn
    // twice: once in the page, once where the drag has taken it and in the selection's colour. That second
    // copy is not a stroke of the page and no revision counts it, so a frame whose selection is not the one
    // below rebuilds — and a frame whose selection is the one below reuses, which is what stops a held
    // selection from walking the page on every frame the view happens to draw.
    //
    // A lasso is not compared: its loop is a stroke that changes with every reading, so a *dragging* lasso
    // is always a rebuild, and the sweep that ends in a selection is a rebuild on the frame that names it.
    // Reading `lasso.is_some()` as "always rebuild" is the same answer for less bookkeeping, and it keeps
    // this rule one comparison (see `Paint`).
    let now = Paint::of(ink);
    if ink.lasso.is_some() || !painted.is(&now) {
        return InkPlan::Rebuild(Demotion::Paint);
    }

    // The region the build covers has to still hold the screen, or the append would add a stroke nothing
    // can see and leave paper the view now shows with no triangles at all.
    if !fits(ink.visible, built_for) {
        return InkPlan::Rebuild(Demotion::Uncovered);
    }

    // The page is the one below: the same revision, the same strokes, and every one of them is already
    // drawn. A pan, a zoom that stayed inside a rung, a second frame of one edit.
    if state.revision == ink.edit
        && state.covered == ink.strokes.len()
        && state.paper == paper_of(canvas)
        && state.detail == detail_of(ink)
        && state.rotation == u8::from(ink.rotation)
    {
        return InkPlan::Reuse;
    }

    // The change has to follow the one that was applied: it takes the page this surface drew to the one
    // after it. A revision that is not the next one — or no delta at all for a page that moved — is a
    // change that went by unread, and what it did cannot be guessed: rebuild.
    let Some(delta) = ink.delta.as_ref() else {
        return InkPlan::Rebuild(Demotion::Gap);
    };
    if delta.before != state.revision || delta.before.wrapping_add(1) != ink.edit {
        return InkPlan::Rebuild(Demotion::Gap);
    }

    // And it has to be exactly one append at the end — the page's last stroke, where the page already
    // ended. Anything else — a removal, a stroke put back in the middle, a drag — moves ink the
    // triangles below were built from. A redo of an erase that lands in the middle is the case a count
    // alone let through: the page grew by one, but at a place the append path cannot reach.
    match delta.changes.as_slice() {
        [InkChange::Append { id, at }]
            if *at == state.covered
                && state.covered.checked_add(1) == Some(ink.strokes.len())
                && ink.strokes.last().map(StrokeId::of) == Some(*id) =>
        {
            InkPlan::Append
        }
        _ => InkPlan::Rebuild(Demotion::NotAnAppend),
    }
}

/// What this surface knows about the page it drew, without looking at it.
///
/// The explicit-rebuild design's state, and the thing that decides whether a frame costs anything:
///
/// * `revision` is the page's own edit count as of the last build. A frame whose page has the same
///   revision and a covering rectangle that still fits has nothing to do (see [`decide`]).
/// * `covered` is how many strokes at the front of the page the triangles below already are. An
///   append extends it; any other edit shortens it to the place the edit started.
/// * `detail` and `paper` are the two things the triangles are a function of *besides* the strokes.
///   A rung change or a page turn rebuilds every triangle, and no delta can avoid it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DeltaState {
    revision: u64,
    covered: usize,
    detail: u32,
    rotation: u8,
    paper: [u32; 2],
}

/// The detail rung the ink of a description is drawn for, as the bits a comparison can hold.
///
/// `Ink::revision` is what says the outlines themselves were rebuilt — a zoom that crossed a rung does
/// not change a single stroke's identity (see `InkLayer`'s own note on it in the canvas description),
/// so the rung is the only other thing the triangles are a function of that the strokes cannot say.
fn detail_of(ink: &Ink) -> u32 {
    ink.detail
        .unwrap_or_else(|| note_format::ink::detail_zoom(1.0).to_bits())
}

/// The paper's own shape, as bits: the triangles are baked in the paper's units, so a different paper is
/// a different set of triangles.
fn paper_of(canvas: &Canvas) -> [u32; 2] {
    let paper = canvas.ink.paper;
    [paper.0.to_bits(), paper.1.to_bits()]
}

/// Why a frame did not append, when it looked like it could have.
///
/// The design document's §5 asks a measurement to record a *reason* for every rebuild it allows, and
/// this is the reason set: an append that had to become a rebuild names which condition it failed,
/// so "the page grew a stroke" is never an unexplained page of `memcpy`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Demotion {
    /// Nothing to append to: the surface has never built this page.
    Cold,
    /// The page's revision is not the one after the last one seen: a change went by unread.
    Gap,
    /// The edit was not an append — a removal, a stroke put back in the middle, a drag, or an append
    /// that did not name the page's last stroke.
    NotAnAppend,
    /// The detail rung the zoom asks for moved, so every outline is stale whatever the edit was.
    Rung,
    /// The page was turned, so the mapping the triangles were baked through is not this one.
    Rotation,
    /// The paper's own shape changed: a page was read, or the sheet's units did.
    Paper,
    /// The view shows paper the build never covered.
    Uncovered,
    /// The stroke in hand, the selection or the lasso changed what is drawn.
    Paint,
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
        // The paper's origin and its two unit steps, turned the way the page is turned: the one
        // conversion, asked of the view rather than worked out here (see `InkTransform::basis`).
        let (base, step_x, step_y) = canvas.ink.transform().basis();

        Mapping {
            base,
            step_x,
            step_y,
        }
    }

    fn point(&self, [x, y]: [f32; 2], shift: (f32, f32)) -> (f32, f32) {
        let (x, y) = (x + shift.0, y + shift.1);
        let across = self.base.0 + x * self.step_x.0 + y * self.step_y.0;
        let down = self.base.1 + x * self.step_x.1 + y * self.step_y.1;

        (across, down)
    }

    /// Whether the map leaves a corner where it is, so a caller can skip the arithmetic entirely.
    ///
    /// The common page is unturned and unmoved: `drawn_of_paper` is the identity for no turns, so
    /// `base`/`step_x`/`step_y` are `(0,0)`, `(1,0)`, `(0,1)` and every corner is already the answer.
    /// A rebuild that asked `point` anyway would spend two multiplies and an add per axis per corner —
    /// about 11.5 million corners at 8 182 strokes — to compute a value it already had.
    fn is_identity(&self) -> bool {
        self.base == (0.0, 0.0) && self.step_x == (1.0, 0.0) && self.step_y == (0.0, 1.0)
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
        let scale = finite_or_one(canvas.scale);
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
    pub fn draw(&mut self, canvas: &Canvas, mark: Option<&Mark>) -> bool {
        self.draw_with_wait(canvas, mark).0
    }

    /// [`Self::draw`], and how much of it was spent waiting for a drawable.
    ///
    /// The wait is the display's clock, not the app's work: `nextDrawable` blocks while the compositor
    /// still holds the layer's buffers, which is why a frame's total carries one refresh interval even
    /// for an empty page. The benchmark needs the two apart to say what a rebuild actually costs, and a
    /// figure it can only get from inside the first call of the frame.
    pub fn draw_with_wait(&mut self, canvas: &Canvas, mark: Option<&Mark>) -> (bool, Duration) {
        let scale = if canvas.scale > 0.0 {
            canvas.scale
        } else {
            1.0
        };

        // A drawable that is not ready is a frame to skip, not work to do: the pen's pump is on
        // the main thread, and building a frame nobody can see would hold the next reading.
        let waited = Instant::now();
        let drawable = self.layer.nextDrawable();
        let waited = waited.elapsed();

        let Some(drawable) = drawable else {
            return (false, waited);
        };

        let Some(msaa) = self.multisample() else {
            return (false, waited);
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

        // Only a rebuild makes the uploaded buffers stale, and `refresh_ink` empties them itself when
        // it makes one. A pan or a zoom that the ink already covers rebuilds nothing, so there is nothing
        // to upload either: the list is byte for byte what the buffers hold, and copying a page's worth of
        // it again into each of three buffers in flight is the one cost a still page has.
        //
        // A buffer cannot simply be handed to all three slots: a frame in flight may still be reading the
        // one it was given, and writing it for the next frame would change what the GPU sees mid-draw.
        // The ring exists for that reason and each slot is refilled on its own turn.
        //
        // A slot holds the current ink exactly when its figure is the list's length. There is no second
        // flag to fall out of step with: an append leaves a slot's figure short, so every slot is refilled
        // with the corners it is missing and none is drawn from a buffer it only partly has.
        self.refresh_ink(canvas);
        if self.ink_starts_at(slot).is_some() {
            self.upload_ink(slot);
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

        // The stencil the ink's two passes share. Cleared once a frame, here: what it holds during the ink is
        // per stroke, and it is cleared again between strokes because two strokes are two layers.
        let stencil = self.stencil();
        if let Some(texture) = &stencil {
            let target = pass.stencilAttachment();
            target.setTexture(Some(texture));
            target.setLoadAction(MTLLoadAction::Clear);
            target.setStoreAction(MTLStoreAction::DontCare);
            target.setClearStencil(STENCIL_EMPTY);
        }

        let Some(buffer) = self.queue.commandBuffer() else {
            return (false, waited);
        };

        let Some(encoder) = buffer.renderCommandEncoderWithDescriptor(&pass) else {
            return (false, waited);
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
            ink_pass(
                &encoder,
                &self.solid,
                &self.stencil,
                &self.counting,
                &self.counted,
                buffer,
                self.ink.len(),
                &self.translucent_ink,
                width,
                height,
                sheet,
                stencil.as_deref(),
            );
        }

        if let Some(buffer) = &self.open_buffers[slot] {
            ink_pass(
                &encoder,
                &self.solid,
                &self.stencil,
                &self.counting,
                &self.counted,
                buffer,
                self.open.len(),
                &self.translucent_open,
                width,
                height,
                sheet,
                stencil.as_deref(),
            );
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

        // Kept for `settle`: the app never reads it, the benchmark does.
        self.last_commit = Some(buffer);

        self.buffer_slot = (self.buffer_slot + 1) % self.ink_buffers.len();

        (true, waited)
    }

    /// Waits for every command buffer this surface has committed to finish on the GPU.
    ///
    /// `draw` commits and returns — a frame the compositor has not yet shown is a frame whose cost has
    /// not been paid, which is right for an app drawing to a display and wrong for a benchmark that is
    /// trying to put a number on that cost. This is the benchmark's door into the same surface: it adds
    /// no work and changes no state, it only waits for what `draw` already queued.
    ///
    /// The wait is on the last committed buffer. Metal runs one queue in order, so the last to be
    /// committed is the last to finish, and waiting on it waits on everything before it too.
    pub fn settle(&self) {
        // `waitUntilCompleted` is the command buffer's own method, and it is called on a buffer that was
        // committed by `draw` — never on one still being encoded.
        if let Some(last) = &self.last_commit {
            last.waitUntilCompleted()
        }
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
        bind(encoder, buffer, width, height, transform)
    }

    /// One run of coloured vertices, in one draw call.
    ///
    /// The stencil is released here rather than left to the caller: this is the draw everything that is not
    /// the ink goes through — the desk, the page, the pen — and the ink leaves a stencil state behind it (see
    /// [`ink_pass`]). A cursor drawn under a `Equal`-against-the-mark test would be drawn nowhere that the
    /// ink had not already been, which is the one place a cursor is not needed and every place it is.
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
        encoder.setDepthStencilState(None);
        self.bind(encoder, buffer, width, height, transform);

        // SAFETY: `count` vertices, all of them in the buffer just bound.
        unsafe {
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, count);
        }
    }
}

/// The ink: opaque runs as they always were, and every translucent stroke counted so it is one layer.
///
/// A free function rather than a method because the test that guards it reads pixels back through it, and a
/// [`Surface`] needs a view and a window to exist at all (see [`a_highlighter_band_comes_out_at_its_alpha`]).
///
/// ## The problem it solves
///
/// A stroke is filled from *pieces* — a disc at every reading and a trapezoid between neighbours — and they
/// overlap by construction (see `note_format::ink::pieces_of`). That costs nothing for a pen: the same colour
/// over itself is the same colour. A highlighter is not: 35% over itself is 58%, and a point under ten of
/// them is `1 − 0.65¹⁰` — 99%, opaque, hiding the writing it was meant to mark.
///
/// ## What is drawn
///
/// The vertices are one list, and `translucent` says where each translucent stroke begins in it. Walking the
/// list, a run no stroke claims is opaque ink and goes out in one draw; a run a stroke claims is drawn twice
/// — once to count the samples its pieces reach, once to colour them. Where the pieces overlap the count
/// advances once, so the colour pass sees one layer, which is the whole of the fix.
///
/// ## Why the stencil needs no clearing between strokes
///
/// Because two strokes are two things the reader drew. A band that crosses itself, or doubles back over its
/// own path, is one stroke and one layer of 35%; a band laid over another is a second layer, and composes to
/// `1 − 0.65²` where they overlap — what a highlighter used twice leaves, and what `HIGHLIGHTER-BUG.md` asks
/// for. A stencil cannot be cleared inside a pass, and a pass per stroke would cost a full-screen clear for
/// every band on the page.
///
/// It does not have to be cleared. A stroke's colour pass writes a sample's mark *away* as it colours it
/// ([`MTLStencilOperation::Zero`]), so when the stroke is done the stencil is exactly as it found it, and the
/// next stroke's mark pass starts from nothing. The stencil never needs to know how many strokes there are,
/// so the frame clears it once and no stroke pays for a pass of its own.
///
/// `stencil_texture` is what the frame's pass was given. Without one — a device that refused it — the ink is
/// drawn straight through as before: bands that read as opaque rather than no ink at all.
#[allow(clippy::too_many_arguments)]
fn ink_pass(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    solid: &ProtocolObject<dyn MTLRenderPipelineState>,
    stencil: &ProtocolObject<dyn MTLRenderPipelineState>,
    counting: &ProtocolObject<dyn MTLDepthStencilState>,
    counted: &ProtocolObject<dyn MTLDepthStencilState>,
    buffer: &ProtocolObject<dyn MTLBuffer>,
    count: usize,
    translucent: &[usize],
    width: f32,
    height: f32,
    transform: Transform,
    stencil_texture: Option<&ProtocolObject<dyn MTLTexture>>,
) {
    if count == 0 {
        return;
    }

    if stencil_texture.is_none() || translucent.is_empty() {
        echo(encoder, solid, buffer, 0, count, width, height, transform);
        return;
    }

    // The runs: whatever precedes the first translucent stroke, then each stroke, then the rest. A run no
    // stroke claims is opaque ink, and goes out as it always did.
    let mut opaque_from = 0;
    for (index, &from) in translucent.iter().enumerate() {
        let to = translucent.get(index + 1).copied().unwrap_or(count);

        echo(
            encoder,
            solid,
            buffer,
            opaque_from,
            from,
            width,
            height,
            transform,
        );
        band(
            encoder, solid, stencil, counting, counted, buffer, from, to, width, height, transform,
        );
        opaque_from = to;
    }

    echo(
        encoder,
        solid,
        buffer,
        opaque_from,
        count,
        width,
        height,
        transform,
    );

    encoder.setDepthStencilState(None);
}

/// Vertices `from..to` in one draw, with no stencil: the same colour over itself is the same colour.
#[allow(clippy::too_many_arguments)]
fn echo(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    solid: &ProtocolObject<dyn MTLRenderPipelineState>,
    buffer: &ProtocolObject<dyn MTLBuffer>,
    from: usize,
    to: usize,
    width: f32,
    height: f32,
    transform: Transform,
) {
    if to <= from {
        return;
    }

    encoder.setRenderPipelineState(solid);
    // No stencil: this run is opaque, and the same colour over itself is the same colour. Saying so here also
    // keeps the state from leaking out of the ink into whatever is drawn next (see [`ink_pass`]).
    encoder.setDepthStencilState(None);
    bind(encoder, buffer, width, height, transform);

    // SAFETY: these vertices, in the buffer just bound.
    unsafe {
        encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, from, to - from);
    }
}

/// One translucent stroke: counted so its pieces read once, then coloured where it reaches.
///
/// `stroke` is which translucent stroke this is, from nought. After the count pass the stencil reads the
/// number of strokes that have reached each sample, so `stroke` is the value the colour pass compares
/// against — see [`ink_pass`] for why that is the test.
#[allow(clippy::too_many_arguments)]
fn band(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    solid: &ProtocolObject<dyn MTLRenderPipelineState>,
    stencil: &ProtocolObject<dyn MTLRenderPipelineState>,
    counting: &ProtocolObject<dyn MTLDepthStencilState>,
    counted: &ProtocolObject<dyn MTLDepthStencilState>,
    buffer: &ProtocolObject<dyn MTLBuffer>,
    from: usize,
    to: usize,
    width: f32,
    height: f32,
    transform: Transform,
) {
    if to <= from {
        return;
    }

    // The mark pass marks every sample this stroke's pieces reach, so the colour pass below skips exactly
    // what they cover. The mark is idempotent: the thirty-odd triangles over a point inside the band leave
    // what one of them would.
    // SAFETY: the encoder this frame is being drawn through.
    let _: () = unsafe { objc2::msg_send![encoder, setStencilReferenceValue: STENCIL_MARKED] };
    encoder.setRenderPipelineState(stencil);
    encoder.setDepthStencilState(Some(counting));
    bind(encoder, buffer, width, height, transform);

    // SAFETY: this stroke's vertices, in the buffer just bound.
    unsafe {
        encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, from, to - from);
    }

    // The colour pass colours exactly the samples the mark pass covered, one fragment at a time, and clears
    // the mark under each so the pieces after it are skipped. The reference stays the mark: the same value
    // the mark pass wrote, and the only value the colour pass will colour.
    encoder.setRenderPipelineState(solid);
    encoder.setDepthStencilState(Some(counted));

    // SAFETY: the same vertices, now through the stencil that decides which samples they colour.
    unsafe {
        encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, from, to - from);
    }
}

/// Binds `buffer` as the vertices, with the viewport and the transform the shader places them by.
fn bind(
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

    // SAFETY: the buffer holds the vertices the draw call reads, and the two values after it are bytes the
    // encoder copies before this function returns.
    unsafe {
        encoder.setVertexBuffer_offset_atIndex(Some(buffer), 0, 0);
        let _: () = objc2::msg_send![
            encoder,
            setVertexBytes: viewport.as_ptr() as *const std::ffi::c_void,
            length: std::mem::size_of_val(&viewport),
            atIndex: 1usize
        ];
        let _: () = objc2::msg_send![
            encoder,
            setVertexBytes: transform.as_ptr() as *const std::ffi::c_void,
            length: std::mem::size_of_val(&transform),
            atIndex: 2usize
        ];
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

    /// One build of the finished ink's triangles, printed when `CHEAP_NOTE_TRACE` asked for a trace.
    ///
    /// The explain line names the condition that forced the build: the answer to "why is this frame
    /// rebuilding" without a debugger, and the reason [`Demotion`] has names.
    pub fn trace(reason: super::Demotion) {
        if std::env::var_os("CHEAP_NOTE_TRACE_EXPLAIN").is_some() {
            eprintln!("ink: rebuilt ({reason:?})");
        }
    }

    /// One build of the finished ink's triangles, counted.
    ///
    /// Counted always, and not only under a trace: a rebuild is a fact about the renderer whether or not
    /// anyone is watching, and the status line's `rebuilds/s` is the count this figure is read from.
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

    /// The region a build covers in these tests: the whole plane, so no view is ever outside it.
    const COVERED: Bounds = Bounds::new(
        (f32::NEG_INFINITY, f32::NEG_INFINITY),
        (f32::INFINITY, f32::INFINITY),
    );

    /// A [`DeltaState`] that matches `canvas` exactly: the surface that has just built this page.
    ///
    /// Every decision a frame makes is a comparison against this, so a test about *one* thing changes one
    /// thing on the canvas and asks — which is what makes a failure name the field that moved.
    fn settled(canvas: &Canvas) -> DeltaState {
        DeltaState {
            revision: canvas.ink.edit,
            covered: canvas.ink.strokes.len(),
            detail: detail_of(&canvas.ink),
            rotation: canvas.ink.rotation.into(),
            paper: paper_of(canvas),
        }
    }

    /// A frame that reuses the ink must not upload it again.
    ///
    /// `refresh_ink` once returned nothing and the caller invalidated all three slots on every frame, so
    /// a pan — which rebuilds no triangles at all, because a scroll is a [`Transform`] — copied the whole
    /// page's worth of vertex data into each of the three buffers in flight, every frame. At 144 Hz on a
    /// dense page that is the largest cost a still page had. The rule is that the invalidation belongs to
    /// the build, the build is announced by `refresh_ink` returning `true`, and a slot is drawn from only
    /// when its `filled` figure is the list's length.
    ///
    /// A real surface with a real page, because "nothing was copied" is a claim about a buffer: the page
    /// is drawn, every slot given it, and then the upload asked for each of the three.
    #[test]
    fn a_reused_ink_is_not_uploaded_again() {
        let Some(mut surface) = Surface::with_device() else {
            return;
        };
        let canvas = page_of([0x11_11_11, 0x22_22_22, 0x33_33_33]);
        surface.rebuild_ink(&canvas, Demotion::Cold);
        let arena = surface.ink.clone();
        assert!(!arena.is_empty(), "the page is not triangles");
        for slot in 0..surface.slots() {
            surface.upload_ink(slot);
        }
        surface.take_stages();
        let longest = surface.ink.len();

        // A pan asks for the ink as it is: the decision is a reuse, the slot already holds the list, and
        // the upload loop asks `ink_starts_at`, gets nothing back, and copies nothing.
        let mut panned = canvas.clone();
        panned.ink.visible = Bounds::new((-40.0, -30.0), (1200.0, 900.0));
        assert_eq!(
            decide(&panned, &settled(&canvas), COVERED, &Paint::nothing()),
            InkPlan::Reuse,
            "a pan asked for the ink to be drawn again"
        );
        assert!(!surface.refresh_ink(&panned), "a pan rebuilt the ink");
        assert_eq!(
            surface.ink_starts_at(0),
            None,
            "a pan asked for a copy into a slot that holds the list"
        );
        for slot in 0..surface.slots() {
            surface.upload_ink(slot);
        }
        assert_eq!(
            (surface.filled, surface.uploaded_bytes),
            ([longest; 3], 0),
            "a pan copied ink into a slot that held it"
        );

        // A rebuild is what empties them: the frame that draws it copies the page.
        surface.rebuild_ink(&canvas, Demotion::NotAnAppend);
        assert_eq!(
            surface.filled, [0; 3],
            "a rebuild left a slot holding the list"
        );
        surface.upload_ink(0);
        assert_eq!(
            surface.uploaded_bytes,
            (longest * std::mem::size_of::<Vertex>()) as u64,
            "a rebuild copied something other than the page"
        );
        assert_eq!(
            surface.contents(0),
            drawn_from(&arena),
            "the rebuild drew another page"
        );
    }

    /// The corners a slot's copy is counted in. The account tests use small counts so that "which range
    /// was copied" is a figure a reader can check by eye — and a corner is one element of the list
    /// `Surface::upload_at` copies, twelve bytes each.
    const INK_120: usize = 120;
    const INK_100: usize = 100;

    /// A page a hand actually wrote, as the describing side sees it: strokes at the app's own resampling
    /// and nib, so a page is a few thousand vertices and a front part-way through it is part-way through
    /// the page.
    ///
    /// The one page the tests here start from. The `decide` tests only need a page that is a page — three
    /// strokes at three colours, removed and restored — and the slot tests need one big enough that "the
    /// front this slot was given" is a real distance into it, which the same fixture gives both without a
    /// second definition of what a page is. Every stroke is closed, so every one of them is triangles.
    pub(super) fn page_of(colours: [u32; 3]) -> Canvas {
        use note_format::ink::{InkPoint, Stroke};

        let mut canvas = Canvas::default();
        let mut strokes = Vec::new();

        // Twelve strokes, one colour in three, each a line across the page at the app's spacing.
        for index in 0..12u32 {
            let colour = colours[index as usize % colours.len()];
            let y = index as f32 * 20.0;
            let mut stroke = Stroke::new(
                InkPoint::new(0.0, y, 2.0),
                note_format::ink::with_alpha(colour, 0.65),
            );
            let mut x = 0.0f32;
            while x < 80.0 {
                x += note_format::settings::Settings::default().resample_spacing;
                stroke.points.push(InkPoint::new(x, y, 2.0));
            }
            stroke.close();
            strokes.push(Arc::new(stroke));
        }

        canvas.ink.strokes = Arc::new(strokes);
        canvas
    }

    /// A real surface on that page, built by the surface's own `rebuild_ink` — the fixture the slot tests
    /// start from, so what they upload is the geometry a frame would.
    ///
    /// `None` on a machine with no Metal device, which is a machine that cannot draw ink at all: the same
    /// skip the other device tests here take.
    fn surface_on_a_page() -> Option<(Surface, Canvas)> {
        let mut surface = Surface::with_device()?;
        let canvas = page_of([0x11_11_11, 0x22_22_22, 0x33_33_33]);
        surface.rebuild_ink(&canvas, Demotion::Cold);
        assert!(!surface.ink.is_empty(), "the page is not triangles");

        Some((surface, canvas))
    }

    /// The same page with one more stroke at its end, and the change that says so: an append.
    fn appended(page: &Canvas) -> Canvas {
        let mut canvas = page.clone();
        let mut strokes = (*canvas.ink.strokes).clone();
        let last = strokes.len() - 1;
        let mut fourth = (*canvas.ink.strokes[last]).clone();
        for point in &mut fourth.points {
            point.y += 20.0;
        }
        strokes.push(Arc::new(fourth));
        canvas.ink.strokes = Arc::new(strokes);

        let at = canvas.ink.strokes.len() - 1;
        let id = StrokeId::of(&canvas.ink.strokes[at]);
        canvas.ink.edit += 1;
        canvas.ink.delta = Some(note_page_delta(
            canvas.ink.edit,
            InkChange::Append { id, at },
        ));

        canvas
    }

    /// The page's change, stamped as the app stamps it: from the revision before to this one.
    fn note_page_delta(after: u64, change: InkChange) -> note_ui::ink::PageDelta {
        note_ui::ink::PageDelta::new(after - 1, vec![change])
    }

    /// The same page as the app describes one with something in hand: `mask` for strokes in hand and `tint`
    /// for the colour they are drawn in.
    ///
    /// The page is untouched — `edit`, `delta` and the strokes are the ones it came with — because that is
    /// exactly what a selection is: a thing drawn *over* the page that the page's revision has never heard
    /// of. A test that moved the revision as well would pass whether or not the renderer noticed the mask.
    fn strictly(page: &Canvas, mask: Arc<Vec<bool>>, tint: Option<gpui_kit::Hsla>) -> Canvas {
        let mut canvas = page.clone();
        canvas.ink.selected = mask;
        canvas.ink.selection = tint;

        canvas
    }

    /// A selection colour for the tests, distinct from any stroke's own.
    fn colour() -> gpui_kit::Hsla {
        gpui_kit::Hsla {
            h: 0.58,
            s: 0.9,
            l: 0.55,
            a: 0.35,
        }
    }

    /// What a buffer holding `vertices` holds byte for byte, which is what [`Surface::contents`] reads
    /// back: the comparison a slot's copy is checked by.
    fn drawn_from(vertices: &[Vertex]) -> Vec<u8> {
        vertices
            .iter()
            .flat_map(|vertex| {
                // SAFETY: `Vertex` is `repr(C)` with no padding (`vertex_tests` pins its size), so its
                // bytes are exactly what a copy into the buffer writes, and one `size_of` of them is
                // inside the one vertex the reference points at.
                unsafe {
                    std::slice::from_raw_parts(
                        (vertex as *const Vertex).cast::<u8>(),
                        std::mem::size_of::<Vertex>(),
                    )
                }
                .to_vec()
            })
            .collect()
    }

    /// The page a slot would draw from its buffer: the first `vertices` of what [`Surface::contents`]
    /// reads back.
    ///
    /// A slot's buffer is room for a page, not the page — an upload allocates room to grow into, so the
    /// bytes past the list are none of the draw's business and are not compared.
    fn drawn(held: &[u8], vertices: usize) -> &[u8] {
        &held[..vertices * std::mem::size_of::<Vertex>()]
    }

    /// Asserts that a buffer holds exactly what `vertices` would be copied into it, naming the first
    /// vertex that differs rather than a page of bytes.
    fn assert_holds(held: &[u8], vertices: &[Vertex], what: &str) {
        let wanted = drawn_from(vertices);
        let held = drawn(held, vertices.len());
        let stride = std::mem::size_of::<Vertex>();

        assert_eq!(
            held.len(),
            wanted.len(),
            "{what}: the buffer is {} vertices, the page {}",
            held.len() / stride,
            vertices.len()
        );

        for corner in 0..vertices.len() {
            let at = corner * stride;
            assert_eq!(
                &held[at..at + stride],
                &wanted[at..at + stride],
                "{what}: vertex {corner} is not the page's"
            );
        }
    }

    /// A frame after an append copies the vertices *its own slot* is missing, not the vertices that arrived.
    ///
    /// `append_ink` used to set every slot's figure to the list's length before the new stroke, which was
    /// true of the one slot that held the whole list and a lie about the other two. A slot that held `100`
    /// of `120` corners, then appended, was claimed to hold `120` of them: the next frame to use it copied
    /// `120..130` and drew the page with corners `100..120` never written — a stroke with a hundred and
    /// twenty vertices of whatever the buffer was made from in front of it.
    ///
    /// The three slots here start at `[n, ¾n, 0]` of a page `n` vertices long, which is what a rebuild
    /// followed by two frames leaves behind, and the append is the surface's own `refresh_ink`. What is
    /// then checked is what a frame would draw from each of the three: the slot's own `MTLBuffer`, read
    /// back after `upload_ink`, against the vertices the CPU built — the slot that was given three
    /// quarters of the page included, which is the one the old code left a hole in.
    #[test]
    fn an_append_copies_what_each_slot_is_missing() {
        let Some((mut surface, page)) = surface_on_a_page() else {
            return;
        };

        // The slots as a rebuild followed by two frames leaves them: the front the first slot was given,
        // three quarters of it in the second, and nothing at all in the third.
        let longest = surface.ink.len();
        let fronts = [longest, longest / 4 * 3, 0];
        assert!(longest / 4 * 3 > 0, "the page in this test is too small");
        surface.stage_fronts(fronts);

        // The append: a fourth stroke at the end of the page, and no slot's figure moves with it.
        let grown = appended(&page);
        assert!(
            surface.refresh_ink(&grown),
            "one stroke at the end of the page was not an append"
        );
        assert_eq!(
            surface.filled, fronts,
            "the append moved a slot's figure past what its buffer was given"
        );
        let longest = surface.ink.len();
        assert!(longest > fronts[1], "the append added no triangles");
        let arena = surface.ink.clone();

        // The three frames after it, one per slot, each into the buffer it already has.
        let mut copied = Vec::new();
        for slot in 0..surface.slots() {
            let before = surface.uploaded_bytes;
            surface.upload_ink(slot);
            copied.push(surface.uploaded_bytes - before);
        }

        assert_eq!(
            copied,
            [longest - fronts[0], longest - fronts[1], longest]
                .map(|vertices| vertices as u64 * std::mem::size_of::<Vertex>() as u64),
            "the copy was not what each slot was missing"
        );
        assert_eq!(
            surface.filled,
            [longest, longest, longest],
            "a slot's figure was not the list's length once its copy had landed"
        );

        // Each slot's own buffer, which is the page the GPU would draw from it — including the slot that
        // only ever held three quarters of it.
        for slot in 0..surface.slots() {
            assert_holds(
                &surface.contents(slot),
                &arena,
                &format!("slot {slot} would draw vertices its copy never wrote"),
            );
        }
    }

    /// A slot whose buffer had to be made is copied *from zero*, in the same frame.
    ///
    /// A frame's buffer was too small for the page, so it was made again. A fresh buffer holds nothing,
    /// whatever the slot's figure remembers; copying only the tail into it leaves the front the draw reads
    /// as the buffer was allocated, and the frame presents a page whose first stroke is noise. The copy is
    /// therefore from the list's first vertex, and the frame that made the buffer draws the whole list.
    ///
    /// The one scenario this pins, end to end on a real device: slot 0 is given 120 vertices of the page,
    /// the page grows past what that buffer can hold, and `upload_ink` is asked once. Before the next
    /// frame the slot's own `MTLBuffer` is read back and compared with the CPU list, vertex for vertex.
    ///
    /// Two assertions, and both are needed. The window is pinned directly — a copy that began at the
    /// slot's figure is the regression, and this is the line that names it — because a device hands back a
    /// *zeroed* buffer: a short copy would leave vertices at the origin, which the bytes of this page do
    /// not happen to be, but a page that did start near the origin would pass the byte check while the
    /// frame drew a stroke it never copied. The bytes are checked as well, because a zeroed buffer is one
    /// device's behaviour and not the contract the shader relies on.
    #[test]
    fn a_made_buffer_is_copied_from_zero_in_the_same_frame() {
        let Some((mut surface, page)) = surface_on_a_page() else {
            return;
        };

        // Slot 0 holds the first `INK_120` vertices in a buffer sized for `arena.len() + 1` vertices — a
        // frame's allocation for a page just one stroke longer. The page then grows past that.
        let arena = surface.ink.clone();
        assert!(
            arena.len() > INK_120,
            "the page is shorter than the front this test gives a slot"
        );
        let room = arena.len() + 1;
        surface.give_front(0, INK_120, room);

        // The page grows by more than the room the buffer has: a rebuild, so every figure is empty and the
        // copy is the whole page rather than a tail.
        let mut bigger = page.clone();
        let mut strokes = (*page.ink.strokes).clone();
        for step in 0..3 {
            let mut more = (*page.ink.strokes[0]).clone();
            for point in &mut more.points {
                point.y += 200.0 + step as f32 * 20.0;
            }
            strokes.push(Arc::new(more));
        }
        bigger.ink.strokes = Arc::new(strokes);
        bigger.ink.edit += 1;
        surface.rebuild_ink(&bigger, Demotion::NotAnAppend);
        assert_eq!(
            surface.filled, [0; 3],
            "a rebuild left a slot holding the page"
        );

        // Slot 0 keeps its buffer, which is now too small: this frame makes one and copies the list into
        // it, in this frame.
        surface.upload_ink(0);

        let longest = surface.ink.len();
        assert!(
            longest > room,
            "the page did not grow past the slot's {room}-vertex buffer"
        );
        assert_eq!(
            (surface.filled[0], surface.filled[1], surface.filled[2]),
            (longest, 0, 0),
            "the frame's own slot did not record the copy its buffer was given, or another slot's did"
        );
        assert_eq!(
            surface.uploaded_bytes,
            (longest * std::mem::size_of::<Vertex>()) as u64,
            "a made buffer was given only the tail, leaving the front uninitialised"
        );

        // The window the copy was, decided by the production function the upload itself calls: a made
        // buffer is written from its first vertex, not from where the slot's figure had got to.
        assert_eq!(
            ink_write(INK_120, longest, false),
            Some(0..longest),
            "a made buffer was given only the tail, leaving the front uninitialised"
        );

        // The buffer itself, vertex for vertex.
        assert_holds(
            &surface.contents(0),
            &arena,
            "a made buffer did not carry the page's vertices",
        );
    }

    /// A slot that was given the list copies nothing more until the ink changes again.
    ///
    /// The other half of the append rule: once each of the three slots has caught up, a frame of the
    /// unchanged page — a pan, a still frame — finds `filled[slot] == ink.len()` and copies no bytes at
    /// all. Without it an append that moved every slot forward would make the next frame of the page copy
    /// the page again, which is the cost the explicit-rebuild design exists to remove.
    #[test]
    fn a_slot_that_was_given_the_list_reuses_it_without_a_copy() {
        let Some((mut surface, page)) = surface_on_a_page() else {
            return;
        };

        // The slots as a rebuild followed by three frames leaves them, one shorter than the next.
        let longest = surface.ink.len();
        let fronts = [INK_120.min(longest), INK_100.min(longest), 0];
        assert_eq!(
            fronts,
            [INK_120, INK_100, 0],
            "the page is shorter than the fronts"
        );
        surface.stage_fronts(fronts);

        // Each slot catches up, once, with what it was missing.
        let grown = appended(&page);
        assert!(surface.refresh_ink(&grown), "the append rebuilt the page");
        let longest = surface.ink.len();
        let caught_up: Vec<u64> = (0..surface.slots())
            .map(|slot| {
                let before = surface.uploaded_bytes;
                surface.upload_ink(slot);
                (surface.uploaded_bytes - before) / std::mem::size_of::<Vertex>() as u64
            })
            .collect();
        assert_eq!(
            caught_up,
            vec![
                (longest - fronts[0]) as u64,
                (longest - fronts[1]) as u64,
                longest as u64
            ]
        );

        // Now the page is unchanged: a frame of a pan copies nothing, into any of the three.
        surface.take_stages();
        for slot in 0..surface.slots() {
            surface.upload_ink(slot);
        }
        assert_eq!(
            surface.uploaded_bytes, 0,
            "a frame of the unchanged page copied ink it already had"
        );
    }

    /// A held selection is baked once, and letting it go is baked once more — never once per frame.
    ///
    /// `decide` used to answer `Rebuild(Paint)` for any frame whose selection was `Some`, so a page with a
    /// stroke in hand was a page rebuilt on every frame the view drew, whatever the reason for the frame. The
    /// other half was the silence: `deselect` cleared the mask and moved no revision, and the selection's
    /// colour was already baked into the vertices the frame had drawn — a frame after it that found everything
    /// else unchanged read a reuse, and the tint of a selection that was no longer there stayed on screen.
    ///
    /// So the whole contract in one sequence, on a real surface over a real page: select one stroke, draw the
    /// same frame several times, let it go, draw the same frame again. Exactly two `Paint` rebuilds — the one
    /// that tints and the one that takes the tint off — and the last frame's vertices are byte for byte the
    /// page the first frame drew, which is what says the tint is gone rather than merely not added again.
    ///
    /// The frames *after* the deselect are the half a test of one frame cannot see: a surface that never
    /// reuses a page the app still names a selection colour on is a surface that rebuilt the page on every
    /// frame of a page nobody had selected anything on, and the figure it would present is right either way.
    #[test]
    fn a_held_selection_is_rebuilt_once_and_deselecting_rebuilds_once() {
        let Some((mut surface, page)) = surface_on_a_page() else {
            return;
        };

        let plain = surface.ink.clone();

        // The builds counted from here on: the fixture above is one this sequence did not make, and what is
        // asserted is what the selection costs, not what the page did before it had one.
        let start = surface.builds();
        let builds = |surface: &Surface| surface.builds() - start;

        // The lasso took the second stroke: one flag per stroke, in the page's order.
        let mut flag = vec![false; page.ink.strokes.len()];
        flag[1] = true;
        let selected = strictly(&page, Arc::new(flag), Some(colour()));
        assert!(
            surface.refresh_ink(&selected),
            "a selection that was not below drew the page it was already drawn"
        );
        assert_eq!(
            builds(&surface),
            1,
            "selecting a stroke was not exactly one build"
        );

        let tinted = surface.ink.clone();
        assert_ne!(
            drawn_from(&tinted),
            drawn_from(&plain),
            "the selected stroke was not drawn again in the selection's colour"
        );

        // The same frame, five times: nothing to do, five times.
        for frame in 0..5 {
            assert!(
                !surface.refresh_ink(&selected),
                "frame {frame} of a selection standing still rebuilt the page"
            );
        }

        // Escape: the mask cleared, the strokes untouched, and the colour the app still names — which is the
        // case that has to be read as *no* selection rather than as a tint whose mask happens to be empty.
        let mut cleared = selected.clone();
        cleared.ink.selected = Arc::new(vec![false; page.ink.strokes.len()]);
        assert!(
            surface.refresh_ink(&cleared),
            "letting the selection go reused vertices still tinted for it"
        );
        assert_eq!(
            builds(&surface),
            2,
            "letting the selection go was not exactly one build"
        );
        assert_eq!(
            drawn_from(&surface.ink),
            drawn_from(&plain),
            "the deselect left something of the tinted stroke behind"
        );

        // And once it is gone it stays gone: the page below is a page with no selection on it, so the frames
        // after it have nothing to do — which is what a deselected page costs for the rest of its life.
        for frame in 0..3 {
            assert!(
                !surface.refresh_ink(&cleared),
                "frame {frame} after a deselect rebuilt the page again"
            );
        }
        assert_eq!(
            builds(&surface),
            2,
            "a page with nothing in hand kept rebuilding after the one that took the tint off"
        );
    }

    /// An append is decided from numbers, not from a walk of the page, and every rebuild has a reason.
    ///
    /// The design document's §5 wants each rebuild to say why it was allowed. This pins the decisions
    /// themselves: a change that follows the last one and adds exactly one stroke at the end is an
    /// append; the same page one revision out of step is a `Gap` — the case that must never be quietly
    /// treated as an append, because the stroke that went by unread could have been an erase.
    #[test]
    fn an_append_is_decided_by_the_revision_and_the_change() {
        use note_format::ink::InkPoint;
        use note_ui::ink::{PageDelta, Stroke};
        use std::sync::Arc;

        let state = DeltaState {
            revision: 7,
            covered: 3,
            detail: note_format::ink::detail_zoom(1.0).to_bits(),
            rotation: 0,
            paper: [0, 0],
        };

        let mut canvas = Canvas::default();
        canvas.ink.visible = Bounds::new((10.0, 10.0), (400.0, 300.0));
        canvas.ink.edit = 8;
        // Both the page and the surface are at the same rung and the same way up, which is what an
        // append is decided on top of.
        canvas.ink.detail = Some(note_format::ink::detail_zoom(1.0).to_bits());
        canvas.ink.strokes = Arc::new(
            (0..4)
                .map(|_| Arc::new(Stroke::new(InkPoint::new(0.0, 0.0, 1.0), 0)))
                .collect(),
        );
        let id = StrokeId::of(&canvas.ink.strokes[3]);
        let covered = Bounds::new((0.0, 0.0), (800.0, 600.0));

        // `at` is the page's length *before* the change: the fourth stroke landed where three ended.
        canvas.ink.delta = Some(PageDelta::new(7, vec![InkChange::Append { id, at: 3 }]));
        assert_eq!(
            decide(&canvas, &state, covered, &Paint::nothing()),
            InkPlan::Append,
            "one stroke added at the end of the page is an append"
        );

        // The same new stroke, announced a place past where it landed. The count alone would let this
        // through; the append path fills `list[covered]`, which here is not the stroke the change named.
        canvas.ink.delta = Some(PageDelta::new(7, vec![InkChange::Append { id, at: 4 }]));
        assert_eq!(
            decide(&canvas, &state, covered, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::NotAnAppend),
            "an append whose `at` is not where the page ended is not the append path"
        );

        // An append that names some other stroke is the same: the page grew, but not by the stroke the
        // change says it did.
        canvas.ink.delta = Some(PageDelta::new(
            7,
            vec![InkChange::Append {
                id: StrokeId::of(&canvas.ink.strokes[0]),
                at: 3,
            }],
        ));
        assert_eq!(
            decide(&canvas, &state, covered, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::NotAnAppend),
            "an append must name the stroke that is actually last"
        );

        // The same page, but the change that arrived is not the one after the last: something went by
        // unread, and the surface cannot know whether it was this append or an erase.
        canvas.ink.delta = Some(PageDelta::new(5, vec![InkChange::Append { id, at: 3 }]));
        assert_eq!(
            decide(&canvas, &state, covered, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::Gap),
            "a revision that does not follow the last one is never an append"
        );

        // A page that moved with no change to say what it did cannot be appended to either.
        canvas.ink.delta = None;
        assert_eq!(
            decide(&canvas, &state, covered, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::Gap),
            "a page that changed without a delta is a gap, not an append"
        );

        // A transaction that adds more than one stroke is not the append path.
        canvas.ink.delta = Some(PageDelta::new(
            7,
            vec![
                InkChange::Append { id, at: 0 },
                InkChange::Append { id, at: 3 },
            ],
        ));
        assert_eq!(
            decide(&canvas, &state, covered, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::NotAnAppend),
            "a transaction that adds more than one stroke is not the append path"
        );

        // A selection that was not the one below changes what is baked, whatever the edit was. The frame
        // before it baked no selection, so the ink in hand has to be drawn a second time.
        canvas.ink.delta = Some(PageDelta::new(7, vec![InkChange::Append { id, at: 3 }]));
        canvas.ink.selected = Arc::new(vec![false, true, false, false]);
        canvas.ink.selection = Some(gpui_kit::Hsla::default());
        assert_eq!(
            decide(&canvas, &state, covered, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::Paint),
            "a selection re-bakes the ink in hand"
        );

        // The same selection on the next frame is what is already below: a selection standing still is one
        // compare of the mask's `Arc`, and drawing the page again would be the cost this exists to remove.
        // The page is the one that bake left: its change applied, its revision where the state says.
        canvas.ink.delta = None;
        canvas.ink.edit = state.revision;
        canvas.ink.strokes = Arc::new(canvas.ink.strokes[..state.covered].to_vec());
        let painted = Paint::of(&canvas.ink);
        assert_eq!(
            decide(&canvas, &state, covered, &painted),
            InkPlan::Reuse,
            "a selection that did not move asked for the ink to be drawn again"
        );
    }

    /// A scroll moves the paper and leaves the strokes exactly as they were, so the ink is *not* drawn
    /// again: the triangles are in the paper's own units and the scroll is a [`Transform`]. That difference
    /// is the whole of why scrolling stutters or does not — the triangles used to be in device pixels, and
    /// every reading of a trackpad rebuilt them.
    #[test]
    fn a_scroll_is_a_transform_and_not_a_build() {
        let mut canvas = Canvas::default();
        let state = settled(&canvas);
        let placed = Transform::of(&canvas);

        canvas.ink.origin = (0.0, -120.0);

        assert_eq!(
            decide(&canvas, &state, COVERED, &Paint::nothing()),
            InkPlan::Reuse,
            "a scroll asked for the ink to be drawn again"
        );
        assert!(
            placed.shift != Transform::of(&canvas).shift,
            "a scroll left the sheet where it was"
        );
    }

    /// A zoom is the same story, and so is the screen's own scale: how large the paper is drawn is the
    /// transform. The *rung* a zoom asks for is another matter — a new one rebuilds every outline — so the
    /// two are told apart by the detail the page reports, not by its size.
    #[test]
    fn a_zoom_is_a_transform_unless_it_crosses_a_rung() {
        let mut canvas = Canvas::default();
        let state = settled(&canvas);
        let placed = Transform::of(&canvas);

        canvas.ink.zoom = 1.5;

        assert_eq!(
            decide(&canvas, &state, COVERED, &Paint::nothing()),
            InkPlan::Reuse,
            "a zoom asked for the ink to be drawn again"
        );
        assert!(
            placed.scale != Transform::of(&canvas).scale,
            "a zoom left the sheet its own size"
        );

        // The pump finished a rebuild for a larger rung: every outline is new, so every triangle is.
        canvas.ink.detail = Some(note_format::ink::detail_zoom(2.0).to_bits());
        assert_eq!(
            decide(&canvas, &state, COVERED, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::Rung),
            "a new detail left the ink alone"
        );
    }

    /// A turn of the page *is* a change to the ink: the paper's own units are turned with it, and every
    /// triangle moves.
    #[test]
    fn a_turn_of_the_page_is_a_build() {
        let mut canvas = Canvas::default();
        let state = settled(&canvas);

        canvas.ink.rotation = note_format::pages::Quarters::from_turns(1);
        assert_eq!(
            decide(&canvas, &state, COVERED, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::Rotation),
            "a turn left the ink alone"
        );
    }

    /// The other half: a frame that changed nothing needs nothing, so the ink is not rebuilt for being
    /// asked twice — the page's triangles are tens of thousands of vertices. A reuse, not an append: the
    /// page's length did not move, so there is no stroke to add.
    #[test]
    fn the_same_page_needs_nothing() {
        let canvas = Canvas::default();

        assert_eq!(
            decide(&canvas, &settled(&canvas), COVERED, &Paint::nothing()),
            InkPlan::Reuse
        );
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
        let screen = Bounds::new((100.0, 200.0), (1100.0, 1000.0));
        let covered = cull_of(screen);

        assert!(
            fits(screen, covered),
            "the screen it was built for always fits"
        );

        // A scroll of half the margin, in every direction at once.
        let scrolled = screen.grown(100.0, 100.0);
        assert!(
            fits(scrolled, covered),
            "a scroll inside the margin is free"
        );

        // Out past the margin: the ink does not cover it.
        let away = screen.grown(1000.0, 0.0);
        assert!(!fits(away, covered), "a scroll out of the margin builds");

        // A zoom in, around the middle: a smaller rectangle inside the same place.
        let closer = Bounds::new((400.0, 400.0), (800.0, 800.0));
        assert!(fits(closer, covered), "zooming in fits inside the build");

        // A zoom out, around the middle: smaller numbers on the paper, so a *larger* rectangle.
        let further = Bounds::new((-600.0, -400.0), (1800.0, 1600.0));
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
            Bounds::new((0.0, 0.0), (640.0, 906.0)),
            Bounds::new((-2000.0, 500.0), (-1000.0, 700.0)),
            Bounds::new((0.0, 0.0), (0.0, 0.0)),
        ] {
            let covered = cull_of(visible);

            assert!(
                fits(visible, covered),
                "{visible:?} is not inside the region built for it, {covered:?}"
            );
            // And the region is larger, never smaller, unless the box had no extent at all.
            if !visible.is_empty() && visible.size() != (0.0, 0.0) {
                assert!(
                    covered.min.0 < visible.min.0 && covered.max.0 > visible.max.0,
                    "{covered:?} did not grow past {visible:?} sideways"
                );
                assert!(
                    covered.min.1 < visible.min.1 && covered.max.1 > visible.max.1,
                    "{covered:?} did not grow past {visible:?} downwards"
                );
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
        let near = Bounds::new((0.0, 0.0), (720.0, 1018.0));
        let far = Bounds::new((0.0, 0.0), (180.0, 254.5));

        for visible in [near, far] {
            let covered = cull_of(visible);
            let margin = visible.min.0 - covered.min.0;
            let (width, _) = visible.size();

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

    /// The pieces fill is a pure copy for an unturned page, so the identity test must be exact.
    ///
    /// `pieces` takes the fast path only when `is_identity` says every corner is already where it goes.
    /// A page whose paper is 720×1 018 and whose origin is 40, 40 is the common one, and it must qualify:
    /// the mapping is built from the *paper's shape* and the page's turn, not its size or its place, so
    /// nothing about a page at rest can stop it. A quarter turn must not qualify — `drawn_of_paper` swaps
    /// the axes, and a corner really does move.
    #[test]
    fn an_unturned_page_maps_to_nothing_and_a_turned_one_does_not() {
        let mut canvas = Canvas {
            scale: 2.0,
            ..Canvas::default()
        };
        canvas.ink.paper = (720.0, 1_018.0);
        canvas.ink.origin = (40.0, 40.0);

        assert!(Mapping::of(&canvas).is_identity());

        for turns in [1, 2, 3] {
            canvas.ink.rotation = note_format::pages::Quarters::from_turns(turns);

            assert!(
                !Mapping::of(&canvas).is_identity(),
                "a page turned {turns} is not the identity"
            );
        }
    }

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

    /// [`fill`] with the scratch a single call needs made for it.
    ///
    /// The app keeps one polygon and one sweep on the surface and reuses them across a page's strokes; a
    /// test fills one stroke, so it makes its own and the call reads the way it did before that.
    fn fill_into(vertices: &mut Vec<Vertex>, stroke: &note_format::ink::Stroke, mapping: &Mapping) {
        let mut scratch = Vec::new();

        fill(
            vertices,
            &mut scratch,
            &mut Sweep::default(),
            stroke,
            mapping,
            (0.0, 0.0),
            colour(),
        );
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
    /// The pieces ink the band, at the zoom a reader draws at and at the ones they magnify to.
    ///
    /// This is the test that found a stroke with a wedge of paper in it: the region the pieces filled and
    /// the region the boundary described disagreed by a few square units wherever a curve turned tightly
    /// between readings close together. Magnification is in the ladder because that wedge is a fraction of
    /// a *disc*, so at 100% it is a pixel and at 800% it is a visible notch — a test at one zoom would
    /// have called the drawing correct.
    ///
    /// The question asked here is coverage, not area. The pieces overlap each other by construction (each
    /// reading's disc is filled whole, and it reaches into its neighbours' trapezoids), and the renderer
    /// marks them into a stencil and colours each sample once, so a place covered twice costs nothing.
    /// What must not happen is a place in the band that no piece reaches.
    #[test]
    fn the_pieces_ink_the_band_at_every_zoom() {
        let mut failures = Vec::new();

        for zoom in [1.0f32, 2.0, 4.0, 8.0] {
            let mapping = Mapping {
                base: (0.0, 0.0),
                step_x: (zoom, 0.0),
                step_y: (0.0, zoom),
            };

            for (what, points) in [
                ("a curve", arc(40.0, 0.6)),
                ("a zigzag", zigzag(6, 40.0, 35.0)),
                ("a there and back", there_and_back(40.0)),
            ] {
                let stroke = stroke_of(&points, 18.0);
                let mut vertices = Vec::new();
                fill_into(&mut vertices, &stroke, &mapping);

                // Every place the pen actually was must be ink: the nib's own centre cannot have been
                // left out, whatever the pieces did at their edges. In device pixels, which is where the
                // vertices are.
                let mut misses = 0;

                for point in &points {
                    let at = mapping.point([point.0, point.1], (0.0, 0.0));

                    if !vertices
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .any(|t| inside_triangle(at, t))
                    {
                        misses += 1;
                    }
                }

                if misses > 0 {
                    failures.push(format!(
                        "{what} at {zoom}×: {misses} of {} readings are not inked",
                        points.len()
                    ));
                }
            }
        }

        assert!(failures.is_empty(), "{}", failures.join("\n  "));
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
        fill_into(&mut vertices, &stroke, &mapping);

        // What the band covers: the rectangle the nib sweeps, plus a round cap at each end — the caps are
        // what a nib's round end leaves, and they are half a disc each. The pieces overlap each other by
        // construction, so the *area sum* is not the overlap a reader sees: the renderer marks the pieces
        // into a stencil and colours each sample once (`Surface::ink_pass`), and what that leaves is the
        // region, coloured once. This asks the two things that follow from the region: every place in it
        // is reached, and no place outside it is.
        let travel = stroke.points.last().expect("a stroke").x;
        let radius = width * 0.5;
        let _ = travel * width + std::f32::consts::PI * radius * radius;

        let mut misses = 0;
        let mut sampled = 0;

        for step in 0..=300 {
            let x = travel * step as f32 / 300.0;

            for across in [-0.8f32, -0.4, 0.0, 0.4, 0.8] {
                sampled += 1;

                if !vertices
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .any(|t| inside_triangle((x, across * radius), t))
                {
                    misses += 1;
                }
            }
        }

        assert_eq!(
            misses, 0,
            "the band is not covered: {misses} of {sampled} places missed"
        );

        // Outside the band, in the paper beside it: nothing may reach.
        let mut spilled = 0;

        for step in 0..=300 {
            let x = travel * step as f32 / 300.0;

            for across in [-1.4f32, 1.4] {
                if vertices
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .any(|t| inside_triangle((x, across * radius), t))
                {
                    spilled += 1;
                }
            }
        }

        assert_eq!(
            spilled, 0,
            "the band paints paper: {spilled} places outside it"
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
        fill_into(&mut vertices, &stroke, &mapping);

        // The band is `travel` long however many times the stroke covered it: the *region* is what is
        // inked, and a stroke that crosses itself is one region rather than a second layer of ink. The
        // pieces overlap each other by construction (a disc reaches into its neighbour's trapezoid), so a
        // sum of their areas is not the right question — the right question is whether the region is
        // covered, and whether a place in it is reached at all. Both are asked here.
        //
        // Every place along the band is reached by some piece...
        let radius = width * 0.5;
        let mut misses = 0;
        let samples = 400;

        for step in 0..=samples {
            let x = travel * step as f32 / samples as f32;

            for across in [-0.75f32, -0.35, 0.0, 0.35, 0.75] {
                if !vertices
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .any(|t| inside_triangle((x, across * radius), t))
                {
                    misses += 1;
                }
            }
        }

        assert_eq!(
            misses, 0,
            "the doubled-back band is not covered: {misses} places missed"
        );

        // ...and the places outside it are not reached, so the fan is not filling paper.
        let mut spilled = 0;

        for step in 0..=samples {
            let x = travel * step as f32 / samples as f32;

            for across in [-1.6f32, 1.6] {
                let at = (x, across * radius);

                if vertices
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .any(|t| inside_triangle(at, t))
                {
                    spilled += 1;
                }
            }
        }

        assert_eq!(
            spilled, 0,
            "the band is painted outside itself: {spilled} places"
        );

        let _ = area(&vertices);
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
        fill_into(&mut vertices, &stroke, &mapping);

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
        fill_into(&mut vertices, &stroke, &mapping);

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
                fill_into(&mut vertices, &stroke, &mapping);

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
        fill_into(&mut vertices, stroke, &flat());
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
        let (width, height) = bounds.size();
        let across = width / columns as f32;
        let down = height / rows as f32;

        let mut wrong = Vec::new();

        for row in 0..rows {
            for column in 0..columns {
                let at = (
                    bounds.min.0 + column as f32 * across,
                    bounds.min.1 + row as f32 * down,
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
                                Vertex::rgb(gpui_kit::Hsla::default()),
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

    /// The pen's cursor is drawn after the ink, and the ink must not take it with it.
    ///
    /// The cursor is the last thing in a frame and shares the frame's encoder. The ink leaves a stencil state
    /// behind it, and a cursor drawn under the last stroke's test appears only where that stroke already is —
    /// nowhere a reader is about to write, which is every place the cursor is for. It went unnoticed because a
    /// cursor over blank paper has no ink to be hidden by and no test to fail; it disappears exactly when a
    /// stroke has been drawn, which is when the reader is looking.
    #[test]
    fn the_cursor_survives_the_stencil_the_ink_leaves() {
        let Some(device) = objc2_metal::MTLCreateSystemDefaultDevice() else {
            return;
        };
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .expect("the shaders compile");

        // A highlight near the middle, so the ink runs through the test's two sample points.
        let band = 18.0_f32;
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };
        let mut stroke = note_format::ink::Stroke::new(
            note_format::ink::InkPoint::new(0.0, 100.0, band),
            note_format::ink::with_alpha(0xFFEB3B, note_ui::ink::HIGHLIGHTER_ALPHA),
        );
        let mut x = 0.0_f32;
        while x < 400.0 {
            x += note_format::settings::Settings::default().resample_spacing;
            stroke
                .points
                .push(note_format::ink::InkPoint::new(x, 100.0, band));
        }
        stroke.close_at(1.0);

        let mut vertices = Vec::new();
        let mut scratch = Vec::new();
        fill(
            &mut vertices,
            &mut scratch,
            &mut Sweep::default(),
            &stroke,
            &mapping,
            (0.0, 0.0),
            ink_colour(stroke.color),
        );

        // What the frame draws last: a black square where the pen is. Drawn through `Surface::solid`'s own
        // sequence, minus the `Surface` — the pipeline, the released stencil, the draw.
        let pen = quad_vertices((50.0, 50.0), (60.0, 60.0), 0x000000);

        let (_, corner) = band_then(&device, &library, &vertices, &[0], (400, 200), |encoder| {
            // SAFETY: a buffer and a pipeline that outlive the closure, drawn the way a frame draws the pen.
            let buffer = unsafe {
                let pointer = std::ptr::NonNull::new(pen.as_ptr() as *mut std::ffi::c_void)
                    .expect("the pen has vertices");
                device
                    .newBufferWithBytes_length_options(
                        pointer,
                        std::mem::size_of_val(&pen[..]),
                        MTLResourceOptions::StorageModeShared,
                    )
                    .expect("a buffer for the pen")
            };
            let solid =
                pipeline(&device, &library, "solid_vertex", "solid_fragment").expect("solid");
            encoder.setRenderPipelineState(&solid);
            encoder.setDepthStencilState(None);
            // `buffer` is owned here, and `bind` borrows it for the length of the call only: the
            // explicit `&` is what says so (the compiler's own auto-ref would count as a clippy
            // `needless_borrow`, because it resolves the deref to the same `&ProtocolObject`).
            #[allow(clippy::needless_borrow)]
            bind(&encoder, &buffer, 400.0, 200.0, Transform::WINDOW);

            // SAFETY: the pen's own vertices, in the buffer just bound.
            unsafe {
                encoder.drawPrimitives_vertexStart_vertexCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    pen.len(),
                );
            }
        });

        // Black pen over white paper: if the state leaked, the pixel is untouched paper.
        assert!(
            corner < 40,
            "the cursor was not drawn: blue {corner} where it should be black (white paper is 255)"
        );
    }

    /// The two triangles of an axis-aligned rectangle, in `rgb` at full opacity.
    fn quad_vertices((x, y): (f32, f32), (right, bottom): (f32, f32), rgb: u32) -> Vec<Vertex> {
        let mut vertices = Vec::new();
        quad(
            &mut vertices,
            Rect {
                x,
                y,
                width: right - x,
                height: bottom - y,
            },
            // Opaque black: `ink_colour` is the app's own packing, so the bytes the vertex carries are the
            // ones a pen would carry.
            ink_colour(rgb),
        );

        vertices
    }

    /// Every pass the renderer needs builds on this device.
    ///
    /// A pipeline that fails to build is not a frame drawn wrongly — [`Surface::install`] returns `None` and
    /// the window never gets a surface at all, so the failure is a blank canvas rather than a bad colour. The
    /// stencil shaders are the ones with something to get wrong (a fragment that writes no colour, a sample
    /// count that has to match the colour target), so they are built here and asserted rather than assumed.
    #[test]
    fn every_pipeline_the_renderer_needs_builds() {
        let Some(device) = objc2_metal::MTLCreateSystemDefaultDevice() else {
            return;
        };

        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .expect("the shaders compile");

        pipeline(&device, &library, "solid_vertex", "solid_fragment").expect("the solid pass");
        pipeline(&device, &library, "page_vertex", "page_fragment").expect("the page pass");
        pipeline(&device, &library, "stencil_vertex", "stencil_fragment").expect("the mark pass");
        stenciling(&device).expect("a counting stencil");
        stencilled(&device).expect("a counted stencil");
    }

    /// A highlighter's band over paper comes out 35% of the way to its colour — measured, not counted.
    ///
    /// [`a_highlighter_band_is_painted_exactly_once`] asks whether the pieces *cover* the band, and a band
    /// laid down ten times over covers it perfectly. That is why the regression it was written for came
    /// back: the pieces of a highlighter overlap by construction, so a renderer that draws them one after
    /// another composites `1 − 0.65ⁿ`, and at the centre of a mark a reader made `n` is some tens. The
    /// pixels come out opaque and a coverage test is happy.
    ///
    /// This draws the band through the renderer's own passes and reads the pixel back, so the number that
    /// matters — how many times a point was coloured — is the number asserted.
    #[test]
    fn a_highlighter_band_comes_out_at_its_alpha() {
        let Some(device) = objc2_metal::MTLCreateSystemDefaultDevice() else {
            // No GPU to ask: a machine without one cannot disagree about this.
            return;
        };

        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .expect("the shaders compile");

        // A band across the middle of a small page, in the app's own colour and width for it.
        let band = 18.0_f32;
        let spacing = note_format::settings::Settings::default().resample_spacing;

        let mut stroke = note_format::ink::Stroke::new(
            note_format::ink::InkPoint::new(40.0, 100.0, band),
            note_format::ink::with_alpha(0xFFEB3B, note_ui::ink::HIGHLIGHTER_ALPHA),
        );
        let mut x = 40.0_f32;
        while x < 360.0 {
            x += spacing;
            stroke
                .points
                .push(note_format::ink::InkPoint::new(x, 100.0, band));
        }
        stroke.close_at(1.0);

        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };
        let mut vertices = Vec::new();
        let mut scratch = Vec::new();
        fill(
            &mut vertices,
            &mut scratch,
            &mut Sweep::default(),
            &stroke,
            &mapping,
            (0.0, 0.0),
            ink_colour(stroke.color),
        );

        // One translucent stroke, and nothing else: the range the renderer would record for it.
        let blue = band_over_white(&device, &library, &vertices, &[0], (400, 200));

        // The paper is white and the band is `0xFF, 0xEB, 0x3B` at 35%, so blue is the channel that moves
        // furthest: `255 - 0.35 * (255 - 59)` is about 186. Drawn once per piece it would be 59 — the
        // highlight's own blue, which is the smear this is here to keep from coming back.
        let once = 255.0 - note_ui::ink::HIGHLIGHTER_ALPHA * (255.0 - 59.0);

        assert!(
            (blue as f32 - once).abs() <= 4.0,
            "the band is not one layer of {}%: blue {blue} at the centre, where one layer is {once:.0} \
             and one layer per piece would be 59",
            note_ui::ink::HIGHLIGHTER_ALPHA * 100.0
        );
    }

    /// Two strokes over the same place are two layers; one stroke over itself is still one.
    ///
    /// Which is the line `HIGHLIGHTER-BUG.md` draws, and the one thing a stencil that simply marked "coloured"
    /// would get wrong: it would make the second band invisible, though the reader drew it twice and expects
    /// the darker mark that follows. The stencil is cleared of its mark as each stroke colours, so the second
    /// stroke starts from nothing and composes — `1 − 0.65²`, about 58% — while the pieces *within* either
    /// stroke still count once.
    #[test]
    fn two_strokes_over_the_same_point_are_two_layers() {
        let Some(device) = objc2_metal::MTLCreateSystemDefaultDevice() else {
            return;
        };
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .expect("the shaders compile");

        let band = 18.0_f32;
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };
        let colour = note_format::ink::with_alpha(0xFFEB3B, note_ui::ink::HIGHLIGHTER_ALPHA);

        // Two strokes drawn over the same line, each filled from its own pieces.
        let mut vertices = Vec::new();
        let mut scratch = Vec::new();
        let mut sweep = Sweep::default();
        let mut starts = Vec::new();

        for _ in 0..2 {
            let mut stroke = note_format::ink::Stroke::new(
                note_format::ink::InkPoint::new(40.0, 100.0, band),
                colour,
            );
            let mut x = 40.0_f32;
            while x < 360.0 {
                x += note_format::settings::Settings::default().resample_spacing;
                stroke
                    .points
                    .push(note_format::ink::InkPoint::new(x, 100.0, band));
            }
            stroke.close_at(1.0);

            starts.push(vertices.len());
            fill(
                &mut vertices,
                &mut scratch,
                &mut sweep,
                &stroke,
                &mapping,
                (0.0, 0.0),
                ink_colour(stroke.color),
            );
        }

        let blue = band_over_white(&device, &library, &vertices, &starts, (400, 200));

        // Twice 35% over white: `255 - (1 - 0.65²) * (255 - 59)`, about 139.
        let twice =
            255.0 - (1.0 - (1.0 - note_ui::ink::HIGHLIGHTER_ALPHA).powi(2)) * (255.0 - 59.0);

        assert!(
            (blue as f32 - twice).abs() <= 6.0,
            "two strokes are not two layers: blue {blue} at the centre, where two layers are {twice:.0} \
             (one layer is 186, and one layer per piece would be 59)"
        );
    }

    /// Draws `vertices` over white through the renderer's ink passes, and answers the centre pixel's blue.
    ///
    /// A multisampled target resolved into one the CPU can read, matching what the window draws into: the
    /// pipelines are built for `SAMPLES`, and a target of a different count is a pass Metal refuses.
    fn band_over_white(
        device: &ProtocolObject<dyn MTLDevice>,
        library: &ProtocolObject<dyn MTLLibrary>,
        vertices: &[Vertex],
        translucent: &[usize],
        (width, height): (usize, usize),
    ) -> u8 {
        band_then(
            device,
            library,
            vertices,
            translucent,
            (width, height),
            |_| {},
        )
        .0
    }

    /// [`band_over_white`], with `after` run through the same encoder once the ink is drawn.
    ///
    /// What `after` exists for is the state the ink leaves behind it. The pen's cursor is drawn last, through
    /// the same encoder, so a stencil state the ink forgot to release is a state the cursor inherits — and a
    /// cursor inheriting `Equal`-against-the-mark is drawn only where ink already is, which is to say not at
    /// all in the place a reader is about to write.
    fn band_then(
        device: &ProtocolObject<dyn MTLDevice>,
        library: &ProtocolObject<dyn MTLLibrary>,
        vertices: &[Vertex],
        translucent: &[usize],
        (width, height): (usize, usize),
        after: impl FnOnce(&ProtocolObject<dyn MTLRenderCommandEncoder>),
    ) -> (u8, u8) {
        use objc2_metal::MTLClearColor;

        let solid = pipeline(device, library, "solid_vertex", "solid_fragment").expect("solid");
        let stencil =
            pipeline(device, library, "stencil_vertex", "stencil_fragment").expect("stencil");
        let counting = stenciling(device).expect("a counting stencil");
        let counted = stencilled(device).expect("a counted stencil");

        // SAFETY: every byte these describe is in `vertices`, which outlives this call.
        let buffer = unsafe {
            let pointer = std::ptr::NonNull::new(vertices.as_ptr() as *mut std::ffi::c_void)
                .expect("a band is not empty");
            device.newBufferWithBytes_length_options(
                pointer,
                std::mem::size_of_val(vertices),
                MTLResourceOptions::StorageModeShared,
            )
        }
        .expect("a vertex buffer");

        let multisampled = |format| {
            // SAFETY: a 2D descriptor for the size this draws at.
            let descriptor = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    format, width, height, false,
                )
            };
            descriptor.setTextureType(MTLTextureType::Type2DMultisample);
            // SAFETY: the count the pipelines were built with.
            unsafe {
                descriptor.setSampleCount(SAMPLES);
            }
            descriptor.setUsage(MTLTextureUsage::RenderTarget);
            device
                .newTextureWithDescriptor(&descriptor)
                .expect("a target")
        };

        let target = multisampled(MTLPixelFormat::BGRA8Unorm);
        let mask = multisampled(STENCIL);

        // SAFETY: a 2D descriptor for the size this reads back.
        let resolve = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                width,
                height,
                false,
            )
        };
        resolve.setUsage(MTLTextureUsage::RenderTarget);
        let resolved = device
            .newTextureWithDescriptor(&resolve)
            .expect("a readable target");

        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: index 0 is the one attachment these pipelines were built for.
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&target));
        attachment.setResolveTexture(Some(&resolved));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::MultisampleResolve);
        attachment.setClearColor(MTLClearColor {
            red: 1.0,
            green: 1.0,
            blue: 1.0,
            alpha: 1.0,
        });

        let mask_attachment = pass.stencilAttachment();
        mask_attachment.setTexture(Some(&mask));
        mask_attachment.setLoadAction(MTLLoadAction::Clear);
        mask_attachment.setStoreAction(MTLStoreAction::DontCare);
        mask_attachment.setClearStencil(STENCIL_EMPTY);

        let queue = device.newCommandQueue().expect("a queue");
        let command = queue.commandBuffer().expect("a command buffer");
        let encoder = command
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("an encoder");

        ink_pass(
            &encoder,
            &solid,
            &stencil,
            &counting,
            &counted,
            &buffer,
            vertices.len(),
            translucent,
            width as f32,
            height as f32,
            Transform::WINDOW,
            Some(&mask),
        );

        // Anything the caller wants drawn after the ink, through the same encoder and whatever state the
        // ink left: this is where the pen's cursor goes in a frame.
        after(&encoder);

        encoder.endEncoding();
        command.commit();
        command.waitUntilCompleted();

        // Two pixels: the middle of the band, and one inside the pen's square (see the caller), which is a
        // place no ink reaches — the only place the cursor can be missed. The square the caller draws spans
        // 50..60 in both axes, so its interior is 55, 55 whatever the target's size.
        let read = |x: usize, y: usize| {
            let mut pixel = [0u8; 4];
            let region = MTLRegion {
                origin: MTLOrigin { x, y, z: 0 },
                size: MTLSize {
                    width: 1,
                    height: 1,
                    depth: 1,
                },
            };
            // SAFETY: `pixel` is four bytes and the region is the one pixel they hold.
            unsafe {
                let out = std::ptr::NonNull::new(pixel.as_mut_ptr() as *mut std::ffi::c_void)
                    .expect("a stack array is never null");
                resolved.getBytes_bytesPerRow_bytesPerImage_fromRegion_mipmapLevel_slice(
                    out, 4, 4, region, 0, 0,
                );
            }

            // A `BGRA8Unorm` pixel: blue is the first byte.
            pixel[0]
        };

        (read(width / 2, height / 2), read(55, 55))
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

    /// The page fixture is the one the slot tests use — see [`super::tests::page_of`] — so an edit and an
    /// upload are judged on the same page rather than on two that happen to look alike.
    use super::tests::page_of;

    /// The region a build covers in these tests: the whole plane, so no view is ever outside it.
    const COVERED: Bounds = Bounds::new(
        (f32::NEG_INFINITY, f32::NEG_INFINITY),
        (f32::INFINITY, f32::INFINITY),
    );

    /// A [`DeltaState`] that matches `canvas` exactly: the surface that has just built this page.
    ///
    /// Every decision a frame makes is a comparison against this, so a test about *one* thing changes one
    /// thing on the canvas and asks — which is what makes the failure name the field that moved.
    fn settled(canvas: &Canvas) -> DeltaState {
        DeltaState {
            revision: canvas.ink.edit,
            covered: canvas.ink.strokes.len(),
            detail: detail_of(&canvas.ink),
            rotation: canvas.ink.rotation.into(),
            paper: paper_of(canvas),
        }
    }

    /// Erasing a stroke from the *middle* of a page is a rebuild, and says why.
    ///
    /// This is the case a length-and-ends fingerprint got wrong: an erase in the middle leaves the count
    /// one lower and both ends where they were, so a key built from those says "nothing changed" and the
    /// next frame draws the stroke the reader just removed. Here the page itself says otherwise — its
    /// revision moved, and so did what the edit was.
    #[test]
    fn erasing_inside_a_page_is_a_rebuild() {
        let full = page_of([0x11_11_11, 0x22_22_22, 0x33_33_33]);
        let state = settled(&full);

        let mut erased = full.clone();
        erased.ink.edit += 1;
        erased.ink.delta = Some(note_ui::ink::PageDelta::new(
            state.revision,
            vec![note_ui::ink::InkChange::Remove {
                id: note_ui::ink::StrokeId::of(&full.ink.strokes[1]),
                at: 1,
                old_bounds: full.ink.strokes[1].bounds,
            }],
        ));
        erased.ink.strokes = Arc::new(
            [0usize, 2]
                .iter()
                .map(|index| full.ink.strokes[*index].clone())
                .collect::<Vec<_>>(),
        );

        assert_eq!(
            decide(&erased, &state, COVERED, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::NotAnAppend),
            "an erase is never an append, wherever on the page it was"
        );
    }

    /// Undoing an erase from the *middle* of a page is a rebuild too, and for the same reason the erase
    /// was.
    ///
    /// The other direction of [`erasing_inside_a_page_is_a_rebuild`], and the case a length-and-count
    /// decision got wrong. With `[A, B, C]` erased to `[A, C]` and drawn that way, an undo puts `B` back
    /// in the middle and the page grows from 2 to 3 — one more stroke, which the count alone read as the
    /// append path. But the append path fills the page's *last* stroke (`C`), which was already there, so
    /// `B` would have been drawn nowhere, or out of the paint order it belongs in. The same `Arc` is kept
    /// across the erase, so this is `B` coming back where it was, not a new stroke at the end.
    #[test]
    fn undoing_a_middle_erase_preserves_paint_order() {
        let mut restored = page_of([0x11_11_11, 0x22_22_22, 0x33_33_33]);
        let mut erased = restored.clone();
        let strokes = &restored.ink.strokes;
        erased.ink.strokes = Arc::new(vec![strokes[0].clone(), strokes[2].clone()]);
        let state = settled(&erased);

        let middle = &strokes[1];
        restored.ink.edit = state.revision + 1;
        restored.ink.delta = Some(note_ui::ink::PageDelta::new(
            state.revision,
            vec![note_ui::ink::InkChange::Insert {
                id: note_ui::ink::StrokeId::of(middle),
                at: 1,
                bounds: middle.bounds,
            }],
        ));

        assert_eq!(
            decide(&restored, &state, COVERED, &Paint::nothing()),
            InkPlan::Rebuild(Demotion::NotAnAppend),
            "a stroke put back in the middle is not an append, and must not be drawn at the end"
        );
    }

    /// A stroke that was given up to save memory is filled from its points again, not drawn empty.
    ///
    /// `Stroke::shed_pieces` is what makes a 100,000-stroke page cost megabytes instead of gigabytes, and the
    /// one thing it must not do is change what a frame draws: a shed stroke has no polygons of its own, so
    /// `fill` builds them — at the detail the stroke was already at, which is what makes the pages the
    /// reader turns back to identical to the pages they left.
    #[test]
    fn a_shed_stroke_is_filled_from_its_points() {
        let canvas = page_of([0x11_11_11, 0x22_22_22, 0x33_33_33]);
        let mapping = Mapping {
            base: (0.0, 0.0),
            step_x: (1.0, 0.0),
            step_y: (0.0, 1.0),
        };

        // The same call the surface makes for one stroke: its own scratch, because a test fills one.
        let fill_one = |stroke: &note_format::ink::Stroke, mapping: &Mapping| {
            let mut vertices = Vec::new();
            fill(
                &mut vertices,
                &mut Vec::new(),
                &mut Sweep::default(),
                stroke,
                mapping,
                (0.0, 0.0),
                gpui_kit::rgba(0x00FF_00FF).into(),
            );
            vertices
        };

        let before = canvas.ink.strokes[1].pieces_at_detail().len();
        assert!(before > 0, "an ordinary stroke has pieces");

        let filled_before = fill_one(&canvas.ink.strokes[1], &mapping);

        let mut shed = (*canvas.ink.strokes[1]).clone();
        assert!(shed.shed_pieces() > 0, "shedding released nothing");
        assert!(shed.pieces.is_none(), "shedding left the polygons behind");
        assert!(!shed.outline.is_empty(), "shedding took the outline too");

        let filled_after = fill_one(&shed, &mapping);

        assert!(
            !filled_after.is_empty(),
            "a stroke given up to save memory drew as nothing"
        );
        assert_eq!(
            as_numbers(&filled_before),
            as_numbers(&filled_after),
            "the stroke drawn after shedding was not the stroke drawn before it"
        );
    }

    /// A vertex list as the numbers a renderer would read, so two fills can be compared.
    ///
    /// `Vertex` is `#[repr(C)]` for the shader's sake and deliberately carries no `PartialEq`: nothing on the
    /// drawing path compares corners. This is the comparison a *test* wants, and it is written here rather
    /// than on the type so the type stays a piece of the buffer format.
    fn as_numbers(vertices: &[Vertex]) -> Vec<(u32, u32, [u8; 4])> {
        vertices
            .iter()
            .map(|vertex| (vertex.x.to_bits(), vertex.y.to_bits(), vertex.colour))
            .collect()
    }
}
