//! The library: every note the app holds, as one file on the desktop — and the way back in.
//!
//! ## Why a library rather than a server
//!
//! A note already knows how to leave: *Save* writes one note as one zip ([`crate::note`]). What was
//! missing was the whole of them at once, and a way for a second machine to receive that file without
//! being asked a question. A backend — accounts, a bucket, a sync loop — would have been the answer to
//! a different problem, and the one thing this app promises is that a note is a file on *your* disk.
//!
//! So the library is a file too: a zip of every note, written where a person looks for it, and read
//! back by an app that is simply *given* it.
//!
//! ```text
//! cheap-note-library.zip
//!     library.json                         version, when, which notes
//!     notes/chapter-3-9f2a1c44/note.db     the same three names a note is
//!     notes/chapter-3-9f2a1c44/source.pdf
//! ```
//!
//! The format is the format crate's ([`note_format::note::write_library`] and
//! [`note_format::note::place_library`]); this module is the application's half — *where* the file
//! goes, and what a person is told about it. Both are pure functions of a few paths, so the rules are
//! tested without a window, and the two places that act on them (`app`'s backup button and its restore
//! on start-up) do nothing but call them.
//!
//! ## Why the desktop
//!
//! Because it is the one folder a person already knows how to drag from. The alternative — asking
//! where to put it, every time — is a dialog in the middle of a button that means "put it where I will
//! find it". A desktop that does not report itself falls back to the home folder, then to the notes'
//! own parent, and the status line says which one was used, so nobody has to guess.
//!
//! ## The restore is automatic, and that is why it is guarded
//!
//! An app that restores on its own is an app that can overwrite a note nobody meant to touch. The
//! guard is the *list*: a library is only restored by an app whose list is empty — a second machine
//! that has never seen a note. There the restore is unambiguous (there is nothing to lose), it is what
//! makes the file feel like a backup rather than an import, and a person with notes of their own
//! restores by choosing the file, which is the path that can say "this replaces what is there."

use std::path::{Path, PathBuf};

use crate::note;
use crate::recent::Recent;

/// The name every backup is written under on the desktop.
///
/// One name rather than one per day: the button means "put my whole library where I can find it", and
/// a desktop that fills with `cheap-note-library-2024-11-18.zip` after a week of pressing it is a
/// desktop nobody can use. Yesterday's copy is worth less than today's, and replacing it is what makes
/// the button worth pressing twice.
pub const LIBRARY_FILE: &str = "cheap-note-library.zip";

/// The library on the desktop: the file the home screen's *Back up everything* button writes.
///
/// `desktop` is handed in rather than found here, so that a test can name a folder and so that the
/// rule stays a rule about paths instead of a walk of a platform's home folder.
pub fn backup_target(
    desktop: Option<PathBuf>,
    home: Option<PathBuf>,
    notes_root: &Path,
) -> PathBuf {
    let folder = desktop
        .filter(|path| path.is_dir())
        .or_else(|| home.filter(|path| path.is_dir()))
        // The notes' own parent: `%LOCALAPPDATA%\cheap-note`, which is at least a folder the app
        // already owns and can write to. Wrong is better than nowhere, as long as it is said.
        .unwrap_or_else(|| {
            notes_root
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| notes_root.to_path_buf())
        });

    folder.join(LIBRARY_FILE)
}

/// This machine's desktop folder, as far as it can be told from the environment.
///
/// GPUI — the one thing here that knows about windows — has no question for this, so it is answered
/// from what every platform this app builds for sets, in the order they matter:
///
/// | Where | How |
/// |---|---|
/// | macOS, Linux | `$XDG_DESKTOP_DIR`, then `~/Desktop` |
/// | Windows | `%USERPROFILE%\Desktop`, then `%OneDrive%\Desktop` — an account whose desktop is synced lives in the second |
///
/// Answered rather than trusted: the folder is checked on disk by the caller, because a desktop that
/// is configured but missing is not a place to write ([`backup_target`] skips it).
pub fn desktop_folder() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(desktop) = std::env::var_os("USERPROFILE")
            .map(|home| PathBuf::from(home).join("Desktop"))
            .filter(|path| path.is_dir())
        {
            return Some(desktop);
        }

        std::env::var_os("OneDrive").map(|onedrive| PathBuf::from(onedrive).join("Desktop"))
    }

    #[cfg(not(windows))]
    {
        // The XDG user directory on a Linux desktop is where the desktop may well have been moved to;
        // it is read from variable rather than from `user-dirs.dirs` because parsing a shell script is
        // more than a fallback for one folder is worth.
        if let Some(desktop) = std::env::var_os("XDG_DESKTOP_DIR").map(PathBuf::from) {
            return Some(desktop);
        }

        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Desktop"))
    }
}

/// The user's own folder, from the variable every platform this app builds for sets.
///
/// The second fallback of a backup with no desktop (see [`backup_target`]): `HOME` on macOS and Linux,
/// `USERPROFILE` on Windows.
pub fn home_folder() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Whether the app was started with nothing of its own: no note in the list, and no folder in the
/// notes folder to adopt.
///
/// This is the guard on the automatic restore (see the module docs). Both halves are asked, because
/// either one means the person has notes: a list with a row is a machine that has written something,
/// and a notes folder with a note is a machine whose list was merely lost — and adopting is the fix for
/// that, not replacing everything with somebody's backup. A notes folder that cannot be read is
/// treated as one with nothing in it, which is the same answer the home screen's scan gives.
pub fn is_empty(entries: &[Recent], notes_root: &Path) -> bool {
    entries.iter().all(|entry| entry.gone)
        && note::notes_in(notes_root)
            .map(|found| found.is_empty())
            .unwrap_or(true)
}

/// What a library says, in the words a status line shows.
///
/// The count and the age, in that order: the count is what the restore will do, and the age is how much
/// of a person's writing it is about to replace. An age of zero — a clock that has moved, or a library
/// whose manifest says nothing sensible — is left out rather than printed as "in the future".
pub fn describe(manifest: &note::LibraryManifest, now_ms: u64) -> String {
    let count = manifest.notes.len();
    let notes = if count == 1 {
        String::from("1 note")
    } else {
        format!("{count} notes")
    };

    if manifest.created_at_ms == 0 {
        return format!("{notes}, from a library with no date on it");
    }

    format!(
        "{notes}, with a library made {}",
        crate::recent::age_label(now_ms.saturating_sub(manifest.created_at_ms))
    )
}

#[cfg(test)]
mod tests;
