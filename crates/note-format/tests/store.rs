//! A note's database, from the outside: the schema a second implementation would find, the ink's round
//! trip, and what happens to a page that is inserted, deleted or damaged.
//!
//! This file opens `note.db` with SQLite itself rather than with this crate wherever it is asking about
//! the *file* — the schema version, the pragmas, the shape of a `chunks` row — because that is what
//! another application has to read. It then uses `NoteStore` for the behaviour, which is the contract
//! both applications owe a person: write ink, get the same ink back.

use std::path::{Path, PathBuf};

use note_format::history::{Edit, HistoryUpdate};
use note_format::ink::Stroke;
use note_format::pages::{Page, PageEntry};
use note_format::settings::{PenWeight, Settings};
use note_format::store::{NoteStore, SCHEMA_VERSION};

mod common;

use common::{scratch, stroke, update, written};

/// A note file in a fresh directory, and the directory's own path.
fn note(name: &str) -> (NoteStore, PathBuf) {
    let dir = scratch(name);
    let path = dir.join("note.db");
    let store = NoteStore::open(&path).expect("a note");
    (store, path)
}

/// A second connection to the same file, opened with SQLite rather than with this crate.
///
/// WAL is what makes this safe while a `NoteStore` is open: a reader sees the last commit and never
/// waits for a writer (see `src/store.rs`).
fn sql(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).expect("a connection to the note file")
}

/// A page of ordinary writing: `count` strokes across and down.
fn ink(count: usize) -> Vec<Stroke> {
    (0..count)
        .map(|index| {
            stroke(
                4,
                10.0 + (index % 10) as f32 * 20.0,
                10.0 + (index / 10) as f32 * 20.0,
                0x1B_1B_1F,
            )
        })
        .collect()
}

/// What a plain ink write tells the note: no history, and this many strokes.
fn plain(count: usize) -> HistoryUpdate {
    update(Vec::new(), 0, count)
}

/// A new note is an ordinary SQLite file, and this build's schema is what it holds.
///
/// The version and the pragmas are part of the format rather than of the implementation: the version is
/// how a build says which shape its tables are in, and the page size and WAL mode are properties of the
/// file that a second application inherits the moment it opens one.
#[test]
fn a_new_note_is_a_sqlite_file_this_build_stamps() {
    let (store, path) = note("schema");
    drop(store);

    let conn = sql(&path);

    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("a schema version");
    assert_eq!(version, i64::from(SCHEMA_VERSION));

    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("a page size");
    assert_eq!(page_size, 8192, "the size the format was designed around");

    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("a journal mode");
    assert_eq!(journal.to_ascii_lowercase(), "wal");

    for table in [
        "pages",
        "chunks",
        "dirty_strokes",
        "meta",
        "bookmarks",
        "edits",
        "page_strokes",
    ] {
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("a look at the schema");
        assert_eq!(found, 1, "the note has no {table}");
    }
}

/// Ink written as a batch is read back after the note is closed and opened again.
#[test]
fn a_batch_of_ink_survives_being_closed_and_opened() {
    let (mut store, path) = note("roundtrip");
    let written = ink(24);

    store
        .append(0, &written, &plain(written.len()))
        .expect("ink");
    assert_eq!(store.stroke_count(0).expect("a count"), 24);

    drop(store);

    let store = NoteStore::open(&path).expect("the note again");
    let read = store.load(0).expect("the page");

    assert_eq!(read.len(), written.len());
    for (index, (before, after)) in written.iter().zip(&read).enumerate() {
        assert_eq!(after.points, before.points, "stroke {index}");
        assert_eq!(after.color, before.color, "stroke {index}");
    }
}

/// Closing a page folds its rows into chunks, and the page reads back exactly the same.
///
/// This is the one moment where a page's ink changes *shape* on disk — rows of single strokes become
/// compressed structure-of-arrays blobs — so it is the one place a bug could reorder or lose a line.
#[test]
fn a_compacted_page_holds_the_same_ink() {
    let (mut store, path) = note("compact");
    let written = ink(40);

    store
        .append(0, &written, &plain(written.len()))
        .expect("ink");
    let folded = store.compact(0).expect("the page folded");
    assert_eq!(folded, written.len(), "every stroke went into a chunk");

    // Nothing is appended twice and nothing is left behind: the rows are gone, the chunks are there.
    let conn = sql(&path);
    let dirty: i64 = conn
        .query_row("SELECT COUNT(*) FROM dirty_strokes", [], |row| row.get(0))
        .expect("a count of rows");
    let chunks: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
        .expect("a count of chunks");
    assert_eq!(dirty, 0, "the page's rows were folded, not copied");
    assert!(chunks >= 1, "and the ink is in chunks");

    let read = store.load(0).expect("the page");
    assert_eq!(read.len(), written.len());
    for (index, (before, after)) in written.iter().zip(&read).enumerate() {
        assert_eq!(after.points, before.points, "stroke {index}");
    }

    // Compacting a page that is already compacted is a no-op, not a second copy.
    assert_eq!(store.compact(0).expect("nothing to do"), 0);
    assert_eq!(store.load(0).expect("the page").len(), written.len());
}

/// A page has an identity that never changes and a position that does, and inserting one moves only
/// the positions.
///
/// This is why `pages` has both `id` and `ord`: the ink is keyed by the identity, so an insert is two
/// small `UPDATE`s rather than a rewrite of every chunk in the note.
#[test]
fn inserting_a_page_moves_positions_and_nothing_else() {
    let (mut store, _path) = note("insert");

    let first = ink(3);
    let second = ink(5);
    store.append(0, &first, &plain(first.len())).expect("ink");
    store.append(1, &second, &plain(second.len())).expect("ink");
    assert_eq!(store.pages().expect("the page list"), vec![0, 1]);

    store.insert_page(0).expect("room for a page");

    // The pages that hold ink are now at 1 and 2, and their ink went with them.
    assert_eq!(store.pages().expect("the page list"), vec![1, 2]);
    assert_eq!(store.load(1).expect("the first page").len(), 3);
    assert_eq!(store.load(2).expect("the second page").len(), 5);
    assert!(
        store.load(0).expect("the new page").is_empty(),
        "the page made for the insertion has no ink yet"
    );
}

/// Deleting a page takes its ink and its bookmark with it, and moves what follows down.
#[test]
fn deleting_a_page_takes_its_ink_and_its_mark() {
    let (mut store, _path) = note("delete");

    for page in 0..3u64 {
        let page_ink = ink(page as usize + 1);
        store
            .append(page, &page_ink, &plain(page_ink.len()))
            .expect("ink");
    }

    store.set_bookmark(1, true).expect("a mark");
    store.set_bookmark(2, true).expect("another mark");
    assert_eq!(store.bookmarks().expect("the marks"), vec![1, 2]);

    store.delete_page(1).expect("the page removed");

    assert_eq!(
        store.bookmarks().expect("the marks"),
        vec![1],
        "the mark on the deleted page went with it, and the one after it moved down"
    );
    assert_eq!(store.pages().expect("the page list"), vec![0, 1]);
    assert_eq!(store.load(1).expect("the page that moved").len(), 3);
}

/// Everything a note says about itself is a `meta` row, and it all comes back after a reopen.
#[test]
fn the_facts_a_note_records_about_itself_round_trip() {
    let (mut store, path) = note("facts");
    let page_ink = ink(6);
    store
        .append(0, &page_ink, &plain(page_ink.len()))
        .expect("ink");

    let layout = vec![
        PageEntry::new(Page::Document(0)),
        PageEntry::new(Page::Blank).turned(1),
    ];

    store.set_title("chapter three").expect("a name");
    store.set_document("chapter-3.pdf").expect("a document");
    store
        .set_source(Some(Path::new(r"C:\notes\chapter-3.pdf")))
        .expect("a source");
    store.set_sheet(Some((720.0, 1018.0))).expect("a sheet");
    store.set_open_page(1).expect("a page");
    store.set_layout(&layout).expect("a page list");

    drop(store);
    let store = NoteStore::open(&path).expect("the note again");

    assert_eq!(
        store.title().expect("a name").as_deref(),
        Some("chapter three")
    );
    assert_eq!(
        store.document().expect("a document").as_deref(),
        Some("chapter-3.pdf")
    );
    assert!(
        store
            .source()
            .expect("a source")
            .expect("a source")
            .ends_with("chapter-3.pdf"),
        "the file the note came from"
    );
    assert_eq!(store.sheet().expect("a sheet"), Some((720.0, 1018.0)));
    assert_eq!(store.open_page().expect("a page"), Some(1));
    assert_eq!(store.layout().expect("a page list"), layout);

    let facts = store.facts().expect("the facts");
    assert_eq!(facts.title.as_deref(), Some("chapter three"));
    assert_eq!(facts.document.as_deref(), Some("chapter-3.pdf"));
    assert_eq!(facts.summary.pages, 2);
    assert_eq!(facts.summary.strokes, 6);
    assert_eq!(facts.summary.sheet, Some((720.0, 1018.0)));
}

/// A note's settings are the note's: written as rows, read back as the same struct, and never a
/// separate file.
#[test]
fn the_settings_a_note_keeps_are_its_own() {
    let (mut store, path) = note("settings");

    let settings = Settings {
        ink_color: 0x00_12_34_56,
        highlighter_color: 0x00_FF_E0_66,
        page_color: 0x00_FA_F7_F0,
        pen_weight: PenWeight::Bold,
        canvas_size: note_format::paper::CanvasSize::A5,
        canvas_style: note_format::paper::CanvasStyle::Grid,
        zoom: 1.21,
        zoom_locked: true,
        erase_radius: 13.5,
        show_toolbar: false,
        ..Settings::default()
    };

    store.set_settings(&settings).expect("the settings");

    drop(store);
    let store = NoteStore::open(&path).expect("the note again");

    let mut read = Settings::default();
    store.read_settings(&mut read).expect("the settings back");

    assert_eq!(read, settings, "a note keeps every setting it is given");
}

/// A setting this build no longer keeps is ignored, and left where it is.
///
/// The ghost cursor used to be switchable and is not any more: the pen's marker is the cursor, and a pen
/// with none is not a state anybody chose. A note written by the build that had the switch still holds
/// its row — and the rule the format states for exactly this case is that nothing here reads a row it
/// does not know, and nothing deletes one either. So the row changes no answer, and survives a save.
#[test]
fn a_setting_this_build_no_longer_keeps_is_ignored_and_left_alone() {
    let (store, path) = note("retired-setting");
    drop(store);

    sql(&path)
        .execute(
            "INSERT INTO meta (key, value) VALUES ('show_tilt_cursor', 'false')
             ON CONFLICT(key) DO UPDATE SET value = 'false'",
            [],
        )
        .expect("a row an older build wrote");

    let mut store = NoteStore::open(&path).expect("the note");

    let mut read = Settings::default();
    store.read_settings(&mut read).expect("the settings");
    assert_eq!(
        read,
        Settings::default(),
        "a row this build does not know changed an answer"
    );

    // And writing the whole set back — which is what every setting change does — leaves it be.
    store.set_settings(&read).expect("the settings written");

    let kept: i64 = sql(&path)
        .query_row(
            "SELECT COUNT(*) FROM meta WHERE key = 'show_tilt_cursor'",
            [],
            |row| row.get(0),
        )
        .expect("a look at the rows");
    assert_eq!(kept, 1, "the row an older build knew was deleted");
}

/// A page's history is written with the ink it describes, and read back with it.
///
/// The log is one row per edit, in the order made, and it survives a compaction — the ink changing
/// shape on disk is not the history changing. This is what lets a page reopened days later take its
/// edits back.
#[test]
fn the_history_of_a_page_is_read_back_with_it() {
    let (mut store, path) = note("history");
    let lines = ink(3);

    let edits: Vec<Edit> = lines
        .iter()
        .enumerate()
        .map(|(at, s)| written(at, s))
        .collect();
    let history = update(edits, lines.len() as u64, lines.len());
    store.append(0, &lines, &history).expect("ink");

    let page = store.load_page(0, 16).expect("the page");
    assert_eq!(page.strokes.len(), 3);
    assert_eq!(page.history.depth(), 3, "three edits came back");
    assert!(page.history.can_undo());

    store.compact(0).expect("the page folded");

    drop(store);
    let store = NoteStore::open(&path).expect("the note again");

    let page = store.load_page(0, 16).expect("the page");
    assert_eq!(page.strokes.len(), 3);
    assert_eq!(
        page.history.depth(),
        3,
        "the ink changed shape, the log not"
    );
    assert!(page.history.can_undo());
}

/// A finished note is one standalone file: what an export writes can be opened on its own.
///
/// `VACUUM INTO` is why — the write-ahead log is folded in and the copy is complete — and this is the
/// file a person carries to another machine, so it has to be a note in its own right rather than
/// something that needs the folder it came from.
#[test]
fn an_exported_note_is_a_standalone_database() {
    let (mut store, path) = note("export");
    let written = ink(9);
    store
        .append(0, &written, &plain(written.len()))
        .expect("ink");
    store.checkpoint().expect("the log folded in");

    let export = path.with_file_name("carried.db");
    store.export(&export).expect("the note written out");
    drop(store);

    assert!(export.is_file(), "an export is a file");

    let carried = NoteStore::open(&export).expect("the carried note");
    let read = carried.load(0).expect("its ink");

    assert_eq!(read.len(), written.len());
    for (before, after) in written.iter().zip(&read) {
        assert_eq!(after.points, before.points);
    }

    let version: i64 = sql(&export)
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("a schema version");
    assert_eq!(version, i64::from(SCHEMA_VERSION));
}

/// A chunk that is damaged is reported rather than decompressed into a page of noise.
///
/// The checksum is in the row and the bytes are in the file, so a disk that flipped a bit has to be
/// caught before anything is drawn — or worse, written back.
#[test]
fn a_damaged_chunk_is_reported() {
    let (mut store, path) = note("damaged");
    let written = ink(20);
    store
        .append(0, &written, &plain(written.len()))
        .expect("ink");
    store.compact(0).expect("the page folded");
    assert!(store.load(0).is_ok());

    // Every byte of every chunk zeroed: the same length, so only the checksum can catch it.
    sql(&path)
        .execute("UPDATE chunks SET data = zeroblob(length(data))", [])
        .expect("the damage");

    let refused = store.load(0);
    assert!(refused.is_err(), "a damaged page was decoded anyway");
}

/// A note written by another build is refused, in both directions, and left exactly as it was found.
///
/// There is no migration: a note this build cannot read is a note it does not touch. What is asserted
/// here is both halves of that — the refusal names which way round the difference is, and the file's
/// own stamp is unchanged afterwards, because "refused" has to mean *left alone*.
#[test]
fn a_note_from_another_build_is_refused_in_both_directions() {
    for (name, stamp, expected) in [
        ("newer", SCHEMA_VERSION + 1, "newer"),
        ("older", SCHEMA_VERSION - 1, "older"),
    ] {
        let (store, path) = note(name);
        drop(store);

        sql(&path)
            .execute_batch(&format!("PRAGMA user_version = {stamp};"))
            .expect("another build's stamp");

        let refused = NoteStore::open(&path).expect_err("a note from another build");

        assert!(
            refused.to_string().contains(expected),
            "{refused} does not say which way round the difference is"
        );
        assert!(
            refused.to_string().contains(&stamp.to_string()),
            "{refused} does not say which version it found"
        );

        let after: i64 = sql(&path)
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("the stamp still there");
        assert_eq!(after, i64::from(stamp), "a refused note is not rewritten");
    }
}
