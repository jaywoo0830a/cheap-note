//! The library: every note as one file, and every note of that file back on a machine.
//!
//! This is the container a *second machine* meets, so it is checked the way the note's own zip is:
//! the entries are read with a plain zip reader, the manifest with a JSON parser, and the restored
//! notes by opening them. What has to stay identical is the layout (`library.json`, `notes/<folder>/`
//! with the three names inside it), the fact that a library is a zip a file manager can open, and the
//! refusals — a zip with no manifest, and an entry that would write outside the note it belongs to.

use std::path::Path;

use note_format::history::HistoryUpdate;
use note_format::ink::Stroke;
use note_format::note::{
    self, Note, ATTACHMENTS, LIBRARY_MANIFEST, LIBRARY_NOTES, NOTE_DB, NOTE_PDF,
};

mod common;

use common::{a_pdf, scratch, stroke};

/// A note with ink in it, under `root`, named `name`: three strokes, as the app would hand them over.
///
/// Written the way the app writes — one `Written` edit per stroke, sent in the same job as the ink,
/// because a page's ink is read back through its history — and then folded back, because a note in the
/// app is a folder with a database in it, which is what a library lists.
fn a_note(root: &Path, name: &str) -> Note {
    let mut note = Note::open(&root.join(name)).expect("a note");
    let ink = ink();
    note.store_mut()
        .append(0, &ink, &history(&ink))
        .expect("ink");
    note.store().checkpoint().expect("a checkpoint");
    note
}

/// The three strokes every test's first page holds.
fn ink() -> Vec<Stroke> {
    (0..3)
        .map(|line| {
            stroke(
                5,
                10.0 + line as f32 * 30.0,
                10.0 + line as f32 * 40.0,
                0x1B_1B_1F,
            )
        })
        .collect()
}

/// What a write tells the note: every stroke an edit, the cursor at the end, and the count.
fn history(strokes: &[Stroke]) -> HistoryUpdate {
    let edits = strokes
        .iter()
        .enumerate()
        .map(|(at, stroke)| common::written(at, stroke))
        .collect();

    common::update(edits, strokes.len() as u64, strokes.len())
}

/// Every entry name in a zip, read the way a file manager does.
fn entries_of(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).expect("a zip");
    let mut zip = zip::ZipArchive::new(file).expect("a readable zip");
    (0..zip.len())
        .map(|index| {
            zip.by_index(index)
                .expect("an entry")
                .name()
                .expect("a name")
                .to_string()
        })
        .collect()
}

/// Read the file at `entry` out of a zip as text.
fn text_entry(path: &Path, entry: &str) -> String {
    let file = std::fs::File::open(path).expect("a zip");
    let mut zip = zip::ZipArchive::new(file).expect("a readable zip");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut zip.by_name(entry).expect(entry), &mut text).expect("text");
    text
}

/// A library is a zip every note goes into, under the same three names a note has, and it says so in
/// a manifest that is plain JSON.
#[test]
fn a_library_is_a_zip_of_every_note_under_its_own_name() {
    let root = scratch("library-write");
    let notes_root = root.join("notes");

    let mut chapter = a_note(&notes_root, "chapter-3");
    chapter
        .put_document("chapter-3.pdf", &a_pdf())
        .expect("a document");
    chapter.store().checkpoint().expect("a checkpoint");
    a_note(&notes_root, "blank-1712345678901");

    let target = root.join("cheap-note-library.zip");
    let folders = note::write_library(&notes_root, &target, 1_700_000_000_000).expect("a library");

    assert_eq!(
        folders,
        vec![
            String::from("blank-1712345678901"),
            String::from("chapter-3")
        ],
        "the notes are listed by their folder names, in the order they are written"
    );
    assert!(target.is_file(), "the library is one file");

    let entries = entries_of(&target);
    assert_eq!(entries.first().map(String::as_str), Some(LIBRARY_MANIFEST));
    for entry in [
        format!("{LIBRARY_NOTES}/chapter-3/{NOTE_DB}"),
        format!("{LIBRARY_NOTES}/chapter-3/{NOTE_PDF}"),
        format!("{LIBRARY_NOTES}/blank-1712345678901/{NOTE_DB}"),
    ] {
        assert!(entries.contains(&entry), "{entry} is in {entries:?}");
    }
    assert!(
        !entries.iter().any(|entry| entry.contains(".tmp")),
        "no temporary survives the write: {entries:?}"
    );
    assert!(
        !notes_root.join("chapter-3").join("library.tmp.db").exists(),
        "the vacuum was made inside the note and is gone"
    );

    // The manifest, read as a person would: JSON, a version, a date, and the names.
    let json: serde_json::Value =
        serde_json::from_str(&text_entry(&target, LIBRARY_MANIFEST)).expect("the manifest is JSON");

    assert_eq!(json["version"], note::LIBRARY_VERSION);
    assert_eq!(json["created_at_ms"], 1_700_000_000_000u64);
    assert_eq!(json["notes"][0], "blank-1712345678901");

    // And it is readable as a library, without being placed.
    let contents = note::read_library(&target).expect("a listing");
    assert_eq!(contents.notes.len(), 2);
    assert_eq!(contents.notes[1].name, "chapter-3");
    assert!(contents
        .notes
        .iter()
        .any(|note| note.files.iter().any(|(inside, _)| inside == NOTE_PDF)));

    let _ = std::fs::remove_dir_all(&root);
}

/// A library placed on a *second* machine has the ink, the document and the page list — and the note
/// that was already in that folder is replaced by the one the library holds.
#[test]
fn a_library_restores_the_ink_and_replaces_what_was_there() {
    let root = scratch("library-restore");

    // The first machine.
    let first = root.join("first");
    let mut chapter = a_note(&first, "chapter-3");
    chapter
        .put_document("chapter-3.pdf", &a_pdf())
        .expect("a document");
    let layout = chapter.store().layout().expect("a list");
    chapter.store().checkpoint().expect("a checkpoint");
    a_note(&first, "blank-1712345678901");

    let carried = root.join("cheap-note-library.zip");
    note::write_library(&first, &carried, 0).expect("a library");

    // The second machine, which already has an empty note in that folder.
    let second = root.join("second");
    Note::open(&second.join("chapter-3")).expect("an empty note");

    let placed = note::place_library(&carried, &second).expect("the library is placed");

    assert_eq!(placed.len(), 2);
    assert_eq!(
        placed,
        vec![second.join("blank-1712345678901"), second.join("chapter-3")],
        "every note the library lists is placed under its own name"
    );

    let arrived = Note::open(&second.join("chapter-3")).expect("the restored note");
    assert_eq!(
        arrived.store().load(0).expect("the ink").len(),
        3,
        "the ink came back"
    );
    assert_eq!(
        arrived.store().layout().expect("a list"),
        layout,
        "the page list came back"
    );
    let (name, bytes) = arrived
        .document()
        .expect("a document")
        .expect("the document came back");
    assert_eq!(name, "chapter-3.pdf");
    assert_eq!(bytes, a_pdf());

    // Nothing of the restore is left over: no staging folder, no old copy beside the new one.
    let leftovers: Vec<_> = std::fs::read_dir(&second)
        .expect("the notes folder")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// Attachments travel in a library too: the container carries whatever is in the folder, and a
/// backup that dropped a person's file would be a backup nobody could trust.
#[test]
fn a_library_carries_attachments() {
    let root = scratch("library-attachments");
    let first = root.join("first");
    let note = a_note(&first, "chapter-3");

    let folder = note.dir().join(ATTACHMENTS).join("images");
    std::fs::create_dir_all(&folder).expect("an attachments folder");
    std::fs::write(folder.join("diagram.txt"), b"a diagram").expect("an attachment");

    let carried = root.join("library.zip");
    note::write_library(&first, &carried, 0).expect("a library");

    let entry = format!("{LIBRARY_NOTES}/chapter-3/{ATTACHMENTS}/images/diagram.txt");
    assert!(entries_of(&carried).contains(&entry), "{entry} is missing");

    let second = root.join("second");
    note::place_library(&carried, &second).expect("a restore");
    assert_eq!(
        std::fs::read(
            second
                .join("chapter-3")
                .join(ATTACHMENTS)
                .join("images/diagram.txt")
        )
        .expect("the attachment"),
        b"a diagram"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A library written on one platform is read on another: a note folder with a non-ASCII name in it,
/// and a document whose name is not ASCII, survive the crossing.
///
/// The bug this pins is the Mac-to-Windows one. A note's folder name used to keep Korean letters on
/// macOS and drop them on Windows, so the same note was `한글-…` on one machine and `note-…` on the
/// next — and a `notes/한글-…/note.db` entry a Windows machine extracts had its folder mangled, which
/// left a note whose database was there and whose name was not.
///
/// The zip's own half is checked the way a Windows tool reads it, from outside this crate:
///
/// * the entry's *stored* name is UTF-8, and the entry's general-purpose flag says so (bit 11);
/// * the entry carries the Info-ZIP Unicode Path extra field (`0x7075`) — a version byte, the CRC-32
///   of the stored name, and then the name again in UTF-8. Windows Explorer and older WinZip builds
///   read the name from that field, and a Mac's own tools read the flag; a file with both is read
///   correctly by both.
///
/// The note's folder is built by hand rather than by `folder_name` (whose output is ASCII on every
/// platform), because this is about reading a library *somebody else made* — a note from a build, or a
/// person's hand, that named a folder in Korean.
#[test]
fn a_library_names_its_entries_so_any_platform_reads_them() {
    let root = scratch("library-unicode");
    let notes_root = root.join("notes");
    let folder = "한글-보고서-9f2a1c44";

    let mut note = a_note(&notes_root, folder);
    note.put_document("한글 보고서.pdf", &a_pdf())
        .expect("a document");
    note.store().checkpoint().expect("a checkpoint");

    let carried = root.join("library.zip");
    note::write_library(&notes_root, &carried, 0).expect("a library");

    let entry = format!("{LIBRARY_NOTES}/{folder}/{NOTE_DB}");
    let file = std::fs::File::open(&carried).expect("a zip");
    let mut zip = zip::ZipArchive::new(file).expect("a readable zip");
    let held = zip.by_name(&entry).expect("the note is in the library");
    assert_eq!(
        held.name_raw(),
        entry.as_bytes(),
        "the name is stored as UTF-8"
    );
    assert!(
        held.flags().as_u16() & 0x0800 != 0,
        "the entry's language flag says its name is UTF-8: {:#06x}",
        held.flags().as_u16()
    );

    let field = extra_field(&held, 0x7075).expect("the Info-ZIP Unicode Path field");
    assert_eq!(field.first(), Some(&1), "the field's version byte");
    assert_eq!(
        &field[1..5],
        crc32fast::hash(entry.as_bytes()).to_le_bytes(),
        "the field's CRC is of the name it repeats"
    );
    assert_eq!(
        &field[5..],
        entry.as_bytes(),
        "and the name itself, in UTF-8"
    );

    // And the other direction: a library an older build wrote — with the Korean name but no field —
    // is still read and placed, with its folder and its document's name intact.
    let second = root.join("second");
    let placed = note::place_library(&carried, &second).expect("the library is placed");
    assert_eq!(placed, vec![second.join(folder)]);

    let arrived = Note::open(&second.join(folder)).expect("the restored note");
    let (name, bytes) = arrived
        .document()
        .expect("a document")
        .expect("the document came back");
    assert_eq!(
        name, "한글 보고서.pdf",
        "the document keeps its Korean name"
    );
    assert_eq!(bytes, a_pdf());

    let _ = std::fs::remove_dir_all(&root);
}

/// A zip entry's extra field, by its header id, read the way APPNOTE describes it: a `u16` id, a
/// `u16` length, and that many bytes.
fn extra_field<R: std::io::Read + ?Sized>(
    entry: &zip::read::ZipFile<'_, R>,
    header: u16,
) -> Option<Vec<u8>> {
    let data = entry.extra_data()?;
    let mut at = 0;
    while at + 4 <= data.len() {
        let id = u16::from_le_bytes([data[at], data[at + 1]]);
        let len = usize::from(u16::from_le_bytes([data[at + 2], data[at + 3]]));
        let body = data.get(at + 4..at + 4 + len)?;
        if id == header {
            return Some(body.to_vec());
        }
        at += 4 + len;
    }
    None
}

/// A zip that is not a library is refused, and nothing is written: a person who chose the wrong file
/// must not end up with a notes folder half-filled from it. A library from a *newer* build is refused
/// as newer rather than as broken — the file is fine, this build is old.
#[test]
fn a_zip_with_no_manifest_is_not_a_library() {
    use std::io::Write as _;

    let root = scratch("library-refusals");
    let notes_root = root.join("notes");

    let target = root.join("photographs.zip");
    let file = std::fs::File::create(&target).expect("a zip");
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("holiday.jpg", zip::write::SimpleFileOptions::default())
        .expect("an entry");
    zip.write_all(b"a photograph").expect("the bytes");
    zip.finish().expect("a finished zip");

    let error = note::place_library(&target, &notes_root).expect_err("a refusal");
    assert_eq!(
        error.variant(),
        Some(note_format::Variant::NotANote),
        "{error}"
    );
    assert!(
        note::notes_in(&notes_root).expect("a listing").is_empty(),
        "nothing was placed"
    );

    let newer = root.join("newer.zip");
    let file = std::fs::File::create(&newer).expect("a zip");
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file(LIBRARY_MANIFEST, zip::write::SimpleFileOptions::default())
        .expect("an entry");
    let manifest = serde_json::to_vec(&serde_json::json!({
        "version": note::LIBRARY_VERSION + 1,
        "created_at_ms": 0,
        "notes": [],
    }))
    .expect("the manifest");
    zip.write_all(&manifest).expect("the bytes");
    zip.finish().expect("a finished zip");

    let error = note::read_library(&newer).expect_err("a refusal");
    assert_eq!(
        error.variant(),
        Some(note_format::Variant::NewerBuild),
        "{error}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// An entry that would write outside the note it belongs to is refused. Nothing escapes the notes
/// folder, and a note that was already in that folder is still there afterwards — a library from
/// somebody else is the one way a file could reach into a disk it was not given, and a refused restore
/// must not cost the copy that was on this machine.
#[test]
fn an_entry_that_climbs_out_of_its_note_is_refused() {
    use std::io::Write as _;

    let root = scratch("library-climb");
    let notes_root = root.join("notes");
    let target = root.join("hostile.zip");

    // The note on this machine, which the library names and must not be allowed to replace.
    let mut present = Note::open(&notes_root.join("chapter-3")).expect("a note");
    let ink = ink();
    present
        .store_mut()
        .append(0, &ink, &history(&ink))
        .expect("ink");
    present.store().checkpoint().expect("a checkpoint");

    let file = std::fs::File::create(&target).expect("a zip");
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file(LIBRARY_MANIFEST, zip::write::SimpleFileOptions::default())
        .expect("an entry");
    let manifest = serde_json::to_vec(&serde_json::json!({
        "version": note::LIBRARY_VERSION,
        "created_at_ms": 0,
        "notes": ["chapter-3"],
    }))
    .expect("the manifest");
    zip.write_all(&manifest).expect("the bytes");

    for (name, bytes) in [
        (
            format!("{LIBRARY_NOTES}/chapter-3/{NOTE_DB}"),
            &b"not really"[..],
        ),
        (
            format!("{LIBRARY_NOTES}/chapter-3/../../escaped.txt"),
            &b"outside"[..],
        ),
    ] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .expect("an entry");
        zip.write_all(bytes).expect("the bytes");
    }
    zip.finish().expect("a finished zip");

    let error = note::place_library(&target, &notes_root).expect_err("a refusal");
    assert_eq!(
        error.variant(),
        Some(note_format::Variant::NotANote),
        "{error}"
    );
    assert!(
        !root.join("escaped.txt").exists(),
        "nothing was written outside the notes folder"
    );

    // The refused library left no staging folder, and the note that was here is the note that is here.
    let leftovers: Vec<_> = std::fs::read_dir(&notes_root)
        .expect("the notes folder")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

    let survivor = Note::open(&notes_root.join("chapter-3")).expect("the old note");
    assert_eq!(
        survivor.store().load(0).expect("the ink").len(),
        3,
        "the copy that was here was not replaced by a refused one"
    );

    let _ = std::fs::remove_dir_all(&root);
}
