//! The sheet: its size, its colour, and what is printed on it.
//!
//! ## Why the sizes are physical
//!
//! A canvas size *is* a paper size, and paper is specified in millimetres. Storing the physical
//! dimensions once and deriving both the drawn aspect ratio and the drawn width from a single
//! pixels-per-millimetre constant keeps the sizes comparable: A5 is really half of A4 on screen,
//! not merely a different shape that happens to be about as big. Picking a size therefore sets
//! both the shape and the scale, which is what a person choosing "A5" expects.
//!
//! ## Why the ruling is cached
//!
//! Ruling is pure geometry derived from the sheet's rectangle and style. Rebuilding it in the
//! paint callback would put a per-frame cost on the one loop this application cares about — and
//! a grid over a full page is several hundred quads — so it is built once per sheet and handed
//! to the frame as an `Arc`, the same way finished strokes are.
//!
//! ## What the ruling applies to
//!
//! The blank sheet only. A PDF page is its own paper: it brings its own size, its own colour and
//! its own printed lines, and laying a note grid over a document would obscure what the document
//! says rather than help write on it.

use std::sync::Arc;

use gpui_kit::*;
use serde::{Deserialize, Serialize};

/// How many logical pixels one millimetre of paper is drawn at.
///
/// Chosen so the default sheet, A4, comes out 720 logical pixels wide — a comfortable reading
/// size in a 1280-pixel window.
const PX_PER_MM: f32 = 720.0 / 210.0;

/// The thickness of a rule, in logical pixels.
const RULE_THICKNESS: f32 = 1.0;

/// The diameter of one dot-grid dot, in logical pixels.
const DOT_DIAMETER: f32 = 2.5;

/// The shape and scale of the sheet written on when no PDF is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasSize {
    /// ISO A4, 210 by 297 mm: the default.
    A4,
    /// ISO A5, 148 by 210 mm: half a sheet of A4, for small notes.
    A5,
    /// US Letter, 8.5 by 11 in.
    Letter,
    /// US Legal, 8.5 by 14 in.
    Legal,
    /// A square sheet, for diagrams rather than prose.
    Square,
    /// A4 turned on its side, for wide material.
    Wide,
}

impl CanvasSize {
    /// Every size the toolbar offers, in the order it offers them.
    pub const ALL: [CanvasSize; 6] = [
        CanvasSize::A4,
        CanvasSize::A5,
        CanvasSize::Letter,
        CanvasSize::Legal,
        CanvasSize::Square,
        CanvasSize::Wide,
    ];

    /// The label on this size's button.
    pub fn label(self) -> &'static str {
        match self {
            CanvasSize::A4 => "A4",
            CanvasSize::A5 => "A5",
            CanvasSize::Letter => "Letter",
            CanvasSize::Legal => "Legal",
            CanvasSize::Square => "Square",
            CanvasSize::Wide => "Wide",
        }
    }

    /// The size a label names.
    ///
    /// The toolbar's chooser carries a choice back as the text that was showing in it — the
    /// options it holds are strings, not sizes — so the label is what has to become a size again.
    /// It is looked up by name rather than by position on purpose: the position is the one thing
    /// about the list that may change, and a size inserted in the middle must not rename the
    /// choice that was already made.
    pub fn from_label(label: &str) -> Option<CanvasSize> {
        CanvasSize::ALL
            .iter()
            .copied()
            .find(|size| size.label() == label)
    }

    /// The sheet's width and height in millimetres.
    pub fn millimetres(self) -> (f32, f32) {
        match self {
            CanvasSize::A4 => (210.0, 297.0),
            CanvasSize::A5 => (148.0, 210.0),
            CanvasSize::Letter => (215.9, 279.4),
            CanvasSize::Legal => (215.9, 355.6),
            CanvasSize::Square => (200.0, 200.0),
            CanvasSize::Wide => (297.0, 210.0),
        }
    }

    /// The height divided by the width.
    pub fn aspect(self) -> f32 {
        let (width, height) = self.millimetres();
        height / width
    }

    /// The width, in logical pixels, this size is drawn at when it is selected.
    pub fn display_width(self) -> f32 {
        self.millimetres().0 * PX_PER_MM
    }

    /// The sheet's drawn size at the given width, in logical pixels.
    pub fn display_size(self, width: f32) -> (f32, f32) {
        (width, width * self.aspect())
    }
}

/// What is printed on the sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasStyle {
    /// A blank sheet.
    Plain,
    /// Horizontal rules, for prose.
    Ruled,
    /// A square grid, for diagrams and sketching.
    Grid,
    /// A dot grid: the least ink that still guides a line.
    Dots,
}

impl CanvasStyle {
    /// Every style the toolbar offers, in the order it offers them.
    pub const ALL: [CanvasStyle; 4] = [
        CanvasStyle::Plain,
        CanvasStyle::Ruled,
        CanvasStyle::Grid,
        CanvasStyle::Dots,
    ];

    /// The label on this style's button.
    pub fn label(self) -> &'static str {
        match self {
            CanvasStyle::Plain => "Plain",
            CanvasStyle::Ruled => "Ruled",
            CanvasStyle::Grid => "Grid",
            CanvasStyle::Dots => "Dots",
        }
    }

    /// The style a label names. See [`CanvasSize::from_label`] for why the lookup is by name.
    pub fn from_label(label: &str) -> Option<CanvasStyle> {
        CanvasStyle::ALL
            .iter()
            .copied()
            .find(|style| style.label() == label)
    }

    /// The distance between two rules, in logical pixels, or `None` for a blank sheet.
    ///
    /// The spacings differ by style on purpose: prose wants a taller line than a diagram wants a
    /// square, so a single number would leave one of the two cramped.
    pub fn spacing(self) -> Option<f32> {
        match self {
            CanvasStyle::Plain => None,
            CanvasStyle::Ruled => Some(34.0),
            CanvasStyle::Grid => Some(28.0),
            CanvasStyle::Dots => Some(28.0),
        }
    }
}

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
    Swatch { name: "white", id: "paper-white", color: 0xFF_FF_FF },
    Swatch { name: "cream", id: "paper-cream", color: 0xFB_F3_E4 },
    Swatch { name: "yellow", id: "paper-yellow", color: 0xFC_F1_C4 },
    Swatch { name: "grey", id: "paper-grey", color: 0xE9_EA_ED },
    Swatch { name: "slate", id: "paper-slate", color: 0x2C_2F_36 },
    Swatch { name: "black", id: "paper-black", color: 0x15_17_1B },
];

/// The ink colours offered.
///
/// A notebook's pen case: the four greys a page is annotated in — black, grey, light grey, and the
/// white that only shows on a dark sheet — and the colours a diagram is drawn in. Twelve, because
/// they are read as a palette and not picked from a wheel: two rows of six is a set a person can
/// learn, and the toolbar wraps them anyway.
pub const INK_COLORS: [Swatch; 12] = [
    Swatch { name: "black", id: "ink-black", color: 0x1C_1C_1E },
    Swatch { name: "grey", id: "ink-grey", color: 0x7A_7A_80 },
    Swatch { name: "light grey", id: "ink-light-grey", color: 0xC7_C7_CC },
    Swatch { name: "white", id: "ink-white", color: 0xFF_FF_FF },
    Swatch { name: "red", id: "ink-red", color: 0xE5_48_4D },
    Swatch { name: "orange", id: "ink-orange", color: 0xF0_8A_24 },
    Swatch { name: "yellow", id: "ink-yellow", color: 0xFF_C4_00 },
    Swatch { name: "green", id: "ink-green", color: 0x2F_A8_4F },
    Swatch { name: "teal", id: "ink-teal", color: 0x1F_A8_A0 },
    Swatch { name: "blue", id: "ink-blue", color: 0x0A_84_FF },
    Swatch { name: "purple", id: "ink-purple", color: 0x8E_5B_F0 },
    Swatch { name: "pink", id: "ink-pink", color: 0xE8_50_8C },
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
/// Compared field by field rather than hashed: the sheet's rectangle is four floats and the rest
/// are two small enums, so there is nothing to gain from a hash and one more thing to get wrong.
#[derive(Clone, Copy, PartialEq)]
struct RulingKey {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    style: CanvasStyle,
    paper: u32,
    /// Part of the key, not derivable from the rectangle: the rule spacing depends on it, and two
    /// different papers drawn at two different zooms can share a rectangle.
    zoom: f32,
}

/// The rule geometry for the current sheet, reused until the sheet changes.
#[derive(Default)]
pub struct Ruling {
    built_for: Option<RulingKey>,
    quads: Arc<Vec<PaintQuad>>,
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
    /// The rule quads for this sheet.
    ///
    /// Returns the cached set when the sheet has not changed, so a frame that only adds ink pays
    /// nothing for the ruling; otherwise rebuilds it once and caches that.
    ///
    /// `zoom` is separate from the sheet's rectangle because the ruling is *printed on the paper*:
    /// the rules have to grow with it, or zooming in would make the grid relatively finer instead
    /// of closer. It is part of the cache key for the same reason — two different papers at two
    /// different zooms can share a rectangle.
    pub fn quads(
        &mut self,
        sheet: Bounds<Pixels>,
        style: CanvasStyle,
        paper: u32,
        zoom: f32,
    ) -> Arc<Vec<PaintQuad>> {
        let key = RulingKey {
            x: sheet.origin.x.into(),
            y: sheet.origin.y.into(),
            width: sheet.size.width.into(),
            height: sheet.size.height.into(),
            style,
            paper,
            zoom,
        };

        if self.built_for != Some(key) {
            self.quads = Arc::new(build_ruling(sheet, style, rgb(rule_color(paper)).into(), zoom));
            self.built_for = Some(key);
            self.rebuilds += 1;
        }

        Arc::clone(&self.quads)
    }
}

/// Builds the ruling for a sheet. Empty for a blank sheet.
///
/// Everything about it — how far apart the rules are, and how thick — is scaled by the zoom, so
/// the ruling behaves like something printed on the paper rather than a screen overlay that happens
/// to be the same colour.
fn build_ruling(
    sheet: Bounds<Pixels>,
    style: CanvasStyle,
    color: Hsla,
    zoom: f32,
) -> Vec<PaintQuad> {
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
                quads.push(rule_quad(x, y + line as f32 * spacing, width, thick, color));
            }
        }
        CanvasStyle::Grid => {
            for line in 1..=rules_that_fit(height, spacing) {
                quads.push(rule_quad(x, y + line as f32 * spacing, width, thick, color));
            }
            for line in 1..=rules_that_fit(width, spacing) {
                quads.push(vertical_rule_quad(
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
                    quads.push(dot_quad(
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

/// How many rules fit inside `extent`, leaving `spacing` of margin at each end.
///
/// The margin is the point: a rule hard against the sheet's edge reads as a border, and a dot
/// centred on the edge would be clipped in half. Counting from one spacing in keeps the entire
/// ruling inside the sheet, so nothing has to be clipped when it is drawn.
fn rules_that_fit(extent: f32, spacing: f32) -> usize {
    if !(spacing > 0.0) || extent <= spacing {
        return 0;
    }

    // `+ 1e-4` absorbs the error of accumulating `line * spacing` in floating point, so a rule
    // that lands exactly on the last legal position is not dropped.
    (((extent - spacing) / spacing) + 1e-4).floor() as usize
}

/// A horizontal rule filling the sheet's width.
fn rule_quad(x: f32, y: f32, width: f32, thickness: f32, color: Hsla) -> PaintQuad {
    fill(
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(width), px(thickness)),
        },
        color,
    )
}

/// A vertical rule filling the sheet's height.
///
/// A separate function rather than a sign flip on [`rule_quad`]: a rule's two dimensions are not
/// interchangeable, and passing a height into a width is exactly the mistake that puts a line
/// off the side of the sheet.
fn vertical_rule_quad(x: f32, y: f32, height: f32, thickness: f32, color: Hsla) -> PaintQuad {
    fill(
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(thickness), px(height)),
        },
        color,
    )
}

/// One dot of a dot grid, centred on the given position.
fn dot_quad(x: f32, y: f32, diameter: f32, color: Hsla) -> PaintQuad {
    let radius = diameter / 2.0;
    let corner = px(radius);

    fill(
        Bounds {
            origin: point(px(x - radius), px(y - radius)),
            size: size(px(diameter), px(diameter)),
        },
        color,
    )
    // A square whose corners are rounded by half its side is a circle.
    .corner_radii(Corners {
        top_left: corner,
        top_right: corner,
        bottom_right: corner,
        bottom_left: corner,
    })
}

#[cfg(test)]
mod tests {
    // Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into
    // scope and shadow the attribute this module needs.
    use super::{
        build_ruling, contrast_color, relative_luminance, rule_color, rules_that_fit, CanvasSize,
        CanvasStyle, Ruling, INK_COLORS, PAPER_COLORS,
    };
    use gpui_kit::{point, px, rgb, size, Bounds, Hsla, PaintQuad, Pixels};
    use std::sync::Arc;

    /// The rule colour for a sheet of white paper, as a paintable colour.
    fn rule_on_white() -> Hsla {
        rgb(rule_color(0xFF_FF_FF)).into()
    }

    /// A sheet at a fixed position, so a test can assert about absolute geometry.
    fn sheet(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(24.0), px(24.0)),
            size: size(px(width), px(height)),
        }
    }

    /// The bounds of a quad, as plain floats.
    fn quad_bounds(quad: &PaintQuad) -> (f32, f32, f32, f32) {
        (
            quad.bounds.origin.x.into(),
            quad.bounds.origin.y.into(),
            quad.bounds.size.width.into(),
            quad.bounds.size.height.into(),
        )
    }

    /// Every size is drawn at one scale, so the sizes are comparable rather than merely
    /// differently shaped.
    #[test]
    fn paper_sizes_share_one_scale() {
        let a4 = CanvasSize::A4;
        let a5 = CanvasSize::A5;

        assert!(
            (a4.display_width() - 720.0).abs() < 0.5,
            "A4 is the width the scale is defined by"
        );
        assert!(
            (a5.display_width() / a4.display_width() - 148.0 / 210.0).abs() < 1e-4,
            "A5 keeps its physical proportion to A4"
        );
        assert!(
            (a4.aspect() - 297.0 / 210.0).abs() < 1e-5,
            "the aspect ratio is the paper's own"
        );
        assert!(
            (CanvasSize::Square.aspect() - 1.0).abs() < 1e-6,
            "a square sheet is square"
        );
        assert!(CanvasSize::Wide.aspect() < 1.0, "the wide sheet is landscape");

        let (width, height) = a4.display_size(500.0);
        assert!(
            (height / width - a4.aspect()).abs() < 1e-5,
            "the height follows the width"
        );
    }

    /// A blank sheet has nothing printed on it, and every other style prints something.
    #[test]
    fn only_the_blank_style_is_left_blank() {
        let sheet = sheet(720.0, 1018.0);
        let color = rule_on_white();

        assert!(build_ruling(sheet, CanvasStyle::Plain, color, 1.0).is_empty());

        for style in CanvasStyle::ALL {
            if style == CanvasStyle::Plain {
                continue;
            }
            assert!(
                !build_ruling(sheet, style, color, 1.0).is_empty(),
                "{style:?} prints something"
            );
        }
    }

    /// No rule or dot may fall outside the sheet, because nothing clips it when it is drawn.
    #[test]
    fn the_ruling_stays_inside_the_sheet() {
        let sheet = sheet(720.0, 1018.0);

        for style in CanvasStyle::ALL {
            for quad in build_ruling(sheet, style, rule_on_white(), 1.0) {
                let (x, y, width, height) = quad_bounds(&quad);
                assert!(
                    x >= 24.0 - 1e-3 && y >= 24.0 - 1e-3,
                    "{style:?} starts outside the sheet at {x},{y}"
                );
                assert!(
                    x + width <= 744.0 + 1e-3 && y + height <= 1042.0 + 1e-3,
                    "{style:?} ends outside the sheet at {},{}",
                    x + width,
                    y + height
                );
            }
        }
    }

    /// Rules are inset from both ends, so one is never mistaken for a border.
    #[test]
    fn rules_leave_a_margin_at_both_ends() {
        assert_eq!(
            rules_that_fit(0.0, 34.0),
            0,
            "a sheet with no height fits nothing"
        );
        assert_eq!(
            rules_that_fit(34.0, 34.0),
            0,
            "a sheet one spacing tall fits nothing"
        );
        assert_eq!(rules_that_fit(68.0, 34.0), 1);
        assert_eq!(rules_that_fit(102.0, 34.0), 2);
    }

    /// The ruling survives across frames: rebuilding it per frame would put geometry work on the
    /// very frame that is supposed to be showing ink.
    #[test]
    fn the_ruling_is_rebuilt_only_when_the_sheet_changes() {
        let mut ruling = Ruling::default();
        let sheet = sheet(720.0, 1018.0);

        let first = ruling.quads(sheet, CanvasStyle::Grid, 0xFF_FF_FF, 1.0);
        let again = ruling.quads(sheet, CanvasStyle::Grid, 0xFF_FF_FF, 1.0);
        assert!(
            Arc::ptr_eq(&first, &again),
            "an unchanged sheet is not rebuilt"
        );

        let restyled = ruling.quads(sheet, CanvasStyle::Dots, 0xFF_FF_FF, 1.0);
        assert!(!Arc::ptr_eq(&first, &restyled), "a new style is rebuilt");

        let repapered = ruling.quads(sheet, CanvasStyle::Dots, 0x14_16_1A, 1.0);
        assert!(
            !Arc::ptr_eq(&restyled, &repapered),
            "new paper changes the rule's colour, so it is rebuilt"
        );
    }

    /// A rule has to be visible on every sheet the toolbar offers, including the dark ones.
    #[test]
    fn a_rule_reads_on_every_paper() {
        for swatch in PAPER_COLORS.iter() {
            let paper = relative_luminance(swatch.color);
            let rule = rule_color(swatch.color);

            assert_ne!(
                rule, swatch.color,
                "a rule the colour of the {} paper is not a rule",
                swatch.name
            );
            assert!(
                (relative_luminance(rule) - paper).abs() > 0.04,
                "the rule on the {} paper is too faint to see",
                swatch.name
            );
        }
    }

    /// The ghost cursor's colour has to sit on the opposite side of mid-grey from the sheet, and
    /// be stronger than the ruling on that sheet.
    ///
    /// Both halves matter: on the same side of mid-grey it would be invisible on some sheet the
    /// toolbar offers, and a cursor no stronger than a rule is a cursor that cannot be found.
    #[test]
    fn a_cursor_reads_on_every_paper_and_beats_a_rule() {
        for swatch in PAPER_COLORS.iter() {
            let paper = relative_luminance(swatch.color);
            let cursor = relative_luminance(contrast_color(swatch.color));
            let rule = relative_luminance(rule_color(swatch.color));

            assert!(
                (cursor > 0.5) != (paper > 0.5),
                "the cursor on the {} paper is not across mid-grey from it",
                swatch.name
            );
            assert!(
                (cursor - paper).abs() > (rule - paper).abs(),
                "the cursor on the {} paper is no stronger than its ruling",
                swatch.name
            );
        }
    }

    /// Every label a chooser can show leads back to the choice it stood for.
    ///
    /// The toolbar's selects hold their options as the strings they draw, so a label is the *only*
    /// thing that comes back out of them: a size whose label did not resolve would leave the sheet
    /// unchanged while the box showed the new name, and two sizes sharing a label would resolve to
    /// whichever came first in the list rather than to the one that was picked.
    #[test]
    fn a_label_leads_back_to_its_choice() {
        let mut seen: Vec<&str> = Vec::new();

        for size in CanvasSize::ALL {
            assert!(
                !seen.contains(&size.label()),
                "two sizes are labelled {}",
                size.label()
            );
            seen.push(size.label());
            assert_eq!(CanvasSize::from_label(size.label()), Some(size));
        }

        for style in CanvasStyle::ALL {
            assert_eq!(CanvasStyle::from_label(style.label()), Some(style));
            assert!(
                CanvasSize::from_label(style.label()).is_none(),
                "a ruling is not a paper size: {}",
                style.label()
            );
        }

        assert_eq!(CanvasSize::from_label("A0"), None);
        assert_eq!(CanvasStyle::from_label("graph"), None);
        assert_eq!(CanvasSize::from_label(""), None);
    }

    /// Element ids address the element tree, so two swatches sharing one would silently share
    /// their hover and press state.
    #[test]
    fn every_control_has_its_own_element_id() {
        let mut ids: Vec<&str> = Vec::new();

        let controls = PAPER_COLORS
            .iter()
            .chain(INK_COLORS.iter())
            .map(|swatch| swatch.id);

        for id in controls {
            assert!(!ids.contains(&id), "duplicate element id {id}");
            ids.push(id);
        }

        assert_eq!(ids.len(), PAPER_COLORS.len() + INK_COLORS.len());
    }

    /// What building a sheet's ruling costs.
    ///
    /// A zoom rebuilds it — the ruling is printed on the paper, so it grows with the zoom — and
    /// this is the price of the gesture steps that move it enough to matter. A dot grid is the
    /// worst of the four styles: its quad count is the product of the rules on both axes rather
    /// than their sum.
    ///
    /// Run `cargo test --release -- --nocapture measures_the_ruling_costs`.
    #[test]
    fn measures_the_ruling_costs() {
        eprintln!("\n── ruling, measured ───────────────────────────────────────────");
        for zoom in [1.0f32, 2.0, 4.0] {
            let sheet = sheet(720.0 * zoom, 1_018.0 * zoom);

            for style in CanvasStyle::ALL {
                // Warm up, then measure what one zoom step pays.
                for _ in 0..8 {
                    std::hint::black_box(build_ruling(sheet, style, rule_on_white(), zoom));
                }

                let rounds = 20;
                let started = std::time::Instant::now();
                for _ in 0..rounds {
                    std::hint::black_box(build_ruling(sheet, style, rule_on_white(), zoom));
                }
                let built = started.elapsed() / rounds;
                let quads = build_ruling(sheet, style, rule_on_white(), zoom).len();

                eprintln!(
                    "  at {zoom:>3.0}x   {:<6} {:>5} quads   {:>8.1?}",
                    style.label(),
                    quads,
                    built
                );
            }
        }
        eprintln!("───────────────────────────────────────────────────────────────\n");
    }
}
