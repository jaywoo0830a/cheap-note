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
