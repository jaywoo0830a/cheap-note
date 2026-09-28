//! The pages a note has marked, and the screen that lists them.
//!
//! ## What a bookmark is
//!
//! A page of the note — the number the page pill shows, counted the way the note's own page list
//! counts (see [`crate::pages`]) — and nothing else. No name, no colour, no text attached to it: this
//! is the list a reader makes while reading, and the one question it answers is *where*.
//!
//! A page is marked or it is not, so [`Bookmarks::toggle`] is the only edit there is: two marks on one
//! page are impossible by construction rather than by a check. The other two edits belong to the
//! note's own page commands — inserting a page renames the marks after it, deleting one takes its mark
//! with it — and [`crate::store::NoteStore`] does the same to the rows on disk in the same
//! transaction, so the list in memory and the list in the note are one list kept in two places.
//!
//! The order is always the note's reading order. A mark carries no position of its own, so a list
//! cannot disagree with the pages it lists, and there is no order for the app to store or repair.
//!
//! ## Why the list is a screen
//!
//! The list could have been a panel floating over the sheet, and it is a screen instead for a reason
//! that is not aesthetic: the pen is refused by the app's own rule, not by the interface toolkit (see
//! [`crate::ink::InkTransform`] and [`crate::app::NoteApp::consume_ink`]) — the pen is a raw
//! `WM_POINTER` stream and nothing in the drawing layer can stand between it and the page. The bar is
//! therefore refused by a *line* and a screen by a *state*; a panel would have had to add a rectangle
//! to the ink rule, and a rectangle that is wrong by a frame is ink drawn under a control.
//!
//! So the marked pages are a screen, drawn the way [`crate::home`] draws the notes that were opened: a
//! list with the keyboard on it, a right-click menu for what a row can do, and Escape to go back to
//! the note — which is also the one place a person already looks for a *list* in this app.

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::empty::{Empty, EmptyDescription, EmptyHeader, EmptyTitle};
use gpui_kit::component::label::Label;
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath};
use gpui_kit::*;

use crate::app::NoteApp;
use crate::pages::Pages;

/// The pages of a note that have a bookmark, in the note's reading order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bookmarks {
    /// The marked pages, ascending, without repeats.
    pages: Vec<usize>,
}

impl Bookmarks {
    /// The marks a note holds: what [`crate::store::NoteStore::bookmarks`] answers, in the order it
    /// answers in.
    ///
    /// Sorted and deduplicated here rather than trusted: the rows belong to the note, a note is a file
    /// a person may edit, and a list that arrived unsorted would be a list nothing else here could
    /// assume anything about.
    pub fn of(pages: impl IntoIterator<Item = u64>) -> Self {
        let mut pages: Vec<usize> = pages.into_iter().map(|page| page as usize).collect();
        pages.sort_unstable();
        pages.dedup();

        Bookmarks { pages }
    }

    /// The marked pages, in reading order.
    pub fn pages(&self) -> &[usize] {
        &self.pages
    }

    /// How many pages are marked.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// Whether nothing is marked.
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Whether this page is one of them.
    pub fn contains(&self, page: usize) -> bool {
        self.pages.binary_search(&page).is_ok()
    }

    /// Marks the page, or takes the mark off it, and answers which of the two it did.
    ///
    /// The answer is what the caller reports and what it writes down, so the message and the note's
    /// row come from one fact rather than from asking the list twice — and so that a toggle that
    /// arrives twice cannot report the state it *had*.
    pub fn toggle(&mut self, page: usize) -> bool {
        match self.pages.binary_search(&page) {
            Ok(at) => {
                self.pages.remove(at);
                false
            }
            Err(at) => {
                self.pages.insert(at, page);
                true
            }
        }
    }

    /// Marks the page, or takes the mark off it, whichever `mark` asks for — answering whether that
    /// changed anything.
    ///
    /// The answer is what keeps a command that found the state it wanted from reporting a change it did
    /// not make: "take the bookmark off" on a page with no bookmark is nothing at all.
    pub fn set(&mut self, page: usize, mark: bool) -> bool {
        if self.contains(page) == mark {
            return false;
        }

        self.toggle(page);
        true
    }

    /// Renames the marks after a page inserted at `at`.
    ///
    /// A mark names a page's *position*, so a page inserted in front of a marked one puts that mark one
    /// place further along — which is what keeps the mark on the page it was made on rather than on the
    /// number it was made at. This is the memory's half of what
    /// [`crate::store::NoteStore::insert_page`] does to the rows.
    pub fn inserted_at(&mut self, at: usize) {
        for page in &mut self.pages {
            if *page >= at {
                *page += 1;
            }
        }
    }

    /// Renames the marks for the page at `at` being deleted: that page's mark goes with it, and the
    /// marks after it close the gap.
    pub fn removed_at(&mut self, at: usize) {
        self.pages.retain(|page| *page != at);

        for page in &mut self.pages {
            if *page > at {
                *page -= 1;
            }
        }
    }

    /// The first marked page after `page`, for the keyboard's step forward.
    pub fn next_from(&self, page: usize) -> Option<usize> {
        self.pages.iter().copied().find(|mark| *mark > page)
    }

    /// The last marked page before `page`, for the keyboard's step back.
    pub fn previous_from(&self, page: usize) -> Option<usize> {
        self.pages.iter().rev().copied().find(|mark| *mark < page)
    }
}

/// What the list needs to draw, gathered once: the note, as the marked pages see it.
///
/// One value, pushed in when the screen is shown, rather than the delegate reaching back into the app
/// for its own rows: a list that reads the app while it draws is a list whose rows can change under it
/// between the count and the row.
#[derive(Clone, Debug, Default)]
pub struct MarkedPages {
    /// The marked pages, in reading order: one row each.
    pub marked: Vec<usize>,
    /// The page in front of the reader, which the list points at when it opens.
    pub here: usize,
    /// How many pages the note has, for "page 3 of 24".
    pub total: usize,
    /// What each page of the note shows (see [`crate::pages`]).
    pub pages: Pages,
    /// The name of the document the note was written on, for a row that is a page of one.
    pub document: String,
}

impl MarkedPages {
    /// What one marked page is, as a row says it: the document page it shows, or the blank sheet it is.
    ///
    /// The note's page list is what decides, and the answer names the *document's* page: a note written
    /// on a document can have pages inserted, deleted and moved, so the note's own number is not the
    /// document's number — reading one out as the other would send a person to the wrong page of their
    /// PDF. See [`crate::pages`].
    fn shows(&self, page: usize) -> String {
        match self.pages.document_page(page) {
            Some(document_page) => format!(
                "{} \u{2014} page {}",
                if self.document.is_empty() {
                    "the document"
                } else {
                    &self.document
                },
                document_page + 1
            ),
            None => String::from("a blank sheet"),
        }
    }

    /// How many rows the list has.
    fn count(&self) -> usize {
        self.marked.len()
    }
}

/// The bookmark list, as the app holds it: whether it is in front of the note, and its list.
///
/// The list itself — its highlight, its scrolling, its keyboard — belongs to the component (see
/// [`MarksDelegate`]), which is the arrangement the home screen uses too. What this type adds is the
/// two things the app has to know about it: whether the sheet is hidden behind a screen, and whether
/// the keyboard has been handed to the list yet.
pub struct Marks {
    /// The list that draws the marked pages.
    list: Entity<ListState<MarksDelegate>>,
    /// Whether the screen is in front of the note.
    open: bool,
    /// Whether the keyboard still has to be put on the list, on a frame that has a window.
    pending: bool,
}

impl Marks {
    /// The screen, built once: an empty list, with the app wired to it.
    pub fn new(window: &mut Window, cx: &mut Context<NoteApp>) -> Self {
        // Taken from the *app's* context, before the list's own context exists: what the delegate holds a
        // reference to is the app, not the list it belongs to.
        let app = cx.weak_entity();
        let list = cx.new(|cx| ListState::new(MarksDelegate::new(app), window, cx));

        // Escape means *cancel* to the list — it takes the highlight off the row — and to this screen it
        // is the way back to the note. The list is where that key arrives, so this is where the screen is
        // closed from, rather than the screen guessing at a keystroke the focused list is eating.
        cx.subscribe(&list, |app, _, event: &ListEvent, cx| {
            if matches!(event, ListEvent::Cancel) {
                app.hide_marks(cx);
            }
        })
        .detach();

        Marks {
            list,
            open: false,
            pending: false,
        }
    }

    /// Whether the screen is in front of the note.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Puts the screen in front of the note, holding `marked`.
    ///
    /// The rows are composed by the caller on the way in rather than kept in step afterwards: the screen
    /// is only ever *shown* from a state the app has just built, so what it draws cannot be a frame
    /// behind what the note says.
    pub fn show(&mut self, marked: MarkedPages, cx: &mut Context<NoteApp>) {
        self.list.update(cx, |state, cx| {
            state.delegate_mut().start(marked);
            cx.notify();
        });

        self.open = true;
        self.pending = true;
    }

    /// Tells a list that is already up what the note says now, leaving the highlight where it was.
    ///
    /// What marking a row off a list that is being looked at comes through: the rows change under the
    /// reader, and their place in the list is the one thing that must not move while they are reading it.
    pub fn refresh(&mut self, marked: MarkedPages, cx: &mut Context<NoteApp>) {
        self.list.update(cx, |state, cx| {
            state.delegate_mut().set(marked);
            cx.notify();
        });
    }

    /// Takes the screen away, leaving the note exactly as it was.
    pub fn hide(&mut self) {
        self.open = false;
    }

    /// Hands the keyboard to the list, on the first frame that has a window to hand it with.
    pub fn settle(&mut self, window: &mut Window, cx: &mut Context<NoteApp>) {
        if !self.pending {
            return;
        }

        self.pending = false;
        self.list.update(cx, |state, cx| state.focus(window, cx));
    }

    /// The page the list has highlighted, for the commands that act on the row in front of the reader.
    ///
    /// While this screen is up, the row the reader is *on* is the one under the highlight rather than the
    /// page behind the screen: marking the page behind a list of bookmarks is the one thing the key would
    /// be understood not to do.
    pub fn highlighted(&self, cx: &Context<NoteApp>) -> Option<usize> {
        self.list.read(cx).delegate().page_under_highlight()
    }

    /// How many pages the list is showing.
    pub fn count(&self, cx: &Context<NoteApp>) -> usize {
        self.list.read(cx).delegate().marked.count()
    }

    /// The screen: the marked pages, and the way back to the note.
    pub fn view(&self, title: &str, message: &str, cx: &mut Context<NoteApp>) -> AnyElement {
        let theme = cx.theme();
        let (desk, ink, card, hairline, radius) = (
            theme.background,
            theme.foreground,
            theme.title_bar,
            theme.border,
            theme.radius_lg,
        );

        v_flex()
            .id("marks")
            .size_full()
            .bg(desk)
            .text_color(ink)
            // The chords that are the app's rather than the list's: `Ctrl+B` still marks the page the
            // list is pointing at. Everything else the keyboard does here belongs to the list, which has
            // the focus.
            .on_key_down(cx.listener(|app, event: &KeyDownEvent, window, cx| {
                app.marks_key_down(event, window, cx)
            }))
            .child(self.header(title, cx))
            // The rows are a card on the desk, as they are on the home screen: one surface with a
            // hairline round it, so a list of places reads as a page rather than as another menu.
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .px(px(crate::home::SIDE_MARGIN))
                    .child(
                        v_flex()
                            .size_full()
                            .rounded(radius)
                            .bg(card)
                            .border_1()
                            .border_color(hairline)
                            .shadow_md()
                            .child(List::new(&self.list).size_full()),
                    ),
            )
            .child(self.footer(message, cx))
            .into_any_element()
    }

    /// The top of the screen: what this list is, and how it is used.
    fn header(&self, title: &str, cx: &mut Context<NoteApp>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let count = self.count(cx);

        let caption = if count == 0 {
            String::from(
                "nothing is marked yet \u{2014} mark a page with Ctrl+B, or with the bookmark button on the bar",
            )
        } else {
            format!(
                "{} marked in {} \u{b7} Enter goes to the row in front, Esc comes back to the note",
                if count == 1 {
                    String::from("1 page")
                } else {
                    format!("{count} pages")
                },
                if title.is_empty() { "this note" } else { title }
            )
        };

        v_flex()
            .gap_1()
            .px(px(crate::home::SIDE_MARGIN))
            .pt(px(40.0))
            .pb(px(18.0))
            .child(
                Label::new("Bookmarks")
                    .text_lg()
                    .font_weight(FontWeight::BOLD),
            )
            .child(Label::new(caption).text_sm().text_color(muted))
            .into_any_element()
    }

    /// The bottom line: what a row's menu holds, and whatever the app last said.
    fn footer(&self, message: &str, cx: &mut Context<NoteApp>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hairline = theme.border;

        let mut bar = StatusBar::new()
            .w_full()
            .px(px(crate::home::SIDE_MARGIN))
            .py(px(8.0))
            .bg(theme.transparent)
            .border_t_1()
            .border_color(hairline)
            .left(
                Label::new("right-click a row to take its bookmark off")
                    .text_sm()
                    .text_color(muted),
            );

        if !message.is_empty() {
            bar = bar.right(Label::new(message.to_string()).text_sm().text_color(muted));
        }

        bar.into_any_element()
    }
}

/// What the list draws: one row per marked page of the note.
///
/// The list asks this for a count, for a row, and for what opening a row means; the highlight, the
/// scrolling and the keys are its own (see [`ListDelegate`]).
pub struct MarksDelegate {
    /// The marked pages, and what the note says about each of them.
    marked: MarkedPages,
    /// The row the list has highlighted.
    selected: Option<IndexPath>,
    /// The app, for the two things a row can do. Weak: this does not own the app.
    app: WeakEntity<NoteApp>,
}

impl MarksDelegate {
    /// A delegate with nothing marked in it yet: the rows arrive with [`Self::set`].
    fn new(app: WeakEntity<NoteApp>) -> Self {
        MarksDelegate {
            marked: MarkedPages::default(),
            selected: None,
            app,
        }
    }

    /// Replaces what the list holds, leaving the highlight on the row it was on when that row is still
    /// there.
    ///
    /// What a list that is only being *refreshed* wants: taking a mark off a row must not move the
    /// reader's place to some other page. The highlight is dropped only when the row it was on has gone.
    fn set(&mut self, marked: MarkedPages) {
        self.selected = self
            .selected
            .filter(|selected| selected.row < marked.marked.len());
        self.marked = marked;
    }

    /// Replaces what the list holds, and points the highlight at the page the reader is on.
    ///
    /// What *showing* the screen wants: a list opened from a marked page should be pointing at it, and a
    /// list opened from an unmarked one should be pointing at something, because Enter with nothing
    /// highlighted is Enter on nothing.
    fn start(&mut self, marked: MarkedPages) {
        let start = marked
            .marked
            .iter()
            .position(|page| *page == marked.here)
            .or_else(|| (!marked.marked.is_empty()).then_some(0));

        self.selected = start.map(IndexPath::new);
        self.marked = marked;
    }

    /// The marked page under the highlight, if the list has one.
    fn page_under_highlight(&self) -> Option<usize> {
        self.marked.marked.get(self.selected?.row).copied()
    }
}

impl ListDelegate for MarksDelegate {
    type Item = ListItem;

    /// How many rows the list has: one per marked page.
    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.marked.count()
    }

    /// One row: the mark, which page it is, what that page shows, and whether it is the page in front.
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let page = *self.marked.marked.get(ix.row)?;
        let theme = cx.theme();
        let (muted, accent) = (theme.muted_foreground, theme.accent);
        let highlighted = self.selected == Some(ix);
        let here = page == self.marked.here;

        let number = format!("Page {} of {}", page + 1, self.marked.total.max(1));
        let shows = self.marked.shows(page);

        let row = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_4()
            // A right-click is what can be done with *this* row: go to it, or take its mark off. The menu
            // is the library's, so the keyboard reaches it too — arrows move, Enter chooses, Escape
            // closes — and it acts on the row it was opened on rather than on the highlight.
            .context_menu({
                let app = self.app.clone();
                move |menu: PopupMenu, _window: &mut Window, _cx: &mut Context<PopupMenu>| {
                    row_menu(menu, app.clone(), page)
                }
            })
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Icon::new(IconName::BookmarkCheck).text_color(if highlighted {
                            theme.accent_foreground
                        } else {
                            accent
                        }),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new(number))
                            .child(Label::new(shows).text_sm().text_color(muted)),
                    ),
            )
            // Said only for the page the reader is on: it is the one row of the list that is not about
            // somewhere else.
            .child(
                Label::new(if here { "you are here" } else { "" })
                    .text_sm()
                    .text_color(muted),
            );

        Some(ListItem::new(ix).selected(highlighted).child(row))
    }

    /// The list moved its highlight.
    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
    }

    /// A row was opened — Enter, or a click: go to that page.
    ///
    /// Unlike the home screen there is no difference between choosing a row and opening it: a list of
    /// places has one thing to do with a row, and asking for two clicks would make the first one mean
    /// nothing.
    fn confirm(
        &mut self,
        _secondary: bool,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        let Some(page) = self.page_under_highlight() else {
            return;
        };

        self.app.update(cx, |app, cx| app.go_to_mark(page, cx)).ok();
    }

    /// There is nothing marked: say what the list is for.
    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        Empty::new()
            .header(
                EmptyHeader::new()
                    .title(EmptyTitle::new().child("No page is marked"))
                    .description(EmptyDescription::new().child(
                        "Mark the page you are on \u{2014} Ctrl+B, or the bookmark button on the bar \u{2014} and it waits here, a place to come back to.",
                    )),
            )
            .into_any_element()
    }
}

/// What a marked page's row can do, on the menu a right-click opens.
///
/// The page is captured rather than looked up by highlight, so a menu opened on a row acts on that row
/// even if the highlight has moved since. *Take the bookmark off* comes second and says what it does:
/// the page itself, and everything written on it, is not touched.
fn row_menu(menu: PopupMenu, app: WeakEntity<NoteApp>, page: usize) -> PopupMenu {
    let go = {
        let app = app.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            app.update(cx, |app, cx| app.go_to_mark(page, cx)).ok();
        }
    };

    let unmark = {
        let app = app.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            app.update(cx, |app, cx| app.unmark_page(page, cx)).ok();
        }
    };

    menu.item(
        PopupMenuItem::new("Go to this page")
            .icon(IconName::BookOpen)
            .on_click(go),
    )
    .separator()
    .item(
        PopupMenuItem::new("Take the bookmark off")
            .icon(IconName::BookmarkMinus)
            .on_click(unmark),
    )
}

#[cfg(test)]
mod tests {
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
}
