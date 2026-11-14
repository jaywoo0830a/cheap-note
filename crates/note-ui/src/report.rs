//! The one contract both measuring commands speak.
//!
//! ## Why this exists
//!
//! The app has two commands that answer numeric questions — `--memory-report` (what does opening and
//! closing a document cost) and `cheap-note-bench` (what does a frame cost). They grew separately, and
//! separately each is fine. Together they were two vocabularies: two argument parsers, two tables, two
//! notions of what a run's summary is, and no machine-readable form for either.
//!
//! A person reading one table at a time never notices. A script that wants to fail a build on either,
//! or an agent that wants to compare two runs, has to learn both and hope they stay put. This module is
//! the answer to that: **one envelope, one exit-code vocabulary, one way of saying "this platform
//! cannot measure that"**, and a `--format` flag that gives every command the same two shapes.
//!
//! ## The shape
//!
//! Every run produces a [`Report`]:
//!
//! ```text
//! Report {
//!     command:  "memory-report" | "bench"
//!     platform: "macos" | "windows" | "linux"
//!     tool:     free-text, the tool that produced the numbers
//!     columns:  the field names this command reports, in order
//!     rows:     one per measurement
//!     summary:  the numbers a verdict is made from
//!     verdict:  Ok | Failed | Skipped — and the reason
//! }
//! ```
//!
//! `columns` is the part that makes a reader generic: a row is a list of [`Value`]s, so a script can
//! zip `columns` against a row without knowing which command it is reading. A memory row and a frame
//! row are then the same thing to a consumer.
//!
//! ## The three rules
//!
//! 1. **Absence is not zero.** A [`Value`] is either measured or [`Value::Absent`], and absence carries
//!    the reason. A platform with no GPU ledger says `no-gpu-ledger`, never `0`.
//! 2. **A verdict is one of three words.** [`Verdict::Ok`], [`Verdict::Failed`] and [`Verdict::Skipped`]
//!    map to exit codes `0`, `1`, `2`. `Skipped` is *"this could not be measured"* and is never a pass.
//! 3. **The same fields, in the same order, on every platform.** A field a platform cannot fill is
//!    absent; the field itself does not disappear from the column list, because a consumer reading by
//!    position must not have its positions shift.
//!
//! ## Output
//!
//! [`Format::Table`] is for a person: aligned columns and a one-line summary. [`Format::Json`] is for a
//! machine: one object, no preamble, no chatter. The same [`Report`] renders both, so the two can never
//! disagree about a number.

use std::fmt::Write as _;

/// Which command produced a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `cheap-note --memory-report`.
    MemoryReport,
    /// `cheap-note-bench`.
    Bench,
}

impl Command {
    /// The name a consumer sees, in JSON and in usage lines.
    pub fn name(self) -> &'static str {
        match self {
            Command::MemoryReport => "memory-report",
            Command::Bench => "bench",
        }
    }

    /// The flag that runs it, as typed.
    pub fn flag(self) -> &'static str {
        match self {
            Command::MemoryReport => "--memory-report",
            Command::Bench => "--bench",
        }
    }
}

/// How a report was asked to be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Format {
    /// Aligned columns for a person at a terminal.
    #[default]
    Table,
    /// One object for a script.
    Json,
}

impl Format {
    /// The value the `--format` flag takes.
    pub fn name(self) -> &'static str {
        match self {
            Format::Table => "table",
            Format::Json => "json",
        }
    }

    /// Reads `--format`'s value, or `None` when it names no format this command writes.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "table" | "text" => Some(Format::Table),
            "json" => Some(Format::Json),
            _ => None,
        }
    }
}

/// One measured value, or the reason there is none.
///
/// The reason is not decoration: *"this platform has no GPU ledger"* and *"the GPU reading failed"* are
/// different facts, and a consumer that only sees `null` cannot tell them apart.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A whole number, in bytes or in count.
    Count(u64),
    /// A duration, in milliseconds, as a float — one unit everywhere, because the table is free to
    /// choose a unit per row and a script is not.
    Millis(f64),
    /// A signed number, in bytes: a delta, where the sign is the point.
    Signed(i64),
    /// Nothing to report, and why.
    Absent(&'static str),
}

impl Value {
    /// The value as JSON, on its own.
    pub fn json(&self) -> String {
        match self {
            Value::Count(n) => n.to_string(),
            Value::Millis(ms) => format!("{ms}"),
            Value::Signed(n) => n.to_string(),
            Value::Absent(_) => String::from("null"),
        }
    }

    /// The value for a human cell, or `—` when absent.
    pub fn cell(&self) -> String {
        match self {
            Value::Count(n) => n.to_string(),
            Value::Millis(ms) => format!("{ms:.2}"),
            Value::Signed(n) => format!("{n:+}"),
            Value::Absent(_) => String::from("—"),
        }
    }

    /// Whether this value was measured at all.
    pub fn is_present(&self) -> bool {
        !matches!(self, Value::Absent(_))
    }
}


/// One measurement: a label and the values [`Report::columns`] names.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// What was measured — `baseline`, `open 화학`, `316 strokes`.
    pub label: String,
    /// The values, in the order the report's columns name them.
    pub values: Vec<Value>,
}

impl Row {
    /// A row of `values` under `label`.
    pub fn new(label: impl Into<String>, values: Vec<Value>) -> Self {
        Row {
            label: label.into(),
            values,
        }
    }

    /// The value under one column name, when this command reports it.
    pub fn get(&self, columns: &[&'static str], name: &str) -> Option<&Value> {
        let at = columns.iter().position(|column| *column == name)?;
        self.values.get(at)
    }
}

/// What a run concluded, in three words that map to exit codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The run met its requirement.
    Ok,
    /// The run did not: a document did not come back, a budget was exceeded.
    Failed,
    /// The run could not measure — no paths, no counters, a platform without the hardware. **Never a
    /// pass**: an unearned success is worse than a failure, because a CI job stops looking.
    Skipped,
}

impl Verdict {
    /// The exit code this verdict means.
    pub fn exit_code(&self) -> i32 {
        match self {
            Verdict::Ok => 0,
            Verdict::Failed => 1,
            Verdict::Skipped => 2,
        }
    }

    /// The word a table prints.
    pub fn word(&self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Failed => "FAILED",
            Verdict::Skipped => "skipped",
        }
    }
}

/// One run of one command, in the shape both commands share.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Which command ran.
    pub command: Command,
    /// The platform these numbers came from: `macos`, `windows`, `linux`.
    pub platform: &'static str,
    /// What produced the numbers — a GPU API, a counter source. Free text, for a human.
    pub tool: String,
    /// The field names of every row, in order, including ones a platform cannot fill.
    pub columns: Vec<&'static str>,
    /// One row per measurement.
    pub rows: Vec<Row>,
    /// The figures a verdict was made from, as `(name, value)` pairs.
    pub summary: Vec<(String, Value)>,
    /// What the run concluded.
    pub verdict: Verdict,
    /// Why, when there is anything to say — the allowance used, a reason a column is empty.
    pub note: Option<String>,
    /// The columns this platform could not measure, and why, name by name.
    pub unmeasured: Vec<(&'static str, &'static str)>,
}

impl Report {
    /// A report with nothing in it yet, for `command` on `platform`.
    pub fn new(command: Command, platform: &'static str, tool: impl Into<String>) -> Self {
        Report {
            command,
            platform,
            tool: tool.into(),
            columns: Vec::new(),
            rows: Vec::new(),
            summary: Vec::new(),
            verdict: Verdict::Ok,
            note: None,
            unmeasured: Vec::new(),
        }
    }

    /// Names the columns, in the order every row's values arrive in.
    pub fn columns(mut self, columns: &[&'static str]) -> Self {
        self.columns = columns.to_vec();
        self
    }

    /// Records a column this platform could not measure, and why.
    ///
    /// The column stays in [`Self::columns`]: this is the note explaining why its cells are `—`.
    pub fn unmeasured(mut self, column: &'static str, reason: &'static str) -> Self {
        self.unmeasured.push((column, reason));
        self
    }

    /// Adds a row.
    pub fn push(&mut self, row: Row) {
        self.rows.push(row);
    }

    /// Adds a figure to the summary.
    pub fn summarise(&mut self, name: impl Into<String>, value: Value) {
        self.summary.push((name.into(), value));
    }

    /// Sets the verdict and the sentence that explains it.
    pub fn conclude(mut self, verdict: Verdict, note: Option<String>) -> Self {
        self.verdict = verdict;
        self.note = note;
        self
    }

    /// Every column this platform actually filled, in order.
    pub fn measured(&self) -> Vec<&'static str> {
        self.columns
            .iter()
            .copied()
            .filter(|column| !self.unmeasured.iter().any(|(name, _)| name == column))
            .collect()
    }
}


/// JSON string escaping, because these reports carry note names and paths.
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);

    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }

    out
}

/// Renders `report` in `format`.
///
/// The two shapes come from the same fields, so a number cannot differ between them.
pub fn render(report: &Report, format: Format) -> String {
    match format {
        Format::Table => table(report),
        Format::Json => json(report),
    }
}

/// The aligned-column form, for a person.
fn table(report: &Report) -> String {
    let mut out = String::new();

    // The label column is as wide as the longest label, within reason: a note called by its full path
    // must not push every number off the edge.
    let width = report
        .rows
        .iter()
        .map(|row| row.label.chars().count())
        .chain(std::iter::once(8))
        .max()
        .unwrap_or(8)
        .min(24);

    let _ = write!(out, "{:<width$}", "", width = width);
    for column in &report.columns {
        // A column a platform cannot fill is marked in the header, so a reader is not left wondering
        // why every cell under it is a dash.
        if report.unmeasured.iter().any(|(name, _)| name == column) {
            let _ = write!(out, "  {:>10}", format!("{column}?"));
        } else {
            let _ = write!(out, "  {:>10}", column);
        }
    }
    out.push('\n');

    for row in &report.rows {
        let label: String = row.label.chars().take(width).collect();
        let _ = write!(out, "{:<width$}", label, width = width);

        for at in 0..report.columns.len() {
            let cell = row
                .values
                .get(at)
                .map_or_else(|| String::from("—"), Value::cell);
            let _ = write!(out, "  {:>10}", cell);
        }
        out.push('\n');
    }

    let rule = width + report.columns.len() * 12;
    out.push_str(&"─".repeat(rule));
    out.push('\n');

    for (name, value) in &report.summary {
        let _ = write!(out, "{name} {}   ", value.cell());
    }
    out.push_str(report.verdict.word());

    if let Some(note) = &report.note {
        let _ = write!(out, " — {note}");
    }
    out.push('\n');

    // Each column a platform could not fill, with the reason. This is the part that makes an absent
    // number a sentence rather than a blank.
    for (column, reason) in &report.unmeasured {
        let _ = writeln!(out, "{column}: nothing measured — {reason}");
    }

    out
}



/// One JSON object, for a script.
///
/// Every field is always present, so a parser sees the same keys whatever the command or platform. What
/// varies is the *values*: `null` for something not measured, and `unmeasured` naming why.
fn json(report: &Report) -> String {
    let mut out = String::from("{\n");

    let _ = writeln!(out, "  \"command\": \"{}\",", report.command.name());
    let _ = writeln!(out, "  \"platform\": \"{}\",", report.platform);
    let _ = writeln!(out, "  \"tool\": \"{}\",", json_escape(&report.tool));

    let measured: Vec<String> = report
        .measured()
        .iter()
        .map(|column| format!("\"{}\"", json_escape(column)))
        .collect();
    let _ = writeln!(out, "  \"measured\": [{}],", measured.join(", "));

    let unmeasured: Vec<String> = report
        .unmeasured
        .iter()
        .map(|(column, reason)| {
            format!(
                "{{ \"column\": \"{}\", \"reason\": \"{}\" }}",
                json_escape(column),
                json_escape(reason)
            )
        })
        .collect();
    let _ = writeln!(out, "  \"unmeasured\": [{}],", unmeasured.join(", "));

    let columns: Vec<String> = report
        .columns
        .iter()
        .map(|column| format!("\"{}\"", json_escape(column)))
        .collect();
    let _ = writeln!(out, "  \"columns\": [{}],", columns.join(", "));

    out.push_str("  \"rows\": [\n");
    for (at, row) in report.rows.iter().enumerate() {
        let mut fields = vec![format!("\"label\": \"{}\"", json_escape(&row.label))];

        for (position, column) in report.columns.iter().enumerate() {
            let value = row
                .values
                .get(position)
                .map_or_else(|| String::from("null"), Value::json);
            fields.push(format!("\"{}\": {value}", json_escape(column)));
        }

        let comma = if at + 1 == report.rows.len() { "" } else { "," };
        let _ = writeln!(out, "    {{ {} }}{comma}", fields.join(", "));
    }
    out.push_str("  ],\n");

    let summary: Vec<String> = report
        .summary
        .iter()
        .map(|(name, value)| format!("\"{}\": {}", json_escape(name), value.json()))
        .collect();
    let _ = writeln!(out, "  \"summary\": {{ {} }},", summary.join(", "));

    let verdict = match report.verdict {
        Verdict::Ok => "ok",
        Verdict::Failed => "failed",
        Verdict::Skipped => "skipped",
    };
    let _ = writeln!(out, "  \"verdict\": \"{verdict}\",");
    let _ = writeln!(out, "  \"exit_code\": {},", report.verdict.exit_code());

    match &report.note {
        Some(note) => {
            let _ = writeln!(out, "  \"note\": \"{}\"", json_escape(note));
        }
        None => out.push_str("  \"note\": null\n"),
    }

    out.push_str("}\n");
    out
}

/// The arguments both commands share, taken off the front of a command line.
///
/// One parser for both, so `--format` means the same thing everywhere and a mistake gets the same
/// sentence from either command. What each command does *not* share — `--strokes`, note folders — it
/// takes from [`Options::rest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// How to write the report.
    pub format: Format,
    /// The arguments this command owns, in the order they were typed.
    pub rest: Vec<String>,
    /// `--help` was asked for. The caller prints its usage and exits `0`.
    pub help: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            format: Format::Table,
            rest: Vec::new(),
            help: false,
        }
    }
}

impl Options {
    /// Takes the shared flags out of `args`, leaving everything else in [`Self::rest`].
    ///
    /// `--format json` and `--format=json` both work, because a script writer will type both and neither
    /// is wrong. A `--` stops the scan, so a path that begins with a dash can still be passed.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Options::default();
        let mut at = 0;

        while at < args.len() {
            let arg = args[at].as_str();

            match arg {
                "--" => {
                    options.rest.extend(args[at + 1..].iter().cloned());
                    break;
                }
                "--help" | "-h" => options.help = true,
                "--format" => {
                    at += 1;
                    let name = args.get(at).ok_or("--format needs table or json")?;
                    options.format = Format::from_name(name)
                        .ok_or_else(|| format!("unknown format: {name} (table or json)"))?;
                }
                other if other.starts_with("--format=") => {
                    let name = &other["--format=".len()..];
                    options.format = Format::from_name(name)
                        .ok_or_else(|| format!("unknown format: {name} (table or json)"))?;
                }
                _ => options.rest.push(args[at].clone()),
            }

            at += 1;
        }

        Ok(options)
    }
}

/// Writes `report` to `out` in `format`, and returns the exit code it carries.
///
/// Both commands end here, so an exit code cannot drift from the verdict in the table: it is read off
/// the same [`Verdict`].
pub fn emit(report: &Report, format: Format, out: &mut impl std::io::Write) -> i32 {
    let _ = out.write_all(render(report, format).as_bytes());
    let _ = out.flush();
    report.verdict.exit_code()
}



#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Report {
        let mut report = Report::new(Command::MemoryReport, "macos", "task_vm_info")
            .columns(&["rss", "delta", "cache", "gpu", "pages"])
            .unmeasured("gpu", "this platform has no graphics ledger");

        report.push(Row::new(
            "baseline",
            vec![
                Value::Count(11),
                Value::Absent("nothing above it"),
                Value::Count(0),
                Value::Absent("no-gpu-ledger"),
                Value::Count(0),
            ],
        ));
        report.push(Row::new(
            "open 화학",
            vec![
                Value::Count(81),
                Value::Signed(70),
                Value::Count(11),
                Value::Absent("no-gpu-ledger"),
                Value::Count(1),
            ],
        ));

        report.summarise("growth", Value::Signed(70));
        report.conclude(Verdict::Failed, Some(String::from("allowance 64 MiB")))
    }

    /// The three verdicts are the three exit codes, and nothing else.
    #[test]
    fn a_verdict_is_one_of_three_exit_codes() {
        assert_eq!(Verdict::Ok.exit_code(), 0);
        assert_eq!(Verdict::Failed.exit_code(), 1);
        assert_eq!(Verdict::Skipped.exit_code(), 2);
    }

    /// A run that could not measure is never a pass.
    ///
    /// The rule worth pinning hardest: a CI job that sees `0` stops looking, so `Skipped` must not be
    /// able to spell success.
    #[test]
    fn skipped_is_not_a_pass() {
        assert_ne!(Verdict::Skipped.exit_code(), 0);
        assert_ne!(Verdict::Skipped.word(), Verdict::Ok.word());
    }

    /// A column a platform cannot fill keeps its place, and says why.
    ///
    /// The column list is what a script reads a row against; dropping an absent column would shift every
    /// later value under the wrong name.
    #[test]
    fn an_unmeasured_column_keeps_its_place_and_its_reason() {
        let report = sample();

        assert_eq!(report.columns.len(), 5, "the column list does not shrink");
        assert_eq!(report.measured(), vec!["rss", "delta", "cache", "pages"]);
        assert_eq!(
            report.unmeasured,
            vec![("gpu", "this platform has no graphics ledger")]
        );

        let row = &report.rows[0];
        assert_eq!(
            row.get(&report.columns, "gpu"),
            Some(&Value::Absent("no-gpu-ledger")),
            "the cell is where it was, and says why it is empty"
        );
    }

    /// Absence renders as `null` in JSON and as a dash in the table — never as a zero.
    #[test]
    fn absence_is_never_a_zero() {
        let absent = Value::Absent("no-gpu-ledger");
        assert_eq!(absent.json(), "null");
        assert_eq!(absent.cell(), "—");
        assert!(!absent.is_present());

        let zero = Value::Count(0);
        assert_eq!(zero.json(), "0");
        assert_eq!(zero.cell(), "0");
        assert!(zero.is_present(), "a measured zero is a measurement");
    }

    /// The JSON is one object with every key, and it parses.
    ///
    /// Parsed with `serde_json` *in the test*, not in the command: the command's own rendering must not
    /// need a JSON library to be right, and the test is what checks it is.
    #[test]
    fn the_json_is_one_object_with_every_key() {
        let parsed: serde_json::Value =
            serde_json::from_str(&render(&sample(), Format::Json)).expect("valid JSON");

        for key in [
            "command",
            "platform",
            "tool",
            "measured",
            "unmeasured",
            "columns",
            "rows",
            "summary",
            "verdict",
            "exit_code",
            "note",
        ] {
            assert!(parsed.get(key).is_some(), "missing key: {key}");
        }

        assert_eq!(parsed["command"], "memory-report");
        assert_eq!(parsed["exit_code"], 1);
        assert_eq!(parsed["verdict"], "failed");
        assert_eq!(parsed["rows"][0]["gpu"], serde_json::Value::Null);
        assert_eq!(parsed["rows"][1]["delta"], 70);
        assert_eq!(parsed["unmeasured"][0]["column"], "gpu");
        assert_eq!(
            parsed["measured"],
            serde_json::json!(["rss", "delta", "cache", "pages"])
        );
    }

    /// A note name with quotes and a backslash in it is still valid JSON.
    #[test]
    fn a_label_with_quotes_still_parses() {
        let mut report = Report::new(Command::Bench, "windows", "d2d").columns(&["warm"]);
        report.push(Row::new(
            "a \"quoted\" note\\name",
            vec![Value::Millis(4.25)],
        ));

        let parsed: serde_json::Value =
            serde_json::from_str(&render(&report, Format::Json)).expect("valid JSON");

        assert_eq!(parsed["rows"][0]["label"], "a \"quoted\" note\\name");
        assert_eq!(parsed["rows"][0]["warm"], 4.25);
    }

    /// Durations are milliseconds as floats, in one unit everywhere.
    #[test]
    fn durations_are_milliseconds_as_floats() {
        assert_eq!(Value::Millis(4.25).json(), "4.25");
        assert_eq!(Value::Millis(4.0).json(), "4");
        assert_eq!(Value::Millis(4.25).cell(), "4.25");
    }

    /// A signed delta keeps its sign — that sign is the whole story of a close.
    #[test]
    fn a_delta_keeps_its_sign() {
        assert_eq!(Value::Signed(-16).json(), "-16");
        assert_eq!(Value::Signed(-16).cell(), "-16");
        assert_eq!(Value::Signed(70).cell(), "+70");
    }

    /// Both commands read `--format` the same way, and leave the rest alone.
    #[test]
    fn one_parser_for_both_commands() {
        let args: Vec<String> = ["--format", "json", "--strokes", "0,29"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let options = Options::parse(&args).expect("parses");
        assert_eq!(options.format, Format::Json);
        assert_eq!(options.rest, vec!["--strokes", "0,29"]);
        assert!(!options.help);
    }

    /// `--format=json` is the same flag, because a script writer will type both.
    #[test]
    fn the_equal_form_is_the_same_flag() {
        let args: Vec<String> = vec![String::from("--format=json")];
        assert_eq!(Options::parse(&args).unwrap().format, Format::Json);
    }

    /// A format nobody writes is a mistake with a sentence, not a silent table.
    #[test]
    fn an_unknown_format_is_an_error() {
        let args: Vec<String> = vec![String::from("--format"), String::from("yaml")];
        let error = Options::parse(&args).expect_err("yaml is not a format");
        assert!(error.contains("yaml"), "the sentence names what was typed");
    }

    /// `--` stops the scan, so a path that begins with a dash can still be measured.
    #[test]
    fn a_double_dash_stops_the_scan() {
        let args: Vec<String> = ["--", "--format", "json"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let options = Options::parse(&args).unwrap();
        assert_eq!(options.format, Format::Table);
        assert_eq!(options.rest, vec!["--format", "json"]);
    }

    /// The exit code comes off the verdict, so the two cannot drift.
    #[test]
    fn the_exit_code_is_the_verdicts() {
        let mut out = Vec::new();
        assert_eq!(emit(&sample(), Format::Table, &mut out), 1);

        let ok = Report::new(Command::Bench, "macos", "metal").conclude(Verdict::Ok, None);
        assert_eq!(emit(&ok, Format::Json, &mut Vec::new()), 0);
    }

    /// The table names an absent column, so the dashes under it are explained.
    #[test]
    fn the_table_marks_an_absent_column() {
        let text = render(&sample(), Format::Table);

        assert!(text.contains("gpu?"), "the header marks it");
        assert!(
            text.contains("gpu: nothing measured — this platform has no graphics ledger"),
            "and the reason is written out: {text}"
        );
        assert!(text.contains("FAILED"));
        assert!(text.contains("allowance 64 MiB"));
    }
}

