//! What opening and closing documents costs, measured rather than guessed.
//!
//! ## Why this is in the app and not in a script around it
//!
//! Because a shell can only see a resident figure, and a resident figure cannot say *which* of the
//! things this app does to memory is growing. Reading a note's document, rasterising a page, handing
//! it to the compositor, caching it and dropping it are five different allocations in three different
//! places, and `ps` reports one number for all of them. A reader who says "it climbs when I open and
//! close PDFs" is reporting something real; the job here is to say **where**.
//!
//! So this runs inside the process, between the operations, and asks the same
//! [`SystemStats`](crate::platform::SystemStats) the status line asks. What it adds over the status
//! line is *sequence*: a row per operation, a delta between them, and a comparison of the end against
//! the start — which is the one figure that answers "did closing it give the memory back".
//!
//! ## The command
//!
//! ```text
//! cheap-note --memory-report <note> [<note> ...]
//! ```
//!
//! No window is opened. Each argument is a note folder, opened, measured and closed — in the same
//! process, which is what makes the deltas mean anything. A launch that opened and closed in one
//! process each would measure the operating system, not the app.
//!
//! ## What it cannot see
//!
//! The GPU column is `None` on a platform that cannot measure it, and on macOS it is `IOSurface` — the
//! textures the compositor holds. A headless run still allocates bitmaps, because a page is rendered
//! into one before it is ever a texture, but the compositor is not presenting and may pool them
//! differently. **The resident figures are the ones to trust here**, and the GPU figure is worth
//! reading for its direction rather than its exact size.
//!
//! ## The failure rule
//!
//! A close that does not give the memory back is the bug this exists to catch, so the report ends with
//! a verdict and an exit code: `0` when the process came back to within [`LEAK_TOLERANCE_BYTES`] of
//! where it started, `1` when it did not. That is what makes it usable in CI — a leak becomes a failing
//! test rather than a number someone has to notice.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ink::{InkPoint, Stroke};
use crate::note::Note;
use crate::pdf::{PageRequest, PdfDocumentView};

/// How much growth after a close is treated as ordinary rather than as a leak.
///
/// Not zero, because a process that has opened a document and dropped it is never byte-for-byte the
/// process it was: the first PDF pays for Pdfium itself (its library, its font cache, its per-process
/// state — measured at 30 to 50 MiB whatever the document's size), a note's SQLite connection grows its
/// own page cache, and the allocator keeps the small blocks it freed for reuse.
///
/// The figure this is *meant* to catch is a document's bytes being held: measured before the mapping
/// change, a 61 MB textbook left 130 MiB behind, and a 14 MB one left 63 — both far above this. After
/// it, the same notes leave 70 and 35, which is Pdfium's own cost rather than the document's, and that
/// is what the allowance is sized for.
///
/// Set from measurement rather than guessed, and worth revisiting if Pdfium's startup cost moves: a
/// value too high hides a leak of one bitmap, a value too low fails every run.
pub const LEAK_TOLERANCE_BYTES: u64 = 64 * 1024 * 1024;

/// How much the **ink** scenarios may leave behind, measured on their own.
///
/// Smaller than [`LEAK_TOLERANCE_BYTES`] because this half pays nothing like Pdfium's cost: the scenarios
/// write 16 pages, draw 400 strokes, walk six zoom rungs and write a note, and a run that gives all of it
/// back finishes about 14 MiB above where the scenarios started. The figure is set from that measurement with
/// room for allocator retention — the free pages a process keeps rather than returns — and is tight enough
/// that a page cache that stopped evicting, or a history that stopped trimming, would cross it: the
/// 400-stroke page alone is 11 MiB, and a note holding every page ever written was 52 MiB at 10 pages.
pub const INK_TOLERANCE_BYTES: u64 = 32 * 1024 * 1024;

/// The flag that asks for this report rather than a window.
pub const FLAG: &str = "--memory-report";

/// The command line this report accepts, for a mistaken invocation.
///
/// One sentence rather than a line per platform: both mains dispatch on the same flag and both hand
/// the rest to the same `report::Options` parser, so the usage cannot drift between them.
pub fn usage() -> String {
    format!(
        "usage: cheap-note {FLAG} [--format table|json] [<note> ...]\n\
         \n\
         The ink and note scenarios run whether or not a note is given; each <note> adds a row for\n\
         opening and closing its document.\n"
    )
}

/// Which half of the app a row measures.
///
/// The two halves are judged separately because they have different costs that are both legitimate: opening a
/// PDF pays Pdfium's fixed per-process cost (30–50 MiB, whatever the document's size), and the ink scenarios
/// pay for a page of derived geometry. Measured against one allowance they add up to 56–67 MiB, straddling a
/// 64 MiB line, so a run would pass or fail on which allocator block happened to survive. Split, each half is
/// compared with the cost it actually has, and neither can hide behind the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// The process before anything was measured.
    Baseline,
    /// A document — `open` and `close` per note. Judged across the whole run.
    Document,
    /// The ink and note scenarios, judged from where the scenarios started to the `end` row.
    Ink,
}

/// One row of the report: what the process held after one operation.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// What was done — `baseline`, `open 화학.pdf`, `close 화학.pdf`.
    pub label: String,
    /// Which half of the app this row judges, for the verdict.
    pub kind: Kind,
    /// Resident memory, from the platform.
    pub rss_bytes: u64,
    /// The GPU's share, when the platform can say.
    pub surface_bytes: Option<u64>,
    /// What the page cache is holding, in bytes.
    pub cache_bytes: u64,
    /// How many bitmaps are in it.
    pub pages: usize,
}

impl Step {
    /// The row as one line of the table.
    ///
    /// Written to fixed widths so that a series of runs can be compared by eye as much as by number,
    /// and so that a delta's sign has a column of its own — `+181` and `-117` side by side are the
    /// whole story of whether a bitmap came back.
    pub fn line(&self, previous: Option<&Step>) -> String {
        let delta = previous.map(|before| self.rss_bytes as i64 - before.rss_bytes as i64);

        format!(
            "{:<24} {:>9} {:>9} {:>9} {:>9} {:>6}",
            truncate(&self.label, 24),
            mib(self.rss_bytes),
            match delta {
                Some(bytes) => format!("{:+} MiB", bytes / (1024 * 1024)),
                None => String::from("—"),
            },
            format!("{} MB", self.cache_bytes / (1024 * 1024)),
            match self.surface_bytes {
                Some(bytes) => format!("{} MB", bytes / (1024 * 1024)),
                None => String::from("—"),
            },
            self.pages,
        )
    }
}

/// A whole run: the rows, and the two figures the verdict needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Every operation, in the order it happened, starting with `baseline`.
    pub steps: Vec<Step>,
}

impl Report {
    /// The process before any note was opened.
    pub fn baseline(&self) -> Option<&Step> {
        self.steps.first()
    }

    /// The process after the last close.
    pub fn final_step(&self) -> Option<&Step> {
        self.steps.last()
    }

    /// How much the resident figure grew across the whole run, from `baseline` to the last row.
    ///
    /// This is the figure the table prints, and it is not the figure the verdict reads: the two halves are
    /// judged separately (see [`Self::verdict`]), because their legitimate costs do not fit one allowance.
    ///
    /// Negative when the process ended smaller than it started, which is ordinary: a first note's
    /// close can release state the baseline was still holding.
    pub fn growth_bytes(&self) -> Option<i64> {
        let baseline = self.baseline()?.rss_bytes;
        let last = self.final_step()?.rss_bytes;
        Some(last as i64 - baseline as i64)
    }

    /// What the document half grew by: `baseline` to the last row that measured a document.
    ///
    /// `None` when no note was given, which is not a pass — there was nothing to measure.
    pub fn document_growth(&self) -> Option<i64> {
        let baseline = self.baseline()?.rss_bytes;
        let last = self
            .steps
            .iter()
            .rev()
            .find(|step| step.kind == Kind::Document)?;
        Some(last.rss_bytes as i64 - baseline as i64)
    }

    /// What the ink half grew by: from the row the scenarios started at to the `end` row.
    ///
    /// The start is the last row before the first ink row — `baseline` when no note was opened, the last
    /// `close` when one was — so the scenarios are judged against the process they actually ran in, not
    /// against a baseline that a document had already moved.
    pub fn ink_growth(&self) -> Option<i64> {
        let first_ink = self.steps.iter().position(|step| step.kind == Kind::Ink)?;
        let start = self.steps.get(first_ink.checked_sub(1)?)?;
        let end = self
            .steps
            .iter()
            .rev()
            .find(|step| step.kind == Kind::Ink)?;
        Some(end.rss_bytes as i64 - start.rss_bytes as i64)
    }

    /// Whether the run came back to where it started, on the half that was measured.
    ///
    /// The run passes when **every half it measured** passed: a document that gave its memory back and ink
    /// that did not is not a pass. A half that was not measured is not counted either way, so an ink-only run
    /// is judged on the ink and a report of notes is judged on both.
    pub fn is_clean(&self) -> bool {
        let document = self
            .document_growth()
            .is_none_or(|growth| growth <= LEAK_TOLERANCE_BYTES as i64);
        let ink = self
            .ink_growth()
            .is_none_or(|growth| growth <= INK_TOLERANCE_BYTES as i64);

        document && ink && (self.document_growth().is_some() || self.ink_growth().is_some())
    }

    /// The whole report as text: the table, then the verdict.
    pub fn render(&self) -> String {
        let mut out = String::new();

        out.push_str(&format!(
            "{:<24} {:>9} {:>9} {:>9} {:>9} {:>6}\n",
            "", "RSS", "delta", "cache", "gpu", "pages"
        ));

        let mut previous: Option<&Step> = None;
        for step in &self.steps {
            out.push_str(&step.line(previous));
            out.push('\n');
            previous = Some(step);
        }

        out.push_str(&"─".repeat(70));
        out.push('\n');

        let peak = self.steps.iter().map(|step| step.rss_bytes).max();

        match (self.growth_bytes(), peak) {
            (Some(growth), Some(peak)) => {
                let final_rss = self.final_step().map_or(0, |step| step.rss_bytes);

                out.push_str(&format!(
                    "peak {}   final {}   growth {:+} MiB   {}\n",
                    mib(peak),
                    mib(final_rss),
                    growth / (1024 * 1024),
                    if self.is_clean() { "ok" } else { "LEAK" },
                ));

                // A failing half is named with its own figure and its own allowance, because the two are
                // judged against different ones: a reader who sees one `growth` figure and one `LEAK` would
                // otherwise compare the wrong pair and conclude the allowance was wrong.
                if let Some(document) = self.document_growth() {
                    if document > LEAK_TOLERANCE_BYTES as i64 {
                        out.push_str(&format!(
                            "leak: documents left {} MiB behind (the allowance is {})\n",
                            document / (1024 * 1024),
                            mib(LEAK_TOLERANCE_BYTES),
                        ));
                    }
                }

                if let Some(ink) = self.ink_growth() {
                    if ink > INK_TOLERANCE_BYTES as i64 {
                        out.push_str(&format!(
                            "leak: the ink and note scenarios left {} MiB behind (the allowance is {})\n",
                            ink / (1024 * 1024),
                            mib(INK_TOLERANCE_BYTES),
                        ));
                    }
                }
            }
            // No platform to ask, which is a build rather than a bug: say so, and let the caller decide
            // what an unmeasurable run means. It is not a pass, and it is not a failure of the app.
            _ => out.push_str("this build cannot measure memory: no system counters\n"),
        }

        out
    }
}

/// Bytes as whole mebibytes, as `12 MiB`.
///
/// Mebibytes for the same reason the status line uses them: every tool a person would compare this
/// against — Activity Monitor, Task Manager, `top` — uses the binary unit while calling it "MB", and a
/// figure that disagreed with all of them by five percent would look wrong.
fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes / (1024 * 1024))
}

/// A label cut to `width` characters, with the cut shown as an ellipsis.
///
/// A textbook's note folder is 70 characters long — the title, the part, and a hash — and printing it
/// whole would push the figures it is a label for off the right of the table. The end of the name is
/// what matters here, because that is where the note is told from its siblings, so the cut is made in
/// the middle: `Harry_Kim…Part-1_Mechanics` still says which part this is.
fn truncate(label: &str, width: usize) -> String {
    let characters: Vec<char> = label.chars().collect();
    if characters.len() <= width {
        return label.to_string();
    }

    // One for the ellipsis itself.
    let keep = width.saturating_sub(1);
    let head = keep.div_ceil(2);
    let tail = keep - head;

    let mut out: String = characters[..head].iter().collect();
    out.push('…');
    out.extend(&characters[characters.len() - tail..]);
    out
}

/// Runs the report in `format`, writing it to `out`.
///
/// Returns the exit code the process should end with: `0` for a run that gave its memory back, `1` for
/// one that did not, and `2` for a report that could not be run at all — no paths, or a note that would
/// not open. Three codes rather than two because "the app leaked" and "the command was wrong" are
/// different problems, and a CI job wants to tell them apart.
///
/// `--format table` keeps this command's own table, which reads better for one note's sequence;
/// `--format json` writes the shared contract, which is what a script reads. Both end at the same
/// verdict, so the two shapes cannot disagree about whether the run passed.
///
/// `stats` is taken rather than built so that the caller decides what a headless run measures itself
/// with — the platform's own counters, which is the same source the status line reads. `platform` is the
/// caller's to name for the same reason: the view is built for every platform and cannot know which one
/// it is running on. Both mains call this function, so neither can grow a table the other does not have.
pub fn run_in(
    paths: &[PathBuf],
    stats: &mut dyn crate::platform::SystemStats,
    out: &mut impl Write,
    format: crate::report::Format,
    platform: &'static str,
) -> i32 {
    match collect(paths, stats) {
        Ok(report) => match format {
            crate::report::Format::Table => {
                let _ = write!(out, "{}", report.render());
                let _ = out.flush();

                if report.is_clean() {
                    0
                } else {
                    1
                }
            }
            crate::report::Format::Json => {
                let contract = to_contract(&report, platform);
                crate::report::emit(&contract, format, out)
            }
        },
        Err(error) => {
            let _ = writeln!(out, "{error}");
            2
        }
    }
}

/// The report in the shape both measuring commands share.
///
/// The table above is this command's own, and stays: it is easier to read for the sequence of a single
/// note. This is the *contract* form — the same fields, under the same names, that `cheap-note-bench`
/// produces — and it is what `--format json` writes.
///
/// `platform` is passed rather than guessed: the view cannot know which platform it was built for, and a
/// wrong name in a JSON field is worse than none.
pub fn to_contract(report: &Report, platform: &'static str) -> crate::report::Report {
    use crate::report::{Command, Report as Contract, Row, Value, Verdict};

    let mut contract = Contract::new(Command::MemoryReport, platform, "process resident memory")
        .columns(&["rss", "delta", "cache", "gpu", "pages"]);

    let mut previous: Option<u64> = None;

    for step in &report.steps {
        let delta = previous.map(|before| step.rss_bytes as i64 - before as i64);
        previous = Some(step.rss_bytes);

        // `delta` is absent on the first row rather than zero: there is nothing above it to be measured
        // against, and a zero would read as "did not change".
        let delta = match delta {
            Some(bytes) => Value::Signed(bytes),
            None => Value::Absent("the first row has nothing above it"),
        };

        let gpu = match step.surface_bytes {
            Some(bytes) => Value::Count(bytes),
            None => Value::Absent("no-gpu-ledger"),
        };

        contract.push(Row::new(
            step.label.clone(),
            vec![
                Value::Count(step.rss_bytes),
                delta,
                Value::Count(step.cache_bytes),
                gpu,
                Value::Count(step.pages as u64),
            ],
        ));
    }

    let peak = report.steps.iter().map(|step| step.rss_bytes).max();
    if let Some(peak) = peak {
        contract.summarise("peak", Value::Count(peak));
    }
    if let Some(final_rss) = report.final_step().map(|step| step.rss_bytes) {
        contract.summarise("final", Value::Count(final_rss));
    }
    if let Some(growth) = report.growth_bytes() {
        contract.summarise("growth", Value::Signed(growth));
    }
    // The two figures the verdict is actually made from, beside the whole-run one, so that a script can see
    // which half failed without reading the note.
    if let Some(document) = report.document_growth() {
        contract.summarise("document_growth", Value::Signed(document));
        contract.summarise("document_tolerance", Value::Count(LEAK_TOLERANCE_BYTES));
    }
    if let Some(ink) = report.ink_growth() {
        contract.summarise("ink_growth", Value::Signed(ink));
        contract.summarise("ink_tolerance", Value::Count(INK_TOLERANCE_BYTES));
    }

    // Whether the GPU column was measured is a fact about the *rows*, not about the platform: macOS
    // reports the ledger and Windows does not, but a headless run may still read zero here. Declaring a
    // column unmeasured while its cells hold numbers would be a claim the table contradicts.
    if report.steps.iter().all(|step| step.surface_bytes.is_none()) {
        contract = contract.unmeasured("gpu", "no graphics ledger was reported");
    }

    let verdict = match report.growth_bytes() {
        Some(_) if report.is_clean() => Verdict::Ok,
        Some(_) => Verdict::Failed,
        // An empty report measured nothing, and saying so is not the same as passing.
        None => Verdict::Skipped,
    };

    let note = match verdict {
        Verdict::Ok => Some(format!(
            "documents within {} MiB, ink within {} MiB",
            LEAK_TOLERANCE_BYTES / (1024 * 1024),
            INK_TOLERANCE_BYTES / (1024 * 1024)
        )),
        Verdict::Failed => Some(match (report.document_growth(), report.ink_growth()) {
            (Some(document), _) if document > LEAK_TOLERANCE_BYTES as i64 => format!(
                "documents left {} MiB behind (the allowance is {} MiB)",
                document / (1024 * 1024),
                LEAK_TOLERANCE_BYTES / (1024 * 1024)
            ),
            (_, Some(ink)) if ink > INK_TOLERANCE_BYTES as i64 => format!(
                "the ink and note scenarios left {} MiB behind (the allowance is {} MiB)",
                ink / (1024 * 1024),
                INK_TOLERANCE_BYTES / (1024 * 1024)
            ),
            _ => String::from("grew more than its allowance"),
        }),
        Verdict::Skipped => Some(String::from("no rows were measured")),
    };

    contract.conclude(verdict, note)
}

/// Opens and closes every note in turn, measuring between the operations.
///
/// One process for the whole run, which is the point: the question is what the app does between two
/// states of itself, and two launches would answer a question about the operating system instead.
///
/// A note is closed by dropping it, which is what releases the document's bytes, its bitmaps and its
/// Pdfium handle — the same drop the app performs when a reader leaves a note. Nothing is written, so a
/// report does not disturb what it measures.
///
/// ## The scenarios, and why each is here
///
/// The document half is only half the app's memory. The ink half is the larger one — a note's page holds
/// 71x its readings in derived geometry (`doc/VIEW.md`, "What the note holds in memory") — and it has
/// bounds that a report about opening PDFs can never see go wrong. So the run measures both, in this
/// order:
///
/// | Rows | What it would catch |
/// |---|---|
/// | `open` / `close` per path | a document's bytes, bitmaps or Pdfium handle outliving its note |
/// | `ink: writing N pages` | the page cache growing without bound (`ink::PAGES_KEPT`) |
/// | `ink: turning back` | a page given up and never given back, or the reverse |
/// | `ink: undo all` | the history outliving its bound (`HISTORY_EDITS`) |
/// | `ink: zoom rungs` | a rebuild leaking the geometry it replaced |
/// | `note: write N pages` | the writer thread and its connection outliving the note |
/// | `note: pages added and deleted` | the page map not renumbering, which would strand ink |
///
/// Every ink scenario ends by dropping what it built, so the run's final figure is comparable to its
/// baseline and the verdict means the same thing it always did: *did the process come back*.
///
/// **Why a `failed` here can be the fixed costs and not the document.** Pdfium does not return its
/// per-document cost to the system after a close (30–50 MiB, whatever the document's size), and the ink
/// scenarios' own ~14 MiB is allocated on top of it. Each fits inside the 64 MiB allowance; together they
/// measure 56–67 MiB, so the verdict flips between `ok` and `failed` with the document — and not with its
/// size, which was checked (a 175 MB PDF reads *lighter* than a 32 MB one). Running the ink scenarios before
/// the notes does not help, and was measured making it worse (+50 → +65 MiB after a close): the allocator's
/// state before the first document is part of what Pdfium's figure depends on. The rows remain the
/// instrument for a real leak, which is the thing that *climbs* — the same note opened three times costs
/// `+69 → +17 → +17 MiB` and settles, and `leaks --atExit` finds 0 bytes on an ink-only run. See
/// `doc/CLI.md`, "The two halves, and why each has its own allowance".
pub fn collect(
    paths: &[PathBuf],
    stats: &mut dyn crate::platform::SystemStats,
) -> Result<Report, String> {
    let mut steps = Vec::with_capacity(paths.len() * 2 + 16);

    steps.push(measure(
        String::from("baseline"),
        Kind::Baseline,
        None,
        stats,
    ));

    for path in paths {
        let name = note_name(path);

        let opened = open(path).map_err(|error| format!("{name}: {error}"))?;

        steps.push(measure(
            format!("open {name}"),
            Kind::Document,
            Some(&opened),
            stats,
        ));

        // Closed by dropping, and measured *after* the drop: a figure taken while the document is still
        // alive is the one that says nothing about whether closing gave the memory back.
        drop(opened);

        steps.push(measure(
            format!("close {name}"),
            Kind::Document,
            None,
            stats,
        ));
    }

    collect_ink(stats, &mut steps);

    Ok(Report { steps })
}

/// A note's document, open, as the app holds it.
///
/// The two things that own memory: the note — its SQLite connection and the document's bytes — and the
/// view on the document, which owns the bitmaps. Both are given up together when this is dropped.
struct Opened {
    _note: Note,
    pdf: PdfDocumentView,
}

/// Opens a note's document the way the app does, and pays for its first page.
///
/// The page is rendered because that is what the app does when a note is opened, and it is the largest
/// single allocation in the sequence — a report that skipped it would miss the thing being measured.
fn open(path: &Path) -> Result<Opened, String> {
    if !path.is_dir() {
        return Err(String::from("pass the note's folder, not the document"));
    }

    let note = Note::open(path).map_err(|error| error.to_string())?;

    let lean = note_format::paper::MemoryMode::Lean;

    let mut pdf = match note.document() {
        Ok(Some((name, bytes))) => {
            // The lean budget and the lean ceiling, because a report that measured the generous
            // configuration would be measuring the one a reader with a big textbook is deliberately not
            // using — and Lean is the mode this exists to check.
            PdfDocumentView::open_bytes(
                name,
                bytes,
                lean.pixel_ceiling(),
                lean.cache_budget_bytes(),
            )
            .map_err(|error| error.to_string())?
        }
        Ok(None) => PdfDocumentView::empty(),
        Err(error) => return Err(error.to_string()),
    };

    if pdf.is_loaded() {
        let _ = pdf.render_page(PageRequest::new(0, lean.pixel_ceiling()));
    }

    Ok(Opened { _note: note, pdf })
}

/// Takes one reading, with the cache's figures filled in from the open document when there is one.
fn measure(
    label: String,
    kind: Kind,
    opened: Option<&Opened>,
    stats: &mut dyn crate::platform::SystemStats,
) -> Step {
    let stats_of_cache = opened.map(|opened| opened.pdf.stats());

    Step {
        label,
        kind,
        rss_bytes: stats.vitals().map_or(0, |vitals| vitals.resident_bytes),
        surface_bytes: stats.surface_bytes(),
        cache_bytes: stats_of_cache.map_or(0, |pdf| pdf.bytes),
        pages: opened.map_or(0, |opened| opened.pdf.cache_len()),
    }
}

/// A name for the report's rows: the note's folder, or the path as it was given.
fn note_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// How many pages the ink scenarios write, and how many strokes on each.
///
/// More than [`crate::ink::PAGES_KEPT`] so that the page cache is actually pushed past its bound — a run
/// that wrote fewer would report a flat figure for the trivial reason that nothing had been given up yet.
const SCENARIO_PAGES: usize = crate::ink::PAGES_KEPT + 8;

/// How many strokes one scenario page holds.
const SCENARIO_STROKES: usize = 2;

/// How many zoom rungs the rebuild scenario walks.
const SCENARIO_RUNGS: usize = 6;

/// The ink and note scenarios: the half of the app's memory a report about PDFs cannot see.
///
/// Each block builds something, measures it, and drops it before the next one starts — so a figure that
/// does not come back is one scenario's, not the run's. What they are *for* is stated on [`collect`];
/// this is how each is built.
///
/// The rows are written whether or not the run has notes to open, because these scenarios need none: an
/// ink-only leak is invisible to a `--memory-report` of a textbook and is the larger of the two halves.
fn collect_ink(stats: &mut dyn crate::platform::SystemStats, steps: &mut Vec<Step>) {
    collect_pages(stats, steps);
    collect_undo(stats, steps);
    collect_zoom(stats, steps);
    collect_note_writes(stats, steps);

    // The last row, so that the verdict is about the process after *everything* has been given up rather
    // than about whichever scenario happened to run last. Without it a reader has to know the order of
    // the scenarios to read the summary, and the summary is the line they will look at.
    steps.push(measure(String::from("end"), Kind::Ink, None, stats));
}

/// A few strokes of the shape a hand makes, for a scenario page to hold.
///
/// Short on purpose — a report is not a benchmark, and a page of these is enough to make the derived
/// geometry real without making the run take minutes.
fn scenario_strokes(page: usize) -> Vec<Stroke> {
    (0..SCENARIO_STROKES)
        .map(|stroke| {
            let seed = (page * SCENARIO_STROKES + stroke) as f32;
            let mut points = Vec::with_capacity(40);
            let (mut x, mut y) = (80.0f32, 80.0f32 + seed * 0.5);

            for reading in 0..40 {
                let t = seed + reading as f32;

                x += 2.0 + (t * 0.7).sin() * 0.6;
                y += (t * 1.1).cos() * 0.9;

                points.push(InkPoint::new(x, y, 3.0));
            }

            let mut stroke = Stroke::new(points[0], 0x0011_2233);
            stroke.points = points;

            stroke
        })
        .collect()
}

/// Writes [`SCENARIO_PAGES`] pages and turns back through them.
///
/// What this catches is the page cache growing without bound: the count of pages a note holds is what its
/// memory is proportional to, so the figure after the walk should be the same whether eight pages were
/// written or eighty. The turn back is the other half — a page given up has to come back, or the bound
/// would be a silent loss of ink.
fn collect_pages(stats: &mut dyn crate::platform::SystemStats, steps: &mut Vec<Step>) {
    let mut notes = crate::ink::Notes::new();

    for page in 0..SCENARIO_PAGES {
        if page > 0 {
            notes.go_to(page);
        }

        notes.put_page(
            page,
            crate::ink::InkDocument::from_strokes(scenario_strokes(page)),
        );
    }

    steps.push(ink_step(
        format!("ink: {SCENARIO_PAGES} pages written"),
        &notes,
        stats,
    ));

    for page in (0..SCENARIO_PAGES).rev() {
        notes.go_to(page);
    }

    steps.push(ink_step(
        String::from("ink: turned back through"),
        &notes,
        stats,
    ));

    drop(notes);
    steps.push(measure(
        String::from("ink: dropped"),
        Kind::Ink,
        None,
        stats,
    ));
}

/// Draws a page, takes every stroke back, and gives the page up.
///
/// The undo stack holds what it took back — that is what makes undo undoable — so a page whose history
/// was not bounded would keep every stroke it ever drew alive. [`crate::history::HISTORY_EDITS`] is the
/// bound; this is what would catch it moving or being bypassed.
///
/// Drawn by *clearing* rather than one stroke at a time, because a clear is one edit holding every stroke
/// it removed: the history's bound is on edits, and this is the shape that puts the most ink behind it.
fn collect_undo(stats: &mut dyn crate::platform::SystemStats, steps: &mut Vec<Step>) {
    // Many strokes, because a clear's edit holds every one of them: this is the shape that puts the most
    // ink behind a bounded history, and so the one that would show the bound not holding.
    let drawn = 200;
    let mut page = crate::ink::InkDocument::from_strokes(
        (0..drawn).flat_map(scenario_strokes).collect::<Vec<_>>(),
    );

    steps.push(measure(
        format!("ink: {} strokes drawn", page.stroke_count()),
        Kind::Ink,
        None,
        stats,
    ));

    page.clear();

    steps.push(measure(
        format!("ink: cleared, {} left", page.stroke_count()),
        Kind::Ink,
        None,
        stats,
    ));

    while page.undo() {}

    steps.push(measure(
        format!("ink: undone, {} back", page.stroke_count()),
        Kind::Ink,
        None,
        stats,
    ));

    drop(page);
    steps.push(measure(
        String::from("ink: page dropped"),
        Kind::Ink,
        None,
        stats,
    ));
}

/// Rebuilds one page at the whole zoom ladder.
///
/// Detail is a rung, and crossing one re-derives every stroke's geometry (`Stroke::freeze`). What this
/// catches is a rebuild that *adds* rather than replaces — an `Arc` copied and the old vectors kept, say
/// — which over six rungs would show as a figure that climbs and never returns.
fn collect_zoom(stats: &mut dyn crate::platform::SystemStats, steps: &mut Vec<Step>) {
    let mut notes = crate::ink::Notes::new();
    notes.put_page(
        0,
        crate::ink::InkDocument::from_strokes(scenario_strokes(0)),
    );

    for rung in 0..SCENARIO_RUNGS {
        notes.set_zoom(2f32.powi(rung as i32));
        notes.build_detail();
    }

    steps.push(ink_step(
        format!("ink: {SCENARIO_RUNGS} zoom rungs"),
        &notes,
        stats,
    ));

    drop(notes);
    steps.push(measure(
        String::from("ink: page dropped"),
        Kind::Ink,
        None,
        stats,
    ));
}

/// Writes pages to a real note through the writer thread, then closes it.
///
/// The writer is a thread with its own SQLite connection, and the note is a folder — both are state that
/// lives outside the page cache and would not be released with it. This is the one scenario that needs a
/// note on disk, so it makes one in the temporary directory and removes it afterwards.
fn collect_note_writes(stats: &mut dyn crate::platform::SystemStats, steps: &mut Vec<Step>) {
    let Ok(folder) = scenario_note_folder() else {
        steps.push(measure(
            String::from("note: could not make one"),
            Kind::Ink,
            None,
            stats,
        ));
        return;
    };

    let written = write_scenario_note(&folder);

    steps.push(measure(
        format!("note: {written} pages written"),
        Kind::Ink,
        None,
        stats,
    ));

    // Dropping the note is what closes its writer thread and its connection — the drop the app performs
    // when a reader leaves a note.
    drop(open(&folder));

    steps.push(measure(
        String::from("note: closed"),
        Kind::Ink,
        None,
        stats,
    ));

    // The folder is the report's, not the reader's: it was made to be measured and is not anyone's note.
    let _ = std::fs::remove_dir_all(&folder);
}

/// Makes an empty folder in the temporary directory for a scenario note.
fn scenario_note_folder() -> Result<PathBuf, std::io::Error> {
    let base = std::env::temp_dir().join(format!("cheap-note-report-{}", std::process::id()));

    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base)?;

    Ok(base)
}

/// Writes [`SCENARIO_PAGES`] pages into `folder`, and waits for them to land.
///
/// The pages go through the store directly rather than through the writer thread, for the same reason a
/// measurement should not race a queue: the figures are read after this returns, and a write still in
/// flight would be a figure about the queue rather than about the note. The writer is spawned anyway and
/// dropped at the end, because it is the other thing a note owns — a thread and its own connection — and
/// it is part of what an open note costs.
///
/// Returns how many pages were written, for the row's label.
fn write_scenario_note(folder: &Path) -> usize {
    let Ok(mut note) = Note::placed_from(folder, folder) else {
        return 0;
    };

    let writer = crate::note::NoteWriter::spawn(note.dir()).ok();

    let mut written = 0;

    for page in 0..SCENARIO_PAGES {
        let strokes: Vec<Stroke> = scenario_strokes(page)
            .into_iter()
            .map(|mut stroke| {
                stroke.close();
                stroke
            })
            .collect();

        let history = note_format::history::HistoryUpdate {
            appended: Vec::new(),
            applied: 0,
            count: strokes.len(),
        };

        if note
            .store_mut()
            .append(page as u64, &strokes, &history)
            .is_ok()
        {
            written += 1;
        } else {
            break;
        }
    }

    // Ends the writer's thread and closes its connection.
    drop(writer);

    written
}

/// A reading of the process, with a note's page count filled in.
///
/// The ink rows report `pages` as how many pages the note is holding, which is what the page-cache bound
/// is about — so a reader comparing the two halves of the report can tell which one a row came from.
fn ink_step(
    label: String,
    notes: &crate::ink::Notes,
    stats: &mut dyn crate::platform::SystemStats,
) -> Step {
    Step {
        label,
        kind: Kind::Ink,
        rss_bytes: stats.vitals().map_or(0, |vitals| vitals.resident_bytes),
        surface_bytes: stats.surface_bytes(),
        cache_bytes: 0,
        pages: notes.taken_len() + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(label: &str, rss_mib: u64) -> Step {
        Step {
            label: label.to_string(),
            kind: Kind::Document,
            rss_bytes: rss_mib * 1024 * 1024,
            surface_bytes: None,
            cache_bytes: 0,
            pages: 0,
        }
    }

    fn report(rss: &[u64]) -> Report {
        Report {
            steps: vec![
                Step {
                    kind: Kind::Baseline,
                    ..step("baseline", rss[0])
                },
                step("close", rss[1]),
            ],
        }
    }

    /// A run with both halves: a `baseline`, an ink scenario, and the `end` row.
    fn both(document_close: u64, ink_end: u64) -> Report {
        Report {
            steps: vec![
                Step {
                    kind: Kind::Baseline,
                    ..step("baseline", 100)
                },
                step("open", 180),
                step("close", document_close),
                Step {
                    kind: Kind::Ink,
                    ..step("ink: drawn", 120)
                },
                Step {
                    kind: Kind::Ink,
                    ..step("end", ink_end)
                },
            ],
        }
    }

    /// A run that ends where it started, or below, is clean.
    #[test]
    fn a_run_that_comes_back_is_clean() {
        assert!(report(&[100, 100]).is_clean(), "unchanged");
        assert!(report(&[100, 80]).is_clean(), "smaller than it started");
    }

    /// Growth inside the allowance is ordinary rather than a leak.
    ///
    /// The boundary is inclusive, because the allowance is what a close is *permitted* to leave rather
    /// than the first figure that is too much.
    #[test]
    fn growth_within_the_allowance_is_not_a_leak() {
        let exactly = 100 + LEAK_TOLERANCE_BYTES / (1024 * 1024);
        assert!(report(&[100, exactly]).is_clean(), "exactly the allowance");

        assert!(
            !report(&[100, exactly + 1]).is_clean(),
            "one mebibyte over is a leak"
        );
    }

    /// The verdict is in the text, so a reader does not have to work it out from the numbers.
    #[test]
    fn the_report_says_which_way_it_went() {
        assert!(report(&[100, 100]).render().contains("ok"));
        assert!(!report(&[100, 100]).render().contains("LEAK"));

        let leaked = report(&[100, 200]).render();
        assert!(leaked.contains("LEAK"));
        assert!(leaked.contains("leak: documents left 100 MiB behind"));
    }

    /// The ink half is judged against its own allowance, not the document's.
    ///
    /// The failing figure in this run is 40 MiB, which is under the 64 MiB document allowance and over the
    /// ink one. Against a single allowance this run would pass; the split is what makes it fail.
    #[test]
    fn the_ink_half_has_its_own_allowance() {
        let ink_over = both(100, 140).render();
        assert!(
            ink_over.contains("LEAK"),
            "40 MiB is over the ink allowance"
        );
        assert!(ink_over.contains("the ink and note scenarios left"));

        // The same ink figure with a document that came back is still a failure: one bad half fails the run.
        assert!(!both(100, 140).is_clean());

        // Within its own allowance, it passes.
        assert!(both(100, 110).is_clean());
    }

    /// A document's figure and the ink's are measured from different baselines.
    ///
    /// The document half runs `baseline → close`; the ink half runs from the last `close` to `end`, because that
    /// is the state the scenarios began in. Here the document grew 50 MiB (inside its 64) and the ink 50 MiB
    /// (outside its 32), and the whole-run figure is 100 — which a single 64 MiB allowance looking only at the
    /// run's total would have passed.
    #[test]
    fn each_half_is_measured_from_its_own_start() {
        let run = both(150, 200);
        let mib = 1024 * 1024;

        assert_eq!(run.document_growth(), Some(50 * mib as i64));
        assert_eq!(
            run.ink_growth(),
            Some(50 * mib as i64),
            "from the close, which is where the scenarios started"
        );
        assert_eq!(run.growth_bytes(), Some(100 * mib as i64));
        assert!(!run.is_clean(), "the ink half is where it failed");
        assert!(run.render().contains("the ink and note scenarios left"));
    }

    /// An ink-only run is judged on the ink, not skipped for having no document.
    ///
    /// `nothing to measure` and `measured and passed` must not be the same answer, so a run with only ink rows
    /// passes on its ink figure and an empty run does not pass at all.
    #[test]
    fn an_ink_only_run_is_judged_on_the_ink() {
        let ink_only = Report {
            steps: vec![
                Step {
                    kind: Kind::Baseline,
                    ..step("baseline", 100)
                },
                Step {
                    kind: Kind::Ink,
                    ..step("end", 110)
                },
            ],
        };

        assert_eq!(ink_only.document_growth(), None, "no document was measured");
        assert_eq!(ink_only.ink_growth(), Some(10 * 1024 * 1024));
        assert!(ink_only.is_clean());
    }

    /// No steps is no measurement, which is not a pass.
    #[test]
    fn an_empty_report_is_not_clean() {
        let empty = Report { steps: Vec::new() };
        assert_eq!(empty.growth_bytes(), None);
        assert!(!empty.is_clean());
        assert!(empty.render().contains("cannot measure"));
    }

    /// A delta is the step against the one before it, signed.
    #[test]
    fn a_line_carries_its_delta() {
        let before = step("baseline", 100);
        let after = step("open", 180);
        let closed = step("close", 120);

        assert!(after.line(Some(&before)).contains("+80 MiB"));
        assert!(closed.line(Some(&after)).contains("-60 MiB"));
        assert!(
            before.line(None).contains('—'),
            "the first row has nothing to compare"
        );
    }

    /// A long name is cut in the middle, and the ends survive.
    ///
    /// The tail is the part that tells one part of a textbook from the next, which is why the cut is
    /// made there rather than at the end of the string.
    #[test]
    fn a_long_label_keeps_its_ends() {
        let label = "Harry_Kim_New_Ogma_Physics_GPA_-_Part-1_Mechanics-a3f0da25";
        let cut = truncate(label, 24);

        assert_eq!(cut.chars().count(), 24);
        assert!(cut.starts_with("Harry_Kim"));
        assert!(cut.ends_with("a3f0da25"));
        assert!(cut.contains('…'));
    }

    /// A name that fits is left exactly as it was.
    #[test]
    fn a_short_label_is_not_touched() {
        assert_eq!(truncate("화학-7f07d9fa", 24), "화학-7f07d9fa");
        assert_eq!(truncate("abcdefghij", 10), "abcdefghij");
    }

    /// Both mains dispatch on this flag, and the usage says the same thing they do.
    #[test]
    fn the_flag_is_the_one_both_mains_dispatch_on() {
        assert_eq!(FLAG, "--memory-report");
        assert!(usage().contains(FLAG));
        assert!(usage().contains("--format table|json"));
    }

    /// The scenarios write more pages than the note keeps, or the bound would never be exercised.
    ///
    /// This is the assertion that keeps the report honest as the bound moves: if `PAGES_KEPT` were raised
    /// past `SCENARIO_PAGES`, every page row would be flat for the trivial reason that nothing was given
    /// up, and a leak past the bound would go unreported.
    #[test]
    fn the_page_scenario_pushes_past_the_notes_bound() {
        // A `const` block rather than a runtime assert: the two values are constants, so the check is the
        // compiler's to make, and clippy is right that a runtime assertion on them can never be false.
        const { assert!(SCENARIO_PAGES > crate::ink::PAGES_KEPT) };

        // And the failure this guards is real: `SCENARIO_PAGES` is defined *from* `PAGES_KEPT`, so raising
        // the bound moves the scenario with it, and the margin is what keeps the rows meaningful.
        assert_eq!(
            SCENARIO_PAGES,
            crate::ink::PAGES_KEPT + 8,
            "the scenario writes a margin past the bound, so the bound is actually crossed"
        );
    }

    /// A scenario page is writeable ink, not an empty vector: a page with no strokes is not held at all,
    /// so a scenario made of them would measure nothing.
    #[test]
    fn a_scenario_page_holds_real_strokes() {
        let strokes = scenario_strokes(0);

        assert_eq!(strokes.len(), SCENARIO_STROKES);

        for stroke in strokes {
            assert!(
                stroke.points.len() >= 2,
                "a stroke is a line, so it needs somewhere to go"
            );
        }
    }

    /// The scenarios leave nothing behind: what each one built is dropped before the next.
    ///
    /// Measured through the note's own count rather than through memory, because a resident figure cannot
    /// tell a freed block from a held one — the allocator keeps both. What this pins is the *structure*:
    /// a run that ended holding pages would be one whose drop was forgotten.
    #[test]
    fn the_page_scenario_gives_its_pages_back() {
        let mut notes = crate::ink::Notes::new();

        for page in 0..SCENARIO_PAGES {
            if page > 0 {
                notes.go_to(page);
            }

            notes.put_page(
                page,
                crate::ink::InkDocument::from_strokes(scenario_strokes(page)),
            );
        }

        assert_eq!(
            notes.taken_len(),
            crate::ink::PAGES_KEPT,
            "the scenario's pages are bounded by the note's own bound"
        );
    }
}
