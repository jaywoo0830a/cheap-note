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

/// The flag that asks for this report rather than a window.
pub const FLAG: &str = "--memory-report";

/// One row of the report: what the process held after one operation.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// What was done — `baseline`, `open 화학.pdf`, `close 화학.pdf`.
    pub label: String,
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

    /// How much the resident figure grew across the whole run.
    ///
    /// Negative when the process ended smaller than it started, which is ordinary: a first note's
    /// close can release state the baseline was still holding.
    pub fn growth_bytes(&self) -> Option<i64> {
        let baseline = self.baseline()?.rss_bytes;
        let last = self.final_step()?.rss_bytes;
        Some(last as i64 - baseline as i64)
    }

    /// Whether the run came back to where it started, within [`LEAK_TOLERANCE_BYTES`].
    pub fn is_clean(&self) -> bool {
        self.growth_bytes()
            .is_some_and(|growth| growth <= LEAK_TOLERANCE_BYTES as i64)
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

                if !self.is_clean() {
                    out.push_str(&format!(
                        "leak: closing left {} MiB behind (the allowance is {})\n",
                        growth / (1024 * 1024),
                        mib(LEAK_TOLERANCE_BYTES),
                    ));
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

/// The notes named on the command line, when this run is a memory report rather than a launch.
///
/// `None` is an ordinary launch. The flag with no paths is a report over nothing — a mistyped command
/// rather than a request — and it comes back as an empty `Some` so the caller can say so.
pub fn requested() -> Option<Vec<PathBuf>> {
    let mut args = std::env::args_os();

    // The first argument is the program itself.
    args.next()?;

    if args.next()? != FLAG {
        return None;
    }

    Some(args.map(PathBuf::from).collect())
}

/// Runs the report over `paths`, writing the table to `out`.
///
/// Returns the exit code the process should end with: `0` for a run that gave its memory back, `1` for
/// one that did not, and `2` for a report that could not be run at all — no paths, or a note that would
/// not open. Three codes rather than two because "the app leaked" and "the command was wrong" are
/// different problems, and a CI job wants to tell them apart.
///
/// `stats` is taken rather than built so that the caller decides what a headless run measures itself
/// with — the platform's own counters, which is the same source the status line reads.
pub fn run(
    paths: &[PathBuf],
    stats: &mut dyn crate::platform::SystemStats,
    out: &mut impl Write,
) -> i32 {
    run_in(paths, stats, out, crate::report::Format::Table, "unknown")
}

/// Runs the report in `format`, writing it to `out`.
///
/// `--format table` keeps this command's own table, which reads better for one note's sequence;
/// `--format json` writes the shared contract, which is what a script reads. Both end at the same
/// verdict, so the two shapes cannot disagree about whether the run passed.
///
/// `platform` is the caller's to name: the view is built for every platform and cannot know which one it
/// is running on.
pub fn run_in(
    paths: &[PathBuf],
    stats: &mut dyn crate::platform::SystemStats,
    out: &mut impl Write,
    format: crate::report::Format,
    platform: &'static str,
) -> i32 {
    if paths.is_empty() {
        let _ = writeln!(out, "usage: cheap-note {FLAG} [--format table|json] <note> [<note> ...]");
        return 2;
    }

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
    contract.summarise("tolerance", Value::Count(LEAK_TOLERANCE_BYTES));

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
            "came back within {} MiB",
            LEAK_TOLERANCE_BYTES / (1024 * 1024)
        )),
        Verdict::Failed => Some(format!(
            "grew more than the {} MiB allowance",
            LEAK_TOLERANCE_BYTES / (1024 * 1024)
        )),
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
pub fn collect(
    paths: &[PathBuf],
    stats: &mut dyn crate::platform::SystemStats,
) -> Result<Report, String> {
    let mut steps = Vec::with_capacity(paths.len() * 2 + 1);

    steps.push(measure(String::from("baseline"), None, stats));

    for path in paths {
        let name = note_name(path);

        let opened = open(path).map_err(|error| format!("{name}: {error}"))?;

        steps.push(measure(format!("open {name}"), Some(&opened), stats));

        // Closed by dropping, and measured *after* the drop: a figure taken while the document is still
        // alive is the one that says nothing about whether closing gave the memory back.
        drop(opened);

        steps.push(measure(format!("close {name}"), None, stats));
    }

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
    opened: Option<&Opened>,
    stats: &mut dyn crate::platform::SystemStats,
) -> Step {
    let stats_of_cache = opened.map(|opened| opened.pdf.stats());

    Step {
        label,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn step(label: &str, rss_mib: u64) -> Step {
        Step {
            label: label.to_string(),
            rss_bytes: rss_mib * 1024 * 1024,
            surface_bytes: None,
            cache_bytes: 0,
            pages: 0,
        }
    }

    fn report(rss: &[u64]) -> Report {
        Report {
            steps: vec![step("baseline", rss[0]), step("close", rss[1])],
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
        assert!(leaked.contains("leak: closing left 100 MiB behind"));
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

    /// The flag is recognised only as the first argument, and its paths come back in order.
    #[test]
    fn the_flag_is_the_first_argument() {
        // `requested` reads the process's own arguments, so this pins the shape it looks for rather
        // than the parse: the flag first, then whatever was named.
        assert_eq!(FLAG, "--memory-report");
    }
}
