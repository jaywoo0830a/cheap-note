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
//!
//! ## The mouse
//!
//! One click *chooses* a row and two open it — the count is the platform's, so a double click is
//! whatever the user's own settings say it is (see [`opens_on_click`]). Choosing is what makes the
//! highlight mean something and what makes the later gestures possible: a right-click acts on the row
//! it was made on, and acting on a row that is not the one on screen would be acting on a guess.
//!
//! A right-click opens a menu: open the note, rename it, or take it out of the list. Renaming puts a
//! field in the row, holding the name it shows now; Enter keeps the name, and Escape — or a click
//! away, or the field losing focus — leaves it as it was, because a half-typed name is not a name.
//!
//! Nothing is destructive behind a menu item that does not say so: *Forget* takes a line out of this
//! list and touches no note.
//!
//! ## Names
//!
//! A note's name is the *note's* (see [`crate::store::META_TITLE`]), not this list's: renaming writes
//! one `meta` row in the note's database and then tells the index what the note answered. So a folder
//! carried to another machine arrives with its name on it, and the index stays what it is — a cache.
//!
//! A name is what a note is *called*, never what it *is*: the folder keeps the name it was made with
//! (a digest of the file, or the moment a blank sheet was made), because that name is what makes
//! importing the same PDF twice find the same note. An empty name is a legitimate answer and the
//! normal state — the row then shows the name the list derives.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::empty::{Empty, EmptyDescription, EmptyHeader, EmptyTitle};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::list::{List, ListDelegate, ListItem, ListState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath, Sizable as _};
use gpui_kit::*;

use crate::app::NoteApp;
use crate::error::Result;
use crate::note;
use crate::recent::{self, Recent, Recents, Stamp};
use crate::store::{Facts, NoteStore};

/// The space the screen keeps from the window's edges.
const SIDE_MARGIN: f32 = 56.0;

/// How long after a click a confirmation still counts as that click's.
///
/// A click and the confirmation it causes arrive in the same event, so this window is generous — what
/// it is *not* is open-ended: an Enter pressed a moment later is the keyboard's Enter, which opens,
/// and a stamp from a click a second ago must not turn it into a click that only chooses.
const CLICK_WINDOW: Duration = Duration::from_millis(250);

/// Whether a click on a row opens the note.
///
/// One click *chooses* a row — that is what a list does, and what makes the highlight mean something —
/// and two open it. The count comes from the platform, so a double click is whatever the user's own
/// settings say it is, and the first click of a double click arrives as a click of its own: choose,
/// then open. Kept a function of its own because it is the whole of the rule, and because a rule about
/// clicks should be testable without a window.
pub fn opens_on_click(count: usize) -> bool {
    count >= 2
}

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
/// The *name* searched is the one the row shows, which is not always one the note carries: an unnamed
/// note shows the words the list derives from its folder ("Blank sheet"), and searching for those
/// words has to find it — a person searching for what is on screen is asking a fair question.
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
    let shown = entry.shown_title();
    let mut haystacks = vec![
        shown.to_lowercase(),
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
pub fn scan(root: &Path, known: &[(PathBuf, Option<Stamp>)]) -> (Vec<PathBuf>, Vec<(PathBuf, Stamp, Facts)>) {
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
            if let Ok(facts) = store.facts() {
                articles.push((folder.clone(), fresh, facts));
            }
        }
    }

    (found, articles)
}

/// The right-click menu for one row: what can be done with the note it names.
///
/// Three things, in the order a person reaches for them: open it, rename it, and take it out of this
/// list. *Forget* is last and says what it does — the note itself is a folder with the writing in it
/// and is not touched, and the words are the only place that can be said.
///
/// The menu acts on the row it was opened on — the `entry` is captured, not looked up by highlight —
/// so a menu opened on a row acts on that row even if the highlight has moved since.
fn row_menu(menu: PopupMenu, app: WeakEntity<NoteApp>, entry: Recent) -> PopupMenu {
    let open = {
        let app = app.clone();
        let entry = entry.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            app.update(cx, |app, cx| app.open_entry(entry.clone(), cx)).ok();
        }
    };

    let rename = {
        let app = app.clone();
        let entry = entry.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            app.update(cx, |app, cx| app.rename_entry(entry.clone(), cx)).ok();
        }
    };

    let forget = {
        let app = app.clone();
        let entry = entry.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            app.update(cx, |app, cx| app.forget_entry(entry.clone(), cx)).ok();
        }
    };

    menu.item(
        PopupMenuItem::new("Open")
            .icon(IconName::BookOpen)
            .on_click(open),
    )
    .separator()
    .item(
        PopupMenuItem::new("Rename")
            .icon(IconName::PencilLine)
            // A note that is not on disk cannot be renamed: the name lives *in* the note.
            .disabled(entry.gone)
            .on_click(rename),
    )
    .item(
        PopupMenuItem::new("Forget this row")
            .icon(IconName::Trash)
            .on_click(forget),
    )
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
    /// The last click on a row, and when it happened.
    ///
    /// The list confirms on *every* click, and by then the click count is no longer in hand: this is
    /// where it is kept. See [`RecentsDelegate::confirm`].
    last_click: Option<(usize, Instant)>,
    /// The row whose name is being typed, if any.
    ///
    /// The *folder* rather than the row number, because a row number means something different every
    /// time the search box changes.
    renaming: Option<PathBuf>,
    /// The field the name is typed into: one field, put in whichever row is being renamed.
    ///
    /// One field rather than one per row, because a virtualised list draws rows that come and go, and
    /// a field that scrolled out of the window would take the half-typed name with it.
    name_input: Entity<InputState>,
    /// The app, for the one thing a confirmation does. Weak: this does not own the app.
    app: WeakEntity<NoteApp>,
}

impl RecentsDelegate {
    /// A delegate over `entries`.
    pub fn new(
        entries: Vec<Recent>,
        name_input: Entity<InputState>,
        app: WeakEntity<NoteApp>,
    ) -> Self {
        let visible = visible_indices(&entries, "");

        // The first row starts highlighted, which is what makes Enter mean something the moment the
        // screen appears — and what gives a right-click a row to act on. A list with no rows has
        // nothing to highlight, and nothing to open.
        let selected = visible.first().map(|_| IndexPath::new(0));

        RecentsDelegate {
            all: entries,
            visible,
            query: String::new(),
            selected,
            last_click: None,
            renaming: None,
            name_input,
            app,
        }
    }

    /// Records a click on a row, for [`Self::confirm`] to read a moment later.
    ///
    /// Called from the row's own click handler, which is a *child* of the list's: the two run on the
    /// same event, this one first.
    pub fn noted_click(&mut self, count: usize) {
        self.last_click = Some((count, Instant::now()));
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

    /// The row this delegate considers chosen.
    ///
    /// The list draws its highlight from *its* own idea of what is chosen, and the choice is decided
    /// here — Enter opens [`Self::selected_entry`] — so [`Home::settle`] compares the two and hands the
    /// list this one when they differ. Without that, a list that has just been filled shows no highlight
    /// at all, and the row Enter would open looks like every other row.
    pub fn selected_path(&self) -> Option<IndexPath> {
        self.selected
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

    /// Puts the name field into the row for `folder`, holding the name it has now.
    ///
    /// What it holds is the name the *list shows*, which may be the derived one ("Blank sheet",
    /// "chapter-3"). That is deliberate: a person renaming a note is editing the words they can see,
    /// not an empty box they have to recreate them in.
    pub fn start_rename(&mut self, folder: &Path, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(entry) = self.all.iter().find(|entry| entry.folder == folder) else {
            return;
        };

        let written = entry.shown_title();

        self.renaming = Some(folder.to_path_buf());
        self.name_input.update(cx, |input, cx| {
            input.set_value(written, window, cx);
            // Selected, so that the first character typed *replaces* what is showing: a person renaming
            // a note means the new name, not the old one with something appended to it.
            input.select_all(window, cx);
            input.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    /// Whether a row is being renamed.
    pub fn is_renaming(&self) -> bool {
        self.renaming.is_some()
    }

    /// The folder being renamed, and what the field holds.
    pub fn take_rename(&mut self, cx: &mut App) -> Option<(PathBuf, String)> {
        let folder = self.renaming.take()?;
        let name = self.name_input.read(cx).value().to_string();

        Some((folder, name))
    }

    /// Puts the row back to a row. The name field is left holding what it held.
    pub fn stop_rename(&mut self) {
        self.renaming = None;
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
        let name = entry.shown_title();
        let age = entry.age(recent::now_ms());
        let renaming = self.renaming.as_deref() == Some(entry.folder.as_path());

        // The name in the row is either the name a person gave the note or the one the list derives
        // from the folder — and while it is being renamed it is a field, in place, holding whichever
        // of the two is showing.
        let name: AnyElement = if renaming {
            Input::new(&self.name_input).small().into_any_element()
        } else {
            Label::new(name).into_any_element()
        };

        // The row's own element: what is clicked, and what a right-click acts on. It is the *child* of
        // the list's row, which matters twice over — a click reaches this handler before the list's (so
        // the count is still in hand when the list confirms), and a menu hangs off it rather than off
        // the row the list owns, which keeps the item the list was given. An id per row, so the menu the
        // library keeps open belongs to the row it was opened on and not to whichever row filled it
        // first.
        let row = h_flex()
            .id(ElementId::Name(format!("note-row-{}", ix.row).into()))
            .w_full()
            .items_center()
            .justify_between()
            .gap_4()
            // A click *chooses* this row; a second one opens it. The count is the platform's, and the
            // list confirms on the same event — so it is recorded here, where it is still in hand, and
            // read there.
            .on_click({
                let list = cx.entity().downgrade();
                move |event: &ClickEvent, _window: &mut Window, cx: &mut App| {
                    list.update(cx, |state, cx| {
                        state.delegate_mut().noted_click(event.click_count());
                        cx.notify();
                    })
                    .ok();
                }
            })
            // A right-click is what can be done with *this* row. The menu is the library's, which means
            // the keyboard reaches it too: arrows move, Enter chooses, Escape closes.
            .context_menu({
                let app = self.app.clone();
                let entry = entry.clone();
                move |menu: PopupMenu, _window: &mut Window, _cx: &mut Context<PopupMenu>| {
                    row_menu(menu, app.clone(), entry.clone())
                }
            })
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(Icon::new(mark).text_color(mark_color))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(name)
                            .child(Label::new(second_line).text_sm().text_color(muted)),
                    ),
            )
            .child(
                v_flex()
                    .items_end()
                    .gap_1()
                    .child(Label::new(counts).text_sm())
                    .child(Label::new(age).text_sm().text_color(muted)),
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

    /// The row was opened — Enter, or the *second* of two clicks — or the row was chosen.
    ///
    /// The list confirms on every click, and a single click is a *choice* rather than an instruction:
    /// this is where the click count is turned back into the two gestures (see [`opens_on_click`]). A
    /// confirmation that came with no click of its own is the keyboard's, and Enter opens.
    ///
    /// Forgetting a row used to be on the quieter gesture — a ctrl-click — and it is in the right-click
    /// menu now: an action that changes what the list holds should be *named* before it is made, and a
    /// modifier held down while clicking a row says nothing at all.
    fn confirm(
        &mut self,
        _secondary: bool,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        let clicked_just_now = matches!(
            self.last_click,
            Some((_, at)) if at.elapsed() < CLICK_WINDOW
        );

        if clicked_just_now {
            let count = self.last_click.map(|(count, _)| count).unwrap_or(1);
            if !opens_on_click(count) {
                return;
            }
        }

        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };

        self.app
            .update(cx, |app, cx| app.open_entry(entry, cx))
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
    /// The row waiting to be renamed, once there is a window to put the field in.
    ///
    /// A rename is asked for by a menu item, and a menu item has no window to focus a field with — so
    /// the ask is kept here and carried out by [`Self::settle`], on the frame that has one.
    asked: Option<PathBuf>,
    /// What happens when the name field is typed into and answered — Enter, or a click away.
    ///
    /// The subscription is held rather than dropped: dropping it is how a listener stops listening.
    _name_events: Subscription,
}

impl Home {
    /// A screen showing what the index remembers: what the app starts on.
    pub fn open(window: &mut Window, cx: &mut Context<NoteApp>) -> Self {
        let recents = Recents::load();
        let entries = recents.entries().to_vec();
        let app = cx.weak_entity();

        // The name field: one field, put in whichever row is being renamed. Enter is the commit and a
        // click away is a cancel; both arrive as events from the field itself.
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("A name"));
        let _name_events = cx.subscribe_in(
            &name_input,
            window,
            |app, _, event: &InputEvent, window, cx| app.home_name_event(event, window, cx),
        );

        let list = cx.new(|cx| {
            ListState::new(RecentsDelegate::new(entries, name_input, app), window, cx)
        });
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
            asked: None,
            _name_events,
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

    /// Asks for `folder`'s row to be renamed, once there is a window.
    ///
    /// The row is also *chosen*, so that the field appears in the row the menu was opened on and the
    /// highlight is where the words are: a menu acts on a row, and a field in a row that is not the one
    /// the highlight is on reads as two different rows.
    pub fn ask_rename(&mut self, folder: &Path, cx: &mut Context<NoteApp>) {
        self.list.update(cx, |state, cx| {
            state.delegate_mut().reveal(folder);
            cx.notify();
        });

        self.asked = Some(folder.to_path_buf());
    }

    /// Whether a row is being renamed.
    pub fn is_renaming(&self, cx: &mut Context<NoteApp>) -> bool {
        self.list.read(cx).delegate().is_renaming()
    }

    /// The folder being renamed, and what the field holds.
    pub fn take_rename(&mut self, cx: &mut Context<NoteApp>) -> Option<(PathBuf, String)> {
        self.list.update(cx, |state, cx| state.delegate_mut().take_rename(cx))
    }

    /// Puts the row back to a row, and the keyboard back on the list.
    ///
    /// The keyboard has to be *put back* rather than merely let go: the field went away with the row,
    /// and a window with nothing focused is a screen whose arrows and typing go nowhere.
    pub fn stop_rename(&mut self, window: &mut Window, cx: &mut Context<NoteApp>) {
        self.list.update(cx, |state, cx| {
            state.delegate_mut().stop_rename();
            state.focus(window, cx);
            cx.notify();
        });
    }

    /// Records the name a person gave a note, and writes the index out.
    pub fn rename(&mut self, folder: &Path, title: &str) -> Result<()> {
        self.recents.rename(folder, title);
        self.dirty = true;
        self.recents.save()
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

        // A rename that was asked for by a menu, carried out now that there is a window to put the
        // keyboard in the field with.
        if let Some(folder) = self.asked.take() {
            self.list
                .update(cx, |state, cx| state.delegate_mut().start_rename(&folder, window, cx));
        }

        if let Some(folder) = self.reveal.take() {
            self.list.update(cx, |state, cx| {
                if state.delegate_mut().reveal(&folder).is_some() {
                    state.scroll_to_selected_item(window, cx);
                }
            });
        }

        // What is chosen and what is *drawn* as chosen are two things — the delegate decides, the list
        // draws — and they are kept the same here. Cheap enough to do every frame: two reads, and a write
        // only on the frames where they disagree. Without it, a list that has just been filled shows no
        // highlight at all, and the row Enter would open looks like every other row.
        let chosen = self.list.read(cx).delegate().selected_path();
        self.list.update(cx, |state, cx| {
            if state.selected_index() != chosen {
                state.set_selected_index(chosen, window, cx);
                cx.notify();
            }
        });
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
    pub fn scanned(&mut self, found: &[PathBuf], articles: &[(PathBuf, Stamp, Facts)]) -> bool {
        let before = self.entries().to_vec();
        self.scanning = false;

        self.recents.reconcile(found, recent::now_ms());
        for (folder, stamp, facts) in articles {
            self.recents.learn(folder, *stamp, facts);
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
        parts.push(String::from(
            "Enter or a double-click opens \u{b7} right-click for more",
        ));

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
            // here — arrows, Enter, Escape, typing — belongs to the list, which has the focus, and the
            // gestures that are not the keyboard at all belong to the rows themselves.
            .on_key_down(
                cx.listener(|app, event: &KeyDownEvent, window, cx| {
                    app.home_key_down(event, window, cx)
                }),
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
            String::from(
                "what you were writing \u{b7} type to find a note, double-click to open it",
            )
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
        assert!(matches(&unnamed, "1712345678901"), "and the folder's own name");
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
        assert!(opens_on_click(3), "and a third click is not a reason to stop");
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
        assert_eq!(
            changed[0].2.summary.strokes, 3,
            "the two strokes and the new one"
        );
        assert_eq!(changed[0].2.summary.pages, 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
