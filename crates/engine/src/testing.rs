//! A fake surface: a half that counts instead of drawing, so the promises are asserted rather than
//! eyeballed (`doc/ENGINE.md` §7.2 and the `cheap-note-engine-test` idea of §5.1).
//!
//! It records what it was asked to draw and how many times each corner was touched — which is E6 ("no
//! pixel is shaded twice for one reading") and E1 ("a still canvas draws nothing") in a form a test can
//! read. It compiles only for tests or behind the `testing` feature, so no release build carries it.

use std::sync::{Arc, Mutex};

use crate::band::Triangle;
use crate::footprint::Rect;
use crate::surface::{Frame, PresentResult, Surface, SurfaceError};

/// What a [`FakeSurface`] was asked to do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Drawn {
    /// Every repair the engine asked for, in order.
    pub repairs: Vec<Rect>,
    /// Every triangle filled, with its colour, in order.
    pub fills: Vec<(Triangle, u32)>,
    /// Every present the engine handed over.
    pub presents: usize,
    /// The largest number of times any one corner was touched by the fills recorded.
    pub overlaps: usize,
}

impl Drawn {
    /// How many triangles were filled.
    pub fn triangles(&self) -> usize {
        self.fills.len()
    }

    /// The area repaired.
    pub fn repaired_area(&self) -> f32 {
        self.repairs.iter().map(Rect::area).sum()
    }
}

/// A surface that records, and never touches a GPU.
#[derive(Clone, Default)]
pub struct FakeSurface {
    size: (f32, f32),
    drawn: Arc<Mutex<Drawn>>,
    /// How many presents to refuse before accepting one, standing for a layer whose drawable is not
    /// ready (`PresentResult::NotReady`).
    refuse: usize,
}

impl FakeSurface {
    /// A fake surface the size of a window.
    pub fn new(width: f32, height: f32) -> Self {
        FakeSurface {
            size: (width, height),
            drawn: Arc::new(Mutex::new(Drawn::default())),
            refuse: 0,
        }
    }

    /// This surface refuses its next `count` presents, as a layer with no drawable does.
    pub fn refusing(mut self, count: usize) -> Self {
        self.refuse = count;
        self
    }

    /// What it was asked to do.
    pub fn drawn(&self) -> Drawn {
        self.drawn.lock().expect("the fake surface's log").clone()
    }

    /// Forgets what it was asked to do, so a test can look at one frame at a time.
    pub fn clear(&self) {
        *self.drawn.lock().expect("the fake surface's log") = Drawn::default();
    }
}

impl Surface for FakeSurface {
    fn size(&self) -> (f32, f32) {
        self.size
    }

    fn resize(&mut self, width: f32, height: f32) {
        self.size = (width, height);
    }

    fn draw(&mut self, frame: &Frame) -> Result<PresentResult, SurfaceError> {
        if self.refuse > 0 {
            self.refuse -= 1;
            return Ok(PresentResult::NotReady);
        }

        let mut drawn = self.drawn.lock().expect("the fake surface's log");
        drawn.repairs.extend(frame.ink.repair.iter().copied());

        for band in &frame.ink.bands {
            for triangle in &band.triangles {
                drawn.fills.push((*triangle, band.colour));
            }
        }

        drawn.presents += 1;
        drawn.overlaps = drawn.overlaps.max(touch_count(&drawn.fills));

        Ok(PresentResult::Presented)
    }
}

/// The largest number of times one corner appears among the triangles filled.
fn touch_count(fills: &[(Triangle, u32)]) -> usize {
    let mut corners: Vec<(f32, f32)> = Vec::with_capacity(fills.len() * 3);

    for (triangle, _) in fills {
        corners.push(triangle.a);
        corners.push(triangle.b);
        corners.push(triangle.c);
    }

    let mut most = 0;
    for corner in &corners {
        let count = corners
            .iter()
            .filter(|other| (other.0 - corner.0).abs() < 1e-3 && (other.1 - corner.1).abs() < 1e-3)
            .count();
        most = most.max(count);
    }

    most
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::band::Band;
    use crate::place::Placement;
    use crate::surface::{BandDraw, FillRule, InkDraw};
    use note_format::ink::Piece;
    use note_format::pages::Quarters;

    /// A fake surface starts with nothing drawn, and a frame with nothing in it presents once.
    #[test]
    fn a_fake_surface_records_what_it_is_asked() {
        let mut fake = FakeSurface::new(800.0, 600.0);
        assert_eq!(fake.size(), (800.0, 600.0), "the fake had the wrong size");

        let result = fake
            .draw(&Frame::default())
            .expect("a fake never fails to draw");

        assert_eq!(result, PresentResult::Presented);
        assert_eq!(fake.drawn().presents, 1, "a present was not counted");
        assert_eq!(
            fake.drawn().triangles(),
            0,
            "an empty frame filled a triangle"
        );
    }

    /// A refused present is `NotReady`, not a failure, and nothing is counted: the layer had no drawable
    /// and the canvas stays dirty (`doc/ENGINE.md` §5.6).
    #[test]
    fn a_refused_present_is_not_an_error() {
        let mut fake = FakeSurface::new(800.0, 600.0).refusing(1);

        assert_eq!(
            fake.draw(&Frame::default()),
            Ok(PresentResult::NotReady),
            "a refused present was not NotReady"
        );
        assert_eq!(fake.drawn().presents, 0, "a refused present was counted");
    }

    /// A band is filled once: a trapezoid's shared diagonal corners are touched twice by the two
    /// triangles, and never more — which is what a union filled through a stencil produces.
    #[test]
    fn a_band_is_filled_once() {
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0));
        let span = Piece::Quad {
            left_from: [0.0, 0.0],
            left_to: [10.0, 0.0],
            right_to: [10.0, 4.0],
            right_from: [0.0, 4.0],
        };
        let band = Band::of(&[span], &place);

        let mut fake = FakeSurface::new(800.0, 600.0);
        let frame = Frame {
            base: Default::default(),
            ink: InkDraw {
                repair: band.damage().rects().to_vec(),
                bands: vec![BandDraw {
                    triangles: band.pieces().to_vec(),
                    colour: 0xFF00_0000,
                    rule: FillRule::NonZero,
                }],
            },
        };

        fake.draw(&frame).expect("a fake never fails to draw");

        let drawn = fake.drawn();
        assert_eq!(drawn.triangles(), 2, "a trapezoid was not two triangles");
        assert_eq!(
            drawn.overlaps, 2,
            "a corner of the band was touched by more than its two triangles"
        );
        assert!(drawn.repaired_area() > 0.0, "a band repaired nothing");
    }
}
