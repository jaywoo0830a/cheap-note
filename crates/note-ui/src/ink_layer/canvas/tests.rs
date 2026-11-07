use super::*;
use gpui_kit::rgba;

/// A rectangle with no area is not added: a shadow step of zero, a sheet off the surface, a
/// window narrower than its own border. They are all the same to a fill and none of them is
/// worth a command.
#[test]
fn a_rectangle_with_no_area_is_not_painted() {
    let mut canvas = Canvas::default();
    canvas.fill(
        Rect {
            x: 1.0,
            y: 1.0,
            width: 0.0,
            height: 10.0,
        },
        rgba(0xFF00_00FF).into(),
    );

    assert!(canvas.fills.is_empty());
}

/// Clearing empties the canvas without forgetting the desk or the scale: a frame's fills are
/// its own, the desk it is painted on and the display it is drawn for belong to the window.
#[test]
fn clearing_forgets_the_fills_and_keeps_the_desk() {
    let desk = Hsla::from(rgba(0x8080_80FF));
    let mut canvas = Canvas {
        desk,
        scale: 1.5,
        ..Default::default()
    };
    canvas.fill(
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        rgba(0xFFFF_FFFF).into(),
    );

    canvas.clear();

    assert!(canvas.fills.is_empty());
    assert_eq!(canvas.desk, desk);
    assert_eq!(canvas.scale, 1.5);
}
