// Imported by name rather than by glob: `use super::*` would bring GPUI's own `test` macro into
// scope and shadow the attribute this module needs.
use super::OutlinePages;
use crate::bookmarks::Bookmarks;
use crate::pages::{Page, PageEntry, Pages};
use crate::pdfium::OutlineEntry;

/// An entry with a title, the page it opens, and whatever is under it.
fn entry(title: &str, page: Option<usize>, children: Vec<OutlineEntry>) -> OutlineEntry {
    OutlineEntry {
        title: title.to_string(),
        page,
        children,
    }
}

/// A document's contents become rows in the document's own order, indented by their depth.
///
/// Parents before their children, depth-first: that is the order the document wrote them in, and the
/// order a person reads a table of contents in. The list's indent is the only thing carrying the
/// structure, which is why the depth travels with every row.
#[test]
fn the_contents_are_flattened_in_the_documents_order() {
    let outline = vec![
        entry("Chapter one", Some(0), vec![]),
        entry(
            "Chapter two",
            Some(1),
            vec![
                entry("Two, part one", Some(2), vec![]),
                entry("An online appendix", None, vec![]),
            ],
        ),
    ];
    let note = Pages::of_document(3);

    let pages = OutlinePages::of(
        &outline,
        &note,
        &Bookmarks::default(),
        0,
        String::from("chapter-3.pdf"),
    );

    assert_eq!(
        pages.count(),
        4,
        "a chapter, the chapter after it, and its two sections"
    );
    assert_eq!(
        pages
            .rows
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        [
            "Chapter one",
            "Chapter two",
            "Two, part one",
            "An online appendix"
        ]
    );
    assert_eq!(
        pages.rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
        [0, 0, 1, 1],
        "the sections are a level under their chapter"
    );
    assert_eq!(pages.document, "chapter-3.pdf");
}

/// Every row says where it goes in *this* note, and says so when it goes nowhere.
///
/// The mapping is the whole of the feature: the contents names the document's pages, and what a
/// reader can be shown is the note's.
#[test]
fn a_row_maps_the_documents_page_to_the_notes() {
    // A note holding the document's first and third pages, with a blank sheet between them.
    let note = Pages::restore(
        Some(vec![
            PageEntry::new(Page::Document(0)),
            PageEntry::new(Page::Blank),
            PageEntry::new(Page::Document(2)),
        ]),
        3,
        3,
    );
    let outline = vec![
        entry("Foreword", Some(0), vec![]),
        entry("Chapter two", Some(1), vec![]),
        entry("Chapter three", Some(2), vec![]),
        entry("Elsewhere", None, vec![]),
    ];

    let pages = OutlinePages::of(
        &outline,
        &note,
        &Bookmarks::default(),
        2,
        String::from("book.pdf"),
    );

    assert_eq!(pages.rows[0].note_page, Some(0));
    assert_eq!(pages.rows[0].detail(), "page 1");
    assert_eq!(
        pages.rows[1].note_page, None,
        "the note does not show the document's second page"
    );
    assert_eq!(pages.rows[1].detail(), "page 2 \u{b7} not in this note");
    assert_eq!(
        pages.rows[2].note_page,
        Some(2),
        "the document's third page is the note's third page"
    );
    assert_eq!(pages.rows[2].detail(), "page 3");

    assert!(pages.rows[3].document_page.is_none());
    assert_eq!(
        pages.rows[3].detail(),
        "opens somewhere outside this document"
    );
}

/// A row carries whether its page is marked, and whether it is the page in front.
///
/// Both are for the row's own drawing and for its menu: what the list shows without the app being asked
/// again, and what a bookmark command acts on while this screen is up.
#[test]
fn a_row_carries_the_notes_marks_and_the_page_in_front() {
    let note = Pages::of_document(3);
    let outline = vec![entry("Chapter two", Some(1), vec![])];
    let marks = Bookmarks::of([1]);

    let pages = OutlinePages::of(&outline, &note, &marks, 1, String::from("book.pdf"));

    assert!(pages.rows[0].marked, "the note's page 2 is marked");
    assert!(
        pages.rows[0].is_here(1),
        "and it is the page the reader is on"
    );

    // The same entry against a note that has nothing to do with the document: no page to go to, no
    // mark, and a row that says so rather than pretending to point somewhere.
    let elsewhere = OutlinePages::of(
        &outline,
        &Pages::default(),
        &Bookmarks::default(),
        0,
        String::new(),
    );

    assert_eq!(elsewhere.rows[0].note_page, None);
    assert!(!elsewhere.rows[0].marked);
    assert!(!elsewhere.rows[0].is_here(0));
    assert_eq!(elsewhere.rows[0].detail(), "page 2 \u{b7} not in this note");
}
