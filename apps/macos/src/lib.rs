//! The macOS platform, as a library rather than only a binary.
//!
//! Two things need this code: the application (`src/main.rs`, which also answers `--bench`,
//! `--memory-report` and `--power-report`) and the measurement adapter (`src/bin/adapter.rs`, which
//! `tests/long_note.py` drives). Both must draw through the same [`platform::metal`] surface and read the
//! same counters, or the numbers in a report would describe a renderer the app does not run.
//!
//! It is the same arrangement `apps/windows` has, for the same reason: a second binary in the crate that
//! needs the platform's own modules. A binary in `src/bin/` is a separate crate root and cannot see
//! `main.rs`'s modules, so what both need lives here.

pub mod platform;
