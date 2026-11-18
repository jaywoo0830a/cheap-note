use std::path::PathBuf;

use super::*;
use note_format::note::{LibraryManifest, LIBRARY_VERSION};

/// A folder of a test's own, removed first so every run starts from nothing.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("cheap-note-library-{name}"));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch folder");
    path
}

/// The desktop is where a backup goes, and it is chosen without asking.
#[test]
fn the_backup_goes_to_the_desktop_when_there_is_one() {
    let root = scratch("desktop");
    let desktop = root.join("Desktop");
    let home = root.join("home");
    std::fs::create_dir_all(&desktop).expect("a desktop");
    std::fs::create_dir_all(&home).expect("a home");

    let target = backup_target(Some(desktop.clone()), Some(home), &root.join("notes"));

    assert_eq!(target, desktop.join(LIBRARY_FILE));
    assert_eq!(target.file_name().unwrap(), "cheap-note-library.zip");

    let _ = std::fs::remove_dir_all(&root);
}

/// A machine that reports no desktop — a server, a stripped session — falls back rather than failing:
/// the home folder, and then the notes' own parent.
#[test]
fn a_backup_with_no_desktop_falls_back_to_the_home_folder() {
    let root = scratch("fallback");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("a home");
    let notes = root.join("notes");

    assert_eq!(
        backup_target(None, Some(home.clone()), &notes),
        home.join(LIBRARY_FILE)
    );

    // Neither a desktop nor a home: the notes' parent, which the app already writes to.
    assert_eq!(
        backup_target(None, None, &notes),
        root.join(LIBRARY_FILE),
        "the last resort is a folder the app owns"
    );

    // A desktop that is reported but is not there is not a place to write: it is skipped like a
    // missing one, because a failed backup is worse than a backup in the second-best folder.
    assert_eq!(
        backup_target(Some(root.join("missing")), Some(home.clone()), &notes),
        home.join(LIBRARY_FILE)
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The automatic restore only happens on a machine with nothing on it — that is the guard, and it is
/// asked of both the list and the folder.
#[test]
fn a_machine_with_notes_is_never_restored_over() {
    let root = scratch("guard");
    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).expect("a notes folder");

    assert!(is_empty(&[], &notes), "nothing opened and nothing on disk");
    assert!(
        is_empty(&[], &notes.join("never-created")),
        "a first run's notes folder does not exist"
    );

    // A folder with a note in it is a machine with notes, whether or not the list knows about it.
    note_format::note::Note::open(&notes.join("chapter-3")).expect("a note");
    assert!(
        !is_empty(&[], &notes),
        "a note on disk is a note, even with an empty list"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The describe line says what the restore is about to do, and how old the copy is.
#[test]
fn a_library_is_described_by_its_count_and_age() {
    let manifest = |notes: &[&str], created_at_ms: u64| LibraryManifest {
        version: LIBRARY_VERSION,
        created_at_ms,
        notes: notes.iter().map(|name| name.to_string()).collect(),
    };

    let now = 1_000_000_000_000;
    assert_eq!(
        describe(&manifest(&["one"], now - 5_000), now),
        "1 note, with a library made just now"
    );
    assert_eq!(
        describe(&manifest(&["one", "two"], now - 3 * 60_000), now),
        "2 notes, with a library made 3 minutes ago"
    );
    assert!(
        describe(&manifest(&["one"], 0), now).contains("no date"),
        "a manifest with no date says so rather than printing a figure"
    );
}
