//! What a note's pages are.
//!
//! ## Why a note needs a page list of its own
//!
//! Three things disagreed about what "page 4" means:
//!
//! * a document has its own pages, in its own order, numbered from zero;
//! * a note written on blank sheets has pages of its own, and none of them is a document page;
//! * a page can be inserted or deleted, after which the *n*th page the reader sees is no longer
//!   the *n*th page of anything.
//!
//! [`Pages`] is the answer to all three: one entry per page, in reading order, saying what that page
//! shows. The ink is keyed by the *position in this list* — see [`crate::ink::Notes`] — so inserting
//! a page renames the pages after it, and that renaming is the whole of what an insert does.
//!
//! ## What a deleted page means for a document
//!
//! Deleting a page removes it from the *note*, not from the document: the PDF inside a saved note is
//! the file that was opened, byte for byte, and this app has no PDF writer. Reopening the original
//! file therefore shows every page it always had, and the note's own list is what says which of them
//! the note is about. That is a real limitation, stated rather than hidden: an "export" that baked
//! the list into a new PDF would be a different feature.

use serde::{Deserialize, Serialize};

/// What one page shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Page {
    /// A blank sheet: its ruling and colour come from the note's style, as they do for a note
    /// with no document open.
    Blank,
    /// A page of the open document, by its index in that document.
    Document(usize),
}

/// Which way up a page is: quarter turns **clockwise**, `0..=3`.
///
/// A rotation belongs to the *page*, not to the view. A page turned on its side is turned for good —
/// it is a sheet with something printed on it, and a PDF's own `/Rotate` is the same fact about the
/// same page — so it is stored with the page and turns with it, rather than being a transform of the
/// window that would have to be remembered somewhere other than the page it belongs to.
///
/// Quarter turns rather than a float angle: a page can be turned on its side, upside down, or on its
/// other side, and there is no fourth thing to ask for. An angle would be stored state that has to be
/// *decided* on the way out — is 359.9° the page it started as? — which is a question this number
/// cannot be asked.
pub type Quarters = u8;

/// `turns` quarter turns from `rotation`, wrapped into `0..=3`.
///
/// Wrapped rather than clamped: four turns is the page it started as, and a command that says "turn
/// clockwise" must not leave a page at a rotation that is not one of the four.
pub fn turned(rotation: Quarters, turns: i32) -> Quarters {
    (rotation as i32 + turns).rem_euclid(4) as Quarters
}

/// Whether a page at this rotation is drawn the other way round: a quarter turn swaps its width and
/// its height.
pub fn swaps_axes(rotation: Quarters) -> bool {
    rotation % 2 == 1
}

/// One page of a note: what it shows, and which way up it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageEntry {
    /// What the page shows.
    pub show: Page,
    /// Which way up it is, in quarter turns clockwise.
    ///
    /// Defaulted rather than required: a note written by a build that had no rotation has every page
    /// the right way up, and says nothing about the field at all.
    #[serde(default)]
    pub rotation: Quarters,
}

impl PageEntry {
    /// A page showing `show`, the right way up.
    pub fn new(show: Page) -> Self {
        PageEntry { show, rotation: 0 }
    }

    /// The same page, turned `turns` quarter turns clockwise.
    pub fn turned(self, turns: i32) -> Self {
        PageEntry {
            rotation: turned(self.rotation, turns),
            ..self
        }
    }
}

/// A page as a *note* on disk has it.
///
/// Two shapes are read, and this app has written both: the page with its rotation, which is what it
/// writes now, and the page alone, which is what a note written before a page could be turned has.
/// The untagged form is what lets one reader accept both — the old shape has no `show` field, so it
/// falls through to [`Page`] — and reading it as "the right way up" is the whole of the compatibility
/// this needs. The alternative is a note that cannot be opened because of a field it has never had,
/// which for a file format this young is a worse failure than a page arriving unturned.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(untagged)]
pub enum StoredPage {
    /// What this build writes: what the page shows, and which way up it is.
    Turned(PageEntry),
    /// What a note written before pages could be turned has.
    Upright(Page),
}

impl StoredPage {
    /// The page, with the right way up for one that was written before pages could be turned.
    pub fn entry(self) -> PageEntry {
        match self {
            StoredPage::Turned(entry) => entry,
            StoredPage::Upright(show) => PageEntry::new(show),
        }
    }
}

/// The pages of a note, in reading order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pages {
    /// One entry per page.
    pages: Vec<PageEntry>,
}

impl Default for Pages {
    /// A note with nothing open is one blank sheet: there is always a page in front of the reader.
    fn default() -> Self {
        Pages {
            pages: vec![PageEntry::new(Page::Blank)],
        }
    }
}

impl Pages {
    /// A document's own pages, in its own order: what opening a PDF gives a note.
    pub fn of_document(count: usize) -> Self {
        Pages {
            pages: (0..count.max(1))
                .map(|page| PageEntry::new(Page::Document(page)))
                .collect(),
        }
    }

    /// The pages a saved note is opened with.
    ///
    /// `layout` is the note's own list, which notes written before the list existed do not have.
    /// Those notes' page indices *were* document page indices wherever there was a document, so
    /// that is what they are read as; a note written on blank sheets gets blanks. Either way the
    /// list is then extended to cover every page that holds ink — a page with ink on it that the
    /// list does not have is ink with nowhere to be shown.
    pub fn restore(
        layout: Option<Vec<PageEntry>>,
        document_pages: usize,
        ink_pages: usize,
    ) -> Self {
        let mut pages = layout.unwrap_or_else(|| {
            if document_pages > 0 {
                Pages::of_document(document_pages).pages
            } else {
                Vec::new()
            }
        });

        if pages.is_empty() {
            pages.push(PageEntry::new(Page::Blank));
        }

        while pages.len() < ink_pages {
            pages.push(PageEntry::new(Page::Blank));
        }

        Pages { pages }
    }

    /// Every page, in reading order: what a save writes out.
    pub fn layout(&self) -> &[PageEntry] {
        &self.pages
    }

    /// Which way up the page at `index` is, in quarter turns clockwise.
    ///
    /// The right way up for an index the note does not have: a page that is not there cannot be
    /// turned, and a caller drawing one is drawing something that is not a page.
    pub fn rotation(&self, index: usize) -> Quarters {
        self.pages.get(index).map_or(0, |page| page.rotation)
    }

    /// Turns one page `turns` quarter turns clockwise.
    pub fn turn(&mut self, index: usize, turns: i32) {
        if let Some(page) = self.pages.get_mut(index) {
            *page = page.turned(turns);
        }
    }

    /// Turns every page of the note `turns` quarter turns clockwise.
    ///
    /// Every page, deliberately: "turn the document round" is one command, each page keeps its own
    /// rotation, and turning the document back is one command the other way — which is not true of a
    /// page-less rotation stored somewhere else.
    pub fn turn_all(&mut self, turns: i32) {
        for page in &mut self.pages {
            *page = page.turned(turns);
        }
    }

    /// How many pages there are.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// The document page shown at `index`, when that page shows one.
    pub fn document_page(&self, index: usize) -> Option<usize> {
        match self.pages.get(index).map(|page| page.show) {
            Some(Page::Document(page)) => Some(page),
            _ => None,
        }
    }

    /// The page of the *note* that shows a document's page, when the note has one.
    ///
    /// The inverse of [`Self::document_page`], and the mapping every outline entry needs: a document's
    /// table of contents names pages of the *document*, while a note is a list of its own pages with the
    /// document's mixed in among blank sheets — inserted, deleted and moved around. A document page the
    /// note does not show has no note page at all, which is an answer rather than a failure: the note was
    /// made from some of the document, and an entry for a page outside that part cannot be opened.
    pub fn note_page(&self, document_page: usize) -> Option<usize> {
        self.pages
            .iter()
            .position(|page| page.show == Page::Document(document_page))
    }

    /// Inserts a blank page before or after `index`, and answers with the page to turn to.
    ///
    /// The new page is the one the reader is looking at afterwards, because a page that has just
    /// been made is the page that is about to be written on. It arrives the right way up: it is blank
    /// paper, and the sheet it is written on is the note's own.
    pub fn insert(&mut self, index: usize, before: bool) -> usize {
        let at = if before { index } else { index + 1 }.min(self.pages.len());
        self.pages.insert(at, PageEntry::new(Page::Blank));
        at
    }

    /// Removes the page at `index`, unless it is the note's only page, and answers with the page to
    /// turn to.
    ///
    /// The last page is refused rather than replaced by a fresh blank one: a note that can be
    /// emptied of pages is a note with nothing to write on, and the refusal has to happen before
    /// the ink is moved rather than after.
    pub fn remove(&mut self, index: usize) -> Option<usize> {
        if self.pages.len() <= 1 || index >= self.pages.len() {
            return None;
        }

        self.pages.remove(index);
        Some(index.min(self.pages.len() - 1))
    }

    /// A page index the note can actually be shown at.
    pub fn clamp(&self, index: usize) -> usize {
        index.min(self.pages.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{swaps_axes, turned, Page, PageEntry, Pages, StoredPage};

    /// A page the way a test says what one shows.
    fn page(show: Page) -> PageEntry {
        PageEntry::new(show)
    }

    /// A blank page.
    fn blank() -> PageEntry {
        page(Page::Blank)
    }

    /// A document's pages are its own, in its own order.
    #[test]
    fn a_document_opens_as_its_own_pages() {
        let pages = Pages::of_document(3);

        assert_eq!(pages.len(), 3);
        assert_eq!(pages.document_page(0), Some(0));
        assert_eq!(pages.document_page(2), Some(2));
        assert_eq!(pages.document_page(3), None, "there is no fourth page");
    }

    /// An inserted page goes where it was asked for, and is the page the reader is turned to.
    #[test]
    fn a_page_is_inserted_before_or_after_the_one_in_hand() {
        let mut pages = Pages::of_document(2);

        let after = pages.insert(0, false);
        assert_eq!(after, 1, "the page after the first is the page just made");
        assert_eq!(pages.len(), 3);
        assert_eq!(pages.layout()[0], page(Page::Document(0)));
        assert_eq!(pages.layout()[1], blank());
        assert_eq!(
            pages.layout()[2],
            page(Page::Document(1)),
            "the document's order held"
        );

        let before = pages.insert(2, true);
        assert_eq!(before, 2);
        assert_eq!(pages.layout()[1], blank());
        assert_eq!(pages.layout()[2], blank());
    }

    /// Deleting takes the page out of the *note*: the document's later pages keep their own numbers,
    /// because that is what a document page is.
    #[test]
    fn a_deleted_document_page_leaves_the_documents_own_pages_alone() {
        let mut pages = Pages::of_document(3);

        assert_eq!(
            pages.remove(1),
            Some(1),
            "the page that followed takes its place"
        );
        assert_eq!(pages.len(), 2);
        assert_eq!(
            pages.layout(),
            &[page(Page::Document(0)), page(Page::Document(2))]
        );
    }

    /// The last page cannot be deleted: a note with no pages has nowhere to write.
    #[test]
    fn the_last_page_is_not_deletable() {
        let mut pages = Pages::default();

        assert_eq!(pages.remove(0), None);
        assert_eq!(pages.len(), 1);
    }

    /// A note's page for a document's page is found, and a document page the note does not show has
    /// none.
    ///
    /// The two directions of the same mapping, and the one an outline entry travels: a table of contents
    /// names document pages, the note holds some of them, in whatever order they ended up in.
    #[test]
    fn a_document_page_is_found_in_the_note_that_shows_it() {
        let mut pages = Pages::of_document(4);
        pages.insert(0, false);
        pages.remove(4);

        assert_eq!(
            pages.layout(),
            &[
                page(Page::Document(0)),
                blank(),
                page(Page::Document(1)),
                page(Page::Document(2))
            ],
            "three of the document's four pages, with a blank sheet in front of them"
        );
        assert_eq!(pages.note_page(0), Some(0));
        assert_eq!(
            pages.note_page(1),
            Some(2),
            "a blank sheet in between is not the document's page 2"
        );
        assert_eq!(pages.note_page(2), Some(3));
        assert_eq!(
            pages.note_page(3),
            None,
            "the page of the document this note no longer shows"
        );
        assert_eq!(pages.note_page(9), None, "and one it never showed");
    }

    /// A note saved before the page list existed opens with the pages it must have had.
    #[test]
    fn a_note_without_a_layout_gets_one_from_what_it_holds() {
        // Written on a document: its page indices were the document's.
        let with_document = Pages::restore(None, 4, 2);
        assert_eq!(
            with_document.layout(),
            &[
                page(Page::Document(0)),
                page(Page::Document(1)),
                page(Page::Document(2)),
                page(Page::Document(3))
            ]
        );

        // Written on blank sheets: as many blanks as the ink needs, and never none.
        let blanks = Pages::restore(None, 0, 3);
        assert_eq!(blanks.layout(), &[blank(), blank(), blank()]);
        assert_eq!(Pages::restore(None, 0, 0).len(), 1);

        // A saved layout is kept, and extended only as far as the ink needs.
        let saved = Pages::restore(Some(vec![page(Page::Document(1)), blank()]), 3, 3);
        assert_eq!(saved.layout(), &[page(Page::Document(1)), blank(), blank()]);
        assert_eq!(
            saved.rotation(0),
            0,
            "and a layout read back is the right way up"
        );
    }

    /// A page is turned in whole quarter turns, and a turn past the fourth comes round to the first.
    #[test]
    fn a_page_turns_in_quarter_turns_and_wraps() {
        assert_eq!(turned(0, 1), 1, "clockwise from the right way up");
        assert_eq!(turned(0, -1), 3, "counter-clockwise is the other way round");
        assert_eq!(turned(3, 1), 0, "a quarter past the last turn is the first");
        assert_eq!(
            turned(1, -4),
            1,
            "a whole turn leaves the page where it was"
        );
        assert_eq!(turned(2, 9), 3);

        assert!(!swaps_axes(0), "the right way up is the shape the page is");
        assert!(swaps_axes(1), "a quarter turn lies it on its side");
        assert!(
            !swaps_axes(2),
            "upside down is the same shape the right way round"
        );
        assert!(
            swaps_axes(3),
            "and the other quarter turn is the same shape again"
        );
    }

    /// One page can be turned without the others, and every page can be turned at once.
    #[test]
    fn a_page_can_be_turned_alone_or_with_the_rest() {
        let mut pages = Pages::of_document(3);

        pages.turn(1, 1);
        assert_eq!(
            pages.rotation(0),
            0,
            "the page in front is left where it was"
        );
        assert_eq!(pages.rotation(1), 1);
        assert_eq!(pages.rotation(2), 0);

        pages.turn_all(1);
        assert_eq!(
            [pages.rotation(0), pages.rotation(1), pages.rotation(2)],
            [1, 2, 1],
            "every page moved by one, and each kept its own rotation"
        );

        pages.turn_all(-1);
        assert_eq!(
            [pages.rotation(0), pages.rotation(1), pages.rotation(2)],
            [0, 1, 0]
        );

        pages.turn(9, 1);
        assert_eq!(
            pages.rotation(9),
            0,
            "a page the note has not got is the right way up"
        );
    }

    /// A turn stays with the page it was made on when the pages around it move.
    #[test]
    fn a_turn_stays_with_its_page_when_the_pages_move() {
        let mut pages = Pages::of_document(2);
        pages.turn(0, 1);

        // A blank sheet in front of the turned page: the pages after it are renamed, and a rotation
        // is part of the page that moves.
        pages.insert(0, true);

        assert_eq!(pages.rotation(0), 0, "the new page is the right way up");
        assert_eq!(pages.rotation(1), 1, "and the turned page is still turned");

        assert_eq!(pages.remove(0), Some(0));
        assert_eq!(pages.rotation(0), 1, "deleting in front moves nothing else");
    }

    /// What this build writes is read back as it was written, and a layout from before a page could
    /// be turned is read as pages that are the right way up.
    #[test]
    fn a_stored_page_reads_back_with_its_rotation() {
        let written = r#"[{"show":{"document":3},"rotation":1},{"show":"blank","rotation":0}]"#;
        let stored: Vec<StoredPage> =
            serde_json::from_str(written).expect("what this build writes");
        let entries: Vec<PageEntry> = stored.into_iter().map(StoredPage::entry).collect();
        assert_eq!(entries, vec![page(Page::Document(3)).turned(1), blank()]);

        // The shape a note had before a page could be turned: the page alone, and nothing about a
        // turn. Reading it as the right way up is the whole of the compatibility this needs.
        let older = r#"["blank",{"document":3}]"#;
        let stored: Vec<StoredPage> = serde_json::from_str(older).expect("what an older note has");
        let entries: Vec<PageEntry> = stored.into_iter().map(StoredPage::entry).collect();
        assert_eq!(entries, vec![blank(), page(Page::Document(3))]);

        // And the shape written back out, so that a change to either side is a change to this test.
        let now = serde_json::to_string(&[page(Page::Blank).turned(3)]).expect("writing a layout");
        assert_eq!(now, r#"[{"show":"blank","rotation":3}]"#);
    }
}
