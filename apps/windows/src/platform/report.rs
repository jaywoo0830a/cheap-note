//! `--memory-report` on Windows, and where its table goes.
//!
//! The report itself is the view's: [`note_ui::memory_report`] owns the operations, the arithmetic and
//! the verdict. This module supplies the one thing the view cannot: the process's own counters
//! ([`ProcessStats`]) and a place to write the table from a *windows-subsystem* image, which has no
//! console beside its window.
//!
//! ## Where the table goes
//!
//! A release build started from a terminal has a parent console but no standard handles of its own, so
//! both routes are tried, cheapest first:
//!
//! 1. the standard output handle, which is what a redirected run (`> report.txt`) and a debug build
//!    already have;
//! 2. `CONOUT$`, opened after attaching to the parent console — this is the route a release build started
//!    from PowerShell or `cmd` needs, because the standard output handle it inherits may be absent.
//!
//! If neither exists — a launch from Explorer, or a terminal that refused the attach — the table is
//! written to `cheap-note-memory-report.txt` beside the executable, with the temporary directory as the
//! fallback. The exit code is the report's own either way, so a CI job sees the verdict even when it
//! redirected the output itself.
//!
//! The GPU column stays `—`: Windows has no `surface_bytes` implementation yet, and a zero would be a
//! claim about an empty GPU rather than an absent measurement (see `SystemStats::surface_bytes`).

use std::io::Write;
use std::path::PathBuf;

use super::vitals::ProcessStats;

/// Runs the report over `paths` and returns the exit code the process should end with.
///
/// The table is buffered rather than streamed because it has to be offered to a console and a file in
/// turn: a writer that had already emitted half the table to a failing handle could not then put the
/// whole of it in the fallback file. It is a handful of rows, so the buffer costs nothing worth naming.
pub fn run(paths: &[PathBuf]) -> i32 {
    let mut report = Vec::new();

    let code = match ProcessStats::install() {
        Some(mut stats) => note_ui::memory_report::run(paths, &mut stats, &mut report),
        None => {
            report.extend_from_slice(b"this build cannot measure memory: no process counters\n");
            1
        }
    };

    deliver(&report);
    code
}

/// Writes the report where a person or a CI job can read it.
fn deliver(report: &[u8]) {
    if write_to_console(report) {
        return;
    }

    if let Some(path) = report_file_beside_exe() {
        if std::fs::write(&path, report).is_ok() {
            return;
        }
    }

    let _ = std::fs::write(std::env::temp_dir().join(REPORT_FILE), report);
}

/// Attaches to the parent console when there is one, then writes the table to stdout or `CONOUT$`.
///
/// Returns whether any route accepted the whole table. `AttachConsole` failing is ordinary — Explorer
/// has no console to attach to — and the standard output and `CONOUT$` attempts below are what decide
/// whether the run has anywhere to speak.
fn write_to_console(report: &[u8]) -> bool {
    use windows::Win32::System::Console::{
        AttachConsole, GetConsoleOutputCP, SetConsoleOutputCP, ATTACH_PARENT_PROCESS,
    };

    // A failed attach means there is no parent console, not that the process is broken. A process that
    // already has one (a debug build, `cargo run`) answers with an error too, and `CONOUT$` still works.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };

    // Note names can be Korean, and the console's default code page is not UTF-8. Save the old one:
    // this console belongs to the shell that started us, and a report command should not leave the
    // reader's next prompt in a different encoding than it found it.
    let previous = unsafe { GetConsoleOutputCP() };
    let _ = unsafe { SetConsoleOutputCP(65001) };

    let mut wrote = false;

    {
        let mut stdout = std::io::stdout().lock();
        if stdout.write_all(report).is_ok() && stdout.flush().is_ok() {
            wrote = true;
        }
    }

    // The standard handle can be missing in a windows-subsystem image even after the attach. `CONOUT$`
    // is the device itself, so it does not depend on what the image inherited.
    if !wrote {
        if let Ok(mut console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            if console.write_all(report).is_ok() && console.flush().is_ok() {
                wrote = true;
            }
        }
    }

    if previous != 0 && previous != 65001 {
        let _ = unsafe { SetConsoleOutputCP(previous) };
    }

    wrote
}

/// The fallback's first choice: beside the executable.
///
/// The same order as the panic hook in `main`, for the same reason: a carried release is a folder, and
/// the report belongs with the binary it describes. An installation directory may not be writable, so
/// `deliver` falls through to the temporary directory rather than to no report at all.
fn report_file_beside_exe() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|executable| {
        executable
            .parent()
            .map(|directory| directory.join(REPORT_FILE))
    })
}

/// The name of the fallback report, beside the executable or in the temporary directory.
const REPORT_FILE: &str = "cheap-note-memory-report.txt";

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback has a name, and it says which command wrote it.
    #[test]
    fn the_fallback_file_is_named_for_the_report() {
        assert_eq!(REPORT_FILE, "cheap-note-memory-report.txt");
    }
}
