//! `cheap-note-bench`: what the Windows canvas costs, measured rather than guessed.
//!
//! This is a separate *console* binary on purpose. The app is a `windows` subsystem image in
//! release — no console beside the window — and a benchmark whose table has nowhere to go is not
//! a measurement. It links the same library the app does, and draws through the same `Device` and
//! `Renderer` (see `src/platform/bench.rs`).
//!
//! ```text
//! cargo run --release -p cheap-note --bin cheap-note-bench
//! cargo run --release -p cheap-note --bin cheap-note-bench -- --strokes 0,29,316,1000,8182 --frames 60
//! ```
//!
//! Off Windows this is the same stub the app is: the workspace builds everywhere, and the real
//! benchmark only exists where the renderer does.

#[cfg(windows)]
fn main() {
    // A thin wrapper kept for scripts that call the old binary name directly. The command line is
    // `cheap-note --bench` now — one flag on both platforms — and this forwards to the same runner.
    let args: Vec<String> = std::env::args().skip(1).collect();

    let options = match note_ui::report::Options::parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("cheap-note-bench: {error}");
            std::process::exit(2);
        }
    };

    if options.help {
        cheap_note::platform::bench::print_usage();
        std::process::exit(0);
    }

    std::process::exit(cheap_note::platform::bench::run(
        &options.rest,
        options.format,
        &mut std::io::stdout(),
    ));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("cheap-note-bench: this is the Windows benchmark; build it on Windows.");
    std::process::exit(1);
}
