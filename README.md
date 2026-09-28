# cheap-note

A minimal, high-performance handwriting note-taking prototype for Windows: write with a pen on a PDF
page, and the ink is on the disk as you write it.

```text
cargo run --release        # the app; `cargo run --release -- <file.pdf>` opens that file's note
cargo test                 # 231 tests, some of which install a real canvas on a real window
```

Pdfium is not in the repository: it is downloaded into `vendor/` (see `vendor/README.md`), and a build
copies it next to the executable. A fresh clone starts, draws and writes ink without it; only opening a
PDF needs it.

## What is where

| Document | Answers |
|---|---|
| [`doc/ARCHITECTURE.md`](doc/ARCHITECTURE.md) | the shape of the program: what runs where, which thread owns what, and what may wait for what |
| [`doc/VIEW.md`](doc/VIEW.md) | why the view is the way it is: the frame loop, the bar, the sheet, the gestures, the pen's rules |
| [`doc/STORE.md`](doc/STORE.md) | one stroke's journey to the disk and back |

Every module in `src/` opens with the same kind of short account of itself — the *why* at the point of
use — and those are the place to look before changing something inside one file.
