//! `cheap-note` on macOS.
//!
//! The *platform* half of the application, and nothing else. Everything a note-taking program does —
//! the bar, the home screen, the sheet, the gestures, the page cache, the ink model, the frame loop —
//! is `crates/note-ui`, written once and portable. What is here is the things that crate cannot
//! write for another machine:
//!
//! | Module | What it lends the view |
//! |---|---|
//! | [`platform::pen`] | `pen-macos`: the digitizer's position, pressure and tilt |
//! | [`platform::pointer`] | `NSCursor`, so the system pointer can be put away while a ghost is up |
//! | [`platform::canvas`] | the desk, the paper and the ink: Metal geometry, drawn by the platform |
//!
//! Plus `main`, which assembles them and installs the one hook the release profile needs.

mod platform;

fn main() {
    // The release profile is `panic = "abort"`, and the pen's readings arrive inside an AppKit callback,
    // where a panic cannot unwind: the process then dies with "panic in a function that cannot unwind" and
    // whatever actually went wrong is never printed. This hook is what prints it — the message, the place
    // in the source, and where the code was — so a crash in a release build is a sentence rather than a
    // mystery. It costs nothing when nothing panics.
    std::panic::set_hook(Box::new(|panic| {
        let mut report = String::new();
        report.push_str("cheap-note panicked: ");
        report.push_str(&panic.to_string());

        if std::env::var_os("RUST_BACKTRACE").is_some() {
            report.push('\n');
            report.push_str(&std::backtrace::Backtrace::force_capture().to_string());
        }

        eprintln!("{report}");
    }));

    // `--memory-report <note> ...`: measure what opening and closing documents costs, print it, exit.
    // Handled here rather than inside the view because the report needs the platform's own counters and
    // no window at all — see `note_ui::memory_report`, which owns the arithmetic and the verdict.
    //
    // A build with no counters can still run: the report says so and exits `1`, which is the answer a CI
    // job should see rather than a pass it did not earn.
    if let Some(paths) = note_ui::memory_report::requested() {
        let mut stats = platform::vitals::ProcessStats::install();

        let code = match stats.as_mut() {
            Some(stats) => note_ui::memory_report::run(&paths, stats, &mut std::io::stdout()),
            None => {
                println!("this build cannot measure memory: no process counters");
                1
            }
        };

        std::process::exit(code);
    }

    note_ui::run(platform::INSTALLER);
}
