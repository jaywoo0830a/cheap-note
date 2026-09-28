//! The document's own table of contents, and the screen that lists it.
//!
//! ## What this is, and what it is not
//!
//! A PDF may carry an *outline*: a tree of titles, each pointing at a page, written by whoever made the
//! document. It is not the same thing as a bookmark (see [`crate::bookmarks`]) — a bookmark is a mark the
//! reader puts on a page of *their note*, an outline is a table of contents the document came with — but
//! the two answer the same question ("how do I get there?"), and the app shows them the same way: a
//! screen in front of the note, a list with the keyboard on it, Enter to go, Escape to come back.
//!
//! **Nothing about an outline is stored.** It belongs to the document, and a note carries its document
//! inside itself, so it is read again every time the note is opened: cheaper than remembering it, and
//! more honest besides — a note whose document was replaced arrives with the new document's contents.
//!
//! ## The two mappings
//!
//! An entry names a page of the *document*. A note is a list of its own pages, with the document's mixed
//! in among blank sheets — inserted, deleted and moved around (see [`crate::pages`]) — so every row
//! carries both: the document's page, which is what the contents says, and the note's page, which is
//! where the row goes. An entry whose document page the note does not show is listed all the same (it is
//! a line of the document's contents, and dropping it would quietly shorten them) and cannot be opened:
//! the row says so, and its menu offers only what can be done.
//!
//! ## Why a screen
//!
//! For the reason [`crate::bookmarks`] gives at length: the pen is refused by an app *state* rather than
//! by the drawing layer, so a list of places is a screen rather than a panel floating on the sheet. The
//! two screens are deliberately twins — same list, same keys, same right-click menu — because a person
//! who has used one of them has used both.

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::empty::{Empty, EmptyDescription, EmptyHeader, EmptyTitle};
use gpui_kit::component::label::Label;
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme as _, IndexPath};
use gpui_kit::*;

use crate::app::NoteApp;
use crate::bookmarks::Bookmarks;
use crate::pages::Pages;
use crate::pdfium::OutlineEntry;

/// One row of the contents: an entry of the document's outline, and where it lands in this note.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutlineRow {
    /// The title the document gives the entry, in its own words.
    pub title: String,
    /// How deep in the tree it sits: what the row's indent is drawn from.
    pub depth: usize,
    /// The page of the document it opens, when it opens one.
    pub document_page: Option<usize>,
    /// The page of the *note* that shows that document page, when the note has it: what the row opens.
    pub note_page: Option<usize>,
    /// Whether that note page is bookmarked, so that the row's menu can say what it will do.
    pub marked: bool,
}

impl OutlineRow {
    /// The second line of the row: which page of the document the entry opens, and whether this note has
    /// it.
    ///
    /// The *document's* number rather than the note's, because that is what a table of contents is
    /// written in and what a person looking at their PDF already knows; the note's own number is a
    /// different number for the same page, and this row is about the document's contents.
    fn detail(&self) -> String {
        match (self.document_page, self.note_page) {
            (Some(page), Some(_)) => format!("page {}", page + 1),
            (Some(page), None) => format!("page {} \u{b7} not in this note", page + 1),
            (None, _) => String::from("opens somewhere outside this document"),
        }
    }

    /// Whether this is the row for the page the reader is on.
    fn is_here(&self, here: usize) -> bool {
        self.note_page == Some(here)
    }
}

/// The document's table of contents, flattened for a list to draw.
///
/// Built when the screen is shown, from the tree the document was read into (see
/// [`crate::pdfium::OutlineEntry`]): the flattening is where the two mappings above are resolved, so the
/// rows a list draws are already answers rather than questions.
#[derive(Clone, Debug, Default)]
pub struct OutlinePages {
    /// The entries, parents before their children, in the document's own order.
    pub rows: Vec<OutlineRow>,
    /// The page in front of the reader, which one row says is where they are.
    pub here: usize,
    /// The document's name, for the header: what this table of contents belongs to.
    pub document: String,
}

impl OutlinePages {
    /// The rows for a document's outline, as this note shows it.
    ///
    /// `pages` is the mapping from document pages to the note's, and `bookmarks` is what the row menus
    /// need to know about the note; nothing here reads the note or the document again.
    pub fn of(
        entries: &[OutlineEntry],
        pages: &Pages,
        bookmarks: &Bookmarks,
        here: usize,
        document: String,
    ) -> Self {
        let mut rows = Vec::new();
        flatten(entries, 0, pages, bookmarks, &mut rows);

        OutlinePages {
            rows,
            here,
            document,
        }
    }

    /// How many rows the list has.
    fn count(&self) -> usize {
        self.rows.len()
    }
}

/// Flattens the tree into rows, resolving each entry's page in this note as it goes.
///
/// Depth-first, parents before their children, which is the order a table of contents is read in: the
/// indent is what says what is under what, so a list cannot hide a parent's children and still be a table
/// of contents.
fn flatten(
    entries: &[OutlineEntry],
    depth: usize,
    pages: &Pages,
    bookmarks: &Bookmarks,
    rows: &mut Vec<OutlineRow>,
) {
    for entry in entries {
        let note_page = entry
            .page
            .and_then(|document_page| pages.note_page(document_page));

        rows.push(OutlineRow {
            title: entry.title.clone(),
            depth,
            document_page: entry.page,
            note_page,
            marked: matches!(note_page, Some(page) if bookmarks.contains(page)),
        });

        flatten(&entry.children, depth + 1, pages, bookmarks, rows);
    }
}

/// The contents screen, as the app holds it: whether it is in front of the note, and its list.
///
/// The list itself — highlight, scrolling, keyboard — belongs to the component (see
/// [`OutlineDelegate`]), as on the bookmark screen. What this type adds is the two things the app has to
/// know: whether the sheet is hidden behind a screen, and whether the keyboard has been handed over.
pub struct Outline {
    /// The list that draws the document's contents.
    list: Entity<ListState<OutlineDelegate>>,
    /// Whether the screen is in front of the note.
    open: bool,
    /// Whether the keyboard still has to be put on the list, on a frame that has a window.
    pending: bool,
}

impl Outline {
    /// The screen, built once: an empty list, with the app wired to it.
    pub fn new(window: &mut Window, cx: &mut Context<NoteApp>) -> Self {
        // Taken from the *app's* context, before the list's own context exists: what the delegate holds a
        // reference to is the app, not the list it belongs to.
        let app = cx.weak_entity();
        let list = cx.new(|cx| ListState::new(OutlineDelegate::new(app), window, cx));

        // Escape means *cancel* to the list — it takes the highlight off the row — and to this screen it
        // is the way back to the note, exactly as it is on the bookmark screen. The list eats that key, so
        // the list is where the screen is closed from.
        cx.subscribe(&list, |app, _, event: &ListEvent, cx| {
            if matches!(event, ListEvent::Cancel) {
                app.hide_outline(cx);
            }
        })
        .detach();

        Outline {
            list,
            open: false,
            pending: false,
        }
    }

    /// Whether the screen is in front of the note.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Puts the screen in front of the note, holding the document's contents.
    ///
    /// The rows are composed by the caller on the way in, and the highlight starts on the row for the page
    /// in front when there is one (see [`OutlineDelegate::start`]).
    pub fn show(&mut self, pages: OutlinePages, cx: &mut Context<NoteApp>) {
        self.list.update(cx, |state, cx| {
            state.delegate_mut().start(pages);
            cx.notify();
        });

        self.open = true;
        self.pending = true;
    }

    /// Tells a screen that is already up what the note says now, leaving the highlight where it was.
    ///
    /// What bookmarking a row's page from the screen comes through: the row is still the row the reader
    /// is on, and only what its menu will do has changed.
    pub fn refresh(&mut self, pages: OutlinePages, cx: &mut Context<NoteApp>) {
        self.list.update(cx, |state, cx| {
            state.delegate_mut().set(pages);
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

    /// The *note* page the list has highlighted, which is what a bookmark command acts on while this
    /// screen is up. `None` for a row that opens nothing — an entry that leaves the document, or one whose
    /// page this note does not show.
    pub fn highlighted(&self, cx: &Context<NoteApp>) -> Option<usize> {
        self.list.read(cx).delegate().page_under_highlight()
    }
}

impl Outline {
    /// The screen: the document's contents, and the way back to the note.
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
            .id("outline")
            .size_full()
            .bg(desk)
            .text_color(ink)
            // The chords that are the app's rather than the list's: `Ctrl+B` still marks the page the list
            // is pointing at. Everything else the keyboard does here belongs to the list, which has the
            // focus. `title` is the note's name, which the app passes in rather than this screen reaching
            // for it: the same arrangement the other screen uses.
            .on_key_down(
                cx.listener(|app, event: &KeyDownEvent, window, cx| {
                    app.outline_key_down(event, window, cx)
                }),
            )
            .child(self.header(title, cx))
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

    /// The top of the screen: what this list is, and where it came from.
    fn header(&self, title: &str, cx: &mut Context<NoteApp>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (count, document) = {
            let delegate = self.list.read(cx).delegate();
            (delegate.pages.count(), delegate.pages.document.clone())
        };

        // A table of contents belongs to a document, so the caption says which one — while the *note's*
        // name is what the screen is titled with, because that is the thing a person is looking at.
        let caption = if count == 0 {
            String::from("this document carries no contents of its own")
        } else {
            format!(
                "{} in {} \u{b7} Enter goes to the row in front, Esc comes back to the note",
                if count == 1 {
                    String::from("1 entry")
                } else {
                    format!("{count} entries")
                },
                if document.is_empty() {
                    "this document"
                } else {
                    &document
                }
            )
        };

        v_flex()
            .gap_1()
            .px(px(crate::home::SIDE_MARGIN))
            .pt(px(40.0))
            .pb(px(18.0))
            .child(
                Label::new(if title.is_empty() {
                    String::from("Contents")
                } else {
                    format!("Contents \u{2014} {title}")
                })
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
                Label::new("right-click a row to bookmark its page, or take that bookmark off")
                    .text_sm()
                    .text_color(muted),
            );

        if !message.is_empty() {
            bar = bar.right(Label::new(message.to_string()).text_sm().text_color(muted));
        }

        bar.into_any_element()
    }
}


/// What the list draws: one row per entry of the document's contents.
///
/// The list asks this for a count, for a row, and for what opening a row means; the highlight, the
/// scrolling and the keys are its own (see [`ListDelegate`]).
pub struct OutlineDelegate {
    /// The document's contents, as this note shows them.
    pages: OutlinePages,
    /// The row the list has highlighted.
    selected: Option<IndexPath>,
    /// The app, for the two things a row can do. Weak: this does not own the app.
    app: WeakEntity<NoteApp>,
}

impl OutlineDelegate {
    /// A delegate with nothing in it yet: the rows arrive with [`Self::start`].
    fn new(app: WeakEntity<NoteApp>) -> Self {
        OutlineDelegate {
            pages: OutlinePages::default(),
            selected: None,
            app,
        }
    }

    /// Replaces what the list holds, leaving the highlight on the row it was on when that row is still
    /// there.
    ///
    /// What a list that is only being *refreshed* wants: bookmarking a row's page must not move the
    /// reader's place in the contents to some other chapter.
    fn set(&mut self, pages: OutlinePages) {
        self.selected = self
            .selected
            .filter(|selected| selected.row < pages.rows.len());
        self.pages = pages;
    }

    /// Replaces what the list holds, and puts the highlight on the row for the page in front.
    ///
    /// That row when there is one — the chapter the reader is inside — and the first row when there is
    /// not, because Enter with nothing highlighted is Enter on nothing. A page can hold several entries (a
    /// section that begins where its chapter did), and the *first* of them is the one this picks: the top
    /// of that part is what a reader looking at the page is inside.
    fn start(&mut self, pages: OutlinePages) {
        let start = pages
            .rows
            .iter()
            .position(|row| row.is_here(pages.here))
            .or_else(|| (!pages.rows.is_empty()).then_some(0));

        self.selected = start.map(IndexPath::new);
        self.pages = pages;
    }

    /// The *note* page under the highlight, when that row opens one.
    fn page_under_highlight(&self) -> Option<usize> {
        let row = self.pages.rows.get(self.selected?.row)?;

        row.note_page
    }
}


impl ListDelegate for OutlineDelegate {
    type Item = ListItem;

    /// How many rows the list has: one per entry of the contents.
    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.pages.count()
    }

    /// One row: the entry's title, indented by its depth, and where it goes.
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = self.pages.rows.get(ix.row)?;
        let theme = cx.theme();
        let (muted, ink) = (theme.muted_foreground, theme.foreground);
        let highlighted = self.selected == Some(ix);
        let here = row.is_here(self.pages.here);
        let opens = row.note_page.is_some();

        // A row that opens nothing is drawn in the muted colour rather than marked in some way of its own:
        // it is a line of the document's contents, and it is not somewhere the reader can go. Why is on
        // the row's second line, which is the part a person actually needs.
        let title_color = if highlighted {
            theme.accent_foreground
        } else if opens {
            ink
        } else {
            muted
        };

        let title = row.title.clone();
        let detail = row.detail();
        // The indent is capped: a document may nest its contents deeper than a list can show without
        // pushing the titles off the card, and a title that has been indented out of the window is a row
        // nobody can read.
        let indent = px(14.0 * row.depth.min(6) as f32);
        let page = row.note_page;
        let marked = row.marked;

        let row = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_4()
            // The indent is the tree: a table of contents is read by what is under what, and the same
            // titles in one flat column would be a different document's contents.
            .pl(indent)
            .context_menu({
                let app = self.app.clone();
                move |menu: PopupMenu, _window: &mut Window, _cx: &mut Context<PopupMenu>| {
                    row_menu(menu, app.clone(), page, marked)
                }
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new(title).text_color(title_color))
                    .child(Label::new(detail).text_sm().text_color(muted)),
            )
            // Said only for the page the reader is on, as on the bookmark screen: the one row of the
            // contents that is not about somewhere else.
            .child(Label::new(if here { "you are here" } else { "" }).text_sm().text_color(muted));

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

    /// A row was opened — Enter, or a click: go to the page it names.
    ///
    /// A row that names nothing does nothing, because there is nowhere to go: the row says so on its own
    /// line, and its menu's one item is disabled.
    fn confirm(
        &mut self,
        _secondary: bool,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        let Some(page) = self.page_under_highlight() else {
            return;
        };

        self.app
            .update(cx, |app, cx| app.go_to_entry(page, cx))
            .ok();
    }

    /// There is nothing in the list: say so, and say what can still be done.
    ///
    /// Reached only if the contents are empty *while the screen is up* — the button that opens it is not
    /// offered for a document without one (see [`crate::pdf::PdfDocumentView::has_outline`]) — so this is
    /// a sentence for a state that should not happen, rather than the common case.
    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        Empty::new()
            .header(
                EmptyHeader::new()
                    .title(EmptyTitle::new().child("No contents"))
                    .description(EmptyDescription::new().child(
                        "This document was not made with a table of contents. Pages of the note can still be marked with Ctrl+B, and listed from the bar.",
                    )),
            )
            .into_any_element()
    }
}


/// What a contents row can do, on the menu a right-click opens.
///
/// Both of them are about the *note*: a document's contents cannot be edited from here — the document is
/// inside the note, unmodified — so what a row offers is where it goes, and whether the page it goes to is
/// kept in the bookmark list.
fn row_menu(
    menu: PopupMenu,
    app: WeakEntity<NoteApp>,
    page: Option<usize>,
    marked: bool,
) -> PopupMenu {
    let go = {
        let app = app.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            if let Some(page) = page {
                app.update(cx, |app, cx| app.go_to_entry(page, cx)).ok();
            }
        }
    };

    let keep = {
        let app = app.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            if let Some(page) = page {
                app.update(cx, |app, cx| app.toggle_page_bookmark(page, cx))
                    .ok();
            }
        }
    };

    let mut menu = menu.item(
        PopupMenuItem::new("Go to this page")
            .icon(IconName::BookOpen)
            // An entry that opens nothing cannot be gone to, and being greyed out is this menu saying what
            // the row's second line says in words.
            .disabled(page.is_none())
            .on_click(go),
    );

    if page.is_some() {
        menu = menu.separator().item(
            PopupMenuItem::new(if marked {
                "Take the bookmark off"
            } else {
                "Bookmark this page"
            })
            .icon(if marked {
                IconName::BookmarkMinus
            } else {
                IconName::Bookmark
            })
            // The label is written from the state the list drew, so a *toggle* gives exactly what the label
            // says: the two can only disagree if the list was refreshed in between, which is what the app
            // does after every mark.
            .on_click(keep),
        );
    }

    menu
}



#[cfg(test)]
mod tests {
    // Imported by name rather than by glob: `use super::*` would bring GPUI's own `test` macro into
    // scope and shadow the attribute this module needs.
    use super::OutlinePages;
    use crate::bookmarks::Bookmarks;
    use crate::pages::{Page, Pages};
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
            Some(vec![Page::Document(0), Page::Blank, Page::Document(2)]),
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
        assert!(pages.rows[0].is_here(1), "and it is the page the reader is on");

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
}
