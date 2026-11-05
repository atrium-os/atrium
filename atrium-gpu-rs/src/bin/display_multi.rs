//! display_multi — display ABI 3.0, several monitors on one node, on gpusim.
//!
//! 1. OUTPUTS lists four outputs. Monitors plugged into outputs 1 and 2 (an
//!    office monitor and a twin of the desktop monitor) show up with
//!    identities that tell them apart, and a monitor with a serial number
//!    keeps its identity when moved to another output.
//! 2. One COMMIT_MULTI lights all three at once (1080p120, 1440p60,
//!    1440p60); OUTPUTS shows them lit and the bandwidth they draw.
//! 3. With the shared budget lowered, TEST_ONLY of a fourth output (4K60)
//!    names it and says BANDWIDTH; the real commit says the same and changes
//!    nothing.
//! 4. A topology commit the hardware refuses part way (re-mode output 1,
//!    light output 3 with no monitor) is rolled back: output 1 is back at
//!    its old mode and still flips.
//! 5. Each output flips at its own rate: per-output vblank counts follow
//!    120 Hz and 60 Hz, and one kqueue registration sees both outputs.
//! 6. Unplugging output 1 bumps the topology generation while outputs 0 and
//!    2 keep flipping; turning it off gives its bandwidth back.
//! Then everything is put back: outputs 1-3 off and empty, output 0 at its
//! preferred mode, the budget restored.
//!
//! Build (host cross): cargo build --target x86_64-unknown-freebsd --bin display_multi
//! Run (guest, as root): /root/d4/display_multi

use std::io;
use std::time::{Duration, Instant};

use atrium_gpu::amd::{
    note_output, Display, Gpu, OutputCommit, Scanout, VblankWatch, FAULT_BANDWIDTH,
    FAULT_NO_OUTPUT, PLUG_4K, PLUG_DESKTOP_TWIN, PLUG_OFFICE, STATUS_COMMIT_PENDING,
};

fn main() {
    match run() {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            cleanup();
            std::process::exit(1);
        }
    }
}

fn fail(msg: String) -> io::Error { io::Error::other(msg) }

fn sysctl(name: &str, value: &str) -> io::Result<()> {
    let st = std::process::Command::new("sysctl")
        .arg(format!("dev.atrium_gpu_amd_display.0.{name}={value}"))
        .stdout(std::process::Stdio::null())
        .status()?;
    if !st.success() {
        return Err(fail(format!("sysctl {name}={value} failed")));
    }
    Ok(())
}

fn plug(output: u32, mode: i32) -> io::Result<()> { sysctl("sim_plug_at", &format!("{output}:{mode}")) }

const BUDGET: u32 = 2_400_000;

/// Put the simulator back as the other tests expect it.
fn cleanup() {
    for n in 1..4 {
        let _ = plug(n, -1);
    }
    let _ = sysctl("sim_bw_budget_khz", &BUDGET.to_string());
}

/// The index of `w`x`h`@`mhz` in output `n`'s modes, and its generation.
fn mode(dpy: &Display, n: u32, w: u32, h: u32, mhz: u32) -> io::Result<(u32, u32)> {
    let (c, _) = dpy.connector_at(n)?;
    let i = c
        .modes
        .iter()
        .position(|m| m.width == w && m.height == h && m.refresh_mhz.abs_diff(mhz) < 100)
        .ok_or_else(|| fail(format!("output {n}: no {w}x{h}@{mhz}")))?;
    Ok((i as u32, c.generation))
}

/// Flip output `n` to `bo` and wait until it is on screen.
fn flip_wait(dpy: &Display, n: u32, bo: &atrium_gpu::amd::Bo<'_>) -> io::Result<()> {
    let r = dpy.commit_multi(&[OutputCommit::flip(n, Some(bo), true)], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("flip on output {n}: {r:?}")));
    }
    let t0 = Instant::now();
    while dpy.status_at(n)?.flags & STATUS_COMMIT_PENDING != 0 {
        if t0.elapsed() > Duration::from_secs(5) {
            return Err(fail(format!("flip on output {n} never landed")));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;
    cleanup();

    // 1. Outputs and identities.
    let o = dpy.outputs()?;
    if o.out.len() != 4 || !o.out[0].connected() || o.out[1..].iter().any(|x| x.connected()) {
        return Err(fail(format!("outputs at start: {o:?}")));
    }
    plug(1, PLUG_OFFICE as i32)?;
    plug(2, PLUG_DESKTOP_TWIN as i32)?;
    let o = dpy.outputs()?;
    let ids: Vec<u64> = o.out.iter().map(|x| x.identity).collect();
    if ids[0] == 0 || ids[1] == 0 || ids[2] == 0 || ids[3] != 0 || ids[0] == ids[1] || ids[0] == ids[2] || ids[1] == ids[2] {
        return Err(fail(format!("identities: {ids:x?}")));
    }
    let (_, twin_id) = dpy.connector_at(2)?;
    plug(2, -1)?;
    plug(3, PLUG_DESKTOP_TWIN as i32)?;
    let moved = dpy.outputs()?.out[3].identity;
    plug(3, -1)?;
    plug(2, PLUG_DESKTOP_TWIN as i32)?;
    if moved != twin_id || dpy.outputs()?.out[2].identity != twin_id {
        return Err(fail(format!("twin moved: {moved:x} vs {twin_id:x}")));
    }
    eprintln!("1 four outputs; three monitors told apart; a monitor keeps its identity across outputs OK");

    // 2. Three monitors lit in one commit.
    let (i0, g0) = mode(&dpy, 0, 1920, 1080, 120000)?;
    let (i1, g1) = mode(&dpy, 1, 2560, 1440, 59950)?;
    let (i2, g2) = mode(&dpy, 2, 2560, 1440, 59950)?;
    let s0 = Scanout::with_buffers(&vm, 1920 * 1080 * 4, 2)?;
    let s1 = Scanout::with_buffers(&vm, 2560 * 1440 * 4, 2)?;
    let s2 = Scanout::with_buffers(&vm, 2560 * 1440 * 4, 2)?;
    let s3 = Scanout::with_buffers(&vm, 3840 * 2160 * 4, 1)?;
    let r = dpy.commit_multi(
        &[
            OutputCommit::modeset(0, s0.buffer(0), i0, g0),
            OutputCommit::modeset(1, s1.buffer(0), i1, g1),
            OutputCommit::modeset(2, s2.buffer(0), i2, g2),
        ],
        false,
    )?;
    let o = dpy.outputs()?;
    let lit: Vec<(u32, u32, u32)> = o.out.iter().map(|x| (x.width, x.height, x.refresh_mhz / 1000)).collect();
    if r.failed.is_some() || !o.out[..3].iter().all(|x| x.lit()) || o.out[3].lit() {
        return Err(fail(format!("three monitors: {r:?} {o:?}")));
    }
    if lit[..3] != [(1920, 1080, 120), (2560, 1440, 59), (2560, 1440, 59)] || o.bandwidth_used_khz != 297_000 + 2 * 241_500 {
        return Err(fail(format!("three monitors lit: {lit:?}, {} kHz", o.bandwidth_used_khz)));
    }
    eprintln!("2 three monitors lit in one commit, {} MHz of {} OK", o.bandwidth_used_khz / 1000, o.bandwidth_khz / 1000);

    // 3. A fourth past the budget: named, with the reason, nothing changed.
    // 780 MHz lit + 594 MHz of 4K > 1200; 538.5 + 594 fits.
    sysctl("sim_bw_budget_khz", "1200000")?;
    plug(3, PLUG_4K as i32)?;
    let (i3, g3) = mode(&dpy, 3, 3840, 2160, 60000)?;
    let four = [OutputCommit::modeset(3, s3.buffer(0), i3, g3)];
    let t = dpy.commit_multi(&four, true)?;
    if t.failed != Some(0) || t.reason != FAULT_BANDWIDTH {
        return Err(fail(format!("TEST_ONLY 4K: {t:?}, want output 3 BANDWIDTH")));
    }
    let r = dpy.commit_multi(&four, false)?;
    let after = dpy.outputs()?;
    if r != t || after.out[3].lit() || after.bandwidth_used_khz != o.bandwidth_used_khz {
        return Err(fail(format!("4K commit: {r:?} {after:?}")));
    }
    // Room made in the same commit: output 1 off, 4K on output 3.
    let swap = [OutputCommit::off(1), OutputCommit::modeset(3, s3.buffer(0), i3, g3)];
    let t = dpy.commit_multi(&swap, true)?;
    if t.failed.is_some() {
        return Err(fail(format!("TEST_ONLY swap: {t:?}")));
    }
    // An output named twice is refused before anything else.
    let twice = [OutputCommit::off(1), OutputCommit::off(1)];
    let t = dpy.commit_multi(&twice, true)?;
    if t.failed != Some(1) || t.reason != FAULT_NO_OUTPUT {
        return Err(fail(format!("output named twice: {t:?}")));
    }
    sysctl("sim_bw_budget_khz", &BUDGET.to_string())?;
    plug(3, -1)?;
    eprintln!("3 a fourth monitor past the budget: TEST_ONLY names it (BANDWIDTH), nothing changed OK");

    // 4. Refused part way: rolled back.
    let (i1b, g1b) = mode(&dpy, 1, 1920, 1080, 60000)?;
    let before = dpy.outputs()?;
    let r = dpy.commit_multi(
        &[OutputCommit::modeset(1, s1.buffer(1), i1b, g1b), OutputCommit::modeset(3, s3.buffer(0), 0, 0)],
        false,
    )?;
    let after = dpy.outputs()?;
    if r.failed != Some(1) || r.reason == 0 {
        return Err(fail(format!("light an empty output: {r:?}")));
    }
    if after.out[1] != before.out[1] || after.out[0] != before.out[0] || after.out[3].lit() {
        return Err(fail(format!("not rolled back: {:?} -> {:?}", before.out, after.out)));
    }
    flip_wait(&dpy, 1, s1.buffer(1))?;
    eprintln!("4 a commit refused part way (fault {}) is rolled back; output 1 still flips OK", r.reason);

    // 5. Own rates.
    let watch = VblankWatch::new(&dpy, note_output(0) | note_output(1))?;
    let v0 = dpy.status_at(0)?.vblank_count;
    let v1 = dpy.status_at(1)?.vblank_count;
    let mut seen = 0u32;
    let t0 = Instant::now();
    let mut k = 0;
    while t0.elapsed() < Duration::from_secs(4) {
        k += 1;
        flip_wait(&dpy, 0, s0.buffer(k % 2))?;
        flip_wait(&dpy, 1, s1.buffer(k % 2))?;
        if let Some((_, f)) = watch.wait(Some(Duration::from_millis(0)))? {
            seen |= f;
        }
    }
    let d0 = dpy.status_at(0)?.vblank_count - v0;
    let d1 = dpy.status_at(1)?.vblank_count - v1;
    let ratio = d0 as f64 / d1.max(1) as f64;
    if !(1.5..2.6).contains(&ratio) {
        return Err(fail(format!("vblanks: output 0 {d0}, output 1 {d1}: ratio {ratio:.2}, want ~2")));
    }
    if seen & (note_output(0) | note_output(1)) != note_output(0) | note_output(1) {
        return Err(fail(format!("one registration for two outputs saw fflags {seen:#x}")));
    }
    eprintln!("5 own rates: {d0} vs {d1} vblanks ({ratio:.2}x), {k} flips each; one kqueue sees both OK");

    // 6. Unplug output 1 while the others keep flipping.
    let gen0 = dpy.outputs()?.topology_generation;
    plug(1, -1)?;
    let t0 = Instant::now();
    loop {
        let o = dpy.outputs()?;
        if o.topology_generation != gen0 && !o.out[1].connected() {
            break;
        }
        if t0.elapsed() > Duration::from_secs(5) {
            return Err(fail(format!("no hotplug on output 1: {o:?}")));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    for k in 0..4 {
        flip_wait(&dpy, 0, s0.buffer(k % 2))?;
        flip_wait(&dpy, 2, s2.buffer(k % 2))?;
    }
    let r = dpy.commit_multi(&[OutputCommit::off(1)], false)?;
    let o = dpy.outputs()?;
    if r.failed.is_some() || o.out[1].lit() || o.bandwidth_used_khz != 297_000 + 241_500 {
        return Err(fail(format!("output 1 off: {r:?} {o:?}")));
    }
    eprintln!("6 unplug: topology generation {gen0} -> {}, outputs 0 and 2 kept flipping; off frees its bandwidth OK", o.topology_generation);

    // Put it back: output 2 off, output 0 at its preferred mode.
    let (ip, gp) = mode(&dpy, 0, 1920, 1080, 60000)?;
    let r = dpy.commit_multi(&[OutputCommit::off(2), OutputCommit::modeset(0, s0.buffer(0), ip, gp)], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("restore: {r:?}")));
    }
    cleanup();
    Ok(())
}
