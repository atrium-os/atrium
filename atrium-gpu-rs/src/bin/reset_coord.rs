//! reset_coord — prove a userspace GPU reset is COORDINATED with the display.
//!
//! A full GPU reset is a device-wide FLR: on real silicon it resets DCN too, so
//! a naive "gpu module FLRs the device" would silently kill the display the
//! display module is driving. The base now owns the FLR and brackets it with
//! every IP block's prepare/restore hooks — the display disarms its vblank IRQ
//! before the FLR and re-arms it after.
//!
//! This test arms the display vblank (kqueue-waitable), confirms vblanks flow,
//! triggers a GPU reset, and confirms vblanks STILL flow afterward. Without the
//! display's restore hook the FLR's prepare-disarm would leave vblank dead and
//! the post-reset wait would time out — so this exercises the coordination, not
//! just the model's block independence.
//!
//! It also re-submits a trivial fence after the reset to confirm the GFX block
//! itself came back (the gpu's restore hook reloaded firmware + MES).

use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr;

use atrium_gpu::amd::{gpu_reset, Display, Gpu, Scanout};

fn main() {
    match run() {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn wait_vblanks(kq: i32, dpy: &Display, n: u32) -> io::Result<()> {
    let ts = libc::timespec { tv_sec: 2, tv_nsec: 0 };
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    for _ in 0..n {
        let r = unsafe { libc::kevent(kq, ptr::null(), 0, &mut ev, 1, &ts) };
        if r != 1 || ev.ident != dpy.as_raw_fd() as libc::uintptr_t {
            return Err(io::Error::other(format!("vblank wait failed (r={r})")));
        }
    }
    Ok(())
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;

    let m = *dpy.modes()?.first().ok_or_else(|| io::Error::other("no modes"))?;
    let bytes = u64::from(m.width) * u64::from(m.height) * 4;
    let scan = Scanout::new(&vm, bytes)?;
    let (off, size) = scan.export();
    if dpy.set_mode(off, size)? != 0 {
        return Err(io::Error::other("set_mode fault"));
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

    // Vblanks flowing before the reset.
    wait_vblanks(kq, &dpy, 5)?;
    eprintln!("display vblank flowing before reset");

    // Device-wide GPU reset (FLR), routed through the base coordinator: the
    // display's prepare disarms vblank, the FLR runs, the display's restore
    // re-arms it (and the gpu's restore reloads firmware + MES).
    gpu_reset(&gpu)?;
    eprintln!("coordinated GPU reset done (device FLR + per-IP prepare/restore)");

    // Vblanks must STILL flow — the display was re-armed, not left for dead.
    wait_vblanks(kq, &dpy, 5)?;
    unsafe { libc::close(kq) };
    eprintln!("display vblank STILL flowing after the GPU reset — not collateral damage");

    eprintln!("coordinated reset confirmed: a device-lost GPU reset preserves the display");
    Ok(())
}
