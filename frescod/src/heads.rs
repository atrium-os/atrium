//! The monitors frescod drives (display ABI 3.0): one head per lit output,
//! each with its own scanout buffers, fences, mode, HDR state and place on
//! the desktop (outputs.rs), and the reconfiguration that changes the set
//! of heads as one atomic commit.
//!
//! The desktop is one surface in pixels, the bounding box of the heads'
//! viewports. The renderer draws it once per frame; each head shows its
//! viewport's part. Heads are paced on their own: a head whose last flip
//! has not landed yet skips a frame and catches up with the newest one
//! (`stale`), so a 60 Hz monitor never holds back a 144 Hz one.

use std::collections::HashMap;
use std::io;

use atrium_gpu::amd::{
    ConnectorV2, Display, Gpu, ModeV2, OutputCommit, Scanout, Syncobj, Vm, COMMIT_VRR,
    FAULT_BANDWIDTH, FORMAT_XRGB2101010, FORMAT_XRGB8888,
};

use crate::outputs::{self, Arrangement, DisplaySet, Seen, Viewport};

/// Scanout buffers per head: one on screen, one being filled.
pub const SCANOUT_BUFFERS: usize = 2;

pub struct Head<'a> {
    pub output: u32,
    pub identity: u64,
    pub conn: ConnectorV2,
    pub mode_index: usize,
    pub mode: ModeV2,
    pub vrr: bool,
    pub scan: Scanout<'a>,
    /// copy_done reaches N when this head's frame N is in VRAM; flip_done
    /// when it is on screen (and frame N-1's buffer is free).
    pub copy_done: Syncobj<'a>,
    pub flip_done: Syncobj<'a>,
    /// The last frame committed (flip_done reaches it once it is up).
    pub frame: u64,
    pub on_screen: usize,
    pub hdr_on: bool,
    pub staging: Vec<u8>,
    pub vp: Viewport,
    /// Missed the newest desktop frame (its flip was still pending).
    pub stale: bool,
    /// Its monitor is attached (false while it is gone within the grace).
    pub present: bool,
}

impl Head<'_> {
    /// The last committed frame is on screen.
    pub fn landed(&self) -> bool {
        self.flip_done.query().map_or(true, |v| v >= self.frame)
    }

    pub fn format(&self) -> u32 {
        if self.hdr_on { FORMAT_XRGB2101010 } else { FORMAT_XRGB8888 }
    }
}

/// What the frame loop needs to (re)build heads.
pub struct Ctx<'a> {
    pub gpu: &'a Gpu,
    pub vm: &'a Vm<'a>,
    pub dpy: &'a Display,
}

/// The listed mode matching (width, height, refresh mHz): refresh 0 takes
/// the first at that size, else the closest within 1 Hz.
pub fn find_mode(modes: &[ModeV2], want: Option<(u32, u32, u32)>) -> Option<usize> {
    let (w, h, r) = want?;
    let mut sized = modes.iter().enumerate().filter(|(_, m)| m.width == w && m.height == h);
    if r == 0 {
        return sized.next().map(|(i, _)| i);
    }
    sized
        .filter(|(_, m)| m.refresh_mhz.abs_diff(r) <= 1000)
        .min_by_key(|(_, m)| m.refresh_mhz.abs_diff(r))
        .map(|(i, _)| i)
}

/// The monitor's preferred mode (flagged; the first listed otherwise).
pub fn preferred_mode(modes: &[ModeV2]) -> Option<usize> {
    modes
        .iter()
        .position(|m| m.flags & atrium_gpu::amd::MODE_PREFERRED != 0)
        .or(if modes.is_empty() { None } else { Some(0) })
}

/// The next mode down from `modes[i]` when the bandwidth runs short: the
/// one with the highest pixel clock below it (ties: the larger), so the
/// choice is the same every time.
pub fn next_lower(modes: &[ModeV2], i: usize) -> Option<usize> {
    let pc = modes[i].pixel_clock_khz;
    modes
        .iter()
        .enumerate()
        .filter(|(_, m)| m.pixel_clock_khz < pc)
        .max_by_key(|(_, m)| (m.pixel_clock_khz, m.width * m.height))
        .map(|(j, _)| j)
}

/// One output's part of a reconfiguration.
struct Plan {
    output: u32,
    identity: u64,
    conn: ConnectorV2,
    mode_index: usize,
}

/// Change the heads to show `target` (outputs.rs Tracker), in one atomic
/// commit. A head that keeps its monitor and mode is left alone (no mode
/// set, no flicker); new monitors get their remembered mode for this set
/// of monitors, else `want` (output 0, FRESCOD_MODE or a WM request), else
/// their preferred mode. `request` re-modes output 0. If the outputs
/// together need more bandwidth than the display has, the refused output
/// steps down to its next mode, deterministically, and is left dark only if
/// none fits; every such choice is logged with its reason.
///
/// Returns whether the heads changed. On a failed commit nothing changed.
#[allow(clippy::too_many_arguments)]
pub fn reconfigure<'a>(
    cx: &Ctx<'a>,
    heads: &mut Vec<Head<'a>>,
    target: &[Seen],
    request: Option<(u32, u32, u32)>,
    want0: Option<(u32, u32, u32)>,
    remembered: &HashMap<DisplaySet, Arrangement>,
    can_hdr: bool,
) -> io::Result<bool> {
    let set = outputs::display_set(target.iter().map(|s| s.identity));
    let memo = remembered.get(&set);
    let mut plans: Vec<Plan> = Vec::new();
    for s in target {
        let held = heads.iter().find(|h| h.output == s.output && h.identity == s.identity);
        if let Some(h) = held {
            if !h.present {
                continue; // gone within its grace: leave it be
            }
        }
        let (conn, _) = cx.dpy.connector_at(s.output)?;
        if !conn.connected || conn.modes.is_empty() {
            if let Some(h) = held {
                // Kept in place without its monitor.
                let _ = h;
                continue;
            }
            continue;
        }
        let cur = held.map(|h| (h.mode.width, h.mode.height, h.mode.refresh_mhz));
        let pick = if s.output == 0 && request.is_some() {
            find_mode(&conn.modes, request)
        } else {
            None
        }
        .or_else(|| find_mode(&conn.modes, cur))
        .or_else(|| find_mode(&conn.modes, memo.and_then(|a| a.get(&s.identity)).map(|&(_, _, w, h, r)| (w, h, r))))
        .or_else(|| if s.output == 0 { find_mode(&conn.modes, want0) } else { None })
        .or_else(|| preferred_mode(&conn.modes));
        let Some(i) = pick else { continue };
        if let Some(h) = held {
            if conn.current == Some(i) {
                continue; // lit at that mode already
            }
            let _ = h;
        }
        plans.push(Plan { output: s.output, identity: s.identity, conn, mode_index: i });
    }
    let offs: Vec<u32> = heads
        .iter()
        .filter(|h| !target.iter().any(|s| s.output == h.output && s.identity == h.identity))
        // A new monitor on a head's output is a mode set, not an off.
        .filter(|h| !plans.iter().any(|p| p.output == h.output))
        .map(|h| h.output)
        .collect();
    if plans.is_empty() && offs.is_empty() {
        return Ok(false);
    }

    // Buffers for every output that gets a mode set: a head's own when
    // they are big enough for the new mode (VRAM is precious: the device's
    // allocator does not take freed VRAM back yet), else fresh ones.
    let mut fresh: Vec<Buf<'a>> = Vec::new();
    let alloc = |p: &Plan, heads: &[Head<'a>], prev: Option<Buf<'a>>| -> io::Result<Buf<'a>> {
        let m = p.conn.modes[p.mode_index];
        let need = u64::from(m.width) * u64::from(m.height) * 4;
        if heads.iter().any(|h| h.output == p.output && h.scan.size() >= need) {
            return Ok(Buf::Reuse(p.output));
        }
        match prev {
            Some(Buf::Fresh(s)) if s.size() >= need => Ok(Buf::Fresh(s)),
            _ => Ok(Buf::Fresh(Scanout::with_buffers(cx.vm, need, SCANOUT_BUFFERS)?)),
        }
    };
    for p in &plans {
        fresh.push(alloc(p, heads, None)?);
    }
    // Check the whole configuration first; step down on BANDWIDTH.
    for _ in 0..32 {
        let parts = parts(heads, &plans, &fresh, &offs);
        let r = cx.dpy.commit_multi(&parts, true)?;
        let Some(i) = r.failed else { break };
        let pi = i.checked_sub(offs.len()).filter(|&k| k < plans.len());
        let Some(k) = pi else {
            eprintln!("frescod: turning output {} off refused (fault {}); display unchanged", offs[i], r.reason);
            return Ok(false);
        };
        let p = &plans[k];
        let m = p.conn.modes[p.mode_index];
        let lower = if r.reason == FAULT_BANDWIDTH { next_lower(&p.conn.modes, p.mode_index) } else { None };
        match lower {
            Some(j) => {
                let n = p.conn.modes[j];
                eprintln!(
                    "frescod: output {}: {}x{}@{} mHz needs more display bandwidth than is left; {}x{}@{} mHz instead",
                    p.output, m.width, m.height, m.refresh_mhz, n.width, n.height, n.refresh_mhz
                );
                plans[k].mode_index = j;
                let prev = std::mem::replace(&mut fresh[k], Buf::Reuse(u32::MAX));
                fresh[k] = alloc(&plans[k], heads, Some(prev))?;
            }
            None => {
                eprintln!(
                    "frescod: output {} stays dark: {}",
                    p.output,
                    if r.reason == FAULT_BANDWIDTH {
                        "no mode fits the display bandwidth left".to_string()
                    } else {
                        format!("the display refused {}x{}@{} mHz (fault {})", m.width, m.height, m.refresh_mhz, r.reason)
                    }
                );
                plans.remove(k);
                fresh.remove(k);
            }
        }
    }
    if plans.is_empty() && offs.is_empty() {
        return Ok(false);
    }
    let r = cx.dpy.commit_multi(&parts(heads, &plans, &fresh, &offs), false)?;
    if let Some(i) = r.failed {
        eprintln!("frescod: display reconfiguration refused at part {i} (fault {}); display unchanged", r.reason);
        return Ok(false);
    }

    // The commit went through: the heads follow it.
    heads.retain(|h| !offs.contains(&h.output));
    for o in &offs {
        eprintln!("frescod: output {o} off (its monitor left)");
    }
    for (p, buf) in plans.into_iter().zip(fresh) {
        let m = p.conn.modes[p.mode_index];
        let vrr = crate::use_vrr(&p.conn, &m);
        let pos = heads.iter().position(|h| h.output == p.output);
        let was_hdr = pos.map(|k| heads[k].hdr_on);
        let hdr_on = crate::apply_hdr(cx.dpy, p.output, &p.conn, can_hdr, was_hdr);
        match pos {
            Some(k) => {
                let h = &mut heads[k];
                eprintln!(
                    "frescod: mode {}x{} @ {} mHz -> {}x{} @ {} mHz{}{}",
                    h.mode.width, h.mode.height, h.mode.refresh_mhz, m.width, m.height, m.refresh_mhz,
                    if vrr { ", variable refresh" } else { "" },
                    if h.identity != p.identity { " (monitor changed)" } else { "" }
                );
                h.identity = p.identity;
                h.mode = m;
                h.mode_index = p.mode_index;
                h.vrr = vrr;
                h.conn = p.conn;
                if let Buf::Fresh(scan) = buf {
                    h.scan = scan;
                    h.on_screen = 0;
                }
                h.staging = vec![0u8; (m.width * m.height * 4) as usize];
                h.hdr_on = hdr_on;
                h.present = true;
            }
            None => {
                let Buf::Fresh(scan) = buf else { continue };
                eprintln!(
                    "frescod: output {} connector type {} {}x{} @ {} mHz ({} of {} modes), {} scanout buffers",
                    p.output, p.conn.connector_type, m.width, m.height, m.refresh_mhz,
                    p.mode_index + 1, p.conn.modes.len(), SCANOUT_BUFFERS
                );
                if vrr {
                    let (lo, hi) = p.conn.vrr.unwrap_or_default();
                    eprintln!("frescod: variable refresh on ({lo}-{hi} Hz): a frame goes up as soon as it is ready");
                }
                heads.push(Head {
                    output: p.output,
                    identity: p.identity,
                    mode_index: p.mode_index,
                    mode: m,
                    vrr,
                    scan,
                    copy_done: cx.gpu.create_syncobj()?,
                    flip_done: cx.gpu.create_syncobj()?,
                    frame: 0,
                    on_screen: 0,
                    hdr_on,
                    staging: vec![0u8; (m.width * m.height * 4) as usize],
                    vp: Viewport { output: p.output, identity: p.identity, x: 0, y: 0, width: m.width, height: m.height, refresh_mhz: m.refresh_mhz },
                    stale: true,
                    present: true,
                    conn: p.conn,
                });
            }
        }
    }
    heads.sort_by_key(|h| h.output);
    arrange(heads, remembered);
    Ok(true)
}

/// Place the heads: the remembered arrangement of this set of monitors,
/// else left to right.
pub fn arrange(heads: &mut [Head<'_>], remembered: &HashMap<DisplaySet, Arrangement>) {
    let mons: Vec<(u32, u64, u32, u32, u32)> =
        heads.iter().map(|h| (h.output, h.identity, h.mode.width, h.mode.height, h.mode.refresh_mhz)).collect();
    let set = outputs::display_set(mons.iter().map(|m| m.1));
    let mut vps = outputs::arrange(&mons, remembered.get(&set));
    if outputs::validate(&vps).is_err() {
        vps = outputs::arrange(&mons, None);
    }
    for h in heads.iter_mut() {
        if let Some(v) = vps.iter().find(|v| v.output == h.output) {
            h.vp = *v;
        }
    }
}

/// Where an output's mode set takes its buffers from.
enum Buf<'a> {
    Fresh(Scanout<'a>),
    /// The head on this output keeps its own (big enough for the mode).
    Reuse(u32),
}

fn parts<'b, 'a>(heads: &'b [Head<'a>], plans: &[Plan], fresh: &'b [Buf<'a>], offs: &[u32]) -> Vec<OutputCommit<'b, 'a>> {
    let mut v: Vec<OutputCommit<'b, 'a>> = offs.iter().map(|&o| OutputCommit::off(o)).collect();
    for (p, b) in plans.iter().zip(fresh) {
        let bo = match b {
            Buf::Fresh(s) => s.buffer(0),
            Buf::Reuse(o) => {
                let h = heads.iter().find(|h| h.output == *o).expect("reused head");
                h.scan.buffer(h.on_screen)
            }
        };
        let mut c = OutputCommit::modeset(p.output, bo, p.mode_index as u32, p.conn.generation);
        if crate::use_vrr(&p.conn, &p.conn.modes[p.mode_index]) {
            c.flags |= COMMIT_VRR;
        }
        v.push(c);
    }
    v
}
