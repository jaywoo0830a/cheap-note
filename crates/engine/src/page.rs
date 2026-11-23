//! A whole page of ink, as the bands a canvas fills: the static path.
//!
//! The reading-by-reading path ([`crate::band`]) draws what a reading laid. This one draws what a page
//! *is* — a page read out of a note, a page turned to, the first frame of a document — and it is the
//! same geometry from the same source (`note_format::ink::ribbon`), so a page drawn this way and a page
//! drawn by reading look the same, because they are the same triangles.
//!
//! The one decision this module makes is the *culling*: a stroke wholly outside the visible box is not
//! turned into triangles at all. That is `Stroke::visible_in`, four comparisons against a box the stroke
//! already carries, and measured at 6.3 µs for 8,182 strokes (`doc/VIEW.md` §11) — cheaper than the
//! tree that would replace it, and it needs no index kept in step with every edit.

use std::sync::Arc;

use note_format::ink::{Bounds, Stroke};
use note_format::pages::Quarters;

use crate::band::{Band, EDGE_SLACK};
use crate::footprint::{Footprint, Rect};
use crate::place::Placement;

/// What a page's ink is drawn as: one band per stroke, and the damage they cover.
#[derive(Clone, Debug, Default)]
pub struct PageBands {
    bands: Vec<(u32, Band)>,
}

impl PageBands {
    /// The bands for a page of strokes, in the order painted, and only the strokes on screen.
    ///
    /// `detail` is the zoom rung the strokes' outlines were built for (`Stroke::detailed_at`), because
    /// a stroke carries the outline and pieces it was frozen with: the caller builds them for the rung
    /// it is drawing at (`InkDocument::set_zoom`) and this turns them into triangles for the placement
    /// it is drawing them in.
    pub fn of(strokes: &[Arc<Stroke>], visible: Bounds, place: &Placement, detail: f32) -> Self {
        let mut bands = Vec::with_capacity(strokes.len().min(4096));

        for stroke in strokes {
            // A stroke off the sheet is not drawn: four comparisons against the box it already has.
            if stroke.points.is_empty() || !stroke.visible_in(visible) {
                continue;
            }

            // A stroke read out of a note arrives without its derived geometry (it is not serialised),
            // so it is frozen here — once, for this detail — and `of_stroke` answers with its pieces,
            // or with the disc a single reading leaves.
            let pieces = if stroke.detailed_at == Some(detail.to_bits()) {
                None
            } else {
                let mut fresh = (**stroke).clone();
                fresh.freeze(detail, note_format::ink::Tip::Curved);
                Some(fresh)
            };

            let band = match &pieces {
                Some(fresh) => Band::of_stroke(fresh, place),
                None => Band::of_stroke(stroke, place),
            };

            if !band.is_empty() {
                bands.push((stroke.color, band));
            }
        }

        PageBands { bands }
    }

    /// The bands, each with the colour it is written in.
    pub fn bands(&self) -> &[(u32, Band)] {
        &self.bands
    }

    /// Whether the page has no ink on screen at all (E1).
    pub fn is_empty(&self) -> bool {
        self.bands.is_empty()
    }

    /// The area the page's ink covers, which is the repair set a frame that redraws the ink needs.
    pub fn damage(&self) -> Footprint {
        let mut damage = Footprint::empty();

        for (_, band) in &self.bands {
            for rect in band.damage().rects() {
                damage.add(*rect);
            }
        }

        damage
    }

    /// The triangles the page draws, which is what the bench's `vertices` figure counts.
    pub fn triangles(&self) -> usize {
        self.bands.iter().map(|(_, band)| band.pieces().len()).sum()
    }
}

/// A page whose ink is the whole sheet: every stroke drawn.
pub fn whole_page() -> Bounds {
    Bounds::new(
        (f32::NEG_INFINITY, f32::NEG_INFINITY),
        (f32::INFINITY, f32::INFINITY),
    )
}

/// The visible box grown by `CULL_SPAN` of itself a side, which is the room a pan has before anything
/// is rebuilt (`doc/VIEW.md` §11; `apps/macos/src/platform/metal.rs`, `CULL_SPAN`).
pub fn cull_of(visible: Bounds) -> Bounds {
    if visible.is_empty() {
        return whole_page();
    }

    const CULL_SPAN: f32 = 0.5;
    let (width, height) = visible.size();

    visible.grown(width * CULL_SPAN, height * CULL_SPAN)
}

/// A `Placement` for a page drawn at `zoom` and `origin`, turned `turns` quarter turns (`paper` is the
/// sheet's own size).
pub fn placement(paper: (f32, f32), turns: Quarters, zoom: f32, origin: (f32, f32)) -> Placement {
    Placement::new(paper, turns, zoom, origin)
}

/// The parts of a band a frame repairs before it draws: the band's own box, grown by the edge slack.
///
/// Here rather than in `band` because it is what a *frame* needs (repair once, then fill), while
/// `Band::damage` is what a *reading* needs (the area it changed).
pub fn repaired(rect: Rect) -> Rect {
    rect.grown(EDGE_SLACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use note_format::ink::InkPoint;

    fn stroke(color: u32, points: &[(f32, f32)]) -> Arc<Stroke> {
        let mut stroke = Stroke::new(InkPoint::new(points[0].0, points[0].1, 4.0), color);
        for (x, y) in &points[1..] {
            stroke.points.push(InkPoint::new(*x, *y, 4.0));
        }
        stroke.freeze(1.0, note_format::ink::Tip::Curved);
        Arc::new(stroke)
    }

    fn place() -> Placement {
        Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0))
    }

    /// A page of ink is a band per stroke, in the order painted, each with its own colour (E6: a
    /// stroke's colour is its own, never the page's).
    #[test]
    fn a_page_is_one_band_a_stroke_with_its_own_colour() {
        let strokes = vec![
            stroke(0x112233, &[(10.0, 10.0), (20.0, 10.0), (30.0, 12.0)]),
            stroke(0x445566, &[(10.0, 40.0), (20.0, 40.0)]),
        ];

        let page = PageBands::of(&strokes, whole_page(), &place(), 1.0);

        assert_eq!(
            page.bands().len(),
            2,
            "a page of two strokes drew not two bands"
        );
        assert_eq!(
            page.bands()[0].0,
            0x112233,
            "the first band lost its colour"
        );
        assert_eq!(
            page.bands()[1].0,
            0x445566,
            "the second band lost its colour"
        );
        assert!(page.triangles() >= 4, "two strokes drew almost nothing");
    }

    /// A stroke wholly off the sheet is not drawn at all, and the page says so by not having it.
    ///
    /// The cull is the reason a zoomed-in page costs what is on screen rather than what the note holds.
    #[test]
    fn a_stroke_off_the_sheet_is_not_drawn() {
        let strokes = vec![
            stroke(0x111111, &[(10.0, 10.0), (20.0, 10.0)]),
            stroke(0x222222, &[(5000.0, 5000.0), (5010.0, 5000.0)]),
        ];

        let visible = Bounds::new((0.0, 0.0), (100.0, 100.0));
        let page = PageBands::of(&strokes, visible, &place(), 1.0);

        assert_eq!(page.bands().len(), 1, "a stroke off the sheet was drawn");
        assert_eq!(
            page.bands()[0].0,
            0x111111,
            "the wrong stroke survived the cull"
        );
    }

    /// A page with nothing on it has no bands, which is the answer that lets a frame draw nothing (E1).
    #[test]
    fn an_empty_page_has_no_bands() {
        let page = PageBands::of(&[], whole_page(), &place(), 1.0);

        assert!(page.is_empty(), "an empty page had a band");
        assert_eq!(page.triangles(), 0, "an empty page drew triangles");
        assert!(page.damage().is_empty(), "an empty page damaged something");
    }

    /// The cull box is the visible one grown by half of itself a side, so a pan has a quarter screen of
    /// slack before anything is rebuilt — `CULL_SPAN`, the Mac canvas's own number.
    #[test]
    fn the_cull_box_has_half_a_screen_of_slack_a_side() {
        let visible = Bounds::new((100.0, 100.0), (500.0, 400.0));
        let cull = cull_of(visible);

        assert_eq!(
            cull,
            Bounds::new(
                (100.0 - 200.0, 100.0 - 150.0),
                (500.0 + 200.0, 400.0 + 150.0)
            ),
            "the cull box was not the visible one grown by half a side"
        );
    }

    /// A page with no visible box — a window not yet laid out — is not culled: every stroke is drawn,
    /// because skipping one that *is* on screen is the one mistake a cull can make.
    #[test]
    fn a_page_with_no_visible_box_draws_everything() {
        let strokes = vec![stroke(0x111111, &[(9000.0, 9000.0), (9010.0, 9000.0)])];
        let page = PageBands::of(&strokes, cull_of(Bounds::NOTHING), &place(), 1.0);

        assert_eq!(page.bands().len(), 1, "an unmeasured window culled its ink");
    }
}
