use super::*;

/// A stick held still asks for nothing, and a second at full tilt is the speed, not a leap.
#[test]
fn a_stick_moves_the_sheet_at_its_speed() {
    assert_eq!(stick_pan(0.0, 0.0, Duration::from_millis(16)), (0.0, 0.0));
    assert_eq!(
        stick_pan(1.0, 1.0, Duration::from_millis(100)),
        (-100.0, 100.0),
        "a tenth of a second at a thousand pixels a second"
    );
}

/// The stick moves the view: right looks further right, up looks earlier in the page.
#[test]
fn the_stick_points_the_way_the_view_looks() {
    let (right, up) = stick_pan(1.0, 1.0, Duration::from_millis(100));
    assert!(right < 0.0, "the sheet moves left when the view goes right");
    assert!(up > 0.0, "the sheet moves down when the view goes up");

    let (left, down) = stick_pan(-1.0, -1.0, Duration::from_millis(100));
    assert!(left > 0.0 && down < 0.0);
}

/// A machine that slept does not wake to the sheet a minute away.
#[test]
fn a_long_sleep_is_only_one_step() {
    assert_eq!(
        stick_pan(1.0, 0.0, Duration::from_secs(60)),
        (-100.0, 0.0),
        "clamped to the longest step, not multiplied by a minute"
    );
}

/// The right stick's Y axis zooms: away from the reader in, toward them out.
#[test]
fn the_right_stick_zooms_in_the_direction_it_points() {
    assert_eq!(stick_zoom(0.0, Duration::from_millis(100)), 1.0);

    let closer = stick_zoom(1.0, Duration::from_millis(100));
    let further = stick_zoom(-1.0, Duration::from_millis(100));

    assert!(closer > 1.0, "away from the reader is closer");
    assert!(further < 1.0, "toward the reader is further away");
    assert!(
        (closer * further - 1.0).abs() < 1e-6,
        "the two directions undo each other"
    );
}

/// Full deflection for a second doubles the sheet, however the second is divided into wakes.
#[test]
fn a_second_at_full_tilt_doubles_the_zoom() {
    let mut factor = 1.0;
    for _ in 0..10 {
        factor *= stick_zoom(1.0, Duration::from_millis(100));
    }

    assert!((factor - 2.0).abs() < 1e-5, "two, not a stack of steps");
}

/// A machine that slept does not wake to a zoom a minute away either.
#[test]
fn a_long_sleep_zooms_once() {
    assert_eq!(
        stick_zoom(1.0, Duration::from_secs(60)),
        stick_zoom(1.0, GAMEPAD_LONGEST_STEP)
    );
}

/// The system pointer follows the right stick: right is right, away from the reader is up, and a
/// stick near its centre is still.
#[test]
fn the_pointer_follows_the_right_stick() {
    assert_eq!(
        stick_pointer(0.0, 0.0, Duration::from_millis(100)),
        (0.0, 0.0)
    );
    assert_eq!(
        stick_pointer(1.0, 1.0, Duration::from_millis(100)),
        (50.0, -50.0),
        "a tenth of a second at five hundred pixels a second, up the screen"
    );
    assert_eq!(
        stick_pointer(0.1, -0.1, Duration::from_millis(100)),
        (0.0, 0.0),
        "inside the pointer's own dead zone"
    );
}

/// The pointer accelerates with the push: a light touch is slower than a firm one, and full
/// deflection is still the speed the constant names.
#[test]
fn a_harder_push_moves_the_pointer_faster() {
    let gentle = stick_pointer(0.6, 0.0, Duration::from_millis(100)).0;
    let firm = stick_pointer(0.8, 0.0, Duration::from_millis(100)).0;
    let full = stick_pointer(1.0, 0.0, Duration::from_millis(100)).0;

    assert!(
        gentle < firm && firm < full,
        "the curve rises with deflection: {gentle} < {firm} < {full}"
    );
    assert!(
        (gentle - 17.677_67).abs() < 0.001,
        "half of the usable travel is half raised to the curve, not half the speed: {gentle}"
    );
    assert_eq!(
        full, 50.0,
        "full deflection is still five hundred pixels a second"
    );
}

/// The system scroll follows the left stick, and away from the reader scrolls up.
#[test]
fn the_scroll_follows_the_left_stick() {
    assert_eq!(
        stick_scroll(1.0, 1.0, Duration::from_millis(100)),
        (90.0, 90.0)
    );
    assert_eq!(
        stick_scroll(-1.0, -1.0, Duration::from_millis(100)),
        (-90.0, -90.0)
    );
}

/// The scroll accelerates with the push too: a light touch creeps, a firm one crosses the page,
/// and full deflection is still the speed the constant names.
#[test]
fn a_harder_push_scrolls_faster() {
    let gentle = stick_scroll(0.3, 0.0, Duration::from_millis(100)).0;
    let firm = stick_scroll(0.8, 0.0, Duration::from_millis(100)).0;
    let full = stick_scroll(1.0, 0.0, Duration::from_millis(100)).0;

    assert!(
        gentle < firm && firm < full,
        "the curve rises with deflection: {gentle} < {firm} < {full}"
    );
    assert!(
        (gentle - 9.428_1).abs() < 0.001,
        "a light push creeps: {gentle}"
    );
    assert_eq!(
        full, 90.0,
        "full deflection is still nine hundred pixels a second"
    );
    assert_eq!(
        stick_scroll(0.1, 0.1, Duration::from_millis(100)),
        (0.0, 0.0),
        "inside the scroll's dead zone"
    );
}

/// A shoulder is a click at its edge, not a click on every read.
#[test]
fn a_shoulder_reports_its_edges() {
    let mut was = false;
    assert_eq!(button_edge(true, &mut was), Some(true), "pressed");
    assert_eq!(button_edge(true, &mut was), None, "held");
    assert_eq!(button_edge(false, &mut was), Some(false), "released");
    assert_eq!(button_edge(false, &mut was), None, "still up");
}

/// The Start+Select chord is one pulse when both are held, not one per read.
#[test]
fn the_toggle_chord_is_one_pulse() {
    let mut was = false;
    assert!(!chord_edge(false, &mut was), "neither held");
    assert!(chord_edge(true, &mut was), "both held: the pulse");
    assert!(!chord_edge(true, &mut was), "still held");
    assert!(!chord_edge(false, &mut was), "released");
}

/// With the setting off, nothing is opened at all — not a manager that is merely ignored.
///
/// This is the difference the switch exists for. "Off" has to mean the platform's HID layer is never touched,
/// because the reason a reader turns it off is a machine that is dropping a Bluetooth pad: a live manager that is
/// simply not read would keep holding the device and would not help at all. So this asks the object rather than
/// the setting — there is no `Gilrs` behind a disabled `Gamepads`, and the sticks therefore ask for nothing.
#[test]
fn a_disabled_gamepad_opens_nothing() {
    let mut off = Gamepads::new(false);

    assert!(
        !off.is_open(),
        "off means the HID manager was never created"
    );
    assert!(!off.is_connected(), "off means no pad is being read");
    assert_eq!(
        off.read(),
        Sticks::default(),
        "and the sticks ask for nothing"
    );
}

/// Turned on, the manager is opened — which, on a machine with no pad attached, is still a live manager.
///
/// The converse, and the reason the switch is a rebuild rather than a filter: on has to actually open something,
/// or flipping it would never start the sticks.
#[test]
fn an_enabled_gamepad_opens_the_manager() {
    let on = Gamepads::new(true);

    assert!(on.is_open(), "on means the manager exists");
}
