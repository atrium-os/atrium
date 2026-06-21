//! display_energy — light the display and hold it, so the energy federation has
//! a live "display0" member to poll. Prints when the mode is up, then sleeps so a
//! shell can read `sysctl kern.sched.energy_members` and see the display drawing
//! power as its own federated member (alongside "gpu0").
//!
//! Run (guest):  /tmp/display_energy &        # hold the display lit
//!               sysctl kern.sched.energy_cap_mw=50000   # enable federation
//!               sleep 1; sysctl kern.sched.energy_members  # display0 has demand

use std::io;
use std::thread::sleep;
use std::time::Duration;

use atrium_gpu::amd::{Display, Gpu, Scanout};

fn main() {
    if let Err(e) = run() {
        eprintln!("FAILED: {e}");
        std::process::exit(1);
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
    if dpy.set_mode(off, size)? != 0 {
        return Err(io::Error::other("set_mode fault"));
    }
    println!("display lit ({}x{}) — holding 8s for energy-federation polling", m.width, m.height);
    sleep(Duration::from_secs(8));
    println!("done");
    Ok(())
}
