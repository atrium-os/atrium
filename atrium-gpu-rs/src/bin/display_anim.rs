//! display_anim — drive a continuously-changing frame through the FULL hardware
//! driver path so the visible end-to-end can be checked live: app → amd driver
//! ('A'/'D': VM_BIND, CP DMA, page-flip) → gpusim (RDNA model) → scanout. Each
//! frame moves a vertical bar, so successive screendumps (or the --display
//! window) must show the bar advancing — proof the scanout updates at refresh,
//! not just once.
//!
//! Build: cargo build --target aarch64-unknown-freebsd --bin display_anim
//! Run (guest): /tmp/display_anim [frames]   (default 300, ~10s at 30fps)

use std::io;
use std::thread::sleep;
use std::time::Duration;

use atrium_gpu::amd::{Display, Gpu, Scanout};

fn main() {
    let frames: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    match run(frames) {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run(frames: u32) -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;

    let m = *dpy.modes()?.first().ok_or_else(|| io::Error::other("no modes"))?;
    let (w, h) = (m.width as usize, m.height as usize);
    let bytes = (w * h * 4) as u64;
    let scan = Scanout::new(&vm, bytes)?;
    let (off, size) = scan.export();
    eprintln!("anim: {}x{}, {} frames", w, h, frames);

    let mut fb = vec![0u8; w * h * 4];
    let bar_w = (w / 16).max(8);

    if dpy.set_mode(off, size)? != 0 {
        return Err(io::Error::other("set_mode fault"));
    }

    // Solid primary quadrants (BGRA8) so any channel mapping is unambiguous:
    //   TL = RED   TR = GREEN
    //   BL = BLUE  BR = WHITE
    // A moving black marker on the top edge keeps it visibly live.
    let _ = bar_w;
    for f in 0..frames {
        let mark_x = ((f as usize) * 4) % w;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let (mut b, mut g, mut r) = match (x < w / 2, y < h / 2) {
                    (true, true) => (0u8, 0, 255),     // TL red
                    (false, true) => (0, 255, 0),      // TR green
                    (true, false) => (255, 0, 0),      // BL blue
                    (false, false) => (255, 255, 255), // BR white
                };
                if y < 12 && x >= mark_x && x < mark_x + 16 {
                    b = 0; g = 0; r = 0; // black moving marker
                }
                fb[i] = b; fb[i + 1] = g; fb[i + 2] = r; fb[i + 3] = 255;
            }
        }
        scan.update(&fb)?;
        if dpy.page_flip(off, size, true)? != 0 {
            return Err(io::Error::other("page_flip fault"));
        }
        sleep(Duration::from_millis(33)); // ~30 fps
    }
    let (vbl, dropped, _) = dpy.status()?;
    eprintln!("anim done: vblank={vbl} dropped={dropped}");
    Ok(())
}
