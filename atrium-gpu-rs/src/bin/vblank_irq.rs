//! vblank_irq — prove gpusim raises a real DCN-like vblank INTERRUPT (not just a
//! polled counter). SET_MODE arms the display's vblank IRQ; the device then
//! raises an IH interrupt each vertical blank, which the GPU's IH ISR services
//! and counts in irq_count (GET_IRQS). With NO submits in flight, irq_count can
//! only rise from vblank IRQs — so it should climb at roughly the refresh rate.
//!
//! Build: cargo build --target aarch64-unknown-freebsd --bin vblank_irq
//! Run (guest): /tmp/vblank_irq   (kill any display_anim first — its submits
//! would also raise interrupts and pollute the count)

use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

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

    // SET_MODE arms the vblank IRQ (DCN-like). After this the device raises a
    // vblank interrupt each refresh — with no submits, that's the only IRQ.
    if dpy.set_mode(off, size)? != 0 {
        return Err(io::Error::other("set_mode fault"));
    }

    let c0 = gpu.irq_count()?;
    let (v0, _, _) = dpy.status()?;
    let t0 = Instant::now();
    sleep(Duration::from_millis(1000));
    let secs = t0.elapsed().as_secs_f64();
    let c1 = gpu.irq_count()?;
    let (v1, _, _) = dpy.status()?;
    let delta = c1 - c0;
    let hz = delta as f64 / secs;
    eprintln!(
        "polled VBLANK_COUNT advanced {} (tick running?); vblank IRQs: {delta} in {secs:.2}s = {hz:.0} Hz",
        v1 - v0
    );

    // ~60 Hz refresh; allow a wide tolerance for timer jitter under HVF.
    if delta == 0 {
        return Err(io::Error::other(
            "no interrupts — vblank IRQ not firing (polled-only?)",
        ));
    }
    if hz < 20.0 || hz > 200.0 {
        return Err(io::Error::other(format!(
            "vblank rate {hz:.0} Hz far from ~60 Hz refresh"
        )));
    }
    eprintln!("vblank interrupt confirmed: device raises it each refresh, IH ISR services it");
    Ok(())
}
