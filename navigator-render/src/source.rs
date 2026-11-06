//! ★ THE CONTENT SOURCE (pergola.md §10.4; backend §4.8 step 3, phase 2):
//! the host-agnostic contract between a document and the view that scrolls
//! it. `ScrollView` owns the viewport; a source owns content — where bands
//! are cut (at checkpoints), what each band's scene is, and how much of it
//! is resident. The source never scrolls; the view never lays out.
//!
//! | direction | message |
//! |---|---|
//! | view → source | `want { range, zoom, prefetch }` |
//! | source → view | `extent { height, settled }`, `band_ready`, `band_updated`, `band_evicted` |
//!
//! Scenes do not go through the view: the source writes each band as ONE
//! scene group, and the leaves a band paints outside itself (that the whole
//! layout paints over the other bands they reach) as one OVERLAY group
//! after the bands, through a `SceneSink` — in process a Pergola surface,
//! out of process the worker's embedded surface (fresco-scene-groups.md §6).
//! The view moves the content group the groups sit in; it never copies
//! scene data. In process this is the Rust trait below; out of process the
//! same messages travel over the doc-viewer role's channel (limen.md §2.1).
//!
//! Coordinates are document space, vertical, in 1/64 px, of the layout at
//! the wanted zoom (§4.9: a zoomed document is laid out at the window's
//! size divided by the zoom; the view scales the content group by it).

use crate::bands::Banded;
use crate::fontset::FontSet;
use crate::{Scene, Subresources, U};
use navigator_style::cascade::Env;
use std::collections::{BTreeMap, BTreeSet};

/// view → source: keep `range` ready at `zoom_64` (64 = 100%), with
/// `prefetch` more above and below (a hint: about a viewport).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want { pub range: (U, U), pub zoom_64: u32, pub prefetch: U }

/// source → view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The document's height; `settled` once it is exact. Sent whenever it
    /// changes.
    Extent { height: U, settled: bool },
    /// Band `band` covers `range` and its group is in the window.
    BandReady { band: u32, range: (U, U) },
    /// Its content or extent changed (a box it cuts was sized by a later
    /// band, leaves from another band arrived, images, a restyle).
    BandUpdated { band: u32, range: (U, U) },
    /// Gone: anything of it still visible shows placeholders (§10.1).
    BandEvicted { band: u32 },
}

/// The groups a source writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupId { Band(u32), Overlay }

/// Where a source's groups go: placed by the source under the content
/// group the view names. A band's group is clipped to its `range`; the
/// overlay's leaves carry their own clips.
pub trait SceneSink {
    fn put(&mut self, group: GroupId, clip: Option<(U, U)>, scene: &Scene);
    fn clear(&mut self, group: GroupId);
}

/// The contract (pergola.md §10.4).
pub trait ContentSource {
    /// The view's wish. Lays out what it needs, writes groups, evicts what
    /// is outside the range plus prefetch, and says what changed.
    fn want(&mut self, w: Want, sink: &mut dyn SceneSink) -> Vec<Event>;
    /// Background work while the view is idle: lay out up to `bands` more
    /// of the document (so the extent settles and far jumps are cheap).
    fn idle(&mut self, bands: usize, sink: &mut dyn SceneSink) -> Vec<Event>;
}

/// ★ A DOCUMENT AS A CONTENT SOURCE: the Navigator worker's, and a
/// `DocumentView`'s. Keeps resident only the bands the last `want` reaches.
pub struct DocumentSource<'f> {
    html: String,
    fonts: &'f FontSet,
    subs: Subresources,
    window: (f64, f64),
    zoom_64: u32,
    band_h: U,
    doc: Banded<'f>,
    /// Bands whose groups are in the window, with what their group was
    /// built from (so a change is noticed and sent as `band_updated`).
    resident: BTreeMap<usize, Vec<(usize, usize, usize)>>,
    last_extent: Option<(U, bool)>,
    overlay_sig: Option<(usize, u64)>,
    last_want: Option<Want>,
    /// The provisional band shown for a far jump, while the first pass has
    /// not reached it: (its id, its range).
    prov: Option<(u32, (U, U))>,
    prov_seq: u32,
}

/// Provisional bands' ids: `PROVISIONAL | n`, apart from exact bands'.
pub const PROVISIONAL: u32 = 1 << 30;

impl<'f> DocumentSource<'f> {
    /// A normalized document in a `window` (CSS px), cut into bands at the
    /// first checkpoint below `band_h` (1/64 px; about a viewport).
    pub fn new(html: String, fonts: &'f FontSet, subs: Subresources, window: (f64, f64), band_h: U) -> Self {
        let doc = Banded::new(&html, fonts, &Env::zoomed(window.0, window.1, 64), &subs, band_h);
        DocumentSource { html, fonts, subs, window, zoom_64: 64, band_h, doc, resident: BTreeMap::new(),
                         last_extent: None, overlay_sig: None, last_want: None, prov: None, prov_seq: 0 }
    }

    /// The layout state (for measurement and checks).
    pub fn banded(&self) -> &Banded<'f> { &self.doc }

    /// The bands resident now.
    pub fn resident(&self) -> Vec<usize> { self.resident.keys().copied().collect() }

    fn rezoom(&mut self, zoom_64: u32, sink: &mut dyn SceneSink, ev: &mut Vec<Event>) {
        for (&k, _) in std::mem::take(&mut self.resident).iter() {
            sink.clear(GroupId::Band(k as u32));
            ev.push(Event::BandEvicted { band: k as u32 });
        }
        sink.clear(GroupId::Overlay);
        self.overlay_sig = None;
        self.drop_prov(sink, ev);
        self.zoom_64 = zoom_64;
        self.doc = Banded::new(&self.html, self.fonts, &Env::zoomed(self.window.0, self.window.1, zoom_64), &self.subs, self.band_h);
    }

    /// Bring the window up to date with `keep`: bands in it resident and
    /// current, the rest evicted, the overlay and extent sent if changed.
    fn sync(&mut self, keep: (U, U), sink: &mut dyn SceneSink, ev: &mut Vec<Event>) {
        // ★ A FAR JUMP: the range is well beyond the first pass. A little of
        // the pass now, and if that does not reach it, a PROVISIONAL band
        // for the range at once (`Banded::provisional`) instead of laying
        // out everything above it first; the pass goes on in idle time and
        // replaces it with exact bands when it gets there.
        let frontier = |d: &Banded| d.bands.last().map(|b| b.range.1).unwrap_or(0);
        if !self.doc.complete && keep.0 > frontier(&self.doc) + 2 * self.band_h {
            for _ in 0..2 { if self.doc.advance(false).is_none() { break } }
            if !self.doc.complete && keep.0 > frontier(&self.doc) + 2 * self.band_h {
                let covered = self.prov.is_some_and(|(_, r)| r.0 <= keep.0 && r.1 >= keep.1.min(r.0 + (keep.1 - keep.0)));
                if !covered {
                    self.drop_prov(sink, ev);
                    if let Some((range, scene)) = self.doc.provisional(keep.0, keep.1 - keep.0) {
                        let id = PROVISIONAL | (self.prov_seq & (PROVISIONAL - 1));
                        self.prov_seq = self.prov_seq.wrapping_add(1);
                        sink.put(GroupId::Band(id), Some(range), &scene);
                        ev.push(Event::BandReady { band: id, range });
                        self.prov = Some((id, range));
                    }
                }
                // Exact bands outside the range go, as ever.
                for k in self.resident.keys().copied().collect::<Vec<_>>() {
                    let r = self.doc.bands[k].range;
                    if !(r.1 > keep.0 && r.0 < keep.1) {
                        self.resident.remove(&k);
                        self.doc.evict(k);
                        sink.clear(GroupId::Band(k as u32));
                        ev.push(Event::BandEvicted { band: k as u32 });
                    }
                }
                let e = self.doc.extent();
                if self.last_extent != Some(e) { self.last_extent = Some(e); ev.push(Event::Extent { height: e.0, settled: e.1 }) }
                return;
            }
        }
        // Lay the first pass out as far as the range reaches.
        while !self.doc.complete && self.doc.bands.last().is_none_or(|b| b.range.1 <= keep.1) {
            let k = self.doc.laid_out();
            // Kept as laid out, and dropped at once unless it lands in the
            // range (known only once laid out): laying it out a second time
            // to keep it doubled a one-band document's cost.
            self.doc.advance(true);
            let r = self.doc.bands[k].range;
            if !(r.1 > keep.0 && r.0 < keep.1) { self.doc.evict(k); }
        }
        let wanted: BTreeSet<usize> = (0..self.doc.laid_out()).filter(|&k| { let r = self.doc.bands[k].range; r.1 > keep.0 && r.0 < keep.1 }).collect();
        for k in self.resident.keys().copied().collect::<Vec<_>>() {
            if !wanted.contains(&k) {
                self.resident.remove(&k);
                self.doc.evict(k);
                sink.clear(GroupId::Band(k as u32));
                ev.push(Event::BandEvicted { band: k as u32 });
            }
        }
        for &k in &wanted { self.doc.ensure(k); }
        for &k in &wanted {
            let sig = self.doc.foreign_sig(k);
            let range = self.doc.bands[k].range;
            match self.resident.get(&k) {
                Some(old) if *old == sig => continue,
                Some(_) => ev.push(Event::BandUpdated { band: k as u32, range }),
                None => ev.push(Event::BandReady { band: k as u32, range }),
            }
            let g = self.doc.group(k).expect("resident");
            sink.put(GroupId::Band(k as u32), Some(range), &g);
            self.resident.insert(k, sig);
        }
        // A band laid out again (a box it cuts was sized later) is sent again.
        for k in self.resident.keys().copied().collect::<Vec<_>>() {
            if self.doc.bands[k].stale {
                self.doc.ensure(k);
                let range = self.doc.bands[k].range;
                sink.put(GroupId::Band(k as u32), Some(range), &self.doc.group(k).expect("resident"));
                self.resident.insert(k, self.doc.foreign_sig(k));
                ev.push(Event::BandUpdated { band: k as u32, range });
            }
        }
        // The exact bands are in: the provisional one goes.
        self.drop_prov(sink, ev);
        let ov = self.doc.overlay();
        let sig = (ov.order.len(), fingerprint(&ov));
        if self.overlay_sig != Some(sig) {
            if ov.order.is_empty() { sink.clear(GroupId::Overlay) } else { sink.put(GroupId::Overlay, None, &ov) }
            self.overlay_sig = Some(sig);
        }
        let e = self.doc.extent();
        if self.last_extent != Some(e) {
            self.last_extent = Some(e);
            ev.push(Event::Extent { height: e.0, settled: e.1 });
        }
    }
}

impl DocumentSource<'_> {
    fn drop_prov(&mut self, sink: &mut dyn SceneSink, ev: &mut Vec<Event>) {
        if let Some((id, _)) = self.prov.take() {
            sink.clear(GroupId::Band(id));
            ev.push(Event::BandEvicted { band: id });
        }
    }
}

/// A cheap fingerprint of a scene's leaf geometry (to tell the overlay
/// changed without sending it again).
fn fingerprint(s: &Scene) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |v: i64| { h ^= v as u64; h = h.wrapping_mul(0x100000001b3) };
    for &(k, i) in &s.order {
        mix(k as i64);
        match k {
            0 => { let r = &s.rects[i]; mix(r.x); mix(r.y); mix(r.w); mix(r.h); mix(r.rgba as i64) }
            1 => { let r = &s.runs[i]; mix(r.x); mix(r.y); mix(r.glyphs.len() as i64) }
            3 => { let r = &s.shadows[i]; mix(r.x); mix(r.y); mix(r.w); mix(r.h) }
            4 => { let a = &s.grads[i].area; mix(a.x); mix(a.y); mix(a.w); mix(a.h) }
            5 => { let a = &s.images[i].area; mix(a.x); mix(a.y); mix(a.w); mix(a.h) }
            _ => {}
        }
    }
    for c in &s.clips { mix(c.1); mix(c.3) }
    h
}

impl ContentSource for DocumentSource<'_> {
    fn want(&mut self, w: Want, sink: &mut dyn SceneSink) -> Vec<Event> {
        let mut ev = vec![];
        if w.zoom_64.max(1) != self.zoom_64 { self.rezoom(w.zoom_64.max(1), sink, &mut ev) }
        self.last_want = Some(w);
        let p = w.prefetch.max(0);
        self.sync((w.range.0.saturating_sub(p), w.range.1.saturating_add(p)), sink, &mut ev);
        ev
    }

    fn idle(&mut self, bands: usize, sink: &mut dyn SceneSink) -> Vec<Event> {
        let mut ev = vec![];
        for _ in 0..bands { if self.doc.advance(false).is_none() { break } }
        if self.doc.complete && !self.doc.settled() { self.doc.settle() }
        if let Some(w) = self.last_want {
            let p = w.prefetch.max(0);
            self.sync((w.range.0.saturating_sub(p), w.range.1.saturating_add(p)), sink, &mut ev);
        }
        ev
    }
}
