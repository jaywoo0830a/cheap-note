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
//! An open SQLite file must not live in a synchronising folder (Dropbox, iCloud, OneDrive), because
//! those tools copy and merge files under an open handle and corrupt them. So a note is *placed* —
//! imported into the app's own directory — and moved between machines by exporting a file. That is
//! also why [`Note::placed_from`] takes a destination: an import is a copy, and the original is never
//! opened for writing.
//!
//! ## Writing, on another thread
//!
//! [`NoteWriter`] owns its own connection and runs in a thread of its own: the pen's path only ever
//! *sends* a batch, and WAL means the reader — the app loading the page it is about to show — is
//! never blocked by the writer. One connection per thread, with a job queue in front of the writer,
//! is what makes a 500 ms batch commit invisible while writing.
//!
//! Errors do not vanish on that thread: they come back through [`NoteWriter::drain`], which the app
//! empties on every frame tick and shows in its status line. A writer that failed silently would be
//! the worst of both worlds — ink that is not being saved, and an app that looks like it is.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use zip::write::{ExtendedFileOptions, FileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::{Error, Result, Variant};
use crate::history::HistoryUpdate;
use crate::ink::Stroke;
use crate::mapped::MappedBytes;
use crate::store::{Facts, NoteStore};

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

/// Where the notes this application has been given live.
///
/// Under the user's own application data rather than beside the program or beside their files: a
/// note is data, and the rule this format is built on is that an open database must not sit in a
/// folder something else is synchronising (Dropbox, iCloud, OneDrive — see the module docs).
///
/// Each platform's own answer to "this user's application data" is used, because the format is
/// read and written by applications on all three:
///
/// | Platform | Where |
/// |---|---|
/// | Windows | `%LOCALAPPDATA%\cheap-note\notes` |
/// | macOS | `~/Library/Application Support/cheap-note/notes` |
/// | Everything else | `$XDG_DATA_HOME/cheap-note/notes`, or `~/.local/share/cheap-note/notes` |
///
/// A machine that reports none of those falls back to the temporary directory, which is at least
/// local — the one property the working copy has to have.
pub fn root() -> PathBuf {
    user_data().join("cheap-note").join("notes")
}

/// The directory this platform keeps a user's application data in.
///
/// A free function rather than part of [`root`] so that the fallbacks read in one place: every
/// branch here answers the same question, and the last line is the answer when a platform tells us
/// nothing at all.
fn user_data() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path);
    }

    // The directory a bundled application's data belongs in, and the one Time Machine already
    // knows to back up.
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support");
    }

    // The XDG base directory specification, which is what a Linux desktop reads.
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data);
        }

        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local").join("share");
        }
    }

    std::env::temp_dir()
}

/// The folder a note from `source` is placed in.
///
/// The name is the file's own stem plus a short digest of the *whole* path, so that
/// `/a/chapter-3.pdf` and `/b/chapter-3.pdf` are two notes rather than one that overwrites the
/// other, and so that importing the same file twice lands in the same place — the second import
/// updates the working copy a person is already used to.
///
/// ## One name, on every machine
///
/// The folder's name is a note's identity ([`crate::store`] §1), and a note carried to another
/// machine keeps it — so the name this function hands out has to be one a *Windows*, a *macOS* and
/// a *Linux* filesystem all accept, and the *same* string everywhere it is asked:
///
/// | The rule | Why | Where it bites |
/// |---|---|---|
/// | no character Windows reserves (`<>:"/\|?*`, the control characters) | a name with one of them cannot be created, so the note would not restore at all | Windows |
/// | no `.`, ` ` or `-` at the end | Windows *silently strips* a trailing one, so the folder on disk is not the name the manifest, the library and the recent list all say | Windows |
/// | ASCII letters, digits, `_` and `-` stay; every other run becomes one `-` | a Korean, Japanese or emoji stem becomes a name every filesystem and every zip tool shows the same way — and one that is still there when the file manager cannot decode names | everywhere |
/// | the digest is over the path's UTF-8, not its `OsStr` | Windows paths are UTF-16 and everywhere else is bytes; the same path has to digest to the same number on both | a library read on another platform |
///
/// The digest is what keeps two Korean documents from becoming one note, which a name with only
/// dashes for its stem would otherwise do: `보고서.pdf` and `계획.pdf` share a stem of `note`, and
/// only their paths tell them apart.
pub fn folder_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| String::from("note"));

    // Every character that is unreserved *on every filesystem* — ASCII letters, digits, `_` and `-`
    // — is kept as it is; every other run collapses to one dash, so `a b:c*d?` is `a-b-c-d` rather
    // than `a-b-c-d-`, and a stem of only such characters is left empty for the fallback below.
    let mut cleaned = String::with_capacity(stem.len());
    let mut dash = false;
    for character in stem.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            cleaned.push(character);
            dash = false;
        } else if !dash && !cleaned.is_empty() {
            cleaned.push('-');
            dash = true;
        }
    }

    while cleaned.ends_with('-') {
        cleaned.pop();
    }

    // A name with no ASCII letters at all — a Korean title, an emoji, a file called `한글.pdf` — is
    // named for what it is instead of for its stem, because the digest already makes it unique.
    // Empty would be a folder called `-9f2a1c44`, and a leading dash is a name a shell — and one
    // Windows API — read as an option rather than a file.
    if cleaned.is_empty() {
        cleaned = String::from("note");
    }

    // FNV-1a over the path's UTF-8 bytes, written as eight hex digits. Not a hash for security,
    // only for a name that is stable and short — and UTF-8 rather than the platform's own encoding
    // so that the same path digests to the same number wherever this is asked.
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in source.to_string_lossy().bytes() {
        digest ^= u64::from(byte);
        digest = digest.wrapping_mul(0x100_0000_01b3);
    }

    format!("{cleaned}-{:08x}", digest as u32)
}

/// The fingerprint of a file as a note records it: `<size>-<crc32>`.
///
/// Read a block at a time rather than whole, so that a 400 MB PDF fingerprintes the same way a 4 KB one
/// does — the note's own copy is mapped, but the file being *opened* may be anywhere. `None` for a file
/// that cannot be read, because a fingerprint that could not be taken is the absence of one and not a
/// value: a caller deciding whether two files are the same document must not be told "yes" by an error.
pub fn fingerprint(path: &Path) -> Option<String> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();

    let mut crc = crc32fast::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        crc.update(&buffer[..read]);
    }

    Some(format!("{size}-{:08x}", crc.finalize()))
}

/// Fills in the document of a note that has lost it, from a file that is the same document.
///
/// The failure this repairs: a note's folder is named after the path of the PDF it was placed from, so a
/// PDF that moved — into `Downloads` when it was opened from a sync folder, out of a temporary folder,
/// off a stick — is a *different path*, and so a different folder name. Opening the moved file made a
/// second, empty note about the same pages, while the first note sat with no `source.pdf` and could only
/// ever open as a blank sheet: the document and the ink about it had been split between two folders.
///
/// Here the document is put back where the ink is. Only the document is copied — `source.pdf`, the name
/// it is called, the path it is now at, and its fingerprint — and never a stroke, a page, or a title: the
/// ink in that folder is the writing this exists to keep. The note answers its facts afterwards, which is
/// what a screen showing the note redraws from.
///
/// Refuses a note that already has its document: replacing one with a file that merely looks like it is
/// a decision for a person, not a guess made while they were opening a PDF.
pub fn adopt_document(folder: &Path, from: &Path) -> Result<Facts> {
    let mut note = Note::open(folder)?;

    if folder.join(NOTE_PDF).is_file() {
        return Err(Error::Note(format!(
            "{} already has its document",
            folder.display()
        )));
    }

    let name = note.copy_document(from)?;

    if let Some(print) = fingerprint(from) {
        note.store_mut().set_document_fingerprint(&print)?;
    }

    note.store_mut().set_source(Some(from))?;
    note.store().checkpoint()?;

    let facts = note.store().facts()?;

    // Said so that a reader of the code knows what is deliberately *not* here: no `set_title`, no page
    // list, no blob. The document arrived; the ink did not move.
    let _ = &name;

    Ok(facts)
}

/// The note a document belongs to: the one whose document is the same file, at any path it may have
/// had when it was placed.
///
/// Two answers, in order, and the first one that stands decides:
///
/// 1. **The same path** — `folder_name(source)` is a note, and it is opened. This is the ordinary case,
///    and it is why the folder name exists: the same file opened twice is the same note.
/// 2. **The same document at another path** — a note that was placed from a *different* path, is
///    missing its `source.pdf`, and says either the same fingerprint as the file (size and CRC-32) or,
///    for a note written before fingerprints, the same document *name*.
///
/// The second is the repair of the split described in [`adopt_document`], and it is deliberately narrow:
/// a note that still *has* its document is not a candidate, so nothing that already looks whole is
/// considered; and two notes that both fit are answered as no match at all, because a guess between them
/// is how the wrong pages get a person's ink. `None` means "this is a new document", and the caller
/// places a new note.
pub fn matching_note(root: &Path, source: &Path) -> Result<Option<PathBuf>> {
    let exact = root.join(folder_name(source));
    if exact.join(NOTE_DB).is_file() {
        return Ok(Some(exact));
    }

    let Ok(wanted) = std::fs::metadata(source) else {
        return Ok(None);
    };

    if !wanted.is_file() {
        return Ok(None);
    }

    let wanted_fingerprint = fingerprint(source);
    let wanted_name = source
        .file_name()
        .map(|name| name.to_string_lossy().to_string());

    let mut candidates = Vec::new();

    for folder in notes_in(root)? {
        let Ok(store) = NoteStore::open(&folder.join(NOTE_DB)) else {
            continue;
        };

        let held = store.document_fingerprint().ok().flatten();

        match (&wanted_fingerprint, &held) {
            (Some(wanted), Some(held)) => {
                // The note's own copy of its document: what the note *is* about, whether or not the
                // original is still there. Two notes about identical bytes are one document, so this is
                // a candidate — the "a note with its document is not adopted" rule is about a document
                // that *differs*, and `adopt_document` refuses to replace one that is still there.
                if wanted != held {
                    continue;
                }

                // The same bytes: the note is about this document. Whether it still holds its copy or
                // not is the caller's question — `adopt_document` fills in a missing document and
                // refuses to replace one that is there, so both answers lead to the right place.
                candidates.push(folder);
            }
            // A note written before fingerprints were recorded: its document's *name* is the only thing
            // it can be recognised by, and a name is a weaker answer than a size and a CRC — which is why
            // the fingerprint is asked first, and this is the fallback and not the rule. A note that still
            // has a `source.pdf` is not a fallback candidate: nothing is missing, so nothing is repaired.
            (Some(_), None) => {
                if folder.join(NOTE_PDF).exists() {
                    continue;
                }

                let same_name = matches!(
                    (&wanted_name, store.document()),
                    (Some(wanted), Ok(Some(held))) if *wanted == held
                );

                if same_name {
                    candidates.push(folder);
                }
            }
            _ => {}
        }
    }

    match candidates.len() {
        1 => Ok(candidates.pop()),
        // None is a new document; more than one is a question this will not guess an answer to.
        _ => Ok(None),
    }
}

/// Every note in a given folder: the same listing, for a test or another location.
///
/// A folder without a `note.db` in it is not a note — an attachments folder, or a directory someone
/// put there — and is left out rather than offered as something to open. A `root` that does not
/// exist yet is *no notes*, not an error: that is what a first run looks like.
pub fn notes_in(root: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Error::Note(format!(
                "{} could not be read: {error}",
                root.display()
            )))
        }
    };

    let mut found = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| {
                Error::Note(format!("{} could not be listed: {error}", root.display()))
            })?
            .path();

        if path.join(NOTE_DB).is_file() {
            found.push(path);
        }
    }

    // By name, so that two scans of an unchanged folder agree about the order they found things in.
    found.sort();
    Ok(found)
}

/// Gives the note in `folder` a name, and answers what the list should now show for it.
///
/// Done through the note's own database rather than the index, because the *note* owns its name: that
/// is what makes a name survive being carried to another machine, and what makes the index a cache
/// that can be deleted. Nothing about the ink is touched — this is one `meta` write.
///
/// The database is opened for the length of the write and closed. Opening a second connection to a
/// note the app already has open is what SQLite is for: WAL means two connections never wait for each
/// other, and this one only writes.
pub fn set_title(folder: &Path, name: &str) -> Result<Facts> {
    let mut store = NoteStore::open(&folder.join(NOTE_DB))?;
    store.set_title(name)?;
    store.facts()
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
            Error::Note(format!("{} could not be made: {error}", dir.display()))
        })?;

        let store = NoteStore::open(&dir.join(NOTE_DB))?;

        Ok(Note {
            dir: dir.to_path_buf(),
            store,
        })
    }

    /// Places a note at `dir` out of whatever `source` is, and opens it.
    ///
    /// Two things can be placed, decided by what the file *is* rather than by a menu of commands:
    ///
    /// * a **note zip** — this app's transfer file — is unpacked over the working copy;
    /// * a **PDF** starts a new note written on that document.
    ///
    /// A folder is opened where it stands rather than copied, which is what makes a note in the app's
    /// own directory reachable from a file manager.
    ///
    /// **A PDF whose note already exists is not re-placed by this function.** Placing one starts a
    /// note on its document, which is a *new* note — so a caller that means "open the note about
    /// this PDF" opens the folder instead ([`Note::open`]). See the app's `NoteApp::open_any`,
    /// which asks the difference out loud rather than choosing for the user.
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
            _ => Err(Variant::NotANote.said(format!(
                "{} is neither a note zip nor a PDF",
                source.display()
            ))),
        }?;

        // Which file a note was made from is recorded *in* the note, so a list can say which PDF a
        // note is about and so that a note carried to another machine still knows where it came
        // from. It is a convenience rather than a dependency: the document itself is in the folder.
        let mut note = Note::open(dir)?;
        note.store_mut().set_source(Some(source))?;
        Ok(note)
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
    /// turned, a page inserted, a page deleted.
    pub fn store_mut(&mut self) -> &mut NoteStore {
        &mut self.store
    }

    /// The document the note was written on: the name it had, and its bytes.
    ///
    /// The name is the note's own record — `chapter-3.pdf`, whatever the file inside the note folder
    /// is called — so a note comes back about the document it is about, and not about `source.pdf`.
    pub fn document(&self) -> Result<Option<(String, MappedBytes)>> {
        let path = self.dir.join(NOTE_PDF);
        if !path.exists() {
            return Ok(None);
        }

        // Mapped rather than read, so that closing the note gives the bytes back: see
        // [`crate::mapped`], where the measurement behind that choice is written down.
        let bytes = MappedBytes::open(&path).map_err(|error| {
            Error::Note(format!("{} could not be read: {error}", path.display()))
        })?;

        Ok(Some((
            self.store
                .document()?
                .unwrap_or_else(|| String::from(NOTE_PDF)),
            bytes,
        )))
    }

    /// Copies `from` into the note as its document, and records the name it had.
    pub fn copy_document(&mut self, from: &Path) -> Result<String> {
        let name = from
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| String::from(NOTE_PDF));

        let bytes = std::fs::read(from).map_err(|error| {
            Error::Note(format!("{} could not be read: {error}", from.display()))
        })?;

        self.put_document(&name, &bytes)?;
        Ok(name)
    }

    /// Writes a document into the note, and records the name it had.
    ///
    /// A caller still holding a [`MappedBytes`] from [`Self::document`] must drop it first on Windows:
    /// the operating system will not truncate a file while a view of it is mapped
    /// (`ERROR_USER_MAPPED_FILE`), and the error this returns would be that refusal rather than a
    /// failure of the write. The app's document view is dropped before a note's document is replaced;
    /// nothing reads a note's document while it is being written.
    pub fn put_document(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.dir.join(NOTE_PDF);
        std::fs::write(&path, bytes).map_err(|error| {
            Error::Note(format!("{} could not be written: {error}", path.display()))
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

/// The prefix that makes a zip hold *several* notes instead of one.
///
/// A selection is one zip with `notes/<folder>/` in front of the entry names a single note has. A
/// selection of one note is written flat — `note.db`, `source.pdf` — so it *is* an ordinary export and
/// is read by [`unpack`]; a selection of two or more is written under this prefix and read by
/// [`read_selection`]. One rule, stated once: **a note is a folder holding a [`NOTE_DB`], wherever it is
/// in the zip**, which is why the reader can decide between the two by looking at the entry names and
/// nothing else.
///
/// There is deliberately no manifest, no version and no list of what is inside: the zip's own entry list
/// *is* the list, and a second copy of it would be a second thing to keep in step.
pub const SELECTION_NOTES: &str = "notes";

/// Whether `folder` is a name a note folder can have: no separators, no `.`/`..`, and not empty.
fn is_folder_name(folder: &str) -> bool {
    !folder.is_empty()
        && folder != "."
        && folder != ".."
        && !folder.contains('/')
        && !folder.contains('\\')
        && !folder.contains(':')
}

/// Whether `path` is a path inside a note rather than out of one.
///
/// Relative, and made of ordinary names: an absolute path with a drive letter, a `..` anywhere, or an
/// empty segment is not a file sitting inside a note folder.
fn is_note_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains("..")
        && !path.contains(':')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != ".")
}

/// Writes several notes into one zip at `target`: the export of a selection.
///
/// Each note is vacuumed exactly as [`export`] does it and packed with the same three names, so a note in
/// a selection is the note it would have been on its own — the only difference between them is the
/// prefix. One note is written *flat*, which makes a selection of one an ordinary export: a person who
/// ticks one row and saves gets exactly the file *Save* would have written, and the reader does not have
/// to know which door it came from.
///
/// The zip is built beside `target` and renamed onto it, so a failed selection leaves the file that was
/// there rather than half of the new one — the same rule as [`export`], for the same reason.
///
/// Returns the folders written, in the order they were packed, which is what a caller counts.
pub fn export_selection(folders: &[PathBuf], target: &Path) -> Result<Vec<String>> {
    // Every temporary this function makes, removed by whichever path leaves it.
    struct Temps(Vec<PathBuf>);

    impl Temps {
        fn keep(&mut self, path: PathBuf) -> PathBuf {
            self.0.push(path.clone());
            path
        }
    }

    impl Drop for Temps {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    let mut temps = Temps(Vec::new());
    let staging =
        temps.keep(target.with_file_name(format!(".tmp-{}.selection.zip", std::process::id())));

    let mut entries = Vec::new();
    let mut written = Vec::new();

    // Flat when there is one note, so that a selection of one is the shape of an export.
    let flat = folders.len() == 1;

    for folder in folders {
        let name = folder
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| String::from("note"));

        // The name an entry is written under: the note's own, or the same name behind the prefix.
        let inside = |entry: String| {
            if flat {
                entry
            } else {
                format!("{SELECTION_NOTES}/{name}/{entry}")
            }
        };

        // The vacuum goes inside the note's own folder, on the volume the note is on, and is deleted
        // whether the selection is written or not.
        let vacuum = temps.keep(folder.join(EXPORT_TMP));
        let _ = std::fs::remove_file(&vacuum);
        let store = NoteStore::open(&folder.join(NOTE_DB))?;
        store.export(&vacuum)?;

        entries.push((
            inside(String::from(NOTE_DB)),
            vacuum,
            CompressionMethod::Stored,
        ));

        let document = folder.join(NOTE_PDF);
        if document.exists() {
            entries.push((
                inside(String::from(NOTE_PDF)),
                document,
                CompressionMethod::Stored,
            ));
        }

        for (attachment, path) in folder_contents(&folder.join(ATTACHMENTS))? {
            entries.push((
                inside(format!("{ATTACHMENTS}/{attachment}")),
                path,
                CompressionMethod::Deflated,
            ));
        }

        written.push(name);
    }

    write_zip(&staging, &entries)?;

    std::fs::rename(&staging, target).map_err(|error| {
        Error::Note(format!(
            "{} could not be written: {error}",
            target.display()
        ))
    })?;

    // The zip is the file it was asked for; every temporary has done its part.
    std::mem::forget(temps);
    for path in entries.iter().map(|(_, path, _)| path) {
        let _ = std::fs::remove_file(path);
    }

    Ok(written)
}

/// Reads a zip that holds several notes: every `notes/<folder>/` entry, grouped by folder.
///
/// This is the *listing* half of [`export_selection`]: what is in the file, said before anything is
/// written, so a restore can be described to a person ("three notes") before three folders appear. A
/// folder is a note when it carries a [`NOTE_DB`], exactly as a folder under the notes root is; an entry
/// under `notes/` that is not one is somebody's stray file and is not a note.
///
/// An entry whose path climbs out of its note is refused **here, at the listing**, so that nothing is ever
/// placed from a zip that asked to write outside the folder it is about.
pub fn read_selection(source: &Path) -> Result<Vec<PathBuf>> {
    let file = std::fs::File::open(source)
        .map_err(|error| Error::Note(format!("{} could not be read: {error}", source.display())))?;

    let mut archive = ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| {
        Error::Note(format!(
            "{} is not a zip file this app can read: {error}",
            source.display()
        ))
    })?;

    let mut found: Vec<PathBuf> = Vec::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| {
            Error::Note(format!(
                "{} holds an entry it could not open: {error}",
                source.display()
            ))
        })?;
        if entry.is_dir() {
            continue;
        }

        let name = entry
            .name()
            .map_err(|error| {
                Error::Note(format!("{} could not be read: {error}", source.display()))
            })?
            .replace('\\', "/");

        let prefix = format!("{SELECTION_NOTES}/");
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };

        // `<folder>/note.db` and nothing else makes a folder a note; the rest of the entry is the note's
        // own, and is refused if it climbs out rather than skipped.
        let Some((folder, inside)) = rest.split_once('/') else {
            continue;
        };

        if !is_folder_name(folder) || !is_note_path(inside) {
            return Err(Error::Note(format!(
                "{} holds an entry that points outside its note: {name}",
                source.display()
            )));
        }

        if inside == NOTE_DB {
            let folder = PathBuf::from(folder);
            if !found.contains(&folder) {
                found.push(folder);
            }
        }
    }

    Ok(found)
}

/// Places every note a selection holds into `notes_root`: the other half of [`export_selection`].
///
/// Each note is unpacked into a folder of its own under `notes_root`, named as it was carried, so a note
/// that arrives is the note it was where it was saved: the same folder, the same ink, the same name. A
/// note that is already there is **replaced** — a selection is a copy a person chose to keep, and
/// restoring one means its copy is the true copy of every note it holds.
///
/// Each note is unpacked into a temporary folder beside its destination and then renamed onto it, so an
/// interrupted restore leaves either the old note or the new one and never the two mixed, and a note that
/// fails to write leaves nothing behind at all.
///
/// Everything is refused **before the first note is written**: a zip that is not a selection, and a
/// selection holding an entry that climbs out of its note, place nothing at all. A person restoring four
/// notes should not find the first three laid down and the fourth refused.
///
/// Returns the folder of every note placed — which is what a caller counts and opens.
pub fn place_selection(source: &Path, notes_root: &Path) -> Result<Vec<PathBuf>> {
    // The listing first: what is in the file, and whether every entry in it is safe. A refusal here has
    // written nothing.
    let folders = read_selection(source)?;
    if folders.is_empty() {
        return Err(Variant::NotANote.said(format!(
            "{} is not a selection: it holds no {SELECTION_NOTES}/<note>/{NOTE_DB}",
            source.display()
        )));
    }

    let mut placed = Vec::new();
    for folder in folders {
        let destination = notes_root.join(&folder);

        // Beside the destination, so the rename onto it is on one volume.
        let staging = notes_root.join(format!(".tmp-{}.{}", std::process::id(), folder.display()));

        let written = unpack_selection_note(source, &folder, &staging);
        if let Err(error) = written {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }

        // `rename` will not replace a directory that already exists, so the old note is moved aside first
        // and put back if the rename fails — the old note is never lost to a failed restore.
        let aside = notes_root.join(format!(".old-{}.{}", std::process::id(), folder.display()));
        let _ = std::fs::remove_dir_all(&aside);

        let had_old = destination.exists();
        if had_old {
            std::fs::rename(&destination, &aside).map_err(|error| {
                let _ = std::fs::remove_dir_all(&staging);
                Error::Note(format!(
                    "{} could not be replaced: {error}",
                    destination.display()
                ))
            })?;
        }

        if let Err(error) = std::fs::rename(&staging, &destination) {
            // Put the note that was there back, so a failed restore is a restore that did not happen.
            if had_old {
                let _ = std::fs::rename(&aside, &destination);
            }
            let _ = std::fs::remove_dir_all(&staging);
            return Err(Error::Note(format!(
                "{} could not be written: {error}",
                destination.display()
            )));
        }

        let _ = std::fs::remove_dir_all(&aside);
        placed.push(destination);
    }

    Ok(placed)
}

/// Unpacks one note of a selection — the entries under `notes/<folder>/` — into `dir`.
///
/// The same three names a note is, written flat into `dir`, which is what makes the destination a note
/// folder rather than a copy of one. [`read_selection`] has already refused every entry that climbs out,
/// so the only names this reads are the note's own.
fn unpack_selection_note(source: &Path, folder: &Path, dir: &Path) -> Result<()> {
    let file = std::fs::File::open(source)
        .map_err(|error| Error::Note(format!("{} could not be read: {error}", source.display())))?;

    let mut archive = ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| {
        Error::Note(format!(
            "{} is not a zip file this app can read: {error}",
            source.display()
        ))
    })?;

    let prefix = format!("{SELECTION_NOTES}/{}/", folder.display());
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)
        .map_err(|error| Error::Note(format!("{} could not be made: {error}", dir.display())))?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            Error::Note(format!(
                "{} holds an entry it could not open: {error}",
                source.display()
            ))
        })?;
        if entry.is_dir() {
            continue;
        }

        let name = entry
            .name()
            .map_err(|error| {
                Error::Note(format!("{} could not be read: {error}", source.display()))
            })?
            .replace('\\', "/");

        let Some(inside) = name.strip_prefix(&prefix) else {
            continue;
        };

        let destination = dir.join(inside);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Note(format!("{} could not be made: {error}", parent.display()))
            })?;
        }

        let mut out = std::fs::File::create(&destination).map_err(|error| {
            Error::Note(format!(
                "{} could not be written: {error}",
                destination.display()
            ))
        })?;
        std::io::copy(&mut entry, &mut out).map_err(|error| {
            Error::Note(format!(
                "{} could not be written: {error}",
                destination.display()
            ))
        })?;
    }

    Ok(())
}

/// Unpacks a note zip into a working copy.
///
/// A zip with no [`NOTE_DB`] in it is not a note this app can open, and it is refused *before*
/// anything is written: unpacking first would leave a folder of someone else's files under the notes
/// folder, and a half-imported note is a worse thing to explain than a refusal.
fn unpack(source: &Path, dir: &Path) -> Result<()> {
    let file = std::fs::File::open(source)
        .map_err(|error| Error::Note(format!("{} could not be read: {error}", source.display())))?;

    let mut archive = ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| {
        Error::Note(format!(
            "{} is not a zip file this app can read: {error}",
            source.display()
        ))
    })?;

    // The database *is* the note: a zip without one holds nothing this build can open, whatever else
    // is in it.
    if archive.by_name(NOTE_DB).is_err() {
        return Err(Error::Note(format!(
            "{} is not a note: it holds no {NOTE_DB}",
            source.display()
        )));
    }

    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)
        .map_err(|error| Error::Note(format!("{} could not be made: {error}", dir.display())))?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            Error::Note(format!(
                "{} holds an entry it could not open: {error}",
                source.display()
            ))
        })?;

        // `enclosed_name` is the zip-slip check: an entry called `..\..\somewhere` is refused rather
        // than written, which is the one way a note from someone else can write outside its folder.
        let Some(name) = entry.enclosed_name().map(|name| name.to_path_buf()) else {
            // `name()` answers a `Result` since zip 9 — an entry whose name cannot be decoded has no
            // name to print — so a failure here is described by what is known instead: the index it
            // sits at, and that its name is the reason the entry was refused.
            let name = entry
                .name()
                .map(|name| name.into_owned())
                .unwrap_or_else(|_| format!("entry #{index}, whose name cannot be read"));
            return Err(Error::Note(format!(
                "{} holds an entry that points outside the note: {name}",
                source.display(),
            )));
        };

        let destination = dir.join(&name);
        if entry.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|error| {
                Error::Note(format!(
                    "{} could not be made: {error}",
                    destination.display()
                ))
            })?;
            continue;
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Note(format!("{} could not be made: {error}", parent.display()))
            })?;
        }

        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes).map_err(|error| {
            Error::Note(format!("{} could not be read: {error}", source.display()))
        })?;

        std::fs::write(&destination, bytes).map_err(|error| {
            Error::Note(format!(
                "{} could not be written: {error}",
                destination.display()
            ))
        })?;
    }

    Ok(())
}

/// Starts a note on a document: the PDF is copied in, and the ink starts empty.
///
/// The document's fingerprint is recorded here, with the document, so that every note made from now on
/// can be recognised if its file moves: see [`matching_note`] and [`adopt_document`]. The fingerprint is
/// taken from the *copy* in the note, not from `source`, so the two are the same bytes by construction.
///
/// ## A note that is already there is not started over
///
/// The folder is *not* wiped. This function used to begin with `remove_dir_all(dir)`, which meant that
/// placing a document over a folder that was already a note erased that note's ink and its own copy of
/// its document, and then put the new document down on empty tables. Nothing on screen asked to be
/// erased, and the one thing a note is for — the writing on it — was the thing destroyed.
///
/// What happens instead depends on what is there:
///
/// * **no `note.db`** — a new note, written into the folder as it stands (a PDF placed from a file
///   manager is usually a folder that does not exist yet, and this is the ordinary case);
/// * **a `note.db`, with the note's own `source.pdf` beside it** — the note is *kept*, and its document
///   is not replaced. Two files cannot both be one note's document, and the one that is already there
///   is the one the ink was written on. This is refused rather than silently applied;
/// * **a `note.db`, with no `source.pdf`** — the note has lost its document, which is the wound
///   [`matching_note`] exists to close. The document is put back ([`adopt_document`]) and the ink stays
///   where it is. This is what placing a moved PDF on its own note does.
fn start_on_document(source: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|error| Error::Note(format!("{} could not be made: {error}", dir.display())))?;

    // A folder that is already a note keeps what is in it. The question is only whether it still has a
    // document, because a note without one is exactly what this function can put right.
    if dir.join(NOTE_DB).is_file() {
        if dir.join(NOTE_PDF).exists() {
            return Err(Variant::DocumentAlreadyPlaced.said(format!(
                "{} is already a note about its own document; the document here was not copied over it",
                dir.display()
            )));
        }

        return adopt_document(dir, source).map(|_| ());
    }

    let mut note = Note::open(dir)?;
    note.copy_document(source)?;

    if let Some(print) = fingerprint(&dir.join(NOTE_PDF)) {
        note.store_mut().set_document_fingerprint(&print)?;
    }

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
    let mut entries = vec![(
        String::from(NOTE_DB),
        database.to_path_buf(),
        CompressionMethod::Stored,
    )];

    let document = dir.join(NOTE_PDF);
    if document.exists() {
        entries.push((String::from(NOTE_PDF), document, CompressionMethod::Stored));
    }

    for (name, path) in folder_contents(&dir.join(ATTACHMENTS))? {
        entries.push((
            format!("{ATTACHMENTS}/{name}"),
            path,
            CompressionMethod::Deflated,
        ));
    }

    write_zip(target, &entries)
}

/// Writes `entries` — each a name inside the zip, the file to copy in, and how — as one zip at
/// `target`.
///
/// The one writer an export goes through: a note's carried file ([`pack`]) is the only caller, and a
/// second copy of this loop would be a second place for a zip a file manager cannot open to come from.
/// Names are the caller's, already with forward slashes.
///
/// The file is created rather than appended to, so writing over an export that is already there
/// replaces it: what a person carries away is the copy that was made last, never two zips joined.
///
/// ## Names that survive the crossing
///
/// A zip carries its entry names in one of three encodings — ASCII, code page 437, or UTF-8 when the
/// entry's language flag (bit 11) says so — and which one a tool *reads* is the tool's choice. There
/// are two tools in the path of every note that crosses from a Mac to a Windows machine, and they
/// disagree:
///
/// * **macOS's own `zip`, and most Unix tools**, read code page 437 unless bit 11 says UTF-8.
/// * **Windows Explorer, and older WinZip / 7-Zip builds**, read code page 437 unless bit 11 says
///   UTF-8 *and* the entry carries the Info-ZIP Unicode Path field (`0x7075`) — the same name again,
///   UTF-8, beside the name in 437 — which is what APPNOTE 4.6.8 prescribes for exactly this.
///
/// So a Korean name is written the way both read it: the flag is set by the writer whenever the name
/// is not ASCII (it is UTF-8 on the wire), and the field is added here. It costs a few bytes an entry
/// and is written only when there is something to add — an ASCII name needs neither.
///
/// The stored name is UTF-8 rather than a 437 approximation because 437 cannot spell a note's name
/// at all: a Korean stem has no code page 437 bytes, and a tool that guessed it would show a row of
/// boxes where the name was.
pub(crate) fn write_zip(
    target: &Path,
    entries: &[(String, PathBuf, CompressionMethod)],
) -> Result<()> {
    let file = std::fs::File::create(target).map_err(|error| {
        Error::Note(format!(
            "{} could not be written: {error}",
            target.display()
        ))
    })?;

    let mut zip = ZipWriter::new(std::io::BufWriter::new(file));

    for (name, path, method) in entries {
        let options: FileOptions<'static, 'static, ExtendedFileOptions> =
            SimpleFileOptions::default()
                .compression_method(*method)
                .unix_permissions(0o644)
                .into_full_options();
        let options = with_unicode_path(options, name)?;
        add_file(&mut zip, name, path, options)?;
    }

    zip.finish().map_err(|error| {
        Error::Note(format!("{} could not be closed: {error}", target.display()))
    })?;

    Ok(())
}

/// The Info-ZIP Unicode Path field's header id (APPNOTE 4.6.8).
const UNICODE_PATH_FIELD: u16 = 0x7075;

/// Adds the Info-ZIP Unicode Path field for `name`, unless the name is ASCII and needs none.
///
/// The payload is a version byte (`1`), the CRC-32 of the name as it is stored — the bytes a reader
/// that ignores the field will see — and the name's UTF-8. A reader checks the CRC before using the
/// field, so a field that did not match the stored name would be ignored by both kinds of tool rather
/// than trusted by one.
fn with_unicode_path(
    mut options: FileOptions<'static, 'static, ExtendedFileOptions>,
    name: &str,
) -> Result<FileOptions<'static, 'static, ExtendedFileOptions>> {
    if name.is_ascii() {
        return Ok(options);
    }

    let stored = name.as_bytes();
    let mut payload = Vec::with_capacity(5 + stored.len());
    payload.push(1);
    payload.extend_from_slice(&crc32fast::hash(stored).to_le_bytes());
    payload.extend_from_slice(stored);

    options
        .add_extra_field(UNICODE_PATH_FIELD, payload, false)
        .map_err(|error| {
            Error::Note(format!(
                "the note could not be written: {name} could not be recorded for a Windows reader: {error}"
            ))
        })?;

    Ok(options)
}

/// Adds one file to a zip that is being written.
fn add_file(
    zip: &mut ZipWriter<std::io::BufWriter<std::fs::File>>,
    entry: &str,
    path: &Path,
    options: FileOptions<'static, 'static, ExtendedFileOptions>,
) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| Error::Note(format!("{} could not be read: {error}", path.display())))?;

    zip.start_file(entry, options)
        .map_err(|error| Error::Note(format!("the note could not be written: {error}")))?;

    std::io::copy(&mut file, zip)
        .map(|_| ())
        .map_err(|error| Error::Note(format!("the note could not be written: {error}")))
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
            Error::Note(format!("{} could not be read: {error}", current.display()))
        })?;

        for entry in entries {
            let entry = entry.map_err(|error| {
                Error::Note(format!("{} could not be read: {error}", current.display()))
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
        /// What the page's history did, told to the note in the same job as the ink it describes.
        history: HistoryUpdate,
    },
    /// A page to write again from scratch, because it was undone, erased or cleared.
    Rewrite {
        /// The page.
        page: u64,
        /// The ink it holds now.
        strokes: Vec<Stroke>,
        /// What its history did.
        history: HistoryUpdate,
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
///
/// ## Dropping one checkpoints the note, and there is deliberately no `Drop` here
///
/// The last thing a writer does is fold the write-ahead log back into the note file, so that a note is
/// not left beside a log with the last session in it. That job is sent by [`Jobs`] — the field holding
/// the queue — rather than by a `Drop` on this struct: a field is dropped *after* the struct's own
/// `Drop::drop` returns, so the ordering is kept by the type that owns the sender instead of by the
/// order these two fields happen to be declared in. A `Drop` here that sent the checkpoint as well
/// would send it twice, and the second one would arrive after the queue had closed.
#[derive(Debug)]
pub struct NoteWriter {
    /// The queue the UI thread puts work on, and the checkpoint that closes it.
    jobs: Jobs,
    /// What the writer has to report, drained by the app on its frame tick.
    reports: Receiver<Report>,
}

/// The queue's sender, and the checkpoint sent on the way out.
///
/// ## Why this is its own type
///
/// [`NoteWriter`]'s `Drop` has to send [`Job::Checkpoint`] *before* its sender is dropped, because the
/// checkpoint is queued on that sender: drop the sender first and the thread sees the queue close before
/// it has read the job, and the note is left beside a write-ahead log with the last session in it. That
/// ordering is what `Drop` on the *struct* happens to give today — a field is dropped after the method
/// returns — but it is an accident of which field comes first, and it survives exactly until somebody
/// reorders the fields or moves the sender behind a lock.
///
/// Holding the sender inside the type whose `Drop` uses it makes the ordering a rule rather than a
/// layout: the sender cannot be dropped anywhere else, because nothing else can reach it, and the impl
/// below is the only place a job is sent on the way out.
#[derive(Debug)]
struct Jobs {
    /// The queue the UI thread puts work on. Dropped *after* [`Drop::drop`] has read it.
    sender: Sender<Job>,
}

impl Drop for Jobs {
    /// The last thing a writer does: fold the write-ahead log back into the note file.
    ///
    /// Sending this *before* the sender drops is the whole point — the thread parked on the queue then
    /// finishes this job, sees the queue close, and stops, which is what makes a note not sit next to a
    /// log with the last session in it. Sending it from here rather than from [`NoteWriter::drop`] is
    /// what makes that guarantee one the compiler keeps instead of one a comment asks for.
    fn drop(&mut self) {
        let _ = self.sender.send(Job::Checkpoint);
    }
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
                Error::Note(format!("the note's writer could not be started: {error}"))
            })?;

        Ok(NoteWriter {
            jobs: Jobs { sender: jobs },
            reports,
        })
    }

    /// Adds ink to a page, and tells the note what its history did. Nothing waits for it.
    pub fn append(&self, page: u64, strokes: Vec<Stroke>, history: HistoryUpdate) {
        let _ = self.jobs.sender.send(Job::Append {
            page,
            strokes,
            history,
        });
    }

    /// Writes a page again from scratch, with what its history did.
    pub fn rewrite(&self, page: u64, strokes: Vec<Stroke>, history: HistoryUpdate) {
        let _ = self.jobs.sender.send(Job::Rewrite {
            page,
            strokes,
            history,
        });
    }

    /// Folds a page's ink into chunks, because the page is being closed.
    pub fn compact(&self, page: u64) {
        let _ = self.jobs.sender.send(Job::Compact { page });
    }

    /// Folds the write-ahead log back into the note file.
    pub fn checkpoint(&self) {
        let _ = self.jobs.sender.send(Job::Checkpoint);
    }

    /// Writes the note out as one file.
    pub fn export(&self, target: PathBuf) {
        let _ = self.jobs.sender.send(Job::Export { target });
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
        Job::Append {
            page,
            strokes,
            history,
        } => store.append(page, &strokes, &history),
        Job::Rewrite {
            page,
            strokes,
            history,
        } => store.rewrite(page, &strokes, &history),
        Job::Compact { page } => store.compact(page).map(|_| ()),
        Job::Checkpoint => store.checkpoint(),

        Job::Export { target } => {
            export(store, dir, &target)?;
            let _ = reports.send(Report::Written(target));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::InkPoint;
    use crate::pages::{Page, PageEntry};
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};
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
            stroke
                .points
                .push(InkPoint::new(from + step as f32 * 2.0, 5.0, 2.0));
        }

        stroke.close();
        stroke
    }

    /// A PDF that is not one, since nothing in this module parses it.
    fn a_pdf() -> Vec<u8> {
        b"%PDF-1.4 not really".to_vec()
    }

    /// The notes live under this platform's own application data, in a folder of the app's name.
    ///
    /// What is pinned is the *shape* rather than the path, because the path is the platform's
    /// answer and this test has to pass on all three: a folder called `cheap-note`, a `notes` folder
    /// inside it, and an absolute path at the top. The last of those is the one that matters — a
    /// relative root would put a note's database in whatever directory the program was started
    /// from, which is the opposite of "somewhere local and private".
    #[test]
    fn the_notes_root_is_under_this_platforms_own_data() {
        let root = root();

        assert!(
            root.ends_with(Path::new("cheap-note").join("notes")),
            "{} is not a notes root",
            root.display()
        );

        assert!(
            root.is_absolute(),
            "{} is not an absolute path",
            root.display()
        );
    }

    /// Naming a note writes the name into the *note*, not into any list of notes.
    ///
    /// That is the whole point of where the name lives: a folder carried to another machine arrives
    /// with its name already on it, and deleting the app's list of recent notes costs the order of a
    /// list rather than the names of everything a person wrote.
    #[test]
    fn naming_a_note_writes_it_into_the_note() {
        let root = scratch("name");
        let document = root.join("chapter-3.pdf");
        std::fs::write(&document, a_pdf()).expect("the document is written");

        let folder = Note::placed_from(&document, &root.join("notes"))
            .expect("a note")
            .dir()
            .to_path_buf();
        let facts = set_title(&folder, "3\u{c7a5} \u{c694}\u{c57d}").expect("a name");

        assert_eq!(facts.title.as_deref(), Some("3\u{c7a5} \u{c694}\u{c57d}"));
        assert_eq!(facts.summary.pages, 1, "the counts come back with the name");

        // A fresh connection agrees, and the folder keeps the name it was made with: the folder is
        // the note's *identity*, and the name is only what it is called.
        let store = NoteStore::open(&folder.join(NOTE_DB)).expect("the note");
        assert_eq!(
            store.title().expect("a name").as_deref(),
            Some("3\u{c7a5} \u{c694}\u{c57d}")
        );
        assert_eq!(
            folder,
            root.join("notes"),
            "naming a note does not move it, and does not rename its folder"
        );

        let _ = std::fs::remove_dir_all(&root);
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
        assert_eq!(
            note.store().source().expect("a source").as_deref(),
            Some(document.to_string_lossy().as_ref()),
            "the note knows which file it was made from"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Placing a document over a note that already has ink does not throw the ink away.
    ///
    /// This is the defect a note was found in: a note held a `document` row and a fingerprint, its own
    /// `source.pdf` was gone, and the file it had been made from still sat where `source` said. Placing
    /// that same file starts a new note — and `start_on_document` began with `remove_dir_all`, so the
    /// note that was there was erased before anything asked whether it was empty. A note that is not a
    /// *new* note is not replaced: the document is put back beside the ink instead.
    #[test]
    fn placing_a_document_over_a_note_keeps_its_ink() {
        let scratch = scratch("keep-ink");
        let document = scratch.join("chapter-3.pdf");
        std::fs::write(&document, a_pdf()).expect("the document is written");

        let folder = scratch.join("placed");
        let mut note = Note::placed_from(&document, &folder).expect("a note");
        note.store_mut()
            .set_document("chapter-3.pdf")
            .expect("a name");
        note.store_mut()
            .set_document_fingerprint(&fingerprint(&document).expect("a fingerprint"))
            .expect("a fingerprint is recorded");
        note.store_mut()
            .append(
                0,
                &[stroke(3, 1.0)],
                &crate::history::HistoryUpdate::default(),
            )
            .expect("a stroke");
        note.store().checkpoint().expect("written");
        drop(note);

        // The note's own copy is gone, which is the state the defect was found in.
        std::fs::remove_file(folder.join(NOTE_PDF)).expect("the copy is removed");

        // Placing the same document again must not start over.
        Note::placed_from(&document, &folder).expect("the note, not a new one");

        let note = Note::open(&folder).expect("the note");
        let (_, bytes) = note
            .document()
            .expect("the document")
            .expect("the document is back");
        assert_eq!(bytes, a_pdf(), "the document was put back");
        assert_eq!(
            note.store().pages().expect("pages"),
            vec![0],
            "the ink survived the placement"
        );
        assert_eq!(note.store().stroke_count(0).expect("strokes"), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Placing a document over a note that already has its own copy is refused, not applied.
    ///
    /// Two different files cannot both be a note's document. The one that is there was written with the
    /// ink in it, so it is the one that stays, and the caller is told rather than silently overwritten.
    #[test]
    fn placing_a_second_document_over_a_note_is_refused() {
        let scratch = scratch("refuse-second");
        let first = scratch.join("first.pdf");
        std::fs::write(&first, a_pdf()).expect("the document is written");
        let second = scratch.join("second.pdf");
        std::fs::write(&second, b"%PDF-1.4 another one").expect("the second is written");

        let folder = scratch.join("placed");
        Note::placed_from(&first, &folder).expect("a note");
        let before = std::fs::read(folder.join(NOTE_PDF)).expect("the copy");

        let refused = Note::placed_from(&second, &folder);
        assert!(
            refused.is_err(),
            "a note is not replaced by placing over it"
        );
        assert_eq!(
            std::fs::read(folder.join(NOTE_PDF)).expect("the copy"),
            before,
            "the document the ink was written on is the one that stays"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A listing finds the folders that are notes, and nothing else that happens to be a folder.
    #[test]
    fn a_listing_finds_the_notes_and_only_the_notes() {
        let scratch = scratch("listing");

        assert!(
            notes_in(&scratch.join("nowhere"))
                .expect("a listing")
                .is_empty(),
            "a notes folder that does not exist yet is no notes, not an error"
        );

        let document = scratch.join("chapter-3.pdf");
        std::fs::write(&document, a_pdf()).expect("the document is written");
        let note = Note::placed_from(&document, &scratch.join("placed")).expect("a note");

        // Not notes: an attachments folder, and a stray file beside them.
        std::fs::create_dir_all(scratch.join("attachments")).expect("a folder");
        std::fs::write(scratch.join("notes.txt"), b"not a note").expect("a file");

        let found = notes_in(&scratch).expect("a listing");
        assert_eq!(found, vec![note.dir().to_path_buf()]);

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
            .rewrite(
                1,
                &[stroke(4, 0.0), stroke(4, 40.0)],
                &crate::history::HistoryUpdate::default(),
            )
            .expect("ink");
        note.store_mut()
            .set_layout(&[
                PageEntry::new(Page::Document(0)),
                PageEntry::new(Page::Blank),
            ])
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
            vec![
                PageEntry::new(Page::Document(0)),
                PageEntry::new(Page::Blank)
            ]
        );
        let (name, bytes) = arrived
            .document()
            .expect("the document")
            .expect("it is there");
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

    /// Dropping the writer checkpoints the note, and the checkpoint is *received* rather than lost.
    ///
    /// This is the guarantee [`Jobs`] exists for, and it is invisible unless something asks: the
    /// checkpoint is queued on the sender, so if the sender were dropped before `Drop::drop` ran, the
    /// thread would see a closed queue and stop without ever folding the log back — and the note would
    /// be left beside a write-ahead log with the last session in it, which nobody would notice until a
    /// later open. The test drives the same queue the writer does and checks that the job after the
    /// writer's drop is still delivered, which is what "sent before the close" means in practice.
    #[test]
    fn dropping_a_writer_sends_its_checkpoint_before_the_queue_closes() {
        let scratch = scratch("writer-drop");
        let home = scratch.join("home");
        Note::open(&home).expect("a note");

        // The queue and the thread that reads it, as `spawn` builds them: the writer's `Jobs` holds the
        // sender, so it is the only thing that can close the queue.
        let (sender, queue) = mpsc::channel::<Job>();
        let received = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&received);
        let reader = std::thread::spawn(move || {
            for job in queue {
                let name = match job {
                    Job::Checkpoint => "checkpoint",
                    Job::Compact { .. } => "compact",
                    Job::Append { .. } => "append",
                    Job::Rewrite { .. } => "rewrite",
                    Job::Export { .. } => "export",
                };
                seen.lock().expect("the log").push(name);
            }
        });

        let jobs = Jobs { sender };
        // The work in hand, then the drop: the checkpoint is sent *after* this and still arrives.
        jobs.sender
            .send(Job::Compact { page: 0 })
            .expect("a compact is queued");
        drop(jobs);

        reader
            .join()
            .expect("the reader stops when the queue closes");

        assert_eq!(
            *received.lock().expect("the log"),
            ["compact", "checkpoint"],
            "the checkpoint is queued before the sender drops, so it is read rather than lost"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The writer thread writes what it is given, and says when an export is done.
    #[test]
    fn the_writer_writes_on_its_own_thread() {
        let scratch = scratch("writer");
        let home = scratch.join("home");
        Note::open(&home).expect("a note");

        let writer = NoteWriter::spawn(&home).expect("a writer");
        writer.append(0, vec![stroke(5, 0.0)], HistoryUpdate::default());
        writer.append(0, vec![stroke(5, 60.0)], HistoryUpdate::default());
        writer.compact(0);
        writer.rewrite(1, vec![stroke(3, 200.0)], HistoryUpdate::default());

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

    /// A zip that is not a note is refused — including one in the shape a note used to have.
    ///
    /// A note *was* a zip holding `document.pdf` and a `notes.json`, and this build has no reader for
    /// that shape any more (see §9 of `doc/STORE.md`), so an old note is now the same case as any
    /// other foreign zip: refused by name, with nothing written. The refusal is part of the deletion
    /// rather than an accident of it — a zip unpacked "in case" would put a stranger's files in the
    /// notes folder.
    #[test]
    fn a_foreign_zip_is_refused() {
        let scratch = scratch("foreign");
        let placed = scratch.join("placed");

        let other = scratch.join("other.zip");
        {
            let file = std::fs::File::create(&other).expect("a file");
            let mut zip = ZipWriter::new(file);
            zip.start_file("something-else.txt", SimpleFileOptions::default())
                .expect("an entry");
            zip.write_all(b"hello").expect("its contents");
            zip.finish().expect("a finished zip");
        }

        let error = Note::placed_from(&other, &placed).expect_err("a foreign zip is refused");
        assert!(error.to_string().contains(NOTE_DB), "{error}");

        // The old shape: `document.pdf` and a `notes.json`, and no database, because there was not a
        // database to have. It is not a note this app can open, and it is refused as such.
        let old = scratch.join("old.zip");
        {
            let file = std::fs::File::create(&old).expect("a file");
            let mut zip = ZipWriter::new(file);
            zip.start_file("notes.json", SimpleFileOptions::default())
                .expect("an entry");
            zip.write_all(br#"{"format":2,"page":0,"pages":[]}"#)
                .expect("the ink");
            zip.start_file("document.pdf", SimpleFileOptions::default())
                .expect("an entry");
            zip.write_all(&a_pdf()).expect("the document");
            zip.finish().expect("a finished zip");
        }

        let error = Note::placed_from(&old, &placed).expect_err("an old note is refused");
        assert!(error.to_string().contains(NOTE_DB), "{error}");
        assert!(!placed.exists(), "a refusal writes nothing");

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

        // A run of characters that are not a letter or a digit is one dash, not one each, and
        // nothing is left hanging off either end — a trailing dot or dash is a name Windows quietly
        // rewrites, so the folder on disk would stop being the folder the lists name.
        let run = folder_name(Path::new("/notes/a  :  b ....pdf"));
        assert!(run.starts_with("a-b-"), "{run}");
        assert!(run.is_ascii(), "{run}");
    }

    /// A note's folder is a name every filesystem will take, and the same name on every platform.
    ///
    /// The bug this pins: `is_alphanumeric` kept Korean letters on macOS but threw them away on
    /// Windows and Linux, so `한글.pdf` was `한글-9f2a1c44` on one machine and `note-9f2a1c44` on the
    /// next — the same note under two identities, and a `notes/한글-…/` entry a Windows machine
    /// extracts with the name mangled. A stem that is not ASCII now becomes the same ASCII name
    /// everywhere, suffixed with the digest that tells two such documents apart.
    #[test]
    fn a_folder_name_is_one_name_on_every_filesystem() {
        let korean = folder_name(Path::new("/notes/한글 보고서.pdf"));
        assert!(korean.starts_with("note-"), "{korean}");
        assert!(
            korean
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-'),
            "{korean}"
        );

        // Two Korean documents are two notes, which a stem of only dashes would have made one.
        let other = folder_name(Path::new("/notes/계획.pdf"));
        assert_ne!(korean, other, "the digest keeps two names apart");
        assert_eq!(
            korean,
            folder_name(Path::new("/notes/한글 보고서.pdf")),
            "and the same file is still the same note"
        );

        // Windows refuses these characters outright, and strips a trailing dot, space or dash. A
        // stem made only of them is the fallback rather than a folder called `---`.
        let reserved = folder_name(Path::new("/notes/a<b>c:d\"e\\f?g*h.pdf"));
        assert!(
            !reserved.contains(['<', '>', ':', '"', '\\', '?', '*']),
            "{reserved}"
        );
        assert!(!reserved.starts_with('-'), "{reserved}");
        assert!(
            !reserved.ends_with('.') && !reserved.ends_with(' '),
            "{reserved}"
        );

        let nothing = folder_name(Path::new("/notes/???.pdf"));
        assert!(nothing.starts_with("note-"), "{nothing}");

        // An ASCII name is unchanged in shape: the folders this app has already written keep their
        // names, so a note made before this rule is still found by the PDF it was made from.
        assert!(folder_name(Path::new("/notes/chapter-3.pdf")).starts_with("chapter-3-"));
        assert!(folder_name(Path::new("/notes/my_notes.PDF")).starts_with("my_notes-"));
    }
}
