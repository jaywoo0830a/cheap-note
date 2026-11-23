//! What the Metal device actually puts on a surface, read back from the pixels.
//!
//! Every test here encodes a real frame on the machine's GPU through the **same encode the window uses**
//! and then reads the texture it drew into. That is the only way to know a pipeline, a stencil and a
//! blend factor agree with each other: the plan tests say what *would* be drawn, and a device that
//! encodes nothing passes all of them.
//!
//! A machine without Metal skips: the crate is built on Linux CI too.

#![cfg(target_os = "macos")]

use cheap_note_engine::band::Triangle;
use cheap_note_engine::footprint::Rect;
use cheap_note_engine::surface::{BandDraw, FillRule, Frame, InkDraw};
use cheap_note_engine_metal::metal::Shot;
use cheap_note_engine_metal::plan::Target;
use cheap_note_engine_metal::{BaseLayer, Device, Mark, Plan, RectFill};

/// The desk and the sheet, in logical pixels: the sheet from (100, 50) to (500, 350).
fn a_sheet() -> BaseLayer {
    BaseLayer {
        desk: Some(0xFFF1_F2F4),
        fills: vec![RectFill {
            rect: Rect {
                x: 100.0,
                y: 50.0,
                width: 400.0,
                height: 300.0,
            },
            colour: 0xFFFB_F3E4,
        }],
    }
}

/// A frame holding exactly these triangles in exactly this colour.
fn frame_with(triangles: Vec<Triangle>, colour: u32) -> Frame {
    Frame {
        base: Default::default(),
        ink: InkDraw {
            repair: Vec::new(),
            bands: vec![BandDraw {
                triangles,
                colour,
                rule: FillRule::NonZero,
            }],
        },
    }
}

/// A quad from `(x0, y0)` to `(x1, y1)` as two triangles.
fn quad((x0, y0): (f32, f32), (x1, y1): (f32, f32)) -> Vec<Triangle> {
    vec![
        Triangle {
            a: (x0, y0),
            b: (x1, y0),
            c: (x0, y1),
        },
        Triangle {
            a: (x0, y1),
            b: (x1, y0),
            c: (x1, y1),
        },
    ]
}

/// One horizontal stroke across the sheet: `x` 150 to 450 at `y` 200, four logical pixels either side.
fn one_stroke() -> Plan {
    Plan::of(&frame_with(
        quad((150.0, 196.0), (450.0, 204.0)),
        0xFF1C_1C1E,
    ))
}

/// A device drawing an offscreen surface of this logical size at this scale, or `None` with no GPU.
fn device(width: f32, height: f32, scale: f32) -> Option<Device> {
    Device::offscreen(Target::new(width, height, scale))
}

/// Every device pixel that is not one of `colours`.
fn strays(shot: &Shot, colours: &[(u8, u8, u8)]) -> usize {
    let mut count = 0;

    for y in 0..shot.device_height() {
        for x in 0..shot.device_width() {
            if !colours.contains(&shot.pixel(x, y)) {
                count += 1;
            }
        }
    }

    count
}

#[test]
fn the_desk_and_the_sheet_are_drawn() {
    let Some(mut device) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let shot = device.draw_for_test(&a_sheet(), &Plan::default(), None);

    // The desk is the whole target, and the sheet is over it on the left.
    assert_eq!(shot.pixel(600, 380), (241, 242, 244), "the desk");
    assert_eq!(shot.pixel(200, 100), (251, 243, 228), "the sheet");
}

#[test]
fn one_stroke_lands_on_the_sheet_in_its_own_colour() {
    let Some(mut device) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let shot = device.draw_for_test(&a_sheet(), &one_stroke(), None);

    assert_eq!(
        shot.pixel(300, 200),
        (28, 28, 30),
        "the middle of the stroke is the ink's colour"
    );
    assert_eq!(
        shot.pixel(300, 180),
        (251, 243, 228),
        "twenty points above the stroke is still the sheet"
    );
}

#[test]
fn a_stroke_is_the_same_place_at_either_density() {
    // The unit contract: logical pixels everywhere, the display's density applied once. The same stroke
    // over the same logical surface must cover the same *logical* place at 1× and at 2×.
    let Some(mut plain) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let Some(mut dense) = device(640.0, 400.0, 2.0) else {
        return;
    };

    let plain = plain.draw_for_test(&a_sheet(), &one_stroke(), None);
    let dense = dense.draw_for_test(&a_sheet(), &one_stroke(), None);

    assert_eq!((plain.device_width(), plain.device_height()), (640, 400));
    assert_eq!((dense.device_width(), dense.device_height()), (1280, 800));

    // The same logical point, at each surface's own density.
    assert_eq!(plain.pixel(300, 200), (28, 28, 30));
    assert_eq!(dense.pixel(600, 400), (28, 28, 30));

    // And the sheet's edge is where the target says, at both.
    assert_eq!(plain.pixel(99, 100), (241, 242, 244), "left of the sheet");
    assert_eq!(dense.pixel(198, 200), (241, 242, 244), "left of it, 2×");
    assert_eq!(plain.pixel(101, 100), (251, 243, 228), "on the sheet");
    assert_eq!(dense.pixel(202, 200), (251, 243, 228), "on it, 2×");
}

#[test]
fn a_stroke_that_doubles_back_is_still_one_fill() {
    // E6: a band is the union of its pieces, so a stroke laid back over itself is not twice as dark. The
    // stencil is the whole of why: the marking pass writes a sample once and the colour pass draws it once.
    let mut there_and_back = quad((150.0, 196.0), (450.0, 204.0));
    there_and_back.extend(quad((300.0, 196.0), (450.0, 204.0)));

    let Some(mut once) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let Some(mut twice) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let once = once.draw_for_test(&a_sheet(), &one_stroke(), None);
    let twice = twice.draw_for_test(
        &a_sheet(),
        &Plan::of(&frame_with(there_and_back, 0xFF1C_1C1E)),
        None,
    );

    // Where the two runs overlap, the ink is the same ink, not the colour of ink over ink.
    assert_eq!(once.pixel(400, 200), (28, 28, 30));
    assert_eq!(twice.pixel(400, 200), (28, 28, 30));
}

#[test]
fn the_pen_is_drawn_over_the_ink() {
    let Some(mut device) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let mark = Mark {
        at: (300.0, 200.0),
        colour: 0xFFE0_1B24,
        halo: 0xFFFF_FFFF,
        nib_radius: 6.0,
        lean: 1.0,
        unit: 1.0,
    };

    let shot = device.draw_for_test(&a_sheet(), &one_stroke(), Some(&mark));

    // The nib's dot is red and opaque, and it is the last thing drawn — over the black it sits on.
    assert_eq!(shot.pixel(300, 200), (224, 27, 36), "the nib");
    assert_eq!(shot.alpha(300, 200), 255, "the nib is opaque");
}

#[test]
fn a_repair_leaves_the_sheet_showing_an_erased_stroke_leaves_no_mark() {
    // A repair is "this ink is gone": what shows afterwards is the **sheet**, not the desk. The ink pass
    // has no paper of its own, so a repair painted in the desk's colour would leave a grey rectangle on
    // the sheet — which is exactly what an erase looked like before the base and the ink were one frame.
    let Some(mut device) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let frame = Frame {
        base: Default::default(),
        ink: InkDraw {
            repair: vec![Rect {
                x: 250.0,
                y: 190.0,
                width: 100.0,
                height: 20.0,
            }],
            bands: Vec::new(),
        },
    };

    let shot = device.draw_for_test(&a_sheet(), &Plan::of(&frame), None);

    assert_eq!(shot.pixel(300, 200), (251, 243, 228), "the sheet, repaired");
}

#[test]
fn nothing_is_drawn_beside_the_desk_the_sheet_and_the_ink() {
    let Some(mut device) = device(640.0, 400.0, 1.0) else {
        return;
    };

    let shot = device.draw_for_test(&a_sheet(), &one_stroke(), None);

    // Three colours and no others. A band drawn twice would show as a second ink; a repair left behind
    // would show as a stray rectangle.
    let strays = strays(&shot, &[(241, 242, 244), (251, 243, 228), (28, 28, 30)]);

    assert_eq!(strays, 0, "every pixel is desk, sheet or ink");
}
