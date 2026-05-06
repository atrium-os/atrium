//! Native FreeBSD keyboard input reader.
//!
//! Reads raw HID reports from the keyboard's `/dev/hidraw*` cdev
//! (`hidraw(4)`, the modern FreeBSD HID-bus interface — `sys/dev/hid/hidraw.c`).
//! USB HID Keyboard input reports use the boot-protocol layout:
//!
//!   byte 0:    modifiers bitmap (LCtrl=1, LShift=2, LAlt=4, LGui=8,
//!                                RCtrl=16, RShift=32, RAlt=64, RGui=128)
//!   byte 1:    reserved
//!   bytes 2-7: up to 6 simultaneously-pressed HID Usage Page 0x07 codes
//!
//! Crucially, **the report bytes are already in the wire format we
//! want** — no AT-scan-code translation, no Linux keycode mapping.
//! We diff successive reports to derive press/release events, look up
//! the focused window's owner from the WM, and push a
//! `DisplayEvent::InputKey` into the compositor's event-sink mpsc.
//! The fan-out thread (in `socket_server`) then encodes one envelope
//! per event and broadcasts it to every connected client; clients
//! filter on `window_id` to know whether the event is theirs.
//!
//! Why not `/dev/kbd0` K_RAW? On modern hidbus systems `/dev/kbd0` is
//! the `kbdmux(4)` multiplexer's interface, which doesn't accept
//! `KDSKBMODE(K_RAW)` (returns `ENOTTY`). The underlying physical
//! keyboard `/dev/kbd1` is held exclusively by kbdmux; we'd have to
//! detach it first. `/dev/hidraw*` works in parallel without
//! disturbing kbdmux/hkbd.

use fresco_scene_server::window::{Compositor, DisplayEvent};

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use atrium_devevents::{DeviceWatcher, Event};

const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const HIDIOCGRDESCSIZE: libc::c_ulong = 0x4004551E;
const HIDIOCGRDESC:     libc::c_ulong = 0x2000551F;

#[repr(C)]
struct HidrawReportDescriptor {
    size:  u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

/// True if `path`'s HID descriptor begins USAGE_PAGE(Generic Desktop)
/// + USAGE(Keyboard).
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
    /* USAGE_PAGE(Generic Desktop) = 0x05 0x01
     * USAGE(Keyboard)             = 0x09 0x06 */
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

/// Spawn the keyboard supervisor thread. It owns the lifetime of the
/// per-device reader: at startup it scans existing `/dev/hidraw*`
/// nodes and spawns a worker if a keyboard is found; on every devd
/// hotplug event it re-scans (so plugging a USB keyboard mid-session
/// just works). Worker exits on device removal (read EOF); the
/// supervisor notices and re-attaches when a new device appears.
pub fn spawn(
    event_sink: Sender<DisplayEvent>,
    wm:         Arc<Mutex<Compositor>>,
) {
    std::thread::Builder::new()
        .name("frescod-kbd-supv".into())
        .spawn(move || supervise(event_sink, wm))
        .expect("spawn keyboard supervisor");
}

fn supervise(event_sink: Sender<DisplayEvent>, wm: Arc<Mutex<Compositor>>) {
    let mut worker: Option<(String, JoinHandle<()>)> = None;

    let try_attach = |worker: &mut Option<(String, JoinHandle<()>)>| {
        /* Reap a finished worker so a re-plug starts a fresh one. */
        if let Some((path, h)) = worker.as_ref() {
            if h.is_finished() {
                let (path, h) = worker.take().unwrap();
                let _ = h.join();
                eprintln!("frescod: keyboard reader for {path} exited");
            }
        }
        if worker.is_some() { return; }
        let Some(path) = find_keyboard_path() else { return; };
        let sink = event_sink.clone();
        let wm   = wm.clone();
        let p2   = path.clone();
        let h = std::thread::Builder::new()
            .name(format!("frescod-kbd:{path}"))
            .spawn(move || run_one(p2, sink, wm))
            .expect("spawn keyboard reader");
        *worker = Some((path, h));
    };

    /* Seed once at startup. */
    try_attach(&mut worker);
    if worker.is_none() {
        eprintln!(
            "frescod: no keyboard /dev/hidraw* found yet — will attach \
             on hotplug. (`kldload hidraw` if hidraw module isn't loaded.)"
        );
    }

    let watcher = match DeviceWatcher::open() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("frescod: devevents unavailable ({e}) — keyboard \
                       hotplug disabled. devd(8) running?");
            return;
        }
    };

    loop {
        let ev = match watcher.recv() {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("frescod: devevents recv error: {e}; supervisor exits");
                return;
            }
        };
        match ev {
            Event::Added { devnode } | Event::Removed { devnode }
                if devnode.starts_with("/dev/hidraw") =>
            {
                /* Either kind triggers a re-scan: an Add gives us a new
                 * device to attach; a Remove drops us back to "no
                 * keyboard" and the next Add re-attaches. The worker
                 * thread itself exits on EOF when its device disappears. */
                eprintln!("frescod: hidraw event {devnode}; re-scanning");
                try_attach(&mut worker);
            }
            _ => {} /* ignore other devices */
        }
    }
}

/// Map the HID Keyboard 8-bit modifier byte to the 3-bit wire bitmap.
/// HID layout (LSB→MSB): LCtrl, LShift, LAlt, LGui, RCtrl, RShift, RAlt, RGui.
/// Wire layout: bit 0 = shift, bit 1 = ctrl, bit 2 = alt. Gui is dropped.
fn hid_mod_to_wire(hid: u8) -> u8 {
    let mut out = 0u8;
    if hid & 0b0010_0010 != 0 { out |= 0x01; } /* shift */
    if hid & 0b0001_0001 != 0 { out |= 0x02; } /* ctrl */
    if hid & 0b0100_0100 != 0 { out |= 0x04; } /* alt */
    out
}

fn run_one(path: String,
           event_sink: Sender<DisplayEvent>,
           wm: Arc<Mutex<Compositor>>) {
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("frescod: cannot open keyboard {path}: {e}");
            return;
        }
    };
    eprintln!("frescod: keyboard reading {path}");

    /* 8-byte HID keyboard reports. Some keyboards emit longer reports
     * when LEDs / report-IDs are in use; we read the first 8 bytes
     * and ignore extras. */
    let mut report = [0u8; 32];
    let mut prev = [0u8; 8];
    let mut wire_mods: u8 = 0;

    loop {
        let n = match file.read(&mut report) {
            Ok(0)  => { eprintln!("frescod: keyboard EOF on {path}"); return; }
            Ok(n)  => n,
            Err(e) => { eprintln!("frescod: keyboard read error: {e}"); return; }
        };
        if n < 8 { continue; }
        let cur: &[u8; 8] = (&report[..8]).try_into().unwrap();

        if cur[0] != prev[0] {
            wire_mods = hid_mod_to_wire(cur[0]);
        }

        let cur_keys  = &cur[2..8];
        let prev_keys = &prev[2..8];

        /* Resolve focus once per report so a chord-burst routes to the
         * same window. If the WM still has the screen window (id 0)
         * focused but real client windows exist, fall through to the
         * topmost client window — apps don't currently raise themselves
         * on WINDOW_CREATE, so initial state would otherwise broadcast
         * every keystroke to every client (bypassing per-window
         * routing). */
        let target_window = {
            let g = wm.lock().unwrap();
            let focus_id = g.focus.unwrap_or(0);
            let effective = if focus_id == 0 {
                g.z_order.iter().rev().find(|&&id| id != 0).copied().unwrap_or(0)
            } else {
                focus_id
            };
            effective as u32
        };

        for &k in cur_keys {
            if k == 0 { continue; }
            if !prev_keys.contains(&k) {
                send(&event_sink, target_window, k as u16, true, wire_mods);
            }
        }
        for &k in prev_keys {
            if k == 0 { continue; }
            if !cur_keys.contains(&k) {
                send(&event_sink, target_window, k as u16, false, wire_mods);
            }
        }

        prev.copy_from_slice(cur);
    }
}

fn send(
    sink:      &Sender<DisplayEvent>,
    window_id: u32,
    hid_usage: u16,
    pressed:   bool,
    modifiers: u8,
) {
    /* Receiver dropped means frescod is shutting down — silently exit
     * the next loop iteration when read errors out, no need to escalate
     * here. */
    let _ = sink.send(DisplayEvent::InputKey {
        window_id, hid_usage, pressed, modifiers,
    });
}
