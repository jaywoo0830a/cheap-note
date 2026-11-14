//! What the Windows canvas costs: the benchmark behind `cheap-note-bench`.
//!
//! ## Why a separate program
//!
//! The app's release image is a Windows-subsystem one, so it has no console to print a table to.
//! The benchmark is therefore its own console binary (`src/bin/bench.rs`), linked against the same
//! library and drawing through the same [`Device`] the window does. It is a measurement of the
//! renderer this app runs, not of a copy of it.
//!
//! ## What it measures
//!
//! A page of synthetic strokes is drawn into a hidden window's composition swap chain. For each
//! stroke count the benchmark reports:
//!
//! * **first** — the first frame, which builds every realization the page needs;
//! * **warm** — the mean of the frames after it, where every realization is already baked;
//! * **worst** — the slowest warm frame, because a mean hides the one frame a hand would feel;
//! * **rebuild** — one frame after a detail rebuild, which invalidates every realization;
//! * **pan** — one frame with the sheet moved, which must rebuild nothing;
//! * **present** — the compositor's own cost, timed separately from the drawing.
//!
//! The page has a desk, a shadow, a sheet and forty ruled lines as well as the ink, because that is
//! what a frame draws: a benchmark of the ink alone would answer a question nobody asked.
//!
//! ## Reading the numbers
//!
//! `frame` is the warm draw plus the present — what one frame costs when the page is being panned
//! or written on. `draw` alone is the app's own work; a present that is much larger is the
//! compositor and not the renderer, and at `Present(0)` (see [`super::ink_layer`]) it is a cost the
//! app cannot remove by drawing faster.
//!
//! The counts 0, 29 and 316 are the ones `doc/WINDOWS.md` §5 quotes; 8,182 is the page the macOS
//! measurements used, and is included because the question this benchmark exists for is whether
//! Windows is draw-bound on a page that size.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{rgb, rgba, Hsla};
use note_format::ink::{InkPoint, Stroke};
use note_ui::ink_layer::canvas::{Canvas, Fill, Ink, Rect};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_OVERLAPPED};

use super::ink_layer::device::Device;

/// The hidden window's client area at 100% scale: the size the app opens at.
const WINDOW: (i32, i32) = (1_280, 900);

/// The sheet the synthetic page is drawn on, and the margin around it, in logical pixels.
const SHEET: (f32, f32) = (720.0, 1_000.0);
const MARGIN: f32 = 40.0;

/// The default pages to measure, in strokes. The first three are the ones the old measurements
/// used; 8,182 is the page the macOS figures in `doc/WINDOWS.md` were taken on.
const DEFAULT_STROKES: [usize; 5] = [0, 29, 316, 1_000, 8_182];

/// How many warm frames each page is measured over.
const DEFAULT_FRAMES: usize = 60;

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

fn print_usage() {
    println!(
        "usage: cheap-note-bench [--strokes 0,29,316,1000,8182] [--frames 60]\n\
         \n\
         Draws a synthetic page through the same Direct2D canvas the app uses and reports the\n\
         first, warm, rebuild and pan costs of a frame, with the present timed separately.\n\
         \n\
         Run it in release: cargo run --release -p cheap-note --bin cheap-note-bench"
    );
}

/// The command line as a process exit code.
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--format` and `--help` are the shared contract's, so this command and `--memory-report` agree on
    // what those flags mean. Anything else is this benchmark's own.
    let shared = match note_ui::report::Options::parse(&args) {
        Ok(shared) => shared,
        Err(error) => {
            eprintln!("cheap-note-bench: {error}");
            return 2;
        }
    };

    if shared.help {
        print_usage();
        return 0;
    }

    let options = match Options::parse(&shared.rest) {
        Ok(Some(options)) => options,
        Ok(None) => return 0,
        Err(error) => {
            eprintln!("cheap-note-bench: {error}");
            print_usage();
            return 2;
        }
    };

    match measure_all(&options, shared.format) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cheap-note-bench: {error}");
            2
        }
    }
}

/// One page's measurements.
#[derive(Clone, Copy, Debug)]
struct Row {
    strokes: usize,
    first: Duration,
    warm: Duration,
    worst: Duration,
    rebuild: Duration,
    pan: Duration,
    present: Duration,
}

fn measure_all(options: &Options, format: note_ui::report::Format) -> Result<i32, String> {
    let hwnd = hidden_window()?;
    let result = measure_on(hwnd, options, format);

    unsafe {
        let _ = DestroyWindow(hwnd);
    }

    result
}

fn measure_on(
    hwnd: HWND,
    options: &Options,
    format: note_ui::report::Format,
) -> Result<i32, String> {
    use note_ui::report::{Command, Format, Report, Row, Value, Verdict};

    let mut device = Device::new(hwnd)
        .map_err(|error| format!("this machine has no Direct3D 11 canvas: {error:#}"))?;

    let mut rows = Vec::new();

    for &strokes in &options.strokes {
        rows.push(measure_page(&mut device, strokes, options.frames)?);
    }

    let mut report = Report::new(
        Command::Bench,
        "windows",
        format!("Direct2D canvas · {}×{} at 1.0x", WINDOW.0, WINDOW.1),
    )
    .columns(&[
        "strokes", "first", "warm", "worst", "rebuild", "pan", "present", "frame",
    ]);

    for row in &rows {
        report.push(Row::new(
            format!("{} strokes", row.strokes),
            vec![
                Value::Count(row.strokes as u64),
                Value::Millis(ms(row.first)),
                Value::Millis(ms(row.warm)),
                Value::Millis(ms(row.worst)),
                Value::Millis(ms(row.rebuild)),
                Value::Millis(ms(row.pan)),
                Value::Millis(ms(row.present)),
                // The figure worth comparing across platforms: one frame while the page is being panned
                // or written on. See the module docs.
                Value::Millis(ms(row.warm + row.present)),
            ],
        ));
    }

    let note = match format {
        Format::Table => Some(format!("{} warm frames per page", options.frames)),
        Format::Json => None,
    };

    let report = report.conclude(Verdict::Ok, note);

    Ok(note_ui::report::emit(
        &report,
        format,
        &mut std::io::stdout(),
    ))
}

/// Measures one page: its first frame, its settled frames, a rebuild and a pan.
fn measure_page(device: &mut Device, strokes: usize, frames: usize) -> Result<Row, String> {
    let mut canvas = page(strokes);

    // The first frame is the one that bakes every realization. It is timed before any other frame
    // has touched the cache, which is what makes it the page's cold cost.
    let first = time_draw(device, &canvas)?;
    device.present().map_err(|error| format!("{error:#}"))?;

    let mut warm = Duration::ZERO;
    let mut worst = Duration::ZERO;
    let mut present = Duration::ZERO;

    for _ in 0..frames {
        let draw = time_draw(device, &canvas)?;
        warm += draw;
        worst = worst.max(draw);

        let started = Instant::now();
        device.present().map_err(|error| format!("{error:#}"))?;
        present += started.elapsed();
    }

    // A detail rebuild: the revision changes, so every realization is stale and the next frame
    // builds all of them. This is the cost `build_detail` moves out of the frame.
    canvas.ink.revision += 1;
    let rebuild = time_draw(device, &canvas)?;
    device.present().map_err(|error| format!("{error:#}"))?;

    // A pan: the same ink and the same detail, moved. It must rebuild nothing; the difference
    // between this and `warm` is what a pan costs over a still page.
    canvas.ink.origin.0 += 1.0;
    let pan = time_draw(device, &canvas)?;
    device.present().map_err(|error| format!("{error:#}"))?;

    Ok(Row {
        strokes,
        first,
        warm: warm / frames as u32,
        worst,
        rebuild,
        pan,
        present: present / frames as u32,
    })
}

fn time_draw(device: &mut Device, canvas: &Canvas) -> Result<Duration, String> {
    let started = Instant::now();
    device.draw(canvas).map_err(|error| format!("{error:#}"))?;
    Ok(started.elapsed())
}

/// A page of synthetic handwriting, a sheet, a shadow and forty ruled lines.
///
/// The strokes are deterministic — no random source, no clock — so two runs are comparable and a
/// change in the numbers is a change in the renderer. They are spread over the sheet rather than
/// stacked in one place, because culling is part of what a frame does and a page of overlapping ink
/// would not exercise it.
fn page(strokes: usize) -> Canvas {
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

/// A window the canvas can be made for, never shown.
///
/// The same shape the canvas's own device tests use: a stock `STATIC` window is enough for a
/// composition target and for `GetClientRect`, and no message loop is needed because nothing is
/// waiting for one.
fn hidden_window() -> Result<HWND, String> {
    use windows::core::w;

    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("cheap-note bench"),
            WS_OVERLAPPED,
            0,
            0,
            WINDOW.0,
            WINDOW.1,
            None,
            None,
            None,
            None,
        )
    }
    .map_err(|error| format!("the benchmark could not make a window: {error}"))
}

/// A duration as milliseconds with three decimals, or microseconds below a millisecond.
fn millis(duration: Duration) -> String {
    let value = duration.as_secs_f64() * 1_000.0;
    if value < 1.0 {
        format!("{:.1} µs", duration.as_secs_f64() * 1_000_000.0)
    } else {
        format!("{value:.3} ms")
    }
}

/// The same figure as a plain number of milliseconds, for the contract.
///
/// One unit everywhere is what lets a script compare the two commands' numbers without knowing which
/// table it came from; the `µs` above is a courtesy for a person reading a row.
fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stroke_list_is_a_list() {
        assert_eq!(parse_strokes("0, 29,316").unwrap(), vec![0, 29, 316]);
        assert_eq!(parse_strokes("1000").unwrap(), vec![1000]);
        assert!(parse_strokes("1,two").is_err());
        assert!(parse_strokes(" , ").is_err());
    }

    #[test]
    fn the_defaults_are_the_pages_the_document_measured() {
        let options = Options::parse(&[]).unwrap().unwrap();
        assert_eq!(
            options.strokes,
            vec![0, 29, 316, 1_000, 8_182],
            "the counts doc/WINDOWS.md and doc/VIEW.md quote"
        );
        assert_eq!(options.frames, 60);
    }

    #[test]
    fn the_options_are_read_where_they_are_written() {
        let options = Options::parse(&[
            String::from("--strokes"),
            String::from("0,316"),
            String::from("--frames"),
            String::from("8"),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(options.strokes, vec![0, 316]);
        assert_eq!(options.frames, 8);
        assert!(Options::parse(&[String::from("--frames"), String::from("0")]).is_err());
        assert!(Options::parse(&[String::from("--nonsense")]).is_err());
    }

    #[test]
    fn a_page_holds_the_strokes_it_was_asked_for() {
        let page = page(3);

        assert_eq!(page.ink.strokes.len(), 3);
        assert!(page
            .ink
            .strokes
            .iter()
            .all(|stroke| !stroke.outline.is_empty()));
        assert_eq!(page.rules.len(), 40);
        assert!(page.sheet.is_some());
    }
}
