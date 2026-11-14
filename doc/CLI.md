# The measuring commands

Two things about this app cannot be seen by using it: whether it *gives memory back*, and whether a
frame is *fast enough*. Both are questions with numeric answers, and both are answered by commands
rather than by looking at a window.

This document is the contract for those commands: what they accept, what they print, what they exit
with, and what a platform has to provide to run them. It exists because the answers are meant to be
read by a machine — a CI job, a script, an agent — as much as by a person, and a contract that lives
only in the code drifts.

## Why a contract at all

An agent or a CI job needs three things from a measuring command, and none of them is a pretty table:

1. **A stable exit code.** Pass or fail, with no interpretation.
2. **A stable shape.** The same fields, in the same order, whatever the platform — so a script written
   against macOS runs unchanged on Windows.
3. **A clear absence.** When a platform cannot measure something, it says so rather than printing a
   zero, because a zero is a measurement and an absent one is not.

The human-readable table stays. `--format json` is added beside it for the three above.

## `cheap-note --memory-report`

Measures what opening and closing documents costs the process.

```sh
cheap-note --memory-report <note-folder> [<note-folder> ...]
cheap-note --memory-report --format json <note-folder> [<note-folder> ...]
```

No window opens. Every note is opened, measured, closed, and measured again **in one process** —
two processes would answer a question about the operating system rather than about the app.

### What each column means

| Field | Meaning |
|---|---|
| `rss` | resident memory, from the same counters the status line reads |
| `delta` | against the row above; a `close` that reads `+0` gave nothing back |
| `cache` | what the page cache holds, in bytes |
| `gpu` | the graphics ledger when the platform can report it, else `null` |
| `pages` | how many bitmaps the cache holds |

`cache` and `pages` going to zero on a `close` is what makes that row worth reading: the cache
emptied, so any resident figure still standing is something *else* holding on.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | every note came back within the allowance |
| `1` | at least one did not |
| `2` | the command could not run — no paths, or a note that would not open |

Three codes rather than two because "the app leaked" and "the command was wrong" are different
problems, and a CI job wants to tell them apart. A run that cannot measure at all (a build with no
process counters) exits `1`, not `0`: a pass it did not earn is worse than a failure.

### The allowance

`LEAK_TOLERANCE_BYTES` (64 MiB) is sized for **Pdfium's fixed per-document cost** — 30–50 MiB
whatever the document's size — and not for a leak. A real leak is proportional to the document:
before the fix, a 62 MB PDF left 130 MiB behind. The allowance sits between the two, so a fixed
cost passes and a proportional one fails.

## `cheap-note --bench`

Measures what a frame of the canvas costs.

```sh
cheap-note --bench [--format table|json] [--strokes 0,29,316,1000,8182] [--frames 30]
```

**One command line on both platforms.** macOS and Windows each implement the drawing half behind their
own seam and hand the same figures to the same report; the contract is what makes the two tables
comparable. `cheap-note-bench` still exists on Windows as a thin wrapper, because it used to be the only
way in and a script may call it.

A page of synthetic strokes is drawn into an offscreen surface — an `NSView` with a Metal layer on macOS,
a hidden `STATIC` window on Windows. On macOS the frame is **waited for** rather than merely queued:
`draw` commits and returns, so `settle` is what turns "queued" into "paid". That is the reason the two
`present` columns are not comparable row for row — see the note below.

For each stroke count:

| Field | Meaning |
|---|---|
| `first` | the first frame, which builds every realization the page needs |
| `warm` | the mean of the frames after it, where every realization is already baked |
| `worst` | the slowest warm frame — a mean hides the one frame a hand would feel |
| `rebuild` | one frame after a detail rebuild, which invalidates every realization |
| `pan` | one frame with the sheet moved, which must rebuild nothing |
| `present` | the GPU's share, timed apart from the drawing — see the note below |

`frame` is the warm draw plus the present — what one frame costs while the page is being panned or
written on. A `present` much larger than the draw is the compositor and not the renderer, and at
`Present(0)` it is a cost the app cannot remove by drawing faster.

### The one figure that is not the same shape on both platforms

Windows times `present` as the cost of *handing* the frame to the compositor. macOS's `settle` waits for
the GPU to finish everything the frame queued, because Metal's command buffer does not separate the
present from the drawing. So macOS's `present` is "all the GPU did for this frame" and Windows's is "the
hand-off", and **`frame` is the figure to compare across platforms**, not `present`.

The macOS table says so on its own summary line rather than leaving a reader to find out.

The default counts are 0, 29, 316, 1 000 and 8 182 strokes. The first three are the ones the older
measurements used, and 8 182 is the page the macOS figures in `doc/WINDOWS.md` were taken on — it is in
the list because the question this benchmark exists for is whether a platform is draw-bound at that
page size.



## What a platform has to provide

Neither command is a platform feature that some platforms have and others lack. They are **one
portable core** (`crates/note-ui`) plus a small set of measurements a platform supplies. A platform
that supplies none of them still builds, still runs, and says so.

| Need | macOS | Windows | Linux (none yet) |
|---|---|---|---|
| resident memory | `task_info`, `TASK_VM_INFO` | `GetProcessMemoryInfo` | — |
| graphics ledger | `IOSurface` via `task_vm_info` | none — the column is `—` | — |
| a console to print to | inherited `stdout` | attach to the parent console, or a file | inherited `stdout` |
| frame timing | Metal, waited for with `waitUntilCompleted` | Direct2D + swap chain | — |

The graphics column is the model for how to be honest: Windows **has no** `surface_bytes`, so the
field is `null` / `—` rather than `0`. A zero would be a claim about an empty GPU.

## Adding Linux

Linux is not a rewrite; it is four small pieces behind seams that already exist.

1. **Counters.** Implement `platform::SystemStats` in a new `apps/linux/src/platform/vitals.rs`:
   resident memory from `/proc/self/statm`, or `smaps_rollup` for a finer figure. The graphics ledger
   has no portable answer — leave `surface_bytes` at its default `None` and the column reads `—`. This
   alone makes `--memory-report` work.
2. **A console.** Nothing to do: a Linux process already has one.
3. **The benchmark.** `cheap-note-bench` should be a console binary from the start and needs none of
   the console-attaching dance Windows needs. Implement the drawing half in
   `apps/linux/src/platform/bench.rs`; the shared runner in `note-ui` does the arithmetic.
4. **The workspace.** `apps/linux` is a workspace member like the other two, so
   `cargo test --workspace` builds and tests it on **every** platform — the same way the Windows stub
   is tested today.

Nothing in `note-ui` should change for any of that. If it has to, the seam was in the wrong place.

## The rule this all follows

A measurement is a claim, and a claim has to say what it measured and what it could not. That is why:

- `null` is printed where a platform cannot measure, never `0`;
- the allowance is sized to a known fixed cost and named in the source, so it can be argued with;
- a build that cannot measure exits `1` rather than passing;
- the fields are the same everywhere, so a script does not need to know the platform to read them.

## Machine-readable output

`--format json` is the shape a script should read. It prints only the object: no preamble, no timing
chatter, nothing a JSON parser has to be taught to skip.

```json
{
  "command": "memory-report",
  "platform": "macos",
  "tool": "process resident memory",
  "measured": ["rss", "delta", "cache", "gpu", "pages"],
  "unmeasured": [],
  "columns": ["rss", "delta", "cache", "gpu", "pages"],
  "rows": [
    { "label": "baseline", "rss": 11534336, "delta": null, "cache": 0, "gpu": 0, "pages": 0 },
    { "label": "open 화학", "rss": 83886080, "delta": 72351744, "cache": 11534336, "gpu": 0, "pages": 1 },
    { "label": "close 화학", "rss": 66060288, "delta": -17825792, "cache": 0, "gpu": 0, "pages": 0 }
  ],
  "summary": { "peak": 83886080, "final": 66060288, "growth": 54525952, "tolerance": 67108864 },
  "verdict": "ok",
  "exit_code": 0,
  "note": "came back within 64 MiB"
}
```

- **`columns`** — the field names of every row, in order, including ones a platform cannot fill. A row
  is a list of values under these names, so a consumer can read a memory report and a benchmark report
  with one piece of code.
- **`measured` / `unmeasured`** — which of those columns actually hold readings here. A script that needs
  `gpu` checks this rather than testing for `null`, because *"nothing was measured"* and *"a measured
  zero"* are different facts. `unmeasured` carries the reason, name by name.
- **`delta: null`** — the first row has nothing above it. `null`, not `0`.
- **`verdict` / `exit_code`** — `ok`/`0`, `failed`/`1`, `skipped`/`2`. The code is printed as well as
  returned, so a run captured to a file still says what it decided.
- **`summary`** — the figures the verdict was made from, `tolerance` included, so the number can be
  argued with rather than only obeyed.

The benchmark's object is the same shape:

```json
{
  "command": "bench",
  "platform": "windows",
  "tool": "Direct2D canvas · 1600×1000 at 1.0x",
  "measured": ["strokes", "first", "warm", "worst", "rebuild", "pan", "present", "frame"],
  "unmeasured": [],
  "columns": ["strokes", "first", "warm", "worst", "rebuild", "pan", "present", "frame"],
  "rows": [
    { "label": "316 strokes", "strokes": 316, "first": 12.4, "warm": 4.1, "worst": 6.8, "rebuild": 11.2, "pan": 3.9, "present": 2.2, "frame": 6.3 }
  ],
  "summary": {},
  "verdict": "ok",
  "exit_code": 0,
  "note": null
}
```

Durations are **milliseconds as floats** — one unit everywhere — because the human table is free to
choose a unit per row and a script is not.

## Where this document and the code part company

Everything above is implemented, and the exit codes, field names and platform table are what the tests
pin (`crates/note-ui/src/report.rs`, `apps/macos/src/platform/bench.rs`). Two honest limits remain:

- **The Windows benchmark has not been compiled on this machine.** Its file is `#[cfg(windows)]` and the
  Windows build needs a GPUI build script that cannot run cross-compiled, so it is syntax-checked here
  and compiled only on Windows. Its *contract* is the shared one, which is tested.
- **The two `present` columns measure different things** (see above), so `frame` is the comparable figure.
- **`surface_bytes` on macOS reports zero in a headless run.** The graphics column is therefore
  *measured* (it is a real reading) rather than declared absent — `unmeasured` is derived from the rows,
  so the contract never says a column is unmeasured while its cells hold numbers.


