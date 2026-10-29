//! Windows' own reading, turned into the view's.
//!
//! `pen-windows` reports `PenSample`: forty fields of what one `WM_POINTER` message carried, named
//! after the API it came from. The view asks for six of them, in [`Reading`]. This is that
//! translation, and it is the *only* place in the application where a Windows reading type and a view
//! reading type meet — which is what a second platform would write for itself in place of this file.

use note_ui::input::{Phase, Point, Reading, Tilt};
use pen_windows::{PenPhase, PenSample};

/// The view's reading, from the platform's.
pub fn reading_of(sample: &PenSample) -> Reading {
    Reading {
        phase: phase_of(sample.phase),
        id: sample.id,
        pixel: Point::new(sample.pixel.x, sample.pixel.y),
        // The mask decides, in `pen-windows`: `None` is a digitizer with no pressure axis, and
        // `Some(0.0)` is a nib resting on the paper. The two must not be folded together.
        pressure: sample.pressure,
        tilt: sample.tilt.map(|tilt| Tilt::new(tilt.x, tilt.y)),
        inverted: sample.inverted,
        eraser: sample.eraser,
        at_us: sample.at_us,
        delay_ms: sample.delay_ms,
    }
}

/// The view's phase, from the platform's.
///
/// One arm per variant, spelled out rather than transmuted: the two enums are the same *shape*
/// because they are the same idea, and a `match` is what makes a variant added on one side a compile
/// error on the other.
fn phase_of(phase: PenPhase) -> Phase {
    match phase {
        PenPhase::Idle => Phase::Idle,
        PenPhase::Enter => Phase::Enter,
        PenPhase::Hover => Phase::Hover,
        PenPhase::Down => Phase::Down,
        PenPhase::Move => Phase::Move,
        PenPhase::Up => Phase::Up,
        PenPhase::Leave => Phase::Leave,
        PenPhase::Cancel => Phase::Cancel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;
    use std::time::Instant;

    /// Every field of a reading survives the crossing — and the two `None`s stay `None`.
    ///
    /// The masks are what this is about as much as the numbers: a digitizer with no pressure axis
    /// reports `pressure: None`, and `Some(0.0)` is a nib resting on the paper. A conversion that
    /// flattened the first into the second would draw every light touch at full width, forever — the
    /// one bug `pen-windows` documents at length (see its `info` module).
    #[test]
    fn a_reading_crosses_the_seam_whole() {
        let mut sample = PenSample {
            phase: PenPhase::Move,
            id: 7,
            pixel: pen_windows::Point::new(120.5, 340.25),
            pressure: Some(0.625),
            tilt: Some(pen_windows::Tilt::new(-12.0, 34.5)),
            inverted: true,
            eraser: true,
            at_us: 1_234_567,
            delay_ms: 9,
            ..PenSample::default()
        };

        let reading = reading_of(&sample);

        assert_eq!(reading.phase, Phase::Move);
        assert_eq!(reading.id, 7);
        assert_eq!(reading.pixel, Point::new(120.5, 340.25));
        assert_eq!(reading.pressure, Some(0.625));
        assert_eq!(
            reading.tilt.map(|tilt| (tilt.x, tilt.y)),
            Some((-12.0, 34.5))
        );
        assert!(reading.inverted);
        assert!(reading.eraser);
        assert_eq!(reading.at_us, 1_234_567);
        assert_eq!(reading.delay_ms, 9);

        // The mask, not the value: a pen without a pressure axis says so, and keeps saying so.
        sample.pressure = None;
        sample.tilt = None;
        let reading = reading_of(&sample);
        assert_eq!(reading.pressure, None, "no sensor is not a force of zero");
        assert_eq!(reading.tilt, None, "no sensor is not a level pen");
    }

    /// What the crossing costs, measured rather than assumed.
    ///
    /// The loop is the pen thread's own — a batch of readings, converted, put into the queue's
    /// backing vector, then cleared — because that is the only shape the cost is real in. Measured on
    /// a single reading in isolation the optimiser strips the fields nobody reads and reports a tenth
    /// of a nanosecond, which is a measurement of the compiler rather than of this program.
    ///
    /// The two buffer sizes are why the answer is "nothing": a `PenSample` is 96 bytes and a
    /// `Reading` 48, so the queue copies *half* the bytes per reading. The conversion that produces
    /// them costs less than the copy it saves.
    ///
    /// Printed rather than asserted: a bound tight enough to catch a regression would be a bound that
    /// fails on a loaded build machine. `cargo test --release -p cheap-note -- --nocapture` prints it.
    #[test]
    fn the_crossing_costs_what_it_costs() {
        /// The batch a message carries, and enough batches to be worth timing.
        const BATCH: usize = 64;
        const BATCHES: usize = 100_000;
        const COUNT: usize = BATCH * BATCHES;

        let mut samples = vec![PenSample::default(); BATCH];
        let mut readings: Vec<Reading> = Vec::with_capacity(BATCH);
        let mut checksum = 0u64;

        let started = Instant::now();

        for batch in 0..BATCHES {
            // A batch that changes every time: the conversion has to read what it copies.
            for (index, sample) in samples.iter_mut().enumerate() {
                sample.at_us = (batch * BATCH + index) as u64 * 13;
                sample.pixel = pen_windows::Point::new(index as f32, batch as f32);
            }

            readings.extend(samples.iter().map(reading_of));
            checksum = checksum.wrapping_add(readings[BATCH - 1].at_us);
            readings.clear();
        }

        let each = started.elapsed().as_secs_f64() / COUNT as f64;

        eprintln!(
            "\n── a reading crosses the seam ────────────────────────────────\n  \
             {:.2} ns each · Reading is {} bytes, PenSample {} · checksum {checksum}",
            each * 1e9,
            size_of::<Reading>(),
            size_of::<PenSample>(),
        );
    }
}
