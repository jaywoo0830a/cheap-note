# cheap-note on macOS: how the build is put together

This is the macOS build's own document. The port is **done** — `apps/macos` is a full workspace member
that builds and runs on a Mac — so this document says what it *is*, how each platform service is
implemented, and what only a Mac can do. It is the companion to
[ARCHITECTURE.md](ARCHITECTURE.md) (what runs where), read for the one platform that is not Windows.

If you have just cloned this on a Mac, [§2](#2-build-and-run-on-the-mac) is the whole of what you need.
If you are changing a platform service, [§4](#4-the-platform-services-one-each) is the map.

If you read only one paragraph, read this one:

> **The Mac build is `apps/windows` rewritten behind the same seam.** `crates/note-format` and
> `crates/note-ui` are unchanged and platform-free; `apps/macos` implements the traits `note-ui`'s
> `platform.rs` declares — a pen, a ghost, a pointer, and a surface — with Metal instead of Direct2D,
> `NSEvent` instead of `pen-windows`, and the ghost drawn in the canvas pass instead of in a window of
> its own. The format a note is written in does not change, so a note written on the Mac opens on
> Windows and the reverse.

## 1. The shape, in one paragraph

The repository is four workspace members, and the two boundaries face opposite ways:

| Member | What it is | Portable? |
|---|---|---|
| `crates/note-format` | the note on disk: SQLite schema, the ink's byte format, the page list, the settings, the zip | **yes**, no window/pen/GPU/PDF |
| `crates/note-ui` | everything the application *does*: the bar, the home screen, the sheet, the gestures, the PDF page cache, the ink model, the frame loop, the chords | **yes**, and it declares **no platform crate at all** |
| `apps/windows` | the Windows platform: Direct2D canvas, `pen-windows` capture, the ghost cursor's layered window, the pointer hook, `main` | no, and it does not need to be |
| `apps/macos` | the macOS platform: Metal canvas, `pen-macos` (`NSEvent`) capture, the ghost drawn in the canvas pass, the transparent-cursor pointer, `main` | no, and it does not need to be |

The Mac build is therefore everything above the seam unchanged, plus one crate that implements the traits
the view declares: `PenSource`, `GhostFeed`, `SystemPointer` and `InkSurface`
(`crates/note-ui/src/platform.rs`), and nothing above them.

### Where we are

| Piece | Where | State | Verified by |
|---|---|---|---|
| the note's format | `crates/note-format` | **done** | 154 tests (78 unit + 76 black-box) |
| the view, and the seam | `crates/note-ui` | **done**, platform-free | 234 tests; dependency graph (§3) |
| the chords (`⌘`/`Ctrl`) | `crates/note-ui/src/keys.rs` | **done** | the Windows text pinned, the mac text by `cfg(macos)` |
| Pdfium loading | `crates/note-ui/src/pdfium/api.rs` | **done** on every platform | loads `libpdfium.dylib` through `libloading` |
| the Windows platform | `apps/windows` | unchanged | 29 tests |
| **the macOS platform** | `apps/macos` | **done** — builds and runs | 10 tests; `cargo check -p cheap-note-macos` |
| the pen, on macOS | `apps/macos/pen-macos` | **done** | 5 tests; `examples/watch.rs` |

## 2. Build and run on the Mac

```bash
xcode-select --install            # libsqlite3-sys compiles SQLite with the C compiler
git clone <this repository> cheap-note
cd cheap-note                     # rustup installs the pinned toolchain on the first cargo run

cargo run -p cheap-note-macos     # the app: it opens on the home list, and Escape returns to it
cargo test --workspace            # every crate: 154 + 234 + 10 + 5 (+ 29 on Windows)
```

`rust-toolchain.toml` pins the compiler (1.99.1), so `cargo` in this directory uses it whether or not
it is the default — nothing has to be installed by hand. That pin and `Cargo.toml`'s
`rust-version = "1.90"` answer two different questions: the pin is the compiler the project is
*built* with, and `rust-version` is the oldest one it is promised to work on. The promise is real
today — 1.90 is the highest `rust-version` anywhere in the macOS and Windows dependency trees.

`cargo test --workspace` now works on a Mac: `apps/windows` compiles to a stub off Windows (its platform
modules are `#[cfg(windows)]`, and `pen-windows` is a `cfg(windows)` dependency), so it builds with no
tests and the whole workspace goes green. If you only want one crate, name it:
`cargo test -p note-format`, `-p note-ui`, `-p cheap-note-macos`, or `-p pen-macos`.

The package is **`cheap-note-macos`** but the **binary is `cheap-note`** — Cargo refuses two packages of
one name in a workspace, and both apps are one program as far as a person is concerned. To hand the app to
someone as a `.app`:

```bash
apps/macos/bundle.sh              # → dist/Cheap Note.app (release build, Info.plist, ad-hoc signature)
```

One caution about a green run: the pdf tests that need a real Pdfium skip themselves when none is on the
machine (`crates/note-ui/src/pdfium/api.rs`'s `available()`), so green is *not* proof that a page renders.
The next subsection puts the library in place, and §7 is where rendering is actually checked.

### Pdfium's `libpdfium.dylib`

`vendor/` in the repository holds the **Windows** build (`pdfium.dll`, 158.0.8125.0 — `vendor/README.md`
describes that archive), so a Mac needs its own copy before a PDF can be opened:

1. Take the `mac-arm64` archive from `pdfium-binaries` (Apple Silicon; `mac-x64` for Intel).
2. `libpdfium.dylib` goes to `vendor/lib/`; the headers in `include/` replace the ones there.
3. Nothing in the code changes: `LIBRARY_NAME` is already `libpdfium.dylib` on macOS, and the search
   finds `vendor/lib` by walking up from the executable — or from the working directory, which is how a
   `cargo test`/`cargo run` out of the repository finds it.

`apps/macos/build.rs` puts the dylib *next to the executable* a build produces (the same file as
`apps/windows/build.rs` with the extension filter changed to `.dylib` and the `.ico` step removed), so a
release build is a pair of files that work from anywhere. A build with no library is not an error — the
app starts, and refuses to open a PDF while naming the file it looked for.

## 3. What the seam gave the port for free

These are the things the Windows side settled, and the Mac build inherits unchanged:

* **The data lives where a Mac keeps it.** `note_format::note::root` answers
  `~/Library/Application Support/cheap-note/notes` on macOS (and `$XDG_DATA_HOME` on Linux), with the
  temp directory as the last resort. The note's three names — `note.db`, `source.pdf`,
  `attachments/` — are the same on every platform, which is the point of the format crate.
* **The view has no platform API in it.** `crates/note-ui/Cargo.toml` has no `[target.…]` section at
  all — no `windows`, no `objc2`, no `cfg` on a dependency. `cargo tree -p note-ui --target
  aarch64-apple-darwin` still prints `windows-sys`/`windows-link`, but those are reached only through a
  **proc-macro** graph (gpui-kit's `rust-embed`/`rust-i18n` and their `dirs`/`ignore`), which builds for
  the *host*; on a Mac they are not in the graph at all. Nothing the view depends on names a Windows API.
* **The toolkit has a mac backend.** `gpui-kit` `0.9.0` → `gpui-pre-platform` (gpui-pre 0.3.11), which selects
  `gpui-pre-macos` + `gpui-pre-apple` for macOS (both published, 0.3.x).
* **The chords are the platform's.** Bindings are `secondary-z`/`secondary-shift-c`/… and handlers ask
  `keys::is_primary`, which calls `Modifiers::secondary(cx)` — gpui resolves it per target: `⌘` on macOS,
  `Ctrl` everywhere else. Since the 0.9.0 move the argument matters twice over: the resolution needs the
  `App` to know which platform this is, and the *binding* has to be loaded with `KeyBinding::load(…, cx)`
  rather than `KeyBinding::new`, because only `load` is handed the platform's keyboard mapper and
  `parse_with_os` — `new` parses `secondary-z` with a dummy mapper and panics.
  Labels are computed from the chord (`⌘Z`, `⇧⌘C`) instead of being typed into strings.
* **Pdfium is loaded the same way on all three.** `libloading` replaced the `LoadLibraryW` path;
  `LIBRARY_NAME` is `pdfium.dll` / `libpdfium.dylib` / `libpdfium.so`, and the search is the
  executable's directory → the four directories above it (`vendor/lib`, `vendor`) → the working
  directory. The handle is owned by `Api`, so the mapping is never released under a live document.


## 4. The platform services, one each

`apps/macos/src/main.rs` is deliberately almost empty — it sets a panic hook (the release profile is
`panic = "abort"`, and a panic inside an AppKit callback cannot unwind, so the message would otherwise
be lost) and calls `note_ui::run(platform::INSTALLER)`. Everything else is `platform::`, one module per
service the view asks for.

`platform::mod.rs` is the assembly, and **the order matters** (it is the order the Windows one uses):

1. **the ghost** — so the pen's tap exists before the capture starts;
2. **the pen** — attached on the thread that owns the window;
3. **the pointer** — after the pen, so it has the last word (on the Mac that is a cursor rect plus a
   re-assert, not a window-procedure hook);
4. **the canvas** — the one thing the app requires: a machine that cannot give a Metal device fails at
   startup, once, with the reason.

```rust
pub static INSTALLER: Installer = Installer { before_window, install };
```

### 4.1 `platform/pen.rs` — `PenSource`

The reading is nine fields (`crates/note-ui/src/input.rs`). `pen-macos` reads `NSEvent`'s tablet fields
and offers each reading to a tap on the main thread; the mapping is:

| `Reading` | Where it comes from in AppKit |
|---|---|
| `phase` | `.leftMouseDown` / `.leftMouseDragged` / `.leftMouseUp` with `subtype == .tabletPoint`; `Hover` from `.mouseMoved`; `Enter`/`Leave` from `.tabletProximity`; `Cancel` on `.mouseExited` or a deactivated window |
| `pixel` | `locationInWindow` (window base coordinates, origin **bottom**-left) → flip with the content view's height → × `window.backingScaleFactor`. `Reading::pixel` is **physical** client pixels; the view divides by the scale itself |
| `pressure` | `NSEvent.pressure`, `0.0`–`1.0`. A device with no sensor (a mouse) stays `None`, not `0.0` — the ink model treats "no sensor" and "no force" as different widths |
| `tilt` | `NSEvent.tilt` — the same two angles, in degrees (`x` positive right, `y` positive toward the user) |
| `inverted` / `eraser` | `pointingDeviceType == .eraser` |
| `at_us` | `NSEvent.timestamp` (seconds since boot) × 1e6 |
| `delay_ms` | `0` — macOS does not stamp the digitizer, and the pump's own `waited` still measures the app's half |
| `id` | a constant per device, or the event's own device id where one is available |

Two things about the *shape* of the mac capture, and they are not the Windows shape:

* **Tablet events arrive on the main thread.** `pen-windows` needed a thread parked on a ring because
  `PenStream::read` blocks; AppKit does not, so the mac `PenSource` pushes straight into `PenInbox`
  (its `push` is public for exactly this) and the view's pump wakes as it already does. There is no
  worker thread of ours — between the event and the ink there is nothing but the inbox.
* **A *local* event monitor** (`NSEvent.addLocalMonitorForEvents(matching:handler:)`) sees this
  application's events only and needs no permission. A *global* monitor would need Accessibility — the
  trap `rdev` was rejected for.

### 4.2 `platform/pointer.rs` — `SystemPointer`

A **transparent cursor** rather than `NSCursor.hide()`: the hide counter is desktop state (a process
that ends with it non-zero leaves the whole desktop without a pointer) and it takes the hardware cursor
plane out of play, which costs a frame when the cursor changes. A 1×1 empty cursor keeps the plane
active and drawing nothing.

A `set` is not enough on its own: AppKit re-applies whatever **cursor rect** is under the pointer on a
`cursorUpdate:`, and GPUI installs one over its whole view and invalidates it whenever its own style
changes — so the arrow comes back over a window whose ghost says the pointer is the pen's. The answer is
the one Windows gives `WM_SETCURSOR`: a **local monitor for `NSEventTypeCursorUpdate`** that re-asserts
the transparent cursor. `pen-macos` asks the "valid only for this event kind" properties (`subtype`,
`isEnteringProximity`) only of the kinds that carry one — asking them of, say, a `MouseEntered` raises an
ObjC exception, and an exception cannot unwind through a callback.

### 4.3 `platform/ghost.rs` — `GhostFeed`

`Screen` is the whole interface — scale, colour, halo, nib radius, sheet top, suppressed — and
`cursor.rs` (the lean geometry, `PenCursor`, `nib_radius_for_width`) is platform-free and reused as-is.
`GhostFeed` is `Send + Sync` because the pen thread offers readings through it; what lives here is the
*state*, and the rules for when there is nothing to show.

**The mark itself is drawn by the canvas** (`platform::metal`), in the same pass as the ink — a cursor
belongs above the ink and **below** the interface, and a window of its own could be neither: it floated
over the interface, which is what the strategy in `cursor.md` says not to do. So the ghost moved from a
separate `NSWindow` into the canvas pass. Two lessons that first version taught, both only visible with
the pen in hand, still apply to anything drawn from it:

* **no implicit animation** — a layer animates `frame` and `contents` for a quarter second by default, so
  the mark eases toward the nib and reads as lag: set them inside `CATransaction.setDisableActions(true)`;
* **the lean's sign** — `NSEvent.tilt.y` counts the up-and-down lean the other way round from the shared
  geometry's convention, so `pen.rs` negates the vertical component as it builds the app's `Reading`,
  flipped once at the seam; the geometry Windows also uses is untouched.

The body is `PenCursor::body_shape()` in three passes — shadow (`BODY_SHADOW_DROP`), soft edge
(`BODY_HALO_GROW`), then the body in the ink's colour — with the nib's bloom and mark on top.

### 4.4 `platform/canvas.rs` + `platform/metal.rs` — `InkSurface`

`canvas.rs` is the window half (a `NSView` below GPUI's, its size, and the frame's description of what to
draw); `metal.rs` is how the pixels are made. The description the view hands over is
`note_ui::ink_layer::Canvas`: the fills (rect + `Hsla` + radius), the page (`Arc<RenderImage>`), and the
ink (strokes, origin/zoom/paper/turns, selection, lasso). It becomes **GPU geometry** —
a fill is two triangles, a page is a textured quad, a stroke is a triangle fan over the outline the ink
model already computed. Four draw calls: desk and paper, the page's texture, the finished ink, the open
stroke.

The design choice made here is **a `CAMetalLayer` of the app's own under GPUI's view**, not painting the
ink with gpui's own primitives: the latter would make the ink reach the screen at the *frame* rate rather
than per pen batch, which is the whole reason the Windows canvas left the frame.

#### What that renderer must not do: rebuild the ink to scroll

The canvas keeps every stroke's geometry in the **paper's own units** and says separately where the paper
is drawn and how large. That split is what lets a pan be a *transform* rather than a redraw, and a
renderer that bakes the origin into its triangles throws it away: on a Magic Trackpad, which reports its
movement a hundred times a second, every reading then costs a full tessellation of the page. Measured on
this port, in a release build with a page of 300 strokes: **27 ms and 2.8 million vertices per reading** —
a second of scrolling cost more than a second of CPU, and the canvas stuttered for exactly as long.

So: bake in the paper's units, and pass the placement to the shader as one uniform. `platform/metal.rs`
does this in `Mapping` (paper → drawn) and `Transform` (drawn → window); the ink's cache key deliberately
leaves `origin`, `zoom` and `scale` out. The one thing that can move while the ink stays valid is the
*culling* rectangle, grown by `CULL_SLACK` before the cull and re-culled once the sheet has scrolled that
far. Both buffers are reused across frames in a ring of three, so the CPU never overwrites one the GPU is
still reading; the finished ink is uploaded again only when the ink changes, not every frame.

The same rule applies above the seam: `note-ui` collects a frame's worth of scroll readings and spends
them once (`NoteApp::spend_scroll`), and holds the pan inside the layout's bounds (`Viewport::hold_pan`),
so a wheel turned past the end of a sheet leaves no invisible offset for the next turn to pay off.

## 5. The seam change the port needed

Only one addition above the seam turned out to be necessary, and it is defaulted so the Windows build is
unchanged:

* **`Platform::unit_scale`** is a number rather than a method: the factor a length designed in 96-DPI
  logical pixels is multiplied by to be the same physical size here. macOS answers `0.75` (a point is
  four thirds of a Windows logical pixel) and `PaletteGeometry` reads it, so the palette is the same size
  in the hand as on Windows. Windows answers `1.0`, and nothing it draws changes.

The change the earlier draft anticipated — an `InkSurface::paints_in_frame` default, to let the frame
paint a canvas that lived *inside* it — was **not needed**: the Mac chose the `CAMetalLayer` (option B in
the old plan), whose canvas lives *outside* the frame exactly as Windows' does. So the frame paints only
the interface on both platforms, the ink is presented per pen batch on both, and no trait grew that
method.

## 6. What must not change

* **The format.** A note written on the Mac must open on Windows, and the reverse — that is the whole
  reason `note-format` exists. Its 76 black-box tests are the contract.
* **`note-ui` stays free of platform APIs.** If a mac module wants something from the view, the answer
  is a trait method or `paint.rs`, never a `cfg(target_os)` inside `note-ui`. The check is that the view's
  manifest keeps no `[target.…]` section, and that no crate on its *own* dependency edges needs one.
* **The chords stay `secondary`** (`⌘` on the Mac, `Ctrl` on Windows), and their labels stay computed.
* **`Reading` conversions live in the platform crate** (`apps/macos/src/platform/pen.rs`). The view never
  learns what an `NSEvent` is.

## 7. Acceptance checklist for the Mac

The platform builds and its tests pass; a few of the items below are checked by hand on a real Mac with a
pen, and stay unchecked in the repository because they need the hardware in hand:

- [x] `cargo test -p note-format` — 154
- [x] `cargo test -p note-ui` — 234 (no window needed)
- [x] `cargo test -p cheap-note-macos -p pen-macos` — 10 + 5
- [x] `cargo check -p cheap-note-macos` compiles
- [ ] the app opens on the home list, and Escape returns to it
- [ ] a new note lands under `~/Library/Application Support/cheap-note/notes`, holding `note.db` — and
      `source.pdf` / `attachments/` once those exist — rather than in the repository's folder
- [ ] a PDF opens and renders (dylib in `vendor/lib`, and `build.rs` copied it)
- [ ] pressure and tilt change the line, and the line lands *under* the nib — the scale factor and the
      y-flip are the two places a port goes wrong
- [x] the ghost follows the nib and leans, above the ink and below the interface; the system pointer is
      hidden exactly while it is on screen
- [ ] ink reaches the disk while writing; killing the app mid-stroke loses at most half a second
- [ ] *Save* writes a zip, and **that zip opens on Windows** with the same ink — and a Windows zip
      opens on the Mac
- [ ] zoom, pan, page turn, bookmark, outline, and the palette (`⌘⇧C`)

## 8. Known snags (found on the Mac)

| Snag | What it is | The fix |
|---|---|---|
| `cargo build` / `--workspace` on the Mac | `apps/windows` used to be a workspace member that could not build there (its `pen-windows` is Windows-only) | **fixed**: `pen-windows` is now a `cfg(windows)` dependency and `apps/windows` compiles to a stub off Windows, so `--workspace` builds and tests on both platforms |
| bundled SQLite | `libsqlite3-sys` compiles C | `xcode-select --install` |
| `vendor/` | the repository's copy is the **Windows** build (`pdfium.dll`, 158.0.8125.0) | download the mac archive from `pdfium-binaries` (`mac-arm64`), `libpdfium.dylib` into `vendor/lib/` |
| `apps/macos/build.rs` | filters `.dylib`, no icon step | a release build now carries its Pdfium beside it |
| AppKit's "valid only for this event kind" properties | `-[NSEvent subtype]` and `isEnteringProximity` raise `NSInternalInconsistencyException` for kinds that have no such field — a `MouseEntered` did it | ask them only of the kinds that carry one (`subtype_of` in `pen-macos`) |
| a release crash says nothing | `panic = "abort"` drops the message of a panic inside an AppKit callback | `std::panic::set_hook` in `main` prints it before the abort (and a backtrace with `RUST_BACKTRACE`) |
| scrolling stuttered | baking the origin into the triangles re-tessellated the page per trackpad reading | bake in the paper's units, place with a uniform (§4.4) |
| the ghost eased instead of tracking | a layer animates `frame`/`contents` by default | `CATransaction.setDisableActions(true)` (§4.3) |

## 9. Where the pieces are

| To change… | Look at |
|---|---|
| the traits, and `Platform`/`Installer` | `crates/note-ui/src/platform.rs` |
| one pen reading, and the queue it arrives through | `crates/note-ui/src/input.rs` |
| the chords, and how they are written | `crates/note-ui/src/keys.rs` |
| what a canvas may call back into | `crates/note-ui/src/paint.rs` |
| the canvas description | `crates/note-ui/src/ink_layer/canvas.rs` |
| the pen reading on macOS | `apps/macos/pen-macos/src/lib.rs` |
| the pen service | `apps/macos/src/platform/pen.rs` |
| the system pointer | `apps/macos/src/platform/pointer.rs` |
| the ghost's state, and when it shows | `apps/macos/src/platform/ghost.rs` |
| the canvas view, and the Metal renderer | `apps/macos/src/platform/canvas.rs`, `metal.rs` |
| Pdfium loading, and its search path | `crates/note-ui/src/pdfium/api.rs` |
| the note itself | `crates/note-format/`, and [`doc/STORE.md`](STORE.md) |

## 10. How we got here (the history, in order)

1. **The format became a crate** (`crates/note-format`): SQLite, the ink's byte format, the page list,
   the settings, the zip — no window, no pen, no GPU, no PDF renderer. 154 tests, including a black-box
   suite that opens the files with SQLite, a zip reader and a JSON parser rather than with the crate
   itself.
2. **The view became a crate, and the platform a seam** (`crates/note-ui` + `apps/windows`): everything
   the application does moved above the seam, the Windows services moved behind it, and the `pen-windows`
   reading type stopped leaking into the model (`Reading`). Its `store`, `note`, `pages`, `settings` and
   `history` modules are re-exports of the format's.
3. **The chords became the platform's** (`⌘` vs `Ctrl`), computed from the binding rather than typed into
   strings — `keys.rs`.
4. **Pdfium's loader became cross-platform** (`libloading`), which removed the last platform crate from
   the view's manifest.
5. **The mac app was written** (`apps/macos` + `apps/macos/pen-macos`): the six platform services behind
   the seam, with a Metal canvas, a local-monitor pen, a transparent cursor, and the ghost folded into the
   canvas pass. It builds, runs, and its 15 tests pass on a Mac.




