//! display_anim — a continuously changing frame through the full driver path,
//! presented the way frescod presents: app → amd driver ('A' CP DMA + display
//! ABI v2 commit by BO fd) → gpusim (RDNA model) → scanout.
//!
//! Double-buffered and fence-paced: frame N is copied into the buffer that is
//! not on screen, the copy signals `copy_done = N`, the commit waits for it
//! (in-fence) and latches at vblank, and its out-fence `flip_done = N` gates
//! frame N+1. Each frame moves a marker, so successive screendumps must show
//! it advancing.
//!
//! Checks: every frame reached the screen (flips_done advanced by `frames`),
//! the hardware dropped none, nothing tore, and presents ran at about the
//! display refresh (paced by the flips, not a timer).
//!
//! Build: cargo build --target x86_64-unknown-freebsd --bin display_anim
//! Run (guest): /tmp/display_anim [frames]   (default 120)

use std::io;
use std::time::Instant;

use atrium_gpu::amd::{Display, Gpu, Scanout, COMMIT_MODESET};

fn main() {
    let frames: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    match run(frames) {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn paint(fb: &mut [u8], w: usize, h: usize, f: u64) {
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
}

fn run(frames: u64) -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;

    let m = *dpy.modes()?.first().ok_or_else(|| io::Error::other("no modes"))?;
    let (w, h) = (m.width as usize, m.height as usize);
    let scan = Scanout::with_buffers(&vm, (w * h * 4) as u64, 2)?;
    let copy_done = gpu.create_syncobj()?;
    let flip_done = gpu.create_syncobj()?;
    eprintln!("anim: {w}x{h} @ {} mHz, {frames} frames, 2 buffers", m.refresh_mhz);

    let mut fb = vec![0u8; w * h * 4];
    paint(&mut fb, w, h, 0);
    scan.upload(0, &fb, 0, Some((&copy_done, 1)))?;
    copy_done.wait(1, 2000)?;
    if dpy.commit(scan.buffer(0), COMMIT_MODESET, 0, None, Some((&flip_done, 1)))? != 0 {
        return Err(io::Error::other("modeset fault"));
    }
    let st0 = dpy.status_v2()?;

    let t0 = Instant::now();
    let mut on_screen = 0usize;
    for k in 2..frames + 2 {
        flip_done.wait(k - 1, 2000)?; // previous frame on screen; its old buffer free
        paint(&mut fb, w, h, k);
        let back = 1 - on_screen;
        scan.upload(back, &fb, 0, Some((&copy_done, k)))?;
        let f = dpy.commit(scan.buffer(back), 0, 0, Some((&copy_done, k)), Some((&flip_done, k)))?;
        if f != 0 {
            return Err(io::Error::other(format!("flip fault={f} at frame {k}")));
        }
        on_screen = back;
    }
    flip_done.wait(frames + 1, 2000)?;
    let secs = t0.elapsed().as_secs_f64();
    let st = dpy.status_v2()?;

    let flips = st.flips_done - st0.flips_done;
    let vblanks = st.vblank_count - st0.vblank_count;
    let fps = frames as f64 / secs;
    let hz = f64::from(m.refresh_mhz) / 1000.0;
    eprintln!(
        "anim done: {frames} frames in {secs:.2}s = {fps:.1} fps over {vblanks} vblanks \
         (refresh {hz:.1} Hz), flips {flips}, dropped {}, tear_line {:#x}",
        st.dropped_flips, st.tear_line
    );
    if flips != frames {
        return Err(io::Error::other(format!("{flips} flips reached the screen, want {frames}")));
    }
    if st.dropped_flips != 0 {
        return Err(io::Error::other(format!("{} flips dropped", st.dropped_flips)));
    }
    if st.tear_line != 0xffff_ffff {
        return Err(io::Error::other(format!("tore at line {}", st.tear_line)));
    }
    if fps > hz * 1.1 {
        return Err(io::Error::other(format!("{fps:.1} fps is faster than the {hz:.1} Hz refresh")));
    }
    Ok(())
}
