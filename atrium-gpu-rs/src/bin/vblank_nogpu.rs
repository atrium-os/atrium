//! vblank_nogpu — vblank reaches a process that holds no GPU objects.
//!
//! The IH ring + ISR live in the base module and the display has its own IH
//! cause handler: vblank is a display signal, and the GPU module plays no part
//! in delivering it.
//!
//! It opens ONLY /dev/atrium-display0 (no /dev/atrium-gpu0, no VM, no BO).
//! Since display ABI v2 the display can't be lit without a BO, and only the gpu
//! module makes BOs; so the CRTC must already be lit (hold-last-frame: run
//! display_flip first). Unloading the gpu module also needs the display to let
//! go of its on-screen BO, which it holds while it scans it out, so the gpu
//! module refuses to unload while a frame is up. This therefore checks the
//! half that can be checked: with the CRTC lit and this process holding no GPU
//! objects, vblanks arrive through the base ISR and the display's own IH cause
//! handler, and a commit without the gpu module's exports fails cleanly.
//!
//! Run (guest):  /tmp/display_flip && /tmp/vblank_nogpu

use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::time::Instant;

use atrium_gpu::amd::Display;

fn main() {
    match run() {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> io::Result<()> {
    let dpy = Display::open()?;

    let st = dpy.status_v2()?;
    if st.flags & atrium_gpu::amd::STATUS_MODE_SET == 0 {
        return Err(io::Error::other("CRTC not lit: run display_flip first (hold-last-frame)"));
    }

    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    let chg = libc::kevent {
        ident: dpy.as_raw_fd() as libc::uintptr_t,
        filter: libc::EVFILT_READ,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: 0,
        data: 0,
        udata: ptr::null_mut(),
        ext: [0; 4],
    };
    if unsafe { libc::kevent(kq, &chg, 1, ptr::null_mut(), 0, ptr::null()) } != 0 {
        unsafe { libc::close(kq) };
        return Err(io::Error::last_os_error());
    }

    const N: u32 = 20;
    let ts = libc::timespec { tv_sec: 2, tv_nsec: 0 };
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    let t0 = Instant::now();
    let mut vblanks: i64 = 0;
    for _ in 0..N {
        let n = unsafe { libc::kevent(kq, ptr::null(), 0, &mut ev, 1, &ts) };
        if n != 1 {
            unsafe { libc::close(kq) };
            return Err(io::Error::other(format!(
                "vblank wait timed out (n={n}) — IRQ path needs the gpu module?"
            )));
        }
        vblanks += ev.data.max(1);
    }
    let secs = t0.elapsed().as_secs_f64();
    unsafe { libc::close(kq) };

    let hz = vblanks as f64 / secs;
    eprintln!("{vblanks} vblanks over {N} kqueue waits in {secs:.3}s = {hz:.0} Hz");
    if hz < 20.0 || hz > 200.0 {
        return Err(io::Error::other(format!("vblank rate {hz:.0} Hz far from ~60 Hz")));
    }
    eprintln!("vblank delivered to a process holding no GPU objects");
    Ok(())
}
