//! ★ BANDS (backend §4.8 step 3, phase 1; pergola.md §10.3): a scene cut
//! into horizontal bands of about a viewport each, so a host — the
//! Navigator's UI, a `DocumentView`, a long list — holds only the bands near
//! what is on screen, and finds the nodes for a viewport without walking the
//! whole page.
//!
//! The contract with a host is "the scene for this range of the document":
//! `nodes_in(y0, y1)` gives the draw-order entries that paint there, and
//! `band_scene(b)` a self-contained `Scene` for band `b` (its nodes in draw
//! order, with the scene's clip, group, transform and scroll tables, which
//! are small, so every attribute index still names the same entry).
//!
//! Every node is in every band its painted extent touches: a page
//! background is in all of them, a line of text in one or two. The extent is
//! the node's own box, through its transform when it has one; a clip can
//! only make it smaller, so it is not consulted (a band may hold a node its
//! clip hides there; it never misses one that shows).

use crate::{Scene, U};

/// Draw-order entries per band.
#[derive(Debug, Clone, Default)]
pub struct BandIndex {
    /// Each band's height, 1/64 px.
    pub band_h: U,
    /// For each band, indices into `scene.order`, ascending (draw order).
    pub bands: Vec<Vec<u32>>,
    /// The scene's extent, 1/64 px (the height the bands cover).
    pub extent: U,
}

/// The vertical extent a node paints over, 1/64 px, `None` for a node that
/// paints nothing (an empty box).
pub fn extent(s: &Scene, kind: u8, i: usize) -> Option<(U, U)> {
    let (y0, y1, x0, x1, xf) = match kind {
        0 => { let r = &s.rects[i]; (r.y, r.y + r.h, r.x, r.x + r.w, s.rect_attrs.get(i).and_then(|a| a.2)) }
        // A run: from its baseline, up an em and down a third of one.
        1 => { let r = &s.runs[i]; let w = r.glyphs.last().map(|g| g.1).unwrap_or(0) + r.size;
               (r.y - r.size, r.y + r.size / 3 + 1, r.x, r.x + w, s.run_attrs.get(i).and_then(|a| a.2)) }
        2 => { let l = &s.links[i]; (l.y, l.y + l.h, l.x, l.x + l.w, s.link_attrs.get(i).and_then(|a| a.2)) }
        3 => { let sh = &s.shadows[i]; let b = sh.blur * 2;
               (sh.y - b, sh.y + sh.h + b, sh.x - b, sh.x + sh.w + b, s.shadow_attrs.get(i).and_then(|a| a.2)) }
        4 => { let a = &s.grads[i].area; (a.y, a.y + a.h, a.x, a.x + a.w, s.grad_attrs.get(i).and_then(|a| a.2)) }
        5 => { let a = &s.images[i].area; (a.y, a.y + a.h, a.x, a.x + a.w, s.image_attrs.get(i).and_then(|a| a.2)) }
        7 => { let e = &s.embeds[i]; (e.y, e.y + e.h, e.x, e.x + e.w, s.embed_attrs.get(i).and_then(|a| a.2)) }
        6 => {
            // A path's points through its own transform (already page units).
            let p = &s.paths[i];
            let (mut lo, mut hi, mut l, mut r) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
            for &(x, y) in &p.points {
                let t = &p.transform;
                let px = ((t[0] as i128 * x as i128 + t[2] as i128 * y as i128) / 65536 + t[4] as i128) as i64;
                let py = ((t[1] as i128 * x as i128 + t[3] as i128 * y as i128) / 65536 + t[5] as i128) as i64;
                lo = lo.min(py); hi = hi.max(py); l = l.min(px); r = r.max(px);
            }
            if lo > hi { return None }
            (lo, hi + 1, l, r + 1, s.path_attrs.get(i).and_then(|a| a.2))
        }
        _ => return None,
    };
    if y1 <= y0 { return None }
    // Through the node's transform (paint-level, composed with its
    // ancestors'): the four corners' vertical span.
    match xf.and_then(|x| s.xforms.get(x as usize)) {
        Some(m) => {
            let ys = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)].map(|(x, y)|
                ((m[1] as i128 * x as i128 + m[3] as i128 * y as i128) / 65536 + m[5] as i128) as i64);
            let (lo, hi) = (ys.iter().copied().min().unwrap(), ys.iter().copied().max().unwrap());
            (hi > lo).then_some((lo, hi))
        }
        None => Some((y0, y1)),
    }
}

/// Index `scene` into bands of `band_h` (1/64 px).
pub fn index(scene: &Scene, band_h: U) -> BandIndex {
    let band_h = band_h.max(1);
    let mut extent_max: U = scene.height.max(0);
    let spans: Vec<Option<(U, U)>> = scene.order.iter().map(|&(k, i)| extent(scene, k, i)).collect();
    for s in spans.iter().flatten() { extent_max = extent_max.max(s.1) }
    let n = ((extent_max + band_h - 1) / band_h).max(1) as usize;
    let mut bands: Vec<Vec<u32>> = vec![vec![]; n];
    for (o, s) in spans.iter().enumerate() {
        let Some((y0, y1)) = *s else { continue };
        let (b0, b1) = ((y0.max(0) / band_h) as usize, ((y1 - 1).max(0) / band_h) as usize);
        for b in bands.iter_mut().take(b1.min(n - 1) + 1).skip(b0.min(n - 1)) { b.push(o as u32) }
    }
    BandIndex { band_h, bands, extent: extent_max }
}

impl BandIndex {
    /// The draw-order entries that paint anywhere in `[y0, y1)`, ascending.
    pub fn nodes_in(&self, y0: U, y1: U) -> Vec<u32> {
        if self.bands.is_empty() || y1 <= y0 { return vec![] }
        let last = self.bands.len() - 1;
        let (b0, b1) = (((y0.max(0)) / self.band_h) as usize, (((y1 - 1).max(0)) / self.band_h) as usize);
        let (b0, b1) = (b0.min(last), b1.min(last));
        if b0 == b1 { return self.bands[b0].clone() }
        let mut out: Vec<u32> = self.bands[b0..=b1].iter().flatten().copied().collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Band `b` as a self-contained scene: the scene's tables, and only the
/// nodes the band holds, in draw order. Node vectors are filtered, so a
/// node's index differs from the full scene's; its attributes do not.
pub fn band_scene(scene: &Scene, idx: &BandIndex, b: usize) -> Scene {
    let y0 = b as U * idx.band_h;
    scene_of(scene, &idx.nodes_in(y0, y0 + idx.band_h))
}

/// A scene holding only the draw-order entries `which` (ascending).
pub fn scene_of(scene: &Scene, which: &[u32]) -> Scene {
    let mut out = Scene {
        width: scene.width, height: scene.height, clips: scene.clips.clone(), xforms: scene.xforms.clone(),
        xform_parents: scene.xform_parents.clone(), groups: scene.groups.clone(), scrolls: scene.scrolls.clone(),
        ..Default::default()
    };
    for &o in which {
        let (k, i) = scene.order[o as usize];
        let at = match k {
            0 => { out.rects.push(scene.rects[i].clone()); out.rect_attrs.push(scene.rect_attrs[i]); out.rects.len() - 1 }
            1 => { out.runs.push(scene.runs[i].clone()); out.run_attrs.push(scene.run_attrs[i]); out.runs.len() - 1 }
            2 => { out.links.push(scene.links[i].clone()); out.link_attrs.push(scene.link_attrs[i]); out.links.len() - 1 }
            3 => { out.shadows.push(scene.shadows[i].clone()); out.shadow_attrs.push(scene.shadow_attrs[i]); out.shadows.len() - 1 }
            4 => { out.grads.push(scene.grads[i].clone()); out.grad_attrs.push(scene.grad_attrs[i]); out.grads.len() - 1 }
            5 => { out.images.push(scene.images[i].clone()); out.image_attrs.push(scene.image_attrs[i]); out.images.len() - 1 }
            6 => { out.paths.push(scene.paths[i].clone()); out.path_attrs.push(scene.path_attrs[i]); out.paths.len() - 1 }
            7 => { out.embeds.push(scene.embeds[i].clone()); out.embed_attrs.push(scene.embed_attrs[i]); out.embeds.len() - 1 }
            _ => continue,
        };
        out.order.push((k, at));
    }
    out
}

// ---------------------------------------------------------------------------
// ★ PHASE 2: CHECKPOINTS, and bands laid out on their own (backend §4.8
// step 3; pergola.md §10.4; fresco-scene-groups.md §6).
// ---------------------------------------------------------------------------

use crate::fontset::FontSet;
use crate::html::{self, BandRun, Prepared, StackKey};
use crate::Subresources;
use navigator_dom::Handle;
use navigator_style::cascade::Env;
use std::collections::HashMap;

/// Where a band begins: before block `at` (none for the document's start),
/// at `y`, with the vector-SVG share left there. Bands are cut ONLY here.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint { pub at: Option<Handle>, pub y: U, pub vector_budget: usize }

/// One band: its range, its scene while resident, and the leaves its run
/// painted OUTSIDE its range (positioned and transformed boxes, shadows) —
/// those go in the overlay group, once for each other band they reach.
#[derive(Debug, Default)]
pub struct BandSlot {
    /// `[y0, y1)`, 1/64 px. The first band starts at `U::MIN / 4` (content
    /// above the page's top is its); the last ends at `U::MAX / 4`.
    pub range: (U, U),
    /// The band's group, while resident: every leaf its run painted that
    /// touches its range (a host clips the group to `range`).
    pub scene: Option<Scene>,
    /// Leaves its run painted that reach outside its range, with their
    /// painted extents.
    cross: Scene,
    cross_ext: Vec<(U, U)>,
    /// Painting-order keys of the resident scene's leaves and of `cross`.
    keys: Vec<GKey>,
    cross_keys: Vec<GKey>,
    /// A box this band cuts was completed by a later band: its scene must be
    /// laid out again before it is shown (it painted nothing for that box).
    pub stale: bool,
    /// …and that box positions or transforms what this band laid out: its
    /// overlay leaves must be laid out again too.
    pub cross_stale: bool,
}

/// What a `Banded` did, for measurement.
#[derive(Debug, Default, Clone, Copy)]
pub struct BandStats {
    /// Band runs: the first pass, and runs again from a checkpoint.
    pub first_runs: u64,
    pub reruns: u64,
    /// Boxes a band cut before their size was known (painted by a later run).
    pub suppressed: u64,
    /// Leaves painted outside their own band (the overlay's sources).
    pub cross_leaves: u64,
    /// Whether checkpoints failed and the document was laid out whole.
    pub whole: bool,
    /// Provisional bands laid out for far jumps (`Banded::provisional`).
    pub provisional: u64,
}

/// ★ A DOCUMENT LAID OUT BAND BY BAND. Parsing and the cascade are done
/// once; the layout is done one band at a time, in document order the
/// first time (each band from where the previous stopped), and again from
/// its checkpoint whenever an evicted band is wanted back. A band run lays
/// out only that band's blocks, so memory and time follow the bands
/// resident, not the document.
///
/// Correctness rests on three things, each checked by pixels against the
/// whole layout (`tests/bands.rs`, `examples/band_check`):
/// - a run lays out exactly what the whole layout lays out there
///   (`BandRun`: cuts only between blocks in plain flow);
/// - a box that spans bands is painted by every band it spans, at its real
///   size once a band has completed it (bands it cut before that are
///   `stale` and laid out again);
/// - a leaf a band paints outside itself is drawn by the overlay group,
///   clipped to each other band it reaches.
pub struct Banded<'f> {
    prep: Prepared,
    fonts: &'f FontSet,
    env: Env,
    subs: Subresources,
    band_h: U,
    /// `checkpoints[k]` begins band `k`.
    pub checkpoints: Vec<Checkpoint>,
    pub bands: Vec<BandSlot>,
    done: HashMap<Handle, (U, u32)>,
    lines_before: HashMap<Handle, (U, u32)>,
    at: HashMap<Handle, U>,
    firsts: HashMap<Handle, u32>,
    row_cuts: HashMap<Handle, std::rc::Rc<html::TableCuts>>,
    rows_cache: HashMap<Handle, std::rc::Rc<(Vec<Handle>, Vec<Handle>)>>,
    shapes: crate::ShapeCache,
    finals: HashMap<Handle, U>,
    span_starts: SpanStarts,
    /// `reach_index`, built when first needed after a run.
    reach: std::cell::RefCell<Option<(HashMap<i64, Vec<(usize, usize)>>, Vec<(usize, usize)>)>>,
    /// The first pass reached the end of the document.
    pub complete: bool,
    /// The lowest painted bottom and the root's height so far.
    bottom: U,
    root_h: U,
    /// Where the first pass has reached, as a fraction of the document's
    /// nodes (for the extent estimate).
    progress: f64,
    pub stats: BandStats,
    /// When checkpoints failed: the whole layout, as the only band.
    whole: Option<Scene>,
}

/// ★ A leaf's place in the WHOLE document's painting order: its stacking
/// key, then where the whole layout would have emitted it — (the band whose
/// run emitted it, its emission index there). A box's own paint belongs
/// where the box began, so a box that spans bands is keyed by its first
/// band's emission in every band it paints. Bands are laid out in document
/// order, so this orders leaves of different bands as the whole layout does.
#[derive(Debug, Clone)]
struct GKey { stack: StackKey, at: (u32, u32) }

fn gcmp(a: &GKey, b: &GKey) -> std::cmp::Ordering { html::stack_cmp(&a.stack, &b.stack).then(a.at.cmp(&b.at)) }

/// Where each spanning box began: (its first band, the emission index of
/// its first leaf there).
type SpanStarts = HashMap<Handle, (u32, u32)>;

const TOP: U = U::MIN / 4;
const END: U = U::MAX / 4;

impl<'f> Banded<'f> {
    /// Parse and style `html`; nothing is laid out yet. `band_h` is the
    /// height a band is cut at (at the first checkpoint below it).
    pub fn new(html: &str, fonts: &'f FontSet, env: &Env, subs: &Subresources, band_h: U) -> Self {
        let prep = html::prepare(html, fonts, env);
        Banded { prep, fonts, env: env.clone(), subs: subs.clone(), band_h: band_h.max(PX_MIN),
                 checkpoints: vec![Checkpoint { at: None, y: 0, vector_budget: crate::path::DOCUMENT_SEGMENT_SHARE }],
                 bands: vec![], done: HashMap::new(), lines_before: HashMap::new(), at: HashMap::new(), firsts: Default::default(), row_cuts: HashMap::new(), rows_cache: HashMap::new(), shapes: Default::default(), finals: HashMap::new(), span_starts: HashMap::new(), reach: Default::default(), complete: false, bottom: 0, root_h: 0,
                 progress: 0.0, stats: BandStats::default(), whole: None }
    }

    /// A block, as its tag path (review tooling).
    pub fn describe(&self, h: Handle) -> String { self.prep.describe(h) }

    /// The bands laid out so far (the first pass's frontier).
    pub fn laid_out(&self) -> usize { self.bands.len() }

    /// The document's height: exact once `complete`, else an estimate from
    /// how far through the document's nodes the first pass is.
    pub fn extent(&self) -> (U, bool) {
        let vh = (self.env.height_px * crate::PX as f64) as U;
        if self.complete { return (self.root_h.max(self.bottom).max(vh), true) }
        let known = self.checkpoints.last().map(|c| c.y).unwrap_or(0).max(self.bottom);
        let est = if self.progress > 0.0 { (known as f64 / self.progress) as U } else { known };
        (est.max(known).max(vh), false)
    }

    /// Nothing left to lay out again: the overlay and every band's
    /// geometry are final.
    pub fn settled(&self) -> bool { self.complete && !self.bands.iter().any(|b| b.cross_stale) }

    /// The band holding `y`, if laid out.
    pub fn band_at(&self, y: U) -> Option<usize> {
        self.bands.iter().position(|b| y >= b.range.0 && y < b.range.1)
    }

    /// Run the first pass one band further. Returns the band laid out, or
    /// `None` once the document is complete. Its scene is kept when `keep`.
    pub fn advance(&mut self, keep: bool) -> Option<usize> {
        if self.complete { return None }
        let k = self.bands.len();
        self.run(k, keep);
        Some(k)
    }

    /// Make band `k` resident (laying out up to it the first time, and from
    /// its checkpoint again when evicted or stale). Returns whether it was
    /// laid out by this call.
    pub fn ensure(&mut self, k: usize) -> bool {
        let mut fresh = false;
        while self.bands.len() <= k && !self.complete { let kk = self.bands.len(); self.run(kk, kk == k); fresh = kk == k }
        let Some(b) = self.bands.get(k) else { return false };
        if b.scene.is_some() && !b.stale { return fresh }
        self.run(k, true);
        true
    }

    /// Drop band `k`'s scene (its checkpoint and overlay leaves stay).
    pub fn evict(&mut self, k: usize) -> bool {
        self.bands.get_mut(k).and_then(|b| b.scene.take()).is_some()
    }

    /// Finish the first pass and lay out again every band whose overlay
    /// leaves are stale, so the overlay is final. Scenes are not kept,
    /// except that resident bands are laid out again if stale.
    pub fn settle(&mut self) {
        while self.advance(false).is_some() {}
        for k in 0..self.bands.len() {
            let resident = self.bands[k].scene.is_some();
            if self.bands[k].cross_stale || (resident && self.bands[k].stale) { self.run(k, resident) }
        }
    }

    /// Band `k`'s scene, if resident.
    pub fn scene(&self, k: usize) -> Option<&Scene> { self.bands.get(k).and_then(|b| b.scene.as_ref()) }

    /// Lay band `k` out: the next one of the first pass (`k == bands.len()`)
    /// or again from its checkpoint.
    fn run(&mut self, k: usize, keep: bool) {
        *self.reach.borrow_mut() = None;
        if let Some(w) = &self.whole {
            if keep { if let Some(b) = self.bands.get_mut(k) { b.scene = Some(scene_of(w, &(0..w.order.len() as u32).collect::<Vec<_>>())) } }
            return;
        }
        let first = k == self.bands.len();
        let cp = self.checkpoints[k].clone();
        let mut r = BandRun::new(k as u32);
        if first { r.limit = Some(if k == 0 { self.band_h } else { cp.y + self.band_h }) }
        else { r.cut_at = self.checkpoints.get(k + 1).and_then(|c| c.at) }
        r.vector_budget = cp.vector_budget;
        if let Some(at) = cp.at { r.resume_path = self.prep.ancestors(at); r.resume_at = Some(at) }
        r.at = std::mem::take(&mut self.at);
        r.firsts = std::mem::take(&mut self.firsts);
        r.row_cuts = std::mem::take(&mut self.row_cuts);
        r.rows_cache = std::mem::take(&mut self.rows_cache);
        // Words already shaped stay shaped; the cache is bounded (it is
        // per word, so a document's vocabulary, not its length).
        if self.shapes.borrow().len() > 200_000 { self.shapes.borrow_mut().clear() }
        r.shape_cache = Some(self.shapes.clone());
        r.canvas_h = self.extent().0;
        r.done = std::mem::take(&mut self.done);
        r.lines_before = std::mem::take(&mut self.lines_before);
        r.finals = std::mem::take(&mut self.finals);
        let (scene, mut r) = html::lay_out_band(&self.prep, self.fonts, &self.env, &self.subs, r);
        self.done = std::mem::take(&mut r.done);
        self.lines_before = std::mem::take(&mut r.lines_before);
        self.at = std::mem::take(&mut r.at);
        self.firsts = std::mem::take(&mut r.firsts);
        self.row_cuts = std::mem::take(&mut r.row_cuts);
        self.rows_cache = std::mem::take(&mut r.rows_cache);
        self.finals = std::mem::take(&mut r.finals);
        if first { self.stats.first_runs += 1 } else { self.stats.reruns += 1 }
        self.stats.suppressed += r.suppressed as u64;
        let y0 = if k == 0 { TOP } else { cp.y };
        let y1 = if first {
            match r.fired {
                Some((at, y)) => {
                    // ★ No progress means a checkpoint the layout cannot
                    // resume from: never loop — lay the document out whole.
                    if y <= cp.y || Some(at) == cp.at { return self.fall_back_whole(keep) }
                    self.checkpoints.push(Checkpoint { at: Some(at), y, vector_budget: r.vector_budget });
                    self.progress = at as f64 / self.prep.node_count().max(1) as f64;
                    y
                }
                None => { self.complete = true; self.root_h = r.root_h; END }
            }
        } else { self.bands[k].range.1 };
        if first {
            // Boxes completed here that began in earlier bands: those bands
            // painted nothing for them (and positioned what they hold
            // without their size) — lay them out again when needed.
            for &(by, painted, moves) in &r.completed_spans {
                for j in 0..k {
                    let (a, b) = self.bands[j].range;
                    if b <= by || a >= y0 { continue }
                    if painted { self.bands[j].stale = true }
                    if moves { self.bands[j].cross_stale = true }
                }
            }
        }
        // Classify every leaf of the run, and key it in the whole
        // document's painting order.
        let mut span: HashMap<(u8, usize), (Handle, U, bool)> = HashMap::new();
        for &(kind, i, h, by, began_before) in &r.spanning { span.insert((kind, i), (h, by, began_before)); }
        // Boxes that begin here and span: where their paint is.
        for (o, &(kind, i)) in scene.order.iter().enumerate() {
            if let Some(&(h, _, false)) = span.get(&(kind, i)) {
                let e = r.keys[o].1;
                let v = self.span_starts.entry(h).or_insert((k as u32, e));
                if v.0 == k as u32 { v.1 = v.1.min(e) }
            }
        }
        let gkey = |o: usize, starts: &SpanStarts, bands: &[BandSlot]| -> GKey {
            let (kind, i) = scene.order[o];
            let (stack, e) = r.keys[o].clone();
            let at = match span.get(&(kind, i)) {
                Some(&(h, by, true)) => starts.get(&h).copied().unwrap_or_else(|| {
                    (bands.iter().position(|b| by >= b.range.0 && by < b.range.1).unwrap_or(0) as u32, 0)
                }),
                _ => (k as u32, e),
            };
            GKey { stack, at }
        };
        let (mut mine, mut cross, mut cross_ext) = (vec![], vec![], vec![]);
        for (o, &(kind, i)) in scene.order.iter().enumerate() {
            let e = extent(&scene, kind, i);
            // (A spanning box is the canvas or a box cut here: its bottom is
            // an estimate, or this run's, not the document's.)
            if let Some((_, b)) = e { if b < END && !span.contains_key(&(kind, i)) { self.bottom = self.bottom.max(b) } }
            match e {
                // Paints nothing (an empty box, a transform that scales to
                // zero): in no band, as in `index`.
                None => {}
                Some(_) if span.contains_key(&(kind, i)) => mine.push(o as u32),
                Some((a, b)) if a >= y0 && b <= y1 => mine.push(o as u32),
                Some((a, b)) => {
                    if b > y0 && a < y1 { mine.push(o as u32) }
                    cross.push(o as u32);
                    cross_ext.push((a, b));
                }
            }
        }
        if first { self.stats.cross_leaves += cross.len() as u64 }
        let keys: Vec<GKey> = if keep { mine.iter().map(|&o| gkey(o as usize, &self.span_starts, &self.bands)).collect() } else { vec![] };
        let cross_keys: Vec<GKey> = cross.iter().map(|&o| gkey(o as usize, &self.span_starts, &self.bands)).collect();
        let cross_scene = scene_of(&scene, &cross);
        // A band holding every leaf its run painted keeps the run's scene
        // itself (a one-band document is the whole layout: no copy).
        let whole_run = mine.len() == scene.order.len() && mine.iter().enumerate().all(|(i, &o)| o as usize == i);
        let band_scene = if !keep { None } else if whole_run { Some(scene) } else { Some(scene_of(&scene, &mine)) };
        let slot = BandSlot { range: (y0, y1), scene: band_scene, cross: cross_scene, cross_ext,
                              keys, cross_keys, stale: false, cross_stale: false };
        if first { self.bands.push(slot) } else {
            let old = &mut self.bands[k];
            let (sc, ks) = if keep { (slot.scene, slot.keys) } else { (old.scene.take(), std::mem::take(&mut old.keys)) };
            *old = BandSlot { scene: sc, keys: ks, ..slot };
        }
    }

    /// ★ A FAR JUMP, WITHOUT THE LAYOUT ABOVE IT. When the view wants `y`
    /// far beyond what the first pass has reached, the band for it cannot be
    /// exact yet (its y depends on everything above). This lays out
    /// `[y, y + h)` PROVISIONALLY: from the block at the fraction of the
    /// document `y` is of the estimated extent (`Prepared::jump_target`),
    /// placed at `y`, with nothing above it laid out. Its text and boxes are
    /// the document's; its y is an estimate, and a box it starts inside of
    /// paints nothing. The first pass replaces it with exact bands when it
    /// gets there (`source::DocumentSource`). `None` when `y` is not beyond
    /// the first pass, or the document is complete.
    pub fn provisional(&mut self, y: U, h: U) -> Option<((U, U), Scene)> {
        if self.complete || self.whole.is_some() { return None }
        let frontier = self.bands.last().map(|b| b.range.1).unwrap_or(0);
        if y <= frontier { return None }
        let (est, _) = self.extent();
        let f = (y as f64 / est.max(1) as f64).clamp(0.0, 1.0);
        let at_node = (f * self.prep.node_count() as f64) as usize;
        let target = self.prep.jump_target(at_node)?;
        let mut r = BandRun::new(u32::MAX);
        r.limit = Some(y + h);
        r.provisional = true;
        r.resume_path = self.prep.ancestors(target);
        r.resume_at = Some(target);
        // Every box on the way to the target starts its children at `y`.
        r.at.insert(target, y);
        for &a in &r.resume_path { r.at.insert(a, y); }
        r.vector_budget = self.checkpoints.last().map(|c| c.vector_budget).unwrap_or(crate::path::DOCUMENT_SEGMENT_SHARE);
        r.canvas_h = est;
        r.shape_cache = Some(self.shapes.clone());
        r.row_cuts = std::mem::take(&mut self.row_cuts);
        r.rows_cache = std::mem::take(&mut self.rows_cache);
        let (scene, mut r) = html::lay_out_band(&self.prep, self.fonts, &self.env, &self.subs, r);
        self.row_cuts = std::mem::take(&mut r.row_cuts);
        self.rows_cache = std::mem::take(&mut r.rows_cache);
        self.stats.provisional += 1;
        let y1 = r.fired.map(|f| f.1).unwrap_or(y + h).max(y + 1);
        let keep: Vec<u32> = scene.order.iter().enumerate()
            .filter(|(_, &(k, i))| extent(&scene, k, i).is_some_and(|(a, b)| b > y && a < y1))
            .map(|(o, _)| o as u32).collect();
        Some(((y, y1), scene_of(&scene, &keep)))
    }

    fn fall_back_whole(&mut self, keep: bool) {
        self.stats.whole = true;
        let out = html::lay_out_whole(&self.prep, self.fonts, &self.env, &self.subs);
        let s = out.scene;
        self.bands.clear();
        self.checkpoints.truncate(1);
        self.root_h = s.height;
        self.bottom = s.height;
        self.complete = true;
        self.bands.push(BandSlot { range: (TOP, END), scene: keep.then(|| scene_of(&s, &(0..s.order.len() as u32).collect::<Vec<_>>())), ..Default::default() });
        self.whole = Some(s);
    }

    /// The leaves other bands painted into band `b`, each placed in `b`'s
    /// painting order: `(position among b's own leaves, source band, index
    /// in its cross scene)`. Position `len` means after all of them.
    fn foreign(&self, b: usize) -> Vec<(usize, usize, usize)> {
        let Some(band) = self.bands.get(b) else { return vec![] };
        let mut out = vec![];
        let mut reach = self.reach.borrow_mut();
        let reach = reach.get_or_insert_with(|| self.reach_index());
        let bh = self.band_h;
        let (b0, b1) = (band.range.0.max(-(1 << 40)).div_euclid(bh), band.range.1.min(1 << 40).saturating_sub(1).div_euclid(bh));
        let mut cand: Vec<(usize, usize)> = reach.1.clone();
        if b1 - b0 < 1 << 16 { for k in b0..=b1 { if let Some(v) = reach.0.get(&k) { cand.extend_from_slice(v) } } }
        else { cand.extend(reach.0.values().flatten().copied()) }
        cand.sort_unstable();
        cand.dedup();
        for (j, n) in cand {
            if j == b { continue }
            let src = &self.bands[j];
            {
                let (a, e) = src.cross_ext[n];
                if band.range.1 <= a || band.range.0 >= e { continue }
                let key = &src.cross_keys[n];
                let pos = band.keys.iter().position(|g| gcmp(g, key) == std::cmp::Ordering::Greater).unwrap_or(band.keys.len());
                out.push((pos, j, n));
            }
        }
        out.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| gcmp(&self.bands[x.1].cross_keys[x.2], &self.bands[y.1].cross_keys[y.2])));
        out
    }

    /// Every band's overlay sources by the band-high buckets their extents
    /// touch (a leaf reaching more than 256 buckets is in the second list),
    /// so finding what reaches a band does not walk the document.
    fn reach_index(&self) -> (HashMap<i64, Vec<(usize, usize)>>, Vec<(usize, usize)>) {
        let (mut m, mut wide) = (HashMap::<i64, Vec<(usize, usize)>>::new(), vec![]);
        let bh = self.band_h;
        for (j, src) in self.bands.iter().enumerate() {
            for (n, &(a, e)) in src.cross_ext.iter().enumerate() {
                let (k0, k1) = (a.max(-(1 << 40)).div_euclid(bh), e.min(1 << 40).saturating_sub(1).div_euclid(bh));
                if k1 - k0 > 256 { wide.push((j, n)); continue }
                for k in k0..=k1 { m.entry(k).or_default().push((j, n)) }
            }
        }
        (m, wide)
    }

    /// ★ BAND `b`'S GROUP, as it is sent: every leaf its run painted there,
    /// and the leaves other bands painted into it that the whole layout
    /// paints UNDER some of them (a positioned note written above a section
    /// whose background covers it), in place. A host clips it to `range`.
    /// `None` unless resident.
    pub fn group(&self, b: usize) -> Option<Group<'_>> {
        let band = self.bands.get(b)?;
        let s = band.scene.as_ref()?;
        let foreign = self.foreign(b);
        // Nothing from other bands goes in under its leaves: the band's own
        // scene is the group, not a copy of it.
        if foreign.iter().all(|x| x.0 >= s.order.len()) { return Some(Group::Band(s)) }
        let mut out = Scene { width: s.width, height: s.height, ..Default::default() };
        let (mut memo, mut memos) = (Memo::default(), HashMap::<usize, Memo>::new());
        let mut f = foreign.iter().filter(|x| x.0 < s.order.len()).peekable();
        for (p, &(kind, i)) in s.order.iter().enumerate() {
            while let Some(&&(pos, j, n)) = f.peek() {
                if pos > p { break }
                let src = &self.bands[j].cross;
                let (fk, fi) = src.order[n];
                append(&mut out, src, fk, fi, Some(band.range), memos.entry(j).or_default());
                f.next();
            }
            append(&mut out, s, kind, i, None, &mut memo);
        }
        Some(Group::Built(out))
    }

    /// ★ THE OVERLAY GROUP: the leaves bands painted outside themselves that
    /// the whole layout paints over everything of each other band they
    /// reach (positioned and transformed content, a fixed header written
    /// last), once per RESIDENT band they reach, clipped to it — so it is
    /// bounded by the window, not the document. Sent after the bands.
    pub fn overlay(&self) -> Scene {
        let mut out = Scene { width: (self.env.width_px * crate::PX as f64) as U, ..Default::default() };
        let mut memos = HashMap::<usize, Memo>::new();
        for b in 0..self.bands.len() {
            if self.bands[b].scene.is_none() { continue }
            let len = self.bands[b].keys.len();
            for (pos, j, n) in self.foreign(b) {
                if pos < len { continue }
                let src = &self.bands[j].cross;
                let (fk, fi) = src.order[n];
                append(&mut out, src, fk, fi, Some(self.bands[b].range), memos.entry(j).or_default());
            }
        }
        out.height = self.extent().0;
        out
    }

    /// Band `b` as it is drawn: its group, then the overlay's leaves for it.
    /// For checking by pixels; `None` unless resident.
    pub fn composite(&self, b: usize) -> Option<Scene> {
        let mut out = match self.group(b)? {
            Group::Band(s) => scene_of(s, &(0..s.order.len() as u32).collect::<Vec<_>>()),
            Group::Built(s) => s,
        };
        let len = self.bands[b].keys.len();
        let mut memos = HashMap::<usize, Memo>::new();
        for (pos, j, n) in self.foreign(b) {
            if pos < len { continue }
            let src = &self.bands[j].cross;
            let (fk, fi) = src.order[n];
            append(&mut out, src, fk, fi, Some(self.bands[b].range), memos.entry(j).or_default());
        }
        out.height = self.extent().0;
        Some(out)
    }

    /// What band `b`'s group is built from besides its own leaves (to
    /// notice a change): the foreign leaves and where they go.
    pub fn foreign_sig(&self, b: usize) -> Vec<(usize, usize, usize)> { self.foreign(b) }

    /// For band `b` (resident): how many foreign leaves its group holds in
    /// place, and how many the overlay draws over it.
    pub fn foreign_counts(&self, b: usize) -> (usize, usize) {
        let len = self.bands.get(b).map(|x| x.keys.len()).unwrap_or(0);
        let f = self.foreign(b);
        let under = f.iter().filter(|x| x.0 < len).count();
        (under, f.len() - under)
    }

    /// Resident bands' leaves and the overlay's sources, for memory accounting.
    pub fn resident_leaves(&self) -> (usize, usize) {
        (self.bands.iter().filter_map(|b| b.scene.as_ref()).map(|s| s.order.len()).sum(),
         self.bands.iter().map(|b| b.cross.order.len()).sum())
    }
}

const PX_MIN: U = 64;

/// A band's group: its own scene when nothing from other bands goes in
/// under its leaves (no copy), else one built with them in place.
pub enum Group<'a> { Band(&'a Scene), Built(Scene) }

impl std::ops::Deref for Group<'_> {
    type Target = Scene;
    fn deref(&self) -> &Scene { match self { Group::Band(s) => s, Group::Built(s) => s } }
}

/// Group ids already copied from one source scene.
#[derive(Default)]
struct Memo { groups: HashMap<u32, u32>, xforms: HashMap<u32, u32>, clips: HashMap<(u32, U, U), u32> }

/// Copy leaf `(kind, i)` of `src` to the end of `dst`'s draw order, its
/// clip, opacity group (with its parents) and transform with it; clipped
/// vertically to `clip_y` when given.
fn append(dst: &mut Scene, src: &Scene, kind: u8, i: usize, clip_y: Option<(U, U)>, memo: &mut Memo) {
    let attrs = match kind {
        0 => src.rect_attrs[i], 1 => src.run_attrs[i], 2 => src.link_attrs[i], 3 => src.shadow_attrs[i],
        4 => src.grad_attrs[i], 5 => src.image_attrs[i], 6 => src.path_attrs[i], 7 => src.embed_attrs[i], _ => return,
    };
    let clip = match (attrs.0, clip_y) {
        (None, None) => None,
        (c, y) => {
            let key = (c.unwrap_or(u32::MAX), y.map(|v| v.0).unwrap_or(0), y.map(|v| v.1).unwrap_or(0));
            Some(*memo.clips.entry(key).or_insert_with(|| {
                let (mut x0, mut y0, mut w, mut h) = c.and_then(|c| src.clips.get(c as usize).copied()).unwrap_or((TOP, TOP, END - TOP, END - TOP));
                if let Some((a, b)) = y {
                    let (t, bt) = (y0.max(a), (y0 + h).min(b));
                    y0 = t; h = (bt - t).max(0);
                }
                x0 = x0.max(TOP); w = w.max(0);
                dst.clips.push((x0, y0, w, h));
                dst.clips.len() as u32 - 1
            }))
        }
    };
    let group = attrs.1.map(|g| copy_group(dst, src, g, memo));
    let xf = attrs.2.map(|x| *memo.xforms.entry(x).or_insert_with(|| {
        dst.xforms.push(src.xforms[x as usize]);
        dst.xform_parents.push(None);
        dst.xforms.len() as u32 - 1
    }));
    let a = (clip, group, xf);
    match kind {
        0 => { dst.order.push((0, dst.rects.len())); dst.rects.push(src.rects[i].clone()); dst.rect_attrs.push(a) }
        1 => { dst.order.push((1, dst.runs.len())); dst.runs.push(src.runs[i].clone()); dst.run_attrs.push(a) }
        2 => { dst.order.push((2, dst.links.len())); dst.links.push(src.links[i].clone()); dst.link_attrs.push(a) }
        3 => { dst.order.push((3, dst.shadows.len())); dst.shadows.push(src.shadows[i].clone()); dst.shadow_attrs.push(a) }
        4 => { dst.order.push((4, dst.grads.len())); dst.grads.push(src.grads[i].clone()); dst.grad_attrs.push(a) }
        5 => { dst.order.push((5, dst.images.len())); dst.images.push(src.images[i].clone()); dst.image_attrs.push(a) }
        6 => { dst.order.push((6, dst.paths.len())); dst.paths.push(src.paths[i].clone()); dst.path_attrs.push(a) }
        _ => { dst.order.push((7, dst.embeds.len())); dst.embeds.push(src.embeds[i].clone()); dst.embed_attrs.push(a) }
    }
}

fn copy_group(dst: &mut Scene, src: &Scene, g: u32, memo: &mut Memo) -> u32 {
    if let Some(&d) = memo.groups.get(&g) { return d }
    let (alpha, parent) = src.groups[g as usize];
    let p = parent.map(|p| copy_group(dst, src, p, memo));
    dst.groups.push((alpha, p));
    let d = dst.groups.len() as u32 - 1;
    memo.groups.insert(g, d);
    d
}

/// `scene` moved up by `dy` (1/64 px), every leaf, clip and transform, so
/// rows `[dy, dy + h)` can be painted on a canvas `h` tall (checking only).
pub fn translated(scene: &Scene, dy: U) -> Scene {
    let mut s = scene_of(scene, &(0..scene.order.len() as u32).collect::<Vec<_>>());
    s.height = scene.height - dy;
    let moved = |a: &crate::Attrs| a.2.is_some();
    for (r, a) in s.rects.iter_mut().zip(&s.rect_attrs) { if !moved(a) { r.y -= dy } }
    for (r, a) in s.runs.iter_mut().zip(&s.run_attrs) { if !moved(a) { r.y -= dy } }
    for (l, a) in s.links.iter_mut().zip(&s.link_attrs) { if !moved(a) { l.y -= dy } }
    for (l, a) in s.shadows.iter_mut().zip(&s.shadow_attrs) { if !moved(a) { l.y -= dy } }
    for (g, a) in s.grads.iter_mut().zip(&s.grad_attrs) { if !moved(a) { g.area.y -= dy; g.area.ty -= dy } }
    for (g, a) in s.images.iter_mut().zip(&s.image_attrs) { if !moved(a) { g.area.y -= dy; g.area.ty -= dy } }
    for (e, a) in s.embeds.iter_mut().zip(&s.embed_attrs) { if !moved(a) { e.y -= dy } }
    for p in s.paths.iter_mut() {
        p.transform[5] -= dy;
        if let Some(c) = p.clip.as_mut() { c[1] -= dy; c[3] -= dy }
    }
    for c in s.clips.iter_mut() { if c.1 > TOP { c.1 -= dy } }
    // A transformed leaf keeps its own coordinates; its matrix moves up.
    for m in s.xforms.iter_mut() { m[5] -= dy }
    for sc in s.scrolls.iter_mut() { sc.content.1 -= dy }
    s
}
