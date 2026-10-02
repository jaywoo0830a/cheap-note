//! A page's history: the edits its ink has been through, in the form that can be taken back.
//!
//! ## Why an edit is a *value*
//!
//! The page used to keep its history as a list of strokes that undo popped and redo pushed back. That is
//! enough for a pen and nothing else: a list of strokes can only be *shortened*, so the eraser — which takes
//! several strokes away and keeps no record of them — could not be undone, and neither could a lasso's drag
//! or a cleared page. Undo is not "give me the last stroke back"; it is "put back what I did", and what the
//! user did has to be a thing the model can hold.
//!
//! So an [`Edit`] is a value with two directions: [`Edit::apply`] (what redo does, and what a command does
//! when it is first made) and [`Edit::revert`] (what undo does). Every change to a page's ink is one of these,
//! which is what makes the rules uniform: no command is special, and there is nothing to remember about which
//! ones "cannot be undone".
//!
//! * **A `Written` edit holds the strokes it wrote**, so undo takes the line back and redo puts it back
//!   exactly as it was drawn.
//! * **An `Erased` edit holds the strokes it removed *and where they were*.** This is the whole reason the
//!   eraser can be undone now: the ink it took away is not thrown away, it is filed.
//! * **A `Shifted` edit holds which strokes moved and by how much**, so a lasso's drag — which changes the
//!   geometry of a page without adding or removing a stroke — reverses exactly.
//!
//! ## The two rules a stack has
//!
//! * **A new edit ends the redo branch.** Anything done after an undo makes the drawing a *new* one, so what
//!   was taken back is no longer something to put forward. This is the rule every editor has, and the reason
//!   redo can be trusted.
//! * **The history is bounded.** [`History`] keeps the most recent [`HISTORY_EDITS`] edits and drops the
//!   oldest, because a session that has been writing for an hour must not hold an hour of ink in memory. The
//!   *note* keeps a deeper log on disk (see [`crate::store`]), and a page reads one more edit out of it when
//!   the session's own history runs out (see `NoteApp::deepen`).
//!
//! ## What is not here
//!
//! The file. This module owns what an edit *is* and how a page's history moves; [`crate::store`] owns the
//! table the same edits are written to, and [`crate::note`] owns the job that carries them. What crosses
//! between the two is [`HistoryUpdate`]: what the ink did since the note was last told, which is the only
//! thing a write path can say about a history without naming an ordinal.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::chunk;
use crate::error::{AppError, Result};
use crate::ink::Stroke;

/// How many edits a page's history keeps in memory.
///
/// The cost of an edit is the ink it holds — one stroke for a `Written` edit, every stroke a *Clear* removed
/// for that one — so this is a bound on how much ink a page can hold twice: once as the page, once as
/// undoable history. A few hundred edits is many minutes of writing by a hand, and the note's own log is
/// deeper than this for the case that matters after a restart.
pub const HISTORY_EDITS: usize = 256;

/// The part of a page an edit acts on: the strokes, the flags over them, and the shift count.
///
/// A view rather than the page itself, so that the two halves of an undo — *what happened* (an [`Edit`]) and
/// *what it happened to* — can be borrowed at the same time without either of them knowing about the other.
/// It is also the whole of what an edit may touch: nothing through here can reach the stroke under the nib,
/// the page's history, or the note.
pub struct Page<'a> {
    /// The finished strokes, in the order they were drawn.
    pub strokes: &'a mut Arc<Vec<Arc<Stroke>>>,
    /// Which of them are in hand, one flag each, in the same order.
    pub selected: &'a mut Arc<Vec<bool>>,
    /// How many times the page has been shifted in place: what the note's write path counts, because a move
    /// is the one edit a stroke *count* cannot see (see `NoteApp::persist`).
    pub shifts: &'a mut u64,
    /// The detail the strokes' outlines are built for, for the strokes an edit rebuilds.
    pub detail: f32,
}

/// One change to a page's ink, in the form that can be taken back.
///
/// Cheap to hold: the strokes are behind `Arc`s, so an edit carries pointers to ink the page may still have
/// and is a *second reference* to it rather than a second copy.
#[derive(Clone, Debug)]
pub enum Edit {
    /// Ink written: where in the page it went, and the lines themselves.
    Written {
        /// The index the first stroke took, which is where undo takes it away from.
        at: usize,
        /// The strokes, in the order they were drawn.
        strokes: Vec<Arc<Stroke>>,
    },
    /// Ink taken away, and where it was: what the eraser, a deleted selection, and *Clear* all are.
    ///
    /// The strokes are kept rather than dropped, which is the difference between an eraser that can be undone
    /// and one that cannot. `removed` is in ascending index order, so putting them back is a sequence of
    /// inserts at the places they came from.
    Erased {
        /// Each stroke that went, with the index it held.
        removed: Vec<(usize, Arc<Stroke>)>,
    },
    /// Ink moved in place: which strokes, and by how much. What a lasso's drag is.
    Shifted {
        /// The indices of the strokes that moved, in ascending order.
        strokes: Vec<usize>,
        /// How far they moved, in the paper's own units.
        by: (f32, f32),
    },
}

impl Edit {
    /// What redo does — and what a command does the first time.
    ///
    /// Answers whether the page changed. An edit whose strokes are no longer where it expects them (a log
    /// replayed against ink that has moved on) does nothing rather than something arbitrary, which is what
    /// makes a stale history harmless instead of destructive.
    pub fn apply(&self, page: &mut Page<'_>) -> bool {
        match self {
            Edit::Written { at, strokes } => insert(page, *at, strokes),

            Edit::Erased { removed } => {
                // Descending, so that each index is still the index it was taken at when its turn comes: the
                // list is stored ascending, and removing from the end of it first is what keeps that true.
                let mut gone = 0;
                for (index, stroke) in removed.iter().rev() {
                    if !remove_at(page, *index, stroke) {
                        return gone > 0;
                    }

                    gone += 1;
                }

                gone > 0
            }

            Edit::Shifted { strokes, by } => shift(page, strokes, *by),
        }
    }

    /// What undo does: the same edit, the other way round.
    pub fn revert(&self, page: &mut Page<'_>) -> bool {
        match self {
            Edit::Written { at, strokes } => {
                let mut taken = 0;
                // Descending for the reason `Erased` is: the indices of one edit are a sequence, and it is the
                // end of the sequence that does not move while the rest of it is taken away.
                for (offset, stroke) in strokes.iter().enumerate().rev() {
                    if !remove_at(page, at + offset, stroke) {
                        return taken > 0;
                    }

                    taken += 1;
                }

                taken > 0
            }

            // Ascending here, and that is the mirror of `apply`: the indices recorded are the ones the strokes
            // held *before* the removal, so putting the lowest back first leaves every later index valid.
            Edit::Erased { removed } => {
                let mut back = 0;
                for (index, stroke) in removed {
                    if insert(page, *index, std::slice::from_ref(stroke)) {
                        back += 1;
                    }
                }

                back > 0
            }

            Edit::Shifted { strokes, by } => shift(page, strokes, (-by.0, -by.1)),
        }
    }
}

/// Inserts strokes at `at`, keeping the flags the same length as the stroke list.
///
/// The mask is the stroke list's *length*, always — a flag per stroke, in the same order — so the two are made to
/// agree here before either is touched. A mask that is out of step is therefore repaired rather than indexed past
/// the end of: a page read out of the note once arrived with no mask at all, and the first stroke written on it
/// took the whole process down (`InkDocument::from_strokes` is where that is fixed, and this is what keeps any
/// *other* way of getting them out of step from being fatal).
fn insert(page: &mut Page<'_>, at: usize, strokes: &[Arc<Stroke>]) -> bool {
    if strokes.is_empty() || at > page.strokes.len() {
        return false;
    }

    let strokes_now = Arc::make_mut(page.strokes);
    let flags = Arc::make_mut(page.selected);
    flags.resize(strokes_now.len(), false);

    for (offset, stroke) in strokes.iter().enumerate() {
        strokes_now.insert(at + offset, Arc::clone(stroke));
        // A stroke that has just arrived is not in hand — the same rule a stroke written by the pen follows.
        flags.insert(at + offset, false);
    }

    true
}

/// Removes the stroke at `index`, if it is the one the edit is about.
///
/// The check is what makes a stale edit harmless rather than destructive: an index that holds a different line
/// than the one the edit recorded means the page is not the page this history is about, and doing nothing is
/// the only honest answer. It compares *ink* rather than pointers, because a page read back out of the note is
/// the same strokes in freshly decoded memory (see [`Stroke::same_ink`]).
fn remove_at(page: &mut Page<'_>, index: usize, stroke: &Arc<Stroke>) -> bool {
    let Some(found) = page.strokes.get(index) else {
        return false;
    };

    if !found.same_ink(stroke) {
        return false;
    }

    // Both are brought into step *before* either is touched: the mask is a flag per stroke, in the same order, and
    // it is the one part of a page that is not stored anywhere (see [`insert`] for the same repair, and why).
    let flags = Arc::make_mut(page.selected);
    flags.resize(page.strokes.len(), false);

    Arc::make_mut(page.strokes).remove(index);
    flags.remove(index);

    true
}

/// Moves the strokes at `indices` by `by`, rebuilding the geometry a move invalidates.
fn shift(page: &mut Page<'_>, indices: &[usize], by: (f32, f32)) -> bool {
    if (by.0 == 0.0 && by.1 == 0.0) || indices.is_empty() {
        return false;
    }

    let detail = page.detail;
    let mut moved = 0;
    let strokes = Arc::make_mut(page.strokes);

    for index in indices {
        let Some(stroke) = strokes.get_mut(*index) else {
            continue;
        };

        Arc::make_mut(stroke).translate(by, detail);
        moved += 1;
    }

    if moved == 0 {
        return false;
    }

    // A move is the one edit a stroke count cannot see, so the page counts it: this is what tells the note
    // that what is on disk is not what is in hand (see `NoteApp::persist`).
    *page.shifts += 1;
    true
}

/// What a page's history went through since the note was last told about it.
///
/// The contract between the model and the note's file, and the reason the store keeps no cursor of its own to
/// argue with: the app says where the applied history *ends* ([`Self::applied`], the ordinal of its last edit),
/// and the note writes what it is told. A tail and a number, and the two sides cannot come to disagree about
/// where they are.
#[derive(Clone, Debug, Default)]
pub struct HistoryUpdate {
    /// The edits to append to the log, oldest first: the tail of the applied history the note has not been told
    /// about. They take the place of everything the note holds from their own first ordinal onwards — which is
    /// how a branch of undone edits is dropped when a new edit is made over it.
    pub appended: Vec<Edit>,
    /// How many edits have been applied: the ordinal of the last one, and the page's cursor.
    pub applied: u64,
    /// How many strokes the page has now.
    pub count: usize,
}

/// A page's history: the edits that have been made, and the ones taken back.
///
/// The applied history is a *run*, and the note knows it by ordinal: the app counts the edits it has applied and
/// tells the note where the run now ends, so neither side has to keep a position in the other's list. The tail
/// the note has not heard about is a slice of `done` rather than a second copy of anything, which is also what
/// makes an edit that was undone and then replaced — a stroke drawn over an undone one — come out right: what is
/// not in `done` is not in the log.
#[derive(Debug, Default)]
pub struct History {
    /// The edits the page's ink has, oldest first. Bounded by [`HISTORY_EDITS`]: the oldest are dropped.
    done: VecDeque<Edit>,
    /// The edits taken back, oldest first, waiting to be put forward again.
    undone: VecDeque<Edit>,
    /// Where in [`Self::done`] the edits the note has not been told about begin.
    ///
    /// An index rather than a list, so that nothing has to be kept in step when the redo branch is dropped: the
    /// edits that leave `done` leave the pending tail with it.
    appended: usize,
    /// How many edits have been applied: the cursor's own value.
    applied: u64,
}

impl History {
    /// A history that has seen nothing: a page nothing has been done to.
    pub fn new() -> Self {
        History::default()
    }

    /// A history read out of the note: the edits the page has, the ones that were taken back, and how many had
    /// been applied.
    pub fn loaded(done: Vec<Edit>, undone: Vec<Edit>, applied: u64) -> Self {
        // Every edit that came out of the note is one the note already knows about, so nothing here is waiting to
        // be told and `appended` starts where `done` ends. This is also what a *deeper* read is measured from: the
        // app already holds the newest `done.len()` applied edits, so the next one to ask for is the one after
        // them (see [`Self::in_note`]).
        let appended = done.len();

        History {
            done: done.into(),
            undone: undone.into(),
            appended,
            applied,
        }
    }

    /// How many edits the page's ink has, in memory.
    pub fn depth(&self) -> usize {
        self.done.len()
    }

    /// How many edits have been taken back, in memory.
    pub fn forward(&self) -> usize {
        self.undone.len()
    }

    /// How many edits have been applied, in the log's own terms: the cursor.
    ///
    /// Read by the tests that check where a cursor is after a write, a reopen, and a step backwards — the numbers a
    /// person never sees, because the *status line* says what a page's history holds instead.
    #[cfg(test)]
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Whether [`History::undo`] would do anything.
    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    /// Whether [`History::redo`] would do anything.
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// How many of the applied edits the note already holds: where the tail the note has not heard about begins.
    ///
    /// What the app asks before reading *deeper* into the note's log: the edits below the cursor are the applied
    /// history, and the newest of them are the ones memory holds — so this is how far past them to look (see
    /// `NoteApp::deepen`).
    pub fn in_note(&self) -> usize {
        self.appended
    }

    /// Records an edit the page has already been given: what a command does with it once it has happened.
    ///
    /// The edit is *not* applied here. Applying is the page's job (see `InkDocument::record`), because the two
    /// directions of an edit have to be one piece of code, and an undo has to run the same `revert` a record
    /// runs.
    pub fn record(&mut self, edit: Edit) {
        // A new edit ends the redo branch: what was taken back is no longer something to put forward. The note is
        // told the same thing by the cursor it is given — the tail it is sent takes the place of everything it
        // holds from the first of those edits onwards — so nothing here has to count what was dropped.
        self.done.push_back(edit);
        self.applied += 1;
        self.undone.clear();

        // Bounded, and never at the expense of what the note has not been told: the oldest edit goes, so only the
        // *deepest* undo is lost. What is left is still a run of edits that follow one another, which is all a
        // stack needs — and while a write is in hand the run may stand over the limit rather than dropping an
        // edit the log would then never hear about (a write is a fraction of a second away).
        while self.done.len() > HISTORY_EDITS && self.appended > 0 {
            self.done.pop_front();
            self.appended -= 1;
        }
    }

    /// Takes the most recent edit back: the stack's half of an undo, where the edit's own [`Edit::revert`] is
    /// the page's half.
    ///
    /// Answers whether it went. An edit whose strokes are no longer where it recorded them does nothing and
    /// stays on the stack, which is what keeps a history that no longer fits its page from quietly corrupting
    /// it.
    pub fn undo(&mut self, page: &mut Page<'_>) -> bool {
        let Some(edit) = self.done.back() else {
            return false;
        };

        if !edit.revert(page) {
            return false;
        }

        let edit = self.done.pop_back().expect("it was there a line ago");
        self.undone.push_back(edit);
        self.applied -= 1;
        true
    }

    /// Puts the most recently taken-back edit forward again.
    pub fn redo(&mut self, page: &mut Page<'_>) -> bool {
        let Some(edit) = self.undone.back() else {
            return false;
        };

        if !edit.apply(page) {
            return false;
        }

        let edit = self.undone.pop_back().expect("it was there a line ago");
        self.done.push_back(edit);
        self.applied += 1;
        true
    }

    /// Puts an older edit at the bottom of what undo can reach: how a page goes deeper than a session's memory.
    ///
    /// The cursor does not move. This edit has *already* been applied — it is one the note knows about and
    /// memory had dropped — so putting it back at the bottom of the stack restores an undo that had been lost
    /// to the limit, and nothing about the page changes.
    pub fn deepened(&mut self, edit: Edit) -> bool {
        if self.done.len() >= HISTORY_EDITS {
            return false;
        }

        self.done.push_front(edit);
        // The front moved, so where the note's unseen tail begins moved with it.
        self.appended += 1;
        true
    }

    /// What to tell the note, for a write that is about to be handed over.
    ///
    /// Takes the news as it goes, because the caller sends this in the same job as the ink it describes: there is
    /// then no moment where the note has heard about an edit the ink has not, or the other way round.
    pub fn take_update(&mut self, count: usize) -> HistoryUpdate {
        let update = HistoryUpdate {
            appended: self.done.iter().skip(self.appended).cloned().collect(),
            applied: self.applied,
            count,
        };

        self.appended = self.done.len();
        update
    }
}

/// The kind byte at the front of an edit's bytes, so that a build meeting an edit it does not know can say so
/// rather than read it as one of its own.
const WRITTEN: u8 = 1;
const ERASED: u8 = 2;
const SHIFTED: u8 = 3;

impl Edit {
    /// The edit as the bytes a note's log holds.
    ///
    /// A header, then one sealed chunk of every stroke the edit holds (see [`chunk::seal`]). The strokes go
    /// through the *same* codec a page's ink goes through, which is what makes a logged edit hold **ink** rather
    /// than a description of an operation: a build that has changed what an edit *means* can still read
    /// yesterday's strokes, and a payload cannot describe a change the page is not in a position to make.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();

        match self {
            Edit::Written { at, strokes } => {
                out.push(WRITTEN);
                out.extend_from_slice(&(*at as u32).to_le_bytes());
                out.extend_from_slice(&chunk::seal(&chunk::encode(strokes)?));
            }

            Edit::Erased { removed } => {
                out.push(ERASED);
                out.extend_from_slice(&(removed.len() as u32).to_le_bytes());
                for (index, _) in removed {
                    out.extend_from_slice(&(*index as u32).to_le_bytes());
                }

                let strokes: Vec<Arc<Stroke>> = removed
                    .iter()
                    .map(|(_, stroke)| Arc::clone(stroke))
                    .collect();
                out.extend_from_slice(&chunk::seal(&chunk::encode(&strokes)?));
            }

            Edit::Shifted { strokes, by } => {
                out.push(SHIFTED);
                out.extend_from_slice(&(strokes.len() as u32).to_le_bytes());
                for index in strokes {
                    out.extend_from_slice(&(*index as u32).to_le_bytes());
                }

                out.extend_from_slice(&by.0.to_le_bytes());
                out.extend_from_slice(&by.1.to_le_bytes());
            }
        }

        Ok(out)
    }

    /// The edit a log row holds, from the bytes [`Self::encode`] wrote.
    ///
    /// Every length is checked against what is actually there, and the strokes are checked by their own CRC on
    /// the way out (see [`chunk::open_sealed`]): a row that cannot be read is an error the app reports, and never
    /// a page with the wrong ink on it.
    pub fn decode(bytes: &[u8]) -> Result<Edit> {
        let mut read = Reading::new(bytes);

        match read.byte()? {
            WRITTEN => {
                let at = read.count()?;
                let strokes = open_strokes(&mut read, Some(1))?;

                Ok(Edit::Written { at, strokes })
            }

            ERASED => {
                let count = read.count()?;
                let indices = read.indices(count)?;
                let strokes = open_strokes(&mut read, Some(count))?;

                Ok(Edit::Erased {
                    removed: indices.into_iter().zip(strokes).collect(),
                })
            }

            SHIFTED => {
                let count = read.count()?;
                let indices = read.indices(count)?;

                Ok(Edit::Shifted {
                    strokes: indices,
                    by: (read.decimal()?, read.decimal()?),
                })
            }

            other => Err(AppError::Note(format!(
                "this note's history holds an edit of kind {other}, which this build does not know"
            ))),
        }
    }
}

/// The strokes at the end of an edit's bytes, from the sealed chunk that carries them.
///
/// `expected` is the count the edit's own header gives: the chunk is asked for exactly that many strokes, so a
/// row whose header and payload disagree is refused rather than half-read.
fn open_strokes(read: &mut Reading<'_>, expected: Option<usize>) -> Result<Vec<Arc<Stroke>>> {
    let blob = read.rest();

    if blob.is_empty() {
        return Err(AppError::Note(String::from(
            "an edit of this note's history holds no ink",
        )));
    }

    let strokes = chunk::open_sealed(blob, expected.unwrap_or(1))?;
    Ok(strokes.into_iter().map(Arc::new).collect())
}

/// A reader over an edit's bytes, with every bounds check in one place.
struct Reading<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reading<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reading { bytes, at: 0 }
    }

    /// The kind byte, which is the first thing in a row.
    fn byte(&mut self) -> Result<u8> {
        let Some(byte) = self.bytes.first() else {
            return Err(AppError::Note(String::from(
                "a history row of this note is empty",
            )));
        };

        self.at = 1;
        Ok(*byte)
    }

    /// One little-endian `u32` read as a count or an index.
    fn count(&mut self) -> Result<usize> {
        Ok(self.number()? as usize)
    }

    /// One little-endian `u32`.
    fn number(&mut self) -> Result<u32> {
        let end = self.at + 4;
        if end > self.bytes.len() {
            return Err(AppError::Note(String::from(
                "a history row of this note ends in the middle of a number",
            )));
        }

        let value = u32::from_le_bytes(self.bytes[self.at..end].try_into().expect("four bytes"));
        self.at = end;
        Ok(value)
    }

    /// One `f32`, for the offset a shifted edit carries.
    fn decimal(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.number()?))
    }

    /// `count` stroke indices.
    fn indices(&mut self, count: usize) -> Result<Vec<usize>> {
        let mut indices = Vec::with_capacity(count.min(1_024));
        for _ in 0..count {
            indices.push(self.count()?);
        }

        Ok(indices)
    }

    /// Everything that is left: the sealed chunk, which is the last thing in a row.
    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at.min(self.bytes.len())..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::InkPoint;

    /// A finished line of ink, for the tests that have to hold one.
    fn stroke(x: f32, color: u32) -> Arc<Stroke> {
        let mut stroke = Stroke::new(InkPoint::new(x, 10.0, 2.0), color);
        stroke.points.push(InkPoint::new(x + 40.0, 12.0, 2.0));
        stroke.close();
        Arc::new(stroke)
    }

    /// Every kind of edit the model can hold.
    fn edits() -> Vec<Edit> {
        vec![
            Edit::Written {
                at: 3,
                strokes: vec![stroke(0.0, 0x11_11_11)],
            },
            Edit::Erased {
                removed: vec![(0, stroke(10.0, 0x22_22_22)), (2, stroke(30.0, 0x33_33_33))],
            },
            Edit::Shifted {
                strokes: vec![1, 4, 9],
                by: (12.5, -3.25),
            },
        ]
    }

    /// An edit is written down as ink and read back as the same edit, to the byte.
    ///
    /// The payload is *strokes* rather than a description of an operation — the same codec a page's ink goes
    /// through — so a round trip is not a re-encoding of something the model meant, it is the same bytes again.
    #[test]
    fn an_edit_round_trips_through_its_bytes() {
        for edit in edits() {
            let bytes = edit.encode().expect("an edit encodes");
            let back = Edit::decode(&bytes).expect("an edit decodes");

            assert_eq!(
                back.encode().expect("it encodes again"),
                bytes,
                "the same edit, to the byte"
            );
        }
    }

    /// An edit applied to a page whose selection mask is out of step repairs it rather than panicking.
    ///
    /// The mask is a flag per stroke, and it is the one part of a page that is *not* stored anywhere: a page read
    /// back out of a note arrives with none of it (see `InkDocument::from_strokes`). Writing the first stroke on such
    /// a page used to index past the end of that empty list and take the process down, which is why the repair is
    /// here as well as at the source — an edit is about *ink*, and a view of it must never be fatal.
    #[test]
    fn an_edit_repairs_a_mask_that_is_out_of_step() {
        let mut strokes: Arc<Vec<Arc<Stroke>>> = Arc::new(vec![
            stroke(0.0, 0x11_11_11),
            stroke(10.0, 0x22_22_22),
            stroke(20.0, 0x33_33_33),
        ]);
        let mut selected: Arc<Vec<bool>> = Arc::new(Vec::new());
        let mut shifts = 0;

        {
            let mut page = Page {
                strokes: &mut strokes,
                selected: &mut selected,
                shifts: &mut shifts,
                detail: 1.0,
            };

            assert!(
                Edit::Written {
                    at: 3,
                    strokes: vec![stroke(30.0, 0x44_44_44)],
                }
                .apply(&mut page),
                "a stroke written at the end of the page"
            );
        }

        assert_eq!(strokes.len(), 4);
        assert_eq!(
            selected.len(),
            4,
            "the mask came into step with the ink rather than past the end of it"
        );
        assert!(selected.iter().all(|flag| !flag), "nothing new is in hand");

        // A mask *longer* than the ink is repaired the same way, and taking a stroke away keeps the two equal.
        selected = Arc::new(vec![true; 9]);

        {
            let mut page = Page {
                strokes: &mut strokes,
                selected: &mut selected,
                shifts: &mut shifts,
                detail: 1.0,
            };

            assert!(Edit::Erased {
                removed: vec![(0, stroke(0.0, 0x11_11_11))],
            }
            .apply(&mut page));
        }

        assert_eq!(strokes.len(), 3);
        assert_eq!(selected.len(), 3, "one flag per stroke, still");
    }

    /// A decoded edit still names its strokes, and holds the ink of them.
    #[test]
    fn a_decoded_edit_still_names_its_strokes() {
        let edit = Edit::Erased {
            removed: vec![(0, stroke(10.0, 0x22_22_22)), (2, stroke(30.0, 0x33_33_33))],
        };

        match Edit::decode(&edit.encode().expect("an edit encodes")).expect("it decodes") {
            Edit::Erased { removed } => {
                assert_eq!(
                    removed.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
                    vec![0, 2],
                    "the places came back"
                );
                assert_eq!(removed[0].1.points[0].x, 10.0, "and the ink with them");
                assert_eq!(removed[1].1.color, 0x33_33_33);

                assert_eq!(
                    removed[0].1.bounds,
                    [10.0, 10.0, 50.0, 12.0],
                    "with the geometry an edit's strokes are put back by"
                );
            }
            other => panic!("an erased edit came back as {other:?}"),
        }
    }

    /// An edit of a kind this build does not know is refused by name rather than read as one of its own.
    #[test]
    fn what_cannot_be_read_is_refused_by_name() {
        let error = Edit::decode(&[9, 0, 0, 0, 0]).expect_err("kind 9 is not an edit");
        assert!(error.to_string().contains("kind 9"), "{error}");

        let error = Edit::decode(&[WRITTEN, 1, 0]).expect_err("a row that ends mid-number");
        assert!(error.to_string().contains("middle of a number"), "{error}");

        assert!(Edit::decode(&[]).is_err(), "an empty row is not an edit");
    }

    /// A move is the one edit a stroke count cannot see, so the page counts it — and the count is a fact about
    /// the page, not about the history: it is what the note's write path reads.
    #[test]
    fn a_move_is_counted() {
        let strokes = vec![stroke(0.0, 0x11_11_11), stroke(100.0, 0x11_11_11)];
        let mut strokes = std::sync::Arc::new(strokes);
        let mut selected = std::sync::Arc::new(vec![false, false]);
        let mut shifts = 0;

        let mut page = Page {
            strokes: &mut strokes,
            selected: &mut selected,
            shifts: &mut shifts,
            detail: 1.0,
        };

        let move_it = |page: &mut Page<'_>| {
            Edit::Shifted {
                strokes: vec![1],
                by: (10.0, 0.0),
            }
            .apply(page)
        };

        assert!(move_it(&mut page));
        assert_eq!(*page.shifts, 1, "a move is counted");

        assert!(Edit::Shifted {
            strokes: vec![1],
            by: (10.0, 0.0),
        }
        .revert(&mut page));
        assert_eq!(*page.shifts, 2, "and so is the undo of one");
        assert_eq!(page.strokes[1].points[0].x, 100.0, "the line is back");
    }
}
