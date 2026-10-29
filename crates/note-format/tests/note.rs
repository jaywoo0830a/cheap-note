//! A note as a folder and as the one file it is carried in, from the outside.
//!
//! This is the container: three names inside a folder, and a zip holding the same three. What a second
//! implementation has to get identical is the *names* (`note.db`, `source.pdf`, `attachments/`), the
//! zip's entry names, and the refusals — a zip with no note in it, and an entry that would write
//! outside the note. The last of those is the one way a note from somebody else can reach into a disk.

use std::path::{Path, PathBuf};

use note_format::history::HistoryUpdate;
use note_format::note::{self, Note, NoteWriter, Report, ATTACHMENTS, NOTE_DB, NOTE_PDF};

mod common;

use common::{a_pdf, scratch, stroke};

/// A PDF to write on: nothing in this crate parses one.
fn a_document(dir: &Path) -> PathBuf {
    let path = dir.join("chapter-3.pdf");
    std::fs::write(&path, a_pdf()).expect("a document");
    path
}

/// A zip at `target` holding exactly these entries, written the way a file manager would: names with
/// forward slashes and entries stored rather than deflated.
fn zip_of(target: &Path, entries: &[(&str, &[u8])]) {
    use std::io::Write as _;

    let file = std::fs::File::create(target).expect("a zip");
    let mut zip = zip::ZipWriter::new(file);

    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("an entry");
        zip.write_all(bytes).expect("the entry's bytes");
    }

    zip.finish().expect("a finished zip");
}

/// One batch of ink, as the app would hand it over.
fn plain(count: usize) -> HistoryUpdate {
    common::update(Vec::new(), 0, count)
}

/// A note is a folder with a database in it, and the names inside it are the format's.
#[test]
fn a_note_is_a_folder_with_a_database_in_it() {
    let root = scratch("folder");
    let dir = root.join("chapter-3-9f2a1c44");

    let note = Note::open(&dir).expect("a note");
    assert_eq!(note.dir(), dir);
    assert!(dir.join(NOTE_DB).is_file(), "the store is {NOTE_DB}");
    assert!(
        !dir.join(NOTE_PDF).exists(),
        "a note need not have a document"
    );

    // The three names are public constants because they are the format, not an implementation detail.
    assert_eq!(NOTE_DB, "note.db");
    assert_eq!(NOTE_PDF, "source.pdf");
    assert_eq!(ATTACHMENTS, "attachments");
}

/// A document is copied into the note and keeps the name it arrived with.
///
/// The bytes inside the note are `source.pdf` whatever the file was called, and the *name* is a `meta`
/// row — which is what makes a note come back about the document it is about rather than about a file
/// called `source.pdf`.
#[test]
fn a_document_is_copied_in_and_keeps_its_name() {
    let root = scratch("document");
    let dir = root.join("note");
    let document = a_document(&root);

    let mut note = Note::open(&dir).expect("a note");
    let name = note.copy_document(&document).expect("the document copied");

    assert_eq!(name, "chapter-3.pdf");
    assert!(dir.join(NOTE_PDF).is_file(), "the bytes are {NOTE_PDF}");
    assert_eq!(
        std::fs::read(dir.join(NOTE_PDF)).expect("the bytes"),
        a_pdf()
    );

    let (recorded, bytes) = note.document().expect("a document").expect("one");
    assert_eq!(recorded, "chapter-3.pdf", "the name travels with the note");
    assert_eq!(bytes, a_pdf());

    // A document written straight in keeps the name it is given, which is what an import of a note
    // from another machine does.
    note.put_document("plate-7.pdf", b"%PDF-1.4 plate")
        .expect("a document written in");
    let (recorded, bytes) = note.document().expect("a document").expect("one");
    assert_eq!(recorded, "plate-7.pdf");
    assert_eq!(bytes, b"%PDF-1.4 plate");
}

/// A note's folder is named after the file it was made from, and the name is stable.
///
/// The digest is what makes importing the same file twice land in the same place rather than in a
/// second copy of the note — so the same path has to give the same name, two paths have to differ, and
/// a name has to be usable on a filesystem rather than whatever the document was called.
#[test]
fn a_notes_folder_is_named_after_its_document() {
    let one = note::folder_name(Path::new("/notes/chapter-3.pdf"));
    let again = note::folder_name(Path::new("/notes/chapter-3.pdf"));
    let other = note::folder_name(Path::new("/elsewhere/chapter-3.pdf"));

    assert_eq!(one, again, "the same file is the same note");
    assert_ne!(
        one, other,
        "the same name in another folder is another note"
    );
    assert!(one.starts_with("chapter-3-"), "{one}");

    // Characters a filesystem would rather not have become dashes, and nothing is left dangling.
    let awkward = note::folder_name(Path::new("/notes/a b:c*d?.pdf"));
    assert!(awkward.starts_with("a-b-c-d-"), "{awkward}");
    assert!(!awkward.contains(' '));
    assert!(!awkward.contains(':'));
}

/// The notes root is under this platform's own application data, and the app's name is in it.
#[test]
fn the_notes_root_is_somewhere_local_and_private() {
    let root = note::root();

    assert!(root.is_absolute(), "{} is relative", root.display());
    assert!(root.ends_with(Path::new("cheap-note").join("notes")));
}

/// A note exported as one file, and placed on another machine, is the same note.
///
/// This is the whole point of the container: the ink, the document and the note's own facts travel in
/// one zip, and unpacking it somewhere else produces a note that opens with everything it had.
#[test]
fn a_note_carried_as_one_file_arrives_whole() {
    let root = scratch("carry");
    let before = root.join("before");
    let after = root.join("after");
    let carried = root.join("chapter-3.zip");

    let mut note = Note::open(&before).expect("a note");
    let document = a_document(&root);
    note.copy_document(&document).expect("the document in");

    let written = vec![
        stroke(5, 10.0, 10.0, 0x1B_1B_1F),
        stroke(3, 40.0, 10.0, 0xCC_22_22),
    ];
    note.store_mut()
        .append(0, &written, &plain(written.len()))
        .expect("ink");
    note.store_mut().set_title("chapter three").expect("a name");

    note::export(note.store(), note.dir(), &carried).expect("the note written out");
    assert!(carried.is_file(), "an export is one file");

    let placed = Note::placed_from(&carried, &after).expect("the note unpacked");

    assert_eq!(placed.dir(), after);
    let (name, bytes) = placed.document().expect("a document").expect("one");
    assert_eq!(name, "chapter-3.pdf");
    assert_eq!(bytes, a_pdf(), "the document travelled byte for byte");

    let read = placed.store().load(0).expect("the ink");
    assert_eq!(read.len(), written.len());
    for (before, after) in written.iter().zip(&read) {
        assert_eq!(after.points, before.points);
    }
    assert_eq!(
        placed.store().title().expect("a name").as_deref(),
        Some("chapter three"),
        "a note arrives already named"
    );
}

/// A zip with no note in it is refused *before* anything is written.
///
/// Unpacking first would leave a folder of somebody else's files under the notes root, and a
/// half-imported note is a worse thing to explain than a refusal.
#[test]
fn a_zip_that_holds_no_note_is_refused_and_writes_nothing() {
    let root = scratch("not-a-note");
    let source = root.join("someone-elses.zip");
    let destination = root.join("destination");

    zip_of(&source, &[("hello.txt", b"not a note")]);

    let refused = Note::placed_from(&source, &destination).expect_err("a zip that is not a note");

    assert!(
        refused.to_string().contains(NOTE_DB),
        "{refused} does not say what was missing"
    );
    assert!(
        !destination.exists(),
        "a refused import left a folder behind"
    );
}

/// An entry that points outside the note is refused: that is the one way a note can write to a disk.
///
/// The check is the zip's own `enclosed_name`, and it is why unpacking cannot trust the entry list of a
/// file somebody else made.
#[test]
fn an_entry_that_points_outside_the_note_is_refused() {
    let root = scratch("zip-slip");
    let source = root.join("hostile.zip");
    let destination = root.join("destination");
    let escape = root.join("escape.txt");

    // A note that unpacks, plus one entry aimed at the folder above it.
    let mut entries: Vec<(&str, &[u8])> = vec![
        (NOTE_DB, b"not really a database"),
        ("../../escape.txt", b"written outside the note"),
    ];
    entries.sort();
    zip_of(&source, &entries);

    let refused = Note::placed_from(&source, &destination).expect_err("an entry pointing outside");

    assert!(
        refused.to_string().contains("outside"),
        "{refused} does not say what was wrong"
    );
    assert!(!escape.exists(), "a note wrote outside its own folder");
}

/// A folder is opened where it stands rather than copied — which is what a note in a synced or
/// hand-kept directory is.
#[test]
fn a_folder_is_opened_where_it_stands() {
    let root = scratch("in-place");
    let dir = root.join("my-notes");
    Note::open(&dir).expect("a note");

    let opened = Note::placed_from(&dir, &root.join("never-used")).expect("the folder opened");

    assert_eq!(opened.dir(), dir.as_path());
    assert!(!root.join("never-used").exists(), "nothing was copied");
}

/// A PDF starts a note written on that document, and the note remembers where it came from.
#[test]
fn a_pdf_starts_a_note_on_that_document() {
    let root = scratch("from-pdf");
    let document = a_document(&root);
    let dir = root.join("note");

    let note = Note::placed_from(&document, &dir).expect("a note on a PDF");

    let (name, bytes) = note.document().expect("a document").expect("one");
    assert_eq!(name, "chapter-3.pdf");
    assert_eq!(bytes, a_pdf());

    assert!(
        note.store()
            .source()
            .expect("a source")
            .expect("a source")
            .ends_with("chapter-3.pdf"),
        "the note remembers the file it was made from"
    );

    // Anything else is neither a note nor a document, and says so.
    let other = root.join("notes.txt");
    std::fs::write(&other, b"hello").expect("a file");
    assert!(Note::placed_from(&other, &root.join("other")).is_err());
}

/// Waits for the writer thread to say something, and gives up rather than hanging a test run.
fn wait_for(writer: &NoteWriter, wanted: impl Fn(&Report) -> bool) -> Vec<Report> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut seen = Vec::new();

    while std::time::Instant::now() < deadline {
        seen.extend(writer.drain());

        if seen.iter().any(&wanted) {
            return seen;
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    panic!("the writer said nothing: {seen:?}");
}

/// The writer thread takes ink, folds the log, writes the note out, and says when it is done.
///
/// This is the queue the app uses, and the only part of the format that runs on a thread of its own:
/// the pen hands over a batch and forgets about it, and everything that could block — a commit, a
/// compaction, a `VACUUM INTO` of the whole note — happens away from the frame.
#[test]
fn the_writer_thread_writes_the_ink_and_reports_an_export() {
    let root = scratch("writer");
    let dir = root.join("note");
    let target = root.join("carried.zip");

    // A writer opens a note that exists: the folder is the app's to make (see `Note::open`), because
    // that is the call that also stamps a new database.
    Note::open(&dir).expect("a note");

    let writer = NoteWriter::spawn(&dir).expect("a writer");
    let written = vec![
        stroke(6, 10.0, 10.0, 0x1B_1B_1F),
        stroke(4, 40.0, 10.0, 0x22_66_CC),
    ];

    writer.append(0, written.clone(), plain(written.len()));
    writer.compact(0);
    writer.checkpoint();
    writer.export(target.clone());

    let reports = wait_for(&writer, |report| matches!(report, Report::Written(_)));

    assert!(
        reports
            .iter()
            .any(|report| matches!(report, Report::Written(path) if path == &target)),
        "the export was not reported: {reports:?}"
    );
    assert!(
        !reports
            .iter()
            .any(|report| matches!(report, Report::Failed(_))),
        "the writer failed: {reports:?}"
    );
    assert!(target.is_file(), "the export is a file");

    // And the ink reached the note, on the writer's own connection.
    let note = Note::open(&dir).expect("the note");
    let read = note.store().load(0).expect("the ink");
    assert_eq!(read.len(), written.len());
    for (before, after) in written.iter().zip(&read) {
        assert_eq!(after.points, before.points);
    }
}

/// A listing is every folder with a note in it, and nothing else.
///
/// The rule is the presence of `note.db`: a folder without one is an attachments folder, a leftover
/// directory, or somebody's files — and offering it as something to open would be a lie.
#[test]
fn a_listing_finds_the_notes_and_only_the_notes() {
    let root = scratch("listing");

    Note::open(&root.join("one")).expect("a note");
    Note::open(&root.join("two")).expect("a note");
    std::fs::create_dir_all(root.join("not-a-note").join(ATTACHMENTS)).expect("a folder");
    std::fs::write(root.join("loose.txt"), b"not a note").expect("a file");

    let found = note::notes_in(&root).expect("a listing");

    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].ends_with("one"), "the listing is in a fixed order");
    assert!(found[1].ends_with("two"));

    // A root that does not exist yet is *no notes*, which is what a first run looks like.
    assert!(note::notes_in(&root.join("missing"))
        .expect("no notes")
        .is_empty());
}
