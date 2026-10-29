//! A page's history, from the outside: an edit in both directions, the stack, and the bytes the log
//! holds.
//!
//! An edit is a *value* with two directions — what redo does and what undo does — and that is what
//! makes every change to a page uniform: no command is special and nothing has to remember which ones
//! cannot be taken back. This file tests exactly that promise, using only what a second implementation
//! of the format gets: the public `Edit`, the `Page` view it acts on, and `Edit::encode`/`decode`.

use std::sync::Arc;

use note_format::history::{Edit, History, HistoryUpdate, Page};
use note_format::ink::Stroke;

mod common;

use common::{stroke, written};

/// A page's ink, in the shape an edit acts on: the strokes, the flags, and how often it was shifted.
///
/// The app owns one of these per page; here it is a small struct so that an edit can be applied and the
/// result looked at, which is the whole of what an edit is allowed to touch.
struct Ink {
    strokes: Arc<Vec<Arc<Stroke>>>,
    selected: Arc<Vec<bool>>,
    shifts: u64,
}

impl Ink {
    fn of(strokes: Vec<Stroke>) -> Self {
        Ink {
            selected: Arc::new(vec![false; strokes.len()]),
            strokes: Arc::new(strokes.into_iter().map(Arc::new).collect()),
            shifts: 0,
        }
    }

    /// The view an edit is handed.
    fn view(&mut self) -> Page<'_> {
        Page {
            strokes: &mut self.strokes,
            selected: &mut self.selected,
            shifts: &mut self.shifts,
            detail: 1.0,
        }
    }

    fn len(&self) -> usize {
        self.strokes.len()
    }

    fn xs(&self, index: usize) -> Vec<f32> {
        self.strokes[index]
            .points
            .iter()
            .map(|point| point.x)
            .collect()
    }

    /// Whether the page holds this ink, in this place, by the only test an edit can make.
    fn holds(&self, index: usize, stroke: &Stroke) -> bool {
        self.strokes
            .get(index)
            .is_some_and(|found| found.same_ink(stroke))
    }
}

/// A written edit takes the line back, and puts it forward again exactly as it was drawn.
#[test]
fn a_written_edit_goes_both_ways() {
    let line = stroke(4, 10.0, 10.0, 0x1B_1B_1F);
    let mut ink = Ink::of(Vec::new());

    let edit = written(0, &line);

    assert!(edit.apply(&mut ink.view()));
    assert_eq!(ink.len(), 1);
    assert!(ink.holds(0, &line));

    assert!(edit.revert(&mut ink.view()));
    assert_eq!(ink.len(), 0, "undo takes the line back");
}

/// An erased edit keeps the ink it removed, and puts it back where it came from.
///
/// This is the whole reason the eraser can be undone: the strokes taken away are filed rather than
/// dropped, with the indices they held.
#[test]
fn an_erased_edit_puts_the_ink_back_where_it_was() {
    let first = stroke(3, 10.0, 10.0, 0x1B_1B_1F);
    let second = stroke(3, 50.0, 10.0, 0x1B_1B_1F);
    let third = stroke(3, 90.0, 10.0, 0x1B_1B_1F);

    let mut ink = Ink::of(vec![first.clone(), second.clone(), third.clone()]);
    let edit = Edit::Erased {
        removed: vec![(1, Arc::new(second.clone()))],
    };

    assert!(edit.apply(&mut ink.view()));
    assert_eq!(ink.len(), 2);
    assert!(ink.holds(1, &third), "the third moved into its place");

    assert!(edit.revert(&mut ink.view()));
    assert_eq!(ink.len(), 3);
    assert!(ink.holds(1, &second), "and it came back where it was");
}

/// A shifted edit moves the ink and counts the move, because a move is the one edit a stroke *count*
/// cannot see.
#[test]
fn a_shifted_edit_moves_the_ink_and_is_counted() {
    let line = stroke(3, 10.0, 10.0, 0x1B_1B_1F);
    let before = line.points.iter().map(|point| point.x).collect::<Vec<_>>();

    let mut ink = Ink::of(vec![line]);
    let edit = Edit::Shifted {
        strokes: vec![0],
        by: (25.0, 0.0),
    };

    assert!(edit.apply(&mut ink.view()));
    assert_eq!(
        ink.xs(0),
        before.iter().map(|x| x + 25.0).collect::<Vec<_>>()
    );
    assert_eq!(ink.shifts, 1, "the page counts the move");

    assert!(edit.revert(&mut ink.view()));
    assert_eq!(ink.xs(0), before, "a drag reverses exactly");
    assert_eq!(ink.shifts, 2);
}

/// An edit about ink that is no longer there does nothing, in either direction.
///
/// The check is what makes a stale edit harmless rather than destructive: an index that holds a
/// different line means the page is not the page this history is about, and doing nothing is the only
/// honest answer. It is a *comparison of ink* rather than of addresses, because a page read back out of
/// a note is the same strokes in freshly decoded memory.
#[test]
fn a_stale_edit_does_nothing() {
    let present = stroke(3, 10.0, 10.0, 0x1B_1B_1F);
    let elsewhere = stroke(3, 200.0, 10.0, 0x1B_1B_1F);

    // Erasing a line that is not at that index: nothing is taken away.
    let mut ink = Ink::of(vec![present.clone()]);
    let stale = Edit::Erased {
        removed: vec![(0, Arc::new(elsewhere.clone()))],
    };

    assert!(!stale.apply(&mut ink.view()), "it took nothing away");
    assert_eq!(ink.len(), 1);
    assert!(ink.holds(0, &present));

    // And undoing a line that has since been replaced: nothing is removed, and the edit stays on the
    // stack rather than being popped and lost.
    let written_edit = written(0, &present);
    assert!(written_edit.apply(&mut ink.view()));
    assert_eq!(ink.len(), 2);

    ink.strokes = Arc::new(vec![Arc::new(elsewhere.clone()), Arc::new(present.clone())]);

    let mut history = History::new();
    history.record(written_edit);
    assert!(!history.undo(&mut ink.view()), "a different line is there");
    assert!(history.can_undo(), "and the edit is still there to try");
    assert_eq!(ink.len(), 2, "the page is untouched");
}

/// Every edit survives being written to the note's log and read back.
///
/// The log is one `BLOB` per edit, in the ink's own encoding, so this is the round trip a note that
/// keeps its history depends on — whichever of the three shapes the edit is.
#[test]
fn an_edit_round_trips_through_its_bytes() {
    let line = stroke(4, 10.0, 10.0, 0x1B_1B_1F);
    let other = stroke(2, 80.0, 30.0, 0xCC_22_22);

    let edits = [
        written(1, &line),
        Edit::Erased {
            removed: vec![(0, Arc::new(line.clone())), (2, Arc::new(other.clone()))],
        },
        Edit::Shifted {
            strokes: vec![0, 3],
            by: (-8.0, 12.5),
        },
    ];

    for edit in edits {
        let bytes = edit.encode().expect("an edit as bytes");
        let read = Edit::decode(&bytes).expect("an edit back");

        // What an edit *is*, is what it does: the same two directions on the same page.
        let mut written_twice = Ink::of(vec![line.clone(), other.clone()]);
        let mut written_once = Ink::of(vec![line.clone(), other.clone()]);

        assert_eq!(
            edit.apply(&mut written_twice.view()),
            read.apply(&mut written_once.view()),
            "{edit:?} did not survive its bytes"
        );
        assert_eq!(written_twice.len(), written_once.len());
        assert_eq!(written_twice.shifts, written_once.shifts);
    }

    // A blob that is not an edit is refused rather than guessed at.
    assert!(Edit::decode(&[0xFF; 12]).is_err());
}

/// The note is told the tail of the history it has not heard about — and only that tail.
///
/// The two sides are kept in step by a number rather than by a shared list: the app says where the
/// applied history *ends*, and the note writes what it is told. This is that hand-off.
#[test]
fn the_note_is_told_only_what_it_has_not_heard() {
    let mut history = History::new();
    let mut ink = Ink::of(Vec::new());

    for (index, x) in [(0usize, 10.0f32), (1, 40.0)] {
        let line = stroke(3, x, 10.0, 0x1B_1B_1F);
        let edit = written(index, &line);

        assert!(edit.apply(&mut ink.view()));
        history.record(edit);
    }

    let first: HistoryUpdate = history.take_update(ink.len());
    assert_eq!(first.appended.len(), 2, "both edits are news");
    assert_eq!(first.applied, 2, "the cursor ends at the second");
    assert_eq!(first.count, 2);

    // Nothing has happened since, so there is nothing to say — but the cursor is still answered, and
    // that is what a checkpoint writes.
    let quiet = history.take_update(ink.len());
    assert!(quiet.appended.is_empty());
    assert_eq!(quiet.applied, 2);

    // One more edit, and only *it* is news.
    let line = stroke(3, 70.0, 10.0, 0x1B_1B_1F);
    let edit = written(2, &line);
    assert!(edit.apply(&mut ink.view()));
    history.record(edit);

    let next = history.take_update(ink.len());
    assert_eq!(next.appended.len(), 1);
    assert_eq!(next.applied, 3);
    assert_eq!(next.count, 3);
    assert_eq!(
        history.in_note(),
        3,
        "everything is now the note's business"
    );
}

/// Undo and redo move the cursor, and a new edit ends the branch that was taken back.
#[test]
fn undo_and_redo_move_the_cursor_and_a_new_edit_ends_the_branch() {
    let line = stroke(3, 10.0, 10.0, 0x1B_1B_1F);
    let mut ink = Ink::of(Vec::new());
    let mut history = History::new();

    let edit = written(0, &line);
    assert!(edit.apply(&mut ink.view()));
    history.record(edit);

    assert_eq!(history.take_update(ink.len()).applied, 1);

    assert!(history.undo(&mut ink.view()));
    assert_eq!(history.take_update(ink.len()).applied, 0, "one step back");
    assert_eq!(ink.len(), 0);
    assert!(history.can_redo());

    assert!(history.redo(&mut ink.view()));
    assert_eq!(history.take_update(ink.len()).applied, 1);
    assert_eq!(ink.len(), 1);

    // Take it back, then draw something else: what was taken back is no longer to be put forward.
    assert!(history.undo(&mut ink.view()));
    let instead = stroke(3, 200.0, 10.0, 0x1B_1B_1F);
    let edit = written(0, &instead);
    assert!(edit.apply(&mut ink.view()));
    history.record(edit);

    assert!(!history.can_redo(), "a new edit ends the redo branch");
    assert_eq!(history.take_update(ink.len()).applied, 1);
}

/// Going deeper into the note puts an old edit back under undo, and does not move the cursor.
///
/// An edit read out of the log has *already* been applied — it is one memory had dropped — so the page
/// must not change and the note must not be told about it twice.
#[test]
fn deepening_restores_an_undo_without_moving_the_cursor() {
    let older = stroke(3, 5.0, 5.0, 0x1B_1B_1F);
    let newer = stroke(3, 40.0, 5.0, 0x1B_1B_1F);

    let ink = Ink::of(vec![older.clone(), newer.clone()]);
    let mut history = History::new();

    let edit = written(1, &newer);
    history.record(edit);
    assert_eq!(history.take_update(ink.len()).applied, 1);
    assert_eq!(history.depth(), 1);
    assert_eq!(history.in_note(), 1);

    assert!(history.deepened(written(0, &older)));

    assert_eq!(history.depth(), 2, "the older edit is under undo again");
    assert_eq!(history.in_note(), 2, "and the note's tail begins before it");
    assert_eq!(
        history.take_update(ink.len()).applied,
        1,
        "the cursor did not move: the edit was already applied"
    );
    assert_eq!(ink.len(), 2, "and the page is untouched");
}

/// A page whose flags are out of step with its ink is repaired rather than indexed past the end.
///
/// A page read out of a note once arrived with no mask at all, and the first stroke written on it took
/// the whole process down. This is the repair, asserted from the outside.
#[test]
fn a_page_whose_flags_are_out_of_step_is_repaired() {
    let line = stroke(3, 10.0, 10.0, 0x1B_1B_1F);
    let mut ink = Ink::of(vec![line.clone(), line.clone()]);

    // The state a broken read leaves behind: two strokes, no flags.
    ink.selected = Arc::new(Vec::new());

    let edit = written(2, &line);
    assert!(edit.apply(&mut ink.view()));

    assert_eq!(ink.len(), 3);
    assert_eq!(
        ink.selected.len(),
        3,
        "the flags are one per stroke, in the same order"
    );
}
