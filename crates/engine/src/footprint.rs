//! What a reading changed: the band it swept, and the ink it uncovered.
//!
//! Contract: `doc/ENGINE.md` §4.2. A reading that extends an open stroke has exactly three
//! consequences, and this module is the arithmetic of them:
//!
//! 1. the band between the previous disc and this one becomes ink (the trapezoid the two discs' outer
//!    tangents cut);
//! 2. the open stroke's own pieces inside that band must be replayed — once each;
//! 3. nothing else on the layer changes.
//!
//! Everything here is in **window pixels**: the paper's coordinates are mapped through a
//! [`Placement`](crate::place::Placement) first, because the layer's damage is the shape on the screen.

use crate::place::Placement;

/// A rectangle in window pixels: what a damage set is made of, and what a layer scissors to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    /// A rectangle from its corners, normalised so that its width and height are never negative.
    ///
    /// A band swept right-to-left or bottom-to-top arrives with its far corner on the left or above;
    /// a scissor rect and an area both want the smaller corner first, so normalising belongs here
    /// rather than at each of the callers that would otherwise have to remember.
    pub fn from_corners((x0, y0): (f32, f32), (x1, y1): (f32, f32)) -> Self {
        let (left, right) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        let (top, bottom) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };

        Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
    }

    /// The area, in square pixels.
    pub fn area(&self) -> f32 {
        self.width.max(0.0) * self.height.max(0.0)
    }

    /// Whether this rectangle has nothing to draw. A zero-area rect is not a fill (`Rect` on the
    /// canvas says the same, `note_ui::ink_layer::canvas::Rect`).
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// This rectangle grown by `by` on every side: the slack a band needs for the antialiased edge
    /// the stroke's own half-width spills past the tangents.
    pub fn grown(&self, by: f32) -> Self {
        Rect {
            x: self.x - by,
            y: self.y - by,
            width: self.width + 2.0 * by,
            height: self.height + 2.0 * by,
        }
    }

    /// The smallest rectangle covering both, which is how a damage set merges.
    pub fn union(&self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return *self;
        }

        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);

        Rect::from_corners((left, top), (right, bottom))
    }

    /// Whether the two share any area. Touching edges do not count: a band that ends where another
    /// begins did not repaint a pixel twice.
    pub fn overlaps(&self, other: Rect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// The rectangle a paper-space box covers on the screen.
///
/// A turned page draws an axis-aligned box as a box (a quarter turn is still axis-aligned), so four
/// corners mapped and bounded is exact rather than conservative. A placement with any other angle
/// would need eight, which this engine does not have.
/// What a batch of readings changed, merged: the rectangles a layer has to repaint.
///
/// The contract's E1 and E2 (`doc/ENGINE.md` §1) are both about this value. **E1** is the empty
/// footprint — a batch that lays no ink answers `is_empty`, and the presenter has nothing to show.
/// **E2** is the bound: the footprint of one reading is its band, grown by the nib's radius, and never
/// the page.
///
/// The rectangles are merged rather than kept apart, because a layer scissors to a handful of rects and
/// a painter that repainted each band separately would repaint the overlap twice — which is exactly the
/// double-composite `doc/ENGINE.md` §4.1 forbids.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Footprint {
    rects: Vec<Rect>,
}

/// The most rectangles a footprint keeps before it merges the rest into the largest one.
///
/// A scissor is cheap but not free, and a band is a few pixels; past this many, repainting the bounding
/// box is cheaper than repainting each band (`doc/ENGINE.md` §4.2 cites `damage-rects`' own rule: past
/// half the viewport, `mark_full`).
const MAX_RECTS: usize = 8;

impl Footprint {
    /// A footprint with nothing in it: what a batch that laid no ink answers (E1).
    pub fn empty() -> Self {
        Footprint { rects: Vec::new() }
    }

    /// Adds a rectangle, merging it into one it overlaps and levelling the set at [`MAX_RECTS`].
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }

        // One it overlaps: grow that one and stop. A band always overlaps the band before it (they
        // share a disc), so the common case is one union rather than an insert.
        for existing in &mut self.rects {
            if existing.overlaps(rect) {
                *existing = existing.union(rect);
                return;
            }
        }

        if self.rects.len() >= MAX_RECTS {
            let merged = self.rects.drain(..).fold(rect, |acc, each| acc.union(each));
            self.rects.push(merged);
            return;
        }

        self.rects.push(rect);
    }

    /// Whether nothing changed, which is the answer that lets a presenter do nothing at all.
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The rectangles to repaint, in no particular order.
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    /// The area covered, as the sum of the rectangles: the number E2 is bounded by.
    ///
    /// Summed rather than unioned exactly, because the rectangles never overlap (see [`Self::add`]).
    pub fn area(&self) -> f32 {
        self.rects.iter().map(Rect::area).sum()
    }

    /// The smallest rectangle covering all of them: what a layer that draws in one pass scissors to.
    pub fn bounds(&self) -> Rect {
        self.rects
            .iter()
            .copied()
            .fold(Rect::default(), |acc, each| acc.union(each))
    }
}

/// The rectangle a paper-space box covers on the screen.
///
/// A turned page draws an axis-aligned box as a box (a quarter turn is still axis-aligned), so four
/// corners mapped and bounded is exact rather than conservative. A placement with any other angle
/// would need eight, which this engine does not have.
pub fn on_screen(paper: note_format::ink::Bounds, place: &Placement) -> Rect {
    if paper.is_empty() {
        return Rect::default();
    }

    let (min_x, min_y) = paper.min;
    let (max_x, max_y) = paper.max;

    let corners = [
        place.to_window((min_x, min_y)),
        place.to_window((max_x, min_y)),
        place.to_window((min_x, max_y)),
        place.to_window((max_x, max_y)),
    ];

    // Folded over the corners as *numbers* rather than through `Rect::union`, because a corner is a
    // rectangle with no area and `union` rightly treats that as nothing to cover (see `is_empty`).
    let mut left = corners[0].0;
    let mut top = corners[0].1;
    let mut right = left;
    let mut bottom = top;

    for (x, y) in corners {
        left = left.min(x);
        top = top.min(y);
        right = right.max(x);
        bottom = bottom.max(y);
    }

    Rect::from_corners((left, top), (right, bottom))
}

#[cfg(test)]
mod tests {
    use super::*;
    use note_format::ink::Bounds;
    use note_format::pages::Quarters;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// A rectangle built from its far corner first is the same rectangle: a band swept right-to-left
    /// or bottom-to-top arrives that way, and a scissor wants the smaller corner first.
    #[test]
    fn a_rect_is_normalised_from_whatever_corner_it_came_from() {
        let forward = Rect::from_corners((10.0, 20.0), (30.0, 60.0));
        let backward = Rect::from_corners((30.0, 60.0), (10.0, 20.0));

        assert_eq!(
            forward, backward,
            "a reversed rectangle was a different one"
        );
        assert_eq!((forward.x, forward.y), (10.0, 20.0));
        assert_eq!((forward.width, forward.height), (20.0, 40.0));
    }

    /// E1: a batch that lays no ink has no footprint at all, which is what lets a presenter do nothing.
    #[test]
    fn nothing_changed_is_no_footprint() {
        let mut foot = Footprint::empty();

        assert!(foot.is_empty(), "a fresh footprint was not empty");
        assert!(
            foot.area() == 0.0,
            "an empty footprint claimed to cover an area"
        );

        // A zero-area nobody asked about is not a change either: a stroke scrolled off the surface, a
        // window narrower than its own margin.
        foot.add(rect(0.0, 0.0, 0.0, 10.0));
        foot.add(Rect::default());

        assert!(foot.is_empty(), "a zero-area rectangle counted as a change");
    }

    /// A band always overlaps the one before it — they share a disc — so two bands are one rectangle,
    /// and the area is the union's rather than the sum of two that repaint the overlap twice (E6).
    #[test]
    fn touching_bands_merge_into_one_rectangle() {
        let mut foot = Footprint::empty();

        foot.add(rect(0.0, 0.0, 10.0, 10.0));
        foot.add(rect(5.0, 0.0, 10.0, 10.0));

        assert_eq!(
            foot.rects().len(),
            1,
            "two overlapping bands were not merged"
        );
        assert_eq!(
            foot.area(),
            150.0,
            "the merged band's area was not the union's 15x10"
        );
        assert_eq!(foot.bounds(), rect(0.0, 0.0, 15.0, 10.0));
    }

    /// Touching edges are not an overlap: a band that ends where another begins did not repaint a
    /// pixel twice, and merging them would grow the area by nothing but would move the bounds.
    #[test]
    fn edges_that_only_touch_are_not_an_overlap() {
        let left = rect(0.0, 0.0, 10.0, 10.0);
        let right = rect(10.0, 0.0, 10.0, 10.0);

        assert!(!left.overlaps(right), "edge-touching rectangles overlapped");

        let mut foot = Footprint::empty();
        foot.add(left);
        foot.add(right);

        assert_eq!(
            foot.rects().len(),
            2,
            "two bands that only touched were merged"
        );
        assert_eq!(
            foot.area(),
            200.0,
            "two disjoint bands did not add up to their areas"
        );
    }

    /// E2: the footprint of a reading is its band, never the page. A damage set that grew to the page
    /// on each reading is the cost this whole design exists to remove.
    #[test]
    fn a_footprint_stays_bounded_to_the_bands_it_was_given() {
        let page = rect(0.0, 0.0, 1000.0, 1400.0);
        let mut foot = Footprint::empty();

        // Forty short bands scattered down the page, as a line of writing leaves.
        for step in 0..40 {
            foot.add(rect(100.0 + step as f32 * 20.0, 200.0, 20.0, 12.0));
        }

        let bounds = foot.bounds();

        assert!(
            bounds.area() < page.area(),
            "a footprint of bands covered the whole page"
        );
        assert!(
            foot.area() <= 40.0 * 20.0 * 12.0,
            "the merged area exceeded the sum of the bands it was given"
        );
    }

    /// Past a handful of rectangles the set is levelled into one, because a scissor is cheap but not
    /// free and repainting a bounding box is cheaper than repainting eight bands (`doc/ENGINE.md` §4.2).
    #[test]
    fn too_many_rectangles_level_into_one() {
        let mut foot = Footprint::empty();

        for step in 0..(MAX_RECTS * 4) {
            foot.add(rect(step as f32 * 50.0, 0.0, 5.0, 5.0));
        }

        assert!(
            foot.rects().len() <= MAX_RECTS,
            "a footprint kept more than {} rectangles",
            MAX_RECTS
        );
    }

    /// A paper-space box on the screen is the box its corners make: exact for a quarter turn, because
    /// a quarter turn is still axis-aligned.
    #[test]
    fn a_paper_box_maps_to_its_screen_box() {
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 2.0, (10.0, 20.0));
        let paper = Bounds::new((0.0, 0.0), (100.0, 50.0));

        assert_eq!(
            on_screen(paper, &place),
            rect(10.0, 20.0, 200.0, 100.0),
            "a paper box was drawn somewhere other than its corners make"
        );
    }

    /// A box that covers nothing covers nothing on the screen either.
    #[test]
    fn a_box_that_covers_nothing_covers_nothing_on_screen() {
        let place = Placement::new((600.0, 800.0), Quarters::UPRIGHT, 1.0, (0.0, 0.0));

        assert!(
            on_screen(Bounds::NOTHING, &place).is_empty(),
            "a box that covers nothing covered something on screen"
        );
    }
}
