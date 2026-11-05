//! display_vrr — display ABI 2.2 variable refresh on gpusim.
//!
//! 1. CONNECTOR reports the desktop monitor's VRR range (48–120 Hz).
//! 2. COMMIT_VRR without MODESET is EINVAL.
//! 3. 1080p120 with VRR: CONNECTOR says VRR is active, and with nothing to
//!    show the monitor refreshes at its floor (~48 Hz), ~2.5x slower than
//!    the same mode at a fixed 120 Hz.
//! 4. Paced flips (a new frame every ~11 ms): with VRR each flip ends the
//!    frame it waits on (~1 refresh per flip); at a fixed 120 Hz a flip
//!    waits for the next 8.3 ms slot (~2 refreshes per flip).
//! 5. A MODESET without the flag turns VRR off.
//! 6. A monitor without VRR (VGA) refuses COMMIT_VRR; the desktop monitor
//!    is plugged back.
//!
//! Build (host cross): cargo build --target x86_64-unknown-freebsd --bin display_vrr
//! Run (guest): /root/d4/display_vrr

use std::io;
use std::time::{Duration, Instant};

use atrium_gpu::amd::{
    Bo, Display, EventWatch, Gpu, Scanout, Syncobj, COMMIT_VRR, CONN_HDMI21, NOTE_HOTPLUG,
    PLUG_DESKTOP, PLUG_VGA,
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

fn solid(w: usize, h: usize, bgr: [u8; 3]) -> Vec<u8> {
    let mut fb = vec![0u8; w * h * 4];
    for px in fb.chunks_exact_mut(4) {
        px.copy_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
    }
    fb
}

/// Vblanks counted over `ms` of wall time.
fn vblank_rate(dpy: &Display, ms: u64) -> io::Result<f64> {
    let v0 = dpy.status_v2()?.vblank_count;
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(ms));
    let v1 = dpy.status_v2()?.vblank_count;
    Ok((v1 - v0) as f64 / t0.elapsed().as_secs_f64())
}

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

/// `n` vsync'd flips between two buffers, each committed `gap_ms` after the
/// last one landed. Returns (refreshes per flip, mean commit->on-screen ms).
fn paced_flips(dpy: &Display, bufs: [&Bo<'_>; 2], done: &Syncobj<'_>, base: &mut u64, n: u32, gap_ms: u64)
    -> io::Result<(f64, f64)>
{
    let v0 = dpy.status_v2()?.vblank_count;
    let mut wait = 0f64;
    for i in 0..n {
        std::thread::sleep(Duration::from_millis(gap_ms));
        *base += 1;
        let t = Instant::now();
        let f = dpy.commit(bufs[(i % 2) as usize], 0, 0, None, Some((done, *base)))?;
        if f != 0 || !reached(done, *base, 2000)? {
            return Err(fail(format!("flip {i}: fault {f} or never on screen")));
        }
        wait += t.elapsed().as_secs_f64() * 1000.0;
    }
    let v1 = dpy.status_v2()?.vblank_count;
    Ok(((v1 - v0) as f64 / f64::from(n), wait / f64::from(n)))
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;
    let watch = EventWatch::new(&dpy, NOTE_HOTPLUG)?;

    // 1. The range.
    let c = dpy.connector_v2()?;
    if c.vrr != Some((48, 120)) {
        return Err(fail(format!("VRR range {:?}, want 48..120 Hz", c.vrr)));
    }
    let i120 = c
        .modes
        .iter()
        .position(|m| (m.width, m.height, m.refresh_mhz) == (1920, 1080, 120000))
        .ok_or_else(|| fail("no 1080p120 mode".into()))?;
    eprintln!("1 CONNECTOR: VRR 48-120 Hz OK");

    let bytes = 1920 * 1080 * 4;
    let scan = Scanout::with_buffers(&vm, bytes, 2)?;
    scan.upload(0, &solid(1920, 1080, [200, 90, 20]), 0, None)?;
    scan.upload(1, &solid(1920, 1080, [20, 160, 60]), 0, None)?;
    let bufs = [scan.buffer(0), scan.buffer(1)];
    let done = gpu.create_syncobj()?;
    let mut seq = 0u64;

    // 2. VRR needs a MODESET.
    match dpy.commit(bufs[0], COMMIT_VRR, 0, None, None) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => eprintln!("2 VRR without MODESET: EINVAL OK"),
        other => return Err(fail(format!("VRR without MODESET: {other:?}"))),
    }

    // 3. Idle refresh: fixed 120 Hz vs VRR floor.
    let f = dpy.modeset_vrr(bufs[0], i120 as u32, c.generation, 0, false, None, None)?;
    if f != 0 || dpy.connector_v2()?.vrr_active {
        return Err(fail(format!("fixed modeset: fault {f}")));
    }
    let fixed_rate = vblank_rate(&dpy, 2000)?;
    let (fixed_per_flip, fixed_ms) = paced_flips(&dpy, bufs, &done, &mut seq, 30, 11)?;
    let f = dpy.modeset_vrr(bufs[0], i120 as u32, c.generation, 0, true, None, None)?;
    let c2 = dpy.connector_v2()?;
    if f != 0 || !c2.vrr_active || c2.current != Some(i120) {
        return Err(fail(format!("VRR modeset: fault {f}, active {}, current {:?}", c2.vrr_active, c2.current)));
    }
    let vrr_rate = vblank_rate(&dpy, 2000)?;
    let ratio = fixed_rate / vrr_rate;
    eprintln!("3 idle refresh: fixed {fixed_rate:.1}/s, VRR {vrr_rate:.1}/s (x{ratio:.2}, 120/48 = 2.5)");
    if !(1.8..3.3).contains(&ratio) {
        return Err(fail(format!("idle VRR doesn't fall to the 48 Hz floor (x{ratio:.2})")));
    }

    // 4. Paced flips.
    let (vrr_per_flip, vrr_ms) = paced_flips(&dpy, bufs, &done, &mut seq, 30, 11)?;
    eprintln!(
        "4 flips every ~11 ms: fixed 120 Hz {fixed_per_flip:.2} refreshes/flip ({fixed_ms:.1} ms to screen), \
         VRR {vrr_per_flip:.2} ({vrr_ms:.1} ms)"
    );
    if vrr_per_flip > 1.35 || fixed_per_flip < 1.5 || vrr_per_flip >= fixed_per_flip {
        return Err(fail("VRR flips don't each end their frame".into()));
    }

    // 5. Off again.
    let f = dpy.modeset_vrr(bufs[0], i120 as u32, c.generation, 0, false, None, None)?;
    if f != 0 || dpy.connector_v2()?.vrr_active {
        return Err(fail("VRR still active after a plain MODESET".into()));
    }
    eprintln!("5 plain MODESET turns VRR off OK");

    // 6. A fixed-refresh monitor refuses it.
    dpy.sim_plug(CONN_HDMI21, PLUG_VGA)?;
    let _ = watch.wait(Some(Duration::from_secs(3)))?;
    let c3 = dpy.connector_v2()?;
    let r = dpy.modeset_vrr(bufs[0], 0, c3.generation, 0, true, None, None);
    dpy.sim_plug(CONN_HDMI21, PLUG_DESKTOP)?;
    let _ = watch.wait(Some(Duration::from_secs(3)))?;
    match r {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) && c3.vrr.is_none() => {}
        other => return Err(fail(format!("VRR on a VGA monitor: {other:?} (range {:?})", c3.vrr))),
    }
    let c4 = dpy.connector_v2()?;
    dpy.modeset_at(bufs[0], 0, c4.generation, 0)?;
    eprintln!("6 VGA monitor: no range, COMMIT_VRR EINVAL OK; desktop monitor back");
    Ok(())
}
