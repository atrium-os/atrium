//! Native FreeBSD keyboard input reader.
//!
//! Reads raw HID reports directly from the keyboard's `/dev/hidraw*`
//! cdev — `hidraw(4)` is the modern FreeBSD HID-bus interface
//! (`sys/dev/hid/hidraw.c`), pure FreeBSD, no Linux library shape.
//! USB HID Keyboard input reports use the boot-protocol layout:
//!
//!   byte 0:    modifiers bitmap (LCtrl=1, LShift=2, LAlt=4, LGui=8,
//!                                RCtrl=16, RShift=32, RAlt=64, RGui=128)
//!   byte 1:    reserved
//!   bytes 2-7: up to 6 simultaneously-pressed HID Usage Page 0x07 codes
//!
//! Crucially, **the report bytes are already in the wire format we
//! want** — no AT-scan-code translation, no Linux keycode mapping.
//! We diff successive reports to derive press/release events and route
//! them by `wm.focus` to the focused window's owning client.
//!
//! Why not `/dev/kbd0` K_RAW? On modern hidbus systems `/dev/kbd0` is
//! the `kbdmux(4)` multiplexer's interface, which doesn't accept
//! `KDSKBMODE(K_RAW)` (returns `ENOTTY`). The underlying physical
//! keyboard `/dev/kbd1` is held exclusively by kbdmux, so we'd need
//! to detach it first. `/dev/hidraw*` works in parallel without
//! disturbing kbdmux/hkbd, identical philosophy to the pointer path.

use fresco_server::command::protocol::{Completion, COMP_INPUT_KEY};
use fresco_server::window::Compositor as WmCompositor;

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// Shared modifier bitmap (Page 0x07 modifier subset, mapped to the
/// 3-bit bitmap the wire uses: bit 0 = shift, bit 1 = ctrl, bit 2 = alt).
/// Updated by this thread on every HID report; read by `pointer_reader`
/// so mouse-button events carry the right modifier byte.
pub type SharedModifiers = Arc<Mutex<u8>>;

const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const HIDIOCGRDESCSIZE: libc::c_ulong = 0x4004551E;
const HIDIOCGRDESC:     libc::c_ulong = 0x2000551F;

#[repr(C)]
struct HidrawReportDescriptor {
    size:  u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

/// True if `path`'s HID descriptor starts with USAGE_PAGE(Generic
/// Desktop) ; USAGE(Keyboard).
fn hidraw_is_keyboard(path: &str) -> bool {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let fd = file.as_raw_fd();
    let mut size: i32 = 0;
    if unsafe { libc::ioctl(fd, HIDIOCGRDESCSIZE, &mut size) } < 0 {
        return false;
    }
    if size < 4 || (size as usize) > HID_MAX_DESCRIPTOR_SIZE {
        return false;
    }
    let mut desc = HidrawReportDescriptor {
        size: size as u32,
        value: [0; HID_MAX_DESCRIPTOR_SIZE],
    };
    if unsafe {
        libc::ioctl(fd, HIDIOCGRDESC, &mut desc as *mut _ as *mut libc::c_void)
    } < 0 {
        return false;
    }
    // USAGE_PAGE(Generic Desktop) = 0x05 0x01
    // USAGE(Keyboard)             = 0x09 0x06
    desc.value[0] == 0x05
        && desc.value[1] == 0x01
        && desc.value[2] == 0x09
        && desc.value[3] == 0x06
}

fn find_keyboard_path() -> Option<String> {
    for n in 0..16 {
        let path = format!("/dev/hidraw{n}");
        if std::path::Path::new(&path).exists() && hidraw_is_keyboard(&path) {
            return Some(path);
        }
    }
    None
}

pub fn spawn(
    event_subs: Arc<Mutex<Vec<(u8, Sender<Completion>)>>>,
    modifiers: SharedModifiers,
    wm: Arc<Mutex<WmCompositor>>,
) {
    std::thread::Builder::new()
        .name("atrium-input-kbd".into())
        .spawn(move || run(event_subs, modifiers, wm))
        .expect("spawn input reader");
}

/// Map the HID Keyboard 8-bit modifier byte to the 3-bit wire bitmap.
/// HID layout (LSB→MSB): LCtrl, LShift, LAlt, LGui, RCtrl, RShift, RAlt, RGui.
/// Wire layout: bit 0 = shift, bit 1 = ctrl, bit 2 = alt. Gui is dropped.
fn hid_mod_to_wire(hid: u8) -> u8 {
    let mut out = 0u8;
    if hid & 0b0010_0010 != 0 { out |= 0x01; } // shift
    if hid & 0b0001_0001 != 0 { out |= 0x02; } // ctrl
    if hid & 0b0100_0100 != 0 { out |= 0x04; } // alt
    out
}

fn run(
    event_subs: Arc<Mutex<Vec<(u8, Sender<Completion>)>>>,
    shared_mods: SharedModifiers,
    wm: Arc<Mutex<WmCompositor>>,
) {
    let path = match find_keyboard_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "kbd: no keyboard /dev/hidraw* found — keyboard input \
                 disabled. `kldload hidraw` and retry."
            );
            return;
        }
    };
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("kbd: cannot open {path}: {e}");
            return;
        }
    };
    eprintln!("kbd: reading {path}");

    // 8-byte HID keyboard reports. Some keyboards emit longer reports
    // when LEDs / report-IDs are in use; we read the first 8 bytes
    // and ignore extras for now.
    let mut report = [0u8; 32];
    let mut prev = [0u8; 8];

    loop {
        let n = match file.read(&mut report) {
            Ok(0)  => { eprintln!("kbd: EOF on {path} — exiting"); return; }
            Ok(n)  => n,
            Err(e) => { eprintln!("kbd: read error: {e}"); return; }
        };
        if n < 8 { continue; }
        let cur: &[u8; 8] = (&report[..8]).try_into().unwrap();

        // ── Modifier diff ────────────────────────────────────────
        if cur[0] != prev[0] {
            *shared_mods.lock().unwrap() = hid_mod_to_wire(cur[0]);
        }

        // ── Key diff: bytes 2..8 are the currently-pressed key set.
        // Anything in `cur` but not in `prev` → press; anything in
        // `prev` but not in `cur` → release.
        let cur_keys = &cur[2..8];
        let prev_keys = &prev[2..8];

        // Resolve focus once per report so a burst of changes
        // (chord) goes to the same window. If the WM still has the
        // screen window (id 0) focused but real client windows
        // exist, fall through to the topmost client window — apps
        // don't currently raise themselves on CREATE_WINDOW, so the
        // initial state would otherwise broadcast every keystroke
        // to every client, bypassing per-window routing entirely.
        let (target_window, owner) = {
            let g = wm.lock().unwrap();
            let focus_id = g.focus.unwrap_or(0);
            let effective = if focus_id == 0 {
                g.z_order.iter().rev().find(|&&id| id != 0).copied().unwrap_or(0)
            } else {
                focus_id
            };
            if effective == 0 {
                (0, 0)
            } else {
                let owner = g.windows.get(&effective)
                    .map(|w| w.owner as u8).unwrap_or(0);
                (effective as u32, owner)
            }
        };
        let mods = *shared_mods.lock().unwrap();

        for &k in cur_keys {
            if k == 0 { continue; }
            if !prev_keys.contains(&k) {
                emit(&event_subs, target_window, owner, k as u16, true, mods);
            }
        }
        for &k in prev_keys {
            if k == 0 { continue; }
            if !cur_keys.contains(&k) {
                emit(&event_subs, target_window, owner, k as u16, false, mods);
            }
        }

        prev.copy_from_slice(cur);
    }
}

fn emit(
    event_subs: &Arc<Mutex<Vec<(u8, Sender<Completion>)>>>,
    target_window: u32,
    owner: u8,
    hid_usage: u16,
    pressed: bool,
    modifiers: u8,
) {
    let mut result_hash = [0u8; 32];
    result_hash[0..2].copy_from_slice(&hid_usage.to_le_bytes());
    result_hash[2] = modifiers;
    let comp = Completion {
        comp_type:   COMP_INPUT_KEY,
        status:      if pressed { 1 } else { 0 },
        id:          target_window,
        result_hash,
        _pad:        [0u32; 22],
    };
    let mut subs = event_subs.lock().unwrap();
    if target_window == 0 {
        subs.retain(|(_, tx)| tx.send(comp).is_ok());
    } else {
        subs.retain(|(client_id, tx)| {
            if *client_id == owner { tx.send(comp).is_ok() } else { true }
        });
    }
}
