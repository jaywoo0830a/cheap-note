//! The engine's head: what changed, what to present, and what to write — and nothing on a clock.
//!
//! Contract: `doc/ENGINE.md` §3.3 and §4.3. The whole of the engine's control is one state machine:
//!
//! | State | What may run | Which promise |
//! |---|---|---|
//! | `Idle` | nothing — no present, no timer, no poll | E1 |
//! | `Drawing` | the band a batch laid, one present per present's worth of change | E2, E3 |
//! | `Settling` | nothing but the quiet window's clock | E4 |
//! | `Writing` | the writer's own thread; the engine is `Idle` again | E4 |
//! | `Rebuilding` | the detail rebuild, off the frame | E5 |
//! | `Presenting` | one present of a fully described canvas | E5 |
//!
//! **`Idle` is the load-bearing state**, and it is a property rather than a mode: it is entered when the
//! last thing that could have changed anything has finished and left when one of four events arrives — a
//! batch of readings, a description from a frame, a page from the rasteriser, a one-shot change. There is
//! no idle *timer* because there is nothing to detect: §4.3, and E1 is 0 presents, not a slow present.

use std::time::{Duration, Instant};

use crate::band::Band;
use crate::footprint::Footprint;
use crate::journal::{Journal, JournalBatch};

/// The most frames a second the canvas will hand over, whatever the display can show (E3).
///
/// The present code's `FrameCap` (`note_ui::settings`), and here only as the arithmetic the presenter
/// needs. `None` is no cap of the engine's own; the display's clock is the platform's (`vsync`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameCap {
    Unlimited,
    Hertz(u32),
}

impl FrameCap {
    /// The shortest gap between two presents, or `None` when the engine has no cap of its own.
    pub fn interval(&self) -> Option<Duration> {
        match self {
            FrameCap::Unlimited => None,
            FrameCap::Hertz(hz) if *hz > 0 => Some(Duration::from_secs_f64(1.0 / f64::from(*hz))),
            FrameCap::Hertz(_) => None,
        }
    }
}

/// What the engine is doing. The status line reads this (§7.3), and the tests assert it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Drawing,
    Settling,
    Writing,
    Rebuilding,
    Presenting,
}

/// What a batch of readings asked the engine to do.
#[derive(Clone, Debug, Default)]
pub struct StrokeWork {
    /// The area to repair on the ink layer: the band this batch laid, merged.
    pub damage: Footprint,
    /// Whether this batch laid any ink at all — false for a hover, a dropped resample, an eraser that
    /// found nothing (E1).
    pub layed_ink: bool,
    /// Whether a present is now owed (subject to the cap, §6.2).
    pub owed: bool,
}

/// The engine: the layer's change, the journal's schedule, and the answer to "does this draw?".
#[derive(Debug)]
pub struct Engine {
    state: State,
    /// The cap the reader chose, which bounds how often a present may be handed over.
    cap: FrameCap,
    /// When the canvas was last handed over: the clock the cap is measured against.
    last_present: Option<Instant>,
    /// Whether something changed after the last present that a present has not yet shown.
    dirty: bool,
    /// The damage a present still owes: folded in until one is handed over (§6.2).
    presented: Footprint,
    /// The changed bands kept until the note may have them (E4).
    journal: Journal,
    /// When the pen last laid ink: the *settle* clock starts here, not the journal's.
    last_ink: Option<Instant>,
}

impl Engine {
    /// A new engine, idle, with a cap and nothing owed.
    pub fn new(cap: FrameCap) -> Self {
        Engine {
            state: State::Idle,
            cap,
            last_present: None,
            dirty: false,
            presented: Footprint::empty(),
            journal: Journal::new(),
            last_ink: None,
        }
    }

    /// What the engine is doing.
    pub fn state(&self) -> State {
        self.state
    }

    /// The cap in force.
    pub fn cap(&self) -> FrameCap {
        self.cap
    }

    /// Replaces the cap. A new cap clears the last present, so a cap moved down is not held back by a
    /// frame drawn a moment ago, and one moved up does not wait for the old interval (§6.2).
    pub fn set_cap(&mut self, cap: FrameCap) {
        self.cap = cap;
        self.last_present = None;
    }

    /// One batch of readings, at `now`: the band it laid, and what the layer owes.
    ///
    /// A batch that laid no ink leaves the engine `Idle` and owes no present (E1). A batch that laid ink
    /// goes to `Drawing`, records its band in the journal (E4), and owes a present — the cap decides
    /// whether *this* wake may hand it over (§6.2).
    pub fn on_batch(&mut self, band: Band, bytes: usize, now: Instant) -> StrokeWork {
        if band.is_empty() {
            // A hover, a dropped resample, an eraser that found nothing: nothing to show, nothing to
            // write, and no state change. This is E1's answer in the one place a reading arrives.
            self.state = if self.dirty {
                State::Drawing
            } else {
                State::Idle
            };

            return StrokeWork::default();
        }

        let damage = band.damage();
        self.journal.record(damage.clone(), bytes, now);

        for rect in damage.rects() {
            self.presented.add(*rect);
        }

        self.dirty = true;
        self.last_ink = Some(now);
        self.state = State::Drawing;

        StrokeWork {
            damage,
            layed_ink: true,
            owed: true,
        }
    }

    /// The pen lifted, or the batch ended, at `now`: the engine waits out the quiet window (E4).
    pub fn on_lift(&mut self, now: Instant) {
        if self.last_ink.is_some() {
            self.last_ink = Some(now);
            self.state = State::Settling;
        }
    }

    /// Whether the cap lets a present be handed over at `now`.
    ///
    /// The one comparison of §6.2: `since < interval` means not yet. A frame that arrives early is
    /// *dropped, not deferred* — the newest description is what the next one draws anyway.
    pub fn may_present(&self, now: Instant) -> bool {
        match (self.cap.interval(), self.last_present) {
            (Some(interval), Some(last)) => now.saturating_duration_since(last) >= interval,
            _ => true,
        }
    }

    /// What the presenter owes the compositor, or `None` — the `Idle` answer (E1).
    ///
    /// The answer is the *damage since the last present*, merged: the newest description is what a
    /// present draws, so a frame the cap dropped is not shown later — it is folded into the next one
    /// (§6.2).
    pub fn present(&mut self, now: Instant) -> Option<Footprint> {
        if !self.dirty || !self.may_present(now) {
            return None;
        }

        let showed = std::mem::replace(&mut self.presented, Footprint::empty());
        self.dirty = false;
        self.last_present = Some(now);
        self.state = State::Presenting;

        Some(showed)
    }

    /// The journal's due writes at `now`, which is what the housekeeping pass drains (E4).
    pub fn due_writes(&mut self, now: Instant) -> Option<JournalBatch> {
        let batch = self.journal.due(now)?;
        self.state = State::Writing;
        Some(batch)
    }

    /// The journal's contents, handed over at once: a page close, an undo, an erase, a `Clear` (E4).
    pub fn flush(&mut self) -> Option<JournalBatch> {
        self.journal.flush()
    }

    /// Whether the journal still holds ink the note has not been told about.
    pub fn owes_a_write(&self) -> bool {
        !self.journal.is_empty()
    }

    /// The engine goes back to `Idle` once everything asked of it has been done.
    pub fn settle(&mut self) {
        if !self.dirty && self.journal.is_empty() {
            self.state = State::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::band::Band;
    use crate::journal::{CAP as JOURNAL_CAP, QUIET};
    use crate::place::Placement;
    use note_format::ink::Piece;
    use note_format::pages::Quarters;

    const HZ_120: FrameCap = FrameCap::Hertz(120);

    fn band(at: f32) -> Band {
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0));
        let span = Piece::Quad {
            left_from: [at, 0.0],
            left_to: [at + 4.0, 0.0],
            right_to: [at + 4.0, 4.0],
            right_from: [at, 4.0],
        };

        Band::of(&[span], &place)
    }

    /// E1: a still canvas draws nothing.
    ///
    /// The engine starts `Idle`, a thousand asks of a presenter answer `None`, and nothing about the
    /// engine changes. This is the promise that makes the app cheap: not a slow present, but a present
    /// that never happens.
    #[test]
    fn stills_cost_nothing() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let now = Instant::now();

        assert_eq!(engine.state(), State::Idle, "a new engine was not idle");

        for step in 0..1000 {
            assert!(
                engine.present(now + Duration::from_millis(step)).is_none(),
                "an idle engine handed over a frame"
            );
        }

        assert_eq!(
            engine.state(),
            State::Idle,
            "asking for a present woke the engine"
        );
        assert!(!engine.owes_a_write(), "an idle engine owed a write");
    }

    /// E1 again, from the other side: a batch that lays no ink — a hover, a dropped resample — draws
    /// nothing, writes nothing and leaves the engine where it found it.
    #[test]
    fn a_reading_that_lays_no_ink_draws_nothing() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let now = Instant::now();

        let work = engine.on_batch(Band::empty(), 0, now);

        assert!(!work.layed_ink, "an empty band claimed to have laid ink");
        assert!(!work.owed, "an empty band owed a present");
        assert_eq!(engine.state(), State::Idle, "an empty band woke the engine");
        assert!(!engine.owes_a_write(), "an empty band owed a write");
        assert!(engine.present(now).is_none(), "an empty band was presented");
    }

    /// E2 and E3: a reading that lays ink goes to `Drawing`, owes a present, and the present carries the
    /// band's damage — not the page.
    #[test]
    fn a_reading_that_lays_ink_owes_its_band() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let now = Instant::now();

        let work = engine.on_batch(band(100.0), 60, now);

        assert!(work.layed_ink, "a reading that laid ink said it had not");
        assert!(work.owed, "a reading that laid ink owed no present");
        assert_eq!(
            engine.state(),
            State::Drawing,
            "the engine did not go to Drawing"
        );
        assert!(
            !work.damage.is_empty(),
            "a reading that laid ink damaged nothing"
        );

        let presented = engine
            .present(now)
            .expect("an unlimited cap refused a present");
        assert!(!presented.is_empty(), "the presented footprint was empty");
        assert_eq!(
            engine.state(),
            State::Presenting,
            "a present did not go to Presenting"
        );
    }

    /// E4 from the engine's side: a band is written by a pause, not by the reading that made it.
    #[test]
    fn ink_is_written_by_the_pause_and_not_the_reading() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let start = Instant::now();

        engine.on_batch(band(100.0), 60, start);

        assert!(
            engine.due_writes(start).is_none(),
            "a fresh band was written on the reading that made it"
        );
        assert!(engine.owes_a_write(), "a fresh band owed no write at all");

        let batch = engine
            .due_writes(start + QUIET)
            .expect("a pause did not write the band behind it");
        assert!(!batch.damage.is_empty(), "the pause's batch was empty");
        assert!(
            !engine.owes_a_write(),
            "the engine still owed a write after one"
        );
    }

    /// A page close flushes at once, because an edit cannot be expressed by a band.
    #[test]
    fn a_page_close_writes_at_once() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let now = Instant::now();

        engine.on_batch(band(100.0), 60, now);

        let batch = engine.flush().expect("a page close wrote nothing");
        assert!(!batch.damage.is_empty(), "the flushed batch was empty");
        assert!(
            engine.flush().is_none(),
            "a second flush wrote an empty batch"
        );
    }

    /// E5 / §6.2: the cap is a floor on the gap between two presents, and a frame that arrives early is
    /// dropped rather than deferred.
    #[test]
    fn the_cap_bounds_how_often_a_frame_is_handed_over() {
        let mut engine = Engine::new(HZ_120);
        let start = Instant::now();

        engine.on_batch(band(0.0), 60, start);
        engine
            .present(start)
            .expect("the first present was refused");

        // A second band arrives immediately: the cap says not yet.
        engine.on_batch(band(20.0), 60, start + Duration::from_millis(1));
        assert!(
            engine.present(start + Duration::from_millis(2)).is_none(),
            "the cap handed over a frame inside its interval"
        );

        // …and one interval later it may.
        assert!(
            engine
                .present(start + Duration::from_secs_f64(1.0 / 120.0))
                .is_some(),
            "the cap refused a frame one interval after the last"
        );
    }

    /// A new cap clears the last present: a cap moved down is not held back by a frame drawn a moment
    /// ago, and one moved up does not wait for the old interval.
    #[test]
    fn changing_the_cap_clears_the_last_present() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let start = Instant::now();

        engine.on_batch(band(0.0), 60, start);
        engine.present(start);

        engine.set_cap(HZ_120);
        engine.on_batch(band(20.0), 60, start + Duration::from_millis(1));

        assert!(
            engine.present(start + Duration::from_millis(2)).is_some(),
            "a new cap was held back by a frame drawn under the old one"
        );
    }

    /// The cap's arithmetic: a hundred and twenty hertz is 1/120 of a second, and no cap is none.
    #[test]
    fn a_cap_is_its_interval() {
        assert!(
            FrameCap::Unlimited.interval().is_none(),
            "an unlimited cap had an interval"
        );

        let interval = HZ_120.interval().expect("120 Hz had no interval");
        assert!(
            (interval.as_secs_f64() - 1.0 / 120.0).abs() < 1e-6,
            "120 Hz was not 1/120 of a second"
        );

        assert!(
            FrameCap::Hertz(0).interval().is_none(),
            "a zero-hertz cap was not treated as no cap"
        );
    }

    /// The two clocks are not the same clock: the journal's cap is the note's 500 ms, and it is reached
    /// by a hand that never pauses even though no gap is ever as long as the quiet window.
    #[test]
    fn the_journal_cap_is_the_notes_own() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let start = Instant::now();

        for step in 0..5 {
            let now = start + Duration::from_millis(step * 100);
            engine.on_batch(band(step as f32 * 30.0), 100, now);
            assert!(
                engine.due_writes(now).is_none(),
                "a busy hand was written on its own reading"
            );
        }

        assert!(
            engine.due_writes(start + JOURNAL_CAP).is_some(),
            "a hand that never paused was never written"
        );
    }

    /// `Settling` is entered on a lift and is not `Idle`: the quiet window is still open, so the ink is
    /// not yet written and the engine is not yet asleep.
    #[test]
    fn a_lift_settles_instead_of_sleeping() {
        let mut engine = Engine::new(FrameCap::Unlimited);
        let now = Instant::now();

        engine.on_batch(band(0.0), 60, now);
        engine.on_lift(now);

        assert_eq!(
            engine.state(),
            State::Settling,
            "a lift did not settle the engine"
        );
        assert!(engine.owes_a_write(), "a lift dropped the ink it had");
    }
}
