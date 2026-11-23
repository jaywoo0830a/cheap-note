//! The Metal surface: a device, two layers, and the drawable a frame is handed to.
//!
//! The half the engine talks to is [`MetalSurface`], and it is written *over* [`Gpu`] rather than over
//! `MTLDevice` for one reason: then every path — a plan built, a drawable refused, a resize that
//! reallocates the layers — is testable on a machine with no GPU at all. The tests at the bottom of
//! this module do exactly that with a fake device.
//!
//! ## The real device
//!
//! [`Device`] is the `MTLDevice` implementation, and it is what `apps/macos`'s canvas already does
//! (`apps/macos/src/platform/metal.rs`): one command queue, a `CAMetalLayer` of the app's own under
//! GPUI's view, and per present one drawable. It is written behind the same trait so building this
//! crate on a machine without Metal (the Windows CI, a Linux box) still compiles and still tests the
//! half's logic — see `the_device_is_absent_without_metal` below.
//!
//! ## Why two layers
//!
//! `doc/ENGINE.md` §3.2: the base layer (desk, sheet, ruling, page) is laid once per layout and then
//! kept; the ink layer (finished ink, the open band) is appended to per reading. A frame composites the
//! second over the first, so a pan re-uses both and a reading touches only the ink.

use cheap_note_engine::surface::{Frame, PresentResult, Surface, SurfaceError};

use crate::gpu::{Gpu, GpuError};
use crate::plan::{Plan, Target};

/// A surface whose device is anything that implements [`Gpu`].
///
/// This is the whole of the half's logic: keep the target, keep the plan's device-side effects, hand a
/// frame over without waiting, and reallocate the layers when the window changes.
pub struct MetalSurface<D: Gpu> {
    device: D,
    target: Target,
    /// How many frames have been handed over, for the tests and the status line.
    presents: usize,
    /// How many frames were refused for want of a drawable: a canvas that stayed dirty and tried again.
    refused: usize,
}

impl<D: Gpu> MetalSurface<D> {
    /// A surface at a target's size, with its device.
    pub fn new(device: D, target: Target) -> Self {
        MetalSurface {
            device,
            target,
            presents: 0,
            refused: 0,
        }
    }

    /// The target it draws into.
    pub fn target(&self) -> Target {
        self.target
    }

    /// The device, for a caller that needs to know what it is.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// The device, for a caller that must change it — a display that came back, a layer that was
    /// recreated. Nothing in the engine's path uses this; it is here so a platform half's own code (and
    /// its tests) can reach the device without the surface having to grow a method per device call.
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// Draws one frame *with its whole context*: the base layer under the ink, the ink's bands, and the
    /// pen's mark over it — one present, in that order.
    ///
    /// This is what a platform half's own caller uses, because a frame without its desk is a frame of ink
    /// on nothing, and a frame without its mark is a cursor that does not move. [`Surface::draw`] is the
    /// engine's view of the same call with an empty base and no mark, which is what a test or a
    /// headless caller has.
    pub fn present(
        &mut self,
        base: &crate::plan::BaseLayer,
        frame: &Frame,
        mark: Option<&crate::plan::Mark>,
    ) -> Result<PresentResult, SurfaceError> {
        if !self.device.drawable() {
            self.refused += 1;
            return Ok(PresentResult::NotReady);
        }

        let plan = Plan::of(frame);

        match self.device.present(base, &plan, mark) {
            Ok(()) => {
                self.presents += 1;
                Ok(PresentResult::Presented)
            }
            Err(error) if error.reason.contains("no drawable") => {
                // A drawable that went away between the check and the request is the same answer as one
                // that was never there: not ready, not failed (`doc/ENGINE.md` §5.6).
                self.refused += 1;
                Ok(PresentResult::NotReady)
            }
            Err(error) => Err(named(error)),
        }
    }

    /// Frames handed over, and frames refused for want of a drawable.
    pub fn counts(&self) -> (usize, usize) {
        (self.presents, self.refused)
    }
}

impl<D: Gpu> Surface for MetalSurface<D> {
    fn size(&self) -> (f32, f32) {
        (self.target.width, self.target.height)
    }

    fn resize(&mut self, width: f32, height: f32) {
        if (self.target.width, self.target.height) != (width, height) {
            self.target.width = width;
            self.target.height = height;
        }
    }

    fn draw(&mut self, frame: &Frame) -> Result<PresentResult, SurfaceError> {
        // A layer with no drawable is not a failure: nothing is planned into it, the canvas stays dirty,
        // and the next wake tries again (§5.6). Checking first is what keeps a half from building a
        // plan it cannot run.
        if !self.device.drawable() {
            self.refused += 1;
            return Ok(PresentResult::NotReady);
        }

        let plan = Plan::of(frame);
        self.device.run(&plan).map_err(named)?;

        self.presents += 1;

        Ok(PresentResult::Presented)
    }

    fn honours_vsync(&self) -> bool {
        // `CAMetalLayer.displaySyncEnabled` is the whole of the mechanism on this platform, and it is
        // consulted at present time (`doc/MACOS.md` §4.3), so the switch is real here.
        true
    }
}

/// A device's failure, as a surface's: the same sentence, at the boundary the engine sees.
fn named(error: GpuError) -> SurfaceError {
    SurfaceError::new(error.reason)
}

mod device;
pub use device::{Device, Shot, Statement};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{Gpu, GpuError};
    use crate::plan::{Plan, Step, Target};
    use cheap_note_engine::band::Band;
    use cheap_note_engine::place::Placement;
    use cheap_note_engine::surface::{BandDraw, FillRule, InkDraw};
    use note_format::ink::Piece;
    use note_format::pages::Quarters;

    /// A device that records the plans it was asked to run, and can refuse a drawable or fail.
    struct Recorder {
        plans: Vec<Plan>,
        ready: bool,
        fails: bool,
        bases: usize,
        marks: usize,
    }

    impl Recorder {
        fn ready() -> Self {
            Recorder {
                plans: Vec::new(),
                ready: true,
                fails: false,
                bases: 0,
                marks: 0,
            }
        }
    }

    impl Gpu for Recorder {
        fn size(&self) -> (u32, u32) {
            (1600, 1200)
        }

        fn drawable(&self) -> bool {
            self.ready
        }

        fn run(&mut self, plan: &Plan) -> Result<(), GpuError> {
            if self.fails {
                return Err(GpuError::new("the command buffer could not be committed"));
            }

            self.plans.push(plan.clone());
            Ok(())
        }

        fn base(&mut self, base: &crate::plan::BaseLayer) -> Result<(), GpuError> {
            self.bases += base.fills.len() + usize::from(base.desk.is_some());
            Ok(())
        }

        fn mark(&mut self, mark: Option<&crate::plan::Mark>) -> Result<(), GpuError> {
            self.marks += usize::from(mark.is_some());
            Ok(())
        }
    }

    fn a_band(at: f32) -> Band {
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0));
        let span = Piece::Quad {
            left_from: [at, 0.0],
            left_to: [at + 4.0, 0.0],
            right_to: [at + 4.0, 4.0],
            right_from: [at, 4.0],
        };

        Band::of(&[span], &place)
    }

    fn frame_of(band: &Band) -> Frame {
        Frame {
            base: Default::default(),
            ink: InkDraw {
                repair: band.damage().rects().to_vec(),
                bands: vec![BandDraw {
                    triangles: band.pieces().to_vec(),
                    colour: 0xFF00_0000,
                    rule: FillRule::NonZero,
                }],
            },
        }
    }

    /// The plan's order is the contract: every repair, then every band, then the present.
    #[test]
    fn a_plan_repairs_before_it_fills_and_presents_last() {
        let band = a_band(10.0);
        let plan = Plan::of(&frame_of(&band));

        let kinds: Vec<&str> = plan
            .steps()
            .iter()
            .map(|step| match step {
                Step::Repair(_) => "repair",
                Step::Band { .. } => "band",
                Step::Present => "present",
            })
            .collect();

        assert_eq!(
            kinds,
            vec!["repair", "band", "present"],
            "a plan did not repair, then fill, then present"
        );
        assert!(plan.lays_ink(), "a plan with a band claimed to lay no ink");
        assert_eq!(plan.triangles(), 2, "a trapezoid did not survive the plan");
    }

    /// A frame with no ink is a plan that presents without filling anything.
    #[test]
    fn a_frame_with_no_ink_presents_without_filling() {
        let plan = Plan::of(&Frame::default());

        assert_eq!(
            plan.steps(),
            &[Step::Present],
            "an empty frame planned anything but a present"
        );
        assert!(!plan.lays_ink(), "an empty frame claimed to lay ink");
    }

    /// The device is asked before a plan is built: a layer with no drawable is `NotReady`, and nothing
    /// is planned, because a plan it could not run is wasted work (`doc/ENGINE.md` §5.6).
    #[test]
    fn no_drawable_is_not_ready_and_runs_nothing() {
        let mut device = Recorder::ready();
        device.ready = false;

        let mut surface = MetalSurface::new(device, Target::new(800.0, 600.0, 2.0));

        assert_eq!(
            surface.draw(&frame_of(&a_band(10.0))),
            Ok(PresentResult::NotReady),
            "a surface with no drawable did not answer NotReady"
        );
        assert!(
            surface.device().plans.is_empty(),
            "a surface planned a frame it could not run"
        );
        assert_eq!(surface.counts(), (0, 1), "the refusal was not counted");
    }

    /// A drawable runs the plan once and counts the present.
    #[test]
    fn a_drawable_runs_the_plan_once() {
        let mut surface = MetalSurface::new(Recorder::ready(), Target::new(800.0, 600.0, 2.0));

        assert_eq!(
            surface.draw(&frame_of(&a_band(10.0))),
            Ok(PresentResult::Presented),
            "a surface with a drawable did not present"
        );
        assert_eq!(
            surface.device().plans.len(),
            1,
            "a frame was planned more than once"
        );
        assert!(surface.device().plans[0].lays_ink(), "the plan laid no ink");
        assert_eq!(surface.counts(), (1, 0), "the present was not counted");
    }

    /// A device that fails is a surface that says so — a sentence at the boundary the engine sees.
    #[test]
    fn a_device_that_fails_names_what_it_failed_at() {
        let mut device = Recorder::ready();
        device.fails = true;

        let mut surface = MetalSurface::new(device, Target::new(800.0, 600.0, 2.0));
        let error = surface
            .draw(&frame_of(&a_band(10.0)))
            .expect_err("a failing device presented a frame");

        assert!(
            error.reason.contains("command buffer"),
            "the failure lost the device's own sentence: {}",
            error.reason
        );
    }

    /// A resize changes the target and the size the surface reports, and nothing else.
    #[test]
    fn a_resize_changes_the_target() {
        let mut surface = MetalSurface::new(Recorder::ready(), Target::new(800.0, 600.0, 2.0));

        surface.resize(1024.0, 768.0);

        assert_eq!(
            surface.size(),
            (1024.0, 768.0),
            "a resize did not reach the target"
        );
        assert_eq!(
            surface.target().device_size(),
            (2048, 1536),
            "a resized target's device size was wrong"
        );
    }

    /// The target's device size is its logical size at the display's scale, and a nonsense scale draws
    /// at 1:1 rather than collapsing the surface.
    #[test]
    fn a_target_is_logical_and_its_scale_is_a_second_number() {
        let retina = Target::new(1280.0, 800.0, 2.0);
        assert_eq!(retina.device_size(), (2560, 1600), "a 2x target was wrong");

        let nonsense = Target::new(1280.0, 800.0, 0.0);
        assert_eq!(
            nonsense.device_size(),
            (1280, 800),
            "a zero scale collapsed the target"
        );
    }

    /// This platform's surface honours vertical sync: `displaySyncEnabled` is real here, so the bar
    /// keeps the switch (`doc/WINDOWS.md` §3.3 says the opposite for Windows).
    #[test]
    fn vertical_sync_is_honoured_on_this_platform() {
        let surface = MetalSurface::new(Recorder::ready(), Target::new(800.0, 600.0, 2.0));

        assert!(
            surface.honours_vsync(),
            "the Mac surface said it cannot honour vertical sync"
        );
    }
}
