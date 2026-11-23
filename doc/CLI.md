# The measuring commands

Three things about this app cannot be seen by using it: whether it *gives memory back*, whether a
frame is *fast enough*, and whether a rebuild is worth its power. All three are questions with
numeric answers, and all three are meant to be answered by commands rather than by looking at a
window. The third is the newest, and its command is still a contract rather than code (see the
`--power-report` section below).

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

Measures what opening and closing documents costs the process, **and** what the ink and the note cost.

```sh
cheap-note --memory-report
cheap-note --memory-report <note-folder> [<note-folder> ...]
cheap-note --memory-report --format json <note-folder> [<note-folder> ...]
```

No window opens. Every note is opened, measured, closed, and measured again **in one process** — two
processes would answer a question about the operating system rather than about the app.

**The ink and note scenarios run whether or not a note is given**, because they need none: an ink-only
leak is invisible to a report of a textbook, and the ink is the larger half of what the app holds (a page
is 71x its readings in derived geometry — see [VIEW.md](VIEW.md), "What the note holds in memory"). Each
`<note>` adds a pair of rows for opening and closing its document.

Both platforms take the same `--format` flag: `report::Options` parses it before the note folders, so
`--format json` is one contract on macOS and Windows. The Windows build writes the result through
`apps/windows/src/platform/console.rs` — a console when the run has one, a file beside the executable
when it does not.

### What each row measures

| Rows | What a figure that does not come back would mean |
|---|---|
| `open` / `close <note>` | a document's bytes, bitmaps or Pdfium handle outliving its note |
| `ink: N pages written` | the page cache growing without bound (`ink::PAGES_KEPT`; the `pages` column is the count it holds) |
| `ink: turned back through` | a page given up and never given back — the bound turning into lost ink |
| `ink: M strokes drawn` / `cleared` / `undone` | the undo history outliving its bound (`history::HISTORY_EDITS`) |
| `ink: 6 zoom rungs` | a rebuild leaking the geometry it replaced |
| `note: N pages written` / `closed` | the writer thread and its SQLite connection outliving the note |
| `end` | the process after everything above was dropped |

Every scenario drops what it built before the next starts, so a figure that does not return is one
scenario's, not the run's.

### What each column means

| Field | Meaning |
|---|---|
| `rss` | resident memory, from the same counters the status line reads |
| `delta` | against the row above; a `close` that reads `+0` gave nothing back |
| `cache` | what the PDF page cache holds, in bytes |
| `gpu` | the graphics ledger when the platform can report it, else `null` |
| `pages` | how many pages the thing being measured is holding — the PDF cache's bitmaps, or the note's pages |

`cache` and `pages` going to zero on a `close` is what makes that row worth reading: the cache
emptied, so any resident figure still standing is something *else* holding on. The `pages` column on an
`ink:` row is deliberately bounded: it reads the note's own page count, which never exceeds
`PAGES_KEPT + 1`, and a figure that climbed would be the page cache leaking.

### The two halves, and why each has its own allowance

The verdict is not one number. A run is judged on **documents** and on **ink** separately, because their
legitimate costs are different sizes and neither is a leak:

| Half | Measured from → to | Allowance | Typical |
|---|---|---|---|
| documents | `baseline` → last `close` | `LEAK_TOLERANCE_BYTES`, 64 MiB | 42–53 MiB |
| ink and note | last row before the first `ink:` row → `end` | `INK_TOLERANCE_BYTES`, 32 MiB | 14 MiB |

**The document allowance** is sized for Pdfium's fixed per-process cost — 30–50 MiB whatever the
document's size. It is not sized for a leak: a document's bytes held in the heap left 130 MiB behind for a
62 MB PDF, and mapping them instead (see `note_format::mapped`) left 70. The allowance sits between the two,
so a fixed cost passes and a proportional one fails.

**The ink allowance** is smaller because this half pays nothing like Pdfium's cost: the 400-stroke page is
11 MiB and the scenarios finish about 14 MiB above where they started. It is tight enough that a page cache
that stopped evicting would cross it — a note holding every page ever written was 52 MiB at 10 pages.

The halves were split after measuring that they do not fit one allowance together. Summed, one document
plus the ink scenarios read 56–67 MiB, and the verdict flipped between `ok` and `failed` with the document
and **not with its size** (a 175 MB PDF read 56, a 32 MB one 64). Split, every local note passes with room:
documents 42–53 of 64, ink 14 of 32. Both tripwires were verified by injecting a leak: 40 MiB held by the
ink made the run fail and named the ink; 40 MiB leaked per note failed and named the documents.

A run fails when **either** half does. A half that was not measured is not counted either way, so
`--memory-report` with no paths is judged on the ink alone.

### Reading a row that does not move

Resident memory is the right figure for "did the process come back", and the wrong one for "was this
freed": **a freed block stays resident and the next allocation reuses it**. So an `ink: dropped` that
reads `+0 MiB` is the normal case, not a pass by itself — what it means is that nothing *grew*.

For the narrower question — was this page released rather than reused — `leaks --atExit` is the answer, and
it agrees with the table: an ink-only report is **0 leaks for 0 bytes**, and a report that opens a note is
208 bytes, which the stack attributes to `report::Options::parse` growing its own argument vector. See
[STATUS.md](STATUS.md), "Checking with `leaks` rather than with the table".

### Exit codes

| Code | Meaning |
|---|---|
| `0` | both halves came back within their allowances |
| `1` | one did not, or the build cannot measure memory at all |
| `2` | the command could not run — a note that would not open |

Two codes past success, because "the app leaked" (`1`) and "the command was wrong" (`2`) are different
problems, and a CI job wants to tell them apart. A run that cannot measure at all exits `1`, not `0`: a pass
it did not earn is worse than a failure.

## `cheap-note --bench`

Measures what a frame of the canvas costs.

```sh
cheap-note --bench [--format table|json] [--strokes 0,29,316,1000,8182] [--frames 30]
```

**One command line on both platforms.** macOS and Windows each implement the drawing half behind their
own seam and hand the same figures to the same report; the contract is what makes the two tables
comparable. `cheap-note-bench` still exists on Windows as a thin wrapper, because it used to be the only
way in and a script may call it.

The Windows release image has no console beside its window, so the app hands the buffered table (or
JSON) to the same `platform::console` route the memory report uses: a console when it can attach to one,
and `cheap-note-bench.txt` beside the executable when it cannot. `cheap-note-bench` remains the console
binary, and a redirected run keeps its stdout.

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

### `warm` on macOS has a floor, and it is the display's clock

`warm` on macOS does not measure the renderer alone: it cannot fall below one refresh interval, because
the frame's first act is to ask `CAMetalLayer` for a drawable and that call waits when the layer's few
buffers are all still with the compositor. Measured on an arm64 laptop, `warm` sits at **8.2–8.5 ms for
every page from 0 to 8 182 strokes** — 8.33 ms is a 120 Hz tick — while `present` grows with the page
(0.66 ms at 0 strokes, 4.8 ms at 8 182). A sampler attributes **98.3 % of the main thread's samples to
`IOSurface`** and 0.3 % to this app: the thread is parked in the drawable call, not drawing.

Two things follow, and both are worth knowing before reading the table as a renderer's report:

* **`warm` says nothing about the ink.** A page with no strokes and a page with 8 182 cost the same
  `warm`, because what is being timed is the wait for the next tick. Comparing `warm` across pages, or
  across machines with different panels, compares refresh rates rather than code.
* **`warm` missed by a lot is still the real signal.** A frame whose own work exceeds the interval runs
  over into the next, so a `warm` of, say, 25 ms is the renderer plus one wait and is not a tie.

`present` and `rebuild` are the figures that respond to the page, so a claim about drawing cost should
rest on those. `--frames` does not change the floor: 10, 100 and 400 warm frames all read 8.1–8.3 ms.

The default counts are 0, 29, 316, 1 000 and 8 182 strokes, and both platforms default to 30 warm
frames. The first three counts are the ones the older measurements used, and 8 182 is the page the macOS
figures in `doc/WINDOWS.md` were taken on — it is in the list because the question this benchmark exists
for is whether a platform is draw-bound at that page size. `--frames 60` is the longer run §6.4 of that
document quotes.

## `cheap-note --power-report` — what a rebuild costs, in joules (draft)

**This section is a contract, not a description of shipped code.** The command is not in the tree
yet: its parser, its workload and its meter reader are still to be written. It is written down first
because the platform half — which counter exists, and what it may claim — is the part that has to be
agreed before the arithmetic is worth writing. Everything below is the shape it will ship in.

The bench answers *how long*. This answers *how many joules, and how many operations per joule* — the
question a rung rebuild raises, and the one `--bench` cannot answer by itself. It is not a system
power monitor, and it does not replace the bench's latency table.

```sh
cheap-note --power-report [--format table|json]
    [--workload rung|frame|both] [--strokes 1000,8182]
    [--repeats 20] [--warmup 2] [--threads N] [--frames 30]
    [--energy-source auto|meter|platform|none] [--meter meter.csv] [--meter-offset 0]
    [--require-energy] [--max-ms N] [--max-joules-per-op N] [--min-ops-per-joule N]
```

`--help` prints a usage block and exits `0`; an unknown flag, a missing value, or two flags that
contradict each other exits `2`.

The command runs the whole workload **in one process**, as the memory report does: a meter around
two processes would answer a question about the operating system. No note is opened or written; the
page is synthetic and lives in memory, and a run leaves the app's own settings and pool alone.

On Windows the buffered report goes through the same `platform::console` route the other two use: a
console when it can attach to one, and `cheap-note-power.txt` beside the executable when it cannot.

### The two workloads

`--workload rung` (the default) measures the view's half of a detail rung: the request and the build
that `InkDocument::set_zoom` performs when a zoom crosses a rung (`ink.rs`: `request_detail`,
`build_detail`). It is the workload `measures_the_rung_costs` already takes, made repeatable and
given an energy window.

It builds a synthetic page of `--strokes` strokes — the same 40-point, 2 px-apart strokes
`written_page` writes — at the page's initial rung, then alternates between two rungs, `1.0` and
`2.25`, for `--repeats` timed rebuilds. `--warmup` rebuilds in each direction run first and are
discarded; the page is built before the window opens and dropped after it closes. Before every timed
rebuild, one `Arc` clone of the page's finished list is held the way a frame's description holds one;
that is what makes the rebuild pay the `Arc::make_mut` copy it pays in the app, rather than measuring
an unshared page as a lower bound.

The default counts are `1 000` and `8 182` strokes, because below a thousand the rebuild is already
under a millisecond and a joule reading of it is mostly the meter's own noise; pass `--strokes` for
the bench's smaller pages.

This is **the view's half only.** On Windows a rung also rebakes the canvas's realizations, which is
the bench's `rebuild` column (134–140 ms for 8,182 strokes in `doc/WINDOWS.md` §6.4). That is a
platform renderer's cost and belongs to `--bench`; the two reports are kept separate so the seam
stays visible.

`--workload frame` measures a **warm** frame of the same synthetic page through the platform's
canvas, reusing the bench's surface and its `--frames` count (default 30). The first frame, which
bakes every realization, is warmup; the measured frames are the steady drawing cost a pan or a still
page pays. It is the bench's `frame` figure — warm draw plus present — not `warm` alone: the present
is part of what the window costs. The bench remains the contract for `first`, `rebuild` and `pan`;
this command does not repeat them.

`--workload both` runs the rung phase first and the frame phase second for each stroke count, and
records one row per phase.

### What one row is

Every number in a row is **per operation** except `count` and `strokes`:

| Field | Meaning |
|---|---|
| `strokes` | the page's stroke count |
| `phase` | `rung` or `frame` |
| `threads` | the Rayon pool size the phase used; absent for `frame` |
| `count` | how many operations were timed (`repeats` for `rung`, `frames` for `frame`) |
| `wall` | mean wall-clock time per operation, ms |
| `median` | median wall-clock time per operation, ms |
| `worst` | the slowest operation, ms — a mean hides the one a hand would feel |
| `cpu` | mean process CPU time per operation, ms of **core-time** (all threads summed) |
| `joules_per_op` | mean energy per operation, J; absent when no energy source |
| `ops_per_joule` | operations per joule, `1 / joules_per_op`; absent with it |

`cpu` is **core-time, not power**: a row whose `wall` is 10.6 ms and whose `cpu` is 42.0 ms ran
about four cores' worth of work. It is reported even when a meter is available, because the meter
measures the machine and `cpu` measures the process.

### Where the joules come from

`--energy-source` chooses the source, and the report always says which one it used:

| Source | What it is | Where it exists |
|---|---|---|
| `meter` | an external wall meter or smart plug, read from a CSV (`--meter`) | every platform |
| `platform` | the platform's own counter: `powermetrics` on macOS, RAPL on Linux | macOS (root), Linux (readable RAPL only) — **not Windows** |
| `none` | no energy at all; `wall` and `cpu` only | every platform |
| `auto` (default) | `meter` if `--meter` was given, else `platform` when it exists, else `none` | — |

An explicit `--energy-source` wins over `auto`; `--meter` is meaningful only with `meter` (or `auto`),
and `--require-energy` together with `none` is a usage error — the two flags ask for opposite things.
`--energy-source meter` without `--meter` is a usage error too.

The meter file is the one source that is the same on every platform, so the contract is written
around it:

```csv
epoch_s,watts
1720000000.000,12.4
1720000000.100,13.1
```

Timestamps are Unix epoch seconds and must be monotonic; the meter's clock must be the machine's
clock, and `--meter-offset` (seconds, default 0) is added to every sample for a meter that keeps its
own. The command integrates the samples over the measurement window with the trapezoid rule. It
requires **at least two samples inside the window**: one sample is a fact about a moment, not an
integral, and a window with fewer than two is `skipped` (`meter-window-too-short`). A sample that is
negative, NaN, or out of order is `skipped` (`meter-invalid`), and so is a window whose integrated
energy is zero: a zero joule count is not an infinite efficiency. A meter run needs enough repeats
to span the meter's own sample interval — a rung is milliseconds, not seconds, so `--repeats 20` is
a CPU-time run and `--repeats 2000` is a meter run.

The platform counter is optional, and Windows has none: there is no standard user-mode API for
package energy there, so `platform` is `unavailable` and never `0`. On macOS `powermetrics` needs
root; without it the readings are `permission-denied`, and with `--require-energy` the run is
`skipped`. On Linux, `intel-rapl`'s `energy_uj` is read when it is readable, its
`max_energy_range_uj` wrap is honoured, and a counter that crossed zero is a `null` rather than a
guess.

**CPU time is never reported as energy.** `cpu` is a proxy for work and the summary says so
(`basis: "cpu-time"`); `joules_per_op` and `ops_per_joule` are `null` when no energy source was read.
`--require-energy` turns that absence into a `skipped` run, because a command whose purpose is joules
cannot pass without one.

### The window, the repeats and the build

- The page, the surface and the warmup are **outside** the window; teardown is after it. The window
  covers the timed operations only.
- Energy is integrated over the same window as CPU time and wall time. `wall` is `Instant` around
  each operation; `cpu` is the process's CPU-time delta over the phase divided by `count`.
- `--repeats` is at least 3, because a median of one is a mean and a mean of one is a guess. The
  default is 20; the default warmup is 2.
- A wall meter and a platform counter use the system clock for their window. If the clock moves
  backwards during a run, the integration is `skipped` (`meter-clock-jumped`) rather than guessed.
- The command runs in release builds only. A debug build exits `2` (`debug-build`) with a note: a
  debug number is not a measurement of the code that ships.

### Threads

`--threads N` runs the rung workload under a scoped `rayon` pool of N threads; the global pool is
left exactly as it was, and the summary says `threads_source: "flag"`. Without the flag the process's
global pool is used and the source is `"global"`. The frame phase does not use the pool, so its
`threads` cell is absent with `not-parallel`.

This is the flag the rung question needed: the same page, the same rungs, at 2, 6 and 12 threads,
with `joules_per_op` and `ops_per_joule` in the same row. A measurement never reaches into a running
note or changes the app's own pool.

### What the report carries

The report is the same envelope as the other two commands — `command`, `platform`, `tool`, `columns`,
`rows`, `summary`, `verdict`, `exit_code`, `note`, `measured`, `unmeasured` — with two additions to
the value vocabulary: a **text** value (for `phase` and for named summary facts) and a
**joules/ratio** value (for the energy columns). The columns are fixed and in this order:

```
strokes, phase, threads, count, wall, median, worst, cpu, joules_per_op, ops_per_joule
```

A cell the platform cannot fill is absent with its own reason; `unmeasured` names a column only when
no row could fill it, as in the other reports. The column stays in `columns` either way.

Summary fields, named in the JSON when they apply (text unless noted):

| Field | Meaning |
|---|---|
| `threads`, `threads_source` | the pool and why it is that size (`flag`, `global`, `not-parallel`) |
| `repeats`, `warmup`, `frames` | what the run was asked to do |
| `build` | `release` or `debug` |
| `power_plan` | the active plan (`powercfg`, `pmset`, `scaling_governor`) or `unknown` |
| `power_source` | `ac`, `battery`, `unknown` |
| `energy_source` | `meter`, `powermetrics`, `rapl`, `none` |
| `energy_scope` | `system`, `package`, `cpu`, `gpu`, `unknown` — what the joules cover |
| `basis` | `meter`, `platform`, `cpu-time` |
| `meter_samples`, `meter_span_ms` | what the integration saw, when a meter was used |
| `energy_total_j` (number) | `sum(joules_per_op × count)` over the rows; absent unless every row has energy |

`energy_scope` is `system` for a meter, `package`/`cpu`/`gpu` for a platform counter as that counter
reports, and `unknown` when no energy was read. For `--workload both`, `meter_samples` and
`meter_span_ms` are the run's totals; a single-phase run reports that phase's own.

### A table and an object

```
strokes  phase  threads  count    wall   median   worst     cpu   J/op?   ops/J?
   8182   rung       12     20   10.60   10.40   12.10   42.00       —       —
ok · release · AC · high performance · 12 threads · energy not measured (no platform counter; cpu is a proxy)
```

```json
{
  "command": "power-report",
  "platform": "windows",
  "tool": "note-ui rung rebuild · 8182 strokes · 12 threads",
  "measured": ["strokes", "phase", "threads", "count", "wall", "median", "worst", "cpu"],
  "unmeasured": [["joules_per_op", "no-platform-counter"], ["ops_per_joule", "no-platform-counter"]],
  "columns": ["strokes", "phase", "threads", "count", "wall", "median", "worst", "cpu", "joules_per_op", "ops_per_joule"],
  "rows": [
    {
      "label": "8182 strokes · rung",
      "strokes": 8182,
      "phase": "rung",
      "threads": 12,
      "count": 20,
      "wall": 10.60,
      "median": 10.40,
      "worst": 12.10,
      "cpu": 42.00,
      "joules_per_op": null,
      "ops_per_joule": null
    }
  ],
  "summary": {
    "threads": 12,
    "threads_source": "flag",
    "repeats": 20,
    "warmup": 2,
    "build": "release",
    "power_plan": "high performance",
    "power_source": "ac",
    "energy_source": "none",
    "energy_scope": "unknown",
    "basis": "cpu-time",
    "energy_total_j": null
  },
  "verdict": "ok",
  "exit_code": 0,
  "note": "energy not measured: no platform counter on this platform; cpu is core-time, not power"
}
```

Durations are milliseconds as floats; energy is joules as floats. `null` is a measurement that was
not taken; `0` is a measurement of zero.

### Exit codes and the budget flags

The three words and the three codes are the ones the other measuring commands use — `ok`/`0`,
`failed`/`1`, `skipped`/`2`:

- `0` — every requested phase was measured, and no budget was exceeded;
- `1` — a budget was given and exceeded: `--max-ms`, `--max-joules-per-op`, `--min-ops-per-joule`;
- `2` — the run could not measure: no release build, no surface, an invalid meter, a window too short,
  permission denied for `powermetrics`, **or an energy budget while no energy was measured**.

The last clause is the one a CI gate needs: `--require-energy` and an energy budget both turn "no
joules" into a skip, never a pass. A budget on a quantity that was not measured is not a success.

A budget applies to every row and one row over it fails the run. `--max-ms` is compared against
`worst`, not `wall`, for the reason the `worst` column exists; the energy budgets are compared
against the row's `joules_per_op` and `ops_per_joule`.

### What this is not

- **Not a system power monitor.** It measures one workload's window; it does not report temperatures,
  frequencies, per-core power, or the rest of the machine.
- **Not a cross-machine comparison.** A meter covers the machine and a package counter covers a
  package; a number is comparable only on the same machine, build, power plan and AC/battery state.
  The summary carries those fields so a script can refuse a comparison it should not make.
- **Not a replacement for `--bench`.** The bench remains the frame-latency contract; this is the
  joules contract.
- **Not a control.** It does not change the power plan, the frame cap, or the global Rayon pool; it
  reports what it found and measures a scoped pool when asked.

### What would implement it

A new `Command::PowerReport` and two value kinds — text, and a float with a unit (joules or
operations per joule) — in the shared report, a portable workload core in `note-ui` (the synthetic
page, the rung alternation, the scoped pool and the arithmetic), and a platform half per app: process
CPU time, an optional energy counter, and the power plan/AC state. The platform halves are the same
seam the memory report and the bench already use; a platform with no energy counter still builds and
still reports `wall` and `cpu`.

## What a platform has to provide

No measuring command is a platform feature that some platforms have and others lack. They are **one
portable core** (`crates/note-ui`) plus a small set of measurements a platform supplies. A platform
that supplies none of them still builds, still runs, and says so.

| Need | macOS | Windows | Linux (none yet) |
|---|---|---|---|
| resident memory | `task_info`, `TASK_VM_INFO` | `GetProcessMemoryInfo` | — |
| graphics ledger | `IOSurface` via `task_vm_info` | none — the column is `—` | — |
| a console to print to | inherited `stdout` | attach to the parent console, or a file | inherited `stdout` |
| frame timing | Metal, waited for with `waitUntilCompleted` | Direct2D + swap chain | — |
| process CPU time | `task_info` | `GetProcessTimes` | `getrusage` |
| an energy counter (optional) | `powermetrics`, as root | none — a meter or CPU time | RAPL `energy_uj`, when readable |
| AC/battery and the active plan | `pmset -g`, `pmset -g batt` | `GetSystemPowerStatus`, `powercfg` | `/sys/class/power_supply`, `scaling_governor` |

The graphics column is the model for how to be honest: Windows **has no** `surface_bytes`, so the
field is `null` / `—` rather than `0`. A zero would be a claim about an empty GPU. The energy row is
the same shape: Windows has no standard package-energy counter, so the power report reads a meter or
reports CPU time as the proxy it is.

## Adding Linux

Linux is not a rewrite; it is five small pieces behind seams that already exist.

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
5. **The power report.** CPU time from `getrusage`; energy from RAPL when it is readable, and `null`
   when it is not. No RAPL is not a zero, for the same reason no graphics ledger is not a zero.

Nothing in `note-ui` should change for any of that. If it has to, the seam was in the wrong place.

## The rule this all follows

A measurement is a claim, and a claim has to say what it measured and what it could not. That is why:

- `null` is printed where a platform cannot measure, never `0`;
- the allowance is sized to a known fixed cost and named in the source, so it can be argued with;
- a build that cannot measure does not pass: it exits `2` (`skipped`) or `1` (`failed`), never `0`;
- the fields are the same everywhere, so a script does not need to know the platform to read them;
- `cpu` is never printed as a joule: core-time is a proxy for work and joules are a measurement;
- a meter says how many samples it integrated, and fewer than two inside the window is a skip;
- a budget on energy is skipped when energy was not measured, because an unmeasured quantity cannot
  meet a budget.

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
  "summary": {
    "peak": 83886080,
    "final": 66060288,
    "growth": 54525952,
    "document_growth": 54525952,
    "document_tolerance": 67108864,
    "ink_growth": 14680064,
    "ink_tolerance": 33554432
  },
  "verdict": "ok",
  "exit_code": 0,
  "note": "documents within 64 MiB, ink within 32 MiB"
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
  "tool": "Direct2D canvas · 1280×900 at 1.0x",
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

The power report's object is the same envelope, with one text column (`phase`) and two energy columns
(`joules_per_op`, `ops_per_joule`) that are `null` together when no energy source was read. Its
section above has the object; the only new value kinds are text and a float with a joules/ratio
unit — numbers stay numbers, and absence stays `null`.

## Where this document and the code part company

Everything above except `--power-report` is implemented on both platforms, and the exit codes, field
names and platform table (the shipped rows) are what the tests pin (`crates/note-ui/src/report.rs`,
`crates/note-ui/src/memory_report.rs`, `apps/macos/src/platform/bench.rs`,
`apps/windows/src/platform/bench.rs`, `apps/windows/src/platform/report.rs`). The power report is a
contract only: no parser, no workload and no meter reader is in the tree yet; its section says what
it will accept and print. Two honest limits remain about the shipped commands:

- **The two `present` columns measure different things** (see above), so `frame` is the comparable figure.
- **`surface_bytes` on macOS reports zero in a headless run.** The graphics column is therefore
  *measured* (it is a real reading) rather than declared absent — `unmeasured` is derived from the rows,
  so the contract never says a column is unmeasured while its cells hold numbers.


