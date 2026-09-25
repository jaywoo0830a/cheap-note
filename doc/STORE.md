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

### What the app remembers about what you opened

Beside the notes folder there is one small file, and it is what the app starts on:

```text
%LOCALAPPDATA%\cheap-note\recent.json      what you have opened, newest first
```

It holds one entry per note the app has been asked to open: the note's *folder* — which is what an
entry opens — the file it was made from, its name, the sheet, how much is in it, and when it was last
opened. The file it came from is a *convenience* and nothing more: a note carries its own copy of the
document, so an entry whose original file has moved is still a whole note.

It is a **cache, not a note**, and it is treated like one:

* deleting it costs the order of a list and nothing else — every note it names is still a folder under
  `notes\`;
* a folder it has never heard of — a note carried in from another machine — is *adopted* by simply
  being there, so the list heals itself;
* an entry whose folder has gone is marked rather than removed, because the note may be one directory
  away (and the list says "not on disk" instead of quietly forgetting).

One thing in the file is *not* a cache: the **folders taken out of the list**. *Forget this row* takes a
line out of the list and touches no note, and the note is still a folder under `notes\` — so adoption
would put the row straight back if the entry were all that was removed. The folder is therefore written
down as taken out, and adoption skips it. Opening the note again is the way back, because that is a person
asking for it and no scan can ask on their behalf. A folder that is no longer on disk is dropped from that
record, since only a folder that is *there* can be adopted — which is what keeps it short. Deleting
`recent.json` therefore costs the order of the list and the record of what was taken out of it, and
nothing else.

The counts are cached together with a **stamp** of the note's database — its size and its modification
time — so drawing the list normally costs one `stat` per note rather than one database read. Only a
note whose database has changed since its stamp is read again, on a worker thread, and its row fills in
when the answer arrives. That is what makes the list cheap however many notes there are: §5's numbers
are a note's *size*, and the size is in its pages, not in its ink.

The index is deliberately *not* kept inside a note — it is a list of *every* note, so no one of them
could hold it — and it holds no settings, because a setting is a property of a note and there is
nowhere left to keep a global one (§3). It is a cache of the notes folder, kept beside the notes
folder, and deleting it costs the order of a list and nothing else.

### The name a person gives a note

A note can be *named* — and the name lives in the note, in the same `meta` table as its open page and
its paper size, under the key `title`:

```text
note.db  →  meta('title') = "3장 요약"      what the note is called (optional, UTF-8)
```

It is **what a note is called, never what it is**. The folder keeps the name it was made with — a
digest of the path it was imported from, or the moment a blank sheet was created — because that name
*is* the note's identity: it is what the index keys on, and what makes importing the same PDF twice
find the same note rather than a second copy of it. Renaming therefore writes one `meta` row and
touches nothing else: no ink, no folder, no page.

Writing it there rather than in the index is the point of the whole cache argument above: a folder
carried to another machine arrives with its name already on it, and deleting `recent.json` costs the
order of a list rather than the names of everything a person wrote.

**An empty name is a legitimate answer and the normal state.** Most notes have none, and the list
derives one — the document's name first, then the folder's name with its digest taken off. That is
why `title` may be absent, and why the app never *requires* a name to write one.

**Where a name is given.** Two places, one word. In the list, *Rename* on the row's right-click menu
puts a field in the row; on the sheet, double-clicking the note's title — the name at the left of the
bar — turns it into a field in place. Both end in the same write: the note is renamed *first*, and the
index is then told what the note answered, never the other way round. Enter keeps the name, Escape
leaves it as it was.

What is written is a *rule* rather than a validation (§ `store::normalize_title`): the text is
trimmed, a newline becomes a space, runs of whitespace collapse, and the result is cut to 64
**characters** — characters rather than bytes, so that a name in Korean is not cut twice as short and
never in the middle of a syllable.

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
| `meta` | one fact and its value | **everything else the note knows**: which page was open, the page list, the sheet the ink is in, the document it came from, the name a person gave it (§1), and every setting there is to change (below) |
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

### What the note's `meta` holds

`meta` is where everything that is *about the note* is filed — **one row per fact, named after the
fact, with its value as text**. The ink paths never touch it: the app reads it when a note is opened,
and writes it back whenever something changes.

```text
note.db → meta('open_page')    = "7"                       the page that was open
          meta('sheet_width')  = "685.71429"               the sheet the ink's coordinates are in
          meta('sheet_height') = "970.0"
          meta('layout')       = ["blank",{"document":3}]  what each page shows, in reading order
          meta('canvas_size')  = "A5"                      …and every setting, one row each
          meta('max_width')    = "4.5"
          meta('title')        = "3장 요약"
```

| Row | What it holds |
|---|---|
| `open_page` | the page that was open, so reopening comes back to it — a whole number |
| `sheet_width`, `sheet_height` | the sheet the ink's coordinates are in — two numbers, written together |
| `layout` | what each page shows, in reading order — JSON, because it is a *list* and the only one |
| `title`, `document`, `source` | what the note is called, the file it was made from, the file it was placed from (§1) |
| the settings | one row each, named after the setting — `src/settings.rs` lists them, and this document does not duplicate the list |

**The rows *are* the schema, and that is the whole point of them.** There is no packed blob to decode
and no stored shape for a struct to match, so a fact can be added to a note without anything being
migrated — and the three ways a row can fail to be there are three different answers:

* **a row the app does not know is never read, never written and never deleted**, so a note written by a
  build that knew more settings — or one somebody added a row to by hand — keeps them. Adding a setting
  is four small edits (a field and its default in `src/settings.rs`, one line each in
  `NoteStore::read_settings` and `NoteStore::set_settings`); *removing* one is the same four edits in
  reverse, and every note that had it is simply left alone;
* **a row that is not there is not a failure**: the note does not say, and the app's own answer stands.
  That is what makes a note written by a build that knew fewer settings open with the shipped ones, and
  what lets a PDF just placed continue the sheet it was placed on (§7);
* **a row that is there and cannot be understood *is* a failure**, reported to the person. Something
  wrote it, and quietly replacing an answer is how a setting disappears without a word.

**Every setting a person can change lives in this table.** The paper, the ruling, the colours, the pen
and its weight, the zoom, whether a document is shown in grey, how the pen *feels* in the hand — the
widths it answers pressure between, the resampling, the smoothing, the eraser's reach — whether the bar,
the status line and the ghost cursor are shown, and where this note was last written out. There is no
settings file any more, and nothing about a note is remembered anywhere but in the note: a sketchbook in
grid written with a marker and a diary in rules written with a fine pen open as themselves, and neither
hands the other its sheet, its pen or its switches.

**The pen is in two kinds of row, on purpose.** `pen_weight` says *which* pen — `Fine`, `Light`,
`Normal`, `Bold`, `Heavy` — while `min_width`, `max_width` and `no_pressure_width` say how *any* pen
answers a hand: the width at the lightest touch, the width at the heaviest press, and the width for a
pen with no sensor at all. The weight is a multiplier on those numbers rather than a width of its own,
which is what keeps the two from disagreeing: a marker is fat at the lightest touch *and* at the
heaviest, and no weight can make a line that thins as it is pressed. What is already written is untouched
by any of it — every point carries the width it was drawn at, exactly as it carries the colour it was
drawn in.

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

* the note's own facts from `meta` — the page list, which page was open, the sheet the ink was written
  on, the document's name, and every setting the note remembers (§3);
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

**Nothing reads them any more.** A note used to be a zip holding `document.pdf` and a `notes.json` —
every point of every stroke as text — and both the reader for that shape and the migration that turned
one into a store have been deleted. An old note is now the same case as any other zip that is not a
note: there is no `note.db` in it, so it is refused by name rather than unpacked (§10).

The refusal is part of the deletion rather than an accident of it. A zip unpacked "in case" would leave
a folder of someone else's files in the notes folder, and a note half-imported is a worse thing to
explain than one that was refused.

## 10. Versioning and integrity

| Mechanism | What it protects against |
|---|---|
| `PRAGMA user_version` (this build writes `5`) | a note written by *any* other build — newer or older: refused, with a message that says which way round the difference is |
| the zip's entry list | a file that is not a note at all: named as such rather than half-read |
| the CRC32 of every blob | a damaged disk or a truncated file: reported as damaged |
| the uncompressed length of every blob | a blob that decompresses to the wrong size |
| the stroke count every blob is filed under | a blob that is intact and is *not the chunk it is filed under* — the case where guessing would write a corrupted page over a good one |
| an unknown `codec` id | a compressor a newer build knows and this one does not |

The version number continues the note format's own history rather than starting over: `1` was the
first zip of JSON, `2` added the page list to it, `3` is the database, `4` moved the paper, the ruling,
the colours and the zoom out of the app's settings and into the note, and `5` finished the job — every
row of `meta` is a fact of its own in text, and the last settings file is gone (§3). So one number
orders every note file this app has ever written, whichever shape it is in.

**A version number covers the *tables* and the *meaning of a row we read*; it no longer covers adding
one.** With rows instead of a blob, a new setting is not a new schema: a note that does not mention it
simply takes the shipped answer (§3). The number moves when a row's shape changes or a table does —
which is rare, and is exactly the kind of change that *does* need both builds to agree.

**Only the current number is read.** A note is opened by the build whose schema wrote it and by no
other: an older note is refused exactly as a newer one is, because this build would write its own
shape back over a shape it does not know. Refused means *left alone* — the stamp is not brought
forward — so the build that wrote the note can still open it afterwards. The message names the number,
so that "open it with the build that wrote it" is advice a person can act on rather than a guess.

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
| The note remembers its page list, open page, sheet and name | `store::tests::a_note_remembers_its_own_page_list` |
| Two notes keep two different answers to every setting | `store::tests::two_notes_keep_their_own_answers` |
| A row this build does not know is left exactly as it was found | `store::tests::a_row_this_build_does_not_know_is_left_alone` |
| A row that is not there keeps the answer in hand | `store::tests::a_missing_row_keeps_the_answer_in_hand` |
| A row that cannot be understood is reported, not replaced | `store::tests::a_row_that_is_not_a_number_is_reported` |
| Half a sheet says nothing | `store::tests::half_a_sheet_says_nothing` |
| The note says which pen and the tuning says how it answers pressure | `settings::tests::a_note_weighs_the_pen_and_the_tuning_shapes_the_line` |
| Every pen has a name of its own, and the name leads back to it | `settings::tests::a_label_leads_back_to_its_weight` |
| Checkpointing folds the log back and the ink is still there | `store::tests::checkpointing_folds_the_log_back` |
| A note from a newer build is refused | `store::tests::a_newer_note_is_refused` |
| A note from an older build is refused, and left as it was found | `store::tests::an_older_note_is_refused` |
| A note travels as one file and comes back whole | `note::tests::a_note_travels_as_one_file` |
| Attachments travel with the note | `note::tests::attachments_travel_with_the_note` |
| The writer thread writes what it is given and reports its export | `note::tests::the_writer_writes_on_its_own_thread` |
| A PDF becomes a note with that document | `note::tests::a_note_starts_on_a_document` |
| A zip that is not a note is refused — an old note zip among them | `note::tests::a_foreign_zip_is_refused` |

## 12. Where the code is

| Module | Responsibility |
|---|---|
| `src/store.rs` | the database: schema, PRAGMAs, the batch write, compaction, the read path, checkpoint, `VACUUM INTO`, and the rows of `meta` |
| `src/settings.rs` | every setting a note remembers, and the row each one is written in |
| `src/chunk.rs` | the blob format: encode, decode, integrity, chunk boundaries |
| `src/note.rs` | the note folder, the writer thread and its job queue, the zip container |
| `src/ink.rs` | strokes in memory: what a stroke is, and where each page's ink is kept |
| `src/recent.rs` | the index of what has been opened: the file beside the notes, and its rules |
| `src/home.rs` | the start screen: the list of recent notes, and what a confirmation opens |
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
| `NoteStore::summary` | what a note holds, without reading a blob |
| `NoteStore::export` | `VACUUM INTO` |
| `NoteStore::read_settings` / `set_settings` | the note's rows and the app's type, in one line per setting: a setting's whole schema |
| `Note::placed_from` | import: a zip, a PDF, or a folder → a working copy |
| `NoteWriter::spawn` | the writing thread and its queue |
| `NoteApp::persist` | the rule that decides *when* ink is handed over |
| `NoteApp::load_page_ink` | the read that happens when you turn a page |
| `home::scan` | what a launch costs: one `stat` per note, and a read only for the ones that changed |
| `recent::Recents::reconcile` | the list healing itself around the notes folder |

---

## 13. What is deliberately *not* stored

Knowing what a store refuses to hold is as useful as knowing what it holds.

| Not stored | Why |
|---|---|
| the stroke under the nib | it is not history yet: it has no final geometry, it is not what a save writes, and taking it back would leave the model thinking the pen was lifted |
| undo/redo history | it is a property of the session, not of the note. A note opens with a page of ink and no way back through last week's strokes |
| an erased stroke | the eraser removes whole strokes, and undo takes back the most recent *surviving* stroke. There is no "recover what I erased" — see `src/ink.rs` |
| anything global | there is nothing global to store. Every setting a person can change is a row of the note it was changed in (§3), so a note carried to another machine arrives the way it was left, and no file beside the program can disagree with it. The one file outside a note is the index of what has been opened, and that is a *cache* of the notes folder rather than an answer to anything |
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

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;  -- one row per fact: see §3

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
| `store::SCHEMA_VERSION` | 5 | what `PRAGMA user_version` says (see §10) |
| `app::BATCH_INTERVAL` | 500 ms | how long ink may wait in memory |
| `app::BATCH_STROKES` | 200 | how much ink may wait in memory |
| `app::CHECKPOINT_QUIET_INTERVAL` | 5 s | how long the pen must be still before a checkpoint |
| `app::CHECKPOINT_INTERVAL` | 5 min | the least time between two checkpoints |
| `note::NOTE_DB` / `NOTE_PDF` / `ATTACHMENTS` | `note.db` / `source.pdf` / `attachments` | the three names inside a note folder |

### A note about the database's own files

While a note is open you will see `note.db-wal` and `note.db-shm` beside it. That is normal, and it
is the whole reason a write does not stop a read. The log is folded back into `note.db` when the app
has been idle (§6), and always before a note is exported — so an exported file never has one.
