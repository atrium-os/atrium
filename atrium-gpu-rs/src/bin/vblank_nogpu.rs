//! vblank_nogpu — prove vblank is delivered with the GPU (render) module UNLOADED.
//!
//! This is the structural payoff of moving the IH ring + ISR into the base module
//! (and giving the display its own IH cause handler): vblank is a display signal,
//! and the GPU module plays no part in it. The §4.1 split promises the display
//! attaches independently of the gpu module — this proves the INTERRUPT path does
//! too.
//!
//! It opens ONLY /dev/atrium-display0 (no /dev/atrium-gpu0, no VM, no BO). It arms
//! the vblank IRQ with a dummy framebuffer: set_mode faults on residency (there is
//! no VRAM BO without the gpu), but the driver arms regDISP_VBLANK_IRQ_EN anyway,
//! so the device raises a vblank interrupt each refresh. The base ISR drains it and
//! routes the VBLANK cause to the display module's handler → the EVFILT_READ knote.
//!
//! Run (guest):  kldunload atrium_gpu_amd   # drop the render module
//!               /tmp/vblank_nogpu          # vblanks still arrive via kqueue
//!               kldload  atrium_gpu_amd     # restore it

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

    // Arm the vblank IRQ with a dummy FB. Without the gpu module there is no VRAM
    // BO, so the referee faults residency — but the driver arms the vblank IRQ
    // regardless, which is all we need to make the device raise it each refresh.
    let fault = dpy.set_mode(0, 0x10_0000)?;
    eprintln!("set_mode fault={fault} (nonzero expected with no VRAM BO — vblank still armed)");

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
    eprintln!("{vblanks} vblanks over {N} kqueue waits in {secs:.3}s = {hz:.0} Hz — GPU module not loaded");
    if hz < 20.0 || hz > 200.0 {
        return Err(io::Error::other(format!("vblank rate {hz:.0} Hz far from ~60 Hz")));
    }
    eprintln!("vblank delivered with the GPU module UNLOADED — it has no part in the vblank path");
    Ok(())
}
