use super::*;

/// A scratch index path, removed first so a test always starts from nothing.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("cheap-note-recent-{name}.json"));
    let _ = std::fs::remove_file(&path);
    path
}

/// A note folder that exists, so that `gone` can be asked about it.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheap-note-recent-{name}"));
    std::fs::create_dir_all(&dir).expect("a note folder");
    dir
}

/// The entry for a folder, or a panic: a test that asks about an entry has made one.
fn found<'a>(recents: &'a Recents, folder: &Path) -> &'a Recent {
    recents
        .entries()
        .iter()
        .find(|entry| entry.folder == folder)
        .expect("the entry was recorded or adopted")
}

/// An entry takes the name the note gives it — whatever the note's database said — and wakes up
/// stale, so that the next scan confirms what was just written instead of trusting this writer.
#[test]
fn what_a_note_says_about_itself_reaches_the_entry() {
    let mut recents = Recents::load_from(&scratch("learn"));
    let dir = folder("learned");
    recents.record(Recent::note(
        dir.clone(),
        None,
        String::from("Blank sheet"),
        None,
        1_000,
    ));

    let stamp = Stamp {
        mtime_ms: 5,
        len: 40,
    };
    let facts = Facts {
        title: Some(String::from("3\u{c7a5} \u{c694}\u{c57d}")),
        document: Some(String::from("chapter-3.pdf")),
        summary: Summary {
            pages: 4,
            strokes: 12,
            sheet: Some((720.0, 1018.0)),
        },
    };

    recents.learn(&dir, stamp, &facts);

    let entry = found(&recents, &dir);
    assert_eq!(
        entry.title, "3\u{c7a5} \u{c694}\u{c57d}",
        "the note's own name"
    );
    assert_eq!(entry.document.as_deref(), Some("chapter-3.pdf"));
    assert_eq!(entry.summary.strokes, 12);
    assert_eq!(entry.stamp, Some(stamp));

    // A name given in the app reaches the entry too, and the stamp is left where it was: a name
    // written to the database changes it, and the next scan is what notices.
    recents.rename(&dir, "chapter three");
    let entry = found(&recents, &dir);
    assert_eq!(entry.title, "chapter three");
    assert_eq!(entry.stamp, Some(stamp), "the entry is stale, by design");
}

/// Two windows' worth of index, as one: what a window has decided stands, and what it says nothing about comes
/// from the other window's file.
///
/// The shape of the hazard, as it happens in practice: two windows of the app are open, one takes a note out of
/// the list, the other opens a *different* note. The second window's write holds a list it read before the take-out
/// happened, so a plain write would put the forgotten note back; and the first window's write holds no entry for
/// the note the other one just opened, so a plain write would drop it. Both are settled here — and what makes a
/// merge safe at all is that this file is a cache of what the notes folder already says.
#[test]
fn two_windows_worth_of_list_are_merged() {
    let here = folder("merge-here");
    let there = folder("merge-there");
    let out = folder("merge-out");

    // This window: `here` in its list, `out` taken out of it.
    let ours = Index {
        entries: vec![Recent::note(
            here.clone(),
            None,
            String::from("here"),
            None,
            100,
        )],
        forgotten: vec![out.clone()],
    };

    // The other window, writing what it read before either of those: `here` in its list too (and opened *later*,
    // which is the one thing that is merged rather than decided), `out` still in it, and `there` newly opened.
    let theirs = Index {
        entries: vec![
            Recent::note(here.clone(), None, String::from("here"), None, 300),
            Recent::note(out.clone(), None, String::from("out"), None, 200),
            Recent::note(there.clone(), None, String::from("there"), None, 400),
        ],
        forgotten: Vec::new(),
    };

    let merged = merge(ours, theirs);
    let folders: Vec<&PathBuf> = merged.entries.iter().map(|entry| &entry.folder).collect();

    assert!(
        folders.contains(&&there),
        "a note only the other window knows is kept: {folders:?}"
    );
    assert!(
        !folders.contains(&&out),
        "and a note this window took out of the list stays out: {folders:?}"
    );
    assert_eq!(
        merged
            .entries
            .iter()
            .find(|entry| entry.folder == here)
            .map(|entry| entry.opened_at),
        Some(300),
        "and of two entries for one note, the later visit is the one that counts"
    );
}

/// A missing index is an empty list, not an error.
#[test]
fn a_missing_index_is_an_empty_list() {
    let recents = Recents::load_from(&scratch("missing"));

    assert!(recents.entries().is_empty());
    assert_eq!(recents.entries(), &[]);
}

/// What is recorded comes back, newest first, through the file.
#[test]
fn what_is_recorded_comes_back_newest_first() {
    let path = scratch("round-trip");
    let mut recents = Recents::load_from(&path);

    recents.record(Recent::note(
        folder("first"),
        Some(PathBuf::from("C:/docs/first.pdf")),
        String::from("first"),
        Some(String::from("first.pdf")),
        1_000,
    ));
    recents.record(Recent::note(
        folder("second"),
        None,
        String::from("Blank sheet"),
        None,
        2_000,
    ));
    recents.save().expect("the index is written");

    let loaded = Recents::load_from(&path);
    let titles: Vec<&str> = loaded
        .entries()
        .iter()
        .map(|entry| entry.title.as_str())
        .collect();
    assert_eq!(titles, vec!["Blank sheet", "first"]);
    assert_eq!(
        loaded.entries()[1].document.as_deref(),
        Some("first.pdf"),
        "the document's name is remembered"
    );
}

/// Opening the same note twice is one entry, at the top, not two.
#[test]
fn opening_the_same_note_twice_keeps_one_entry() {
    let path = scratch("twice");
    let mut recents = Recents::load_from(&path);
    let dir = folder("twice-note");

    recents.record(Recent::note(
        dir.clone(),
        None,
        String::from("chapter-3"),
        None,
        1_000,
    ));
    recents.record(Recent::note(
        folder("another"),
        None,
        String::from("another"),
        None,
        2_000,
    ));
    recents.record(Recent::note(
        dir.clone(),
        None,
        String::from("chapter-3"),
        None,
        3_000,
    ));

    assert_eq!(recents.entries().len(), 2, "one entry per folder");
    assert_eq!(recents.entries()[0].folder, dir, "and it is the newest one");
    assert_eq!(recents.entries()[0].opened_at, 3_000);
}

/// The list is capped: a note that falls off the end is still a folder on disk.
#[test]
fn the_list_is_capped() {
    let path = scratch("capped");
    let mut recents = Recents::load_from(&path);

    for index in 0..RECENT_MAX + 5 {
        recents.record(Recent::note(
            PathBuf::from(format!("note-{index}")),
            None,
            format!("note-{index}"),
            None,
            index as u64,
        ));
    }

    assert_eq!(recents.entries().len(), RECENT_MAX);
    assert_eq!(
        recents.entries()[0].title,
        format!("note-{}", RECENT_MAX + 4),
        "the newest is kept and the oldest is what goes"
    );
}

/// A folder that is gone is marked, and one nobody told us about is adopted.
#[test]
fn the_list_heals_itself_around_the_notes_folder() {
    let path = scratch("heal");
    let mut recents = Recents::load_from(&path);
    let here = folder("heal-here");
    let missing = std::env::temp_dir().join("cheap-note-recent-heal-gone");
    let _ = std::fs::remove_dir_all(&missing);

    recents.record(Recent::note(
        here.clone(),
        None,
        String::from("here"),
        None,
        10,
    ));
    recents.record(Recent::note(
        missing.clone(),
        None,
        String::from("gone"),
        None,
        20,
    ));

    let carried_in = folder("heal-carried-in");
    recents.reconcile(&[here.clone(), carried_in.clone()], 5_000);

    assert_eq!(recents.entries().len(), 3, "the folder found is adopted");
    let adopted = found(&recents, &carried_in);
    assert_eq!(
        adopted.title,
        Recent::title_for(&carried_in),
        "and is called what its folder says"
    );
    assert!(!adopted.counted(), "with nothing counted yet");
    assert!(found(&recents, &missing).gone);
    assert!(!found(&recents, &here).gone);
}

/// A folder that lives outside the notes folder is not called gone for being outside it.
#[test]
fn a_note_opened_where_it_stands_is_not_gone() {
    let path = scratch("in-place");
    let mut recents = Recents::load_from(&path);
    let elsewhere = folder("in-place-elsewhere");

    recents.record(Recent::note(
        elsewhere.clone(),
        None,
        String::from("in place"),
        None,
        10,
    ));
    recents.reconcile(&[], 20);

    assert!(
        !found(&recents, &elsewhere).gone,
        "being outside the notes folder is not being missing"
    );
}

/// A broken index is an empty list rather than a launch that cannot happen.
#[test]
fn a_broken_index_is_an_empty_list() {
    let path = scratch("broken");
    std::fs::write(&path, "{ not json at all").expect("a broken index");

    assert!(Recents::load_from(&path).entries().is_empty());
}

/// Ages are words, and a clock that moved backwards is not a negative age.
#[test]
fn ages_read_as_words() {
    let minute = 60_000;
    let hour = 60 * minute;
    let day = 24 * hour;
    let entry = Recent::note(PathBuf::new(), None, String::new(), None, 10 * day);
    let now = 10 * day;

    assert_eq!(entry.age(now), "just now");
    assert_eq!(entry.age(now + 3 * minute), "3 minutes ago");
    assert_eq!(entry.age(now + 2 * hour), "2 hours ago");
    assert_eq!(entry.age(now + day), "yesterday");
    assert_eq!(entry.age(now + 3 * day), "3 days ago");
    assert_eq!(entry.age(now + 45 * day), "a month ago");
    assert_eq!(entry.age(now + 100 * day), "3 months ago");
    assert_eq!(entry.age(now + 700 * day), "a year ago");
    assert_eq!(
        entry.age(now - 5 * minute),
        "just now",
        "a clock that went backwards reports the smallest age there is"
    );
}

/// A folder's name is turned back into words: the digest goes, the stamp becomes a phrase.
#[test]
fn a_folder_name_is_turned_back_into_words() {
    assert_eq!(
        Recent::title_for(Path::new("C:/notes/chapter-3-9f2a1c44")),
        "chapter-3"
    );
    assert_eq!(
        Recent::title_for(Path::new("C:/notes/blank-1712345678901")),
        "Blank sheet"
    );
    assert_eq!(
        Recent::title_for(Path::new("C:/notes/not-a-digest-here")),
        "not-a-digest-here",
        "only a digest is stripped"
    );
}

/// A file that was opened is remembered as the note it made, with the file it came from.
#[test]
fn an_entry_says_where_the_note_came_from() {
    let placed = Recent::note(
        folder("origin-note"),
        Some(PathBuf::from("C:/docs/chapter-3.pdf")),
        String::from("chapter-3"),
        Some(String::from("chapter-3.pdf")),
        10,
    );
    let blank = Recent::note(
        folder("origin-blank"),
        None,
        String::from("Blank sheet"),
        None,
        10,
    );

    assert!(
        placed.origin().ends_with("chapter-3.pdf"),
        "the file a note was made from is what the line under it says: {}",
        placed.origin()
    );
    assert_eq!(
        blank.origin(),
        "written in cheap-note",
        "a note made on a blank sheet came from nowhere, and says so"
    );
}

/// Counts read as a sentence, including the singular.
#[test]
fn counts_read_as_a_sentence() {
    let mut entry = Recent::note(PathBuf::new(), None, String::new(), None, 0);

    assert_eq!(entry.counts(), "no pages yet");

    entry.summary = Summary {
        pages: 1,
        strokes: 1,
        sheet: None,
    };
    assert_eq!(entry.counts(), "1 page · 1 stroke");

    entry.summary = Summary {
        pages: 12,
        strokes: 340,
        sheet: None,
    };
    assert_eq!(entry.counts(), "12 pages · 340 strokes");
}
/// Taking a row out of the list keeps it out.
///
/// The note is still a folder under `notes\`, so the scan that follows has to *not* adopt it back:
/// a row that returns on its own makes the menu item that removed it meaningless.
#[test]
fn a_note_taken_out_of_the_list_is_not_adopted_back() {
    let path = scratch("forget");
    let mut recents = Recents::load_from(&path);
    let dir = folder("forget-note");
    // What a scan of the notes folder sees: this note's folder, still on disk — which is exactly
    // why an entry that was merely removed would come straight back.
    let scan = [dir.clone()];

    recents.record(Recent::note(
        dir.clone(),
        None,
        String::from("Blank sheet"),
        None,
        1_000,
    ));
    recents.reconcile(&scan, 2_000);
    assert_eq!(recents.entries().len(), 1, "the note is in the list");

    recents.forget(&dir);
    assert!(recents.entries().is_empty(), "the row is gone");

    // The scan that follows: the folder is still there, and a folder a person took out is not a
    // folder nobody told us about.
    recents.reconcile(&scan, 3_000);
    assert!(
        recents.entries().is_empty(),
        "a scan does not adopt back what a person took out"
    );

    // Opening it again is a person asking for it, and that is the one way back.
    recents.record(Recent::note(
        dir.clone(),
        None,
        String::from("Blank sheet"),
        None,
        4_000,
    ));
    assert_eq!(recents.entries().len(), 1, "opening it says so");
    assert_eq!(recents.entries()[0].folder, dir);

    recents.reconcile(&scan, 5_000);
    assert_eq!(
        recents.entries().len(),
        1,
        "and it is an ordinary note again"
    );
}

/// Being taken out is written down, so a restart does not bring the row back either.
#[test]
fn a_note_taken_out_stays_out_across_a_restart() {
    let path = scratch("forget-persisted");
    let dir = folder("forget-persisted-note");
    let scan = [dir.clone()];

    let mut recents = Recents::load_from(&path);
    recents.record(Recent::note(
        dir.clone(),
        None,
        String::from("a note"),
        None,
        1_000,
    ));
    recents.forget(&dir);
    recents.save().expect("the index is written");

    // What the next launch has before anything is scanned.
    let mut loaded = Recents::load_from(&path);
    assert!(loaded.entries().is_empty(), "the row is not in the file");

    loaded.reconcile(&scan, 2_000);
    assert!(
        loaded.entries().is_empty(),
        "and the scan that follows does not bring it back"
    );
}

/// A folder that is not on disk needs no reminder, because it cannot be adopted.
///
/// This is what keeps the reminders from piling up: a note that was deleted, or a folder taken out
/// of the list while it was already gone, is dropped on the next scan.
#[test]
fn a_folder_that_is_gone_needs_no_reminder() {
    let path = scratch("forget-gone");
    let mut recents = Recents::load_from(&path);
    let missing = std::env::temp_dir().join("cheap-note-recent-forget-gone");
    let _ = std::fs::remove_dir_all(&missing);

    recents.record(Recent::note(
        missing.clone(),
        None,
        String::from("gone"),
        None,
        10,
    ));
    recents.reconcile(&[], 20);
    recents.forget(&missing);
    recents.reconcile(&[], 30);
    recents.save().expect("the index is written");

    let text = std::fs::read_to_string(&path).expect("the index is there");
    assert!(
        !text.contains("forget-gone"),
        "a folder that is not there is not kept as taken out: {text}"
    );
}

/// A folder that *is* there is kept as taken out — once, however often the row was removed.
#[test]
fn a_taken_out_folder_is_remembered_once() {
    let path = scratch("forget-twice");
    let mut recents = Recents::load_from(&path);
    let dir = folder("forget-twice-note");
    let scan = [dir.clone()];

    recents.record(Recent::note(
        dir.clone(),
        None,
        String::from("a note"),
        None,
        10,
    ));
    recents.forget(&dir);
    recents.forget(&dir);
    recents.reconcile(&scan, 20);
    recents.save().expect("the index is written");

    let text = std::fs::read_to_string(&path).expect("the index is there");
    assert_eq!(
        text.matches("forget-twice-note").count(),
        1,
        "one reminder, not one per attempt: {text}"
    );
    assert!(recents.entries().is_empty());
}
