// Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into scope
// and shadow the attribute this module needs.
use super::{matches, opens_on_click, scan, visible_indices};
use crate::ink::{InkPoint, Stroke};
use crate::note::{self, Note};
use crate::recent::{Recent, Stamp};
use std::path::{Path, PathBuf};

/// A folder in the temp directory, removed first so a test starts from nothing.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheap-note-home-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A note in `root` with `strokes` strokes on its first page.
fn a_note(root: &Path, name: &str, strokes: usize) -> PathBuf {
    let dir = root.join(name);
    let mut note = Note::open(&dir).expect("a note");
    let ink: Vec<Stroke> = (0..strokes).map(|_| a_stroke()).collect();

    if !ink.is_empty() {
        note.store_mut()
            .append(0, &ink, &crate::history::HistoryUpdate::default())
            .expect("ink");
    }

    dir
}

/// A stroke with two points.
fn a_stroke() -> Stroke {
    let mut stroke = Stroke::new(InkPoint::new(1.0, 2.0, 2.0), Stroke::DEFAULT_COLOR);
    stroke.points.push(InkPoint::new(3.5, 4.5, 2.0));
    stroke.close();
    stroke
}

/// An entry whose folder is named after its title.
fn entry(title: &str, document: Option<&str>, source: Option<&str>) -> Recent {
    Recent::note(
        PathBuf::from(format!("C:/notes/{title}")),
        source.map(PathBuf::from),
        String::from(title),
        document.map(str::to_string),
        1_000,
    )
}

/// A search keeps the notes it names, whichever part of the row it was.
#[test]
fn a_search_keeps_what_it_names() {
    let placed = Recent::note(
        PathBuf::from("C:/notes/chapter-3-9f2a1c44"),
        Some(PathBuf::from("C:/docs/chapter-3.pdf")),
        String::from("chapter-3"),
        Some(String::from("chapter-3.pdf")),
        1_000,
    );
    let blank = Recent::note(
        PathBuf::from("C:/notes/blank-1712345678901"),
        None,
        String::from("Blank sheet"),
        None,
        1_000,
    );

    assert!(matches(&placed, ""));
    assert!(matches(&placed, "chap"), "the name");
    assert!(matches(&placed, "CHAP"), "and it is not case-sensitive");
    assert!(matches(&placed, "docs"), "the file it came from");
    assert!(
        matches(&placed, "9f2a1c44"),
        "the folder's own name, digest and all"
    );
    assert!(matches(&blank, "blank sheet"));
    assert!(!matches(&blank, "chapter"));
    assert!(
        !matches(&blank, "cheap-note"),
        "the words a row says when there is no file must not match every row"
    );

    // A note with no name of its own is searched by the words the row *shows* for it: the words a
    // person is looking at are the words they will type.
    let unnamed = Recent::note(
        PathBuf::from("C:/notes/blank-1712345678901"),
        None,
        String::new(),
        None,
        1_000,
    );
    assert!(matches(&unnamed, "blank sheet"), "the derived name");
    assert!(
        matches(&unnamed, "1712345678901"),
        "and the folder's own name"
    );
    assert!(!matches(&unnamed, "chapter"));

    // A note that *was* named is searched by the name a person gave it.
    let named = Recent {
        title: String::from("3\u{c7a5} \u{c694}\u{c57d}"),
        ..unnamed
    };
    assert!(matches(&named, "3\u{c7a5}"), "the name that was given");
    assert!(
        !matches(&named, "blank sheet"),
        "and not the words the list would have derived"
    );
}

/// One click chooses a row and two open it — and the first click of a double click is a click.
///
/// The counts the platform reports are 1, then 2, then 3 for a triple click, so what has to hold
/// is that the first one does *not* open and the second one does.
#[test]
fn the_first_click_chooses_and_the_second_opens() {
    assert!(!opens_on_click(1), "one click chooses");
    assert!(opens_on_click(2), "two open");
    assert!(
        opens_on_click(3),
        "and a third click is not a reason to stop"
    );
}

/// A query keeps the entries it matches, in the order they were given, and an empty query keeps
/// everything.
#[test]
fn a_query_keeps_the_entries_it_matches_in_order() {
    let entries = vec![
        entry("chapter-3", Some("chapter-3.pdf"), None),
        entry("notes", None, None),
        entry("chapter-4", Some("chapter-4.pdf"), None),
    ];

    assert_eq!(visible_indices(&entries, ""), vec![0, 1, 2]);
    assert_eq!(visible_indices(&entries, "chapter"), vec![0, 2]);
    assert_eq!(visible_indices(&entries, "4"), vec![2]);
    assert_eq!(
        visible_indices(&entries, "nothing"),
        Vec::<usize>::new(),
        "a query that matches nothing keeps nothing"
    );
}

/// A scan finds the notes that are there, and reads only the ones that changed.
#[test]
fn a_scan_reads_only_the_notes_that_changed() {
    let root = scratch("scan");
    let first = a_note(&root, "first-00000001", 2);
    let second = a_note(&root, "second-00000002", 1);

    let (found, articles) = scan(&root, &[]);
    assert_eq!(
        found,
        vec![first.clone(), second.clone()],
        "both folders are notes, and the listing is by name"
    );
    assert!(articles.is_empty(), "nothing is known about them yet");

    // Nothing has been written since: the counts are not read again.
    let stamps: Vec<(PathBuf, Option<Stamp>)> = [&first, &second]
        .iter()
        .filter_map(|folder| {
            Stamp::of(&folder.join(note::NOTE_DB)).map(|stamp| ((*folder).clone(), Some(stamp)))
        })
        .collect();
    let (_, unchanged) = scan(&root, &stamps);
    assert!(
        unchanged.is_empty(),
        "a note that has not been written in is not opened"
    );

    // One of them is written in: that one, and only that one, is read.
    Note::open(&first)
        .expect("a note")
        .store_mut()
        .append(0, &[a_stroke()], &crate::history::HistoryUpdate::default())
        .expect("ink");

    let (_, changed) = scan(&root, &stamps);
    assert_eq!(changed.len(), 1, "one note changed");
    assert_eq!(changed[0].0, first);
    assert_eq!(
        changed[0].2.summary.strokes, 3,
        "the two strokes and the new one"
    );
    assert_eq!(changed[0].2.summary.pages, 1);

    let _ = std::fs::remove_dir_all(&root);
}
