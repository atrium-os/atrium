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
    text_64: u32,
    /// The reader's appearance: `prefers-color-scheme: dark` matches when set.
    dark: bool,
    band_h: U,
    doc: Banded<'f>,
    /// Bands whose groups are in the window, with what their group was
    /// built from (so a change is noticed and sent as `band_updated`).
    resident: BTreeMap<usize, (Vec<(usize, usize, usize)>, u64)>,
    last_extent: Option<(U, bool)>,
    overlay_sig: Option<(usize, u64)>,
    last_want: Option<Want>,
    /// The provisional band shown for a far jump, while the first pass has
    /// not reached it: (its id, its range).
    prov: Option<(u32, (U, U))>,
    prov_seq: u32,
    /// The band the last `replace` laid out again from (`None`: nothing).
    replaced_from: Option<usize>,
    /// The state the last `replace` replaced: its normalized text, prepared.
    previous: Option<(String, crate::html::Prepared)>,
}

/// Provisional bands' ids: `PROVISIONAL | n`, apart from exact bands'.
pub const PROVISIONAL: u32 = 1 << 30;

impl<'f> DocumentSource<'f> {
    /// A normalized document in a `window` (CSS px), cut into bands at the
    /// first checkpoint below `band_h` (1/64 px; about a viewport).
    pub fn new(html: String, fonts: &'f FontSet, subs: Subresources, window: (f64, f64), band_h: U) -> Self {
        let doc = Banded::new(&html, fonts, &Env::zoomed(window.0, window.1, 64), &subs, band_h);
        DocumentSource { html, fonts, subs, window, zoom_64: 64, text_64: 64, dark: false, band_h, doc, resident: BTreeMap::new(),
                         last_extent: None, overlay_sig: None, last_want: None, prov: None, prov_seq: 0, replaced_from: None, previous: None }
    }

    /// The reader's dark appearance, so a page's `prefers-color-scheme: dark`
    /// rules apply (the host passes the system setting). Set it before the
    /// first `want`: it lays the document out again.
    pub fn with_dark(mut self, dark: bool) -> Self {
        if dark != self.dark {
            self.dark = dark;
            self.doc = Banded::new(&self.html, self.fonts, &self.env(self.zoom_64), &self.subs, self.band_h);
        }
        self
    }

    fn env(&self, zoom_64: u32) -> Env { Env { dark: self.dark, text_scale_64: self.text_64, ..Env::zoomed(self.window.0, self.window.1, zoom_64) } }

    /// The reader's text size in 1/64 (backend §4.9 item 2; 64 = the
    /// profile's own). Set it before the first `want`: it lays the
    /// document out again.
    pub fn with_text_scale(mut self, text_64: u32) -> Self {
        let t = text_64.clamp(16, 1024);
        if t != self.text_64 {
            self.text_64 = t;
            self.doc = Banded::new(&self.html, self.fonts, &self.env(self.zoom_64), &self.subs, self.band_h);
        }
        self
    }

    /// The layout state (for measurement and checks).
    pub fn banded(&self) -> &Banded<'f> { &self.doc }

    /// The band the last `replace` laid out again from (`None`: the change
    /// made no difference the layout can see).
    pub fn replaced_from(&self) -> Option<usize> { self.replaced_from }

    /// The state the last `replace` replaced (its normalized text, and that
    /// prepared at this zoom), once: a caller keeping laid-out states (the
    /// worker's, for going back) takes it.
    pub fn take_previous(&mut self) -> Option<(String, crate::html::Prepared)> { self.previous.take() }

    /// The zoom this source lays out at (64 = 100%).
    pub fn zoom_64(&self) -> u32 { self.zoom_64 }

    /// Whether it lays out in the dark appearance (`with_dark`).
    pub fn dark(&self) -> bool { self.dark }

    /// The bands resident now.
    pub fn resident(&self) -> Vec<usize> { self.resident.keys().copied().collect() }

    /// ★ THE DOCUMENT CHANGED (backend §4.3, replay part 2): `html` is its
    /// new normalized state, and `prep` that state prepared at this
    /// source's zoom when the caller already has it. The layout is redone
    /// only from the checkpoint above the first change (`Banded::replace`),
    /// the window is brought up to date with the last `want`, and a band is
    /// sent again only if what it shows changed: `band_updated` names
    /// exactly the bands whose range or scene differ.
    pub fn replace(&mut self, html: String, prep: Option<crate::html::Prepared>, sink: &mut dyn SceneSink) -> Vec<Event> {
        let mut ev = vec![];
        let prep = prep.unwrap_or_else(|| crate::html::prepare(&html, self.fonts, &self.env(self.zoom_64)));
        let shown = self.shown();
        let old_html = std::mem::take(&mut self.html);
        let (from, old) = self.doc.replace_keeping(prep);
        self.replaced_from = from;
        self.previous = old.map(|p| (old_html, p));
        self.html = html;
        if self.replaced_from.is_none() { return ev }
        self.drop_prov(sink, &mut ev);
        self.resync(shown, sink, &mut ev);
        ev
    }

    /// A source on a document already prepared at `zoom_64` (and the
    /// appearance `dark`: `with_dark`).
    pub fn from_prepared(html: String, prep: crate::html::Prepared, fonts: &'f FontSet, subs: Subresources, window: (f64, f64), band_h: U, zoom_64: u32, dark: bool) -> Self {
        let doc = Banded::from_prepared(prep, fonts, &Env { dark, ..Env::zoomed(window.0, window.1, zoom_64) }, &subs, band_h);
        DocumentSource { html, fonts, subs, window, zoom_64, text_64: 64, dark, band_h, doc, resident: BTreeMap::new(),
                         last_extent: None, overlay_sig: None, last_want: None, prov: None, prov_seq: 0, replaced_from: None, previous: None }
    }

    /// Lay out what `w` would show, sending nothing (a state laid out
    /// ahead, to be swapped in by `swap_in`).
    pub fn prime(&mut self, w: Want) {
        struct Null;
        impl SceneSink for Null { fn put(&mut self, _: GroupId, _: Option<(U, U)>, _: &Scene) {} fn clear(&mut self, _: GroupId) {} }
        let _ = self.want(w, &mut Null);
    }

    /// The size of its normalized text (memory accounting).
    pub fn html_len(&self) -> usize { self.html.len() }

    /// The last `want` (the view's place).
    pub fn last_want(&self) -> Option<Want> { self.last_want }

    /// ★ A STATE LAID OUT AHEAD, SHOWN (replay part 2): `other` — a source
    /// on the new state, built and primed in idle time — takes this one's
    /// place in the view: the window is brought up to date with this one's
    /// last `want`, and only what differs from what the view holds is sent,
    /// as `replace` does. Returns the source it replaced (kept by a caller
    /// for going back). `other` must lay out at this source's zoom and
    /// appearance.
    pub fn swap_in(&mut self, mut other: DocumentSource<'f>, sink: &mut dyn SceneSink) -> (Vec<Event>, DocumentSource<'f>) {
        let mut ev = vec![];
        let shown = self.shown();
        self.drop_prov(sink, &mut ev);
        other.last_want = self.last_want;
        other.last_extent = self.last_extent;
        // What priming laid out was never sent: the view holds this one's.
        other.prov = None;
        other.prov_seq = other.prov_seq.max(self.prov_seq);
        let old = std::mem::replace(self, other);
        self.resync(shown, sink, &mut ev);
        (ev, old)
    }

    /// What the view holds now: each resident band's range and group (NSG),
    /// and the overlay (`None`: none sent).
    fn shown(&self) -> Shown {
        Shown {
            bands: self.resident.keys().filter_map(|&k| self.doc.group(k).map(|g| (k, (self.doc.bands[k].range, crate::nsg::write(&g, self.fonts))))).collect(),
            overlay: self.overlay_sig.filter(|s| s.0 > 0).map(|_| crate::nsg::write(&self.doc.overlay(), self.fonts)),
        }
    }

    /// Bring the view, which holds `shown`, up to date with this source at
    /// the last `want`, sending only what differs.
    fn resync(&mut self, shown: Shown, sink: &mut dyn SceneSink, ev: &mut Vec<Event>) {
        let Some(w) = self.last_want else {
            for k in shown.bands.into_keys() { sink.clear(GroupId::Band(k as u32)); ev.push(Event::BandEvicted { band: k as u32 }) }
            if shown.overlay.is_some() { sink.clear(GroupId::Overlay) }
            self.resident.clear();
            return;
        };
        self.resident.clear();
        self.overlay_sig = None;
        let mut cap = Capture::default();
        let mut got = vec![];
        let p = w.prefetch.max(0);
        self.sync((w.range.0.saturating_sub(p), w.range.1.saturating_add(p)), &mut cap, &mut got);
        // Bands the view held that are no longer there.
        for &k in shown.bands.keys() {
            if !self.resident.contains_key(&k) { sink.clear(GroupId::Band(k as u32)); ev.push(Event::BandEvicted { band: k as u32 }) }
        }
        let mut sent: BTreeMap<GroupId, (Option<(U, U)>, Scene)> = BTreeMap::new();
        let mut overlay_cleared = false;
        for c in cap.0 {
            match c {
                Cap::Put(g, clip, scene) => { sent.insert(g, (clip, scene)); }
                Cap::Clear(GroupId::Overlay) => { sent.remove(&GroupId::Overlay); overlay_cleared = true }
                Cap::Clear(g) => { sent.remove(&g); if !matches!(g, GroupId::Band(k) if shown.bands.contains_key(&(k as usize))) { sink.clear(g) } }
            }
        }
        // The overlay: sent again only if it differs from what the view
        // holds; gone if the new layout has none or the view is waiting on
        // a provisional band (which a source built on this state would show
        // with no overlay).
        match (sent.remove(&GroupId::Overlay), &shown.overlay) {
            (Some((_, scene)), Some(old)) if *old == crate::nsg::write(&scene, self.fonts) => {}
            (Some((clip, scene)), _) => sink.put(GroupId::Overlay, clip, &scene),
            (None, Some(_)) if overlay_cleared || self.prov.is_some() => { sink.clear(GroupId::Overlay); self.overlay_sig = None }
            (None, _) => {}
        }
        for (g, (clip, scene)) in sent {
            match g {
                GroupId::Band(k) => match shown.bands.get(&(k as usize)) {
                    Some((r, s)) if Some(*r) == clip && *s == crate::nsg::write(&scene, self.fonts) => {}
                    Some(_) => { sink.put(g, clip, &scene); ev.push(Event::BandUpdated { band: k, range: clip.unwrap_or_default() }) }
                    None => { sink.put(g, clip, &scene); ev.push(Event::BandReady { band: k, range: clip.unwrap_or_default() }) }
                },
                GroupId::Overlay => unreachable!("taken above"),
            }
        }
        // Extent and eviction events as `sync` produced them; its ready and
        // updated ones are replaced by the comparison above.
        ev.extend(got.into_iter().filter(|e| matches!(e, Event::Extent { .. } | Event::BandEvicted { .. })));
    }

    fn rezoom(&mut self, zoom_64: u32, sink: &mut dyn SceneSink, ev: &mut Vec<Event>) {
        for (&k, _) in std::mem::take(&mut self.resident).iter() {
            sink.clear(GroupId::Band(k as u32));
            ev.push(Event::BandEvicted { band: k as u32 });
        }
        sink.clear(GroupId::Overlay);
        self.overlay_sig = None;
        self.drop_prov(sink, ev);
        self.zoom_64 = zoom_64;
        self.doc = Banded::new(&self.html, self.fonts, &self.env(zoom_64), &self.subs, self.band_h);
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
        // ★ A band laid out again here (stale: a box it cuts was sized by a
        // later band) is sent again even when its foreign leaves are the
        // same: `ensure` clears `stale`, so the loop for stale bands below
        // never saw it, and the view kept the band without the box.
        let mut relaid = BTreeSet::new();
        for &k in &wanted { if self.doc.ensure(k) { relaid.insert(k); } }
        for &k in &wanted {
            let sig = (self.doc.foreign_sig(k), self.doc.bands[k].built);
            let range = self.doc.bands[k].range;
            match self.resident.get(&k) {
                Some(old) if *old == sig && !relaid.contains(&k) => continue,
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
                self.resident.insert(k, (self.doc.foreign_sig(k), self.doc.bands[k].built));
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

/// What a view holds (`DocumentSource::shown`).
struct Shown { bands: BTreeMap<usize, ((U, U), String)>, overlay: Option<String> }

/// A sink that keeps what it is given, in order (`DocumentSource::replace`).
#[derive(Default)]
struct Capture(Vec<Cap>);
enum Cap { Put(GroupId, Option<(U, U)>, Scene), Clear(GroupId) }
impl SceneSink for Capture {
    fn put(&mut self, group: GroupId, clip: Option<(U, U)>, scene: &Scene) { self.0.push(Cap::Put(group, clip, scene.clone())) }
    fn clear(&mut self, group: GroupId) { self.0.push(Cap::Clear(group)) }
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
