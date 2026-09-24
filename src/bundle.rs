//! A note as one file: the PDF it was written on, and the ink laid over it.
//!
//! ## What the file is
//!
//! A plain ZIP, with two entries:
//!
//! | Entry          | What it is                                                      |
//! | -------------- | --------------------------------------------------------------- |
//! | `document.pdf` | the PDF exactly as it was opened — copied, never rewritten      |
//! | `notes.json`   | the ink, one entry per page written on, and which page was open |
//!
//! Both entries are ordinary: any zip tool can list them, the JSON can be read in any editor, and
//! the PDF inside is a valid PDF. That is the point of choosing this shape. A container only the
//! app that wrote it can open is one the user cannot inspect, cannot repair, and cannot recover
//! their own writing from when the app has a bug — and this app is a prototype.
//!
//! ## Why the PDF is copied rather than annotated
//!
//! Writing the ink *into* the PDF would mean generating content streams, embedding fonts for any
//! text, and deciding what an annotation should do when someone else edits the file — a large
//! amount of work whose result is *worse*: in another viewer the ink would be burnt in, with no way
//! to turn it off, and the original page would be gone. Keeping the two apart is also what the
//! memory model already does: the page bitmap comes from Pdfium, and the ink is a separate layer of
//! strokes in sheet coordinates. This file is that arrangement, written down.
//!
//! ## Why the ink is per page
//!
//! A stroke belongs to the sheet it was drawn on, so `notes.json` holds a list of pages rather than
//! one pile of strokes, and a page that was never written on is simply absent.
//!
//! ## What is *not* in the file
//!
//! The toolbar's paper setting is the user's, not the note's — so if a note was written on a
//! different sheet than the one in use, opening it *says so* rather than quietly rescaling the ink.
//!
//! ## Checking that the file really is a zip
//!
//! The claim above is worth testing against an implementation that is not this one, and it is easy
//! to: the round-trip test keeps its file when `CHEAP_NOTE_KEEP_TEST_FILES` is set, and then any
//! zip tool can read it.
//!
//! ```text
//! $env:CHEAP_NOTE_KEEP_TEST_FILES = '1'; cargo test --offline a_note_round_trips
//! Expand-Archive -Path $env:TEMP\cheap-note-round-trip.zip -DestinationPath $env:TEMP\cn -Force
//! Get-ChildItem $env:TEMP\cn     # document.pdf, notes.json
//! ```
//!
//! That is how the format was verified: Windows' own zip implementation (through .NET) extracts
//! `document.pdf` and `notes.json` from a bundle this module wrote, and the JSON opens in an editor.

use std::io::{Read as _, Write as _};
use std::path::Path;

use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::{AppError, Result};
use crate::ink::{InkDocument, Stroke};
use crate::pages::Page;

/// The PDF inside a bundle.
const DOCUMENT_ENTRY: &str = "document.pdf";

/// The ink inside a bundle.
const NOTES_ENTRY: &str = "notes.json";

/// The version of `notes.json` this build writes.
///
/// Read back, a *newer* version is refused rather than guessed at: the fields it added are ones
/// this build does not know, and guessing is how a note gets silently rewritten without its ink.
///
/// * 1: the ink and the page that was open.
/// * 2: the page *list* as well — see [`crate::pages`] — which is what a note needs once pages can
///   be inserted and deleted, because a page index alone no longer says what that page shows. A 1
///   is read, not refused: its page indices were document pages, and [`Pages::restore`] is what
///   makes that true again.
const FORMAT: u32 = 2;

/// A note ready to be written, or one that has just been read.
#[derive(Debug)]
pub struct Bundle {
    /// The PDF the note was written on, if there is one.
    pub document: Option<Document>,
    /// The ink, one entry per page written on.
    pub pages: Vec<(usize, InkDocument)>,
    /// The page that was open.
    pub page: usize,
    /// The sheet the ink was written on, in logical pixels, when it is known.
    pub sheet: Option<(f32, f32)>,
    /// What each page shows, in reading order.
    ///
    /// Empty means *no list* rather than *no pages*: a note always has at least one page, so an
    /// empty list is the note format that predates the list, and reading it back means working the
    /// pages out from the document and the ink.
    pub layout: Vec<Page>,
}

/// A PDF held in memory, with the name it was saved under.
#[derive(Debug, Clone)]
pub struct Document {
    /// The file name to show for it.
    pub name: String,
    /// The bytes, exactly as they were read.
    pub bytes: Vec<u8>,
}

/// Writes a note to `path`, replacing whatever was there.
///
/// The notes are deflated and the document is *stored*: a PDF is compressed already, so deflating it
/// would spend milliseconds of the user's time to save a fraction of a percent of space.
pub fn write(path: &Path, bundle: &Bundle) -> Result<()> {
    let file = std::fs::File::create(path).map_err(|error| {
        AppError::Note(format!("{} could not be written: {error}", path.display()))
    })?;

    let mut zip = ZipWriter::new(std::io::BufWriter::new(file));

    let notes = NotesFile {
        format: FORMAT,
        page: bundle.page,
        sheet: bundle.sheet,
        document: bundle
            .document
            .as_ref()
            .map(|document| document.name.clone()),
        layout: bundle.layout.clone(),
        pages: bundle
            .pages
            .iter()
            .map(|(page, ink)| PageFile {
                page: *page,
                strokes: ink
                    .finished()
                    .iter()
                    .map(|stroke| (**stroke).clone())
                    .collect(),
            })
            .collect(),
    };

    let notes = serde_json::to_vec_pretty(&notes)
        .map_err(|error| AppError::Note(format!("the ink could not be encoded: {error}")))?;

    zip.start_file(
        NOTES_ENTRY,
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644),
    )
    .map_err(|error| AppError::Note(format!("the note could not be written: {error}")))?;
    zip.write_all(&notes)
        .map_err(|error| AppError::Note(format!("the note could not be written: {error}")))?;

    if let Some(document) = &bundle.document {
        zip.start_file(
            DOCUMENT_ENTRY,
            SimpleFileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .unix_permissions(0o644),
        )
        .map_err(|error| AppError::Note(format!("the document could not be written: {error}")))?;
        zip.write_all(&document.bytes).map_err(|error| {
            AppError::Note(format!("the document could not be written: {error}"))
        })?;
    }

    zip.finish().map_err(|error| {
        AppError::Note(format!("{} could not be closed: {error}", path.display()))
    })?;

    Ok(())
}

/// Reads a note from `path`.
///
/// A bundle with no `document.pdf` is a *note on a blank sheet* rather than a broken file: the app
/// can be used without a PDF open, and what the user writes then has to be saveable too.
pub fn read(path: &Path) -> Result<Bundle> {
    let file = std::fs::File::open(path)
        .map_err(|error| AppError::Note(format!("{} could not be read: {error}", path.display())))?;

    let mut zip = ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| {
        AppError::Note(format!(
            "{} is not a zip file this app can read: {error}",
            path.display()
        ))
    })?;

    let notes: NotesFile = {
        let mut entry = zip.by_name(NOTES_ENTRY).map_err(|_| {
            AppError::Note(format!(
                "{} is a zip file, but it holds no {NOTES_ENTRY}: it was not written by this app",
                path.display()
            ))
        })?;

        serde_json::from_reader(&mut entry).map_err(|error| {
            AppError::Note(format!("the ink in {} is not valid: {error}", path.display()))
        })?
    };

    if notes.format > FORMAT {
        return Err(AppError::Note(format!(
            "{} was written by a newer version of this app (note format {}, this build writes {FORMAT})",
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

            Some(Document {
                // The name the document had when it was saved, so `chapter-3.pdf` saved as
                // `notes.zip` still comes back as `chapter-3.pdf`. The zip's own name is the
                // fallback, for a bundle whose notes were written by hand.
                name: notes
                    .document
                    .clone()
                    .or_else(|| {
                        path.file_stem()
                            .map(|stem| format!("{}.pdf", stem.to_string_lossy()))
                    })
                    .unwrap_or_else(|| String::from("document.pdf")),
                bytes,
            })
        }
        Err(_) => None,
    };

    Ok(Bundle {
        document,
        pages: notes
            .pages
            .into_iter()
            .map(|page| (page.page, InkDocument::from_strokes(page.strokes)))
            .collect(),
        page: notes.page,
        sheet: notes.sheet,
        layout: notes.layout,
    })
}

/// `notes.json`, as it is written.
#[derive(Debug, Serialize, Deserialize)]
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
    /// What each page shows, in reading order. Absent in notes written before the list existed.
    #[serde(default)]
    layout: Vec<Page>,
    /// The pages that hold ink.
    #[serde(default)]
    pages: Vec<PageFile>,
}

/// One page of `notes.json`.
#[derive(Debug, Serialize, Deserialize)]
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
    use crate::ink::InkPoint;

    /// A page holding `count` one-point strokes, spaced out so they are distinguishable.
    fn page_of(count: usize) -> InkDocument {
        let strokes = (0..count)
            .map(|index| {
                let x = index as f32 * 10.0;
                Stroke::new(InkPoint::new(x, x, 3.0), Stroke::DEFAULT_COLOR)
            })
            .collect();

        InkDocument::from_strokes(strokes)
    }

    /// A path in the temp directory for a test to write to, cleared before use.
    fn temporary_path(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("cheap-note-{name}.zip"));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// The whole round trip: ink in, ink out, on the pages it was written on.
    #[test]
    fn a_note_round_trips_through_a_file() {
        let path = temporary_path("round-trip");

        let bundle = Bundle {
            document: Some(Document {
                name: String::from("page.pdf"),
                bytes: b"%PDF-1.4 not really".to_vec(),
            }),
            pages: vec![(0, page_of(3)), (4, page_of(2))],
            page: 4,
            sheet: Some((794.0, 1123.0)),
            layout: vec![Page::Document(0), Page::Blank, Page::Document(1), Page::Blank],
        };

        write(&path, &bundle).expect("the note is written");
        let read_back = read(&path).expect("the note is read");

        assert_eq!(read_back.page, 4, "the page that was open");
        assert_eq!(read_back.sheet, Some((794.0, 1123.0)));
        assert_eq!(
            read_back.layout,
            vec![Page::Document(0), Page::Blank, Page::Document(1), Page::Blank],
            "the page list comes back in reading order, inserted pages and all"
        );
        assert_eq!(read_back.pages.len(), 2, "two pages were written on");
        assert_eq!(read_back.pages[0].0, 0);
        assert_eq!(read_back.pages[0].1.stroke_count(), 3);
        assert_eq!(read_back.pages[1].0, 4);
        assert_eq!(read_back.pages[1].1.stroke_count(), 2);

        let document = read_back.document.expect("the document came back");
        assert_eq!(
            document.name, "page.pdf",
            "the name the document was saved under comes back with it"
        );
        assert_eq!(document.bytes, b"%PDF-1.4 not really");

        // Kept on disk so the shell check can read it with a *different* zip implementation: see
        // the interop note in the module docs.
        if std::env::var_os("CHEAP_NOTE_KEEP_TEST_FILES").is_none() {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// A note written before the page list existed still reads.
    ///
    /// Its layout comes back empty, which is how the reader knows to work the pages out from the
    /// document rather than to draw a note with no pages at all.
    #[test]
    fn a_note_from_before_the_page_list_still_reads() {
        let old = r#"{"format":1,"page":1,"pages":[{"page":1,"strokes":[]}]}"#;
        let notes: NotesFile = serde_json::from_str(old).expect("a version 1 note parses");

        assert_eq!(notes.format, 1);
        assert!(notes.layout.is_empty(), "there was no list to read");
        assert_eq!(notes.page, 1);
    }

    /// A loaded stroke can be erased: its caches are rebuilt on the way in.
    ///
    /// Bounds and outlines are `#[serde(skip)]` because they derive from the points — and they are
    /// exactly what the eraser hit-tests against, so a loaded note that skipped rebuilding them
    /// would draw correctly and refuse to be erased.
    #[test]
    fn a_loaded_stroke_can_be_erased() {
        let path = temporary_path("erase");

        let bundle = Bundle {
            document: None,
            pages: vec![(0, page_of(1))],
            page: 0,
            layout: vec![Page::Blank],
            sheet: Some((794.0, 1123.0)),
        };

        write(&path, &bundle).expect("the note is written");
        let mut read_back = read(&path).expect("the note is read");
        let page = &mut read_back.pages[0].1;

        assert!(
            page.finished()[0].hits(0.0, 0.0, 4.0),
            "the stroke knows where it is"
        );
        assert!(page.undo(), "and it can be taken back");
        assert_eq!(page.stroke_count(), 0);

        let _ = std::fs::remove_file(&path);
    }

    /// A note written without a PDF is a valid file, not a broken one.
    #[test]
    fn a_note_without_a_document_is_a_note_on_a_blank_sheet() {
        let path = temporary_path("no-document");

        write(
            &path,
            &Bundle {
                document: None,
                pages: vec![(0, page_of(1))],
                page: 0,
                sheet: Some((595.0, 842.0)),
                layout: vec![Page::Blank],
            },
        )
        .expect("the note is written");

        let read_back = read(&path).expect("the note is read");
        assert!(read_back.document.is_none());
        assert_eq!(read_back.pages.len(), 1);

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
        let message = error.to_string();
        assert!(
            message.contains(NOTES_ENTRY),
            "the error names what was missing: {message}"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A note from a newer build is refused rather than silently reinterpreted.
    #[test]
    fn a_newer_note_format_is_refused() {
        let path = temporary_path("newer");

        let file = std::fs::File::create(&path).expect("a file");
        let mut zip = ZipWriter::new(file);
        let notes = serde_json::json!({
            "format": FORMAT + 1,
            "page": 0,
            "pages": [],
        })
        .to_string();

        zip.start_file(NOTES_ENTRY, SimpleFileOptions::default())
            .expect("an entry");
        zip.write_all(notes.as_bytes()).expect("its contents");
        zip.finish().expect("a finished zip");

        let error = read(&path).expect_err("a newer format is refused");
        assert!(
            error.to_string().contains("newer version"),
            "the error explains itself: {error}"
        );

        let _ = std::fs::remove_file(&path);
    }
}
