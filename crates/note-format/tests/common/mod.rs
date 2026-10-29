//! What every integration test needs: a scratch directory, a page of ink, and the news a write carries.
//!
//! ## Why these tests are outside the crate
//!
//! The unit tests in `src/` test a module against its own insides — they can reach a private helper,
//! because they are compiled as part of it. These are the other half of the story, and they are a
//! *different* check: they use nothing but `note_format`'s public API, and where they look at a file
//! they open it with SQLite, a zip reader or a JSON parser rather than with this crate.
//!
//! That is the contract a second implementation has to satisfy. A macOS build, or a checker written
//! in another language, cannot see `fn cursor(&self)`; it sees a `note.db` with a `user_version`, a
//! `pages` table and a `chunks` table full of bytes. If a change here passes the unit tests and fails
//! these, it has changed the format rather than the code — which is exactly the thing worth being
//! told about.
//!
//! Each file is named after the module it is about (`store.rs` for `src/store.rs`), so a change to a
//! module has one obvious place to be asked about.

#![allow(dead_code)]

use std::path::PathBuf;

use note_format::history::{Edit, HistoryUpdate};
use note_format::ink::{InkPoint, Stroke};

/// A fresh directory in the temp directory, named after the test that wants it.
///
/// Removed first, so a failed run leaves nothing that makes the next one pass by accident.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheap-note-it-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A stroke of `points` points walking across the page from `from`, at `y`.
///
/// Closed, because a stroke that arrives at a note or a blob is one the pen has lifted from: the
/// outline and the bounds are derived at that moment, and a stroke that was never closed has neither.
pub fn stroke(points: usize, from: f32, y: f32, color: u32) -> Stroke {
    let mut stroke = Stroke::new(InkPoint::new(from, y, 2.0), color);

    for step in 1..points {
        stroke
            .points
            .push(InkPoint::new(from + step as f32 * 2.0, y, 2.0));
    }

    stroke.close();
    stroke
}

/// The edit that says `stroke` was written at `at`.
pub fn written(at: usize, stroke: &Stroke) -> Edit {
    Edit::Written {
        at,
        strokes: vec![std::sync::Arc::new(stroke.clone())],
    }
}

/// What a write tells the note: the tail of edits it has not heard about, the cursor, and the count.
pub fn update(appended: Vec<Edit>, applied: u64, count: usize) -> HistoryUpdate {
    HistoryUpdate {
        appended,
        applied,
        count,
    }
}

/// A PDF that is not one: nothing in this crate parses a document.
pub fn a_pdf() -> Vec<u8> {
    b"%PDF-1.4 not really".to_vec()
}
