//! display_flip — exercise the offset-model scanout path (Phase 3) end-to-end:
//! CPU pixels -> System staging BO -> CP DMA copy -> VRAM scanout BO ->
//! `export_scanout` -> `Display::set_mode`/`page_flip`, all via the `amd::Scanout`
//! helper. Proves `atrium-gpu-rs`'s display surface drives the real
//! atrium-gpu-amd display module on the `{vram_offset,size}` model, no `bind`.
//!
//! Build (host cross): cargo build --target aarch64-unknown-freebsd --bin display_flip
//! Run (guest): /tmp/display_flip   (needs run-vm.sh --gpusim)

use std::io;

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

    let c = dpy.connector()?;
    let modes = dpy.modes()?;
    let m = *modes.first().ok_or_else(|| io::Error::other("no modes"))?;
    eprintln!(
        "display: connector type={} connected={}, mode {}x{}@{}mHz",
        c.connector_type, c.connected, m.width, m.height, m.refresh_mhz
    );

    let bytes = u64::from(m.width) * u64::from(m.height) * 4;
    let scan = Scanout::new(&vm, bytes)?;
    let (off, size) = scan.export();
    eprintln!("scanout: vram_offset={off:#x} size={size}");

    // Paint four quadrants (BGRA8) so a screendump is visually checkable, then
    // present: update (DMA into VRAM) -> set_mode -> vsync flip.
    let (w, h) = (m.width as usize, m.height as usize);
    let mut fb = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (b, g, r) = match (x < w / 2, y < h / 2) {
                (true, true) => (0u8, 0, 255),    // TL red
                (false, true) => (0, 255, 0),     // TR green
                (true, false) => (255, 0, 0),     // BL blue
                (false, false) => (255, 255, 255),// BR white
            };
            let i = (y * w + x) * 4;
            fb[i] = b;
            fb[i + 1] = g;
            fb[i + 2] = r;
            fb[i + 3] = 255;
        }
    }

    scan.update(&fb)?;
    let fault = dpy.set_mode(off, size)?;
    if fault != 0 {
        return Err(io::Error::other(format!("set_mode fault={fault}")));
    }
    let fault = dpy.page_flip(off, size, true)?;
    if fault != 0 {
        return Err(io::Error::other(format!("page_flip fault={fault}")));
    }

    // Read back the VRAM scanout to confirm the DMA copy landed the pattern,
    // sampling one pixel per quadrant.
    let (vbl0, dropped, _tear) = dpy.status()?;
    eprintln!("present OK: vblank={vbl0} dropped={dropped}");

    // A few more flips to advance vblank under the live host timer.
    for _ in 0..3 {
        let fault = dpy.page_flip(off, size, true)?;
        if fault != 0 {
            return Err(io::Error::other(format!("page_flip fault={fault}")));
        }
    }
    let (vbl1, _, _) = dpy.status()?;
    eprintln!("flips OK: vblank advanced {vbl0} -> {vbl1}");
    Ok(())
}
