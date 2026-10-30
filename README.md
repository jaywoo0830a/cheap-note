# cheap-note

A minimal, high-performance handwriting note-taking prototype for Windows: write with a pen on a PDF
page, and the ink is on the disk as you write it.

```text
cargo run -p cheap-note --release        # the app; add `-- <file.pdf>` to open that file's note
cargo test --workspace     # 397 tests: the app's, the format's, and the format's black-box suite
```

Pdfium is not in the repository: it is downloaded into `vendor/` (see `vendor/README.md`), and a build
copies it next to the executable. A fresh clone starts, draws and writes ink without it; only opening a
PDF needs it.

A release build is the window and nothing else — no console beside it — because the executable is linked
as a *windows* subsystem program (see the crate attribute in `apps/windows/src/main.rs`): started by double-click, by
a shortcut, or through a file association, it opens without the black window a console-subsystem program
gets. Debug builds and `cargo test` keep the console, where the diagnostics a run can print and the
measurements the timing tests take are readable.

## Three crates

The repository is a workspace, and the splits are the point of it:

| Crate | What it is |
|---|---|
| [`crates/note-format`](crates/note-format) | the note itself: SQLite schema, the ink's byte format, the page list, the settings, the zip a note is carried in. **No window, no pen, no GPU, no PDF renderer.** |
| [`crates/note-ui`](crates/note-ui) | the view: the bar, the home screen, the sheet, the gestures, the PDF page cache, the ink model, the frame loop. **No platform's own API** — it asks for what it needs through the traits in `platform.rs`. |
| [`apps/windows`](apps/windows) | the platform: the Direct2D canvas, the pen capture, the ghost cursor's window, the pointer hook, Pdfium's entry points, and `main`. Nothing here is portable, and nothing here needs to be. |

Each boundary is a question the compiler can ask, and they face opposite ways: the view depends on the
format, and the platform depends on the view — never the other way round. That is what makes a second
application possible without a second application's worth of code: a macOS build is
[`apps/windows`](apps/windows) rewritten (Metal or CoreGraphics instead of Direct2D, `NSEvent` instead
of `pen-windows`, the ghost drawn in the frame instead of in a window of its own, `libpdfium.dylib`
behind `dlopen`), and everything in `note-ui` above the seam stays as it is.

The view's `store.rs`, `note.rs`, `pages.rs`, `settings.rs` and `history.rs` are re-exports of their
`note-format` counterparts, so a path inside the view still reads `crate::store::NoteStore` — the
split moved the code without renaming anything.

The format has **two test suites**, and both matter:
[`crates/note-format/src`](crates/note-format/src) holds the unit tests, which may look inside a module;
[`crates/note-format/tests`](crates/note-format/tests) holds one black-box file per module (`store.rs`,
`note.rs`, `chunk.rs`, …) that uses nothing but the public API and opens a note with SQLite, a zip reader
or a JSON parser rather than with the crate. Those are the contract a second implementation on another
platform has to satisfy.

## What is where

| Document | Answers |
|---|---|
| [`doc/ARCHITECTURE.md`](doc/ARCHITECTURE.md) | the shape of the program: what runs where, which thread owns what, and what may wait for what |
| [`doc/VIEW.md`](doc/VIEW.md) | why the view is the way it is: the frame loop, the bar, the sheet, the gestures, the pen's rules |
| [`doc/STORE.md`](doc/STORE.md) | one stroke's journey to the disk and back — which is the format crate's story |
| [`doc/MACOS.md`](doc/MACOS.md) | the macOS port: what the seam already gives you, and what only a Mac can do |

Every module in the three `src/` trees opens with the same kind of short account of
itself — the *why* at the point of use — and those are the place to look before changing something
inside one file.
