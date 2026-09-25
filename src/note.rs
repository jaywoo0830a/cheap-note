//! A note as a *folder*, and the single file it is carried in.
//!
//! ## The two shapes of a note
//!
//! ```text
//! <notes root>/chapter-3-9f2a1c44/       the note, where it is written
//!     note.db                            the store: ink, page list, and what the note is
//!     source.pdf                         the document, byte for byte
//!     attachments/                       anything else the note carries
//!
//! chapter-3.zip                          the note, where it is *carried*
//!     note.db                            a VACUUM INTO copy: complete, standalone
//!     source.pdf
//!     attachments/
//! ```
//!
//! The folder is what the app works in. A save is a batch of ink into `note.db` (see
//! [`crate::store`]), which is why the pen never waits for a file to be rewritten, and why a crash
//! costs at most the last half-second of ink rather than the note. The zip is what moves: it holds a
//! *vacuumed* copy of the database — the WAL folded in, nothing half-written — plus the document, so
//! a note arrives on another machine complete.
//!
//! ## Why the working copy is not where the user's files are
//!
//! The design note is explicit about it: an open SQLite file must not live in a synchronising folder
//! (Dropbox, iCloud, OneDrive), because those tools copy and merge files under an open handle and
//! corrupt them. So a note is *placed* — imported into the app's own directory — and moved between
//! machines by exporting a file. That is also why [`Note::placed_from`] takes a destination: an
//! import is a copy, and the original is never opened for writing.
//!
//! ## Writing, on another thread
//!
//! [`NoteWriter`] owns its own connection and runs in a thread of its own: the pen's path only ever
//! *sends* a batch, and WAL means the reader — the app loading the page it is about to show — is
//! never blocked by the writer. This is the design note's "one connection per thread" with a job
//! queue in front of the writer, and it is what makes a 500 ms batch commit invisible while writing.
//!
//! Errors do not vanish on that thread: they come back through [`NoteWriter::drain`], which the app
//! empties on every frame tick and shows in its status line. A writer that failed silently would be
//! the worst of both worlds — ink that is not being saved, and an app that looks like it is.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::{AppError, Result};
use crate::ink::Stroke;
use crate::store::NoteStore;

/// The database inside a note folder.
pub const NOTE_DB: &str = "note.db";

/// The document inside a note folder, whatever it was called when it arrived.
pub const NOTE_PDF: &str = "source.pdf";

/// The folder inside a note folder that holds anything else the note carries.
pub const ATTACHMENTS: &str = "attachments";

/// The file an export is vacuumed into before it is packed.
///
/// Inside the note folder, so the vacuum-and-pack happens on one volume: a copy across volumes is
/// two writes of the whole note, and the temporary file is deleted either way.
const EXPORT_TMP: &str = "export.tmp.db";

/// Where the notes this app has been given live.
///
/// Under the user's local application data rather than beside the app or beside their files: a note
/// is data, and the design note's rule is that an open database must not sit in a folder something
/// else is synchronising. `LOCALAPPDATA` is the Windows answer to that; anything else falls back to
/// the temp directory, which is at least local.
pub fn root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("cheap-note")
        .join("notes")
}

/// The folder a note from `source` is placed in.
///
/// The name is the file's own stem plus a short digest of the *whole* path, so that
/// `/a/chapter-3.pdf` and `/b/chapter-3.pdf` are two notes rather than one that overwrites the
/// other, and so that importing the same file twice lands in the same place — the second import
/// updates the working copy a person is already used to.
pub fn folder_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| String::from("note"));

    let cleaned: String = stem
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();

    // FNV-1a over the path as the user gave it, written as eight hex digits. Not a hash for
    // security, only for a name that is stable and short.
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in source.to_string_lossy().bytes() {
        digest ^= u64::from(byte);
        digest = digest.wrapping_mul(0x100_0000_01b3);
    }

    format!("{}-{:08x}", cleaned.trim_matches('-'), digest as u32)
}

/// A note: a folder, and the store inside it.
#[derive(Debug)]
pub struct Note {
    /// The folder the note lives in.
    dir: PathBuf,
    /// The app's own connection to the note.
    ///
    /// Pages are read through it, and so is the little the app itself records about the note — which
    /// page is open, the page list, the paper size. The *ink* is written by [`NoteWriter`], on a
    /// connection of its own: two connections to one file is the design's arrangement, and WAL is
    /// what makes it safe.
    store: NoteStore,
}

impl Note {
    /// Opens the note in `dir`, creating the folder and the store if they are not there yet.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|error| {
            AppError::Note(format!("{} could not be made: {error}", dir.display()))
        })?;

        let store = NoteStore::open(&dir.join(NOTE_DB))?;

        Ok(Note {
            dir: dir.to_path_buf(),
            store,
        })
    }

    /// Places a note at `dir` out of whatever `source` is, and opens it.
    ///
    /// Three things can be placed, decided by what the file *is* rather than by a menu of commands:
    ///
    /// * a **note zip** — this app's transfer file — is unpacked over the working copy;
    /// * an **old note** (a zip of JSON, see [`crate::legacy`]) is migrated into a store;
    /// * a **PDF** starts a new note written on that document.
    ///
    /// A folder is opened where it stands rather than copied, which is what makes a note in the app's
    /// own directory reachable from a file manager.
    pub fn placed_from(source: &Path, dir: &Path) -> Result<Self> {
        if source.is_dir() {
            return Note::open(source);
        }

        let extension = source
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase());

        match extension.as_deref() {
            Some("zip") => unpack(source, dir),
            Some("pdf") => start_on_document(source, dir),
            _ => Err(AppError::Note(format!(
                "{} is neither a note, a note zip, nor a PDF",
                source.display()
            ))),
        }?;

        Note::open(dir)
    }

    /// The folder the note lives in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The app's connection, for reading a page and for the note's own facts.
    pub fn store(&self) -> &NoteStore {
        &self.store
    }

    /// The app's connection, mutably: what the app records about the note when it changes — a page
    /// turned, a page inserted, a note being migrated.
    pub fn store_mut(&mut self) -> &mut NoteStore {
        &mut self.store
    }

    /// The strokes of one page, as the note holds them.
    ///
    /// The page is read when it is turned to rather than when the note is opened — a thousand-page
    /// note opens as fast as a one-page one — and this is that read: the page's chunks and its dirty
    /// strokes, in the order it was drawn.
    pub fn page_strokes(&self, page: u64) -> Result<Vec<Stroke>> {
        self.store.load(page)
    }

    /// The document the note was written on: the name it had, and its bytes.
    ///
    /// The name is the note's own record — `chapter-3.pdf`, whatever the file inside the note folder
    /// is called — so a note comes back about the document it is about, and not about `source.pdf`.
    pub fn document(&self) -> Result<Option<(String, Vec<u8>)>> {
        let path = self.dir.join(NOTE_PDF);
        if !path.exists() {
            return Ok(None);
        }

        let bytes = std::fs::read(&path).map_err(|error| {
            AppError::Note(format!("{} could not be read: {error}", path.display()))
        })?;

        Ok(Some((self.store.document()?.unwrap_or_else(|| String::from(NOTE_PDF)), bytes)))
    }

    /// Copies `from` into the note as its document, and records the name it had.
    pub fn copy_document(&mut self, from: &Path) -> Result<String> {
        let name = from
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| String::from(NOTE_PDF));

        let bytes = std::fs::read(from).map_err(|error| {
            AppError::Note(format!("{} could not be read: {error}", from.display()))
        })?;

        self.put_document(&name, &bytes)?;
        Ok(name)
    }

    /// Writes a document into the note, and records the name it had.
    pub fn put_document(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.dir.join(NOTE_PDF);
        std::fs::write(&path, bytes).map_err(|error| {
            AppError::Note(format!("{} could not be written: {error}", path.display()))
        })?;

        self.store.set_document(name)
    }
}

/// Writes a note out as one file: a complete, standalone copy for a person to carry.
///
/// This is what the bar's *Save* does, on the writer's thread. The note is already saved — the folder
/// is written as the pen moves — so what is left is the file that moves between machines:
/// `VACUUM INTO` for the database (WAL folded in, nothing half-written), then the document and the
/// attachments beside it in a zip. The vacuum goes to a temporary file *inside* the note folder, so
/// packing it is one volume's worth of copying, and the temporary file is removed either way.
pub fn export(store: &NoteStore, dir: &Path, target: &Path) -> Result<()> {
    store.checkpoint()?;

    let temporary = dir.join(EXPORT_TMP);
    let _ = std::fs::remove_file(&temporary);

    let result = store
        .export(&temporary)
        .and_then(|()| pack(&temporary, dir, target));

    let _ = std::fs::remove_file(&temporary);
    result
}

/// Unpacks a zip into a working copy, migrating an old note if that is what it is.
fn unpack(source: &Path, dir: &Path) -> Result<()> {
    let file = std::fs::File::open(source).map_err(|error| {
        AppError::Note(format!("{} could not be read: {error}", source.display()))
    })?;

    let mut archive = ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| {
        AppError::Note(format!(
            "{} is not a zip file this app can read: {error}",
            source.display()
        ))
    })?;

    // The old shape is a zip with `notes.json` in it and no database; the new one always has the
    // database, because the database *is* the note.
    if archive.by_name(NOTE_DB).is_err() {
        return migrate(source, dir);
    }

    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|error| {
        AppError::Note(format!("{} could not be made: {error}", dir.display()))
    })?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            AppError::Note(format!(
                "{} holds an entry it could not open: {error}",
                source.display()
            ))
        })?;

        // `enclosed_name` is the zip-slip check: an entry called `..\..\somewhere` is refused rather
        // than written, which is the one way a note from someone else can write outside its folder.
        let Some(name) = entry.enclosed_name().map(|name| name.to_path_buf()) else {
            return Err(AppError::Note(format!(
                "{} holds an entry that points outside the note: {}",
                source.display(),
                entry.name()
            )));
        };

        let destination = dir.join(&name);
        if entry.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|error| {
                AppError::Note(format!("{} could not be made: {error}", destination.display()))
            })?;
            continue;
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                AppError::Note(format!("{} could not be made: {error}", parent.display()))
            })?;
        }

        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes).map_err(|error| {
            AppError::Note(format!("{} could not be read: {error}", source.display()))
        })?;

        std::fs::write(&destination, bytes).map_err(|error| {
            AppError::Note(format!(
                "{} could not be written: {error}",
                destination.display()
            ))
        })?;
    }

    Ok(())
}

/// Turns an old note — a zip of JSON — into a store.
///
/// Every page is written as a chunk and every note-level fact into `meta`, so the migration is the
/// shape of a save with the "later" taken out: a migration has no later, so it compacts immediately
/// instead of leaving the ink in the write-ahead area.
fn migrate(source: &Path, dir: &Path) -> Result<()> {
    let old = crate::legacy::read(source)?;

    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|error| {
        AppError::Note(format!("{} could not be made: {error}", dir.display()))
    })?;

    let mut note = Note::open(dir)?;

    for (page, strokes) in &old.pages {
        note.store_mut().rewrite(*page as u64, strokes)?;
    }

    if let Some((name, bytes)) = &old.document {
        note.put_document(name, bytes)?;
    }

    note.store_mut().set_sheet(old.sheet)?;
    note.store_mut().set_layout(&old.layout)?;
    note.store_mut().set_open_page(old.page as u64)?;
    note.store().checkpoint()?;

    Ok(())
}

/// Starts a note on a document: the PDF is copied in, and the ink starts empty.
fn start_on_document(source: &Path, dir: &Path) -> Result<()> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|error| {
        AppError::Note(format!("{} could not be made: {error}", dir.display()))
    })?;

    let mut note = Note::open(dir)?;
    note.copy_document(source)?;
    note.store().checkpoint()?;

    Ok(())
}

/// Packs a vacuumed database and the rest of a note folder into one file.
///
/// The database and the document are **stored** rather than deflated: both are compressed already —
/// SQLite fills its pages, a PDF is a compressed file — so deflating them would spend milliseconds to
/// save a fraction of a percent. Attachments are deflated, because an attachment is whatever the user
/// put there and is often text or a plain image.
fn pack(database: &Path, dir: &Path, target: &Path) -> Result<()> {
    let file = std::fs::File::create(target).map_err(|error| {
        AppError::Note(format!("{} could not be written: {error}", target.display()))
    })?;

    let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
    let stored = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o644);
    let deflated = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    add_file(&mut zip, NOTE_DB, database, stored)?;

    let document = dir.join(NOTE_PDF);
    if document.exists() {
        add_file(&mut zip, NOTE_PDF, &document, stored)?;
    }

    for (name, path) in folder_contents(&dir.join(ATTACHMENTS))? {
        let entry = format!("{ATTACHMENTS}/{name}");
        add_file(&mut zip, &entry, &path, deflated)?;
    }

    zip.finish().map_err(|error| {
        AppError::Note(format!("{} could not be closed: {error}", target.display()))
    })?;

    Ok(())
}

/// Adds one file to a zip that is being written.
fn add_file(
    zip: &mut ZipWriter<std::io::BufWriter<std::fs::File>>,
    entry: &str,
    path: &Path,
    options: SimpleFileOptions,
) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| AppError::Note(format!("{} could not be read: {error}", path.display())))?;

    zip.start_file(entry, options)
        .map_err(|error| AppError::Note(format!("the note could not be written: {error}")))?;

    std::io::copy(&mut file, zip)
        .map(|_| ())
        .map_err(|error| AppError::Note(format!("the note could not be written: {error}")))
}

/// Every file under `dir`, by its path relative to it.
///
/// Nothing writes attachments yet — the folder is where a future feature puts a pasted image or a
/// recording — but the container carries whatever is in it, so a note that *does* have an attachment
/// survives being packed and unpacked by this build.
fn folder_contents(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut found = BTreeMap::new();

    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current).map_err(|error| {
            AppError::Note(format!("{} could not be read: {error}", current.display()))
        })?;

        for entry in entries {
            let entry = entry.map_err(|error| {
                AppError::Note(format!("{} could not be read: {error}", current.display()))
            })?;
            let path = entry.path();

            if path.is_dir() {
                pending.push(path);
                continue;
            }

            let name = path
                .strip_prefix(dir)
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| String::from("attachment"));

            found.insert(name, path);
        }
    }

    Ok(found.into_iter().collect())
}

/// One thing for the writer thread to do.
#[derive(Debug)]
pub enum Job {
    /// Finished strokes to add to a page: the pen's own path.
    Append {
        /// The page the ink belongs to.
        page: u64,
        /// The strokes, in the order they were drawn.
        strokes: Vec<Stroke>,
    },
    /// A page to write again from scratch, because it was undone, erased or cleared.
    Rewrite {
        /// The page.
        page: u64,
        /// The ink it holds now.
        strokes: Vec<Stroke>,
    },
    /// A page to fold into chunks, because it is being closed.
    Compact {
        /// The page.
        page: u64,
    },
    /// The write-ahead log to fold back into the note file.
    Checkpoint,
    /// The note to write out as one file, and to say so when it is done.
    Export {
        /// Where the file goes.
        target: PathBuf,
    },
}

/// What the writer thread has to say when it is done.
#[derive(Debug)]
pub enum Report {
    /// A note was written out as one file.
    Written(PathBuf),
    /// Something did not work, in the words the status line will show.
    Failed(String),
}

/// The writing half of a note: its own connection, on its own thread.
///
/// The pen's path calls [`Self::append`] and forgets about it. Everything the queue does — a batch
/// commit, a compaction, a `VACUUM INTO` of the whole note — happens off the UI thread, and WAL means
/// none of it blocks the reader that is drawing the page in front of the user.
///
/// The connection is opened *before* the thread starts, so that a note that cannot be opened at all
/// is an error the app can show, rather than a thread that quietly does nothing for the rest of the
/// session.
#[derive(Debug)]
pub struct NoteWriter {
    /// The queue the UI thread puts work on.
    jobs: Sender<Job>,
    /// What the writer has to report, drained by the app on its frame tick.
    reports: Receiver<Report>,
}

impl NoteWriter {
    /// Starts a writer for the note in `dir`.
    pub fn spawn(dir: &Path) -> Result<Self> {
        let mut store = NoteStore::open(&dir.join(NOTE_DB))?;
        let dir = dir.to_path_buf();

        let (jobs, queue) = mpsc::channel::<Job>();
        let (reporter, reports) = mpsc::channel::<Report>();

        std::thread::Builder::new()
            .name(String::from("cheap-note-writer"))
            .spawn(move || {
                // The queue is the lifetime: when the app drops the writer, the loop ends and the
                // connection closes, which is the last checkpoint the note needs.
                while let Ok(job) = queue.recv() {
                    if let Err(error) = run(&mut store, &dir, job, &reporter) {
                        let _ = reporter.send(Report::Failed(error.to_string()));
                    }
                }
            })
            .map_err(|error| {
                AppError::Note(format!("the note's writer could not be started: {error}"))
            })?;

        Ok(NoteWriter { jobs, reports })
    }

    /// Adds ink to a page. Nothing waits for it.
    pub fn append(&self, page: u64, strokes: Vec<Stroke>) {
        let _ = self.jobs.send(Job::Append { page, strokes });
    }

    /// Writes a page again from scratch.
    pub fn rewrite(&self, page: u64, strokes: Vec<Stroke>) {
        let _ = self.jobs.send(Job::Rewrite { page, strokes });
    }

    /// Folds a page's ink into chunks, because the page is being closed.
    pub fn compact(&self, page: u64) {
        let _ = self.jobs.send(Job::Compact { page });
    }

    /// Folds the write-ahead log back into the note file.
    pub fn checkpoint(&self) {
        let _ = self.jobs.send(Job::Checkpoint);
    }

    /// Writes the note out as one file.
    pub fn export(&self, target: PathBuf) {
        let _ = self.jobs.send(Job::Export { target });
    }

    /// What the writer has done or failed to do since the last call. Never blocks.
    pub fn drain(&self) -> Vec<Report> {
        let mut taken = Vec::new();
        while let Ok(report) = self.reports.try_recv() {
            taken.push(report);
        }

        taken
    }
}

/// One job, run on the writer's thread.
///
/// An export is the one job that reports success as well as failure: it is the only one the user is
/// waiting for, and "the note was written" is a claim this app makes to their face.
fn run(store: &mut NoteStore, dir: &Path, job: Job, reports: &Sender<Report>) -> Result<()> {
    match job {
        Job::Append { page, strokes } => store.append(page, &strokes),
        Job::Rewrite { page, strokes } => store.rewrite(page, &strokes),
        Job::Compact { page } => store.compact(page).map(|_| ()),
        Job::Checkpoint => store.checkpoint(),

        Job::Export { target } => {
            export(store, dir, &target)?;
            let _ = reports.send(Report::Written(target));
            Ok(())
        }
    }
}

/// The last thing a writer does: fold the write-ahead log back into the note file.
///
/// Dropping the writer drops its sender, and the thread that is parked on the queue then finishes
/// the work in hand and stops. Sending this *before* that — a checkpoint is the one job worth
/// asking for on the way out — means a note does not sit next to a log with the last session in it.
impl Drop for NoteWriter {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Checkpoint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::InkPoint;
    use crate::pages::Page;
    use std::io::Write as _;
    use std::time::{Duration, Instant};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    /// An empty directory in the temp directory, named after the test that wants it.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cheap-note-note-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// A stroke of `points` points across the page.
    fn stroke(points: usize, from: f32) -> Stroke {
        let mut stroke = Stroke::new(InkPoint::new(from, 5.0, 2.0), Stroke::DEFAULT_COLOR);

        for step in 1..points {
            stroke.points.push(InkPoint::new(from + step as f32 * 2.0, 5.0, 2.0));
        }

        stroke.close();
        stroke
    }

    /// A PDF that is not one, since nothing in this module parses it.
    fn a_pdf() -> Vec<u8> {
        b"%PDF-1.4 not really".to_vec()
    }

    /// A note on a document can be placed from the document alone.
    #[test]
    fn a_note_starts_on_a_document() {
        let scratch = scratch("start");
        let document = scratch.join("chapter-3.pdf");
        std::fs::write(&document, a_pdf()).expect("the document is written");

        let note = Note::placed_from(&document, &scratch.join("placed")).expect("a note");

        let (name, bytes) = note.document().expect("the document").expect("it is there");
        assert_eq!(name, "chapter-3.pdf");
        assert_eq!(bytes, a_pdf());
        assert!(note.store().pages().expect("pages").is_empty());

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A note, its ink and its page list travel as one file and come back whole.
    #[test]
    fn a_note_travels_as_one_file() {
        let scratch = scratch("travel");
        let home = scratch.join("home");
        let mut note = Note::open(&home).expect("a note");
        note.put_document("chapter-3.pdf", &a_pdf())
            .expect("a document");
        note.store_mut()
            .rewrite(1, &[stroke(4, 0.0), stroke(4, 40.0)])
            .expect("ink");
        note.store_mut()
            .set_layout(&[Page::Document(0), Page::Blank])
            .expect("a list");
        note.store_mut()
            .set_sheet(Some((794.0, 1123.0)))
            .expect("a sheet");
        note.store_mut().set_open_page(1).expect("a page");

        let carried = scratch.join("chapter-3.zip");
        export(note.store(), note.dir(), &carried).expect("the note is written out");

        let elsewhere = scratch.join("elsewhere");
        let arrived = Note::placed_from(&carried, &elsewhere).expect("the note is placed");

        assert_eq!(arrived.dir(), elsewhere.as_path());
        assert_eq!(arrived.store().load(1).expect("a page").len(), 2);
        assert_eq!(arrived.store().open_page().expect("a page"), Some(1));
        assert_eq!(
            arrived.store().sheet().expect("a sheet"),
            Some((794.0, 1123.0))
        );
        assert_eq!(
            arrived.store().layout().expect("a list"),
            vec![Page::Document(0), Page::Blank]
        );
        let (name, bytes) = arrived.document().expect("the document").expect("it is there");
        assert_eq!(name, "chapter-3.pdf");
        assert_eq!(bytes, a_pdf());

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Anything in `attachments/` is carried with the note: nothing writes one yet, and a container
    /// that dropped one would be a container that loses a user's file.
    #[test]
    fn attachments_travel_with_the_note() {
        let scratch = scratch("attachments");
        let home = scratch.join("home");
        let note = Note::open(&home).expect("a note");

        let folder = home.join(ATTACHMENTS);
        std::fs::create_dir_all(folder.join("images")).expect("an attachments folder");
        std::fs::write(folder.join("images/diagram.txt"), b"a diagram").expect("an attachment");

        let carried = scratch.join("carried.zip");
        export(note.store(), note.dir(), &carried).expect("the note is written out");

        let elsewhere = scratch.join("elsewhere");
        Note::placed_from(&carried, &elsewhere).expect("the note is placed");
        let arrived = std::fs::read(elsewhere.join(ATTACHMENTS).join("images/diagram.txt"))
            .expect("the attachment came back");

        assert_eq!(arrived, b"a diagram");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A note written by the old build is migrated rather than lost.
    #[test]
    fn an_old_note_is_migrated_into_a_store() {
        let scratch = scratch("legacy");
        let old = scratch.join("old.zip");

        let notes = serde_json::json!({
            "format": 2,
            "page": 1,
            "sheet": [595.0, 842.0],
            "document": "old.pdf",
            "layout": ["blank"],
            "pages": [{
                "page": 0,
                "strokes": [{
                    "points": [
                        {"x": 1.0, "y": 2.0, "width": 3.0},
                        {"x": 4.0, "y": 5.0, "width": 6.0}
                    ],
                    "color": 1776415
                }]
            }]
        })
        .to_string();

        {
            let file = std::fs::File::create(&old).expect("a file");
            let mut zip = ZipWriter::new(file);
            zip.start_file("notes.json", SimpleFileOptions::default())
                .expect("an entry");
            zip.write_all(notes.as_bytes()).expect("the ink");
            zip.start_file("document.pdf", SimpleFileOptions::default())
                .expect("an entry");
            zip.write_all(&a_pdf()).expect("the document");
            zip.finish().expect("a finished zip");
        }

        let placed = scratch.join("placed");
        let note = Note::placed_from(&old, &placed).expect("the old note is migrated");

        assert_eq!(note.store().load(0).expect("a page").len(), 1);
        assert_eq!(note.store().open_page().expect("a page"), Some(1));
        assert_eq!(note.store().sheet().expect("a sheet"), Some((595.0, 842.0)));
        assert_eq!(note.store().layout().expect("a list"), vec![Page::Blank]);
        let (name, bytes) = note.document().expect("the document").expect("it is there");
        assert_eq!(name, "old.pdf");
        assert_eq!(bytes, a_pdf());

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The writer thread writes what it is given, and says when an export is done.
    #[test]
    fn the_writer_writes_on_its_own_thread() {
        let scratch = scratch("writer");
        let home = scratch.join("home");
        Note::open(&home).expect("a note");

        let writer = NoteWriter::spawn(&home).expect("a writer");
        writer.append(0, vec![stroke(5, 0.0)]);
        writer.append(0, vec![stroke(5, 60.0)]);
        writer.compact(0);
        writer.rewrite(1, vec![stroke(3, 200.0)]);

        let carried = scratch.join("carried.zip");
        writer.export(carried.clone());

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut written = false;
        while Instant::now() < deadline && !written {
            for report in writer.drain() {
                match report {
                    Report::Written(path) => {
                        assert_eq!(path, carried);
                        written = true;
                    }
                    Report::Failed(message) => panic!("the writer failed: {message}"),
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(written, "the export was reported");
        assert!(carried.exists());

        let arrived = scratch.join("arrived");
        let note = Note::placed_from(&carried, &arrived).expect("the note is placed");
        assert_eq!(note.store().load(0).expect("a page").len(), 2);
        assert_eq!(note.store().load(1).expect("a page").len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A zip that is not a note is refused.
    #[test]
    fn a_foreign_zip_is_refused() {
        let scratch = scratch("foreign");
        let other = scratch.join("other.zip");

        {
            let file = std::fs::File::create(&other).expect("a file");
            let mut zip = ZipWriter::new(file);
            zip.start_file("something-else.txt", SimpleFileOptions::default())
                .expect("an entry");
            zip.write_all(b"hello").expect("its contents");
            zip.finish().expect("a finished zip");
        }

        let placed = scratch.join("placed");
        let error = Note::placed_from(&other, &placed).expect_err("a foreign zip is refused");
        assert!(error.to_string().contains("notes.json"), "{error}");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A note from one path is placed in a folder of its own, and the same path twice is the same
    /// folder: importing a note again updates the working copy rather than making a second one.
    #[test]
    fn a_note_is_placed_where_its_source_says() {
        let one = folder_name(Path::new("/notes/chapter-3.pdf"));
        let again = folder_name(Path::new("/notes/chapter-3.pdf"));
        let two = folder_name(Path::new("/other/chapter-3.pdf"));

        assert_eq!(one, again);
        assert_ne!(one, two, "two files of one name are two notes");
        assert!(
            one.starts_with("chapter-3-"),
            "the name says which file it came from: {one}"
        );
        assert!(one
            .chars()
            .all(|character| character != ' ' && character != '/'));
    }
}
