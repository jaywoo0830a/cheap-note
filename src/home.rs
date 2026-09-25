//! The screen the app opens on: what you were writing, and the way back to it.
//!
//! ## Why a screen rather than a dialog
//!
//! The window is the app: a sheet, a floating bar, and a status line. A list of recent notes is a
//! *place to be* rather than a question to answer, and it is where the app already starts — on an
//! empty sheet, with nothing to write on. Making it a place keeps one window, one pen capture and
//! one set of pumps; a modal would have to be dismissed before the pen had anywhere to go.
//!
//! ## The list is the library's list
//!
//! The rows are a [`List`] with a [`ListDelegate`], not a hand-drawn column: the library's list owns
//! the scrolling, the highlight, the keyboard (arrows, Enter, Escape), the search box, the empty
//! state and the loading state, and it draws them in the theme the app already set. What this module
//! supplies is the *data* — one row per note the app remembers — and what a confirmation means:
//! opening that note.
//!
//! That is why there is no windowing arithmetic here: a virtualised list shows the rows that fit and
//! asks for the ones it needs, so forty notes and four hundred cost the same to open.
//!
//! ## What is read, and when
//!
//! A row needs four facts: what the note is called, which file it came from, how much is in it, and
//! how long ago it was opened. Everything but the counts is in [`crate::recent`]'s index, so a launch
//! costs a `stat` per note rather than a database read; the counts of the notes whose database has
//! changed are read by [`scan`], off the UI thread, and land in the rows when they arrive.
//!
//! ## The keyboard
//!
//! The list takes the keyboard when it has focus: arrows move, Enter opens, Escape cancels, and a
//! typed character lands in the search box the list draws for itself — which is how a screen with no
//! text field of its own is still typed into. The chords that are the *app's* rather than the list's
//! (`Ctrl+N`, `Ctrl+O`, `Ctrl+S`) are bound as actions, the way [`crate::app`]'s undo and redo are.

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::empty::{Empty, EmptyDescription, EmptyHeader, EmptyTitle};
use gpui_kit::component::label::Label;
use gpui_kit::component::list::{List, ListDelegate, ListItem, ListState};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath};
use gpui_kit::*;

use crate::app::NoteApp;
use crate::error::Result;
use crate::note;
use crate::recent::{self, Recent, Recents, Stamp};
use crate::store::{NoteStore, Summary};

/// The space the screen keeps from the window's edges.
const SIDE_MARGIN: f32 = 56.0;

/// Which entries a query keeps, in the order they are drawn.
///
/// Kept a function of its own — and a pure one — so that what the search box does is testable
/// without a window: the delegate's `perform_search` is this, plus telling the list to redraw.
pub fn visible_indices(entries: &[Recent], query: &str) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| matches(entry, query))
        .map(|(index, _)| index)
        .collect()
}

/// Whether an entry is kept by the query.
///
/// Everything a row *says* is searchable, because a row's words are how a person remembers which note
/// it is: the name, the document, the folder, and the file it came from.
///
/// The file is searched only when there *is* one. A row with no file says "written in cheap-note",
/// and searching that sentence would make every note in the list match half the alphabet — a search
/// that keeps everything is worse than no search at all.
fn matches(entry: &Recent, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    let needle = query.to_lowercase();
    let folder = entry
        .folder
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut haystacks = vec![
        entry.title.to_lowercase(),
        entry.document.clone().unwrap_or_default().to_lowercase(),
        folder.to_lowercase(),
    ];

    if let Some(source) = &entry.source {
        haystacks.push(source.to_string_lossy().to_lowercase());
    }

    haystacks.iter().any(|text| text.contains(&needle))
}

/// What a scan sees, off the UI thread.
///
/// Two things: the folders in the notes folder that are notes — so that one carried in from another
/// machine is adopted — and the counts of the entries whose database has changed since the last
/// scan, so that a launch normally reads no database at all.
///
/// The counts are read with a write-capable connection because that is the only kind this app knows
/// how to open: `note.db` may be in WAL mode, whose readers need the `-shm` file to exist. Nothing
/// is written — the store is dropped as soon as its summary is taken, and a summary is two counts
/// and one small read.
pub fn scan(root: &Path, known: &[(PathBuf, Option<Stamp>)]) -> (Vec<PathBuf>, Vec<(PathBuf, Stamp, Summary)>) {
    let found = note::notes_in(root).unwrap_or_default();
    let mut articles = Vec::new();

    for (folder, stamp) in known {
        let database = folder.join(note::NOTE_DB);
        let Some(fresh) = Stamp::of(&database) else {
            // The note is gone; `reconcile` is what says so.
            continue;
        };

        if *stamp == Some(fresh) {
            continue;
        }

        if let Ok(store) = NoteStore::open(&database) {
            if let Ok(summary) = store.summary() {
                articles.push((folder.clone(), fresh, summary));
            }
        }
    }

    (found, articles)
}

/// What the list draws: one row per note the app remembers, narrowed by what has been typed.
///
/// The list asks this for a count, for a row, and for what a confirmation means. It owns everything
/// else — the highlight, the scrolling, the search box, the keys — see [`ListDelegate`].
pub struct RecentsDelegate {
    /// Every entry the app remembers, newest first.
    all: Vec<Recent>,
    /// The entries the query keeps, as indices into [`Self::all`], in the order they are drawn.
    visible: Vec<usize>,
    /// What the search box currently holds, so that new entries are filtered by it too.
    query: String,
    /// The row the list has highlighted.
    selected: Option<IndexPath>,
    /// The app, for the one thing a confirmation does. Weak: this does not own the app.
    app: WeakEntity<NoteApp>,
}

impl RecentsDelegate {
    /// A delegate over `entries`.
    pub fn new(entries: Vec<Recent>, app: WeakEntity<NoteApp>) -> Self {
        let visible = visible_indices(&entries, "");

        RecentsDelegate {
            all: entries,
            visible,
            query: String::new(),
            selected: None,
            app,
        }
    }

    /// Replaces what the list holds, keeping the search and the highlight.
    ///
    /// Used whenever the index changes under the list: a note was opened, an entry was forgotten, or
    /// a scan found notes the list had not been told about.
    pub fn set_entries(&mut self, entries: Vec<Recent>) {
        self.all = entries;
        self.visible = visible_indices(&self.all, &self.query);

        // A highlight on a row that is no longer there is not a highlight.
        self.selected = self
            .selected
            .filter(|selected| selected.row < self.visible.len());
    }

    /// The entry under the highlight.
    pub fn selected_entry(&self) -> Option<&Recent> {
        let row = self.selected?.row;
        self.entry_at(row)
    }

    /// The entry a row of the list shows.
    fn entry_at(&self, row: usize) -> Option<&Recent> {
        let index = *self.visible.get(row)?;
        self.all.get(index)
    }

    /// Highlights the entry for `folder`, if the query keeps it showing.
    pub fn reveal(&mut self, folder: &Path) -> Option<IndexPath> {
        let row = self
            .visible
            .iter()
            .position(|index| self.all[*index].folder == folder)?;

        let index = IndexPath::new(row);
        self.selected = Some(index);
        Some(index)
    }
}

impl ListDelegate for RecentsDelegate {
    type Item = ListItem;

    /// How many rows the list has.
    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.visible.len()
    }

    /// The search box changed: keep the rows whose words match it.
    ///
    /// No task is spawned and nothing is read: forty notes are already in memory, and the answer is a
    /// filter over a handful of strings. A query that *did* have to read something — the notes folder
    /// itself — is the app's business and runs on a worker; see [`Home::scanned`].
    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.query = query.to_string();
        self.visible = visible_indices(&self.all, &self.query);

        // A search that leaves rows showing starts at the top of them, rather than leaving the
        // highlight on a row that may not be there any more.
        self.selected = self.visible.first().map(|_| IndexPath::new(0));
        cx.notify();

        Task::ready(())
    }

    /// One row: the mark, the name, where it came from, the counts, and the age.
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let entry = self.entry_at(ix.row)?;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let highlighted = self.selected == Some(ix);

        // The mark says what the note is *about*: a document, or paper. It is the one thing about a
        // row that has no words of its own — the name, the file and the counts say the rest.
        let mark = if entry.document.is_some() {
            IconName::FileText
        } else {
            IconName::NotebookPen
        };
        let mark_color = if highlighted {
            theme.accent_foreground
        } else {
            muted
        };

        let second_line = if entry.gone {
            format!("not on disk \u{2014} {}", entry.origin())
        } else {
            entry.origin()
        };
        let counts = if entry.counted() {
            entry.counts()
        } else {
            String::from("\u{2026}")
        };
        let name = if entry.title.is_empty() {
            Recent::title_for(&entry.folder)
        } else {
            entry.title.clone()
        };
        let age = entry.age(recent::now_ms());

        Some(
            ListItem::new(ix)
                .selected(highlighted)
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_3()
                                .child(Icon::new(mark).text_color(mark_color))
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(Label::new(name))
                                        .child(Label::new(second_line).text_sm().text_color(muted)),
                                ),
                        )
                        .child(
                            v_flex()
                                .items_end()
                                .gap_1()
                                .child(Label::new(counts).text_sm())
                                .child(Label::new(age).text_sm().text_color(muted)),
                        ),
                ),
        )
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

    /// The row was opened — a click, or Enter: the note is opened. A *secondary* click forgets it.
    ///
    /// Forgetting is the only destructive-looking thing this screen does, and it destroys nothing:
    /// the entry is a line in a list, and the note it names is a folder with the writing in it. That
    /// is why it is on the quieter gesture rather than on a button of its own.
    fn confirm(
        &mut self,
        secondary: bool,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };

        self.app
            .update(cx, |app, cx| {
                if secondary {
                    app.forget_entry(entry, cx);
                } else {
                    app.open_entry(entry, cx);
                }
            })
            .ok();
    }

    /// There is nothing to show: say what the list is for, and where notes live.
    ///
    /// Two sentences for two different emptinesses: a first run has nothing yet, and a search that
    /// matched nothing has everything except that word.
    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let (title, description) = if self.query.is_empty() {
            (
                String::from("Nothing here yet"),
                format!(
                    "Write on a PDF, or on a blank sheet \u{2014} whatever you open comes back here. Notes live in {}",
                    note::root().display()
                ),
            )
        } else {
            (
                format!("Nothing matches \u{201c}{}\u{201d}", self.query),
                String::from("A word finds a note by its name, its document, its file, or its folder."),
            )
        };

        Empty::new()
            .header(
                EmptyHeader::new()
                    .title(EmptyTitle::new().child(title))
                    .description(EmptyDescription::new().child(description)),
            )
            .into_any_element()
    }
}

/// The screen the app opens on, and the one Escape returns to.
pub struct Home {
    /// What has been opened, newest first.
    recents: Recents,
    /// The list that draws [`Self::recents`], with its search box and its highlight.
    list: Entity<ListState<RecentsDelegate>>,
    /// Whether the screen is in front of the sheet.
    open: bool,
    /// Whether a scan is running, for the status line.
    scanning: bool,
    /// Whether the list still has to be told what the index holds.
    ///
    /// Recording an entry happens on paths that have no context to notify with — the pen's own path
    /// makes a note, and `persist` runs without a `Context` — so the push is deferred to the frame
    /// that draws the screen, which has both the context and the reason to do it.
    dirty: bool,
    /// Whether the keyboard still has to be put on the list.
    pending: bool,
    /// The note to bring into view when the screen settles.
    reveal: Option<PathBuf>,
}

impl Home {
    /// A screen showing what the index remembers: what the app starts on.
    pub fn open(window: &mut Window, cx: &mut Context<NoteApp>) -> Self {
        let recents = Recents::load();
        let entries = recents.entries().to_vec();
        let app = cx.weak_entity();
        let list = cx.new(|cx| ListState::new(RecentsDelegate::new(entries, app), window, cx));
        list.update(cx, |state, cx| {
            // The search box is the list's own; see `ListDelegate::perform_search`.
            state.set_searchable(true, cx);
            state.focus(window, cx);
        });

        Home {
            recents,
            list,
            open: true,
            scanning: false,
            dirty: false,
            pending: false,
            reveal: None,
        }
    }

    /// Whether the screen is in front of the sheet.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Puts the screen in front of the sheet.
    ///
    /// The keyboard is put on the list for real in [`Self::settle`], on the next frame: focus needs
    /// the window, and this is called from places — a keystroke, a button — that have only a context.
    pub fn show(&mut self) {
        self.open = true;
        self.pending = true;
        self.dirty = true;
    }

    /// Gives the window back to the note.
    pub fn hide(&mut self) {
        self.open = false;
    }

    /// Asks for the entry for `folder` to be brought into view, once there is a window to do it in.
    pub fn reveal(&mut self, folder: &Path) {
        self.reveal = Some(folder.to_path_buf());
    }

    /// Everything the list has to be told before the frame that shows it.
    ///
    /// Called once per frame while the screen is up: it hands the list what the index holds if the
    /// index changed under it, puts the keyboard on the list when the screen has just appeared, and
    /// brings the note the app came from into view.
    pub fn settle(&mut self, window: &mut Window, cx: &mut Context<NoteApp>) {
        if self.dirty {
            self.dirty = false;
            let entries = self.recents.entries().to_vec();
            self.list.update(cx, |state, cx| {
                state.delegate_mut().set_entries(entries);
                cx.notify();
            });
        }

        if self.pending {
            self.pending = false;
            self.list.update(cx, |state, cx| state.focus(window, cx));
        }

        if let Some(folder) = self.reveal.take() {
            self.list.update(cx, |state, cx| {
                if state.delegate_mut().reveal(&folder).is_some() {
                    state.scroll_to_selected_item(window, cx);
                }
            });
        }
    }

    /// Every entry the app remembers, in the order the list shows them.
    pub fn entries(&self) -> &[Recent] {
        self.recents.entries()
    }

    /// Records that something was opened, and writes the index out.
    ///
    /// The write is one small file at a human-speed event, so it is made here rather than queued:
    /// a list that survives a crash is worth more than the millisecond it costs to rename a file.
    pub fn record(&mut self, entry: Recent) -> Result<()> {
        self.recents.record(entry);
        self.dirty = true;
        self.recents.save()
    }

    /// Takes an entry out of the list. The note is not touched.
    pub fn forget(&mut self, folder: &Path) -> Result<()> {
        self.recents.forget(folder);
        self.dirty = true;
        self.recents.save()
    }

    /// Whether a scan is running.
    pub fn is_scanning(&self) -> bool {
        self.scanning
    }

    /// Says that a scan has started, for the status line.
    pub fn begin_scan(&mut self) {
        self.scanning = true;
    }

    /// Applies what a scan saw, and answers whether the list changed.
    pub fn scanned(&mut self, found: &[PathBuf], articles: &[(PathBuf, Stamp, Summary)]) -> bool {
        let before = self.entries().to_vec();
        self.scanning = false;

        self.recents.reconcile(found, recent::now_ms());
        for (folder, stamp, summary) in articles {
            self.recents.learn(folder, *stamp, *summary);
        }

        let changed = before != self.entries();
        self.dirty |= changed;
        changed
    }

    /// Writes the index out, for a change a scan made.
    pub fn save(&self) -> Result<()> {
        self.recents.save()
    }

    /// What the status line says about the list.
    pub fn status(&self) -> String {
        let total = self.entries().len();
        let gone = self.entries().iter().filter(|entry| entry.gone).count();

        let mut parts = vec![format!(
            "{total} note{}",
            if total == 1 { "" } else { "s" }
        )];

        if gone > 0 {
            parts.push(format!("{gone} not on disk"));
        }
        if self.scanning {
            parts.push(String::from("reading notes\u{2026}"));
        }
        parts.push(String::from("Enter opens \u{b7} type to find \u{b7} Esc leaves the note"));

        parts.join("   \u{b7}   ")
    }
}

impl Home {
    /// The screen: the list, and the two ways out of it.
    ///
    /// Takes the message from the status line rather than reading it: the message belongs to the app
    /// (a note that would not open, an export that failed), and this screen is where a person is
    /// standing when they need to read it.
    pub fn view(&self, message: &str, cx: &mut Context<NoteApp>) -> AnyElement {
        let theme = cx.theme();
        let (desk, ink, card, hairline, radius) = (
            theme.background,
            theme.foreground,
            theme.title_bar,
            theme.border,
            theme.radius_lg,
        );

        v_flex()
            .id("home")
            .size_full()
            .bg(desk)
            .text_color(ink)
            // The chords that are the app's rather than the list's. Everything else the keyboard does
            // here — arrows, Enter, Escape, typing — belongs to the list, which has the focus.
            .on_key_down(
                cx.listener(|app, event: &KeyDownEvent, _, cx| app.home_key_down(event, cx)),
            )
            .child(self.header(cx))
            // The list is a card on the desk, like the bar over the sheet: one white surface with a
            // hairline round it, so what was written reads as *paper* rather than as another menu.
            .child(
                div().flex_1().w_full().px(px(SIDE_MARGIN)).child(
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
            .child(self.actions(cx))
            .child(self.footer(message, cx))
            .into_any_element()
    }

    /// The top of the screen: what this is, and what the list is for.
    fn header(&self, cx: &mut Context<NoteApp>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;

        let caption = if self.entries().is_empty() {
            String::from("nothing has been opened yet")
        } else {
            String::from("what you were writing \u{b7} type to find a note, Enter to open it")
        };

        v_flex()
            .gap_1()
            .px(px(SIDE_MARGIN))
            .pt(px(40.0))
            .pb(px(18.0))
            .child(
                Label::new("cheap-note")
                    .text_lg()
                    .font_weight(FontWeight::BOLD),
            )
            .child(Label::new(caption).text_sm().text_color(muted))
            .into_any_element()
    }

    /// The two ways out of the list, and the one way forward.
    ///
    /// *Open* is first because it is what a person with nothing to reopen needs, and *New blank
    /// sheet* is there because writing on nothing at all is a thing this app does rather than a
    /// fallback: the first stroke makes the note.
    fn actions(&self, cx: &mut Context<NoteApp>) -> AnyElement {
        h_flex()
            .items_center()
            .gap_2()
            .px(px(SIDE_MARGIN))
            .pt(px(14.0))
            .child(
                Button::new("home-open")
                    .primary()
                    .icon(IconName::FolderOpen)
                    .label("Open a PDF or a note\u{2026}")
                    .on_click(cx.listener(|app, _, _, cx| app.prompt_for_pdf(cx))),
            )
            .child(
                Button::new("home-new")
                    .ghost()
                    .icon(IconName::SquarePen)
                    .label("New blank sheet")
                    .on_click(cx.listener(|app, _, _, cx| app.new_blank_sheet(cx))),
            )
            .into_any_element()
    }

    /// The bottom line: what the list holds, what is missing, and whatever the app last said.
    fn footer(&self, message: &str, cx: &mut Context<NoteApp>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hairline = theme.border;

        let mut bar = StatusBar::new().w_full().px(px(SIDE_MARGIN)).py(px(8.0)).bg(theme.transparent).border_t_1().border_color(hairline)
            .left(Label::new(self.status()).text_sm().text_color(muted));

        if !message.is_empty() {
            bar = bar.right(Label::new(message.to_string()).text_sm().text_color(muted));
        }

        bar.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into scope
    // and shadow the attribute this module needs.
    use super::{matches, scan, visible_indices};
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
            note.store_mut().append(0, &ink).expect("ink");
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
            .append(0, &[a_stroke()])
            .expect("ink");

        let (_, changed) = scan(&root, &stamps);
        assert_eq!(changed.len(), 1, "one note changed");
        assert_eq!(changed[0].0, first);
        assert_eq!(changed[0].2.strokes, 3, "the two strokes and the new one");
        assert_eq!(changed[0].2.pages, 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
