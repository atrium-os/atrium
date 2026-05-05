//! Native FreeBSD pointer (mouse) input reader.
//!
//! Reads HID Mouse reports from `/dev/hidraw*` (`hidraw(4)`) — same
//! device class as the keyboard reader, different USAGE descriptor.
//! USB HID Mouse boot-protocol input reports:
//!
//!   byte 0:  buttons bitmap (bit 0=primary, 1=secondary, 2=middle,
//!            bits 3-7 = additional buttons in report-protocol mode)
//!   byte 1:  dx (signed 8-bit, X delta)
//!   byte 2:  dy (signed 8-bit, Y delta)
//!   byte 3+: wheel deltas (optional, varies per device)
//!
//! Server-side cursor state lives in `Compositor::cursor` (already a
//! field, just unused until now). On each report:
//!
//!   1. Update cursor (clamped to screen bounds).
//!   2. Hit-test against the WM's window list.
//!   3. Emit DisplayEvent::InputPointerMotion (translated into window-
//!      local coords) when the cursor moves.
//!   4. Diff button bitmap; emit InputPointerButton on changes.
//!
//! Cursor *rendering* (a visible cursor sprite drawn over the scene)
//! is a separate follow-up. For now the cursor moves invisibly; apps
//! that want a cursor-shaped reticle can render their own from the
//! motion events. Real-HW cursor planes (atrium-display0's HW cursor)
//! land alongside server-side rendering.

use fresco_scene_server::window::{Compositor, DisplayEvent};

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const HIDIOCGRDESCSIZE: libc::c_ulong = 0x4004551E;
const HIDIOCGRDESC:     libc::c_ulong = 0x2000551F;

#[repr(C)]
struct HidrawReportDescriptor {
    size:  u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

/// True if `path`'s descriptor begins USAGE_PAGE(Generic Desktop) +
/// USAGE(Mouse).
fn hidraw_is_mouse(path: &str) -> bool {
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
     * USAGE(Mouse)                = 0x09 0x02 */
    desc.value[0] == 0x05
        && desc.value[1] == 0x01
        && desc.value[2] == 0x09
        && desc.value[3] == 0x02
}

fn find_mouse_path() -> Option<String> {
    for n in 0..16 {
        let path = format!("/dev/hidraw{n}");
        if std::path::Path::new(&path).exists() && hidraw_is_mouse(&path) {
            return Some(path);
        }
    }
    None
}

pub fn spawn(
    event_sink: Sender<DisplayEvent>,
    wm:         Arc<Mutex<Compositor>>,
    screen_w:   u32,
    screen_h:   u32,
) {
    std::thread::Builder::new()
        .name("frescod-mouse".into())
        .spawn(move || run(event_sink, wm, screen_w, screen_h))
        .expect("spawn pointer reader");
}

fn run(
    event_sink: Sender<DisplayEvent>,
    wm:         Arc<Mutex<Compositor>>,
    screen_w:   u32,
    screen_h:   u32,
) {
    let path = match find_mouse_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "frescod: no mouse /dev/hidraw* found — pointer input \
                 disabled. `kldload hidraw` and retry."
            );
            return;
        }
    };
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("frescod: cannot open mouse {path}: {e}");
            return;
        }
    };
    eprintln!("frescod: mouse reading {path}");

    /* Seed cursor at screen center. */
    {
        let mut g = wm.lock().unwrap();
        g.cursor = (screen_w as f32 / 2.0, screen_h as f32 / 2.0);
    }

    let mut report = [0u8; 32];
    let mut prev_buttons: u8 = 0;
    let sw = screen_w as f32;
    let sh = screen_h as f32;

    loop {
        let n = match file.read(&mut report) {
            Ok(0)  => { eprintln!("frescod: mouse EOF on {path}"); return; }
            Ok(n)  => n,
            Err(e) => { eprintln!("frescod: mouse read error: {e}"); return; }
        };
        if n < 3 { continue; }

        let buttons = report[0];
        let dx = report[1] as i8 as f32;
        let dy = report[2] as i8 as f32;

        /* Update cursor + resolve hit-target window in one critical
         * section so the resulting event is internally consistent. */
        let (cur_x, cur_y, target, win_pos) = {
            let mut g = wm.lock().unwrap();
            let (mut x, mut y) = g.cursor;
            x = (x + dx).clamp(0.0, sw - 1.0);
            y = (y + dy).clamp(0.0, sh - 1.0);
            g.cursor = (x, y);

            let target = g.hit_test(x, y).unwrap_or(0);
            let win_pos = if target == 0 {
                (0.0, 0.0)
            } else {
                g.windows.get(&target).map(|w| w.pos).unwrap_or((0.0, 0.0))
            };
            (x, y, target as u32, win_pos)
        };

        let local_x = cur_x - win_pos.0;
        let local_y = cur_y - win_pos.1;

        if dx != 0.0 || dy != 0.0 {
            let _ = event_sink.send(DisplayEvent::InputPointerMotion {
                window_id: target,
                x: local_x, y: local_y,
            });
        }

        /* Button diff. Bit 0 = primary, 1 = secondary, 2 = middle. We
         * map directly to the wire's `button` field (1-indexed). */
        let changed = buttons ^ prev_buttons;
        if changed != 0 {
            for bit in 0..8 {
                let mask = 1u8 << bit;
                if changed & mask != 0 {
                    let pressed = (buttons & mask) != 0;
                    let _ = event_sink.send(DisplayEvent::InputPointerButton {
                        window_id: target,
                        x: local_x, y: local_y,
                        button:    (bit + 1) as u8,
                        pressed,
                        modifiers: 0,
                    });
                }
            }
            prev_buttons = buttons;
        }

        /* Wheel: byte 3 (signed) when present. Many mice in
         * report-protocol mode emit a different layout; for the boot-
         * protocol-shaped path we just decode byte 3 if it's there. */
        if n >= 4 {
            let wheel = report[3] as i8 as f32;
            if wheel != 0.0 {
                let _ = event_sink.send(DisplayEvent::InputPointerScroll {
                    window_id: target,
                    dx: 0.0,
                    /* HID wheel: positive = wheel-up = scroll content
                     * up. Wire convention: positive dy = scroll content
                     * down. Negate. */
                    dy: -wheel * 8.0,
                });
            }
        }
    }
}
