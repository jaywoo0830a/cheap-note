//! The note store: one SQLite file per note, holding the ink and what the note is.
//!
//! ## Why a database replaced a zip of JSON
//!
//! The first version wrote a note as a zip with `notes.json` in it: the whole note re-serialised,
//! re-deflated and rewritten on every save. That is fine for a page and hopeless for a notebook —
//! a save is O(everything), a crash in the middle leaves a half-written file, and nothing is
//! incremental. What replaced it: **SQLite as the container**, with the strokes as compressed BLOBs
//! rather than as rows of text.
//!
//! What that buys:
//!
//! * **A save is a batch, not a rewrite.** Ink arrives as `dirty_strokes` rows in one transaction;
//!   the page is compacted into chunks when it is closed. Nothing rewrites what is already stored.
//! * **WAL, so writing does not stop reading.** The pen never waits for a commit, and a reader — the
//!   app loading the page it is about to show — is never blocked by one.
//! * **Partial loading.** Opening a note opens the file and reads *nothing*; a page is read and
//!   decompressed when it is turned to.
//! * **Crash safety.** A half-written note is a note missing one batch of at most half a second of
//!   ink, not a note that will not open, because SQLite commits or does not.
//! * **One file to move.** See [`Self::export`], which is `VACUUM INTO`: a complete, standalone
//!   copy with the WAL folded in, which is what the app packs into a zip.
//!
//! ## The schema
//!
//! Three tables for the ink, and a `meta` table that holds everything else a note knows — one row per
//! fact, in text, and an ordering column on `pages`:
//!
//! ```sql
//! pages(id, ord, created_at, updated_at, bbox_*, stroke_count)
//! chunks(id, page_id, seq, stroke_start, stroke_count, codec, raw_len, data, crc32)
//! dirty_strokes(id, page_id, seq, data, created_at)
//! meta(key, value)
//! bookmarks(ord, created_at)
//! ```
//!
//! **The one table that is not ink is `bookmarks`**: one row per page a person marked, keyed by that
//! page's position in the note's reading order — the same number `pages.ord` is, and the same one the
//! app shows in the page pill. Keying it by position is what makes a bookmark a *page* rather than a
//! moment: inserting a page before a marked one renames the mark along with the page it is on, and
//! deleting a page takes its mark with it (see [`NoteStore::insert_page`] and
//! [`NoteStore::delete_page`], which do both in the same transaction as the shift). Nothing about the
//! ink depends on it: a note whose bookmarks are all removed is a note whose ink is untouched.
//!
//! **Why `pages` has both `id` and `ord`.** `id` is an identity that never changes — it is what
//! `chunks.page_id` points at, so it has to be stable — while `ord` is the page's position in the
//! note's reading order, which *does* change: inserting a page renames every page after it. Keeping
//! the two apart means an insert is two small `UPDATE`s on `pages` rather than a rewrite of every
//! chunk in the note. The shift goes through an offset because `ord` is unique, and a single
//! `ord = ord + 1` would collide with the row it is about to move.
//!
//! **Why the `meta` table is the whole of the rest of the schema.** Everything a note knows about
//! itself — which page was open, what the page list is, the sheet the ink is in, the document it came
//! from, its name, and every setting a person can change — is a row of its own: the *key* is the
//! fact's name and the *value* is that fact, as text. There is no packed blob to decode and no shape
//! for a struct to match, which is what makes a fact something that can be added to a note without
//! anything being migrated: a row nobody knows is simply never read, and never written, so it is
//! still there for the build that knows it. See [`crate::settings`] for the settings half of that
//! table, row by row, and [`NoteStore::read_settings`] for the two lists that marry the rows to the
//! app's own type.
//!
//! ## The write path
//!
//! ```text
//! pen → app batches finished strokes (500 ms or 200 strokes)
//!     → NoteStore::append(page, &strokes)      one transaction, one BLOB per stroke
//!     → NoteStore::compact(page)               when the page closes: dirty → chunks, zstd
//!     → NoteStore::checkpoint()                when idle: fold the WAL back into the file
//! ```
//!
//! ## The read path
//!
//! [`Self::load`] reads the page's chunks and its dirty strokes through the `page_strokes` view and
//! decompresses the chunks **in parallel** — a page is a handful of independent 64 KB blobs, so
//! opening one is a broadcast across the pool rather than a loop. Every blob is checked against the
//! CRC and the stroke count it is filed under before its strokes are used; see [`crate::chunk`].
//!
//! ## What this module does not do
//!
//! It does not know that a note has a PDF, a page list or attachments: those are the note *folder*
//! and the transfer container, which are [`crate::note`]. This is the database, and the only thing
//! it knows about a page is its position.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use crate::canvas::{CanvasSize, CanvasStyle};
use crate::chunk::{self, Codec};
use crate::error::{AppError, Result};
use crate::history::{Edit, History, HistoryUpdate};
use crate::ink::Stroke;
use crate::settings::{PenWeight, Settings};

/// The schema version this build writes, in `PRAGMA user_version`.
///
/// A note is read by the build whose schema wrote it and by no other. A *newer* file is refused
/// because the tables and the `meta` keys it added are ones this build has never heard of, and
/// guessing is how a note gets rewritten without the ink that was in them. An *older* one is refused
/// for the mirror image of that reason: this build would read its pages and write back its own shape,
/// and anything it did not know about would be gone the next time the note was opened.
///
/// There is deliberately no migration. A note this build cannot read is a note it does not touch, and
/// the message says which way round the difference is — a person with an old note is told to open it
/// with the build that wrote it, not handed a note that was quietly rewritten.
///
/// `0` is not an older note: it is a file with no tables in it yet, which is what a note looks like
/// between [`NoteStore::open`] creating the database and [`NoteStore::create_schema`] stamping it.
pub const SCHEMA_VERSION: i32 = 6;

/// How many edits of a page's history the note keeps.
///
/// The app keeps a session's worth in memory ([`crate::history::HISTORY_EDITS`]) and this is the depth that
/// outlives it: a page reopened days later can take back this many edits, and the oldest are dropped by the
/// idle checkpoint rather than by a write, so a note does not grow a history the size of itself. Deep enough
/// to cover a session of writing, bounded so that "how far back can I go" has an answer.
pub const HISTORY_DEPTH: i64 = 4_096;

/// The page the note was last on, as text: a whole number.
pub const META_OPEN_PAGE: &str = "open_page";
/// The sheet the ink was written on, as text: a number of logical pixels.
///
/// A sheet is a width and a height, and it is two rows rather than one — like every other fact here.
/// They are written together, in one transaction, so a note that holds one without the other is a note
/// somebody edited by hand; [`NoteStore::sheet`] answers that with "the note does not say" rather than
/// with half a sheet.
pub const META_SHEET_WIDTH: &str = "sheet_width";
/// The other half of the sheet: see [`META_SHEET_WIDTH`].
pub const META_SHEET_HEIGHT: &str = "sheet_height";
/// What each page shows, in reading order, as JSON: a list of [`crate::pages::PageEntry`]s.
///
/// The one row that is a *list* rather than a value, and it is JSON so that page kinds can be added
/// without either side having to agree on an order for them — see the module docs. A page's rotation is
/// part of the entry, so a note carries the pages the way the reader left them, and a page list written
/// before a page could be turned is read as pages that are the right way up (see
/// [`crate::pages::StoredPage`]).
pub const META_LAYOUT: &str = "layout";
/// The name the document had when the note was made, as UTF-8.
pub const META_DOCUMENT: &str = "document";
/// The file the note was placed from, as it was given, as UTF-8.
///
/// What it is *for*: the home screen's list can say which PDF a note is about, and a note carried to
/// another machine still knows where it came from. Nothing about the ink depends on it — a note
/// carries its own copy of the document — so a note whose original file has gone is still whole.
pub const META_SOURCE: &str = "source";

/// The name a person gave the note, as UTF-8.
///
/// *Not* the folder's name, and never a path: the folder keeps the name it was made with — a digest of
/// the file it came from, or the moment a blank sheet was made — and that name is the note's identity
/// (it is what the index keys on, and what makes importing the same PDF twice find the same note). So
/// a name given here is what the note is *called*, and the folder is what it *is*. An empty value
/// means "no name", which is the normal state: the list derives one from the document, or from the
/// blank sheet it started as.
pub const META_TITLE: &str = "title";
/// How many characters a name may have. Characters, not bytes: a name in Korean is a name.
pub const TITLE_MAX: usize = 64;

/// What a note holds, without reading any of its ink.
///
/// Three numbers the home screen's list needs and can afford: the counts are answered by the
/// database, and the sheet by one small `meta` read. Reading a page of ink to answer "how long is
/// this note" would be the one thing a list of forty notes cannot do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    /// Pages the note's own list holds: what a reader can turn to.
    pub pages: usize,
    /// Strokes in the note, chunks and dirty strokes together.
    pub strokes: usize,
    /// The sheet the ink was written on, when the note remembers one.
    pub sheet: Option<(f32, f32)>,
}

/// What a note says about itself: the name a person gave it, the document it was made from, and how
/// much is in it.
///
/// Gathered in one place and read in one go, because the home screen's list needs all of it to draw
/// one row — a row is a name, where the note came from, and its counts. [`crate::recent`]'s entries
/// cache exactly this, and re-read it only when the database has changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    /// The name a person gave the note, if they gave it one.
    pub title: Option<String>,
    /// The name the document had, if the note remembers one.
    pub document: Option<String>,
    /// The counts, and the sheet.
    pub summary: Summary,
}

/// One note's ink, in one SQLite file.
#[derive(Debug)]
pub struct NoteStore {
    conn: Connection,
}

impl NoteStore {
    /// Opens a note's database, creating it if it is not there yet.
    ///
    /// The PRAGMAs come first, and one of them has to: `page_size` is only read when a database is
    /// *created*, so a store that set it later would be a store with 4 KB pages for as long as it
    /// existed. The rest are the design note's tuning — WAL so a reader never waits for a writer,
    /// `synchronous = NORMAL` because WAL makes that safe, a 64 MB cache and a 256 MB mapping
    /// because ink is read a page at a time and the file is local.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|error| {
            AppError::Note(format!("{} could not be opened: {error}", path.display()))
        })?;

        conn.execute_batch(PRAGMAS).map_err(|error| {
            AppError::Note(format!(
                "{} could not be tuned for writing: {error}",
                path.display()
            ))
        })?;

        let store = NoteStore { conn };
        store.check_version(path)?;
        store.create_schema()?;
        Ok(store)
    }

    /// Refuses a file this build did not write.
    ///
    /// One rule, two directions: a note from a *newer* build holds tables and `meta` this build has
    /// never heard of, and a note from an *older* one is a shape this build would rewrite in its own.
    /// Both are refused, and the message says which way round it is, because "open it with the build
    /// that wrote it" is only advice if the person is told which build that is.
    ///
    /// `0` is neither. It is a file with no tables in it yet — what a note looks like between
    /// [`Self::open`] creating the database and [`Self::create_schema`] stamping it — so it is let
    /// through to become one.
    fn check_version(&self, path: &Path) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| {
                AppError::Note(format!("{} is not a note: {error}", path.display()))
            })?;

        if version == 0 || version == SCHEMA_VERSION {
            return Ok(());
        }

        Err(AppError::Note(format!(
            "{} was written by a {} version of this app (note schema {version}, this build reads {SCHEMA_VERSION})",
            path.display(),
            if version > SCHEMA_VERSION { "newer" } else { "older" },
        )))
    }

    /// Creates the tables and the view, and stamps the version on a file that is new.
    fn create_schema(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA_SQL).map_err(|error| {
            AppError::Note(format!("the note's tables could not be made: {error}"))
        })?;

        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);

        if version < SCHEMA_VERSION {
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
                .map_err(|error| {
                    AppError::Note(format!("the note could not be stamped: {error}"))
                })?;
        }

        Ok(())
    }
}

/// The tuning the design note gives, in the order it has to be applied.
///
/// `page_size` comes **first**, before `journal_mode`: a page size is read only when a database has
/// no pages yet, and switching to WAL *writes* to the file, which fixes the size at SQLite's default
/// of 4 KB for the life of the file. The design asks for 8 KB pages because a page's ink is one
/// BLOB and a bigger page means fewer page boundaries to cross while reading it.
const PRAGMAS: &str = "PRAGMA page_size = 8192;
                          PRAGMA journal_mode = WAL;
                          PRAGMA synchronous = NORMAL;
                          PRAGMA busy_timeout = 5000;
                          PRAGMA cache_size = -64000;
                          PRAGMA temp_store = MEMORY;
                          PRAGMA mmap_size = 268435456;
                          PRAGMA wal_autocheckpoint = 1000;
                          PRAGMA foreign_keys = ON;";

/// The tables, the indexes and the view.
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS pages (
    id            INTEGER PRIMARY KEY,
    ord           INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    bbox_min_x    REAL,
    bbox_min_y    REAL,
    bbox_max_x    REAL,
    bbox_max_y    REAL,
    stroke_count  INTEGER NOT NULL DEFAULT 0,
    history_at    INTEGER NOT NULL DEFAULT 0,
    history_count INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_pages_ord ON pages(ord);

CREATE TABLE IF NOT EXISTS chunks (
    id           INTEGER PRIMARY KEY,
    page_id      INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    stroke_start INTEGER NOT NULL,
    stroke_count INTEGER NOT NULL,
    codec        INTEGER NOT NULL,
    raw_len      INTEGER NOT NULL,
    data         BLOB NOT NULL,
    crc32        INTEGER NOT NULL,
    UNIQUE(page_id, seq)
) STRICT;

CREATE TABLE IF NOT EXISTS dirty_strokes (
    id         INTEGER PRIMARY KEY,
    page_id    INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    data       BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_chunks_page ON chunks(page_id, stroke_start);
CREATE INDEX IF NOT EXISTS idx_dirty_page ON dirty_strokes(page_id, seq);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

-- The pages a person marked, by their position in the note's reading order.
--
-- One row per marked page rather than a list in a `meta` row: a mark is a fact about a page, it is
-- renamed by the same shift `pages.ord` gets, and a table is what lets the database answer which
-- pages are marked, in reading order, without anything decoding a blob. `created_at` is not read by
-- this build — it is there because a row that cannot say when it was written is a row that has to be
-- rewritten to find out, and because a list of marks is the one place a person may later want the
-- order they were made in.
CREATE TABLE IF NOT EXISTS bookmarks (
    ord        INTEGER PRIMARY KEY,
    created_at INTEGER NOT NULL
) STRICT;

-- A page's history: one row per edit its ink has been through, in the order they were made.
--
-- `ord` counts the edits a page has ever had and does not restart: it is the *cursor* the app and the note
-- agree about (see `pages.history_at`), and a row that was deleted because the history is deeper than it
-- is kept for simply leaves a gap at the bottom. `data` is the edit, encoded by `src/history.rs`, and it
-- holds strokes — the same codec a page's ink goes through — so what is written is ink rather than a
-- command whose meaning a later build might have changed.
CREATE TABLE IF NOT EXISTS edits (
    page_id    INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    ord        INTEGER NOT NULL,
    data       BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (page_id, ord)
) STRICT;

CREATE VIEW IF NOT EXISTS page_strokes AS
    SELECT page_id,
           seq,
           stroke_start AS ord,
           stroke_count,
           codec,
           raw_len,
           crc32,
           data,
           0 AS is_dirty
      FROM chunks
    UNION ALL
    SELECT dirty_strokes.page_id,
           dirty_strokes.seq,
           COALESCE((SELECT p.stroke_count FROM pages p WHERE p.id = dirty_strokes.page_id), 0)
               + dirty_strokes.seq AS ord,
           1 AS stroke_count,
           2 AS codec,
           0 AS raw_len,
           0 AS crc32,
           dirty_strokes.data,
           1 AS is_dirty
      FROM dirty_strokes;
";

/// The offset the page shift goes through.
///
/// `ord` is unique, so `UPDATE pages SET ord = ord + 1 WHERE ord >= ?` collides with the row it is
/// moving *to*. Moving the rows aside by a large offset and then back is two statements, needs no
/// ordering, and cannot collide with anything a note will ever hold: a million-page note is not a
/// note.
const SHIFT: i64 = 1_000_000;

/// Now, in milliseconds since the epoch, for the two timestamp columns.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl NoteStore {
    /// Adds finished strokes to a page: one transaction, one BLOB per stroke.
    ///
    /// This is the *only* write the pen's path makes, and it is deliberately the smallest one there
    /// is. A stroke arrives encoded as a one-stroke chunk and lands in `dirty_strokes`, which is the
    /// design note's write-ahead area for ink: rows that are committed, are read back with the
    /// page, and are folded into chunks later (see [`Self::compact`]). Nothing already stored is
    /// rewritten, so the cost of a save is the ink the user just drew and not the ink of the note.
    ///
    /// The strokes are appended in the order they are given and after whatever the page already
    /// holds, which is what makes a page's ink a sequence rather than a set: `stroke_start` and
    /// `seq` are where the order lives.
    pub fn append(&mut self, page: u64, strokes: &[Stroke], history: &HistoryUpdate) -> Result<()> {
        let tx = self.begin()?;
        let id = page_row(&tx, page, true)?.expect("the row was just made");
        let now = now_millis();

        if !strokes.is_empty() {
            let mut seq = next_dirty_seq(&tx, id)?;

            {
                let mut insert = tx
                    .prepare_cached(
                        "INSERT INTO dirty_strokes (page_id, seq, data, created_at) VALUES (?1, ?2, ?3, ?4)",
                    )
                    .map_err(sql)?;

                for stroke in strokes {
                    let blob = sealed_dirty(stroke)?;
                    insert.execute(params![id, seq, blob, now]).map_err(sql)?;
                    seq += 1;
                }
            }

            tx.execute(
                "UPDATE pages SET updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(sql)?;
        }

        // The page's history is written in the same transaction as the ink it describes, and that is the whole
        // reason it is not a job of its own: a crash can leave the note with ink and no edit, or an edit and no
        // ink, but never an edit that describes ink the note does not have.
        write_history(&tx, id, history, now)?;

        tx.commit().map_err(sql)?;
        Ok(())
    }

    /// Folds a page's dirty strokes into chunks: what happens when the page is closed.
    ///
    /// This is where the ink stops being rows and becomes compressed arrays — one chunk per
    /// 64 KB-or-512-strokes of them, in the order they were drawn, continuing the page's
    /// `stroke_start` sequence so the chunks of one page tile its ink exactly. The dirty rows are
    /// deleted in the same transaction, so there is no moment where the page's ink exists twice.
    ///
    /// Returns how many strokes were compacted, which is what the caller needs to know whether the
    /// page was worth rewriting at all.
    pub fn compact(&mut self, page: u64) -> Result<usize> {
        let tx = self.begin()?;
        let Some(id) = page_row(&tx, page, false)? else {
            return Ok(0);
        };

        let dirty = read_dirty(&tx, id)?;
        if dirty.is_empty() {
            return Ok(0);
        }

        let start = strokes_in_chunks(&tx, id)?;
        let seq = next_chunk_seq(&tx, id)?;
        let written = dirty.len();
        write_chunks(&tx, id, seq, start, &dirty)?;

        tx.execute("DELETE FROM dirty_strokes WHERE page_id = ?1", params![id])
            .map_err(sql)?;
        tx.execute(
            "UPDATE pages SET stroke_count = stroke_count + ?1 WHERE id = ?2",
            params![written as i64, id],
        )
        .map_err(sql)?;
        touch_bounds(&tx, id, &dirty)?;
        tx.commit().map_err(sql)?;

        Ok(written)
    }

    /// Replaces a page's ink: what a page that was undone, erased or cleared needs.
    ///
    /// Appending only describes ink being *added*. Undo takes a stroke back, the eraser removes the
    /// ones it passed over, and `clear` removes all of them — none of which is an append, and all of
    /// which the app marks as a rewrite rather than as a batch. The page's chunks and dirty rows are
    /// dropped and its ink is written again from the strokes it now has, in one transaction, so a
    /// crash leaves either the old page or the new one.
    pub fn rewrite(
        &mut self,
        page: u64,
        strokes: &[Stroke],
        history: &HistoryUpdate,
    ) -> Result<()> {
        let tx = self.begin()?;

        let id = match page_row(&tx, page, !strokes.is_empty())? {
            Some(id) => id,
            // An empty page with no row is a page with nothing on it, which is what it was asked to
            // become.
            None => return Ok(()),
        };

        tx.execute("DELETE FROM chunks WHERE page_id = ?1", params![id])
            .map_err(sql)?;
        tx.execute("DELETE FROM dirty_strokes WHERE page_id = ?1", params![id])
            .map_err(sql)?;

        if strokes.is_empty() {
            tx.execute(
                "UPDATE pages
                    SET stroke_count = 0,
                        bbox_min_x = NULL, bbox_min_y = NULL,
                        bbox_max_x = NULL, bbox_max_y = NULL,
                        updated_at = ?1
                  WHERE id = ?2",
                params![now_millis(), id],
            )
            .map_err(sql)?;
        } else {
            write_chunks(&tx, id, 0, 0, strokes)?;
            tx.execute(
                "UPDATE pages
                    SET stroke_count = ?1,
                        bbox_min_x = NULL, bbox_min_y = NULL,
                        bbox_max_x = NULL, bbox_max_y = NULL
                  WHERE id = ?2",
                params![strokes.len() as i64, id],
            )
            .map_err(sql)?;
            touch_bounds(&tx, id, strokes)?;
        }

        // The history, in the same transaction as the ink (see [`NoteStore::append`]).
        write_history(&tx, id, history, now_millis())?;

        tx.commit().map_err(sql)?;
        Ok(())
    }
}

impl NoteStore {
    /// A page as the reader gets it: its ink, and the history its ink has been through.
    ///
    /// The two are read together because they are one answer — "what is on this page, and what has been done to
    /// it" — and because the second is only meaningful while it agrees with the first: `pages.history_count` is
    /// the stroke count the cursor stands for, and a log whose count is not the page's is one this build cannot
    /// trust (a page written by a build that kept no history, or by something else entirely). Such a log is
    /// **ignored** rather than repaired — the next write of that page replaces it from its own cursor onwards —
    /// so opening a note never writes to it.
    pub fn load_page(&self, page: u64, depth: usize) -> Result<LoadedPage> {
        let strokes = self.load(page)?;

        let Some((id, at)) = self.cursor(page)? else {
            return Ok(LoadedPage {
                strokes,
                history: History::new(),
            });
        };

        let depth = depth as i64;
        Ok(LoadedPage {
            strokes,
            history: History::loaded(
                edits_applied(&self.conn, id, at, depth)?,
                edits_undone(&self.conn, id, at, depth)?,
                at as u64,
            ),
        })
    }

    /// One more applied edit of a page, older than the `skip` the caller already holds.
    ///
    /// This is how a session goes deeper than its own memory: the page's in-memory history holds
    /// [`crate::history::HISTORY_EDITS`], the note holds [`HISTORY_DEPTH`], and an undo that runs out of the
    /// first asks for one more out of the second. Nothing is applied by knowing it — the edit is *already*
    /// applied, since it is below the cursor — so this only extends the stack an undo walks.
    pub fn older_history(&self, page: u64, skip: usize) -> Result<Option<Edit>> {
        let Some((id, at)) = self.cursor(page)? else {
            return Ok(None);
        };

        older_edit(&self.conn, id, at, skip as i64)
    }

    /// How many edits a page's history holds, for the tests that check the depth is a depth.
    #[cfg(test)]
    pub fn history_rows(&self, page: u64) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edits WHERE page_id = (SELECT id FROM pages WHERE ord = ?1)",
                params![page as i64],
                |row| row.get(0),
            )
            .map_err(sql)?)
    }

    /// A page's row and its cursor, when its history is one this build can trust.
    ///
    /// `None` for a page with no row, a page nothing has been done to, and a page whose log no longer describes
    /// its ink — the three answers that all mean "there is no history here" (see [`Self::load_page`]). The count a
    /// cursor stands for is the page's *whole* ink, compacted strokes and write-ahead rows together, which is the
    /// same number [`Self::stroke_count`] answers with and the same one the app's write path measures against.
    fn cursor(&self, page: u64) -> Result<Option<(i64, i64)>> {
        let row = self
            .conn
            .query_row(
                "SELECT p.id, p.history_at, p.history_count,
                        p.stroke_count
                        + COALESCE((SELECT COUNT(*) FROM dirty_strokes d WHERE d.page_id = p.id), 0)
                   FROM pages p
                  WHERE p.ord = ?1",
                params![page as i64],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(sql)?;

        let Some((id, at, history_count, stroke_count)) = row else {
            return Ok(None);
        };

        if at <= 0 || history_count != stroke_count {
            return Ok(None);
        }

        Ok(Some((id, at)))
    }
}

/// A page as it is read out of a note: the ink, and the history it has been through.
#[derive(Debug)]
pub struct LoadedPage {
    /// The strokes, in the order they were drawn.
    pub strokes: Vec<Stroke>,
    /// The history: the edits applied, the ones taken back, and the cursor they both hang off.
    pub history: History,
}

/// Starts a transaction that is going to write, reporting failure the way every other failure here is
/// reported.
///
/// `BEGIN IMMEDIATE` rather than the deferred transaction `rusqlite` opens by default, and that is the
/// difference between a note that saves and one that reports "database is locked". The writer thread
/// and the app each hold their own connection to one file (see the module docs), and every write here
/// reads before it writes — the page's row, then the next sequence number, then the insert. A deferred
/// transaction takes its write lock at that first write, so a commit on the other connection in between
/// leaves this one holding a snapshot it can no longer write over; SQLite answers that with
/// `SQLITE_BUSY_SNAPSHOT`, which reaches the user as "database is locked", and the busy timeout does not
/// apply to it, because waiting cannot change the snapshot the transaction is holding. Taking the lock
/// at the start turns the wait back into one the timeout covers: the transaction is queued behind the
/// other writer rather than failing for being late.
impl NoteStore {
    fn begin(&mut self) -> Result<rusqlite::Transaction<'_>> {
        self.conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)
    }
}

/// Writes what a page's history did: the tail of applied edits the note has not been told about, and the cursor
/// that tail ends at.
///
/// Called by [`NoteStore::append`] and [`NoteStore::rewrite`] inside their transactions rather than sent as a
/// job, which is the whole design: the log and the ink are written together, so a crash can never leave the note
/// with an edit that describes ink it does not have.
///
/// The rule for the tail is one statement: everything the note holds from the tail's own first ordinal onwards is
/// replaced by it. That is also how a page's *redo branch* is dropped — an edit made over an undone one takes its
/// place — without either side having to count what went.
fn write_history(conn: &Connection, id: i64, update: &HistoryUpdate, now: i64) -> Result<()> {
    if !update.appended.is_empty() {
        // The tail replaces everything the note holds from its own first ordinal onwards (1-based: the tail's
        // first edit is `applied - len + 1`), which is also how a page's redo branch is dropped when an edit is
        // made over an undone one.
        let first = (update.applied - update.appended.len() as u64 + 1) as i64;
        conn.execute(
            "DELETE FROM edits WHERE page_id = ?1 AND ord >= ?2",
            params![id, first],
        )
        .map_err(sql)?;
    }

    for (index, edit) in update.appended.iter().enumerate() {
        let ord = update.applied - update.appended.len() as u64 + 1 + index as u64;
        conn.execute(
            "INSERT INTO edits (page_id, ord, data, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, ord as i64, edit.encode()?, now],
        )
        .map_err(sql)?;
    }

    // The cursor, and the stroke count it stands for: the second is what tells a later open whether the log still
    // describes this page (see [`NoteStore::history`]).
    conn.execute(
        "UPDATE pages SET history_at = ?1, history_count = ?2 WHERE id = ?3",
        params![update.applied as i64, update.count as i64, id],
    )
    .map_err(sql)?;

    Ok(())
}

/// The applied edits of a page: the newest `depth` of them at or below the cursor, oldest first.
fn edits_applied(conn: &Connection, id: i64, at: i64, depth: i64) -> Result<Vec<Edit>> {
    let mut edits = edits_at_most(conn, id, at, depth, 0)?;
    edits.reverse();
    Ok(edits)
}

/// The undone edits of a page: the newest `depth` of them above the cursor, oldest first.
fn edits_undone(conn: &Connection, id: i64, at: i64, depth: i64) -> Result<Vec<Edit>> {
    let mut statement = conn
        .prepare(
            "SELECT data FROM edits
              WHERE page_id = ?1 AND ord > ?2
              ORDER BY ord
              LIMIT ?3",
        )
        .map_err(sql)?;

    let rows = statement
        .query_map(params![id, at, depth], |row| row.get::<_, Vec<u8>>(0))
        .map_err(sql)?;

    let mut edits = Vec::new();
    for row in rows {
        edits.push(Edit::decode(&row.map_err(sql)?)?);
    }

    Ok(edits)
}

/// One applied edit of a page: the `skip`th newest line at or below the cursor, or nothing when the log does not
/// reach that far.
fn older_edit(conn: &Connection, id: i64, at: i64, skip: i64) -> Result<Option<Edit>> {
    Ok(edits_at_most(conn, id, at, 1, skip)?.pop())
}

/// The newest `depth` edits of a page at or below `at`, newest first, missing the first `skip` of them.
fn edits_at_most(conn: &Connection, id: i64, at: i64, depth: i64, skip: i64) -> Result<Vec<Edit>> {
    let mut statement = conn
        .prepare(
            "SELECT data FROM edits
              WHERE page_id = ?1 AND ord <= ?2
              ORDER BY ord DESC
              LIMIT ?3 OFFSET ?4",
        )
        .map_err(sql)?;

    let rows = statement
        .query_map(params![id, at, depth, skip], |row| row.get::<_, Vec<u8>>(0))
        .map_err(sql)?;

    let mut edits = Vec::new();
    for row in rows {
        edits.push(Edit::decode(&row.map_err(sql)?)?);
    }

    Ok(edits)
}

/// The error type rusqlite gives, reported as a note problem.
fn sql(error: rusqlite::Error) -> AppError {
    AppError::Note(format!("the note's database refused a change: {error}"))
}

impl NoteStore {
    /// Every page that holds ink, in reading order.
    ///
    /// A page with nothing on it has no row: the note's *page list* — including which pages are
    /// blank — belongs to the note, and is [`META_LAYOUT`]. This is the ink.
    pub fn pages(&self) -> Result<Vec<u64>> {
        let mut statement = self
            .conn
            .prepare("SELECT ord FROM pages ORDER BY ord")
            .map_err(sql)?;

        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(sql)?;

        let mut pages = Vec::new();
        for row in rows {
            pages.push(row.map_err(sql)? as u64);
        }

        Ok(pages)
    }

    /// How many strokes a page holds, chunks and dirty strokes together.
    ///
    /// Counted by the database rather than by loading the page: the answer is wanted by the status
    /// line, and by nothing that needs the ink itself.
    pub fn stroke_count(&self, page: u64) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE((SELECT p.stroke_count FROM pages p WHERE p.ord = ?1), 0)
                      + COALESCE((SELECT COUNT(*) FROM dirty_strokes d
                                    JOIN pages p ON p.id = d.page_id
                                   WHERE p.ord = ?1), 0)",
                params![page as i64],
                |row| row.get(0),
            )
            .map_err(sql)?;

        Ok(count as usize)
    }

    /// A page's ink: its chunks and its dirty strokes, in the order it was drawn.
    ///
    /// The rows come back through the `page_strokes` view in the order `ord` gives them — chunks by
    /// where their strokes start, dirty strokes after them in the order they were appended. Each
    /// chunk's strokes are then decompressed in parallel and put back into place, so opening a page
    /// costs one blob's worth of work per core rather than a sum over the page.
    pub fn load(&self, page: u64) -> Result<Vec<Stroke>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT ord, stroke_count, codec, raw_len, crc32, data, is_dirty
                   FROM page_strokes
                  WHERE page_id = (SELECT id FROM pages WHERE ord = ?1)
                  ORDER BY ord, seq",
            )
            .map_err(sql)?;

        let rows = statement
            .query_map(params![page as i64], |row| {
                Ok(Row {
                    strokes: row.get::<_, i64>(1)? as usize,
                    codec: row.get::<_, i64>(2)?,
                    raw_len: row.get::<_, i64>(3)? as usize,
                    crc32: row.get::<_, i64>(4)? as u32,
                    data: row.get::<_, Vec<u8>>(5)?,
                    dirty: row.get::<_, i64>(6)? != 0,
                })
            })
            .map_err(sql)?;

        let mut loaded: Vec<Row> = Vec::new();
        for row in rows {
            loaded.push(row.map_err(sql)?);
        }

        if loaded.is_empty() {
            return Ok(Vec::new());
        }

        // The chunks are decoded in parallel and the dirty strokes are not: a chunk is a page's
        // worth of ink and a dirty row is one stroke, so the pool would spend more on the hand-off
        // than on the work.
        let decoded: Result<Vec<Vec<Stroke>>> = loaded
            .par_iter()
            .map(|row| {
                if row.dirty {
                    open_dirty(&row.data)
                } else {
                    chunk::decode(
                        &row.data,
                        row.raw_len,
                        Codec::from_id(row.codec as u32)?,
                        row.strokes,
                        row.crc32,
                    )
                }
            })
            .collect();

        let mut strokes: Vec<Stroke> = Vec::new();
        for part in decoded? {
            strokes.extend(part);
        }

        Ok(strokes)
    }
}

/// One row of the `page_strokes` view, as [`NoteStore::load`] reads it.
#[derive(Debug)]
struct Row {
    /// How many strokes the row describes.
    strokes: usize,
    /// The compressor, for a chunk row.
    codec: i64,
    /// The length of the arrays before compression, for a chunk row.
    raw_len: usize,
    /// The checksum of the blob, for a chunk row.
    crc32: u32,
    /// The blob itself: a chunk, or one stroke.
    data: Vec<u8>,
    /// Whether this row is a dirty stroke rather than a chunk.
    dirty: bool,
}

impl NoteStore {
    /// Makes room at `at` for a page being inserted there, moving every page after it along.
    ///
    /// Two statements through the offset, because `ord` is unique: see [`SHIFT`]. Nothing else has
    /// to move — `chunks` and `dirty_strokes` point at `pages.id`, which does not change — which is
    /// the whole reason a page has an identity *and* a position.
    ///
    /// The bookmarks move with the pages, in the same transaction and by the same two statements: a
    /// mark names a page's *position*, so a page inserted before a marked one puts that mark one
    /// place further along. The alternative — a mark left behind at the number it was made at — is a
    /// bookmark that opens a page the person never marked, which is worse than no bookmark at all.
    /// The ink needs none of this because it is keyed by `pages.id`; a mark is a position, so it is
    /// renamed the way every position after the insertion is.
    pub fn insert_page(&mut self, at: u64) -> Result<()> {
        let tx = self.begin()?;

        for table in ["pages", "bookmarks"] {
            tx.execute(
                &format!("UPDATE {table} SET ord = ord + ?1 WHERE ord >= ?2"),
                params![SHIFT, at as i64],
            )
            .map_err(sql)?;
            tx.execute(
                &format!("UPDATE {table} SET ord = ord - ?1 WHERE ord >= ?2"),
                params![SHIFT - 1, SHIFT + at as i64],
            )
            .map_err(sql)?;
        }

        tx.commit().map_err(sql)?;
        Ok(())
    }

    /// Removes the page at `at`, and the ink written on it, and closes the gap behind it.
    ///
    /// The ink goes with the page: a deleted page's writing has nowhere to be shown. The cascade on
    /// `chunks` and `dirty_strokes` is what makes that one statement rather than three.
    ///
    /// A bookmark on that page goes too, and the marks after it close the gap exactly as the pages do:
    /// what is being deleted is a *page*, and a mark for the page that used to be here would be a mark
    /// that opens its neighbour.
    pub fn delete_page(&mut self, at: u64) -> Result<()> {
        let tx = self.begin()?;

        tx.execute("DELETE FROM pages WHERE ord = ?1", params![at as i64])
            .map_err(sql)?;
        tx.execute("DELETE FROM bookmarks WHERE ord = ?1", params![at as i64])
            .map_err(sql)?;

        for table in ["pages", "bookmarks"] {
            tx.execute(
                &format!("UPDATE {table} SET ord = ord + ?1 WHERE ord > ?2"),
                params![SHIFT, at as i64],
            )
            .map_err(sql)?;
            tx.execute(
                &format!("UPDATE {table} SET ord = ord - ?1 WHERE ord >= ?2"),
                params![SHIFT + 1, SHIFT + at as i64],
            )
            .map_err(sql)?;
        }

        tx.commit().map_err(sql)?;
        Ok(())
    }

    /// Marks the page at `ord` with a bookmark, or takes the mark off it.
    ///
    /// Marking twice is marking once: the row is inserted only if the page has none, so a command
    /// that arrives twice — two clicks, a key as well as a button — cannot make a second mark or
    /// rewrite the first one's time. Taking a mark off a page that has none is likewise nothing rather
    /// than an error: the state that was asked for is the state that is there.
    ///
    /// The page needs no ink for this: a mark is a fact about a page of the note, and a blank page
    /// somebody marked is a page of the note like any other (this is why `bookmarks` is not keyed by
    /// `pages.id`, which only exists once a page holds ink).
    pub fn set_bookmark(&mut self, ord: u64, mark: bool) -> Result<()> {
        if mark {
            self.conn
                .execute(
                    "INSERT INTO bookmarks (ord, created_at) VALUES (?1, ?2)
                     ON CONFLICT(ord) DO NOTHING",
                    params![ord as i64, now_millis()],
                )
                .map_err(sql)?;
        } else {
            self.conn
                .execute("DELETE FROM bookmarks WHERE ord = ?1", params![ord as i64])
                .map_err(sql)?;
        }

        Ok(())
    }

    /// The marked pages, in the note's reading order.
    ///
    /// The order is the database's rather than the list's: marks made in any order come back in the
    /// order the pages are read in, which is the only order a bookmark list can be useful in, and
    /// which means there is no order of its own for the app to store or repair.
    pub fn bookmarks(&self) -> Result<Vec<u64>> {
        let mut statement = self
            .conn
            .prepare("SELECT ord FROM bookmarks ORDER BY ord")
            .map_err(sql)?;

        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(sql)?;

        let mut marks = Vec::new();
        for row in rows {
            marks.push(row.map_err(sql)? as u64);
        }

        Ok(marks)
    }

    /// Folds the write-ahead log back into the note file.
    ///
    /// WAL means writes are cheap and reads never wait, at the cost of a `-wal` file growing beside
    /// the note. This truncates it — the design note calls it the idle housekeeping — so a note does
    /// not sit next to a log the size of the note itself until something opens it again.
    ///
    /// The same idle hour is when a page's history is cut back to [`HISTORY_DEPTH`]: history is a *depth* and
    /// not a lifetime, and the rows this drops are the deepest undos — the ones a reader would have had to walk
    /// back through every edit since to reach. It happens here rather than on a write because it is housekeeping:
    /// how far back a note can be undone is not something a person is waiting for.
    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM edits
                  WHERE ord <= (SELECT history_at FROM pages WHERE pages.id = edits.page_id) - ?1",
                params![HISTORY_DEPTH],
            )
            .map_err(sql)?;

        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|error| AppError::Note(format!("the note could not be tidied up: {error}")))
    }

    /// Writes a complete, standalone copy of the note to `path`: `VACUUM INTO`.
    ///
    /// This is what makes a note *movable*. The file it writes has the WAL folded in, no `-wal`
    /// beside it, no journal and no free pages: it is the whole note, and it is what the app packs
    /// into a zip for the user to carry to another machine. Copying an open database file instead
    /// would copy a database in the middle of being written.
    pub fn export(&self, path: &Path) -> Result<()> {
        let destination = path.to_string_lossy().to_string();

        self.conn
            .execute("VACUUM INTO ?1", params![destination])
            .map_err(|error| {
                AppError::Note(format!("{} could not be written: {error}", path.display()))
            })?;

        Ok(())
    }
}

impl NoteStore {
    /// Records which page was open, so reopening the note comes back to it.
    pub fn set_open_page(&mut self, page: u64) -> Result<()> {
        put_row(&self.conn, META_OPEN_PAGE, page)
    }

    /// The page that was open, if the note says.
    pub fn open_page(&self) -> Result<Option<u64>> {
        row_maybe(&self.conn, META_OPEN_PAGE, whole_of)
    }

    /// Records the sheet the ink was written on, in logical pixels.
    ///
    /// A sheet is two rows, written together or not at all: one row per value is how everything in this
    /// table is kept, and the transaction is what stops a note from holding a width and no height.
    pub fn set_sheet(&mut self, sheet: Option<(f32, f32)>) -> Result<()> {
        let tx = self.begin()?;

        match sheet {
            Some((width, height)) => {
                put_row(&tx, META_SHEET_WIDTH, width)?;
                put_row(&tx, META_SHEET_HEIGHT, height)?;
            }
            None => {
                drop_row(&tx, META_SHEET_WIDTH)?;
                drop_row(&tx, META_SHEET_HEIGHT)?;
            }
        }

        tx.commit().map_err(sql)?;
        Ok(())
    }

    /// The sheet the ink was written on, when the note says.
    ///
    /// Both rows or nothing: a note holding one of them says nothing rather than half a sheet, which is
    /// the state a row edited by hand leaves behind.
    pub fn sheet(&self) -> Result<Option<(f32, f32)>> {
        let width = row_maybe(&self.conn, META_SHEET_WIDTH, number_of)?;
        let height = row_maybe(&self.conn, META_SHEET_HEIGHT, number_of)?;

        Ok(match (width, height) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => None,
        })
    }

    /// Records what each page shows, in reading order: the note's own page list.
    ///
    /// A page's rotation is part of the page here as it is everywhere else (see
    /// [`crate::pages::PageEntry`]): this list is the note's answer to "what is page 4", and "which
    /// way up is it" is part of that answer rather than a property of the window.
    pub fn set_layout(&mut self, layout: &[crate::pages::PageEntry]) -> Result<()> {
        let text = serde_json::to_string(layout).map_err(|error| {
            AppError::Note(format!(
                "the note's page list could not be written: {error}"
            ))
        })?;

        put_row(&self.conn, META_LAYOUT, text)
    }

    /// What each page shows, in reading order. Empty means the note has no list of its own: its pages
    /// are the ones that hold ink — see [`crate::pages::Pages::restore`].
    ///
    /// A row that is not a list of pages is reported rather than replaced, as everything else here is:
    /// the alternative is a note whose pages are silently rearranged. A row written *before* a page
    /// could be turned is a list of pages with nothing about a rotation in it, and is read as pages
    /// that are the right way up — see [`crate::pages::StoredPage`].
    pub fn layout(&self) -> Result<Vec<crate::pages::PageEntry>> {
        let Some(text) = row_text(&self.conn, META_LAYOUT)? else {
            return Ok(Vec::new());
        };

        let stored: Vec<crate::pages::StoredPage> =
            serde_json::from_str(&text).map_err(|error| {
                AppError::Note(format!("the note's page list could not be read: {error}"))
            })?;

        Ok(stored
            .into_iter()
            .map(crate::pages::StoredPage::entry)
            .collect())
    }

    /// Reads the note's settings into `into`.
    ///
    /// **This is where the note's rows meet the app's type**: one line per setting, named exactly as its
    /// row is, in the order [`crate::settings::Settings`] declares them. Those lines, the field, and its
    /// default are the whole of a setting's schema — which is what makes adding one four small edits and
    /// no migration, and why a row a *newer* build wrote is not read here, not written by
    /// [`Self::set_settings`], and not deleted: it is simply not ours to touch (see [`crate::settings`]).
    ///
    /// `into` is the answer for everything the note does not say. The caller passes the settings in
    /// hand, so a note that has never been told anything keeps the sheet and the pen that are already
    /// up — which is how a PDF just placed continues the page it was placed on instead of snapping back
    /// to the shipped defaults. A note that does say something is read over it.
    ///
    /// A row that is there and cannot be understood is an error, not a default: something wrote it, and
    /// quietly replacing a person's answer is worse than saying the note cannot be read.
    pub fn read_settings(&self, into: &mut Settings) -> Result<()> {
        let conn = &self.conn;

        into.ink_color = row_value(conn, "ink_color", into.ink_color, colour_of)?;
        into.page_color = row_value(conn, "page_color", into.page_color, colour_of)?;
        into.pen_weight = row_value(conn, "pen_weight", into.pen_weight, PenWeight::from_label)?;
        into.grayscale_pages = row_value(conn, "grayscale_pages", into.grayscale_pages, flag_of)?;
        into.canvas_size = row_value(
            conn,
            "canvas_size",
            into.canvas_size,
            CanvasSize::from_label,
        )?;
        into.canvas_style = row_value(
            conn,
            "canvas_style",
            into.canvas_style,
            CanvasStyle::from_label,
        )?;
        into.page_display_width = row_value(
            conn,
            "page_display_width",
            into.page_display_width,
            number_of,
        )?;
        into.zoom = row_value(conn, "zoom", into.zoom, number_of)?;
        into.resample_spacing =
            row_value(conn, "resample_spacing", into.resample_spacing, number_of)?;
        into.smoothing_ms = row_value(conn, "smoothing_ms", into.smoothing_ms, number_of)?;
        into.min_width = row_value(conn, "min_width", into.min_width, number_of)?;
        into.max_width = row_value(conn, "max_width", into.max_width, number_of)?;
        into.no_pressure_width =
            row_value(conn, "no_pressure_width", into.no_pressure_width, number_of)?;
        into.erase_radius = row_value(conn, "erase_radius", into.erase_radius, number_of)?;
        into.show_toolbar = row_value(conn, "show_toolbar", into.show_toolbar, flag_of)?;
        into.show_status = row_value(conn, "show_status", into.show_status, flag_of)?;
        into.show_tilt_cursor =
            row_value(conn, "show_tilt_cursor", into.show_tilt_cursor, flag_of)?;

        // A path is text with nothing to look up, and *no row* is the honest answer for "no place
        // remembered yet" — an empty row would be a place with no name.
        into.export_dir = row_text(conn, "export_dir")?
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);

        Ok(())
    }

    /// Writes every setting the note keeps, in one transaction.
    ///
    /// *Every* setting, every time something changes: eighteen small rows in one commit, and writing the
    /// whole set is what makes the note the authority — a note written by a build that knew fewer
    /// settings, or one whose rows were edited by hand, is given this build's complete answer the first
    /// time anything changes. A row this build does not know is not in the list, so it is neither
    /// overwritten nor deleted.
    ///
    /// One transaction because some of these belong together: a paper size and the width it is drawn at
    /// are set by one command (see [`crate::app::NoteApp::set_canvas_size`]), and a note caught holding
    /// one without the other is a note whose sheet and scale disagree.
    pub fn set_settings(&mut self, settings: &Settings) -> Result<()> {
        let tx = self.begin()?;

        put_row(&tx, "ink_color", settings.ink_color)?;
        put_row(&tx, "page_color", settings.page_color)?;
        put_row(&tx, "pen_weight", settings.pen_weight.label())?;
        put_flag(&tx, "grayscale_pages", settings.grayscale_pages)?;
        put_row(&tx, "canvas_size", settings.canvas_size.label())?;
        put_row(&tx, "canvas_style", settings.canvas_style.label())?;
        put_row(&tx, "page_display_width", settings.page_display_width)?;
        put_row(&tx, "zoom", settings.zoom)?;
        put_row(&tx, "resample_spacing", settings.resample_spacing)?;
        put_row(&tx, "smoothing_ms", settings.smoothing_ms)?;
        put_row(&tx, "min_width", settings.min_width)?;
        put_row(&tx, "max_width", settings.max_width)?;
        put_row(&tx, "no_pressure_width", settings.no_pressure_width)?;
        put_row(&tx, "erase_radius", settings.erase_radius)?;
        put_flag(&tx, "show_toolbar", settings.show_toolbar)?;
        put_flag(&tx, "show_status", settings.show_status)?;
        put_flag(&tx, "show_tilt_cursor", settings.show_tilt_cursor)?;

        match &settings.export_dir {
            Some(dir) => put_row(&tx, "export_dir", dir.display())?,
            None => drop_row(&tx, "export_dir")?,
        }

        tx.commit().map_err(sql)?;
        Ok(())
    }

    /// Records the name the document had, so a note stays about the file it was made from.
    pub fn set_document(&mut self, name: &str) -> Result<()> {
        put_row(&self.conn, META_DOCUMENT, name)
    }

    /// The name the document had, if the note has one.
    pub fn document(&self) -> Result<Option<String>> {
        Ok(row_text(&self.conn, META_DOCUMENT)?.filter(|name| !name.is_empty()))
    }

    /// Records the file the note was placed from, or that there was none.
    ///
    /// "There was none" is the row's *absence*, not an empty path: a note written on a blank sheet has
    /// nothing to remember, so there is nothing to store.
    pub fn set_source(&mut self, path: Option<&Path>) -> Result<()> {
        match path {
            Some(path) => put_row(&self.conn, META_SOURCE, path.display()),
            None => drop_row(&self.conn, META_SOURCE),
        }
    }

    /// The file the note was placed from, if it remembers one.
    pub fn source(&self) -> Result<Option<String>> {
        Ok(row_text(&self.conn, META_SOURCE)?.filter(|path| !path.is_empty()))
    }

    /// Gives the note a name, or takes its name away.
    ///
    /// An empty name is a legitimate answer, and means *no name* — so it takes the row away rather than
    /// storing an empty one: the absence of a name is the absence of the row. What is written is
    /// [`normalize_title`]'s answer, so a name that arrives from a paste on two lines or is longer than
    /// a row becomes the name a person meant rather than an error.
    pub fn set_title(&mut self, name: &str) -> Result<()> {
        let name = normalize_title(name);

        match name.is_empty() {
            true => drop_row(&self.conn, META_TITLE),
            false => put_row(&self.conn, META_TITLE, name),
        }
    }

    /// The name a person gave the note, if they gave it one.
    pub fn title(&self) -> Result<Option<String>> {
        Ok(row_text(&self.conn, META_TITLE)?.filter(|name| !name.is_empty()))
    }

    /// Everything the note says about itself: its name, its document, and its counts.
    ///
    /// The read the home screen's list makes for a note that changed: three small `meta` reads and two
    /// counts, and not a byte of ink. See [`Facts`] and [`Summary`].
    pub fn facts(&self) -> Result<Facts> {
        Ok(Facts {
            title: self.title()?,
            document: self.document()?,
            summary: self.summary()?,
        })
    }

    /// What the note holds: its pages, its strokes, and the sheet they were written on.
    ///
    /// Two counts and one small read, and no blob is touched — which is what lets a list of forty
    /// notes be drawn without opening forty pages of ink (see [`crate::recent`]). The page count is
    /// the note's own *list* when it has one, because that is what a reader can turn to; a note
    /// written before the list existed has only the pages that hold ink.
    fn summary(&self) -> Result<Summary> {
        let (strokes, ink_pages): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COALESCE((SELECT SUM(stroke_count) FROM pages), 0)
                      + (SELECT COUNT(*) FROM dirty_strokes),
                        (SELECT COUNT(*) FROM pages)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(sql)?;

        let listed = self.layout()?.len();
        let pages = if listed > 0 {
            listed
        } else {
            (ink_pages.max(1)) as usize
        };

        Ok(Summary {
            pages,
            strokes: strokes as usize,
            sheet: self.sheet()?,
        })
    }
}

// ── the rows ─────────────────────────────────────────────────────────────────────────────────────
//
// Everything a note knows is one row of `meta`: a name, and its value as text. These five functions are
// the whole of how a row is read and written, and they take a *connection* rather than `&NoteStore`
// because a batch of rows has to share one transaction — rusqlite's `Transaction` is a `Connection` for
// every purpose here, so the same call serves one row and eighteen.

/// A row's value, as text.
fn row_text(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(sql)
}

/// A row's value, turned into the app's value by `parse` — or `fallback` when there is no such row.
///
/// This is the one place a note's answer becomes the app's, and the difference between the two ways that
/// can fail to happen is the whole of the design. **No row is not a failure**: the note simply does not
/// say, and the caller's own value stands — which is what makes a note written by a build that knew
/// fewer settings open with the shipped answer instead of with nothing. **A row that will not parse
/// is** a failure, because something wrote it, and quietly replacing a person's answer is worse than
/// saying the note cannot be read.
fn row_value<T>(
    conn: &Connection,
    key: &str,
    fallback: T,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T> {
    let Some(text) = row_text(conn, key)? else {
        return Ok(fallback);
    };

    parse(&text).ok_or_else(|| {
        AppError::Note(format!(
            "the note's {key} is not something this build knows: {text}"
        ))
    })
}

/// A row's value, or `None` when the note has no such row: for the facts that have no default.
///
/// The same two cases as [`row_value`], with "no row" answered by `None` rather than by a fallback —
/// which is what a *fact* wants, because there is no shipped answer for the file a note was placed
/// from. `None` says the note does not remember it; a row that will not parse is still an error.
fn row_maybe<T>(
    conn: &Connection,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>> {
    let Some(text) = row_text(conn, key)? else {
        return Ok(None);
    };

    parse(&text).map(Some).ok_or_else(|| {
        AppError::Note(format!(
            "the note's {key} is not something this build knows: {text}"
        ))
    })
}

/// Writes a row, as the text its value spells.
fn put_row(conn: &Connection, key: &str, value: impl std::fmt::Display) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value.to_string()],
    )
    .map_err(sql)?;

    Ok(())
}

/// Writes a flag spelled out, so a person reading the database sees a word rather than a number.
fn put_flag(conn: &Connection, key: &str, value: bool) -> Result<()> {
    put_row(conn, key, if value { "true" } else { "false" })
}

/// Takes a row away, so the note goes back to not saying whatever it said.
///
/// What "not saying" means is [`row_value`]'s business: for a setting it is the app's own default, and
/// for a fact — a source file, a name, a sheet — it is *nothing*.
fn drop_row(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", params![key])
        .map_err(sql)?;

    Ok(())
}

/// The number a row holds: a float, which is every measurement here.
fn number_of(text: &str) -> Option<f32> {
    text.parse().ok()
}

/// The whole number a row holds: a page, or a count.
fn whole_of(text: &str) -> Option<u64> {
    text.parse().ok()
}

/// The colour a row holds: `0xRRGGBB` as one whole number, as it is everywhere else in the app.
fn colour_of(text: &str) -> Option<u32> {
    text.parse().ok()
}

/// The flag a row holds: `true` or `false`, spelled out.
fn flag_of(text: &str) -> Option<bool> {
    match text {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// What a name becomes when it is written down: one line, trimmed, and not too long.
///
/// A rule rather than a validation, deliberately: a name that arrives *wrong* — pasted on two lines,
/// padded with spaces, longer than a row can show — should become the name a person meant, not an
/// error to argue with. So a newline becomes a space, runs of whitespace collapse to one, the ends are
/// trimmed, and the result is cut to [`TITLE_MAX`] characters.
///
/// Cutting on characters and not on bytes is the one part of this that is not cosmetic: a name in
/// Korean is three bytes a syllable, so a byte cut would both shorten the name twice as fast and be
/// able to split a syllable in half.
fn normalize_title(name: &str) -> String {
    let mut clean = String::with_capacity(name.len().min(TITLE_MAX * 4));
    let mut letters = 0;
    let mut space = false;

    for character in name.chars() {
        if character.is_whitespace() || character.is_control() {
            space = true;
            continue;
        }

        if space && letters > 0 {
            if letters == TITLE_MAX {
                break;
            }
            clean.push(' ');
            letters += 1;
        }
        space = false;

        if letters == TITLE_MAX {
            break;
        }

        clean.push(character);
        letters += 1;
    }

    clean
}

/// The row id of the page at `ord`, making the row if it is asked for and is not there yet.
///
/// A page has no row until it has ink, which is what makes "the pages that hold ink" a query rather
/// than a column to keep in step.
fn page_row(conn: &Connection, ord: u64, create: bool) -> Result<Option<i64>> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT id FROM pages WHERE ord = ?1",
            params![ord as i64],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)?;

    if found.is_some() || !create {
        return Ok(found);
    }

    let now = now_millis();
    conn.execute(
        "INSERT INTO pages (ord, created_at, updated_at, stroke_count) VALUES (?1, ?2, ?2, 0)",
        params![ord as i64, now],
    )
    .map_err(sql)?;

    Ok(Some(conn.last_insert_rowid()))
}

/// The next sequence number for a page's dirty strokes.
fn next_dirty_seq(conn: &Connection, id: i64) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM dirty_strokes WHERE page_id = ?1",
        params![id],
        |row| row.get(0),
    )
    .map_err(sql)
}

/// The next sequence number for a page's chunks.
fn next_chunk_seq(conn: &Connection, id: i64) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM chunks WHERE page_id = ?1",
        params![id],
        |row| row.get(0),
    )
    .map_err(sql)
}

/// How many of a page's strokes are in its chunks, which is where the next chunk starts.
fn strokes_in_chunks(conn: &Connection, id: i64) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT stroke_count FROM pages WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .map_err(sql)?;

    Ok(count as usize)
}

/// The strokes of every dirty row of a page, in the order they were appended.
fn read_dirty(conn: &Connection, id: i64) -> Result<Vec<Stroke>> {
    let mut statement = conn
        .prepare("SELECT data FROM dirty_strokes WHERE page_id = ?1 ORDER BY seq")
        .map_err(sql)?;

    let rows = statement
        .query_map(params![id], |row| row.get::<_, Vec<u8>>(0))
        .map_err(sql)?;

    let mut strokes = Vec::new();
    for row in rows {
        strokes.extend(open_dirty(&row.map_err(sql)?)?);
    }

    Ok(strokes)
}

/// Writes strokes as chunks, starting the sequence and the stroke count where the page is.
///
/// The slicing is [`crate::chunk::chunk_ranges`]'s: a chunk per 64 KB of estimated arrays or per 512
/// strokes, in order, so the chunks of a page tile its ink and `stroke_start` is where each one
/// begins in the page.
fn write_chunks(
    conn: &Connection,
    id: i64,
    seq_start: i64,
    start: usize,
    strokes: &[Stroke],
) -> Result<()> {
    let mut insert = conn
        .prepare_cached(
            "INSERT INTO chunks (page_id, seq, stroke_start, stroke_count, codec, raw_len, data, crc32)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .map_err(sql)?;

    for (index, range) in chunk::chunk_ranges(strokes).into_iter().enumerate() {
        let slice = &strokes[range.clone()];
        let encoded = chunk::encode(slice)?;

        insert
            .execute(params![
                id,
                seq_start + index as i64,
                (start + range.start) as i64,
                slice.len() as i64,
                encoded.codec.id() as i64,
                encoded.raw_len as i64,
                encoded.data,
                encoded.crc32 as i64,
            ])
            .map_err(sql)?;
    }

    Ok(())
}

/// Moves a page's bounds out to cover these strokes, and touches its timestamp.
///
/// The bounds are what the app can answer "is this page blank?" and "where is the ink?" with without
/// loading it, so they are maintained by the writer rather than derived by a reader.
fn touch_bounds(conn: &Connection, id: i64, strokes: &[Stroke]) -> Result<()> {
    let mut bounds: Option<[f32; 4]> = None;

    for stroke in strokes {
        if stroke.points.is_empty() {
            continue;
        }

        bounds = Some(match bounds {
            Some(current) => [
                current[0].min(stroke.bounds[0]),
                current[1].min(stroke.bounds[1]),
                current[2].max(stroke.bounds[2]),
                current[3].max(stroke.bounds[3]),
            ],
            None => stroke.bounds,
        });
    }

    match bounds {
        Some([min_x, min_y, max_x, max_y]) => {
            conn.execute(
                "UPDATE pages
                    SET bbox_min_x = COALESCE(MIN(bbox_min_x, ?1), ?1),
                        bbox_min_y = COALESCE(MIN(bbox_min_y, ?2), ?2),
                        bbox_max_x = COALESCE(MAX(bbox_max_x, ?3), ?3),
                        bbox_max_y = COALESCE(MAX(bbox_max_y, ?4), ?4),
                        updated_at = ?5
                  WHERE id = ?6",
                params![min_x, min_y, max_x, max_y, now_millis(), id],
            )
            .map_err(sql)?;
        }
        None => {
            conn.execute(
                "UPDATE pages SET updated_at = ?1 WHERE id = ?2",
                params![now_millis(), id],
            )
            .map_err(sql)?;
        }
    }

    Ok(())
}

/// A dirty stroke's blob: the encoded chunk, sealed with the three values the frame carries.
///
/// `dirty_strokes` has one column for the ink — the design note's schema — while a chunk in the `chunks` table
/// describes itself with `codec`, `raw_len` and `crc32`. The frame that carries those three values is
/// [`chunk::seal`]'s, because a page's *history* writes rows of strokes the same way; what is the store's here
/// is only the fact that a dirty row is one stroke.
fn sealed_dirty(stroke: &Stroke) -> Result<Vec<u8>> {
    Ok(chunk::seal(&chunk::encode(std::slice::from_ref(stroke))?))
}

/// The one stroke in a dirty row's blob.
fn open_dirty(blob: &[u8]) -> Result<Vec<Stroke>> {
    chunk::open_sealed(blob, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::InkPoint;
    use crate::pages::{Page, PageEntry};

    /// A note file in the temp directory, named after the test that wants it.
    fn store_for(name: &str) -> (NoteStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cheap-note-store-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");

        let path = dir.join("note.db");
        let store = NoteStore::open(&path).expect("a note");
        (store, path)
    }

    /// Takes a test's note file, and the directory it is in, back out of the temp directory.
    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A transaction that reads before it writes is refused if another connection commits in between —
    /// and taking the lock at the start is what makes it wait instead.
    ///
    /// It is pinned down here as a hazard rather than as a feature of this store, because what decides
    /// it is SQLite's behaviour with two connections on one file — the arrangement the app runs (the
    /// writer thread owns one connection to `note.db`, the app owns another) — and because it is the
    /// reason [`NoteStore::begin`] asks for `BEGIN IMMEDIATE`. The busy timeout is set on both
    /// connections here, deliberately: the refusal is the point, and it is one that waiting cannot fix.
    #[test]
    fn a_deferred_transaction_cannot_write_over_another_connection() {
        let (store, path) = store_for("snapshot-upgrade");
        // The file is created, and its schema with it, before these two open it.
        drop(store);

        let deferred = Connection::open(&path).expect("a connection");
        let other = Connection::open(&path).expect("a second connection");
        for conn in [&deferred, &other] {
            conn.execute_batch("PRAGMA busy_timeout = 5000;")
                .expect("a timeout to wait with");
        }

        deferred
            .execute_batch("BEGIN DEFERRED;")
            .expect("a deferred transaction");
        // The read that fixes the snapshot this transaction will later try to write over. Every write in
        // this store starts with one of these.
        let _: i64 = deferred
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .expect("a read");

        // The other connection commits in the meantime — the app saving a page while the writer thread is
        // halfway through a page of ink.
        other
            .execute(
                "INSERT INTO meta (key, value) VALUES ('touched', 'yes')
                 ON CONFLICT(key) DO UPDATE SET value = 'yes'",
                [],
            )
            .expect("the other connection writes while the first only reads");

        // And now the deferred transaction's own write: refused, with the timeout unable to help.
        let refused = deferred.execute(
            "INSERT INTO meta (key, value) VALUES ('late', 'no')
             ON CONFLICT(key) DO UPDATE SET value = 'no'",
            [],
        );

        assert!(
            refused.is_err(),
            "a snapshot taken before the other commit cannot be written over: {refused:?}"
        );
        let _ = deferred.execute_batch("ROLLBACK;");

        cleanup(&path);
    }

    /// A stroke of `points` points walking across the page, in `color`.
    fn stroke(points: usize, from: f32, color: u32) -> Stroke {
        let mut stroke = Stroke::new(InkPoint::new(from, 10.0, 2.0), color);

        for step in 1..points {
            stroke.points.push(InkPoint::new(
                from + step as f32 * 1.5,
                10.0 + (step % 4) as f32,
                2.0,
            ));
        }

        stroke.close();
        stroke
    }

    /// The first point of every stroke, which is what says whether an order held.
    fn first_points(strokes: &[Stroke]) -> Vec<f32> {
        strokes
            .iter()
            .map(|stroke| stroke.points.first().map(|point| point.x).unwrap_or(0.0))
            .collect()
    }

    /// A write with no history to tell: what these tests mean by ink landing in a note.
    ///
    /// The store's own writes always carry a page's history, because its log and its ink are written in one
    /// transaction (see [`write_history`]). A test about *ink* has nothing to say about history, and this is how
    /// it says so — the same "no history here" that a page written by a build which kept none is read as.
    fn append(store: &mut NoteStore, page: u64, strokes: &[Stroke]) -> Result<()> {
        store.append(page, strokes, &HistoryUpdate::default())
    }

    /// The same, for a page written again from scratch.
    fn rewrite(store: &mut NoteStore, page: u64, strokes: &[Stroke]) -> Result<()> {
        store.rewrite(page, strokes, &HistoryUpdate::default())
    }

    impl NoteStore {
        /// Asks the database a one-number question, for the tests that check the file's settings.
        fn pragma_number(&self, sql: &str) -> i64 {
            self.conn
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .expect("a pragma")
        }

        /// Asks the database a one-word question.
        fn pragma_word(&self, sql: &str) -> String {
            self.conn
                .query_row(sql, [], |row| row.get::<_, String>(0))
                .expect("a pragma")
        }

        /// How many rows a table holds, for the tests that check where the ink went.
        fn rows(&self, table: &str) -> i64 {
            self.conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("a count")
        }

        /// Damages the first chunk of a page, the way a bad disk would.
        fn damage_a_chunk(&self, page: u64) {
            let (id, data): (i64, Vec<u8>) = self
                .conn
                .query_row(
                    "SELECT c.id, c.data FROM chunks c
                       JOIN pages p ON p.id = c.page_id
                      WHERE p.ord = ?1
                      ORDER BY c.seq LIMIT 1",
                    params![page as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("a chunk");

            let mut damaged = data;
            damaged[0] ^= 0x20;
            self.conn
                .execute(
                    "UPDATE chunks SET data = ?1 WHERE id = ?2",
                    params![damaged, id],
                )
                .expect("a damaged chunk");
        }

        /// Runs a statement the tests need, such as stamping a version by hand.
        fn run(&self, sql: &str) {
            self.conn.execute_batch(sql).expect("a statement");
        }
    }

    /// A page list written by an older build — one with nothing about a rotation in it — is read as
    /// pages that are the right way up, and what this build writes keeps its rotation.
    ///
    /// The compatibility is one `serde` attribute away from being a note that cannot be opened, so it
    /// is checked here against the *bytes* such a note has rather than against the type: the shape is
    /// the whole of what an older note and this build have to agree on.
    #[test]
    fn a_page_list_from_an_older_build_is_read_the_right_way_up() {
        let (store, path) = store_for("layout-shapes");

        store.run(
            "INSERT INTO meta (key, value) VALUES ('layout', '[\"blank\",{\"document\":3}]')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        );

        assert_eq!(
            store.layout().expect("a layout"),
            vec![
                PageEntry::new(Page::Blank),
                PageEntry::new(Page::Document(3))
            ],
            "an older note's pages are pages, the right way up"
        );

        cleanup(&path);
    }

    /// A rotation is part of the page, so the note's own list carries it through the file.
    #[test]
    fn a_page_list_keeps_the_rotation_of_each_page() {
        let (mut store, path) = store_for("layout-rotations");

        store
            .set_layout(&[
                PageEntry::new(Page::Document(0)).turned(1),
                PageEntry::new(Page::Blank),
                PageEntry::new(Page::Blank).turned(-1),
            ])
            .expect("a layout");

        assert_eq!(
            store.layout().expect("the layout read back"),
            vec![
                PageEntry::new(Page::Document(0)).turned(1),
                PageEntry::new(Page::Blank),
                PageEntry::new(Page::Blank).turned(3),
            ],
            "each page came back the way up it was written"
        );

        cleanup(&path);
    }

    /// A note is a file with the tables, the view and the journal mode the design asks for.
    #[test]
    fn a_new_note_is_a_tuned_database() {
        let (store, path) = store_for("new");

        assert_eq!(store.pragma_word("PRAGMA journal_mode"), "wal");
        assert_eq!(
            store.pragma_number("PRAGMA user_version"),
            SCHEMA_VERSION as i64
        );
        assert_eq!(store.pragma_number("PRAGMA page_size"), 8192);

        assert!(store.pages().expect("pages").is_empty());
        assert!(store.load(0).expect("a page").is_empty());
        assert_eq!(store.stroke_count(0).expect("a count"), 0);

        cleanup(&path);
    }

    /// Ink that is appended comes back, in the order it was drawn, and waits as a dirty row.
    #[test]
    fn what_is_appended_comes_back() {
        let (mut store, path) = store_for("append");
        let strokes = vec![stroke(6, 0.0, 0x11_11_11), stroke(4, 100.0, 0x22_22_22)];
        append(&mut store, 0, &strokes).expect("the ink is written");

        let loaded = store.load(0).expect("the page is read");
        assert_eq!(first_points(&loaded), vec![0.0, 100.0]);
        assert_eq!(store.pages().expect("pages"), vec![0]);

        assert_eq!(store.stroke_count(0).expect("a count"), 2);
        assert_eq!(
            store.rows("dirty_strokes"),
            2,
            "the ink waits in the write-ahead area"
        );
        assert_eq!(store.rows("chunks"), 0, "and is not a chunk yet");

        cleanup(&path);
    }

    /// Compaction folds the dirty strokes into chunks, and the page reads the same either way.
    #[test]
    fn compaction_folds_the_dirty_strokes_into_chunks() {
        let (mut store, path) = store_for("compact");
        let strokes = vec![stroke(9, 0.0, 0x11_11_11), stroke(7, 40.0, 0x11_11_11)];
        append(&mut store, 0, &strokes).expect("the ink is written");
        let before = store.load(0).expect("the page is read");

        assert_eq!(store.compact(0).expect("the page is compacted"), 2);
        assert_eq!(store.rows("dirty_strokes"), 0, "nothing is left waiting");
        assert_eq!(store.rows("chunks"), 1, "the strokes became one chunk");
        assert_eq!(store.rows("pages"), 1);
        assert_eq!(store.stroke_count(0).expect("a count"), 2);

        let after = store.load(0).expect("the page is read again");
        assert_eq!(first_points(&after), first_points(&before));
        for (before, after) in before.iter().zip(&after) {
            assert_eq!(before.points.len(), after.points.len());
            assert_eq!(before.points[0].x, after.points[0].x);
            assert_eq!(before.color, after.color);
        }

        cleanup(&path);
    }

    /// Ink added after a compaction follows it, because the chunks of a page tile its ink.
    #[test]
    fn a_page_written_in_two_batches_keeps_its_order() {
        let (mut store, path) = store_for("batches");
        append(&mut store, 0, &[stroke(5, 0.0, 0x11_11_11)]).expect("the first batch");
        store.compact(0).expect("the page is closed");

        append(&mut store, 0, &[stroke(5, 60.0, 0x22_22_22)]).expect("the second batch");
        assert_eq!(
            first_points(&store.load(0).expect("a page")),
            vec![0.0, 60.0],
            "the new ink comes after the old"
        );

        assert_eq!(store.compact(0).expect("the page is closed again"), 1);
        assert_eq!(store.rows("chunks"), 2, "two batches, two chunks");
        assert_eq!(
            first_points(&store.load(0).expect("a page")),
            vec![0.0, 60.0]
        );

        cleanup(&path);
    }

    /// A rewrite replaces a page: what undo, the eraser and clear need.
    #[test]
    fn rewriting_a_page_replaces_what_was_there() {
        let (mut store, path) = store_for("rewrite");
        append(
            &mut store,
            0,
            &[
                stroke(5, 0.0, 0x1),
                stroke(5, 10.0, 0x1),
                stroke(5, 20.0, 0x1),
            ],
        )
        .expect("ink");
        store.compact(0).expect("the page is closed");

        rewrite(&mut store, 0, &[stroke(5, 300.0, 0x2)]).expect("the page is rewritten");
        assert_eq!(
            first_points(&store.load(0).expect("a page")),
            vec![300.0],
            "the page is the ink it was given"
        );
        assert_eq!(store.stroke_count(0).expect("a count"), 1);

        rewrite(&mut store, 0, &[]).expect("the page is emptied");
        assert!(store.load(0).expect("a page").is_empty());
        assert_eq!(store.stroke_count(0).expect("a count"), 0);

        cleanup(&path);
    }

    /// Inserting and deleting a page move the ink with it, by position.
    #[test]
    fn a_page_moves_when_one_is_inserted_or_deleted() {
        let (mut store, path) = store_for("pages");
        for (page, x) in [(0u64, 0.0f32), (1, 100.0), (2, 200.0)] {
            append(&mut store, page, &[stroke(4, x, 0x1)]).expect("ink");
        }

        store.insert_page(1).expect("a page is inserted");
        assert_eq!(store.pages().expect("pages"), vec![0, 2, 3]);
        assert_eq!(
            first_points(&store.load(2).expect("a page")),
            vec![100.0],
            "the ink moved to the page it is now on"
        );
        assert!(
            store.load(1).expect("a page").is_empty(),
            "and the new page is blank"
        );

        store.delete_page(1).expect("a page is deleted");
        assert_eq!(store.pages().expect("pages"), vec![0, 1, 2]);
        assert_eq!(first_points(&store.load(1).expect("a page")), vec![100.0]);

        cleanup(&path);
    }

    /// A marked page is marked, and the marks come back in reading order however they were made.
    ///
    /// The order is the one thing a bookmark list cannot be allowed to get wrong: a list in the order
    /// the marks were made would be a list that has to be read rather than used. It is also the
    /// database's answer rather than the app's, so a note carried to another machine lists the same
    /// pages in the same order without anything being stored about it.
    #[test]
    fn the_marked_pages_come_back_in_reading_order() {
        let (mut store, path) = store_for("bookmarks");

        store.set_bookmark(2, true).expect("a mark");
        store.set_bookmark(0, true).expect("a mark");
        store.set_bookmark(1, true).expect("a mark");

        assert_eq!(store.bookmarks().expect("marks"), vec![0, 1, 2]);
        assert!(
            store.pages().expect("pages").is_empty(),
            "a mark is not ink: no page row is made for it"
        );

        // A mark is a row of the note rather than a fact about the session, so it is there when the
        // note is opened again.
        drop(store);
        let reopened = NoteStore::open(&path).expect("the note opens again");
        assert_eq!(reopened.bookmarks().expect("marks"), vec![0, 1, 2]);

        cleanup(&path);
    }

    /// Marking twice is marking once, and taking a mark off a page that has none is nothing.
    ///
    /// The toggle is what the app's button and its keyboard binding both come through, and both can
    /// arrive twice: a second mark on a page is impossible here rather than merely unlikely.
    #[test]
    fn a_page_is_marked_or_it_is_not() {
        let (mut store, path) = store_for("bookmark-once");

        store.set_bookmark(1, true).expect("a mark");
        store.set_bookmark(1, true).expect("the same mark again");
        assert_eq!(store.bookmarks().expect("marks"), vec![1]);

        store.set_bookmark(1, false).expect("the mark goes");
        store
            .set_bookmark(1, false)
            .expect("the mark is already gone");
        assert!(store.bookmarks().expect("marks").is_empty());

        cleanup(&path);
    }

    /// A mark is on a *page*: inserting a page before it moves the mark along with that page.
    ///
    /// This is the whole reason a mark is stored by position and shifted by the same statements
    /// `pages.ord` is: a mark left behind at the number it was made at would open some other page,
    /// which is the one failure a bookmark must not have.
    #[test]
    fn a_mark_moves_when_a_page_is_inserted_before_it() {
        let (mut store, path) = store_for("bookmark-insert");
        store.set_bookmark(0, true).expect("a mark");
        store.set_bookmark(2, true).expect("a mark");

        store.insert_page(1).expect("a page is inserted");
        assert_eq!(
            store.bookmarks().expect("marks"),
            vec![0, 3],
            "the mark that was page 2 is page 3 now, and the mark before the insertion stays"
        );

        // An insertion *at* a mark's position puts the new page in front of the marked page, so the
        // mark follows its page rather than sitting still on a number.
        store.insert_page(0).expect("another page is inserted");
        assert_eq!(store.bookmarks().expect("marks"), vec![1, 4]);

        cleanup(&path);
    }

    /// Deleting a page takes its mark with it, and the marks after it close the gap.
    #[test]
    fn deleting_a_page_takes_its_mark_with_it() {
        let (mut store, path) = store_for("bookmark-delete");
        for page in 0..4 {
            store.set_bookmark(page, true).expect("a mark");
        }

        store.delete_page(1).expect("a page is deleted");
        assert_eq!(
            store.bookmarks().expect("marks"),
            vec![0, 1, 2],
            "the deleted page's mark went with it and the rest closed the gap"
        );

        cleanup(&path);
    }

    /// A note written before the table existed gains it when it is opened.
    ///
    /// The table is added by the same `CREATE TABLE IF NOT EXISTS` batch every open already runs, so a
    /// note that never had it is brought up to date by being opened — no migration, and no version
    /// this build refuses. A note that has the table and is opened by a build that does not know it is
    /// equally fine: the rows are not read, not written, and not deleted by the other build.
    #[test]
    fn an_older_note_gains_the_bookmarks_it_never_had() {
        let (mut store, path) = store_for("bookmark-old");
        store.set_bookmark(3, true).expect("a mark");
        // What the file looks like to the build that wrote it before this table existed.
        store.run("DROP TABLE bookmarks");
        drop(store);

        let mut reopened = NoteStore::open(&path).expect("the note still opens");
        assert!(
            reopened.bookmarks().expect("marks").is_empty(),
            "the table comes back empty rather than as an error"
        );

        reopened
            .set_bookmark(3, true)
            .expect("a mark on the table that came back");
        assert_eq!(reopened.bookmarks().expect("marks"), vec![3]);

        cleanup(&path);
    }

    /// A damaged chunk is reported rather than drawn as noise.
    #[test]
    fn a_damaged_chunk_is_reported() {
        let (mut store, path) = store_for("damaged");
        append(&mut store, 0, &[stroke(40, 0.0, 0x1)]).expect("ink");
        store.compact(0).expect("the page is closed");
        store.damage_a_chunk(0);

        let error = store.load(0).expect_err("a damaged chunk is refused");
        assert!(error.to_string().contains("damaged"), "{error}");

        cleanup(&path);
    }

    /// An export is a note of its own: a complete, standalone file a person can carry away.
    #[test]
    fn an_export_is_a_note_of_its_own() {
        let (mut store, path) = store_for("export");
        append(&mut store, 0, &[stroke(6, 0.0, 0x1)]).expect("ink");
        store.compact(0).expect("the page is closed");

        let target = path.parent().expect("a directory").join("exported.db");
        store.export(&target).expect("the note is exported");

        let exported = NoteStore::open(&target).expect("the export is a note");
        assert_eq!(first_points(&exported.load(0).expect("a page")), vec![0.0]);
        assert_eq!(exported.stroke_count(0).expect("a count"), 1);

        cleanup(&path);
    }

    /// A note remembers what it knows — the page it was on, the sheet, the page list, its name — and
    /// says `None` for everything it has not been told.
    ///
    /// The settings are the other half of that table and have tests of their own, below.
    #[test]
    fn a_note_remembers_its_own_page_list() {
        let (mut store, path) = store_for("meta");

        assert_eq!(store.open_page().expect("a page"), None);
        assert_eq!(store.layout().expect("a layout"), Vec::<PageEntry>::new());
        assert_eq!(store.sheet().expect("a sheet"), None);

        store.set_open_page(4).expect("a page");
        store.set_sheet(Some((794.0, 1123.0))).expect("a sheet");
        store
            .set_layout(&[
                PageEntry::new(Page::Blank),
                PageEntry::new(Page::Document(3)),
            ])
            .expect("a layout");
        store.set_document("chapter-3.pdf").expect("a name");

        assert_eq!(store.open_page().expect("a page"), Some(4));
        assert_eq!(store.sheet().expect("a sheet"), Some((794.0, 1123.0)));
        assert_eq!(
            store.layout().expect("a layout"),
            vec![
                PageEntry::new(Page::Blank),
                PageEntry::new(Page::Document(3))
            ]
        );
        assert_eq!(
            store.document().expect("a name").as_deref(),
            Some("chapter-3.pdf")
        );

        cleanup(&path);
    }

    /// Half a sheet says nothing: the two rows are written together, and read together.
    ///
    /// A note holding a width and no height is a note somebody edited by hand, and the honest answer is
    /// that the note does not say — not a sheet with a height of zero.
    #[test]
    fn half_a_sheet_says_nothing() {
        let (mut store, path) = store_for("sheet-half");

        store.set_sheet(Some((794.0, 1123.0))).expect("a sheet");
        store.set_sheet(None).expect("no sheet");
        assert_eq!(
            store.sheet().expect("a sheet"),
            None,
            "forgetting a sheet takes both rows away"
        );

        put_row(&store.conn, META_SHEET_WIDTH, 794.0).expect("half a sheet");
        assert_eq!(store.sheet().expect("a sheet"), None);

        cleanup(&path);
    }

    /// Two notes keep two different answers, whatever the answers are.
    ///
    /// This is the whole reason there is no global state. The paper, the ruling, the pen, the tuning and
    /// the switches are a *note's*, so opening one must not hand its answers to the next — and the only
    /// way to say that is two notes side by side, each with its own file. Every field is set to something
    /// that is *not* the shipped default, so a setting that is written but never read back fails here.
    #[test]
    fn two_notes_keep_their_own_answers() {
        let (mut ruled, ruled_path) = store_for("settings-ruled");
        let (mut grid, grid_path) = store_for("settings-grid");

        let ruled_settings = Settings {
            ink_color: 0xDC_26_26,
            page_color: 0xF5_F0_E6,
            pen_weight: PenWeight::Fine,
            grayscale_pages: true,
            canvas_size: CanvasSize::A5,
            canvas_style: CanvasStyle::Ruled,
            page_display_width: 640.0,
            zoom: 1.5,
            resample_spacing: 0.5,
            smoothing_ms: 4.0,
            min_width: 0.8,
            max_width: 3.2,
            no_pressure_width: 1.6,
            erase_radius: 9.0,
            export_dir: Some(PathBuf::from("C:/carry")),
            show_toolbar: false,
            show_status: false,
            show_tilt_cursor: false,
        };
        let grid_settings = Settings {
            ink_color: 0x1D_4E_D8,
            page_color: 0x20_20_24,
            pen_weight: PenWeight::Heavy,
            grayscale_pages: false,
            canvas_size: CanvasSize::Square,
            canvas_style: CanvasStyle::Grid,
            page_display_width: 720.0,
            zoom: 2.0,
            resample_spacing: 1.25,
            smoothing_ms: 0.0,
            min_width: 2.0,
            max_width: 9.0,
            no_pressure_width: 4.0,
            erase_radius: 20.0,
            export_dir: None,
            show_toolbar: true,
            show_status: true,
            show_tilt_cursor: true,
        };

        ruled.set_settings(&ruled_settings).expect("settings");
        grid.set_settings(&grid_settings).expect("settings");

        // Read into a *default* set, so anything the note fails to say comes back as the shipped answer
        // and the comparison below can see it.
        let mut read = Settings::default();
        ruled.read_settings(&mut read).expect("the ruled note");
        assert_eq!(read, ruled_settings, "every answer comes back");

        read = Settings::default();
        grid.read_settings(&mut read).expect("the grid note");
        assert_eq!(read, grid_settings);

        assert_ne!(ruled_settings, grid_settings, "the two notes differ");
        assert_ne!(
            ruled_settings,
            Settings::default(),
            "and neither fixture is the shipped set, so nothing here passes by accident"
        );
        assert_ne!(grid_settings, Settings::default());

        cleanup(&ruled_path);
        cleanup(&grid_path);
    }

    /// A row this build does not know is left exactly as it was found.
    ///
    /// The whole point of one row per setting: a note written by a build that knew more settings, or one
    /// somebody added a row to by hand, keeps them. Nothing here writes a row it does not read and
    /// nothing deletes one, so a newer build's answers survive a note passing through this one.
    #[test]
    fn a_row_this_build_does_not_know_is_left_alone() {
        let (mut store, path) = store_for("row-unknown");

        put_row(&store.conn, "brushes_from_the_future", "42").expect("a row");
        put_row(&store.conn, "another", "a word").expect("a row");

        store.set_settings(&Settings::default()).expect("settings");
        let mut read = Settings::default();
        store.read_settings(&mut read).expect("settings");

        assert_eq!(
            row_text(&store.conn, "brushes_from_the_future")
                .expect("a row")
                .as_deref(),
            Some("42")
        );
        assert_eq!(
            row_text(&store.conn, "another").expect("a row").as_deref(),
            Some("a word")
        );

        cleanup(&path);
    }

    /// A row that is there and cannot be understood is reported, not replaced.
    ///
    /// Something wrote it, and quietly answering with the shipped default is how a person's setting
    /// disappears without a word — which is the one failure this whole design exists to avoid.
    #[test]
    fn a_row_that_is_not_a_number_is_reported() {
        let (store, path) = store_for("row-broken");

        put_row(&store.conn, "max_width", "as wide as you like").expect("a row");

        let mut read = Settings::default();
        let error = store
            .read_settings(&mut read)
            .expect_err("a broken row is reported");

        assert!(error.to_string().contains("max_width"), "{error}");

        cleanup(&path);
    }

    /// A row that is *not there* is not a failure: the answer in hand stands.
    ///
    /// This is what makes a note written by a build that knew fewer settings open with the shipped
    /// answers rather than with nothing, and what lets a PDF just placed keep the sheet it was placed on.
    #[test]
    fn a_missing_row_keeps_the_answer_in_hand() {
        let (store, path) = store_for("row-missing");

        let mut read = Settings {
            max_width: 7.25,
            zoom: 3.0,
            ..Settings::default()
        };
        store
            .read_settings(&mut read)
            .expect("a note that says nothing");

        assert_eq!(read.max_width, 7.25, "the setting in hand is kept");
        assert_eq!(read.zoom, 3.0);
        assert_eq!(read.ink_color, Settings::default().ink_color);

        cleanup(&path);
    }

    /// A note can be given a name, and the name is the row's word for it — never the folder's.
    #[test]
    fn a_note_can_be_named() {
        let (mut store, path) = store_for("title");

        assert_eq!(
            store.facts().expect("the facts").title,
            None,
            "a new note has no name of its own, and the list derives one"
        );

        store
            .set_title("3\u{c7a5} \u{c694}\u{c57d}")
            .expect("a name");
        assert_eq!(
            store.title().expect("a name").as_deref(),
            Some("3\u{c7a5} \u{c694}\u{c57d}"),
            "a name is written exactly as it is given"
        );

        // The rule is forgiving rather than a validation: what arrives *wrong* is made into the name
        // a person meant.
        store
            .set_title("  two\nlines  and   spaces \n")
            .expect("a name");
        assert_eq!(
            store.title().expect("a name").as_deref(),
            Some("two lines and spaces"),
            "one line, trimmed, and no runs of whitespace"
        );

        let long = "\u{ac00}".repeat(TITLE_MAX + 20);
        store.set_title(&long).expect("a name");
        let cut = store.title().expect("a name").expect("a name");
        assert_eq!(
            cut.chars().count(),
            TITLE_MAX,
            "cut on characters, not bytes: a Korean name is three bytes a syllable"
        );
        assert!(long.starts_with(&cut), "and cut from the end");

        // Taking the name away is a legitimate answer rather than an error: the list derives one.
        store.set_title("   \n  ").expect("no name");
        assert_eq!(store.title().expect("no name"), None);

        cleanup(&path);
    }

    /// A note says what it holds without reading any of its ink: the counts, the pages it lists, and
    /// the sheet, and where it came from.
    #[test]
    fn a_note_summarises_itself() {
        let (mut store, path) = store_for("summary");

        assert_eq!(
            store.facts().expect("the facts").summary,
            Summary {
                pages: 1,
                strokes: 0,
                sheet: None
            },
            "a new note is one blank page with nothing on it"
        );

        // Ink that has not been compacted counts too: a note read the moment it is written in must
        // not look emptier than it is.
        append(&mut store, 0, &[stroke(4, 0.0, 0), stroke(4, 40.0, 0)]).expect("ink");
        append(&mut store, 1, &[stroke(4, 80.0, 0)]).expect("ink");

        let dirty = store.facts().expect("the facts").summary;
        assert_eq!(dirty.strokes, 3, "dirty rows are strokes as well");
        assert_eq!(dirty.pages, 2, "two pages hold ink");

        // A note's own page list is what a reader can turn to, and it is what a summary reports.
        store
            .set_layout(&[
                PageEntry::new(Page::Blank),
                PageEntry::new(Page::Blank),
                PageEntry::new(Page::Document(0)),
                PageEntry::new(Page::Blank),
            ])
            .expect("a list");
        store.set_sheet(Some((794.0, 1123.0))).expect("a sheet");
        store.compact(0).expect("a compaction");

        let listed = store.facts().expect("the facts").summary;
        assert_eq!(
            listed.pages, 4,
            "the list, not the pages that happen to hold ink"
        );
        assert_eq!(
            listed.strokes, 3,
            "compaction moved the ink, it did not lose it"
        );
        assert_eq!(listed.sheet, Some((794.0, 1123.0)));

        cleanup(&path);
    }

    /// A note remembers the file it was placed from, and says so when there was none.
    #[test]
    fn a_note_remembers_the_file_it_came_from() {
        let (mut store, path) = store_for("source");

        assert_eq!(store.source().expect("a source"), None);

        store
            .set_source(Some(Path::new("C:/docs/chapter-3.pdf")))
            .expect("a source");
        assert_eq!(
            store.source().expect("a source").as_deref(),
            Some("C:/docs/chapter-3.pdf")
        );

        store.set_source(None).expect("no source");
        assert_eq!(
            store.source().expect("a source"),
            None,
            "a blank sheet's note came from nowhere, and says so rather than naming an empty path"
        );

        cleanup(&path);
    }

    /// A note written by a newer build is refused rather than read as if it were this one.
    #[test]
    fn a_newer_note_is_refused() {
        let (store, path) = store_for("newer");
        store.run(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1));
        drop(store);

        let error = NoteStore::open(&path).expect_err("a newer note is refused");
        assert!(error.to_string().contains("newer version"), "{error}");

        cleanup(&path);
    }

    /// A note from a build with *another* schema is refused in both directions, and untouched.
    ///
    /// There is no migration (see [`SCHEMA_VERSION`]), so a note from an older build is refused exactly
    /// as one from a newer build is. The stamp is asserted as well, because "refused" has to mean
    /// *left alone*: a note that was refused and stamped on the way out would be a note the build that
    /// wrote it could no longer open.
    #[test]
    fn an_older_note_is_refused() {
        let (store, path) = store_for("older");
        store.run(&format!("PRAGMA user_version = {};", SCHEMA_VERSION - 1));
        drop(store);

        let error = NoteStore::open(&path).expect_err("an older note is refused");
        assert!(error.to_string().contains("older version"), "{error}");

        let stamp: i32 = Connection::open(&path)
            .expect("the file is still there")
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("a version");
        assert_eq!(
            stamp,
            SCHEMA_VERSION - 1,
            "a refused note is left exactly as it was found"
        );

        cleanup(&path);
    }

    /// The write-ahead log is folded back into the file, so a note does not sit beside a log of
    /// itself, and the ink waiting in it is still there afterwards.
    #[test]
    fn checkpointing_folds_the_log_back() {
        let (mut store, path) = store_for("checkpoint");
        append(&mut store, 0, &[stroke(20, 0.0, 0x1)]).expect("ink");

        store.checkpoint().expect("the note is tidied");

        let log = path.with_file_name("note.db-wal");
        let size = std::fs::metadata(&log).map(|meta| meta.len()).unwrap_or(0);
        assert!(size < 4096, "the log was truncated, it is {size} bytes");
        assert_eq!(store.rows("dirty_strokes"), 1, "and the ink is still there");
        assert_eq!(first_points(&store.load(0).expect("a page")), vec![0.0]);

        cleanup(&path);
    }

    /// A page left with nothing on it keeps no row: "the pages that hold ink" is a query, not a
    /// column to keep in step.
    #[test]
    fn an_emptied_page_stops_counting_as_a_page() {
        let (mut store, path) = store_for("empty");
        append(&mut store, 3, &[stroke(4, 0.0, 0x1)]).expect("ink");
        assert_eq!(store.pages().expect("pages"), vec![3]);

        rewrite(&mut store, 3, &[]).expect("the page is emptied");
        assert_eq!(store.stroke_count(3).expect("a count"), 0);
        assert_eq!(
            store.pages().expect("pages"),
            vec![3],
            "the row stays until the page is deleted, but holds nothing"
        );

        cleanup(&path);
    }

    /// An edit that wrote a stroke, for the tests about a page's history.
    fn written(at: usize, stroke: &Stroke) -> Edit {
        Edit::Written {
            at,
            strokes: vec![std::sync::Arc::new(stroke.clone())],
        }
    }

    /// A page's history is written with its ink and read back with it: what a session's undo hangs off.
    #[test]
    fn a_page_carries_its_history() {
        let (mut store, path) = store_for("history");
        let strokes = vec![stroke(4, 0.0, 0x1), stroke(4, 40.0, 0x1)];

        store
            .append(0, &strokes, &written_history(&strokes))
            .expect("ink");

        let page = store.load_page(0, 16).expect("a page");
        assert_eq!(page.strokes.len(), 2, "the ink is there");
        assert_eq!(page.history.depth(), 2, "and the edits came with it");
        assert_eq!(page.history.applied(), 2, "with the cursor they end at");
        assert!(page.history.can_undo() && !page.history.can_redo());

        cleanup(&path);
    }

    /// An undo survives a restart, which is the whole point of writing a history down.
    #[test]
    fn an_undo_survives_a_restart() {
        let (mut store, path) = store_for("restart");
        let strokes = vec![stroke(4, 0.0, 0x1), stroke(4, 40.0, 0x1)];
        store
            .append(0, &strokes, &written_history(&strokes))
            .expect("ink");

        // The reader takes the second one back: the page is written again with one stroke, and the cursor moves back
        // with it — no appended edits, one fewer applied.
        store
            .rewrite(
                0,
                &strokes[..1],
                &HistoryUpdate {
                    appended: Vec::new(),
                    applied: 1,
                    count: 1,
                },
            )
            .expect("the page taken back");

        // A new session opens the note.
        drop(store);
        let reopened = NoteStore::open(&path).expect("the note opens again");
        let page = reopened.load_page(0, 16).expect("a page");

        assert_eq!(page.strokes.len(), 1, "the ink is what was left");
        assert_eq!(page.history.applied(), 1, "the cursor is where it was left");
        assert_eq!(page.history.depth(), 1, "the edit still applied is here");
        assert_eq!(page.history.forward(), 1, "and the one taken back is here");
        assert!(page.history.can_redo(), "so it can be put forward again");

        cleanup(&path);
    }

    /// A log that no longer describes its page is ignored, and the ink is still read in full.
    #[test]
    fn a_log_that_disagrees_with_the_ink_is_ignored() {
        let (mut store, path) = store_for("stale");
        let strokes = vec![stroke(4, 0.0, 0x1)];

        // A cursor that claims a stroke count the page does not have: what a page written by something that kept no
        // history looks like, and the one thing a log cannot survive.
        let wrong = HistoryUpdate {
            appended: vec![written(0, &strokes[0])],
            applied: 1,
            count: 7,
        };
        store.append(0, &strokes, &wrong).expect("ink");

        let page = store.load_page(0, 16).expect("a page");
        assert_eq!(page.strokes.len(), 1, "the ink is read");
        assert_eq!(page.history.depth(), 0, "and the log is not trusted");
        assert!(!page.history.can_undo(), "so there is nothing to take back");

        cleanup(&path);
    }

    /// The history of a page whose strokes were written one after another, in order.
    fn written_history(strokes: &[Stroke]) -> HistoryUpdate {
        HistoryUpdate {
            appended: strokes
                .iter()
                .enumerate()
                .map(|(at, stroke)| written(at, stroke))
                .collect(),
            applied: strokes.len() as u64,
            count: strokes.len(),
        }
    }

    /// An edit made over an undone one takes its place in the log, so the branch cannot be redone.
    #[test]
    fn an_edit_over_an_undone_one_takes_its_place() {
        let (mut store, path) = store_for("branch");
        let strokes = vec![stroke(4, 0.0, 0x1), stroke(4, 40.0, 0x1)];
        store
            .append(0, &strokes, &written_history(&strokes))
            .expect("ink");

        // The cursor walks back to the first stroke: the second becomes the redo branch.
        store
            .rewrite(
                0,
                &strokes[..1],
                &HistoryUpdate {
                    appended: Vec::new(),
                    applied: 1,
                    count: 1,
                },
            )
            .expect("taken back");

        // A *different* stroke is drawn over it. The tail replaces everything the note holds from its own first
        // ordinal on, which is what makes the branch the reader walked away from go.
        let drawn = stroke(4, 100.0, 0x2);
        store
            .append(
                0,
                std::slice::from_ref(&drawn),
                &HistoryUpdate {
                    appended: vec![written(1, &drawn)],
                    applied: 2,
                    count: 2,
                },
            )
            .expect("the new ink");

        let page = store.load_page(0, 16).expect("a page");
        assert_eq!(page.history.applied(), 2);
        assert_eq!(page.history.depth(), 2, "the drawn stroke replaced it");
        assert_eq!(page.history.forward(), 0, "and the branch is gone");
        assert_eq!(
            store.history_rows(0).expect("a count"),
            2,
            "the rows agree with the cursor"
        );

        cleanup(&path);
    }

    /// A page's history can be read one edit at a time, older than what memory already holds: what makes an undo
    /// possible days after the edits were made.
    #[test]
    fn a_history_reaches_deeper_than_memory() {
        let (mut store, path) = store_for("deeper");
        let strokes = vec![
            stroke(4, 0.0, 0x1),
            stroke(4, 40.0, 0x1),
            stroke(4, 80.0, 0x1),
        ];
        store
            .append(0, &strokes, &written_history(&strokes))
            .expect("ink");

        // A session that keeps only the newest edit in memory, which is what the app does when a note is opened:
        // the rest are still in the note, and are read one at a time as an undo asks for them.
        let page = store.load_page(0, 1).expect("a page");
        assert_eq!(page.history.depth(), 1, "one edit in memory");
        assert_eq!(page.history.in_note(), 1, "and it is one the note holds");

        // Each step deeper reads its own row, and the page never held more than three: the walk ends where the ink
        // does, which is what makes an undo stop rather than read something that was never there.
        let older = store
            .older_history(0, 1)
            .expect("the log is read")
            .expect("the second edit");
        let further = store
            .older_history(0, 2)
            .expect("the log is read")
            .expect("the third edit");

        assert_ne!(
            older.encode().expect("it encodes"),
            further.encode().expect("it encodes"),
            "each step reads the row before the last"
        );
        assert!(
            store
                .older_history(0, 3)
                .expect("the log is read")
                .is_none(),
            "and past the page's own ink there is nothing"
        );

        cleanup(&path);
    }
    #[test]
    fn the_history_is_cut_back_to_a_depth() {
        let (store, path) = store_for("trim");

        // A page that has been written to more times than the depth allows, put there straight: this is about the
        // housekeeping, not about drawing four thousand lines.
        store.run(&format!(
            "INSERT INTO pages (ord, created_at, updated_at, stroke_count, history_at, history_count)
             VALUES (0, 0, 0, 1, {}, 1)",
            HISTORY_DEPTH + 10
        ));
        let values: Vec<String> = (1..=(HISTORY_DEPTH + 10))
            .map(|ord| format!("(1, {ord}, x'00', 0)"))
            .collect();
        store.run(&format!(
            "INSERT INTO edits (page_id, ord, data, created_at) VALUES {}",
            values.join(", ")
        ));
        assert_eq!(store.rows("edits"), HISTORY_DEPTH + 10);

        store.checkpoint().expect("the idle housekeeping");

        assert_eq!(store.rows("edits"), HISTORY_DEPTH, "a depth is a depth");
        assert_eq!(
            store.pragma_number("SELECT MIN(ord) FROM edits"),
            11,
            "and it is the *oldest* edits that went"
        );

        cleanup(&path);
    }
}
