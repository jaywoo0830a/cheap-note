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
        assert_eq!(
            meter.samples(),
            0,
            "nothing is recorded until the scope ends"
        );
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

    let unmeasured = timings.summary();
    assert!(
        unmeasured.contains("ink —"),
        "an unmeasured path is not a zero: {unmeasured}"
    );
    assert!(
        unmeasured.contains("pump — ms (— /s)"),
        "and neither is an unmeasured rate: {unmeasured}"
    );

    timings.ink.record(Duration::from_micros(250));
    timings.count_painted(12, 240, 3);
    let measured = timings.summary();

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

    let first = timings.summary();
    let second = timings.summary();

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

    assert_eq!(
        measured.over,
        Duration::from_secs(2),
        "the window it covered"
    );
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
    assert_eq!(
        measured.frames.samples, 1,
        "a three-second gap is not a frame"
    );
    assert_eq!(measured.frames.slowest, Duration::from_millis(8));
    assert_eq!(measured.frames.idle, 1, "and it is not lost either");

    session.start();
    session.record_frame(Duration::from_secs(3));
    let nothing = session.stop(Duration::from_secs(3));

    assert_eq!(nothing.frames.samples, 0);
    assert!(
        nothing.summary().contains("1 idle, 3.00 s longest"),
        "a session with nothing measured still says what it was: {}",
        nothing.summary()
    );
}

/// A frame a 60 Hz screen would not have got is counted as one, and an idle gap is not.
///
/// The count is what tells a session that was uniformly late from one that was fine except for a few
/// hitches — the two have the same shape of `min`/`max`/`avg` and differ only in how many frames went
/// over the line.
#[test]
fn a_slow_frame_is_counted_and_an_idle_gap_is_not() {
    let session = Session::default();
    session.start();

    session.record_frame(Duration::from_millis(8));
    session.record_frame(Duration::from_millis(20));
    // Just under the line: a frame that was late but not a frame the screen missed.
    session.record_frame(Duration::from_millis(16));
    session.record_frame(Duration::from_secs(3));

    let measured = session.stop(Duration::from_secs(4));

    assert_eq!(measured.frames.samples, 3);
    assert_eq!(measured.frames.slow, 1, "only the one over the line");
    assert_eq!(
        measured.frames.idle, 1,
        "the three-second gap is not a frame"
    );
    assert_eq!(measured.frames.longest_idle, Duration::from_secs(3));
    assert!(
        measured.summary().contains("1 over 16.7 ms"),
        "and the line says which line was crossed: {}",
        measured.summary()
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

/// PDF work is reported by kind, and a note with no PDF carries no zeros.
///
/// A whole page and a few slices are the same mean and nothing like each other — one is paid for on a
/// wake of its own and the other between frames, where it can take a frame's time — and the count is
/// what says which of the two a number came from. This is the clause that used to report the page
/// render alone, which is how a session that rendered no page at all still showed a page's cost.
#[test]
fn pdf_work_is_reported_by_kind() {
    let timings = Timings::default();

    assert_eq!(
        pdf_clause(&timings.pdf, &timings.pdf_slice),
        "—",
        "nothing rendered is nothing said"
    );

    timings.pdf.record(Duration::from_micros(9500));
    assert_eq!(
        pdf_clause(&timings.pdf, &timings.pdf_slice),
        "page 9.50 (9.50) ×1",
        "the whole page is named, and counted"
    );

    timings.pdf_slice.record(Duration::from_micros(4000));
    timings.pdf_slice.record(Duration::from_micros(4000));
    assert_eq!(
        pdf_clause(&timings.pdf, &timings.pdf_slice),
        "page 9.50 ×1, slice 4.00 (4.00) ×2",
        "and the slices sit beside it rather than being averaged into it"
    );
}

/// The present clause is the ink's own rate: the gap between two presents of the canvas, and what
/// that works out to per second.
///
/// Its own number rather than the frames' — the ink is presented by the pen's pump as well as by
/// a frame, so a session that read the frame rate would report the interface's pace as the ink's
/// (see [`Timings::present_gap`]).
#[test]
fn the_present_clause_is_the_ink_s_own_rate() {
    let timings = Timings::default();

    assert!(
        timings.summary().contains("present — ms (— /s)"),
        "nothing presented is nothing said: {}",
        timings.summary()
    );

    timings.present_gap.record(Duration::from_micros(6_250));
    timings.present_gap.record(Duration::from_micros(6_250));

    assert!(
        timings.summary().contains("present 6.25 ms (160 /s)"),
        "a hundred and sixty presents a second: {}",
        timings.summary()
    );
}
