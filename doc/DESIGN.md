# The interface's one scale

This document is the reasoning and the contract behind `crates/note-ui/src/design/`: the module that
turns a design length into a whole number of logical pixels, whatever platform the app is running on.
It is meant to be read beside [ARCHITECTURE.md](ARCHITECTURE.md) (what runs where) and
[VIEW.md](VIEW.md) (why the view is arranged as it is).

> **Status.** The view consumes the module. `NoteApp` holds one `LayoutMetrics` snapshot, the
> window-furniture and full-screen lengths are read from it, the bar's **Scale** chooser replaces
> the snapshot in one transaction, and `crates/note-ui/src/prefs.rs` stores `ScaleStep::index()` in
> `prefs.json` beside the notes. The palette follows the same step. What still does not follow it
> are the type ramp and the component library's own control heights; §7.5 and §7.6 say why those
> are a separate ramp rather than four-unit tokens.

If you read only one paragraph, read this one:

> **A design length is written once, in design units at 100%, on a four-unit grid. The person's scale
> is one of seven quarter steps; a pure integer function turns the token and the step into whole
> logical pixels. The platform's display density never enters the calculation. The same step
> therefore produces the same logical layout on every machine, and the layout's arithmetic cannot
> disagree with itself by a fraction of a pixel.**

## 1. Three different things called “scale”

The old arrangement mixed three ideas that have to stay apart.

| Thing | Question it answers | Who owns it |
|---|---|---|
| **Design size** | how large is this spacing, control, or margin in the design? | the design system |
| **Interface scale** | how large does *this person* want the interface? | the person, as a preference |
| **Display density** | how many physical pixels is one logical pixel on this screen? | the operating system |

The module owns the first two. It deliberately knows nothing about the third.

A *design unit* (du) is the number the layout was drawn at when the app shipped: 4 units of space,
148 units of bar height, 24 units of page margin. Those numbers do not change when a person changes
their interface scale. What changes is how many logical pixels one design unit becomes.

The operating system already converts logical pixels to physical pixels, monitor by monitor. Feeding
a platform's DPI into this module as well would create a second multiplier with its own rounding,
its own lifetime, and its own idea of what “100%” means. The old `Platform::unit_scale` did exactly
that for one part of the palette: macOS said `0.75` and Windows said `1.0`. It made the palette a
different size from the bar beside it. This module replaces that idea with one scale for every
length.

### Logical equality is not physical equality

The module guarantees the same **logical** layout on Windows, macOS, or any future platform that
speaks logical pixels. It does not guarantee the same physical millimetres, because a Windows
logical pixel and a macOS point are different physical units. Reaching that would require a
calibration input from each machine — its exact points-per-inch, per monitor, including the monitor
the window is currently on. That is not a pure function of the design system, and chasing it is how
the palette acquired a platform factor in the first place. The operating system's own scale is the
right layer for physical density.

## 2. The grid and the steps

The exactness of the module rests on two deliberately small choices.

### 2.1 Every layout token is on a four-unit grid

`GRID` is **4 design units**. Every token in `tokens.rs` is a multiple of it. `DesignUnit::new`
refuses a number that is not, rather than rounding it: a length off the grid has no exact answer at
a quarter step, and rounding at construction would hide the mistake until two call sites rounded it
differently.

### 2.2 Every interface scale is a quarter step

`ScaleStep` has exactly seven variants:

| Step | Percentage | Persisted index |
|---|---:|---:|
| `P50` | 50% | 0 |
| `P75` | 75% | 1 |
| `P100` | 100% | 2 |
| `P125` | 125% | 3 |
| `P150` | 150% | 4 |
| `P175` | 175% | 5 |
| `P200` | 200% | 6 |

The index is what a preference file should store. It is stable, small, and cannot name a scale the
interface does not have. The percentage is only for the person reading the screen.

### 2.3 Why the arithmetic is exact

At one scale, one grid cell becomes a whole number of pixels:

```text
quantum = (GRID × percent) / 100
        = (4 × percent) / 100
        = 2, 3, 4, 5, 6, 7, or 8
```

Every percent is a multiple of 25, so `4 × percent` is always divisible by 100. The conversion is
therefore:

```text
pixels = (design_units / GRID) × quantum
```

Both factors are integers. There is no floating point, no rounding, and no place for two platforms
to disagree.

The same rule read as examples:

```text
 4 du ×  75% = (4 / 4) × 3  =  3 px
12 du × 175% = (12 / 4) × 7 = 21 px
```

### 2.4 Why not 5% steps?

Because the two promises cannot both hold for arbitrary percentages:

* exact whole logical pixels at every step, **and**
* a scale the person can tune in 5% increments.

A 5% step needs a denominator of 20. A four-unit token does not divide evenly by 20 at every step,
so one of the promises has to give: either output fractional logical pixels, or round and accept
that lengths no longer add up exactly. This module chooses exactness and a closed set of quarter
steps. If finer control is wanted later, it is a different contract — fixed-point subpixels or a
deliberately authored ramp — not a quiet change to this one.

## 3. The API

| Item | What it is | Contract |
|---|---|---|
| `DesignUnit` | a length on the design grid, at 100% | construct only through `new`, which refuses off-grid values; constants are written directly on the grid |
| `GRID` | 4 design units | the quantum that makes quarter steps exact |
| `ScaleStep` | one of the seven allowed scales | `percent()` for display, `index()` / `from_index()` for persistence |
| `px()` | the conversion | `const fn`, integer-only, no platform or window input |
| `LayoutPx` | an already-scaled whole logical pixel | the boundary marker between design units and a framework's length |
| `LayoutMetrics` | every layout length at one scale | one snapshot, all fields from the same step |
| token constants | the design's named lengths | every one is a multiple of `GRID` |

`px()` is the rule; `LayoutMetrics::at()` is how a renderer should use it. A render pass holds one
snapshot and reads fields from it. It does not call `px()` at every use and it never multiplies an
already-scaled value again. That is what keeps a control and the gap beside it on the same scale even
after a person changes the preference.

`LayoutMetrics` is `Copy`, small, and independent of GPUI. An adapter that turns `LayoutPx` into
GPUI's `Pixels` or `Rems` belongs outside the module, at the framework boundary.

## 4. The tokens

The tokens currently in `tokens.rs` are the first layout vocabulary, not a complete style guide. They
are deliberately coarse: a small number of steps that compose, not a bespoke number for every
control.

| Group | Tokens (design units) |
|---|---|
| Spacing | `SPACE_1` 4, `SPACE_2` 8, `SPACE_3` 12, `SPACE_4` 16, `SPACE_6` 24, `SPACE_8` 32, `SPACE_12` 48, `SPACE_16` 64 |
| Controls | `CONTROL_SMALL` 24, `CONTROL_MEDIUM` 32, `CONTROL_LARGE` 40, `CONTROL_GAP` 8 |
| Window furniture | `BAR_HEIGHT` 148, `BAR_MARGIN` 12, `PAGE_MARGIN` 24, `SCREEN_MARGIN` 56, `FOOT_BAND` 72, `FOOT_SLACK` 12, `STATUS_DROP` 12 |
| Radii | `RADIUS_SMALL` 4, `RADIUS_MEDIUM` 8, `RADIUS_LARGE` 16 |

The names are the design's, not the framework's. `BAR_HEIGHT` is the estimate the view falls back to
before it has measured the bar, and `FOOT_SLACK` is the distance around a desk pill that still counts
as a press on it. The module does not decide how those values are used; it decides only what they
become at a scale.

The categories are also the test surface. `SPACING`, `CONTROLS`, `LAYOUT`, `RADII`, and
`CONTROL_GAP` are walked together, so a token that drifts off the grid is caught by the same suite as
a quantum that stops being exact.

### What is not a layout token

Some lengths must not follow this grid:

* **A hairline.** A one-pixel border is a device-level mark, not a proportion. Scaling it to 1.5 px
  makes a line that is neither one pixel nor a clean fraction of one.
* **A pen's width and the ink's geometry.** Those are physical writing properties, measured in
  millimetres and in the document's own space. Changing the interface's scale must not change what a
  stroke looks like on the paper.
* **The page's zoom.** The zoom is a property of the view and the sheet, not of the controls around
  it. The interface scale and the page zoom are two different magnifications.
* **Typography, while it is a ramp.** A type size such as 13 px is not on the four-unit grid. Type is
  usually authored as a deliberate set of sizes per scale rather than as an exact linear multiple.
  The module can grow a type ramp later; it should not pretend a 13 px font is a layout token.

## 5. The scale table

A few representative tokens, resolved at every allowed step. The module tests every token at every
step; this table is the shape of the arithmetic.

| Design units | 50% | 75% | 100% | 125% | 150% | 175% | 200% |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 4 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
| 8 | 4 | 6 | 8 | 10 | 12 | 14 | 16 |
| 12 | 6 | 9 | 12 | 15 | 18 | 21 | 24 |
| 16 | 8 | 12 | 16 | 20 | 24 | 28 | 32 |
| 24 | 12 | 18 | 24 | 30 | 36 | 42 | 48 |
| 32 | 16 | 24 | 32 | 40 | 48 | 56 | 64 |
| 56 | 28 | 42 | 56 | 70 | 84 | 98 | 112 |
| 148 | 74 | 111 | 148 | 185 | 222 | 259 | 296 |

Every row is exactly the percentage of its 100% value, and every cell is an integer. That is the
property the module exists to make structural rather than accidental.

## 6. What the module does not do

It is a scale, not an application framework. Deliberately absent:

* **A preference file.** `ScaleStep::index()` and `from_index()` are the persistence contract, but
  loading and saving a machine preference belongs to the app: `crates/note-ui/src/prefs.rs` keeps
  the index in `prefs.json` beside the notes. The file holds the index, never a floating-point
  percentage.
* **A GPUI adapter.** `LayoutPx` is not a `Pixels` or a `Rems`. The conversion belongs at the window
  boundary, where the framework's current window and rem size are known.
* **Platform calibration.** No DPI, no monitor identifier, no `cfg(target_os)`, and no per-platform
  multiplier. The OS owns physical density.
* **Typography.** The type ramp described above is a separate concern. A font size can be scaled by a
  future ramp without pretending it is a four-unit token.
* **A theme.** Colours, fonts, and shadows are not lengths. The module does not read or write the
  GPUI theme.
* **The view's measured state.** The bar's real bottom edge and the pills' boxes are measured by the
  layout, not computed here. The module supplies the fallback estimates; the last frame supplies the
  truth.

## 7. The rules for wiring it in

The view consumes this module; the following rules keep the contract intact as it grows.

### 7.1 One source, one snapshot

All layout lengths should come from `LayoutMetrics::at(step)`, not from local `px()` calls. The
snapshot is computed once when the scale changes and passed to the render path; `window_px` in
`app.rs` is the one place it is opened into GPUI's logical pixels. A per-call conversion would
recreate the disagreement this module is meant to remove. The home, bookmarks and contents screens
read the same snapshot for their margins, card radii and spacing.

### 7.2 The preference stores an index

The preference file holds `ScaleStep::index()` as an integer. It does not hold `1.25`, a percentage
string, or a platform factor. `from_index()` is the checked way back; an unknown index falls back
to the shipped step. `crates/note-ui/src/prefs.rs` is that file's owner, and the bar's Scale chooser
is the one control that changes it.

### 7.3 Change the scale in one transaction

Changing the scale has to do all of the following together:

1. replace the current `ScaleStep`;
2. recompute the `LayoutMetrics` snapshot;
3. tell the framework's theme or root about the new base size, if the adapter uses one;
4. discard measurements made at the old scale — at minimum the bar's measured bottom and the desk
   pills' boxes — because they are in old logical pixels;
5. redraw.

Step 4 is easy to miss. A measured line from the old scale is not wrong by a percentage; it is wrong
by whatever the layout actually did, and it will remain the line the pen is judged by until the next
frame measures it again.

`NoteApp::set_interface_scale` is the one implementation of those five steps. The framework has no
base size to tell: the adapter writes logical pixels into every length, so step 3 has nothing to do.

### 7.4 Convert only at the framework boundary

An adapter is allowed to do this:

```text
LayoutPx → framework Pixels / Rems → window
```

Nothing else in the app should cast a `LayoutPx` to a raw number and multiply it. If a component
needs a length in a framework-specific type, the adapter supplies it; the design module stays
integer-only and framework-free.

### 7.5 Leave non-layout lengths alone

Hairlines, ink widths, page zoom, and PDF rasterisation do not move to this grid. The interface
scale changes the furniture around the page, not the page's contents or the pen's physical mark.

### 7.6 Keep the type ramp separate

If the interface needs text to follow the scale, add a `TypeRamp` beside `LayoutMetrics` rather than
putting font sizes into `DesignUnit`. The ramp may be authored per step, and its values do not need
to divide by four. The two systems can share `ScaleStep` without sharing their arithmetic.

## 8. What the tests pin

The module is pure, so the tests can be exhaustive instead of representative. The suite in
`crates/note-ui/src/design/tests.rs` covers:

| Test | Property |
|---|---|
| `the_grid_refuses_off_grid_lengths` | a length not divisible by four is not a `DesignUnit` |
| `every_token_is_on_the_grid` | every named token is constructible and has a whole number of grid cells |
| `the_steps_are_the_seven_quarter_steps` | the seven percentages, their order, and their persisted indices |
| `one_hundred_percent_is_identity` | 100% returns the token's own number |
| `every_step_is_an_exact_whole_pixel` | for every token and step, `pixels × 100 == units × percent` |
| `scaling_is_linear` | `scale(a + b) == scale(a) + scale(b)` for every pair of tokens |
| `scaling_is_monotonic` | no token becomes smaller as the step increases |
| `the_scale_table_is_what_the_design_expects` | the golden values for the bar, page margin, gap, and radius |
| `metrics_are_the_tokens_at_the_same_scale` | every `LayoutMetrics` field is the same scale's conversion |
| `the_default_metrics_are_the_shipped_design` | `Default` is 100%, not an accidental zero |

The exactness test is the important one. It does not merely assert that the result “looks close”: it
compares the integer result against the percentage written as an integer equation, so a change to
`GRID`, to a percent, or to the quantum formula fails immediately. The linearity test is the second:
it proves that the scale can be applied to a sum and to its parts with the same answer, which is what
keeps nested spacing from drifting.

Run just this suite with:

```text
cargo test -p note-ui design
```

At the time of writing the module adds ten tests, and the full `cargo test -p note-ui` suite passes.
The module compiles as part of `note-ui`; there is no window, GPU, or platform setup in any of these
tests.

## 9. Why this is a module, not a crate

The module has a crate's discipline without a crate's manifest:

* it imports nothing from `gpui`, `app`, `platform`, or `theme`;
* its public surface is re-exported from `design/mod.rs`;
* its tests can be run on any target;
* `tokens.rs` has no dependencies at all.

That is enough while the only consumer is `note-ui`. A separate crate becomes worthwhile when one of
these is true:

1. a second consumer needs the tokens (another application, a CLI, a documentation generator);
2. the purity needs to be enforced by Cargo rather than by review;
3. the design system grows to a size that deserves its own release cadence.

If that happens, extract it as `note-design`, keep it dependency-free, and leave the GPUI adapter in
`note-ui`. Until then, the extra manifest, version, and workspace boundary would buy nothing that the
module boundary does not already provide.

## 10. Where to look

| File | What it contains |
|---|---|
| [`crates/note-ui/src/design/mod.rs`](../crates/note-ui/src/design/mod.rs) | the public surface and the argument for the module |
| [`crates/note-ui/src/design/tokens.rs`](../crates/note-ui/src/design/tokens.rs) | the four-unit tokens |
| [`crates/note-ui/src/design/scale.rs`](../crates/note-ui/src/design/scale.rs) | `ScaleStep`, `LayoutPx`, and `px()` |
| [`crates/note-ui/src/design/metrics.rs`](../crates/note-ui/src/design/metrics.rs) | the resolved layout snapshot |
| [`crates/note-ui/src/design/tests.rs`](../crates/note-ui/src/design/tests.rs) | the exhaustive tests |
| [`crates/note-ui/src/prefs.rs`](../crates/note-ui/src/prefs.rs) | the machine preference: the step's index in `prefs.json` |
| [ARCHITECTURE.md](ARCHITECTURE.md) | where the modules run, and what may wait for what |
| [VIEW.md](VIEW.md) | why the view is arranged as it is, including the lengths this module owns |
| [MACOS.md](MACOS.md) | the platform that made the old palette factor visible |

One sentence to carry away:

> **The scale is one integer step, the grid is four design units, and the output is a whole logical
> pixel. Everything else — DPI, windows, themes, fonts — belongs on the other side of the adapter.**
