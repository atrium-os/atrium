//! Native FreeBSD keyboard input reader.
//!
//! Opens `/dev/input/event0` (FreeBSD evdev — kbdmux0 multiplexer) and
//! decodes each 24-byte `struct input_event` record. EV_KEY events are
//! translated from Linux keycodes to USB HID Usage Page 0x07 codes
//! (so the wire stays HID-shaped per the protocol contract) and fanned
//! out as `COMP_INPUT_KEY` Completions through `event_subs`.
//!
//! Why evdev as the source despite the project's "no Linux-shape
//! input" rule: evdev is the path of least resistance on FreeBSD-virt
//! today (kbdmux0 already plumbs into it), and the wire stays HID.
//! A future iteration replaces this with a direct `/dev/kbd0` K_RAW
//! reader translating AT scan codes — same output, no Linux library
//! shape touched.

use fresco_server::command::protocol::{Completion, COMP_INPUT_KEY};

use std::fs::File;
use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

const EV_KEY: u16 = 0x01;

#[repr(C)]
#[derive(Default, Copy, Clone)]
struct InputEvent {
    tv_sec:  i64,
    tv_usec: i64,
    type_:   u16,
    code:    u16,
    value:   i32,
}

/// Linux keycode → USB HID Usage Page 0x07 code. Returns None for
/// keys we don't translate yet. Modifier keys are mapped to their
/// HID modifier bits (returned via [`MODIFIER_MASK`]) instead, so the
/// regular hid-usage path doesn't see them.
fn linux_to_hid(linux_code: u16) -> Option<u16> {
    Some(match linux_code {
        // Letters: KEY_A..KEY_Z go in scattered Linux order. Use direct map.
        30 => 0x04, // A
        48 => 0x05, // B
        46 => 0x06, // C
        32 => 0x07, // D
        18 => 0x08, // E
        33 => 0x09, // F
        34 => 0x0a, // G
        35 => 0x0b, // H
        23 => 0x0c, // I
        36 => 0x0d, // J
        37 => 0x0e, // K
        38 => 0x0f, // L
        50 => 0x10, // M
        49 => 0x11, // N
        24 => 0x12, // O
        25 => 0x13, // P
        16 => 0x14, // Q
        19 => 0x15, // R
        31 => 0x16, // S
        20 => 0x17, // T
        22 => 0x18, // U
        47 => 0x19, // V
        17 => 0x1a, // W
        45 => 0x1b, // X
        21 => 0x1c, // Y
        44 => 0x1d, // Z

        // Digits.
        2  => 0x1e, // 1
        3  => 0x1f, // 2
        4  => 0x20, // 3
        5  => 0x21, // 4
        6  => 0x22, // 5
        7  => 0x23, // 6
        8  => 0x24, // 7
        9  => 0x25, // 8
        10 => 0x26, // 9
        11 => 0x27, // 0

        28 => 0x28, // Enter
        1  => 0x29, // Escape
        14 => 0x2a, // Backspace
        15 => 0x2b, // Tab
        57 => 0x2c, // Space

        12 => 0x2d, // -
        13 => 0x2e, // =
        26 => 0x2f, // [
        27 => 0x30, // ]
        43 => 0x31, // backslash
        39 => 0x33, // ;
        40 => 0x34, // '
        41 => 0x35, // `
        51 => 0x36, // ,
        52 => 0x37, // .
        53 => 0x38, // /

        58 => 0x39, // CapsLock

        // Arrows.
        106 => 0x4f, // Right
        105 => 0x50, // Left
        108 => 0x51, // Down
        103 => 0x52, // Up

        _ => return None,
    })
}

/// Update the modifier bitmap in response to a modifier key event.
/// Returns true if the key was consumed (i.e. don't emit a regular
/// HID Usage event for it).
fn update_modifiers(linux_code: u16, pressed: bool, mods: &mut u8) -> bool {
    const SHIFT: u8 = 0x01;
    const CTRL:  u8 = 0x02;
    const ALT:   u8 = 0x04;
    let bit = match linux_code {
        42 | 54 => SHIFT, // L/R Shift
        29 | 97 => CTRL,  // L/R Ctrl
        56 | 100 => ALT,  // L/R Alt
        _ => return false,
    };
    if pressed { *mods |= bit; } else { *mods &= !bit; }
    true
}

pub fn spawn(event_subs: Arc<Mutex<Vec<Sender<Completion>>>>) {
    std::thread::Builder::new()
        .name("atrium-input-evdev".into())
        .spawn(move || run(event_subs))
        .expect("spawn input reader");
}

fn run(event_subs: Arc<Mutex<Vec<Sender<Completion>>>>) {
    // Find a keyboard event device. On `-machine virt` with usb-kbd,
    // hkbd0 lands on its own /dev/input/eventN (typically event3),
    // separate from kbdmux0's event0 which sees only console-style
    // input (none on a headless arm64 virt). Probe the first ~16
    // event devices and pick one whose evdev name contains "kbd"
    // or "keyboard".
    let mut path = String::new();
    for n in 0..16 {
        let p = format!("/dev/input/event{n}");
        if !std::path::Path::new(&p).exists() { continue; }
        // Read sysctl-style metadata: kern.evdev.input.N.name
        let key = format!("kern.evdev.input.{n}.name");
        let out = std::process::Command::new("sysctl")
            .args(["-n", &key])
            .output();
        let name = out.ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.contains("kbd") || name.contains("keyboard") {
            // Skip kbdmux on bare virt (no underlying keyboards
            // feed it; we want the actual hkbd device).
            if name.contains("multiplexer") { continue; }
            path = p;
            eprintln!("input: matched event{n} = {name:?}");
            break;
        }
    }
    if path.is_empty() {
        // Fallback to event0; better to attach there than not at all.
        path = "/dev/input/event0".to_string();
        eprintln!("input: no keyboard device matched by name; falling back to {path}");
    }

    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("input: cannot open {path}: {e}");
            return;
        }
    };
    eprintln!("input: reading {path}");

    let mut buf = [0u8; std::mem::size_of::<InputEvent>()];
    let mut modifiers: u8 = 0;

    loop {
        if file.read_exact(&mut buf).is_err() {
            eprintln!("input: read error / EOF — input thread exiting");
            return;
        }
        // SAFETY: InputEvent is repr(C) over plain integers; `buf` is
        // 24 bytes (verified via sizeof) so the transmute is sound.
        let ev: InputEvent = unsafe { std::mem::transmute(buf) };

        if ev.type_ != EV_KEY {
            continue;
        }
        // value: 0=release, 1=press, 2=repeat. Treat repeat as press.
        let pressed = ev.value != 0;

        if update_modifiers(ev.code, pressed, &mut modifiers) {
            continue;
        }
        // Skip auto-repeat — we want one event per logical keystroke.
        // Apps that want repeat can implement it on the client side
        // from the timing of repeated press events.
        if ev.value == 2 {
            continue;
        }

        let Some(hid) = linux_to_hid(ev.code) else { continue; };

        let mut result_hash = [0u8; 32];
        result_hash[0..2].copy_from_slice(&hid.to_le_bytes());
        result_hash[2] = modifiers;
        let comp = Completion {
            comp_type:   COMP_INPUT_KEY,
            status:      if pressed { 1 } else { 0 },
            id:          0, // broadcast for v0.1; per-window-focus routing later
            result_hash,
            _pad:        [0u32; 22],
        };

        let mut subs = event_subs.lock().unwrap();
        subs.retain(|tx| tx.send(comp).is_ok());
    }
}
