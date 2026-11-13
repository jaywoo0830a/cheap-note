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
> parallel zoom rebuild, the pen-pan clamp, the jitter filter, the frame cap. What did *not* reach
> Windows is the part whose meaning depends on the platform underneath: **`ScrollEvent.position` is a
> different thing on Windows** (the live cursor, not where the gesture began), and **`set_vsync` is a
> no-op** while the setting is still stored. The wheel, which used to be dropped, is now read on Windows
> through a seam answer (`Platform::wheel_pans`), and the V-Sync switch is left out where the surface
> cannot honour it (§3.2, §3.3). The Windows canvas also has a cost model of its own — baked Direct2D
> realizations rather than a multisampled Metal pass — and a benchmark of its own (§6.4). One release
> did not start at all: GPUI 0.3.12 took the DirectComposition layer the canvas lives in, and the
> workspace now pins a one-line patched copy of that crate (§3.5).

## 1. How to read the tables

Three verdicts are used, and they mean specific things:

| Verdict | Means |
|---|---|
| **shared** | the code is in `crates/note-ui` or `crates/note-format` and names no platform; it is the same code on both machines, and any difference would be a bug in a platform crate |
| **Mac only** | the code is in `apps/macos` (or gated on `cfg`), so Windows never runs it. *Not* a regression — something the Mac build can do that Windows has no implementation of |
| **broken on Windows** | the shared code assumes something the Windows platform does not do, so the feature silently does nothing or does the wrong thing. **This is the list that matters** |

The third verdict is the one worth being strict about, because it is invisible: the app compiles, the
tests pass, and the feature is simply absent at run time. A fourth label, **unknown**, appeared once in
the earlier version of this document, for Windows' frame cost; §6.4 has since measured it, so the label
is retired.

## 2. What reached Windows for free

All of the following live above the seam and therefore run identically on both platforms. This is the
seam doing its job, and it is worth saying plainly so nobody re-does the work.

| Change | Where | Why it is the same on Windows |
|---|---|---|
| The zoom rung no longer costs the frame: `request_detail` (frame) split from `build_detail` (pump), on `DETAIL_SETTLE_INTERVAL` (32 ms) | `crates/note-ui/src/app.rs`, `ink.rs` | the split is a scheduling decision inside the view; the frame loop is the same loop on both |
| `build_detail` parallelised with `rayon` — 23.6 ms → 5.0 ms, 12 threads verified | `crates/note-ui/src/ink.rs` | `rayon` is a dependency of `note-ui`, which has no `[target.…]` section, so it uses Windows' own pool there |
| The pen pan is clamped at `settle_pan` (gesture end) instead of on every reading | `crates/note-ui/src/app.rs` | the bug was `Viewport::hold_pan` called mid-gesture, above the seam |
| `PenJitter` (`PEN_JITTER_SPAN`, 0.5 px), on by default, gestures only | `crates/note-ui/src/pen_gesture.rs` | a threshold on a reading's position, and the reading type is the view's own |
| `FrameCap` and the `Frames` select, honoured by `held_by_cap` in `draw_canvas` | `crates/note-ui/src/app.rs` | the cap is checked against the time since the last present, which is the view's clock |
| Pinch and pen-zoom describe the sheet per event (`describe_sheet`) instead of leaving intermediate frames stale | `crates/note-ui/src/app.rs` | the description is built from the layout the view keeps |
| `⌘\` / `Ctrl+\` toggles the bar (`chord::BAR`), label computed from the chord | `crates/note-ui/src/keys.rs` | `keys::is_primary` resolves `secondary` per platform, so Windows gets `Ctrl+\` |
| `flag_of` accepts `1`/`0`, any case, with whitespace — which stopped two notes locking out | `crates/note-format/src/store.rs` | the note format is identical on both machines, so that bug was only reachable on the machine that wrote `1` |
| `FACET` 1.5 → 2.0 on the outline | `crates/note-format/src/ink.rs` | one number, read by every outline builder |

One change that used to be in this table is **not** shared, and the earlier version of this document
said it was: the three-buffer upload fix lives in the *macOS surface* (`refresh_ink` returns `bool` and
clears its own slots — `apps/macos/src/platform/metal.rs`). Windows never uploads geometry per frame at
all: Direct2D realizations live on the device and are reused (`apps/windows/platform/ink_layer/render.rs`),
so there are no three buffers to refill. It is recorded in §4, where Mac-only things belong.

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
  display binds first on both, which is why the V-Sync switch shows nothing on the Mac until the cap is
  loose — and on Windows it is not offered at all (§3.3).

## 3. What was broken on Windows

Four things were. Two now have a seam answer — the wheel is read where the platform says it is, and the
V-Sync switch is left out where the surface cannot act on it — and one remains: the edge swipe's origin.
The fourth was the window itself: a GPUI update took the DirectComposition layer the canvas lives in, so
the app did not start at all; it is fixed by a pinned, one-line patch (§3.5).
The edge-swipe one is invisible in the way this section is about: it compiles, the tests pass, and the
feature is unreliable at run time.

### 3.1 An edge swipe's origin is not reliable

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

That is true of macOS. Windows does not report the gesture's start. GPUI's Windows backend reads the
cursor afresh for every event:

```rust
// gpui-pre-windows 0.3.12, src/direct_manipulation.rs — OnContentUpdated
let position = self.mouse_position();          // the system cursor, every update
…
let dx = x_offset - last_x;
let dy = y_offset - last_y;
… ScrollEvent { position, delta: ScrollDelta::Pixels(point(px(dx), px(dy))), … }
```

`mouse_position` is `GetCursorPos` (`direct_manipulation.rs:196-203`). What that proves is that
`position` is the **system cursor**, not necessarily the gesture's contact. GPUI does route a
precision touchpad's `PT_TOUCHPAD` contact into Direct Manipulation, but a two-finger scroll normally
leaves the cursor parked; a pen may move it. So the earlier sentence “`position` is where the fingers
are now” is too strong: it is where the cursor is, and whether the cursor follows the gesture is the
device's answer.

The failure is real either way — a band measured from the wrong place is a page turn that cannot be
aimed — but that distinction changes the fix. A quick swipe can arm on a reading whose x has already
left the band, and nothing turns; **it is not deterministic**, which is worse than not working.

**Why this is Windows-only.** `apps/windows` has no scroll code at all — `grep ScrollDelta apps/windows`
finds nothing — so every scroll and pinch on Windows arrives through GPUI's own window procedure. The
wheel now has a seam field (§3.2), but the edge origin does not: the code that decides it is above the
seam, where a platform answer cannot reach it.

**What would fix it, in the order worth trying:**

1. **Recover the origin, with the first delta included.** `EdgeSwipe` already accumulates a signed
   `travel`, the running integral of the deltas. At the arming reading `travel` is still zero, so the
   first origin is `position - delta`; later readings can use `position - travel`. This is only valid
   where `position` really follows the gesture, so it has to be gated by a seam answer — a bool such
   as `scroll_position_is_live`, answered `true` by Windows and `false` by macOS — and tested on a
   pen, a touchscreen and a precision touchpad before it is trusted. A `cfg` is not allowed; a seam
   bit leaves macOS, whose position is already the start, unchanged.
2. **A platform answer for the gesture's contact.** GPUI carries a `ScrollSource`, but on Windows both a
   wheel and Direct Manipulation report `ScrollSource::Pointer`, so it cannot answer this. A new seam
   method could carry the contact's start from the platform that has it.
3. **Declare the gesture unsupported on devices whose position is not the contact**, and say so in §7.

Option 1 is the smallest start, but only after the device test: the information is in the event only
when the event's `position` is the gesture's.

### 3.2 A wheel is read where the platform has one

The rule used to be “a wheel reports lines and a trackpad reports pixels, so a wheel is dropped”. That
is still the rule on macOS, where the trackpad is part of the machine, but on Windows it meant a
plugged-in mouse could not move the sheet at all. The wheel is therefore a seam answer:

- `crates/note-ui/src/app.rs` — `scroll_of` turns a `ScrollDelta::Lines` reading into
  `WHEEL_PAN_PER_LINE` logical pixels (32 at 96 DPI) when the platform answered
  `Platform::wheel_pans` `true`. `on_scroll` still feeds only `ScrollDelta::Pixels` into `EdgeSwipe`,
  so a wheel can never turn a page — it has no gesture and no origin to measure.
- `apps/windows/src/platform/mod.rs` answers `true`; `apps/macos/src/platform/mod.rs` answers `false`
  and keeps the old behaviour.

GPUI's Windows backend gives both wheel messages the shape this reads
(`gpui-pre-windows 0.3.12`, `src/events.rs`):

```rust
// WM_MOUSEWHEEL
delta: ScrollDelta::Lines(…wheel_distance…)
// WM_MOUSEHWHEEL
delta: ScrollDelta::Lines(Point { x: wheel_distance, y: 0.0 }),
```

The number 32 is chosen rather than measured: GPUI has already read the system's lines-per-notch
setting and turned a notch into that many lines (three by default), so the only remaining guess is how
tall a line is. A line of text plus its leading is about 32 logical pixels at 96 DPI. A three-line
notch therefore moves the sheet 96 px. The test that pins the rule is
`a_scroll_is_a_trackpad_or_a_wheel_the_platform_reads` in `crates/note-ui/src/app/tests.rs`; the Mac's
side of the trade is still written down in [VIEW.md §9](VIEW.md#9-gestures).

### 3.3 The V-Sync switch is gone where it cannot act

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

What was missing was the last step: the switch was still on the bar, and pressing it changed nothing.
The seam now answers the question the switch asks:

```rust
// crates/note-ui/src/platform.rs
fn honours_vsync(&self) -> bool { true }       // the default, for a surface with no opinion
// apps/windows/src/platform/ink_layer/mod.rs
fn honours_vsync(&self) -> bool { false }
```

`NoteApp::honours_vsync` asks the installed surface, and the bar leaves the divider and the switch out
when the answer is `false` (`crates/note-ui/src/app.rs`). The setting still round-trips in the note;
what is gone is the control that could not do anything. Labelling the absence in a tooltip was the other
option, and hiding it is the one taken: a disabled control still asks the reader to learn a platform
fact to understand why it is disabled.

### 3.4 The wheel's line height is a choice, not a measurement

The 32 px above is the one Windows-specific number this work introduces. It is a constant in
`crates/note-ui/src/app.rs` rather than a setting, because it is a reading preference and not a note
property; if a measurement says it is wrong, the number changes in one place. The wheel's *lines* are
not guessed — GPUI applied the system setting before the view sees them.

### 3.5 A GPUI update took the layer the canvas lives in

The release build did not start. There was no window and no message — double-clicking
`cheap-note.exe` produced a process that was gone before it could be seen — and the event log
recorded `0xc0000409`, the exception `abort` raises through `__fastfail`, against the executable. A
debug build, which has a console, had the sentence:

```
thread 'main' panicked at apps\windows\src\platform\mod.rs:
a canvas for the window: "composing the canvas under the window's content: DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED (0x88980800)"
```

The cause is the arrangement `apps/windows/src/platform/ink_layer/device.rs` describes: the interface
and the canvas share one window by taking one DirectComposition target each. Microsoft's
[`CreateTargetForHwnd`](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-idcompositiondevice-createtargetforhwnd)
is exact about how many there can be: **at most two targets per HWND, one topmost and one not**. GPUI
draws the bar, the pills and the lists, so it must be the topmost one; the canvas asks for
`topmost = false` and is drawn behind it.

`gpui-pre-windows` 0.3.11 did that — its `DirectComposition::new` called
`CreateTargetForHwnd(hwnd, true)`. 0.3.12, which `update crates` moved to, calls it with `false`: the
dependency took the only non-topmost layer for itself. The canvas then has no layer left, and its own
`CreateTargetForHwnd` fails with `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED` before the first frame
can be drawn. The app's `expect("a canvas for the window")` turns that into the panic above.

The fix is one word in a vendored copy of the dependency:

```diff
-let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, false) }?;
+let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }?;
```

`third_party/gpui-pre-windows/` is the published 0.3.12 crate with only that line changed (and the
reason written above it); the workspace `Cargo.toml` selects it with `[patch.crates-io]`, and the
vendored README records the check to make when GPUI is next upgraded. The crate is Apache-2.0, and its
licence is carried with it.

The failure was silent because two release-only choices met: `panic = "abort"` (so the abort path is
taken rather than an unwind) and a windows-subsystem image (so there is no console for the default
panic hook to print to). `apps/windows/src/main.rs` now installs the Windows counterpart of the macOS
hook: the message and an optional backtrace go to stderr when there is one, and to
`cheap-note-panic.log` beside the executable otherwise — and to the temporary directory if the
executable's folder is not writable. A release crash is a sentence in a file rather than a process that
vanished.

## 4. What is Mac only, and rightly

Not a defect list — a map, so nobody hunts for a Windows equivalent of something that has none.

| Piece | Where | Why Windows has no counterpart |
|---|---|---|
| `CAMetalLayer.displaySyncEnabled`, and `CHEAP_NOTE_NO_VSYNC` overriding it | `apps/macos/src/platform/metal.rs` | Windows has no choice to make: `Present(0, …)` is the whole policy (§3.3) |
| The three-buffer upload fix: `refresh_ink` returns `bool` and clears its own upload slots | `apps/macos/src/platform/metal.rs` | Windows uploads no geometry per frame: Direct2D realizations live on the device and `apps/windows/src/platform/ink_layer/render.rs` reuses them, so there are no three buffers to refill |
| The `vsync` field's *effect* | `crates/note-format/src/settings.rs` | it is stored on both and honoured on one; the format is deliberately shared, and the switch is only shown where a surface can honour it (§3.3) |
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
| What costs the frame | the **fragment** stage — a window-sized multisampled pass plus the page texture. Measured at 16.4 M MSAA samples, and this is what the raised `FACET` could not help | the **draw** of the realizations, each already baked. One at a time it measured **~21 µs a stroke** — 1.69 ms for 29 strokes, 7.87 ms for 316; in chunks of 32 (§6.4) a page of 8,182 draws in 1.02 ms |
| What a zoom rung rebuild costs | the frame, until `request_detail`/`build_detail` were split | a rebake of every realization — the same handful of rebuilds `InkDocument::set_zoom` forces, and now off the frame on both platforms because the *view* schedules it. Chunking took 8,182 strokes from 257–303 ms to about 140 ms |
| What panning costs | a fresh pass over the whole canvas | the same transform applied to already-baked geometry; nothing is rebuilt. Measured at 1.10 ms for 8,182 strokes in chunks |
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
* **Opaque runs are baked in chunks of 32** (`CHUNK_STROKES`), one `ID2D1GeometryRealization` for the
  run. The growing tail, a highlighter (whose overlaps would otherwise be composited once instead of
  twice) and a stroke a selection is moving stay per-stroke. The ribbon outline keeps one winding
  direction even when a stroke is drawn backwards (a test pins it), and a chunk normalizes the sign
  anyway so that a combined fill cannot cancel to a hole where two strokes overlap.

## 6. Windows-only work, and what is left

The first item is the one still open; the wheel, the V-Sync switch and the benchmark are done. The
reason each was *Windows* work rather than shared work is kept beside it.

### 6.1 Fix the edge swipe's origin (§3.1)

Highest value, because the gesture is currently non-deterministic here. The recommended start — recover
the origin as `position - delta` on the first reading, then `position - travel` — needs one seam bit
(`scroll_position_is_live`) and no `cfg`: Windows answers `true` and macOS `false`, so the Mac's existing
behaviour is unchanged. A test can be written in `crates/note-ui/src/app/tests.rs` by feeding readings
whose `position` **advances** as a Windows backend's would. The existing edge-swipe tests do not do that:
they call `EdgeSwipe` directly and never pass a position at all, and there is no `on_scroll`-level test
in the suite. A test that feeds an advancing position would therefore fail today, which is the point of
writing it — but it should be added only after the device test §3.1 describes, because the correction is
sound only where the cursor follows the gesture.

### 6.2 The wheel (§3.2) — done

`Platform::wheel_pans` is the seam answer, and `WHEEL_PAN_PER_LINE` is the one number it needed. The
wheel is read on Windows and stays unread on the Mac; a wheel is never an edge gesture. The test is in
`crates/note-ui/src/app/tests.rs`.

### 6.3 The V-Sync switch (§3.3) — done

`InkSurface::honours_vsync` is the seam bit, defaulted to `true`; Windows answers `false` and the bar
leaves the switch out. The setting still round-trips in the note.

### 6.4 Measure the Windows canvas

The gap is closed. `cheap-note-bench` is a console binary in `apps/windows` — separate from the app,
whose release image is a `windows` subsystem one with no console to print a table to — and it draws
through the same `Device` and `Renderer` the window does:

```sh
cargo run --release -p cheap-note --bin cheap-note-bench
cargo run --release -p cheap-note --bin cheap-note-bench -- --strokes 0,29,316,1000,8182 --frames 60
```

It draws a synthetic page (desk, shadow, sheet, forty rules and the ink) into a hidden window's
composition swap chain. `first` is the frame that bakes the page, `warm` is the mean of the frames
after it, `rebuild` is one frame after a detail rebuild, `pan` is the same ink moved with no rebuild,
and `present` is the compositor's own cost, timed apart from the draw.

On the reference Windows machine, at 1280×900 and 1.0×, sixty warm frames (the final run; `first`,
`rebuild` and `pan` are single frames):

| strokes | first | warm draw | worst warm | rebuild | pan | present | frame (draw + present) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 0.56 ms | 0.047 ms | 0.110 ms | 0.038 ms | 0.038 ms | 0.17 ms | 0.22 ms |
| 29 | 1.89 ms | 0.149 ms | 0.694 ms | 1.18 ms | 0.115 ms | 0.17 ms | 0.32 ms |
| 316 | 7.44 ms | 0.144 ms | 0.709 ms | 7.11 ms | 0.123 ms | 0.35 ms | 0.50 ms |
| 1,000 | 18.5 ms | 0.167 ms | 0.873 ms | 16.8 ms | 0.147 ms | 0.24 ms | 0.40 ms |
| 8,182 | 136 ms | **1.02 ms** | 2.38 ms | 141 ms | **1.10 ms** | 1.00 ms | **2.03 ms** |

The change that produced the right-hand columns is the renderer's chunk cache
(`apps/windows/src/platform/ink_layer/render.rs`): consecutive opaque strokes of one colour are baked
32 at a time into one `ID2D1GeometryRealization`, while the growing tail, a highlighter, and a stroke a
selection needs on its own stay per-stroke. The same 8,182-stroke page measured **22.9 ms warm draw and
23.4 ms pan** before that cache; the frame was 23.2 ms. It is now 1.02 ms and 1.10 ms, and 2.03 ms with
the present included — about 490 frames a second of canvas work, against 43 before.

`first` and `rebuild` are still about 135–170 ms on that page: a chunk is one flattening and one
realization, but 8,182 strokes still make 256 of them. That is the remaining Windows hitch after a zoom
rung, and the number to attack next if it is felt. The benchmark exists so that the next attempt is
judged by a measurement rather than by the doc's earlier guess. The `present` column is the
compositor's and moves with whatever else the desktop is doing — it measured between 0.2 and 2 ms across
runs — so the draw columns are the ones to compare between builds.

### 6.5 Things that are *not* worth doing

Written down so they do not get re-litigated:

* **A software / WARP path.** `apps/windows/src/platform/ink_layer/mod.rs` argues this at length: GPUI
  asks for the same hardware device and has no software path either, so a machine without one cannot run
  the window at all.
* **A waitable swap chain.** Tried, and it is the `47 presents/s` measurement in §3.3. A dropped present
  costs its own draw; a wait costs more, every time.
* **Replacing the stroke renderer.** Direct2D realizations already solve the problem Metal's fragment cost
  posed, and the pan path rebuilds nothing. Replacing it would be a rewrite in search of a measurement
  that now exists (§6.4) and does not ask for a replacement.
* **`target-cpu=native`.** Tried on the Mac for no measurable gain (~48.6 µs against ~49.8 µs) and
  deliberately not added; there is no Windows measurement, but there is also no reason to expect one.

## 7. The two platforms, side by side

| | macOS | Windows |
|---|---|---|
| Canvas | Metal, one MSAA pass, fragment-bound | Direct2D, baked realizations, draw-bound |
| Window composition | the canvas is a texture handed to GPUI | two DirectComposition layers in one HWND: GPUI's target topmost, the canvas's not; a GPUI release that takes both layers stops the app starting (§3.5) |
| Pacing | frame cap **and** `displaySyncEnabled`, both real | frame cap only; presents immediately, always |
| Scroll | trackpad pixels; `position` = where the gesture began | trackpad pixels **or** wheel lines, read at 32 px a line; `position` = the system cursor |
| Edge swipe | works | **unreliable**, because the band is read from a position that is not necessarily the gesture's (§3.1) |
| Pinch | `magnify` gestures (`NSEvent`) | Direct Manipulation scale, classified against scroll by GPUI |
| Wheel | dropped, by choice and with little lost | read as lines at `WHEEL_PAN_PER_LINE`; never an edge gesture (§3.2) |
| V-Sync switch | works, and measurably, when the cap is `Unlimited` | setting stored; the switch is left out where the surface cannot honour it (§3.3) |
| Unit scale | `0.75` | `1.0` |
| Frame cost | measured: 143–146 draws/s on 8,182 strokes | measured: **2.03 ms a frame** on 8,182 (draw 1.02 + present 1.00), about 490 fps of canvas work (§6.4) |
| Hardware clause | `17 MiB · 0% cpu · 17 threads` | the same shape, from `sysinfo` + a toolhelp snapshot (§8) |
| Document bytes | `MappedBytes` over `mmap`/`munmap` | the same type over `CreateFileMappingW`/`MapViewOfFile` (§8.1) |
| Memory report | `--memory-report`; GPU from `TASK_VM_INFO` | `--memory-report`; GPU column `—` (§8.1) |

If one sentence is wanted:

> **The shared half of this app now runs on Windows as well as it does on the Mac; the edge swipe's
> origin is the one remaining case where a Mac-shaped assumption about the *platform* leaked into code
> that was supposed to be about the *view*. The wheel and the V-Sync switch now have their seam answers:
> read where the platform says it is, and left out where it cannot act.**

And one failure was not about a gesture at all: a GPUI release took the composition layer the canvas
lives in, so the window did not start until the one-line pin in §3.5.

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

### 8.1 The mapped document and the memory report (both platforms)

Two pieces used to be Mac-only and are now answered on Windows as well.

* **A PDF's bytes are mapped, not copied.** `crates/note-format/src/mapped.rs` uses `mmap`/`munmap` on
  Unix and `CreateFileMappingW`/`MapViewOfFile`/`UnmapViewOfFile` on Windows; the drop returns the
  document's size to the machine rather than to the allocator's free list. A platform with neither call
  still falls back to a copy, and an empty or unmappable file still opens. The Windows consequence is
  that a live mapping keeps `source.pdf` from being truncated, so the document view is dropped before a
  note's document is replaced (see `Note::put_document`).
* **`--memory-report` runs here.** The arithmetic is shared (`crates/note-ui/src/memory_report.rs`); the
  Windows half is `apps/windows/src/platform/report.rs`, which supplies `ProcessStats` and a way out of a
  windows-subsystem image. A release started from a terminal attaches to that console; a redirected run
  uses its stdout; with neither, the table goes to `cheap-note-memory-report.txt` beside the executable
  (the temporary directory is the fallback). The exit codes are the same as macOS: `0` clean, `1` over
  the allowance, `2` the report could not run.

The GPU column is still `—` on Windows: `SystemStats::surface_bytes` has no Windows implementation, so
the report measures resident memory and the page cache rather than a graphics ledger. That is why the
rows to read are `RSS`, `cache` and `pages`: a close that empties the cache and returns the process to
its baseline is the clean one.

## 9. Keeping this document honest

This file is only useful while it is checked. The checks that settle most of it:

```bash
# Nothing in apps/windows handles a scroll: every gesture above the seam speaks for Windows too.
grep -rn 'ScrollDelta' apps/windows/src        # (no output)

# Nothing above the seam names a platform — except two narrow, legitimate cases (see below).
grep -rn 'cfg(target_os' crates/note-ui/src
#   pdfium/api.rs:129    a library *file name*: libpdfium.dylib
#   keys/tests.rs:106    a mac-only chord-formatting test

# The one patched dependency still asks for the topmost layer the canvas does not take (§3.5).
grep -rn 'CreateTargetForHwnd(hwnd, true)' third_party/gpui-pre-windows/src

# The Windows document is mapped through the kernel32 calls, not copied into the heap (§8.1).
grep -n 'CreateFileMappingW' crates/note-format/src/mapped.rs

# The memory report is wired into the Windows main, not only the Mac one (§8.1).
grep -n 'memory_report::requested' apps/windows/src/main.rs
```

The second check is the invariant [MACOS.md §6](MACOS.md#6-what-must-not-change) states, and the two
exceptions are exactly the kind it allows: a `const &str` naming a shared library, and a `#[test]` that
only makes sense on one platform. Neither is behaviour. The reason that distinction matters here is that
**every *behavioural* problem in §3 came from an assumption above the seam, where this grep cannot see
it**: `event.position` looks like a number on both platforms and means two different things, a
`ScrollDelta::Pixels` branch read like a filter on a format nobody argues about, and `fn set_vsync` was
present and typed on Windows while doing nothing. No search finds those. The two that were fixed were
fixed by a seam *answer* — `Platform::wheel_pans` and `InkSurface::honours_vsync` — not by a `cfg`.

The startup failure in §3.5 is the exception, and it is instructive for a different reason: it was not
above the seam at all. It was a dependency's choice about the window's composition layers, a fact the
compiler could not see and no `cfg` in this workspace could change; the answer was to pin the choice.
The third check is there because that pin is load-bearing and one line long.

The first check is why §3.1 cannot be fixed in `apps/windows`: the code that decides the edge band is not
there. It is above the seam, which is the only place it can be fixed.
