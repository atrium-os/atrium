//! atrium-clock-socket — analog clock on the Atrium FreeBSD-native
//! stack, ported to fresco-client + the atrium-core PATH op (M3c).
//!
//! Renders 12 hour ticks + 3 hands + a centre hub as 16 path nodes
//! per frame. A 1 Hz `EVFILT_TIMER` drives re-render; keyboard `Esc`
//! quits via the EV_INPUT_KEY (M3a) event surface.

mod render;

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::ptr;

use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;

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

    let win = conn.window_create(WIN_W, WIN_H, "clock", WindowHints::default())?;
    eprintln!("atrium-clock-socket: window {win} created — {WIN_W}x{WIN_H}");

    let renderer = render::Renderer::new(WIN_W, WIN_H);
    let (h, m, s) = local_hms();
    renderer.render(&mut conn, h, m, s)?;
    eprintln!("atrium-clock-socket: first frame rendered ({h:02}:{m:02}:{s:02})");

    /* kqueue: socket fd (events) + 1-second timer. */
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
            fflags: 0,            /* milliseconds */
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
            /* Server event from socket. */
            while let Some(input) = conn.poll_event()? {
                match input {
                    Event::CloseRequested { window_id } if window_id == win => {
                        alive = false;
                    }
                    Event::Key { hid_usage, pressed, window_id, .. }
                        if window_id == win && pressed && hid_usage == HID_ESCAPE =>
                    {
                        alive = false;
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
