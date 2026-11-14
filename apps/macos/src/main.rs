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
    // `--format` is read here rather than in the view for the same reason: only this file knows which
    // platform it is, and the contract's JSON names it.
    //
    // A build with no counters can still run: the report says so and exits `1`, which is the answer a CI
    // job should see rather than a pass it did not earn.
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--bench`: what a frame of this canvas costs, measured rather than guessed. The same command line
    // the Windows build answers, so the two numbers can be put in one table — see `doc/CLI.md`.
    if args.first().map(String::as_str) == Some("--bench") {
        // The shared flags are taken off here, so the benchmark is handed only what it owns — the same
        // split `--memory-report` makes, and the reason `--format` means one thing on both.
        let options = match note_ui::report::Options::parse(&args[1..]) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("cheap-note: {error}");
                std::process::exit(2);
            }
        };

        if options.help {
            platform::bench::print_usage();
            std::process::exit(0);
        }

        std::process::exit(platform::bench::run(&options.rest, options.format));
    }

    if args.first().map(String::as_str) == Some(note_ui::memory_report::FLAG) {
        let options = match note_ui::report::Options::parse(&args[1..]) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("cheap-note: {error}");
                std::process::exit(2);
            }
        };

        let mut stats = platform::vitals::ProcessStats::install();

        let code = match stats.as_mut() {
            Some(stats) => note_ui::memory_report::run_in(
                &options
                    .rest
                    .iter()
                    .map(std::path::PathBuf::from)
                    .collect::<Vec<_>>(),
                stats,
                &mut std::io::stdout(),
                options.format,
                "macos",
            ),
            None => {
                println!("this build cannot measure memory: no process counters");
                1
            }
        };

        std::process::exit(code);
    }

    // A flag this build does not know is a mistake, not a request to open a window. Saying so is the
    // difference between a typo and an app that seems to ignore what it was asked.
    if let Some(unknown) = args.iter().find(|arg| arg.starts_with('-')) {
        eprintln!("cheap-note: unknown flag: {unknown}");
        eprintln!(
            "usage: cheap-note [--memory-report <note> ... | --bench] [-h | --help]"
        );
        std::process::exit(2);
    }

    note_ui::run(platform::INSTALLER);
}
