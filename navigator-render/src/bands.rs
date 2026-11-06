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
            _ => continue,
        };
        out.order.push((k, at));
    }
    out
}
