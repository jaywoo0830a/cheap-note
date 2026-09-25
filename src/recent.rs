//! What the app remembers opening: the list the home screen offers.
//!
//! ## Why this is a file, and where it lives
//!
//! A note is a folder, and a folder cannot say *how recently* it was written in without being
//! opened — and opening forty of them at launch is forty databases to read before the first frame.
//! So the app keeps one small index of what it has opened, beside the notes themselves:
//! `%LOCALAPPDATA%\cheap-note\recent.json`, a sibling of the `notes` folder.
//!
//! Not in `cheap-note.settings.json`, which is *relative to the working directory* (see
//! [`crate::settings::SETTINGS_FILE`]): a list of what you last wrote in must not depend on where
//! the program happened to be started from.
//!
//! ## This list is a cache, and it is treated like one
//!
//! Losing it loses nothing: every note it names is still a folder on disk, and the next launch
//! *adopts* the folders it finds and never mentioned (a note carried in from another machine, or a
//! folder whose entry was lost). Losing a *note* is the thing to avoid, and nothing here can touch
//! one: the only write this module makes is to its own file.
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
use crate::store::Summary;

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
}

/// What the app remembers opening, newest first.
#[derive(Debug)]
pub struct Recents {
    /// Where the index is written.
    path: PathBuf,
    /// The entries, newest first.
    entries: Vec<Recent>,
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
        let entries = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Index>(&text).ok())
            .map(|index| index.entries)
            .unwrap_or_default();

        let mut recents = Recents {
            path: path.to_path_buf(),
            entries,
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
    pub fn forget(&mut self, folder: &Path) {
        self.entries.retain(|entry| entry.folder != folder);
    }

    /// Records what a scan saw of one note: its counts, and the database they came from.
    pub fn learn(&mut self, folder: &Path, stamp: Stamp, summary: Summary) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.folder == folder) {
            entry.stamp = Some(stamp);
            entry.summary = summary;
        }
    }

    /// Marks what is no longer there, and adopts what the notes folder has and the list does not.
    ///
    /// `found` is what a scan of the notes folder saw. Whether an entry is *still* there is answered
    /// by the filesystem rather than by that list: a folder opened where it stands lives outside the
    /// notes folder, and reading its absence from the list would call every such note gone.
    pub fn reconcile(&mut self, found: &[PathBuf], now_ms: u64) {
        for entry in &mut self.entries {
            entry.gone = !entry.folder.is_dir();
        }

        for folder in found {
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
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let index = Index {
            entries: self.entries.clone(),
        };
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
mod tests {
    use super::*;

    /// A scratch index path, removed first so a test always starts from nothing.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("cheap-note-recent-{name}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A note folder that exists, so that `gone` can be asked about it.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cheap-note-recent-{name}"));
        std::fs::create_dir_all(&dir).expect("a note folder");
        dir
    }

    /// The entry for a folder, or a panic: a test that asks about an entry has made one.
    fn found<'a>(recents: &'a Recents, folder: &Path) -> &'a Recent {
        recents
            .entries()
            .iter()
            .find(|entry| entry.folder == folder)
            .expect("the entry was recorded or adopted")
    }

    /// A missing index is an empty list, not an error.
    #[test]
    fn a_missing_index_is_an_empty_list() {
        let recents = Recents::load_from(&scratch("missing"));

        assert!(recents.entries().is_empty());
        assert_eq!(recents.entries(), &[]);
    }

    /// What is recorded comes back, newest first, through the file.
    #[test]
    fn what_is_recorded_comes_back_newest_first() {
        let path = scratch("round-trip");
        let mut recents = Recents::load_from(&path);

        recents.record(Recent::note(
            folder("first"),
            Some(PathBuf::from("C:/docs/first.pdf")),
            String::from("first"),
            Some(String::from("first.pdf")),
            1_000,
        ));
        recents.record(Recent::note(
            folder("second"),
            None,
            String::from("Blank sheet"),
            None,
            2_000,
        ));
        recents.save().expect("the index is written");

        let loaded = Recents::load_from(&path);
        let titles: Vec<&str> = loaded
            .entries()
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Blank sheet", "first"]);
        assert_eq!(
            loaded.entries()[1].document.as_deref(),
            Some("first.pdf"),
            "the document's name is remembered"
        );
    }

    /// Opening the same note twice is one entry, at the top, not two.
    #[test]
    fn opening_the_same_note_twice_keeps_one_entry() {
        let path = scratch("twice");
        let mut recents = Recents::load_from(&path);
        let dir = folder("twice-note");

        recents.record(Recent::note(
            dir.clone(),
            None,
            String::from("chapter-3"),
            None,
            1_000,
        ));
        recents.record(Recent::note(
            folder("another"),
            None,
            String::from("another"),
            None,
            2_000,
        ));
        recents.record(Recent::note(
            dir.clone(),
            None,
            String::from("chapter-3"),
            None,
            3_000,
        ));

        assert_eq!(recents.entries().len(), 2, "one entry per folder");
        assert_eq!(recents.entries()[0].folder, dir, "and it is the newest one");
        assert_eq!(recents.entries()[0].opened_at, 3_000);
    }

    /// The list is capped: a note that falls off the end is still a folder on disk.
    #[test]
    fn the_list_is_capped() {
        let path = scratch("capped");
        let mut recents = Recents::load_from(&path);

        for index in 0..RECENT_MAX + 5 {
            recents.record(Recent::note(
                PathBuf::from(format!("note-{index}")),
                None,
                format!("note-{index}"),
                None,
                index as u64,
            ));
        }

        assert_eq!(recents.entries().len(), RECENT_MAX);
        assert_eq!(
            recents.entries()[0].title,
            format!("note-{}", RECENT_MAX + 4),
            "the newest is kept and the oldest is what goes"
        );
    }

    /// A folder that is gone is marked, and one nobody told us about is adopted.
    #[test]
    fn the_list_heals_itself_around_the_notes_folder() {
        let path = scratch("heal");
        let mut recents = Recents::load_from(&path);
        let here = folder("heal-here");
        let missing = std::env::temp_dir().join("cheap-note-recent-heal-gone");
        let _ = std::fs::remove_dir_all(&missing);

        recents.record(Recent::note(
            here.clone(),
            None,
            String::from("here"),
            None,
            10,
        ));
        recents.record(Recent::note(
            missing.clone(),
            None,
            String::from("gone"),
            None,
            20,
        ));

        let carried_in = folder("heal-carried-in");
        recents.reconcile(&[here.clone(), carried_in.clone()], 5_000);

        assert_eq!(recents.entries().len(), 3, "the folder found is adopted");
        let adopted = found(&recents, &carried_in);
        assert_eq!(
            adopted.title,
            Recent::title_for(&carried_in),
            "and is called what its folder says"
        );
        assert!(!adopted.counted(), "with nothing counted yet");
        assert!(found(&recents, &missing).gone);
        assert!(!found(&recents, &here).gone);
    }

    /// A folder that lives outside the notes folder is not called gone for being outside it.
    #[test]
    fn a_note_opened_where_it_stands_is_not_gone() {
        let path = scratch("in-place");
        let mut recents = Recents::load_from(&path);
        let elsewhere = folder("in-place-elsewhere");

        recents.record(Recent::note(
            elsewhere.clone(),
            None,
            String::from("in place"),
            None,
            10,
        ));
        recents.reconcile(&[], 20);

        assert!(
            !found(&recents, &elsewhere).gone,
            "being outside the notes folder is not being missing"
        );
    }

    /// A broken index is an empty list rather than a launch that cannot happen.
    #[test]
    fn a_broken_index_is_an_empty_list() {
        let path = scratch("broken");
        std::fs::write(&path, "{ not json at all").expect("a broken index");

        assert!(Recents::load_from(&path).entries().is_empty());
    }

    /// Ages are words, and a clock that moved backwards is not a negative age.
    #[test]
    fn ages_read_as_words() {
        let minute = 60_000;
        let hour = 60 * minute;
        let day = 24 * hour;
        let entry = Recent::note(PathBuf::new(), None, String::new(), None, 10 * day);
        let now = 10 * day;

        assert_eq!(entry.age(now), "just now");
        assert_eq!(entry.age(now + 3 * minute), "3 minutes ago");
        assert_eq!(entry.age(now + 2 * hour), "2 hours ago");
        assert_eq!(entry.age(now + day), "yesterday");
        assert_eq!(entry.age(now + 3 * day), "3 days ago");
        assert_eq!(entry.age(now + 45 * day), "a month ago");
        assert_eq!(entry.age(now + 100 * day), "3 months ago");
        assert_eq!(entry.age(now + 700 * day), "a year ago");
        assert_eq!(
            entry.age(now - 5 * minute),
            "just now",
            "a clock that went backwards reports the smallest age there is"
        );
    }

    /// A folder's name is turned back into words: the digest goes, the stamp becomes a phrase.
    #[test]
    fn a_folder_name_is_turned_back_into_words() {
        assert_eq!(
            Recent::title_for(Path::new("C:/notes/chapter-3-9f2a1c44")),
            "chapter-3"
        );
        assert_eq!(
            Recent::title_for(Path::new("C:/notes/blank-1712345678901")),
            "Blank sheet"
        );
        assert_eq!(
            Recent::title_for(Path::new("C:/notes/not-a-digest-here")),
            "not-a-digest-here",
            "only a digest is stripped"
        );
    }

    /// A file that was opened is remembered as the note it made, with the file it came from.
    #[test]
    fn an_entry_says_where_the_note_came_from() {
        let placed = Recent::note(
            folder("origin-note"),
            Some(PathBuf::from("C:/docs/chapter-3.pdf")),
            String::from("chapter-3"),
            Some(String::from("chapter-3.pdf")),
            10,
        );
        let blank = Recent::note(folder("origin-blank"), None, String::from("Blank sheet"), None, 10);

        assert!(
            placed.origin().ends_with("chapter-3.pdf"),
            "the file a note was made from is what the line under it says: {}",
            placed.origin()
        );
        assert_eq!(
            blank.origin(),
            "written in cheap-note",
            "a note made on a blank sheet came from nowhere, and says so"
        );
    }

    /// Counts read as a sentence, including the singular.
    #[test]
    fn counts_read_as_a_sentence() {
        let mut entry = Recent::note(PathBuf::new(), None, String::new(), None, 0);

        assert_eq!(entry.counts(), "no pages yet");

        entry.summary = Summary {
            pages: 1,
            strokes: 1,
            sheet: None,
        };
        assert_eq!(entry.counts(), "1 page · 1 stroke");

        entry.summary = Summary {
            pages: 12,
            strokes: 340,
            sheet: None,
        };
        assert_eq!(entry.counts(), "12 pages · 340 strokes");
    }
}
