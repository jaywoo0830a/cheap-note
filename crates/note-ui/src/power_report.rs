//! What a rebuild costs, in joules, measured rather than guessed.
//!
//! ## The question this answers
//!
//! The bench answers *how long* a frame or a rebuild takes. This answers *how many joules, and how many
//! operations per joule* — the question a rung rebuild raises, and the one the bench cannot answer by
//! itself, because a duration and an energy are different claims: a rebuild can be quick and expensive
//! (spread over many cores, or holding a package in its high-power state) or slow and cheap.
//!
//! ## The command
//!
//! ```text
//! cheap-note --power-report [--format table|json]
//!     [--workload rung|frame|both] [--strokes 1000,8182]
//!     [--repeats 20] [--warmup 2] [--threads N] [--frames 30]
//!     [--energy-source auto|meter|platform|none] [--meter meter.csv] [--meter-offset 0]
//!     [--require-energy] [--max-ms N] [--max-joules-per-op N] [--min-ops-per-joule N]
//! ```
//!
//! It runs the whole workload **in one process**, as the memory report does: a meter around two
//! processes would measure the operating system. `--help` prints a usage block and exits `0`; a usage
//! mistake — an unknown flag, a missing value, two flags that contradict — exits `2`.
//!
//! ## What is and is not in this file
//!
//! This module is the **portable core**: the arguments, the two workloads' arithmetic, the energy
//! sources the report can read on its own (a CSV meter, and the platform's own counter through the
//! [`SystemStats`] seam), and the report. The one thing it does not do is *draw* the `frame` phase
//! through a platform canvas — that surface belongs to an app, so [`plan`] leaves the frame phase to the
//! caller, and a caller with no canvas says why it was not measured. The energy a canvas *does* consume
//! is still in the window, because CPU time and the platform counter are read around the whole phase.
//!
//! ## The three rules the other commands follow, and so does this one
//!
//! 1. **Absence is not zero.** Windows has no package-energy counter, so `joules_per_op` is `null`, not
//!    `0`. A budget on a quantity nothing measured is a `skipped` run, never a pass.
//! 2. **`cpu` is never printed as a joule.** It is core-time — all threads summed — and is a proxy for
//!    work; `basis` says `cpu-time` when no energy source was read, so a script can tell a joule from a
//!    proxy.
//! 3. **A number that could not be taken stays absent, with its reason.** `unmeasured` names a column
//!    only when *no* row could fill it; the column stays in `columns` either way.

use std::io::Write;
use std::time::Instant;

use crate::platform::SystemStats;
use crate::report::{Command, Format, Report, Row, Value, Verdict};

/// The flag that asks for this report rather than a window.
pub const FLAG: &str = "--power-report";

/// The energy columns, `null` together when no energy source was read.
pub const ENERGY_COLUMNS: [&str; 2] = ["joules_per_op", "ops_per_joule"];

/// The columns, fixed and in this order — the same on every platform.
pub const COLUMNS: [&str; 10] = [
    "strokes",
    "phase",
    "threads",
    "count",
    "wall",
    "median",
    "worst",
    "cpu",
    "joules_per_op",
    "ops_per_joule",
];

/// The default stroke counts: below a thousand the rebuild is under a millisecond and a joule reading
/// of it is mostly the meter's noise.
pub const DEFAULT_STROKES: [usize; 2] = [1_000, 8_182];

/// The default number of timed rebuilds per stroke count.
pub const DEFAULT_REPEATS: usize = 20;

/// The default number of discarded rebuilds in each direction before the window opens.
pub const DEFAULT_WARMUP: usize = 2;

/// The default number of warm frames the `frame` phase measures.
pub const DEFAULT_FRAMES: usize = 30;

/// The fewest repeats a run may ask for: a median of one is a mean, and a mean of one is a guess.
pub const MIN_REPEATS: usize = 3;

/// The two rungs the rung workload alternates between.
///
/// The detail ladder rounds up, so `1.0` and `2.25` are two different rungs and every call is a real
/// rebuild rather than a cache hit — which is what `measures_the_rung_costs` established and what makes
/// this workload repeatable.
pub const RUNGS: (f32, f32) = (1.0, 2.25);

/// What the command line asked for, after every default has been applied.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// How to write the report.
    pub format: Format,
    /// Which phases to run.
    pub workload: Workload,
    /// The page sizes, in strokes.
    pub strokes: Vec<usize>,
    /// Timed rebuilds per stroke count (the `rung` phase); at least [`MIN_REPEATS`].
    pub repeats: usize,
    /// Discarded rebuilds in each direction before the window opens.
    pub warmup: usize,
    /// The scoped Rayon pool size, when `--threads` was given.
    pub threads: Option<usize>,
    /// Warm frames the `frame` phase measures.
    pub frames: usize,
    /// Which energy source to read.
    pub energy: EnergyChoice,
    /// A meter CSV path, when `--meter` was given.
    pub meter: Option<String>,
    /// Seconds added to every meter sample, for a meter that keeps its own clock.
    pub meter_offset: f64,
    /// Whether a run with no energy is a skip rather than a pass.
    pub require_energy: bool,
    /// The budgets, applied after the phases run.
    pub budgets: Budgets,
}

/// Which workloads a run measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workload {
    /// The view's half of a detail rung only.
    Rung,
    /// Warm frames of the same page only.
    Frame,
    /// Both, rung first, one row per phase.
    Both,
}

impl Workload {
    /// The value the `--workload` flag takes.
    pub fn name(self) -> &'static str {
        match self {
            Workload::Rung => "rung",
            Workload::Frame => "frame",
            Workload::Both => "both",
        }
    }

    /// Reads `--workload`'s value, or `None` when it names no workload this command runs.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "rung" => Some(Workload::Rung),
            "frame" => Some(Workload::Frame),
            "both" => Some(Workload::Both),
            _ => None,
        }
    }

    /// Whether the run measures `phase`.
    pub fn includes(self, phase: PhaseKind) -> bool {
        match phase {
            PhaseKind::Rung => matches!(self, Workload::Rung | Workload::Both),
            PhaseKind::Frame => matches!(self, Workload::Frame | Workload::Both),
        }
    }
}

/// One of the two phases a row names in its `phase` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseKind {
    /// A detail rung rebuild: `set_zoom` across a rung boundary.
    Rung,
    /// Warm frames of the same page, drawn through a platform canvas.
    Frame,
}

impl PhaseKind {
    /// The word the `phase` column prints.
    pub fn name(self) -> &'static str {
        match self {
            PhaseKind::Rung => "rung",
            PhaseKind::Frame => "frame",
        }
    }
}

/// Where the joules are asked to come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EnergyChoice {
    /// `meter` if `--meter` was given, else `platform` when it exists, else `none`.
    #[default]
    Auto,
    /// An external wall meter or smart plug, read from the CSV `--meter` names.
    Meter,
    /// The platform's own counter, read through the [`SystemStats`] seam.
    Platform,
    /// No energy at all; `wall` and `cpu` only.
    None,
}

impl EnergyChoice {
    /// The value the `--energy-source` flag takes.
    pub fn name(self) -> &'static str {
        match self {
            EnergyChoice::Auto => "auto",
            EnergyChoice::Meter => "meter",
            EnergyChoice::Platform => "platform",
            EnergyChoice::None => "none",
        }
    }

    /// Reads `--energy-source`'s value.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(EnergyChoice::Auto),
            "meter" => Some(EnergyChoice::Meter),
            "platform" => Some(EnergyChoice::Platform),
            "none" => Some(EnergyChoice::None),
            _ => None,
        }
    }
}

/// The budget flags, all optional, checked after the phases run.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Budgets {
    /// `--max-ms`: the most a row's `wall` may be, in milliseconds.
    pub max_ms: Option<f64>,
    /// `--max-joules-per-op`: the most a row's `joules_per_op` may be, in joules.
    pub max_joules_per_op: Option<f64>,
    /// `--min-ops-per-joule`: the fewest operations a joule must buy.
    pub min_ops_per_joule: Option<f64>,
}

impl Budgets {
    /// Whether any budget was asked for.
    pub fn any(&self) -> bool {
        self.max_ms.is_some()
            || self.max_joules_per_op.is_some()
            || self.min_ops_per_joule.is_some()
    }
}

/// One reading from an external meter: Unix epoch seconds, and watts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterSample {
    /// Unix epoch seconds.
    pub epoch_s: f64,
    /// Instantaneous power, in watts.
    pub watts: f64,
}

/// A parsed meter file, or the reason it could not be used.
///
/// The format is the contract's: a `epoch_s,watts` header and rows of the same shape. Parsing is strict
/// about the *shape* — a bad row is a `meter-invalid` skip — and the run integrates whatever window it
/// can from the samples the run's own clock falls inside.
#[derive(Clone, Debug, PartialEq)]
pub enum MeterState {
    /// A usable meter, with its samples in the order they were read.
    Ready(Vec<MeterSample>),
    /// A meter that could not be read or parsed, and why. A `skipped` run, never a pass.
    Invalid(&'static str),
}

/// Parses `text` as a meter CSV.
///
/// A header line that begins with `epoch` is skipped; a blank line is ignored; a row is two comma-
/// separated numbers, the first non-negative and neither NaN. A sample whose timestamp is not greater
/// than the one before it is out of order and the file is `meter-invalid` — the integration needs the
/// samples sorted, and a file that is not sorted is a meter or a reader the report cannot trust.
pub fn parse_meter(text: &str) -> MeterState {
    let mut samples: Vec<MeterSample> = Vec::new();

    for (at, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // The header is the first line, and only the first line.
        if at == 0 && line.to_ascii_lowercase().starts_with("epoch") {
            continue;
        }

        let mut fields = line.split(',');
        let (Some(epoch), Some(watts)) = (fields.next(), fields.next()) else {
            return MeterState::Invalid("meter-invalid");
        };

        let (Ok(epoch_s), Ok(watts)) = (epoch.trim().parse::<f64>(), watts.trim().parse::<f64>())
        else {
            return MeterState::Invalid("meter-invalid");
        };

        if !epoch_s.is_finite() || epoch_s < 0.0 || !watts.is_finite() || watts < 0.0 {
            return MeterState::Invalid("meter-invalid");
        }

        if samples.last().is_some_and(|last| epoch_s <= last.epoch_s) {
            return MeterState::Invalid("meter-invalid");
        }

        samples.push(MeterSample { epoch_s, watts });
    }

    if samples.len() < 2 {
        return MeterState::Invalid("meter-window-too-short");
    }

    MeterState::Ready(samples)
}

/// The outcome of integrating a meter over one window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Integration {
    /// The window's energy, in joules, and how many samples fell inside it.
    Joules {
        /// Energy over the window, `∫ power dt`, by the trapezoid rule.
        joules: f64,
        /// Samples inside the window.
        samples: usize,
    },
    /// The window could not be integrated, and why. A `skipped` run.
    Skipped(&'static str),
}

/// Integrates `samples` over the closed window `[start_s, end_s]`, with the meter's own clock offset
/// already applied to `start_s` and `end_s`.
///
/// At least two samples must fall inside the window: one sample is a fact about a moment, not an
/// integral, and a window with fewer than two is `meter-window-too-short`. An integrated energy of
/// zero is `meter-invalid` — a zero joule count is not an infinite efficiency.
pub fn integrate(samples: &[MeterSample], start_s: f64, end_s: f64) -> Integration {
    let inside: Vec<MeterSample> = samples
        .iter()
        .copied()
        .filter(|sample| sample.epoch_s >= start_s && sample.epoch_s <= end_s)
        .collect();

    if inside.len() < 2 {
        return Integration::Skipped("meter-window-too-short");
    }

    // The trapezoid rule, seconds × watts = joules.
    let mut joules = 0.0;
    for pair in inside.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let dt = b.epoch_s - a.epoch_s;
        if dt <= 0.0 {
            return Integration::Skipped("meter-invalid");
        }
        joules += 0.5 * (a.watts + b.watts) * dt;
    }

    if joules <= 0.0 || !joules.is_finite() {
        return Integration::Skipped("meter-invalid");
    }

    Integration::Joules {
        joules,
        samples: inside.len(),
    }
}

/// A positive number, or a sentence naming the flag that wanted it.
fn positive(flag: &str, text: &str) -> Result<f64, String> {
    let value = text
        .parse::<f64>()
        .map_err(|_| format!("{flag} must be a number: {text}"))?;

    if !value.is_finite() || value <= 0.0 {
        return Err(format!("{flag} must be a positive number: {text}"));
    }

    Ok(value)
}

/// A whole number, or a sentence naming the flag that wanted it.
fn whole(flag: &str, text: &str) -> Result<usize, String> {
    text.parse::<usize>()
        .map_err(|_| format!("{flag} must be a whole number: {text}"))
}

/// A comma-separated list of stroke counts.
fn parse_strokes(text: &str) -> Result<Vec<usize>, String> {
    let mut counts = Vec::new();

    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        counts.push(whole("--strokes", part)?);
    }

    if counts.is_empty() {
        return Err(String::from("--strokes named no counts"));
    }

    Ok(counts)
}

/// What the command line accepts, for `--help` and for a mistake.
pub fn usage() -> String {
    format!(
        "usage: cheap-note {FLAG} [--format table|json]\n\
         \x20   [--workload rung|frame|both] [--strokes 1000,8182]\n\
         \x20   [--repeats 20] [--warmup 2] [--threads N] [--frames 30]\n\
         \x20   [--energy-source auto|meter|platform|none] [--meter meter.csv] [--meter-offset 0]\n\
         \x20   [--require-energy] [--max-ms N] [--max-joules-per-op N] [--min-ops-per-joule N]\n\
         \n\
         Measures what a detail rung rebuild costs — wall time, process CPU time, and joules when an\n\
         energy source is available — and the operations per joule of it. `--workload frame` measures\n\
         warm frames instead; `both` runs rung then frame for each stroke count.\n\
         \n\
         An energy budget or --require-energy makes a run with no energy a skip (exit 2), never a\n\
         pass. Windows has no package-energy counter: read a meter, or report CPU time. Run it in\n\
         release.\n"
    )
}

/// Reads the command line this command owns — everything after `--power-report`, with `--format`
/// already taken off by [`crate::report::Options`].
///
/// Every rule about a value lives here, so a mistake is caught before any workload runs and gets the
/// same sentence from a test as from a person typing it: a missing value, a number that is not positive,
/// a `--repeats` below [`MIN_REPEATS`], `--energy-source meter` with no `--meter`, and `--require-energy`
/// together with `--energy-source none` — the last two ask for opposite things.
///
/// The one "error" that is not a mistake is `--help`: it arrives as the sentence `"help"`, so the caller
/// can tell it from a bad flag and print the usage with exit `0` rather than `2`.
pub fn plan(args: &[String], format: Format) -> Result<Plan, String> {
    let mut plan = Plan {
        format,
        workload: Workload::Rung,
        strokes: DEFAULT_STROKES.to_vec(),
        repeats: DEFAULT_REPEATS,
        warmup: DEFAULT_WARMUP,
        threads: None,
        frames: DEFAULT_FRAMES,
        energy: EnergyChoice::Auto,
        meter: None,
        meter_offset: 0.0,
        require_energy: false,
        budgets: Budgets::default(),
    };

    let mut at = 0;
    while at < args.len() {
        let arg = args[at].as_str();

        // A `--flag=value` form and a `--flag value` form both work; the `=` is peeled off here so the
        // match below sees only the flag name.
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg, None),
        };

        // Reads this flag's value: the `=` part when there was one, else the next argument.
        macro_rules! value {
            () => {
                match &inline {
                    Some(value) => value.clone(),
                    None => {
                        at += 1;
                        args.get(at)
                            .cloned()
                            .ok_or_else(|| format!("{name} needs a value"))?
                    }
                }
            };
        }

        match name {
            "--help" | "-h" => return Err(String::from("help")),
            "--workload" => {
                let value = value!();
                plan.workload = Workload::from_name(&value)
                    .ok_or_else(|| format!("unknown workload: {value} (rung, frame or both)"))?;
            }
            "--strokes" => plan.strokes = parse_strokes(&value!())?,
            "--repeats" => {
                let value = value!();
                let repeats = whole(name, &value)?;
                if repeats < MIN_REPEATS {
                    return Err(format!(
                        "--repeats must be at least {MIN_REPEATS}, because a median of one is a guess"
                    ));
                }
                plan.repeats = repeats;
            }
            "--warmup" => plan.warmup = whole(name, &value!())?,
            "--threads" => {
                let value = value!();
                let threads = whole(name, &value)?;
                if threads == 0 {
                    return Err(String::from("--threads must be at least one"));
                }
                plan.threads = Some(threads);
            }
            "--frames" => {
                let value = value!();
                let frames = whole(name, &value)?;
                if frames == 0 {
                    return Err(String::from("--frames must be at least one"));
                }
                plan.frames = frames;
            }
            "--energy-source" => {
                let value = value!();
                plan.energy = EnergyChoice::from_name(&value).ok_or_else(|| {
                    format!("unknown energy source: {value} (auto, meter, platform or none)")
                })?;
            }
            "--meter" => plan.meter = Some(value!()),
            "--meter-offset" => {
                let value = value!();
                let offset = value
                    .parse::<f64>()
                    .map_err(|_| format!("--meter-offset must be a number of seconds: {value}"))?;
                if !offset.is_finite() {
                    return Err(format!("--meter-offset must be finite: {value}"));
                }
                plan.meter_offset = offset;
            }
            "--require-energy" => plan.require_energy = true,
            "--max-ms" => plan.budgets.max_ms = Some(positive(name, &value!())?),
            "--max-joules-per-op" => {
                plan.budgets.max_joules_per_op = Some(positive(name, &value!())?);
            }
            "--min-ops-per-joule" => {
                plan.budgets.min_ops_per_joule = Some(positive(name, &value!())?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }

        at += 1;
    }

    // The two contradictions and the one missing dependency, checked after the whole line is read so
    // that the order the flags were typed in cannot matter.
    if plan.energy == EnergyChoice::Meter && plan.meter.is_none() {
        return Err(String::from(
            "--energy-source meter needs --meter <meter.csv>",
        ));
    }
    if plan.require_energy && plan.energy == EnergyChoice::None {
        return Err(String::from(
            "--require-energy and --energy-source none ask for opposite things",
        ));
    }

    Ok(plan)
}

// ── The workloads ────────────────────────────────────────────────────────────────────────────────────
//
// Everything above this line is the contract: what the command accepts, and how a CSV meter is read.
// Everything below is the measurement, kept behind [`run`] so the apps see one entry point.

use std::time::Duration;

use crate::ink::InkTransform;
use crate::ink_layer::canvas::{Ink, Rect};
use crate::ink_layer::Canvas;
use crate::input::{Phase, Point, Reading};
use note_format::settings::Settings;

/// One phase's result, already reduced: every number is **per operation** except `count`.
#[derive(Clone, Debug, PartialEq)]
pub struct PhaseResult {
    /// How many operations were timed (`repeats` for `rung`, `frames` for `frame`).
    pub count: usize,
    /// Mean wall-clock time per operation, in milliseconds.
    pub wall_ms: f64,
    /// Median wall-clock time per operation, in milliseconds.
    pub median_ms: f64,
    /// The slowest operation, in milliseconds — a mean hides the one a hand would feel.
    pub worst_ms: f64,
    /// Mean process CPU time per operation, in milliseconds of core-time, when it was measured.
    pub cpu_ms: Option<f64>,
    /// Mean energy per operation, in joules, when an energy source was read.
    pub joules_per_op: Option<f64>,
    /// The pool size the phase used (`None` for `frame`, which does not parallelise).
    pub threads: Option<usize>,
}

impl PhaseResult {
    /// Operations per joule, `1 / joules_per_op`, or `None` with it.
    pub fn ops_per_joule(&self) -> Option<f64> {
        self.joules_per_op
            .filter(|joules| *joules > 0.0)
            .map(|joules| 1.0 / joules)
    }

    /// The row these figures make.
    pub fn row(&self, strokes: usize, phase: PhaseKind) -> Row {
        let joules = |value: Option<f64>| match value {
            Some(value) => Value::Joules(value),
            None => Value::Absent("no-energy-source"),
        };

        Row::new(
            format!("{strokes} strokes · {}", phase.name()),
            vec![
                Value::Count(strokes as u64),
                Value::Text(phase.name()),
                match self.threads {
                    Some(threads) => Value::Count(threads as u64),
                    None => Value::Absent("not-parallel"),
                },
                Value::Count(self.count as u64),
                Value::Millis(self.wall_ms),
                Value::Millis(self.median_ms),
                Value::Millis(self.worst_ms),
                match self.cpu_ms {
                    Some(ms) => Value::Millis(ms),
                    None => Value::Absent("no-cpu-time"),
                },
                joules(self.joules_per_op),
                joules(self.ops_per_joule()),
            ],
        )
    }
}

/// The result of measuring one page's one phase, or the reason it could not be.
#[derive(Clone, Debug, PartialEq)]
pub enum Measured {
    /// A phase that ran.
    Done(PhaseResult),
    /// A phase that could not be measured, and why. A `skipped` run.
    Skipped(&'static str),
}

/// A page of `count` synthetic strokes: the 40-point, 2 px-apart strokes `written_page` writes in the
/// ink tests, so this measures the page `measures_the_rung_costs` established.
///
/// Deterministic — no clock, no random source — so two runs are comparable and a change in the numbers
/// is a change in the code rather than in the page.
pub fn synthetic_page(count: usize) -> crate::ink::InkDocument {
    use crate::ink::InkDocument;

    let mut ink = InkDocument::default();
    let settings = Settings {
        smoothing_ms: 0.0,
        ..Settings::default()
    };

    for index in 0..count {
        let x = (index % 20) as f32 * 40.0;
        let y = (index / 20) as f32 * 60.0;
        let mut samples = Vec::with_capacity(41);
        samples.push(reading(Phase::Down, x, y, Some(0.5)));

        for step in 1..40 {
            samples.push(reading(
                Phase::Move,
                x + step as f32 * 2.0,
                y + (step % 7) as f32,
                Some(0.5),
            ));
        }

        samples.push(reading(Phase::Up, x + 80.0, y, None));
        ink.consume(&samples, &InkTransform::identity(), &settings);
    }

    ink
}

/// One reading at `(x, y)`, from pointer 7.
fn reading(phase: Phase, x: f32, y: f32, pressure: Option<f32>) -> Reading {
    Reading {
        phase,
        id: 7,
        pixel: Point::new(x, y),
        pressure,
        ..Reading::default()
    }
}

/// Mean, median and worst of a set of durations, in milliseconds.
fn summarise(samples: &[Duration]) -> (f64, f64, f64) {
    let mut ordered: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1_000.0).collect();
    ordered.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let count = ordered.len().max(1);
    let wall = ordered.iter().sum::<f64>() / count as f64;

    // The median of an even count is the mean of the two middle readings.
    let median = match ordered.len() {
        0 => 0.0,
        n if n % 2 == 1 => ordered[n / 2],
        n => (ordered[n / 2 - 1] + ordered[n / 2]) / 2.0,
    };

    let worst = ordered.last().copied().unwrap_or(0.0);
    (wall, median, worst)
}

/// Process CPU time: user plus system, in milliseconds. Core-time, all threads summed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CpuTime {
    /// The process's user time, in milliseconds.
    pub user_ms: u64,
    /// The process's system time, in milliseconds.
    pub system_ms: u64,
}

impl CpuTime {
    /// Core-time: all threads summed.
    pub fn total_ms(&self) -> f64 {
        self.user_ms as f64 + self.system_ms as f64
    }
}

/// The platform's CPU-time clock, or `None` when the build has none.
pub type CpuClock<'a> = &'a dyn Fn() -> Option<CpuTime>;

/// The platform's canvas surface: draw one frame of `canvas`, settle, and return.
///
/// The `frame` phase draws through an app's real canvas; this crate has no surface, so the app lends one.
pub type Surface<'a> = &'a mut dyn FnMut(&Canvas) -> Result<(), String>;

/// The wrap width of the platform's cumulative energy counter, in microjoules, when the caller knows it.
///
/// RAPL wraps at `max_energy_range_uj`; a counter that crossed zero reads as a small number after a large
/// one. Without the width the report cannot say by how much it wrapped, so a backwards step is `None`.
pub type EnergyWrap = Option<u64>;

/// The energy over one phase: the counter differenced, or the meter integrated, across the whole phase.
///
/// The window is the **phase**, not one operation: a rung is milliseconds and a meter samples every
/// tenth of a second, so a per-operation meter window would be `meter-window-too-short` every time. The
/// phase's total joules are divided by its count to get `joules_per_op`.
struct Energy {
    /// Total joules over the phase, when the source could cover it.
    joules: Option<f64>,
    /// What the source was: `meter`, `platform` or `none`.
    source: &'static str,
    /// How the joules were arrived at: `meter`, `platform` or `cpu-time`.
    basis: &'static str,
    /// The meter samples inside the window, when a meter was read.
    meter_samples: Option<usize>,
    /// The meter's window span, in milliseconds, when a meter was read.
    meter_span_ms: Option<f64>,
}

impl Energy {
    /// The phase's energy divided by its count, the per-operation figure a row carries.
    fn per_op(&self, count: usize) -> Option<f64> {
        self.joules
            .map(|joules| joules / count.max(1) as f64)
            .filter(|joules| *joules > 0.0)
    }
}

/// A clock a phase's energy is measured against: the seam's cumulative counter, a meter, or nothing.
enum Clock<'a> {
    /// No energy source: `wall` and `cpu` only.
    None,
    /// The seam's cumulative `energy_uj`, differenced across the phase.
    Platform {
        /// The platform's counters, taken from the seam.
        stats: &'a mut dyn SystemStats,
        /// The counter's wrap width, when known.
        wrap: EnergyWrap,
    },
    /// Epoch seconds against the meter's samples.
    Meter(&'a [MeterSample]),
}

impl Clock<'_> {
    /// Opens the phase's window: the counter's reading, or the wall-clock start.
    fn open(&mut self) -> PhaseWindow {
        match self {
            Clock::None => PhaseWindow::Closed,
            Clock::Platform { stats, .. } => PhaseWindow::Platform(stats.energy_uj()),
            Clock::Meter(_) => PhaseWindow::Meter(epoch_seconds()),
        }
    }

    /// Closes the phase's window `opened` after `seconds` of measured work, returning the energy.
    fn close(&mut self, opened: PhaseWindow, seconds: f64, offset: f64) -> Energy {
        match (self, opened) {
            (Clock::Platform { stats, wrap }, PhaseWindow::Platform(Some(before))) => {
                let joules = stats.energy_uj().and_then(|after| {
                    let delta = match after.checked_sub(before) {
                        Some(delta) => delta,
                        // Wrapped: only a known width can say by how much.
                        None => (*wrap)?.saturating_sub(before).saturating_add(after),
                    };
                    (delta > 0).then(|| delta as f64 / 1_000_000.0)
                });
                Energy {
                    joules,
                    source: "platform",
                    basis: if joules.is_some() {
                        "platform"
                    } else {
                        "cpu-time"
                    },
                    meter_samples: None,
                    meter_span_ms: None,
                }
            }
            (Clock::Meter(samples), PhaseWindow::Meter(start)) => {
                let end = start + seconds;
                match integrate(samples, start + offset, end + offset) {
                    Integration::Joules {
                        joules,
                        samples: inside,
                    } => Energy {
                        joules: Some(joules),
                        source: "meter",
                        basis: "meter",
                        meter_samples: Some(inside),
                        meter_span_ms: Some(seconds * 1_000.0),
                    },
                    Integration::Skipped(_) => Energy {
                        joules: None,
                        source: "meter",
                        basis: "cpu-time",
                        meter_samples: None,
                        meter_span_ms: Some(seconds * 1_000.0),
                    },
                }
            }
            _ => Energy {
                joules: None,
                source: "none",
                basis: "cpu-time",
                meter_samples: None,
                meter_span_ms: None,
            },
        }
    }
}

/// Where a phase's window began.
enum PhaseWindow {
    /// No source: nothing to close.
    Closed,
    /// The counter's reading when the phase opened, if it could be read.
    Platform(Option<u64>),
    /// The epoch second the phase opened at.
    Meter(f64),
}

/// The current Unix epoch second, or `0.0` on a clock before the epoch.
fn epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Reads `path` as a meter CSV, or `meter-invalid` when it cannot be opened.
fn read_meter(path: &str) -> MeterState {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_meter(&text),
        Err(_) => MeterState::Invalid("meter-invalid"),
    }
}

/// Skips a phase and lets its clock record nothing, so the summary still says what the source was.
fn empty_energy(clock: &mut Clock<'_>, offset: f64) -> Energy {
    let window = clock.open();
    clock.close(window, 0.0, offset)
}

/// The `rung` phase: alternate two rungs for `repeats` timed rebuilds, after `warmup` discarded ones.
///
/// The page is built before the window opens and dropped after it closes. One `Arc` clone of the
/// finished list is held across every timed rebuild — the way a frame's description holds one — so the
/// rebuild pays the `Arc::make_mut` copy it pays in the app rather than measuring an unshared page as a
/// lower bound.
///
/// `pool` runs the timed rebuilds on a scoped pool when `--threads` was given; the global pool is left
/// exactly as it was. `write` is called with each `PhaseResult` before it is returned, so the caller can
/// fold the energy into the summary without knowing the phase's shape.
fn rung(
    strokes: usize,
    plan: &Plan,
    clock: &mut Clock<'_>,
    cpu: CpuClock<'_>,
    pool: Option<rayon::ThreadPool>,
) -> (Measured, Energy) {
    let mut page = synthetic_page(strokes);
    let (low, high) = RUNGS;

    // Establish the initial rung, then warm up in each direction and discard it.
    page.set_zoom(low);
    for step in 0..(plan.warmup.max(1) * 2) {
        let zoom = if step % 2 == 0 { high } else { low };
        page.set_zoom(zoom);
    }

    // One `Arc` clone, the way a frame's description holds the finished list.
    let shared = page.finished().clone();
    let timed = |page: &mut crate::ink::InkDocument, zoom: f32| {
        let started = Instant::now();
        if let Some(pool) = &pool {
            pool.install(|| std::hint::black_box(page.set_zoom(zoom)));
        } else {
            std::hint::black_box(page.set_zoom(zoom));
        }
        started.elapsed()
    };

    let cpu_before = cpu();
    let window = clock.open();
    let mut samples = Vec::with_capacity(plan.repeats);
    let mut seconds = 0.0;

    for step in 0..plan.repeats {
        let zoom = if step % 2 == 0 { high } else { low };
        let elapsed = timed(&mut page, zoom);
        samples.push(elapsed);
        seconds += elapsed.as_secs_f64();
    }

    let energy = clock.close(window, seconds, plan.meter_offset);
    let cpu_ms = cpu_delta(cpu_before, cpu(), plan.repeats);
    drop(shared);
    drop(page);

    let (wall_ms, median_ms, worst_ms) = summarise(&samples);
    let result = PhaseResult {
        count: plan.repeats,
        wall_ms,
        median_ms,
        worst_ms,
        cpu_ms,
        joules_per_op: energy.per_op(plan.repeats),
        threads: plan.threads,
    };

    (Measured::Done(result), energy)
}

/// The `frame` phase: draw `frames` warm frames of the same page, through the caller's canvas.
///
/// The first `draw` is warmup — it bakes every realization — and is not in the window. `draw` is the
/// caller's because this crate has no surface: the thing that paints the page belongs to an app, and a
/// build with none skips the phase rather than timing the model alone and calling it a frame.
fn frame(
    strokes: usize,
    plan: &Plan,
    clock: &mut Clock<'_>,
    cpu: CpuClock<'_>,
    surface: Surface<'_>,
) -> (Measured, Energy) {
    let mut canvas = canvas_of(&synthetic_page(strokes));

    // Warmup: the first frame bakes every realization, so it is not the steady cost a pan pays.
    if surface(&canvas).is_err() {
        return (
            Measured::Skipped("no-surface"),
            empty_energy(clock, plan.meter_offset),
        );
    }

    let cpu_before = cpu();
    let window = clock.open();
    let mut samples = Vec::with_capacity(plan.frames);
    let mut seconds = 0.0;

    for _ in 0..plan.frames {
        let started = Instant::now();
        if surface(&canvas).is_err() {
            let energy = clock.close(window, seconds, plan.meter_offset);
            return (Measured::Skipped("no-surface"), energy);
        }
        let elapsed = started.elapsed();
        samples.push(elapsed);
        seconds += elapsed.as_secs_f64();

        // A visible pan: the same ink and detail, moved. The description is rebuilt so a frame is a
        // real draw rather than the same buffer handed back.
        canvas.ink.origin.0 += 1.0;
    }

    let energy = clock.close(window, seconds, plan.meter_offset);
    let cpu_ms = cpu_delta(cpu_before, cpu(), plan.frames);

    let (wall_ms, median_ms, worst_ms) = summarise(&samples);
    let result = PhaseResult {
        count: plan.frames,
        wall_ms,
        median_ms,
        worst_ms,
        cpu_ms,
        joules_per_op: energy.per_op(plan.frames),
        threads: None,
    };

    (Measured::Done(result), energy)
}

/// The CPU time spent over `count` operations, per operation, or `None` when either reading is absent.
fn cpu_delta(before: Option<CpuTime>, after: Option<CpuTime>, count: usize) -> Option<f64> {
    let (before, after) = (before?, after?);
    let delta = after.total_ms() - before.total_ms();
    (delta >= 0.0).then(|| delta / count.max(1) as f64)
}

/// The sheet the `frame` phase draws on, in the paper's own units.
const SHEET: (f32, f32) = (720.0, 1_000.0);

/// Where the sheet sits on the desk.
const MARGIN: f32 = 40.0;

/// The page as a canvas the caller's surface can draw: the ink, a sheet, a shadow and forty rules.
///
/// The same description the bench draws (see `apps/macos/src/platform/bench.rs`'s `page`), because the
/// `frame` phase is the bench's `frame` figure — warm draw plus present — given an energy window.
fn canvas_of(page: &crate::ink::InkDocument) -> Canvas {
    use crate::ink_layer::canvas::Fill;
    use gpui_kit::{rgb, rgba, Hsla};
    use note_format::ink::Bounds;

    let ink = Ink {
        origin: (MARGIN, MARGIN),
        zoom: 1.0,
        paper: SHEET,
        visible: Bounds::new((0.0, 0.0), SHEET),
        revision: 1,
        strokes: page.finished().clone(),
        ..Default::default()
    };

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

// ── The command ──────────────────────────────────────────────────────────────────────────────────────

/// What an app lends the report: the process's own clocks, and a canvas for the `frame` phase.
///
/// Every field is optional because a platform may honestly lack it — a build with no canvas skips the
/// `frame` phase, a platform with no energy counter reads no joules — and the report says so rather than
/// printing a zero.
pub struct Harness<'a> {
    /// The process's CPU time, when the platform can read it.
    pub cpu: Option<CpuClock<'a>>,
    /// The `frame` phase's surface, when the app has one.
    pub surface: Option<Surface<'a>>,
    /// The platform's cumulative energy counter, when the seam has one.
    pub stats: Option<&'a mut dyn SystemStats>,
    /// That counter's wrap width, when the platform knows it.
    pub wrap: EnergyWrap,
    /// The platform's name, for the JSON envelope: `macos`, `windows`, `linux`.
    pub platform: &'static str,
    /// Whether this is a release build. A debug build is `skipped`, because a debug number is not a
    /// measurement of the code that ships.
    pub release: bool,
}

/// Runs the plan, writing the report to `out`, and returns the exit code.
///
/// The three codes are the other commands' — `ok`/`0`, `failed`/`1`, `skipped`/`2` — and the run is
/// `skipped` when it could not measure: a debug build, an invalid meter, a `--require-energy` run with no
/// energy, or an energy budget while no energy was read.
pub fn run(plan: &Plan, harness: &mut Harness<'_>, out: &mut impl Write) -> i32 {
    if !harness.release {
        let report = Report::new(Command::PowerReport, harness.platform, tool(plan)).conclude(
            Verdict::Skipped,
            Some(String::from(
                "this is a debug build; a debug number is not a measurement of the code that ships",
            )),
        );
        return write_report(report, plan.format, out);
    }

    let meter = plan.meter.as_deref().map(read_meter);
    // A meter named explicitly, or an `auto` that found one, that will not parse is a skip, never a
    // silent downgrade to `none`.
    if matches!(plan.energy, EnergyChoice::Meter | EnergyChoice::Auto) {
        if let Some(MeterState::Invalid(reason)) = &meter {
            let report = Report::new(Command::PowerReport, harness.platform, tool(plan))
                .columns(&COLUMNS)
                .conclude(Verdict::Skipped, Some((*reason).to_string()));
            return write_report(report, plan.format, out);
        }
    }

    let samples = match &meter {
        Some(MeterState::Ready(samples)) => Some(samples.clone()),
        _ => None,
    };

    let mut clock = match plan.energy {
        EnergyChoice::None => Clock::None,
        EnergyChoice::Platform => match harness.stats.as_deref_mut() {
            Some(stats) => Clock::Platform {
                stats,
                wrap: harness.wrap,
            },
            // An explicit `platform` with no counter is a skip, not a silent `none`.
            None => {
                let report = Report::new(Command::PowerReport, harness.platform, tool(plan))
                    .columns(&COLUMNS)
                    .conclude(Verdict::Skipped, Some(String::from("no-platform-counter")));
                return write_report(report, plan.format, out);
            }
        },
        EnergyChoice::Meter => match &samples {
            Some(samples) => Clock::Meter(samples),
            None => Clock::None,
        },
        EnergyChoice::Auto => {
            if let Some(samples) = &samples {
                Clock::Meter(samples)
            } else {
                let can_meter = harness
                    .stats
                    .as_deref_mut()
                    .is_some_and(|stats| stats.energy_uj().is_some());
                if can_meter {
                    match harness.stats.as_deref_mut() {
                        Some(stats) => Clock::Platform {
                            stats,
                            wrap: harness.wrap,
                        },
                        None => Clock::None,
                    }
                } else {
                    Clock::None
                }
            }
        }
    };

    let cpu = harness.cpu;
    let mut rows: Vec<Row> = Vec::new();
    let mut energies: Vec<Energy> = Vec::new();
    let mut skipped: Option<&'static str> = None;

    for &strokes in &plan.strokes {
        if plan.workload.includes(PhaseKind::Rung) {
            let pool = plan.threads.and_then(build_pool);
            let (measured, energy) = rung(strokes, plan, &mut clock, cpu.unwrap_or(&no_cpu), pool);
            match measured {
                Measured::Done(result) => rows.push(result.row(strokes, PhaseKind::Rung)),
                Measured::Skipped(reason) => skipped = Some(reason),
            }
            energies.push(energy);
        }

        if plan.workload.includes(PhaseKind::Frame) {
            match harness.surface.as_deref_mut() {
                Some(surface) => {
                    let (measured, energy) =
                        frame(strokes, plan, &mut clock, cpu.unwrap_or(&no_cpu), surface);
                    match measured {
                        Measured::Done(result) => rows.push(result.row(strokes, PhaseKind::Frame)),
                        Measured::Skipped(reason) => skipped = Some(reason),
                    }
                    energies.push(energy);
                }
                // A build with no canvas: the frame phase is not measured, and says why.
                None => skipped = Some("no-surface"),
            }
        }
    }

    let report = assemble(plan, harness.platform, rows, &energies, skipped);
    write_report(report, plan.format, out)
}

/// A CPU-time reader that never answers, for a platform with none.
fn no_cpu() -> Option<CpuTime> {
    None
}

/// A scoped pool of `threads`, or `None` when it could not be built (the global pool is then used).
fn build_pool(threads: usize) -> Option<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .ok()
}

/// The free-text `tool` line the envelope carries.
fn tool(plan: &Plan) -> String {
    let strokes: Vec<String> = plan.strokes.iter().map(usize::to_string).collect();
    let threads = match (plan.workload, plan.threads) {
        (Workload::Frame, _) => String::new(),
        (_, Some(threads)) => format!(" · {threads} threads"),
        (_, None) => String::from(" · global pool"),
    };
    format!(
        "note-ui {} rebuild · {} strokes{threads}",
        plan.workload.name(),
        strokes.join(",")
    )
}

/// The summary field that says which pool the rung phase used, and why.
fn threads_source(plan: &Plan) -> &'static str {
    match (plan.workload, plan.threads) {
        (Workload::Frame, _) => "not-parallel",
        (_, Some(_)) => "flag",
        (_, None) => "global",
    }
}

/// Builds the [`Report`] from the rows and the energy each phase read, and decides the verdict.
///
/// The verdict is the first of these that fires: a phase that was skipped; an energy budget or
/// `--require-energy` with no energy; a budget exceeded. `ok` only when none of them did.
fn assemble(
    plan: &Plan,
    platform: &'static str,
    rows: Vec<Row>,
    energies: &[Energy],
    skipped: Option<&'static str>,
) -> Report {
    let mut report = Report::new(Command::PowerReport, platform, tool(plan)).columns(&COLUMNS);
    let measured = !rows.is_empty();
    for row in rows {
        report.push(row);
    }

    let source = energies.first().map_or("none", |energy| energy.source);
    let basis = energies.first().map_or("cpu-time", |energy| energy.basis);
    let any_energy = energies.iter().any(|energy| energy.joules.is_some());
    let total: f64 = energies.iter().filter_map(|energy| energy.joules).sum();

    report.summarise(
        "threads",
        plan.threads.map_or(Value::Absent("global"), |threads| {
            Value::Count(threads as u64)
        }),
    );
    report.summarise("threads_source", Value::Text(threads_source(plan)));
    report.summarise("repeats", Value::Count(plan.repeats as u64));
    report.summarise("warmup", Value::Count(plan.warmup as u64));
    report.summarise("frames", Value::Count(plan.frames as u64));
    report.summarise("build", Value::Text("release"));
    report.summarise("energy_source", Value::Text(source));
    report.summarise(
        "energy_scope",
        Value::Text(match source {
            "platform" => "package",
            "meter" => "system",
            _ => "unknown",
        }),
    );
    report.summarise("basis", Value::Text(basis));

    if let Some(energy) = energies
        .iter()
        .find(|energy| energy.meter_samples.is_some())
    {
        if let Some(samples) = energy.meter_samples {
            report.summarise("meter_samples", Value::Count(samples as u64));
        }
        if let Some(span) = energy.meter_span_ms {
            report.summarise("meter_span_ms", Value::Millis(span));
        }
    }

    if any_energy && total > 0.0 {
        report.summarise("energy_total_j", Value::Joules(total));
    }

    let energy_budget =
        plan.budgets.max_joules_per_op.is_some() || plan.budgets.min_ops_per_joule.is_some();

    // The verdict, in the order the contract names it.
    let mut report = if let Some(reason) = skipped {
        report.conclude(Verdict::Skipped, Some(String::from(reason)))
    } else if !measured {
        report.conclude(Verdict::Skipped, Some(String::from("nothing measured")))
    } else if (energy_budget || plan.require_energy) && !any_energy {
        // An energy budget or --require-energy with no energy is a skip, not a pass: an unmeasured
        // quantity cannot meet a budget.
        report.conclude(
            Verdict::Skipped,
            Some(String::from(
                "no energy was measured; a budget on it cannot be met",
            )),
        )
    } else {
        let (verdict, note) = judge(&report.columns, &report.rows, &plan.budgets);
        let note = note.or_else(|| {
            Some(match basis {
                "cpu-time" => String::from(
                    "energy not measured: cpu is core-time, a proxy for work, not power",
                ),
                _ => format!("energy read from the {source} source"),
            })
        });
        report.conclude(verdict, note)
    };

    // A column no row could fill is named once, after the rows are all in, and every row's own cell for it
    // is `null` — the measured cells stay what they were measured.
    if !any_energy {
        report = report
            .unmeasured("joules_per_op", "no-energy-source")
            .unmeasured("ops_per_joule", "no-energy-source");

        let energy_columns: Vec<usize> = COLUMNS
            .iter()
            .enumerate()
            .filter(|(_, name)| ENERGY_COLUMNS.contains(name))
            .map(|(at, _)| at)
            .collect();

        for row in &mut report.rows {
            for &at in &energy_columns {
                if let Some(cell) = row.values.get_mut(at) {
                    *cell = Value::Absent("no-energy-source");
                }
            }
        }
    }

    report
}

/// Judges every row against the budgets, returning the verdict and a sentence.
///
/// A row over any budget fails the run and the sentence names it. A budget whose column a row could not
/// fill is a skip — the same rule the bench's `--max-ms` follows.
fn judge(columns: &[&'static str], rows: &[Row], budgets: &Budgets) -> (Verdict, Option<String>) {
    let mut over: Vec<String> = Vec::new();
    let mut missing = false;

    for row in rows {
        if let Some(max_ms) = budgets.max_ms {
            match row.get(columns, "wall") {
                Some(Value::Millis(spent)) if *spent > max_ms => {
                    over.push(format!("{} wall {spent:.2} > {max_ms:.2} ms", row.label));
                }
                Some(Value::Millis(_)) => {}
                _ => missing = true,
            }
        }
        if let Some(max_joules) = budgets.max_joules_per_op {
            match row.get(columns, "joules_per_op") {
                Some(Value::Joules(joules)) if *joules > max_joules => {
                    over.push(format!(
                        "{} joules_per_op {joules:.3} > {max_joules:.3} J",
                        row.label
                    ));
                }
                Some(Value::Joules(_)) => {}
                _ => missing = true,
            }
        }
        if let Some(min_ops) = budgets.min_ops_per_joule {
            match row.get(columns, "ops_per_joule") {
                Some(Value::Joules(ops)) if *ops < min_ops => {
                    over.push(format!(
                        "{} ops_per_joule {ops:.3} < {min_ops:.3}",
                        row.label
                    ));
                }
                Some(Value::Joules(_)) => {}
                _ => missing = true,
            }
        }
    }

    if !over.is_empty() {
        return (
            Verdict::Failed,
            Some(format!("over budget: {}", over.join(", "))),
        );
    }
    if missing {
        return (
            Verdict::Skipped,
            Some(String::from(
                "a budget was given but its column was not measured",
            )),
        );
    }
    (Verdict::Ok, None)
}

/// Writes the report in `format`, returning the exit code from its verdict.
fn write_report(report: Report, format: Format, out: &mut impl Write) -> i32 {
    crate::report::emit(&report, format, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn plans(list: &[&str]) -> Plan {
        plan(&args(list), Format::Table).expect("parses")
    }

    /// The same flags, but asking for the machine-readable report a test can parse.
    fn json_plans(list: &[&str]) -> Plan {
        plan(&args(list), Format::Json).expect("parses")
    }

    /// The defaults are the two stroke counts the contract names, twenty repeats and two warmups.
    #[test]
    fn the_defaults_are_the_contracts() {
        let plan = plans(&[]);
        assert_eq!(plan.workload, Workload::Rung);
        assert_eq!(plan.strokes, vec![1_000, 8_182]);
        assert_eq!(plan.repeats, DEFAULT_REPEATS);
        assert_eq!(plan.warmup, DEFAULT_WARMUP);
        assert_eq!(plan.frames, DEFAULT_FRAMES);
        assert_eq!(plan.energy, EnergyChoice::Auto);
        assert!(!plan.require_energy);
    }

    /// Every flag is read, in both the `--flag value` and `--flag=value` forms.
    #[test]
    fn every_flag_is_read() {
        let plan = plans(&[
            "--workload",
            "both",
            "--strokes=316,1000",
            "--repeats",
            "5",
            "--warmup=1",
            "--threads",
            "4",
            "--frames=10",
            "--energy-source",
            "none",
            "--max-ms",
            "8.33",
            "--max-joules-per-op=0.5",
            "--min-ops-per-joule",
            "2",
        ]);

        assert_eq!(plan.workload, Workload::Both);
        assert_eq!(plan.strokes, vec![316, 1_000]);
        assert_eq!(plan.repeats, 5);
        assert_eq!(plan.warmup, 1);
        assert_eq!(plan.threads, Some(4));
        assert_eq!(plan.frames, 10);
        assert_eq!(plan.energy, EnergyChoice::None);
        assert_eq!(plan.budgets.max_ms, Some(8.33));
        assert_eq!(plan.budgets.max_joules_per_op, Some(0.5));
        assert_eq!(plan.budgets.min_ops_per_joule, Some(2.0));
    }

    /// `--repeats` below three is a mistake: a median of one is a guess.
    #[test]
    fn a_median_of_one_is_refused() {
        let error = plan(&args(&["--repeats", "2"]), Format::Table).unwrap_err();
        assert!(error.contains("at least 3"), "{error}");
    }

    /// Every usage mistake is a sentence naming the flag that wanted the value.
    #[test]
    fn usage_mistakes_name_their_flag() {
        for bad in [
            vec!["--strokes", "abc"],
            vec!["--max-ms", "0"],
            vec!["--max-joules-per-op", "-1"],
            vec!["--frames", "0"],
            vec!["--threads", "0"],
            vec!["--workload", "panic"],
        ] {
            let error = plan(&args(&bad), Format::Table).expect_err("a mistake");
            assert!(!error.is_empty(), "{bad:?} produced no sentence");
        }
    }

    /// The two flags that contradict, and the one that needs a dependency, are refused.
    #[test]
    fn contradictions_are_refused() {
        let error = plan(
            &args(&["--require-energy", "--energy-source", "none"]),
            Format::Table,
        )
        .unwrap_err();
        assert!(error.contains("opposite"), "{error}");

        let error = plan(&args(&["--energy-source", "meter"]), Format::Table).unwrap_err();
        assert!(error.contains("--meter"), "{error}");
    }

    /// `--help` is told apart from a mistake so the caller can print usage and exit `0`.
    #[test]
    fn help_is_not_a_mistake() {
        assert_eq!(plan(&args(&["--help"]), Format::Table).unwrap_err(), "help");
    }

    /// A meter CSV is read, and a bad row is refused rather than read as a zero.
    #[test]
    fn a_meter_is_read_or_refused() {
        let good = parse_meter("epoch_s,watts\n1.0,10\n1.1,12\n1.2,11\n");
        assert!(matches!(good, MeterState::Ready(ref samples) if samples.len() == 3));

        for bad in [
            "epoch_s,watts\n1.0,10\n",          // one row: a moment, not an integral
            "epoch_s,watts\n2.0,10\n1.0,12\n",  // out of order
            "epoch_s,watts\n1.0,-5\n1.1,12\n",  // negative
            "epoch_s,watts\n1.0,abc\n1.1,12\n", // not a number
        ] {
            assert!(
                matches!(parse_meter(bad), MeterState::Invalid(_)),
                "{bad:?} was accepted"
            );
        }
    }

    /// The integration is the trapezoid rule, and it needs two samples inside the window.
    #[test]
    fn energy_is_the_area_under_the_meter() {
        // 10 W held across one second is 10 J.
        let samples = [
            MeterSample {
                epoch_s: 0.0,
                watts: 10.0,
            },
            MeterSample {
                epoch_s: 1.0,
                watts: 10.0,
            },
        ];
        match integrate(&samples, 0.0, 1.0) {
            Integration::Joules { joules, samples } => {
                assert!((joules - 10.0).abs() < 1e-9, "{joules}");
                assert_eq!(samples, 2);
            }
            other => panic!("{other:?}"),
        }

        // A window holding one sample cannot be integrated.
        assert_eq!(
            integrate(&samples, 0.0, 0.5),
            Integration::Skipped("meter-window-too-short")
        );
    }

    /// A page is built deterministically, and holds the strokes it was asked for.
    #[test]
    fn the_synthetic_page_is_deterministic() {
        let first = synthetic_page(50);
        let second = synthetic_page(50);
        assert_eq!(first.finished().len(), 50);
        assert_eq!(
            first.finished()[7].points.len(),
            second.finished()[7].points.len()
        );
        assert_eq!(
            first.finished()[7].points[3].x,
            second.finished()[7].points[3].x
        );
    }

    /// Operations per joule is one over the energy, and absent with it.
    #[test]
    fn ops_per_joule_is_one_over_the_energy() {
        let with = PhaseResult {
            count: 10,
            wall_ms: 1.0,
            median_ms: 1.0,
            worst_ms: 1.0,
            cpu_ms: None,
            joules_per_op: Some(0.08),
            threads: None,
        };
        assert!((with.ops_per_joule().unwrap() - 12.5).abs() < 1e-9);

        let without = PhaseResult {
            joules_per_op: None,
            ..with
        };
        assert_eq!(without.ops_per_joule(), None);
    }

    /// A test seam: a stats whose energy is a fixed sequence, so a counter that advances can be simulated.
    struct MutableCounter {
        readings: Vec<u64>,
        at: usize,
    }

    impl SystemStats for MutableCounter {
        fn vitals(&mut self) -> Option<crate::platform::Vitals> {
            None
        }

        fn energy_uj(&mut self) -> Option<u64> {
            let value = self.readings.get(self.at).copied();
            self.at += 1;
            value
        }
    }

    /// A harness a test can run: no CPU clock, no energy, no canvas.
    fn harness<'a>() -> Harness<'a> {
        Harness {
            cpu: None,
            surface: None,
            stats: None,
            wrap: None,
            platform: "macos",
            release: true,
        }
    }

    /// A run with no energy source still lists its phases, with `null` energy and a named reason.
    ///
    /// This is what a Windows build or a macOS run without a meter produces: `wall` and `cpu` measured,
    /// `joules_per_op` and `ops_per_joule` absent, and the column named in `unmeasured` — not a table
    /// with three cells and two blanks.
    #[test]
    fn no_energy_still_names_every_column() {
        let plan = json_plans(&[
            "--strokes",
            "100",
            "--repeats",
            "3",
            "--energy-source",
            "none",
        ]);
        let mut harness = harness();
        let mut out = Vec::new();
        assert_eq!(run(&plan, &mut harness, &mut out), 0);

        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).expect("valid JSON");
        assert_eq!(parsed["verdict"], "ok");
        // The row has every column the header names; the energy cells are null.
        let row = &parsed["rows"][0];
        for column in COLUMNS {
            assert!(row.get(column).is_some(), "missing column: {column}");
        }
        assert_eq!(row["joules_per_op"], serde_json::Value::Null);
        assert_eq!(
            parsed["unmeasured"][0]["column"], "joules_per_op",
            "the absent column is named"
        );
    }

    /// `--require-energy` with no energy is a skip, never a pass.
    #[test]
    fn require_energy_without_energy_skips() {
        // `auto` with no meter and no counter resolves to `none`, which `--require-energy` refuses.
        let plan = json_plans(&["--strokes", "100", "--repeats", "3", "--require-energy"]);
        let mut harness = harness();
        let mut out = Vec::new();
        assert_eq!(run(&plan, &mut harness, &mut out), 2);
    }

    /// A debug build measures nothing: a debug number is not a measurement of the code that ships.
    #[test]
    fn a_debug_build_skips() {
        let plan = json_plans(&["--energy-source", "none"]);
        let mut harness = harness();
        harness.release = false;
        let mut out = Vec::new();
        assert_eq!(run(&plan, &mut harness, &mut out), 2);
    }

    /// The platform counter is differenced across the phase, so a closed seam gives `joules_per_op`.
    #[test]
    fn a_platform_counter_gives_joules() {
        let plan = json_plans(&[
            "--strokes",
            "100",
            "--repeats",
            "3",
            "--energy-source",
            "platform",
        ]);

        // A counter that advances by 3 000 000 µJ (3 J) across the phase: read when the phase opens, and
        // again when it closes.
        let mut counter = MutableCounter {
            readings: vec![1_000_000, 4_000_000],
            at: 0,
        };

        let mut harness = Harness {
            cpu: None,
            surface: None,
            stats: Some(&mut counter),
            wrap: None,
            platform: "macos",
            release: true,
        };
        let mut out = Vec::new();
        assert_eq!(run(&plan, &mut harness, &mut out), 0);

        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).expect("valid JSON");
        let joules = parsed["rows"][0]["joules_per_op"].as_f64().expect("joules");
        // 3 J over three operations is 1 J per operation, and 1 op per joule its reciprocal.
        assert!((joules - 1.0).abs() < 1e-6, "{joules}");
        let ops = parsed["rows"][0]["ops_per_joule"].as_f64().expect("ops");
        assert!((ops - 1.0).abs() < 1e-6, "{ops}");
        assert_eq!(parsed["summary"]["basis"], "platform");
    }
}
