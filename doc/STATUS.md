# How to read the status line

The status line is the pill that hangs under the bar, at the top left of the window. It is a readout, not
a control: nothing in it can be clicked, and it says what the open note and the page in front are made of.

This document is about **reading** it — what each clause means, and which figure to look at when something
feels wrong. For why it exists where it does, and why it is on a clock, see
[VIEW.md § The status line](VIEW.md#the-status-line). For what runs where, see [ARCHITECTURE.md](ARCHITECTURE.md).

If you read only one paragraph, read this one:

> **A clause that is absent is not a zero.** The line carries only what is true and worth saying, so a page
> with no marks has no `marked`, an unturned page has no `turned`, and a document that is not a PDF has no
> `pdf cache`. When a clause disappears you have reached the ordinary case, not a broken counter.

## 1. The shape of the line

The clauses run in one order, always the same, separated by `·`:

```
화학.pdf page 150/1218 · turned 90° · 2 selected · 3 back, 0 forward · 1 marked
· ink 3 strokes, 0 points (0 resampled, 0%) · 1 off the sheet
· pdf cache 87% (26 hit 4 miss) 3 render 1 placeholder 0 cancel, 122 MB
· view 158% · measuring: Cmd+M stops it · <the frame's own timings> · <vitals> · opened <path>
```

Read left to right, the line moves outward from the document to the machine:

| Order | What it answers |
|---|---|
| document, page | *what am I looking at* |
| turn, selection, history, marks | *what have I done to it* |
| ink, off the sheet | *what is on the page* |
| pdf cache | *is the page arriving cleanly* |
| view | *how is it shown* |
| session, timings | *how fast is it going* |
| vitals | *what is this process costing* |
| `opened`, `log: off` | *where did it come from, and did the log work* |

The line is rebuilt when the reader changes something, and once a second besides
(`STATUS_REFRESH_INTERVAL`), because the counters move on their own. It is also appended to `status.log` —
see §10.

## 2. The document and the page

### `<file> page <n>/<total>`

The file's name, the page in front, and how many there are. A note with no document reads `1/1`: one page
is still one page, rather than a count of nothing.

### `turned <degrees>`

**Only when the reader has turned the page.** `turned 90°`, `180°`, `270°`. The right way up is the ordinary
case, so it says nothing — the same rule the marks follow. If a page looks sideways and this clause is
absent, nobody turned it: the PDF itself is sideways.

### `<n> selected`

**Only when a lasso has hold of something.** It answers *what would Delete take?* — the count of strokes
currently selected.

### `history <n> back, <m> forward`

**Only when there is a history.** How far back and forward the undo stack reaches. This is the **session's**
memory, which is shallower than the note's own log on disk: `Undo` can run out while the note's history is
still deeper. A page that has just been opened reads nothing here.

### `<n> marked`

**Only when there are marks.** How many pages are bookmarked. The answer to *which pages did I keep*.

## 3. The ink

### `ink <n> strokes, <m> points (<r> resampled, <p>%)`

- **strokes** — how many strokes the page in front holds.
- **points** — how many points those strokes kept, *after* resampling.
- **resampled / %** — how many points came from resampling, and what share of the kept total they are.

The percentage is the one number worth watching: a capture that batches coarsely shows up here as a high
resample share, because the pen handed over few, far-apart readings and the ink had to be smoothed into a
line. A low share means the ink is close to the raw capture.

### `<n> off the sheet`

**Only when ink landed beside the page.** Readings that arrived outside the sheet's bounds and were dropped.
If a line stops at the edge of the paper without explaining itself, this clause is the explanation: the pen
was still moving when it left the page.

## 4. The page cache

### `pdf cache <ratio> (<h> hit <m> miss) <r> render <p> placeholder <c> cancel, <mb> MB`

**Only when a document is open.** An unfamiliar clause, and the one that earns its place: a 0% hit ratio and
a 99% one look identical from the outside, and *placeholder* is what the eye actually saw.

| Figure | Meaning |
|---|---|
| `ratio` | share of page requests answered from the cache; `—` when nothing has asked yet |
| `hit` / `miss` | requests served from memory / requests that needed a rasterise |
| `render` | pages actually rasterised by Pdfium |
| `placeholder` | frames drawn with no bitmap ready — **a soft page, visibly blank or blurry** |
| `cancel` | renders abandoned because the reader moved on before they finished |
| `MB` | memory the cached bitmaps are holding |

**Read `placeholder` and `cancel` together.** A high `placeholder` count with a high `cancel` count means the
reader is flipping faster than pages can render — an ordinary cost of speed. A high `placeholder` count with
a *low* `cancel` count means rendering is genuinely slow, and the timings further right will say where.

## 5. The view and the session

### `view <percent>`

The zoom the sheet is drawn at. `view 158% (locked)` when the zoom lock is on — the lock is the reason a
pinch or a *Fit* did nothing, and this is where it says so rather than leaving it looking like a bug.

### `memory Lean (<mib> MiB cache, <px>)`

**Only when the Lean memory mode is on.** `Room` is what the app always did and naming it every second
would be noise; `Lean` is a state a reader chose for a reason and may need reminding of.

Turning it on trades detail for memory, all at once: pages render at half the scale (one bitmap pixel per
logical pixel instead of per screen pixel), no page is rasterised above 1536 px, and the cache may hold
32 MiB rather than 96. Measured on a 1218-page chemistry textbook, that is **484 MiB → 272 MiB** of
resident memory, and the status line above is where those two figures are compared.

The three numbers in the clause are the mode's own: what it allows the cache, and the widest bitmap it
will ask for. They are here rather than in a settings screen because they explain the two things a
reader notices — a page that softens as they zoom past 150%, and a `MiB` figure that stopped climbing.

### `measuring: <key> stops it`

**Only while a measurement session is running.** A session is a stopwatch: it is started and stopped by hand
with the same key (`secondary-m`, which is `Ctrl+M` on Windows and `Cmd+M` on the Mac), because starting and
stopping are the same gesture a moment apart. While it runs the line says only *that* it is running.

### `measured <seconds> s: frames <n> · <fps> fps · <last> (<worst>) · pen→app <ms> · <n> · <last> (<worst>)`

**Only after a session has been stopped**, and in front of the live timings rather than instead of them: this
is the stretch that was measured, and the timings to its right are what is happening now.

- **frames** — how many frames were seen, the rate they worked out to, and the most recent interval with the
  worst interval beside it in brackets.
- **pen→app** — how long a reading from the pen waited before it was acted on.

The rate is in frames per second because that is the form a stutter is read in: *118 fps, slowest 41 ms
apart* says both the pace and what interrupted it.

## 6. The timings

```
ink 0.00 (0.00)  render 1.54 (1.54)  canvas 0.61 (0.67)  pdf page 4.92 ×1, slice 3.29 ×4
·  pump 8.74 ms (114 /s)  ·  present 8.60 ms (116 /s)  ·  pen→app 0.44  ·  2 strokes 32 px
```

Every `a (b)` pair reads **most recent (worst since this was last read)**. A mean would hide exactly the
frames worth looking at, so this line reports the last sample and the worst one — and the worst is cleared
as it is read, so it answers "what has happened since I last looked".

| Clause | What it is |
|---|---|
| `ink` | time to build the ink for a frame |
| `render` | time to build a frame's draw data |
| `canvas` | time to hand that to the renderer |
| `pdf` | PDF work, split by kind: `page <ms> ×<count>` for whole pages, `slice …` for the few-millisecond slices paid between frames. The two are never averaged together — a page and thirty-seven slices are the same mean and nothing like each other |
| `pump` | the mean gap between housekeeping wakes, and how many per second |
| `present` | the mean gap between frames handed to the screen, and how many per second |
| `pen→app` | the mean wait between a reading arriving and being acted on |
| `<n> strokes <m> px` | what a frame is actually painting: strokes and vertices, with `(<n> culled)` when some were dropped |

`—` means nothing has been measured on that path yet, which is not a zero and not an error.

## 7. The machine

### `<mib> MiB · <cpu>% cpu · <n> threads`

What the process is holding and burning. Resident memory, whole-CPU percentage, thread count. This is the
figure to compare against Activity Monitor or Task Manager — it is in mebibytes (1024²) precisely so that it
agrees with them rather than being five percent off.

A process with a single thread omits the thread count. On a platform with no counters the whole clause is
absent, so the line keeps its shape.

## 8. The ends of the line

### `opened <path>`

The file the note was read from. Still here after the note is renamed, because a note that was renamed still
came from somewhere.

### `log: off`

**Only when the status log could not be opened.** The app still works; this is the only place the failure
would otherwise be seen.

A message is **not** the status line's own clause set: it replaces the previous message and stays until
the next one. The clauses in §1 keep describing the note in front regardless.

## 9. Reading `CHEAP_NOTE_TRACE`
Set `CHEAP_NOTE_TRACE` in the environment and the line grows a few more clauses. None of them is a fact about
the note — they are what the platform's own capture says about itself — which is why they are behind the
variable, and why the line is otherwise the same sentence on every OS.

| Clause | What it means |
|---|---|
| `<text>  <n> readings/message` | how the capture is attached, and how coarsely it batches |
| `pen: <axes>` | which axes the digitizer reports; `position only` when it reports nothing else |
| `no pointer hook` | the system pointer could not be hooked — the window still works, but shows two cursors |
| `lean <x>,<y>` | the live pen tilt: the number the ghost cursor is drawing, and where you check that the cursor tells the truth about which way the pen leans |

## 10. Reading `status.log`

The same line is appended to a file beside the notes, with a UTC timestamp and a session header:

- **macOS** — `~/Library/Application Support/cheap-note/status.log`
- **Windows** — `%LOCALAPPDATA%\cheap-note\status.log`

A file past 8 MB is rotated to `status.log.1` on the next launch. There is no copy button: the log is how a
measurement leaves the app.

```sh
# what the line said over the last minute
tail -20 ~/Library/Application\ Support/cheap-note/status.log

# the worst frame waits recorded in this log, largest last
grep -o 'present [0-9.]* ([0-9.]*)' ~/Library/Application\ Support/cheap-note/status.log \
  | sort -t'(' -k2 -n | tail
```

## 11. What to look at when something feels wrong

| Symptom | Look at | What it means |
|---|---|---|
| The page is slow to appear | `pdf cache` → `placeholder` vs `cancel` | many placeholders with few cancels: rendering is slow. Many of both: you are outrunning it, which is ordinary |
| A frame stutters while writing | timings → `render`, `canvas`, and the `(worst)` in brackets | which stage of the frame overran |
| Choppy, even though the frame timings look fast | `present` rate against your display's rate | the frame cap or vertical sync, not the drawing |
| A line stops at the edge of the paper | `off the sheet` | the pen left the page while drawing |
| Ink looks smoother than it should | `ink` → `resampled %` | the capture batched coarsely and the ink was smoothed |
| Undo refuses to go further | `history` → `back` | the session's memory, not the note's log on disk |
| A pinch or *Fit* did nothing | `view` → `(locked)` | the zoom lock is on |
| The machine swaps while a big PDF is open | `memory` → `Lean` | the note is spending what full detail is worth; turn Lean on |
| A page softens when zoomed past 150% | `memory` → `Lean` | the lean mode's 1536 px ceiling, working as chosen |
| The pen draws in the wrong place | `CHEAP_NOTE_TRACE` → `lean`, `pen: <axes>` | the capture's own report; compare the lean against the ghost cursor |
| Two cursors on screen | `CHEAP_NOTE_TRACE` → `no pointer hook` | the system pointer could not be hooked |
| A clause you expect is missing | §1 above | absent means zero or ordinary, not broken |

## 12. Measuring memory across documents, and the ink

The status line says what this process holds *now*. To ask what opening and closing documents
*costs* — the question behind "it climbs when I open PDFs" — and whether the ink and the note give their
memory back, there is a separate command:

```sh
cheap-note --memory-report [<note-folder> ...]
```

No window opens. Each note is opened, measured, closed, and measured again, all in **one process**,
because two processes would answer a question about the operating system rather than about the app.

```
                               RSS     delta     cache       gpu  pages
baseline                    11 MiB         —      0 MB      0 MB      0
open 화학-7f07d9fa            80 MiB   +69 MiB     11 MB      0 MB      1
close 화학-7f07d9fa           63 MiB   -16 MiB      0 MB      0 MB      0
ink: 16 pages written       64 MiB    +1 MiB      0 MB      0 MB      9
ink: turned back through    64 MiB    +0 MiB      0 MB      0 MB      9
ink: dropped                64 MiB    +0 MiB      0 MB      0 MB      0
ink: 400 strokes drawn      75 MiB   +11 MiB      0 MB      0 MB      0
ink: cleared, 0 left        75 MiB    +0 MiB      0 MB      0 MB      0
ink: undone, 400 back       75 MiB    +0 MiB      0 MB      0 MB      0
ink: page dropped           75 MiB    +0 MiB      0 MB      0 MB      0
ink: 6 zoom rungs           76 MiB    +1 MiB      0 MB      0 MB      1
ink: page dropped           76 MiB    +0 MiB      0 MB      0 MB      0
note: 16 pages written      79 MiB    +2 MiB      0 MB      0 MB      0
note: closed                79 MiB    +0 MiB      0 MB      0 MB      0
end                         79 MiB    +0 MiB      0 MB      0 MB      0
──────────────────────────────────────────────────────────────────────
peak 80 MiB   final 79 MiB   growth +68 MiB   ok
```

- **`RSS`** — resident memory, read from the same counters the status line uses.
- **`delta`** — against the row above. A `close` that reads `-16 MiB` gave memory back; one that reads
  `+0 MiB` did not.
- **`cache`** / **`pages`** — what the page cache holds, and how many bitmaps. Both go to zero on a
  close, which is what makes a `close` row worth reading: the cache emptied, so any resident figure
  still standing is something else. On an `ink:` row `pages` is the **note's** page count instead, which
  is bounded by `ink::PAGES_KEPT + 1` — a figure that climbed there would be the page cache leaking.
- **`gpu`** — the graphics ledger (`IOSurface` on macOS), when the platform can report it.

**The ink and note rows run whether or not a note is given**, because they need none, and the ink is the
larger half of what the app holds. They drive the scenarios a leak would have to show up in: writing past
the page bound and turning back through it (`PAGES_KEPT`), clearing and undoing a page (`HISTORY_EDITS`),
walking the zoom rungs, and writing a note through the writer thread.

**The verdict is two figures, not one.** Documents are judged `baseline` → last `close` against 64 MiB; the
ink is judged from where the scenarios started to `end` against 32 MiB. The run passes only if both do.
Their legitimate costs are different sizes — Pdfium's fixed per-process cost against a page of derived
geometry — and summed they straddle a single allowance, which made the verdict flip with the document and
not with its size. On the local notes every one passes with room: documents 42–53 MiB of 64, ink 14 of 32.
The reasoning and the tripwire tests are in [`CLI.md`](CLI.md), "The two halves".

**Exit code** — `0` when both halves came back within their allowances, `1` when either did not (or the build
cannot measure at all), `2` when the command could not run. That is what makes it usable in CI.

On Windows the same command runs through `apps/windows/src/platform/report.rs`. A release image is a
windows-subsystem program, so it attaches to the console that started it (or uses redirected stdout);
with no console it writes `cheap-note-memory-report.txt` beside the executable, then to the temporary
directory. The `gpu` column is `—` there because Windows has no `surface_bytes` implementation.

### What it found

Measuring the 1 218-page chemistry textbook: memory used to grow **with the document's size** —
130 MiB left behind for a 62 MB PDF, 63 MiB for a 14 MB one — because the document's bytes were read
into the heap, and macOS does not return a freed large block to the system. Mapping them instead (see
`note_format::mapped`) took the 62 MB case to 70 MiB and the 14 MB case to 35 MiB, and a document
opened twice now costs nothing the second time.

Measuring the ink half found the thing the PDF half could not: a note held **every page ever written**
in derived geometry, at 71x its readings, so opening a long note cost memory in proportion to how much
had been written on it. `ink::PAGES_KEPT` (8 pages, LRU) fixed it, and the report now shows the count
stopping at 9 rather than climbing with the pages written — 52 MB at 10, 20 or 40 pages instead of
52/104/208, and 294 MB at 8x zoom instead of climbing again.

### What it cannot see

A headless run renders bitmaps but never presents them, so the `gpu` column reports what the process
allocated rather than what a compositor pooled. Read it for its direction, not its size.

**Resident memory cannot say whether a specific allocation was freed** — a freed block stays resident and
the next one reuses it, so a page that gave its 12 MiB back reads `+0 MiB`. What the report is good at is
the question that matters: does a figure *climb and stay climbed* by the `end` row. It was measured that
way once: 156 MB of live ink read as `+0.1 MiB` in `ps`.

### Checking with `leaks` rather than with the table

The checker that ships with the instrument settles the question outright:

```sh
leaks --atExit -- ./target/release/cheap-note --memory-report
leaks --atExit -- ./target/release/cheap-note --memory-report "$HOME/Library/Application Support/cheap-note/notes/<note>"
```

What it found:

- **An ink-only report: 0 leaks for 0 bytes.** The page cache, the undo history, the zoom rebuilds and the
  writer thread all give their memory back.
- **A report that opens a note: 2 leaks for 208 bytes**, and the stack is `report::Options::parse` →
  `RawVec<String>` growing — the command line's own path strings, which are alive until `process::exit`.
  That is argument memory, not the document. Three notes in one run report 4 leaks for 400 bytes, so it
  follows the arguments and not the pages.

A binary built with `--profile release-sym` is what makes that stack readable; `[profile.release]` strips
symbols on purpose, and a stripped binary's leak report names addresses only.


The commands' full contract — arguments, exit codes, the shape a script can read, and what a new
platform has to provide — is [`doc/CLI.md`](CLI.md).


## Where this document and the code part company

The clause order and the exact wording of each clause live in `compose_status` in
`crates/note-ui/src/app.rs`; the timing and machine clauses are spelled by `Timings::summary` in
`crates/note-ui/src/timing.rs`, and the cache clause by `PdfStats::summary` in `crates/note-ui/src/pdf.rs`.
This document describes them. If the two disagree, the code is right.

### `--bench --max-ms` — the frame budget, and what it deliberately does not gate

`cheap-note --bench --max-ms 8.33` gates on `present` and exits `1` when a row exceeds it. Verified on
macOS: `8.33` → `ok` / `0`; `3.0` → `FAILED — over the 3.00 ms budget: 8182 strokes present 5.55` / `1`;
`--max-ms abc` → exit `2` with the usage.

**It does not gate `frame` or `rebuild`, and this is a finding rather than a shortcut.** Both are timed
from the start of a draw, whose first act is to ask `CAMetalLayer` for a drawable — a call that waits for
the display. Measured, `rebuild` minus `warm` is −0.09 ms at 0 strokes and 18.2 ms at 8 182, and
`rebuild` reads 8.2 ms on an empty page. A 120 Hz budget on either figure would therefore fail the app
for the panel's clock.

So the honest scope is: **`--max-ms` is a GPU-work gate, not a latency gate.** A frame that meets it can
still miss a tick, because reaching the glass within 8.33 ms also depends on the display's rate and the
compositor. `frame` remains the figure to read for latency and is reported ungated.

The remaining budget flags (`--max-joules-per-op`, `--min-ops-per-joule`, `--require-energy`) are still
documented as a future contract under `--power-report` and are not parsed; `doc/CLI.md` says so.



