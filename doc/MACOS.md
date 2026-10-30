# cheap-note on macOS: where the port stands, and what the Mac has to do

This is the port's own document. It says what the plan is, what is **already true** and how that was
verified, and what can only be done on a Mac. If you have just cloned this repository on a Mac, read
[§2](#2-clone-and-build-on-the-mac-exact-commands) first and the rest after it; if you are the one
writing the mac app, start at [§4](#4-the-mac-app-crate-what-to-write-in-the-order-it-is-needed).

## 1. The plan, in one paragraph

The repository is three crates, and the two boundaries face opposite ways:

| Crate | What it is | Portable? |
|---|---|---|
| `crates/note-format` | the note on disk: SQLite schema, the ink's byte format, the page list, the settings, the zip | **yes**, no window/pen/GPU/PDF |
| `crates/note-ui` | everything the application *does*: the bar, the home screen, the sheet, the gestures, the PDF page cache, the ink model, the frame loop, the chords | **yes**, and it declares **no platform crate at all** |
| `apps/windows` | the platform: Direct2D canvas, `pen-windows` capture, the ghost cursor's layered window, the pointer hook, `main` | no, and it does not need to be |

The port is therefore **one new crate** (`apps/macos`) that implements four traits the view declares —
`PenSource`, `GhostFeed`, `SystemPointer`, `InkSurface` (`crates/note-ui/src/platform.rs`) — and
nothing above the seam. That is not an aspiration: §3 is the list of what has already been made true
to get here.

### Where we are

| Piece | Where | State | Verified by |
|---|---|---|---|
| the note's format | `crates/note-format` | **done** | 154 tests (78 unit + 76 black-box) |
| the view, and the seam | `crates/note-ui` | **done**, platform-free | 214 tests; dependency graph (§3) |
| the chords (`⌘`/`Ctrl`) | `crates/note-ui/src/keys.rs` | **done** | 5 tests; the Windows text is pinned, the mac text by `cfg(macos)` |
| Pdfium loading | `crates/note-ui/src/pdfium/api.rs` | **done** on every platform | 21 pdf tests on Windows, loading the real DLL through `libloading` |
| the Windows platform | `apps/windows` | unchanged | 29 tests |
| **the macOS platform** | `apps/macos` | **not started** — this document's work | — |

## 2. Clone and build on the Mac (exact commands)

```bash
xcode-select --install            # libsqlite3-sys compiles SQLite with the C compiler
git clone <this repository> cheap-note
cd cheap-note
rustup toolchain install 1.90     # Cargo.toml's rust-version

cargo test -p note-format         # 154 tests: the note itself
cargo test -p note-ui             # 214 tests: the view — and none of them needs a window
```

**Do not run `cargo build` or `cargo test --workspace` with no `-p` yet.** The workspace's third member
is `apps/windows`, which cannot build on a Mac. Until `apps/macos` exists, name the crates. That is a
workspace-shape snag rather than a port bug, and it is listed with the others in §8.

If everything is as it should be, **368 tests pass on a Mac with no platform crate written at all** —
154 + 214. That number *is* the seam's claim: the note and the view are the same code on every
machine, and only the four services are not.

One caution about that green run: the pdf tests that need a real Pdfium skip themselves when none is on
the machine (`crates/note-ui/src/pdfium/api.rs`'s `available()`), so 368 green is *not* proof that a page
renders. The next subsection puts the library in place, and §7 is where rendering is actually checked.

### Pdfium's `libpdfium.dylib`

`vendor/` in the repository holds the **Windows** build (`pdfium.dll`, 158.0.8125.0 — `vendor/README.md`
describes that archive), so a Mac needs its own copy before a PDF can be opened:

1. Take the `mac-arm64` archive from `pdfium-binaries` (Apple Silicon; `mac-x64` for Intel).
2. `libpdfium.dylib` goes to `vendor/lib/`; the headers in `include/` replace the ones there.
3. Nothing in the code changes: `LIBRARY_NAME` is already `libpdfium.dylib` on macOS, and the search
   finds `vendor/lib` by walking up from the executable — or from the working directory, which is how a
   `cargo test`/`cargo run` out of the repository finds it.

The build script has to put the dylib *next to the executable* for the app to find it when it is run from
elsewhere: `apps/windows/build.rs` does that for `*.dll`, and `apps/macos/build.rs` is the same file with
the filter changed (and the `.ico` step removed). A build with no library is not an error — the app
starts, and refuses to open a PDF while naming the file it looked for.

## 3. What is already true, and how it was checked

These are the things the Windows side could settle, and did:

* **The data lives where a Mac keeps it.** `note_format::note::root` answers
  `~/Library/Application Support/cheap-note/notes` on macOS (and `$XDG_DATA_HOME` on Linux), with the
  temp directory as the last resort. The note's three names — `note.db`, `source.pdf`,
  `attachments/` — are the same on every platform, which is the point of the format crate.
* **The view has no platform API in it.** `crates/note-ui/Cargo.toml` has no `[target.…]` section at
  all any more — no `windows`, no `objc2`, no `cfg` on a dependency. `cargo tree -p note-ui --target
  aarch64-apple-darwin` still prints `windows-sys`/`windows-link`, but those are reached only through a
  **proc-macro** graph (gpui-kit's `rust-embed`/`rust-i18n` and their `dirs`/`ignore`), which builds for
  the *host*; on a Mac they are not in the graph at all. Nothing the view depends on names a Windows API.
* **The toolkit has a mac backend.** `gpui-kit 0.7.3` → `gpui-pre-platform`, which selects
  `gpui-pre-macos` + `gpui-pre-apple` for macOS (both published, 0.3.x). Resolving the dependency graph
  for `--target aarch64-apple-darwin` from Windows picks them, so this is checked rather than hoped for.
* **The chords are the platform's.** Bindings are `secondary-z`/`secondary-shift-c`/… and handlers ask
  `Modifiers::secondary()`, which gpui resolves per target: `⌘` on macOS, `Ctrl` everywhere else.
  Labels are computed from the chord (`⌘Z`, `⇧⌘C`) instead of being typed into strings.
* **Pdfium is loaded the same way on all three.** `libloading` replaced the `LoadLibraryW` path;
  `LIBRARY_NAME` is `pdfium.dll` / `libpdfium.dylib` / `libpdfium.so`, and the search is the
  executable's directory → the four directories above it (`vendor/lib`, `vendor`) → the working
  directory. The handle is owned by `Api`, so the mapping is never released under a live document.

**What could not be checked from Windows:** the actual mac build. `cargo check -p note-format --target
aarch64-apple-darwin` on a Windows machine stops at `libsqlite3-sys` (no cross `cc`), so the first true
compile of the port is on the Mac. §2 is written to make that first compile uneventful.

## 4. The mac app crate: what to write, in the order it is needed

`apps/macos/Cargo.toml`:

```toml
[package]
name = "cheap-note"                 # the same binary name as the Windows app; only one is ever built
version = "0.1.0"
edition = "2021"
rust-version = "1.90"

[dependencies]
note-ui = { path = "../../crates/note-ui" }
note-format = { path = "../../crates/note-format" }
gpui-kit = "=0.7.3"
# plus whatever the four modules settle on for macOS (objc2 / cocoa / core-graphics …); those crates
# already arrive through gpui-pre-macos and gpui-pre-apple.
```

`apps/macos/src/main.rs` — and there is deliberately nothing else in it:

```rust
mod platform;

fn main() {
    note_ui::run(platform::INSTALLER);
}
```

(No `windows_subsystem` attribute and no icon resource: those are Windows' two lines of `main`.)

`apps/macos/src/platform/mod.rs` — the assembly, and **the order matters** (it is the order the Windows
one uses, and it explains why):

1. **the ghost** — so the pen's tap exists before the capture starts;
2. **the pen** — attached on the thread that owns the window;
3. **the pointer** — after the pen, so it has the last word: on the Mac that is a cursor rect plus a
   re-assert, not a window-procedure hook;
4. **the canvas** — the one thing the app requires: a machine that cannot give a canvas fails at
   startup, once, with the reason.

```rust
pub static INSTALLER: Installer = Installer { before_window, install };
```

### 4.1 `platform/input.rs` — NSEvent's tablet fields → `Reading` (write this first)

Nothing can be written without it. The view's reading is nine fields
(`crates/note-ui/src/input.rs`); the mapping is:

| `Reading` | Where it comes from in AppKit |
|---|---|
| `phase` | `.leftMouseDown` / `.leftMouseDragged` / `.leftMouseUp` with `subtype == .tabletPoint`; `Hover` from `.mouseMoved`; `Enter`/`Leave` from `.tabletProximity`; `Cancel` on `.mouseExited` or a deactivated window |
| `pixel` | `locationInWindow` (window base coordinates, origin **bottom**-left) → flip with the content view's height → × `window.backingScaleFactor`. `Reading::pixel` is **physical** client pixels, and the view divides by the scale itself |
| `pressure` | `NSEvent.pressure`, `0.0`–`1.0`. A device with no sensor (a mouse) must stay `None`, not `0.0` — the ink model treats "no sensor" and "no force" as different widths |
| `tilt` | `NSEvent.tilt` — the same two angles, in degrees; check the sign against `input.rs`'s `Tilt` docs (`x` positive right, `y` positive toward the user) |
| `inverted` / `eraser` | `pointingDeviceType == .eraser`; one field is enough on the Mac |
| `at_us` | `NSEvent.timestamp` (seconds since boot) × 1e6 |
| `delay_ms` | `0` — macOS does not stamp the digitizer, and the pump's own `waited` still measures the app's half |
| `id` | a constant per device, or the event's own device id where one is available |

Two things about the *shape* of the mac capture, and they are not the Windows shape:

* **Tablet events arrive on the main thread.** `pen-windows` needed a thread parked on a ring because
  `PenStream::read` blocks; AppKit does not, so the mac `PenSource` can push straight into
  `PenInbox` (its `push` is public for exactly this) and the view's pump wakes as it already does.
  A worker thread is optional, not a requirement.
* **Use a *local* event monitor** (`NSEvent.addLocalMonitorForEvents(matching:handler:)`), which sees
  this application's events only and needs no permission. A *global* monitor needs the Accessibility
  permission — and that is the trap `rdev` was rejected for (see `keys.rs`'s module docs).

### 4.2 `platform/pointer.rs` — `SystemPointer`

`NSCursor.hide()` / `NSCursor.unhide()`, driven by the boolean the view already computes (hide exactly
while the ghost is on screen). Re-assert after a window activation or a `.mouseEntered`: AppKit puts the
arrow back on its own, and a ghost plus an arrow is two cursors.

### 4.3 `platform/ghost.rs` — `GhostFeed`

`Screen` is the whole interface — scale, colour, halo, nib radius, sheet top, suppressed — and
`cursor.rs` (the lean geometry, `PenCursor`, `nib_radius_for_width`) is platform-free and can be reused
as-is. Two implementations, and the choice follows §5:

* **a borderless `NSWindow`/`NSPanel`** (`ignoresMouseEvents = true`, `isOpaque = false`,
  `backgroundColor = .clear`, a high `level`), drawn from the pen thread — the same shape as Windows,
  and the fastest;
* **in the frame** (a GPUI overlay), which needs the frame to paint it — see §5.

Start with a plain disc at the nib and let it grow into the pen's body later: the first version's job is
to prove the *feed* (batch in, screen published, alive), not the drawing.

One constraint shapes it: `GhostFeed` is `Send + Sync`, because the pen thread offers readings through
it. An `NSWindow` is neither, so — exactly as on Windows — the feed holds *handles* and a thread, and the
window itself lives on the thread that created it (AppKit wants a window made on the main thread;
`performSelectorOnMainThread:`/`dispatch_async` from the drawing thread is the usual way to keep that
promise).

### 4.4 `platform/canvas.rs` — `InkSurface` (the one big piece)

The description the view hands over is `note_ui::ink_layer::Canvas`: the fills (rect + `Hsla` + radius),
the page (`Arc<RenderImage>`, already a gpui image), and the ink (strokes, origin/zoom/paper/turns,
selection, lasso, highlight bands). Two ways to paint it:

| | How | Effort | What it costs later |
|---|---|---|---|
| **A. in the frame** | paint the same description with gpui's own primitives (paths, images) | days | the ink reaches the screen at the *frame* rate — a present per frame, not per pen batch |
| **B. `CAMetalLayer`** | port `apps/windows/src/platform/ink_layer/render.rs` (the Direct2D geometry builder) to Metal; the layer goes under gpui's view | weeks | none: the ink is presented per pen batch, as on Windows |

Whichever is chosen, `apps/windows`'s renderer is the reference for *what* is drawn, and
`note_ui::paint` is the call-back surface: `drawn_of_paper` (the one place a paper point becomes a
surface point), `alpha_of`, `with_alpha`, `HIGHLIGHTER_ALPHA`. The strokes carry their own points and
colours; the outline is built by the platform from `drawn_of_paper` and the stroke's width. The
description itself (`ink_layer/canvas.rs`) is the view's, and a mac canvas must not need to change it.

## 5. The one seam change the port may need

If the canvas is **A**, or the ghost paints in the frame, the frame has to be able to paint it — and the
traits were designed for a platform that draws *outside* the frame. The smallest honest addition is one
method with a default, so Windows is untouched:

```rust
pub trait InkSurface {
    fn draw(&mut self, canvas: &Canvas) -> Result<(), String>;

    /// Whether this surface paints inside the frame; when it does, the view includes it in `render`.
    ///
    /// `false` is the Windows answer: its canvas is a composition of its own *under* the window, and
    /// painting it from `render` as well would draw it twice.
    fn paints_in_frame(&self) -> bool {
        false
    }
}
```

and, in `NoteApp::render`, one element when the answer is `true`. `GhostFeed` gets the same treatment if
the ghost is in-frame. That is the only change above the seam the port should need — and it is worth
making on Windows first (with the default answering `false`, the Windows build is byte-identical), so
that the mac crate only *uses* it.

## 6. What must not change

* **The format.** A note written on the Mac must open on Windows, and the reverse — that is the whole
  reason `note-format` exists. Its 76 black-box tests are the contract.
* **`note-ui` stays free of platform APIs.** If a mac module wants something from the view, the answer
  is a trait method or `paint.rs`, never a `cfg(target_os)` inside `note-ui`. The check is that the view's
  manifest keeps no `[target.…]` section, and that no crate on its *own* dependency edges needs one.
* **The chords stay `secondary`** (`⌘` on the Mac, `Ctrl` on Windows), and their labels stay computed.
* **`Reading` conversions live in the platform crate** (`apps/windows/src/platform/input.rs` is the
  template). The view never learns what an `NSEvent` is.

## 7. Acceptance checklist for the Mac

- [ ] `cargo test -p note-format` — 154
- [ ] `cargo test -p note-ui` — 214 (no window needed)
- [ ] the app opens on the home list, and Escape returns to it
- [ ] a new note lands under `~/Library/Application Support/cheap-note/notes`, holding `note.db` — and
      `source.pdf` / `attachments/` once those exist — rather than in the repository's folder
- [ ] a PDF opens and renders (dylib in `vendor/lib`, and `build.rs` copied it)
- [ ] pressure and tilt change the line, and the line lands *under* the nib — the scale factor and the
      y-flip are the two places a port goes wrong
- [ ] the ghost follows the nib and leans; the system pointer is hidden exactly while it is on screen
- [ ] ink reaches the disk while writing; killing the app mid-stroke loses at most half a second
- [ ] *Save* writes a zip, and **that zip opens on Windows** with the same ink — and a Windows zip
      opens on the Mac
- [ ] zoom, pan, page turn, bookmark, outline, and the palette (`⌘⇧C`)

## 8. Known snags (found from the Windows side)

| Snag | What it is | The fix |
|---|---|---|
| `cargo build` / `--workspace` on the Mac | `apps/windows` is a workspace member and cannot build there | `-p note-ui -p note-format` until `apps/macos` exists; then `default-members` or a small script |
| bundled SQLite | `libsqlite3-sys` compiles C | `xcode-select --install` |
| `vendor/` | the repository's copy is the **Windows** build (`pdfium.dll`, 158.0.8125.0) | download the mac archive from `pdfium-binaries` (`mac-arm64`), `libpdfium.dylib` into `vendor/lib/` |
| cross-checking from Windows is impossible | `cargo check --target aarch64-apple-darwin` fails at the SQLite C compile (no cross `cc`) | the first real compile is the Mac's |
| `apps/windows/build.rs` | filters `.dll`, embeds a `.ico` | copy it, filter `.dylib`, drop the icon step |
| gpui's mac backend | `gpui-pre 0.3.9` publishes `gpui-pre-macos` + `gpui-pre-apple` | nothing: the resolution is already verified |
| the ghost's "outside the frame" window | the Windows one is a layered window; AppKit has no `UpdateLayeredWindow` | a borderless `NSPanel`, or the in-frame overlay (§4.3, §5) |

## 9. Where the pieces are

| To change… | Look at |
|---|---|
| the four traits, and `Platform`/`Installer` | `crates/note-ui/src/platform.rs` |
| one pen reading, and the queue it arrives through | `crates/note-ui/src/input.rs` |
| the chords, and how they are written | `crates/note-ui/src/keys.rs` |
| what a canvas may call back into | `crates/note-ui/src/paint.rs` |
| the canvas description | `crates/note-ui/src/ink_layer/canvas.rs` |
| a working implementation of all four | `apps/windows/src/platform/` — `pen.rs`, `cursor_overlay.rs`, `system_cursor.rs`, `ink_layer/` |
| Pdfium loading, and its search path | `crates/note-ui/src/pdfium/api.rs` |
| the note itself | `crates/note-format/`, and [`doc/STORE.md`](STORE.md) |

## 10. How we got here (the history, in order)

1. **The format became a crate** (`crates/note-format`): SQLite, the ink's byte format, the page list,
   the settings, the zip — no window, no pen, no GPU, no PDF renderer. 154 tests, including a black-box
   suite that opens the files with SQLite, a zip reader and a JSON parser rather than with the crate
   itself.
2. **The view became a crate, and the platform a seam** (`crates/note-ui` + `apps/windows`): everything
   the application does moved above the seam, the four Windows services moved behind it, and the
   `pen-windows` reading type stopped leaking into the model (`Reading`). Nothing inside the view was
   renamed: its `store`, `note`, `pages`, `settings` and `history` modules are re-exports.
3. **The chords became the platform's** (`⌘` vs `Ctrl`), computed from the binding rather than typed
   into strings — `keys.rs`.
4. **Pdfium's loader became cross-platform** (`libloading`), which is also the change that removed the
   last platform crate from the view's manifest.

The next step is §4 of this document, on a Mac.

