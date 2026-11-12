//! The sheet on screen: its colour, what is printed on it, and the palettes the bar offers.
//!
//! The sizes and styles themselves are the *note's* — a note remembers the paper it was written on,
//! so they live in the format crate ([`note_format::paper`]) and are re-exported here, which is
//! what keeps `crate::canvas::CanvasSize` the name the rest of the app already uses.
//!
//! ## Why the ruling is cached
//!
//! Ruling is pure geometry derived from the sheet's rectangle and style. Rebuilding it every frame
//! would put a per-frame cost on the one loop this application cares about — and a grid over a full
//! page is several hundred marks — so it is built once per sheet and handed to the canvas as an
//! `Arc`, the same way the finished strokes are.
//!
//! ## What the ruling applies to
//!
//! The blank sheet only. A PDF page is its own paper: it brings its own size, its own colour and
//! its own printed lines, and laying a note grid over a document would obscure what the document
//! says rather than help write on it.

use std::sync::Arc;

use gpui_kit::*;

use crate::ink_layer::canvas::{Fill, Rect};
use crate::pages::Quarters;

/// The paper a note is written on: the sizes, and what is printed on it.
///
/// Re-exported rather than defined here, because the note remembers both in its own settings (see
/// [`note_format::paper`]). The names the app uses are unchanged.
pub use note_format::paper::{CanvasSize, CanvasStyle, FrameCap};

/// The thickness of a rule, in logical pixels.
///
/// The view's number, not the note's: how thick a printed line is drawn is a question about a
/// screen, and two applications may answer it differently.
const RULE_THICKNESS: f32 = 1.0;

/// The diameter of one dot-grid dot, in logical pixels. See [`RULE_THICKNESS`].
const DOT_DIAMETER: f32 = 2.5;

/// A colour offered in the toolbar, together with the stable element id its swatch draws with.
///
/// The id lives in the table rather than being derived from the colour because element ids must
/// be unique across the whole element tree *and* stable between frames; a table gives both for
/// free, and a duplicate becomes a visibly wrong build rather than a subtle loss of state.
pub struct Swatch {
    /// What the colour is called. Shown in the tooltip and used by tests.
    pub name: &'static str,
    /// The swatch's element id.
    pub id: &'static str,
    /// The colour as `0xRRGGBB`.
    pub color: u32,
}

/// The paper colours offered.
///
/// The colours a notebook's own paper comes in rather than a full colour wheel: white, the cream and
/// yellow of a legal pad, a cool grey, and the two darks for writing on with a light pen. A full
/// picker would also let someone choose a sheet the same colour as their ink, which is the one
/// combination that cannot be written in.
pub const PAPER_COLORS: [Swatch; 6] = [
    Swatch {
        name: "white",
        id: "paper-white",
        color: 0xFF_FF_FF,
    },
    Swatch {
        name: "cream",
        id: "paper-cream",
        color: 0xFB_F3_E4,
    },
    Swatch {
        name: "yellow",
        id: "paper-yellow",
        color: 0xFC_F1_C4,
    },
    Swatch {
        name: "grey",
        id: "paper-grey",
        color: 0xE9_EA_ED,
    },
    Swatch {
        name: "slate",
        id: "paper-slate",
        color: 0x2C_2F_36,
    },
    Swatch {
        name: "black",
        id: "paper-black",
        color: 0x15_17_1B,
    },
];

/// The ink colours offered.
///
/// A notebook's pen case: the four greys a page is annotated in — black, grey, light grey, and the
/// white that only shows on a dark sheet — and the colours a diagram is drawn in. Twelve, because
/// they are read as a palette and not picked from a wheel: two rows of six is a set a person can
/// learn, and the toolbar wraps them anyway.
pub const INK_COLORS: [Swatch; 12] = [
    Swatch {
        name: "black",
        id: "ink-black",
        color: 0x1C_1C_1E,
    },
    Swatch {
        name: "grey",
        id: "ink-grey",
        color: 0x7A_7A_80,
    },
    Swatch {
        name: "light grey",
        id: "ink-light-grey",
        color: 0xC7_C7_CC,
    },
    Swatch {
        name: "white",
        id: "ink-white",
        color: 0xFF_FF_FF,
    },
    Swatch {
        name: "red",
        id: "ink-red",
        color: 0xE5_48_4D,
    },
    Swatch {
        name: "orange",
        id: "ink-orange",
        color: 0xF0_8A_24,
    },
    Swatch {
        name: "yellow",
        id: "ink-yellow",
        color: 0xFF_C4_00,
    },
    Swatch {
        name: "green",
        id: "ink-green",
        color: 0x2F_A8_4F,
    },
    Swatch {
        name: "teal",
        id: "ink-teal",
        color: 0x1F_A8_A0,
    },
    Swatch {
        name: "blue",
        id: "ink-blue",
        color: 0x0A_84_FF,
    },
    Swatch {
        name: "purple",
        id: "ink-purple",
        color: 0x8E_5B_F0,
    },
    Swatch {
        name: "pink",
        id: "ink-pink",
        color: 0xE8_50_8C,
    },
];

/// The colours the highlighter lays down, as the `0xRRGGBB` each stroke is stamped with.
///
/// A palette of their own rather than the pen's, and the reason is the *alpha* the tool adds to them
/// ([`crate::ink::HIGHLIGHTER_ALPHA`]): a band that is read *through* has to be a colour there is something to
/// read behind. Laid over black ink at that alpha, these four stay recognisably themselves and leave the writing
/// and the ruling visible; a pen's black at the same alpha would be a grey smear over the page.
pub const HIGHLIGHTER_COLORS: [Swatch; 4] = [
    Swatch {
        name: "yellow",
        id: "highlighter-yellow",
        color: 0xFF_EB_3B,
    },
    Swatch {
        name: "green",
        id: "highlighter-green",
        color: 0xA8_E0_6A,
    },
    Swatch {
        name: "pink",
        id: "highlighter-pink",
        color: 0xFF_8F_B1,
    },
    Swatch {
        name: "blue",
        id: "highlighter-blue",
        color: 0x7F_C4_FF,
    },
];

/// The colour a rule is drawn in on the given paper.
///
/// One fixed grey cannot serve both a white sheet and a blackboard: it vanishes on one of them. On a
/// light sheet the rule is mixed *toward a blue-grey* rather than toward black, because that is what
/// a printed rule looks like — the ink a notebook's ruling is printed in is blue-black, and a
/// neutral grey line reads as a pencil mark rather than as part of the page. On a dark sheet there
/// is nothing to print and the rule is mixed toward white instead.
pub fn rule_color(paper: u32) -> u32 {
    if relative_luminance(paper) > 0.5 {
        mix(paper, 0x5A_7C_9E, 0.25)
    } else {
        mix(paper, 0xFF_FF_FF, 0.22)
    }
}

/// The colour a mark that has to be *noticed* is drawn in on the given paper.
///
/// Where [`rule_color`] is deliberately faint — a guide must not compete with the ink written over
/// it — this is the other end of the same idea: mixed most of the way to the far extreme, so it
/// stands out on white paper and on a blackboard alike, while on neither does it look like ink.
/// It is what the pen's ghost cursor is drawn in.
pub fn contrast_color(paper: u32) -> u32 {
    if relative_luminance(paper) > 0.5 {
        mix(paper, 0x00_00_00, 0.62)
    } else {
        mix(paper, 0xFF_FF_FF, 0.72)
    }
}

/// The perceived brightness of `0xRRGGBB`, from 0 (black) to 1 (white).
///
/// The channels are weighted the way the eye weights them — green carries most of the perceived
/// brightness and blue the least — so "is this sheet light?" is answered the way a person would
/// answer it rather than by a flat average.
pub fn relative_luminance(color: u32) -> f32 {
    let (r, g, b) = channels(color);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Mixes `t` of `toward` into `color`, channel by channel.
pub fn mix(color: u32, toward: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let (a, b) = (channels(color), channels(toward));

    let blend = |a: f32, b: f32| (a + (b - a) * t).clamp(0.0, 1.0);
    let channel = |value: f32| (value * 255.0).round() as u32;

    (channel(blend(a.0, b.0)) << 16) | (channel(blend(a.1, b.1)) << 8) | channel(blend(a.2, b.2))
}

/// Splits `0xRRGGBB` into three channels scaled to 0..1.
fn channels(color: u32) -> (f32, f32, f32) {
    let scale = |shift: u32| ((color >> shift) & 0xFF) as f32 / 255.0;
    (scale(16), scale(8), scale(0))
}

/// What the cached ruling was built for.
///
/// Compared field by field rather than hashed: the sheet's rectangles are a few floats and the rest
/// are two small enums, so there is nothing to gain from a hash and one more thing to get wrong.
#[derive(Clone, Copy, PartialEq)]
struct RulingKey {
    at: RulingSheet,
    style: CanvasStyle,
    paper: u32,
    /// Part of the key, not derivable from the rectangle: the rule spacing depends on it, and two
    /// different papers drawn at two different zooms can share a rectangle.
    zoom: f32,
}

/// Where a sheet's ruling goes: the paper's own rectangle, and which way up the page is.
///
/// **Not** the rectangle it is drawn in. The ruling is *printed on the paper*: turning the page turns its
/// ruling, so the turns are part of what is built, but where the paper happens to sit in the window is not —
/// a pan moves every mark by the same distance, and re-placing a few hundred of them on each frame of a pan is
/// work whose answer never differs from the last one by more than that offset. So the geometry is built and
/// placed at the paper's own corner, and the pan is added when it is drawn (see [`Ruling::rules`]).
///
/// Building it in the drawn rectangle instead would give a turned page a ruling that is the wrong way round,
/// which is the difference between paper that was turned and a grid an interface painted over it.
#[derive(Clone, Copy, PartialEq)]
pub struct RulingSheet {
    /// The paper's own rectangle, in logical pixels at the *drawn* zoom: the un-turned shape.
    pub paper: Bounds<Pixels>,
    /// How far the page has been turned, in quarter turns clockwise.
    pub turns: Quarters,
}

/// The rule geometry for the current sheet, reused until the sheet changes.
#[derive(Default)]
pub struct Ruling {
    built_for: Option<RulingKey>,
    rules: Arc<Vec<Fill>>,
    /// How many times the geometry has actually been built.
    ///
    /// Counted here rather than inferred by the caller: whether a request was served from the cache
    /// is the one thing about this type that its caller cannot see, and it is exactly what the
    /// frame's own measurements need to know.
    rebuilds: u64,
}

impl Ruling {
    /// How many times the geometry has been built since the app started.
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// The ruling for this sheet, as the marks the canvas draws.
    ///
    /// Returns the cached set when the sheet has not changed, so a frame that only adds ink pays
    /// nothing for the ruling; otherwise rebuilds it once and caches that.
    ///
    /// `zoom` is separate from the rectangles because the ruling is *printed on the paper*: the rules
    /// have to grow with it, or zooming in would make the grid relatively finer instead of closer. It is
    /// part of the cache key for the same reason — two different papers at two different zooms can share
    /// a rectangle.
    pub fn rules(
        &mut self,
        at: RulingSheet,
        style: CanvasStyle,
        paper: u32,
        zoom: f32,
    ) -> Arc<Vec<Fill>> {
        let key = RulingKey {
            at,
            style,
            paper,
            zoom,
        };

        if self.built_for != Some(key) {
            let rules = build_ruling(at.paper, style, rgb(rule_color(paper)).into(), zoom);
            // Placed at the paper's *own* corner, which is where the rectangle it was built on starts. The
            // pan is added by whoever draws it, so moving the sheet never rebuilds this.
            self.rules = Arc::new(place(rules, at, (0.0, 0.0)));
            self.built_for = Some(key);
            self.rebuilds += 1;
        }

        Arc::clone(&self.rules)
    }

    /// The ruling shifted by where the sheet is drawn, for a caller that is about to draw it.
    ///
    /// A pan is one offset applied to every mark, and applying it *here* — once per frame, over a vector that
    /// is already built — is what keeps a pan cheap: the geometry a few hundred marks long is placed once, and
    /// the frame that follows only adds the pan to each of them. Doing it in [`Self::rules`] instead made the
    /// cache key include the pan, so every frame of every pan missed and rebuilt the whole grid.
    ///
    /// Nothing is allocated: the caller draws straight from this.
    pub fn rules_at(rules: &[Fill], origin: (f32, f32)) -> impl Iterator<Item = Fill> + '_ {
        rules.iter().map(move |fill| {
            let mut moved = *fill;
            moved.rect.x += origin.0;
            moved.rect.y += origin.1;
            moved
        })
    }
}

/// Builds the ruling for a sheet. Empty for a blank sheet.
///
/// Everything about it — how far apart the rules are, and how thick — is scaled by the zoom, so
/// the ruling behaves like something printed on the paper rather than a screen overlay that happens
/// to be the same colour.
fn build_ruling(sheet: Bounds<Pixels>, style: CanvasStyle, color: Hsla, zoom: f32) -> Vec<Fill> {
    let Some(spacing) = style.spacing() else {
        return Vec::new();
    };

    // A zoom of zero or NaN would make every rule land on top of every other one.
    let zoom = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    };
    let spacing = spacing * zoom;

    let x: f32 = sheet.origin.x.into();
    let y: f32 = sheet.origin.y.into();
    let width: f32 = sheet.size.width.into();
    let height: f32 = sheet.size.height.into();
    let thick = RULE_THICKNESS * zoom;

    let mut quads = Vec::new();
    match style {
        CanvasStyle::Plain => {}
        CanvasStyle::Ruled => {
            for line in 1..=rules_that_fit(height, spacing) {
                quads.push(rule(x, y + line as f32 * spacing, width, thick, color));
            }
        }
        CanvasStyle::Grid => {
            for line in 1..=rules_that_fit(height, spacing) {
                quads.push(rule(x, y + line as f32 * spacing, width, thick, color));
            }
            for line in 1..=rules_that_fit(width, spacing) {
                quads.push(vertical_rule(
                    x + line as f32 * spacing,
                    y,
                    height,
                    thick,
                    color,
                ));
            }
        }
        CanvasStyle::Dots => {
            let diameter = DOT_DIAMETER * zoom;
            for row in 1..=rules_that_fit(height, spacing) {
                for column in 1..=rules_that_fit(width, spacing) {
                    quads.push(dot(
                        x + column as f32 * spacing,
                        y + row as f32 * spacing,
                        diameter,
                        color,
                    ));
                }
            }
        }
    }

    quads
}

/// Moves marks built on the paper's own rectangle into the rectangle the sheet is drawn in, turned with
/// the page.
///
/// A quarter turn takes an axis-aligned rectangle to another one, so each mark is still one fill — and
/// the mapping is [`crate::ink::drawn_of_paper`]'s, applied to whole rectangles, which is what keeps the
/// ruling and the ink agreeing about which way up the page is.
fn place(rules: Vec<Fill>, at: RulingSheet, origin: (f32, f32)) -> Vec<Fill> {
    let (x, y) = origin;
    let (width, height) = (
        f32::from(at.paper.size.width),
        f32::from(at.paper.size.height),
    );

    rules
        .into_iter()
        .map(|mut fill| {
            let rect = fill.rect;

            fill.rect = match at.turns % 4 {
                1 => Rect {
                    x: x + height - rect.y - rect.height,
                    y: y + rect.x,
                    width: rect.height,
                    height: rect.width,
                },
                2 => Rect {
                    x: x + width - rect.x - rect.width,
                    y: y + height - rect.y - rect.height,
                    width: rect.width,
                    height: rect.height,
                },
                3 => Rect {
                    x: x + rect.y,
                    y: y + width - rect.x - rect.width,
                    width: rect.height,
                    height: rect.width,
                },
                _ => Rect {
                    x: x + rect.x,
                    y: y + rect.y,
                    width: rect.width,
                    height: rect.height,
                },
            };

            fill
        })
        .collect()
}

/// How many rules fit inside `extent`, leaving `spacing` of margin at each end.
///
/// The margin is the point: a rule hard against the sheet's edge reads as a border, and a dot
/// centred on the edge would be clipped in half. Counting from one spacing in keeps the entire
/// ruling inside the sheet, so nothing has to be clipped when it is drawn.
fn rules_that_fit(extent: f32, spacing: f32) -> usize {
    // `is_nan` spelled out rather than relying on `!(spacing > 0.0)`: a `NaN` spacing is the case this
    // has to refuse, and a reader should not have to know that every comparison against a `NaN` is false
    // to see that it does.
    if spacing.is_nan() || spacing <= 0.0 || extent <= spacing {
        return 0;
    }

    // `+ 1e-4` absorbs the error of accumulating `line * spacing` in floating point, so a rule
    // that lands exactly on the last legal position is not dropped.
    (((extent - spacing) / spacing) + 1e-4).floor() as usize
}

/// A horizontal rule filling the sheet's width.
fn rule(x: f32, y: f32, width: f32, thickness: f32, colour: Hsla) -> Fill {
    Fill::new(
        Rect {
            x,
            y,
            width,
            height: thickness,
        },
        colour,
    )
}

/// A vertical rule filling the sheet's height.
///
/// A separate function rather than a sign flip on [`rule`]: a rule's two dimensions are not
/// interchangeable, and passing a height into a width is exactly the mistake that puts a line
/// off the side of the sheet.
fn vertical_rule(x: f32, y: f32, height: f32, thickness: f32, colour: Hsla) -> Fill {
    Fill::new(
        Rect {
            x,
            y,
            width: thickness,
            height,
        },
        colour,
    )
}

/// One dot of a dot grid, centred on the given position.
fn dot(x: f32, y: f32, diameter: f32, colour: Hsla) -> Fill {
    let radius = diameter / 2.0;

    // A square whose corners are rounded by half its side is a circle.
    Fill::new(
        Rect {
            x: x - radius,
            y: y - radius,
            width: diameter,
            height: diameter,
        },
        colour,
    )
    .rounded(radius)
}

#[cfg(test)]
mod tests;
