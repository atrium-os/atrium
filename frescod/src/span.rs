//! A fullscreen window spanning several monitors (fresco-span.md §5, §6):
//! the members' timing locked into one sync group (display ABI 3.1), their
//! flips committed together, the bezel strips moved off screen, and the
//! window's owner told its tiles (`EV_WINDOW_SPAN`).
//!
//! Forum asks (`OP_WM_SPAN_GROUP`, already checked and answered by the
//! frontend); the frame loop applies it here. While a span is up:
//! - the members (and every monitor past the group) sit moved apart by the
//!   bezel gaps, so the desktop is that much larger and the window covers
//!   the strips; the frontend lays out new spans from where they were;
//! - every member runs one mode, VRR on all or none (frescod turns it off
//!   on all if one lacks it) and HDR on all or none;
//! - the members present together, one `COMMIT_MULTI` per frame;
//! - a member whose monitor drops out stays in the span for
//!   [`crate::outputs::SPAN_GRACE`] with its tile `present: false` and its flip lock off,
//!   and one that drops [`FLAP_DROPS`] times within [`FLAP_WINDOW`] is held
//!   out until it has stayed up for [`FLAP_REJOIN`].
//!
//! Direct scanout of the game's surface (§5) is not done yet: the members
//! show the composed desktop, cropped per monitor, as every multi-monitor
//! desktop does today.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use atrium_gpu::amd::Display;
use fresco_protocol::{SpanTile, WmSpanGroupPayload};
use fresco_scene_server::span::{self, Place, SpanLayout};
use fresco_scene_server::window::DisplayEvent;

use crate::heads::Head;

/// Drops within [`FLAP_WINDOW`] that make a member flapping.
pub const FLAP_DROPS: usize = 3;
pub const FLAP_WINDOW: Duration = Duration::from_secs(60);
/// How long a flapping member must stay up before it rejoins.
pub const FLAP_REJOIN: Duration = Duration::from_secs(10);
/// Monitors whose variable refresh a span turned off (one member lacks it:
/// the group runs fixed refresh).
pub static NO_VRR: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

/// The display sync group a span uses.
const GROUP: u32 = 1;

pub struct Span {
    pub window_id: u32,
    /// Identities, in the order Forum named them (the tiles' order).
    pub members: Vec<u64>,
    gaps_x: Vec<u32>,
    gaps_y: Vec<u32>,
    layout: SpanLayout,
    /// Where each moved monitor was before the span: (identity, x, y).
    orig: Vec<(u64, u32, u32)>,
    /// HDR the span turned off, to turn back on when it ends.
    pub hdr_off: Vec<u64>,
    /// Whether the span turned VRR off on its members.
    pub vrr_off: bool,
    /// Per member: its monitor is attached (as OUTPUTS last showed).
    attached: HashMap<u64, bool>,
    /// Per member: when it dropped, the last [`FLAP_WINDOW`].
    drops: HashMap<u64, VecDeque<Instant>>,
    /// Held-out members, and since when each has been back (None: gone).
    held: HashMap<u64, Option<Instant>>,
    /// The tiles as the window was last told.
    told: Vec<bool>,
}

/// The monitors as a span lays them out: plain heads (mirrors have no
/// place of their own), at `orig` where a span moved them.
fn places(heads: &[Head<'_>], orig: &[(u64, u32, u32)]) -> Vec<Place> {
    heads
        .iter()
        .filter(|h| h.mirror_of.is_none())
        .map(|h| {
            let (x, y) = orig.iter().find(|o| o.0 == h.identity).map_or((h.vp.x, h.vp.y), |o| (o.1, o.2));
            Place { identity: h.identity, x: x as i32, y: y as i32, w: h.vp.width, h: h.vp.height }
        })
        .collect()
}

impl Span {
    /// Lay out `req` over `heads` (where they sat before any span) and move
    /// the monitors for its bezels. Nothing is committed yet ([`Span::lock`]).
    pub fn new(req: &WmSpanGroupPayload, heads: &mut [Head<'_>]) -> Result<Span, String> {
        if heads.iter().any(|h| h.mirror_of.is_some_and(|_| req.members.contains(&h.identity))) {
            return Err("a mirror can't be in a span".into());
        }
        let layout = span::layout(&places(heads, &[]), &req.members, &req.gaps_x, &req.gaps_y)?;
        let first = heads.iter().find(|h| h.identity == req.members[0]).map(|h| (h.mode.width, h.mode.height, h.mode.refresh_mhz));
        if req.members.iter().any(|id| heads.iter().find(|h| h.identity == *id).map(|h| (h.mode.width, h.mode.height, h.mode.refresh_mhz)) != first) {
            return Err("the members run different modes".into());
        }
        let mut s = Span {
            window_id: req.window_id,
            members: req.members.clone(),
            gaps_x: req.gaps_x.clone(),
            gaps_y: req.gaps_y.clone(),
            layout,
            orig: Vec::new(),
            hdr_off: Vec::new(),
            vrr_off: false,
            attached: req.members.iter().map(|&id| (id, true)).collect(),
            drops: HashMap::new(),
            held: HashMap::new(),
            told: Vec::new(),
        };
        s.attached = req.members.iter().map(|&id| (id, heads.iter().any(|h| h.identity == id && h.present))).collect();
        s.place(heads);
        Ok(s)
    }

    /// Lay the span out again over the heads as they sit now (after a
    /// reconfiguration put them back in their places), keeping its state.
    pub fn relayout(&mut self, heads: &mut [Head<'_>]) -> Result<(), String> {
        let r = self.request();
        let fresh = Span::new(&r, heads)?;
        self.layout = fresh.layout;
        self.orig = fresh.orig;
        Ok(())
    }

    /// HDR on all members or none: if they differ, off on all of them (and
    /// noted, to go back on when the span ends).
    pub fn unify_hdr(&mut self, dpy: &Display, heads: &mut [Head<'_>]) {
        let on: Vec<bool> = heads.iter().filter(|h| self.is_member(h.identity)).map(|h| h.hdr_on).collect();
        if on.iter().all(|&b| b) || on.iter().all(|&b| !b) {
            return;
        }
        for h in heads.iter_mut().filter(|h| h.hdr_on && self.members.contains(&h.identity)) {
            eprintln!("frescod: span: output {} to SDR (not every member takes HDR)", h.output);
            h.hdr_on = crate::apply_hdr(dpy, h.output, &h.conn, false, Some(true));
            if !self.hdr_off.contains(&h.identity) {
                self.hdr_off.push(h.identity);
            }
        }
    }

    /// Variable refresh on all members or none.
    pub fn vrr_mixed(&self, heads: &[Head<'_>]) -> bool {
        let on: Vec<bool> = heads.iter().filter(|h| self.is_member(h.identity)).map(|h| h.vrr).collect();
        !(on.iter().all(|&b| b) || on.iter().all(|&b| !b))
    }

    /// Move the monitors as the layout says, keeping where they were.
    fn place(&mut self, heads: &mut [Head<'_>]) {
        for &(id, x, y) in &self.layout.moves {
            if let Some(h) = heads.iter_mut().find(|h| h.identity == id) {
                if !self.orig.iter().any(|o| o.0 == id) {
                    self.orig.push((id, h.vp.x, h.vp.y));
                }
                h.vp.x = x.max(0) as u32;
                h.vp.y = y.max(0) as u32;
            }
        }
        crate::heads::place_mirrors(heads);
    }

    /// Put the monitors back where they were.
    pub fn unplace(&self, heads: &mut [Head<'_>]) {
        for &(id, x, y) in &self.orig {
            if let Some(h) = heads.iter_mut().find(|h| h.identity == id) {
                h.vp.x = x;
                h.vp.y = y;
            }
        }
        crate::heads::place_mirrors(heads);
    }

    /// Where the moved monitors were (identity, x, y), for the frontend.
    pub fn moved(&self) -> Vec<(u64, i32, i32)> {
        self.orig.iter().map(|&(id, x, y)| (id, x as i32, y as i32)).collect()
    }

    /// A member is in this frame's flips: attached and not held out.
    pub fn active(&self, id: u64) -> bool {
        self.attached.get(&id).copied().unwrap_or(false) && !self.held.contains_key(&id)
    }

    pub fn is_member(&self, id: u64) -> bool {
        self.members.contains(&id)
    }

    /// The heads (indices) presenting together this frame.
    pub fn active_heads(&self, heads: &[Head<'_>]) -> Vec<usize> {
        (0..heads.len()).filter(|&k| self.is_member(heads[k].identity) && self.active(heads[k].identity) && heads[k].present).collect()
    }

    /// Lock the members' timing, with the active ones flip-locked (the
    /// others flip without them). After any mode set on a member too: a
    /// mode set takes it out of the group.
    pub fn lock(&self, dpy: &Display, heads: &[Head<'_>]) -> Result<(), String> {
        let (mut members, mut locked) = (0u32, 0u32);
        for id in &self.members {
            let h = heads.iter().find(|h| h.identity == *id).ok_or_else(|| format!("monitor {id:016x} is gone"))?;
            members |= 1 << h.output;
            if self.active(*id) && h.present {
                locked |= 1 << h.output;
            }
        }
        let r = dpy.commit_sync_group(GROUP, members, locked).map_err(|e| e.to_string())?;
        match r.failed {
            None => Ok(()),
            Some(n) => Err(format!("the display refused the sync group at output {n} (fault {})", r.reason)),
        }
    }

    /// Dissolve the members' sync group.
    pub fn unlock(dpy: &Display) {
        if let Err(e) = dpy.commit_sync_group(GROUP, 0, 0) {
            eprintln!("frescod: span: dissolving the sync group: {e}");
        }
    }

    /// Tell the window's owner its tiles, if they changed.
    pub fn tell(&mut self, ev: &mpsc::Sender<DisplayEvent>) {
        let present: Vec<bool> = self.members.iter().map(|&id| self.active(id)).collect();
        if present == self.told {
            return;
        }
        self.told = present.clone();
        let tiles = self.layout.tiles.iter().zip(&present).map(|(&rect, &present)| SpanTile { rect, present }).collect();
        let _ = ev.send(DisplayEvent::WindowSpan { window_id: self.window_id, tiles, hidden: self.layout.hidden.clone() });
    }

    /// Tell the window's owner the span ended.
    pub fn tell_ended(&self, ev: &mpsc::Sender<DisplayEvent>) {
        let _ = ev.send(DisplayEvent::WindowSpan { window_id: self.window_id, tiles: Vec::new(), hidden: Vec::new() });
    }

    /// What OUTPUTS shows now (identities attached): note drops and
    /// returns, hold out a flapping member, let a held one back once it has
    /// stayed up. Returns whether the active set changed, and when to look
    /// again.
    pub fn attach(&mut self, now: Instant, attached: &[u64]) -> (bool, Option<Instant>) {
        let before: Vec<bool> = self.members.iter().map(|&id| self.active(id)).collect();
        for &id in &self.members {
            let there = attached.contains(&id);
            let was = self.attached.insert(id, there).unwrap_or(false);
            if was && !there {
                let d = self.drops.entry(id).or_default();
                d.push_back(now);
                while d.front().is_some_and(|&t| now.saturating_duration_since(t) > FLAP_WINDOW) {
                    d.pop_front();
                }
                if d.len() >= FLAP_DROPS && !self.held.contains_key(&id) {
                    eprintln!(
                        "frescod: span: monitor {id:016x} dropped {} times in {} s; held out until it stays up {} s",
                        d.len(), FLAP_WINDOW.as_secs(), FLAP_REJOIN.as_secs()
                    );
                    self.held.insert(id, None);
                }
            }
            if let Some(up) = self.held.get_mut(&id) {
                *up = match (there, *up) {
                    (true, None) => Some(now),
                    (true, s) => s,
                    (false, _) => None,
                };
            }
        }
        let mut again: Option<Instant> = None;
        self.held.retain(|id, up| match up {
            Some(t) if now.saturating_duration_since(*t) >= FLAP_REJOIN => {
                eprintln!("frescod: span: monitor {id:016x} stayed up {} s; it rejoins", FLAP_REJOIN.as_secs());
                false
            }
            Some(t) => {
                let at = *t + FLAP_REJOIN;
                again = Some(again.map_or(at, |a: Instant| a.min(at)));
                true
            }
            None => true,
        });
        let after: Vec<bool> = self.members.iter().map(|&id| self.active(id)).collect();
        (before != after, again)
    }

    /// The members as Forum asked, for a span laid out again (after the
    /// heads changed under it).
    pub fn request(&self) -> WmSpanGroupPayload {
        WmSpanGroupPayload { window_id: self.window_id, members: self.members.clone(), gaps_x: self.gaps_x.clone(), gaps_y: self.gaps_y.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(members: &[u64]) -> Span {
        Span {
            window_id: 5,
            members: members.to_vec(),
            gaps_x: Vec::new(),
            gaps_y: Vec::new(),
            layout: SpanLayout { rect: [0; 4], moves: Vec::new(), tiles: vec![[0; 4]; members.len()], hidden: Vec::new() },
            orig: Vec::new(),
            hdr_off: Vec::new(),
            vrr_off: false,
            attached: members.iter().map(|&id| (id, true)).collect(),
            drops: HashMap::new(),
            held: HashMap::new(),
            told: Vec::new(),
        }
    }

    #[test]
    fn a_dropout_is_a_dark_tile_and_a_return_lights_it() {
        let mut s = bare(&[1, 2]);
        let t0 = Instant::now();
        assert_eq!(s.attach(t0, &[1]), (true, None));
        assert!(!s.active(2) && s.active(1));
        assert_eq!(s.attach(t0 + Duration::from_secs(5), &[1, 2]), (true, None));
        assert!(s.active(2));
    }

    #[test]
    fn a_flapping_member_is_held_out_until_it_stays_up() {
        let mut s = bare(&[1, 2]);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        for k in 0..2 {
            s.attach(at(k * 10), &[1]);
            s.attach(at(k * 10 + 1), &[1, 2]);
            assert!(s.active(2), "drop {} is a dropout", k + 1);
        }
        s.attach(at(20), &[1]);
        let (changed, again) = s.attach(at(21), &[1, 2]);
        assert!(!changed && !s.active(2), "the third drop in 60 s holds it out");
        assert_eq!(again, Some(at(31)));
        // Flapping again resets the wait.
        s.attach(at(25), &[1]);
        let (_, again) = s.attach(at(26), &[1, 2]);
        assert_eq!(again, Some(at(36)));
        assert!(!s.attach(at(35), &[1, 2]).0);
        assert_eq!(s.attach(at(36), &[1, 2]), (true, None));
        assert!(s.active(2), "up 10 s: back");
    }

    #[test]
    fn drops_far_apart_are_not_flapping() {
        let mut s = bare(&[1, 2]);
        let t0 = Instant::now();
        for k in 0..5u64 {
            s.attach(t0 + Duration::from_secs(k * 40), &[1]);
            s.attach(t0 + Duration::from_secs(k * 40 + 1), &[1, 2]);
            assert!(s.active(2));
        }
    }
}
