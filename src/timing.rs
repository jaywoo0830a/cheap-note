//! What the app is actually spending its time on.
//!
//! ## Why this exists
//!
//! "The writing feels laggy" has half a dozen plausible causes — the digitizer's own delay, the
//! pump's interval, the ink model, building the frame, painting it, or a PDF page being rasterised
//! in the middle of one — and guessing between them is how optimisation work gets spent on the
//! wrong one. Every hot path here records its own duration into a [`Meter`], and the status line
//! prints one line built from all of them, so the answer is a number on screen rather than an
//! opinion.
//!
//! ## What it costs
//!
//! Two `Instant::now()` calls and three relaxed atomic adds per measurement — tens of nanoseconds,
//! against paths that take tens of *micro*seconds. Nothing here allocates, locks, or formats
//! anything until the status line asks for it.
//!
//! ## What the numbers mean
//!
//! * `pen→app` is the one that answers "how responsive is this": how long a reading waited between
//!   the digitizer producing it and this process acting on it, including the system's own delay
//!   (which no code here can remove — see `PenSample::delay_ms`).
//! * `pump` is the gap between two wakes of the ink pump — that is, how often the pen handed the app
//!   a batch. It is the number that says whether the ink is reaching the screen at the rate the pen
//!   reports it; it is no longer compared with a timer the app set, because there is no timer.
//! * `ink`, `render`, `paint` and `pdf` split the per-frame work. `paint` growing with the amount
//!   of ink on the page is the shape of the immediate-mode renderer, and the counters beside it
//!   say how much of that ink was actually on screen.
//! * A [`Session`] is the other half of that, and the half a live line cannot give: the numbers above
//!   describe the frame that is happening, and a session describes a stretch of frames a person chose
//!   to measure — the fastest, the slowest and the mean of the wait between them, and of the pen's own
//!   wait for one. It is started and stopped by hand, and its answer is read once, afterwards.
//!
//! The line itself is not on a clock: it is rebuilt when the user changes something and when a session
//! is started or stopped, and no longer while nothing is happening. A number that moves on its own is a
//! number nobody can read, which is what a session replaced.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// One measured path, accumulated over the session.
///
/// The mean is over the whole session; the worst is reset by whoever reads it (see
/// [`Meter::take_worst`]) so the status line can report the worst *recent* sample instead of the
/// worst one ever, which a single hiccup at startup would otherwise pin forever.
#[derive(Debug, Default)]
pub struct Meter {
    /// How many samples were recorded.
    samples: AtomicU64,
    /// The sum of every sample, in nanoseconds.
    total_nanos: AtomicU64,
    /// The slowest sample since the worst was last taken, in nanoseconds.
    worst_nanos: AtomicU64,
    /// The most recent sample, in nanoseconds.
    last_nanos: AtomicU64,
}

impl Meter {
    /// Adds one sample.
    pub fn record(&self, elapsed: Duration) {
        let nanos = elapsed.as_nanos() as u64;

        self.samples.fetch_add(1, Ordering::Relaxed);
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.last_nanos.store(nanos, Ordering::Relaxed);
        // `fetch_max` rather than a load/store pair: the paint callback and the pump are both on
        // the main thread today, but a meter that is only correct on one thread is a trap for
        // whoever moves a path onto another one.
        self.worst_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    /// How many samples were recorded.
    pub fn samples(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }

    /// The mean of every sample, or `None` before the first one.
    pub fn mean(&self) -> Option<Duration> {
        let samples = self.samples();
        if samples == 0 {
            return None;
        }

        Some(Duration::from_nanos(
            self.total_nanos.load(Ordering::Relaxed) / samples,
        ))
    }

    /// The most recent sample.
    pub fn last(&self) -> Option<Duration> {
        (self.samples() != 0).then(|| Duration::from_nanos(self.last_nanos.load(Ordering::Relaxed)))
    }

    /// The slowest sample since this was last called, resetting it.
    pub fn take_worst(&self) -> Option<Duration> {
        let worst = self.worst_nanos.swap(0, Ordering::Relaxed);
        (worst != 0).then(|| Duration::from_nanos(worst))
    }
}

/// Times one scope, and records it when the scope ends.
///
/// Held in a binding that starts with an underscore, which is the whole interface:
///
/// ```ignore
/// let _timed = timing::measure(&self.timings.ink);
/// // ... the work ...
/// ```
///
/// The lifetime tie to the meter is what makes it impossible to record into a meter that has been
/// dropped.
pub struct Measured<'a> {
    /// Where the sample goes.
    meter: &'a Meter,
    /// When the scope began.
    started: Instant,
}

impl Drop for Measured<'_> {
    fn drop(&mut self) {
        self.meter.record(self.started.elapsed());
    }
}

/// Starts timing a scope against `meter`.
pub fn measure(meter: &Meter) -> Measured<'_> {
    Measured {
        meter,
        started: Instant::now(),
    }
}

/// How long an interval has to be before it is called idle rather than slow.
///
/// A session is measured by a person who is writing, and writing includes thinking: the pause between
/// two words, the look at the page, the reach for the pen. None of those is a slow frame, and the
/// longest one would be the only number a session reported if they were measured as one — which is
/// exactly the number this is meant to find. They are counted instead of measured, so a session says
/// both what the slowest real frame was and how much of the window was not frames at all.
pub const IDLE_INTERVAL: Duration = Duration::from_millis(250);

/// The running statistics of one kind of interval: how many, how fast, how slow, and what they average.
///
/// Atomics like everything else here — a session is fed from the frame path and read when it is stopped
/// — and the one operation an atomic counter cannot do in a single `fetch_*` is a minimum, so it is a
/// `fetch_min` (which is one instruction, not a loop). Nothing here is on the frame's critical path in
/// any sense that matters: a handful of relaxed writes per frame, into a cache line that is warm.
#[derive(Debug)]
pub struct Intervals {
    /// How many intervals were measured, leaving out the idle ones.
    samples: AtomicU64,
    /// The sum of those, in nanoseconds.
    total_nanos: AtomicU64,
    /// The shortest, in nanoseconds. `u64::MAX` until one is measured.
    min_nanos: AtomicU64,
    /// The longest, in nanoseconds.
    max_nanos: AtomicU64,
    /// How many intervals were left out for being idle rather than slow.
    idle: AtomicU64,
}

impl Default for Intervals {
    fn default() -> Self {
        Intervals {
            samples: AtomicU64::new(0),
            total_nanos: AtomicU64::new(0),
            // A minimum has nowhere to start but the top: the first interval that arrives has to be
            // able to replace it.
            min_nanos: AtomicU64::new(u64::MAX),
            max_nanos: AtomicU64::new(0),
            idle: AtomicU64::new(0),
        }
    }
}

impl Intervals {
    /// Folds in one interval, or counts it as idle.
    pub fn record(&self, interval: Duration, idle: Duration) {
        if interval >= idle {
            self.idle.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let nanos = interval.as_nanos() as u64;

        self.samples.fetch_add(1, Ordering::Relaxed);
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.min_nanos.fetch_min(nanos, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    /// Forgets everything measured so far.
    pub fn reset(&self) {
        self.samples.store(0, Ordering::Relaxed);
        self.total_nanos.store(0, Ordering::Relaxed);
        self.min_nanos.store(u64::MAX, Ordering::Relaxed);
        self.max_nanos.store(0, Ordering::Relaxed);
        self.idle.store(0, Ordering::Relaxed);
    }

    /// What has been measured so far.
    ///
    /// Never `None`, because a session that measured *nothing* still has something to say: how much of
    /// its window was idle. A count of zero is what says the three durations mean nothing.
    pub fn moments(&self) -> Moments {
        let samples = self.samples.load(Ordering::Relaxed);
        let idle = self.idle.load(Ordering::Relaxed);

        if samples == 0 {
            return Moments {
                samples: 0,
                fastest: Duration::ZERO,
                slowest: Duration::ZERO,
                mean: Duration::ZERO,
                idle,
            };
        }

        Moments {
            samples,
            fastest: Duration::from_nanos(self.min_nanos.load(Ordering::Relaxed)),
            slowest: Duration::from_nanos(self.max_nanos.load(Ordering::Relaxed)),
            mean: Duration::from_nanos(self.total_nanos.load(Ordering::Relaxed) / samples),
            idle,
        }
    }
}

/// The shortest, longest and mean of a stretch of intervals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moments {
    /// How many intervals were measured. Zero means the three durations below mean nothing.
    pub samples: u64,
    /// The shortest interval.
    pub fastest: Duration,
    /// The longest.
    pub slowest: Duration,
    /// The mean.
    pub mean: Duration,
    /// How many intervals were left out for being idle rather than slow.
    pub idle: u64,
}

impl Moments {
    /// `8.47 avg (6.31-41.20) ms`, with the idle count when there was one.
    ///
    /// The average in the middle of the extremes, because that is how the three are read together: the
    /// mean is the pace a person feels, the fastest is what the machine can do when nothing gets in the
    /// way, and the slowest is what did. A `slowest` far above the mean is the stutter; its distance from
    /// the mean is how bad it was. An interval that was never measured has no average to be the middle
    /// of, and says so rather than reporting zeroes as if they were fast frames.
    fn extremes_ms(&self) -> String {
        if self.samples == 0 {
            return match self.idle {
                0 => String::from("—"),
                idle => format!("— ({idle} idle)"),
            };
        }

        format!(
            "{:.2} avg ({:.2}-{:.2}) ms{}",
            millis(self.mean),
            millis(self.fastest),
            millis(self.slowest),
            match self.idle {
                0 => String::new(),
                idle => format!(", {idle} idle"),
            },
        )
    }

    /// The whole clause: how many were measured, then the extremes.
    ///
    /// The count comes first because it is what decides whether the rest is worth reading — three
    /// intervals are an anecdote and four hundred are a measurement — and because a window with a pause
    /// in it holds fewer frames than its length suggests.
    fn summary_ms(&self) -> String {
        match self.samples {
            0 => self.extremes_ms(),
            samples => format!("{samples} · {}", self.extremes_ms()),
        }
    }
}

/// One measurement session: the window it was measured over, and what it came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    /// How long the session ran, from the moment it was started to the moment it was stopped.
    pub over: Duration,
    /// The interval between one frame and the next: how fast the screen was answering.
    pub frames: Moments,
    /// The pen's own wait over the same window — how long a reading had to wait to be acted on.
    pub response: Moments,
}

impl Measurement {
    /// The session as one clause of the status line.
    ///
    /// Read as: how long was measured, how fast the screen answered, and how fast the pen's own readings
    /// were answered. The rate is in frames per second because that is the form a stutter is read in —
    /// "118 fps, slowest 41 ms apart" says both what the pace was and what interrupted it.
    pub fn summary(&self) -> String {
        format!(
            "measured {:.1} s: frames {} · pen→app {}",
            self.over.as_secs_f64(),
            self.frame_clause(),
            self.response.summary_ms(),
        )
    }

    /// The frame interval as a rate and as the extremes it was made of.
    fn frame_clause(&self) -> String {
        // A rate from the mean, unless the mean is nothing: a clock too coarse to tell two frames apart
        // would otherwise be reported as an infinite number of them per second.
        if self.frames.samples == 0 || self.frames.mean.is_zero() {
            return self.frames.summary_ms();
        }

        format!(
            "{} · {:.0} fps · {}",
            self.frames.samples,
            // The rate the mean works out to, which is the count over the time those frames actually
            // took: a window with a pause in it holds fewer frames than its length suggests, and the idle
            // count at the end of the clause is what says so.
            1000.0 / millis(self.frames.mean),
            self.frames.extremes_ms(),
        )
    }
}

/// The measurement a person starts and stops by hand.
///
/// ## Why a session rather than a live number
///
/// Everything else in this module answers "what is happening right now", and right now is the one
/// thing that cannot be read while a hand is moving: the numbers change faster than the eye, a spike is
/// gone before it is noticed, and there is nothing to compare one look at the line against. A session
/// answers the question that *can* be answered — "was that minute of writing smooth?" — with the three
/// numbers that decide it: the fastest anything went, the slowest, and the average in between.
///
/// ## Why the window belongs to the person
///
/// Measuring in practice means: start it, write for a while, stop it, read one line. So the window is
/// whatever the person measuring wanted to measure rather than a clock in here — which is also what
/// makes two attempts comparable, since both were asked for the same way.
///
/// ## What it measures
///
/// Two intervals, and they are the two halves of "responsive". The **frame interval** is the wait
/// between one frame and the next, which no per-path meter in this module can see: `ink`, `render` and
/// `paint` measure what a frame *does*, and the wait between frames is the toolkit's scene, its upload
/// and its present. The **pen's own wait** is the input side — how long a reading sat before a frame
/// acted on it.
///
/// Intervals longer than [`IDLE_INTERVAL`] are counted rather than measured: a person reading is not a
/// person watching a slow frame.
#[derive(Debug, Default)]
pub struct Session {
    /// Whether one is running. Nothing is recorded into the counters while it is false, so a stopped
    /// session cannot be changed by the frames that come after it.
    running: AtomicBool,
    /// The intervals between frames.
    frames: Intervals,
    /// The pen's own waits, over the same window.
    response: Intervals,
}

impl Session {
    /// Starts a session, forgetting everything measured before it.
    pub fn start(&self) {
        self.frames.reset();
        self.response.reset();
        self.running.store(true, Ordering::Relaxed);
    }

    /// Ends it, and answers what it measured over `over`.
    ///
    /// The counters are left as they are rather than reset: a stopped session is a result, and a
    /// result that cleaned up after itself could not be read twice.
    pub fn stop(&self, over: Duration) -> Measurement {
        self.running.store(false, Ordering::Relaxed);
        self.measurement(over)
    }

    /// Whether a session is running.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Folds in the interval between two frames, if a session is running.
    pub fn record_frame(&self, interval: Duration) {
        if self.running() {
            self.frames.record(interval, IDLE_INTERVAL);
        }
    }

    /// Folds in one reading's wait for a frame, if a session is running.
    pub fn record_response(&self, waited: Duration) {
        if self.running() {
            self.response.record(waited, IDLE_INTERVAL);
        }
    }

    /// What it has measured so far, as of a session that has run for `over`.
    pub fn measurement(&self, over: Duration) -> Measurement {
        Measurement {
            over,
            frames: self.frames.moments(),
            response: self.response.moments(),
        }
    }
}

/// Every path the app measures, plus the counters that explain them.
///
/// All of it is scalar and atomic, so a frame can hold an `Arc<Timings>` — the paint callback
/// needs one, because it runs after the view has been borrowed.
#[derive(Debug, Default)]
pub struct Timings {
    /// Readings in, ink out: the whole of [`crate::ink::InkDocument::consume`].
    pub ink: Meter,
    /// Building a frame's geometry and element tree.
    pub render: Meter,
    /// The canvas paint callback: polygon building and quad issuing.
    pub paint: Meter,
    /// Rasterising a PDF page. Pdfium runs on this thread, so this is time the frame spent
    /// waiting rather than drawing.
    pub pdf: Meter,
    /// Building the rule geometry for a sheet that changed.
    pub ruling: Meter,
    /// The wait between the digitizer and this process acting on a reading.
    pub pen_latency: Meter,
    /// The gap between two wakes of the ink pump: how often the pen reported.
    pub pump_gap: Meter,
    /// The measurement a person starts and stops by hand, and what it found.
    ///
    /// The meters above answer "what is happening right now", each for its own path; this is the only
    /// place a stretch of time is measured as a whole, which is what makes a stutter visible at all —
    /// see [`Session`] for what it measures and why a live line could not.
    pub session: Session,

    /// Strokes the last frame painted.
    pub painted: AtomicU64,
    /// Outline vertices those strokes cost.
    pub vertices: AtomicU64,
    /// Strokes the last frame skipped because they were outside the sheet.
    pub culled: AtomicU64,
    /// Sheets' worth of rule geometry built over the session.
    pub rulings_built: AtomicU64,
    /// PDF pages rasterised over the session.
    pub pages_rasterised: AtomicU64,
}

impl Timings {
    /// Clears the counters that describe a single frame, before a frame records them.
    ///
    /// The durations are *not* cleared: a mean over the session and a worst since the last status
    /// rebuild are both more useful than a mean over one frame.
    pub fn start_frame(&self) {
        self.painted.store(0, Ordering::Relaxed);
        self.vertices.store(0, Ordering::Relaxed);
        self.culled.store(0, Ordering::Relaxed);
    }

    /// Adds one to the count of sheets' rule geometry built.
    pub fn count_ruling(&self) {
        self.rulings_built.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds to the count of PDF pages rasterised.
    pub fn count_rasterised(&self, pages: u64) {
        self.pages_rasterised.fetch_add(pages, Ordering::Relaxed);
    }

    /// Adds what a frame painted, for the paint callback.
    pub fn count_painted(&self, strokes: u64, vertices: u64, culled: u64) {
        self.painted.fetch_add(strokes, Ordering::Relaxed);
        self.vertices.fetch_add(vertices, Ordering::Relaxed);
        self.culled.fetch_add(culled, Ordering::Relaxed);
    }

    /// The whole measurement as one line of the status bar.
    ///
    /// Shaped for a line that is read at a glance: the mean, then the worst since this was last
    /// called in brackets, then the counters that make sense of the two. `pump` is reported
    /// against the interval the loop asked for, because "4 ms" only means anything next to
    /// "4 ms wanted".
    pub fn summary(&self, pump_interval: Duration) -> String {
        let wanted = pump_interval.as_secs_f64() * 1000.0;

        format!(
            "ink {}  render {}  paint {}  pdf {} ms  ·  pump {} (of {:.2})  ·  pen→app {}  ·  {} strokes {} px{}",
            span(&self.ink),
            span(&self.render),
            span(&self.paint),
            span(&self.pdf),
            span_mean(&self.pump_gap),
            wanted,
            span_mean(&self.pen_latency),
            self.painted.load(Ordering::Relaxed),
            self.vertices.load(Ordering::Relaxed),
            match self.culled.load(Ordering::Relaxed) {
                0 => String::new(),
                culled => format!(" ({culled} culled)"),
            },
        )
    }
}

/// `most recent (worst since this was last read)` in milliseconds.
///
/// The most recent sample rather than the mean: this line is read to answer "what is happening
/// now", and a mean over a session hides exactly the frames worth looking at.
fn span(meter: &Meter) -> String {
    let Some(last) = meter.last() else {
        return String::from("—");
    };

    match meter.take_worst() {
        Some(worst) => format!("{:.2} ({:.2})", millis(last), millis(worst)),
        None => format!("{:.2}", millis(last)),
    }
}

/// `mean` in milliseconds, for a meter whose spikes are not the interesting part.
fn span_mean(meter: &Meter) -> String {
    match meter.mean() {
        Some(mean) => format!("{:.2}", millis(mean)),
        None => String::from("—"),
    }
}

/// Milliseconds in a duration, as a float.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A meter that has never seen a sample has no mean, and does not pretend to.
    #[test]
    fn a_meter_with_no_samples_has_no_mean() {
        let meter = Meter::default();

        assert_eq!(meter.samples(), 0);
        assert_eq!(meter.mean(), None);
        assert_eq!(meter.last(), None);
        assert_eq!(meter.take_worst(), None);
    }

    /// The mean is over every sample, and the last is the last one.
    #[test]
    fn the_mean_is_over_every_sample() {
        let meter = Meter::default();

        meter.record(Duration::from_micros(100));
        meter.record(Duration::from_micros(300));

        assert_eq!(meter.samples(), 2);
        assert_eq!(meter.mean(), Some(Duration::from_micros(200)));
        assert_eq!(meter.last(), Some(Duration::from_micros(300)));
    }

    /// Taking the worst resets it, which is what makes it a rolling window instead of a
    /// life-time record that one startup hiccup would pin forever.
    #[test]
    fn taking_the_worst_resets_it() {
        let meter = Meter::default();

        meter.record(Duration::from_micros(100));
        meter.record(Duration::from_micros(900));
        assert_eq!(
            meter.take_worst(),
            Some(Duration::from_micros(900)),
            "the spike is reported"
        );

        meter.record(Duration::from_micros(200));
        assert_eq!(
            meter.take_worst(),
            Some(Duration::from_micros(200)),
            "the old spike is gone"
        );
        assert_eq!(
            meter.samples(),
            3,
            "resetting the worst does not reset the mean"
        );
    }

    /// A measured scope records exactly once, when it ends.
    #[test]
    fn a_measured_scope_records_when_it_ends() {
        let meter = Meter::default();

        {
            let _timed = measure(&meter);
            assert_eq!(meter.samples(), 0, "nothing is recorded until the scope ends");
        }

        assert_eq!(meter.samples(), 1);
    }

    /// The counters a frame accumulates start at zero, so a frame's numbers are its own.
    #[test]
    fn a_frame_starts_from_zero() {
        let timings = Timings::default();

        timings.count_painted(3, 30, 1);
        timings.start_frame();

        assert_eq!(timings.painted.load(Ordering::Relaxed), 0);
        assert_eq!(timings.vertices.load(Ordering::Relaxed), 0);
        assert_eq!(timings.culled.load(Ordering::Relaxed), 0);
    }

    /// The line names every path, and says so plainly when one has not run yet.
    #[test]
    fn the_summary_names_every_path() {
        let timings = Timings::default();

        let unmeasured = timings.summary(Duration::from_micros(4166));
        assert!(
            unmeasured.contains("ink —"),
            "an unmeasured path is not a zero: {unmeasured}"
        );
        assert!(
            unmeasured.contains("of 4.17"),
            "the wanted interval is reported: {unmeasured}"
        );

        timings.ink.record(Duration::from_micros(250));
        timings.count_painted(12, 240, 3);
        let measured = timings.summary(Duration::from_micros(4166));

        assert!(
            measured.contains("ink 0.25"),
            "a mean in milliseconds: {measured}"
        );
        assert!(
            measured.contains("(0.25)"),
            "with the worst in brackets: {measured}"
        );
        assert!(
            measured.contains("12 strokes 240 px"),
            "and the frame's counters: {measured}"
        );
        assert!(
            measured.contains("(3 culled)"),
            "including what was skipped: {measured}"
        );
    }

    /// Reading the summary twice must not lose the worst: it is a window that slides when it is
    /// read, not when it is written.
    #[test]
    fn reading_the_summary_twice_reports_the_worst_once() {
        let timings = Timings::default();
        timings.ink.record(Duration::from_millis(9));

        let first = timings.summary(Duration::from_micros(4166));
        let second = timings.summary(Duration::from_micros(4166));

        assert!(first.contains("(9.00)"), "the spike is reported: {first}");
        assert!(
            !second.contains("(9.00)"),
            "and is not reported again: {second}"
        );
        assert!(
            second.contains("ink 9.00"),
            "the mean does not depend on reading it: {second}"
        );
    }

    /// A session reports the fastest, the slowest and the mean of what was measured, over the window
    /// it was measured for.
    ///
    /// This is the reading the whole session exists for: three numbers that can be compared between one
    /// attempt and the next, which a line that moves on its own never gave.
    #[test]
    fn a_session_reports_the_fastest_the_slowest_and_the_mean() {
        let session = Session::default();
        session.start();

        session.record_frame(Duration::from_millis(8));
        session.record_frame(Duration::from_millis(6));
        session.record_frame(Duration::from_millis(16));

        let measured = session.stop(Duration::from_secs(2));

        assert_eq!(measured.over, Duration::from_secs(2), "the window it covered");
        assert_eq!(measured.frames.samples, 3, "every interval is counted");
        assert_eq!(measured.frames.fastest, Duration::from_millis(6));
        assert_eq!(measured.frames.slowest, Duration::from_millis(16));
        assert_eq!(measured.frames.mean, Duration::from_millis(10));
    }

    /// Nothing reaches a session that is not running, in either direction, and starting one forgets.
    ///
    /// The two halves of one rule: a stopped session is a *result*, and a result the frames after it
    /// could still change would be a number that means nothing.
    #[test]
    fn a_stopped_session_is_a_result() {
        let session = Session::default();

        session.record_frame(Duration::from_millis(8));
        assert_eq!(
            session.measurement(Duration::from_secs(1)).frames.samples,
            0,
            "a frame recorded before the session started is not in it"
        );

        session.start();
        session.record_frame(Duration::from_millis(8));
        assert_eq!(session.stop(Duration::from_secs(1)).frames.samples, 1);

        session.record_frame(Duration::from_millis(99));
        assert_eq!(
            session.measurement(Duration::from_secs(2)).frames.samples,
            1,
            "and a frame recorded after it stopped is not in it either"
        );

        session.start();
        assert_eq!(
            session.measurement(Duration::ZERO).frames.samples,
            0,
            "a new session starts from nothing"
        );
    }

    /// An idle gap is counted rather than measured, and a session that was all idle says so.
    ///
    /// A person thinking is not a person watching a slow frame, and the longest pause of a session would
    /// otherwise be the only number it reported — the opposite of what it is for.
    #[test]
    fn an_idle_gap_is_counted_rather_than_measured() {
        let session = Session::default();
        session.start();

        session.record_frame(Duration::from_millis(8));
        session.record_frame(Duration::from_secs(3));

        let measured = session.stop(Duration::from_secs(4));
        assert_eq!(measured.frames.samples, 1, "a three-second gap is not a frame");
        assert_eq!(measured.frames.slowest, Duration::from_millis(8));
        assert_eq!(measured.frames.idle, 1, "and it is not lost either");

        session.start();
        session.record_frame(Duration::from_secs(3));
        let nothing = session.stop(Duration::from_secs(3));

        assert_eq!(nothing.frames.samples, 0);
        assert!(
            nothing.summary().contains("1 idle"),
            "a session with nothing measured still says what it was: {}",
            nothing.summary()
        );
    }

    /// The line carries all three numbers, for the frames and for the pen's own wait.
    #[test]
    fn the_measurement_line_carries_the_three_numbers() {
        let session = Session::default();
        session.start();

        session.record_frame(Duration::from_millis(8));
        session.record_frame(Duration::from_millis(12));
        session.record_response(Duration::from_micros(400));

        let line = session.stop(Duration::from_secs(5)).summary();

        assert!(line.contains("measured 5.0 s"), "the window: {line}");
        assert!(
            line.contains("frames 2 · 100 fps · 10.00 avg (8.00-12.00) ms"),
            "the count, the rate it works out to, and the three numbers: {line}"
        );
        assert!(
            line.contains("pen→app 1 · 0.40 avg (0.40-0.40) ms"),
            "and the pen's own wait, counted the same way: {line}"
        );
    }
}
