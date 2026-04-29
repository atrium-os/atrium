//! atrium-clock — analog + digital clock on Fresco.
//!
//! Vertical app #4. Adds animation timers (kqueue EVFILT_TIMER firing
//! every second), solid-color geometry (clock face + hands), and the
//! first use of the slot graph's child relation (root has both analog
//! and digital sub-slots). Press Esc / Ctrl+Q to quit.

mod glyph_cache;
mod render;

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::ptr;

use fresco_rs::{Connection, Event};

const FONT_PATH: &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32 = 48.0;

const HID_ESCAPE: u16 = 0x29;
const HID_Q:      u16 = 0x14;
const HID_LCTRL:  u16 = 0xe0;
const HID_RCTRL:  u16 = 0xe4;

fn local_hms() -> (u32, u32, u32) {
    unsafe {
        let mut t: libc::time_t = 0;
        libc::time(&mut t);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let font = std::fs::read(FONT_PATH)?;
    let cache = glyph_cache::GlyphCache::build(&font, FONT_SIZE_PX)?;

    let conn = Connection::open()?;
    let disp = conn.display();
    eprintln!("display: {}x{}", disp.width, disp.height);

    let mut renderer = render::Renderer::new(&cache);
    while conn.poll_event().is_some() {}        // drain stale input

    // First frame.
    let (h, m, s) = local_hms();
    renderer.render(&conn, h, m, s)?;
    eprintln!("clock running — {:02}:{:02}:{:02}", h, m, s);

    // ── kqueue: fresco fd (input) + 1-second timer ───────────────
    let kq = unsafe { libc::kqueue() };
    if kq < 0 { return Err(io::Error::last_os_error().into()); }

    let chgs = [
        libc::kevent {
            ident:  conn.as_raw_fd() as _,
            filter: libc::EVFILT_READ,
            flags:  libc::EV_ADD,
            fflags: 0, data: 0,
            udata: ptr::null_mut(), ext: [0; 4],
        },
        libc::kevent {
            ident:  1,                          // arbitrary timer id
            filter: libc::EVFILT_TIMER,
            flags:  libc::EV_ADD | libc::EV_ENABLE,
            fflags: 0,                          // 0 = millisecond units
            data:   1000,                       // 1 second
            udata:  ptr::null_mut(), ext: [0; 4],
        },
    ];
    unsafe { libc::kevent(kq, chgs.as_ptr(), 2, ptr::null_mut(), 0, ptr::null()); }

    let mut events: [MaybeUninit<libc::kevent>; 16] =
        unsafe { MaybeUninit::uninit().assume_init() };
    let mut shift = false;
    let mut ctrl  = false;
    let mut alive = true;

    while alive {
        let n = unsafe {
            libc::kevent(kq, ptr::null(), 0,
                events.as_mut_ptr() as *mut libc::kevent, events.len() as i32,
                ptr::null())
        };
        if n < 0 { return Err(io::Error::last_os_error().into()); }

        let mut tick = false;
        for i in 0..n as usize {
            let ev = unsafe { events[i].assume_init() };
            if ev.filter == libc::EVFILT_TIMER {
                tick = true;
                continue;
            }
            // Otherwise: input from fresco cdev.
            while let Some(input) = conn.poll_event() {
                if let Event::Key { keysym, pressed } = input {
                    let _ = shift;
                    match keysym {
                        HID_LCTRL | HID_RCTRL => ctrl = pressed,
                        HID_ESCAPE if pressed => alive = false,
                        HID_Q if pressed && ctrl => alive = false,
                        _ => {}
                    }
                }
            }
        }

        if tick && alive {
            let (h, m, s) = local_hms();
            renderer.render(&conn, h, m, s)?;
        }
    }

    eprintln!("exiting");
    let _ = unsafe { libc::close(kq) };
    Ok(())
}
