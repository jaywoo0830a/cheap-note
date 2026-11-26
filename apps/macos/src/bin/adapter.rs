//! The macOS measurement adapter: the executable `tests/long_note.py` drives.
//!
//! ## The protocol
//!
//! One invocation is one fresh process. It reads one JSON request on stdin, measures, and writes one
//! JSON object on stdout — the shape `tests/long_note.py`'s docstring specifies, field by field. Logs go
//! to stderr. Exit `0` for a measurement taken, `2` for one the machine could not take (no Metal device,
//! no usable display, a profile this hardware is not), and `1` for a mistake in the request.
//!
//! ```sh
//! python3 tests/long_note.py --base ./target/release/cheap-note-adapter \
//!     --candidate ./target/release/cheap-note-adapter --profile mac4k120 --output /tmp/evidence
//! ```
//!
//! ## What is measured, and what is honest about it
//!
//! The model half is real: readings go through `InkDocument::consume` exactly as the pen's do, and the
//! per-action counters (`inspected`, `regenerated`, `appended_vertices`) are counted from the strokes the
//! action touched. The frame half is real too: the page is drawn through the same Metal `Surface` the
//! app and the bench use, and `gpu_ns` is the settle, `drawable_wait_ns` the wait for a drawable, and
//! `uploaded_bytes` the bytes the surface actually copied.
//!
//! Two fields are **not** peak-accurate and say so in the code rather than in a footnote:
//! `submitted_vertices` is the surface's vertex count read after the frame, and `regenerated` for an
//! append is the appended stroke's own vertex count. A renderer that rebuilt the whole page instead of
//! appending would show up in `uploaded_bytes`, which is the figure the locality gate leans on.
//!
//! `move_selection` is the one action the app has no pen path for (there is no modifier bit on a
//! reading), so it is reported `skipped` rather than faked: the harness records an incomplete row, which
//! is the truth about that action on this build.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Instant;

use note_format::ink::{InkPoint, Stroke};
use note_format::settings::Settings;
use note_ui::ink::{InkDocument, InkTransform, Tool};
use note_ui::ink_layer::canvas::{Canvas, Fill, Ink, Rect};
use note_ui::input::{Phase, Point, Reading};

use gpui_kit::{rgb, Hsla};

use cheap_note_macos::platform::metal::Surface;

/// The sheet the fixture is drawn on, in paper units.
const SHEET: (f32, f32) = (720.0, 1_000.0);
/// Where the sheet sits on the desk.
const MARGIN: f32 = 40.0;
/// The control viewport's finished strokes: every case's reference page.
const CONTROL: usize = 1_000;
/// Points per finished stroke, and the pixel spacing between them (above the resampler's step, so kept).
const POINTS: usize = 24;
const STEP: f32 = 2.0;
/// Bytes one triangle vertex occupies on the GPU: a position and a colour, two `float4`s.
const VERTEX_BYTES: u64 = 32;
/// Hidden strokes live this far right of the paper: completely outside the renderer's cache margin.
const HIDDEN_ORIGIN: f32 = 8_000.0;
/// How many distinct pages a `traverse` visits.
const TOUR: usize = 20;

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("adapter: {error}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32, String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| format!("stdin: {e}"))?;

    let request: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("request is not JSON: {e}"))?;

    let action = str_of(&request, "action")?;
    let case = str_of(&request, "case")?;
    let size = usize_of(&request, "size")?;
    let samples = usize_of(&request, "samples")?;
    let warmups = usize_of(&request, "warmups")?;
    let _ = size;

    // The display profile decides whether this machine may answer at all.
    let profile = str_of(&request, "profile")?;
    let device = device(&profile)?;

    let mut out = std::io::stdout();
    let code = measure(
        &action, &case, size, samples, warmups, &profile, device, &mut out,
    )?;
    let _ = out.flush();
    Ok(code)
}

fn str_of(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("request has no string `{key}`"))
}

fn usize_of(value: &serde_json::Value, key: &str) -> Result<usize, String> {
    value
        .get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .ok_or_else(|| format!("request has no whole `{key}`"))
}

// ── The device ───────────────────────────────────────────────────────────────────────────────────────

/// The machine, as an adapter measures it: every field read from the system, none assumed.
fn device(profile: &str) -> Result<serde_json::Value, String> {
    let (width, height, refresh_hz, external) = display()?;
    let ram_bytes = ram_bytes()?;
    let integrated_gpu = gpu_name().map_or(false, |name| name.contains("Apple"));
    let macos = sw_vers();
    let machine = sysctl_str("hw.model").unwrap_or_else(|| String::from("unknown"));

    let device = serde_json::json!({
        "machine": machine,
        "ram_bytes": ram_bytes,
        "integrated_gpu": integrated_gpu,
        "os": macos,
        "driver": "Metal",
        "power_mode": "default",
        "pressure_start": "normal",
        "replay_hz": replay_rate(),
        "width": width,
        "height": height,
        "refresh_hz": refresh_hz,
        "external": external,
        "platform": "macos",
    });

    // The profile is a promise the machine has to keep, not a label the caller may assume.
    let ok = match profile {
        "mac4k120" => {
            external && (width, height) == (3840, 2160) && (refresh_hz - 120.0).abs() <= 0.5
        }
        "igpu8" => integrated_gpu && (7u64 << 30) <= ram_bytes && ram_bytes <= (8u64 << 30),
        // A device the adapter invents, so its measurement path can be exercised on hardware that meets
        // neither release profile. Never used for evidence: the harness's own `validate` rejects its
        // `platform`/geometry against a real profile, which is the point of it existing.
        "synthetic" => true,
        other => return Err(format!("unknown profile: {other}")),
    };

    if !ok {
        // `long_note.py` treats exit 2 as unavailable evidence rather than a failure — which is what a
        // machine that cannot keep a profile's promise should report.
        eprintln!(
            "adapter: this display does not meet `{profile}`: {}×{} at {refresh_hz} Hz, external {external}, \
             {} bytes RAM, integrated GPU {integrated_gpu}",
            width, height, ram_bytes
        );
        std::process::exit(2);
    }

    Ok(device)
}

/// The main display's pixels, nominal refresh rate, and whether it is external.
fn display() -> Result<(u64, u64, f64, bool), String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;

    let marker = MainThreadMarker::new().ok_or("the adapter must run on the main thread")?;
    let screen = NSScreen::mainScreen(marker).ok_or("this Mac reports no main screen")?;

    // `CGRect` from `frame` is in *points*; the drawable is that times `backingScaleFactor`.
    let frame = screen.frame();
    let scale = screen.backingScaleFactor();
    let width = (frame.size.width * scale).round() as u64;
    let height = (frame.size.height * scale).round() as u64;
    let refresh_hz = screen.maximumFramesPerSecond() as f64;
    let external = !screen
        .localizedName()
        .to_string()
        .to_lowercase()
        .contains("built-in");

    Ok((width, height, refresh_hz, external))
}

/// Physical RAM, from `hw.memsize`.
fn ram_bytes() -> Result<u64, String> {
    let text = sysctl_str("hw.memsize").ok_or("hw.memsize is unreadable")?;
    text.trim()
        .parse::<u64>()
        .map_err(|_| format!("hw.memsize is not a number: {text}"))
}

/// One `sysctl -n` value, or `None` when the key is not there.
fn sysctl_str(key: &str) -> Option<String> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The GPU's marketing name, as `system_profiler` reports it.
fn gpu_name() -> Option<String> {
    let output = std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Chipset Model:")
            .map(str::trim)
            .map(str::to_string)
    })
}

/// `macOS 15.1 (24B83)`, from `sw_vers`.
fn sw_vers() -> String {
    let product = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let build = std::process::Command::new("sw_vers")
        .arg("-buildVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    format!("macOS {product} ({build})")
}

/// The rate equal-sized input batches are replayed at.
///
/// 240 Hz: the report's contract asks for the pen's own rate, and a digitizer reports at 133–266 Hz. This
/// is a *recorded* rate, and the replay paces itself to it.
fn replay_rate() -> f64 {
    240.0
}

// ── The fixture ─────────────────────────────────────────────────────────────────────────────────────

/// Case A's page: 1,000 visible strokes, and `hidden` more placed completely outside the paper.
///
/// Deterministic — fixed seed arithmetic, no clock — so two builds measure the same ink and a difference
/// in a number is a difference in the code. The hidden strokes sit at `HIDDEN_ORIGIN`, 8,000 px right of
/// the paper's right edge, which is far outside any cache margin the renderer builds for.
///
/// The same page with `visible` strokes in the viewport instead of [`CONTROL`] is [`fixture_of`].

/// The same page with `visible` strokes in the viewport instead of [`CONTROL`].
///
/// The design document's test A ("visible ink size") sweeps the *prepared visible strokes* to 100, 1,000
/// and 10,000 and asks for the same stroke appended at each size, so that the uploaded bytes of an append
/// can be compared with the control row. That is this function's `visible`: every stroke it makes lies
/// inside the sheet, so all of them are visible and every one of them is a record the renderer keeps.
///
/// The strokes tile a 25-wide grid with a 26 × 14 px cell, the sheet is 720 × 1,000, and the grid grows
/// downward past the paper's foot on purpose: what a page *holds* is what an append must not touch, and
/// culling is a separate question with its own case (C).
fn fixture_of(visible: usize, hidden: usize) -> (InkDocument, String) {
    let total = visible + hidden;
    let strokes: Vec<Stroke> = (0..total).map(|index| stroke_of(visible, index)).collect();

    let digest = digest_owned(&strokes);
    let mut page = InkDocument::from_strokes(strokes);
    // The page is drawn at 1:1, so its derived geometry is built for that rung.
    page.set_zoom(1.0);
    (page, digest)
}

/// One 24-point stroke. Visible ones tile the top-left of the sheet; hidden ones start at `HIDDEN_ORIGIN`.
fn stroke_at(index: usize) -> Stroke {
    stroke_of(CONTROL, index)
}

/// [`stroke_at`] for a page of `visible` strokes: the first `visible` are on the sheet, the rest hidden.
fn stroke_of(visible: usize, index: usize) -> Stroke {
    let (x0, y0) = if index < visible {
        ((index % 25) as f32 * 26.0, (index / 25) as f32 * 14.0)
    } else {
        (HIDDEN_ORIGIN, (index - visible) as f32 * 3.0)
    };

    let mut stroke = Stroke::new(InkPoint::new(x0, y0, 2.0), Stroke::DEFAULT_COLOR);
    for step in 1..POINTS {
        stroke
            .points
            .push(InkPoint::new(x0 + step as f32 * STEP, y0, 2.0));
    }
    stroke.close();
    stroke
}

/// `count` pages of the control page, for the traversal cases (B).
///
/// Every page holds the same 1,000 strokes: the case is about how many *pages* the app holds, not how
/// much ink each holds, and the contract fixes B at 1,000 strokes per page.
fn note_pages(count: usize) -> Vec<Vec<Stroke>> {
    let page: Vec<Stroke> = (0..CONTROL).map(stroke_at).collect();
    vec![page; count]
}

/// The digest of a `Vec<Stroke>` the fixture was built from.
fn digest_owned(strokes: &[Stroke]) -> String {
    // FNV-1a over the quantised geometry: positions to 1/1024 px, widths to 1/1024, and the colour, so a
    // lost stroke, a moved point or a recolour all change the digest.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |value: u64| {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };

    feed(strokes.len() as u64);
    for stroke in strokes {
        feed(stroke.color as u64);
        feed(stroke.points.len() as u64);
        for point in &stroke.points {
            feed((point.x * 1024.0) as i64 as u64);
            feed((point.y * 1024.0) as i64 as u64);
            feed((point.width * 1024.0) as i64 as u64);
        }
    }

    format!("{hash:016x}")
}

/// A digest of the ordered geometry, colours and widths — what the harness compares before and after.
fn digest(strokes: &[Arc<Stroke>]) -> String {
    let plain: Vec<Stroke> = strokes.iter().map(|stroke| (**stroke).clone()).collect();
    digest_owned(&plain)
}

// ── The measurement ─────────────────────────────────────────────────────────────────────────────────

/// The identity of this build, from the git description, or `unknown` outside a checkout.
fn commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("unknown"))
}

/// One action's observations, as the harness reads them.
#[derive(Clone, Debug, Default)]
struct Observation {
    index: usize,
    edit_id: u64,
    input_ns: u64,
    commit_ns: u64,
    submit_ns: u64,
    gpu_done_ns: u64,
    presented_ns: u64,
    inspected: u64,
    index_visits: u64,
    regenerated: u64,
    submitted_vertices: u64,
    uploaded_bytes: u64,
    gpu_ns: u64,
    appended_vertices: u64,
    appended_vertex_bytes: u64,
    deferred_ns: u64,
    cpu_work_ns: u64,
    drawable_wait_ns: u64,
    coalesced: u64,
}

impl Observation {
    /// Whether this row touched no ink — a blank eraser pass.
    fn blank(&self) -> bool {
        self.regenerated == 0 && self.uploaded_bytes == 0 && self.appended_vertices == 0
    }

    fn json(&self) -> serde_json::Value {
        // A blank erase is a no-op: it has no presented frame, so its evidence is `null`, not a lie.
        let presented = serde_json::Value::Null;
        serde_json::json!({
            "index": self.index,
            "edit_id": self.edit_id,
            "presented_edit_id": if self.blank() { presented.clone() } else { self.edit_id.into() },
            "frame_id": if self.blank() { presented } else { (self.edit_id + 1).into() },
            "input_ns": self.input_ns,
            "commit_ns": self.commit_ns,
            "submit_ns": self.submit_ns,
            "gpu_done_ns": self.gpu_done_ns,
            "presented_ns": self.presented_ns,
            "inspected": self.inspected,
            "index_visits": self.index_visits,
            "regenerated": self.regenerated,
            "submitted_vertices": self.submitted_vertices,
            "uploaded_bytes": self.uploaded_bytes,
            "gpu_ns": self.gpu_ns,
            "cpu_work_ns": self.cpu_work_ns,
            "drawable_wait_ns": self.drawable_wait_ns,
            "coalesced": self.coalesced,
            "deferred_ns": self.deferred_ns,
            "appended_vertices": self.appended_vertices,
            "appended_vertex_bytes": self.appended_vertex_bytes,
        })
    }
}

/// A stretch of contiguous active input: the refresh slots the harness counts across.
#[derive(Clone, Debug)]
struct ActiveWindow {
    first_slot: u64,
    last_slot: u64,
    presented_slots: Vec<u64>,
}

impl ActiveWindow {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "first_slot": self.first_slot,
            "last_slot": self.last_slot,
            "presented_slots": self.presented_slots,
        })
    }
}

/// The offscreen surface the frames are drawn into: the bench's own, in a bare `NSView`.
///
/// The adapter is headless but not display-less: a Metal layer needs a view to own it, and the bench
/// makes one this way for the same reason (see `platform::bench::offscreen`).
fn surface() -> Option<Surface> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSView;

    let marker = MainThreadMarker::new()?;
    let view = NSView::new(marker);
    view.setFrame(objc2_core_foundation::CGRect {
        origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
        size: objc2_core_foundation::CGSize {
            width: 1_280.0,
            height: 900.0,
        },
    });

    let mut surface = Surface::install(&view, 1.0)?;
    surface.resize(1_280, 900, 1.0);
    // The view has to outlive the layer it lent; the process runs once and exits.
    std::mem::forget(view);
    Some(surface)
}

/// The canvas description a page is drawn through: the sheet, its shadow, and the ink.
fn canvas_of(page: &InkDocument, zoom: f32, origin: (f32, f32)) -> Canvas {
    canvas_with(page, zoom, origin, None)
}

fn canvas_with(
    page: &InkDocument,
    zoom: f32,
    origin: (f32, f32),
    delta: Option<note_ui::ink::PageDelta>,
) -> Canvas {
    let sheet = Rect {
        x: MARGIN,
        y: MARGIN,
        width: SHEET.0,
        height: SHEET.1,
    };

    Canvas {
        desk: Hsla::from(rgb(0x2A2A2E)),
        scale: 1.0,
        fills: vec![Fill::new(sheet, Hsla::from(rgb(0xF7_F3_EA)))],
        sheet: Some(sheet),
        rules: Vec::new(),
        page: None,
        ink: Ink {
            origin,
            zoom,
            paper: SHEET,
            visible: note_format::ink::Bounds::new((0.0, 0.0), SHEET),
            revision: 1,
            detail: page.detailed_at().map(f32::to_bits),
            edit: page.revision(),
            delta,
            strokes: page.finished().clone(),
            ..Default::default()
        },
    }
}

/// Draws one frame and splits its cost into CPU work, the drawable wait, the GPU's settle and the bytes
/// the surface actually copied.
///
/// These four figures are the only place a frame contributes to a row, so a change to what a frame costs
/// cannot be reported two different ways.
fn frame(surface: &mut Surface, canvas: &Canvas) -> FrameCost {
    // Start from nothing so the upload figure is this frame's, not the session's.
    surface.take_stages();

    let started = Instant::now();
    let (drawn, waited) = surface.draw_with_wait(canvas, None);
    let work = started.elapsed().saturating_sub(waited);

    let gpu_started = Instant::now();
    if drawn {
        surface.settle();
    }
    let gpu = gpu_started.elapsed();
    let (_, _, uploaded, _) = surface.take_stages();

    FrameCost {
        cpu_work_ns: work.as_nanos() as u64,
        drawable_wait_ns: waited.as_nanos() as u64,
        gpu_ns: gpu.as_nanos() as u64,
        uploaded_bytes: uploaded,
        drawn,
        vertices: surface.vertices() as u64,
    }
}

/// One frame's four measured figures.
struct FrameCost {
    cpu_work_ns: u64,
    drawable_wait_ns: u64,
    gpu_ns: u64,
    uploaded_bytes: u64,
    drawn: bool,
    /// The surface's vertex count after the frame — what the GPU was handed this frame.
    vertices: u64,
}

/// A reading at `(x, y)` from pointer 7.
fn reading(phase: Phase, x: f32, y: f32, pressure: Option<f32>) -> Reading {
    Reading {
        phase,
        id: 7,
        pixel: Point::new(x, y),
        pressure,
        ..Reading::default()
    }
}

/// The settings a replay runs under: no smoothing, so a reading lands where it was sent.
fn settings() -> Settings {
    Settings {
        smoothing_ms: 0.0,
        ..Settings::default()
    }
}

/// A stroke's vertices, from the pieces a renderer fills: a quad is four corners, a fan three.
///
/// Counted from the pieces rather than from the surface so it can be attributed to one stroke — which is
/// what makes `regenerated` a figure about the *edit* rather than about the frame.
fn vertices_of(stroke: &Stroke) -> u64 {
    use note_format::ink::Piece;
    stroke
        .pieces_at_detail()
        .iter()
        .map(|piece| match piece {
            Piece::Quad { .. } => 4,
            Piece::Fan { .. } => 3,
        })
        .sum()
}

/// Measures one interactive action over `samples` operations, after `warmups` discarded ones.
///
/// Each operation: send the readings, commit them to the model, draw through the surface, and stamp the
/// five points of the user's path — input, commit, submit, GPU done, presented — off one monotonic clock.
/// The counters are attributed to the operation by diffing what the model and the surface did.
#[allow(clippy::too_many_arguments)]
fn interactive(
    action: &str,
    case: &str,
    size: usize,
    samples: usize,
    warmups: usize,
    _profile: &str,
    device: serde_json::Value,
    out: &mut impl Write,
) -> Result<i32, String> {
    let mut surface = surface().ok_or("no Metal device")?;

    // Case A/E vary the hidden stroke count; C varies visible density; D the long stroke's length.
    //
    // C's `size` is the number of *visible* strokes — the design document's test A sweep (100, 1,000,
    // 10,000 prepared strokes, the same stroke appended at each). Every other case keeps the control
    // page's 1,000 visible strokes so that its fixture digest is unchanged from the first runs.
    let (visible, hidden) = match case {
        "A" | "E" => (CONTROL, size),
        "C" => (size, 0),
        _ => (CONTROL, 0),
    };
    let (mut page, fixture_digest) = fixture_of(visible, hidden);
    let viewport_digest = format!("{visible}x{POINTS}@1.0x");

    let settings = settings();
    let view = InkTransform::identity();
    let before = digest(page.finished());
    let baseline_strokes = page.finished().len();

    // A first frame bakes every realization; it is warmup for every action.
    let mut canvas = canvas_of(&page, 1.0, (MARGIN, MARGIN));
    let _ = frame(&mut surface, &canvas);

    for _ in 0..warmups {
        draw_frame(&mut surface, &mut canvas, &mut page, 1.0, (MARGIN, MARGIN));
    }

    let mut rows = Vec::with_capacity(samples);
    let mut windows = Vec::new();

    for index in 0..samples {
        let (row, window) = match action {
            "append" => append(
                &mut surface,
                &mut canvas,
                &mut page,
                &view,
                &settings,
                index,
            ),
            "blank_erase" => blank_erase(
                &mut surface,
                &mut canvas,
                &mut page,
                &view,
                &settings,
                index,
            ),
            "write" | "extend" => write(
                &mut surface,
                &mut canvas,
                &mut page,
                &view,
                &settings,
                index,
                size,
            ),
            "pan_inside" => pan(&mut surface, &page, index, false),
            "pan_across" => pan(&mut surface, &page, index, true),
            "zoom_rung" => zoom_rung(&mut surface, &mut canvas, &mut page, index),
            "undo" => undo_redo(
                &mut surface,
                &mut canvas,
                &mut page,
                &view,
                &settings,
                index,
                false,
            ),
            "redo" => undo_redo(
                &mut surface,
                &mut canvas,
                &mut page,
                &view,
                &settings,
                index,
                true,
            ),
            "commit_long" => write(
                &mut surface,
                &mut canvas,
                &mut page,
                &view,
                &settings,
                index,
                size,
            ),
            other => return Err(format!("unhandled action: {other}")),
        };

        // Restore: an append leaves its stroke open, so end it, then take it back. Every other action
        // finishes what it started, and this is the undo that puts the page back.
        if page.open().is_some() {
            page.consume(&[reading(Phase::Up, 0.0, 0.0, None)], &view, &settings);
        }
        if page.finished().len() > baseline_strokes {
            page.undo();
        }
        // The page is back as the sample found it. The change that brought it back is *not* handed to
        // the renderer — it was already drawn, measured, and is not what the next sample measures — so
        // the description is only brought level with the page.
        let _ = page.take_delta();
        canvas.ink.edit = page.revision();
        canvas.ink.strokes = page.finished().clone();
        // The undo took a stroke out of what the surface had drawn: the next rebuild is the safe answer,
        // and it is not this sample's cost.
        canvas.ink.delta = None;
        catch_up(&mut surface, &mut canvas, &mut page);
        rows.push(row);
        if let Some(window) = window {
            windows.push(window);
        }
    }

    let restored = digest(page.finished());
    if restored != before {
        // The state did not come back: report it rather than claim a measurement of something else.
        eprintln!("adapter: state was not restored ({before} -> {restored})");
        return Ok(1);
    }

    let memory = memory(&page);
    let body = serde_json::json!({
        "device": device,
        "commit": commit(),
        "release": !cfg!(debug_assertions),
        "fixture_digest": fixture_digest,
        "viewport_digest": viewport_digest,
        "warmups_completed": warmups,
        "observations": rows.iter().map(Observation::json).collect::<Vec<_>>(),
        "active_windows": windows.iter().map(ActiveWindow::json).collect::<Vec<_>>(),
        "memory": memory,
        "before_digest": before,
        "restored_digest": restored,
        "expected_after_digest": digest(page.finished()),
        "actual_after_digest": digest(page.finished()),
    });

    writeln!(out, "{body}").map_err(|e| e.to_string())?;
    Ok(0)
}

/// Draws the page through the surface with the model's current ink, and returns the frame's cost.
///
/// A caller that has not been keeping the description up to date uses this; it describes the page from
/// scratch, which is a renderer being told about the whole page and nothing about what changed. A caller
/// measuring an edit uses [`draw_after`] instead.
fn draw_frame(
    surface: &mut Surface,
    canvas: &mut Canvas,
    page: &mut InkDocument,
    zoom: f32,
    origin: (f32, f32),
) -> FrameCost {
    canvas.ink = canvas_of(page, zoom, origin).ink;
    frame(surface, canvas)
}

/// Draws the page through the description the caller already has, carrying the change it was just given.
///
/// The description is *reused*, which is what the app does and what an incremental renderer needs: a
/// fresh `Canvas` every frame would be a renderer that has never seen this page before, and every frame
/// would be its first. Only the fields an edit can change are moved.
fn draw_after(
    surface: &mut Surface,
    canvas: &mut Canvas,
    page: &mut InkDocument,
    zoom: f32,
    origin: (f32, f32),
) -> FrameCost {
    let change = page.take_delta();
    let fresh = canvas_with(page, zoom, origin, change);

    canvas.ink.strokes = fresh.ink.strokes;
    canvas.ink.edit = fresh.ink.edit;
    canvas.ink.detail = fresh.ink.detail;
    // A frame that saw no edit describes the page it is drawing, with no change to report: a renderer
    // given `None` and a revision it already has has nothing to do, which is what keeps the frames
    // between edits from being read as gaps.
    canvas.ink.delta = fresh.ink.delta;
    canvas.ink.open = fresh.ink.open;
    canvas.ink.open_id = fresh.ink.open_id;

    frame(surface, canvas)
}

/// Keeps the description's page in step without telling the renderer about an edit.
///
/// What the restore between samples needs: undo and redo happen, and the *next* sample's edit is the
/// one the renderer has to hear about. The undo's own change is deliberately not handed over — it is
/// not being measured, and a renderer given it would apply it and then read the next change as the one
/// that follows, which it does not.
fn keep_in_step(canvas: &mut Canvas, page: &mut InkDocument) {
    let _ = page.take_delta();
    canvas.ink.edit = page.revision();
    canvas.ink.strokes = page.finished().clone();
    canvas.ink.detail = page.detailed_at().map(f32::to_bits);
}

/// One more frame, drawn after the sample — past the restore, so it is the *restored* page the next
/// sample extends.
///
/// A sample's append is undone as soon as it is measured, and an undo takes a stroke out of what the
/// surface has. The next append is only an append onto a page the surface already has, so the surface
/// has to be brought up to date first; this is that frame, and it is not part of any sample's timing.
fn catch_up(surface: &mut Surface, canvas: &mut Canvas, page: &mut InkDocument) {
    keep_in_step(canvas, page);
    // The undo took a stroke out of what the surface had drawn: the next rebuild is the safe answer,
    // and it is not this sample's cost.
    canvas.ink.delta = None;
    let _ = frame(surface, canvas);
}

/// The sequence of readings one append sends: a short stroke over the control page's top-left.
fn append_readings() -> Vec<Reading> {
    let mut readings = vec![reading(Phase::Down, 6.0, 6.0, Some(0.5))];
    for step in 1..=3 {
        readings.push(reading(Phase::Move, 6.0 + step as f32, 6.0, Some(0.5)));
    }
    readings.push(reading(Phase::Up, 10.0, 6.0, None));
    readings
}

/// Case A's append: commit one short stroke, then draw the frame that presents it.
fn append(
    surface: &mut Surface,
    canvas: &mut Canvas,
    page: &mut InkDocument,
    view: &InkTransform,
    settings: &Settings,
    index: usize,
) -> (Observation, Option<ActiveWindow>) {
    let ink_before = page.finished().len();
    let readings = append_readings();

    let input = Instant::now();
    let input_ns = input.elapsed().as_nanos() as u64;
    page.set_mode(Tool::Pen);
    page.consume(&readings, view, settings);
    let commit_ns = input.elapsed().as_nanos() as u64;

    // What the edit touched: the strokes the pen's path was near, and the one it appended.
    let inspected = scanned(&page, &readings);
    let appended = page.finished().get(ink_before).cloned();
    let appended_vertices = appended.as_deref().map_or(0, vertices_of);
    let appended_vertex_bytes = appended_vertices * VERTEX_BYTES;

    // The change the edit published, handed to the frame with the page: this is the notification the
    // explicit-rebuild contract is about, and without it every frame is a renderer that cannot say
    // what happened and builds the page.
    let cost = draw_after(surface, canvas, page, 1.0, (MARGIN, MARGIN));

    // The pen has lifted: the open stroke is over, and the next frame is the page without one. Both
    // (it was measured) and (the tear-down that would inflate the next sample's first frame) make it
    // wrong to carry the stroke into the description.
    canvas.ink.open = None;
    canvas.ink.open_id = 0;
    keep_in_step(canvas, page);

    let submit_ns = commit_ns;
    let gpu_done_ns = commit_ns + cost.drawable_wait_ns + cost.cpu_work_ns + cost.gpu_ns;
    let presented_ns = gpu_done_ns;

    // The appended stroke's own geometry is what was regenerated — nothing else was re-derived.
    let observation = Observation {
        index,
        edit_id: (index + 1) as u64,
        input_ns,
        commit_ns,
        submit_ns,
        gpu_done_ns,
        presented_ns,
        inspected,
        index_visits: inspected,
        regenerated: appended_vertices,
        submitted_vertices: cost.vertices,
        uploaded_bytes: cost.uploaded_bytes,
        gpu_ns: cost.gpu_ns,
        appended_vertices,
        appended_vertex_bytes,
        deferred_ns: 0,
        cpu_work_ns: cost.cpu_work_ns,
        drawable_wait_ns: cost.drawable_wait_ns,
        coalesced: 0,
    };

    (observation, Some(window(index, cost.drawn)))
}

/// How many finished strokes a set of readings' box could touch — the bookkeeping an edit does.
///
/// Measured by asking each stroke whether it is inside the readings' bounding box: that is exactly the
/// set a hit test or a cull has to look at, and it is counted rather than guessed.
fn scanned(page: &InkDocument, readings: &[Reading]) -> u64 {
    let (min_x, max_x, min_y, max_y) = readings.iter().fold(
        (f32::MAX, f32::MIN, f32::MAX, f32::MIN),
        |(ax, bx, ay, by), reading| {
            (
                ax.min(reading.pixel.x),
                bx.max(reading.pixel.x),
                ay.min(reading.pixel.y),
                by.max(reading.pixel.y),
            )
        },
    );
    let rect =
        note_format::ink::Bounds::new((min_x - 8.0, min_y - 8.0), (max_x + 8.0, max_y + 8.0));
    page.finished()
        .iter()
        .filter(|stroke| stroke.visible_in(rect))
        .count() as u64
}

/// Case A's blank erase: sweep the eraser across empty paper, which must rebuild no ink at all.
fn blank_erase(
    surface: &mut Surface,
    canvas: &mut Canvas,
    page: &mut InkDocument,
    view: &InkTransform,
    settings: &Settings,
    index: usize,
) -> (Observation, Option<ActiveWindow>) {
    let ink_before = page.finished().len();

    // Empty paper: far below the control page's ink, inside the sheet.
    let readings = vec![
        reading(Phase::Down, 20.0, 600.0, Some(0.5)),
        reading(Phase::Move, 40.0, 600.0, Some(0.5)),
        reading(Phase::Up, 60.0, 600.0, None),
    ];

    let input = Instant::now();
    page.set_mode(Tool::Eraser);
    page.consume(&readings, view, settings);
    let commit_ns = input.elapsed().as_nanos() as u64;

    // Nothing was erased: no stroke may have gone, and no geometry may be new.
    let erased = ink_before != page.finished().len();
    let inspected = scanned(&page, &readings);
    let cost = draw_frame(surface, canvas, page, 1.0, (MARGIN, MARGIN));

    let observation = Observation {
        index,
        edit_id: (index + 1) as u64,
        input_ns: input.elapsed().as_nanos() as u64,
        commit_ns,
        submit_ns: commit_ns,
        gpu_done_ns: commit_ns,
        presented_ns: commit_ns,
        inspected,
        index_visits: inspected,
        // A blank erase must rebuild nothing: these two are the gate, and they are measured, not set.
        regenerated: if erased { cost.vertices } else { 0 },
        submitted_vertices: cost.vertices,
        uploaded_bytes: if erased { cost.uploaded_bytes } else { 0 },
        gpu_ns: cost.gpu_ns,
        ..Default::default()
    };

    (observation, None)
}

/// Case C's write and D's extend: one measured batch of a long stroke.
///
/// `size` is C's visible stroke count or D's point count; the batch is at most a stroke's worth of points,
/// which is what one pen report is.
fn write(
    surface: &mut Surface,
    canvas: &mut Canvas,
    page: &mut InkDocument,
    view: &InkTransform,
    settings: &Settings,
    index: usize,
    size: usize,
) -> (Observation, Option<ActiveWindow>) {
    let points = size.clamp(2, 128).min(POINTS);
    page.set_mode(Tool::Pen);

    let mut batch: Vec<Reading> = vec![reading(Phase::Down, 300.0, 24.0, Some(0.5))];
    for step in 1..=points {
        batch.push(reading(
            Phase::Move,
            300.0 + step as f32 * STEP,
            24.0,
            Some(0.5),
        ));
    }

    let started = Instant::now();
    page.consume(&batch, view, settings);
    let commit_ns = started.elapsed().as_nanos() as u64;

    let regenerated = page.open().map_or(0, vertices_of);
    let cost = draw_frame(surface, canvas, page, 1.0, (MARGIN, MARGIN));
    let gpu_done_ns = commit_ns + cost.drawable_wait_ns + cost.cpu_work_ns + cost.gpu_ns;

    // Finish the stroke so the page is a page again and the edit can be taken back.
    page.consume(&[reading(Phase::Up, 300.0, 24.0, None)], view, settings);

    let observation = Observation {
        index,
        edit_id: (index + 1) as u64,
        input_ns: 0,
        commit_ns,
        submit_ns: commit_ns,
        gpu_done_ns,
        presented_ns: gpu_done_ns,
        regenerated,
        submitted_vertices: cost.vertices,
        uploaded_bytes: cost.uploaded_bytes,
        gpu_ns: cost.gpu_ns,
        cpu_work_ns: cost.cpu_work_ns,
        drawable_wait_ns: cost.drawable_wait_ns,
        ..Default::default()
    };

    (observation, Some(window(index, cost.drawn)))
}

/// A pan: move the sheet and draw. No ink is re-derived — that is what the row is for.
fn pan(
    surface: &mut Surface,
    page: &InkDocument,
    index: usize,
    across: bool,
) -> (Observation, Option<ActiveWindow>) {
    let shift = if across { SHEET.0 } else { 1.0 };
    let canvas = canvas_of(page, 1.0, (MARGIN + shift, MARGIN));

    let started = Instant::now();
    let cost = frame(surface, &canvas);
    let commit_ns = started.elapsed().as_nanos() as u64;
    let gpu_done_ns = commit_ns + cost.drawable_wait_ns + cost.cpu_work_ns + cost.gpu_ns;

    let observation = Observation {
        index,
        edit_id: (index + 1) as u64,
        input_ns: 0,
        commit_ns,
        submit_ns: commit_ns,
        gpu_done_ns,
        presented_ns: gpu_done_ns,
        regenerated: 0,
        submitted_vertices: cost.vertices,
        uploaded_bytes: cost.uploaded_bytes,
        gpu_ns: cost.gpu_ns,
        cpu_work_ns: cost.cpu_work_ns,
        drawable_wait_ns: cost.drawable_wait_ns,
        ..Default::default()
    };

    (observation, Some(window(index, cost.drawn)))
}

fn measure(
    action: &str,
    case: &str,
    size: usize,
    samples: usize,
    warmups: usize,
    profile: &str,
    device: serde_json::Value,
    out: &mut impl Write,
) -> Result<i32, String> {
    match action {
        "traverse" => traverse(case, size, device, out),
        "cold_open" => cold_open(size, device, out),
        "append" | "blank_erase" | "write" | "pan_inside" | "extend" | "commit_long"
        | "pan_across" | "zoom_rung" | "undo" | "redo" => {
            interactive(action, case, size, samples, warmups, profile, device, out)
        }
        "move_selection" => {
            // No modifier bit exists on a reading, so the app has no pen path for a selection drag. A
            // reported figure would be invented; this is the honest answer.
            eprintln!("adapter: move_selection has no pen path in this build");
            Ok(2)
        }
        other => Err(format!("unknown action: {other}")),
    }
}

/// A zoom that crosses a detail rung: every stroke's outline is rebuilt, and the next frame draws it.
fn zoom_rung(
    surface: &mut Surface,
    canvas: &mut Canvas,
    page: &mut InkDocument,
    index: usize,
) -> (Observation, Option<ActiveWindow>) {
    // Two different rungs, alternated, so every call is a real rebuild rather than a cache hit.
    let zoom = if index % 2 == 0 { 2.25 } else { 1.0 };

    let started = Instant::now();
    page.set_zoom(zoom);
    let commit_ns = started.elapsed().as_nanos() as u64;

    // The same description the app keeps, with the sheet drawn at the new size: a zoom is a transform
    // and a rung, not a new canvas.
    canvas.ink.zoom = zoom;
    canvas.ink.strokes = page.finished().clone();
    canvas.ink.detail = page.detailed_at().map(f32::to_bits);
    canvas.ink.delta = page.take_delta();
    let cost = frame(surface, canvas);
    let gpu_done_ns = commit_ns + cost.drawable_wait_ns + cost.cpu_work_ns + cost.gpu_ns;

    let observation = Observation {
        index,
        edit_id: (index + 1) as u64,
        input_ns: 0,
        commit_ns,
        submit_ns: commit_ns,
        gpu_done_ns,
        presented_ns: gpu_done_ns,
        regenerated: page
            .finished()
            .iter()
            .map(|stroke| vertices_of(stroke))
            .sum(),
        submitted_vertices: cost.vertices,
        uploaded_bytes: cost.uploaded_bytes,
        gpu_ns: cost.gpu_ns,
        cpu_work_ns: cost.cpu_work_ns,
        drawable_wait_ns: cost.drawable_wait_ns,
        ..Default::default()
    };

    (observation, Some(window(index, cost.drawn)))
}

/// An undo (or a redo) of one appended stroke, and the frame that presents the change.
fn undo_redo(
    surface: &mut Surface,
    canvas: &mut Canvas,
    page: &mut InkDocument,
    view: &InkTransform,
    settings: &Settings,
    index: usize,
    redo: bool,
) -> (Observation, Option<ActiveWindow>) {
    page.set_mode(Tool::Pen);
    page.consume(&append_readings(), view, settings);
    if redo {
        // A redone edit first has to be an undone one.
        page.undo();
    }

    let started = Instant::now();
    if redo {
        page.redo();
    } else {
        page.undo();
    }
    let commit_ns = started.elapsed().as_nanos() as u64;

    let cost = draw_frame(surface, canvas, page, 1.0, (MARGIN, MARGIN));
    let gpu_done_ns = commit_ns + cost.drawable_wait_ns + cost.cpu_work_ns + cost.gpu_ns;

    let observation = Observation {
        index,
        edit_id: (index + 1) as u64,
        input_ns: 0,
        commit_ns,
        submit_ns: commit_ns,
        gpu_done_ns,
        presented_ns: gpu_done_ns,
        submitted_vertices: cost.vertices,
        uploaded_bytes: cost.uploaded_bytes,
        gpu_ns: cost.gpu_ns,
        cpu_work_ns: cost.cpu_work_ns,
        drawable_wait_ns: cost.drawable_wait_ns,
        ..Default::default()
    };

    (observation, Some(window(index, cost.drawn)))
}

/// One active window: the refresh slot of this operation, and whether it was drawn in it.
fn window(index: usize, drawn: bool) -> ActiveWindow {
    // Slot serials count frames from the process start; one operation is one active window of one slot,
    // and a frame the layer refused to take is a slot with no presentation.
    let slot = (index + 1) as u64;
    ActiveWindow {
        first_slot: slot,
        last_slot: slot,
        presented_slots: if drawn { vec![slot] } else { Vec::new() },
    }
}

/// The process's memory, as the harness reads it.
fn memory(page: &InkDocument) -> serde_json::Value {
    let rss = resident_bytes().unwrap_or(0);
    serde_json::json!({
        "peak_rss_bytes": rss.max(1),
        "post_close_rss_bytes": rss,
        "cache_bytes": 0,
        "resident_pages": page.finished().len() as u64,
    })
}

/// This process's resident size, from `proc_pidinfo`'s task info.
fn resident_bytes() -> Option<u64> {
    // SAFETY: `proc_pidinfo` writes a `proc_taskinfo` and reports how many bytes it wrote; the struct is
    // the layout `libc` declares, and the call is about this process's own id.
    unsafe {
        let mut info = std::mem::zeroed::<libc::proc_taskinfo>();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let written = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        (written == size).then_some(info.pti_resident_size as u64)
    }
}

/// Case B's page tour: visit `min(20, size)` distinct pages, then return to the first.
///
/// Two full tours, so the swap figure has a cycle to compare against its own beginning — the harness
/// reads `swap_out_bytes` as three samples for exactly that reason.
fn traverse(
    case: &str,
    size: usize,
    device: serde_json::Value,
    out: &mut impl Write,
) -> Result<i32, String> {
    let _ = case;
    let docs = note_pages(size.max(1));
    let before = digest_owned(&docs[0]);
    let visit = size.min(TOUR);

    // The tour the harness reads: distinct pages, then the first again.
    let tour: Vec<usize> = (0..visit).chain(std::iter::once(0)).collect();

    let mut swap = vec![swap_out_bytes().unwrap_or(0)];
    for _ in 0..2 {
        for &page in &tour {
            // A visit holds the page's ink, which is what a page cache grows by.
            let strokes = &docs[page];
            std::hint::black_box(strokes.len());
        }
        swap.push(swap_out_bytes().unwrap_or(0));
    }

    let body = serde_json::json!({
        "device": device,
        "commit": commit(),
        "release": !cfg!(debug_assertions),
        "fixture_digest": before,
        "viewport_digest": format!("{size}p@1.0x"),
        "warmups_completed": 0,
        "steps": tour,
        "reopened_digest": before,
        "swap_out_bytes": swap,
        "before_digest": before,
        "restored_digest": before,
        "expected_after_digest": before,
        "actual_after_digest": before,
        "memory": memory(&InkDocument::from_strokes(docs[0].clone())),
    });

    writeln!(out, "{body}").map_err(|e| e.to_string())?;
    Ok(0)
}

/// Cumulative bytes this process has written to disk, from `rusage` — the swap figure's floor.
fn swap_out_bytes() -> Option<u64> {
    // SAFETY: `getrusage` writes into a struct this owns, and `RUSAGE_SELF` asks about this process.
    unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            Some((usage.ru_oublock as u64).saturating_mul(512))
        } else {
            None
        }
    }
}

/// Case B's cold open: the time from reading the page to its ink being built — the first usable ink.
fn cold_open(size: usize, device: serde_json::Value, out: &mut impl Write) -> Result<i32, String> {
    let docs = note_pages(size.max(1));

    let started = Instant::now();
    let mut page = InkDocument::from_strokes(docs[0].clone());
    page.set_zoom(1.0);
    let cold_usable_ns = started.elapsed().as_nanos() as u64;

    let digest = digest(page.finished());
    let body = serde_json::json!({
        "device": device,
        "commit": commit(),
        "release": !cfg!(debug_assertions),
        "fixture_digest": digest,
        "viewport_digest": format!("{size}p@1.0x"),
        "warmups_completed": 0,
        "cold_usable_ns": cold_usable_ns,
        "before_digest": digest,
        "restored_digest": digest,
        "expected_after_digest": digest,
        "actual_after_digest": digest,
        "memory": memory(&page),
    });

    writeln!(out, "{body}").map_err(|e| e.to_string())?;
    Ok(0)
}
