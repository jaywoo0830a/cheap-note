//! Where a measuring command's output goes on Windows.
//!
//! Both `--memory-report` and `--bench` are commands rather than windows, and the release image is a
//! *windows-subsystem* program: it has no console of its own. This module is the one way out they share,
//! so a run started from Explorer behaves the same way whichever measurement it asked for — a console
//! when there is one, a file beside the executable when there is not.
//!
//! ## Where the output goes
//!
//! A release build started from a terminal has a parent console but no standard handles of its own, so
//! both routes are tried, cheapest first:
//!
//! 1. the standard output handle, which is what a redirected run (`> report.txt`) and a debug build
//!    already have;
//! 2. `CONOUT$`, opened after attaching to the parent console — this is the route a release build started
//!    from PowerShell or `cmd` needs, because the standard output handle it inherits may be absent.
//!
//! If neither exists — a launch from Explorer, or a terminal that refused the attach — the output is
//! written beside the executable, with the temporary directory as the fallback. The caller names that
//! file, because the two commands write different measurements: `cheap-note-memory-report.txt` and
//! `cheap-note-bench.txt`. The exit code is the command's own either way, so a CI job sees the verdict
//! even when it redirected the output itself.
//!
//! ## SAFETY: module-wide
//!
//! Four `unsafe` calls, all console state rather than memory: `AttachConsole`, `GetConsoleOutputCP`
//! and two `SetConsoleOutputCP`. Each takes and returns a plain integer, holds no pointer and
//! borrows nothing, so the only thing they can get wrong is leaving the console in a state it was
//! not in — which is why the code page read before the change is put back afterwards. They run on
//! the process's first thread, where a console attachment belongs.

use std::io::Write;
use std::path::PathBuf;

/// Writes `output` where a person or a CI job can read it.
///
/// The output is buffered rather than streamed because it has to be offered to a console and a file in
/// turn: a writer that had already emitted half of it to a failing handle could not then put the whole
/// of it in the fallback file. It is a handful of rows, so the buffer costs nothing worth naming.
pub fn deliver(output: &[u8], fallback: &str) {
    if write_to_console(output) {
        return;
    }

    if let Some(path) = beside_exe(fallback) {
        if std::fs::write(&path, output).is_ok() {
            return;
        }
    }

    let _ = std::fs::write(std::env::temp_dir().join(fallback), output);
}

/// Attaches to the parent console when there is one, then writes to stdout or `CONOUT$`.
///
/// Returns whether any route accepted the whole output. `AttachConsole` failing is ordinary — Explorer
/// has no console to attach to — and the standard output and `CONOUT$` attempts below are what decide
/// whether the run has anywhere to speak.
fn write_to_console(output: &[u8]) -> bool {
    use windows::Win32::System::Console::{
        AttachConsole, GetConsoleOutputCP, SetConsoleOutputCP, ATTACH_PARENT_PROCESS,
    };

    // A failed attach means there is no parent console, not that the process is broken. A process that
    // already has one (a debug build, `cargo run`) answers with an error too, and `CONOUT$` still works.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };

    // Note names can be Korean, and the console's default code page is not UTF-8. Save the old one:
    // this console belongs to the shell that started us, and a measuring command should not leave the
    // reader's next prompt in a different encoding than it found it.
    let previous = unsafe { GetConsoleOutputCP() };
    let _ = unsafe { SetConsoleOutputCP(65001) };

    let mut wrote = false;

    {
        let mut stdout = std::io::stdout().lock();
        if stdout.write_all(output).is_ok() && stdout.flush().is_ok() {
            wrote = true;
        }
    }

    // The standard handle can be missing in a windows-subsystem image even after the attach. `CONOUT$`
    // is the device itself, so it does not depend on what the image inherited.
    if !wrote {
        if let Ok(mut console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            if console.write_all(output).is_ok() && console.flush().is_ok() {
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
/// [`deliver`] falls through to the temporary directory rather than to no report at all.
fn beside_exe(name: &str) -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|directory| directory.join(name)))
}
