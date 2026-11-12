# What Windows has and has not

This is the Windows build's own document, and the counterpart to [MACOS.md](MACOS.md). The Windows
application came first and then went quiet for a while: the Mac port happened, and the tuning that came
with it was measured on a Mac. This document is the record of what that leaves behind — **which of
those changes reached Windows, which did not, and what is still worth doing on Windows only.**

It is written to be *checked*, not trusted. Every row names the file that decides it, so a reader can
disagree with a claim by opening one file. Where a number is quoted it is one the code or an earlier
measurement produced, not an estimate.

If you read only one paragraph, read this one:

> **Nothing in `crates/note-ui` is Mac-specific, so most of the tuning reached Windows for free** — the
> parallel zoom rebuild, the whole-page re-upload fix, the pen-pan clamp, the jitter filter, the frame
> cap. What did *not* reach Windows is the part whose meaning depends on the platform underneath:
> **`ScrollEvent.position` is a different thing on Windows** (the live cursor, not where the gesture
> began), **the wheel is dropped** (which on Windows means a plugged-in mouse cannot scroll at all),
> and **`set_vsync` is a no-op** while the switch that calls it is still on the bar. The Windows canvas also
> has a cost model of its own — baked Direct2D realizations rather than a multisampled Metal pass — so
> the optimisation the Mac needed most is one Windows had already solved differently.

## 1. How to read the tables

Three verdicts are used, and they mean specific things:

| Verdict | Means |
|---|---|
| **shared** | the code is in `crates/note-ui` or `crates/note-format` and names no platform; it is the same code on both machines, and any difference would be a bug in a platform crate |
| **Mac only** | the code is in `apps/macos` (or gated on `cfg`), so Windows never runs it. *Not* a regression — something the Mac build can do that Windows has no implementation of |
| **broken on Windows** | the shared code assumes something the Windows platform does not do, so the feature silently does nothing or does the wrong thing. **This is the list that matters** |

The third verdict is the one worth being strict about, because it is invisible: the app compiles, the
tests pass, and the feature is simply absent at run time. A fourth label, **unknown**, appears once, for
the thing this document genuinely cannot say: Windows' frame cost has never been measured (§6.4).

## 2. What reached Windows for free

All of the following live above the seam and therefore run identically on both platforms. This is the
seam doing its job, and it is worth saying plainly so nobody re-does the work.

| Change | Where | Why it is the same on Windows |
|---|---|---|
| The zoom rung no longer costs the frame: `request_detail` (frame) split from `build_detail` (pump), on `DETAIL_SETTLE_INTERVAL` (32 ms) | `crates/note-ui/src/app.rs`, `ink.rs` | the split is a scheduling decision inside the view; the frame loop is the same loop on both |
| `build_detail` parallelised with `rayon` — 23.6 ms → ~4.7 ms, 12 threads verified | `crates/note-ui/src/ink.rs` | `rayon` is a dependency of `note-ui`, which has no `[target.…]` section, so it uses Windows' own pool there |
| `refresh_ink` returns `bool` and clears its own slots, so a pan frame no longer re-uploads the page through three buffers | `crates/note-ui/src/view.rs`, `app.rs` | the re-upload was the view asking the surface to redraw; the fix is in the asking |
| The pen pan is clamped at `settle_pan` (gesture end) instead of on every reading | `crates/note-ui/src/app.rs` | the bug was `Viewport::hold_pan` called mid-gesture, above the seam |
| `PenJitter` (`PEN_JITTER_SPAN`, 0.5 px), on by default, gestures only | `crates/note-ui/src/pen_gesture.rs` | a threshold on a reading's position, and the reading type is the view's own |
| `FrameCap` and the `Frames` select, honoured by `held_by_cap` in `draw_canvas` | `crates/note-ui/src/app.rs` | the cap is checked against the time since the last present, which is the view's clock |
| Pinch and pen-zoom describe the sheet per event (`describe_sheet`) instead of leaving intermediate frames stale | `crates/note-ui/src/app.rs` | the description is built from the layout the view keeps |
| `⌘\` / `Ctrl+\` toggles the bar (`chord::BAR`), label computed from the chord | `crates/note-ui/src/keys.rs` | `keys::is_primary` resolves `secondary` per platform, so Windows gets `Ctrl+\` |
| `flag_of` accepts `1`/`0`, any case, with whitespace — which stopped two notes locking out | `crates/note-format/src/store.rs` | the note format is identical on both machines, so that bug was only reachable on the machine that wrote `1` |
| `FACET` 1.5 → 2.0 on the outline | `crates/note-format/src/ink.rs` | one number, read by every outline builder |

Two of these deserve a caveat rather than a clean tick, because "shared code" does not always mean
"same effect":

* **`FACET`.** The number is shared; what it costs is not. On the Mac the raised facet moved the test
  note from 4.65 M vertices to 3.49 M, and those measurements showed *fragments*, not vertices,
  dominating. The Windows canvas draws baked realizations through Direct2D, whose cost is per
  *realization* (§5), so a coarser outline there buys a cheaper bake rather than a cheaper frame. The
  trade is still the right one; it is a different trade.
* **`FrameCap`.** The cap is honest on both, but on Windows it is a pause on a compositor that would
  otherwise present as fast as it is handed frames (`Present(0, …)`, §3.3). On the Mac it is a pause on
  a layer that may also be waiting for the display (`displaySyncEnabled`). A cap tighter than the
  display binds first on both, which is why the V-Sync switch shows nothing on either until the cap is
  loose.

## 3. Broken on Windows

Three things. Each is a shared assumption that the Windows platform does not hold, and each is
invisible — it compiles, the tests pass, and the feature is absent at run time.

### 3.1 An edge swipe never turns a page

`Edge::at` reads `event.position` and asks which side of the window it is within `EDGE_SWIPE_BAND`
(120 logical px) of:

```rust
// crates/note-ui/src/app.rs — NoteApp::on_scroll
if let Some(edge) = Edge::at(f32::from(event.position.x), self.sheet.window.0) {
    self.edge_swipe = Some(EdgeSwipe::new(edge));
}
```

The band is answered **once per gesture**, on the first reading, which is right — and the comment above
that line says why:

> An edge swipe is begun from where the gesture *started*, which the platform reports alongside every
> reading of it, so the band is answered for the whole gesture from one position.

That is true of macOS. A trackpad on a Mac reports the position the gesture **started** at with every
reading of it — that fact is what made a per-gesture band possible at all. Windows does not. GPUI's
Windows backend reads the cursor afresh for every event:

```rust
// gpui-pre-windows 0.3.11, src/direct_manipulation.rs — OnContentUpdated
let position = self.mouse_position();          // the live cursor, every update
…
let dx = x_offset - last_x;
let dy = y_offset - last_y;
… ScrollEvent { position, delta: ScrollDelta::Pixels(point(px(dx), px(dy))), … }
```

So on Windows `position` is *where the fingers are now*, not where they began. A swipe that starts 40 px
from the left rim arms on a reading very near the rim and then travels away from it, so that case mostly
works — the arming reading happened before much travel had accumulated. What fails is the *first*
reading arriving after Direct Manipulation has already accumulated travel, which depends on when
`OnContentUpdated` first lands relative to the hand. A quick right-to-left swipe begun at the right rim
can arm on a reading whose x has already left the band, and nothing turns. **It is not deterministic**,
which is worse than not working: it works when tried by hand at the rim and fails when the hand moves
quickly.

**Why this is Windows-only.** `apps/windows` has no scroll code at all — `grep ScrollDelta apps/windows`
finds nothing — so every scroll and pinch on Windows arrives through GPUI's own window procedure, and
there is no place in this repository where the position could be corrected for that platform.

**What would fix it, in the order worth trying:**

1. **Recover the origin from the reading instead of trusting it.** `EdgeSwipe` already accumulates a
   signed `travel`, the running integral of the deltas, so the gesture's origin is `position - travel` —
   which is exactly what macOS' `position` already is, and is computable on either platform. Smallest
   fix: no new seam method, no `cfg`, and nothing changes on the Mac.
2. **A seam answer for the source.** GPUI does carry a `ScrollSource`, but on Windows both a wheel and
   Direct Manipulation report `ScrollSource::Pointer`, so it cannot tell a trackpad from a mouse and does
   not answer this question.
3. **Declare the gesture Windows-unsupported** and say so in §7. Honest, and free.

Option 1 is what this document recommends, because the information is already in the event and the
shared code is throwing it away.

### 3.2 A mouse wheel pans nothing

`on_scroll` drops a line-based delta on the first line:

```rust
let ScrollDelta::Pixels(delta) = event.delta else {
    // A line-based delta is a wheel's, and a wheel is not read (see this method's docs). Ignoring
    // it here is what keeps a plugged-in mouse from moving the sheet at a step the trackpad never
    // asked for.
    return;
};
```

The reasoning is sound and it is written down in [VIEW.md §9](VIEW.md#9-gestures): a wheel reports
lines, a trackpad reports pixels, and the conversion between them is a guess about a system setting the
app has no API for. One zoom gesture and one pan gesture, no chord that turns one into the other.

On a Mac that is a small loss: the trackpad is built into the machine and nearly everyone has one. **On
Windows it is a different proposition.** A desktop Windows machine usually has a wheel and no precision
trackpad; the machines that have one are laptops. So this app's Windows build is the one note app on the
platform where scrolling with the mouse does not work — and GPUI's Windows backend makes that concrete,
because both wheel messages become exactly the shape that is dropped:

```rust
// gpui-pre-windows 0.3.11, src/events.rs
// WM_MOUSEWHEEL
delta: ScrollDelta::Lines(…wheel_distance…)
// WM_MOUSEHWHEEL
delta: ScrollDelta::Lines(Point { x: wheel_distance, y: 0.0 }),
```

This is the one place where a rule written on the Mac probably should not be *shared*. Three options,
with their honest costs:

| Option | What it costs |
|---|---|
| **Read the wheel where the platform means it** — a seam answer rather than a `cfg` | the two-gestures rule survives on the Mac and Windows gains a pan it can use. [MACOS.md §6](MACOS.md#6-what-must-not-change) forbids `cfg(target_os)` inside `note-ui`, so this has to be a seam method |
| **Read the wheel everywhere, with a fixed multiplier** | simplest, but it is exactly the guess the rule was written to avoid — and it will be wrong on a machine whose wheel is set to three lines rather than one |
| **Leave it dropped, and say so in the bar or the help** | no code and no lie; the status line names the gestures the app takes. Weakest, but the current behaviour is not *wrong* — it is *chosen*, and only the choice's fitness for Windows is in question |

There is a hardware answer worth naming too: the pan a wheel would perform is what a pen drag with space
held already does ([VIEW.md §9](VIEW.md#9-gestures)), so the loss on Windows is one of *convenience* on a
desktop rather than of capability.

### 3.3 The V-Sync switch does nothing

`InkSurface::set_vsync` is implemented on Windows as empty, deliberately:

```rust
// apps/windows/src/platform/ink_layer/mod.rs
fn set_vsync(&mut self, _on: bool) {}
```

The reason is measured and recorded: this platform presents at a sync interval of zero on purpose,
because waiting for the compositor measured **47 presents a second against the 165 the display could
take** (`device.rs`). A note that asks for vertical sync is a note whose wish this platform cannot grant,
and the setting is still stored with it — correctly, since a note travels between machines and the
platform that *can* honour it should.

What is missing is the last step: **the switch is still on the bar.** A reader on Windows presses
`V-Sync`, the tooltip says what it means, and nothing observable changes. That is a small lie in the
interface, and the kind that costs trust in the switches beside it.

Two ways out, differing in honesty rather than in effort:

| Option | What it means |
|---|---|
| **Hide the switch** where the surface cannot honour it | needs one bit above the seam — `InkSurface::honours_vsync() -> bool`, defaulted to `true` so the Mac is unchanged and Windows answers `false`. The setting still round-trips in the note |
| **Keep it and label the absence** — a disabled control with the reason in its tooltip | more informative; a reader who wonders why has somewhere to read |

Either is a few lines. The point of listing it is that leaving it as it is is not the neutral option.

## 4. What is Mac only, and rightly

Not a defect list — a map, so nobody hunts for a Windows equivalent of something that has none.

| Piece | Where | Why Windows has no counterpart |
|---|---|---|
| `CAMetalLayer.displaySyncEnabled`, and `CHEAP_NOTE_NO_VSYNC` overriding it | `apps/macos/src/platform/metal.rs` | Windows has no choice to make: `Present(0, …)` is the whole policy (§3.3) |
| The `vsync` field's *effect* | `crates/note-format/src/settings.rs` | it is stored on both and honoured on one; the format is deliberately shared |
| `unit_scale: 0.75` | `apps/macos/src/platform/mod.rs` | a point is four thirds of a Windows logical pixel, so the palette must scale down to match a Windows one in the hand. Windows answers `1.0` and nothing it draws changes |

There is **no** GPU counter on either platform, and this document said there was before it was checked.
`apps/macos` has no `MTLCounterSampleBuffer` and no offscreen probe; its frame-cost figures were taken
with the app's own meters (`crates/note-ui/src/timing.rs`) and a frame cap, not with the driver's
counters. When a GPU figure is wanted, the honest instrument is the one that already exists: a frame cap
that holds a rate says the frame has headroom, and one that cannot says it does not (§6.4).

Everything else in `apps/macos` — the Metal canvas, `pen-macos`, the transparent-cursor pointer, the
ghost drawn in the canvas pass, `platform/vitals.rs` — is **a different implementation of a service
Windows also has**, not a capability Windows lacks. The two surfaces are peers behind `InkSurface`; neither
is the reference, and the hardware clause is the same shape on both by construction (§8).

## 5. Where Windows' cost model differs

This is the section to read before profiling anything on Windows, because "where do the frames go" has a
different answer than on the Mac.

| | macOS | Windows |
|---|---|---|
| The canvas | one Metal pass into a `CAMetalLayer`, the whole canvas drawn per present | Direct2D bound to a DXGI swap chain's back buffer, per present |
| A stroke on screen | a triangle strip rebuilt from the outline | an `ID2D1GeometryRealization`: the outline flattened, filled and antialiased **once**, then drawn |
| What costs the frame | the **fragment** stage — a window-sized multisampled pass plus the page texture. Measured at 16.4 M MSAA samples, and this is what the raised `FACET` could not help | the **draw** of the realizations, each already baked. Measured when a bake happens: **~21 µs a stroke** — 1.69 ms for 29 strokes, 7.87 ms for 316 |
| What a zoom rung rebuild costs | the frame, until `request_detail`/`build_detail` were split | a rebake of every realization — the same handful of rebuilds `InkDocument::set_zoom` forces, and now off the frame on both platforms because the *view* schedules it |
| What panning costs | a fresh pass over the whole canvas | the same transform applied to already-baked geometry; nothing is rebuilt |
| Antialiasing | MSAA on the pass | `D2D1_ANTIALIAS_MODE_PER_PRIMITIVE`, analytic |

The consequence, stated plainly: **the optimisations found on the Mac are largely already done on
Windows by a different mechanism**, and the ones that transfer are the ones above the seam (§2). Anyone
who profiles Windows and finds the frame dominated by something this document does not mention should
trust the Windows profile — the two platforms are not the same shape, and the Mac's measurements are not
evidence about this one.

Two details worth knowing because they are easy to get wrong:

* **Direct2D's DPI is pinned to 96** in this canvas (`DOTS_PER_INCH`), because a context otherwise
  scales everything by the desktop's scale factor and a physical pixel would be drawn one and a half
  times too large on a 150% display. The window's own scale is a number the canvas carries instead.
* **A stroke's geometry is kept in the paper's units**, which is what lets the canvas pan and zoom without
  rebuilding anything. Only two things invalidate it: a stroke the layer has not seen, and the app's
  `revision`, which counts the rebuilds a detail rung forces.

## 6. Windows-only work worth doing

Ordered by what it buys, with the reason each is *Windows* work rather than shared work.

### 6.1 Fix the edge swipe's origin (§3.1)

Highest value, because the gesture is currently non-deterministic here. The recommended fix — recover the
origin as `position - travel` — needs no new seam method and no `cfg`. A test can be written in
`crates/note-ui/src/app/tests.rs` by feeding readings whose `position` **advances** as a Windows backend's
would, which the existing edge-swipe tests do not do: they hold the position fixed, because that is what
macOS does. That test would fail today, which is the point of writing it.

### 6.2 Decide the wheel (§3.2)

Not urgent enough to guess at, but it should be a decision rather than an inheritance. If the answer is
"no", it belongs in [VIEW.md §9](VIEW.md#9-gestures) as a stated Windows caveat; if "yes", it belongs
behind a seam method rather than a `cfg`.

### 6.3 Hide or label the V-Sync switch (§3.3)

A few lines. The interesting part is the seam bit, because the same question — "can this surface honour
what the note asked for?" — is one any future platform will ask too.

### 6.4 Measure the Windows canvas the way the Mac one was measured

**This is the honest gap in this document.** Nothing in the repository records a Windows frame cost after
the recent changes. The Mac's numbers (143–146 draws/s on an 8,182-stroke note, fragments dominating) were
taken with a probe built into `apps/macos`. Windows has no equivalent, so the correct statement is that
**its frame cost is currently unknown**, not that it is fine.

The cheapest useful instrument is already in the view: the frame cap. A cap that holds a known rate means
the frame has headroom, and a cap that cannot be held means it does not. So one Windows run at `Unlimited`
and one at `144 Hz` on a heavy note would already say whether this platform is where the Mac was before
its tuning — and if it cannot hold 144 on a page the Mac now holds it on, that is the measurement that
would justify §6.5's rejected items being reconsidered.

### 6.5 Things that are *not* worth doing

Written down so they do not get re-litigated:

* **A software / WARP path.** `apps/windows/src/platform/ink_layer/mod.rs` argues this at length: GPUI
  asks for the same hardware device and has no software path either, so a machine without one cannot run
  the window at all.
* **A waitable swap chain.** Tried, and it is the `47 presents/s` measurement in §3.3. A dropped present
  costs its own draw; a wait costs more, every time.
* **Replacing the stroke renderer.** Direct2D realizations already solve the problem Metal's fragment cost
  posed, and the pan path rebuilds nothing. Replacing it would be a rewrite in search of a measurement
  that does not exist yet (§6.4).
* **`target-cpu=native`.** Tried on the Mac for no measurable gain (~48.6 µs against ~49.8 µs) and
  deliberately not added; there is no Windows measurement, but there is also no reason to expect one.

## 7. The two platforms, side by side

| | macOS | Windows |
|---|---|---|
| Canvas | Metal, one MSAA pass, fragment-bound | Direct2D, baked realizations, draw-bound |
| Pacing | frame cap **and** `displaySyncEnabled`, both real | frame cap only; presents immediately, always |
| Scroll | trackpad pixels; `position` = where the gesture began | trackpad pixels **or** wheel lines (dropped); `position` = the live cursor |
| Edge swipe | works | **unreliable**, because the band is read from a position that moves (§3.1) |
| Pinch | `magnify` gestures (`NSEvent`) | Direct Manipulation scale, classified against scroll by GPUI |
| Wheel | dropped, by choice and with little lost | dropped, which on a desktop means no scrolling at all (§3.2) |
| V-Sync switch | works, and measurably, when the cap is `Unlimited` | present on the bar, and does nothing (§3.3) |
| Unit scale | `0.75` | `1.0` |
| Frame cost | measured: 143–146 draws/s on 8,182 strokes | **unknown** (§6.4) |
| Hardware clause | `17 MiB · 0% cpu · 17 threads` | the same shape, from `sysinfo` + a toolhelp snapshot (§8) |

If one sentence is wanted:

> **The shared half of this app now runs on Windows as well as it does on the Mac; the three things that
> do not are all cases where a Mac-shaped assumption about the *platform* leaked into code that was
> supposed to be about the *view* — into a scroll position, a wheel rule, and a switch on the bar.**

## 8. The hardware clause (both platforms)

One figure on the status line is gathered rather than computed: **what the process is costing the
machine.** It reads `17 MiB · 0% cpu · 17 threads` and it is the same shape on both platforms by
construction, which is what makes the two lines comparable — the question *"why does this feel different
on Windows"* is answered by numbers, and numbers that are formatted differently on each side would not
answer it.

| Piece | macOS | Windows |
|---|---|---|
| resident memory, cpu share | `sysinfo` (`Process::memory`, `Process::cpu_usage`) | the same crate, the same pin |
| thread count | `proc_pidinfo(PROC_PIDTASKINFO)` → `pti_threadnum` | a toolhelp snapshot (`CreateToolhelp32Snapshot` + `Thread32Next`) |
| seam answer | `apps/macos/src/platform/vitals.rs` | `apps/windows/src/platform/vitals.rs` |
| the numbers' meaning | `crates/note-ui/src/timing.rs` (`Vitals`) | the same code — it is above the seam |

Two things about it are worth recording, because both were discovered rather than assumed.

* **`sysinfo` cannot count threads here.** Its `Process::tasks` is Linux-only and returns `None` on macOS
  *and* Windows. The first version of this code used it, the test asserting "a running process has
  threads" failed with `0`, and the fix was one small per-platform call on each side. This is the one
  place in the feature where the two platforms genuinely diverge.
* **`sysinfo` is listed in the *app* manifests, not in `note-ui`.** It is a cross-platform crate and it
  would have been shorter to put above the seam, but it declares `[target.…]` dependencies, and
  [MACOS.md §6](MACOS.md#6-what-must-not-change) forbids the view from sitting on a dependency edge that
  does. The view is handed three plain numbers instead.

What it costs: one `refresh_processes(ProcessesToUpdate::Some(&[pid]))` a second — measured at 77 µs
first call and ~200 µs on a settled one — plus the one per-platform thread call. It is sampled on the
existing once-a-second status refresh, and **not at all while the status line is hidden**, so a reader who
turns the line off also stops paying for the figure.

## 9. Keeping this document honest

This file is only useful while it is checked. The two checks that settle most of it:

```bash
# Nothing in apps/windows handles a scroll: every gesture above the seam speaks for Windows too.
grep -rn 'ScrollDelta' apps/windows/src        # (no output)

# Nothing above the seam names a platform — except two narrow, legitimate cases (see below).
grep -rn 'cfg(target_os' crates/note-ui/src
#   pdfium/api.rs:129    a library *file name*: libpdfium.dylib
#   keys/tests.rs:106    a mac-only chord-formatting test
```

The second check is the invariant [MACOS.md §6](MACOS.md#6-what-must-not-change) states, and the two
exceptions are exactly the kind it allows: a `const &str` naming a shared library, and a `#[test]` that
only makes sense on one platform. Neither is behaviour. The reason that distinction matters here is that
**every entry in §3 exists because a *behavioural* assumption got above the seam, where this grep cannot
see it**: `event.position` looks like a number on both platforms and means two different things, a
`ScrollDelta::Pixels` branch reads like a filter on a format nobody argues about, and `fn set_vsync` is
present and typed on Windows while doing nothing. No search finds those.

The first check is why §3.1 and §3.2 cannot be fixed in `apps/windows`: the code that decides them is not
there. It is all above the seam, which is the only place either can be fixed.
