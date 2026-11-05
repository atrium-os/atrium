//! display_hdr — display ABI 2.3 HDR on gpusim.
//!
//! 1. CONNECTOR and HDR report the desktop monitor's HDR: SDR, PQ and HLG,
//!    BT.2020, about 604 / 400 / 0.05 cd/m².
//! 2. An EOTF the monitor doesn't offer (traditional HDR) is EINVAL.
//! 3. HDR10 (PQ, BT.2020, metadata) set, then a 10-bit flip: the monitor
//!    only switches with the commit, then decodes PQ, BT.2020, MaxCLL /
//!    MaxFALL and a 10-bit surface (the simulator's sim_sink readback).
//! 4. HLG, then back to SDR with an 8-bit flip: the infoframes stop.
//! 5. A monitor without HDR (VGA) offers SDR only and refuses PQ.
//! 6. HDR10 on, then a monitor swap to one without HDR: the next MODESET
//!    falls back to SDR. The desktop monitor is plugged back.
//!
//! Build (host cross): cargo build --target x86_64-unknown-freebsd --bin display_hdr
//! Run (guest): /root/d4/display_hdr

use std::io;
use std::time::Duration;

use atrium_gpu::amd::{
    Display, EventWatch, Gpu, Hdr, Scanout, COLORIMETRY_BT2020, CONN_HDMI21, EOTF_HLG, EOTF_PQ,
    EOTF_SDR, EOTF_TRAD_HDR, FORMAT_XRGB2101010, FORMAT_XRGB8888, NOTE_HOTPLUG, PLUG_1080P,
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

/// A solid frame: `word` is one pixel, in either 32-bit format.
fn solid(w: usize, h: usize, word: u32) -> Vec<u8> {
    let mut fb = vec![0u8; w * h * 4];
    for px in fb.chunks_exact_mut(4) {
        px.copy_from_slice(&word.to_le_bytes());
    }
    fb
}

/// The simulated monitor's view of the stream (sysctl sim_sink).
fn sink() -> io::Result<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "dev.atrium_gpu_amd_display.0.sim_sink"])
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn expect_sink(step: &str, want: &str) -> io::Result<()> {
    let got = sink()?;
    if got != want {
        return Err(fail(format!("{step}: monitor sees \"{got}\", want \"{want}\"")));
    }
    Ok(())
}

fn einval(r: io::Result<impl std::fmt::Debug>, what: &str) -> io::Result<()> {
    match r {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => Err(fail(format!("{what}: {other:?}, want EINVAL"))),
    }
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;
    let dpy = Display::open()?;
    let watch = EventWatch::new(&dpy, NOTE_HOTPLUG)?;

    // 1. What the monitor offers.
    let c = dpy.connector_v2()?;
    let (now, caps) = dpy.hdr()?;
    let want_eotfs = 1 << EOTF_SDR | 1 << EOTF_PQ | 1 << EOTF_HLG;
    if !c.hdr || caps.eotfs != want_eotfs || caps.colorimetry != 0b11 {
        return Err(fail(format!("HDR caps: connector {} {caps:?}", c.hdr)));
    }
    if caps.max_nits != 604 || caps.max_fall_nits != 400 || !(480..500).contains(&caps.min_nits_x10000) {
        return Err(fail(format!("HDR luminance: {caps:?}")));
    }
    if now != Hdr::default() {
        return Err(fail(format!("signalled before any SET: {now:?}")));
    }
    eprintln!(
        "1 HDR: EOTFs SDR+PQ+HLG, BT.2020, {} / {} / {:.3} cd/m2 OK",
        caps.max_nits,
        caps.max_fall_nits,
        f64::from(caps.min_nits_x10000) / 10000.0
    );

    let i60 = c.modes.iter().position(|m| (m.width, m.height, m.refresh_mhz) == (1920, 1080, 60000));
    let i60 = i60.ok_or_else(|| fail("no 1080p60".into()))? as u32;
    let bytes = 1920 * 1080 * 4;
    let scan = Scanout::with_buffers(&vm, bytes, 3)?;
    scan.upload(0, &solid(1920, 1080, 0xff20_4080), 0, None)?; // 8-bit
    // 10-bit: R 0x200, G 0x100, B 0x3ff (PQ code values).
    let deep = (0x200u32 << 20) | (0x100 << 10) | 0x3ff;
    scan.upload(1, &solid(1920, 1080, deep), 0, None)?;
    scan.upload(2, &solid(1920, 1080, deep ^ 0xff), 0, None)?;
    let done = gpu.create_syncobj()?;
    let mut seq = 0u64;
    let mut flip = |buf: usize| -> io::Result<()> {
        seq += 1;
        let f = dpy.commit(scan.buffer(buf), 0, 0, None, Some((&done, seq)))?;
        if f != 0 || done.wait(seq, 2000).is_err() {
            return Err(fail(format!("flip to {buf}: fault {f} or never on screen")));
        }
        Ok(())
    };

    dpy.set_format(FORMAT_XRGB8888);
    let f = dpy.modeset_at(scan.buffer(0), i60, c.generation, 0)?;
    if f != 0 {
        return Err(fail(format!("SDR modeset: fault {f}")));
    }
    expect_sink("SDR mode set", "eotf=0 bt2020=0 cll=0 fall=0 bpc=8")?;

    // 2. Not offered.
    einval(dpy.set_hdr(&Hdr { eotf: EOTF_TRAD_HDR, ..Default::default() }), "traditional HDR")?;
    eprintln!("2 an EOTF the monitor doesn't offer: EINVAL OK");

    // 3. HDR10 goes out with the next commit, not before.
    let hdr10 = Hdr::hdr10(1000, 800, 300);
    dpy.set_hdr(&hdr10)?;
    expect_sink("HDR10 set, nothing committed yet", "eotf=0 bt2020=0 cll=0 fall=0 bpc=8")?;
    dpy.set_format(FORMAT_XRGB2101010);
    flip(1)?;
    expect_sink("HDR10 flip", "eotf=2 bt2020=1 cll=800 fall=300 bpc=10")?;
    let (now, _) = dpy.hdr()?;
    if now != hdr10 || !dpy.connector_v2()?.hdr_active {
        return Err(fail(format!("HDR read back {now:?}")));
    }
    flip(2)?;
    expect_sink("a second HDR10 flip", "eotf=2 bt2020=1 cll=800 fall=300 bpc=10")?;
    eprintln!("3 HDR10 with a 10-bit flip: monitor sees PQ, BT.2020, 800/300 cd/m2, 10 bpc OK");

    // 4. HLG, then SDR.
    dpy.set_hdr(&Hdr { eotf: EOTF_HLG, colorimetry: COLORIMETRY_BT2020, ..Default::default() })?;
    flip(1)?;
    expect_sink("HLG flip", "eotf=3 bt2020=1 cll=0 fall=0 bpc=10")?;
    dpy.set_hdr(&Hdr::default())?;
    dpy.set_format(FORMAT_XRGB8888);
    flip(0)?;
    expect_sink("SDR flip", "eotf=0 bt2020=0 cll=0 fall=0 bpc=8")?;
    if dpy.connector_v2()?.hdr_active {
        return Err(fail("HDR still active after SDR".into()));
    }
    eprintln!("4 HLG, then SDR with an 8-bit flip OK");

    // 5. A monitor without HDR.
    dpy.sim_plug(CONN_HDMI21, PLUG_VGA)?;
    let _ = watch.wait(Some(Duration::from_secs(3)))?;
    let (_, vga) = dpy.hdr()?;
    let r = dpy.set_hdr(&hdr10);
    dpy.sim_plug(CONN_HDMI21, PLUG_DESKTOP)?;
    let _ = watch.wait(Some(Duration::from_secs(3)))?;
    if vga.eotfs != 1 << EOTF_SDR || vga.colorimetry != 1 || vga.max_nits != 0 {
        return Err(fail(format!("VGA monitor HDR caps {vga:?}")));
    }
    einval(r, "PQ on a VGA monitor")?;
    eprintln!("5 VGA monitor: SDR only, PQ EINVAL OK");

    // 6. HDR10 on, then swap to a monitor without it: the MODESET falls back.
    let c = dpy.connector_v2()?;
    dpy.set_format(FORMAT_XRGB2101010);
    dpy.set_hdr(&hdr10)?;
    if dpy.modeset_at(scan.buffer(1), i60, c.generation, 0)? != 0 {
        return Err(fail("HDR10 modeset fault".into()));
    }
    expect_sink("HDR10 modeset", "eotf=2 bt2020=1 cll=800 fall=300 bpc=10")?;
    dpy.sim_plug(CONN_HDMI21, PLUG_1080P)?;
    let _ = watch.wait(Some(Duration::from_secs(3)))?;
    let c1 = dpy.connector_v2()?;
    if c1.hdr || dpy.modeset_at(scan.buffer(2), 0, c1.generation, 0)? != 0 {
        return Err(fail("modeset on the 1080p monitor".into()));
    }
    let (now, _) = dpy.hdr()?;
    let seen = sink()?;
    dpy.sim_plug(CONN_HDMI21, PLUG_DESKTOP)?;
    let _ = watch.wait(Some(Duration::from_secs(3)))?;
    if now.eotf != EOTF_SDR || !seen.starts_with("eotf=0 bt2020=0") {
        return Err(fail(format!("after the swap: {now:?}, monitor \"{seen}\"")));
    }
    let c2 = dpy.connector_v2()?;
    dpy.set_format(FORMAT_XRGB8888);
    dpy.modeset_at(scan.buffer(0), 0, c2.generation, 0)?;
    eprintln!("6 swap to a monitor without HDR: MODESET falls back to SDR OK; desktop monitor back");
    Ok(())
}
