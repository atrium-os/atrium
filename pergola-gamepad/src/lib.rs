//! pergola-gamepad — standard gamepad names over Fresco's gamepad events.
//!
//! Fresco hands an app HID-faithful gamepad events (atrium-input.md §4.1,
//! §5.4): a connect with the vendor, product and counts, then buttons by
//! HID usage (page 9 buttons, Generic Desktop D-pad usages) and axes by
//! usage, every axis scaled to -32768..=32767 and the hat as 0..=7
//! clockwise from up (-1 centred). Which button is "A" is not on the wire:
//! that is a convenience this crate layers on top (§7).
//!
//! [`Pads`] keeps one [`Pad`] per device. Feed it each event
//! ([`Pads::connect`], [`Pads::button`], [`Pads::axis`],
//! [`Pads::disconnect`]); each call returns the [`Change`]s in standard
//! names, and [`Pad`] holds the current state. The names follow the W3C
//! Gamepad "standard" layout, with XInput-style face names ([`Button::South`]
//! is A on an Xbox pad, Cross on a PlayStation pad).
//!
//! Mapping: a [`Mapping`] says which HID usage is which standard control.
//! [`Mapping::for_device`] picks a known controller's mapping by vendor and
//! product, else [`Mapping::generic`]: page 9 buttons in the standard
//! layout's order, X/Y the left stick, Rx/Ry (or else Z/Rz) the right
//! stick, Simulation Brake/Accelerator (or else Z/Rz, when Rx/Ry are the
//! right stick) the triggers, the hat and the D-pad usages the D-pad.
//! Which axes a pad has is known only from its events, so the generic
//! mapping holds axis events back until the pad has reported as many axes
//! as its connect counted (Fresco sends every axis right after a connect),
//! or a button arrives, then decides. An
//! app can build its own ([`Mapping::set_button`], [`Mapping::set_axis`])
//! for a controller it knows better.
//!
//! Example, with fresco-client's events:
//!
//! ```ignore
//! match ev {
//!     Event::GamepadConnect { device_id, vendor, product, buttons, axes, name, .. } =>
//!         pads.connect(device_id, vendor, product, buttons, axes, &name),
//!     Event::GamepadButton { device_id, usage, pressed, .. } => pads.button(device_id, usage, pressed),
//!     Event::GamepadAxis { device_id, usage, value, .. } => pads.axis(device_id, usage, value),
//!     Event::GamepadDisconnect { device_id, .. } => pads.disconnect(device_id),
//!     _ => vec![],
//! }
//! ```

#![forbid(unsafe_code)]

use std::collections::HashMap;

/// HID usages this crate knows (page << 16 | usage).
pub mod usage {
    /// Page 9 button `n` (1-based).
    pub const fn button(n: u16) -> u32 {
        0x0009_0000 | n as u32
    }
    pub const X: u32 = 0x0001_0030;
    pub const Y: u32 = 0x0001_0031;
    pub const Z: u32 = 0x0001_0032;
    pub const RX: u32 = 0x0001_0033;
    pub const RY: u32 = 0x0001_0034;
    pub const RZ: u32 = 0x0001_0035;
    pub const HAT: u32 = 0x0001_0039;
    pub const DPAD_UP: u32 = 0x0001_0090;
    pub const DPAD_DOWN: u32 = 0x0001_0091;
    pub const DPAD_RIGHT: u32 = 0x0001_0092;
    pub const DPAD_LEFT: u32 = 0x0001_0093;
    pub const ACCELERATOR: u32 = 0x0002_00c4;
    pub const BRAKE: u32 = 0x0002_00c5;
}

/// A standard button (W3C Gamepad "standard" layout, XInput-style names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Button {
    /// A / Cross.
    South,
    /// B / Circle.
    East,
    /// X / Square.
    West,
    /// Y / Triangle.
    North,
    LeftShoulder,
    RightShoulder,
    /// A trigger's digital click, where the pad reports one.
    LeftTrigger,
    RightTrigger,
    /// Back / View / Share / Select.
    Back,
    /// Start / Menu / Options.
    Start,
    LeftStick,
    RightStick,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    /// Xbox / PS / Home.
    Guide,
}

impl Button {
    /// All, in the standard layout's index order.
    pub const ALL: [Button; 17] = [
        Button::South, Button::East, Button::West, Button::North,
        Button::LeftShoulder, Button::RightShoulder, Button::LeftTrigger, Button::RightTrigger,
        Button::Back, Button::Start, Button::LeftStick, Button::RightStick,
        Button::DpadUp, Button::DpadDown, Button::DpadLeft, Button::DpadRight, Button::Guide,
    ];
}

/// A standard axis. Sticks are -1..=1 (right and down positive, as HID
/// reports them); triggers are 0..=1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Axis {
    LeftX,
    LeftY,
    RightX,
    RightY,
    LeftTrigger,
    RightTrigger,
}

/// What an event changed, in standard names.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    Connected { device: u32 },
    Disconnected { device: u32 },
    Button { device: u32, button: Button, pressed: bool },
    /// The axis's new value after the dead zone ([`Pad::axis`]).
    Axis { device: u32, axis: Axis, value: f32 },
}

/// Which HID usage is which standard control.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mapping {
    buttons: HashMap<u32, Button>,
    axes: HashMap<u32, Axis>,
    /// Whether the hat drives the D-pad buttons.
    hat_dpad: bool,
    /// The right stick and triggers are still to be decided from the
    /// axes the pad has ([`Mapping::resolve`]).
    auto_axes: bool,
}

impl Mapping {
    /// No controls mapped.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The generic mapping (see the crate docs); its right stick and
    /// triggers are decided by [`Mapping::resolve`].
    pub fn generic() -> Self {
        let mut m = Self::empty();
        for (i, b) in Button::ALL.iter().enumerate() {
            m.buttons.insert(usage::button(i as u16 + 1), *b);
        }
        m.dpad_usages();
        m.axes.insert(usage::X, Axis::LeftX);
        m.axes.insert(usage::Y, Axis::LeftY);
        m.axes.insert(usage::BRAKE, Axis::LeftTrigger);
        m.axes.insert(usage::ACCELERATOR, Axis::RightTrigger);
        m.hat_dpad = true;
        m.auto_axes = true;
        m
    }

    /// Whether the right stick and triggers still wait for
    /// [`Mapping::resolve`].
    pub fn unresolved(&self) -> bool {
        self.auto_axes
    }

    /// Decide the generic right stick and triggers from the axis usages a
    /// pad has: Rx/Ry the right stick if it has either, else Z/Rz; then
    /// Z/Rz the triggers if they are free and it has no Brake/Accelerator.
    /// Usages already mapped stay as they are.
    pub fn resolve(&mut self, has: &[u32]) {
        if !self.auto_axes {
            return;
        }
        self.auto_axes = false;
        let has = |u| has.contains(&u);
        let r_on_rx = has(usage::RX) || has(usage::RY);
        let (rx, ry) = if r_on_rx { (usage::RX, usage::RY) } else { (usage::Z, usage::RZ) };
        self.axes.entry(rx).or_insert(Axis::RightX);
        self.axes.entry(ry).or_insert(Axis::RightY);
        if r_on_rx && !has(usage::BRAKE) && !has(usage::ACCELERATOR) {
            self.axes.entry(usage::Z).or_insert(Axis::LeftTrigger);
            self.axes.entry(usage::RZ).or_insert(Axis::RightTrigger);
        }
    }

    /// Sony DualShock 4 and DualSense in HID mode (USB and Bluetooth):
    /// Square, Cross, Circle, Triangle, L1, R1, L2, R2, Share/Create,
    /// Options, L3, R3, PS, touchpad click; Z/Rz the right stick, Rx/Ry the
    /// triggers.
    pub fn sony() -> Self {
        let mut m = Self::empty();
        let order = [
            Button::West, Button::South, Button::East, Button::North,
            Button::LeftShoulder, Button::RightShoulder, Button::LeftTrigger, Button::RightTrigger,
            Button::Back, Button::Start, Button::LeftStick, Button::RightStick, Button::Guide,
        ];
        for (i, b) in order.iter().enumerate() {
            m.buttons.insert(usage::button(i as u16 + 1), *b);
        }
        m.dpad_usages();
        m.axes.insert(usage::X, Axis::LeftX);
        m.axes.insert(usage::Y, Axis::LeftY);
        m.axes.insert(usage::Z, Axis::RightX);
        m.axes.insert(usage::RZ, Axis::RightY);
        m.axes.insert(usage::RX, Axis::LeftTrigger);
        m.axes.insert(usage::RY, Axis::RightTrigger);
        m.hat_dpad = true;
        m
    }

    /// The mapping for a known controller, else [`Mapping::generic`].
    pub fn for_device(vendor: u16, product: u16) -> Self {
        const SONY: u16 = 0x054c;
        // DualShock 4 (two revisions), DualShock 4 wireless adapter,
        // DualSense, DualSense Edge.
        const SONY_PADS: [u16; 5] = [0x05c4, 0x09cc, 0x0ba0, 0x0ce6, 0x0df2];
        if vendor == SONY && SONY_PADS.contains(&product) {
            return Self::sony();
        }
        Self::generic()
    }

    fn dpad_usages(&mut self) {
        self.buttons.insert(usage::DPAD_UP, Button::DpadUp);
        self.buttons.insert(usage::DPAD_DOWN, Button::DpadDown);
        self.buttons.insert(usage::DPAD_RIGHT, Button::DpadRight);
        self.buttons.insert(usage::DPAD_LEFT, Button::DpadLeft);
    }

    /// Map HID usage `usage` to `button` (replacing what it was).
    pub fn set_button(&mut self, usage: u32, button: Button) -> &mut Self {
        self.buttons.insert(usage, button);
        self
    }

    /// Map HID usage `usage` to `axis` (replacing what it was). Usages
    /// mapped this way are kept by [`Mapping::resolve`].
    pub fn set_axis(&mut self, usage: u32, axis: Axis) -> &mut Self {
        self.axes.insert(usage, axis);
        self
    }

    /// Unmap HID usage `usage`.
    pub fn unset(&mut self, usage: u32) -> &mut Self {
        self.buttons.remove(&usage);
        self.axes.remove(&usage);
        self
    }

    /// Whether the hat switch drives the D-pad.
    pub fn set_hat_dpad(&mut self, on: bool) -> &mut Self {
        self.hat_dpad = on;
        self
    }
}

/// The D-pad directions a hat value (0..=7 clockwise from up, else
/// centred) holds: up, down, left, right.
fn hat_dirs(v: i32) -> [bool; 4] {
    match v {
        0 => [true, false, false, false],
        1 => [true, false, false, true],
        2 => [false, false, false, true],
        3 => [false, true, false, true],
        4 => [false, true, false, false],
        5 => [false, true, true, false],
        6 => [false, false, true, false],
        7 => [true, false, true, false],
        _ => [false; 4],
    }
}

/// One connected pad.
#[derive(Debug, Clone)]
pub struct Pad {
    pub device: u32,
    pub vendor: u16,
    pub product: u16,
    pub name: String,
    /// Button and axis counts the connect gave.
    pub counts: (u16, u16),
    pub mapping: Mapping,
    /// Stick dead zone, as a fraction of full deflection (radial, per
    /// stick); default 0.15.
    pub stick_dead_zone: f32,
    /// Trigger dead zone, as a fraction; default 0.05.
    pub trigger_dead_zone: f32,
    pressed: HashMap<Button, bool>,
    /// Raw axis values by standard axis, -1..=1 (sticks) or 0..=1 (triggers).
    raw: HashMap<Axis, f32>,
    /// Reported (dead-zoned) values.
    shown: HashMap<Axis, f32>,
    /// Which D-pad directions each source holds: [hat, usages].
    dpad: [[bool; 4]; 2],
    /// Axis events held back while the generic mapping is unresolved.
    held_back: Vec<(u32, i32)>,
}

const DPAD: [Button; 4] = [Button::DpadUp, Button::DpadDown, Button::DpadLeft, Button::DpadRight];

impl Pad {
    fn new(device: u32, vendor: u16, product: u16, counts: (u16, u16), name: &str) -> Self {
        Pad {
            device,
            vendor,
            product,
            name: name.to_string(),
            counts,
            mapping: Mapping::for_device(vendor, product),
            stick_dead_zone: 0.15,
            trigger_dead_zone: 0.05,
            pressed: HashMap::new(),
            raw: HashMap::new(),
            shown: HashMap::new(),
            dpad: [[false; 4]; 2],
            held_back: Vec::new(),
        }
    }

    /// Decide the generic mapping's axes now and replay what was held back.
    fn settle(&mut self, out: &mut Vec<Change>) {
        if !self.mapping.unresolved() {
            return;
        }
        let has: Vec<u32> = self.held_back.iter().map(|e| e.0).collect();
        self.mapping.resolve(&has);
        for (u, v) in std::mem::take(&mut self.held_back) {
            self.axis_event(u, v, out);
        }
    }

    /// Whether `b` is held.
    pub fn pressed(&self, b: Button) -> bool {
        self.pressed.get(&b).copied().unwrap_or(false)
    }

    /// `a` after its dead zone: sticks -1..=1, triggers 0..=1; 0 if never
    /// reported.
    pub fn axis(&self, a: Axis) -> f32 {
        self.shown.get(&a).copied().unwrap_or(0.0)
    }

    /// A stick as (x, y) after its radial dead zone.
    pub fn stick(&self, left: bool) -> (f32, f32) {
        if left {
            (self.axis(Axis::LeftX), self.axis(Axis::LeftY))
        } else {
            (self.axis(Axis::RightX), self.axis(Axis::RightY))
        }
    }

    fn set_button(&mut self, b: Button, down: bool, out: &mut Vec<Change>) {
        if self.pressed(b) != down {
            self.pressed.insert(b, down);
            out.push(Change::Button { device: self.device, button: b, pressed: down });
        }
    }

    /// The D-pad is held where either the hat or the D-pad usages hold it.
    fn set_dpad(&mut self, source: usize, dirs: [bool; 4], out: &mut Vec<Change>) {
        self.dpad[source] = dirs;
        for (i, b) in DPAD.iter().enumerate() {
            let down = self.dpad[0][i] || self.dpad[1][i];
            self.set_button(*b, down, out);
        }
    }

    fn button(&mut self, usage: u32, down: bool, out: &mut Vec<Change>) {
        self.settle(out);
        let Some(&b) = self.mapping.buttons.get(&usage) else { return };
        if let Some(i) = DPAD.iter().position(|d| *d == b) {
            let mut dirs = self.dpad[1];
            dirs[i] = down;
            self.set_dpad(1, dirs, out);
        } else {
            self.set_button(b, down, out);
        }
    }

    fn axis_event(&mut self, usage: u32, value: i32, out: &mut Vec<Change>) {
        if self.mapping.unresolved() {
            self.held_back.push((usage, value));
            let mut seen: Vec<u32> = self.held_back.iter().map(|e| e.0).collect();
            seen.sort();
            seen.dedup();
            if seen.len() >= self.counts.1 as usize || self.held_back.len() >= 64 {
                self.settle(out);
            }
            return;
        }
        if usage == usage::HAT && self.mapping.hat_dpad {
            self.set_dpad(0, hat_dirs(value), out);
            return;
        }
        let Some(&a) = self.mapping.axes.get(&usage) else { return };
        let v = value.clamp(-32768, 32767) as f32;
        let raw = match a {
            Axis::LeftTrigger | Axis::RightTrigger => (v + 32768.0) / 65535.0,
            _ => (v / 32767.0).max(-1.0),
        };
        self.raw.insert(a, raw);
        let pair = match a {
            Axis::LeftX | Axis::LeftY => Some((Axis::LeftX, Axis::LeftY)),
            Axis::RightX | Axis::RightY => Some((Axis::RightX, Axis::RightY)),
            _ => None,
        };
        match pair {
            Some((ax, ay)) => {
                let (x, y) = (self.raw.get(&ax).copied().unwrap_or(0.0), self.raw.get(&ay).copied().unwrap_or(0.0));
                let (x, y) = radial(x, y, self.stick_dead_zone);
                self.show(ax, x, out);
                self.show(ay, y, out);
            }
            None => {
                let t = rescale(raw, self.trigger_dead_zone);
                self.show(a, t, out);
            }
        }
    }

    fn show(&mut self, a: Axis, v: f32, out: &mut Vec<Change>) {
        // Every axis starts at rest (0).
        if self.shown.get(&a).copied().unwrap_or(0.0) != v {
            self.shown.insert(a, v);
            out.push(Change::Axis { device: self.device, axis: a, value: v });
        }
    }

    /// Release everything held, as changes (on disconnect).
    fn release_all(&mut self, out: &mut Vec<Change>) {
        let mut held: Vec<Button> = self.pressed.iter().filter(|(_, d)| **d).map(|(b, _)| *b).collect();
        held.sort();
        for b in held {
            self.set_button(b, false, out);
        }
        self.dpad = [[false; 4]; 2];
    }
}

/// `v` (0..=1) with the bottom `dz` cut and the rest stretched to 0..=1.
fn rescale(v: f32, dz: f32) -> f32 {
    if v <= dz {
        0.0
    } else {
        ((v - dz) / (1.0 - dz)).min(1.0)
    }
}

/// A stick's (x, y) with a radial dead zone of radius `dz`, the rest of
/// the travel stretched so full deflection stays 1.
fn radial(x: f32, y: f32, dz: f32) -> (f32, f32) {
    let m = (x * x + y * y).sqrt();
    if m <= dz || m == 0.0 {
        return (0.0, 0.0);
    }
    let k = rescale(m.min(1.0), dz) / m;
    ((x * k).clamp(-1.0, 1.0), (y * k).clamp(-1.0, 1.0))
}

/// Every connected pad, by Fresco device id.
#[derive(Debug, Default)]
pub struct Pads {
    pads: HashMap<u32, Pad>,
}

impl Pads {
    pub fn new() -> Self {
        Self::default()
    }

    /// A pad connected (or this window was given it). A connect for a
    /// device already held replaces it.
    pub fn connect(&mut self, device: u32, vendor: u16, product: u16, buttons: u16, axes: u16, name: &str) -> Vec<Change> {
        self.pads.insert(device, Pad::new(device, vendor, product, (buttons, axes), name));
        vec![Change::Connected { device }]
    }

    /// The pad is gone (or this window lost it): its held buttons are
    /// released first.
    pub fn disconnect(&mut self, device: u32) -> Vec<Change> {
        let mut out = Vec::new();
        if let Some(mut p) = self.pads.remove(&device) {
            p.release_all(&mut out);
            out.push(Change::Disconnected { device });
        }
        out
    }

    /// A button event (HID usage).
    pub fn button(&mut self, device: u32, usage: u32, pressed: bool) -> Vec<Change> {
        let mut out = Vec::new();
        if let Some(p) = self.pads.get_mut(&device) {
            p.button(usage, pressed, &mut out);
        }
        out
    }

    /// An axis event (HID usage, -32768..=32767; the hat 0..=7 or -1).
    pub fn axis(&mut self, device: u32, usage: u32, value: i32) -> Vec<Change> {
        let mut out = Vec::new();
        if let Some(p) = self.pads.get_mut(&device) {
            p.axis_event(usage, value, &mut out);
        }
        out
    }

    pub fn get(&self, device: u32) -> Option<&Pad> {
        self.pads.get(&device)
    }

    /// To change a pad's mapping or dead zones.
    pub fn get_mut(&mut self, device: u32) -> Option<&mut Pad> {
        self.pads.get_mut(&device)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Pad> {
        self.pads.values()
    }
}
