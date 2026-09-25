# How cheap-note stores your ink

This document explains, from the outside in, what happens to a stroke after you draw it: where it
goes on disk, how it is encoded, when it is written, and what it costs. It is written for anyone who
has to use, debug, or change this program — no prior knowledge of SQLite, compression, or the app's
code is assumed.

If you read only one paragraph, read this one:

> **A note is a folder with one SQLite file in it.** Ink is written into that file *while you draw*,
> in batches, on a background thread — nothing waits for a *Save*. Strokes are not stored as JSON
> but as compressed structure-of-arrays blobs, so a page costs a few kilobytes rather than a few
> hundred. *Save* does not "save" anything: it writes one portable file (a zip) that you can carry
> to another machine.

---

## 1. The two shapes of a note

A note exists in two forms, and only one of them is the one you work in.

**The working copy** is what the app opens and writes to. It lives in the app's own state directory:

```text
%LOCALAPPDATA%\cheap-note\notes\chapter-3-9f2a1c44\
    note.db                  the SQLite database: all of the ink and the note's own facts
    source.pdf               the PDF the note was written on, byte for byte (optional)
    attachments\             anything else the note carries (nothing writes here yet)
```

**The carried file** is what you move between machines — it is what the *Save* button writes:

```text
chapter-3.zip
    note.db                  a complete, standalone copy of the database
    source.pdf
    attachments\
```

The two are the same note; the zip is a *snapshot* of the folder, made with `VACUUM INTO` so that it
has nothing half-written in it (see §8).

`chapter-3-9f2a1c44` is the name of the file the note was made from — `chapter-3.pdf` — plus a short
digest of its full path, so that two different files with the same name are two different notes, and
so that importing the same file twice finds the same working copy again.

**Why the working copy is not next to your files.** An open SQLite database is three files (`note.db`
and, while it is open, `note.db-wal` and `note.db-shm`) that are written independently. Synchronising
folders — Dropbox, OneDrive, iCloud — copy and merge files under a running program, which corrupts
databases. So a note is *imported* into the app's own directory and *exported* as a single file. That
is the whole of the rule: open locally, move by exporting.

Whereas a *folder* you point the app at is opened where it stands, for someone who wants their notes
in their own directory and promises not to sync it.

## 2. Where the ink goes, end to end

```text
  you draw
     │
     ▼
  memory ──────────────────── the stroke under the nib is not stored: it is not history yet
     │  a stroke is finished (the pen lifts)
     ▼
  a batch goes out: 200 finished strokes, or 500 ms, whichever comes first
     │
     ▼
  dirty_strokes ───────────── one row per stroke, one transaction per batch, WAL
     │  the page closes (you turn away from it)
     ▼
  chunks ──────────────────── the dirty rows are encoded, compressed, and deleted in the
     │                        same transaction — this is called *compaction*
     │  the app has been quiet for a while
     ▼
  the write-ahead log is folded back into note.db      (wal_checkpoint)
     │
     │  you press Save
     ▼
  chapter-3.zip ───────────── VACUUM INTO, then packed with the document and attachments
```

There are only two things to remember about this pipeline:

1. **Writes are small and incremental.** A batch adds rows; it never rewrites the page, and never
   touches the ink that is already stored.
2. **Reads never wait for writes.** The database runs in WAL mode, so the thread that draws the page
   in front of you is never blocked by the thread that is writing it (see §6).

---

## 3. The database, table by table

`note.db` holds four tables and one view. Only two of them ever hold ink.

| Table | One row is | Why it exists |
|---|---|---|
| `pages` | one page of the note | the page's position, its timestamps, its bounding box, and how many strokes its chunks hold |
| `chunks` | up to 512 strokes, compressed | the *stored* ink: this is what a page is made of once it is closed |
| `dirty_strokes` | exactly one stroke, compressed | freshly drawn ink waiting to become a chunk |
| `meta` | one key and its value | the note's own facts: which page was open, the page list, the paper size, the document's name |
| `page_strokes` *(view)* | one chunk **or** one dirty stroke | a page's ink, whole, in the order it was drawn — what a read runs against |

A page with nothing on it has **no row** in `pages`. "The pages that hold ink" is therefore a query,
not a column somebody has to remember to update.

### Why a page has both an `id` and an `ord`

```sql
pages(id INTEGER PRIMARY KEY,   -- an identity: never changes, and what chunks point at
      ord INTEGER NOT NULL,     -- the page's position in the note's reading order
      ...)
```

Inserting a page renames every page after it, and deleting one closes the gap. If the identity *were*
the position, that would mean rewriting the `page_id` of every chunk in the note. Keeping the two
apart makes an insert two small updates of `pages` — and the ink, which points at `id`, does not move
at all.

### The ink itself

```sql
chunks(id, page_id, seq, stroke_start, stroke_count, codec, raw_len, data BLOB, crc32)
dirty_strokes(id, page_id, seq, data BLOB, created_at)
```

`stroke_start` and `seq` are the page's two coordinates: `stroke_start` says *which stroke of the page*
a chunk begins at, and `seq` says in *which order* the chunks were written. Chunks are written in
order, so the two agree — `stroke_start` is what makes a chunk's place in the page explicit, which is
what a rewrite and a re-compaction need. The view hands both kinds of row back with one `ord` that a
read can simply sort by:

```sql
CREATE VIEW page_strokes AS
    SELECT page_id, seq, stroke_start AS ord, stroke_count, codec, raw_len, crc32, data, 0 AS is_dirty
      FROM chunks
    UNION ALL
    SELECT page_id, seq, <the page's chunk total> + seq AS ord, 1, 2, 0, 0, data, 1 AS is_dirty
      FROM dirty_strokes;
```

The dirty strokes are given an `ord` *after* the chunked ones, because that is where they belong:
dirty ink is always newer than compacted ink. So a read is `ORDER BY ord, seq` and the page comes back
in the order it was drawn. `is_dirty` says how each row has to be decoded — see §4.

## 4. How a stroke becomes bytes

This is the part that replaced JSON, and the reason a page is kilobytes instead of megabytes.

### The shape of a chunk

One `chunks.data` blob is a page's worth of ink — up to 512 strokes or about 64 KB of arrays,
whichever comes first — laid out as three separate arrays rather than as a list of points:

```text
  [total points]  [stroke count]  [palette: count, then the colours]
  [per stroke: colour index (1 byte), point count]
  x array:  | x0 (4 bytes) | Δx1 Δx2 Δx3 ... |  x0 | Δx1 ... |   <- one run per stroke
  y array:  | y0 (4 bytes) | Δy1 Δy2 Δy3 ... |  y0 | Δy1 ... |
  width array: | w0 (4 bytes) | Δw1 Δw2 ... |
```

Everything except the four-byte starting values is a variable-length integer, and everything is
*deltas*: the difference from the previous point. Both choices matter because of what a pen actually
produces.

* A resampled pen path steps a pixel or two at a time, so a delta is a small number, and a small
  number is one or two bytes as a varint where an `f32` is always four.
* Values are fixed point at **1/64 of a pixel** (`QUANTUM`). That is finer than any digitizer reports
  and finer than one pixel at 400% zoom, and it keeps a two-pixel step inside two bytes.

The result, per point, is about **2 bytes for `x`, 2 for `y`, 1 for `width`** — four to five bytes in
total, where JSON spent forty-seven.

The charm of fixed point is that it is a *lattice*: decode gives back exactly the values that were
written, so a note that is read and written again produces byte-identical chunks and cannot drift a
little further from the original on every save. There is a test for exactly that.

### Colours are a palette

A note is usually written in a handful of colours, so a chunk carries a table of them at its head and
each stroke stores a one-byte index. A chunk ends early if a 256th colour would arrive — which is why
the palette index can be a single byte.

### The blob is compressed, and the compressor is recorded

`codec` says how the blob was produced, and it is decided by *measuring*, not by policy:

| `codec` | What it is |
|---|---|
| `0` | nothing: the arrays as they are |
| `1` | LZ4 (`lz4_flex`) |
| `2` | zstd, level 3 |

The writer tries zstd first, then LZ4, then gives up — and keeps whichever is smaller than the arrays.
That order is not decoration: a *dirty* row is a blob of **one stroke**, and a single resampled stroke
is usually 20-80 points — a hundred to three hundred bytes, which is smaller than a compressor's own
header (measured: raw wins at 20, 45 and 70 points; zstd only takes over somewhere past a hundred). So
dirty rows are normally stored uncompressed, and it is the `chunks` rows, a page at a time, that
compress. There is a test pinning both halves of that.

Every blob is sealed with a **CRC32**, and decoded with all four of its facts checked — the checksum,
the expected uncompressed length, the codec, and the number of strokes it is filed under. A blob that
disagrees with any of them is reported as damaged rather than guessed at: this is the difference
between "your note has a bad chunk" and "your page is now noise".

### What a dirty row looks like

`dirty_strokes` has exactly one column for the ink, so a dirty row carries the three facts a chunk
column would have given it in a nine-byte header:

```text
  [codec: 1 byte] [raw length: 4 bytes] [crc32: 4 bytes] [the blob]
```

### Two ways to encode, one way to read

`is_dirty` in the view is what tells a reader which of the two decodings to use — the nine-byte header
or the chunk columns — and the two paths produce the same `Stroke` values. The hastiest way to think
about it: **dirty rows are the write-ahead area of the *ink*, and chunks are the ink.**

---

## 5. What it costs

Measured with a deliberately *irregular* fixture — strokes that walk with 1-2 px steps and a slowly
turning heading, a different shape for every stroke — because a page of identical strokes compresses
to almost nothing and would flatter the format:

| Ink | Arrays before compression | What is written | As JSON (the old format) |
|---|---|---|---|
| 10 strokes, 445 points | 1.9 KB | 1.7 KB | 21 KB |
| 100 strokes, 5.4 K points | 23.2 KB | 18.9 KB | 258 KB |
| 1,000 strokes, 54.9 K points | 235 KB | 184 KB | 2.6 MB |

*(The fixture and the shape of these numbers are pinned by the test
`chunk::tests::a_hand_written_page_is_much_smaller_than_json`, which asserts a page of this fixture is
at least eight times smaller than its JSON; the exact byte counts move a little with the fixture.)*

Two things follow:

* **About 4 bytes per point before compression, about 3.4 after.** A dense handwritten A4 page is
  maybe 4,000 points, so roughly 14 KB in the file. A hundred such pages is around 1.4 MB.
* **The old format cost about 47 bytes per point.** The same notebook would have been 19 MB, and every
  save rewrote all of it.

Compression is the smaller half of the win, and it varies with your hand: the fixture above is close
to a *worst case* for zstd because every stroke is unrelated to every other. Real writing repeats
letters and long smooth runs, so real notes usually do better. The encoding — deltas, fixed point,
structure of arrays — is what does the rest, and it is deterministic.

## 6. When you draw

Nothing on the drawing path touches a file. A `Down`…`Up` pair becomes a `Stroke` in memory, and the
app hands finished strokes to a background thread on a schedule:

| When | What happens |
|---|---|
| 200 finished strokes are waiting | they are sent as one batch |
| 500 ms have passed since the last batch | the smaller batch is sent anyway |
| the page is closed (you turn away) | the batch goes out, and the page is **compacted** into chunks |
| `undo`, the eraser, or *Clear* changed the page | the page is **rewritten**, not appended to |
| the pen has been still for 5 s, and 5 minutes have passed | the write-ahead log is folded back into `note.db` |
| you press *Save* | everything outstanding is written, then the export is made |
| the app closes | the writer's last job is a checkpoint |

Everything after "sent" happens on the writer thread, which owns its own connection to `note.db`; the
app holds a second connection of its own, which it uses to read a page and to write the note's own
facts (which page is open, the page list). Two connections, one file — and WAL is what makes that
safe: a reader sees a consistent snapshot of the ink without ever waiting for a commit.

### Why *rewrite* has to exist

Appending can only describe ink being **added**. Undo removes a stroke, the eraser removes several,
and *Clear* removes all of them — none of which a batch of new strokes can express. The app notices
this the only way that is cheap and reliable: it remembers how many strokes of the page it has already
sent, and when the finished count goes **down**, the page is marked for rewriting. A rewritten page
has its chunks and dirty rows dropped and its whole ink written again, in one transaction — so a crash
in the middle leaves either the old page or the new one, never half of each.

### What a crash costs

At most the ink of the batches that had not been sent yet: the last 500 ms, or the last 200 strokes,
whichever is smaller. Everything else is in `note.db`, committed. Pressing *Save* is not a way to
avoid that risk — the risk is already bounded to a fraction of a second by the batch rule.

## 7. When you open a note

Opening does *not* read the note. It reads:

* the note's own facts from `meta` — the page list, which page was open, the paper size, the
  document's name;
* the document, from `source.pdf`;
* **one page**: the one that was open.

Every other page is read the moment you turn to it, and once read it stays in memory for the rest of
the session. A thousand-page notebook opens in the time a one-page one does, because nothing scales
with the number of pages.

Reading a single page is one query against the `page_strokes` view, then decompression of each chunk
**in parallel** (a page is typically a handful of independent 64 KB blobs, so this is a broadcast
across the thread pool rather than a loop). Dirty rows are decoded one at a time: they are single
strokes, and parallelising them would cost more in hand-offs than it saves.

Two more things a page carries, both read from `pages` rather than from the ink: how many strokes it
holds (the `stroke_count` query counts chunks *and* dirty rows) and its bounding box. The box is kept
up to date as ink is written — it is not read by anything yet, and it is there for the questions a
future feature asks without loading a page ("is this page blank?", "where is the ink?").

---

## 8. Moving a note to another machine

*Save* writes the note out as a single file. It is not a re-save of the note — the note is always
current — it is a **snapshot**:

1. fold the write-ahead log back into `note.db` (`wal_checkpoint(TRUNCATE)`);
2. **`VACUUM INTO`** a temporary file next to it: this is SQLite's own "write me a clean, standalone
   copy" — no log, no free pages, nothing half-written;
3. pack that copy, `source.pdf`, and everything under `attachments\` into a zip;
4. delete the temporary file.

Copying `note.db` by hand would be the wrong thing to do, and this is worth understanding: an open
database has its newest pages in the `-wal` file, so a plain copy can miss recent ink, or capture a
page that was being written at that instant. `VACUUM INTO` cannot: it reads the database as a
consistent snapshot and writes a new file. That is also why this is the *only* way this app hands you
a note to carry.

To open one on the other machine: *Open*, choose the zip. The app unpacks it into a working copy and
opens it. If the given name is already in use, the working copy is replaced — the same file imported
twice is one note, not two.

The format itself is portable in the boring way that matters: SQLite files are byte-order
independent, so x86 and ARM machines read each other's notes, and the file format has been stable for
twenty years. What is *this app's* is the chunk encoding, and its version travels in the database
(`PRAGMA user_version`, see §10).

## 9. Notes written by older versions

Before this, a note was a zip holding `document.pdf` and a `notes.json` — every point of every stroke
as text. That reader still exists, read-only, for one purpose: so that opening an old note **migrates**
it instead of losing it.

*Open* an old zip and the app notices there is no `note.db` inside, reads the JSON, and writes a new
working copy: every page becomes a chunk, the document is copied in as `source.pdf`, and the page
list, paper size, the page that was open, and the document's name go into `meta`. From then on it is a
normal note. Nothing writes the old format — there is no reason to, and a format with two writers
would be a format with two behaviours.

The migrated ink is quantised to 1/64 of a pixel on the way in, because that is what the new encoding
stores. The difference is invisible (it is a sixtieth of a pixel) and it never accumulates: the
conversion happens once, not on every save.

An old note that claims a *newer* format version than this build knows is refused rather than guessed
at — reading a file whose fields you do not know is how a note gets rewritten without its ink.

## 10. Versioning and integrity

| Mechanism | What it protects against |
|---|---|
| `PRAGMA user_version` (this build writes `3`) | a note written by a newer build: refused, with a message that says so |
| the zip's entry list | a file that is not a note at all: named as such rather than half-read |
| the CRC32 of every blob | a damaged disk or a truncated file: reported as damaged |
| the uncompressed length of every blob | a blob that decompresses to the wrong size |
| the stroke count every blob is filed under | a blob that is intact and is *not the chunk it is filed under* — the case where guessing would write a corrupted page over a good one |
| an unknown `codec` id | a compressor a newer build knows and this one does not |

The version number continues the note format's own history rather than starting over: `1` was the
first zip of JSON, `2` added the page list to it, and `3` is the database. So one number orders every
note file this app has ever written, whichever shape it is in.

---

## 11. The promises, and the tests that keep them

Every claim in this document is checked by a test that fails if the behaviour changes. The names are
listed so a reader can go and read the code that proves the thing they just took on trust.

| Promise | Test |
|---|---|
| A page of ink comes back exactly, stroke for stroke, point for point | `chunk::tests::a_chunk_round_trips` |
| A one-point stroke (a dot) survives — the case a delta-only format gets wrong | `chunk::tests::a_single_point_stroke_round_trips` |
| Reading and writing again produces identical bytes: fixed point cannot drift | `chunk::tests::re_encoding_what_was_decoded_gives_the_same_bytes` |
| A damaged blob is refused, not turned into noise | `chunk::tests::a_damaged_chunk_is_refused` |
| A blob that is intact but is not the chunk it claims to be is refused | `chunk::tests::a_chunk_filed_under_the_wrong_count_is_refused` |
| A hand-written page is at least 8× smaller than its JSON | `chunk::tests::a_hand_written_page_is_much_smaller_than_json` |
| A single-stroke blob is usually stored uncompressed (and a long one is not) | `chunk::tests::a_single_stroke_blob_usually_skips_the_compressor` |
| The chunks of a page tile it: every stroke in exactly one chunk, in order | `chunk::tests::a_page_is_split_into_chunks_that_tile_it` |
| A page of hundreds of colours is split before the palette overflows | `chunk::tests::the_palette_ends_a_chunk_before_it_overflows` |
| The database is created with the journal mode, page size and version it asks for | `store::tests::a_new_note_is_a_tuned_database` |
| Appended ink comes back, in the order it was drawn | `store::tests::what_is_appended_comes_back` |
| Compaction folds the dirty rows into chunks and the page reads the same | `store::tests::compaction_folds_the_dirty_strokes_into_chunks` |
| Ink added after a compaction follows the ink already there | `store::tests::a_page_written_in_two_batches_keeps_its_order` |
| A rewrite replaces a page (what undo, the eraser and Clear need) | `store::tests::rewriting_a_page_replaces_what_was_there` |
| Inserting or deleting a page moves its ink with it | `store::tests::a_page_moves_when_one_is_inserted_or_deleted` |
| A page left empty stops counting as a page with ink | `store::tests::an_emptied_page_stops_counting_as_a_page` |
| A damaged chunk in a note is reported when the page is read | `store::tests::a_damaged_chunk_is_reported` |
| An export is a note of its own, complete and standalone | `store::tests::an_export_is_a_note_of_its_own` |
| The note remembers its page list, open page and paper size | `store::tests::a_note_remembers_its_own_page_list` |
| Checkpointing folds the log back and the ink is still there | `store::tests::checkpointing_folds_the_log_back` |
| A note from a newer build is refused | `store::tests::a_newer_note_is_refused` |
| A note travels as one file and comes back whole | `note::tests::a_note_travels_as_one_file` |
| Attachments travel with the note | `note::tests::attachments_travel_with_the_note` |
| The writer thread writes what it is given and reports its export | `note::tests::the_writer_writes_on_its_own_thread` |
| A PDF becomes a note with that document | `note::tests::a_note_starts_on_a_document` |
| A zip that is not a note is refused | `note::tests::a_foreign_zip_is_refused` |
| An old zip of JSON is migrated into a database | `note::tests::an_old_note_is_migrated_into_a_store` |
| A migrated stroke knows where it is (so the eraser works on it) | `legacy::tests::an_old_note_is_read_for_migration` |

## 12. Where the code is

| Module | Responsibility |
|---|---|
| `src/store.rs` | the database: schema, PRAGMAs, the batch write, compaction, the read path, checkpoint, `VACUUM INTO` |
| `src/chunk.rs` | the blob format: encode, decode, integrity, chunk boundaries |
| `src/note.rs` | the note folder, the writer thread and its job queue, the zip container, migration |
| `src/legacy.rs` | the old zip of JSON, read-only |
| `src/ink.rs` | strokes in memory: what a stroke is, and where each page's ink is kept |
| `src/app.rs` | when to write: the batch clock, page close, the checkpoint rule, and the UI |

The handful of functions worth knowing by name:

| Function | What it does |
|---|---|
| `chunk::encode` / `chunk::decode` | one batch of strokes → one blob, and back |
| `chunk::chunk_ranges` | where a page's ink is cut into chunks |
| `NoteStore::append` | a batch of strokes into `dirty_strokes`, one transaction |
| `NoteStore::compact` | dirty rows → chunks, in one transaction |
| `NoteStore::rewrite` | a page written again from scratch |
| `NoteStore::load` | one page's ink, in order |
| `NoteStore::export` | `VACUUM INTO` |
| `Note::placed_from` | import: a zip, a PDF, or a folder → a working copy |
| `NoteWriter::spawn` | the writing thread and its queue |
| `NoteApp::persist` | the rule that decides *when* ink is handed over |
| `NoteApp::load_page_ink` | the read that happens when you turn a page |

---

## 13. What is deliberately *not* stored

Knowing what a store refuses to hold is as useful as knowing what it holds.

| Not stored | Why |
|---|---|
| the stroke under the nib | it is not history yet: it has no final geometry, it is not what a save writes, and taking it back would leave the model thinking the pen was lifted |
| undo/redo history | it is a property of the session, not of the note. A note opens with a page of ink and no way back through last week's strokes |
| an erased stroke | the eraser removes whole strokes, and undo takes back the most recent *surviving* stroke. There is no "recover what I erased" — see `src/ink.rs` |
| the app's own settings | text size, the toolbar, the pen's smoothing and so on are the *user's*, and live in `cheap-note.settings.json` next to the program. A note records only what belongs to the note: its page list, paper size, open page, and document name |
| attachments (for now) | the container carries an `attachments\` folder faithfully, but nothing in this build writes one yet — it is where a pasted image or a recording will go |
| rendered PDF pages | those are a cache, rebuilt from `source.pdf` whenever they are needed |

One more non-storage worth naming: **the note is not rewritten when you press Save.** Save cannot
destroy anything — it reads the note and writes a *new* file somewhere else.

## 14. Appendix: the exact schema, PRAGMAs and constants

### The schema

```sql
CREATE TABLE pages (
    id           INTEGER PRIMARY KEY,   -- identity, what chunks point at
    ord          INTEGER NOT NULL,      -- position in the note's reading order
    created_at   INTEGER NOT NULL,      -- milliseconds since the epoch
    updated_at   INTEGER NOT NULL,
    bbox_min_x   REAL, bbox_min_y REAL, bbox_max_x REAL, bbox_max_y REAL,
    stroke_count INTEGER NOT NULL DEFAULT 0   -- strokes in this page's *chunks*
) STRICT;
CREATE UNIQUE INDEX idx_pages_ord ON pages(ord);

CREATE TABLE chunks (
    id           INTEGER PRIMARY KEY,
    page_id      INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,      -- write order within the page
    stroke_start INTEGER NOT NULL,      -- which stroke of the page this chunk begins at
    stroke_count INTEGER NOT NULL,
    codec        INTEGER NOT NULL,      -- 0 raw, 1 lz4, 2 zstd
    raw_len      INTEGER NOT NULL,      -- length before compression
    data         BLOB NOT NULL,
    crc32        INTEGER NOT NULL,
    UNIQUE(page_id, seq)
) STRICT;

CREATE TABLE dirty_strokes (
    id         INTEGER PRIMARY KEY,
    page_id    INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,        -- append order within the page
    data       BLOB NOT NULL,           -- 9-byte header + one stroke's blob
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;

CREATE INDEX idx_chunks_page ON chunks(page_id, stroke_start);
CREATE INDEX idx_dirty_page  ON dirty_strokes(page_id, seq);
```

`STRICT` means SQLite checks the declared type of every value, which is what keeps a `BLOB` column a
`BLOB` column. `ON DELETE CASCADE` is why deleting a page is one statement: its chunks and dirty rows
go with it.

### The pragmas, in the order they are applied

```sql
PRAGMA page_size = 8192;          -- must come first: see the note below
PRAGMA journal_mode = WAL;        -- a reader never waits for a writer
PRAGMA synchronous = NORMAL;      -- safe under WAL, and it does not fsync per commit
PRAGMA busy_timeout = 5000;       -- wait five seconds rather than fail on a lock
PRAGMA cache_size = -64000;       -- 64 MB of page cache
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 268435456;     -- 256 MB mapping
PRAGMA wal_autocheckpoint = 1000; -- SQLite's own housekeeping, on top of the app's
PRAGMA foreign_keys = ON;         -- the cascade above only works with this
```

**Why `page_size` comes first.** A page size is only read when a database has no pages yet, and
switching to WAL *writes* to the file. Set the page size afterwards and the file keeps SQLite's 4 KB
default for the rest of its life. This one was found by a test, which is the reason the test asserts
it.

### The constants

| Constant | Value | Meaning |
|---|---|---|
| `chunk::QUANTUM` | 64 | fixed-point units per logical pixel (1/64 px) |
| `chunk::CHUNK_RAW_TARGET` | 64 KB | arrays per chunk, before compression |
| `chunk::CHUNK_MAX_STROKES` | 512 | strokes per chunk, whichever comes first |
| (palette) | 255 | distinct colours per chunk, enforced by ending the chunk |
| `chunk::ZSTD_LEVEL` | 3 | the compression level the design settles on |
| `store::SCHEMA_VERSION` | 3 | what `PRAGMA user_version` says (see §10) |
| `app::BATCH_INTERVAL` | 500 ms | how long ink may wait in memory |
| `app::BATCH_STROKES` | 200 | how much ink may wait in memory |
| `app::CHECKPOINT_QUIET_INTERVAL` | 5 s | how long the pen must be still before a checkpoint |
| `app::CHECKPOINT_INTERVAL` | 5 min | the least time between two checkpoints |
| `note::NOTE_DB` / `NOTE_PDF` / `ATTACHMENTS` | `note.db` / `source.pdf` / `attachments` | the three names inside a note folder |

### A note about the database's own files

While a note is open you will see `note.db-wal` and `note.db-shm` beside it. That is normal, and it
is the whole reason a write does not stop a read. The log is folded back into `note.db` when the app
has been idle (§6), and always before a note is exported — so an exported file never has one.
