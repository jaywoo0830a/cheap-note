// Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into
// scope and shadow the attribute this module needs.
use super::{
    build_ruling, contrast_color, place, relative_luminance, rule_color, rules_that_fit,
    CanvasSize, CanvasStyle, Rect, Ruling, RulingSheet, HIGHLIGHTER_COLORS, INK_COLORS,
    PAPER_COLORS,
};
use gpui_kit::{point, px, rgb, size, Bounds, Hsla, Pixels};
use std::sync::Arc;

use crate::ink_layer::canvas::Fill;

/// The highlighter's palette is paler than the pen's, and its own.
///
/// The rule the four colours are chosen by: a highlighter is read *through*, so every one of them has to be a
/// colour there is something to read behind — and a palette of its own is what keeps a reader from having to
/// pick a pen colour to get the marker they meant.
#[test]
fn the_highlighter_palette_is_pale_and_its_own() {
    for swatch in HIGHLIGHTER_COLORS.iter() {
        assert!(
            relative_luminance(swatch.color) > 0.5,
            "the {} highlighter is too dark to read through",
            swatch.name
        );
        assert!(
            !INK_COLORS.iter().any(|pen| pen.color == swatch.color),
            "the {} highlighter is one of the pen's colours",
            swatch.name
        );
    }
}

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

/// A ruling request for a sheet the right way up: the paper and the rectangle it is drawn in are the
/// same shape, which is what a page nobody has turned is.
fn upright(paper: Bounds<Pixels>) -> RulingSheet {
    RulingSheet {
        drawn: Rect {
            x: 24.0,
            y: 24.0,
            width: f32::from(paper.size.width),
            height: f32::from(paper.size.height),
        },
        paper,
        turns: 0,
    }
}

/// The rectangle of a ruling mark, as plain floats.
fn fill_bounds(fill: &Fill) -> (f32, f32, f32, f32) {
    (fill.rect.x, fill.rect.y, fill.rect.width, fill.rect.height)
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
    assert!(
        CanvasSize::Wide.aspect() < 1.0,
        "the wide sheet is landscape"
    );

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
        for mark in build_ruling(sheet, style, rule_on_white(), 1.0) {
            let (x, y, width, height) = fill_bounds(&mark);
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

    let first = ruling.rules(upright(sheet), CanvasStyle::Grid, 0xFF_FF_FF, 1.0);
    let again = ruling.rules(upright(sheet), CanvasStyle::Grid, 0xFF_FF_FF, 1.0);
    assert!(
        Arc::ptr_eq(&first, &again),
        "an unchanged sheet is not rebuilt"
    );

    let restyled = ruling.rules(upright(sheet), CanvasStyle::Dots, 0xFF_FF_FF, 1.0);
    assert!(!Arc::ptr_eq(&first, &restyled), "a new style is rebuilt");

    let repapered = ruling.rules(upright(sheet), CanvasStyle::Dots, 0x14_16_1A, 1.0);
    assert!(
        !Arc::ptr_eq(&restyled, &repapered),
        "new paper changes the rule's colour, so it is rebuilt"
    );

    // A page turned is a different ruling, even when the paper and the drawn rectangle are the
    // shapes of a page that has not been: the rules run the other way. (A square, because this test's
    // own `sheet` is the sheet it drew with — the helper has been shadowed.)
    let square = Bounds {
        origin: point(px(24.0), px(24.0)),
        size: size(px(720.0), px(720.0)),
    };
    let turned = RulingSheet {
        turns: 1,
        ..upright(square)
    };
    let upright_ruling = ruling.rules(upright(square), CanvasStyle::Dots, 0x14_16_1A, 1.0);
    let turned_ruling = ruling.rules(turned, CanvasStyle::Dots, 0x14_16_1A, 1.0);
    assert!(
        !Arc::ptr_eq(&upright_ruling, &turned_ruling),
        "a turn is a rebuild"
    );
}

/// The ruling turns with the page: a rule that runs across the paper runs down it once the page is on
/// its side, inside the rectangle the sheet is drawn in.
#[test]
fn the_ruling_turns_with_the_page() {
    let paper = sheet(720.0, 1018.0);
    let rule = vec![Fill::new(
        Rect {
            x: 30.0,
            y: 40.0,
            width: 200.0,
            height: 2.0,
        },
        rgb(0x00_00_00).into(),
    )];

    // The rectangle the sheet is drawn in, for a page at `turns`: the paper's shape, or that shape on
    // its side. Written out per turn because it is part of the input and not something the mapping
    // can be asked to guess — a page and its drawn rectangle always agree about their shape.
    let drawn = |turns: u8| Rect {
        x: 10.0,
        y: 20.0,
        width: if turns % 2 == 1 { 1018.0 } else { 720.0 },
        height: if turns % 2 == 1 { 720.0 } else { 1018.0 },
    };

    let once = place(
        rule.clone(),
        RulingSheet {
            paper,
            drawn: drawn(1),
            turns: 1,
        },
    );
    assert_eq!(
        fill_bounds(&once[0]),
        (986.0, 50.0, 2.0, 200.0),
        "the rule lies the other way, inside the drawn rectangle"
    );

    // Every turn places the mark inside the sheet that is drawn, which is the whole of what the
    // mapping has to get right: a ruling outside its paper is a grid on the desk instead.
    for turns in 0..4u8 {
        let at = RulingSheet {
            paper,
            drawn: drawn(turns),
            turns,
        };
        let (x, y, width, height) = fill_bounds(&place(rule.clone(), at)[0]);
        let rect = drawn(turns);

        assert!(
            x >= rect.x
                && y >= rect.y
                && x + width <= rect.x + rect.width
                && y + height <= rect.y + rect.height,
            "turn {turns} put the rule at {x}, {y} ({width}x{height})"
        );
    }

    // And the whole way round is where it started.
    let round = place(
        rule.clone(),
        RulingSheet {
            paper,
            drawn: drawn(4),
            turns: 4,
        },
    );
    assert_eq!(
        fill_bounds(&round[0]),
        (40.0, 60.0, 200.0, 2.0),
        "four turns is the page it was"
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
        .chain(HIGHLIGHTER_COLORS.iter())
        .map(|swatch| swatch.id);

    for id in controls {
        assert!(!ids.contains(&id), "duplicate element id {id}");
        ids.push(id);
    }

    assert_eq!(
        ids.len(),
        PAPER_COLORS.len() + INK_COLORS.len() + HIGHLIGHTER_COLORS.len()
    );
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
