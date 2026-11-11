//! Standard names over Fresco's gamepad events.

use pergola_gamepad::{usage, Axis, Button, Change, Mapping, Pads};

const B: fn(u16) -> u32 = usage::button;

/// The VM check's stand-in pad (tactus/examples/fake-pad.rs): X, Y, Rx, Ry
/// sticks, a Z trigger, Brake, a hat, 15 buttons; the events frescod sent
/// the window, in order.
#[test]
fn a_generic_pad_waits_for_its_axes_then_maps_them() {
    let mut p = Pads::new();
    assert_eq!(p.connect(3, 0x1234, 0x5678, 15, 7, "Fake Pad"), vec![Change::Connected { device: 3 }]);
    let mut out = Vec::new();
    for (u, v) in [(usage::X, 0), (usage::Y, 0), (usage::RX, 0), (usage::RY, 0), (usage::Z, -32768), (usage::BRAKE, -32768)] {
        out.extend(p.axis(3, u, v));
    }
    assert!(out.is_empty(), "held back until all 7 axes reported: {out:?}");
    out.extend(p.axis(3, usage::HAT, -1));
    assert!(out.is_empty(), "everything at rest, nothing changed: {out:?}");
    let pad = p.get(3).unwrap();
    assert!(!pad.mapping.unresolved());

    // Rx/Ry are the right stick and Brake the left trigger, so Z is left
    // unmapped rather than guessed.
    let c = p.axis(3, usage::RX, 32767);
    assert_eq!(c, vec![Change::Axis { device: 3, axis: Axis::RightX, value: 1.0 }]);
    assert!(p.axis(3, usage::Z, 32767).is_empty());
    let c = p.axis(3, usage::BRAKE, 32767);
    assert_eq!(c, vec![Change::Axis { device: 3, axis: Axis::LeftTrigger, value: 1.0 }]);
    assert_eq!(p.button(3, B(1), true), vec![Change::Button { device: 3, button: Button::South, pressed: true }]);
    assert_eq!(p.button(3, B(1), true), vec![], "no change, no event");
    assert_eq!(p.button(3, B(4), true), vec![Change::Button { device: 3, button: Button::North, pressed: true }]);
    let c = p.axis(3, usage::HAT, 2);
    assert_eq!(c, vec![Change::Button { device: 3, button: Button::DpadRight, pressed: true }]);
    let c = p.axis(3, usage::HAT, 3);
    assert_eq!(c, vec![Change::Button { device: 3, button: Button::DpadDown, pressed: true }]);
    assert!(p.get(3).unwrap().pressed(Button::DpadRight));

    // Disconnect releases what is held, then says so.
    let c = p.disconnect(3);
    assert_eq!(c.last(), Some(&Change::Disconnected { device: 3 }));
    for b in [Button::South, Button::North, Button::DpadDown, Button::DpadRight] {
        assert!(c.contains(&Change::Button { device: 3, button: b, pressed: false }), "{b:?} released: {c:?}");
    }
    assert!(p.get(3).is_none());
    assert!(p.button(3, B(1), false).is_empty(), "events for a pad not held are ignored");
}

/// Without Rx/Ry, Z/Rz are the right stick.
#[test]
fn z_and_rz_are_the_right_stick_without_rx() {
    let mut p = Pads::new();
    p.connect(1, 0x0001, 0x0002, 12, 4, "Joystick");
    let mut out = Vec::new();
    for (u, v) in [(usage::X, 0), (usage::Y, 0), (usage::Z, 0), (usage::RZ, -32768)] {
        out.extend(p.axis(1, u, v));
    }
    assert!(out.contains(&Change::Axis { device: 1, axis: Axis::RightY, value: -1.0 }), "{out:?}");
}

/// A button before every axis has reported settles the mapping with what
/// was seen, and replays the held-back axes first.
#[test]
fn a_button_settles_the_mapping() {
    let mut p = Pads::new();
    p.connect(1, 0x0001, 0x0002, 4, 6, "Pad");
    assert!(p.axis(1, usage::X, 32767).is_empty());
    let c = p.button(1, B(2), true);
    assert_eq!(c[0], Change::Axis { device: 1, axis: Axis::LeftX, value: 1.0 });
    assert_eq!(c.last(), Some(&Change::Button { device: 1, button: Button::East, pressed: true }));
}

#[test]
fn sony_pads_have_their_own_order() {
    let mut p = Pads::new();
    p.connect(7, 0x054c, 0x09cc, 14, 7, "Wireless Controller");
    assert!(!p.get(7).unwrap().mapping.unresolved());
    assert_eq!(p.button(7, B(2), true), vec![Change::Button { device: 7, button: Button::South, pressed: true }], "Cross");
    assert_eq!(p.button(7, B(1), true)[0], Change::Button { device: 7, button: Button::West, pressed: true }, "Square");
    assert_eq!(p.button(7, B(13), true)[0], Change::Button { device: 7, button: Button::Guide, pressed: true }, "PS");
    assert_eq!(p.axis(7, usage::RZ, 32767), vec![Change::Axis { device: 7, axis: Axis::RightY, value: 1.0 }]);
    assert_eq!(p.axis(7, usage::RX, 32767), vec![Change::Axis { device: 7, axis: Axis::LeftTrigger, value: 1.0 }]);
}

/// Sticks have a radial dead zone, triggers a linear one; both stretch the
/// rest so full travel stays 1.
#[test]
fn dead_zones() {
    let mut p = Pads::new();
    p.connect(1, 0, 0, 0, 0, "Pad");
    p.get_mut(1).unwrap().mapping = {
        let mut m = Mapping::empty();
        m.set_axis(usage::X, Axis::LeftX).set_axis(usage::Y, Axis::LeftY).set_axis(usage::Z, Axis::LeftTrigger);
        m
    };
    assert!(p.axis(1, usage::X, 3000).is_empty(), "inside the stick dead zone: still 0");
    let c = p.axis(1, usage::X, 32767);
    assert_eq!(c, vec![Change::Axis { device: 1, axis: Axis::LeftX, value: 1.0 }]);
    let c = p.axis(1, usage::Y, 32767);
    let (x, y) = p.get(1).unwrap().stick(true);
    assert!(c.len() == 2 && (x - y).abs() < 1e-6 && (x - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3, "a full diagonal is clamped to the unit circle: {x} {y}");
    assert!(p.axis(1, usage::Z, -32768 + 1000).is_empty(), "trigger rest");
    let half = p.get(1).unwrap().axis(Axis::LeftTrigger);
    assert_eq!(half, 0.0);
    p.axis(1, usage::Z, 0);
    let t = p.get(1).unwrap().axis(Axis::LeftTrigger);
    assert!((t - (0.5 - 0.05) / 0.95).abs() < 1e-3, "{t}");
}

/// The D-pad is held where either the hat or a D-pad usage holds it.
#[test]
fn hat_and_dpad_usages_share_the_dpad() {
    let mut p = Pads::new();
    p.connect(1, 0, 0, 4, 0, "Pad");
    assert_eq!(p.button(1, usage::DPAD_UP, true), vec![Change::Button { device: 1, button: Button::DpadUp, pressed: true }]);
    assert!(p.axis(1, usage::HAT, 0).is_empty(), "already up");
    assert!(p.button(1, usage::DPAD_UP, false).is_empty(), "the hat still holds it");
    assert_eq!(p.axis(1, usage::HAT, -1), vec![Change::Button { device: 1, button: Button::DpadUp, pressed: false }]);
}

/// An app's own mapping for a pad it knows.
#[test]
fn an_app_can_remap() {
    let mut p = Pads::new();
    p.connect(1, 0x045e, 0x0b13, 15, 7, "Xbox Wireless Controller");
    let pad = p.get_mut(1).unwrap();
    pad.mapping.set_button(B(4), Button::West).set_button(B(5), Button::North).unset(B(3));
    assert_eq!(p.button(1, B(4), true)[0], Change::Button { device: 1, button: Button::West, pressed: true });
    assert!(p.button(1, B(3), true).is_empty());
}
