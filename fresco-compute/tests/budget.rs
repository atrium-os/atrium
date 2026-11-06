//! The op budget: a frame past `MAX_OPS` ops packs within it, culling
//! first, then dropping whole nodes from the one that crosses the cap.

use fresco_compute::fill::drop;
use fresco_compute::*;

const W: u32 = 64;

fn rect(x: f32, y: f32, c: f32) -> Rect {
    Rect { position: [x, y], size: [3.0, 2.0], color: [c, 0.5, 0.2, 1.0], radius: 0.0 }
}

fn run(x: f32, glyphs: usize) -> GlyphRun {
    let g = Glyph { d_offset: [0.0, 0.0], atlas_uv: [0.0, 0.0, 4.0, 4.0], bearing: [0.0, 4.0] };
    GlyphRun { origin: [x, 20.0], color: [0.0, 0.0, 0.0, 1.0], glyphs: vec![g; glyphs] }
}

fn atlas() -> Texture {
    Texture { width: 8, height: 8, format: TexFormat::R8, data: vec![255; 64] }
}

/// On-screen rects, `off` of them off the target in between.
fn rects(on: usize, off: usize) -> Vec<Rect> {
    let mut v: Vec<Rect> = (0..on).map(|i| rect((i % 60) as f32, ((i / 60) % 60) as f32, (i % 7) as f32 / 7.0)).collect();
    for k in 0..off {
        v.insert(k * 2 + 1, rect(-500.0 - k as f32, 10.0, 1.0));
    }
    v
}

#[test]
fn past_the_cap_whole_nodes_drop_from_the_crossing_one() {
    let cap = MAX_OPS as usize;
    let scene = Scene {
        // 100 off the target: culled first, never reported.
        rects: rects(cap - 6, 100),
        textures: vec![atlas()],
        // Run 0 holds ops cap-6 .. cap+4: it crosses, so it and run 1 go.
        glyph_batches: vec![GlyphBatch { atlas: 0, runs: vec![run(5.0, 10), run(9.0, 3)] }],
        ..Default::default()
    };
    let (f, dropped) = Frame::pack_report(&scene, W, W, W);
    assert_eq!(f.header.op_count as usize, cap - 6, "the rects that are on the target, and no glyph");
    assert!(f.ops[..cap - 6].iter().all(|o| o.meta[0] == KIND_RECT && o.p3[0] >= 0.0));
    assert_eq!(dropped, vec![(Node::Run(0, 0), drop::OP_BUDGET), (Node::Run(0, 1), drop::OP_BUDGET)]);
    f.validate(f.target_words()).expect("a frame past the cap now validates");
    // It draws what the on-target rects alone draw.
    let alone = Frame::pack(&Scene { rects: rects(cap - 6, 0), ..Default::default() }, W, W, W);
    assert_eq!(reference::render(&f), reference::render(&alone));
}

#[test]
fn culling_alone_can_bring_a_frame_within_the_cap() {
    let cap = MAX_OPS as usize;
    let scene = Scene {
        rects: rects(cap - 20, 500),
        textures: vec![atlas()],
        glyph_batches: vec![GlyphBatch { atlas: 0, runs: vec![run(5.0, 10)] }],
        ..Default::default()
    };
    let (f, dropped) = Frame::pack_report(&scene, W, W, W);
    assert!(dropped.is_empty(), "{dropped:?}");
    assert_eq!(f.header.op_count as usize, cap - 10);
    f.validate(f.target_words()).unwrap();
}

#[test]
fn within_the_cap_nothing_is_culled() {
    // Frames within the cap pack exactly as before, off-target ops and all.
    let scene = Scene { rects: rects(50, 10), ..Default::default() };
    let (f, dropped) = Frame::pack_report(&scene, W, W, W);
    assert!(dropped.is_empty());
    assert_eq!(f.header.op_count, 60);
}

#[test]
fn a_fill_past_the_cap_is_reported_as_its_index() {
    let cap = MAX_OPS as usize;
    let tri = fill::Fill {
        verbs: vec![fill::VERB_MOVE, fill::VERB_LINE, fill::VERB_LINE, fill::VERB_CLOSE],
        points: vec![[1.0, 1.0], [30.0, 2.0], [10.0, 30.0]],
        transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        clip: None,
        gradient: None,
        ..fuzz::fills()[0].clone()
    };
    let scene = Scene { rects: rects(cap, 0), fills: vec![tri.clone(), tri], ..Default::default() };
    let (f, dropped) = Frame::pack_report(&scene, W, W, W);
    assert_eq!(f.header.op_count as usize, cap);
    assert_eq!(dropped, vec![(Node::Fill(0), drop::OP_BUDGET), (Node::Fill(1), drop::OP_BUDGET)]);
}
