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

use std::collections::HashMap;
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

/// `path`'s HID report descriptor.
fn descriptor(path: &str) -> Option<Vec<u8>> {
    let file = OpenOptions::new().read(true).open(path).ok()?;
    let fd = file.as_raw_fd();
    let mut size: i32 = 0;
    if unsafe { libc::ioctl(fd, HIDIOCGRDESCSIZE, &mut size) } < 0 {
        return None;
    }
    if size < 4 || (size as usize) > HID_MAX_DESCRIPTOR_SIZE {
        return None;
    }
    let mut desc = HidrawReportDescriptor {
        size: size as u32,
        value: [0; HID_MAX_DESCRIPTOR_SIZE],
    };
    if unsafe {
        libc::ioctl(fd, HIDIOCGRDESC, &mut desc as *mut _ as *mut libc::c_void)
    } < 0 {
        return None;
    }
    Some(desc.value[..size as usize].to_vec())
}

/// True if `path`'s HID descriptor begins USAGE_PAGE(Generic Desktop)
/// + USAGE(Keyboard).
fn hidraw_is_keyboard(path: &str) -> bool {
    /* USAGE_PAGE(Generic Desktop) = 0x05 0x01
     * USAGE(Keyboard)             = 0x09 0x06 */
    descriptor(path).is_some_and(|d| d.starts_with(&[0x05, 0x01, 0x09, 0x06]))
}

/// The hidraw nodes other than a keyboard's that carry consumer-control
/// keys (a headset, a remote, a keyboard's second interface).
fn find_consumer_paths() -> Vec<(String, crate::consumer::Layout)> {
    (0..16)
        .map(|n| format!("/dev/hidraw{n}"))
        .filter(|p| std::path::Path::new(p).exists() && !hidraw_is_keyboard(p))
        .filter_map(|p| descriptor(&p).and_then(|d| crate::consumer::Layout::parse(&d)).map(|l| (p, l)))
        .collect()
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
    /* Consumer-control readers, one per device. */
    let mut media: HashMap<String, JoinHandle<()>> = HashMap::new();
    let attach_media = |media: &mut HashMap<String, JoinHandle<()>>| {
        media.retain(|_, h| !h.is_finished());
        for (path, layout) in find_consumer_paths() {
            if media.contains_key(&path) {
                continue;
            }
            let sink = event_sink.clone();
            let p2 = path.clone();
            if let Ok(h) = std::thread::Builder::new()
                .name(format!("frescod-media:{path}"))
                .spawn(move || run_consumer(p2, layout, sink))
            {
                media.insert(path, h);
            }
        }
    };
    attach_media(&mut media);

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
                attach_media(&mut media);
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
    /* A keyboard whose media keys share its node (numbered reports). */
    let media = descriptor(&path).and_then(|d| crate::consumer::Layout::parse(&d)).filter(|l| !l.report_ids().contains(&0));
    let mut held: Vec<u16> = Vec::new();

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
        if let Some(pressed) = media.as_ref().and_then(|l| l.pressed(&report[..n])) {
            send_media(&event_sink, &mut held, pressed);
            continue;
        }
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
        // (Never a notification window: tabula.md §5.)
        let target_window = wm.lock().unwrap().keyboard_window();

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

/// Read consumer-control reports from `path` until it goes.
fn run_consumer(path: String, layout: crate::consumer::Layout, sink: Sender<DisplayEvent>) {
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("frescod: cannot open media keys {path}: {e}");
            return;
        }
    };
    eprintln!("frescod: media keys reading {path}");
    let mut report = [0u8; 64];
    let mut held: Vec<u16> = Vec::new();
    loop {
        match file.read(&mut report) {
            Ok(0) => { eprintln!("frescod: media keys EOF on {path}"); return; }
            Ok(n) => {
                if let Some(pressed) = layout.pressed(&report[..n]) {
                    send_media(&sink, &mut held, pressed);
                }
            }
            Err(e) => { eprintln!("frescod: media keys read error on {path}: {e}"); return; }
        }
    }
}

/// Media keys go to the WM alone (window 0), as `0x0C00 | usage`, with no
/// activation token: presses newly in `pressed`, releases of those gone.
fn send_media(sink: &Sender<DisplayEvent>, held: &mut Vec<u16>, pressed: Vec<u16>) {
    for &u in pressed.iter().filter(|u| !held.contains(u)) {
        let _ = sink.send(DisplayEvent::InputKey {
            window_id: 0, hid_usage: crate::consumer::wire_usage(u), pressed: true, modifiers: 0, activation: [0; 16],
        });
    }
    for &u in held.iter().filter(|u| !pressed.contains(u)) {
        let _ = sink.send(DisplayEvent::InputKey {
            window_id: 0, hid_usage: crate::consumer::wire_usage(u), pressed: false, modifiers: 0, activation: [0; 16],
        });
    }
    *held = pressed;
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
        // A real device: frescod mints the press's token.
        activation: if pressed { fresco_scene_server::window::MINT_HERE } else { [0; 16] },
    });
}
