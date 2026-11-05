//! display_flip — display ABI v2 end to end on atrium-gpu-amd + gpusim.
//!
//! CPU pixels -> System staging BO -> CP DMA copy -> VRAM scanout BOs ->
//! `Display::commit` by BO fd, with in- and out-fences. Checks:
//!
//!   1. MODESET lights the CRTC with buffer A.
//!   2. The display node is exclusive (a second open is EBUSY).
//!   3. A vsync'd flip to B is pending until a vblank, then its out-fence fires.
//!   4. A commit while one is outstanding is EBUSY (nothing dropped silently).
//!   5. A flip waits for its in-fence: B's out-fence doesn't fire while the
//!      in-fence is unsignalled, and does soon after it is signalled.
//!   6. The render-to-flip chain: the DMA copy signals the in-fence itself.
//!   7. An ASYNC flip lands at once.
//!   8. The referee and the fd checks: a bad pitch faults, a System (non-VRAM)
//!      BO is refused, TEST_ONLY validates without changing anything.
//!   9. Hold-last-frame: after close and reopen the mode is still lit.
//!
//! Build (host cross): cargo build --target x86_64-unknown-freebsd --bin display_flip
//! Run (guest): /tmp/display_flip   (needs the VM booted with GPUSIM=1)

use std::io;
use std::time::{Duration, Instant};

use atrium_gpu::amd::{
    Display, Gpu, Scanout, Syncobj, COMMIT_ASYNC, COMMIT_TEST_ONLY,
    STATUS_COMMIT_PENDING, STATUS_MODE_SET,
};

fn main() {
    match run() {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn fail(msg: String) -> io::Error { io::Error::other(msg) }

/// Paint four quadrants (BGRA8) with `tint` mixed in, so frames differ.
fn paint(w: usize, h: usize, tint: u8) -> Vec<u8> {
    let mut fb = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (b, g, r) = match (x < w / 2, y < h / 2) {
                (true, true) => (tint, 0, 255),
                (false, true) => (tint, 255, 0),
                (true, false) => (255, tint, 0),
                (false, false) => (255, 255, 255 - tint),
            };
            let i = (y * w + x) * 4;
            fb[i..i + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    fb
}

/// Wait for `so` to reach `v` (polling), up to `ms`.
fn reached(so: &Syncobj<'_>, v: u64, ms: u64) -> io::Result<bool> {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        if so.query()? >= v {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(so.query()? >= v)
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;

    let c = dpy.connector()?;
    let m = *dpy.modes()?.first().ok_or_else(|| fail("no modes".into()))?;
    eprintln!(
        "display: connector type={} connected={}, mode {}x{}@{}mHz",
        c.connector_type, c.connected, m.width, m.height, m.refresh_mhz
    );
    let (w, h) = (m.width as usize, m.height as usize);
    let bytes = (w * h * 4) as u64;
    let scan = Scanout::with_buffers(&vm, bytes, 2)?;
    let (a, b) = (scan.buffer(0), scan.buffer(1));
    let out = gpu.create_syncobj()?;
    let inf = gpu.create_syncobj()?;

    // 1. MODESET with A.
    scan.upload(0, &paint(w, h, 0), 0, None)?;
    let f = dpy.modeset(a)?;
    if f != 0 {
        return Err(fail(format!("modeset fault={f}")));
    }
    let st = dpy.status_v2()?;
    if st.flags & STATUS_MODE_SET == 0 || st.refresh_mhz == 0 {
        return Err(fail(format!("after modeset: flags={:#x} refresh={}", st.flags, st.refresh_mhz)));
    }
    eprintln!("1 modeset OK ({} mHz)", st.refresh_mhz);

    // 2. Exclusive.
    match Display::open() {
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => eprintln!("2 exclusive open OK (EBUSY)"),
        Err(e) => return Err(fail(format!("second open: {e}, want EBUSY"))),
        Ok(_) => return Err(fail("second open succeeded".into())),
    }

    // 3 + 4. vsync'd flip to B with an out-fence; a second commit is EBUSY.
    scan.upload(1, &paint(w, h, 128), 0, None)?;
    let f = dpy.commit(b, 0, 0, None, Some((&out, 1)))?;
    if f != 0 {
        return Err(fail(format!("flip B fault={f}")));
    }
    match dpy.commit(a, 0, 0, None, None) {
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => eprintln!("4 commit while pending: EBUSY OK"),
        Err(e) => return Err(fail(format!("commit while pending: {e}"))),
        Ok(f) => {
            // The flip may have landed already if a vblank fell in between.
            if dpy.status_v2()?.flips_done == 0 {
                return Err(fail(format!("commit while pending accepted (fault {f})")));
            }
            eprintln!("4 skipped: B landed before the second commit");
        }
    }
    if !reached(&out, 1, 500)? {
        return Err(fail("B's out-fence never fired".into()));
    }
    dpy.wait_idle(500)?;
    eprintln!("3 vsync flip landed, out-fence fired");

    // 5. In-fence gating: flip to A waits on `inf`.
    let f = dpy.commit(a, 0, 0, Some((&inf, 1)), Some((&out, 2)))?;
    if f != 0 {
        return Err(fail(format!("fenced flip fault={f}")));
    }
    std::thread::sleep(Duration::from_millis(100)); // ~6 vblanks
    if out.query()? >= 2 {
        return Err(fail("flip landed before its in-fence".into()));
    }
    if dpy.status_v2()?.flags & STATUS_COMMIT_PENDING == 0 {
        return Err(fail("fenced commit not pending".into()));
    }
    let t = Instant::now();
    inf.signal(1)?;
    if !reached(&out, 2, 500)? {
        return Err(fail("fenced flip never landed after its in-fence".into()));
    }
    eprintln!("5 in-fence gated the flip; landed {:?} after the signal", t.elapsed());

    // 6. Render-to-flip chain: the DMA copy into B signals the in-fence.
    scan.upload(1, &paint(w, h, 64), 0, Some((&inf, 2)))?;
    let f = dpy.commit(b, 0, 0, Some((&inf, 2)), Some((&out, 3)))?;
    if f != 0 {
        return Err(fail(format!("chained flip fault={f}")));
    }
    if !reached(&out, 3, 500)? {
        return Err(fail("chained flip never landed".into()));
    }
    eprintln!("6 copy -> in-fence -> flip -> out-fence OK");

    // 7. ASYNC lands at once.
    let before = dpy.status_v2()?.flips_done;
    let f = dpy.commit(a, COMMIT_ASYNC, 0, None, Some((&out, 4)))?;
    let st = dpy.status_v2()?;
    if f != 0 || st.flags & STATUS_COMMIT_PENDING != 0 || st.flips_done != before + 1 || out.query()? < 4 {
        return Err(fail(format!("async flip: fault={f} flags={:#x} flips {}->{}", st.flags, before, st.flips_done)));
    }
    eprintln!("7 async flip landed at once");

    // 8. Referee + fd checks.
    let f = dpy.commit(b, 0, w as u32 * 4 + 256 + 1, None, None);
    match f {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {} // not 256-aligned: rejected up front
        other => return Err(fail(format!("unaligned pitch: {other:?}, want EINVAL"))),
    }
    // Aligned but shorter than a line: since the isolation fix (73a94ec) the
    // kmod refuses it up front, because pitch x height must fit in the BO.
    match dpy.commit(b, 0, 256, None, None) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
        other => return Err(fail(format!("short pitch: {other:?}, want EINVAL"))),
    }
    let sys = vm.alloc(bytes, 0)?; // System BO: not scannable
    match dpy.commit(&sys, 0, 0, None, None) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
        other => return Err(fail(format!("System BO commit: {other:?}, want EINVAL"))),
    }
    let flips = dpy.status_v2()?.flips_done;
    let f = dpy.commit(b, COMMIT_TEST_ONLY, 0, None, None)?;
    let st = dpy.status_v2()?;
    if f != 0 || st.flips_done != flips || st.flags & STATUS_COMMIT_PENDING != 0 {
        return Err(fail("TEST_ONLY changed something".into()));
    }
    eprintln!("8 bad pitches refused, System BO refused, TEST_ONLY inert");

    // 9. Hold-last-frame across close/reopen.
    drop(dpy);
    let dpy = Display::open()?;
    let st = dpy.status_v2()?;
    if st.flags & STATUS_MODE_SET == 0 {
        return Err(fail("mode not held across close".into()));
    }
    let v0 = st.vblank_count;
    std::thread::sleep(Duration::from_millis(100));
    let v1 = dpy.status_v2()?.vblank_count;
    if v1 <= v0 {
        return Err(fail(format!("no vblanks after reopen ({v0} -> {v1})")));
    }
    eprintln!("9 hold-last-frame OK: still lit, vblank {v0} -> {v1}");
    Ok(())
}
