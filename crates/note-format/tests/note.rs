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
    // The document is *mapped* rather than read into the heap: that is what lets closing the note give
    // the memory back, and it is the reason `MappedBytes` exists (see `src/mapped.rs`).
    #[cfg(any(unix, windows))]
    assert!(
        bytes.is_mapped(),
        "a note's document is mapped, not copied into the heap"
    );

    // A document written straight in keeps the name it is given, which is what an import of a note
    // from another machine does. The old mapping has to go first: Windows will not truncate a file
    // while a view of it is mapped (`ERROR_USER_MAPPED_FILE`), and the app drops its document view
    // before replacing one.
    drop(bytes);
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

/// The root cause, as a test: a document that has **moved** is still the same document.
///
/// This is the failure a person actually hit. A PDF opened from one folder made a note named after that
/// path; the file later turned up in `Downloads`; opening it there made a *second*, empty note, while the
/// first sat with no `source.pdf` and opened as a blank sheet. The ink and the document had been split
/// apart by nothing more than where the file happened to be.
#[test]
fn a_document_that_moved_leads_back_to_the_note_made_about_it() {
    let root = scratch("moved-document");
    let notes = root.join("notes");

    // The note as it was made: on a PDF in one folder, with ink on its first page.
    let first_home = root.join("sync-folder");
    std::fs::create_dir_all(&first_home).expect("a folder");
    let original = first_home.join("effective-rust.pdf");
    std::fs::write(&original, a_pdf()).expect("the document");

    let folder = notes.join(note::folder_name(&original));
    {
        let mut note = Note::placed_from(&original, &folder).expect("a note on the document");
        let ink = vec![stroke(6, 10.0, 20.0, 0xff00_00ff)];
        note.store_mut()
            .append(0, &ink, &plain(1))
            .expect("the ink");
        note.store().checkpoint().expect("a checkpoint");
    }

    // The document moves: same bytes, new path — what a person sees when a file lands in `Downloads`
    // rather than where they first opened it.
    let moved = root.join("Downloads");
    std::fs::create_dir_all(&moved).expect("a folder");
    let moved_to = moved.join("effective-rust.pdf");
    std::fs::copy(&original, &moved_to).expect("the move");
    std::fs::remove_file(&original).expect("the old file gone");

    // The new path would be a different folder name — the thing that used to split the note.
    let would_be = notes.join(note::folder_name(&moved_to));
    assert_ne!(
        would_be, folder,
        "a new path is a new folder name: this is exactly why the note was lost"
    );

    // But the moved file is recognised as the note's own document, and answers *that* folder — even
    // though the note still holds its own copy: the bytes are what it is about.
    let found = note::matching_note(&notes, &moved_to).expect("a search");
    assert_eq!(
        found,
        Some(folder.clone()),
        "the same bytes lead back to the note made about them"
    );

    // --- The failure as it actually happened: the note *lost* its copy of the document. (A restore, or a
    // backup taken after the original had gone, is how the folder ends up holding the ink without the
    // pages.) Opening the file from its new home now puts the document back, and leaves the ink alone.
    std::fs::remove_file(folder.join(NOTE_PDF)).expect("the document gone from the note");
    assert!(
        Note::open(&folder)
            .expect("the note")
            .document()
            .expect("a document")
            .is_none(),
        "the note can only open as a blank sheet before the repair"
    );

    let before = Note::open(&folder)
        .expect("the note")
        .store()
        .load(0)
        .expect("the ink before");
    note::adopt_document(&folder, &moved_to).expect("the document put back");
    let after = Note::open(&folder).expect("the note");

    assert_eq!(
        after.document().expect("a document").expect("one").0,
        "effective-rust.pdf",
        "the note is about the document again, by the name it has"
    );
    assert_eq!(
        after.store().load(0).expect("the ink after"),
        before,
        "and not one stroke moved"
    );
    assert_eq!(
        after.store().source().expect("a source"),
        Some(moved_to.to_string_lossy().replace('\\', "/")),
        "and it remembers where the file is now"
    );

    assert!(
        !would_be.exists(),
        "no second note was made under the new path's name"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A different document with the same name is **not** taken to be the same document.
///
/// A note with a fingerprint is matched by its bytes, not by what the file is called: two `chapter-3.pdf`s
/// from two books are two documents, and putting one on the note made about the other would be the
/// opposite of the repair above — the note would open with someone else's pages under its ink.
#[test]
fn a_different_document_with_the_same_name_is_not_the_same_document() {
    let root = scratch("different-document");

    let notes = root.join("notes");
    let one = root.join("book-one");
    std::fs::create_dir_all(&one).expect("a folder");
    let first = one.join("chapter-3.pdf");
    std::fs::write(&first, a_pdf()).expect("a document");
    let folder = notes.join(note::folder_name(&first));
    Note::placed_from(&first, &folder).expect("a note");

    // The document goes missing, and somebody opens a *different* chapter-3.pdf: same name, other bytes.
    std::fs::remove_file(folder.join(NOTE_PDF)).expect("the document gone");
    let two = root.join("book-two");
    std::fs::create_dir_all(&two).expect("a folder");
    let other = two.join("chapter-3.pdf");
    std::fs::write(&other, b"%PDF-1.4 not the same pages at all").expect("another document");

    assert_eq!(
        note::matching_note(&notes, &other).expect("a search"),
        None,
        "different bytes are a different document, however they are named"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A note that *has* its document is never given another one: a file that looks like it is not evidence
/// enough to replace the pages its ink was written on.
#[test]
fn a_note_that_has_its_document_is_not_adopted_again() {
    let root = scratch("adopt-refused");
    let notes = root.join("notes");

    let document = a_document(&root);
    let folder = notes.join(note::folder_name(&document));
    Note::placed_from(&document, &folder).expect("a note");

    let copy = root.join("copy.pdf");
    std::fs::copy(&document, &copy).expect("a copy");

    assert!(
        note::adopt_document(&folder, &copy).is_err(),
        "a note with its document is not given another"
    );
    assert_eq!(
        std::fs::read(folder.join(NOTE_PDF)).expect("the document"),
        a_pdf(),
        "and the one it has is the one it keeps"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Two notes, each with ink, ready to be selected between.
fn two_notes(root: &Path) -> (PathBuf, PathBuf) {
    let notes = root.join("notes");
    let first = notes.join("chapter-3-9f2a1c44");
    let second = notes.join("blank-1712345678901");

    let mut one = Note::open(&first).expect("a note");
    one.put_document("chapter-3.pdf", &a_pdf())
        .expect("a document");
    one.store_mut()
        .rewrite(
            1,
            &[
                stroke(4, 0.0, 10.0, 0x1B_1B_1F),
                stroke(4, 40.0, 10.0, 0xCC_22_22),
            ],
            &plain(2),
        )
        .expect("ink");
    one.store_mut().checkpoint().expect("a checkpoint");

    let mut other = Note::open(&second).expect("a note");
    other
        .store_mut()
        .rewrite(1, &[stroke(6, 10.0, 20.0, 0x1B_1B_1F)], &plain(1))
        .expect("ink");
    other.store_mut().checkpoint().expect("a checkpoint");

    (first, second)
}

/// A selection of several notes is one zip with `notes/<folder>/` in front of every entry, and every one
/// of them comes back whole — the ink, the document, and the folder name that is the note's identity.
#[test]
fn a_selection_of_several_notes_travels_as_one_zip() {
    let root = scratch("selection-many");
    let (first, second) = two_notes(&root);

    let carried = root.join("chosen.zip");
    let written = note::export_selection(&[first.clone(), second.clone()], &carried)
        .expect("the selection is written");
    assert_eq!(written.len(), 2, "both notes are in it");

    // The zip says what it holds, in the names it holds them under.
    let listed = note::read_selection(&carried).expect("a listing");
    assert!(listed.contains(&PathBuf::from("chapter-3-9f2a1c44")));
    assert!(listed.contains(&PathBuf::from("blank-1712345678901")));

    // And it unpacks into two note folders that are the two notes.
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("a notes folder");
    let placed = note::place_selection(&carried, &elsewhere).expect("the selection is placed");
    assert_eq!(placed.len(), 2);

    let arrived = Note::open(&elsewhere.join("chapter-3-9f2a1c44")).expect("the note");
    assert_eq!(arrived.store().load(1).expect("a page").len(), 2);
    assert!(elsewhere
        .join("chapter-3-9f2a1c44")
        .join(NOTE_PDF)
        .is_file());

    let blank = Note::open(&elsewhere.join("blank-1712345678901")).expect("the note");
    assert_eq!(blank.store().load(1).expect("a page").len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

/// The entry names inside a zip, in order.
fn entry_names(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).expect("a zip");
    let mut archive = zip::ZipArchive::new(file).expect("a zip archive");
    (0..archive.len())
        .map(|index| {
            archive
                .by_index(index)
                .expect("an entry")
                .name()
                .expect("a name")
                .to_string()
        })
        .collect()
}

/// A selection of one note is written *flat*, so it is the shape of an ordinary export: the same entry
/// names, and a file *Open* already reads.
#[test]
fn a_selection_of_one_is_an_ordinary_export() {
    let root = scratch("selection-one");
    let (first, second) = two_notes(&root);

    // The note with no document, so that "the same names" is the narrowest case as well as the widest.
    let selected = root.join("chosen.zip");
    note::export_selection(std::slice::from_ref(&second), &selected).expect("the selection");

    let single = root.join("saved.zip");
    let note = Note::open(&second).expect("the note");
    note::export(note.store(), note.dir(), &single).expect("an export");

    assert_eq!(
        entry_names(&selected),
        entry_names(&single),
        "one note is written flat, under the names an export has"
    );

    // A note with a document carries it under the same flat name too.
    let with_document = root.join("chosen-document.zip");
    note::export_selection(std::slice::from_ref(&first), &with_document).expect("the selection");
    assert!(
        entry_names(&with_document).contains(&String::from(NOTE_PDF)),
        "the document sits at the root, as it does in an export"
    );

    // And it opens the way a saved note does.
    let elsewhere = root.join("elsewhere");
    Note::placed_from(&selected, &elsewhere).expect("a selection of one opens as a note");
    assert!(elsewhere.join(NOTE_DB).is_file());

    let _ = std::fs::remove_dir_all(&root);
}

/// Restoring replaces the notes it names and leaves the ones it does not name alone: a selection is a
/// copy of *those* notes, not a statement about the others.
#[test]
fn a_selection_replaces_what_it_names_and_nothing_else() {
    let root = scratch("selection-restore");
    let (first, second) = two_notes(&root);
    let untouched_folder = root.join("notes").join("untouched-1712345678999");
    Note::open(&untouched_folder).expect("a third note");

    // A selection of two, so the file is the multi-note shape; the third note is not in it.
    let carried = root.join("chosen.zip");
    note::export_selection(&[first.clone(), second.clone()], &carried).expect("the selection");

    // Both notes in the file change after the snapshot was taken; so does the one that is not in it.
    let mut one = Note::open(&first).expect("the note");
    one.store_mut()
        .rewrite(1, &[stroke(3, 0.0, 30.0, 0x22_66_CC)], &plain(1))
        .expect("ink");
    one.store_mut().checkpoint().expect("a checkpoint");

    let mut other = Note::open(&second).expect("the note");
    other.store_mut().set_title("kept").expect("a name");
    other.store_mut().checkpoint().expect("a checkpoint");

    let mut third = Note::open(&untouched_folder).expect("the note");
    third
        .store_mut()
        .rewrite(1, &[stroke(2, 50.0, 50.0, 0x1B_1B_1F)], &plain(1))
        .expect("ink");
    third.store_mut().checkpoint().expect("a checkpoint");

    note::place_selection(&carried, &root.join("notes")).expect("the restore");

    // The note in the file is the file's copy again.
    let back = Note::open(&first).expect("the note");
    assert_eq!(
        back.store().load(1).expect("a page").len(),
        2,
        "the copy in the file is the copy that is restored"
    );

    // The second note in the file is restored too, name and all.
    let restored = Note::open(&second).expect("the note");
    assert_eq!(
        restored.store().title().expect("a name"),
        None,
        "the name written after the snapshot is gone, because the file's copy won"
    );

    // The note the file does not name is not touched at all.
    let untouched = Note::open(&untouched_folder).expect("the note");
    assert_eq!(
        untouched.store().load(1).expect("a page").len(),
        1,
        "a note the file does not name keeps its ink"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A zip that is not a selection places nothing at all, and an entry that climbs out of its note is
/// refused at the listing rather than written.
#[test]
fn a_zip_that_is_not_a_selection_places_nothing() {
    let root = scratch("selection-refused");
    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).expect("a notes folder");

    // A zip holding only a document: not a note, and not a selection.
    let plain_zip = root.join("photos.zip");
    zip_of(&plain_zip, &[("holiday.jpg", b"not a note")]);
    assert!(note::place_selection(&plain_zip, &notes).is_err());
    assert!(note::read_selection(&plain_zip)
        .expect("a listing")
        .is_empty());

    // An entry that points outside its note is refused at the listing, before anything is written.
    let climbing = root.join("climbing.zip");
    zip_of(
        &climbing,
        &[
            ("notes/good/note.db", b"a database"),
            ("notes/good/../../../escape.txt", b"outside"),
        ],
    );
    assert!(
        note::read_selection(&climbing).is_err(),
        "refused at the listing"
    );
    assert!(
        note::place_selection(&climbing, &notes).is_err(),
        "and nothing is placed"
    );
    assert!(
        !root.join("escape.txt").exists(),
        "the entry outside the note was never written"
    );
    assert!(
        !notes.join("good").exists(),
        "and not even the note that was fine"
    );

    let _ = std::fs::remove_dir_all(&root);
}
