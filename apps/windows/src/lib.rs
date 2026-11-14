//! The Windows platform, as a library rather than only a binary.
//!
//! There are two things that need this code: the application (`src/main.rs`) and the benchmark
//! (`src/bin/bench.rs`). The benchmark is deliberately a *console* program — the app's release
//! image is a `windows` subsystem one, so it has no console to print a measurement to — and both
//! must draw through the same [`platform::ink_layer`] device or the numbers would describe a
//! renderer the app does not run.
//!
//! Off Windows the library is empty. `apps/windows` is a workspace member on every machine so that
//! `cargo build --workspace` and `cargo test --workspace` work on a Mac, where the real platform
//! (Direct3D, Direct2D, `pen-windows`) is neither possible nor wanted.

#[cfg(windows)]
pub mod platform;
