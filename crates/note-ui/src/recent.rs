//! What the app remembers opening: the list the home screen offers.
//!
//! ## Why this is a file, and where it lives
//!
//! A note is a folder, and a folder cannot say *how recently* it was written in without being
//! opened — and opening forty of them at launch is forty databases to read before the first frame.
//! So the app keeps one small index of what it has opened, beside the notes themselves:
//! `%LOCALAPPDATA%\cheap-note\recent.json`, a sibling of the `notes` folder.
//!
//! Not inside the settings at all — there are none any more, because every setting belongs to a note
//! (see [`crate::settings`]). The index is about *every* note rather than any one of them, and it
//! belongs beside them: a list of what you last wrote in must not depend on which note is open, nor on
//! where the program happened to be started from.
//!
//! ## This list is a cache, and it is treated like one
//!
//! Losing it costs nothing that is *written*: every note it names is still a folder on disk, and the
//! next launch *adopts* the folders it finds and never mentioned (a note carried in from another
//! machine, or a folder whose entry was lost). Losing a *note* is the thing to avoid, and nothing
//! here can touch one: the only write this module makes is to its own file.
//!
//! ## The one thing the file has to remember
//!
//! What a person took out of the list. Adoption is why: a forgotten note is a folder the notes folder
//! still has, so a list that forgot only the *entry* would adopt it straight back on the next scan —
//! the row would come back on its own, and the menu item that took it away would be a lie. So the
//! folder is remembered as *taken out*, and adoption skips it (see [`Recents::reconcile`]).
//!
//! Opening the note again is the way back, and it is the only one: that is a person asking for it,
//! and no scan can ask on their behalf. Which also means this list is no longer purely a cache —
//! losing the file loses one small decision with it, and the module is honest about that here rather
//! than in a comment buried in [`Recents::forget`].
//!
//! ## What an entry is
//!
//! Three facts and a summary:
//!
//! * `folder` — where the note lives. This is the key, and the thing that is opened. A note carries
//!   its own document (`source.pdf`), so the folder is enough to open it even if the file it was
//!   made from has moved or gone.
//! * `source` — the file it was made from. A convenience: it decides the folder's name, and it is
//!   what "open the original again" means. Nothing about the ink depends on it.
//! * `stamp` — what `note.db` looked like when `summary` was read, so that a launch normally costs a
//!   `stat` per note rather than a database read. Only a note whose database has changed since is
//!   read again.
//!
//! ## Why an entry shows an age rather than a date
//!
//! A calendar date is a timezone question, and this app has no date library — answering it locally
//! would take a platform call it does not otherwise make. An age is a duration, which needs
//! nothing: "2 days ago" is true in every timezone. Ages are composed at the moment they are drawn,
//! from [`Recent::opened_at`] and a `now` the caller has.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::note;
use crate::store::{Facts, Summary};

/// The index of what the app has opened, beside the `notes` folder.
pub const RECENT_FILE: &str = "recent.json";

/// How many entries the list keeps.
///
/// A list is a way back to the thing you were writing, not an archive: forty is more than anyone
/// scrolls through, and a note that falls off the end is still a folder under `notes\` — the next
/// scan adopts it again if nothing else is there.
pub const RECENT_MAX: usize = 40;

/// What `note.db` looked like when a summary was taken.
///
/// Two numbers off the same `stat` the listing already needs, which is what makes a launch cheap:
/// an unchanged note is not opened at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// When the note's database was last written, in milliseconds since the epoch.
    pub mtime_ms: u64,
    /// How large it was, in bytes.
    pub len: u64,
}

impl Stamp {
    /// The stamp of a file, or `None` when it is not there.
    pub fn of(path: &Path) -> Option<Stamp> {
        let metadata = std::fs::metadata(path).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);

        Some(Stamp {
            mtime_ms: modified,
            len: metadata.len(),
        })
    }
}

/// Now, in milliseconds since the epoch: the clock every timestamp here is written with.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// How long ago something happened, in words.
///
/// Deliberately coarse: a list of notes is not a log, and "2 days ago" is what a person is actually
/// asking. Anything under a minute is "just now", which is also what a clock that moved backwards
/// reports rather than a negative age.
pub fn age_label(age_ms: u64) -> String {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if age_ms < MINUTE {
        return String::from("just now");
    }
    if age_ms < 2 * MINUTE {
        return String::from("a minute ago");
    }
    if age_ms < HOUR {
        return format!("{} minutes ago", age_ms / MINUTE);
    }
    if age_ms < 2 * HOUR {
        return String::from("an hour ago");
    }
    if age_ms < DAY {
        return format!("{} hours ago", age_ms / HOUR);
    }
    if age_ms < 2 * DAY {
        return String::from("yesterday");
    }
    if age_ms < 30 * DAY {
        return format!("{} days ago", age_ms / DAY);
    }
    if age_ms < 60 * DAY {
        return String::from("a month ago");
    }
    if age_ms < 365 * DAY {
        return format!("{} months ago", age_ms / (30 * DAY));
    }

    String::from("a year ago")
}

/// One thing the app remembers opening.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recent {
    /// Where the note lives. The key: this is what an entry opens.
    pub folder: PathBuf,
    /// The file it was made from, when there was one.
    pub source: Option<PathBuf>,
    /// What the list calls it.
    pub title: String,
    /// The name the document had, when the note remembers one.
    pub document: Option<String>,
    /// When it was last opened, in milliseconds since the epoch. The sort key.
    pub opened_at: u64,
    /// The note's counts, as of `stamp`.
    pub summary: Summary,
    /// What `note.db` looked like when `summary` was taken; `None` until the first scan.
    pub stamp: Option<Stamp>,
    /// Whether the folder is missing. Recomputed on every scan, so it is not written down.
    #[serde(skip)]
    pub gone: bool,
}

impl Default for Recent {
    fn default() -> Self {
        Recent {
            folder: PathBuf::new(),
            source: None,
            title: String::new(),
            document: None,
            opened_at: 0,
            summary: Summary::default(),
            stamp: None,
            gone: false,
        }
    }
}

impl Recent {
    /// A note that was opened.
    pub fn note(
        folder: PathBuf,
        source: Option<PathBuf>,
        title: String,
        document: Option<String>,
        opened_at: u64,
    ) -> Self {
        Recent {
            folder,
            source,
            title,
            document,
            opened_at,
            ..Recent::default()
        }
    }

    /// Whether the counts are known yet. The list shows a placeholder until they are.
    pub fn counted(&self) -> bool {
        self.stamp.is_some()
    }

    /// What to call a note whose entry does not know a name.
    ///
    /// A note made on a blank sheet is named after the moment it was made (`blank-1712345678901`),
    /// and one placed from a file is named `<stem>-<digest>` — so this is where the digest and the
    /// stamp are turned back into words.
    pub fn title_for(folder: &Path) -> String {
        let name = folder
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();

        if name.starts_with("blank-") {
            return String::from("Blank sheet");
        }

        // `chapter-3-9f2a1c44` is `chapter-3` with the path's digest on the end; the digest is
        // always eight hex digits, and a name that ends in anything else is left alone.
        match name.rsplit_once('-') {
            Some((stem, digest))
                if !stem.is_empty()
                    && digest.len() == 8
                    && digest.chars().all(|digit| digit.is_ascii_hexdigit()) =>
            {
                stem.to_string()
            }
            _ => name,
        }
    }

    /// What the list calls this note: the name a person gave it, or the words derived for it.
    ///
    /// One rule, in one place, because three things ask it — the row that draws it, the search that
    /// looks for it, and the rename that starts from it — and a note whose name is shown one way and
    /// searched another is a note that cannot be found by looking at it.
    pub fn shown_title(&self) -> String {
        if self.title.is_empty() {
            Recent::title_for(&self.folder)
        } else {
            self.title.clone()
        }
    }

    /// How long ago this was opened, in words.
    pub fn age(&self, now_ms: u64) -> String {
        age_label(now_ms.saturating_sub(self.opened_at))
    }

    /// The second line of an entry: where the note came from.
    pub fn origin(&self) -> String {
        match &self.source {
            Some(source) => source.to_string_lossy().to_string(),
            None => String::from("written in cheap-note"),
        }
    }

    /// What the entry's counts read as, for a row that has them.
    pub fn counts(&self) -> String {
        let pages = self.summary.pages;
        let strokes = self.summary.strokes;

        match (pages, strokes) {
            (0, _) => String::from("no pages yet"),
            (1, 1) => String::from("1 page · 1 stroke"),
            (1, _) => format!("1 page · {strokes} strokes"),
            (_, 1) => format!("{pages} pages · 1 stroke"),
            (_, _) => format!("{pages} pages · {strokes} strokes"),
        }
    }
}

/// Where the index is written: beside the notes folder, so it does not depend on the working
/// directory.
pub fn path() -> PathBuf {
    let notes = note::root();
    match notes.parent() {
        Some(home) => home.join(RECENT_FILE),
        None => notes.join(RECENT_FILE),
    }
}

/// What the index file holds. A named record rather than a bare array, so that a field can be added
/// later without the file becoming unreadable.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Index {
    entries: Vec<Recent>,
    /// The folders taken out of the list, which a scan must not adopt back. See [`Recents::forgotten`].
    forgotten: Vec<PathBuf>,
}

impl Index {
    /// What the file holds right now, or nothing when it is missing or unreadable.
    ///
    /// Nothing rather than an error, for the reason [`Recents::load_from`] gives: this file is a cache, and a window
    /// that could not read it has nothing to merge with — which is a smaller problem than refusing to write at all.
    fn read(path: &Path) -> Index {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Index>(&text).ok())
            .unwrap_or_default()
    }
}

/// Two windows' worth of index, as one.
///
/// The rule is: **what this window has decided stands, and what it says nothing about is taken from the other's
/// file.** A folder this window has an opinion about — kept in its list, or taken out of it — is not touched by the
/// other window's copy, because that copy was read before this window opened the note it is about to write. A folder
/// neither of them has ever mentioned here is added, and a folder the other window has forgotten stays forgotten.
///
/// The one number that is *merged* rather than decided is `opened_at`: two windows both knowing a note, the later visit
/// wins, because that is the sort key and the only thing about an entry a reader would notice being stale.
fn merge(mut ours: Index, theirs: Index) -> Index {
    for entry in theirs.entries {
        // Asked before the list is borrowed mutably, and it is the *folder* that is asked about, so a push for
        // another note in an earlier turn of this loop cannot change the answer.
        let known = mentions(&ours, &entry.folder);

        match ours
            .entries
            .iter_mut()
            .find(|mine| mine.folder == entry.folder)
        {
            Some(mine) if mine.opened_at < entry.opened_at => *mine = entry,
            Some(_) => {}
            None if known => {}
            None => ours.entries.push(entry),
        }
    }

    for folder in theirs.forgotten {
        if !mentions(&ours, &folder) && !ours.forgotten.contains(&folder) {
            ours.forgotten.push(folder);
        }
    }

    ours
}

/// Whether an index has an opinion about a folder: it is in the list, or it has been taken out of it.
///
/// A free function rather than a closure over the index being written, because the loop that asks is also the loop that
/// grows it: the answer is needed *before* the mutable borrow, and a closure would hold one from the first question to
/// the last.
fn mentions(index: &Index, folder: &PathBuf) -> bool {
    index.entries.iter().any(|entry| &entry.folder == folder) || index.forgotten.contains(folder)
}

/// What the app remembers opening, newest first.
#[derive(Debug)]
pub struct Recents {
    /// Where the index is written.
    path: PathBuf,
    /// The entries, newest first.
    entries: Vec<Recent>,
    /// Folders a person took out of the list, which adoption therefore skips.
    ///
    /// The one thing here that is not a cache of something the disk already says: it is a decision,
    /// and the file is the only place it can live (the note must not be modified to leave a list —
    /// see [`Self::forget`]). It stays small because a folder that is not on disk is dropped from it:
    /// only a folder that is *there* can be adopted, so only one that is there needs a reminder.
    forgotten: Vec<PathBuf>,
}

impl Recents {
    /// Loads the index. A missing file is the first run; a broken one is an empty list.
    ///
    /// A broken index is *not* an error the user is shown: this file is a cache of what the notes
    /// folder already knows, and interrupting a launch to complain about it would be worse than
    /// rebuilding it — which the next scan does.
    pub fn load() -> Self {
        Recents::load_from(&path())
    }

    /// Loads the index from a given path: the same thing, for a test or another location.
    pub fn load_from(path: &Path) -> Self {
        let index = Index::read(path);

        let mut recents = Recents {
            path: path.to_path_buf(),
            entries: index.entries,
            forgotten: index.forgotten,
        };

        recents.sort();
        recents
    }

    /// Every entry, newest first.
    pub fn entries(&self) -> &[Recent] {
        &self.entries
    }

    /// Records that something was opened: the most recent thing the list has seen.
    ///
    /// The counts and the stamp of an entry that is already here are kept — they describe the note,
    /// not the moment it was opened — and the next scan corrects them if the note has changed. An
    /// entry that is *new* has no stamp, so its counts are read on that scan.
    pub fn record(&mut self, fresh: Recent) {
        // Opening a note is a person asking for it, and that is the one thing that takes it back off
        // the list of folders they took out of the list.
        self.forgotten.retain(|folder| folder != &fresh.folder);

        match self
            .entries
            .iter_mut()
            .find(|entry| entry.folder == fresh.folder)
        {
            Some(existing) => {
                let (summary, stamp) = (existing.summary, existing.stamp);
                *existing = Recent {
                    summary,
                    stamp,
                    ..fresh
                };
            }
            None => self.entries.push(fresh),
        }

        self.sort();
    }

    /// Takes a folder out of the list. The note itself is not touched.
    ///
    /// And it *stays* out. That is the whole of this method's care: the note is still a folder under
    /// `notes\`, so a scan would adopt it right back if the only thing taken out were the entry — the
    /// row would return on its own, which makes the menu item that removed it meaningless. The folder
    /// is therefore remembered as one a person took out, and [`Self::reconcile`] skips it.
    ///
    /// The note is not marked: a list a person keeps on *this* machine is not something to write into
    /// a note, which may be opened on another one. The decision lives in the index, beside the list it
    /// is about, and [`Self::record`] is what reverses it.
    pub fn forget(&mut self, folder: &Path) {
        self.entries.retain(|entry| entry.folder != folder);

        if !self.forgotten.iter().any(|path| path == folder) {
            self.forgotten.push(folder.to_path_buf());
        }
    }

    /// Records what a scan saw of one note: everything the note says about itself, and the database
    /// that answer came from.
    ///
    /// The name is included because it *can* change — a person renames a note, and a note carried in
    /// from another machine arrives with a name already given to it — and this is the only place that
    /// hears about it without the note being open.
    pub fn learn(&mut self, folder: &Path, stamp: Stamp, facts: &Facts) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.folder == folder) {
            entry.stamp = Some(stamp);
            entry.summary = facts.summary;
            entry.document = facts.document.clone();
            entry.title = facts.title.clone().unwrap_or_default();
        }
    }

    /// Sets the name a person gave the note.
    ///
    /// The stamp is deliberately left alone: it is a description of the database as it was *read*, and
    /// writing a name to the database changes it. Leaving the old stamp means the next scan notices
    /// the difference and re-reads the note — which is how the list confirms the new name instead of
    /// trusting this writer.
    pub fn rename(&mut self, folder: &Path, title: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.folder == folder) {
            entry.title = title.to_string();
        }
    }

    /// Marks what is no longer there, and adopts what the notes folder has and the list does not.
    ///
    /// `found` is what a scan of the notes folder saw. Whether an entry is *still* there is answered
    /// by the filesystem rather than by that list: a folder opened where it stands lives outside the
    /// notes folder, and reading its absence from the list would call every such note gone.
    ///
    /// A folder a person took out of the list is not adopted, though it is a folder the notes folder
    /// knows: being told about it is exactly what taking it out meant, and a scan that adopted it back
    /// would undo the one thing this list is asked to remember. See [`Self::forget`].
    pub fn reconcile(&mut self, found: &[PathBuf], now_ms: u64) {
        for entry in &mut self.entries {
            entry.gone = !entry.folder.is_dir();
        }

        // A folder that is not there cannot be adopted, so it needs no reminder — and this is what
        // keeps the list of them short: a note that is deleted, or a folder a person forgot while it
        // was already gone, is dropped here. Should such a folder turn up again it turns up as a note
        // the notes folder is telling us about, which is the same road a carried-in note takes.
        self.forgotten.retain(|folder| folder.is_dir());

        for folder in found {
            if self.forgotten.iter().any(|path| path == folder) {
                continue;
            }

            if self.entries.iter().any(|entry| entry.folder == *folder) {
                continue;
            }

            // A note nobody told us about: one carried in from another machine, or one whose entry
            // was lost. Its folder name is what it can be called, and its database's timestamp is
            // the closest thing to "when it was last written in".
            let opened_at = Stamp::of(&folder.join(note::NOTE_DB))
                .map(|stamp| stamp.mtime_ms)
                .unwrap_or(now_ms);

            self.entries.push(Recent::note(
                folder.clone(),
                None,
                Recent::title_for(folder),
                None,
                opened_at,
            ));
        }

        self.sort();
    }

    /// Writes the index out: a temporary file and a rename, so a crash mid-write cannot leave half
    /// a list behind.
    ///
    /// **The file is merged with what is on disk before it is written**, because two windows of the app share it: each
    /// holds the list it read when it started, and writing that straight back would drop everything the other window
    /// has done since — a note opened there would fall out of the list, and a note forgotten there would come back.
    /// Merging is safe *because* this file is a cache: the worst a merge can do is keep an entry the reader no longer
    /// wants, and the next scan settles what is actually on disk (see the module docs) — and what a merge keeps and
    /// what a merge lets go is [`merge`]'s rule, per folder.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let ours = Index {
            entries: self.entries.clone(),
            forgotten: self.forgotten.clone(),
        };
        let index = merge(ours, Index::read(&self.path));
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_string_pretty(&index)?)?;
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    /// Newest first, and never longer than [`RECENT_MAX`].
    fn sort(&mut self) {
        self.entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.opened_at));
        self.entries.truncate(RECENT_MAX);
    }
}

#[cfg(test)]
mod tests;
