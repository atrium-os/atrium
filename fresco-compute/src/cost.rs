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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Rect, Scene};

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
