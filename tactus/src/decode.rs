//! Reports to canonical events (docs/spec/atrium-input.md §5): keys and
//! buttons as presses and releases by HID usage, relative motion,
//! absolute position, wheels. One [`Decoder`] per device; it keeps what
//! is held so each report turns into the changes since the last.

use crate::hid::{self, Class, Descriptor, Usage};

/// What one report says, before the pointer stage and key repeat.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A key (keyboard page 0x07, consumer page 0x0C, system control).
    Key { usage: Usage, down: bool },
    /// A pointer button (page 0x09).
    Button { usage: Usage, down: bool },
    /// Relative motion in device counts.
    Motion { dx: i32, dy: i32 },
    /// Absolute position, 0..=1 across the device's range.
    Absolute { x: f32, y: f32 },
    /// Wheel detents (HID sign: positive is away from the user / right).
    Scroll { axis: Axis, detents: i32 },
    /// One touchpad frame: every contact the pad reported (lifted ones
    /// with `tip` false) and whether its button is down.
    Touch { contacts: Vec<Contact>, button: bool },
    /// A gamepad or joystick button, by the usage the descriptor gives it
    /// (page 0x09, or a one-bit control such as a D-pad direction).
    PadButton { usage: Usage, down: bool },
    /// A gamepad or joystick axis changed (spec §5.4): `value` is the
    /// logical range scaled to -32768..=32767. The hat switch
    /// ([`HAT`]) is 0..=7 clockwise from up, or -1 when centred.
    PadAxis { usage: Usage, value: i16 },
}

/// The hat switch (Generic Desktop 0x39): a gamepad's D-pad.
pub const HAT: Usage = hid::usage(hid::PAGE_DESKTOP, 0x39);
const PAGE_SIMULATION: u16 = 0x02;

/// What a gamepad's controls are: its buttons and axes by usage, in
/// descriptor order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PadLayout {
    pub buttons: Vec<Usage>,
    pub axes: Vec<Usage>,
}

/// How a gamepad field's value is read.
#[derive(Clone, Copy, PartialEq)]
enum PadControl {
    Button,
    Axis,
    Hat,
    Ignored,
}

fn pad_control(f: &hid::Field, u: Usage) -> PadControl {
    let page = (u >> 16) as u16;
    if f.relative || u & 0xffff == 0 {
        return PadControl::Ignored;
    }
    if u == HAT {
        return PadControl::Hat;
    }
    // Buttons, and the D-pad as four one-bit Generic Desktop controls
    // (0x90..=0x93: up, down, right, left).
    if page == hid::PAGE_BUTTON || (page == hid::PAGE_DESKTOP && (0x90..=0x93).contains(&(u & 0xffff))) {
        return PadControl::Button;
    }
    if (page == hid::PAGE_DESKTOP || page == PAGE_SIMULATION) && f.logical_max > f.logical_min {
        return PadControl::Axis;
    }
    PadControl::Ignored
}

/// `v` in `lo..=hi` scaled to `i16`, ends to ends.
fn scale_axis(v: i64, lo: i32, hi: i32) -> i16 {
    let (lo, hi) = (i64::from(lo), i64::from(hi));
    let t = (v.clamp(lo, hi) - lo) * 65535 / (hi - lo);
    (t - 32768) as i16
}

/// A hat switch's position, 0..=7 clockwise from up (a 4-way hat's
/// positions are doubled), or -1 for a value outside its range (centred).
fn hat_position(v: i64, lo: i32, hi: i32) -> i16 {
    let (lo, hi) = (i64::from(lo), i64::from(hi));
    if v < lo || v > hi {
        return -1;
    }
    let n = hi - lo + 1;
    if n == 4 {
        return ((v - lo) * 2) as i16;
    }
    if n != 8 {
        return -1;
    }
    (v - lo) as i16
}

/// One finger on a touchpad, in millimetres from the pad's corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contact {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    /// Touching (else this report says the finger lifted).
    pub tip: bool,
    /// The pad thinks it is a finger, not a palm (Confidence usage;
    /// true when the pad does not say).
    pub confident: bool,
}

const FINGER: Usage = hid::usage(hid::PAGE_DIGITIZER, 0x22);
const TIP: Usage = hid::usage(hid::PAGE_DIGITIZER, 0x42);
const CONFIDENCE: Usage = hid::usage(hid::PAGE_DIGITIZER, 0x47);
const CONTACT_ID: Usage = hid::usage(hid::PAGE_DIGITIZER, 0x51);
const CONTACT_COUNT: Usage = hid::usage(hid::PAGE_DIGITIZER, 0x54);

/// A touchpad frame being put together: hybrid-mode pads send a frame's
/// contacts over several reports, the count in the first.
#[derive(Debug, Default)]
struct Pending {
    expect: usize,
    contacts: Vec<Contact>,
    button: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Axis {
    Vertical,
    Horizontal,
}

pub struct Decoder {
    desc: Descriptor,
    /// Per application collection, what was held after the last report.
    held: Vec<Vec<Usage>>,
    /// Last absolute position per collection, to send only changes.
    abs: Vec<Option<(f32, f32)>>,
    /// Per collection, a touchpad frame in progress.
    touch: Vec<Pending>,
    /// Per collection, each gamepad axis's last value.
    pad: Vec<Vec<(Usage, i16)>>,
}

/// Classes whose reports this decoder turns into events (touch screens
/// and pens come later, spec §8).
fn decoded(c: Class) -> bool {
    matches!(c, Class::Pointer | Class::Keyboard | Class::SystemKeys | Class::MediaKeys | Class::Touchpad | Class::Gamepad | Class::Joystick)
}

fn is_pad(c: Class) -> bool {
    matches!(c, Class::Gamepad | Class::Joystick)
}

/// Keyboard usages 1..=3 are ErrorRollOver, POSTFail, ErrorUndefined:
/// the keyboard cannot say what is held, so keep the last state.
fn phantom(u: Usage) -> bool {
    u >> 16 == u32::from(hid::PAGE_KEYBOARD) && (1..=3).contains(&(u & 0xffff))
}

impl Decoder {
    pub fn new(desc: Descriptor) -> Decoder {
        let n = desc.apps.len();
        Decoder {
            desc,
            held: vec![Vec::new(); n],
            abs: vec![None; n],
            touch: (0..n).map(|_| Pending::default()).collect(),
            pad: vec![Vec::new(); n],
        }
    }

    pub fn descriptor(&self) -> &Descriptor {
        &self.desc
    }

    /// The report ids this device sends input on (0 if unnumbered).
    pub fn decodes_anything(&self) -> bool {
        self.desc.apps.iter().any(|a| decoded(a.class))
    }

    /// Turn one report (as read from hidraw: the id byte first on a
    /// numbered device) into events, appended to `out`.
    pub fn report(&mut self, report: &[u8], out: &mut Vec<Input>) {
        let (id, body) = if self.desc.numbered {
            match report.split_first() {
                Some((id, body)) => (*id, body),
                None => return,
            }
        } else {
            (0, report)
        };
        for app in 0..self.desc.apps.len() {
            if !decoded(self.desc.apps[app].class) {
                continue;
            }
            if self.desc.apps[app].class == Class::Touchpad {
                self.touchpad(app, id, body, out);
                continue;
            }
            if is_pad(self.desc.apps[app].class) {
                self.gamepad(app, id, body, out);
                continue;
            }
            let fields: Vec<&hid::Field> =
                self.desc.fields.iter().filter(|f| f.app == app && f.report_id == id).collect();
            if fields.is_empty() {
                continue;
            }
            let mut pressed: Vec<Usage> = Vec::new();
            let (mut dx, mut dy) = (0i32, 0i32);
            let (mut ax, mut ay) = (None, None);
            let mut phantom_state = false;
            let mut short = false;
            for f in fields {
                for k in 0..f.count {
                    let Some(v) = f.value(body, k) else {
                        short = true;
                        break;
                    };
                    if f.array {
                        if let Some(u) = f.usage_of_index(v) {
                            phantom_state |= phantom(u);
                            if !pressed.contains(&u) {
                                pressed.push(u);
                            }
                        }
                        continue;
                    }
                    let Some(u) = f.usage_at(k) else { continue };
                    if f.relative {
                        let v = v.clamp(i32::MIN.into(), i32::MAX.into()) as i32;
                        match u {
                            hid::X => dx = dx.saturating_add(v),
                            hid::Y => dy = dy.saturating_add(v),
                            hid::WHEEL if v != 0 => out.push(Input::Scroll { axis: Axis::Vertical, detents: v }),
                            hid::AC_PAN if v != 0 => out.push(Input::Scroll { axis: Axis::Horizontal, detents: v }),
                            _ => {}
                        }
                    } else if u == hid::X || u == hid::Y {
                        let span = (f.logical_max - f.logical_min).max(1) as f32;
                        let t = ((v - i64::from(f.logical_min)) as f32 / span).clamp(0.0, 1.0);
                        if u == hid::X { ax = Some(t) } else { ay = Some(t) }
                    } else if v != 0 && u & 0xffff != 0 && !pressed.contains(&u) {
                        // A one-bit (or on/off) control: buttons, modifiers,
                        // bitmap keyboards, media-key bits.
                        pressed.push(u);
                    }
                }
            }
            if short && pressed.is_empty() {
                // A truncated report says nothing about what is held.
                continue;
            }
            if dx != 0 || dy != 0 {
                out.push(Input::Motion { dx, dy });
            }
            if let (Some(x), Some(y)) = (ax, ay) {
                if self.abs[app] != Some((x, y)) {
                    self.abs[app] = Some((x, y));
                    out.push(Input::Absolute { x, y });
                }
            }
            if phantom_state {
                continue;
            }
            let held = &mut self.held[app];
            let event = |usage: Usage, down: bool| {
                if usage >> 16 == u32::from(hid::PAGE_BUTTON) {
                    Input::Button { usage, down }
                } else {
                    Input::Key { usage, down }
                }
            };
            for &u in held.iter().filter(|u| !pressed.contains(u)) {
                out.push(event(u, false));
            }
            for &u in pressed.iter().filter(|u| !held.contains(u)) {
                out.push(event(u, true));
            }
            *held = pressed;
        }
    }

    /// The pad's size in millimetres (its X and Y ranges), for a touchpad.
    pub fn pad_mm(&self) -> Option<(f32, f32)> {
        let f = |u| {
            self.desc
                .fields
                .iter()
                .find(|f| self.desc.apps[f.app].class == Class::Touchpad && f.collection_usage == FINGER && f.usage_at(0) == Some(u))
                .map(|f| (f.logical_max - f.logical_min) as f32 * f.mm_per_unit().unwrap_or(0.1))
        };
        Some((f(hid::X)?, f(hid::Y)?))
    }

    fn touchpad(&mut self, app: usize, id: u8, body: &[u8], out: &mut Vec<Input>) {
        let fields: Vec<&hid::Field> = self.desc.fields.iter().filter(|f| f.app == app && f.report_id == id).collect();
        if fields.is_empty() {
            return;
        }
        let mut fingers: Vec<(u32, Contact)> = Vec::new();
        let (mut count, mut button) = (None, false);
        for f in fields {
            for k in 0..f.count {
                let Some(v) = f.value(body, k) else { return };
                let Some(u) = (if f.array { f.usage_of_index(v) } else { f.usage_at(k) }) else { continue };
                if f.collection_usage != FINGER {
                    match u {
                        CONTACT_COUNT => count = Some(v.max(0) as usize),
                        _ if u >> 16 == u32::from(hid::PAGE_BUTTON) => button |= v != 0,
                        _ => {}
                    }
                    continue;
                }
                let c = match fingers.iter_mut().find(|(c, _)| *c == f.collection) {
                    Some((_, c)) => c,
                    None => {
                        fingers.push((f.collection, Contact { id: 0, x: 0.0, y: 0.0, tip: false, confident: true }));
                        &mut fingers.last_mut().unwrap().1
                    }
                };
                let mm = |v: i64| (v - i64::from(f.logical_min)) as f32 * f.mm_per_unit().unwrap_or(0.1);
                match u {
                    TIP => c.tip = v != 0,
                    CONFIDENCE => c.confident = v != 0,
                    CONTACT_ID => c.id = v as u32,
                    hid::X => c.x = mm(v),
                    hid::Y => c.y = mm(v),
                    _ => {}
                }
            }
        }
        let p = &mut self.touch[app];
        match count {
            // Without a count field every report is a whole frame.
            None => *p = Pending { expect: fingers.len(), contacts: Vec::new(), button },
            Some(n) if n > 0 => *p = Pending { expect: n, contacts: Vec::new(), button },
            // A continuation report (hybrid mode), or an empty frame.
            Some(_) if p.expect == 0 => *p = Pending { expect: 0, contacts: Vec::new(), button },
            Some(_) => p.button |= button,
        }
        for (_, c) in fingers {
            if p.contacts.len() < p.expect {
                p.contacts.push(c);
            }
        }
        if p.contacts.len() >= p.expect {
            let p = std::mem::take(p);
            out.push(Input::Touch { contacts: p.contacts, button: p.button });
        }
    }

    /// A gamepad's buttons and axes, if it has a gamepad or joystick
    /// collection.
    pub fn pad_layout(&self) -> Option<PadLayout> {
        let mut l = PadLayout::default();
        let mut any = false;
        for f in self.desc.fields.iter().filter(|f| is_pad(self.desc.apps[f.app].class)) {
            any = true;
            for k in 0..f.count {
                if f.array {
                    let n = (i64::from(f.logical_max) - i64::from(f.logical_min) + 1).clamp(0, 256);
                    for v in 0..n {
                        if let Some(u) = f.usage_of_index(i64::from(f.logical_min) + v) {
                            if !l.buttons.contains(&u) {
                                l.buttons.push(u);
                            }
                        }
                    }
                    break;
                }
                let Some(u) = f.usage_at(k) else { continue };
                let list = match pad_control(f, u) {
                    PadControl::Button => &mut l.buttons,
                    PadControl::Axis | PadControl::Hat => &mut l.axes,
                    PadControl::Ignored => continue,
                };
                if !list.contains(&u) {
                    list.push(u);
                }
            }
        }
        any.then_some(l)
    }

    fn gamepad(&mut self, app: usize, id: u8, body: &[u8], out: &mut Vec<Input>) {
        let fields: Vec<&hid::Field> = self.desc.fields.iter().filter(|f| f.app == app && f.report_id == id).collect();
        if fields.is_empty() {
            return;
        }
        let mut pressed: Vec<Usage> = Vec::new();
        let mut axes: Vec<(Usage, i16)> = Vec::new();
        for f in fields {
            for k in 0..f.count {
                // A short report says nothing reliable: drop it whole.
                let Some(v) = f.value(body, k) else { return };
                if f.array {
                    if let Some(u) = f.usage_of_index(v) {
                        if !pressed.contains(&u) {
                            pressed.push(u);
                        }
                    }
                    continue;
                }
                let Some(u) = f.usage_at(k) else { continue };
                match pad_control(f, u) {
                    PadControl::Button => {
                        if v != 0 && !pressed.contains(&u) {
                            pressed.push(u);
                        }
                    }
                    PadControl::Axis => axes.push((u, scale_axis(v, f.logical_min, f.logical_max))),
                    PadControl::Hat => axes.push((u, hat_position(v, f.logical_min, f.logical_max))),
                    PadControl::Ignored => {}
                }
            }
        }
        let last = &mut self.pad[app];
        for (u, v) in axes {
            match last.iter_mut().find(|(l, _)| *l == u) {
                Some((_, old)) if *old == v => {}
                Some((_, old)) => {
                    *old = v;
                    out.push(Input::PadAxis { usage: u, value: v });
                }
                None => {
                    last.push((u, v));
                    out.push(Input::PadAxis { usage: u, value: v });
                }
            }
        }
        let held = &mut self.held[app];
        for &u in held.iter().filter(|u| !pressed.contains(u)) {
            out.push(Input::PadButton { usage: u, down: false });
        }
        for &u in pressed.iter().filter(|u| !held.contains(u)) {
            out.push(Input::PadButton { usage: u, down: true });
        }
        *held = pressed;
    }

    /// Releases for everything held: the device went away.
    pub fn release_all(&mut self, out: &mut Vec<Input>) {
        for (app, held) in self.held.iter_mut().enumerate() {
            let pad = is_pad(self.desc.apps[app].class);
            for u in held.drain(..) {
                out.push(if pad {
                    Input::PadButton { usage: u, down: false }
                } else if u >> 16 == u32::from(hid::PAGE_BUTTON) {
                    Input::Button { usage: u, down: false }
                } else {
                    Input::Key { usage: u, down: false }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::tests::{BOOT_KEYBOARD, GAMEPAD_16BIT, GAMING_MOUSE, GENERIC_JOYSTICK, QEMU_TABLET};
    use crate::hid::usage;

    fn dec(d: &[u8]) -> Decoder {
        Decoder::new(Descriptor::parse(d).unwrap())
    }
    fn run(d: &mut Decoder, r: &[u8]) -> Vec<Input> {
        let mut v = Vec::new();
        d.report(r, &mut v);
        v
    }
    fn key(id: u16, down: bool) -> Input {
        Input::Key { usage: usage(7, id), down }
    }

    fn pad_axis(id: u16, value: i16) -> Input {
        Input::PadAxis { usage: usage(1, id), value }
    }
    fn pad_button(id: u16, down: bool) -> Input {
        Input::PadButton { usage: usage(9, id), down }
    }

    /// A generic USB joystick (DragonRise-style): four 8-bit axes, an
    /// 8-way hat with a null state, twelve buttons, vendor bits.
    #[test]
    fn joystick_axes_hat_and_buttons() {
        let mut d = dec(GENERIC_JOYSTICK);
        assert_eq!(d.descriptor().apps[0].class, Class::Joystick);
        let l = d.pad_layout().unwrap();
        assert_eq!(l.axes, vec![usage(1, 0x30), usage(1, 0x31), usage(1, 0x32), usage(1, 0x35), HAT]);
        assert_eq!(l.buttons, (1..=12).map(|b| usage(9, b)).collect::<Vec<_>>(), "vendor bits are not buttons");
        // Centred: every axis once, the hat centred (null state 0xf).
        assert_eq!(
            run(&mut d, &[0x80, 0x80, 0x80, 0x80, 0x0f, 0, 0xff, 0xff]),
            vec![pad_axis(0x30, 128), pad_axis(0x31, 128), pad_axis(0x32, 128), pad_axis(0x35, 128), Input::PadAxis { usage: HAT, value: -1 }]
        );
        // Same report again: nothing.
        assert_eq!(run(&mut d, &[0x80, 0x80, 0x80, 0x80, 0x0f, 0, 0, 0]), vec![]);
        // X hard left, hat up-right, button 1.
        assert_eq!(
            run(&mut d, &[0x00, 0x80, 0x80, 0x80, 0x11, 0, 0, 0]),
            vec![pad_axis(0x30, -32768), Input::PadAxis { usage: HAT, value: 1 }, pad_button(1, true)]
        );
        // X hard right, button 12 too, then release all.
        assert_eq!(run(&mut d, &[0xff, 0x80, 0x80, 0x80, 0x11, 0x80, 0, 0]), vec![pad_axis(0x30, 32767), pad_button(12, true)]);
        let mut out = Vec::new();
        d.release_all(&mut out);
        assert_eq!(out, vec![pad_button(1, false), pad_button(12, false)]);
        // A short report is dropped whole.
        assert_eq!(run(&mut d, &[0x00, 0x80]), vec![]);
    }

    /// A Bluetooth-style gamepad: numbered report, 16-bit sticks, 10-bit
    /// trigger and brake (Simulation page), a 1..=8 hat, fifteen buttons.
    #[test]
    fn gamepad_sticks_triggers_hat() {
        let mut d = dec(GAMEPAD_16BIT);
        assert_eq!(d.descriptor().apps[0].class, Class::Gamepad);
        let l = d.pad_layout().unwrap();
        assert_eq!(l.axes, vec![usage(1, 0x30), usage(1, 0x31), usage(1, 0x33), usage(1, 0x34), usage(1, 0x32), usage(2, 0xc5), HAT]);
        assert_eq!(l.buttons.len(), 15);
        let report = |lx: u16, z: u16, hat: u8, buttons: u16| {
            let mut r = vec![1];
            for v in [lx, 0x8000, 0x8000, 0x8000] {
                r.extend(v.to_le_bytes());
            }
            r.extend(z.to_le_bytes());
            r.extend(0u16.to_le_bytes());
            r.push(hat);
            r.extend(buttons.to_le_bytes());
            r
        };
        let first = run(&mut d, &report(0x8000, 0, 0, 0));
        assert_eq!(first.len(), 7, "every axis once: {first:?}");
        assert_eq!(first[0], pad_axis(0x30, 0));
        assert_eq!(first[4], pad_axis(0x32, -32768));
        assert_eq!(first[6], Input::PadAxis { usage: HAT, value: -1 }, "0 is outside 1..=8: centred");
        assert_eq!(
            run(&mut d, &report(0xffff, 1023, 3, 0b101)),
            vec![pad_axis(0x30, 32767), pad_axis(0x32, 32767), Input::PadAxis { usage: HAT, value: 2 }, pad_button(1, true), pad_button(3, true)]
        );
        // Another report id: not this collection's.
        assert_eq!(run(&mut d, &[2, 0, 0]), vec![]);
    }

    #[test]
    fn keys_and_modifiers_press_and_release() {
        let mut d = dec(BOOT_KEYBOARD);
        assert_eq!(run(&mut d, &[0x02, 0, 0x04, 0, 0, 0, 0, 0]), vec![key(0xe1, true), key(0x04, true)], "shift, a");
        assert_eq!(run(&mut d, &[0x02, 0, 0x04, 0x05, 0, 0, 0, 0]), vec![key(0x05, true)]);
        assert_eq!(run(&mut d, &[0, 0, 0x05, 0, 0, 0, 0, 0]), vec![key(0xe1, false), key(0x04, false)]);
        assert_eq!(run(&mut d, &[0, 0, 1, 1, 1, 1, 1, 1]), vec![], "rollover keeps the last state");
        assert_eq!(run(&mut d, &[0; 8]), vec![key(0x05, false)]);
        assert_eq!(run(&mut d, &[0; 3]), vec![], "short report");
    }

    #[test]
    fn an_nkro_bitmap_keyboard() {
        // Report 1: 8 modifier bits, then a 104-bit bitmap from usage 0.
        let desc = [
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
            0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x19, 0x00, 0x29, 0x67, 0x95, 0x68, 0x81, 0x02, 0xc0,
        ];
        let mut d = dec(&desc);
        let mut r = [0u8; 15];
        r[0] = 1;
        r[2] = 0b0001_0000; // usage 4 (a)
        r[3] = 0b0000_0001; // usage 8 (e)
        assert_eq!(run(&mut d, &r), vec![key(0x04, true), key(0x08, true)]);
    }

    #[test]
    fn the_tablet_moves_absolutely_and_scrolls() {
        let mut d = dec(QEMU_TABLET);
        let ev = run(&mut d, &[0b001, 0xff, 0x3f, 0xff, 0x7f, 0x01]);
        assert_eq!(ev[0], Input::Scroll { axis: Axis::Vertical, detents: 1 });
        let Input::Absolute { x, y } = ev[1] else { panic!("{ev:?}") };
        assert!((x - 0.5).abs() < 0.001 && (y - 1.0).abs() < 0.001);
        assert_eq!(ev[2], Input::Button { usage: usage(9, 1), down: true });
        assert_eq!(run(&mut d, &[0b001, 0xff, 0x3f, 0xff, 0x7f, 0]), vec![], "nothing changed");
    }

    #[test]
    fn a_report_protocol_mouse() {
        let mut d = dec(GAMING_MOUSE);
        let r = [1, 0x00, 0x80, 0x10, 0x00, 0xf0, 0xff, 0xff, 0x01];
        assert_eq!(
            run(&mut d, &r),
            vec![
                Input::Scroll { axis: Axis::Vertical, detents: -1 },
                Input::Scroll { axis: Axis::Horizontal, detents: 1 },
                Input::Motion { dx: 16, dy: -16 },
                Input::Button { usage: usage(9, 16), down: true },
            ]
        );
        assert_eq!(run(&mut d, &[2, 1, 2, 3]), vec![], "another report id");
        let mut out = Vec::new();
        d.release_all(&mut out);
        assert_eq!(out, vec![Input::Button { usage: usage(9, 16), down: false }]);
    }

    #[test]
    fn media_keys_through_the_same_decoder() {
        // A keyboard's second interface (consumer.rs's ARRAY descriptor).
        let desc = [
            0x05, 0x01, 0x09, 0x80, 0xa1, 0x01, 0x85, 0x01, 0x19, 0x81, 0x29, 0x83, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
            0x95, 0x03, 0x81, 0x02, 0x95, 0x05, 0x81, 0x01, 0xc0, 0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x02, 0x19,
            0x00, 0x2a, 0x3c, 0x02, 0x15, 0x00, 0x26, 0x3c, 0x02, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xc0,
        ];
        let mut d = dec(&desc);
        assert_eq!(d.descriptor().classes(), vec![Class::SystemKeys, Class::MediaKeys]);
        assert_eq!(run(&mut d, &[2, 0xcd, 0]), vec![Input::Key { usage: usage(0x0c, 0xcd), down: true }]);
        assert_eq!(run(&mut d, &[2, 0, 0]), vec![Input::Key { usage: usage(0x0c, 0xcd), down: false }]);
        assert_eq!(run(&mut d, &[1, 0b010]), vec![Input::Key { usage: usage(1, 0x82), down: true }], "sleep");
    }

    /// A short item: `prefix` is the item byte with size bits 0; the
    /// size follows the value.
    fn item(d: &mut Vec<u8>, prefix: u8, v: i64) {
        let b = v.to_le_bytes();
        let n = if (-128..=127).contains(&v) || (0..=255).contains(&v) && prefix & 0xf0 != 0x10 { 1 } else if (-32768..=65535).contains(&v) { 2 } else { 4 };
        let n = if v > 127 && prefix == 0x14 { 2 } else { n };
        d.push(prefix | [0, 1, 2, 0, 3][n]);
        d.extend_from_slice(&b[..n]);
    }

    /// A Precision Touchpad: report 1, `fingers` Finger collections
    /// (confidence, tip, 4-bit id, 16-bit X/Y over 100 x 60 mm), scan
    /// time, contact count, one button.
    fn ptp(fingers: usize) -> Vec<u8> {
        let mut d = Vec::new();
        let i = |d: &mut Vec<u8>, p, v| item(d, p, v);
        i(&mut d, 0x04, 0x0d); i(&mut d, 0x08, 0x05); i(&mut d, 0xa0, 1); i(&mut d, 0x84, 1);
        for _ in 0..fingers {
            i(&mut d, 0x04, 0x0d); i(&mut d, 0x08, 0x22); i(&mut d, 0xa0, 2);
            i(&mut d, 0x14, 0); i(&mut d, 0x24, 1); i(&mut d, 0x74, 1); i(&mut d, 0x94, 1);
            i(&mut d, 0x08, 0x47); i(&mut d, 0x80, 2); i(&mut d, 0x08, 0x42); i(&mut d, 0x80, 2);
            i(&mut d, 0x94, 6); i(&mut d, 0x80, 3);
            i(&mut d, 0x74, 8); i(&mut d, 0x94, 1); i(&mut d, 0x24, 15); i(&mut d, 0x08, 0x51); i(&mut d, 0x80, 2);
            i(&mut d, 0x04, 0x01); i(&mut d, 0x74, 16); i(&mut d, 0x54, 0x0e); i(&mut d, 0x64, 0x11);
            i(&mut d, 0x34, 0); i(&mut d, 0x44, 1000); i(&mut d, 0x24, 1000); i(&mut d, 0x08, 0x30); i(&mut d, 0x80, 2);
            i(&mut d, 0x44, 600); i(&mut d, 0x24, 600); i(&mut d, 0x08, 0x31); i(&mut d, 0x80, 2);
            i(&mut d, 0x44, 0); i(&mut d, 0x54, 0); i(&mut d, 0x64, 0);
            d.push(0xc0);
        }
        i(&mut d, 0x04, 0x0d); i(&mut d, 0x74, 16); i(&mut d, 0x24, 0xffff); i(&mut d, 0x08, 0x56); i(&mut d, 0x80, 2);
        i(&mut d, 0x74, 8); i(&mut d, 0x24, 127); i(&mut d, 0x08, 0x54); i(&mut d, 0x80, 2);
        i(&mut d, 0x04, 0x09); i(&mut d, 0x08, 1); i(&mut d, 0x24, 1); i(&mut d, 0x74, 1); i(&mut d, 0x80, 2);
        i(&mut d, 0x94, 7); i(&mut d, 0x80, 3);
        d.push(0xc0);
        d
    }

    /// One finger's bytes: flags, id, X, Y (device units: 0.1 mm).
    fn finger(tip: bool, id: u8, x: u16, y: u16) -> Vec<u8> {
        let mut v = vec![u8::from(tip) << 1 | 1, id];
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v
    }

    #[test]
    fn a_precision_touchpad_in_parallel_and_hybrid_mode() {
        let mut d = dec(&ptp(2));
        assert_eq!(d.descriptor().classes(), vec![Class::Touchpad]);
        let (w, h) = d.pad_mm().unwrap();
        assert!((w - 100.0).abs() < 0.01 && (h - 60.0).abs() < 0.01, "{w} x {h}");
        // Parallel: both fingers in one report, count 2, button down.
        let mut r = vec![1];
        r.extend(finger(true, 3, 100, 200));
        r.extend(finger(true, 4, 500, 200));
        r.extend([0x10, 0x00, 2, 1]);
        let ev = run(&mut d, &r);
        let [Input::Touch { contacts, button: true }] = &ev[..] else { panic!("{ev:?}") };
        assert_eq!(contacts.len(), 2);
        assert_eq!((contacts[1].id, contacts[1].tip, contacts[1].confident), (4, true, true));
        assert!((contacts[1].x - 50.0).abs() < 0.01 && (contacts[1].y - 20.0).abs() < 0.01);
        // Hybrid on a one-finger-per-report pad: count 2, then the second.
        let mut d = dec(&ptp(1));
        let mut a = vec![1];
        a.extend(finger(true, 1, 10, 10));
        a.extend([0, 0, 2, 0]);
        assert_eq!(run(&mut d, &a), vec![], "waits for the second contact");
        let mut b = vec![1];
        b.extend(finger(false, 2, 20, 20));
        b.extend([0, 0, 0, 0]);
        let ev = run(&mut d, &b);
        let [Input::Touch { contacts, button: false }] = &ev[..] else { panic!("{ev:?}") };
        assert_eq!(contacts.iter().map(|c| (c.id, c.tip)).collect::<Vec<_>>(), vec![(1, true), (2, false)]);
    }
}
