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
//! * `pump` is the gap between two wakes of the frame loop against the interval it asked for. A
//!   gap that runs long is the timer oversleeping, which puts a ceiling on everything else.
//! * `ink`, `render`, `paint` and `pdf` split the per-frame work. `paint` growing with the amount
//!   of ink on the page is the shape of the immediate-mode renderer, and the counters beside it
//!   say how much of that ink was actually on screen.

use std::sync::atomic::{AtomicU64, Ordering};
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
    /// The gap between two wakes of the pump.
    pub pump_gap: Meter,

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
}
