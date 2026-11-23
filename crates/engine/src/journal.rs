//! The journal: what a reading changed, kept until the page is quiet enough to write it.
//!
//! Contract: `doc/ENGINE.md` §4.4, promise **E4** — *"ink reaches the disk inside a bounded window, and
//! the window closes early when the page settles."*
//!
//! The rule that makes it work is one sentence: **the window is the pause, not the clock.** A writing
//! hand is never quiet for [`QUIET`], so the journal does not flush per reading; a hand that pauses
//! between two words is a pause the note's writer can use. The hard ceiling ([`CAP`]) and the byte bound
//! ([`BYTES`]) are the two floors under that, for a hand that writes for a minute without pausing and
//! for a page of very long strokes.
//!
//! The journal is deliberately *not* the note's writer. It is a buffer with a rule for when to hand its
//! contents over; the thread that writes them is the note's, and this crate has no thread at all.

use std::time::{Duration, Instant};

use crate::footprint::Footprint;

/// How long a page must be untouched before its ink is handed to the writer: a pause, not a tick.
///
/// Half the note's own `BATCH_INTERVAL` (500 ms, `doc/STORE.md` §6). A pause that long is one a reader
/// makes between two words or at the end of a line, so the ink is written *while they are thinking*
/// rather than 500 ms later — and a hand that never pauses for it is still covered by [`CAP`].
pub const QUIET: Duration = Duration::from_millis(250);

/// The longest ink may wait, whatever the hand does: the note's own hard ceiling, unchanged.
pub const CAP: Duration = Duration::from_millis(500);

/// How much damage one journal holds before it hands its contents over on size alone.
///
/// A batch's dirty rows at their worst (200 strokes of a raw single-stroke blob) are about 60 KiB of
/// ink; this is four batches of headroom, so a writer that has fallen behind is asked to catch up
/// before the journal, not after it.
pub const BYTES: usize = 256 * 1024;

/// What the journal hands over: the rects to write, and how they were made.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalBatch {
    /// The areas that changed, merged: what the writer replays.
    pub damage: Footprint,
    /// How much longer than [`QUIET`] the pause was, in case a reader is asked why a write was late.
    pub late_by: Duration,
}

/// One page's changed bands, and when they may be written.
#[derive(Debug)]
pub struct Journal {
    damage: Footprint,
    /// When the *newest* band arrived. `None` when nothing has arrived since the last flush.
    since: Option<Instant>,
    /// When the *first* band of this run arrived: the cap is measured from here.
    started: Option<Instant>,
    /// A hard deadline: set on the first band, never moved, cleared on flush.
    deadline: Option<Instant>,
    /// An estimate of the bytes the damage stands for.
    bytes: usize,
}

impl Journal {
    /// An empty journal: nothing has been laid, and nothing is owed.
    pub fn new() -> Self {
        Journal {
            damage: Footprint::empty(),
            since: None,
            started: None,
            deadline: None,
            bytes: 0,
        }
    }

    /// Whether anything is waiting to be written.
    pub fn is_empty(&self) -> bool {
        self.damage.is_empty()
    }

    /// Records what a reading changed at `now`, and the size it stands for.
    ///
    /// Two clocks, and both are needed. `since` is the *newest* band, so a hand that keeps writing is
    /// never quiet and is written only by the cap — which is what stops a busy hand being written per
    /// reading. `started` is the *first* band of the run, so the cap is a ceiling and not a window a
    /// continuous hand keeps re-opening (E4, `doc/ENGINE.md` §4.4).
    pub fn record(&mut self, damage: Footprint, bytes: usize, now: Instant) {
        if damage.is_empty() {
            return;
        }

        for rect in damage.rects() {
            self.damage.add(*rect);
        }

        self.bytes += bytes;

        if self.started.is_none() {
            self.started = Some(now);
            self.deadline = Some(now + CAP);
        }

        self.since = Some(now);
    }

    /// Whether the journal must hand its contents over at `now`: a pause, the ceiling, or its size.
    pub fn is_due(&self, now: Instant) -> bool {
        if self.is_empty() {
            return false;
        }

        if self.bytes >= BYTES {
            return true;
        }

        // The pause is measured from the newest band: a hand still moving has not paused.
        let paused = self
            .since
            .is_some_and(|since| now.saturating_duration_since(since) >= QUIET);
        // …and the ceiling from the oldest unwritten one, so a hand that never pauses is still written.
        let capped = self.deadline.is_some_and(|deadline| now >= deadline);

        paused || capped
    }

    /// Hands the journal over if it is due, and answers what was handed over.
    ///
    /// `None` is the ordinary answer of a hand that is still writing (E4's "not per reading"), and an
    /// empty journal is never "due": a flush of nothing would be a write of nothing.
    pub fn due(&mut self, now: Instant) -> Option<JournalBatch> {
        if !self.is_due(now) {
            return None;
        }

        let waited = self.waited(now);
        self.take(waited)
    }

    /// Hands the journal over *now*, whatever the clock says: what a page close, an undo, an erase or a
    /// `Clear` calls. An edit cannot be expressed by a band, so it does not wait for a pause.
    ///
    /// Answers `None` when there was nothing to hand over, which is what makes the caller's code uniform.
    pub fn flush(&mut self) -> Option<JournalBatch> {
        if self.is_empty() {
            return None;
        }

        let waited = self
            .started
            .map(|started| Instant::now().saturating_duration_since(started))
            .unwrap_or_default();
        self.take(waited)
    }

    /// How long the oldest unwritten band has waited, measured from when the run began.
    fn waited(&self, now: Instant) -> Duration {
        self.started
            .map(|started| now.saturating_duration_since(started))
            .unwrap_or_default()
    }

    fn take(&mut self, waited: Duration) -> Option<JournalBatch> {
        if self.is_empty() {
            return None;
        }

        let damage = std::mem::replace(&mut self.damage, Footprint::empty());
        self.since = None;
        self.started = None;
        self.deadline = None;
        self.bytes = 0;

        Some(JournalBatch {
            damage,
            late_by: waited.saturating_sub(QUIET),
        })
    }
}

impl Default for Journal {
    fn default() -> Self {
        Journal::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::Rect;

    /// A band one pixel wide and ten tall, standing for whatever a reading swept.
    fn band(x: f32) -> Footprint {
        let mut foot = Footprint::empty();
        foot.add(Rect::from_corners((x, 0.0), (x + 1.0, 10.0)));
        foot
    }

    /// E4: a pause is written before it is felt, and a hand that is still writing is not written per
    /// reading.
    ///
    /// The two halves are one test because they are one rule: at 249 ms a fresh band is *not* due, and at
    /// 250 ms it is. A journal that flushed on every `record` would pass the second half alone and lose
    /// the whole point of the window.
    #[test]
    fn a_pause_is_written_before_it_is_felt() {
        let start = Instant::now();
        let mut journal = Journal::new();

        journal.record(band(0.0), 300, start);

        assert!(
            journal.due(start + Duration::from_millis(249)).is_none(),
            "a band 249 ms old was written as if the hand had paused"
        );

        let written = journal
            .due(start + QUIET)
            .expect("a pause of QUIET did not write the ink behind it");

        assert!(!written.damage.is_empty(), "an empty batch was handed over");
        assert!(
            journal.is_empty(),
            "the journal held its contents after handing them over"
        );
    }

    /// A hand that never pauses is still written: the cap is a ceiling, and it is the note's own 500 ms
    /// rather than a number the engine chose.
    #[test]
    fn a_hand_that_never_pauses_is_written_at_the_cap() {
        let start = Instant::now();
        let mut journal = Journal::new();

        // A reading every 100 ms: no gap is ever as long as QUIET.
        for step in 0..5 {
            let now = start + Duration::from_millis(step * 100);
            journal.record(band(step as f32 * 2.0), 100, now);
            assert!(
                journal.due(now).is_none(),
                "a band was written on the reading that made it"
            );
        }

        let batch = journal
            .due(start + CAP)
            .expect("a run of writing with no pause reached the cap and was not written");

        assert!(!batch.damage.is_empty(), "the cap wrote nothing");
        assert!(
            journal.is_empty(),
            "the journal held its contents after the cap"
        );
    }

    /// The size bound closes the journal on its own, without a pause and before the cap.
    #[test]
    fn a_journal_closes_on_its_own_size() {
        let start = Instant::now();
        let mut journal = Journal::new();

        journal.record(band(0.0), BYTES, start);

        assert!(
            journal.is_due(start),
            "a journal at its byte bound was not due"
        );
    }

    /// An edit does not wait for a pause: a page close, an undo, an erase and a `Clear` flush at once,
    /// because none of them can be expressed by a band.
    #[test]
    fn an_edit_is_written_at_once() {
        let mut journal = Journal::new();
        let now = Instant::now();

        journal.record(band(0.0), 50, now);

        assert!(
            !journal.is_due(now),
            "a fresh band was due before any time had passed"
        );

        let batch = journal.flush().expect("an explicit flush wrote nothing");
        assert!(!batch.damage.is_empty(), "the flushed batch was empty");
        assert!(
            journal.flush().is_none(),
            "a second flush wrote an empty batch"
        );
    }

    /// Nothing is ever written for nothing: an empty journal is never due, whatever the clock says.
    #[test]
    fn an_empty_journal_is_never_due() {
        let journal = Journal::new();
        let far_future = Instant::now() + Duration::from_secs(3600);

        assert!(
            !journal.is_due(far_future),
            "an empty journal claimed to be due"
        );
    }
}
