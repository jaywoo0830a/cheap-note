//! The note store: one SQLite file per note, holding the ink and what the note is.
//!
//! ## Why a database replaced a zip of JSON
//!
//! The first version wrote a note as a zip with `notes.json` in it: the whole note re-serialised,
//! re-deflated and rewritten on every save. That is fine for a page and hopeless for a notebook —
//! a save is O(everything), a crash in the middle leaves a half-written file, and nothing is
//! incremental. `BUNDLE.md` is the design note this module implements: **SQLite as the container**,
//! with the strokes as compressed BLOBs rather than as rows of text.
//!
//! What that buys, in the terms the design note uses:
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
//! As specified in the design note, plus a `meta` table (the note's page list and what belongs to
//! the note rather than to the app — see [`META_LAYOUT`]) and an ordering column on `pages`:
//!
//! ```sql
//! pages(id, ord, created_at, updated_at, bbox_*, stroke_count)
//! chunks(id, page_id, seq, stroke_start, stroke_count, codec, raw_len, data, crc32)
//! dirty_strokes(id, page_id, seq, data, created_at)
//! meta(key, value)
//! ```
//!
//! **Why `pages` has both `id` and `ord`.** `id` is an identity that never changes — it is what
//! `chunks.page_id` points at, so it has to be stable — while `ord` is the page's position in the
//! note's reading order, which *does* change: inserting a page renames every page after it. Keeping
//! the two apart means an insert is two small `UPDATE`s on `pages` rather than a rewrite of every
//! chunk in the note. The shift goes through an offset because `ord` is unique, and a single
//! `ord = ord + 1` would collide with the row it is about to move.
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

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use crate::chunk::{self, Codec};
use crate::error::{AppError, Result};
use crate::ink::Stroke;

/// The schema version this build writes, in `PRAGMA user_version`.
///
/// A *newer* file is refused rather than read: the tables it added are ones this build does not
/// know, and guessing is how a note gets rewritten without the ink that was in them. An older one
/// is read as it stands — the format has not changed yet, and when it does the migration belongs
/// here, keyed on this number.
pub const SCHEMA_VERSION: i32 = 3;

/// The page the note was last on, as a `u64` little-endian.
pub const META_OPEN_PAGE: &str = "open_page";
/// The sheet the ink was written on, as two `f32`s little-endian.
pub const META_SHEET: &str = "sheet";
/// What each page shows, in reading order, as `postcard`-encoded [`crate::pages::Page`]s.
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

    /// Refuses a file written by a build that knows more than this one.
    fn check_version(&self, path: &Path) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| {
                AppError::Note(format!("{} is not a note: {error}", path.display()))
            })?;

        if version > SCHEMA_VERSION {
            return Err(AppError::Note(format!(
                "{} was written by a newer version of this app (note schema {version}, this build reads {SCHEMA_VERSION})",
                path.display()
            )));
        }

        Ok(())
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
                .map_err(|error| AppError::Note(format!("the note could not be stamped: {error}")))?;
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
    id           INTEGER PRIMARY KEY,
    ord          INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    bbox_min_x   REAL,
    bbox_min_y   REAL,
    bbox_max_x   REAL,
    bbox_max_y   REAL,
    stroke_count INTEGER NOT NULL DEFAULT 0
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
    value BLOB NOT NULL
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
    pub fn append(&mut self, page: u64, strokes: &[Stroke]) -> Result<()> {
        if strokes.is_empty() {
            return Ok(());
        }

        let tx = self.begin()?;
        let id = page_row(&tx, page, true)?.expect("the row was just made");
        let now = now_millis();
        let mut seq = next_dirty_seq(&tx, id)?;

        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO dirty_strokes (page_id, seq, data, created_at) VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(sql)?;

            for stroke in strokes {
                let blob = sealed_dirty(&chunk::encode(std::slice::from_ref(stroke))?);
                insert
                    .execute(params![id, seq, blob, now])
                    .map_err(sql)?;
                seq += 1;
            }
        }

        tx.execute(
            "UPDATE pages SET updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )
        .map_err(sql)?;

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
    pub fn rewrite(&mut self, page: u64, strokes: &[Stroke]) -> Result<()> {
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

            tx.commit().map_err(sql)?;
            return Ok(());
        }

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

        tx.commit().map_err(sql)?;
        Ok(())
    }
}

/// Starts a transaction, reporting failure the way every other failure here is reported.
impl NoteStore {
    fn begin(&mut self) -> Result<rusqlite::Transaction<'_>> {
        self.conn.transaction().map_err(sql)
    }
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
    pub fn insert_page(&mut self, at: u64) -> Result<()> {
        let tx = self.begin()?;

        tx.execute(
            "UPDATE pages SET ord = ord + ?1 WHERE ord >= ?2",
            params![SHIFT, at as i64],
        )
        .map_err(sql)?;
        tx.execute(
            "UPDATE pages SET ord = ord - ?1 WHERE ord >= ?2",
            params![SHIFT - 1, SHIFT + at as i64],
        )
        .map_err(sql)?;

        tx.commit().map_err(sql)?;
        Ok(())
    }

    /// Removes the page at `at`, and the ink written on it, and closes the gap behind it.
    ///
    /// The ink goes with the page: a deleted page's writing has nowhere to be shown. The cascade on
    /// `chunks` and `dirty_strokes` is what makes that one statement rather than three.
    pub fn delete_page(&mut self, at: u64) -> Result<()> {
        let tx = self.begin()?;

        tx.execute("DELETE FROM pages WHERE ord = ?1", params![at as i64])
            .map_err(sql)?;
        tx.execute(
            "UPDATE pages SET ord = ord + ?1 WHERE ord > ?2",
            params![SHIFT, at as i64],
        )
        .map_err(sql)?;
        tx.execute(
            "UPDATE pages SET ord = ord - ?1 WHERE ord >= ?2",
            params![SHIFT + 1, SHIFT + at as i64],
        )
        .map_err(sql)?;

        tx.commit().map_err(sql)?;
        Ok(())
    }

    /// Folds the write-ahead log back into the note file.
    ///
    /// WAL means writes are cheap and reads never wait, at the cost of a `-wal` file growing beside
    /// the note. This truncates it — the design note calls it the idle housekeeping — so a note does
    /// not sit next to a log the size of the note itself until something opens it again.
    pub fn checkpoint(&self) -> Result<()> {
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
    /// A value stored with the note, whatever it is.
    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(sql)
    }

    /// Stores a value with the note.
    pub fn set_meta(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(sql)?;

        Ok(())
    }

    /// Records which page was open, so reopening the note comes back to it.
    pub fn set_open_page(&mut self, page: u64) -> Result<()> {
        self.set_meta(META_OPEN_PAGE, &page.to_le_bytes())
    }

    /// The page that was open, if the note says.
    pub fn open_page(&self) -> Result<Option<u64>> {
        Ok(self
            .meta(META_OPEN_PAGE)?
            .and_then(|bytes| <[u8; 8]>::try_from(&bytes[..]).ok())
            .map(u64::from_le_bytes))
    }

    /// Records the sheet the ink was written on, in logical pixels.
    pub fn set_sheet(&mut self, sheet: Option<(f32, f32)>) -> Result<()> {
        match sheet {
            Some((width, height)) => {
                let mut bytes = Vec::with_capacity(9);
                bytes.push(1);
                bytes.extend_from_slice(&width.to_le_bytes());
                bytes.extend_from_slice(&height.to_le_bytes());
                self.set_meta(META_SHEET, &bytes)
            }
            None => self.set_meta(META_SHEET, &[0]),
        }
    }

    /// The sheet the ink was written on, when the note knows.
    pub fn sheet(&self) -> Result<Option<(f32, f32)>> {
        let Some(bytes) = self.meta(META_SHEET)? else {
            return Ok(None);
        };

        if bytes.len() < 9 || bytes[0] != 1 {
            return Ok(None);
        }

        let width = f32::from_le_bytes(bytes[1..5].try_into().expect("four bytes"));
        let height = f32::from_le_bytes(bytes[5..9].try_into().expect("four bytes"));
        Ok(Some((width, height)))
    }

    /// Records what each page shows, in reading order: the note's own page list.
    pub fn set_layout(&mut self, layout: &[crate::pages::Page]) -> Result<()> {
        let bytes = postcard::to_allocvec(layout).map_err(|error| {
            AppError::Note(format!("the note's page list could not be written: {error}"))
        })?;

        self.set_meta(META_LAYOUT, &bytes)
    }

    /// What each page shows, in reading order. Empty means the note has no list of its own, which is
    /// a note written before there was one — see [`crate::pages::Pages::restore`].
    pub fn layout(&self) -> Result<Vec<crate::pages::Page>> {
        let Some(bytes) = self.meta(META_LAYOUT)? else {
            return Ok(Vec::new());
        };

        postcard::from_bytes(&bytes).map_err(|error| {
            AppError::Note(format!("the note's page list could not be read: {error}"))
        })
    }

    /// Records the name the document had, so a note stays about the file it was made from.
    pub fn set_document(&mut self, name: &str) -> Result<()> {
        self.set_meta(META_DOCUMENT, name.as_bytes())
    }

    /// The name the document had, if the note has one.
    pub fn document(&self) -> Result<Option<String>> {
        Ok(self
            .meta(META_DOCUMENT)?
            .and_then(|bytes| String::from_utf8(bytes).ok()))
    }

    /// Records the file the note was placed from, or that there was none.
    pub fn set_source(&mut self, path: Option<&Path>) -> Result<()> {
        match path {
            Some(path) => self.set_meta(META_SOURCE, path.to_string_lossy().as_bytes()),
            None => self.set_meta(META_SOURCE, &[]),
        }
    }

    /// The file the note was placed from, if it remembers one.
    pub fn source(&self) -> Result<Option<String>> {
        Ok(self
            .meta(META_SOURCE)?
            .filter(|bytes| !bytes.is_empty())
            .and_then(|bytes| String::from_utf8(bytes).ok()))
    }

    /// Gives the note a name, or takes its name away.
    ///
    /// An empty name is a legitimate answer, and means *no name*: the list derives one, which is what
    /// most notes do. What is written is [`normalize_title`]'s answer, so a name that arrives from a
    /// paste on two lines or is longer than a row becomes the name a person meant rather than an
    /// error.
    pub fn set_title(&mut self, name: &str) -> Result<()> {
        self.set_meta(META_TITLE, normalize_title(name).as_bytes())
    }

    /// The name a person gave the note, if they gave it one.
    pub fn title(&self) -> Result<Option<String>> {
        Ok(self
            .meta(META_TITLE)?
            .filter(|bytes| !bytes.is_empty())
            .and_then(|bytes| String::from_utf8(bytes).ok()))
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

/// A dirty stroke's blob: the encoded chunk with the three columns it needs attached.
///
/// `dirty_strokes` has one column for the ink — the design note's schema — while a chunk in the
/// `chunks` table describes itself with `codec`, `raw_len` and `crc32`. A dirty row therefore carries
/// those three values in a nine-byte header of its own, so one blob is still self-contained and the
/// schema is still the one the design note gives.
fn sealed_dirty(encoded: &chunk::Encoded) -> Vec<u8> {
    let mut blob = Vec::with_capacity(9 + encoded.data.len());
    blob.push(encoded.codec.id() as u8);
    blob.extend_from_slice(&encoded.raw_len.to_le_bytes());
    blob.extend_from_slice(&encoded.crc32.to_le_bytes());
    blob.extend_from_slice(&encoded.data);
    blob
}

/// The one stroke in a dirty row's blob.
fn open_dirty(blob: &[u8]) -> Result<Vec<Stroke>> {
    if blob.len() < 9 {
        return Err(AppError::Note(String::from(
            "a stroke of this note is stored in fewer bytes than its own header",
        )));
    }

    let codec = Codec::from_id(u32::from(blob[0]))?;
    let raw_len = u32::from_le_bytes(blob[1..5].try_into().expect("four bytes")) as usize;
    let crc32 = u32::from_le_bytes(blob[5..9].try_into().expect("four bytes"));

    chunk::decode(&blob[9..], raw_len, codec, 1, crc32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::InkPoint;
    use crate::pages::Page;
    use std::path::PathBuf;

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
        store.append(0, &strokes).expect("the ink is written");

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
        store.append(0, &strokes).expect("the ink is written");
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
        store.append(0, &[stroke(5, 0.0, 0x11_11_11)]).expect("the first batch");
        store.compact(0).expect("the page is closed");

        store
            .append(0, &[stroke(5, 60.0, 0x22_22_22)])
            .expect("the second batch");
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
        store
            .append(
                0,
                &[stroke(5, 0.0, 0x1), stroke(5, 10.0, 0x1), stroke(5, 20.0, 0x1)],
            )
            .expect("ink");
        store.compact(0).expect("the page is closed");

        store
            .rewrite(0, &[stroke(5, 300.0, 0x2)])
            .expect("the page is rewritten");
        assert_eq!(
            first_points(&store.load(0).expect("a page")),
            vec![300.0],
            "the page is the ink it was given"
        );
        assert_eq!(store.stroke_count(0).expect("a count"), 1);

        store.rewrite(0, &[]).expect("the page is emptied");
        assert!(store.load(0).expect("a page").is_empty());
        assert_eq!(store.stroke_count(0).expect("a count"), 0);

        cleanup(&path);
    }

    /// Inserting and deleting a page move the ink with it, by position.
    #[test]
    fn a_page_moves_when_one_is_inserted_or_deleted() {
        let (mut store, path) = store_for("pages");
        for (page, x) in [(0u64, 0.0f32), (1, 100.0), (2, 200.0)] {
            store.append(page, &[stroke(4, x, 0x1)]).expect("ink");
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

    /// A damaged chunk is reported rather than drawn as noise.
    #[test]
    fn a_damaged_chunk_is_reported() {
        let (mut store, path) = store_for("damaged");
        store.append(0, &[stroke(40, 0.0, 0x1)]).expect("ink");
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
        store.append(0, &[stroke(6, 0.0, 0x1)]).expect("ink");
        store.compact(0).expect("the page is closed");

        let target = path.parent().expect("a directory").join("exported.db");
        store.export(&target).expect("the note is exported");

        let exported = NoteStore::open(&target).expect("the export is a note");
        assert_eq!(first_points(&exported.load(0).expect("a page")), vec![0.0]);
        assert_eq!(exported.stroke_count(0).expect("a count"), 1);

        cleanup(&path);
    }

    /// A note remembers what belongs to the note: the page it was on, the sheet, the page list.
    #[test]
    fn a_note_remembers_its_own_page_list() {
        let (mut store, path) = store_for("meta");

        assert_eq!(store.open_page().expect("a page"), None);
        assert_eq!(store.layout().expect("a layout"), Vec::<Page>::new());
        assert_eq!(store.sheet().expect("a sheet"), None);

        store.set_open_page(4).expect("a page");
        store.set_sheet(Some((794.0, 1123.0))).expect("a sheet");
        store
            .set_layout(&[Page::Blank, Page::Document(3)])
            .expect("a layout");
        store.set_document("chapter-3.pdf").expect("a name");

        assert_eq!(store.open_page().expect("a page"), Some(4));
        assert_eq!(store.sheet().expect("a sheet"), Some((794.0, 1123.0)));
        assert_eq!(
            store.layout().expect("a layout"),
            vec![Page::Blank, Page::Document(3)]
        );
        assert_eq!(
            store.document().expect("a name").as_deref(),
            Some("chapter-3.pdf")
        );

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

        store.set_title("3\u{c7a5} \u{c694}\u{c57d}").expect("a name");
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
        store
            .append(0, &[stroke(4, 0.0, 0), stroke(4, 40.0, 0)])
            .expect("ink");
        store.append(1, &[stroke(4, 80.0, 0)]).expect("ink");

        let dirty = store.facts().expect("the facts").summary;
        assert_eq!(dirty.strokes, 3, "dirty rows are strokes as well");
        assert_eq!(dirty.pages, 2, "two pages hold ink");

        // A note's own page list is what a reader can turn to, and it is what a summary reports.
        store
            .set_layout(&[Page::Blank, Page::Blank, Page::Document(0), Page::Blank])
            .expect("a list");
        store.set_sheet(Some((794.0, 1123.0))).expect("a sheet");
        store.compact(0).expect("a compaction");

        let listed = store.facts().expect("the facts").summary;
        assert_eq!(listed.pages, 4, "the list, not the pages that happen to hold ink");
        assert_eq!(listed.strokes, 3, "compaction moved the ink, it did not lose it");
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

    /// The write-ahead log is folded back into the file, so a note does not sit beside a log of
    /// itself, and the ink waiting in it is still there afterwards.
    #[test]
    fn checkpointing_folds_the_log_back() {
        let (mut store, path) = store_for("checkpoint");
        store.append(0, &[stroke(20, 0.0, 0x1)]).expect("ink");

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
        store.append(3, &[stroke(4, 0.0, 0x1)]).expect("ink");
        assert_eq!(store.pages().expect("pages"), vec![3]);

        store.rewrite(3, &[]).expect("the page is emptied");
        assert_eq!(store.stroke_count(3).expect("a count"), 0);
        assert_eq!(
            store.pages().expect("pages"),
            vec![3],
            "the row stays until the page is deleted, but holds nothing"
        );

        cleanup(&path);
    }
}
