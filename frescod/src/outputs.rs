//! Several monitors (display ABI 3.0): which outputs make up the desktop,
//! where each one sits on it, and when a burst of hotplugs has settled
//! enough to act on.
//!
//! Pure logic, no device: the frame loop feeds it what OUTPUTS reports and
//! the clock, and commits what it returns (main.rs).
//!
//! - **Settle.** Docks, KVMs and monitors waking from sleep send bursts of
//!   hotplug edges. Nothing is reconfigured until the hotplugs have been
//!   quiet for [`SETTLE`], so a burst costs one reconfiguration.
//! - **Grace.** A monitor that drops out (a DisplayPort monitor entering
//!   standby, a cable wiggle) keeps its place for [`GRACE`]: its output
//!   stays lit and in the layout, so coming back within that time changes
//!   nothing on the desktop. After it, the output is let go.
//! - **Identity.** Places are remembered by monitor identity, per set of
//!   monitors: when the same set comes back (after a reboot, on other
//!   ports), each monitor gets its old place and mode.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// Hotplugs must be quiet this long before the desktop is reconfigured.
pub const SETTLE: Duration = Duration::from_millis(500);
/// A monitor gone for less than this keeps its place.
pub const GRACE: Duration = Duration::from_secs(5);

/// One output as OUTPUTS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub output: u32,
    /// 0 = no monitor.
    pub identity: u64,
}

/// A monitor's place on the desktop and the mode it runs at, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub output: u32,
    pub identity: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl Viewport {
    pub fn right(&self) -> u32 { self.x + self.width }
    pub fn bottom(&self) -> u32 { self.y + self.height }
}

/// The desktop's size: the bounding box of the viewports.
pub fn desktop_size(vps: &[Viewport]) -> (u32, u32) {
    (
        vps.iter().map(Viewport::right).max().unwrap_or(0),
        vps.iter().map(Viewport::bottom).max().unwrap_or(0),
    )
}

/// A set of monitors, as the key for what is remembered about it.
pub type DisplaySet = Vec<u64>;

pub fn display_set(ids: impl IntoIterator<Item = u64>) -> DisplaySet {
    let mut v: Vec<u64> = ids.into_iter().filter(|&i| i != 0).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Where each monitor of a set sat, and at which mode: by identity,
/// (x, y, width, height, refresh_mhz).
pub type Arrangement = BTreeMap<u64, (u32, u32, u32, u32, u32)>;

/// Lay `monitors` out: (output, identity, width, height, refresh_mhz) each,
/// with sizes already chosen. A remembered arrangement for exactly this set
/// puts each monitor back where it was (when the sizes still match);
/// otherwise they go left to right in output order, top-aligned, touching.
pub fn arrange(monitors: &[(u32, u64, u32, u32, u32)], remembered: Option<&Arrangement>) -> Vec<Viewport> {
    if let Some(a) = remembered {
        let fits = monitors.iter().all(|&(_, id, w, h, _)| {
            a.get(&id).is_some_and(|&(_, _, aw, ah, _)| (aw, ah) == (w, h))
        }) && a.len() == monitors.len();
        if fits {
            return monitors
                .iter()
                .map(|&(output, identity, width, height, refresh_mhz)| {
                    let (x, y, ..) = a[&identity];
                    Viewport { output, identity, x, y, width, height, refresh_mhz }
                })
                .collect();
        }
    }
    let mut x = 0;
    let mut sorted = monitors.to_vec();
    sorted.sort_by_key(|m| m.0);
    sorted
        .iter()
        .map(|&(output, identity, width, height, refresh_mhz)| {
            let v = Viewport { output, identity, x, y: 0, width, height, refresh_mhz };
            x += width;
            v
        })
        .collect()
}

/// What to remember of a layout.
pub fn remember(vps: &[Viewport]) -> (DisplaySet, Arrangement) {
    (
        display_set(vps.iter().map(|v| v.identity)),
        vps.iter().map(|v| (v.identity, (v.x, v.y, v.width, v.height, v.refresh_mhz))).collect(),
    )
}

/// Every viewport must touch another along an edge (no gaps the pointer
/// cannot cross, no islands), and none may overlap.
pub fn validate(vps: &[Viewport]) -> Result<(), String> {
    for (i, a) in vps.iter().enumerate() {
        for b in &vps[i + 1..] {
            if a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom() {
                return Err(format!("outputs {} and {} overlap", a.output, b.output));
            }
        }
    }
    if vps.len() < 2 {
        return Ok(());
    }
    let touches = |a: &Viewport, b: &Viewport| {
        let vertical = (a.right() == b.x || b.right() == a.x) && a.y < b.bottom() && b.y < a.bottom();
        let horizontal = (a.bottom() == b.y || b.bottom() == a.y) && a.x < b.right() && b.x < a.right();
        vertical || horizontal
    };
    // Connected as a graph: grow from the first.
    let mut reached = vec![false; vps.len()];
    reached[0] = true;
    let mut grew = true;
    while grew {
        grew = false;
        for i in 0..vps.len() {
            if reached[i] {
                continue;
            }
            if (0..vps.len()).any(|j| reached[j] && touches(&vps[i], &vps[j])) {
                reached[i] = true;
                grew = true;
            }
        }
    }
    match reached.iter().position(|r| !r) {
        Some(i) => Err(format!("output {} touches no other", vps[i].output)),
        None => Ok(()),
    }
}

/// Hotplug settling and the grace for monitors that drop out.
#[derive(Default)]
pub struct Tracker {
    last_hotplug: Option<Instant>,
    /// Monitors in the layout whose output lost them: when, and where.
    gone: HashMap<u64, (u32, Instant)>,
}

/// What [`Tracker::target`] decided.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// The monitors the desktop should have, by output: connected ones, and
    /// layout members still within their grace (keeping their place).
    pub keep: Vec<Seen>,
    /// When to look again: the earliest grace that runs out.
    pub recheck: Option<Instant>,
}

impl Tracker {
    /// A hotplug happened at `now`.
    pub fn hotplug(&mut self, now: Instant) {
        self.last_hotplug = Some(now);
    }

    /// Hotplugs have been quiet for [`SETTLE`] (or there were none).
    pub fn settled(&self, now: Instant) -> bool {
        self.last_hotplug.is_none_or(|t| now.saturating_duration_since(t) >= SETTLE)
    }

    /// When the current burst settles, if one is pending.
    pub fn settles_at(&self) -> Option<Instant> {
        self.last_hotplug.map(|t| t + SETTLE)
    }

    /// The desktop's monitors now, from what OUTPUTS shows (`seen`) and the
    /// layout (`layout`: output and identity of each viewport).
    pub fn target(&mut self, now: Instant, seen: &[Seen], layout: &[Seen]) -> Target {
        let present: Vec<Seen> = seen.iter().copied().filter(|s| s.identity != 0).collect();
        for s in &present {
            self.gone.remove(&s.identity);
        }
        let mut keep = present.clone();
        let mut recheck: Option<Instant> = None;
        for l in layout {
            if present.iter().any(|s| s.identity == l.identity) {
                continue;
            }
            // Another monitor took its output: no place to keep.
            if present.iter().any(|s| s.output == l.output) {
                self.gone.remove(&l.identity);
                continue;
            }
            let since = self.gone.entry(l.identity).or_insert((l.output, now)).1;
            let until = since + GRACE;
            if now < until {
                keep.push(*l);
                recheck = Some(recheck.map_or(until, |r: Instant| r.min(until)));
            } else {
                self.gone.remove(&l.identity);
            }
        }
        keep.sort_by_key(|s| s.output);
        Target { keep, recheck }
    }
}

/// Copy the `vp` part of a `desktop_w`-wide BGRA desktop frame into `out`
/// (`vp.width * vp.height * 4` bytes). Rows past the frame read as black.
pub fn crop(desktop: &[u8], desktop_w: u32, vp: &Viewport, out: &mut [u8]) {
    let row = vp.width as usize * 4;
    let stride = desktop_w as usize * 4;
    for y in 0..vp.height as usize {
        let dst = &mut out[y * row..(y + 1) * row];
        let start = (vp.y as usize + y) * stride + vp.x as usize * 4;
        match desktop.get(start..start + row) {
            Some(src) => dst.copy_from_slice(src),
            None => dst.fill(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(output: u32, identity: u64, x: u32, y: u32, w: u32, h: u32) -> Viewport {
        Viewport { output, identity, x, y, width: w, height: h, refresh_mhz: 60000 }
    }

    #[test]
    fn default_layout_is_left_to_right_and_touching() {
        let v = arrange(&[(2, 30, 2560, 1440, 60000), (0, 10, 1920, 1080, 120000)], None);
        assert_eq!(v[0], Viewport { output: 0, identity: 10, x: 0, y: 0, width: 1920, height: 1080, refresh_mhz: 120000 });
        assert_eq!((v[1].output, v[1].x, v[1].y), (2, 1920, 0));
        assert_eq!(desktop_size(&v), (1920 + 2560, 1440));
        assert!(validate(&v).is_ok());
    }

    #[test]
    fn a_remembered_set_comes_back_where_it_was_on_any_port() {
        // Monitor 30 above monitor 10, remembered.
        let was = vec![vp(1, 30, 0, 0, 2560, 1440), vp(0, 10, 0, 1440, 1920, 1080)];
        let (set, a) = remember(&was);
        assert_eq!(set, vec![10, 30]);
        // Back on swapped ports.
        let v = arrange(&[(0, 30, 2560, 1440, 60000), (3, 10, 1920, 1080, 60000)], Some(&a));
        assert_eq!((v[0].output, v[0].x, v[0].y), (0, 0, 0));
        assert_eq!((v[1].output, v[1].x, v[1].y), (3, 0, 1440));
        // A different size: not the remembered layout.
        let v = arrange(&[(0, 30, 1920, 1080, 60000), (3, 10, 1920, 1080, 60000)], Some(&a));
        assert_eq!((v[1].x, v[1].y), (1920, 0));
    }

    #[test]
    fn layouts_with_gaps_or_overlaps_are_refused() {
        assert!(validate(&[vp(0, 1, 0, 0, 100, 100), vp(1, 2, 101, 0, 100, 100)]).is_err());
        assert!(validate(&[vp(0, 1, 0, 0, 100, 100), vp(1, 2, 50, 0, 100, 100)]).is_err());
        // Corner-only contact is not an edge.
        assert!(validate(&[vp(0, 1, 0, 0, 100, 100), vp(1, 2, 100, 100, 100, 100)]).is_err());
        assert!(validate(&[vp(0, 1, 0, 0, 100, 100), vp(1, 2, 0, 100, 100, 50)]).is_ok());
    }

    #[test]
    fn a_burst_of_hotplugs_settles_once() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        assert!(t.settled(t0));
        for ms in [0, 100, 250, 400] {
            t.hotplug(t0 + Duration::from_millis(ms));
        }
        assert!(!t.settled(t0 + Duration::from_millis(800)));
        assert!(t.settled(t0 + Duration::from_millis(900)));
        assert_eq!(t.settles_at(), Some(t0 + Duration::from_millis(900)));
    }

    #[test]
    fn a_monitor_that_drops_out_keeps_its_place_for_the_grace() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let a = Seen { output: 0, identity: 10 };
        let b = Seen { output: 1, identity: 20 };
        let layout = [a, b];
        // Monitor 20 sleeps: kept, with a recheck at the end of the grace.
        let r = t.target(t0, &[a, Seen { output: 1, identity: 0 }], &layout);
        assert_eq!(r.keep, vec![a, b]);
        assert_eq!(r.recheck, Some(t0 + GRACE));
        // Back within the grace: nothing to do.
        let r = t.target(t0 + Duration::from_secs(3), &[a, b], &layout);
        assert_eq!(r, Target { keep: vec![a, b], recheck: None });
        // Gone again; this time past the grace: let go.
        let t1 = t0 + Duration::from_secs(10);
        let r = t.target(t1, &[a], &layout);
        assert_eq!(r.keep, vec![a, b]);
        let r = t.target(t1 + GRACE, &[a], &layout);
        assert_eq!(r, Target { keep: vec![a], recheck: None });
    }

    #[test]
    fn another_monitor_on_the_output_ends_the_grace() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let a = Seen { output: 0, identity: 10 };
        let c = Seen { output: 0, identity: 99 };
        let r = t.target(t0, &[c], &[a]);
        assert_eq!(r, Target { keep: vec![c], recheck: None });
    }

    #[test]
    fn crop_takes_the_viewport_rows() {
        // A 4x2 desktop, pixel value = x + 10 y in the first byte.
        let mut d = vec![0u8; 4 * 2 * 4];
        for y in 0..2 {
            for x in 0..4 {
                d[(y * 4 + x) * 4] = (x + 10 * y) as u8;
            }
        }
        let mut out = vec![0u8; 2 * 2 * 4];
        crop(&d, 4, &vp(1, 1, 2, 0, 2, 2), &mut out);
        assert_eq!([out[0], out[4], out[8], out[12]], [2, 3, 12, 13]);
        // Taller than the desktop: black below.
        let mut out = vec![7u8; 2 * 3 * 4];
        crop(&d, 4, &vp(1, 1, 0, 0, 2, 3), &mut out);
        assert!(out[16..].iter().all(|&b| b == 0));
    }
}
