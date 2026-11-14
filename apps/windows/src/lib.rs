//! The Windows platform, as a library rather than only a binary.
//!
//! There are two things that need this code: the application (`src/main.rs`, which answers
//! `--bench` as well as opening a window) and the console wrapper (`src/bin/bench.rs`). The app's
//! release image is a `windows` subsystem one, so its benchmark report goes through
//! [`platform::console`] rather than a standard handle; the wrapper exists for scripts that used the
//! old binary name. Both must draw through the same [`platform::ink_layer`] device or the numbers
//! would describe a renderer the app does not run.
//!
//! Off Windows the library is empty. `apps/windows` is a workspace member on every machine so that
//! `cargo build --workspace` and `cargo test --workspace` work on a Mac, where the real platform
//! (Direct3D, Direct2D, `pen-windows`) is neither possible nor wanted.

#[cfg(windows)]
pub mod platform;
