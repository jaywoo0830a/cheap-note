//! The page list, from the outside: reading order, rotation, and the JSON a note stores it in.
//!
//! A note is not a document. It is a *list* of pages — some of the PDF, some blank sheets — which can
//! be inserted into, deleted from and turned, and the ink is keyed by position in that list. So this
//! file tests the two things a second implementation has to get identical: the mapping between a
//! note's page index and a document's, and the bytes the list is stored as.

use note_format::pages::{swaps_axes, turned, Page, PageEntry, Pages, StoredPage};

/// A document's pages arrive in the document's own order, one for one.
#[test]
fn a_documents_pages_are_in_document_order() {
    let pages = Pages::of_document(3);

    assert_eq!(pages.len(), 3);
    assert_eq!(pages.rotation(0), 0, "a page arrives the right way up");

    for index in 0..3 {
        assert_eq!(pages.document_page(index), Some(index));
        assert_eq!(pages.note_page(index), Some(index));
    }

    // A document with no pages at all is still a note to write on: there is always a page in front
    // of the reader.
    assert_eq!(Pages::of_document(0).len(), 1);
    assert_eq!(Pages::default().len(), 1);
    assert_eq!(Pages::default().layout()[0].show, Page::Blank);
}

/// A note written before the list existed is read from what it has, and never loses ink that is
/// already there.
#[test]
fn a_note_without_a_list_is_read_from_what_it_has() {
    // Pages but no list: the note's indices were document indices, so that is what they become.
    let from_a_document = Pages::restore(None, 3, 2);
    assert_eq!(from_a_document.len(), 3);
    assert_eq!(from_a_document.document_page(0), Some(0));

    // No document and no list: blank sheets, enough for the ink the note holds.
    let from_nothing = Pages::restore(None, 0, 2);
    assert_eq!(from_nothing.len(), 2);
    assert_eq!(from_nothing.layout()[0].show, Page::Blank);

    // Neither: one blank sheet, because a note with no pages has nowhere to write.
    assert_eq!(Pages::restore(None, 0, 0).len(), 1);

    // A list, but more pages of ink than it knows about: the list is extended rather than truncating
    // a page somebody wrote on out of existence.
    let extended = Pages::restore(Some(vec![PageEntry::new(Page::Document(0))]), 0, 3);
    assert_eq!(extended.len(), 3);
    assert_eq!(extended.document_page(0), Some(0));
    assert_eq!(extended.document_page(2), None, "the extra pages are blank");
}

/// Four quarter turns are the page it started with, and a quarter turn is what swaps its axes.
#[test]
fn turns_wrap_and_swap_the_axes() {
    for rotation in 0..=3u8 {
        assert_eq!(turned(rotation, 4), rotation);
        assert_eq!(turned(rotation, 0), rotation);
        assert_eq!(
            turned(rotation, -4),
            rotation,
            "the other way is wrapped too"
        );
        assert_eq!(turned(rotation, 1), turned(rotation, -3));
        assert_eq!(swaps_axes(rotation), rotation % 2 == 1);
    }

    assert_eq!(turned(3, 1), 0, "a page turned past upside down is upright");
    assert_eq!(turned(0, -1), 3);
}

/// Turning one page turns only that page; turning the note turns every page by its own amount.
#[test]
fn a_page_carries_its_own_rotation() {
    let mut pages = Pages::of_document(3);

    pages.turn(1, 1);
    assert_eq!(
        [pages.rotation(0), pages.rotation(1), pages.rotation(2)],
        [0, 1, 0]
    );

    pages.turn_all(1);
    assert_eq!(
        [pages.rotation(0), pages.rotation(1), pages.rotation(2)],
        [1, 2, 1],
        "every page moves by one, from wherever it was"
    );

    // An index the note does not have cannot be turned, and answers as the right way up.
    pages.turn(99, 1);
    assert_eq!(pages.rotation(99), 0);
}

/// A new page lands where it was asked for, the right way up, and it is the page to turn to.
#[test]
fn inserting_a_page_puts_a_blank_sheet_there() {
    let mut pages = Pages::of_document(2);
    pages.turn(0, 2);

    let at = pages.insert(0, true);
    assert_eq!(
        at, 0,
        "a page inserted before the first one is the first one"
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(pages.layout()[0].show, Page::Blank, "it is blank paper");
    assert_eq!(pages.rotation(0), 0, "and it arrives upright");
    assert_eq!(pages.rotation(1), 2, "the page it displaced kept its turn");

    let at = pages.insert(2, false);
    assert_eq!(at, 3, "after the third page");
    assert_eq!(pages.len(), 4);

    // Past the end is the end: an index that is not there cannot be inserted before or after.
    assert_eq!(pages.insert(99, false), 4);
    assert_eq!(pages.len(), 5);
}

/// The last page is kept: a note cannot be emptied of the paper it is written on.
#[test]
fn the_last_page_cannot_be_removed() {
    let mut one = Pages::default();
    assert_eq!(one.remove(0), None, "a note always has a page");
    assert_eq!(one.len(), 1);

    let mut two = Pages::of_document(2);
    assert_eq!(two.remove(0), Some(0));
    assert_eq!(two.len(), 1);
    assert_eq!(two.remove(0), None);
    assert_eq!(two.remove(5), None, "an index that is not there");
}

/// A blank sheet in the middle of a document shifts the mapping, and the two directions agree.
///
/// This is the mapping an outline entry depends on: the document's table of contents names document
/// pages, and the note has to find which of *its* pages shows one.
#[test]
fn the_note_and_the_document_agree_about_which_page_is_which() {
    let mut pages = Pages::of_document(3);
    pages.insert(1, true);

    assert_eq!(pages.document_page(0), Some(0));
    assert_eq!(pages.document_page(1), None, "the inserted sheet");

    for document_page in 0..3 {
        let note_page = pages
            .note_page(document_page)
            .expect("the note shows every page of the document");

        assert_eq!(pages.document_page(note_page), Some(document_page));
    }

    assert_eq!(pages.note_page(0), Some(0));
    assert_eq!(pages.note_page(1), Some(2), "the blank sheet is in the way");
    assert_eq!(pages.note_page(9), None, "a page the note does not show");
    assert_eq!(pages.clamp(99), pages.len() - 1);
}

/// The stored form of a page: what this build writes, and what a note written before rotation has.
///
/// The older shape is a page with no `rotation` field at all, and reading it as "the right way up"
/// is the whole of the compatibility this needs. A note that could not be opened because of a field
/// it never had would be a worse failure than a page arriving unturned.
#[test]
fn a_page_written_before_rotation_is_read_upright() {
    let turned: StoredPage = serde_json::from_str(r#"{"show":{"document":3},"rotation":1}"#)
        .expect("what this build writes");
    assert_eq!(
        turned.entry(),
        PageEntry {
            show: Page::Document(3),
            rotation: 1
        }
    );

    let older: StoredPage =
        serde_json::from_str(r#"{"document":3}"#).expect("what an older note has");
    assert_eq!(older.entry(), PageEntry::new(Page::Document(3)));

    let blank: StoredPage = serde_json::from_str(r#""blank""#).expect("a blank page");
    assert_eq!(blank.entry(), PageEntry::new(Page::Blank));
}

/// The list is JSON, and reading it back gives the same list — rotations included.
#[test]
fn a_page_list_survives_its_json() {
    let mut pages = Pages::of_document(2);
    pages.turn(1, 3);
    pages.insert(1, true);

    let json = serde_json::to_string(pages.layout()).expect("a page list as JSON");
    let read: Vec<PageEntry> = serde_json::from_str(&json).expect("a page list back");

    assert_eq!(read, pages.layout());
    assert_eq!(read[2].rotation, 3);
    assert_eq!(read[1].show, Page::Blank);
}
