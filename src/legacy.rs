//! The note format this app used before SQLite: a zip with `notes.json` in it.
//!
//! ## Why this module exists, and why it only reads
//!
//! A note used to be one zip holding the original PDF and a `notes.json` that listed every page and
//! every point of every stroke on it. `BUNDLE.md` is why that was replaced — the whole note was
//! re-serialised on every save, and JSON spends more bytes on the punctuation of a number than on
//! the number — and this module is what is left of it: the reader, kept so that a note written by
//! the old build can be *migrated* rather than lost.
//!
//! There is no writer here. A note this build writes is a folder with a SQLite file in it (see
//! [`crate::note`]), and the file a person carries between machines is a zip of that folder — so
//! writing the old shape would be writing a format nothing in this build reads.
//! [`crate::note::Note::placed_from`] reads through here exactly once, when it meets a zip that has
//! no `note.db`, and turns it into a store.
//!
//! ## What the old file held
//!
//! | Entry          | What it is                                                       |
//! | -------------- | ---------------------------------------------------------------- |
//! | `document.pdf` | the PDF exactly as it was opened — copied, never rewritten       |
//! | `notes.json`   | the ink, one entry per page written on, and which page was open  |
//!
//! ## Checking that the file really is what it says
//!
//! Read as before, with the same two refusals: a zip with no `notes.json` is named as a zip this app
//! did not write, and a `format` newer than this build knows is refused rather than guessed at. The
//! tests build the old shape by hand — the writer that made it is gone — which is the right way
//! round for a migration path: what is being read is a file from the past, not one this code made.

use std::io::Read as _;
use std::path::Path;

use serde::Deserialize;
use zip::ZipArchive;

use crate::error::{AppError, Result};
use crate::ink::Stroke;
use crate::pages::Page;

/// The PDF inside an old note.
const DOCUMENT_ENTRY: &str = "document.pdf";

/// The ink inside an old note.
const NOTES_ENTRY: &str = "notes.json";

/// The newest `notes.json` this build understands.
///
/// * 1: the ink and the page that was open.
/// * 2: the page *list* as well — see [`crate::pages`] — which is what a note needs once a page can
///   be inserted or deleted.
const FORMAT: u32 = 2;

/// A note as the old format held it.
#[derive(Debug)]
pub struct Note {
    /// The PDF the note was written on, if there was one, with the name it was saved under.
    pub document: Option<(String, Vec<u8>)>,
    /// The ink, one entry per page written on, in the order the pages are read.
    pub pages: Vec<(usize, Vec<Stroke>)>,
    /// The page that was open.
    pub page: usize,
    /// The sheet the ink was written on, in logical pixels, when it was recorded.
    pub sheet: Option<(f32, f32)>,
    /// What each page showed, in reading order. Empty means the note predates the list.
    pub layout: Vec<Page>,
}

/// Reads an old note from `path`.
///
/// A zip with no `document.pdf` was a note written on a blank sheet, which is a note and not a
/// broken file: the app could be used with nothing open, and what was written then has to be
/// migratable too.
pub fn read(path: &Path) -> Result<Note> {
    let file = std::fs::File::open(path).map_err(|error| {
        AppError::Note(format!("{} could not be read: {error}", path.display()))
    })?;

    let mut zip = ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| {
        AppError::Note(format!(
            "{} is not a zip file this app can read: {error}",
            path.display()
        ))
    })?;

    let notes: NotesFile = {
        let mut entry = zip.by_name(NOTES_ENTRY).map_err(|_| {
            AppError::Note(format!(
                "{} is a zip file, but it holds no {NOTES_ENTRY} and no note database: it was not written by this app",
                path.display()
            ))
        })?;

        serde_json::from_reader(&mut entry).map_err(|error| {
            AppError::Note(format!("the ink in {} is not valid: {error}", path.display()))
        })?
    };

    if notes.format > FORMAT {
        return Err(AppError::Note(format!(
            "{} was written by a newer version of this app (note format {}, this build reads {FORMAT})",
            path.display(),
            notes.format
        )));
    }

    let document = match zip.by_name(DOCUMENT_ENTRY) {
        Ok(mut entry) => {
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes).map_err(|error| {
                AppError::Note(format!(
                    "the document in {} could not be read: {error}",
                    path.display()
                ))
            })?;

            Some((
                // The name the document had when it was saved, so `chapter-3.pdf` saved as
                // `notes.zip` still comes back as `chapter-3.pdf`. The zip's own name is the
                // fallback, for a note whose JSON was written by hand.
                notes
                    .document
                    .clone()
                    .or_else(|| {
                        path.file_stem()
                            .map(|stem| format!("{}.pdf", stem.to_string_lossy()))
                    })
                    .unwrap_or_else(|| String::from("document.pdf")),
                bytes,
            ))
        }
        Err(_) => None,
    };

    Ok(Note {
        document,
        pages: notes
            .pages
            .into_iter()
            .map(|mut page| {
                // The JSON never carried the bounds or the outline — they are derivable from the
                // points, so they were `#[serde(skip)]` — and a stroke without them draws and refuses
                // to be erased. This is the one place a migrated stroke gets them back.
                for stroke in &mut page.strokes {
                    stroke.close();
                }

                (page.page, page.strokes)
            })
            .collect(),
        page: notes.page,
        sheet: notes.sheet,
        layout: notes.layout,
    })
}

/// `notes.json`, as it was written.
#[derive(Debug, Deserialize)]
struct NotesFile {
    /// The note format version.
    format: u32,
    /// The page that was open.
    #[serde(default)]
    page: usize,
    /// The sheet the ink was written on, in logical pixels.
    #[serde(default)]
    sheet: Option<(f32, f32)>,
    /// The file name the document had when the note was saved.
    #[serde(default)]
    document: Option<String>,
    /// What each page showed, in reading order. Absent in notes written before the list existed.
    #[serde(default)]
    layout: Vec<Page>,
    /// The pages that hold ink.
    #[serde(default)]
    pages: Vec<PageFile>,
}

/// One page of `notes.json`.
#[derive(Debug, Deserialize)]
struct PageFile {
    /// The page index.
    page: usize,
    /// Its strokes, in the order they were drawn.
    #[serde(default)]
    strokes: Vec<Stroke>,
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::PathBuf;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    /// A path in the temp directory for a test to write to, cleared before use.
    fn temporary_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("cheap-note-legacy-{name}.zip"));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Writes the old shape by hand: the way a test should build a file it is migrating *from*.
    fn write_old_note(path: &Path, notes: &str, document: Option<&[u8]>) {
        let file = std::fs::File::create(path).expect("a file");
        let mut zip = ZipWriter::new(file);

        zip.start_file(
            NOTES_ENTRY,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .expect("an entry");
        zip.write_all(notes.as_bytes()).expect("the ink");

        if let Some(bytes) = document {
            zip.start_file(
                DOCUMENT_ENTRY,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .expect("an entry");
            zip.write_all(bytes).expect("the document");
        }

        zip.finish().expect("a finished zip");
    }

    /// An old note is read: its pages, its document, and the page that was open.
    #[test]
    fn an_old_note_is_read_for_migration() {
        let path = temporary_path("read");
        let notes = serde_json::json!({
            "format": 2,
            "page": 1,
            "sheet": [794.0, 1123.0],
            "document": "chapter-3.pdf",
            "layout": [{"document": 0}, "blank"],
            "pages": [
                {
                    "page": 0,
                    "strokes": [
                        {
                            "points": [
                                {"x": 1.0, "y": 2.0, "width": 3.0},
                                {"x": 4.0, "y": 5.0, "width": 6.0}
                            ],
                            "color": 1776415
                        }
                    ]
                }
            ]
        })
        .to_string();

        write_old_note(&path, &notes, Some(b"%PDF-1.4 not really"));

        let note = read(&path).expect("an old note is read");
        assert_eq!(note.page, 1);
        assert_eq!(note.sheet, Some((794.0, 1123.0)));
        assert_eq!(note.layout, vec![Page::Document(0), Page::Blank]);
        assert_eq!(note.pages.len(), 1);
        assert_eq!(note.pages[0].0, 0);
        assert_eq!(note.pages[0].1.len(), 1);
        assert_eq!(note.pages[0].1[0].points.len(), 2);
        assert_eq!(
            note.pages[0].1[0].bounds,
            [1.0, 2.0, 4.0, 5.0],
            "and the stroke knows where it is, which is what the eraser needs"
        );

        let (name, bytes) = note.document.expect("the document came back");
        assert_eq!(name, "chapter-3.pdf");
        assert_eq!(bytes, b"%PDF-1.4 not really");

        let _ = std::fs::remove_file(&path);
    }

    /// A note written before the page list existed still reads, listless.
    #[test]
    fn a_note_from_before_the_page_list_reads() {
        let path = temporary_path("format-1");
        write_old_note(&path, r#"{"format":1,"page":0,"pages":[]}"#, None);

        let note = read(&path).expect("the oldest note reads");
        assert_eq!(note.page, 0);
        assert!(note.layout.is_empty(), "there is no list to have");
        assert!(note.document.is_none(), "and no document");

        let _ = std::fs::remove_file(&path);
    }

    /// A zip that is not a note is refused by name, rather than half-read.
    #[test]
    fn a_foreign_zip_is_refused() {
        let path = temporary_path("foreign");

        let file = std::fs::File::create(&path).expect("a file");
        let mut zip = ZipWriter::new(file);
        zip.start_file("something-else.txt", SimpleFileOptions::default())
            .expect("an entry");
        zip.write_all(b"hello").expect("its contents");
        zip.finish().expect("a finished zip");

        let error = read(&path).expect_err("a foreign zip is refused");
        assert!(error.to_string().contains(NOTES_ENTRY), "{error}");

        let _ = std::fs::remove_file(&path);
    }

    /// A note from a newer build is refused rather than silently reinterpreted.
    #[test]
    fn a_newer_note_format_is_refused() {
        let path = temporary_path("newer");
        let notes = serde_json::json!({
            "format": FORMAT + 1,
            "page": 0,
            "pages": [],
        })
        .to_string();
        write_old_note(&path, &notes, None);

        let error = read(&path).expect_err("a newer format is refused");
        assert!(error.to_string().contains("newer version"), "{error}");

        let _ = std::fs::remove_file(&path);
    }
}
