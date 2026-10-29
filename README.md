# cheap-note

A minimal, high-performance handwriting note-taking prototype for Windows: write with a pen on a PDF
page, and the ink is on the disk as you write it.

```text
cargo run --release        # the app; `cargo run --release -- <file.pdf>` opens that file's note
cargo test --workspace     # 390 tests: the app's, the format's, and the format's black-box suite
```

Pdfium is not in the repository: it is downloaded into `vendor/` (see `vendor/README.md`), and a build
copies it next to the executable. A fresh clone starts, draws and writes ink without it; only opening a
PDF needs it.

A release build is the window and nothing else — no console beside it — because the executable is linked
as a *windows* subsystem program (see the crate attribute in `src/main.rs`): started by double-click, by
a shortcut, or through a file association, it opens without the black window a console-subsystem program
gets. Debug builds and `cargo test` keep the console, where the diagnostics a run can print and the
measurements the timing tests take are readable.

## Two crates

The repository is a workspace, and the split is the point of it:

| Crate | What it is |
|---|---|
| `cheap-note` (the root) | the Windows app: the Direct2D canvas, the pen capture, Pdfium, the view. Nothing here is portable, and nothing here needs to be. |
| [`crates/note-format`](crates/note-format) | the note itself: SQLite schema, the ink's byte format, the page list, the settings, the zip a note is carried in. **No window, no pen, no GPU, no PDF renderer.** |

That is what makes a second application possible. The format is a crate rather than a set of modules
because a crate boundary is the only kind the compiler enforces: `note-format` cannot reach into the
app even by accident, so a build on another platform gets the whole format and none of Windows. The
app depends on the format; the format depends on nothing of the app's.

The app's `src/store.rs`, `src/note.rs`, `src/pages.rs`, `src/settings.rs` and `src/history.rs` are
re-exports of their `note-format` counterparts, so every path inside the app still reads
`crate::store::NoteStore` — the split moved the code without renaming anything.

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

Every module in `src/` and `crates/note-format/src/` opens with the same kind of short account of
itself — the *why* at the point of use — and those are the place to look before changing something
inside one file.
