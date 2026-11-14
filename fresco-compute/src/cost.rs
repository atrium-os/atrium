//! A frame's raster cost, estimated before it runs (scene-graph hardening
//! H6; officina.md §10.5; fresco-f4-split.md §F4j).
//!
//! The raster kernel walks, for every pixel of a dirty tile, the ops binned
//! to that tile. So a frame costs about the sum, over its ops, of the dirty
//! pixels each op's tiles cover, weighted by what the op does per pixel,
//! plus the mask scan. The estimate is O(ops x rects) on the CPU, from the
//! packed frame alone (op boxes are `Op::p3`), before anything is
//! submitted. Units are "pixel-ops": one solid rect over one pixel.
//!
//! [`FAR_OVER`] is the ceiling past which a frame is not drawn at all (the
//! window keeps its last frame and its client is told, `budget::RASTER_TIME`).
//! It is deliberately far above any legitimate content: legitimate content
//! can be expensive, so slowness below it is handled by never waiting on a
//! window's raster (compose shows its last finished surface), not refused.
//! The number is provisional until measured on the R9700.

use crate::{Frame, KIND_FILL, KIND_POP, KIND_PUSH, KIND_TEXTURE, TILE};

/// Frames estimated above this many pixel-ops are not drawn. About 580
/// opaque full-window layers at 2560x1440 (or 145 full-window fills): no
/// document, sheet, deck or page comes near it; 65,536 overlapping
/// full-window translucent fills is ~2,000x over.
pub const FAR_OVER: u64 = 1 << 31;

/// What one op costs per pixel, relative to a solid rect.
pub fn weight(kind: u32) -> u64 {
    match kind {
        KIND_FILL => 4,
        KIND_TEXTURE | KIND_PUSH | KIND_POP => 2,
        _ => 1,
    }
}

/// The estimated cost of rastering `tiles` (tile rects `[tx0, ty0, tx1,
/// ty1)`, as `damage::rects` gives them) of `f`. Empty `tiles`: nothing.
pub fn estimate(f: &Frame, tiles: &[[u32; 4]]) -> u64 {
    let h = &f.header;
    let px: Vec<[u64; 4]> = tiles
        .iter()
        .map(|r| {
            [
                (r[0] * TILE).min(h.width) as u64,
                (r[1] * TILE).min(h.height) as u64,
                (r[2] * TILE).min(h.width) as u64,
                (r[3] * TILE).min(h.height) as u64,
            ]
        })
        .filter(|r| r[0] < r[2] && r[1] < r[3])
        .collect();
    let area = |r: &[u64; 4]| (r[2] - r[0]) * (r[3] - r[1]);
    // Every pixel of a dirty tile scans the tile's mask, 32 ops a word
    // (a word costs about a 32nd of an op drawn: even 65,536 ops binned to
    // every tile of a 1440p window stay at a ninth of `FAR_OVER`).
    let mut cost: u64 = px.iter().map(|r| area(r) * h.wpt as u64 / 32).sum();
    let t = TILE as f32;
    for op in &f.ops {
        let b = op.p3;
        if !(b[0] < b[2] && b[1] < b[3]) {
            continue;
        }
        // The op's box, out to its tiles (the kernel works per tile).
        let ob = [
            ((b[0].max(0.0) / t).floor() * t) as u64,
            ((b[1].max(0.0) / t).floor() * t) as u64,
            ((b[2].max(0.0) / t).ceil() * t) as u64,
            ((b[3].max(0.0) / t).ceil() * t) as u64,
        ];
        let w = weight(op.meta[0]);
        for r in &px {
            let x0 = ob[0].max(r[0]);
            let y0 = ob[1].max(r[1]);
            let x1 = ob[2].min(r[2]);
            let y1 = ob[3].min(r[3]);
            if x0 < x1 && y0 < y1 {
                cost = cost.saturating_add(w * (x1 - x0) * (y1 - y0));
            }
        }
    }
    cost
}

/// What binning one op into one tile costs, in pixel-ops (one mask bit
/// set; provisional, like [`FAR_OVER`]).
pub const BIN_TILE_COST: u64 = 4;

/// One submit's share of a frame's raster ([`split`]): bin the op
/// workgroups `bin` (`(first, count)`), then raster `tiles`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub bin: Option<(u32, u32)>,
    pub tiles: Vec<[u32; 4]>,
    /// Its estimated cost, pixel-ops.
    pub cost: u64,
}

/// Split rastering `tiles` of `f` into slices of about `budget` pixel-ops
/// each, in the order they must run: bin parts by op workgroups, then
/// raster parts by tile rows, a row cut into column pieces when it alone
/// is over `budget`. A unit (one bin workgroup, one tile) is never cut, so
/// a slice can be over `budget` by one unit. Small frames are one slice.
///
/// This is what keeps one window's frame from holding the GPU
/// (fresco-f4-split.md §F4j): frescod submits a window's slices one at a
/// time, the rest of the screen's work in between, and shows the window's
/// new drawing only once its last slice is done.
pub fn split(f: &Frame, tiles: &[[u32; 4]], budget: u64) -> Vec<Slice> {
    split_with(f, tiles, budget, true)
}

/// [`split`] for the raster part only (no bin slices): what is left of a
/// frame whose bin parts have run, cut again when the budget has shrunk
/// since it was first split (frescod learns the GPU's speed as it goes).
pub fn split_raster(f: &Frame, tiles: &[[u32; 4]], budget: u64) -> Vec<Slice> {
    split_with(f, tiles, budget, false)
}

fn split_with(f: &Frame, tiles: &[[u32; 4]], budget: u64, bin: bool) -> Vec<Slice> {
    let budget = budget.max(1);
    let h = &f.header;
    let t = TILE as f32;
    // Units, in order: (bin group or tile rect, cost).
    enum Unit {
        Bin(u32),
        Tiles([u32; 4]),
    }
    let mut units: Vec<(Unit, u64)> = Vec::new();
    // Bin: each workgroup of BIN_LOCAL ops, by the tiles their boxes cover.
    let groups = if bin { h.bin_groups()[0] } else { 0 };
    for g in 0..groups {
        let lo = (g * crate::BIN_LOCAL) as usize;
        let hi = (lo + crate::BIN_LOCAL as usize).min(f.ops.len());
        let mut c = 0u64;
        for op in f.ops.get(lo..hi).unwrap_or(&[]) {
            let b = op.p3;
            if !(b[0] < b[2] && b[1] < b[3]) {
                continue;
            }
            let tx = ((b[2].min(h.width as f32) / t).ceil() - (b[0].max(0.0) / t).floor()).max(0.0) as u64;
            let ty = ((b[3].min(h.height as f32) / t).ceil() - (b[1].max(0.0) / t).floor()).max(0.0) as u64;
            c = c.saturating_add(tx * ty * BIN_TILE_COST);
        }
        units.push((Unit::Bin(g), c.max(1)));
    }
    // Raster: cost per tile row (ops' tile boxes overlapping each dirty
    // rect, as `estimate`, spread over rows with a difference array).
    let rows = h.tiles_y as usize;
    let mut row_cost = vec![0i128; rows + 1];
    let clip = |r: &[u32; 4]| [r[0].min(h.tiles_x), r[1].min(h.tiles_y), r[2].min(h.tiles_x), r[3].min(h.tiles_y)];
    let rects: Vec<[u32; 4]> = tiles.iter().map(clip).filter(|r| r[0] < r[2] && r[1] < r[3]).collect();
    for r in &rects {
        // The mask scan: every pixel of the rect, a word per 32 ops.
        let per_row = ((r[2] - r[0]) * TILE) as i128 * TILE as i128 * h.wpt as i128 / 32;
        row_cost[r[1] as usize] += per_row;
        row_cost[r[3] as usize] -= per_row;
    }
    for op in &f.ops {
        let b = op.p3;
        if !(b[0] < b[2] && b[1] < b[3]) {
            continue;
        }
        let ob = [
            (b[0].max(0.0) / t).floor() as u32,
            (b[1].max(0.0) / t).floor() as u32,
            (b[2].max(0.0) / t).ceil() as u32,
            (b[3].max(0.0) / t).ceil() as u32,
        ];
        let w = weight(op.meta[0]) as i128;
        for r in &rects {
            let (x0, x1) = (ob[0].max(r[0]), ob[2].min(r[2]));
            let (y0, y1) = (ob[1].max(r[1]), ob[3].min(r[3]));
            if x0 < x1 && y0 < y1 {
                let per_row = w * ((x1 - x0) * TILE) as i128 * TILE as i128;
                row_cost[y0 as usize] += per_row;
                row_cost[y1 as usize] -= per_row;
            }
        }
    }
    let mut acc = 0i128;
    for y in 0..rows as u32 {
        acc += row_cost[y as usize];
        let bands: Vec<[u32; 4]> = rects.iter().filter(|r| r[1] <= y && y < r[3]).map(|r| [r[0], y, r[2], y + 1]).collect();
        if bands.is_empty() {
            continue;
        }
        let cost = acc.max(1) as u64;
        let width: u64 = bands.iter().map(|b| (b[2] - b[0]) as u64).sum();
        // A row over budget goes in pieces of about `budget`, by tile columns.
        let pieces = cost.div_ceil(budget).min(width).max(1);
        let cols = width.div_ceil(pieces) as u32;
        for b in bands {
            let mut x = b[0];
            while x < b[2] {
                let x1 = (x + cols).min(b[2]);
                units.push((Unit::Tiles([x, y, x1, y + 1]), (cost * (x1 - x) as u64 / width).max(1)));
                x = x1;
            }
        }
    }
    // Pack units in order into slices of at most `budget` (or one unit).
    let mut out: Vec<Slice> = Vec::new();
    let mut cur = Slice { bin: None, tiles: Vec::new(), cost: 0 };
    let empty = |s: &Slice| s.bin.is_none() && s.tiles.is_empty();
    for (u, c) in units {
        if !empty(&cur) && cur.cost.saturating_add(c) > budget {
            out.push(std::mem::replace(&mut cur, Slice { bin: None, tiles: Vec::new(), cost: 0 }));
        }
        cur.cost = cur.cost.saturating_add(c);
        match u {
            Unit::Bin(g) => {
                cur.bin = Some(match cur.bin {
                    Some((first, n)) => (first, n + 1),
                    None => (g, 1),
                })
            }
            Unit::Tiles(r) => {
                // A row piece under the same piece of the row above: grow it.
                match cur.tiles.iter_mut().find(|p| p[0] == r[0] && p[2] == r[2] && p[3] == r[1]) {
                    Some(p) => p[3] = r[3],
                    None => cur.tiles.push(r),
                }
            }
        }
    }
    if !empty(&cur) {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Rect, Scene};

    #[test]
    fn the_raster_rest_splits_again_without_bins() {
        let f = Frame::pack(&rects(200, 1024.0, 1024.0, 0.5), 1024, 1024, 1024);
        let tiles = [[0, 0, f.header.tiles_x, f.header.tiles_y]];
        let whole: u64 = split(&f, &tiles, FAR_OVER).iter().map(|s| s.cost).sum();
        let s = split_raster(&f, &tiles, whole / 40);
        assert!(s.len() >= 20, "{} slices", s.len());
        assert!(s.iter().all(|x| x.bin.is_none() && !x.tiles.is_empty()));
        // Every tile once.
        let mut n = 0u64;
        for x in &s {
            for r in &x.tiles {
                n += ((r[2] - r[0]) * (r[3] - r[1])) as u64;
            }
        }
        assert_eq!(n, (f.header.tiles_x * f.header.tiles_y) as u64);
    }

    fn all_tiles(f: &Frame) -> Vec<[u32; 4]> {
        vec![[0, 0, f.header.tiles_x, f.header.tiles_y]]
    }

    fn rects(n: usize, w: f32, h: f32, a: f32) -> Scene {
        Scene {
            rects: (0..n).map(|_| Rect { position: [0.0, 0.0], size: [w, h], color: [1.0, 0.0, 0.0, a], radius: 0.0 }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_document_is_far_under_and_a_hostile_frame_far_over() {
        let (w, h) = (2560, 1440);
        // A document window: a background and 5,000 glyph-sized rects.
        let mut doc = rects(1, w as f32, h as f32, 1.0);
        for i in 0..5000 {
            let (x, y) = ((i % 200) as f32 * 12.0, (i / 200) as f32 * 20.0);
            doc.rects.push(Rect { position: [x, y], size: [9.0, 14.0], color: [0.0, 0.0, 0.0, 1.0], radius: 0.0 });
        }
        let f = Frame::pack(&doc, w, h, w);
        let c = estimate(&f, &all_tiles(&f));
        assert!(c < FAR_OVER / 50, "document {c}");
        // Thousands of overlapping full-window translucent layers.
        let f = Frame::pack(&rects(4096, w as f32, h as f32, 0.1), w, h, w);
        let c = estimate(&f, &all_tiles(&f));
        assert!(c > FAR_OVER * 4, "hostile {c}");
    }

    #[test]
    fn a_small_frame_is_one_slice_with_all_its_work() {
        let f = Frame::pack(&rects(3, 100.0, 100.0, 0.5), 256, 256, 256);
        let tiles = all_tiles(&f);
        let s = split(&f, &tiles, FAR_OVER);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].bin, Some((0, f.header.bin_groups()[0])));
        assert_eq!(s[0].tiles, tiles);
    }

    #[test]
    fn a_heavy_frame_splits_into_slices_that_cover_every_tile_once() {
        let (w, h) = (1280u32, 800u32);
        let f = Frame::pack(&rects(4096, w as f32, h as f32, 0.02), w, h, w);
        let tiles = all_tiles(&f);
        let total = estimate(&f, &tiles);
        let budget = total / 200;
        let s = split(&f, &tiles, budget);
        assert!(s.len() >= 150, "{} slices", s.len());
        // Every bin group once, in order, before any raster part.
        let mut next = 0;
        let mut rastered = false;
        for x in &s {
            if let Some((first, n)) = x.bin {
                assert!(!rastered && first == next);
                next += n;
            }
            rastered |= !x.tiles.is_empty();
        }
        assert_eq!(next, f.header.bin_groups()[0]);
        // Every tile exactly once.
        let mut seen = vec![0u8; (f.header.tiles_x * f.header.tiles_y) as usize];
        for x in &s {
            for r in &x.tiles {
                for ty in r[1]..r[3] {
                    for tx in r[0]..r[2] {
                        seen[(ty * f.header.tiles_x + tx) as usize] += 1;
                    }
                }
            }
        }
        assert!(seen.iter().all(|&n| n == 1));
        // Slices keep near the budget (one unit over at most).
        let biggest = s.iter().map(|x| x.cost).max().unwrap();
        assert!(biggest <= budget * 2, "{biggest} vs {budget}");
        // The raster parts add up to about the estimate.
        let sum: u64 = s.iter().map(|x| x.cost).sum();
        assert!(sum >= total / 2 && sum <= total * 2, "{sum} vs {total}");
    }

    #[test]
    fn only_dirty_tiles_count() {
        let f = Frame::pack(&rects(100, 1024.0, 1024.0, 0.5), 1024, 1024, 1024);
        let whole = estimate(&f, &all_tiles(&f));
        let corner = estimate(&f, &[[0, 0, 1, 1]]);
        assert!(corner * 1000 < whole, "{corner} vs {whole}");
        assert_eq!(estimate(&f, &[]), 0);
        // About 100 ops x 1024^2 pixels, plus the mask scan.
        assert!((100 << 20..=110 << 20).contains(&whole), "{whole}");
    }
}
