//! ★ A FEED THE NAVIGATOR DRIVES ITSELF (fresco-video P3, with the
//! media-player thread, 2026-11-06): a track whose segments the Navigator
//! knows (a segment index — byte ranges of one file, or one URL per
//! segment, with alternates), fetched through its fetcher (Nuntius,
//! `Stream`) and appended to the player through the MSE front end
//! ([`crate::mse::Media`]), which keeps one `Append` in flight and maps
//! `TOO_BIG`. Under a page's own MSE the page script chooses segments; this
//! is for what the Navigator fetches itself.
//!
//! What to fetch after a seek — the page's, or the player's `Seeking` from
//! its native controls:
//! - every fetch not for the segments around the new time is CANCELLED at
//!   once (Nuntius `Cancel` shuts its socket);
//! - the segment that covers the target is fetched FIRST (priority 0), then
//!   the ones after it up to [`Feed::ahead`] seconds;
//! - buffered ranges more than [`Feed::behind`] seconds before the playhead
//!   are `Remove`d, so the player's 150 MB budget holds the playhead's
//!   neighbourhood; a `TOO_BIG` evicts the same way and appending resumes.
//!
//! The fetcher retries, resumes and times out (nuntius.rs); a segment that
//! still fails moves to its next alternate URL; one with none left is
//! reported (`NETWORK`, as `MEDIA_ERR_NETWORK`) — and a later play or seek
//! starts afresh, so a slow stream never needs a refresh.

use crate::mse::Media;
use navigator_fetch::nuntius::{Deliver, Event, Limits, Open, Request, Retry};
use std::collections::{BTreeMap, BTreeSet};

/// One segment: where its bytes are, and the media time it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// The URL and its alternates (mirrors, the same segment elsewhere).
    pub urls: Vec<String>,
    /// `(first byte, length)` within the URL's resource, or the whole of it.
    pub range: Option<(u64, u64)>,
    pub start: f64,
    pub end: f64,
}

/// What the Navigator's fetcher takes: a Nuntius instance, or a test's.
pub trait Fetch { fn request(&self, r: Request); }
impl Fetch for navigator_fetch::nuntius::Nuntius { fn request(&self, r: Request) { navigator_fetch::nuntius::Nuntius::request(self, r) } }

struct Pending { seg: usize, url: usize, bytes: Vec<u8> }

pub struct Feed {
    pub track: u8,
    init: Segment,
    segs: Vec<Segment>,
    /// Seconds to keep fetched ahead of the playhead, and kept behind it.
    pub ahead: f64,
    pub behind: f64,
    /// Request ids in flight → their segment (`usize::MAX`: the init segment).
    inflight: BTreeMap<u32, Pending>,
    /// Segments fetched and waiting to be appended, and those appended.
    ready: BTreeMap<usize, Vec<u8>>,
    appended: BTreeSet<usize>,
    /// Segments whose every URL failed (until a play or seek).
    failed: BTreeSet<usize>,
    /// Alternates already tried per segment.
    tried: BTreeMap<usize, usize>,
    init_done: bool,
    next_id: u32,
    pub playhead: f64,
    /// Requests opened, cancelled and failed over (to check bounds).
    pub opened: u64,
    pub cancelled: u64,
    pub failovers: u64,
    pub network_errors: u64,
    pub limits: Limits,
    pub retry: Retry,
    /// The order segments were appended in (checks and diagnostics).
    pub append_order: Vec<usize>,
}

const INIT: usize = usize::MAX;

impl Feed {
    pub fn new(track: u8, init: Segment, segs: Vec<Segment>) -> Feed {
        Feed { track, init, segs, ahead: 20.0, behind: 30.0, inflight: BTreeMap::new(), ready: BTreeMap::new(), appended: BTreeSet::new(),
               failed: BTreeSet::new(), tried: BTreeMap::new(), init_done: false, next_id: 1, playhead: 0.0,
               opened: 0, cancelled: 0, failovers: 0, network_errors: 0,
               limits: Limits { max_bytes: 64 << 20, connect_ms: 4_000, idle_ms: 4_000, total_ms: 120_000 },
               retry: Retry { max_attempts: 4, backoff_ms: 200 }, append_order: vec![] }
    }

    /// The segment covering `t` (the last one for a time past the end).
    pub fn covering(&self, t: f64) -> usize {
        self.segs.iter().position(|s| t < s.end).unwrap_or(self.segs.len().saturating_sub(1))
    }

    fn open(&mut self, seg: usize, priority: u8, net: &dyn Fetch) {
        let s = if seg == INIT { &self.init } else { &self.segs[seg] };
        let url = *self.tried.get(&seg).unwrap_or(&0);
        let Some(u) = s.urls.get(url) else { return };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let mut o = Open::get(id, u);
        o.range = s.range.map(|(a, n)| (a, Some(n)));
        o.deliver = Deliver::Stream;
        o.limits = self.limits;
        o.retry = self.retry;
        o.priority = priority;
        // Credit for this segment's bytes (flow control is per instance).
        net.request(Request::Budget { bytes: s.range.map(|r| r.1).unwrap_or(8 << 20) });
        net.request(Request::Open(o));
        self.inflight.insert(id, Pending { seg, url, bytes: vec![] });
        self.opened += 1;
    }

    fn wanted(&self) -> Vec<usize> {
        let first = self.covering(self.playhead);
        (first..self.segs.len()).take_while(|&i| i == first || self.segs[i].start < self.playhead + self.ahead).collect()
    }

    /// Fetch what the playhead needs and is not in hand or on the way.
    fn refill(&mut self, net: &dyn Fetch) {
        if !self.init_done && !self.inflight.values().any(|p| p.seg == INIT) { self.open(INIT, 0, net) }
        let busy: BTreeSet<usize> = self.inflight.values().map(|p| p.seg).collect();
        for (n, i) in self.wanted().into_iter().enumerate() {
            if self.appended.contains(&i) || self.ready.contains_key(&i) || busy.contains(&i) || self.failed.contains(&i) { continue }
            self.open(i, if n == 0 { 0 } else { 1 }, net);
        }
    }

    /// Start (or restart after an error): fetch from the playhead.
    pub fn start(&mut self, net: &dyn Fetch) {
        self.failed.clear();
        self.tried.clear();
        self.refill(net);
    }

    /// ★ A SEEK, by the page or from the player's controls (`Seeking`):
    /// cancel what the new time does not need, fetch its segment first.
    pub fn seek(&mut self, t: f64, net: &dyn Fetch, media: &mut Media) {
        self.playhead = t.max(0.0);
        let want: BTreeSet<usize> = self.wanted().into_iter().collect();
        let stale: Vec<u32> = self.inflight.iter().filter(|(_, p)| p.seg != INIT && !want.contains(&p.seg)).map(|(id, _)| *id).collect();
        for id in stale { net.request(Request::Cancel { id }); self.inflight.remove(&id); self.cancelled += 1; }
        self.ready.retain(|i, _| want.contains(i));
        // A failed stream starts afresh on a seek: nothing wedges.
        self.failed.clear();
        self.tried.clear();
        // The covering segment goes first: ahead of anything still queued.
        let first = self.covering(self.playhead);
        if !self.appended.contains(&first) && !self.inflight.values().any(|p| p.seg == first) { self.open(first, 0, net) }
        self.refill(net);
        self.evict(media);
        self.pump(media);
    }

    /// The playhead moved (`Time`): refill ahead, evict far behind.
    pub fn time(&mut self, t: f64, net: &dyn Fetch, media: &mut Media) {
        self.playhead = t;
        self.refill(net);
        self.evict(media);
        self.pump(media);
    }

    /// `Remove` what lies more than `behind` seconds before the playhead.
    fn evict(&mut self, media: &mut Media) {
        if media.updating(self.track) { return }
        let cut = self.playhead - self.behind;
        let old: Vec<usize> = self.appended.iter().copied().filter(|&i| self.segs[i].end <= cut).collect();
        if old.is_empty() { return }
        let end = old.iter().map(|&i| self.segs[i].end).fold(0.0f64, f64::max);
        if media.remove(self.track, 0.0, end).is_ok() { for i in old { self.appended.remove(&i); } }
    }

    /// Append the next segment if the track is free: the init segment first,
    /// then whichever ready segment is nearest the playhead.
    pub fn pump(&mut self, media: &mut Media) {
        if media.updating(self.track) { return }
        if !self.init_done {
            if let Some(b) = self.ready.remove(&INIT) {
                if media.append_buffer(self.track, &b).is_ok() { self.init_done = true; self.append_order.push(INIT) }
                else { self.ready.insert(INIT, b); }
            }
            return;
        }
        let first = self.covering(self.playhead);
        let next = self.ready.keys().copied().filter(|&i| i >= first).min().or_else(|| self.ready.keys().copied().next());
        let Some(i) = next else { return };
        let b = self.ready.remove(&i).unwrap();
        match media.append_buffer(self.track, &b) {
            Ok(()) => { self.appended.insert(i); self.append_order.push(i) }
            // Full: make room behind the playhead and try again later.
            Err(crate::mse::DomError::QuotaExceeded) => { self.ready.insert(i, b); self.behind = (self.behind / 2.0).max(2.0); self.evict(media) }
            Err(_) => { self.ready.insert(i, b); }
        }
    }

    /// A fetcher event (for this feed's requests; others are ignored).
    pub fn on_fetch(&mut self, ev: Event, net: &dyn Fetch, media: &mut Media) {
        let id = ev.id();
        if !self.inflight.contains_key(&id) { return }
        match ev {
            Event::Data { bytes, .. } => self.inflight.get_mut(&id).unwrap().bytes.extend(bytes),
            Event::Done { .. } => {
                let p = self.inflight.remove(&id).unwrap();
                self.ready.insert(p.seg, p.bytes);
                self.pump(media);
                self.refill(net);
            }
            Event::Error { .. } => {
                let p = self.inflight.remove(&id).unwrap();
                let urls = if p.seg == INIT { self.init.urls.len() } else { self.segs[p.seg].urls.len() };
                if p.url + 1 < urls {
                    // Failover: the next URL for this segment, from its start.
                    self.tried.insert(p.seg, p.url + 1);
                    self.failovers += 1;
                    self.open(p.seg, 0, net);
                } else {
                    self.failed.insert(p.seg);
                    self.network_errors += 1;
                    media.network_error();
                }
            }
            _ => {}
        }
    }
}
