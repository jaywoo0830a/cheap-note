//! `--memory-report` on Windows, and the counters it measures.
//!
//! The report itself is the view's: [`note_ui::memory_report`] owns the operations, the arithmetic and
//! the verdict. This module supplies the one thing the view cannot: the process's own counters
//! ([`ProcessStats`]) and the platform's name for the shared contract. The place its output goes is
//! [`super::console`], which both measuring commands share — a console when the run has one, a file
//! beside the executable when it does not.
//!
//! `--format json` is the same envelope here as on macOS: `memory_report::run_in` renders it from the
//! same rows and exits with the same `0` / `1` / `2`. A Windows run started from Explorer therefore
//! writes `cheap-note-memory-report.txt` rather than printing, and the file holds exactly the bytes the
//! table or the JSON would have written.
//!
//! The GPU column stays `—`: Windows has no `surface_bytes` implementation yet, and a zero would be a
//! claim about an empty GPU rather than an absent measurement (see `SystemStats::surface_bytes`).

use std::path::PathBuf;

use note_ui::report::Format;

use super::console;
use super::vitals::ProcessStats;

/// Runs the report over `paths` in `format` and returns the exit code the process should end with.
///
/// The output is buffered rather than streamed because it has to be offered to a console and a file in
/// turn: a writer that had already emitted half of it to a failing handle could not then put the whole
/// of it in the fallback file. It is a handful of rows, so the buffer costs nothing worth naming.
///
/// `format` is the shared `--format` flag, and `"windows"` is the name the contract's JSON carries. Both
/// call the same `run_in` the macOS `main` calls, so the two platforms cannot grow different tables or
/// different exit codes.
pub fn run(paths: &[PathBuf], format: Format) -> i32 {
    let mut report = Vec::new();

    let code = match ProcessStats::install() {
        Some(mut stats) => {
            note_ui::memory_report::run_in(paths, &mut stats, &mut report, format, "windows")
        }
        None => {
            report.extend_from_slice(b"this build cannot measure memory: no process counters\n");
            1
        }
    };

    console::deliver(&report, FALLBACK_FILE);
    code
}

/// The name of the fallback report, beside the executable or in the temporary directory.
pub const FALLBACK_FILE: &str = "cheap-note-memory-report.txt";

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback has a name, and it says which command wrote it.
    #[test]
    fn the_fallback_file_is_named_for_the_report() {
        assert_eq!(FALLBACK_FILE, "cheap-note-memory-report.txt");
    }
}
