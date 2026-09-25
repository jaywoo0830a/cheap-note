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
    /// A blank sheet: its ruling and colour come from the canvas settings, as they do for a note
    /// with no document open.
    Blank,
    /// A page of the open document, by its index in that document.
    Document(usize),
}

/// The pages of a note, in reading order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pages {
    /// One entry per page.
    pages: Vec<Page>,
}

impl Default for Pages {
    /// A note with nothing open is one blank sheet: there is always a page in front of the reader.
    fn default() -> Self {
        Pages {
            pages: vec![Page::Blank],
        }
    }
}

impl Pages {
    /// A document's own pages, in its own order: what opening a PDF gives a note.
    pub fn of_document(count: usize) -> Self {
        Pages {
            pages: (0..count.max(1)).map(Page::Document).collect(),
        }
    }

    /// The pages a saved note is opened with.
    ///
    /// `layout` is the note's own list, which notes written before the list existed do not have.
    /// Those notes' page indices *were* document page indices wherever there was a document, so
    /// that is what they are read as; a note written on blank sheets gets blanks. Either way the
    /// list is then extended to cover every page that holds ink — a page with ink on it that the
    /// list does not have is ink with nowhere to be shown.
    pub fn restore(layout: Option<Vec<Page>>, document_pages: usize, ink_pages: usize) -> Self {
        let mut pages = layout.unwrap_or_else(|| {
            if document_pages > 0 {
                Pages::of_document(document_pages).pages
            } else {
                Vec::new()
            }
        });

        if pages.is_empty() {
            pages.push(Page::Blank);
        }

        while pages.len() < ink_pages {
            pages.push(Page::Blank);
        }

        Pages { pages }
    }

    /// Every page, in reading order: what a save writes out.
    pub fn layout(&self) -> &[Page] {
        &self.pages
    }

    /// How many pages there are.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// The document page shown at `index`, when that page shows one.
    pub fn document_page(&self, index: usize) -> Option<usize> {
        match self.pages.get(index) {
            Some(Page::Document(page)) => Some(*page),
            _ => None,
        }
    }

    /// Inserts a blank page before or after `index`, and answers with the page to turn to.
    ///
    /// The new page is the one the reader is looking at afterwards, because a page that has just
    /// been made is the page that is about to be written on.
    pub fn insert(&mut self, index: usize, before: bool) -> usize {
        let at = if before { index } else { index + 1 }.min(self.pages.len());
        self.pages.insert(at, Page::Blank);
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
    use super::{Page, Pages};

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
        assert_eq!(pages.layout()[0], Page::Document(0));
        assert_eq!(pages.layout()[1], Page::Blank);
        assert_eq!(pages.layout()[2], Page::Document(1), "the document's order held");

        let before = pages.insert(2, true);
        assert_eq!(before, 2);
        assert_eq!(pages.layout()[1], Page::Blank);
        assert_eq!(pages.layout()[2], Page::Blank);
    }

    /// Deleting takes the page out of the *note*: the document's later pages keep their own numbers,
    /// because that is what a document page is.
    #[test]
    fn a_deleted_document_page_leaves_the_documents_own_pages_alone() {
        let mut pages = Pages::of_document(3);

        assert_eq!(pages.remove(1), Some(1), "the page that followed takes its place");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages.layout(), &[Page::Document(0), Page::Document(2)]);
    }

    /// The last page cannot be deleted: a note with no pages has nowhere to write.
    #[test]
    fn the_last_page_is_not_deletable() {
        let mut pages = Pages::default();

        assert_eq!(pages.remove(0), None);
        assert_eq!(pages.len(), 1);
    }

    /// A note saved before the page list existed opens with the pages it must have had.
    #[test]
    fn a_note_without_a_layout_gets_one_from_what_it_holds() {
        // Written on a document: its page indices were the document's.
        let with_document = Pages::restore(None, 4, 2);
        assert_eq!(
            with_document.layout(),
            &[
                Page::Document(0),
                Page::Document(1),
                Page::Document(2),
                Page::Document(3)
            ]
        );

        // Written on blank sheets: as many blanks as the ink needs, and never none.
        let blanks = Pages::restore(None, 0, 3);
        assert_eq!(blanks.layout(), &[Page::Blank, Page::Blank, Page::Blank]);
        assert_eq!(Pages::restore(None, 0, 0).len(), 1);

        // A saved layout is kept, and extended only as far as the ink needs.
        let saved = Pages::restore(Some(vec![Page::Document(1), Page::Blank]), 3, 3);
        assert_eq!(saved.layout(), &[Page::Document(1), Page::Blank, Page::Blank]);
    }
}
