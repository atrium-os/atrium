//! display_modes — display ABI 2.1 modes, EDID and hotplug on gpusim.
//!
//! 1. CONNECTOR lists the desktop monitor's modes (CTA-861 extension parsed):
//!    1080p60 preferred, 1080p120, 720p60, 640x480, 1440p60.
//! 2. MODESET at every index lights that mode: STATUS reports its refresh,
//!    CONNECTOR its index, and the vblank rate follows it (120 Hz ~ 2x 60 Hz).
//! 3. An index past the list is EINVAL.
//! 4. Unplug: a NOTE_HOTPLUG event with the new generation; CONNECTOR shows
//!    no monitor; a MODESET with the old generation is ESTALE.
//! 5. Plug a VGA-only monitor: an event, one mode, and it lights.
//! 6. Vblanks alone never wake a hotplug watch.
//! 7. Swap back to the desktop monitor and light its preferred mode.
//!
//! Build (host cross): cargo build --target x86_64-unknown-freebsd --bin display_modes
//! Run (guest): /root/d4/display_modes

use std::io;
use std::time::{Duration, Instant};

use atrium_gpu::amd::{
    Display, EventWatch, Gpu, Scanout, CONN_HDMI21, MODE_PREFERRED, NOTE_HOTPLUG, PLUG_DESKTOP,
    PLUG_VGA, UNPLUG,
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

/// Vertical bars whose count tells the modes apart on a screenshot.
fn paint(w: usize, h: usize, bars: usize) -> Vec<u8> {
    let mut fb = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let k = x * bars / w;
            let (b, g, r) = if k % 2 == 0 { (40, 160, 230) } else { (230, 230, 230) };
            let edge = y < 8 || y >= h - 8 || x < 8 || x >= w - 8;
            let px = if edge { [0, 0, 255, 255] } else { [b, g, r, 255] };
            fb[(y * w + x) * 4..][..4].copy_from_slice(&px);
        }
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

fn expect_hotplug(w: &EventWatch, what: &str) -> io::Result<u64> {
    match w.wait(Some(Duration::from_secs(3)))? {
        Some(e) if e.hotplug => Ok(e.data),
        other => Err(fail(format!("{what}: no hotplug event ({other:?})"))),
    }
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;
    let watch = EventWatch::new(&dpy, NOTE_HOTPLUG)?;

    // 1. The mode list.
    let c = dpy.connector_v2()?;
    let list: Vec<String> = c
        .modes
        .iter()
        .map(|m| format!("{}x{}@{:.2}", m.width, m.height, m.refresh_mhz as f64 / 1000.0))
        .collect();
    eprintln!("connector gen {} edid {} bytes, modes: {}", c.generation, c.edid.len(), list.join(", "));
    let want = [(1920, 1080, 60000), (1920, 1080, 120000), (1280, 720, 60000), (640, 480, 59940), (2560, 1440, 59950)];
    let got: Vec<(u32, u32, u32)> = c.modes.iter().map(|m| (m.width, m.height, m.refresh_mhz)).collect();
    if !c.connected || c.edid.len() != 256 || got != want || c.modes[0].flags & MODE_PREFERRED == 0 {
        return Err(fail(format!("connector: {c:?}")));
    }
    eprintln!("1 CONNECTOR: 5 modes, preferred first, 256-byte EDID OK");

    // 2. Every mode lights.
    let biggest = c.modes.iter().map(|m| m.width as u64 * m.height as u64 * 4).max().unwrap();
    let scan = Scanout::new(&vm, biggest)?;
    let bo = scan.buffer(0);
    let mut rate = [0f64; 2];
    for (i, m) in c.modes.iter().enumerate() {
        scan.upload(0, &paint(m.width as usize, m.height as usize, i + 2), 0, None)?;
        let f = dpy.modeset_at(bo, i as u32, c.generation, 0)?;
        let st = dpy.status_v2()?;
        let cur = dpy.connector_v2()?.current;
        if f != 0 || st.refresh_mhz != m.refresh_mhz || cur != Some(i) {
            return Err(fail(format!("mode {i}: fault {f}, refresh {}, current {cur:?}", st.refresh_mhz)));
        }
        if i < 2 {
            rate[i] = vblank_rate(&dpy, 1500)?;
        }
        eprintln!("2 mode {i} {}x{}@{} lit", m.width, m.height, m.refresh_mhz);
        if std::env::var_os("HOLD").is_some() {
            std::thread::sleep(Duration::from_secs(4)); // time for a screendump
        }
    }
    let ratio = rate[1] / rate[0];
    eprintln!("2 vblank rate: 60 Hz mode {:.1}/s, 120 Hz mode {:.1}/s (x{ratio:.2})", rate[0], rate[1]);
    if !(1.5..2.5).contains(&ratio) {
        return Err(fail(format!("120 Hz mode doesn't run ~2x the 60 Hz one (x{ratio:.2})")));
    }

    // 3. Out of range.
    match dpy.modeset_at(bo, c.modes.len() as u32, c.generation, 0) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => eprintln!("3 index past the list: EINVAL OK"),
        other => return Err(fail(format!("index past the list: {other:?}"))),
    }

    // 4. Unplug.
    dpy.sim_plug(CONN_HDMI21, UNPLUG)?;
    let gen = expect_hotplug(&watch, "unplug")?;
    let c2 = dpy.connector_v2()?;
    if c2.connected || !c2.modes.is_empty() || u64::from(c2.generation) != gen || c2.generation == c.generation {
        return Err(fail(format!("after unplug: {c2:?}, event gen {gen}")));
    }
    match dpy.modeset_at(bo, 0, c.generation, 0) {
        Err(e) if e.raw_os_error() == Some(libc::ESTALE) => {}
        other => return Err(fail(format!("stale generation: {other:?}, want ESTALE"))),
    }
    eprintln!("4 unplug: event gen {gen}, no monitor, old generation ESTALE OK");

    // 5. A VGA-only monitor.
    dpy.sim_plug(CONN_HDMI21, PLUG_VGA)?;
    let gen = expect_hotplug(&watch, "plug VGA")?;
    let c3 = dpy.connector_v2()?;
    if !c3.connected || c3.modes.len() != 1 || c3.modes[0].width != 640 || u64::from(c3.generation) != gen {
        return Err(fail(format!("after VGA plug: {c3:?}")));
    }
    scan.upload(0, &paint(640, 480, 3), 0, None)?;
    let f = dpy.modeset_at(bo, 0, c3.generation, 0)?;
    if f != 0 || dpy.status_v2()?.refresh_mhz != c3.modes[0].refresh_mhz {
        return Err(fail(format!("VGA modeset fault {f}")));
    }
    eprintln!("5 VGA monitor: event, 1 mode ({} mHz), lit OK", c3.modes[0].refresh_mhz);

    // 6. Vblanks don't wake a hotplug-only watch.
    if let Some(e) = watch.wait(Some(Duration::from_millis(300)))? {
        return Err(fail(format!("hotplug watch woke with no hotplug: {e:?}")));
    }
    eprintln!("6 no hotplug, no event OK");

    // 7. Back to the desktop monitor (a swap: one event).
    dpy.sim_plug(CONN_HDMI21, PLUG_DESKTOP)?;
    expect_hotplug(&watch, "swap back")?;
    let c4 = dpy.connector_v2()?;
    let m = c4.modes[0];
    scan.upload(0, &paint(m.width as usize, m.height as usize, 2), 0, None)?;
    let f = dpy.modeset_at(bo, 0, c4.generation, 0)?;
    if c4.modes.len() != 5 || f != 0 {
        return Err(fail(format!("after swap back: {} modes, fault {f}", c4.modes.len())));
    }
    eprintln!("7 swap back to the desktop monitor, preferred mode lit OK");
    Ok(())
}
