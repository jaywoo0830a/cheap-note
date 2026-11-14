//! What the macOS canvas costs: the benchmark behind `cheap-note --bench`.
//!
//! ## Why this is a module and not a binary
//!
//! Windows needs `cheap-note-bench` to be its own *console* binary because its release image is a
//! windows-subsystem program with no console beside the window. macOS has no such constraint, and the
//! contract is one command line: `cheap-note --bench` on both platforms, with the same `--format` and the
//! same envelope (see `crates/note-ui/src/report.rs`).
//!
//! ## How a window is avoided
//!
//! [`Surface`] is installed *into an `NSView`*, not into a window — the app reaches its view through
//! GPUI's window handle, but the surface itself needs only the view. So the benchmark makes a bare
//! `NSView` of its own and gives it a Metal layer. `draw` never needs a window either: it asks the layer
//! for a drawable, and an offscreen layer still supplies one.
//!
//! ## What it measures
//!
//! The same six figures as the Windows benchmark, so the two numbers can be put in the same table:
//!
//! * **first** — the first frame, which builds every realization the page needs;
//! * **warm** — the mean of the frames after it, where every realization is already baked;
//! * **worst** — the slowest warm frame, because a mean hides the one frame a hand would feel;
//! * **rebuild** — one frame after a detail rebuild, which invalidates every realization;
//! * **pan** — one frame with the sheet moved, which must rebuild nothing;
//! * **present** — the GPU's own cost, which on macOS is waited for rather than timed: `draw` commits and
//!   returns, so [`Surface::settle`] is what turns "queued" into "paid", and its time is the present.
//!
//! `frame` is the warm draw plus the present, as on Windows.
//!
//! ## The one honest difference
//!
//! Windows times `present` as the cost of handing the frame to the compositor. macOS's `settle` waits for
//! the GPU to finish *all* the work the frame queued — not only the present — because Metal's command
//! buffer does not separate the two. So the macOS `present` is "everything the GPU did for this frame",
//! which is why `frame` here is the figure to read and `present` alone is not comparable row for row.

use std::sync::Arc;
use std::time::{Duration, Instant};

use note_format::ink::InkPoint;
use note_ui::ink_layer::canvas::{Canvas, Ink, Rect};

use gpui_kit::{rgb, rgba, Hsla};
use note_ui::report::{Command, Format, Report, Row, Value, Verdict};

use crate::platform::metal::Surface;

/// The canvas the benchmark asks for, in device pixels.
const CANVAS: (usize, usize) = (1_280, 900);

/// The sheet the page is drawn on, in the paper's own units — the same figures Windows uses.
const SHEET: (f32, f32) = (720.0, 1_000.0);

/// Where the sheet sits on the desk.
const MARGIN: f32 = 40.0;

/// The pages to measure, in strokes: the counts `doc/WINDOWS.md` §5 quotes, plus 8 182 — the page the
/// macOS figures were taken on.
const DEFAULT_STROKES: [usize; 5] = [0, 29, 316, 1_000, 8_182];

/// How many warm frames each page is measured over.
const DEFAULT_FRAMES: usize = 30;

/// What the command line asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Options {
    strokes: Vec<usize>,
    frames: usize,
}

impl Options {
    fn parse(args: &[String]) -> Result<Option<Self>, String> {
        let mut strokes = DEFAULT_STROKES.to_vec();
        let mut frames = DEFAULT_FRAMES;
        let mut at = 0;

        while at < args.len() {
            match args[at].as_str() {
                "--help" | "-h" => {
                    print_usage();
                    return Ok(None);
                }
                "--strokes" => {
                    at += 1;
                    let list = args
                        .get(at)
                        .ok_or("--strokes needs a comma-separated list of counts")?;
                    strokes = parse_strokes(list)?;
                }
                "--frames" => {
                    at += 1;
                    let count = args
                        .get(at)
                        .ok_or("--frames needs a count")?
                        .parse::<usize>()
                        .map_err(|_| "--frames must be a whole number")?;
                    if count == 0 {
                        return Err(String::from("--frames must be at least one"));
                    }
                    frames = count;
                }
                other => return Err(format!("unknown argument: {other}")),
            }

            at += 1;
        }

        Ok(Some(Options { strokes, frames }))
    }
}

/// A comma-separated list of stroke counts.
fn parse_strokes(text: &str) -> Result<Vec<usize>, String> {
    let mut counts = Vec::new();

    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let count = part
            .parse::<usize>()
            .map_err(|_| format!("'{part}' is not a stroke count"))?;
        counts.push(count);
    }

    if counts.is_empty() {
        return Err(String::from("--strokes named no counts"));
    }

    Ok(counts)
}

/// What the command line accepts, for `--help` and for a mistake.
pub fn print_usage() {
    println!(
        "usage: cheap-note --bench [--format table|json] [--strokes 0,29,316,1000,8182] [--frames 30]\n\
         \n\
         Draws a synthetic page through the same Metal canvas the app uses and reports the first,\n\
         warm, worst, rebuild and pan costs of a frame, with the GPU's own time waited for apart.\n\
         \n\
         Run it in release: cargo run --release -p cheap-note-macos -- --bench"
    );
}

/// One page's measurements.
#[derive(Clone, Copy, Debug)]
struct PageCost {
    strokes: usize,
    first: Duration,
    warm: Duration,
    worst: Duration,
    rebuild: Duration,
    pan: Duration,
    gpu: Duration,
}

/// Runs the benchmark over the arguments it owns, writing the report in `format`.
///
/// `args` is what is left after the shared flags are taken — `--strokes`, `--frames`, `--help` — so this
/// function never sees `--format`, and there is one place that decides what it means.
pub fn run(args: &[String], format: Format) -> i32 {
    let options = match Options::parse(args) {
        Ok(Some(options)) => options,
        Ok(None) => return 0,
        Err(error) => {
            eprintln!("cheap-note: {error}");
            print_usage();
            return 2;
        }
    };

    match measure(&options, format) {
        Ok(code) => code,
        Err(error) => {
            // A machine with no Metal device, or a layer that will not make a drawable: this is a
            // measurement that could not be taken, which is `skipped` and never a pass.
            println!("cheap-note --bench: {error}");
            2
        }
    }
}

fn measure(options: &Options, format: Format) -> Result<i32, String> {
    let mut surface = offscreen().ok_or_else(|| {
        String::from("no Metal device, or the offscreen layer would not make a drawable")
    })?;

    let mut costs = Vec::with_capacity(options.strokes.len());
    for &strokes in &options.strokes {
        costs.push(measure_page(&mut surface, strokes, options.frames)?);
    }

    let mut report = Report::new(
        Command::Bench,
        "macos",
        format!("Metal · {}×{} at 1.0x", CANVAS.0, CANVAS.1),
    )
    .columns(&[
        "strokes", "first", "warm", "worst", "rebuild", "pan", "present", "frame",
    ]);

    for cost in &costs {
        report.push(Row::new(
            format!("{} strokes", cost.strokes),
            vec![
                Value::Count(cost.strokes as u64),
                Value::Millis(ms(cost.first)),
                Value::Millis(ms(cost.warm)),
                Value::Millis(ms(cost.worst)),
                Value::Millis(ms(cost.rebuild)),
                Value::Millis(ms(cost.pan)),
                Value::Millis(ms(cost.gpu)),
                Value::Millis(ms(cost.warm + cost.gpu)),
            ],
        ));
    }

    let note = match format {
        Format::Table => Some(format!(
            "{} warm frames per page · `present` is the GPU's whole frame on macOS",
            options.frames
        )),
        Format::Json => None,
    };

    let report = report.conclude(Verdict::Ok, note);

    Ok(note_ui::report::emit(
        &report,
        format,
        &mut std::io::stdout(),
    ))
}

/// A surface of the benchmark's own, not in any window.
///
/// The app reaches its view through GPUI's window handle; the surface takes the view. So a bare
/// `NSView` with a frame and a Metal layer is the whole of what a canvas needs to exist.
fn offscreen() -> Option<Surface> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSView;

    let marker = MainThreadMarker::new()?;
    let view = NSView::new(marker);

    // The frame the surface is made for: the view's own, which is what `Surface::install` reads. An
    // `NSView` with no superview starts with a zero frame, so it is set to the canvas's size here.
    view.setFrame(objc2_core_foundation::CGRect {
        origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
        size: objc2_core_foundation::CGSize {
            width: CANVAS.0 as f64,
            height: CANVAS.1 as f64,
        },
    });

    let mut surface = Surface::install(&view, 1.0)?;
    surface.resize(CANVAS.0, CANVAS.1, 1.0);

    // The view has to outlive the surface it lent its layer to. Leaking it is deliberate: the benchmark
    // runs once and exits, and a view kept alive by Objective-C's own reference count would otherwise be
    // released while the layer still points at it.
    std::mem::forget(view);

    Some(surface)
}

/// Measures one page: its first frame, its settled frames, a rebuild and a pan.
fn measure_page(
    surface: &mut Surface,
    strokes: usize,
    frames: usize,
) -> Result<PageCost, String> {
    let mut canvas = page(strokes);

    // The first frame is the one that bakes every realization. It is timed before any other frame has
    // touched the cache, which is what makes it the page's cold cost.
    let first = time_frame(surface, &canvas)?;

    let mut warm = Duration::ZERO;
    let mut worst = Duration::ZERO;
    let mut gpu = Duration::ZERO;

    for _ in 0..frames {
        let started = Instant::now();
        draw(surface, &canvas)?;
        let gpu_started = Instant::now();
        surface.settle();
        gpu += gpu_started.elapsed();

        let elapsed = started.elapsed();
        warm += elapsed;
        worst = worst.max(elapsed);
    }

    // A detail rebuild: the revision changes, so every realization is stale and the next frame builds
    // all of them. This is the cost a frame must be able to absorb.
    canvas.ink.revision = canvas.ink.revision.wrapping_add(1);
    let rebuild = time_frame(surface, &canvas)?;

    // A pan: the same ink and the same detail, moved. It must rebuild nothing; the difference between
    // this and `warm` is what a pan costs over a still page.
    canvas.ink.origin.0 += 1.0;
    let pan = time_frame(surface, &canvas)?;

    Ok(PageCost {
        strokes,
        first,
        warm: warm / frames as u32,
        worst,
        rebuild,
        pan,
        gpu: gpu / frames as u32,
    })
}

/// One frame, waited for, as the time it took.
fn time_frame(surface: &mut Surface, canvas: &Canvas) -> Result<Duration, String> {
    let started = Instant::now();
    draw(surface, canvas)?;
    surface.settle();
    Ok(started.elapsed())
}

/// Draws one frame, or says why the layer would not take it.
fn draw(surface: &mut Surface, canvas: &Canvas) -> Result<(), String> {
    if surface.draw(canvas, None) {
        Ok(())
    } else {
        Err(String::from(
            "the layer had no drawable ready — offscreen surfaces still need one to draw into",
        ))
    }
}

/// A duration as milliseconds, in the one unit the contract uses.
fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}


/// A page of synthetic handwriting, a sheet, a shadow and forty ruled lines.
///
/// The strokes are deterministic — no random source, no clock — so two runs are comparable and a change
/// in the numbers is a change in the renderer rather than in the page. They are spread over the sheet
/// rather than stacked in one place, because culling is part of what a frame does and a page of
/// overlapping ink would not exercise it.
///
/// The figures match the Windows benchmark's page, so the two platforms are drawing the same thing and
/// their numbers can be put side by side.
fn page(strokes: usize) -> Canvas {
    use note_format::ink::Stroke;
    use note_ui::ink_layer::canvas::Fill;

    let mut ink = Ink {
        origin: (MARGIN, MARGIN),
        zoom: 1.0,
        paper: SHEET,
        visible: [0.0, 0.0, SHEET.0, SHEET.1],
        revision: 1,
        ..Default::default()
    };

    let mut list = Vec::with_capacity(strokes);
    for index in 0..strokes {
        let x = 70.0 + ((index * 37) % 580) as f32;
        let y = 70.0 + ((index * 53) % 880) as f32;
        let points = (0..24)
            .map(|step| {
                let step = step as f32;
                InkPoint::new(x + step * 2.0, y + (step * 0.45).sin() * 6.0, 2.0)
            })
            .collect();

        let mut stroke = Stroke {
            points,
            color: 0x1B_1B_1F,
            outline: Vec::new(),
            bounds: [0.0; 4],
        };
        stroke.close();
        list.push(Arc::new(stroke));
    }
    ink.strokes = Arc::new(list);

    let sheet = Rect {
        x: MARGIN,
        y: MARGIN,
        width: SHEET.0,
        height: SHEET.1,
    };

    let fills = vec![
        Fill::new(
            Rect {
                x: MARGIN + 8.0,
                y: MARGIN + 8.0,
                width: SHEET.0,
                height: SHEET.1,
            },
            Hsla::from(rgba(0x0000_0022)),
        ),
        Fill::new(sheet, Hsla::from(rgb(0xF7_F3_EA))),
    ];

    let rules = (0..40)
        .map(|line| {
            Fill::new(
                Rect {
                    x: MARGIN + 24.0,
                    y: MARGIN + 40.0 + line as f32 * 22.0,
                    width: SHEET.0 - 48.0,
                    height: 1.0,
                },
                Hsla::from(rgba(0x3B6E_A522)),
            )
        })
        .collect();

    Canvas {
        desk: Hsla::from(rgb(0x2A2A2E)),
        scale: 1.0,
        fills,
        sheet: Some(sheet),
        rules,
        page: None,
        ink,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_stroke_list_is_a_list() {
        assert_eq!(parse_strokes("0, 29,316").unwrap(), vec![0, 29, 316]);
        assert_eq!(parse_strokes("1000").unwrap(), vec![1000]);
    }

    /// The default page set is the one the Windows benchmark uses, so the two tables can be read side by
    /// side. A platform-specific default would make the comparison an exercise in arithmetic.
    #[test]
    fn the_defaults_are_the_shared_page_set() {
        let options = Options::parse(&args(&[])).unwrap().unwrap();

        assert_eq!(options.strokes, vec![0, 29, 316, 1_000, 8_182]);
        assert_eq!(options.frames, DEFAULT_FRAMES);
    }

    #[test]
    fn a_stroke_list_and_a_frame_count_are_both_read() {
        let options = Options::parse(&args(&["--strokes", "1,2", "--frames", "5"]))
            .unwrap()
            .unwrap();

        assert_eq!(options.strokes, vec![1, 2]);
        assert_eq!(options.frames, 5);
    }

    #[test]
    fn zero_frames_is_a_mistake() {
        let error = Options::parse(&args(&["--frames", "0"])).unwrap_err();
        assert!(error.contains("at least one"), "{error}");
    }

    #[test]
    fn a_stroke_count_that_is_not_a_number_names_itself() {
        let error = Options::parse(&args(&["--strokes", "abc"])).unwrap_err();
        assert!(error.contains("abc"), "the sentence names what was typed: {error}");
    }

    /// `--help` asks for the usage rather than for a run.
    #[test]
    fn help_asks_for_nothing_to_be_measured() {
        assert_eq!(Options::parse(&args(&["--help"])).unwrap(), None);
    }

    /// A flag the benchmark does not own is refused here rather than passed to `draw`.
    #[test]
    fn an_unknown_argument_is_refused() {
        let error = Options::parse(&args(&["--format", "json"])).unwrap_err();
        assert!(error.contains("--format"), "{error}");
    }

    /// The page the benchmark draws is a whole sheet: a desk, a shadow, ink and ruling.
    ///
    /// A page of ink alone would leave the fills and the rules untimed, and those are most of what a
    /// frame's `behind` list is.
    #[test]
    fn the_page_has_a_sheet_ruling_and_ink() {
        let page = page(29);

        assert_eq!(page.ink.strokes.len(), 29);
        assert_eq!(page.ink.revision, 1);
        assert_eq!(page.fills.len(), 2, "the shadow and the sheet");
        assert_eq!(page.rules.len(), 40);
        assert!(page.sheet.is_some());
    }

    /// Two runs draw the same page, so a change in the numbers is a change in the renderer.
    #[test]
    fn the_page_is_deterministic() {
        let first = page(316);
        let second = page(316);

        assert_eq!(first.ink.strokes.len(), second.ink.strokes.len());
        assert_eq!(
            first.ink.strokes[7].points.len(),
            second.ink.strokes[7].points.len()
        );
        assert_eq!(
            first.ink.strokes[7].points[3].x,
            second.ink.strokes[7].points[3].x
        );
    }

    /// Durations reach the contract as milliseconds, the one unit both commands use.
    #[test]
    fn durations_are_milliseconds() {
        assert_eq!(ms(Duration::from_millis(4)), 4.0);
        assert!((ms(Duration::from_micros(1_500)) - 1.5).abs() < f64::EPSILON);
    }
}

