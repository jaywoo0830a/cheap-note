//! `cheap-note` on Windows.
//!
//! ## What this crate is
//!
//! The *platform* half of the application, and nothing else. Everything a note-taking program does —
//! the bar, the home screen, the sheet, the gestures, the page cache, the ink model, the frame loop —
//! is `crates/note-ui`, written once and portable. What is here is the things that crate cannot
//! write for another machine:
//!
//! | Module | What it lends the view |
//! |---|---|
//! | [`platform::pen`] | `pen-windows` on a thread of its own: WM_POINTER readings, coalesced |
//! | [`platform::cursor_overlay`] | the nib's ghost, drawn in a layered window above everything |
//! | [`platform::system_cursor`] | `SetCursor`, so the system pointer is out of the way while it is |
//! | [`platform::ink_layer`] | the desk, the paper and the ink: Direct3D 11, Direct2D, DirectComposition |
//!
//! Plus Pdfium's own C entry points, which are `#[cfg(windows)]` in the view's `pdfium` module for
//! now, and `main`, which is the six lines that assemble the lot (see [`platform`]).
//!
//! ## What it means for the next platform
//!
//! A macOS build is this crate, rewritten: the canvas on Metal or CoreGraphics, the pen off `NSEvent`,
//! the ghost drawn inside the frame rather than in a window of its own, and `libpdfium.dylib` behind
//! `dlopen`. Nothing above the seam changes — that is what the seam is for.
//!
//! ## The documents
//!
//! [`doc/ARCHITECTURE.md`](../../doc/ARCHITECTURE.md) is the picture of the whole program: which thread
//! owns what, what crosses a thread boundary, and what may wait for what. [`doc/VIEW.md`](../../doc/VIEW.md)
//! is why the view is the way it is, and [`doc/STORE.md`](../../doc/STORE.md) follows one stroke to the
//! disk and back.
//!
//! Pdfium is loaded at run time from `vendor/lib/pdfium.dll`, which a build copies next to the
//! executable it produces (`build.rs`; `vendor/README.md` says where the library comes from). The app
//! starts and draws without it; only opening a PDF needs it.

// No console beside the window: this is a graphical application, so the linker is told to mark the
// image as a *windows* subsystem one (`/SUBSYSTEM:WINDOWS`). Without it the image is a console
// executable, and Windows opens a console window for one of those whenever the process is started
// with none already attached — double-clicking the executable, a shortcut, a file association, or a
// drag onto it — which is the black window that used to appear next to the app.
//
// Left to the debug and test builds on purpose. A run started *from* a terminal (`cargo run`) shares
// that terminal's console and would show either way, and `cargo test` prints the measurements its
// timing tests take: `cfg(test)` covers the test build of this crate, including `cargo test
// --release`, so those prints survive. What is lost in release is only a diagnostic (the font failing
// to register — `theme`); a panic is preserved by the hook in `main`, which writes its message to
// stderr when there is one and to `cheap-note-panic.log` beside the executable otherwise.
#![cfg_attr(
    all(windows, not(debug_assertions), not(test)),
    windows_subsystem = "windows"
)]

#[cfg(windows)]
use cheap_note::platform;

#[cfg(windows)]
fn main() {
    // The release profile is `panic = "abort"` and the image is a windows-subsystem one, so a panic
    // during startup would otherwise leave nothing behind: no console, no message, just a process
    // that vanished. This hook is what turns that into a sentence — the message, the place in the
    // source, and a backtrace when `RUST_BACKTRACE` is set — for the same reason the macOS `main`
    // has one. It costs nothing when nothing panics.
    std::panic::set_hook(Box::new(|panic| {
        let mut report = String::new();
        report.push_str("cheap-note panicked: ");
        report.push_str(&panic.to_string());

        if std::env::var_os("RUST_BACKTRACE").is_some() {
            report.push('\n');
            report.push_str(&std::backtrace::Backtrace::force_capture().to_string());
        }

        // Best-effort: a windows-subsystem build started from Explorer has no stderr handle at all,
        // and `eprintln!` would panic on it — inside the panic hook, turning one panic into an abort
        // before the report below is written.
        {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "{report}");
        }
        write_panic_report(&report);
    }));

    note_ui::run(platform::INSTALLER);
}

/// Writes a panic report where a person can find it after a release build has closed.
///
/// Beside the executable is the first choice: the release is a folder that can be carried anywhere,
/// and the report belongs with the binary it describes. An installation directory may not be
/// writable, though, so the temporary directory is the fallback rather than no report at all.
#[cfg(windows)]
fn write_panic_report(report: &str) {
    let report = format!("{report}\n");
    let beside = std::env::current_exe().ok().and_then(|executable| {
        executable
            .parent()
            .map(|directory| directory.join("cheap-note-panic.log"))
    });

    if let Some(path) = beside {
        if std::fs::write(path, &report).is_ok() {
            return;
        }
    }

    let _ = std::fs::write(std::env::temp_dir().join("cheap-note-panic.log"), report);
}

/// Off Windows this crate is a workspace member with nothing to build — the app for *this* machine is
/// `apps/macos`. It compiles to this stub so that `cargo build`/`cargo test --workspace` succeeds on a
/// Mac, where building the real `apps/windows` (its `pen-windows` capture and its Direct2D canvas) is
/// neither possible nor wanted. On Windows the cfg above makes this module disappear and the real
/// `main` take its place, so the machine it is written for is unchanged.
#[cfg(not(windows))]
fn main() {
    eprintln!(
        "cheap-note: this is the Windows build; on macOS run `cargo run -p cheap-note-macos`."
    );
    std::process::exit(1);
}
