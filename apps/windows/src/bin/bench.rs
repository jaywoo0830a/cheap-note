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
    std::process::exit(cheap_note::platform::bench::run());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("cheap-note-bench: this is the Windows benchmark; build it on Windows.");
    std::process::exit(1);
}
