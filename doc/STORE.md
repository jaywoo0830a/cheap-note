# How cheap-note stores your ink

This document follows one stroke from the pen to the disk and back: what is written, when it is
written, how it is encoded, and what it costs. No knowledge of SQLite, of compression, or of this
code is assumed.

If you read only one paragraph, read this one:

> **A note is a folder with one SQLite file in it.** Ink is written into that file *while you draw*,
> in batches, on a background thread — nothing waits for a *Save*. Strokes are not stored as JSON but
> as compressed structure-of-arrays blobs, so a page costs a few kilobytes rather than a few hundred.
> *Save* does not "save" anything: it writes one portable file (a zip) you can carry to another
> machine.

---

## 1. What a note is on disk

A note is one folder in the app's own state directory, and the same three names make up the single file
a person carries — this is what the *Save* button writes:

```text
%LOCALAPPDATA%\cheap-note\notes\chapter-3-9f2a1c44\
    note.db                  the SQLite database: all of the ink and the note's own facts
    source.pdf               the PDF the note was written on, byte for byte (optional)
    attachments\             anything else the note carries (nothing writes here yet)

chapter-3.zip                the carried form: a standalone copy of note.db, plus the two above
```

While it is open, `note.db-wal` and `note.db-shm` sit beside it. That is the write-ahead log, it is
normal, and it is the whole reason a write does not stop a read (§6): it is folded back into `note.db`
when the pen has been still, and always before the note is exported (§8).

**Why the working copy is not next to your files.** An open database is three files written
independently, and synchronising folders (Dropbox, OneDrive, iCloud) copy and merge files under a
running program, which corrupts databases. So a note is *imported* into the app's own directory and
*exported* as one file: open locally, move by exporting. A folder you point the app at is opened where
it stands, for someone who keeps their notes in their own directory and will not sync it.

`chapter-3-9f2a1c44` is the name of the file the note was made from plus a short digest of its full
path, and that name *is* the note's identity: it is what makes importing the same PDF twice find the
same note rather than a second copy of it. What a note is *called* — the name on the bar, and in the
list — is a `meta` row, so a note carried to another machine arrives already named (§3).

Beside the notes folder there is one small file, `%LOCALAPPDATA%\cheap-note\recent.json`: the list of
what has been opened. It is a **cache of the notes folder, not a note** — deleting it costs the order of
a list, and a folder it has never heard of is adopted by being there.

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
  chunks ──────────────────── the dirty rows are encoded, compressed and deleted in the
     │                        same transaction — this is called *compaction*
     │  the app has been quiet for a while
     ▼
  the write-ahead log is folded back into note.db      (wal_checkpoint)
     │
     │  you press Save
     ▼
  chapter-3.zip ───────────── VACUUM INTO, then packed with the document and attachments
```

Two things to remember about this pipeline:

1. **Writes are small and incremental.** A batch adds rows; it never rewrites the page, and never
   touches ink that is already stored.
2. **Reads never wait for writes.** The database runs in WAL mode, so the thread drawing the page in
   front of you is never blocked by the thread writing it (§6).

---

## 3. The database

`note.db` holds four tables and one view. Only two of them ever hold ink.

| Table | One row is | Why it exists |
|---|---|---|
| `pages` | one page of the note | its position, its timestamps, its bounding box, and how many strokes its chunks hold |
| `chunks` | up to 512 strokes, compressed | the *stored* ink: what a page is made of once it is closed |
| `dirty_strokes` | exactly one stroke, compressed | freshly drawn ink waiting to become a chunk |
| `meta` | one fact and its value | **everything else the note knows**: the page that was open, the page list, the sheet the ink is in, the document it came from, its name, and every setting there is to change |
| `page_strokes` *(view)* | one chunk **or** one dirty stroke | a page's ink, whole, in the order it was drawn — what a read runs against |

A page with nothing on it has **no row** in `pages`. "The pages that hold ink" is therefore a query
rather than a column somebody has to remember to update.

**Why a page has both an `id` and an `ord`.** `id` is an identity that never changes, and it is what
chunks point at; `ord` is the page's position in the note's reading order. Inserting a page renames
every page after it and deleting one closes the gap — if the identity *were* the position, that would
mean rewriting the `page_id` of every chunk in the note. Keeping the two apart makes an insert two
small updates of `pages`, and the ink does not move at all.

### The ink itself

```sql
chunks(id, page_id, seq, stroke_start, stroke_count, codec, raw_len, data BLOB, crc32)
dirty_strokes(id, page_id, seq, data BLOB, created_at)
```

`stroke_start` says *which stroke of the page* a chunk begins at, and `seq` says in *which order* the
chunks were written. Chunks are written in order, so the two agree; `stroke_start` is what makes a
chunk's place in the page explicit, which is what a rewrite needs. The view hands both kinds of row
back with one `ord` a read can simply sort by — the dirty rows *after* the chunked ones, because dirty
ink is always newer than compacted ink:

```sql
CREATE VIEW page_strokes AS
    SELECT page_id, seq, stroke_start AS ord, stroke_count, codec, raw_len, crc32, data, 0 AS is_dirty
      FROM chunks
    UNION ALL
    SELECT page_id, seq, <the page's stroke_count> + seq AS ord, 1, 2, 0, 0, data, 1 AS is_dirty
      FROM dirty_strokes;
```

A read is `ORDER BY ord, seq`, and the page comes back in the order it was drawn. `is_dirty` says how
each row has to be decoded (§4).

### The history

```sql
pages(…, stroke_count, history_at, history_count)
edits(page_id, ord, data BLOB, created_at)
```

One row per edit a page's ink has been through — a stroke written, ink erased, a selection dragged,
the page cleared — each holding **ink**, not a description of an operation: `data` is the strokes the
edit is about, sealed by the same codec a page or a dirty row uses (§4). That is what makes undo
survive a restart without the note having to understand what an edit *means*: a build that changes the
model can still read yesterday's strokes, and a row of a kind it does not know is refused by name
rather than read as one of its own.

`ord` counts a page's edits and never restarts, and the pair of columns on `pages` is the **cursor**:
`history_at` is the ordinal of the last applied edit, `history_count` is how many strokes the page had
when that edit was made. The rows at or below the cursor are the page as it stands; the rows above it
are the redo branch. So:

* **Undo** moves the cursor back one and reverts that edit; **redo** moves it forward and applies one.
  Rows are never rewritten to do it — the cursor *is* the state.
* **A new edit over an undone one** takes the place of the branch: the tail the app sends is written
  from its own first ordinal onwards, and the rows it lands on are deleted in the same transaction.
  That is the whole of "a new edit ends the redo branch".
* **The log is only trusted while it agrees with the ink.** A cursor whose `history_count` is not the
  page's stroke count — a page written by something that kept no history — is ignored rather than
  repaired: the ink is read in full, there is simply nothing to take back. Opening a note never writes.
* **It is a depth, not a lifetime.** The idle checkpoint drops everything below `history_at - 4096`
  (§5's housekeeping), so "how far back can I go" has an answer and a note does not grow a history the
  size of itself. What goes is the *deepest* undo, and the run that is left still follows.

The log is written **in the same transaction as the ink it describes** — the app hands the writer a
tail of edits in the job that carries the strokes — so a crash can leave the note with ink and no edit,
or an edit and no ink, but never an edit that describes ink the note does not have.

### What the note's `meta` holds

`meta` is where everything that is *about the note* is filed — **one row per fact, named after the
fact, with its value as text**. The ink paths never touch it: the app reads it when a note is opened,
and writes it back whenever something changes.

```text
note.db → meta('open_page')    = "7"                       the page that was open
          meta('sheet_width')  = "685.71429"               the sheet the ink's coordinates are in
          meta('sheet_height') = "970.0"
          meta('layout')       = [{"show":"blank","rotation":0},   what each page shows, in reading
                                  {"show":{"document":3},          order, and which way up it is
                                   "rotation":1}]                  (quarter turns clockwise)
          meta('canvas_size')  = "A5"                      …and every setting, one row each
          meta('title')        = "3장 요약"
```

**A page's rotation is in this list and not in the ink.** Turning a page writes nothing at all: the ink
of a page is stored in the page's own coordinates and never moves, and a turn changes only *where those
coordinates are drawn* — so the note remembers a turn as a field of one row of the page list, the same
way it remembers what a page shows. That is also what makes turning a page free: no stroke is rewritten,
no chunk is re-encoded, and a page turned while writing does not hand the writer anything. A note
written before pages could be turned has a list of bare pages in this row; they are read as pages that
are the right way up (see `crate::pages::StoredPage`).

**The rows *are* the schema, and that is the whole point of them.** There is no packed blob to decode
and no stored shape for a struct to match, so a fact can be added to a note without anything being
migrated — and the three ways a row can fail to be there are three different answers:

* **a row this build does not know is never read, written or deleted**, so a note written by a build
  that knew more settings — or one somebody added a row to by hand — keeps them. Adding a setting is
  four small edits: a field and its default in `src/settings.rs`, and one line each in
  `NoteStore::read_settings` and `NoteStore::set_settings`;
* **a row that is not there is not a failure**: the note does not say, and the app's own answer stands;
* **a row that is there and cannot be understood *is* a failure**, reported to the person — something
  wrote it, and quietly replacing an answer is how a setting disappears without a word.

Every setting a person can change is a row here — the paper, the ruling, the colours, the pen and its
weight, the zoom, whether the bar and the status line are shown, and how the pen *feels* in the hand —
and nothing about a note is remembered anywhere but in the note. The pen is in two kinds of row on
purpose: `pen_weight` says *which* pen (`Fine` … `Heavy`) while `min_width`, `max_width` and
`no_pressure_width` say how *any* pen answers a hand, so a marker is fat at the lightest touch *and* at
the heaviest and no weight can make a line that thins as it is pressed. What is already written is
untouched: every point carries the width and the colour it was drawn with.

---

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

Everything except the four-byte starting values is a variable-length integer, and everything is a
*delta*: the difference from the previous point. Both choices matter because of what a pen actually
produces. A resampled path steps a pixel or two at a time, so a delta is a small number, and a small
number is one or two bytes as a varint where an `f32` is always four; and the values are fixed point
at **1/64 of a pixel** (`chunk::QUANTUM`), which is finer than any digitizer reports and finer than one
pixel at 400% zoom. The result is about **2 bytes for `x`, 2 for `y`, 1 for `width`** — four to five
bytes per point, where JSON spent forty-seven.

Fixed point is also a *lattice*: decode gives back exactly the values that were written, so a note that
is read and written again produces byte-identical chunks and cannot drift a little further from the
original on every save. There is a test for exactly that.

**Colours are a palette.** A note is usually written in a handful of colours, so a chunk carries a
table of them at its head and each stroke stores a one-byte index. A chunk ends early if a 256th colour
would arrive, which is why the index can be a single byte.

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
compress.

Every blob is sealed with a **CRC32**, and decoded with all four of its facts checked — the checksum,
the expected uncompressed length, the codec, and the number of strokes it is filed under. A blob that
disagrees with any of them is reported as damaged rather than guessed at: this is the difference
between "your note has a bad chunk" and "your page is now noise".

### What a dirty row looks like

`dirty_strokes` has exactly one column for the ink, so a dirty row carries the three facts a chunk
column would have given it in a nine-byte header — `[codec: 1 byte] [raw length: 4 bytes] [crc32: 4
bytes] [the blob]`.

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

*(Pinned by `chunk::tests::a_hand_written_page_is_much_smaller_than_json`; the exact byte counts move a
little with the fixture.)*

**About 4 bytes per point before compression, about 3.4 after.** A dense handwritten A4 page is maybe
4,000 points — roughly 14 KB in the file, so a hundred such pages is about 1.4 MB, where the JSON format
spent about 47 bytes per point and rewrote all of it on every save. Compression is the smaller half of
that win and varies with the hand: real writing repeats letters and long smooth runs.

## 6. When you draw

Nothing on the drawing path touches a file. A `Down`…`Up` pair becomes a `Stroke` in memory, and the
app hands finished strokes to a background thread on a schedule:

| When | What happens |
|---|---|
| 200 finished strokes are waiting | they are sent as one batch |
| 500 ms have passed since the last batch | the smaller batch is sent anyway |
| the page is closed (you turn away) | the batch goes out, and the page is **compacted** into chunks |
| `undo`, the eraser, *Clear*, or a lasso moving a selection | the page is **rewritten**, not appended to |
| an edit is made | the edits the note has not heard about go with the ink, in the same transaction (§3, *The history*) |
| the pen has been still for 5 s, and 5 minutes have passed | the write-ahead log is folded back into `note.db` |
| you press *Save* | everything outstanding is written, then the export is made |
| the app closes | the writer's last job is a checkpoint |

Everything after "sent" happens on the writer thread, which owns its own connection to `note.db`; the
app holds a second connection of its own, which it uses to read a page and to write the note's own
facts (which page is open, the page list). Two connections, one file — and WAL is what makes that safe:
a reader sees a consistent snapshot of the ink without ever waiting for a commit.

**Why *rewrite* has to exist.** Appending can only describe ink being **added**. Undo removes a stroke,
the eraser removes several, and *Clear* removes all of them — none of which a batch of new strokes can
express. The app notices this the only way that is cheap and reliable: it remembers how many strokes of
the page it has already sent, and when the finished count goes **down**, the page is marked for
rewriting. A rewritten page has its chunks and dirty rows dropped and its whole ink written again, in
one transaction — so a crash in the middle leaves either the old page or the new one, never half of
each.

A **lasso that moves a selection** is the one edit counting cannot see: it takes the strokes it has hold
of and shifts every point in them, so the number of strokes is exactly what it was while none of them is
where the note last saw it. The page therefore carries a second number — how many times its ink has been
*shifted* — and the app remembers the value it last wrote, beside the stroke count. One move, or twelve,
marks the page for a rewrite the same way a deletion does, and nothing is written for a drag that was put
down where it started. That count is per page and in memory only: it is a fact about what has *not* been
written yet, and the ink itself says everything else (see `src/ink.rs`).

**What a crash costs.** At most the ink of the batches that had not been sent yet: the last 500 ms, or
the last 200 strokes, whichever is smaller. Everything else is in `note.db`, committed. Pressing *Save*
is not a way to avoid that risk — the risk is already bounded to a fraction of a second by the batch
rule.

## 7. When you open a note

Opening does *not* read the note. It reads:

* the note's own facts from `meta` — the page list, which page was open, the sheet the ink was written
  on, the document's name, and every setting the note remembers (§3);
* the document, from `source.pdf`;
* **one page**: the one that was open.

Every other page is read the moment you turn to it, and once read it stays in memory for the rest of
the session. A thousand-page notebook opens in the time a one-page one does, because nothing scales
with the number of pages.

Reading a single page is one query against the `page_strokes` view, then decompression of each chunk
**in parallel** (a page is a handful of independent 64 KB blobs, so this is a broadcast across the
thread pool rather than a loop); dirty rows are decoded one at a time, because parallelising single
strokes would cost more in hand-offs than it saves. Two more things live in `pages` rather than in the
ink: how many strokes the page holds, and its bounding box — kept up to date as ink is written, and
there for the questions a future feature asks without loading a page.

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
consistent snapshot and writes a new file. That is also why this is the *only* way this app hands you a
note to carry.

To open one on the other machine: *Open*, choose the zip. The app unpacks it into a working copy and
opens it. If the given name is already in use, the working copy is replaced — the same file imported
twice is one note, not two. The format itself is portable in the boring way that matters: SQLite files
are byte-order independent, so x86 and ARM machines read each other's notes. What is *this app's* is the
chunk encoding, and its version travels in the database (§9).

---

## 9. Versioning, integrity, and old notes

| Mechanism | What it protects against |
|---|---|
| `PRAGMA user_version` (this build writes `5`) | a note written by *any* other build — newer or older: refused, with a message that says which way round the difference is |
| the zip's entry list | a file that is not a note at all: named as such rather than half-read |
| the CRC32 of every blob | a damaged disk or a truncated file: reported as damaged |
| the uncompressed length of every blob | a blob that decompresses to the wrong size |
| the stroke count every blob is filed under | a blob that is intact and is *not the chunk it is filed under* — the case where guessing would write a corrupted page over a good one |
| an unknown `codec` id | a compressor a newer build knows and this one does not |

**Only the current number is read, and there is deliberately no migration.** A note is opened by the
build whose schema wrote it and by no other, because this build would otherwise write its own shape
back over one it does not know. Refused means *left alone* — the stamp is not brought forward — so the
build that wrote the note can still open it afterwards, and the message names the number, so "open it
with the build that wrote it" is advice a person can act on. A version number covers the *tables* and
the meaning of a row this build reads; it no longer covers *adding* one, because with rows instead of a
blob a new setting is not a new schema (§3).

**Nothing reads the old format.** A note used to be a zip holding `document.pdf` and a `notes.json` —
every point of every stroke as text — and both its reader and the migration that turned one into a store
have been deleted. An old note is therefore the same case as any other zip that is not a note: there is
no `note.db` in it, so it is refused by name. The refusal is part of the deletion rather than an
accident of it: a zip unpacked "in case" would leave a stranger's files in the notes folder, and a note
half-imported is a worse thing to explain than one that was refused.

## 10. What is deliberately *not* stored

| Not stored | Why |
|---|---|
| the stroke under the nib | it is not history yet: it has no final geometry, and taking it back would leave the model thinking the pen was lifted |
| the *selection* a lasso holds | it is not ink and not an edit: it is the page's idea of what is in hand for as long as the note is open, and nothing about it belongs in a file |
| anything global | there is nothing global to store: every setting is a row of the note it was changed in (§3), so a note carried to another machine arrives as it was left. The one file outside a note is the index of what has been opened, and that is a cache (§1) |
| attachments (for now) | the container carries an `attachments\` folder faithfully, but nothing in this build writes one yet — it is where a pasted image or a recording will go |
| rendered PDF pages | a cache, rebuilt from `source.pdf` whenever they are needed |
| the document's outline | it belongs to the *document* — which the note carries inside itself — so it is read again at every open: a note whose document was replaced arrives with the new contents (see `src/outline.rs`) |

Undo/redo history used to be in this table, as "a property of the session, not of the note". It is stored
now (§3, *The history*): a page keeps its last few thousand edits beside its ink, so a note opened on Friday
can take back what was written on Monday. What is still *not* stored is the part of it that is a session's:
the in-memory history ([`history::HISTORY_EDITS`]) and which edits the app has already handed over — the
note only needs to be told about a tail of edits and the cursor it ends at.

## 11. Where the code is

| Module | Responsibility |
|---|---|
| `src/store.rs` | the database: schema, PRAGMAs, the batch write, compaction, the read path, the history's log, checkpoint, `VACUUM INTO`, and the rows of `meta` |
| `src/chunk.rs` | the blob format: encode, decode, integrity, chunk boundaries, the sealed frame a dirty row and an edit share |
| `src/history.rs` | what an edit *is*: apply and revert, a page's history, and the bytes the note's log holds |
| `src/settings.rs` | every setting a note remembers, and the row each one is written in |
| `src/note.rs` | the note folder, the writer thread and its job queue, the zip container |
| `src/ink.rs` | strokes in memory: what a stroke is, and where each page's ink is kept |
| `src/recent.rs`, `src/home.rs` | the index of what has been opened, and the list that shows it |
| `src/app.rs` | when to write: the batch clock, page close, the checkpoint rule, and the UI |

| Function | What it does |
|---|---|
| `chunk::encode` / `chunk::decode` | one batch of strokes → one blob, and back |
| `chunk::chunk_ranges` | where a page's ink is cut into chunks |
| `NoteStore::append` | a batch of strokes into `dirty_strokes`, one transaction |
| `NoteStore::compact` | dirty rows → chunks, in one transaction |
| `NoteStore::rewrite` | a page written again from scratch |
| `NoteStore::load` | one page's ink, in order |
| `NoteStore::summary` | what a note holds, without reading a blob |
| `NoteStore::export` | `VACUUM INTO` |
| `NoteStore::read_settings` / `set_settings` | the note's rows and the app's type: a setting's whole schema |
| `Note::placed_from` | import: a zip, a PDF, or a folder → a working copy |
| `NoteWriter::spawn` | the writing thread and its queue |
| `NoteApp::persist` | the rule that decides *when* ink is handed over |
| `NoteApp::load_page_ink` | the read that happens when you turn a page |
| `home::scan`, `recent::Recents::reconcile` | what a launch costs, and the list healing itself |

**Every claim in this document is checked by a test that fails if the behaviour changes.** The format is
pinned by `chunk::tests` (a round trip, a one-point stroke, byte-identical re-encoding, a damaged blob,
chunk tiling), the database by `store::tests` (append, compaction, rewrite, pages moving, damaged
chunks, exports, every way a `meta` row can behave), the container by `note::tests`, and the pen by
`settings::tests`.

---

## 12. Appendix: the schema, the PRAGMAs and the constants

### The schema

```sql
CREATE TABLE pages (
    id            INTEGER PRIMARY KEY,   -- identity, what chunks point at
    ord           INTEGER NOT NULL,      -- position in the note's reading order
    created_at    INTEGER NOT NULL,      -- milliseconds since the epoch
    updated_at    INTEGER NOT NULL,
    bbox_min_x    REAL, bbox_min_y REAL, bbox_max_x REAL, bbox_max_y REAL,
    stroke_count  INTEGER NOT NULL DEFAULT 0,  -- strokes in this page's *chunks*
    history_at    INTEGER NOT NULL DEFAULT 0,  -- the cursor: the last applied edit's ordinal
    history_count INTEGER NOT NULL DEFAULT 0   -- …and the page's stroke count when it was made
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

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;  -- one row per fact: see §3

CREATE TABLE bookmarks (               -- the pages a person marked: one row per marked page
    ord        INTEGER PRIMARY KEY,    -- the same position `pages.ord` is, renamed with it
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE edits (                   -- a page's history: one row per edit, in the order made: see §3
    page_id    INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    ord        INTEGER NOT NULL,       -- the cursor's ordinal space; never restarts, gaps at the bottom
    data       BLOB NOT NULL,          -- the edit, as ink: see §4 and src/history.rs
    created_at INTEGER NOT NULL,
    PRIMARY KEY (page_id, ord)
) STRICT;

CREATE INDEX idx_chunks_page ON chunks(page_id, stroke_start);
CREATE INDEX idx_dirty_page  ON dirty_strokes(page_id, seq);
```

`STRICT` means SQLite checks the declared type of every value, which is what keeps a `BLOB` column a
`BLOB` column. `ON DELETE CASCADE` is why deleting a page is one statement: its chunks and dirty rows go
with it.

**`bookmarks` is the one table that is not ink.** A mark names a *page* by its position, so it is renamed
with `pages.ord` when a page is inserted and deleted with the page it is on. It arrives in an older note
the way any table does — `CREATE TABLE IF NOT EXISTS` runs on every open — so the version stays `5` (§9).

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
default for the rest of its life. This one was found by a test, which is the reason the test asserts it.

### The constants

| Constant | Value | Meaning |
|---|---|---|
| `chunk::QUANTUM` | 64 | fixed-point units per logical pixel (1/64 px) |
| `chunk::CHUNK_RAW_TARGET` | 64 KB | arrays per chunk, before compression |
| `chunk::CHUNK_MAX_STROKES` | 512 | strokes per chunk, whichever comes first |
| (palette) | 255 | distinct colours per chunk, enforced by ending the chunk |
| `chunk::ZSTD_LEVEL` | 3 | the compression level the design settles on |
| `store::SCHEMA_VERSION` | 5 | what `PRAGMA user_version` says (§9) |
| `app::BATCH_INTERVAL` | 500 ms | how long ink may wait in memory |
| `app::BATCH_STROKES` | 200 | how much ink may wait in memory |
| `app::CHECKPOINT_QUIET_INTERVAL` | 5 s | how long the pen must be still before a checkpoint |
| `app::CHECKPOINT_INTERVAL` | 5 min | the least time between two checkpoints |
| `note::NOTE_DB` / `NOTE_PDF` / `ATTACHMENTS` | `note.db` / `source.pdf` / `attachments` | the three names inside a note folder |

The settings a note remembers are not listed here: `src/settings.rs` is the list, one row each (§3).
