# cheap-note

A minimal, high-performance handwriting note-taking prototype for **Windows and macOS**: write with a
pen on a PDF page, and the ink is on the disk as you write it.

```text
cargo run -p cheap-note --release         # Windows; add `-- <file.pdf>` to open that file's note
cargo run -p cheap-note-macos --release   # macOS;   the same binary name, `cheap-note`
```

The two platform apps build to the same binary name (`cheap-note`), but they are separate workspace
members: run the one your machine is. `cargo test --workspace` builds and tests everything on **either**
platform — the app for the other platform compiles to a stub and runs no tests, so the same command works
on Windows and on a Mac. (See [doc/MACOS.md](doc/MACOS.md) §2 for the Mac commands.)

Pdfium is not in the repository: it is downloaded into `vendor/` (see `vendor/README.md`), and a build
copies it next to the executable. A fresh clone starts, draws and writes ink without it; only opening a
PDF needs it.

A release build is the window and nothing else — no console beside it — because the Windows executable
is linked as a *windows* subsystem program (see the crate attribute in `apps/windows/src/main.rs`): started
by double-click, by a shortcut, or through a file association, it opens without the black window a
console-subsystem program gets. On a Mac the same release build is wrapped in a `.app` by
`apps/macos/bundle.sh`. Debug builds and `cargo test` keep the console, where the diagnostics a run can
print and the measurements the timing tests take are readable.

## Four workspace members

The repository is a workspace, and the splits are the point of it:

| Crate | What it is |
|---|---|
| [`crates/note-format`](crates/note-format) | the note itself: SQLite schema, the ink's byte format, the page list, the settings, the zip a note is carried in. **No window, no pen, no GPU, no PDF renderer.** |
| [`crates/note-ui`](crates/note-ui) | the view: the bar, the home screen, the sheet, the gestures, the PDF page cache, the ink model, the frame loop. **No platform's own API** — it asks for what it needs through the traits in `platform.rs`. |
| [`apps/windows`](apps/windows) | the Windows platform: the Direct2D canvas, the pen capture, the ghost cursor's window, the pointer hook, Pdfium's entry points, and `main`. Nothing here is portable, and nothing here needs to be. |
| [`apps/macos`](apps/macos) | the macOS platform: the Metal canvas, `pen-macos` (the digitizer read out of `NSEvent`), the ghost drawn in the canvas pass, the transparent-cursor pointer, the Core Graphics mouse, and `main`. The `apps/windows` platform rewritten behind the same seam. |

Each boundary is a question the compiler can ask, and they face opposite ways: the view depends on the
format, and each platform depends on the view — never the other way round. That is what made a second
application possible without a second application's worth of code: the macOS build is [`apps/windows`](apps/windows)
rewritten ([`apps/macos`](apps/macos) — Metal instead of Direct2D, `NSEvent` instead of `pen-windows`,
the ghost drawn in the canvas pass instead of in a window of its own, `libpdfium.dylib` behind `dlopen`),
and everything in `note-ui` above the seam stayed as it was.

The view's `store.rs`, `note.rs`, `pages.rs`, `settings.rs` and `history.rs` are re-exports of their
`note-format` counterparts, so a path inside the view still reads `crate::store::NoteStore` — the
split moved the code without renaming anything.

The format has **two test suites**, and both matter:
[`crates/note-format/src`](crates/note-format/src) holds the unit tests, which may look inside a module;
[`crates/note-format/tests`](crates/note-format/tests) holds one black-box file per module (`store.rs`,
`note.rs`, `chunk.rs`, …) that uses nothing but the public API and opens a note with SQLite, a zip reader
or a JSON parser rather than with the crate. Those are the contract a second implementation on another
platform has to satisfy — and the macOS build is the proof they can be met: the two portable crates hold
**435 tests** (171 in `note-format`, 264 in `note-ui`) that pass unchanged on either platform, and each
platform adds its own (36 in `apps/macos`, 5 in `pen-macos`, 31 in `apps/windows`).

## What is where

| Document | Answers |
|---|---|
| [`doc/ARCHITECTURE.md`](doc/ARCHITECTURE.md) | the shape of the program: what runs where, which thread owns what, and what may wait for what |
| [`doc/VIEW.md`](doc/VIEW.md) | why the view is the way it is: the frame loop, the bar, the sheet, the gestures, the pen's rules |
| [`doc/STATUS.md`](doc/STATUS.md) | how to read the status line: what each clause means, and which figure to look at when something feels wrong |
| [`doc/STORE.md`](doc/STORE.md) | one stroke's journey to the disk and back — which is the format crate's story |
| [`doc/MACOS.md`](doc/MACOS.md) | the macOS build: what the seam gives you, how each platform service is implemented, and what only a Mac can do |
| [`doc/WINDOWS.md`](doc/WINDOWS.md) | the Windows build's counterpart: which tuning reached it, which platform behaviours still differ (and silently break a gesture), and what is worth optimising here only |

Every module in the `src/` trees opens with the same kind of short account of
itself — the *why* at the point of use — and those are the place to look before changing something
inside one file.
