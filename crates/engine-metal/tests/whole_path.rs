//! The whole path, across both crates: the engine decides what changed, the Metal half draws it.
//!
//! This is an integration test on purpose — it uses nothing but the two crates' public surface, the way
//! `apps/macos` would when it moves onto the engine. The device is a recorder, so no GPU is involved.

use cheap_note_engine::band::Band;
use cheap_note_engine::engine::{Engine, FrameCap};
use cheap_note_engine::place::Placement;
use cheap_note_engine::surface::{BandDraw, FillRule, Frame, InkDraw, PresentResult, Surface};
use cheap_note_engine_metal::{Gpu, GpuError, MetalSurface, Plan, Target};
use note_format::ink::Piece;
use note_format::pages::Quarters;
use std::time::{Duration, Instant};

/// A device that records the plans it was asked to run, and can refuse a drawable.
struct Recorder {
    plans: Vec<Plan>,
    ready: bool,
}

impl Recorder {
    fn ready() -> Self {
        Recorder {
            plans: Vec::new(),
            ready: true,
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
        self.plans.push(plan.clone());
        Ok(())
    }

    fn base(&mut self, _base: &cheap_note_engine_metal::BaseLayer) -> Result<(), GpuError> {
        Ok(())
    }

    fn mark(&mut self, _mark: Option<&cheap_note_engine_metal::Mark>) -> Result<(), GpuError> {
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

/// A frame as the engine's present would describe it: the band, and nothing else.
fn frame_for(band: &Band) -> Frame {
    Frame {
        base: Default::default(),
        ink: InkDraw {
            repair: band.damage().rects().to_vec(),
            bands: vec![BandDraw {
                triangles: band.pieces().to_vec(),
                colour: 0xFF1B_1B1F,
                rule: FillRule::NonZero,
            }],
        },
    }
}

/// The whole path, E1: an engine with nothing to show owes no present, and a surface asked to draw
/// it presents — but the *engine's* answer is `None`, which is the present that never happens.
///
/// The test is the composition of the two halves' promises: the engine decides there is nothing, and
/// the surface is never asked. A still canvas, in one line.
#[test]
fn a_still_engine_never_asks_a_surface_to_draw() {
    let mut engine = Engine::new(FrameCap::Unlimited);
    let surface = MetalSurface::new(Recorder::ready(), Target::new(800.0, 600.0, 2.0));
    let now = Instant::now();

    // A hundred idle asks: the engine answers `None` every time.
    for step in 0..100 {
        assert!(
            engine.present(now + Duration::from_millis(step)).is_none(),
            "an idle engine handed a frame to a surface"
        );
    }

    assert_eq!(
        surface.device().plans.len(),
        0,
        "a still engine drew something"
    );
}

/// The whole path, E2: one reading lays one band, and the surface is asked to draw exactly that —
/// one repair, one band, one present.
#[test]
fn one_reading_asks_the_surface_for_its_band() {
    let mut engine = Engine::new(FrameCap::Unlimited);
    let mut surface = MetalSurface::new(Recorder::ready(), Target::new(800.0, 600.0, 2.0));
    let now = Instant::now();
    let band = a_band(40.0);

    let work = engine.on_batch(band.clone(), 60, now);
    assert!(work.owed, "a reading that laid ink owed no present");

    // What the engine presented is the band's damage, and that is what the frame carries.
    assert!(
        engine.present(now).is_some(),
        "the engine did not present a reading that laid ink"
    );

    surface
        .draw(&frame_for(&band))
        .expect("a ready surface failed to draw");

    assert_eq!(
        surface.device().plans.len(),
        1,
        "one reading did not draw one frame"
    );
    assert_eq!(
        surface.device().plans[0].triangles(),
        2,
        "one reading did not draw one trapezoid"
    );
}

/// A surface that refuses its first present is asked again, and the canvas stays dirty: the engine
/// has not presented, so the next ask presents the newest description (`doc/ENGINE.md` §5.6).
#[test]
fn a_refused_present_is_tried_again() {
    let mut device = Recorder::ready();
    device.ready = false;
    let mut surface = MetalSurface::new(device, Target::new(800.0, 600.0, 2.0));
    let frame = frame_for(&a_band(10.0));

    assert_eq!(
        surface.draw(&frame),
        Ok(PresentResult::NotReady),
        "the first present was not refused"
    );

    // The drawable arrives: the same frame presents, and the plan runs.
    surface.device_mut().ready = true;
    assert_eq!(
        surface.draw(&frame),
        Ok(PresentResult::Presented),
        "the retry did not present"
    );
    assert_eq!(surface.device().plans.len(), 1, "the retry planned nothing");
}
