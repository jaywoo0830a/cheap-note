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
//! shows. The ink is keyed by the *position in this list* — see the app's `Notes` — so inserting
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
///
/// ## Why this is a type and not a `u8`
///
/// It was `pub type Quarters = u8` once, and a type alias is a comment: nothing stopped a rotation of
/// `7` from being stored in a page and drawn in a way no page can actually be. A newtype is the same
/// byte in memory ([`Self::NEWTYPE_REPRESENTATION`] says so out loud) and makes the four the only
/// four values that can be made. Reading a value that is not one of them — from a note somebody
/// edited, or from a `u8` this build did not write — is [`Self::from_turns`]'s job, and it wraps
/// rather than refuses, which is the same rule [`turned`] follows for a command that turns a page.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct Quarters(u8);

impl Quarters {
    /// That this type is exactly a `u8` in memory, as a fact a reader can check.
    ///
    /// `#[repr(transparent)]` above is the claim; this is the constant that makes the claim part of
    /// the API. A build that changed the field's type would fail to compile here rather than quietly
    /// change what a note's stored bytes mean.
    pub const NEWTYPE_REPRESENTATION: () = assert!(std::mem::size_of::<Quarters>() == 1);

    /// The page the right way up.
    pub const UPRIGHT: Quarters = Quarters(0);

    /// Every way up a page can be, in the order a turn takes them.
    pub const ALL: [Quarters; 4] = [Quarters(0), Quarters(1), Quarters(2), Quarters(3)];

    /// `turns` quarter turns clockwise, wrapped into `0..=3`.
    ///
    /// The constructor, and the reason the four are the only four: `rem_euclid` is what makes
    /// `-1` the rotation before upright rather than a `u8` underflow.
    pub const fn from_turns(turns: i32) -> Self {
        Quarters(turns.rem_euclid(4) as u8)
    }

    /// The number as it is stored: a quarter turn count, `0..=3`.
    ///
    /// Named rather than `.0` at every call site, because the number is what goes into a note and
    /// into a PDF's `/Rotate`, and the places that need it should read as such.
    pub const fn turns(self) -> u8 {
        self.0
    }

    /// Whether a page at this rotation is drawn the other way round: a quarter turn swaps its width
    /// and its height.
    ///
    /// A method rather than the free [`swaps_axes`], which stays for callers that have the number
    /// and not the type.
    pub const fn swaps_axes(self) -> bool {
        self.0 % 2 == 1
    }

    /// This rotation `turns` quarter turns clockwise.
    ///
    /// The method the free [`turned`] is the `u8` spelling of.
    pub const fn turned(self, turns: i32) -> Self {
        Quarters::from_turns(self.0 as i32 + turns)
    }

    /// A sheet of paper `size`, as a page at this rotation is drawn: a quarter turn swaps its width and
    /// its height, and the two other turns leave it as it was.
    ///
    /// The one place the swap is written. It was three — a PDF page's display size, the sheet the app
    /// lays out, and the box a reading is checked against — and the three had to agree or a turned page
    /// put ink on the desk past its edges. Every caller that has a paper size *and* a rotation asks here.
    pub const fn draws(self, size: (f32, f32)) -> (f32, f32) {
        if self.swaps_axes() {
            (size.1, size.0)
        } else {
            size
        }
    }
}

impl From<Quarters> for u8 {
    fn from(quarters: Quarters) -> Self {
        quarters.0
    }
}

impl From<Quarters> for f32 {
    /// The turn as a float, for the renderers that take one.
    fn from(quarters: Quarters) -> Self {
        f32::from(quarters.0)
    }
}

impl std::fmt::Display for Quarters {
    /// The number, because that is how a reader says it and how a note stores it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// `turns` quarter turns from `rotation`, wrapped into `0..=3`.
///
/// Wrapped rather than clamped: four turns is the page it started as, and a command that says "turn
/// clockwise" must not leave a page at a rotation that is not one of the four.
pub fn turned(rotation: Quarters, turns: i32) -> Quarters {
    rotation.turned(turns)
}

/// Whether a page at this rotation is drawn the other way round: a quarter turn swaps its width and
/// its height.
pub fn swaps_axes(rotation: Quarters) -> bool {
    rotation.swaps_axes()
}

/// One page of a note: what it shows, and which way up it is.
///
/// ## Why `rotation` is not a public field
///
/// It was, and that made the one invariant a page has — that it is one of the four ways up — a rule
/// that only [`Pages`] followed. A `PageEntry` could be built by hand with `rotation: 7` and handed to
/// a renderer, which is the same hole `Quarters` itself closed for the *type* one level down: a value
/// that says it is a rotation and is not. Reading it is [`Self::rotation`], which answers
/// [`Quarters`]; writing it is [`Self::turned`], which is [`Quarters::turned`] and therefore wraps.
/// There is no other way in, so a page that exists is a page that can be drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageEntry {
    /// What the page shows.
    pub show: Page,
    /// Which way up it is, in quarter turns clockwise. Read with [`Self::rotation`].
    ///
    /// Private, so every value that reaches it has come through [`Quarters`]. Serde still reads and
    /// writes the bare number it always did (see [`Self::rotation`]'s tests).
    ///
    /// Defaulted rather than required: a note written by a build that had no rotation has every page
    /// the right way up, and says nothing about the field at all.
    #[serde(default)]
    rotation: Quarters,
}

impl PageEntry {
    /// A page showing `show`, the right way up.
    pub fn new(show: Page) -> Self {
        PageEntry {
            show,
            rotation: Quarters::UPRIGHT,
        }
    }

    /// The same page, turned `turns` quarter turns clockwise.
    pub fn turned(self, turns: i32) -> Self {
        PageEntry {
            rotation: self.rotation.turned(turns),
            ..self
        }
    }

    /// Which way up the page is, in quarter turns clockwise.
    ///
    /// The only way to read the rotation, so the number a caller gets is always one of the four.
    pub fn rotation(self) -> Quarters {
        self.rotation
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
        self.pages
            .get(index)
            .map_or(Quarters::UPRIGHT, |page| page.rotation)
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

    /// Whether the document has no pages at all.
    ///
    /// A note always has at least one, so this is `false` in every state a reader can reach — it exists
    /// because a `len` on a collection is read beside its `is_empty` and a caller that has one will want
    /// the other. Clippy asks for the pair; the honest answer here is that the pair is never both true.
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// The document page shown at `index`, when that page shows one.
    ///
    /// The two "may not be there" are chained rather than matched on together: the index may be past
    /// the end of the list, and the page it names may be a blank sheet. [`Option::and_then`] says
    /// exactly that — each step either has an answer or the answer is `None` — and it keeps the
    /// question ("does this page show a document page, and which?") in one expression instead of a
    /// `match` with a `None` arm for each way the answer can be absent.
    pub fn document_page(&self, index: usize) -> Option<usize> {
        self.pages.get(index).and_then(|page| match page.show {
            Page::Document(page) => Some(page),
            Page::Blank => None,
        })
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

    /// Inserts a blank page before or after `index`, and answers with where the blank page went.
    ///
    /// A page inserted *after* `index` is the one the reader is looking at afterwards, because a page
    /// that has just been made is the page that is about to be written on. A page inserted *before*
    /// `index` is not: the reader stays on the page they were reading, which is now one position along,
    /// so the caller follows `index + 1` rather than turning to `at` (see the app's `page_after_insert`).
    /// It arrives the right way up either way: it is blank paper, and the sheet it is written on is the
    /// note's own.
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
    use super::{turned, Page, PageEntry, Pages, Quarters, StoredPage};

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

    /// An inserted page goes where it was asked for, and the answer is where the blank page landed.
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
            Quarters::UPRIGHT,
            "and a layout read back is the right way up"
        );
    }

    /// A page is turned in whole quarter turns, and a turn past the fourth comes round to the first.
    #[test]
    fn a_page_turns_in_quarter_turns_and_wraps() {
        assert_eq!(
            turned(Quarters::UPRIGHT, 1),
            Quarters::from_turns(1),
            "clockwise from the right way up"
        );
        assert_eq!(
            turned(Quarters::UPRIGHT, -1),
            Quarters::from_turns(3),
            "counter-clockwise is the other way round"
        );
        assert_eq!(
            turned(Quarters::from_turns(3), 1),
            Quarters::UPRIGHT,
            "a quarter past the last turn is the first"
        );
        assert_eq!(
            turned(Quarters::from_turns(1), -4),
            Quarters::from_turns(1),
            "a whole turn leaves the page where it was"
        );
        assert_eq!(turned(Quarters::from_turns(2), 9), Quarters::from_turns(3));

        assert!(
            !Quarters::UPRIGHT.swaps_axes(),
            "the right way up is the shape the page is"
        );
        assert!(
            Quarters::from_turns(1).swaps_axes(),
            "a quarter turn lies it on its side"
        );
        assert!(
            !Quarters::from_turns(2).swaps_axes(),
            "upside down is the same shape the right way round"
        );
        assert!(
            Quarters::from_turns(3).swaps_axes(),
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
            Quarters::UPRIGHT,
            "the page in front is left where it was"
        );
        assert_eq!(pages.rotation(1), Quarters::from_turns(1));
        assert_eq!(pages.rotation(2), Quarters::UPRIGHT);

        let turns = |pages: &Pages| [0, 1, 2].map(|index| pages.rotation(index).turns());

        pages.turn_all(1);
        assert_eq!(
            turns(&pages),
            [1, 2, 1],
            "every page moved by one, and each kept its own rotation"
        );

        pages.turn_all(-1);
        assert_eq!(turns(&pages), [0, 1, 0]);

        pages.turn(9, 1);
        assert_eq!(
            pages.rotation(9),
            Quarters::UPRIGHT,
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

        assert_eq!(
            pages.rotation(0),
            Quarters::UPRIGHT,
            "the new page is the right way up"
        );
        assert_eq!(
            pages.rotation(1),
            Quarters::from_turns(1),
            "and the turned page is still turned"
        );

        assert_eq!(pages.remove(0), Some(0));
        assert_eq!(
            pages.rotation(0),
            Quarters::from_turns(1),
            "deleting in front moves nothing else"
        );
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

    /// A page turned on its side is drawn the other way round; the two turns that are not upside down
    /// leave its shape alone.
    ///
    /// The one place the swap is written, so this is the one place it is checked: a PDF page's display
    /// size, the sheet the app lays out, and the box a pen reading is checked against all ask here.
    #[test]
    fn a_quarter_turn_swaps_the_sheets_sides() {
        let sheet = (600.0, 800.0);

        assert_eq!(Quarters::UPRIGHT.draws(sheet), sheet);
        assert_eq!(Quarters::from_turns(1).draws(sheet), (800.0, 600.0));
        assert_eq!(Quarters::from_turns(2).draws(sheet), sheet);
        assert_eq!(Quarters::from_turns(3).draws(sheet), (800.0, 600.0));

        // Turning it twice is the shape it started as, which is why a swap and not a rotation.
        for turns in Quarters::ALL {
            assert_eq!(turns.turned(2).draws(sheet), turns.draws(sheet));
        }
    }
}
