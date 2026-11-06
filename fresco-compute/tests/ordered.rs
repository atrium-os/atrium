//! `Frame::pack_ordered` (fresco-scene-groups.md §3): the ordered entry
//! point draws what the by-kind one draws when given the by-kind order,
//! interleaves kinds, clips, and keeps to the op budget.

use fresco_compute::fuzz::Rng;
use fresco_compute::*;

/// Every element of `s` in the by-kind order `Frame::pack` draws.
fn kind_order(s: &Scene) -> Vec<Placed> {
    let mut o = Vec::new();
    o.extend((0..s.rects.len()).map(|i| Placed { item: Node::Rect(i), clip: None }));
    o.extend((0..s.paths.len()).map(|i| Placed { item: Node::Path(i), clip: None }));
    o.extend((0..s.fills.len()).map(|i| Placed { item: Node::Fill(i), clip: None }));
    for (b, tb) in s.texture_batches.iter().enumerate() {
        o.extend((0..tb.quads.len()).map(|q| Placed { item: Node::Quad(b, q), clip: None }));
    }
    for (b, gb) in s.glyph_batches.iter().enumerate() {
        o.extend((0..gb.runs.len()).map(|r| Placed { item: Node::Run(b, r), clip: None }));
    }
    o
}

fn same(a: &Frame, b: &Frame) -> bool {
    a.ops == b.ops && a.tex == b.tex && a.texels == b.texels && a.lut == b.lut
        && format!("{:?}", a.header) == format!("{:?}", b.header)
}

#[test]
fn by_kind_order_packs_the_same_bytes() {
    let mut r = Rng(0x5eed_0001);
    for _ in 0..300 {
        let (w, h) = (1 + r.below(96), 1 + r.below(80));
        let s = fuzz::scene(&mut r, w, h);
        let (a, da) = Frame::pack_report(&s, w, h, w);
        let (b, db) = Frame::pack_ordered(&s, &kind_order(&s), w, h, w, None);
        assert!(same(&a, &b), "ordered pack differs from the by-kind pack");
        // The same nodes dropped, for the same reasons.
        let (mut da, mut db) = (da, db);
        da.sort();
        db.sort();
        assert_eq!(da, db);
    }
}

fn red() -> [f32; 4] { [1.0, 0.0, 0.0, 1.0] }
fn blue() -> [f32; 4] { [0.0, 0.0, 1.0, 1.0] }

fn rect(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> Rect {
    Rect { position: [x, y], size: [w, h], color, radius: 0.0 }
}

#[test]
fn kinds_interleave_in_the_order_given() {
    // A path (blue) then a rect (red) over it: by kind the rect would be
    // under the path; in this order it covers it.
    let s = Scene {
        rects: vec![rect(0.0, 0.0, 32.0, 32.0, red())],
        paths: vec![Path { model: [16.0, 16.0, 32.0, 8.0], angle: 0.0, color: blue() }],
        ..Default::default()
    };
    let by_kind = reference::render(&Frame::pack(&s, 32, 32, 32));
    let order = [Placed { item: Node::Path(0), clip: None }, Placed { item: Node::Rect(0), clip: None }];
    let ordered = reference::render(&Frame::pack_ordered(&s, &order, 32, 32, 32, None).0);
    let at = |px: &[u32]| px[16 * 32 + 16];
    assert_ne!(at(&by_kind), at(&ordered));
    // In the given order the centre is the rect's colour everywhere.
    assert!(ordered.iter().all(|&p| p == ordered[0]));
}

#[test]
fn a_clip_bounds_every_kind() {
    let s = Scene { rects: vec![rect(0.0, 0.0, 64.0, 64.0, red())], ..Default::default() };
    let order = [Placed { item: Node::Rect(0), clip: Some([8.0, 8.0, 24.5, 40.0]) }];
    let (f, _) = Frame::pack_ordered(&s, &order, 64, 64, 64, None);
    let px = reference::render(&f);
    let clear = reference::render(&Frame::pack(&Scene::default(), 64, 64, 64))[0];
    for y in 0..64u32 {
        for x in 0..64u32 {
            // Rounded out to whole pixels: [8, 25) x [8, 40).
            let inside = (8..25).contains(&x) && (8..40).contains(&y);
            assert_eq!(px[(y * 64 + x) as usize] != clear, inside, "pixel {x},{y}");
        }
    }
    // Wholly outside: culled, not drawn and not reported.
    let order = [Placed { item: Node::Rect(0), clip: Some([100.0, 100.0, 120.0, 120.0]) }];
    let (f, d) = Frame::pack_ordered(&s, &order, 64, 64, 64, None);
    assert_eq!(f.header.op_count, 0);
    assert!(d.is_empty());
}

#[test]
fn past_the_op_budget_an_element_is_dropped_and_reported() {
    let n = MAX_OPS as usize + 3;
    let s = Scene { rects: (0..n).map(|i| rect((i % 30) as f32, 0.0, 1.0, 1.0, red())).collect(), ..Default::default() };
    let order: Vec<Placed> = (0..n).map(|i| Placed { item: Node::Rect(i), clip: None }).collect();
    let (f, d) = Frame::pack_ordered(&s, &order, 32, 32, 32, None);
    assert_eq!(f.header.op_count, MAX_OPS);
    assert!(f.validate(f.target_words()).is_ok());
    assert_eq!(d, (MAX_OPS as usize..n).map(|i| (Node::Rect(i), fill::drop::OP_BUDGET)).collect::<Vec<_>>());
}

#[test]
fn an_element_not_named_is_not_drawn() {
    let s = Scene { rects: vec![rect(0.0, 0.0, 8.0, 8.0, red()), rect(0.0, 0.0, 8.0, 8.0, blue())], ..Default::default() };
    let (f, _) = Frame::pack_ordered(&s, &[Placed { item: Node::Rect(1), clip: None }], 8, 8, 8, None);
    assert_eq!(f.header.op_count, 1);
    assert_eq!(f.ops[0].p1, blue());
}
