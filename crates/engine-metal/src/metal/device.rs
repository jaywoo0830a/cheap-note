//! The macOS half's device: `MTLDevice`, one queue, one target, and one frame a command buffer.
//!
//! ## What a frame is
//!
//! **One command buffer, two render passes, one present.** The base pass clears and paints the desk and
//! the sheet; the ink pass loads what the first pass left, writes each band's triangles into a stencil,
//! and then colours each marked sample once. That is what makes a band the *union* of its pieces rather
//! than their composite — a highlighter reads at its own alpha however many pieces met under it
//! (`doc/ENGINE.md` §4.1, E6) — and it is why a stroke never draws itself twice and darkens.
//!
//! ## The unit contract
//!
//! Every corner this device is handed is in **logical pixels**, and [`Target`] is the only thing that
//! knows the display's density. [`Target::device_point`] multiplies once, here, in [`Device::corner`].
//! Nothing above this file multiplies by the scale: a placement, a band and a rectangle all arrive in
//! the window's own units. Two densities for one point is not a rounding error — it draws the ink at
//! twice its distance from the origin while the sheet, which is logical, stays put.
//!
//! ## Failure is loud
//!
//! Metal refuses a pipeline whose sample count or attachment formats disagree with the pass, and a
//! refused pipeline draws *nothing at all* — no error on the frame, no frame, just an empty canvas. So
//! every step here is either checked when it is made ([`Device::make`] returns `None` unless every
//! pipeline was accepted) or reported as a [`GpuError`] when a frame cannot be encoded
//! ([`Device::encode_from`]). Nothing here answers `Ok(())` for a frame it did not draw.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLColorWriteMask, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCompareFunction,
    MTLDevice as _, MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLResourceOptions, MTLStencilOperation, MTLStorageMode,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use crate::gpu::{Gpu, GpuError};
use crate::plan::{BaseLayer, Mark, Plan, Step, Target};
use cheap_note_engine::footprint::Rect;

/// Samples a pixel: the ink is a polygon and Metal has no analytic antialiasing to give it, where
/// Direct2D on Windows fills with coverage instead. Four is what `apps/macos` measured as the point
/// where the edge is smooth and the pass is still cheap (`doc/VIEW.md` §12).
///
/// `MTLRenderPipelineDescriptor.rasterSampleCount` must equal the sample count of every render target,
/// and `MTLDevice.supportsTextureSampleCount(_:)` is what says this GPU agrees — checked in
/// [`Device::make`], never assumed.
const SAMPLES: usize = 4;

/// The one colour format every pass and every layer uses: the compositor's own, so a drawable needs no
/// conversion, and a resolve texture must share its pixel format
/// (`MTLRenderPassAttachmentDescriptor`).
const FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

/// The stencil format: eight bits, the least that can say "marked".
const STENCIL_FORMAT: MTLPixelFormat = MTLPixelFormat::Stencil8;

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

/// The three pipelines and their stencil states, built together because a device with two of them
/// cannot draw a frame.
struct Pipelines {
    /// Fills, ruling, ink and the pen's mark: one colour a corner, source over what is there.
    solid: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The marking pass: writes 1 wherever a band's pieces reach, and colours nothing.
    marking: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The colouring pass: draws only where the stencil is 1, so a band is coloured once.
    coloured: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// No stencil test: the whole of the base and the pen's mark.
    solid_state: Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>,
    /// Compare always, replace: a band's pieces, marked.
    marking_state: Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>,
    /// Compare equal, keep: a band's one fill.
    coloured_state: Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>,
}

/// The two textures a frame draws into, at the target's device size.
struct Targets {
    /// Multisampled, `type2DMultisample`, private: what the passes render into.
    colour: Retained<ProtocolObject<dyn MTLTexture>>,
    /// The stencil, at the same size and sample count as `colour` — a pass whose stencil attachment
    /// disagrees with its colour attachment is refused.
    mask: Retained<ProtocolObject<dyn MTLTexture>>,
    /// Single sample, the resolve destination: the drawable's texture, or an offscreen one.
    resolved: Retained<ProtocolObject<dyn MTLTexture>>,
}

/// The window's GPU canvas, drawn by the engine's plan.
///
/// **One target, one density.** The device holds its [`Target`] — logical size and device pixels per
/// logical pixel — and nothing else about size: every corner arrives in logical pixels, and
/// [`Target::device_point`] is the one multiplication in the whole half.
pub struct Device {
    device: Retained<ProtocolObject<dyn objc2_metal::MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Logical size and density: the half's only definition of where a pixel is.
    target: Target,
    pipelines: Pipelines,
    /// The layer a drawable comes from; `None` for an offscreen device.
    layer: Option<Retained<CAMetalLayer>>,
    msaa: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    mask: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// A single-sample texture of the target's device size, for an offscreen frame: shared, so the CPU
    /// can read what the GPU wrote.
    offscreen: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// The command buffer of the frame handed over last, for the caller's own bookkeeping.
    last: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// What the last frame drew, for the trace and the benchmark.
    pub stated: Statement,
}

/// The device, the queue, and every pipeline — or `None`, with the reason under `CHEAP_NOTE_TRACE`.
///
/// A machine with no Metal at all is the ordinary `None`. Everything else here is a *refusal*: a shader
/// that will not compile, a GPU that will not sample [`SAMPLES`] times, a pipeline Metal declined to
/// build. Those are logged rather than folded into the same silent `None`, because the symptom of each
/// is identical — a canvas that draws nothing — and only the log can tell them apart.
type Made = (
    Retained<ProtocolObject<dyn objc2_metal::MTLDevice>>,
    Retained<ProtocolObject<dyn MTLCommandQueue>>,
    Pipelines,
);

fn make() -> Option<Made> {
    let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
    let queue = device.newCommandQueue()?;

    // The sample count is the GPU's to agree to: a texture with more samples than it supports is not a
    // texture, and a pipeline built for one is refused.
    if !device.supportsTextureSampleCount(SAMPLES) {
        trace(&format!(
            "metal: this GPU does not support {SAMPLES} samples, so the half cannot draw"
        ));
        return None;
    }

    let library =
        match device.newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None) {
            Ok(library) => library,
            Err(error) => {
                trace(&format!("metal: the shaders did not compile: {error}"));
                return None;
            }
        };

    let pipelines = Pipelines {
        solid: built(pipeline(&device, &library, Stage::Solid), "solid")?,
        marking: built(pipeline(&device, &library, Stage::Marking), "marking")?,
        coloured: built(pipeline(&device, &library, Stage::Coloured), "coloured")?,
        solid_state: built(depth_stencil(&device, Stage::Solid), "no-stencil state")?,
        marking_state: built(depth_stencil(&device, Stage::Marking), "marking state")?,
        coloured_state: built(depth_stencil(&device, Stage::Coloured), "coloured state")?,
    };

    Some((device, queue, pipelines))
}

/// A pipeline or a state that Metal built, or `None` with the name of the one it refused.
fn built<T>(made: Option<T>, what: &str) -> Option<T> {
    if made.is_none() {
        trace(&format!("metal: Metal refused the {what}"));
    }

    made
}

/// Says something under `CHEAP_NOTE_TRACE`, and does nothing otherwise: the note's own output is the
/// pen's, and a device that talks all the time would drown it.
fn trace(what: &str) {
    if std::env::var_os("CHEAP_NOTE_TRACE").is_some() {
        eprintln!("{what}");
    }
}

impl Device {
    /// A device on the machine's GPU, drawing into `layer` at a [`Target`]'s size and density.
    ///
    /// `None` is a machine that cannot draw — see [`make`] — and whoever installs the surface reports it
    /// once, at startup: the app keeps its last frame and says in the status line what is wrong
    /// (`doc/ENGINE.md` §4.3).
    pub fn new(layer: Retained<CAMetalLayer>, target: Target) -> Option<Self> {
        let (device, queue, pipelines) = make()?;

        layer.setDevice(Some(&device));
        layer.setPixelFormat(FORMAT);
        layer.setFramebufferOnly(true);
        // Three in flight: enough for the compositor to hold one while the next is drawn, and no more.
        layer.setMaximumDrawableCount(3);
        // A drawable that is not ready is a frame to skip, not a thread to park: the pen's pump draws on
        // the main thread, and waiting here would hold the next reading behind the compositor.
        layer.setAllowsNextDrawableTimeout(false);
        // The display's clock is the compositor's; whether to wait is the note's setting, and the
        // environment variable still wins so an experiment needs no note written to try it.
        layer.setDisplaySyncEnabled(std::env::var_os("CHEAP_NOTE_NO_VSYNC").is_none());

        let device = Self::bare(device, queue, pipelines, target, Some(layer));
        device.follow_target();

        Some(device)
    }

    /// A device drawing an offscreen surface instead of a layer: the same passes, over the same target,
    /// into a texture the CPU can read.
    ///
    /// This is what the runtime tests draw through (`crates/engine-metal/tests/rendered.rs`), so what they
    /// prove is what the window draws — the pipelines, the stencil, the blend and the unit contract are
    /// not tested twice, they are tested once on the path the app uses.
    pub fn offscreen(target: Target) -> Option<Self> {
        let (device, queue, pipelines) = make()?;

        Some(Self::bare(device, queue, pipelines, target, None))
    }

    /// Everything a device is, before it knows its layer.
    fn bare(
        device: Retained<ProtocolObject<dyn objc2_metal::MTLDevice>>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        pipelines: Pipelines,
        target: Target,
        layer: Option<Retained<CAMetalLayer>>,
    ) -> Self {
        Device {
            device,
            queue,
            target,
            pipelines,
            layer,
            msaa: None,
            mask: None,
            offscreen: None,
            last: None,
            stated: Statement::default(),
        }
    }

    /// The target this device draws into.
    pub fn target(&self) -> Target {
        self.target
    }

    /// Matches the drawable and the targets to a new target, dropping what was built for the old one.
    pub fn resize(&mut self, target: Target) {
        if self.target == target {
            return;
        }

        self.target = target;
        self.follow_target();

        self.msaa = None;
        self.mask = None;
        self.offscreen = None;
    }

    /// Tells the layer the density and the size the target says, and nothing about either itself.
    fn follow_target(&self) {
        let Some(layer) = &self.layer else {
            return;
        };

        let (width, height) = self.target.device_size();

        layer.setContentsScale(f64::from(self.target.scale_or_one()));
        layer.setDrawableSize(objc2_core_foundation::CGSize::new(
            f64::from(width.max(1)),
            f64::from(height.max(1)),
        ));
    }

    /// A corner of a triangle, in **device** pixels, in `colour`.
    ///
    /// The one place a logical point becomes a device one (`doc/ENGINE.md` §4.7): every caller hands in
    /// the window's own units and [`Target::device_point`] multiplies them by the display's density
    /// exactly once.
    fn corner(&self, point: (f32, f32), colour: u32) -> Vertex {
        let (x, y) = self.target.device_point(point);

        Vertex::new(x, y, colour)
    }

    /// The multisampled colour target and the stencil, made once per target size.
    ///
    /// Both are `type2DMultisample` with [`SAMPLES`] samples and the same pixel size, which is what a
    /// single pass needs of its attachments; the stencil's sample count has to match the colour's or the
    /// pass is refused.
    fn targets(&mut self, resolved: Retained<ProtocolObject<dyn MTLTexture>>) -> Option<Targets> {
        if let (Some(msaa), Some(mask)) = (&self.msaa, &self.mask) {
            return Some(Targets {
                colour: msaa.clone(),
                mask: mask.clone(),
                resolved,
            });
        }

        let (width, height) = self.target.device_size();
        let (width, height) = (width.max(1) as usize, height.max(1) as usize);

        let msaa = multisampled(&self.device, FORMAT, width, height)?;
        let mask = multisampled(&self.device, STENCIL_FORMAT, width, height)?;

        self.msaa = Some(msaa.clone());
        self.mask = Some(mask.clone());

        Some(Targets {
            colour: msaa,
            mask,
            resolved,
        })
    }

    /// Waits for the last frame to be paid for. The benchmark's door, and nothing else's.
    pub fn settle(&self) {
        if let Some(last) = &self.last {
            last.waitUntilCompleted();
        }
    }

    /// The layer the canvas is composed from, if this device has one.
    pub fn layer(&self) -> Option<&Retained<CAMetalLayer>> {
        self.layer.as_ref()
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

        if let Some(layer) = &self.layer {
            layer.setDisplaySyncEnabled(on);
        }
    }

    /// Encodes one frame into `drawable`, hands it over, and never waits for it.
    ///
    /// Two passes into the same multisampled target, resolved into the drawable once:
    ///
    /// 1. **the base** — the desk, the shadow's steps, the sheet and the ruling. Opaque rectangles over a
    ///    clear, so this pass needs no stencil.
    /// 2. **the ink** — every repair, then each band *twice*: once to mark its pieces into the stencil,
    ///    once to colour each marked sample. The pen's mark is last, because a cursor belongs above the
    ///    ink it is about to write.
    ///
    /// The resolve happens at the end of the base pass into the drawable, and the ink pass loads that
    /// same multisampled colour — so the second pass's resolve must land in the drawable too. Both
    /// passes therefore share one target and state the same `resolved` texture; what distinguishes them
    /// is `loadAction` and the stencil.
    fn encode(
        &mut self,
        base: &BaseLayer,
        plan: &Plan,
        mark: Option<&Mark>,
    ) -> Result<(), GpuError> {
        let waited = std::time::Instant::now();
        let drawable = self.layer.as_ref().and_then(|layer| layer.nextDrawable());
        let waited = waited.elapsed().as_millis() as u32;

        let Some(drawable) = drawable else {
            self.stated = Statement {
                presented: false,
                waited_ms: waited,
                ..Statement::default()
            };
            return Err(GpuError::new("no drawable was ready"));
        };

        self.encode_from(base, plan, mark, Some(&drawable), waited)
    }

    /// Encodes one frame into `drawable`, or into the target's offscreen texture when there is none.
    ///
    /// `resolved` is where the multisampled passes land. A window resolves into the drawable's own
    /// texture and is handed to the compositor; an offscreen frame resolves into a shared texture the CPU
    /// reads. The present is scheduled **before** the commit — a handler added after a buffer is committed
    /// is an assertion, not a frame.
    fn encode_from(
        &mut self,
        base: &BaseLayer,
        plan: &Plan,
        mark: Option<&Mark>,
        drawable: Option<&Retained<ProtocolObject<dyn CAMetalDrawable>>>,
        waited: u32,
    ) -> Result<(), GpuError> {
        let resolved = match drawable {
            Some(drawable) => drawable.texture(),
            None => {
                let (width, height) = self.target.device_size();
                let (width, height) = (width.max(1) as usize, height.max(1) as usize);

                if self.offscreen.is_none() {
                    self.offscreen = readable(&self.device, width, height);
                }

                self.offscreen
                    .clone()
                    .ok_or_else(|| GpuError::new("the offscreen texture could not be made"))?
            }
        };

        let Some(targets) = self.targets(resolved) else {
            return Err(GpuError::new("the render targets could not be made"));
        };

        // What the plan asks for: a repair is a rectangle whose base must be painted back, a band is its
        // triangles in the band's colour.
        let mut repairs: Vec<Rect> = Vec::new();
        let mut bands: Vec<Vec<Vertex>> = Vec::new();

        for step in plan.steps() {
            match step {
                Step::Repair(rect) if !rect.is_empty() => repairs.push(*rect),
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

        let base_corners = base_vertices(self.target, base);

        let Some(command) = self.queue.commandBuffer() else {
            return Err(GpuError::new("the command buffer could not be made"));
        };

        self.base_pass(&command, &targets, &base_corners);
        self.ink_pass(&command, &targets, base, &repairs, &bands, mark);

        // SAFETY: `presentDrawable:` is the buffer's own method for the drawable whose texture it just
        // resolved into, and it is called when the buffer is still being encoded, which is when a
        // scheduled handler is allowed.
        if let Some(drawable) = drawable {
            unsafe {
                let _: () = objc2::msg_send![&*command, presentDrawable: &**drawable];
            }
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

    /// The base: the desk and every fill, cleared first, resolved into the frame's texture.
    fn base_pass(
        &self,
        command: &ProtocolObject<dyn MTLCommandBuffer>,
        targets: &Targets,
        corners: &[Vertex],
    ) {
        let pass = MTLRenderPassDescriptor::new();

        // SAFETY: index 0 is the one colour attachment these pipelines were built for; the multisampled
        // texture is private and the resolve texture is single-sample of the same format and size, which
        // is what `MTLStoreAction::MultisampleResolve` requires.
        //
        // **`StoreAndMultisampleResolve`, not `MultisampleResolve`.** The first pass resolves the desk and
        // the sheet into the frame's texture *and keeps the multisampled contents* for the ink pass that
        // comes next; a plain resolve discards them, so the ink pass's `Load` finds nothing, paints its
        // strokes over black, and then resolves that black over the base it had just written. That is a
        // canvas with ink and no paper — and, with no ink at all, a canvas with nothing on it, which is
        // what this device used to draw.
        unsafe {
            let colour = pass.colorAttachments().objectAtIndexedSubscript(0);
            colour.setTexture(Some(&targets.colour));
            colour.setResolveTexture(Some(&targets.resolved));
            colour.setLoadAction(MTLLoadAction::Clear);
            colour.setStoreAction(MTLStoreAction::StoreAndMultisampleResolve);
        }

        let Some(encoder) = command.renderCommandEncoderWithDescriptor(&pass) else {
            trace("metal: the base pass was refused");
            return;
        };

        self.draw(&encoder, Stage::Solid, corners);

        encoder.endEncoding();
    }

    /// The ink: repairs, then each band marked and coloured once, then the pen's mark.
    ///
    /// The colour attachment **loads** what the base pass left, and resolves into the frame's texture at
    /// the end; the stencil is cleared here, so every frame starts unmarked.
    fn ink_pass(
        &self,
        command: &ProtocolObject<dyn MTLCommandBuffer>,
        targets: &Targets,
        base: &BaseLayer,
        repairs: &[Rect],
        bands: &[Vec<Vertex>],
        mark: Option<&Mark>,
    ) {
        let pass = MTLRenderPassDescriptor::new();

        // SAFETY: the colour target is loaded rather than cleared — the base pass put the desk and the
        // sheet there — and the stencil is the mask, cleared here so every frame starts unmarked. The
        // stencil is not kept: nothing reads it after this pass.
        unsafe {
            let colour = pass.colorAttachments().objectAtIndexedSubscript(0);
            colour.setTexture(Some(&targets.colour));
            colour.setResolveTexture(Some(&targets.resolved));
            colour.setLoadAction(MTLLoadAction::Load);
            colour.setStoreAction(MTLStoreAction::MultisampleResolve);

            let stencil = pass.stencilAttachment();
            stencil.setTexture(Some(&targets.mask));
            stencil.setLoadAction(MTLLoadAction::Clear);
            stencil.setStoreAction(MTLStoreAction::DontCare);
            stencil.setClearStencil(0);
        }

        let Some(encoder) = command.renderCommandEncoderWithDescriptor(&pass) else {
            trace("metal: the ink pass was refused");
            return;
        };

        // Repairs first: a repaired rectangle is a place the ink layer no longer holds anything.
        //
        // The base is *in* this same multisampled colour buffer — the base pass resolved it and then
        // stored the samples — so a repair cannot simply "leave the base alone": the ink it must remove is
        // already composited on top of the paper in that buffer. What a repair draws is therefore the
        // base's own pixels there: the sheet where the sheet is, the desk where the desk is, and each in
        // its own rectangle. `repaint_base` does that from the base layer's fills, in logical pixels.
        for rect in repairs {
            let mut corners = Vec::with_capacity(base.fills.len() * 6 + 6);
            repaint_base(&mut corners, self.target, base, *rect);
            self.draw(&encoder, Stage::Solid, &corners);
        }

        // Each band: marked, then coloured. Two draws a band, and one pass — the stencil is what makes
        // the pieces one region rather than a stack of them (E6).
        for corners in bands {
            self.draw(&encoder, Stage::Marking, corners);
            self.draw(&encoder, Stage::Coloured, corners);
        }

        if let Some(mark) = mark {
            let mut corners = Vec::with_capacity(18);
            mark_vertices(&mut corners, self.target, mark);
            self.draw(&encoder, Stage::Solid, &corners);
        }

        encoder.endEncoding();
    }

    /// One run of corners, in one draw call, through `stage`'s pipeline and stencil state.
    ///
    /// A buffer that cannot be made is *not* silently skipped: a frame with a band missing is a stroke
    /// that vanished, so it is logged and the draw does not happen — the same failure a refused pipeline
    /// would give, made visible instead of silent.
    fn draw(
        &self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        stage: Stage,
        vertices: &[Vertex],
    ) {
        if vertices.is_empty() {
            return;
        }

        let Some(buffer) = self.buffer(vertices) else {
            trace(&format!(
                "metal: a buffer for {} corners could not be made",
                vertices.len()
            ));
            return;
        };

        let (pipeline, state) = match stage {
            Stage::Solid => (&self.pipelines.solid, &self.pipelines.solid_state),
            Stage::Marking => (&self.pipelines.marking, &self.pipelines.marking_state),
            Stage::Coloured => (&self.pipelines.coloured, &self.pipelines.coloured_state),
        };

        encoder.setRenderPipelineState(pipeline);
        encoder.setDepthStencilState(Some(state));
        // The marking pass writes 1, and the colouring pass compares against 1: one reference for both.
        encoder.setStencilReferenceValue(1);

        // The viewport is the target's device size, in device pixels — the same units the shader places.
        let (width, height) = self.target.device_size();
        let viewport: [f32; 2] = [width.max(1) as f32, height.max(1) as f32];

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

    /// Uploads a run of corners into a shared buffer.
    fn buffer(
        &self,
        vertices: &[Vertex],
    ) -> Option<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>> {
        use objc2_metal::MTLBuffer;

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
}

/// The base layer as triangles: the desk, then every fill, in the order painted.
///
/// The fills arrive in logical pixels and [`Device::corner`] is the one place they are densified. A fill
/// with no area is not a fill — a sheet scrolled off the surface, a window narrower than its margin.
fn base_vertices(target: Target, base: &BaseLayer) -> Vec<Vertex> {
    let mut vertices = Vec::with_capacity(6 + base.fills.len() * 6);

    quad(
        &mut vertices,
        target,
        (target.width, target.height),
        (0.0, 0.0),
        base.desk.unwrap_or(0xFFFF_FFFF),
    );

    for fill in &base.fills {
        let rect = fill.rect;
        if rect.width <= 0.0 || rect.height <= 0.0 {
            continue;
        }

        quad(
            &mut vertices,
            target,
            (rect.x + rect.width, rect.y + rect.height),
            (rect.x, rect.y),
            fill.colour,
        );
    }

    vertices
}

/// The base's own pixels inside a repaired rectangle: the desk, then each fill clipped to it.
///
/// A repair removes ink, and the ink sits in the same colour buffer as the paper, so removing it means
/// painting the paper back. The pixels come from the base layer itself rather than from a colour of the
/// device's choosing, which is the difference between an erased stroke that leaves the sheet looking like
/// the sheet and one that leaves a grey block where it was.
///
/// Everything here is clipped to `repair`: a repair that crosses the sheet's edge gets the sheet inside it
/// and the desk outside it, which is what the base pass drew there.
fn repaint_base(vertices: &mut Vec<Vertex>, target: Target, base: &BaseLayer, repair: Rect) {
    // The desk first, over the whole screen, clipped to the repair.
    let desk = Rect {
        x: 0.0,
        y: 0.0,
        width: target.width,
        height: target.height,
    };

    if let Some((right_bottom, left_top)) = overlap(desk, repair) {
        quad(
            vertices,
            target,
            right_bottom,
            left_top,
            base.desk.unwrap_or(0xFFFF_FFFF),
        );
    }

    for fill in &base.fills {
        if let Some((right_bottom, left_top)) = overlap(fill.rect, repair) {
            quad(vertices, target, right_bottom, left_top, fill.colour);
        }
    }
}

/// The far and near corners of two rectangles' overlap, or `None` if they do not meet.
fn overlap(a: Rect, b: Rect) -> Option<((f32, f32), (f32, f32))> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);

    if right <= left || bottom <= top {
        return None;
    }

    Some(((right, bottom), (left, top)))
}

///
/// `(right, bottom)` is the far corner and `(x, y)` the near one, so a rectangle is four logical numbers
/// and the density is applied here and nowhere else.
fn quad(
    vertices: &mut Vec<Vertex>,
    target: Target,
    (right, bottom): (f32, f32),
    (x, y): (f32, f32),
    colour: u32,
) {
    let corner = |(x, y): (f32, f32)| {
        let (x, y) = target.device_point((x, y));

        Vertex::new(x, y, colour)
    };

    vertices.extend_from_slice(&[
        corner((x, y)),
        corner((right, y)),
        corner((x, bottom)),
        corner((x, bottom)),
        corner((right, y)),
        corner((right, bottom)),
    ]);
}

/// The pen's mark: a bloom round the nib, the nib's own dot, and the rod its lean draws.
///
/// The engine hands the half *where* and *how thick*; the view owns what a cursor looks like
/// (`note_ui::cursor`, platform-free) and this draws what it is told. Everything here is logical pixels
/// and goes through [`quad`] and [`disc`], which densify once.
fn mark_vertices(vertices: &mut Vec<Vertex>, target: Target, mark: &Mark) {
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
        target,
        mark.at,
        radius * 1.35 * unit,
        mark.halo,
        0.35,
    );
    disc(vertices, target, mark.at, radius, mark.colour, 1.0);

    // The body's rod: a thin quad leaning away from the nib, faint while the pen hovers.
    let lean = if mark.lean.is_finite() {
        mark.lean.clamp(0.0, 1.0)
    } else {
        0.0
    };

    let length = radius * 6.0 * unit;
    let width = radius * 0.7 * unit;

    if length <= 0.0 || width <= 0.0 {
        return;
    }

    // Leaning right and down, the way a pen held in a right hand leans over the paper it touches.
    let (tip, tail) = (
        mark.at,
        (mark.at.0 + length * lean, mark.at.1 + length * lean),
    );
    let (dx, dy) = (tail.0 - tip.0, tail.1 - tip.1);
    let extent = (dx * dx + dy * dy).sqrt();

    if extent < 1e-4 {
        return;
    }

    // The rod's normal, in logical pixels.
    let (nx, ny) = (-dy / extent * width * 0.5, dx / extent * width * 0.5);
    let colour = with_alpha(mark.colour, 0.55 * (1.0 - lean * 0.5));

    quad(
        vertices,
        target,
        (tail.0 + nx, tail.1 + ny),
        (tip.0 + nx, tip.1 + ny),
        colour,
    );

    quad(
        vertices,
        target,
        (tail.0 - nx, tail.1 - ny),
        (tip.0 - nx, tip.1 - ny),
        colour,
    );
}

/// The shaders: place a corner, and colour it.
///
/// The engine hands the half its triangles *already placed* — the placement is applied when a band is
/// built (`cheap_note_engine::place::Placement`) — so the shader only has to turn a window pixel into
/// clip space. That is one multiplication fewer per corner than the old canvas did.
///
/// `Vertex` is twelve bytes with no packing attribute, because a `uchar4` is four-byte aligned in Metal
/// exactly as it is in Rust: the two structs are the same bytes and nothing has to be padded. The
/// shader's `float4(vertices[id].colour)` is an *unnormalised* conversion of those bytes, so the colour
/// is divided by 255 here rather than by `unorm` in the format — the ink's alpha is part of its colour
/// and nothing else has to know.
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

/// A corner of one triangle: where, in device pixels, and the colour it carries.
///
/// Twelve bytes, and the layout is the shader's own `Vertex` above — `position` then `colour`, with the
/// colour as four bytes rather than four floats because a page of writing is millions of corners and
/// every corner of one stroke carries the same colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
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

/// A filled circle as a fan of triangles round its centre, in logical pixels.
///
/// Forty-eight wedges: at a cursor's six logical pixels that is under a pixel of chord, which reads as a
/// circle, and it is one draw with the rest of the mark.
fn disc(
    vertices: &mut Vec<Vertex>,
    target: Target,
    at: (f32, f32),
    radius: f32,
    colour: u32,
    coverage: f32,
) {
    if radius <= 0.0 {
        return;
    }

    const WEDGES: usize = 48;

    let colour = with_alpha(colour, coverage);
    let corner = |(x, y): (f32, f32)| {
        let (x, y) = target.device_point((x, y));
        Vertex::new(x, y, colour)
    };

    let centre = corner(at);

    for step in 0..WEDGES {
        let from = std::f32::consts::TAU * step as f32 / WEDGES as f32;
        let to = std::f32::consts::TAU * (step + 1) as f32 / WEDGES as f32;

        vertices.push(centre);
        vertices.push(corner((
            at.0 + radius * from.cos(),
            at.1 + radius * from.sin(),
        )));
        vertices.push(corner((at.0 + radius * to.cos(), at.1 + radius * to.sin())));
    }
}

/// A colour at a fraction of the alpha it was given: what a halo and a rod are painted with.
fn with_alpha(colour: u32, coverage: f32) -> u32 {
    let alpha = ((colour >> 24) & 0xFF) as f32;
    let alpha = if alpha == 0.0 { 255.0 } else { alpha };
    let alpha = (alpha * coverage.clamp(0.0, 1.0)).round() as u32;

    (alpha << 24) | (colour & 0x00FF_FFFF)
}

/// Which of the three pipelines a pass uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Opaque or translucent colour over what is there: the base, a repair, the pen's mark.
    Solid,
    /// Writes 1 into the stencil and colours nothing: a band's pieces, marked.
    Marking,
    /// Colour only where the stencil is 1: a band's one fill (E6).
    Coloured,
}

/// A pipeline that does one of the three.
///
/// Every pipeline states its colour attachment's format and [`SAMPLES`] samples, and the two that use a
/// stencil state its format too — all of which a pass checks before it encodes. Blending is source-over
/// for the two that write colour (the same arithmetic Core Graphics did, so a highlighter over a page
/// looks the way it looked before) and off for the marking pass, which writes no colour at all.
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

    // The attachment is set through its **typed** methods, never through `msg_send!`: a selector's
    // argument type is checked by nothing at run time, and `setWriteMask:` takes an `NSUInteger`. A `u32`
    // there is a wrong-width integer and Metal aborts the process on the call — inside pipeline
    // creation, before a single frame exists.
    let attachment = &*attachment;
    attachment.setPixelFormat(FORMAT);

    if stage == Stage::Marking {
        // Nothing to colour: the marking pass exists to write the stencil.
        attachment.setWriteMask(MTLColorWriteMask::None);
    } else {
        use objc2_metal::{MTLBlendFactor, MTLBlendOperation};

        attachment.setBlendingEnabled(true);
        attachment.setRgbBlendOperation(MTLBlendOperation::Add);
        attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    }

    // A stencil format is stated for the two passes that render with a stencil, and left `Invalid` for
    // the base, which has none: a pipeline whose stencil format disagrees with its pass is refused, and a
    // refused pipeline is a canvas that draws nothing.
    if matches!(stage, Stage::Marking | Stage::Coloured) {
        descriptor.setStencilAttachmentPixelFormat(STENCIL_FORMAT);
    }

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .ok()
}

/// The stencil state a pass binds: what "is a place marked" means for this pass.
///
/// `MTLRenderPipelineDescriptor` has no stencil state of its own — it lives in an `MTLDepthStencilState`
/// a pass binds, which is how two passes can share one pair of shaders and differ in what the stencil
/// means.
fn depth_stencil(
    device: &ProtocolObject<dyn objc2_metal::MTLDevice>,
    stage: Stage,
) -> Option<Retained<ProtocolObject<dyn objc2_metal::MTLDepthStencilState>>> {
    use objc2_metal::MTLDepthStencilDescriptor;

    let descriptor = MTLDepthStencilDescriptor::new();
    // No depth at all: every pass here draws into one plane.
    descriptor.setDepthCompareFunction(MTLCompareFunction::Always);
    descriptor.setDepthWriteEnabled(false);

    let face = objc2_metal::MTLStencilDescriptor::new();

    match stage {
        Stage::Marking => {
            // Every fragment that reaches the stencil writes the reference (1). The comparison is
            // `Always`, so the write never depends on what was already marked, and all three outcomes
            // replace — a sample cannot be left unmarked by a test nobody runs.
            face.setStencilCompareFunction(MTLCompareFunction::Always);
            face.setStencilFailureOperation(MTLStencilOperation::Replace);
            face.setDepthFailureOperation(MTLStencilOperation::Replace);
            face.setDepthStencilPassOperation(MTLStencilOperation::Replace);
            face.setReadMask(0xFF);
            face.setWriteMask(0xFF);
        }
        Stage::Coloured => {
            // Colour only where the marking pass wrote 1, and change nothing: `Equal` against the same
            // reference, with no operation that could clear a bit already set.
            face.setStencilCompareFunction(MTLCompareFunction::Equal);
            face.setStencilFailureOperation(MTLStencilOperation::Keep);
            face.setDepthFailureOperation(MTLStencilOperation::Keep);
            face.setDepthStencilPassOperation(MTLStencilOperation::Keep);
            face.setReadMask(0xFF);
            face.setWriteMask(0x00);
        }
        Stage::Solid => {
            // No stencil test at all: the base, a repair and the pen's mark are drawn wherever they are.
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

/// A multisampled render target of `format`, at the given device size.
///
/// `type2DMultisample` with [`SAMPLES`] samples, private and renderable: `rasterSampleCount` needs its
/// render targets to be multisampled textures of that count, and a pass needs its stencil and colour
/// attachments to agree on both.
fn multisampled(
    device: &ProtocolObject<dyn objc2_metal::MTLDevice>,
    format: MTLPixelFormat,
    width: usize,
    height: usize,
) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
    // SAFETY: a 2D descriptor for the size this target draws at; it is only read to make a texture.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            format, width, height, false,
        )
    };

    descriptor.setTextureType(MTLTextureType::Type2DMultisample);
    // SAFETY: the count was checked against this GPU in `make`.
    unsafe {
        descriptor.setSampleCount(SAMPLES);
    }
    descriptor.setStorageMode(MTLStorageMode::Private);
    descriptor.setUsage(MTLTextureUsage::RenderTarget);

    device.newTextureWithDescriptor(&descriptor)
}

/// A single-sample colour target the CPU can read: the offscreen path's resolve destination.
///
/// `shared`, so reading it back needs no blit. A window resolves into its drawable's texture instead,
/// which is the compositor's to read.
fn readable(
    device: &ProtocolObject<dyn objc2_metal::MTLDevice>,
    width: usize,
    height: usize,
) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
    // SAFETY: a 2D descriptor for the size this target draws at; it is only read to make a texture.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            FORMAT, width, height, false,
        )
    };

    descriptor.setStorageMode(MTLStorageMode::Shared);
    descriptor.setUsage(MTLTextureUsage::RenderTarget);

    device.newTextureWithDescriptor(&descriptor)
}

/// A frame the device drew offscreen, read back: what the runtime tests judge.
///
/// The pixels are `BGRA8Unorm` in metal's own order, read once and kept — a `Shot` does not hold the
/// texture, so a test may draw again before it looks.
pub struct Shot {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Shot {
    /// The frame's size in device pixels.
    pub fn device_width(&self) -> u32 {
        self.width
    }

    /// The frame's height in device pixels.
    pub fn device_height(&self) -> u32 {
        self.height
    }

    /// The colour at a device pixel, as `(r, g, b)`. A pixel outside the frame is transparent black.
    pub fn pixel(&self, x: u32, y: u32) -> (u8, u8, u8) {
        if x >= self.width || y >= self.height {
            return (0, 0, 0);
        }

        let at = ((y as usize) * (self.width as usize) + x as usize) * 4;
        let pixel = &self.pixels[at..at + 4];

        (pixel[2], pixel[1], pixel[0])
    }

    /// The alpha at a device pixel, from 0 to 255.
    pub fn alpha(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }

        let at = ((y as usize) * (self.width as usize) + x as usize) * 4;

        self.pixels[at + 3]
    }

    /// Every colour in the frame, most common first: what a test reads to see the whole picture at once.
    pub fn colours(&self) -> Vec<((u8, u8, u8), usize)> {
        let mut colours: Vec<((u8, u8, u8), usize)> = Vec::new();

        for pixel in self.pixels.as_chunks::<4>().0 {
            let colour = (pixel[2], pixel[1], pixel[0]);
            match colours.iter_mut().find(|(seen, _)| *seen == colour) {
                Some((_, count)) => *count += 1,
                None => colours.push((colour, 1)),
            }
        }

        colours.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        colours
    }

    /// Reads the pixels of a texture the GPU has finished with.
    fn of(texture: &ProtocolObject<dyn MTLTexture>) -> Option<Self> {
        let (width, height) = (texture.width() as u32, texture.height() as u32);
        let mut pixels = vec![0u8; width as usize * height as usize * 4];

        // SAFETY: the buffer is exactly `bytesPerRow * height` bytes and the texture agrees with the
        // target it was made for. The frame this reads was waited for by the caller.
        unsafe {
            let _: () = objc2::msg_send![
                texture,
                getBytes: pixels.as_mut_ptr().cast::<c_void>(),
                bytesPerRow: width as usize * 4,
                fromRegion: objc2_metal::MTLRegion {
                    origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
                    size: objc2_metal::MTLSize {
                        width: width as usize,
                        height: height as usize,
                        depth: 1,
                    },
                },
                mipmapLevel: 0u64
            ];
        }

        Some(Shot {
            width,
            height,
            pixels,
        })
    }
}

impl Device {
    /// Draws one frame offscreen and reads it back: base, ink and mark, in that order, waited for.
    ///
    /// This is the runtime test's whole surface. It draws through **the same `encode_from` the window
    /// uses**, so a pipeline, stencil or blend that is wrong here is wrong on the screen — there is no
    /// second implementation to disagree with.
    ///
    /// A device made by [`Device::new`] has a layer and no offscreen texture; this is the offscreen
    /// device's, so it says so rather than drawing into a window nobody asked it to touch.
    pub fn draw_for_test(&mut self, base: &BaseLayer, plan: &Plan, mark: Option<&Mark>) -> Shot {
        self.encode_from(base, plan, mark, None, 0)
            .expect("an offscreen frame is encoded without a drawable");

        self.settle();

        let resolved = self
            .offscreen
            .clone()
            .expect("an offscreen frame made a texture to read");

        Shot::of(&resolved).expect("the frame it just drew can be read")
    }
}

impl Gpu for Device {
    fn size(&self) -> (u32, u32) {
        self.target.device_size()
    }

    fn drawable(&self) -> bool {
        // Asking for a drawable *waits* when the compositor holds all three (`doc/MACOS.md` §8's `warm`),
        // which is the one wait a reading cannot afford — so the check is whether the layer exists to ask,
        // not a blocking request. The encode takes one, once.
        self.layer.is_some()
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
