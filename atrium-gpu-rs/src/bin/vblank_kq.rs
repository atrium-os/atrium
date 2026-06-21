//! vblank_kq — prove userspace can WAIT on vblank via kqueue (the milestone that
//! retires WAIT_VBLANK). SET_MODE arms the DCN-like vblank interrupt; the GPU
//! module's IH ISR services it and fires the EVFILT_READ knote registered on
//! /dev/atrium-display0. So a `kevent()` with no timeout blocks until the next
//! vblank and returns `data` = the number of vblanks elapsed since the last wake
//! (EV_CLEAR edge semantics). Timing N blocking waits gives the refresh rate
//! WITHOUT polling — the kqueue blocks the thread until the hardware says go.
//!
//! Build: cargo build --target aarch64-unknown-freebsd --bin vblank_kq
//! Run (guest): /tmp/vblank_kq   (kill display_anim first — its flips would also
//! generate vblanks but that's fine; with EV_CLEAR each wake just counts more).

use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::time::Instant;

use atrium_gpu::amd::{Display, Gpu, Scanout};

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
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;

    let m = *dpy.modes()?.first().ok_or_else(|| io::Error::other("no modes"))?;
    let bytes = u64::from(m.width) * u64::from(m.height) * 4;
    let scan = Scanout::new(&vm, bytes)?;
    let (off, size) = scan.export();

    // SET_MODE arms the vblank IRQ; from here the device raises one each refresh.
    if dpy.set_mode(off, size)? != 0 {
        return Err(io::Error::other("set_mode fault"));
    }

    // Register the display fd for EVFILT_READ — fires once the ISR delivers a
    // vblank. No threshold (data=0): any vblank wakes us.
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

    // First wait proves we BLOCK on hardware, not poll: a 2s timeout that should
    // fire well within ~17ms at 60Hz.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    let ts = libc::timespec { tv_sec: 2, tv_nsec: 0 };
    let n = unsafe { libc::kevent(kq, ptr::null(), 0, &mut ev, 1, &ts) };
    if n != 1 || ev.ident != dpy.as_raw_fd() as libc::uintptr_t {
        unsafe { libc::close(kq) };
        return Err(io::Error::other(format!(
            "kqueue did not fire on vblank (n={n}) — IRQ→knote path broken?"
        )));
    }
    eprintln!("first vblank delivered via kqueue (data={} vblanks)", ev.data);

    // Now time N blocking waits — the elapsed time / N is the refresh period.
    const N: u32 = 30;
    let t0 = Instant::now();
    let mut vblanks: i64 = ev.data; // count the one we already got
    for _ in 0..N {
        let n = unsafe { libc::kevent(kq, ptr::null(), 0, &mut ev, 1, &ts) };
        if n != 1 {
            unsafe { libc::close(kq) };
            return Err(io::Error::other(format!("vblank wait timed out (n={n})")));
        }
        vblanks += ev.data.max(1);
    }
    let secs = t0.elapsed().as_secs_f64();
    unsafe { libc::close(kq) };

    let hz = vblanks as f64 / secs;
    eprintln!(
        "{vblanks} vblanks over {N} kqueue waits in {secs:.3}s = {hz:.0} Hz (blocking, not polling)"
    );
    if hz < 20.0 || hz > 200.0 {
        return Err(io::Error::other(format!(
            "vblank rate {hz:.0} Hz far from ~60 Hz refresh"
        )));
    }
    eprintln!("vblank kqueue wait confirmed: EVFILT_READ on /dev/atrium-display0 blocks until vblank");
    Ok(())
}
