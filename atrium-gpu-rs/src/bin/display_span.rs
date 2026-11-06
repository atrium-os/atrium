//! display_span — display ABI 3.1, span groups (fresco-span.md §5), on gpusim.
//!
//! Three identical 1080p60 monitors (outputs 0-2) are locked into one sync
//! group with COMMIT_SYNC_GROUP, then:
//! 1. Refusals: a dark output (NO_MODE) and one at another timing
//!    (SYNC_MISMATCH) can't join; OUTPUTS shows the group, its master and
//!    the flip-locked members.
//! 2. One clock: the members' vblank counters advance in step (their
//!    differences never change).
//! 3. A flip commit naming only some flip-locked members is EINVAL.
//! 4. 100 group flips, each as one COMMIT_MULTI. In most of them one
//!    member's in-fence is signalled ~3 frames late; the other members'
//!    flips are held until it is. A snapshot of what every pipe scans out
//!    (sim_scanout), taken continuously, must never show two members on
//!    different frames: 100 of 100 frames land in the same vblank.
//! 5. A member drops out (§6): with its flip lock off the others flip
//!    without it while its own flip waits; the detector sees them differ,
//!    which shows it can.
//! 6. The master leaves: the next member drives the group; a mode set takes
//!    a member out; members = 0 dissolves the group.
//! 7. Variable refresh: a 48-120 Hz and a 60-120 Hz monitor at 1080p120
//!    with VRR idle at their own floors apart, and both at 60 Hz (the range
//!    both take) in a group; the 48 Hz one goes back to 48 when the group
//!    ends. VRR on one member and not the other is SYNC_MISMATCH.
//!
//! Build (host cross): cargo build --target x86_64-unknown-freebsd --bin display_span
//! Run (guest, as root, frescod stopped and the lease cleared): /root/d4/display_span

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::time::{Duration, Instant};

use atrium_gpu::amd::{
    Display, Gpu, OutputCommit, Scanout, FAULT_NO_MODE, FAULT_SYNC_MISMATCH, OUTPUT_FLIP_LOCKED,
    OUTPUT_SYNC_MASTER, PLUG_DESKTOP_TWIN, PLUG_DESKTOP_VRR60, PLUG_OFFICE, STATUS_COMMIT_PENDING,
    COMMIT_VRR,
};

const FRAMES: usize = 100;
const BUFS: usize = 3;

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

fn cleanup() {
    if let Ok(d) = Display::open() {
        let _ = d.commit_sync_group(1, 0, 0);
    }
    for n in 1..4 {
        let _ = plug(n, -1);
    }
}

/// Every pipe's scanout address, one snapshot (no vblank in between).
fn scanout() -> io::Result<[u64; 4]> {
    let name = CString::new("dev.atrium_gpu_amd_display.0.sim_scanout").unwrap();
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    let r = unsafe {
        libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null(), 0)
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let s = std::str::from_utf8(&buf[..len]).map_err(|e| fail(e.to_string()))?.trim_end_matches('\0');
    let mut out = [0u64; 4];
    for (i, w) in s.split(' ').enumerate().take(4) {
        out[i] = u64::from_str_radix(w, 16).map_err(|e| fail(format!("sim_scanout {s:?}: {e}")))?;
    }
    Ok(out)
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

fn pending(dpy: &Display, n: u32) -> io::Result<bool> {
    Ok(dpy.status_at(n)?.flags & STATUS_COMMIT_PENDING != 0)
}

fn wait_landed(dpy: &Display, outs: &[u32]) -> io::Result<()> {
    let t0 = Instant::now();
    for &n in outs {
        while pending(dpy, n)? {
            if t0.elapsed() > Duration::from_secs(5) {
                return Err(fail(format!("flip on output {n} never landed")));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    Ok(())
}

/// The members' vblank counts, read with no vblank in between.
fn vblanks(dpy: &Display, outs: &[u32]) -> io::Result<Vec<u64>> {
    for _ in 0..32 {
        let a: Vec<u64> = outs.iter().map(|&n| dpy.status_at(n).map(|s| s.vblank_count)).collect::<io::Result<_>>()?;
        let b = dpy.status_at(outs[0])?.vblank_count;
        if b == a[0] {
            return Ok(a);
        }
    }
    Err(fail("vblank counts never steady".into()))
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;
    cleanup();

    // Three identical monitors, lit together at 1080p60.
    plug(1, PLUG_DESKTOP_TWIN as i32)?;
    plug(2, PLUG_DESKTOP_TWIN as i32)?;
    let sc: Vec<Scanout> = (0..3).map(|_| Scanout::with_buffers(&vm, 1920 * 1080 * 4, BUFS)).collect::<io::Result<_>>()?;
    let mut lit = Vec::new();
    for n in 0..3u32 {
        let (i, g) = mode(&dpy, n, 1920, 1080, 60000)?;
        lit.push(OutputCommit::modeset(n, sc[n as usize].buffer(0), i, g));
    }
    let r = dpy.commit_multi(&lit, false)?;
    if r.failed.is_some() {
        return Err(fail(format!("light three: {r:?}")));
    }
    // Learn which address is which buffer: flip each up on its own.
    let mut which: HashMap<u64, (usize, usize)> = HashMap::new();
    for b in (0..BUFS).rev() {
        let parts: Vec<_> = (0..3u32).map(|n| OutputCommit::flip(n, Some(sc[n as usize].buffer(b)), true)).collect();
        let r = dpy.commit_multi(&parts, false)?;
        if r.failed.is_some() {
            return Err(fail(format!("flip buffer {b}: {r:?}")));
        }
        wait_landed(&dpy, &[0, 1, 2])?;
        let s = scanout()?;
        for n in 0..3 {
            if which.insert(s[n], (n, b)).is_some() {
                return Err(fail(format!("address {:#x} seen twice", s[n])));
            }
        }
    }
    // The frame index each member shows now (an Err if one shows something
    // this test never put up).
    let shown = || -> io::Result<[usize; 3]> {
        let s = scanout()?;
        let mut f = [0; 3];
        for n in 0..3 {
            let &(m, b) = which.get(&s[n]).ok_or_else(|| fail(format!("output {n} shows {:#x}", s[n])))?;
            if m != n {
                return Err(fail(format!("output {n} shows output {m}'s buffer")));
            }
            f[n] = b;
        }
        Ok(f)
    };

    // 1. Refusals, then the group.
    let r = dpy.commit_sync_group(1, 0b1111, 0b0111)?;
    if r.failed != Some(3) || r.reason != FAULT_NO_MODE {
        return Err(fail(format!("dark output 3 joined: {r:?}")));
    }
    plug(3, PLUG_OFFICE as i32)?;
    let s3 = Scanout::new(&vm, 2560 * 1440 * 4)?;
    let (i3, g3) = mode(&dpy, 3, 2560, 1440, 59950)?;
    let r = dpy.commit_multi(&[OutputCommit::modeset(3, s3.buffer(0), i3, g3)], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("light output 3: {r:?}")));
    }
    let r = dpy.commit_sync_group(1, 0b1111, 0b0111)?;
    if r.failed != Some(3) || r.reason != FAULT_SYNC_MISMATCH {
        return Err(fail(format!("1440p output 3 joined a 1080p group: {r:?}")));
    }
    if dpy.outputs()?.out.iter().any(|o| o.sync_group != 0) {
        return Err(fail("a refused group changed something".into()));
    }
    let r = dpy.commit_multi(&[OutputCommit::off(3)], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("output 3 off: {r:?}")));
    }
    plug(3, -1)?;
    let r = dpy.commit_sync_group(1, 0b0111, 0b0111)?;
    if r.failed.is_some() {
        return Err(fail(format!("group of three: {r:?}")));
    }
    let o = dpy.outputs()?;
    let groups: Vec<u32> = o.out.iter().map(|x| x.sync_group).collect();
    let master: Vec<bool> = o.out.iter().map(|x| x.flags & OUTPUT_SYNC_MASTER != 0).collect();
    let locked: Vec<bool> = o.out.iter().map(|x| x.flags & OUTPUT_FLIP_LOCKED != 0).collect();
    if groups != [1, 1, 1, 0] || master != [true, false, false, false] || locked != [true, true, true, false] {
        return Err(fail(format!("OUTPUTS: groups {groups:?} master {master:?} locked {locked:?}")));
    }
    eprintln!("1 dark and 1440p outputs refused (NO_MODE, SYNC_MISMATCH); group of three, output 0 master OK");

    // 2. One clock.
    let a = vblanks(&dpy, &[0, 1, 2])?;
    std::thread::sleep(Duration::from_secs(2));
    let b = vblanks(&dpy, &[0, 1, 2])?;
    let d: Vec<u64> = (0..3).map(|i| b[i] - a[i]).collect();
    if d[0] < 30 || d[1] != d[0] || d[2] != d[0] {
        return Err(fail(format!("vblanks in 2 s: {d:?}, want equal")));
    }
    eprintln!("2 one clock: {} vblanks on every member in 2 s OK", d[0]);

    // 3. Part of a locked group: EINVAL.
    let part = [OutputCommit::flip(0, Some(sc[0].buffer(1)), true), OutputCommit::flip(1, Some(sc[1].buffer(1)), true)];
    match dpy.commit_multi(&part, false) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
        other => return Err(fail(format!("flip of two of three locked members: {other:?}"))),
    }
    eprintln!("3 a flip naming two of three locked members is EINVAL OK");

    // 4. 100 group flips, one member's in-fence late in most of them.
    let fences: Vec<_> = (0..3).map(|_| gpu.create_syncobj()).collect::<io::Result<_>>()?;
    let mut fence_v = [0u64; 3];
    let mut snaps = 0u64;
    let mut held_snaps = 0u64;
    let mut cur = shown()?;
    if cur != [0, 0, 0] {
        return Err(fail(format!("start: {cur:?}")));
    }
    for f in 0..FRAMES {
        let next = (cur[0] + 1) % BUFS;
        let late = (f % 4 != 3).then_some(f % 3);
        let mut parts: Vec<_> = (0..3u32).map(|n| OutputCommit::flip(n, Some(sc[n as usize].buffer(next)), true)).collect();
        if let Some(m) = late {
            fence_v[m] += 1;
            parts[m].in_fence = Some((&fences[m], fence_v[m]));
        }
        let r = dpy.commit_multi(&parts, false)?;
        if r.failed.is_some() {
            return Err(fail(format!("frame {f}: {r:?}")));
        }
        if let Some(m) = late {
            // ~3 frames with one member's flip not yet queued: the
            // others must hold.
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_millis(50) {
                let s = shown()?;
                snaps += 1;
                held_snaps += 1;
                if s != cur {
                    return Err(fail(format!("frame {f}: {s:?} while output {m}'s fence is unsignalled (was {cur:?})")));
                }
            }
            fences[m].signal(fence_v[m])?;
        }
        let t0 = Instant::now();
        loop {
            let s = shown()?;
            snaps += 1;
            if s[0] != s[1] || s[0] != s[2] {
                return Err(fail(format!("frame {f}: members on different frames {s:?}")));
            }
            if s == [next; 3] {
                break;
            }
            if t0.elapsed() > Duration::from_secs(5) {
                return Err(fail(format!("frame {f}: never landed, {s:?}")));
            }
        }
        wait_landed(&dpy, &[0, 1, 2])?;
        cur = [next; 3];
    }
    let done: Vec<u64> = (0..3).map(|n| dpy.status_at(n).map(|s| s.flips_done)).collect::<io::Result<_>>()?;
    if done[1] != done[0] || done[2] != done[0] {
        return Err(fail(format!("flips done differ: {done:?}")));
    }
    eprintln!(
        "4 {FRAMES} of {FRAMES} group flips landed in one vblank on all three ({snaps} snapshots, {held_snaps} while a member's fence was late, none split) OK"
    );

    // 5. A member drops out: flip lock off, it keeps the clock, the others
    // flip without it while its own flip waits on a fence.
    let r = dpy.commit_sync_group(1, 0b0111, 0b0011)?;
    if r.failed.is_some() {
        return Err(fail(format!("drop output 2's lock: {r:?}")));
    }
    let next = (cur[0] + 1) % BUFS;
    fence_v[2] += 1;
    let mut f2 = OutputCommit::flip(2, Some(sc[2].buffer(next)), true);
    f2.in_fence = Some((&fences[2], fence_v[2]));
    let r = dpy.commit_multi(&[f2], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("output 2 alone: {r:?}")));
    }
    let pair = [OutputCommit::flip(0, Some(sc[0].buffer(next)), true), OutputCommit::flip(1, Some(sc[1].buffer(next)), true)];
    let r = dpy.commit_multi(&pair, false)?;
    if r.failed.is_some() {
        return Err(fail(format!("outputs 0 and 1 without 2: {r:?}")));
    }
    wait_landed(&dpy, &[0, 1])?;
    let s = shown()?;
    if s != [next, next, cur[2]] || !pending(&dpy, 2)? {
        return Err(fail(format!("dropout: {s:?}, want [{next}, {next}, {}]", cur[2])));
    }
    fences[2].signal(fence_v[2])?;
    wait_landed(&dpy, &[2])?;
    let a = vblanks(&dpy, &[0, 1, 2])?;
    std::thread::sleep(Duration::from_millis(500));
    let b = vblanks(&dpy, &[0, 1, 2])?;
    if b[2] - a[2] != b[0] - a[0] {
        return Err(fail(format!("the dropped member left the clock: {a:?} -> {b:?}")));
    }
    eprintln!("5 a member out of the flip lock: the others flip without it, it keeps the clock; the detector saw {s:?} OK");

    // 6. The master leaves; a mode set takes a member out; dissolve.
    let r = dpy.commit_sync_group(1, 0b0110, 0b0110)?;
    let o = dpy.outputs()?;
    if r.failed.is_some() || o.out[0].sync_group != 0 || o.out[1].flags & OUTPUT_SYNC_MASTER == 0 {
        return Err(fail(format!("master hand-over: {r:?} {o:?}")));
    }
    let a = vblanks(&dpy, &[1, 2])?;
    std::thread::sleep(Duration::from_millis(500));
    let b = vblanks(&dpy, &[1, 2])?;
    if b[0] - a[0] < 10 || b[1] - a[1] != b[0] - a[0] {
        return Err(fail(format!("after hand-over: {a:?} -> {b:?}")));
    }
    let (i2, g2) = mode(&dpy, 2, 1920, 1080, 60000)?;
    let r = dpy.commit_multi(&[OutputCommit::modeset(2, sc[2].buffer(0), i2, g2)], false)?;
    let o = dpy.outputs()?;
    if r.failed.is_some() || o.out[2].sync_group != 0 || o.out[1].sync_group != 1 {
        return Err(fail(format!("mode set on a member: {r:?} {o:?}")));
    }
    let r = dpy.commit_sync_group(1, 0, 0)?;
    if r.failed.is_some() || dpy.outputs()?.out.iter().any(|x| x.sync_group != 0) {
        return Err(fail(format!("dissolve: {r:?}")));
    }
    eprintln!("6 master hand-over to output 1; a mode set takes a member out; dissolved OK");

    // 7. Variable refresh over the range both monitors take.
    plug(2, PLUG_DESKTOP_VRR60 as i32)?;
    let vrr_on = |n: u32, vrr: bool| -> io::Result<OutputCommit<'_, '_>> {
        let (i, g) = mode(&dpy, n, 1920, 1080, 120000)?;
        let mut c = OutputCommit::modeset(n, sc[n as usize].buffer(0), i, g);
        if vrr {
            c.flags |= COMMIT_VRR;
        }
        Ok(c)
    };
    let r = dpy.commit_multi(&[vrr_on(1, true)?, vrr_on(2, true)?], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("1080p120 VRR on outputs 1 and 2: {r:?}")));
    }
    // Idle refresh, Hz, over a second.
    let rate = || -> io::Result<[f64; 2]> {
        let a = vblanks(&dpy, &[1, 2])?;
        let t = Instant::now();
        std::thread::sleep(Duration::from_millis(1000));
        let b = vblanks(&dpy, &[1, 2])?;
        let s = t.elapsed().as_secs_f64();
        Ok([(b[0] - a[0]) as f64 / s, (b[1] - a[1]) as f64 / s])
    };
    let near = |hz: f64, want: f64| (hz - want).abs() < 5.0;
    let apart = rate()?;
    if !near(apart[0], 48.0) || !near(apart[1], 60.0) {
        return Err(fail(format!("idle VRR apart: {apart:?} Hz, want 48 and 60")));
    }
    let r = dpy.commit_sync_group(1, 0b0110, 0b0110)?;
    if r.failed.is_some() {
        return Err(fail(format!("VRR group: {r:?}")));
    }
    let together = rate()?;
    if !near(together[0], 60.0) || !near(together[1], 60.0) {
        return Err(fail(format!("idle VRR in a group: {together:?} Hz, want 60 and 60")));
    }
    let r = dpy.commit_sync_group(1, 0, 0)?;
    let after = rate()?;
    if r.failed.is_some() || !near(after[0], 48.0) {
        return Err(fail(format!("VRR group dissolved: {r:?}, {after:?} Hz, want output 1 at 48")));
    }
    let r = dpy.commit_multi(&[vrr_on(1, false)?], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("output 1 fixed 120 Hz: {r:?}")));
    }
    let r = dpy.commit_sync_group(1, 0b0110, 0b0110)?;
    if r.failed != Some(2) || r.reason != FAULT_SYNC_MISMATCH {
        return Err(fail(format!("fixed and variable refresh joined: {r:?}")));
    }
    eprintln!(
        "7 VRR idle apart {:.0}/{:.0} Hz, in a group {:.0}/{:.0} Hz, after {:.0} Hz; fixed + VRR refused OK",
        apart[0], apart[1], together[0], together[1], after[0]
    );

    let r = dpy.commit_multi(&[OutputCommit::off(1), OutputCommit::off(2)], false)?;
    if r.failed.is_some() {
        return Err(fail(format!("off: {r:?}")));
    }
    cleanup();
    Ok(())
}
