//! What a rebuild costs, in joules: the macOS half of `cheap-note --power-report`.
//!
//! The arithmetic, the arguments and the verdict are the view's (see `note_ui::power_report`). What this
//! module lends is the two things only the app has: the process's CPU time, and — for the `frame` phase —
//! the Metal surface the window is drawn on. The same offscreen `NSView` the bench draws into is reused,
//! so a `frame` row is the bench's `frame` figure with an energy window around it.
//!
//! ## Energy on macOS
//!
//! There is no user-mode package-energy counter the report may read. `powermetrics` needs root and
//! reports watts over an interval, not a joule counter, so the platform source is `None` here — absent,
//! never a zero. A run that wants joules reads a meter (`--meter`); a run that does not gets `wall` and
//! `cpu`, and `basis: "cpu-time"` says the second is a proxy for work, not power.

use std::cell::RefCell;

use note_ui::ink_layer::canvas::Canvas;
use note_ui::power_report::{self, CpuTime, Harness, PhaseKind, Plan};

use crate::platform::metal::Surface;
use crate::platform::vitals::ProcessStats;

/// The canvas the `frame` phase draws into, in device pixels — the bench's own size.
const CANVAS: (usize, usize) = (1_280, 900);

/// Runs the plan, writing the report to `out` and returning its exit code.
///
/// The CPU clock is `sysinfo`'s accumulated CPU time; the frame phase draws only when `--workload` names
/// it, because a `rung`-only run has no use for a Metal device.
pub fn run(plan: &Plan, out: &mut impl std::io::Write) -> i32 {
    // The CPU reader, hung off the process's own counters. `sysinfo`'s `accumulated_cpu_time` is the
    // reading; a build that cannot name its own process answers `None` and the row says `cpu` is absent.
    let cpu_state = RefCell::new(ProcessStats::install());
    let cpu = move || -> Option<CpuTime> { cpu_state.borrow_mut().as_mut()?.cpu_time() };

    // The surface, only when the frame phase will use it. `None` here means the phase reports
    // `no-surface` rather than timing the model alone and calling it a frame.
    let wants_frame = plan.workload.includes(PhaseKind::Frame);
    let mut surface = if wants_frame {
        offscreen_surface()
    } else {
        None
    };

    let mut draw = |canvas: &Canvas| -> Result<(), String> {
        let surface = surface
            .as_mut()
            .ok_or_else(|| String::from("no Metal surface"))?;
        // `draw` commits and returns; `settle` is what turns "queued" into "paid", as in the bench.
        surface.draw(canvas, None);
        surface.settle();
        Ok(())
    };

    let mut harness = Harness {
        cpu: Some(&cpu),
        surface: Some(&mut draw),
        // macOS has no counter the seam may read (see the module docs), so the platform energy source is
        // absent; an explicit `--energy-source platform` reports `skipped` rather than a zero.
        stats: None,
        wrap: None,
        platform: "macos",
        release: !cfg!(debug_assertions),
    };

    power_report::run(plan, &mut harness, out)
}

/// The offscreen surface the frame phase draws into: the same `NSView` + Metal layer the bench builds.
fn offscreen_surface() -> Option<Surface> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSView;

    let marker = MainThreadMarker::new()?;
    let view = NSView::new(marker);
    view.setFrame(objc2_core_foundation::CGRect {
        origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
        size: objc2_core_foundation::CGSize {
            width: CANVAS.0 as f64,
            height: CANVAS.1 as f64,
        },
    });

    let mut surface = Surface::install(&view, 1.0)?;
    surface.resize(CANVAS.0, CANVAS.1, 1.0);
    // The view has to outlive the surface it lent its layer to. Leaking it is deliberate: the report runs
    // once and exits, and a view kept alive by Objective-C's own count would otherwise be released while
    // the layer still points at it.
    std::mem::forget(view);

    Some(surface)
}
