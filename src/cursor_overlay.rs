//! The pen's ghost cursor, in a window of its own.
//!
//! ## Why it left the frame
//!
//! [`crate::cursor`] describes the shape, and it used to be painted into the canvas: the ghost was
//! one more element of the frame the ink is drawn in. That made the cursor as late as the frame is
//! — and a frame is the wrong place for it, because the two do not cost the same thing. The ink is
//! the frame's job: the element tree, the text, the page, the strokes. The cursor is a position,
//! and it is the one thing on screen whose whole meaning is *now*.
//!
//! A Windows pointer moves on its own path — the system draws it, above a window's contents — and
//! no composited window can match that from inside its own swap chain. So the ghost is drawn in a
//! window of its own, above the app window, updated from the pen thread the instant a reading
//! lands, and the frame never hears about it. What is left is the composed path: the reading, the
//! overlay's own present, and the compositor's next tick.
//!
//! ## What it draws, and with what
//!
//! The shapes are [`crate::cursor`]'s: the pen's body with a cast shadow and a soft edge, and the nib
//! mark with its bloom — the mark drawn as wide as the line in hand, so the ghost says how thick the
//! next stroke will be. They are rasterised here — a few hundred pixels of coverage, on the pen
//! thread — and handed to `UpdateLayeredWindow`, which both moves the window and replaces its pixels
//! in one call. No GPU, no swap chain, no frame.
//!
//! ## The window must never take input
//!
//! The overlay sits under the pen, so anything it did with a pointer message would be a stroke
//! that never lands. Three things say "not me": `WS_EX_TRANSPARENT`, `WS_DISABLED`, and answering
//! `WM_NCHITTEST` with `HTTRANSPARENT`. Any one of them is enough; together they mean the pen
//! cannot reach this window even if one of them turns out to be wrong on some future Windows.
//!
//! ## What it is not
//!
//! It is not a second cursor: the system pointer is hidden by [`crate::system_cursor`] exactly
//! while the ghost is on screen, and the same rule decides both — a window with neither would be a
//! window with no cursor at all. And it is not part of the note: nothing about the ghost is
//! persisted, and the next launch starts with the system pointer.
//!
//! ## When it is not there
//!
//! Installing one is optional, and failing to install one is not an error — exactly as with the
//! pointer hook. Without an overlay the app keeps the system pointer and the pen simply has no
//! ghost of its own; [`CursorFeed::is_alive`] is what says so, and it goes quiet if the window is
//! ever lost.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use pen_windows::PenSample;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, PostQuitMessage,
    RegisterClassW, ShowWindow, UnregisterClassW, UpdateLayeredWindow, HTTRANSPARENT,
    MA_NOACTIVATE, MSG, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_APP, WM_DESTROY,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WNDCLASSW, WS_DISABLED, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::cursor::{
    PenCursor, BODY_ALPHA, BODY_HALO_ALPHA, BODY_HALO_GROW, BODY_LENGTH, NIB_ALPHA,
    NIB_BLOOM_ALPHA, NIB_BLOOM_SPREAD, NIB_RADIUS_MAX, NIB_RADIUS_MIN,
};

/// The message the pen thread uses to wake the overlay's own thread.
///
/// A private message rather than an event: that thread is in a message loop for its window anyway,
/// and one posted message per reading collapses on its own — the handler draws whatever is latest
/// and ignores how many wakes announced it, which is the same coalescing the ink queue gets from
/// being a queue.
const WM_PEN_CURSOR: u32 = WM_APP + 1;

/// The window class the overlay registers. Recognisable in a debugger, and unique enough for one.
const CLASS_NAME: PCWSTR = windows::core::w!("cheap-note-pen-cursor");

/// The longest the drawn body can be, in logical pixels.
///
/// [`PenCursor::body_length`] scales the body by the lean, so this is that length's own ceiling —
/// with a little to spare, because `sin` of an angle the digitizer rounds to ninety degrees can
/// land a hair above one.
const BODY_LENGTH_LONGEST: f32 = BODY_LENGTH * 1.02;

/// How far the body's shadow is dropped, in logical pixels: down and to the right, as if the light came
/// from above and to the left.
///
/// A *cast* shadow rather than a rim: the halo thickens the body's own edge, while this is the same shape in the same
/// colour a little way off, which is what makes the rod read as lying *above* the paper rather than being drawn on
/// it — and it is what a reader's eye finds first on a page of writing. [`BODY_SHADOW_REACH`] is this offset's
/// length, and a test holds the two together.
const BODY_SHADOW_DROP: [f32; 2] = [3.0, 3.6];

/// How far [`BODY_SHADOW_DROP`] reaches, as a length: how much further the shadow can be drawn than the body itself.
///
/// Written out rather than derived because a `const` cannot take a square root, and pinned to the offset by a test so
/// the two cannot drift apart: a shadow that reaches further than the window is sized for is a ghost with its edge cut
/// off.
const BODY_SHADOW_REACH: f32 = 4.8;

/// How opaque the body's shadow is.
///
/// Fainter than the body and stronger than the halo, because it is the layer with the job of being *found*: it is the
/// only part of the rod that is not the colour of the rod's own faint self, and on paper the same colour as the body it
/// is the whole of what a reader sees.
const BODY_SHADOW_ALPHA: f32 = 0.26;

/// How far the shape reaches from the nib, in logical pixels: the longest body, its soft edge and its
/// shadow, and the widest nib mark with its bloom. The overlay's window is twice this across, so the
/// ghost fits whichever way the pen leans.
const REACH: f32 =
    BODY_LENGTH_LONGEST + BODY_HALO_GROW + BODY_SHADOW_REACH + NIB_RADIUS_MAX + NIB_BLOOM_SPREAD;

/// How many scanlines are sampled per pixel row when a shape is filled.
///
/// Four is the usual bargain for a shape this size: the error is a sixteenth of a pixel of
/// coverage at worst, and the cost is four crossings per row rather than an analytic area per edge.
const SUB_ROWS: usize = 4;

/// How many sub-samples across and down a disc is tested at.
const DISC_SAMPLES: usize = 4;

/// One cursor reading, ready to draw: where the nib is, how the pen leans, and the things the
/// drawing needs that the reading does not carry.
///
/// `scale` is the window's DPI scale factor, because the pen reports **physical** pixels while the
/// shape's own numbers are logical (see [`crate::cursor`]) — and the surface is a window, so it is
/// physical. `colour` is `0xRRGGBB`, chosen by the app from the paper it is drawn on. `sheet_top`
/// is the line the ghost is drawn below, in physical client pixels: the bar's bottom edge, so a
/// body leaning up ends under the controls exactly as it does today.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorShape {
    /// Where the pen is and how it leans, in logical window pixels.
    pub cursor: PenCursor,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    /// The colour the ghost's *mark* is drawn in, as `0xRRGGBB`: the ink the ghost is a ghost of.
    pub colour: u32,
    /// The colour of the soft edge under the mark, and of the nib's bloom.
    ///
    /// A second colour because the first one is the *ink's*, and ink can be the colour of the paper it is on — a
    /// white pen, a highlighter's pale yellow. The halo is what makes such a mark findable: it is the app's answer
    /// for the paper in front (see [`crate::canvas::contrast_color`]), so a pale dot reads as a pale dot with a dark
    /// rim rather than as nothing at all.
    pub halo: u32,
    /// How wide the ghost's nib mark is, in logical pixels: half the width of the line the tool in hand lays.
    ///
    /// The frame's answer, for the same reason `colour` is one — only the app knows the pen's weight and what is in
    /// hand — and the mark is where the *ink* is previewed, so its size says how thick the ink will be: a heavier pen
    /// has a heavier dot, and a highlighter's mark is as wide as the band it is about to lay
    /// ([`crate::cursor::nib_radius_for_width`]). The drawing holds it between [`NIB_RADIUS_MIN`] and
    /// [`NIB_RADIUS_MAX`] whatever arrives.
    pub nib_radius: f32,
    /// The line the ghost is drawn below, in physical client pixels.
    pub sheet_top: f32,
}

/// The side of the overlay's window for a scale factor, in physical pixels: twice [`REACH`], so the
/// nib can sit at its centre whichever way the body lies.
///
/// Odd, not even, so that the surface's centre is the centre of a pixel: the nib mark is the part
/// that has to be exact. The floor of sixteen is for a scale factor that arrived as nonsense — the
/// window is still created, and the ghost is still drawn, at the size one physical pixel per
/// logical pixel would have given it.
pub fn surface_span(scale: f32) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };

    ((REACH * 2.0 * scale).ceil() as i32).max(16) | 1
}

/// Where the overlay's window goes so that the nib lands on the centre of its surface, in screen
/// pixels.
///
/// The *floor* is deliberate: the window is placed on whole screen pixels and the nib is drawn at
/// the fraction that is left over, so a pen between two pixels is drawn between two pixels instead
/// of being snapped to one. [`nib_offset`] is the other half of this.
pub fn window_origin(nib_screen: [f32; 2], span: i32) -> [i32; 2] {
    let half = span as f32 / 2.0;

    [
        (nib_screen[0] - half).floor() as i32,
        (nib_screen[1] - half).floor() as i32,
    ]
}

/// Where the nib is inside the surface, given where the window was put.
///
/// This is what makes the pair lossless: `origin + offset` is the nib's position on screen, to the
/// fraction, whatever the floor left over.
pub fn nib_offset(nib_screen: [f32; 2], origin: [i32; 2]) -> [f32; 2] {
    [
        nib_screen[0] - origin[0] as f32,
        nib_screen[1] - origin[1] as f32,
    ]
}

/// How many straight segments a quadratic curve is drawn in.
///
/// The body is at most ninety pixels long and a few pixels wide, so twelve segments leave the
/// flank's error well under a tenth of a pixel — below what the coverage sampling can see.
const CURVE_STEPS: usize = 12;

/// A premultiplied `0xAARRGGBB` surface: what `UpdateLayeredWindow` wants, and what the tests fill
/// without a window at all.
///
/// The buffers are kept rather than allocated per reading — a pen reports at up to a few hundred
/// hertz, and a `Vec` per reading would be a malloc and a free in the middle of a stroke — and the
/// drawing is a pure function of the shape it is given, so the whole of it can be tested by
/// looking at pixels.
pub struct Surface {
    /// The surface's size in physical pixels.
    width: usize,
    height: usize,
    /// The pixels, row by row from the top, each premultiplied `0xAARRGGBB`.
    pixels: Vec<u32>,
    /// The coverage of the shape being filled, one entry per pixel, reused between shapes.
    coverage: Vec<f32>,
    /// Where a scanline crossed the shape being filled, reused between scanlines.
    crossings: Vec<f32>,
    /// The mapped outline of the shape being filled, reused between shapes.
    outline: Vec<[f32; 2]>,
}

impl Surface {
    /// A transparent surface of the given size.
    pub fn new(width: usize, height: usize) -> Self {
        let pixels = vec![0; width * height];

        Surface {
            width,
            height,
            coverage: vec![0.0; width * height],
            crossings: Vec::with_capacity(8),
            pixels,
            outline: Vec::with_capacity(3 * CURVE_STEPS + 1),
        }
    }

    /// The pixels, premultiplied `0xAARRGGBB`, row by row.
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// How wide the surface is, in pixels.
    ///
    /// Only the tests ask — the drawing reads the fields, which are a few lines above it — so this
    /// exists to make a surface's geometry readable from outside the module's own code. There is no
    /// `height` beside it because a surface is square: it is the ghost's window, and the ghost can
    /// lean in any direction.
    #[cfg(test)]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Empties the surface: every pixel transparent, so that a reading draws only what it draws.
    pub fn clear(&mut self) {
        self.pixels.fill(0);
    }

    /// Draws the whole ghost: the body, the soft edge under it, and the nib mark with its bloom.
    ///
    /// `nib` is where the pen is **on this surface**, in physical pixels, and `clip` is the line
    /// above which nothing is drawn — the bar's bottom edge, in the same coordinates, so that a
    /// body leaning up ends at the controls rather than over them.
    ///
    /// The shapes come from [`PenCursor`] in logical window pixels, which is why both are mapped
    /// here rather than by the caller: the reading is the input and the pixels are the output.
    pub fn draw(&mut self, shape: &CursorShape, nib: [f32; 2], clip: Option<f32>) {
        let scale = sane_scale(shape.scale);
        let colour = rgb_of(shape.colour);
        let halo = rgb_of(shape.halo);
        let origin = shape.cursor.position();
        let map = |p: [f32; 2]| {
            [
                nib[0] + (p[0] - origin[0]) * scale,
                nib[1] + (p[1] - origin[1]) * scale,
            ]
        };

        // The body is absent for a pen with no tilt sensor, and for one held straight up: it fades
        // in as the lean grows, so the threshold is not a shape appearing out of nothing.
        if let Some(body) = shape.cursor.body_shape() {
            let fade = shape.cursor.body_fade();

            // Three passes over one shape, widest and faintest first — the order the page's own shadow
            // is painted in, and the whole of what makes the rod findable. The shadow is the same body
            // dropped a little down and to the right in the *halo's* colour, which is the one colour
            // this app knows can be seen on the paper in front; the edge is that colour too, so the two
            // read as one soft shadow around the body rather than as an outline with a line beside it.
            for (grow, shift, alpha, tint) in [
                (0.0, BODY_SHADOW_DROP, BODY_SHADOW_ALPHA * fade, halo),
                (BODY_HALO_GROW, [0.0, 0.0], BODY_HALO_ALPHA * fade, halo),
                (0.0, [0.0, 0.0], BODY_ALPHA * fade, colour),
            ] {
                let shifted = |p: [f32; 2]| map([p[0] + shift[0], p[1] + shift[1]]);
                flatten(&body.outline(grow), &shifted, &mut self.outline);
                let outline = std::mem::take(&mut self.outline);
                self.fill_polygon(&outline, clip, alpha, tint);
                self.outline = outline;
            }
        }

        // The nib: a disc as wide as the line in hand, so there is a fixed point that says exactly where
        // the ink will land *and how thick it will be*. The faint bloom around it is what lets it sit in
        // the page rather than on it, and the mark is the exception to how faint the rest of it is — and
        // it is drawn *in the ink's colour*, over a bloom in the halo's, so the dot says both what will
        // be written and where.
        let mark = sane_radius(shape.nib_radius);
        for (radius, alpha, tint) in [
            (mark + NIB_BLOOM_SPREAD, NIB_BLOOM_ALPHA, halo),
            (mark, NIB_ALPHA, colour),
        ] {
            self.fill_disc(nib, radius * scale, clip, alpha, tint);
        }
    }

    /// Fills a closed convex outline, antialiased, in one colour.
    ///
    /// The rules are the ones an outline wants and no more: a scanline crosses a convex shape twice,
    /// so the crossings are sorted and paired, and the cover of a pixel is how much of its width
    /// lies between a pair. Each row is sampled [`SUB_ROWS`] times down its height, which is what
    /// softens the flanks and the cap. The coverage is accumulated per row and composited once, so
    /// the sub-rows of one pixel add up instead of painting the pixel twice.
    fn fill_polygon(
        &mut self,
        points: &[[f32; 2]],
        clip: Option<f32>,
        alpha: f32,
        colour: [u8; 3],
    ) {
        if points.len() < 3 || alpha <= 0.0 {
            return;
        }

        let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
        for point in points {
            min_y = min_y.min(point[1]);
            max_y = max_y.max(point[1]);
        }

        let floor = clip.unwrap_or(f32::NEG_INFINITY);
        let Some((top, bottom)) = row_bounds(min_y.floor().max(floor), max_y.ceil(), self.height)
        else {
            return;
        };

        for row in top..=bottom {
            let mut touched: Option<(usize, usize)> = None;

            for sub in 0..SUB_ROWS {
                let y = row as f32 + (sub as f32 + 0.5) / SUB_ROWS as f32;
                if y < floor {
                    continue;
                }

                self.crossings.clear();
                for index in 0..points.len() {
                    let from = points[index];
                    let to = points[(index + 1) % points.len()];

                    // The half-open rule: an edge counts on the row it starts, not the one it ends
                    // on, so a vertex is crossed once rather than twice or not at all.
                    if (from[1] <= y) != (to[1] <= y) {
                        let along = (y - from[1]) / (to[1] - from[1]);
                        self.crossings.push(from[0] + (to[0] - from[0]) * along);
                    }
                }

                self.crossings
                    // `total_cmp` rather than a comparison that answers `None` for a NaN: a crossing
                    // that came out of arithmetic that produced one is still an ordering, and the
                    // filler is better off with a deterministic one than with a panic.
                    .sort_by(f32::total_cmp);

                // The pairs are copied out before the spans are added, because adding a span needs
                // the surface mutably and the crossings are part of it.
                let count = self.crossings.len();
                for index in (0..count - count % 2).step_by(2) {
                    let (first, second) = (self.crossings[index], self.crossings[index + 1]);
                    let span = self.cover(row, first.min(second), first.max(second));
                    touched = Some(match touched {
                        Some((from, to)) => (from.min(span.0), to.max(span.1)),
                        None => span,
                    });
                }
            }

            if let Some((from, to)) = touched {
                self.composite(row, from, to, alpha, colour);
            }
        }
    }

    /// Adds one span of a scanline to the coverage of a row, and answers the pixels it touched.
    fn cover(&mut self, row: usize, from: f32, to: f32) -> (usize, usize) {
        let first = from.floor().max(0.0) as usize;
        let last = to.ceil().min(self.width as f32).max(0.0) as usize;
        let share = 1.0 / SUB_ROWS as f32;

        for column in first..last.min(self.width) {
            let left = column as f32;
            let overlap = (to.min(left + 1.0) - from.max(left)).clamp(0.0, 1.0);
            if overlap > 0.0 {
                self.coverage[row * self.width + column] += overlap * share;
            }
        }

        (first, last)
    }

    /// Paints a row's coverage into its pixels, and takes the coverage back out again.
    ///
    /// The coverage buffer is left as it was found — every entry it used is zeroed here — so a shape
    /// starts from nothing without the whole surface being cleared between shapes.
    fn composite(&mut self, row: usize, from: usize, to: usize, alpha: f32, colour: [u8; 3]) {
        for column in from..to.min(self.width) {
            let index = row * self.width + column;
            let coverage = self.coverage[index];

            if coverage > 0.0 {
                self.pixels[index] = over(self.pixels[index], colour, coverage, alpha);
                self.coverage[index] = 0.0;
            }
        }
    }

    /// Fills a disc, antialiased by sampling it [`DISC_SAMPLES`] times across and down.
    ///
    /// A disc could be covered analytically, and this would be the wrong shape to do it for: the nib
    /// mark is a handful of pixels across, so sixteen samples per pixel is under two hundred tests
    /// and no more arithmetic than the analytic answer needs.
    fn fill_disc(
        &mut self,
        centre: [f32; 2],
        radius: f32,
        clip: Option<f32>,
        alpha: f32,
        colour: [u8; 3],
    ) {
        if alpha <= 0.0 || !radius.is_finite() || radius <= 0.0 {
            return;
        }

        let floor = clip.unwrap_or(f32::NEG_INFINITY);
        let Some((top, bottom)) = row_bounds(
            (centre[1] - radius).max(floor),
            centre[1] + radius,
            self.height,
        ) else {
            return;
        };

        let left = (centre[0] - radius).floor().max(0.0) as usize;
        let right = (centre[0] + radius).ceil().min(self.width as f32).max(0.0) as usize;
        let samples = (DISC_SAMPLES * DISC_SAMPLES) as f32;
        let square = radius * radius;

        for row in top..=bottom {
            for column in left..right.min(self.width) {
                let mut inside = 0u32;

                for down in 0..DISC_SAMPLES {
                    for across in 0..DISC_SAMPLES {
                        let point = [
                            column as f32 + (across as f32 + 0.5) / DISC_SAMPLES as f32,
                            row as f32 + (down as f32 + 0.5) / DISC_SAMPLES as f32,
                        ];

                        // The clip is a line, so a row it cuts through is covered less rather than
                        // dropped: the ghost ends under the controls instead of ending at a seam.
                        if point[1] < floor {
                            continue;
                        }

                        let (dx, dy) = (point[0] - centre[0], point[1] - centre[1]);
                        if dx * dx + dy * dy <= square {
                            inside += 1;
                        }
                    }
                }

                let coverage = inside as f32 / samples;
                if coverage > 0.0 {
                    let index = row * self.width + column;
                    self.pixels[index] = over(self.pixels[index], colour, coverage, alpha);
                }
            }
        }
    }
}

/// The rows of a surface a shape can touch, or `None` when it touches none of them.
///
/// Clamping here rather than per scanline is what keeps a shape that is off the surface — a cursor
/// dragged off the edge of the window — from costing anything at all.
fn row_bounds(top: f32, bottom: f32, height: usize) -> Option<(usize, usize)> {
    if height == 0 || !top.is_finite() || !bottom.is_finite() {
        return None;
    }

    let highest = height as f32 - 1.0;
    let first = top.max(0.0).min(highest);
    let last = bottom.max(0.0).min(highest);

    (bottom > 0.0 && top <= highest && last >= first).then_some((first as usize, last as usize))
}

/// Turns a body's seven outline points into the closed polygon the filler wants, mapping each point
/// to the surface on the way.
///
/// The order of the points is [`crate::cursor::Body::outline`]'s contract — nib-left, flank
/// control, far-left, cap control, far-right, flank control, nib-right — which is three quadratics
/// and the closing line back to the nib. It is flattened here rather than in the filler because a
/// curve is a shape's business and a scanline's crossings are the filler's.
fn flatten(
    outline: &[[f32; 2]; 7],
    map: &impl Fn([f32; 2]) -> [f32; 2],
    points: &mut Vec<[f32; 2]>,
) {
    let curve = |points: &mut Vec<[f32; 2]>, from: [f32; 2], control: [f32; 2], to: [f32; 2]| {
        for step in 1..=CURVE_STEPS {
            let along = step as f32 / CURVE_STEPS as f32;
            let left = 1.0 - along;

            points.push(map([
                left * left * from[0] + 2.0 * left * along * control[0] + along * along * to[0],
                left * left * from[1] + 2.0 * left * along * control[1] + along * along * to[1],
            ]));
        }
    };

    points.clear();
    points.push(map(outline[0]));
    curve(points, outline[0], outline[1], outline[2]);
    curve(points, outline[2], outline[3], outline[4]);
    curve(points, outline[4], outline[5], outline[6]);
}

/// Composites a colour over a premultiplied pixel, and answers the result.
///
/// `coverage` is how much of the pixel the shape reaches and `alpha` how opaque the shape is at all,
/// so the two multiply: a half-covered, half-opaque pixel is a quarter of the colour. The
/// arithmetic is the one `UpdateLayeredWindow` wants — the destination is already premultiplied, and
/// so is what this returns — which is why the channels are scaled by the alpha rather than by 255.
fn over(destination: u32, colour: [u8; 3], coverage: f32, alpha: f32) -> u32 {
    let opacity = (coverage.clamp(0.0, 1.0) * alpha.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return destination;
    }

    let under = 1.0 - opacity;
    let channel = |value: f32| value.round().clamp(0.0, 255.0) as u32;
    let existing = |shift: u32| (((destination >> shift) & 0xFF) as f32) * under;

    (channel(opacity * 255.0) << 24)
        | (channel(colour[0] as f32 * opacity + existing(16)) << 16)
        | (channel(colour[1] as f32 * opacity + existing(8)) << 8)
        | channel(colour[2] as f32 * opacity + existing(0))
}

/// A `0xRRGGBB` colour as its three channels.
fn rgb_of(colour: u32) -> [u8; 3] {
    [
        ((colour >> 16) & 0xFF) as u8,
        ((colour >> 8) & 0xFF) as u8,
        (colour & 0xFF) as u8,
    ]
}

/// A scale factor that can be multiplied by, and by 1.0 when the window gave one that cannot.
fn sane_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// A nib mark's radius that can be draw with, held between the two the shape declares.
///
/// [`crate::cursor::nib_radius_for_width`] already clamps what the app sends, so this is the *drawing's* own
/// defence rather than a second opinion: a surface is a public thing to hand a shape to, and a radius that is
/// not finite would draw nothing at all — a cursor with no dot in it, which is the one part of it that has to
/// be exact.
fn sane_radius(radius: f32) -> f32 {
    if radius.is_finite() {
        radius.clamp(NIB_RADIUS_MIN, NIB_RADIUS_MAX)
    } else {
        NIB_RADIUS_MIN
    }
}

/// What the frame publishes about the screen the ghost is drawn on: everything about it that a
/// reading cannot say.
///
/// These five numbers are the whole of what the app knows and the overlay does not. `suppressed`
/// means "there is nothing to draw a ghost for here" — the home list is up, or the Tilt switch is
/// off — and it is published rather than inferred because only the frame knows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    /// Physical pixels per logical pixel, as the frame laid the window out.
    pub scale: f32,
    /// The colour the ghost's mark is drawn in, as `0xRRGGBB`.
    pub colour: u32,
    /// The colour of the soft edge under it, and of the nib's bloom.
    ///
    /// The frame's own answer for the paper in front: the ghost's mark is the *ink's* colour, which can be the
    /// colour of the paper it is drawn on, and this is what keeps a white pen or a highlighter's pale yellow
    /// findable.
    pub halo: u32,
    /// How wide the ghost's nib mark is, in logical pixels: half the width of the line the tool in hand lays.
    ///
    /// Published for the same reason the colour is — the app is the only thing that knows the pen's weight — and it is
    /// what makes the mark a preview of the ink rather than a point. See [`CursorShape::nib_radius`].
    pub nib_radius: f32,
    /// The line the ghost is drawn below, in physical client pixels: the bar's bottom edge.
    pub sheet_top: f32,
    /// Whether the ghost is switched off, or the pen is not on the sheet at all.
    pub suppressed: bool,
}

impl Default for Screen {
    /// The state before the first frame has said anything: no ghost, which is the honest answer for
    /// a screen nobody has described yet. The mark is the smallest one, because nothing has said what
    /// is in hand — and it is not drawn at all while `suppressed` is set, which is what this is.
    fn default() -> Self {
        Screen {
            scale: 1.0,
            colour: 0x00_00_00,
            halo: 0x00_00_00,
            nib_radius: NIB_RADIUS_MIN,
            sheet_top: 0.0,
            suppressed: true,
        }
    }
}

/// The newest reading, and what the frame last said about the screen.
#[derive(Clone, Copy, Debug)]
struct Pending {
    /// The newest reading the pen thread offered, if any.
    sample: Option<PenSample>,
    /// What the frame published.
    screen: Screen,
}

/// Draws the ghost for one reading, or answers `None` when there is none to draw.
///
/// The whole decision, in one place: the pen has to be on the sheet — below the bar's edge, and on
/// a screen that wants a ghost — and the reading has to be one a cursor exists for at all, which is
/// the same phase rule [`PenCursor::from_sample`] applies to ink and to the system pointer.
///
/// It answers the reading as well as the shape because the drawing still needs the pen's own
/// position: the shape is in logical pixels, and the surface it is drawn on is in physical ones.
fn cursor_for(sample: PenSample, screen: &Screen) -> Option<(PenSample, CursorShape)> {
    if screen.suppressed || sample.pixel.y <= screen.sheet_top {
        return None;
    }

    let cursor = PenCursor::from_sample(sample, screen.scale)?;
    let shape = CursorShape {
        cursor,
        scale: screen.scale,
        colour: screen.colour,
        halo: screen.halo,
        nib_radius: screen.nib_radius,
        sheet_top: screen.sheet_top,
    };

    Some((sample, shape))
}

/// The state the overlay's three threads share.
///
/// One lock rather than a handful of atomics: the pen thread takes it once per batch and the frame
/// once per paint, both for the few instructions it takes to copy a value out. The drawing never
/// holds it, either — a reader waiting on a lock while pixels were rasterised would be a stutter in
/// the very thing this module exists to make smooth.
struct Shared {
    /// What the pen thread last saw, and what the frame last said.
    pending: Mutex<Pending>,
    /// The overlay's window, once its thread has one; zero until then, and zero again when it goes.
    window: AtomicIsize,
    /// Whether the overlay's thread is still there to draw.
    alive: AtomicBool,
    /// The app window, whose client area pen coordinates are relative to.
    owner: isize,
}

impl Shared {
    /// The state, with a poisoned lock read through rather than refused: the payload is a reading
    /// and four numbers, so a panic elsewhere cannot leave it inconsistent.
    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Asks the overlay's thread to draw whatever is latest.
    ///
    /// One message per reading, and no queue behind them: a handler that draws the newest state
    /// makes a wake announcing a reading it has already drawn cost nothing, which is what lets the
    /// pen report faster than the overlay draws without anything backing up.
    fn wake(&self) {
        let window = self.window.load(Ordering::Relaxed);
        if window == 0 {
            return;
        }

        let posted = unsafe {
            PostMessageW(
                Some(HWND(window as *mut core::ffi::c_void)),
                WM_PEN_CURSOR,
                WPARAM(0),
                LPARAM(0),
            )
        };

        // A window that will not take a message is a window that is gone, and the app has to be told
        // so that it can give the system pointer back.
        if posted.is_err() {
            self.alive.store(false, Ordering::Relaxed);
            self.window.store(0, Ordering::Relaxed);
        }
    }
}

/// The application's handle on the ghost cursor's window: what the pen thread offers readings to,
/// and what the frame tells about the screen.
///
/// `Clone` because more than one thread holds it and none owns it — the pen thread offers, the frame
/// publishes, and the overlay's own thread draws.
#[derive(Clone)]
pub struct CursorFeed {
    shared: Arc<Shared>,
}

impl CursorFeed {
    /// Installs an overlay owned by a window, or answers `None` if it cannot be installed.
    ///
    /// The window is created on a thread of its own, because that is what owning a window means: it
    /// has a message loop, it draws when it is told to, and nothing about it touches GPUI's thread.
    /// A handle that is not a Win32 one, or a class the system refuses, both mean "no overlay": the
    /// app keeps the system pointer and the pen has no ghost of its own, which is the state the app
    /// was in before any of this existed.
    pub fn install<W: HasWindowHandle>(window: &W) -> Option<Self> {
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return None;
        };

        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                sample: None,
                screen: Screen::default(),
            }),
            window: AtomicIsize::new(0),
            alive: AtomicBool::new(false),
            owner: handle.hwnd.get(),
        });

        let thread = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(String::from("cheap-note-cursor"))
            .spawn(move || run(thread))
            .ok()?;

        Some(CursorFeed { shared })
    }

    /// Whether the ghost is on screen: the overlay has a window, and a thread still drawing into it.
    ///
    /// This is what the app asks before it hides the system pointer, so a lost overlay gives the
    /// pointer back rather than leaving a window with no cursor at all.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// Publishes what the frame knows about the screen the ghost is drawn on.
    ///
    /// Called from the frame, and only when something in here has changed: a scale factor that
    /// moved, the paper's colour, where the bar ends, and whether this screen wants a ghost at all.
    /// Publishing is what makes a switch take effect at once — a suppressed ghost is hidden here
    /// rather than left on screen until the pen next moves.
    pub fn set_screen(&self, screen: Screen) {
        let hidden = screen.suppressed;

        {
            let mut pending = self.shared.pending();
            if pending.screen == screen {
                return;
            }

            pending.screen = screen;
            if hidden {
                pending.sample = None;
            }
        }

        self.shared.wake();
    }

    /// Offers the newest reading to the overlay. Called by the pen thread.
    ///
    /// Nothing is decided here: the reading is kept, and the overlay's own thread decides whether
    /// there is a ghost in it — that thread is the one with the scale factor and the sheet's edge to
    /// hand. A batch is taken by its newest reading, because the ones behind it describe a position
    /// the pen has already left.
    pub fn offer(&self, samples: &[PenSample]) {
        let Some(sample) = samples.last() else {
            return;
        };

        // The reading is kept whether or not there is a window to draw it in yet — a wake with no
        // window does nothing — because a pen that reported once and then held still has to be drawn
        // when the window arrives.
        self.shared.pending().sample = Some(*sample);
        self.shared.wake();
    }
}

/// Everything the overlay's thread owns: the window, and the pixels it is made of.
struct Overlay {
    /// The state, for the reading to draw and the window to post to.
    shared: Arc<Shared>,
    /// The window this thread created and owns.
    window: HWND,
    /// A memory device context, holding whichever bitmap is current.
    device: HDC,
    /// The bitmap `UpdateLayeredWindow` is handed: 32 bits per pixel, top-down, premultiplied.
    bitmap: HBITMAP,
    /// The bitmap the device context came with, to be put back before the context is freed.
    stock: HGDIOBJ,
    /// Where the bitmap's bits are, or null before the first one is made.
    bits: *mut u32,
    /// The side the current bitmap was made for, in pixels; zero before the first one is made.
    span: i32,
    /// The pixels being painted, kept between readings.
    surface: Surface,
    /// Whether the window is on screen, so that hiding it is a transition rather than a habit.
    shown: bool,
}

impl Overlay {
    /// Makes the pixels the size the surface should be, if they are not already.
    ///
    /// Only the scale factor moves this, so it happens when a window is dragged to a monitor with a
    /// different DPI — not once per reading.
    fn resize(&mut self, span: i32) -> anyhow::Result<()> {
        if span == self.span {
            return Ok(());
        }

        let header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: span,
                // Negative: rows from the top down, which is the order the drawing is in and the
                // order `UpdateLayeredWindow` is handed.
                biHeight: -span,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(
                Some(self.device),
                &header,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            )?
        };
        let previous = unsafe { SelectObject(self.device, HGDIOBJ(bitmap.0)) };

        // The first bitmap selected is the one the device context came with, and it is not ours to
        // free: it is kept until the end, when it goes back where it was. Every one after it is a
        // surface this thread made and is done with.
        if self.span == 0 {
            self.stock = previous;
        } else {
            let _ = unsafe { DeleteObject(previous) };
        }

        self.bitmap = bitmap;
        self.bits = bits as *mut u32;
        self.span = span;
        self.surface = Surface::new(span as usize, span as usize);

        Ok(())
    }

    /// Draws whatever is latest, or hides the window when the latest reading is not one to draw.
    ///
    /// Everything the drawing needs is taken from the state in one lock and then let go: rasterising
    /// a cursor while holding the lock the pen thread wants would make the pen wait for a picture.
    fn paint(&mut self) -> anyhow::Result<()> {
        let (sample, screen) = {
            let pending = self.shared.pending();
            (pending.sample, pending.screen)
        };

        let Some((sample, shape)) = sample.and_then(|sample| cursor_for(sample, &screen)) else {
            return self.hide();
        };

        // Where the pen is *on screen*: the reading is in client pixels, and the overlay is placed in
        // screen ones. The client area's own corner is what is converted — a whole pixel — and the
        // reading's fraction is added on top, so a pen between two pixels is drawn between two.
        let mut corner = POINT { x: 0, y: 0 };
        let owner = HWND(self.shared.owner as *mut core::ffi::c_void);
        if !unsafe { ClientToScreen(owner, &mut corner) }.as_bool() {
            // The app window has gone. The next reading will find the same, and the app is already
            // closing, so there is nothing to report and nothing to draw.
            return Ok(());
        }

        let nib_screen = [
            corner.x as f32 + sample.pixel.x,
            corner.y as f32 + sample.pixel.y,
        ];

        let span = surface_span(shape.scale);
        self.resize(span)?;

        let origin = window_origin(nib_screen, span);
        let nib = nib_offset(nib_screen, origin);

        // The clip is the bar's bottom edge in this surface's coordinates: the same line, measured
        // from the nib, which is where the drawing starts from.
        let clip = nib[1] + (shape.sheet_top - sample.pixel.y);

        self.surface.clear();
        self.surface.draw(&shape, nib, Some(clip));

        // The surface's pixels *are* a 32-bit DIB's pixels, byte for byte: `0xAARRGGBB` in a `u32` is
        // B, G, R, A in memory, which is the order a `BI_RGB` bitmap at 32 bits is stored in.
        if self.bits.is_null() {
            return Ok(());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.surface.pixels().as_ptr(),
                self.bits,
                self.surface.pixels().len(),
            );
        }

        let destination = POINT {
            x: origin[0],
            y: origin[1],
        };
        let size = SIZE { cx: span, cy: span };
        let source = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        // One call moves the window and replaces its pixels: two would be a frame with the ghost in
        // the old place, drawn at the new one.
        unsafe {
            UpdateLayeredWindow(
                self.window,
                None,
                Some(&destination),
                Some(&size),
                Some(self.device),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?
        };

        // Shown without being activated: the app keeps the keyboard, and the pen keeps writing.
        if !self.shown {
            let _ = unsafe { ShowWindow(self.window, SW_SHOWNOACTIVATE) };
            self.shown = true;
        }

        Ok(())
    }

    /// Takes the ghost off the screen, if it was on it.
    fn hide(&mut self) -> anyhow::Result<()> {
        if self.shown {
            let _ = unsafe { ShowWindow(self.window, SW_HIDE) };
            self.shown = false;
        }

        Ok(())
    }

    /// Gives back what the thread borrowed: the pixels, the device context, and the window.
    ///
    /// The stock bitmap goes back into the context before the context is freed, and the surface is
    /// deleted after it: a bitmap still selected into a live context cannot be destroyed, and a
    /// resource this small freed this rarely is not worth the ambiguity of deleting it anyway.
    fn retire(&mut self) {
        let _ = self.hide();

        if self.span != 0 {
            let _ = unsafe { SelectObject(self.device, self.stock) };
            let _ = unsafe { DeleteObject(HGDIOBJ(self.bitmap.0)) };
        }

        let _ = unsafe { DeleteDC(self.device) };
    }
}

/// The overlay's own thread: a window, a message loop, and the drawing.
///
/// Nothing is reported when the window cannot be made: the app asks [`CursorFeed::is_alive`] at its
/// next frame and gives the system pointer back, which is a visible answer where a log line is not.
fn run(shared: Arc<Shared>) {
    let _ = serve(&shared);

    // Whatever happened, the overlay is not drawing into a window any more, and the app has to be
    // able to find that out.
    shared.alive.store(false, Ordering::Relaxed);
    shared.window.store(0, Ordering::Relaxed);
}

/// Makes the window and runs its message loop until it is destroyed.
fn serve(shared: &Arc<Shared>) -> anyhow::Result<()> {
    let instance = unsafe { GetModuleHandleW(None)? };

    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };

    if unsafe { RegisterClassW(&class) } == 0 {
        anyhow::bail!("the pen cursor's window class was refused");
    }

    // Created hidden and at the size a first draw would want: `UpdateLayeredWindow` moves and resizes
    // it per reading, and nothing is shown until there is a ghost to show.
    let span = surface_span(1.0);
    let window = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            windows::core::w!("cheap-note pen cursor"),
            WS_POPUP | WS_DISABLED,
            0,
            0,
            span,
            span,
            // Owned by the app window, so that it stays above it, follows it through minimize, and
            // dies with it — no topmost window of its own to leave behind.
            Some(HWND(shared.owner as *mut core::ffi::c_void)),
            None,
            Some(instance.into()),
            None,
        )?
    };

    let device = unsafe { CreateCompatibleDC(None) };
    let mut overlay = Overlay {
        shared: Arc::clone(shared),
        window,
        device,
        bitmap: HBITMAP(std::ptr::null_mut()),
        stock: HGDIOBJ(std::ptr::null_mut()),
        bits: std::ptr::null_mut(),
        span: 0,
        surface: Surface::new(1, 1),
        shown: false,
    };

    // Published only now: a reading offered before this is one the app knows has nowhere to go.
    shared.window.store(window.0 as isize, Ordering::Relaxed);
    shared.alive.store(true, Ordering::Relaxed);

    // And a first draw, in case a reading arrived while the window was being made: the pen thread
    // offers them whether or not there is anywhere to put them, and one that landed in that gap
    // would otherwise wait for the pen to move again.
    shared.wake();

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        if message.message == WM_PEN_CURSOR {
            // A failed draw ends the thread: the window is gone, or the device is lost, and the app
            // will notice at its next frame and give the pointer back.
            if overlay.paint().is_err() {
                break;
            }
        } else {
            unsafe { DispatchMessageW(&message) };
        }
    }

    overlay.retire();
    let _ = unsafe { UnregisterClassW(CLASS_NAME, Some(instance.into())) };

    Ok(())
}

/// The overlay's window procedure: the answers that keep the pen away from it, and a clean end.
///
/// # Safety
///
/// Called by Windows on the thread that owns the window. Nothing is read from the window's user data
/// — there is none — so there is no state here that a message can find stale.
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Under the pen, and never the window it is talking to. Three styles already say so — the
        // layered one, `WS_EX_TRANSPARENT`, and `WS_DISABLED` — and this is the fourth: answering
        // `HTTRANSPARENT` makes the system hand the message to the window underneath, which is the
        // app's, where the stroke belongs.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        // Nor by activating: a click that made the overlay the active window would take the pen
        // away from the note.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pen_windows::{PenPhase, Point, Tilt};

    /// A reading at a position, with a phase and a lean.
    fn reading(phase: PenPhase, pixel: (f32, f32), tilt: Option<(f32, f32)>) -> PenSample {
        PenSample {
            phase,
            id: 1,
            pixel: Point::new(pixel.0, pixel.1),
            tilt: tilt.map(|(x, y)| Tilt::new(x, y)),
            ..PenSample::default()
        }
    }

    /// A screen that wants a ghost, with nothing in the way.
    ///
    /// The mark is the pen this app ships with — [`crate::settings::Settings::default`]'s widest press, halved —
    /// because the *size* is the app's answer and these tests are about what the drawing does with one.
    fn screen() -> Screen {
        Screen {
            scale: 1.0,
            colour: 0x00_00_00,
            halo: 0x00_00_00,
            nib_radius: crate::cursor::nib_radius_for_width(4.5),
            sheet_top: 0.0,
            suppressed: false,
        }
    }

    /// The shape a reading asks for.
    fn shape(pixel: (f32, f32), tilt: Option<(f32, f32)>) -> CursorShape {
        cursor_for(reading(PenPhase::Hover, pixel, tilt), &screen())
            .expect("a pen on the sheet has a ghost")
            .1
    }

    /// The nib's mark is the ink's colour and its bloom is the halo's: the dot says what will be written, and its
    /// edge is what makes a mark the colour of the paper findable (see [`Screen::halo`]).
    #[test]
    fn the_mark_is_the_ink_and_the_bloom_is_the_halo() {
        let span = surface_span(1.0) as usize;
        let middle = span as f32 / 2.0;

        let mut surface = Surface::new(span, span);
        surface.clear();

        let shape = CursorShape {
            colour: 0xFF_00_00,
            halo: 0x00_00_FF,
            ..shape((middle, middle), None)
        };

        surface.draw(&shape, [middle, middle], None);

        let at =
            |radius: f32| surface.pixels()[(middle as usize) * span + (middle + radius) as usize];
        let centre = at(0.0);
        let ring = at(shape.nib_radius + NIB_BLOOM_SPREAD * 0.5);

        let (centre_red, centre_blue) = ((centre >> 16) & 0xFF, centre & 0xFF);
        let (ring_red, ring_blue) = ((ring >> 16) & 0xFF, ring & 0xFF);

        assert!(
            centre_red > centre_blue,
            "the mark in the middle is the colour in hand: {centre:#010X}"
        );
        assert!(
            ring_blue > ring_red,
            "and the bloom around it is the halo's: {ring:#010X}"
        );
    }

    /// The mark is drawn as wide as the app says the line in hand is: a heavier pen's dot is visibly heavier, which is
    /// the only way the weight can be seen without putting ink on the page.
    ///
    /// The bloom is a fixed *spread* beyond the mark rather than a multiple of it, and that is the second half of this:
    /// a wide mark gets the same thickness of edge as a narrow one, so the edge stays an edge when the mark is as wide
    /// as a highlighter's band.
    #[test]
    fn the_mark_is_as_wide_as_the_line_in_hand() {
        let span = surface_span(1.0) as usize;
        let middle = span as f32 / 2.0;

        // The mark's colour leads in red and the bloom's in blue, so a pixel belongs to the mark when red leads — which
        // is how the two are told apart on one surface.
        let extents = |radius: f32| {
            let shape = CursorShape {
                colour: 0xFF_00_00,
                halo: 0x00_00_FF,
                nib_radius: radius,
                ..shape((middle, middle), None)
            };

            let mut surface = Surface::new(span, span);
            surface.draw(&shape, [middle, middle], None);

            let mut mark = 0.0f32;
            let mut whole = 0.0f32;

            for (index, pixel) in surface.pixels().iter().enumerate() {
                if *pixel == 0 {
                    continue;
                }

                let reach = ((index % span) as f32 - middle).abs();
                whole = whole.max(reach);

                if (pixel >> 16) & 0xFF > pixel & 0xFF {
                    mark = mark.max(reach);
                }
            }

            (mark, whole)
        };

        let (narrow, narrow_whole) = extents(2.25);
        let (wide, wide_whole) = extents(NIB_RADIUS_MAX);

        assert!(
            (narrow - 2.25).abs() <= 1.5,
            "the mark is drawn at the radius it was given: {narrow}"
        );
        assert!(
            wide > narrow + 4.0,
            "and a wider line is a wider dot: {wide} against {narrow}"
        );
        assert!(
            (narrow_whole - narrow - NIB_BLOOM_SPREAD).abs() <= 1.5,
            "the bloom is a spread past the mark, not a multiple of it: {}",
            narrow_whole - narrow
        );
        assert!(
            (wide_whole - wide - NIB_BLOOM_SPREAD).abs() <= 1.5,
            "and the same spread on a mark as wide as a highlighter's band: {}",
            wide_whole - wide
        );
    }

    /// The body casts a shadow: the same rod dropped down and to the right, which is what makes a faint body findable
    /// on paper nearly the colour of the body.
    ///
    /// The halo alone is not enough, and this is the difference: it thickens the body's *own* edge, so on a page the
    /// same colour as the rod there is still only a faint rod with a faint edge. The shadow is drawn past that edge, in
    /// the halo's colour, and the two together are what an eye finds.
    #[test]
    fn the_body_casts_a_shadow_past_its_own_edge() {
        let span = surface_span(1.0) as usize;
        let middle = span as f32 / 2.0;

        // A pen laid flat to the right, so the body runs along x and its shadow is the only thing that can be drawn
        // past the far end of the body itself.
        let shape = CursorShape {
            colour: 0xFF_00_00,
            halo: 0x00_00_FF,
            ..shape((middle, middle), Some((80.0, 0.0)))
        };
        let body = shape.cursor.body_shape().expect("a flat pen has a body");
        // The body's own reach, flattened exactly as the drawing flattens it: the control points' hull is far wider
        // than the curves inside it, so a point past *this* is past the body, its edge and its antialiasing.
        let mut outline = Vec::new();
        flatten(&body.outline(0.0), &|point| point, &mut outline);
        let furthest = outline
            .iter()
            .map(|point| point[0])
            .fold(f32::MIN, f32::max);
        let lowest = outline
            .iter()
            .map(|point| point[1])
            .fold(f32::MIN, f32::max);

        let mut surface = Surface::new(span, span);
        surface.draw(&shape, [middle, middle], None);

        let (_, _, right, bottom) = drawn(&surface).expect("a leaning pen draws a body");
        assert!(
            right as f32 > furthest + BODY_HALO_GROW,
            "the shadow reaches past the body and its edge, to the right: {right} against {furthest} + {BODY_HALO_GROW}"
        );
        assert!(
            bottom as f32 > lowest + BODY_HALO_GROW,
            "and below it: {bottom} against {lowest} + {BODY_HALO_GROW}"
        );

        let shadow = (0..span)
            .map(|row| surface.pixels()[row * span + right])
            .find(|pixel| *pixel != 0)
            .expect("the rightmost drawn column has a pixel in it");

        assert!(
            shadow & 0xFF > (shadow >> 16) & 0xFF,
            "and what is out there is the halo's colour rather than the body's: {shadow:#010X}"
        );
    }

    /// The window is sized for the shadow's own reach, and the reach is the length of the drop: a shadow that reached
    /// further than the window allows would be a ghost with its edge cut off.
    ///
    /// The two are written down separately because a `const` cannot take a square root, so this is what holds them
    /// together.
    #[test]
    fn the_shadow_reaches_no_further_than_the_window_allows() {
        let reach = (BODY_SHADOW_DROP[0].powi(2) + BODY_SHADOW_DROP[1].powi(2)).sqrt();

        assert!(
            BODY_SHADOW_REACH >= reach,
            "the drop reaches {reach} and the window allows {BODY_SHADOW_REACH}"
        );
        assert!(
            BODY_SHADOW_DROP[0] > BODY_HALO_GROW && BODY_SHADOW_DROP[1] > BODY_HALO_GROW,
            "the shadow is thrown further than the edge reaches, or the two would be one edge"
        );
        assert!(
            BODY_SHADOW_ALPHA < BODY_ALPHA,
            "a shadow is fainter than the body that throws it"
        );
        assert!(
            BODY_SHADOW_ALPHA > BODY_HALO_ALPHA,
            "and stronger than the rim, because being found is its whole job"
        );
    }

    /// The bounding box of everything drawn on a surface, or `None` when nothing was.
    fn drawn(surface: &Surface) -> Option<(usize, usize, usize, usize)> {
        let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);

        for (index, pixel) in surface.pixels().iter().enumerate() {
            if *pixel == 0 {
                continue;
            }

            let (x, y) = (index % surface.width(), index / surface.width());
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }

        (left != usize::MAX).then_some((left, top, right, bottom))
    }

    /// The window is odd — so that the nib's centre is the centre of a pixel — and wide enough for
    /// the longest body a pen can lean out, whatever the scale factor says.
    #[test]
    fn the_window_is_odd_and_reaches_further_than_the_body() {
        let span = surface_span(1.0);

        assert_eq!(span % 2, 1, "an odd side has a centre pixel: {span}");
        assert!(
            span as f32 >= REACH * 2.0,
            "the ghost reaches {REACH} and the window is {span} across"
        );
        assert!(surface_span(2.0) > span, "a denser screen gets more pixels");
        assert_eq!(
            surface_span(0.0),
            surface_span(1.0),
            "a scale factor that cannot be multiplied by is treated as one"
        );
        assert_eq!(surface_span(f32::NAN), surface_span(1.0));
    }

    /// Placing the window and drawing the nib in it is lossless: the fraction the floor left over is
    /// the fraction the drawing puts back, so a pen between two pixels is drawn between two pixels.
    #[test]
    fn the_nib_keeps_its_fraction() {
        let span = surface_span(1.5);

        for nib in [[100.25, 200.75], [0.0, 0.0], [-12.5, 30.125]] {
            let origin = window_origin(nib, span);
            let offset = nib_offset(nib, origin);

            for axis in 0..2 {
                let placed = origin[axis] as f32 + offset[axis];
                assert!(
                    (placed - nib[axis]).abs() < 0.0001,
                    "the nib is where it was: {placed} against {}",
                    nib[axis]
                );
                assert!(
                    (0.0..span as f32).contains(&offset[axis]),
                    "the nib landed inside the surface it is drawn in: {offset:?} of {span}"
                );
            }
        }
    }

    /// The blend is what `UpdateLayeredWindow` asks for: premultiplied, so no channel outruns its
    /// own alpha, and half of a colour is half of the colour rather than all of it at half alpha.
    #[test]
    fn a_half_covered_colour_is_premultiplied() {
        assert_eq!(
            over(0, [255, 255, 255], 0.5, 1.0),
            0x80_80_80_80,
            "half a white pixel"
        );
        assert_eq!(
            over(0, [255, 0, 0], 1.0, 0.5),
            0x80_80_00_00,
            "half an opaque red pixel"
        );
        assert_eq!(
            over(0, [255, 255, 255], 0.0, 1.0),
            0,
            "no coverage changes nothing"
        );

        let pixel = over(0, [200, 100, 50], 0.25, 0.8);
        let alpha = pixel >> 24;
        for shift in [16, 8, 0] {
            assert!(
                (pixel >> shift) & 0xFF <= alpha,
                "a channel is at most the alpha it was premultiplied by: {pixel:#010x}"
            );
        }
    }

    /// A disc covers the pixel under its centre and leaves the ones outside its radius alone.
    #[test]
    fn a_disc_covers_its_centre_and_nothing_beyond_its_radius() {
        let mut surface = Surface::new(16, 16);
        surface.fill_disc([8.0, 8.0], 4.0, None, 1.0, [0, 0, 0]);

        let opaque = |surface: &Surface, x: usize, y: usize| surface.pixels()[y * 16 + x] >> 24;

        assert_eq!(opaque(&surface, 8, 8), 255, "the centre is covered");
        assert_eq!(opaque(&surface, 2, 2), 0, "the corner never was");
        assert_eq!(opaque(&surface, 8, 3), 0, "nor a row above the radius");

        // Somewhere on the rim a pixel is only part-covered: that is the antialiasing, and it is the
        // whole reason the coverage is computed rather than the pixel being in or out.
        let partial = surface
            .pixels()
            .iter()
            .filter(|pixel| {
                let alpha = *pixel >> 24;
                alpha > 0 && alpha < 255
            })
            .count();

        assert!(
            partial > 0,
            "the rim is part-covered: {partial} such pixels"
        );
    }

    /// The ghost is asked for only where there is a sheet to draw it on.
    #[test]
    fn a_ghost_is_asked_for_only_on_the_sheet() {
        let hidden = |screen: Screen, phase: PenPhase| {
            cursor_for(reading(phase, (100.0, 300.0), Some((10.0, 10.0))), &screen).is_none()
        };

        assert!(
            !hidden(screen(), PenPhase::Hover),
            "a pen on the sheet has one"
        );
        assert!(hidden(
            Screen {
                suppressed: true,
                ..screen()
            },
            PenPhase::Hover
        ));
        assert!(
            hidden(
                Screen {
                    sheet_top: 320.0,
                    ..screen()
                },
                PenPhase::Hover
            ),
            "above the bar's edge the pointer is the cursor, not the ghost"
        );
        assert!(
            hidden(screen(), PenPhase::Leave),
            "a pen out of range has no position to draw"
        );
        assert!(hidden(screen(), PenPhase::Cancel));
    }

    /// The body leans up out of a clip and is cut there: nothing above the line, and nothing lost
    /// below it.
    #[test]
    fn the_clip_cuts_the_body_at_the_bar() {
        let span = surface_span(1.0) as usize;
        let mut surface = Surface::new(span, span);
        let middle = span as f32 / 2.0;
        // A lean forty degrees toward the top of the window: `tilt_y` is positive toward the user.
        let shape = shape((100.0, 300.0), Some((0.0, -40.0)));
        let clip = middle - 8.0;

        surface.draw(&shape, [middle, middle], Some(clip));
        let (_, cut_top, _, cut_bottom) = drawn(&surface).expect("a ghost was drawn");

        assert!(
            (cut_top as f32 - clip).abs() <= 1.0,
            "the drawing starts in the row the clip passes through: {cut_top} against {clip}"
        );
        assert!(
            cut_bottom as f32 > middle,
            "the nib mark is still drawn below it"
        );

        surface.clear();
        surface.draw(&shape, [middle, middle], None);
        let (_, free_top, _, free_bottom) = drawn(&surface).expect("a ghost was drawn");

        assert!(
            free_top < cut_top,
            "without the clip the body reaches higher: {free_top} against {cut_top}"
        );
        assert_eq!(
            free_bottom, cut_bottom,
            "and the clip takes nothing away below it"
        );
    }

    /// Wherever the pen leans, the ghost fits inside the window it is drawn in with room to spare —
    /// which is the whole reason the window is twice [`REACH`] across.
    #[test]
    fn the_ghost_fits_inside_the_window() {
        let leans = [
            None,
            Some((0.0, 80.0)),
            Some((0.0, -80.0)),
            Some((80.0, 0.0)),
            Some((-80.0, 0.0)),
        ];

        for tilt in leans {
            for scale in [1.0, 1.5, 2.0] {
                let span = surface_span(scale) as usize;
                let mut surface = Surface::new(span, span);
                let middle = span as f32 / 2.0;
                let mut shape = shape((100.0, 300.0), tilt);
                shape.scale = scale;
                // The widest mark the shape allows, so the window is checked against the biggest ghost there can be.
                shape.nib_radius = NIB_RADIUS_MAX;

                surface.draw(&shape, [middle, middle], None);
                let (left, top, right, bottom) =
                    drawn(&surface).expect("a pen has a nib mark with or without a lean");

                assert!(
                    left > 0 && top > 0 && right + 1 < span && bottom + 1 < span,
                    "a lean of {tilt:?} at {scale} was drawn to the window's edge: \
                     {left},{top}..{right},{bottom} of {span}"
                );
            }
        }
    }

    /// A shape that is off the surface is not drawn at all, rather than wrapped into it.
    #[test]
    fn a_shape_off_the_surface_is_left_alone() {
        assert_eq!(row_bounds(-40.0, -10.0, 8), None, "above the surface");
        assert_eq!(row_bounds(20.0, 30.0, 8), None, "below it");
        assert_eq!(row_bounds(f32::NAN, 4.0, 8), None, "and nothing finite");
        assert_eq!(
            row_bounds(0.0, 100.0, 8),
            Some((0, 7)),
            "clamped to the rows"
        );
        assert_eq!(row_bounds(-10.0, 3.0, 8), Some((0, 3)));
    }
}
