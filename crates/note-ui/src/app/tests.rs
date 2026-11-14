// Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into
// scope and shadow the attribute this module needs.
use super::{
    bar_line, confirmed, file_label, file_stem, held_by_cap, page_after_insert, page_of_write,
    page_owes_a_rewrite, palette_at, palette_centre, palette_colours, palette_face, palette_id,
    palette_in_hand, palette_point, parse_zoom, pdf_render_due, pen_cursor_shows,
    pinch_zoom_factor, quantise_width,
    samples_vitals, scroll_of, sheet_matches, spun, touches_paper, zoom_text, Confirmation, Edge,
    EdgeSwipe, FootPills, FootPlace, FootStrips, PaletteGeometry, PalettePart, PenGesture, Swipe,
    Turn, BAR_HEIGHT, EDGE_SWIPE_BAND, EDGE_SWIPE_TRAVEL, PALETTE_DISC_BOX, PALETTE_HUB,
    PALETTE_HUB_RING, PALETTE_MARGIN, PALETTE_RADIUS, PALETTE_REACH, PALETTE_RING_RADIUS,
    PALETTE_TOOLS, PALETTE_TOOL_DISC, PALETTE_TOOL_RADIUS, PALETTE_WELL, PALETTE_WELL_PAD,
    PDF_QUIET_INTERVAL, WHEEL_PAN_PER_LINE,
};
use crate::canvas::{HIGHLIGHTER_COLORS, INK_COLORS};
use crate::ink::{Notes, Tool};
use crate::pages::Pages;
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

/// The band is where a gesture *began*, and a gesture that began over the page is never a page turn.
#[test]
fn an_edge_is_a_side_of_the_window() {
    let wide = 1_600.0;

    assert_eq!(Edge::at(0.0, wide), Some(Edge::Left), "the very rim");
    assert_eq!(
        Edge::at(EDGE_SWIPE_BAND - 1.0, wide),
        Some(Edge::Left),
        "just inside the band"
    );
    assert_eq!(
        Edge::at(EDGE_SWIPE_BAND, wide),
        None,
        "the band is open at its inner edge: a gesture that began here is a pan"
    );
    assert_eq!(Edge::at(wide / 2.0, wide), None, "the middle of the page");
    assert_eq!(
        Edge::at(wide - EDGE_SWIPE_BAND + 1.0, wide),
        Some(Edge::Right),
        "the right band"
    );
    assert_eq!(Edge::at(wide, wide), Some(Edge::Right), "the very rim");

    // A window with no width has no sides, and a position that is not a number is not on one.
    assert_eq!(Edge::at(0.0, 0.0), None);
    assert_eq!(Edge::at(f32::NAN, wide), None);
}

/// The two gestures the app is built around: rightwards from the left edge is back, leftwards from the right
/// edge is forward.
#[test]
fn a_swipe_away_from_its_edge_turns_the_way_it_points() {
    let mut back = EdgeSwipe::new(Edge::Left);
    back.moved((EDGE_SWIPE_TRAVEL, 0.0));
    assert_eq!(back.decided(), Swipe::Turn(Turn::Previous));

    let mut forward = EdgeSwipe::new(Edge::Right);
    forward.moved((-EDGE_SWIPE_TRAVEL, 0.0));
    assert_eq!(forward.decided(), Swipe::Turn(Turn::Next));
}

/// A swipe that pushes *into* the page from the rim is a pan, and the sign is what says so.
#[test]
fn a_swipe_back_towards_its_edge_is_no_turn() {
    let mut pressed = EdgeSwipe::new(Edge::Left);
    pressed.moved((-EDGE_SWIPE_TRAVEL * 2.0, 0.0));
    assert_eq!(pressed.decided(), Swipe::None, "left, from the left edge");

    let mut pressed = EdgeSwipe::new(Edge::Right);
    pressed.moved((EDGE_SWIPE_TRAVEL * 2.0, 0.0));
    assert_eq!(pressed.decided(), Swipe::None, "right, from the right edge");
}

/// The travel is accumulated across readings, and a gesture that goes out and comes back does not add up to one.
#[test]
fn a_swipe_is_the_travel_of_the_whole_gesture() {
    // A trackpad sends a swipe as many small deltas; none of them alone is a swipe.
    let mut swipe = EdgeSwipe::new(Edge::Left);
    let steps = (EDGE_SWIPE_TRAVEL as i32 / 5) - 1;
    for _ in 0..steps {
        swipe.moved((5.0, 0.0));
        assert_eq!(swipe.decided(), Swipe::None, "not yet");
    }
    swipe.moved((EDGE_SWIPE_TRAVEL - 5.0 * steps as f32, 0.0));
    assert_eq!(
        swipe.decided(),
        Swipe::Turn(Turn::Previous),
        "and now it is"
    );

    // Out and back is not a swipe: the signed travel comes back to nothing.
    let mut wandered = EdgeSwipe::new(Edge::Left);
    wandered.moved((EDGE_SWIPE_TRAVEL, 0.0));
    wandered.moved((-EDGE_SWIPE_TRAVEL, 0.0));
    assert_eq!(wandered.decided(), Swipe::None, "it ended where it began");
}

/// A reader scrolling down the page with a thumb near the edge is the accident the slope rules out.
#[test]
fn a_mostly_vertical_gesture_is_a_scroll_and_not_a_swipe() {
    let mut scroll = EdgeSwipe::new(Edge::Left);

    // Sideways enough to pass the travel, but twice as much of it downwards.
    scroll.moved((EDGE_SWIPE_TRAVEL, -EDGE_SWIPE_TRAVEL));
    assert_eq!(scroll.decided(), Swipe::None, "two to one is not a swipe");

    // And a diagonally-flicked swipe still counts: three to one is.
    let mut flick = EdgeSwipe::new(Edge::Left);
    flick.moved((EDGE_SWIPE_TRAVEL * 3.0, -EDGE_SWIPE_TRAVEL));
    assert_eq!(flick.decided(), Swipe::Turn(Turn::Previous));
}

/// One gesture turns one page. A fast flick keeps sending readings after the page has turned, and following
/// them would run through four pages on one swipe.
#[test]
fn a_swipe_that_has_turned_a_page_turns_no_more() {
    let mut swipe = EdgeSwipe::new(Edge::Left);
    swipe.moved((EDGE_SWIPE_TRAVEL, 0.0));
    assert_eq!(swipe.decided(), Swipe::Turn(Turn::Previous));

    swipe.turned();
    assert_eq!(swipe.decided(), Swipe::None, "the same gesture, again");

    // However much further the hand carries on.
    swipe.moved((EDGE_SWIPE_TRAVEL * 4.0, 0.0));
    assert_eq!(swipe.decided(), Swipe::None, "no matter how far it goes");
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

/// A page added **before** this one leaves the reader where they were; one added **after** is turned to.
///
/// The two halves of `NoteApp::add_page`, checked at the layer they share: the page list moves, the ink
/// moves with the page it was written on, and the page carried in hand moves with it. "Before" is the one
/// that must not become a turn — the reader is on a page, needs a sheet in front of it, and means to stay
/// on the page they were reading — so what the command does afterwards is only *follow* the page.
#[test]
fn a_page_added_before_this_one_leaves_the_reader_where_they_were() {
    let mut pages = Pages::of_document(3);
    let mut ink = Notes::new();
    ink.go_to(1);

    // "Before" from page 1: the blank page takes position 1, so the page being read is now page 2. The
    // list and the ink each make the same move, and the view is told to *stay* on the page it was on.
    let at = pages.insert(1, true);
    assert_eq!(at, 1, "the blank page took the place in front");
    assert_eq!(
        page_after_insert(at, true),
        Some(2),
        "the reader stays, on the page the list has renamed for them"
    );

    ink.insert_at(at);
    assert_eq!(
        page_of_write(&ink),
        2,
        "and the ink followed its own page, as it always does"
    );

    // "After" from the same place: the blank page lands at 2, and the command turns to it — the page
    // made is the page about to be written on.
    let at = pages.insert(1, false);
    assert_eq!(at, 2, "the blank page took the place behind");
    assert_eq!(
        page_after_insert(at, false),
        None,
        "so the command turns to the page it made"
    );
}

/// The pills at the foot of the desk are the only part of the row the pen is refused in.
///
/// The row is full-width and the pills float in it, so the desk beside them is still desk: a reader
/// may be writing on paper behind the gap between the two pills, and refusing the whole row would take
/// that away. Each strip is the pill's own measured box grown by `FOOT_SLACK`.
#[test]
fn the_foot_refuses_the_pills_and_not_the_row() {
    // The page pill measured at 400..600 and the zoom pill at 700..900, with 12 px of slack either side.
    let strips = FootPills::default()
        .measured(FootPlace::Page, 400.0, 600.0, 930.0)
        .measured(FootPlace::Zoom, 700.0, 900.0, 910.0)
        .strips();

    let held = |x: f32, y: f32| strips.held((x, y), 900.0);

    assert!(held(500.0, 950.0), "the middle of the page pill");
    assert!(held(400.0, 950.0), "its left edge");
    assert!(
        held(388.0, 950.0),
        "and the slack beside it, a press that missed"
    );
    assert!(!held(300.0, 950.0), "but not the desk further out");

    assert!(held(712.0, 950.0), "the slack before the zoom pill");
    assert!(held(900.0, 950.0), "its right edge");
    assert!(!held(930.0, 950.0), "and not past the slack after it");

    assert!(
        !held(500.0, 899.0),
        "above the line the row begins at is page, whatever the x is"
    );

    // An unmeasured pill is no strip at all: the pen is refused nowhere it cannot see a pill.
    assert!(!FootPills::default().strips().held((500.0, 950.0), 900.0));
}

/// Each pill's measured box becomes its strip, and the taller pill's top is the line — because a
/// reading over either pill is a press.
#[test]
fn the_foot_takes_the_tallest_pill_as_its_line() {
    let measured = FootPills::default()
        .measured(FootPlace::Page, 400.0, 600.0, 930.0)
        .measured(FootPlace::Zoom, 700.0, 900.0, 910.0);

    assert_eq!(measured.page, Some((400.0, 600.0)), "the page pill's box");
    assert_eq!(measured.zoom, Some((700.0, 900.0)), "and the zoom pill's");
    assert_eq!(
        measured.top,
        Some(910.0),
        "the taller pill is where the row really begins"
    );

    let strips = measured.strips();
    assert_eq!(
        strips.page,
        Some((388.0, 612.0)),
        "the strip is the box grown by the slack"
    );
    assert_eq!(strips.zoom, Some((688.0, 912.0)));
}

/// The system pointer is the cursor over the bar and over the desk's pills; the ghost is the pen's
/// everywhere else on the page.
///
/// The half that is easy to get wrong is the *transition*: a pen that was writing a moment ago has a
/// cursor of its own, and one that moves up onto the bar — or down onto a pill — must give the arrow
/// back on that very reading. A platform that restores the arrow only when the *state* changes leaves a
/// transparent cursor over its own controls, because the pen arrived there without any state edge being
/// crossed. So this is asked per reading, and this test is the rule it asks.
#[test]
fn the_arrow_is_the_cursor_over_the_bar_and_the_pills() {
    // A bar 100 tall, and a foot whose pills begin at 900: the page pill at 400..600 and the zoom pill
    // at 700..900.
    let (bar, foot) = (100.0, 900.0);
    let strips = FootPills::default()
        .measured(FootPlace::Page, 400.0, 600.0, 930.0)
        .measured(FootPlace::Zoom, 700.0, 900.0, 910.0)
        .strips();
    let shows = |x: f32, y: f32| pen_cursor_shows((x, y), bar, foot, strips);

    assert!(!shows(500.0, 50.0), "on the bar, the arrow is the cursor");
    assert!(!shows(500.0, bar), "and on the bar's own last pixel");
    assert!(shows(500.0, bar + 1.0), "one pixel below it is page again");

    assert!(!shows(500.0, 950.0), "over the page pill, the arrow");
    assert!(!shows(800.0, 950.0), "over the zoom pill too");
    assert!(shows(650.0, 950.0), "and the desk between them is page");
}

/// The two edges answer on their own: a hidden bar has no line, and a foot nobody has measured refuses
/// nothing.
#[test]
fn the_two_edges_answer_on_their_own() {
    let none = FootStrips::default();

    assert!(
        pen_cursor_shows((500.0, 500.0), 0.0, 0.0, none),
        "no bar and no pills leaves the page the ghost's"
    );
    assert!(
        pen_cursor_shows((500.0, 990.0), 0.0, 900.0, none),
        "and an unmeasured foot refuses nothing"
    );
    assert!(
        !pen_cursor_shows((500.0, 0.0), 0.0, 900.0, none),
        "the window's top pixel is still the bar's line, which a hidden bar puts at zero"
    );
}

/// A pill measured as an empty box covers only its own point and the slack around it — never the desk.
///
/// This is the failure that made the pen write on its own pills: a zero-size probe measured every pill at
/// nothing, and an answer derived from a width then came out as the whole row. Reading the box itself
/// means an empty measurement is an empty strip, which is the safe way round.
#[test]
fn a_pill_laid_out_at_no_width_covers_only_its_own_point() {
    let strips = FootPills::default()
        .measured(FootPlace::Page, 626.0, 626.0, 752.0)
        .measured(FootPlace::Zoom, 1013.0, 1013.0, 752.0)
        .strips();

    assert_eq!(
        (strips.page, strips.zoom),
        (Some((614.0, 638.0)), Some((1001.0, 1025.0))),
        "an unmeasured pill is a point grown by the slack, not a strip across the desk"
    );
    assert!(
        !strips.held((500.0, 950.0), 752.0),
        "so the desk beside it is still page"
    );
}

/// A probe's measurement has to *land*, one pill after the other.
///
/// This is the bug the strips had on the first day: `row_width` answered a new value rather than mutating
/// the receiver, the probe dropped the answer, and so `row` stayed `None` and no strip was ever derived —
/// the pen wrote on its own pills and the ghost drew its mark over them, with no error anywhere. The
/// builder is `#[must_use]` so a dropped answer is a warning rather than silence, and each probe now
/// records the pill it measured rather than a piece of a geometry to be reassembled.
#[test]
fn the_two_pills_measure_themselves_one_after_the_other() {
    let mut measured = FootPills::default();

    measured = measured.measured(FootPlace::Page, 400.0, 600.0, 930.0);
    assert!(
        measured.strips().page.is_some(),
        "one pill is enough for a strip"
    );

    measured = measured.measured(FootPlace::Zoom, 700.0, 900.0, 910.0);

    let strips = measured.strips();
    assert!(strips.page.is_some(), "the page pill's strip");
    assert!(strips.zoom.is_some(), "and the zoom pill's");
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

/// A scroll only pans, and only when the platform says what it is.
///
/// Pixels are a trackpad's and are always read: they are already the units the view pans in. A
/// line-based delta is a wheel's; whether it is read is the platform's answer
/// ([`crate::platform::Platform::wheel_pans`]), and a line is converted at
/// [`WHEEL_PAN_PER_LINE`]. A wheel is never a gesture that can turn a page.
#[test]
fn a_scroll_is_a_trackpad_or_a_wheel_the_platform_reads() {
    use gpui_kit::{point, px, ScrollDelta};

    let pixels = ScrollDelta::Pixels(point(px(1.5), px(-4.0)));
    let read = scroll_of(&pixels, false).expect("pixels are read everywhere");
    assert_eq!(read.distance, (1.5, -4.0), "pixels are already the units");
    assert!(read.gesture, "a trackpad reading may become an edge swipe");

    let wheel = ScrollDelta::Lines(point(0.0, 3.0));
    let read = scroll_of(&wheel, true).expect("a wheel is read where it scrolls");
    assert_eq!(
        read.distance,
        (0.0, 3.0 * WHEEL_PAN_PER_LINE),
        "three lines are three line heights"
    );
    assert!(!read.gesture, "a wheel is never an edge swipe");

    assert!(
        scroll_of(&wheel, false).is_none(),
        "a platform whose pointer is a trackpad leaves a wheel unread"
    );
}

/// A pen in the air is not a pen on the paper, and the gestures are the difference.
///
/// A digitizer reports a hovering pen, and hovering is where the ghost cursor is drawn and nothing else. With
/// a key held, a nib that is merely near the screen must not move the sheet — that was a real bug: the check
/// named the rule in a comment and then let `Hover` through, so a hand resting over the display dragged the
/// page around while the nib touched nothing.
///
/// `Down` begins a gesture and `Move` continues it; `Up` and `Cancel` end it. Everything else is air.
#[test]
fn only_a_nib_on_the_paper_gestures() {
    use crate::input::Phase;

    assert!(touches_paper(Phase::Down), "the nib came down");
    assert!(touches_paper(Phase::Move), "the nib is down and moving");

    for air in [
        Phase::Hover,
        Phase::Enter,
        Phase::Idle,
        Phase::Leave,
        Phase::Up,
        Phase::Cancel,
    ] {
        assert!(!touches_paper(air), "{air:?} is not a nib on the paper");
    }
}

/// A continuous gesture's end is a gap, and the gap is short enough to feel like an end.
///
/// A pinch delivers a stream of events with no final one, so the commit to the note waits for the
/// stream to go quiet. The interval has to be longer than any gap *inside* a live gesture — a pinch at 120 Hz
/// sends one every 8 ms — or a gesture would be committed, and its settings written, many times mid-pinch.
/// That is the whole bug this constant exists to avoid: the write is a four-table SQLite transaction.
#[test]
fn a_gesture_settles_only_after_a_real_gap() {
    use super::GESTURE_SETTLE_INTERVAL;

    // A fast pinch: one event every 8 ms at 120 Hz.
    let fastest_gap_between_events = Duration::from_millis(8);
    assert!(
        GESTURE_SETTLE_INTERVAL > fastest_gap_between_events * 5,
        "the settle must not fire inside a live pinch: {GESTURE_SETTLE_INTERVAL:?}"
    );

    // And short enough that a reader who stops sees the number written before they look away.
    assert!(
        GESTURE_SETTLE_INTERVAL <= Duration::from_millis(200),
        "a settled zoom should reach the note quickly: {GESTURE_SETTLE_INTERVAL:?}"
    );
}

/// The frame cap holds a frame that arrives early, and only that.
///
/// The whole of the setting is this one comparison, so it is checked against both sides of every
/// boundary: a frame inside the interval is held, one at or past it goes, and `Unlimited` never holds
/// anything. The *first* frame has no interval behind it and must always go — a cap that delayed the
/// first frame would be a cap that made a page open slower, which is the one thing pacing must not do.
#[test]
fn the_frame_cap_holds_only_a_frame_that_arrived_early() {
    use note_format::paper::FrameCap;

    // Nothing has drawn yet: no gap to measure, so no cap to apply.
    for cap in FrameCap::ALL {
        assert!(
            !held_by_cap(cap, None),
            "{} held the very first frame",
            cap.label()
        );
    }

    // Unlimited is never a cap, whatever the gap since the last frame.
    assert!(!held_by_cap(FrameCap::Unlimited, Some(Duration::ZERO)));

    let interval = FrameCap::Hz60.interval().expect("60 Hz has an interval");

    assert!(
        held_by_cap(FrameCap::Hz60, Some(Duration::ZERO)),
        "a frame at the same instant as the last is held"
    );
    assert!(
        held_by_cap(FrameCap::Hz60, Some(interval - Duration::from_micros(1))),
        "a frame a microsecond early is held"
    );
    assert!(
        !held_by_cap(FrameCap::Hz60, Some(interval)),
        "a frame exactly one interval later goes: a display and a cap that agree must not stall"
    );
    assert!(
        !held_by_cap(FrameCap::Hz60, Some(interval * 2)),
        "a frame long after the last one goes"
    );

    // The same gap, two caps, two answers — this is what makes the setting do anything at all. A gap of
    // 1 ms is too soon for every named rate and goes out under `Unlimited` alone; a gap of 4 ms is long
    // enough for 240 Hz and still too soon for 360 Hz.
    let soon = Duration::from_millis(1);
    assert!(
        held_by_cap(FrameCap::Hz60, Some(soon)),
        "1 ms is not a 60 Hz frame"
    );
    assert!(held_by_cap(FrameCap::Hz360, Some(soon)), "nor a 360 Hz one");
    assert!(
        !held_by_cap(FrameCap::Unlimited, Some(soon)),
        "unlimited holds nothing, however early"
    );

    let later = Duration::from_millis(4);
    assert!(
        !held_by_cap(FrameCap::Hz360, Some(later)),
        "4 ms is past 360 Hz's 2.78 ms, so it goes"
    );
    assert!(
        held_by_cap(FrameCap::Hz240, Some(later)),
        "4 ms is still under 240 Hz's 4.17 ms, so it is held"
    );
    assert!(
        held_by_cap(FrameCap::Hz60, Some(later)),
        "and well under 60 Hz's 16.7 ms"
    );

    // A gap shorter than the tightest cap still holds under the tightest cap: 2 ms is early for 360 Hz.
    let early = Duration::from_millis(2);
    assert!(
        held_by_cap(FrameCap::Hz360, Some(early)),
        "2 ms is under 360 Hz's 2.78 ms"
    );
    assert!(
        held_by_cap(FrameCap::Hz240, Some(early)),
        "and under 240 Hz's 4.17 ms as well: the tighter cap holds what the looser one does too"
    );
}
/// A pen pan asks for the whole distance the hand travelled, and the view decides what can be shown.
///
/// This is the split that fixed the sheet sitting still: the gesture's job is to report where the hand is, and
/// the clamp belongs to the drawing. Holding the pan inside its bounds mid-gesture — which the app used to do on
/// every reading — threw away travel that no later reading could recover, because each one is measured from the
/// gesture's origin. The result was up to 500 px of hand movement with the paper motionless, then a jump.
#[test]
fn a_pen_pan_asks_for_the_whole_distance_out_of_range_included() {
    let gesture = PenGesture {
        mode: crate::pen_gesture::Gesture::Pan,
        origin: (500.0, 400.0),
        zoom: 1.0,
        pan: (0.0, 0.0),
        // The arithmetic of `panned_to`, which is what this is about: the tremor is a step in front of it
        // (see [`crate::pen_gesture::PenJitter`]) and is tested where it lives.
        jitter: crate::pen_gesture::PenJitter::new(),
        smoothed: false,
    };

    // The nib has not moved: the pan it began with, unchanged.
    assert_eq!(gesture.panned_to((500.0, 400.0)), (0.0, 0.0));

    // Far past any page edge, in both directions: the answer is the hand's own number, not a clamped one.
    let far = gesture.panned_to((500.0, 400.0 - 1.0e4));
    assert!(
        far.1 < -1.0e4,
        "the gesture clamped the pan instead of reporting where the hand is: {far:?}"
    );

    // And it is the gain times the distance, so the number is not simply the pixel travel.
    let near = gesture.panned_to((520.0, 380.0));
    let gain = crate::pen_gesture::PEN_PAN_GAIN;
    assert!((near.0 - 20.0 * gain).abs() < 1.0e-4, "{near:?}");
    assert!((near.1 - -20.0 * gain).abs() < 1.0e-4, "{near:?}");
}

/// Coming back towards the origin asks for the same pan it started from, however far out it had gone.
///
/// The property that makes a gesture absolute: each reading is asked about against the origin, never against the
/// previous reading, so a hand that overshoots and returns leaves the sheet exactly where it began.
#[test]
fn a_pen_pan_that_returns_asks_for_where_it_started() {
    let gesture = PenGesture {
        mode: crate::pen_gesture::Gesture::Pan,
        origin: (300.0, 300.0),
        zoom: 1.0,
        pan: (12.0, -34.0),
        jitter: crate::pen_gesture::PenJitter::new(),
        smoothed: false,
    };

    let overshot = gesture.panned_to((300.0, -5000.0));
    assert!(
        overshot.1 < -5000.0,
        "it should have gone out: {overshot:?}"
    );

    let back = gesture.panned_to((300.0, 300.0));
    assert_eq!(
        back,
        (12.0, -34.0),
        "a hand that came back must ask for the pan it began at"
    );
}

/// A hidden status line asks the platform for nothing.
///
/// The rule that makes the hardware clause free when nobody is reading it: the sample is an operating
/// system call a second, and it happens only while the line is on screen. Hiding the line is therefore
/// also how a reader stops paying for it, which is what lets the figure be offered without a switch of
/// its own (see [`NoteApp::sample_vitals`]).
#[test]
fn the_counters_are_read_only_while_the_line_is_shown() {
    assert!(
        samples_vitals(true),
        "a visible line has a hardware clause to fill"
    );
    assert!(
        !samples_vitals(false),
        "and a hidden one costs the platform nothing"
    );
}
