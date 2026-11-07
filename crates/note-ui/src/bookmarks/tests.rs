// Imported by name rather than by glob: `use super::*` would bring GPUI's own `test` macro into
// scope and shadow the attribute this module needs.
use super::{Bookmarks, MarkedPages};
use crate::pages::Pages;

/// The marks are the pages, in the note's order, whatever order they were made in.
///
/// The order is the whole of what a bookmark list is: marks that came back in the order they were
/// made would be a list that has to be read rather than used. It is also sorted here rather than
/// trusted, because the rows belong to a file a person may edit.
#[test]
fn the_marks_are_the_pages_in_reading_order() {
    let marks = Bookmarks::of([7, 1, 3, 1]);

    assert_eq!(
        marks.pages(),
        &[1, 3, 7],
        "sorted, and a repeat is one mark"
    );
    assert_eq!(marks.len(), 3);
    assert!(marks.contains(3));
    assert!(!marks.contains(4));
}

/// Marking a page twice is marking it once, and the answer says which way the page ended up.
#[test]
fn a_page_is_marked_or_it_is_not() {
    let mut marks = Bookmarks::default();

    assert!(marks.toggle(2), "the first time marks the page");
    assert_eq!(marks.pages(), &[2], "and it is now in the list");

    assert!(!marks.toggle(2), "the second time takes the mark off");
    assert!(marks.is_empty(), "and the list is empty again");
}

/// A mark is on a page, not on a number: inserting one before it moves the mark along.
#[test]
fn a_mark_follows_its_page_when_one_is_inserted_before_it() {
    let mut marks = Bookmarks::of([0, 2]);

    marks.inserted_at(1);
    assert_eq!(
        marks.pages(),
        &[0, 3],
        "the mark that was page 2 is page 3 now, and the one before the insertion stays"
    );

    marks.inserted_at(0);
    assert_eq!(
        marks.pages(),
        &[1, 4],
        "and an insertion at a mark pushes it along too"
    );
}

/// Deleting a page takes its mark with it, and the marks after it close the gap.
#[test]
fn deleting_a_page_takes_its_mark_and_closes_the_gap() {
    let mut marks = Bookmarks::of([0, 1, 2, 3]);

    marks.removed_at(1);
    assert_eq!(marks.pages(), &[0, 1, 2]);

    // A page that was not marked: only the gap closes.
    let mut marks = Bookmarks::of([0, 3]);
    marks.removed_at(1);
    assert_eq!(marks.pages(), &[0, 2]);
}

/// The keyboard's step looks either way from the page in front, and stops at the ends.
#[test]
fn the_step_from_a_page_looks_either_way() {
    let marks = Bookmarks::of([1, 5, 9]);

    assert_eq!(
        marks.next_from(0),
        Some(1),
        "the first mark after the page in front"
    );
    assert_eq!(
        marks.next_from(5),
        Some(9),
        "the mark in front is not the next one"
    );
    assert_eq!(
        marks.next_from(9),
        None,
        "and there is nothing after the last"
    );

    assert_eq!(marks.previous_from(9), Some(5));
    assert_eq!(marks.previous_from(5), Some(1));
    assert_eq!(marks.previous_from(1), None);
}

/// A row says which page it is *in the note*, and what that page holds.
///
/// The two numbers are different and both are needed: a note written on a document can have pages
/// inserted and deleted, so the note's page 3 can be the document's page 12 — and a person looking
/// for it in their PDF has only the document's number to go by.
#[test]
fn a_row_says_which_document_page_it_holds() {
    let with_a_document = MarkedPages {
        marked: vec![0, 2],
        here: 2,
        total: 4,
        pages: Pages::restore(None, 4, 0),
        document: String::from("chapter-3.pdf"),
    };
    assert_eq!(with_a_document.shows(0), "chapter-3.pdf \u{2014} page 1");
    assert_eq!(with_a_document.shows(2), "chapter-3.pdf \u{2014} page 3");

    // A page with no document behind it is a blank sheet, and says so rather than naming a file.
    let blank = MarkedPages {
        marked: vec![1],
        here: 1,
        total: 8,
        pages: Pages::default(),
        document: String::new(),
    };
    assert_eq!(blank.shows(1), "a blank sheet");
    assert_eq!(blank.count(), 1, "one row per marked page");
}
