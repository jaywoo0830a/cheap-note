//! The one preference that belongs to the machine rather than to a note: the interface scale.
//!
//! ## Why this file exists
//!
//! Every setting a *note* has travels with the note (see [`crate::settings`]). The size of the
//! interface is not about a note: it is about the person reading on this machine, and it has to be
//! the same before a note is open. The app therefore keeps one small file beside the notes —
//! `%LOCALAPPDATA%\cheap-note\prefs.json` on Windows,
//! `~/Library/Application Support/cheap-note/prefs.json` on macOS — the same sibling of the `notes`
//! folder as `recent.json` and `status.log`.
//!
//! ## What it holds
//!
//! `interface_scale` is [`ScaleStep::index()`], an integer from 0 to 6. It is deliberately not a
//! percentage, a floating-point factor, or a platform's DPI: the index is stable, small, and
//! cannot name a scale the interface does not have (`doc/DESIGN.md` §7.2). The percentage is only
//! what the chooser shows a person.
//!
//! ## A file that cannot be read is not an error
//!
//! A missing, unreadable, or unknown value falls back to the shipped step. Opening an interface at
//! the size its design was drawn at is always better than refusing to open it because a preference
//! is old or somebody edited the file by hand. The next change writes the shipped step back.
//!
//! The reverse is not true: an unknown *field* is kept. A build that later adds a machine
//! preference should find its value still here after an older build has saved the scale.
//!
//! ## Two windows
//!
//! Each window reads the file when it starts and writes it when its own chooser changes. The last
//! window to change the scale wins: this is a preference, not the recent list, and two windows do
//! not need it to converge while both are open.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::design::ScaleStep;
use crate::error::Result;
use crate::note;

/// The file the machine's preferences live in, beside the `notes` folder.
pub const PREFS_FILE: &str = "prefs.json";

/// Where the preferences are written: beside the notes folder, so the path does not depend on the
/// working directory.
pub fn path() -> PathBuf {
    let notes = note::root();
    match notes.parent() {
        Some(home) => home.join(PREFS_FILE),
        None => notes.join(PREFS_FILE),
    }
}

/// The machine preferences the app has read.
#[derive(Debug)]
pub struct Prefs {
    /// Where the file is written.
    path: PathBuf,
    /// The interface scale, already checked against the steps this build has.
    scale: ScaleStep,
    /// Fields this build does not know, kept so a newer build's preference is not lost when this
    /// one saves the scale.
    rest: BTreeMap<String, Value>,
}

impl Prefs {
    /// Loads the preferences. A missing or broken file is the shipped set.
    pub fn load() -> Self {
        Prefs::load_from(&path())
    }

    /// Loads the preferences from a given path: the same thing, for a test or another location.
    pub fn load_from(path: &Path) -> Self {
        let file = File::read(path);
        let scale = ScaleStep::from_index(file.interface_scale).unwrap_or_default();

        Prefs {
            path: path.to_path_buf(),
            scale,
            rest: file.rest,
        }
    }

    /// The interface scale in hand.
    pub fn scale(&self) -> ScaleStep {
        self.scale
    }

    /// Replaces the interface scale, and answers whether it changed.
    pub fn set_scale(&mut self, scale: ScaleStep) -> bool {
        if self.scale == scale {
            return false;
        }

        self.scale = scale;
        true
    }

    /// Writes the file: a temporary file and a rename, so a crash mid-write cannot leave half a
    /// preference behind.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = File {
            interface_scale: self.scale.index(),
            rest: self.rest.clone(),
        };
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_string_pretty(&file)?)?;
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

/// What the file holds. A named record rather than a bare number, so a field can be added later
/// without the file becoming unreadable.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct File {
    /// The interface scale as [`ScaleStep::index()`].
    #[serde(default = "shipped_index")]
    interface_scale: usize,
    /// Anything a build this one does not know wrote.
    #[serde(flatten)]
    rest: BTreeMap<String, Value>,
}

impl Default for File {
    fn default() -> Self {
        File {
            interface_scale: shipped_index(),
            rest: BTreeMap::new(),
        }
    }
}

impl File {
    /// What the file holds right now, or the shipped set when it is missing or unreadable.
    ///
    /// Nothing rather than an error, for the reason the module gives: an interface that cannot
    /// remember a preference should still open.
    fn read(path: &Path) -> File {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<File>(&text).ok())
            .unwrap_or_default()
    }
}

/// The index of the scale the design ships at, which is also the default for every field the file
/// does not say.
fn shipped_index() -> usize {
    ScaleStep::DEFAULT.index()
}

#[cfg(test)]
mod tests;
