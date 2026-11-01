//! `cheap-note` on macOS.
//!
//! The *platform* half of the application, and nothing else. Everything a note-taking program does —
//! the bar, the home screen, the sheet, the gestures, the page cache, the ink model, the frame loop —
//! is `crates/note-ui`, written once and portable. What is here is the three things that crate cannot
//! write for another machine:
//!
//! | Module | What it lends the view |
//! |---|---|
//! | [`platform::pen`] | `pen-macos`: the digitizer's position, pressure and tilt |
//! | [`platform::pointer`] | `NSCursor`, so the system pointer can be put away while a ghost is up |
//! | [`platform::canvas`] | the desk, the paper and the ink: Core Graphics into a bitmap, shown as a layer |
//!
//! Plus `main`, which is the two lines that assemble them.

mod platform;

fn main() {
    note_ui::run(platform::INSTALLER);
}
