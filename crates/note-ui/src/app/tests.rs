// Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into
// scope and shadow the attribute this module needs.
use super::{
    bar_line, confirmed, file_label, file_stem, notch_in_pixels, page_of_write,
    page_owes_a_rewrite, palette_at, palette_centre, palette_colours, palette_face, palette_id,
    palette_in_hand, palette_point, parse_zoom, pdf_render_due, pinch_zoom_factor, quantise_width,
    sheet_matches, spun, wheel_pan, wheel_zoom_factor, zoom_text, Confirmation, PaletteGeometry,
    PalettePart, BAR_HEIGHT, PALETTE_DISC_BOX, PALETTE_HUB, PALETTE_HUB_RING, PALETTE_MARGIN,
    PALETTE_RADIUS, PALETTE_REACH, PALETTE_RING_RADIUS, PALETTE_TOOLS, PALETTE_TOOL_DISC,
    PALETTE_TOOL_RADIUS, PALETTE_WELL, PALETTE_WELL_PAD, PDF_QUIET_INTERVAL, WHEEL_LINES_PER_NOTCH,
    WHEEL_LINE_HEIGHT, WHEEL_ZOOM_STEP,
};
use crate::canvas::{HIGHLIGHTER_COLORS, INK_COLORS};
use crate::ink::{Notes, Tool};
use gpui_kit::ElementId;
use std::time::{Duration, Instant};

/// A destructive command is reachable only through an answer.
///
/// The whole of the rule the box exists for, checked here because it can be checked without a window: "yes" means
/// the command runs, and *anything else* — the refusing button, a click off the box, Escape — means nothing
/// happened at all (see [`confirmed`]). What this test cannot run is the command itself: that these two are only
/// ever run from `NoteApp::answer` is what this function being the only way in *means*.
#[test]
fn a_destructive_command_needs_an_answer() {
    for what in [Confirmation::ClearInk, Confirmation::DeletePage] {
        assert_eq!(confirmed(what, true), Some(what), "yes carries it out");
        assert_eq!(confirmed(what, false), None, "and no leaves the page alone");

        let words = what.words();

        assert!(
            words.heading.ends_with('?'),
            "the box asks rather than announces: {}",
            words.heading
        );
        assert!(
            words.detail.contains("cannot be taken back"),
            "and says what it costs, because the note cannot undo it: {}",
            words.detail
        );
        assert!(
            !words.agree.eq_ignore_ascii_case("ok") && !words.refuse.eq_ignore_ascii_case("cancel"),
            "the buttons name what they do rather than OK and Cancel: {} / {}",
            words.agree,
            words.refuse
        );
    }
}

/// A zoom is typed as a percentage, and only a percentage.
///
/// The field shows a whole percent, so a number typed into it means the same thing the field does — with or
/// without the sign — and the fraction the view takes is that number over a hundred. Everything else is refused
/// rather than guessed at: a zoom the view cannot be drawn at (zero, negative, not a number) and text that is not a
/// number at all would otherwise snap to the nearest limit or step, and a field that answers "150" with 5% is worse
/// than one that says it did not understand (see [`NoteApp::set_zoom_from_text`]).
#[test]
fn a_zoom_is_typed_as_a_percentage() {
    assert_eq!(parse_zoom("60"), Some(0.6));
    assert_eq!(parse_zoom("60%"), Some(0.6));
    assert_eq!(parse_zoom(" 150 % "), Some(1.5));
    assert_eq!(parse_zoom("100"), Some(1.0));
    assert_eq!(
        parse_zoom("0.5"),
        Some(0.005),
        "half a percent, if that is what was typed"
    );

    for refused in ["", "  ", "%", "wide", "60mm", "-50", "0", "nan", "inf"] {
        assert_eq!(parse_zoom(refused), None, "{refused:?} is not a zoom");
    }
}

/// What the field shows is what was typed, read back: the zoom as a whole percent, with the sign on it.
///
/// The two halves have to agree, because the field is filled from the view and read back into it: a number that did
/// not survive the round trip would move the zoom every time the field was refilled, once a frame.
#[test]
fn the_zoom_field_shows_a_whole_percentage() {
    assert_eq!(zoom_text(0.6), "60%");
    assert_eq!(zoom_text(1.0), "100%");
    assert_eq!(zoom_text(1.5), "150%");

    for zoom in [0.05_f32, 0.6, 0.75, 1.0, 1.25, 2.0, 16.0] {
        let shown = zoom_text(zoom);

        assert_eq!(
            parse_zoom(&shown),
            Some(zoom),
            "{zoom} came back as something else from {shown}"
        );
    }
}

/// One notch of a wheel is one zoom step, however many lines the platform reports it as.
#[test]
fn one_wheel_notch_is_one_zoom_step() {
    assert_eq!(
        wheel_zoom_factor(WHEEL_LINES_PER_NOTCH, false),
        WHEEL_ZOOM_STEP,
        "a notch is what a notch of a wheel reports"
    );
    assert_eq!(wheel_zoom_factor(0.0, false), 1.0);
    assert!(wheel_zoom_factor(-WHEEL_LINES_PER_NOTCH, false) < 1.0);
    assert!(
        (wheel_zoom_factor(-WHEEL_LINES_PER_NOTCH, false) * WHEEL_ZOOM_STEP - 1.0).abs() < 1e-6,
        "and undoes a notch of the other way"
    );
}

/// Two notches are the step applied twice rather than twice the step, so a fast roll zooms smoothly.
#[test]
fn two_notches_are_two_steps_and_not_a_doubled_one() {
    let twice = wheel_zoom_factor(WHEEL_LINES_PER_NOTCH * 2.0, false);

    assert!((twice - WHEEL_ZOOM_STEP * WHEEL_ZOOM_STEP).abs() < 1e-6);
    assert!(
        twice < WHEEL_ZOOM_STEP * 2.0,
        "a roll is not a multiplication"
    );
}

/// A trackpad reports pixels, and the same distance zooms the same either way it is reported:
#[test]
fn a_scroll_of_pixels_matches_a_scroll_of_lines() {
    let by_lines = wheel_zoom_factor(WHEEL_LINES_PER_NOTCH, false);
    let by_pixels = wheel_zoom_factor(notch_in_pixels(), true);

    assert!(
        (by_lines - by_pixels).abs() < 1e-6,
        "a notch of lines and a notch of pixels are the same gesture: {by_lines} vs {by_pixels}"
    );
    assert_eq!(by_lines, WHEEL_ZOOM_STEP, "and both are one step");
}

/// One enormous delta from a driver must not take the sheet from 100% to 1600%.
#[test]
fn a_wild_delta_is_clamped() {
    assert_eq!(wheel_zoom_factor(10_000.0, false), 5.0);
    assert_eq!(wheel_zoom_factor(-10_000.0, false), 0.2);
    assert_eq!(wheel_zoom_factor(f32::NAN, false), 1.0);
    assert_eq!(wheel_zoom_factor(f32::INFINITY, true), 1.0);
}

/// Panning is a distance in logical pixels, in the direction of the scroll.
#[test]
fn a_scroll_pans_in_the_direction_it_points() {
    assert_eq!(wheel_pan((0.0, 1.0), false), (0.0, WHEEL_LINE_HEIGHT));
    assert_eq!(
        wheel_pan((0.0, 3.0), true),
        (0.0, 3.0),
        "pixels are already pixels"
    );
    assert_eq!(wheel_pan((-1.0, 0.0), true), (-1.0, 0.0));
}

/// A pinch reports a fraction of the current size, and a nonsense one is refused.
#[test]
fn a_pinch_reports_a_fraction_of_the_size() {
    assert!(
        (pinch_zoom_factor(0.1) - 1.1).abs() < 1e-6,
        "ten percent closer"
    );
    assert!(
        (pinch_zoom_factor(-0.1) - 0.9).abs() < 1e-6,
        "and ten percent back"
    );
    assert_eq!(pinch_zoom_factor(0.0), 1.0, "three fingers held still");
    assert_eq!(pinch_zoom_factor(100.0), 5.0, "clamped");
    assert_eq!(pinch_zoom_factor(f32::NAN), 1.0, "refused");
}

/// A page's bitmap width is quantised onto the ladder, so a pixel of zoom does not throw away a
/// bitmap that cost milliseconds to make.
#[test]
fn a_page_is_rendered_on_the_ladder() {
    // The rungs themselves, and everything between them rounded up to the next one.
    assert_eq!(quantise_width(100.0), 1_024);
    assert_eq!(quantise_width(1_024.0), 1_024);
    assert_eq!(
        quantise_width(1_025.0),
        1_536,
        "just past a rung is the next rung"
    );
    assert_eq!(quantise_width(1_500.0), 1_536);
    assert_eq!(quantise_width(2_000.0), 2_304);
    assert_eq!(quantise_width(3_000.0), 3_456);

    // Past the top rung it stops growing: the page is magnified rather than resolved.
    assert_eq!(quantise_width(9_999.0), 3_456);

    // A window that reports nonsense still asks for something renderable.
    assert_eq!(quantise_width(0.0), 1_024);
    assert_eq!(quantise_width(-10.0), 1_024);
    assert_eq!(quantise_width(f32::NAN), 1_024);
}

/// The whole zoom range is covered by a handful of rasterisations, which is the point of the ladder.
#[test]
fn a_whole_pinch_needs_only_a_few_rasterisations() {
    let paper_width = 720.0;
    let mut widths = Vec::new();

    // 5% to 1600%, in the steps a gesture would actually produce.
    let mut zoom = 0.05f32;
    while zoom <= 16.0 {
        let width = quantise_width(paper_width * zoom * 2.0);
        if widths.last() != Some(&width) {
            widths.push(width);
        }
        zoom *= 1.25f32.powf(0.1);
    }

    assert_eq!(
        widths,
        vec![1_024, 1_536, 2_304, 3_456],
        "one rasterisation per rung, and no more"
    );
}

/// A note written on the same sheet is not reported as a different one.
#[test]
fn a_note_on_the_same_sheet_matches() {
    let sheet = (794.0, 1123.0);

    assert!(sheet_matches(sheet, sheet), "the same numbers");
    assert!(
        sheet_matches(sheet, (794.4, 1122.6)),
        "a rounding step through JSON is not a different sheet"
    );
    assert!(!sheet_matches(sheet, (595.0, 842.0)), "A5 is not A4");
    assert!(
        !sheet_matches(sheet, (794.0, 1123.0 + 8.0)),
        "a taller sheet is a different sheet"
    );
}

/// A message about a file names the file, not its whole path.
#[test]
fn a_file_is_labelled_by_its_name() {
    // A Windows path is only a path on Windows: on the Mac a backslash is an ordinary character, and
    // the whole string is the name — which is the platform's answer, not a bug in `file_label`.
    #[cfg(windows)]
    assert_eq!(
        file_label(std::path::Path::new(r"C:\notes\chapter-3.zip")),
        "chapter-3.zip"
    );
    assert_eq!(file_label(std::path::Path::new("note.zip")), "note.zip");
}

/// A name a person gave a note becomes a file name, with what Windows refuses turned into dashes.
#[test]
fn a_note_s_name_becomes_a_file_name() {
    assert_eq!(
        file_stem("3\u{c7a5} \u{c694}\u{c57d}"),
        "3\u{c7a5} \u{c694}\u{c57d}"
    );
    assert_eq!(file_stem("  chapter   3  "), "chapter 3");
    assert_eq!(file_stem("a/b\\c:d*e?f\"g<h>i|j"), "a-b-c-d-e-f-g-h-i-j");
    assert_eq!(file_stem("report."), "report", "a trailing dot is dropped");
    assert_eq!(
        file_stem("..."),
        "",
        "and a name that was only dots is nothing"
    );
    assert_eq!(
        file_stem("\u{2026}"),
        "\u{2026}",
        "a character a file system is happy with is left alone"
    );
    assert_eq!(file_stem(""), "");
    assert_eq!(
        file_stem(&"\u{c7a5}".repeat(80)).chars().count(),
        64,
        "a long name is cut by characters, so a Korean one is not halved"
    );
}

/// A page is rasterised when the pen stops, not while it is writing.
#[test]
fn a_page_render_waits_for_the_pen_to_stop() {
    let now = Instant::now();

    assert!(!pdf_render_due(now, now), "the pen has just laid ink");
    assert!(
        !pdf_render_due(now, now + PDF_QUIET_INTERVAL - Duration::from_millis(1)),
        "a pause inside a word is not the end of writing"
    );
    assert!(
        pdf_render_due(now, now + PDF_QUIET_INTERVAL),
        "and a hand that has stopped is one to spend a render on"
    );
    assert!(
        !pdf_render_due(now + Duration::from_secs(1), now),
        "a clock that goes backwards must not force a render either"
    );
}

/// A write belongs to the page the *ink* is on, not to the index the view happens to be at.
///
/// The two are the same number everywhere except in the middle of a page-list move, and that one
/// moment is the whole of the bug this pins down: `add_page` closes the page being left, moves
/// the list, and only then turns, so a write that read the view's index put the page being left
/// onto the sheet of the page that took its place — ink the note kept, which is why it came back
/// the next time the note was opened.
#[test]
fn a_write_belongs_to_the_page_the_ink_is_on() {
    let mut ink = Notes::new();
    ink.go_to(2);
    assert_eq!(page_of_write(&ink), 2, "the page in hand");

    // A page put in before this one, which is the move `add_page` makes: the pages move, and the
    // ink moves with the page it was written on rather than staying at the number it was at.
    ink.insert_at(2);
    assert_eq!(
        page_of_write(&ink),
        3,
        "and the write follows the ink, which is now page 3"
    );
}

/// A page is written out again when an append cannot say what happened to it.
///
/// Three cases, and the second is the one the app cannot see by counting: a page that has only grown is a
/// longer version of what the note holds (an append says it); a page that has *lost* strokes is not; and a
/// page whose strokes have been **moved** is not either, because a lasso drag changes every point it holds
/// and leaves the count exactly where it was.
#[test]
fn a_page_is_written_out_again_when_an_append_cannot_say_what_changed() {
    // Grown: ten strokes on disk and twelve in memory — an append.
    assert!(!page_owes_a_rewrite(12, 10, 0, 0));
    // Nothing at all: not even a batch worth sending.
    assert!(!page_owes_a_rewrite(10, 10, 0, 0));
    // Undone, erased, or a selection deleted: fewer strokes than the note holds.
    assert!(page_owes_a_rewrite(9, 10, 0, 0));
    // Moved since it was last written: the same strokes, and not the ones on disk.
    assert!(page_owes_a_rewrite(10, 10, 1, 0));
    assert!(
        !page_owes_a_rewrite(10, 10, 1, 1),
        "and not twice for one move"
    );
    assert!(page_owes_a_rewrite(10, 10, 2, 1));
}

/// The palette's ring is a clock face: a table's first colour at the top, and the table's own order
/// going clockwise — which is what makes a palette *learnt* rather than hunted for.
#[test]
fn the_ring_reads_like_a_clock() {
    let middle = [200.0, 200.0];
    let near = |at: [f32; 2], want: [f32; 2]| {
        (at[0] - want[0]).abs() < 0.01 && (at[1] - want[1]).abs() < 0.01
    };
    let at = |place| palette_point(middle, 100.0, place, 4);

    // Twelve, three, six and nine o'clock — with `y` growing *down*, so three o'clock is to the right.
    assert!(near(at(0), [200.0, 100.0]), "{:?}", at(0));
    assert!(near(at(1), [300.0, 200.0]), "{:?}", at(1));
    assert!(near(at(2), [200.0, 300.0]), "{:?}", at(2));
    assert!(near(at(3), [100.0, 200.0]), "{:?}", at(3));

    // A ring of twelve keeps the same reading: the fourth place is a quarter turn on, whatever the
    // count, because the step is the count's and the *reading* is the clock's.
    assert!(near(palette_point(middle, 100.0, 3, 12), [300.0, 200.0]));
}

/// The palette's geometry at the unit its design was written in: the numbers the palette's tests
/// below were tuned at. The platform's own unit is what
/// [`a_smaller_unit_scales_the_ring_and_the_reach_of_a_tap`] is about.
fn design_palette() -> PaletteGeometry {
    PaletteGeometry::of(1.0)
}

/// A tap belongs to the control it landed on, and to none of the ones it did not.
///
/// The gaps are the whole of what makes a ring pressable with a pen, and the reach is what they are
/// wide enough to clear (see [`PALETTE_REACH`]) — the next test is that property, and this one is what
/// it buys: every control reachable, and the space between two of them, or the middle, reaching
/// nothing at all.
#[test]
fn a_tap_takes_what_it_landed_on() {
    let middle = [400.0, 400.0];
    let colours = INK_COLORS.len();
    let geometry = design_palette();

    for place in 0..colours {
        let at = palette_point(middle, PALETTE_RING_RADIUS, place, colours);

        assert_eq!(
            palette_at(geometry, middle, at, colours),
            Some(PalettePart::Colour(place)),
            "place {place} of the ring"
        );
    }

    for place in 0..PALETTE_TOOLS.len() {
        let at = palette_point(middle, PALETTE_TOOL_RADIUS, place, PALETTE_TOOLS.len());

        assert_eq!(
            palette_at(geometry, middle, at, colours),
            Some(PalettePart::Tool(place)),
            "tool {place} of the inner ring"
        );
    }

    // The middle of the palette is the colour in hand, which is a *readout* rather than a control: a
    // tap there is the palette's own background, and putting the palette away is what that means.
    assert_eq!(palette_at(geometry, middle, middle, colours), None);

    // And halfway between the first colour and the second — the closest two places on a ring of twelve
    // get to each other — is neither of them.
    let here = palette_point(middle, PALETTE_RING_RADIUS, 0, colours);
    let there = palette_point(middle, PALETTE_RING_RADIUS, 1, colours);
    let between = [(here[0] + there[0]) / 2.0, (here[1] + there[1]) / 2.0];

    assert_eq!(
        palette_at(geometry, middle, between, colours),
        None,
        "a tap between two colours takes neither"
    );
}

/// No two controls are inside one reach of the same point: the property [`PALETTE_REACH`] claims, and
/// the one thing that makes a ring answered by distance workable at all.
#[test]
fn no_two_controls_are_within_one_reach_of_a_point() {
    let colours = INK_COLORS.len();
    let mut places: Vec<[f32; 2]> = (0..colours)
        .map(|place| palette_point([0.0, 0.0], PALETTE_RING_RADIUS, place, colours))
        .collect();

    places.extend(
        (0..PALETTE_TOOLS.len()).map(|place| {
            palette_point([0.0, 0.0], PALETTE_TOOL_RADIUS, place, PALETTE_TOOLS.len())
        }),
    );

    for (one, here) in places.iter().enumerate() {
        for there in places.iter().skip(one + 1) {
            let apart = ((here[0] - there[0]).powi(2) + (here[1] - there[1]).powi(2)).sqrt();

            assert!(
                apart > PALETTE_REACH * 2.0,
                "{here:?} and {there:?} are {apart} apart: inside two reaches"
            );
        }
    }
}

/// The palette's zones stay the shape they were drawn as: the tools inside the well and the well clear
/// of the ring of colours, the hub clear of the tools, and the colours clear of the palette's own edge.
///
/// This pins the *drawing* rather than any behaviour — the reach test above is what holds the taps up —
/// and every one of these numbers was chosen for how the palette looks. They are the relationships that
/// make it read as two zones rather than as thirteen discs on a field, and they are exactly what a
/// change made for another reason would break by accident: a disc widened by four pixels, a hub by ten,
/// and one thing is growing into the next.
#[test]
fn the_palettes_zones_do_not_grow_into_each_other() {
    let (ring, disc, tool_ring, tool, well, hub) = (
        PALETTE_RING_RADIUS,
        PALETTE_DISC_BOX / 2.0,
        PALETTE_TOOL_RADIUS,
        PALETTE_TOOL_DISC / 2.0,
        PALETTE_WELL / 2.0,
        PALETTE_HUB / 2.0,
    );

    // The ring of colours is inside the palette, with room for the shadow the palette casts and for
    // the white a disc's box is placed with: a ring of discs tangent to the rim is a ring of discs that
    // looks drawn by accident (which is what this was before the white was added).
    assert!(
        ring + disc + PALETTE_MARGIN <= PALETTE_RADIUS,
        "the discs reach the palette's own edge"
    );

    // The well holds the chips with the gap it was given, and stops short of the colours: a disc drawn
    // on the well's rim would be a colour in the tools' zone.
    assert!(
        tool_ring + tool + PALETTE_WELL_PAD <= well,
        "a tool chip is not inside the well"
    );
    assert!(well <= ring - disc, "the well reaches the ring of colours");

    // And the hub is the middle of the middle: inside the well, and clear of the tools around it — the
    // ring of white it is drawn in counted, because that ring is part of what it is.
    assert!(
        hub + PALETTE_HUB_RING <= tool_ring - tool,
        "the hub and the tools are not clear of each other"
    );
}

/// The keyboard walks the ring and wraps at both ends: a ring has no end, so no arrow is a key that
/// does nothing.
#[test]
fn the_keyboard_walks_the_ring_and_wraps() {
    assert_eq!(spun(0, 1, 12), 1);
    assert_eq!(spun(0, -1, 12), 11);
    assert_eq!(spun(11, 1, 12), 0);
    assert_eq!(spun(3, -13, 4), 2, "more than a lap is still a place");
    assert_eq!(spun(5, 0, 12), 5, "and no step leaves it where it was");
    assert_eq!(
        spun(9, 1, 0),
        0,
        "a ring with nothing on it has nowhere to walk"
    );
}

/// The ring is the palette of the tool in hand — and the *pen's* for the tools that have no palette
/// of their own, because a colour is what a pen writes with (see [`palette_colours`]).
#[test]
fn the_palette_follows_the_tool_in_hand() {
    let ring = |tool| {
        palette_colours(tool)
            .iter()
            .map(|swatch| swatch.color)
            .collect::<Vec<u32>>()
    };
    let inks: Vec<u32> = INK_COLORS.iter().map(|swatch| swatch.color).collect();
    let markers: Vec<u32> = HIGHLIGHTER_COLORS
        .iter()
        .map(|swatch| swatch.color)
        .collect();

    assert_eq!(ring(Tool::Pen), inks, "the pen draws with the pen's table");
    assert_eq!(
        ring(Tool::Highlighter),
        markers,
        "the marker with the marker's"
    );
    assert_eq!(
        ring(Tool::Eraser),
        inks,
        "and an eraser is handed the pen's"
    );
    assert_eq!(ring(Tool::Lasso), inks);
}

/// A chooser opens on the value it holds: the ring's own place starts on the colour in hand, and
/// falls back to the first place for a palette that does not hold it (see [`palette_in_hand`]).
#[test]
fn the_ring_starts_on_the_colour_in_hand() {
    let inks = palette_colours(Tool::Pen);

    for (place, swatch) in INK_COLORS.iter().enumerate() {
        assert_eq!(
            palette_in_hand(inks, swatch.color),
            place,
            "{}",
            swatch.name
        );
    }

    // The marker's yellow is not one of the pen's colours, so the pen's ring has no place for it.
    assert_eq!(palette_in_hand(inks, HIGHLIGHTER_COLORS[0].color), 0);
}

/// The inner ring is the pen, the marker and the eraser — and not the lasso, which is a sweep rather
/// than a press (see [`PALETTE_TOOLS`]).
#[test]
fn the_ring_offers_three_tools_and_not_the_lasso() {
    assert_eq!(PALETTE_TOOLS.len(), 3);

    for tool in PALETTE_TOOLS {
        let (id, _, label) = palette_face(tool).expect("a face for every tool on the ring");

        assert!(
            label.len() > 1,
            "a button a person cannot read is not offered"
        );
        assert!(
            id.starts_with("palette-"),
            "the ring's ids are its own, so they cannot collide with the bar's: {id}"
        );
    }

    assert_eq!(
        palette_face(Tool::Lasso),
        None,
        "the lasso stays on the bar, where a sweep belongs"
    );
}

/// The palette is kept on the screen wherever the pen is: a ring at the window's edge is moved in
/// rather than drawn half off it, and a pen in the middle is not moved at all.
#[test]
fn the_palette_is_kept_on_the_screen() {
    let window = [1_280.0, 900.0];
    let geometry = design_palette();
    let least = PALETTE_RADIUS + PALETTE_MARGIN;

    for near in [
        [0.0, 0.0],
        [1_280.0, 0.0],
        [0.0, 900.0],
        [1_280.0, 900.0],
        [640.0, 450.0],
    ] {
        let at = palette_centre(geometry, near, window);

        assert!(at[0] >= least, "{near:?} -> {at:?}");
        assert!(at[0] <= window[0] - least, "{near:?} -> {at:?}");
        assert!(at[1] >= least, "{near:?} -> {at:?}");
        assert!(at[1] <= window[1] - least, "{near:?} -> {at:?}");
    }

    // A pen in the middle is where the hand is, and the clamp is there for the edges rather than for
    // every tap: the palette arrives under the nib it was opened at.
    assert_eq!(
        palette_centre(geometry, [640.0, 450.0], window),
        [640.0, 450.0]
    );

    // A window too small to hold the palette puts it in the middle rather than somewhere off it.
    assert_eq!(
        palette_centre(geometry, [0.0, 0.0], [100.0, 100.0]),
        [50.0, 50.0]
    );
}

/// A platform's unit changes every length, and the reach of a tap with them.
///
/// The design is written in 96-DPI logical pixels and macOS measures in points, four thirds of one,
/// so the palette there is drawn at three quarters of the design's size. What has to move with the
/// drawing is the *answer to a tap*: a ring drawn smaller and hit-tested at the design's reach would
/// take presses outside its discs, and one drawn at three quarters with a reach that stayed behind
/// would refuse the edge of its own. Both are the same property at two scales, which is what this
/// pins.
#[test]
fn a_smaller_unit_scales_the_ring_and_the_reach_of_a_tap() {
    let centre = [300.0, 300.0];
    let colours = INK_COLORS.len();

    for unit in [1.0f32, 0.75] {
        let geometry = PaletteGeometry::of(unit);
        let on = palette_point(centre, geometry.ring(), 4, colours);

        assert_eq!(
            palette_at(geometry, centre, on, colours),
            Some(PalettePart::Colour(4)),
            "the point on the ring is the colour, at a unit of {unit}"
        );

        // Further out than the reach allows — on the same spoke, so the nearest control is the
        // colour — is not that colour any more. Had the reach stayed at the design's number, this
        // would still be inside it at 0.75.
        let away = palette_point(centre, geometry.ring() + geometry.reach() * 1.5, 4, colours);

        assert_ne!(
            palette_at(geometry, centre, away, colours),
            Some(PalettePart::Colour(4)),
            "a tap past the reach is not the colour, at a unit of {unit}"
        );
    }

    // A unit a platform cannot mean is the design's own, rather than a palette of no size at all.
    assert_eq!(PaletteGeometry::of(0.0), design_palette());
    assert_eq!(PaletteGeometry::of(f32::NAN), design_palette());
}

/// A bar that is not there has no line, and a bar that is there has the edge a frame measured.
///
/// The two answers this pins are the two ways the line used to be wrong. A hidden bar kept the
/// estimate, so the strip its own switch sat in stayed "interface": the ghost was not drawn there
/// and the system arrow came out under the hand that had just hidden the bar. And a bar that
/// wrapped taller than the estimate would have let the ghost be drawn across the bar itself. The
/// measured edge is the one line both answers need — and the estimate is only what the frame
/// before the first measurement falls back to.
#[test]
fn a_hidden_bar_has_no_line_and_a_shown_one_has_the_measured_edge() {
    assert_eq!(
        bar_line(false, Some(200.0)),
        0.0,
        "a hidden bar is page, even where its switch was"
    );
    assert_eq!(
        bar_line(false, None),
        0.0,
        "and it is page before it was ever measured"
    );
    assert_eq!(
        bar_line(true, Some(133.0)),
        133.0,
        "the edge the frame measured wins over the estimate"
    );
    assert_eq!(
        bar_line(true, None),
        BAR_HEIGHT,
        "the estimate is only the frame before the first measurement"
    );
    assert_eq!(
        bar_line(true, Some(0.0)),
        BAR_HEIGHT,
        "a measurement of zero is no measurement: a bar that is up has some height"
    );
}

/// Element ids address the whole tree, and the bar's swatches and the palette's discs are drawn at
/// the same moment — the palette floats over the bar — so the two sets of ids cannot be the same ids.
#[test]
fn the_palettes_discs_are_not_the_bars_swatches() {
    for swatch in INK_COLORS.iter().chain(HIGHLIGHTER_COLORS.iter()) {
        assert_ne!(
            palette_id(swatch),
            ElementId::from(swatch.id),
            "the palette's disc for the {} colour would share the bar's swatch id",
            swatch.name
        );
    }
}
