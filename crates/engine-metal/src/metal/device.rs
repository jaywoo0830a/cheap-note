//! The macOS half's device: `MTLDevice`, one queue, and a `CAMetalLayer` of the app's own.
//!
//! One command buffer a frame, and **two render passes into the same multisampled target**: the first
//! marks every band's triangles into a stencil, the second colours each marked sample once. That is what
//! makes a band the *union* of its pieces rather than the composite — a highlighter reads at its 35 %
//! however many pieces met under it (`doc/ENGINE.md` §4.1, E6).
//!
//! The vertex format is the one `apps/macos/src/platform/metal.rs` settled on: twelve bytes a corner,
//! the colour as four `unorm` bytes rather than four floats, because a page of writing is millions of
//! corners and a stroke's corners all carry the same colour.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCompareFunction, MTLDevice as _,
    MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState,
    MTLResourceOptions, MTLStencilOperation, MTLStorageMode, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use crate::gpu::{Gpu, GpuError};
use crate::plan::{BaseLayer, Mark, Plan, Step};

/// Samples a pixel: the ink is a polygon and Metal has no analytic antialiasing to give it, where
/// Direct2D on Windows fills with coverage instead. Four is what `apps/macos` measured as the point
/// where the edge is smooth and the pass is still cheap (`doc/VIEW.md` §12, `SAMPLES`).
const SAMPLES: usize = 4;

/// The one colour format both passes and the layer use: the compositor's own, so a drawable needs no
/// conversion.
const FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

/// The shaders: place a pixel, and colour it.
///
/// The engine hands the half its triangles *already placed* — the placement is applied when a band is
/// built (`cheap_note_engine::place::Placement`) — so the shader only has to turn a window pixel into
/// clip space. That is one multiplication fewer per corner than the old canvas did.
const SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Vertex { packed_float2 position; uchar4 colour; };
struct Varyings { float4 position [[position]]; float4 colour; };

static inline float4 place(float2 position, float2 viewport) {
    return float4(position.x / viewport.x * 2.0 - 1.0,
                  1.0 - position.y / viewport.y * 2.0,
                  0.0, 1.0);
}

vertex Varyings solid_vertex(uint id [[vertex_id]],
                             constant Vertex *vertices [[buffer(0)]],
                             constant float2 &viewport [[buffer(1)]]) {
    Varyings out;
    out.position = place(float2(vertices[id].position), viewport);
    out.colour = float4(vertices[id].colour) / 255.0;
    return out;
}

fragment float4 solid_fragment(Varyings in [[stage_in]]) {
    return in.colour;
}
"#;

/// One corner: where it is in device pixels, and the colour it carries as four `unorm` bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Vertex {
    x: f32,
    y: f32,
    colour: [u8; 4],
}

impl Vertex {
    /// A corner at `(x, y)` in device pixels, in `colour` (`0xAARRGGBB`, or `0xRRGGBB` for opaque).
    fn new(x: f32, y: f32, colour: u32) -> Self {
        // Alpha in the top byte, as the note writes it: a colour with no top byte is opaque
        // (`note_format::ink::alpha_of`).
        let alpha = ((colour >> 24) & 0xFF) as u8;
        let alpha = if alpha == 0 { 0xFF } else { alpha };

        Vertex {
            x,
            y,
            colour: [
                ((colour >> 16) & 0xFF) as u8,
                ((colour >> 8) & 0xFF) as u8,
                (colour & 0xFF) as u8,
                alpha,
            ],
        }
    }
}

/// The window's GPU canvas, drawn by the engine's plan.
pub struct Device {
    device: Retained<ProtocolObject<dyn objc2_metal::MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Fills, ruling, ink and the pen's mark: one colour a corner.
    solid: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The marking pass: writes 1 wherever a band's pieces reach, and colours nothing.
    marking: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The colouring pass: draws only where the stencil is 1, so a band is coloured once.
    coloured: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// What "marked" means for each pass: the stencil state a draw binds.
    solid_state: Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>,
    marking_state: Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>,
    coloured_state: Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>,
    /// The colouring pass: draws only where the stencil is 1, so a band is coloured once.
    layer: Retained<CAMetalLayer>,
    width: u32,
    height: u32,
    scale: f32,
    /// The multisampled target the passes draw into, resolved into the drawable. Made with the size.
    msaa: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// The stencil buffer, shared by the marking pass and the colouring pass.
    mask: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// The last command buffer committed, kept so a benchmark can wait for it (`settle`).
    last: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// What the last frame drew, for the trace and the benchmark.
    pub stated: Statement,
}

/// What one frame asked the device to do, counted rather than guessed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statement {
    /// Corners drawn for the base layer.
    pub base: u32,
    /// Bands filled.
    pub bands: u32,
    /// Corners drawn for the ink and the pen's mark.
    pub ink: u32,
    /// Whether a drawable was handed the frame.
    pub presented: bool,
    /// How long asking for a drawable blocked — the display's clock, which the bench takes out.
    pub waited_ms: u32,
}

/// The two textures a frame draws into: the multisampled colour target, and the stencil buffer the
/// marking pass writes and the colouring pass reads.
struct Targets {
    colour: Retained<ProtocolObject<dyn MTLTexture>>,
    mask: Retained<ProtocolObject<dyn MTLTexture>>,
}

impl Device {
    /// A device on the machine's GPU, drawing into `layer` at a device size and a display scale.
    ///
    /// `None` is a machine with no Metal device — which no supported Mac is, but a sandbox or a remote
    /// session can be — and it is reported once, at startup, by whoever installs the surface: the app
    /// keeps its last frame and says in the status line what is wrong (`doc/ENGINE.md` §4.3).
    pub fn new(layer: Retained<CAMetalLayer>, width: u32, height: u32, scale: f32) -> Option<Self> {
        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;

        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .ok()?;

        let solid = pipeline(&device, &library, Stage::Solid)?;
        let coloured = pipeline(&device, &library, Stage::Coloured)?;
        let marking = pipeline(&device, &library, Stage::Marking)?;

        let solid_state = depth_stencil(&device, Stage::Solid)?;
        let marking_state = depth_stencil(&device, Stage::Marking)?;
        let coloured_state = depth_stencil(&device, Stage::Coloured)?;

        layer.setDevice(Some(&device));
        layer.setPixelFormat(FORMAT);
        layer.setFramebufferOnly(true);
        layer.setContentsScale(f64::from(scale));
        // Three in flight: enough for the compositor to hold one while the next is drawn, and no more.
        layer.setMaximumDrawableCount(3);
        // A drawable that is not ready is a frame to skip, not a thread to park: the pen's pump draws on
        // the main thread, and waiting here would hold the next reading behind the compositor.
        layer.setAllowsNextDrawableTimeout(false);
        // The display's clock is the compositor's; whether to wait is the note's setting, and the
        // environment variable still wins so an experiment needs no note written to try it.
        layer.setDisplaySyncEnabled(std::env::var_os("CHEAP_NOTE_NO_VSYNC").is_none());
        layer.setDrawableSize(objc2_core_foundation::CGSize::new(
            f64::from(width.max(1)),
            f64::from(height.max(1)),
        ));

        Some(Device {
            device,
            queue,
            solid,
            marking,
            coloured,
            solid_state,
            marking_state,
            coloured_state,
            layer,
            width: width.max(1),
            height: height.max(1),
            scale: if scale > 0.0 { scale } else { 1.0 },
            msaa: None,
            mask: None,
            last: None,
            stated: Statement::default(),
        })
    }

    /// Matches the drawable and the targets to a new size. A resize drops what was built for the old one.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        if (self.width, self.height) == (width, height) && (self.scale - scale).abs() < 1e-6 {
            return;
        }

        self.width = width.max(1);
        self.height = height.max(1);
        self.scale = if scale > 0.0 { scale } else { 1.0 };

        self.layer.setContentsScale(f64::from(self.scale));
        self.layer
            .setDrawableSize(objc2_core_foundation::CGSize::new(
                f64::from(self.width),
                f64::from(self.height),
            ));

        self.msaa = None;
        self.mask = None;
    }

    /// Waits for the last frame to be paid for. The benchmark's door, and nothing else's.
    pub fn settle(&self) {
        if let Some(last) = &self.last {
            last.waitUntilCompleted();
        }
    }

    /// The layer the canvas is composed from.
    pub fn layer(&self) -> &CAMetalLayer {
        &self.layer
    }

    /// Waits for the display's clock before a frame is shown, or stops waiting.
    ///
    /// One property of the layer, consulted when a drawable is presented rather than when the layer is
    /// made — so the switch takes effect the moment it moves. `CHEAP_NOTE_NO_VSYNC` still overrides it,
    /// because that is the *experiment*, and an experiment a note setting could override would be a
    /// measurement of the note (`doc/MACOS.md` §4.3).
    pub fn set_vsync(&mut self, on: bool) {
        if std::env::var_os("CHEAP_NOTE_NO_VSYNC").is_some() {
            return;
        }

        self.layer.setDisplaySyncEnabled(on);
    }

    /// A corner of a triangle, in device pixels, in `colour`.
    fn corner(&self, (x, y): (f32, f32), colour: u32) -> Vertex {
        Vertex::new(x * self.scale, y * self.scale, colour)
    }

    /// The multisampled colour target and the stencil buffer, made once per size.
    fn targets(&mut self) -> Option<Targets> {
        if let (Some(msaa), Some(mask)) = (&self.msaa, &self.mask) {
            return Some(Targets {
                colour: msaa.clone(),
                mask: mask.clone(),
            });
        }

        let (width, height) = (self.width as usize, self.height as usize);

        // SAFETY: a 2D descriptor for the size this surface draws at.
        let colour = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                FORMAT, width, height, false,
            )
        };
        colour.setTextureType(MTLTextureType::Type2DMultisample);
        // SAFETY: a sample count Metal supports for this format on every Mac that runs this.
        unsafe {
            colour.setSampleCount(SAMPLES);
        }
        colour.setStorageMode(MTLStorageMode::Private);
        colour.setUsage(MTLTextureUsage::RenderTarget);

        let msaa = self.device.newTextureWithDescriptor(&colour)?;

        // The mask: a stencil rendered into, never sampled, with the same sample count as the colour
        // target — a stencil attachment must agree with its colour attachment.
        let stencil = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::Stencil8,
                width,
                height,
                false,
            )
        };
        stencil.setTextureType(MTLTextureType::Type2DMultisample);
        // SAFETY: the same count as above.
        unsafe {
            stencil.setSampleCount(SAMPLES);
        }
        stencil.setStorageMode(MTLStorageMode::Private);
        stencil.setUsage(MTLTextureUsage::RenderTarget);

        let mask = self.device.newTextureWithDescriptor(&stencil)?;

        self.msaa = Some(msaa.clone());
        self.mask = Some(mask.clone());

        Some(Targets { colour: msaa, mask })
    }

    /// Uploads a run of corners into a shared buffer.
    fn buffer(
        &self,
        vertices: &[Vertex],
    ) -> Option<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>> {
        use objc2_metal::MTLBuffer;

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

    /// Charts a device from the last frame: corners, bands, whether a base was laid.
    pub fn stated(&self) -> Statement {
        self.stated
    }

    /// Encodes one frame and hands it over: never waits, never re-draws what did not change.
    ///
    /// Two passes into the same multisampled target, resolved into the drawable once:
    ///
    /// 1. **the base** — the desk, the shadow, the sheet and the ruling. Opaque rectangles over a clear,
    ///    so this pass needs no stencil and no blending surprise.
    /// 2. **the ink** — every repair in the desk's colour, then each band *twice*: once to mark its pieces
    ///    into the stencil, once to colour each marked sample. A band that doubles back is still one
    ///    region, and a highlighter reads at its own 35 % (E6, `doc/ENGINE.md` §4.1). The pen's mark is
    ///    the last thing drawn in this pass, because a cursor belongs above the ink it is about to write.
    pub fn encode(
        &mut self,
        base: &BaseLayer,
        plan: &Plan,
        mark: Option<&Mark>,
    ) -> Result<(), GpuError> {
        let waited = std::time::Instant::now();
        let drawable = self.layer.nextDrawable();
        let waited = waited.elapsed().as_millis() as u32;

        let Some(drawable) = drawable else {
            self.stated = Statement {
                presented: false,
                waited_ms: waited,
                ..Statement::default()
            };
            return Err(GpuError::new("no drawable was ready"));
        };

        let Some(Targets { colour: msaa, mask }) = self.targets() else {
            return Err(GpuError::new("the render targets could not be made"));
        };

        // What the plan asks for, as corners: a repair is a quad in the desk's colour, a band is its
        // triangles in the band's colour.
        let mut repairs: Vec<Vec<Vertex>> = Vec::new();
        let mut bands: Vec<Vec<Vertex>> = Vec::new();

        for step in plan.steps() {
            match step {
                Step::Repair(rect) if !rect.is_empty() => {
                    let mut corners = Vec::with_capacity(6);
                    let (x, y) = (rect.x, rect.y);
                    quad(
                        &mut corners,
                        x + rect.width,
                        y + rect.height,
                        x,
                        y,
                        base.desk.unwrap_or(0xFFFF_FFFF),
                        self.scale,
                    );
                    repairs.push(corners);
                }
                Step::Band { triangles, colour } => {
                    let mut corners = Vec::with_capacity(triangles.len() * 3);
                    for triangle in triangles {
                        corners.push(self.corner(triangle.a, *colour));
                        corners.push(self.corner(triangle.b, *colour));
                        corners.push(self.corner(triangle.c, *colour));
                    }
                    bands.push(corners);
                }
                _ => {}
            }
        }

        let base_corners = base_vertices(base, self.width as f32, self.height as f32, self.scale);

        let Some(command) = self.queue.commandBuffer() else {
            return Err(GpuError::new("the command buffer could not be made"));
        };

        if !self.base_pass(&command, &msaa, &drawable, &base_corners) {
            return Err(GpuError::new("the base pass could not be encoded"));
        }

        if !self.ink_pass(&command, &msaa, &mask, &repairs, &bands, mark) {
            return Err(GpuError::new("the ink pass could not be encoded"));
        }

        // SAFETY: `presentDrawable:` is the command buffer's own method for the drawable it was handed.
        unsafe {
            let _: () = objc2::msg_send![&*command, presentDrawable: &*drawable];
        }

        command.commit();

        self.stated = Statement {
            base: base_corners.len() as u32,
            bands: bands.len() as u32,
            ink: bands.iter().map(|corners| corners.len() as u32).sum(),
            presented: true,
            waited_ms: waited,
        };

        self.last = Some(command);

        Ok(())
    }

    /// The base: the desk and every fill, into the drawable, cleared first.
    fn base_pass(
        &self,
        command: &ProtocolObject<dyn MTLCommandBuffer>,
        msaa: &ProtocolObject<dyn MTLTexture>,
        drawable: &ProtocolObject<dyn CAMetalDrawable>,
        corners: &[Vertex],
    ) -> bool {
        let pass = MTLRenderPassDescriptor::new();

        // SAFETY: index 0 is the one colour attachment these pipelines were built for, and the drawable's
        // texture outlives the command buffer's encoding (the buffer retains it until it is presented).
        unsafe {
            let colour = pass.colorAttachments().objectAtIndexedSubscript(0);
            colour.setTexture(Some(msaa));
            colour.setResolveTexture(Some(&drawable.texture()));
            colour.setLoadAction(MTLLoadAction::Clear);
            colour.setStoreAction(MTLStoreAction::MultisampleResolve);
        }

        let Some(encoder) = command.renderCommandEncoderWithDescriptor(&pass) else {
            return false;
        };

        self.draw(&encoder, Stage::Solid, corners);
        encoder.endEncoding();

        true
    }

    /// The ink: repairs, then each band marked and coloured once, then the pen's mark.
    fn ink_pass(
        &self,
        command: &ProtocolObject<dyn MTLCommandBuffer>,
        msaa: &ProtocolObject<dyn MTLTexture>,
        mask: &ProtocolObject<dyn MTLTexture>,
        repairs: &[Vec<Vertex>],
        bands: &[Vec<Vertex>],
        mark: Option<&Mark>,
    ) -> bool {
        let pass = MTLRenderPassDescriptor::new();

        // SAFETY: the colour target is loaded rather than cleared — the base pass put the desk and the
        // sheet there — and the stencil is the mask, cleared here so every frame starts unmarked.
        unsafe {
            let colour = pass.colorAttachments().objectAtIndexedSubscript(0);
            colour.setTexture(Some(msaa));
            colour.setLoadAction(MTLLoadAction::Load);
            colour.setStoreAction(MTLStoreAction::Store);

            let stencil = pass.stencilAttachment();
            stencil.setTexture(Some(mask));
            stencil.setLoadAction(MTLLoadAction::Clear);
            stencil.setStoreAction(MTLStoreAction::Store);
            stencil.setClearStencil(0);
        }

        let Some(encoder) = command.renderCommandEncoderWithDescriptor(&pass) else {
            return false;
        };

        // Repairs first, in the desk's colour: what an erase or an undo uncovered is desk again until a
        // band paints over it.
        for corners in repairs {
            self.draw(&encoder, Stage::Solid, corners);
        }

        // Each band: marked, then coloured. Two draws a band, and one pass.
        for corners in bands {
            self.draw(&encoder, Stage::Marking, corners);
            self.draw(&encoder, Stage::Coloured, corners);
        }

        if let Some(mark) = mark {
            let mut corners = Vec::with_capacity(18);
            mark_vertices(&mut corners, mark, self.scale);
            self.draw(&encoder, Stage::Solid, &corners);
        }

        encoder.endEncoding();

        true
    }

    /// One run of corners, in one draw call, through `stage`'s pipeline and stencil state.
    fn draw(
        &self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        stage: Stage,
        vertices: &[Vertex],
    ) {
        let Some(buffer) = self.buffer(vertices) else {
            return;
        };

        let (pipeline, state) = match stage {
            Stage::Solid => (&self.solid, &self.solid_state),
            Stage::Marking => (&self.marking, &self.marking_state),
            Stage::Coloured => (&self.coloured, &self.coloured_state),
        };

        encoder.setRenderPipelineState(pipeline);
        encoder.setDepthStencilState(Some(state));
        // The marking pass writes 1, and the colouring pass compares against 1: one reference for both.
        encoder.setStencilReferenceValue(1);

        let viewport: [f32; 2] = [self.width as f32, self.height as f32];

        // SAFETY: the buffer holds the corners the draw call reads, and the viewport is bytes the encoder
        // copies before this function returns.
        unsafe {
            encoder.setVertexBuffer_offset_atIndex(Some(&buffer), 0, 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(viewport.as_ptr() as *mut c_void).unwrap(),
                std::mem::size_of_val(&viewport),
                1,
            );
            encoder.drawPrimitives_vertexStart_vertexCount(
                MTLPrimitiveType::Triangle,
                0,
                vertices.len(),
            );
        }
    }
}

/// Which of the three pipelines a pass uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Opaque or translucent colour over what is there: the base, a repair, the pen's mark.
    Solid,
    /// Colour only where the stencil is 1: a band's one fill (E6).
    Coloured,
    /// Writes 1 into the stencil and colours nothing: a band's pieces, marked.
    Marking,
}

/// A pipeline that does one of the three, blending colour over what is already there.
fn pipeline(
    device: &ProtocolObject<dyn objc2_metal::MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    stage: Stage,
) -> Option<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
    let vertex = library.newFunctionWithName(&NSString::from_str("solid_vertex"))?;
    let fragment = library.newFunctionWithName(&NSString::from_str("solid_fragment"))?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vertex));
    descriptor.setFragmentFunction(Some(&fragment));
    descriptor.setRasterSampleCount(SAMPLES);

    // SAFETY: index 0 is the one colour attachment every pass here renders into, and the descriptor is
    // alive for this function.
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(FORMAT);

    // Source over destination — the same arithmetic Core Graphics did, so a highlighter over a page
    // looks the way it looked before. The marking pass writes nothing to colour, so it leaves blending
    // and colour writes off.
    //
    // SAFETY: the attachment came from the descriptor above, which is alive for this function; each call
    // is a member of the protocol's own colour-attachment descriptor, and the values are the enum cases
    // for source-over.
    unsafe {
        use objc2_metal::{MTLBlendFactor, MTLBlendOperation};

        let attachment = &*attachment;

        if stage == Stage::Marking {
            let _: () = objc2::msg_send![attachment, setWriteMask: 0u32];
        } else {
            let _: () =
                objc2::msg_send![attachment, setSourceRGBBlendFactor: MTLBlendFactor::SourceAlpha];
            let _: () = objc2::msg_send![attachment, setDestinationRGBBlendFactor: MTLBlendFactor::OneMinusSourceAlpha];
            let _: () =
                objc2::msg_send![attachment, setSourceAlphaBlendFactor: MTLBlendFactor::One];
            let _: () = objc2::msg_send![attachment, setDestinationAlphaBlendFactor: MTLBlendFactor::OneMinusSourceAlpha];
            let _: () = objc2::msg_send![attachment, setBlendingEnabled: true];
            let _: () = objc2::msg_send![attachment, setRgbBlendOperation: MTLBlendOperation::Add];
        }
    }

    // SAFETY: the descriptor's own attachment format; a stencil format is declared for the two passes
    // that use one, and left `Invalid` for the base, which has no stencil attachment at all.
    if stage == Stage::Marking || stage == Stage::Coloured {
        descriptor.setStencilAttachmentPixelFormat(MTLPixelFormat::Stencil8);
    }

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .ok()
}

/// The stencil state a pass binds: what "is a place marked" means for this pass.
///
/// `MTLRenderPipelineDescriptor` in this binding has no stencil descriptor of its own — the state lives
/// in an `MTLDepthStencilState` the pass binds, which is also how a marking pass and a colouring pass
/// can share a pipeline's worth of shaders and differ in what the stencil means.
fn depth_stencil(
    device: &ProtocolObject<dyn objc2_metal::MTLDevice>,
    stage: Stage,
) -> Option<Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>> {
    use objc2_metal::MTLDepthStencilDescriptor;

    let descriptor = MTLDepthStencilDescriptor::new();
    // No depth at all: every pass here draws into one plane.
    descriptor.setDepthCompareFunction(objc2_metal::MTLCompareFunction::Always);
    descriptor.setDepthWriteEnabled(false);

    let face = objc2_metal::MTLStencilDescriptor::new();

    match stage {
        Stage::Marking => {
            // Write 1 wherever a band's pieces reach, and nowhere else.
            face.setStencilCompareFunction(MTLCompareFunction::Always);
            face.setStencilFailureOperation(MTLStencilOperation::Replace);
            face.setDepthFailureOperation(MTLStencilOperation::Replace);
            face.setDepthStencilPassOperation(MTLStencilOperation::Replace);
            face.setReadMask(0xFF);
            face.setWriteMask(0xFF);
        }
        Stage::Coloured => {
            // Colour only where the marking pass reached, and keep the stencil as it is.
            face.setStencilCompareFunction(MTLCompareFunction::Equal);
            face.setStencilFailureOperation(MTLStencilOperation::Keep);
            face.setDepthFailureOperation(MTLStencilOperation::Keep);
            face.setDepthStencilPassOperation(MTLStencilOperation::Keep);
            face.setReadMask(0xFF);
            face.setWriteMask(0x00);
        }
        Stage::Solid => {
            // No stencil test: the base and the pen's mark are drawn wherever they are.
            face.setStencilCompareFunction(MTLCompareFunction::Always);
            face.setStencilFailureOperation(MTLStencilOperation::Keep);
            face.setDepthFailureOperation(MTLStencilOperation::Keep);
            face.setDepthStencilPassOperation(MTLStencilOperation::Keep);
            face.setReadMask(0x00);
            face.setWriteMask(0x00);
        }
    }

    descriptor.setFrontFaceStencil(Some(&face));
    descriptor.setBackFaceStencil(Some(&face));

    device.newDepthStencilStateWithDescriptor(&descriptor)
}

/// The base layer as triangles: the desk, then every fill, in the order painted.
///
/// `width` and `height` are the target's device pixels; the fills arrive in logical ones and are scaled
/// here. A fill with no area is not a fill — a sheet scrolled off the surface, a window narrower than its
/// own margin — and is skipped.
fn base_vertices(base: &BaseLayer, width: f32, height: f32, scale: f32) -> Vec<Vertex> {
    let mut vertices = Vec::with_capacity(6 + base.fills.len() * 6);

    quad(
        &mut vertices,
        width,
        height,
        0.0,
        0.0,
        base.desk.unwrap_or(0xFFFF_FFFF),
        scale,
    );

    for fill in &base.fills {
        let rect = fill.rect;
        if rect.width <= 0.0 || rect.height <= 0.0 {
            continue;
        }

        quad(
            &mut vertices,
            rect.x + rect.width,
            rect.y + rect.height,
            rect.x,
            rect.y,
            fill.colour,
            scale,
        );
    }

    vertices
}

/// Two triangles covering a rectangle in logical pixels, scaled into device ones.
fn quad(
    vertices: &mut Vec<Vertex>,
    right: f32,
    bottom: f32,
    x: f32,
    y: f32,
    colour: u32,
    scale: f32,
) {
    let vertex = |x: f32, y: f32| Vertex::new(x * scale, y * scale, colour);

    vertices.extend_from_slice(&[
        vertex(x, y),
        vertex(right, y),
        vertex(x, bottom),
        vertex(x, bottom),
        vertex(right, y),
        vertex(right, bottom),
    ]);
}

/// The pen's mark: a bloom round the nib, the nib's own dot, and the rod its lean draws.
///
/// The engine hands the half *where* and *how thick*; the view owns what a cursor looks like
/// (`note_ui::cursor`, platform-free) and this draws what it is told.
fn mark_vertices(vertices: &mut Vec<Vertex>, mark: &Mark, scale: f32) {
    let unit = if mark.unit.is_finite() && mark.unit > 0.0 {
        mark.unit
    } else {
        1.0
    };

    let radius = if mark.nib_radius.is_finite() {
        mark.nib_radius.max(0.0)
    } else {
        0.0
    };

    // The halo first, so a nib of the paper's own colour can still be found; then the dot itself.
    disc(
        vertices,
        mark.at,
        radius * 1.35 * unit,
        mark.halo,
        0.35,
        scale,
    );
    disc(vertices, mark.at, radius, mark.colour, 1.0, scale);

    // The body's rod: a thin quad leaning away from the nib, faint while the pen hovers.
    let lean = if mark.lean.is_finite() {
        mark.lean.clamp(0.0, 1.0)
    } else {
        0.0
    };

    if lean <= 0.0 {
        return;
    }

    let (x, y) = mark.at;
    let length = 18.0 * unit * lean;
    let width = 2.0 * unit;
    let tip = (x + length * 0.6, y - length * 0.35);
    let colour = (mark.halo & 0x00FF_FFFF) | (0x55 << 24);
    let vertex = |x: f32, y: f32| Vertex::new(x * scale, y * scale, colour);

    vertices.extend_from_slice(&[
        vertex(x - width, y),
        vertex(tip.0 - width, tip.1),
        vertex(x + width, y),
        vertex(x + width, y),
        vertex(tip.0 - width, tip.1),
        vertex(tip.0 + width, tip.1),
    ]);
}

/// One disc, as a fan of triangles: the nib's dot and its bloom.
fn disc(
    vertices: &mut Vec<Vertex>,
    at: (f32, f32),
    radius: f32,
    colour: u32,
    alpha: f32,
    scale: f32,
) {
    /// How many points a round mark is drawn with: enough that the rim reads as round at any nib size.
    const ARC_STEPS: usize = 16;

    if radius <= 0.0 {
        return;
    }

    let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    let colour = (colour & 0x00FF_FFFF) | (alpha << 24);
    let centre = Vertex::new(at.0 * scale, at.1 * scale, colour);

    for step in 0..ARC_STEPS {
        let from = std::f32::consts::TAU * step as f32 / ARC_STEPS as f32;
        let to = std::f32::consts::TAU * (step + 1) as f32 / ARC_STEPS as f32;

        vertices.push(centre);
        vertices.push(Vertex::new(
            (at.0 + radius * from.cos()) * scale,
            (at.1 + radius * from.sin()) * scale,
            colour,
        ));
        vertices.push(Vertex::new(
            (at.0 + radius * to.cos()) * scale,
            (at.1 + radius * to.sin()) * scale,
            colour,
        ));
    }
}

/// The engine's seam, as this device implements it.
///
/// A frame is handed over in one command: `base`, then `run`, then `mark`, all into the same buffer, so
/// the three are one present rather than three (`doc/ENGINE.md` §5.2). The engine's `Surface::draw` calls
/// them in that order; a plan with nothing in it still presents, and a frame with no drawable is
/// `NotReady` — never a wait.
#[cfg(target_os = "macos")]
impl Gpu for Device {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn drawable(&self) -> bool {
        // Asking for a drawable *waits* when the compositor holds all three
        // (`doc/MACOS.md` §8's `warm`), which is the one wait a reading cannot afford — so the check is
        // whether the layer has one to give, not a blocking request. The encode takes it, once.
        true
    }

    fn run(&mut self, plan: &Plan) -> Result<(), GpuError> {
        // The plan is encoded whole by `encode`; a `run` on its own is the frame with no base and no
        // mark, which is what a plan handed in isolation means.
        let base = BaseLayer {
            desk: None,
            fills: Vec::new(),
        };
        self.encode(&base, plan, None)
    }

    fn present(
        &mut self,
        base: &BaseLayer,
        plan: &Plan,
        mark: Option<&Mark>,
    ) -> Result<(), GpuError> {
        // One command buffer, two passes, one present: the base under the ink and the mark over it.
        self.encode(base, plan, mark)
    }

    fn base(&mut self, _base: &BaseLayer) -> Result<(), GpuError> {
        // The base is encoded by `encode`, with the ink, into one command buffer: a separate pass here
        // would be a second present, which is a frame nobody asked for (E1).
        Ok(())
    }

    fn mark(&mut self, _mark: Option<&Mark>) -> Result<(), GpuError> {
        // Likewise: the mark is the last thing in the ink pass.
        Ok(())
    }
}

/// A machine with no Metal: a surface that says why it cannot draw.
#[cfg(not(target_os = "macos"))]
impl Gpu for Device {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn drawable(&self) -> bool {
        false
    }

    fn run(&mut self, _plan: &Plan) -> Result<(), GpuError> {
        Err(GpuError::new("this machine has no Metal device"))
    }

    fn base(&mut self, _base: &BaseLayer) -> Result<(), GpuError> {
        Err(GpuError::new("this machine has no Metal device"))
    }

    fn mark(&mut self, _mark: Option<&Mark>) -> Result<(), GpuError> {
        Err(GpuError::new("this machine has no Metal device"))
    }
}
