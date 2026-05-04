//! atrium-clock-socket — analog clock on the Atrium FreeBSD-native
//! stack, demoing multi-window FBO compositing with a non-text app.
//!
//! Renders 12 hour ticks + hour/minute/second hands + a small center
//! hub in its own window via fresco-socket-rs. A 1 Hz `EVFILT_TIMER`
//! drives re-render; keyboard `Esc` quits. No glyph atlas — only
//! rotated rectangles + a center square.

mod render;

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::ptr;

use fresco_socket::{Connection, Event};

const HID_ESCAPE: u16 = 0x29;

const WIN_W: u32 = 480;
const WIN_H: u32 = 480;

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
    let sock = std::env::var("FRESCOD_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-clock-socket: connected to {sock}");

    let win = conn.create_window(WIN_W, WIN_H, Some("clock"))?;
    let _ = conn.window_set_pos(win as u16, 380.0, 80.0);
    conn.set_default_window(win as u16);
    eprintln!("atrium-clock-socket: window {win} created — {WIN_W}x{WIN_H}");

    let mut renderer = render::Renderer::new(&mut conn, WIN_W, WIN_H)?;
    let (h, m, s) = local_hms();
    renderer.render(&mut conn, h, m, s)?;
    eprintln!("atrium-clock-socket: first frame rendered ({h:02}:{m:02}:{s:02})");

    // ── kqueue: socket fd (events) + 1-second timer ─────────────
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
            ident:  1,
            filter: libc::EVFILT_TIMER,
            flags:  libc::EV_ADD | libc::EV_ENABLE,
            fflags: 0,            // 0 = millisecond units
            data:   1000,
            udata:  ptr::null_mut(), ext: [0; 4],
        },
    ];
    if unsafe { libc::kevent(kq, chgs.as_ptr(), 2, ptr::null_mut(), 0, ptr::null()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }

    let mut events: [MaybeUninit<libc::kevent>; 8] =
        unsafe { MaybeUninit::uninit().assume_init() };
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
            // Otherwise: server event from socket.
            while let Some(input) = conn.poll_event()? {
                match input {
                    Event::CloseRequested { .. } => alive = false,
                    Event::Key { hid_usage, pressed, .. } => {
                        if pressed && hid_usage == HID_ESCAPE { alive = false; }
                    }
                    _ => {}
                }
            }
        }

        if tick && alive {
            let (h, m, s) = local_hms();
            renderer.render(&mut conn, h, m, s)?;
        }
    }

    eprintln!("atrium-clock-socket: exiting");
    let _ = unsafe { libc::close(kq) };
    Ok(())
}
