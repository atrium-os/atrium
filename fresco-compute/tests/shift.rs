//! Scrolling by shifting (fresco-scene-groups.md §4): moving a scrolled
//! area's pixels and rastering only the rest gives exactly the pixels of a
//! full draw.

use fresco_compute::damage::{self, Shift};
use fresco_compute::*;

const W: u32 = 256;
const H: u32 = 200;
/// The viewport: an opaque background exactly there, content under it.
const AREA: [u32; 4] = [16, 24, 240, 184];

fn scene(off: [f32; 2], over: bool) -> Scene {
    let mut r = fuzz::Rng(11);
    let mut s = Scene::default();
    // Something under the viewport, hidden by its background.
    s.rects.push(Rect { position: [0.0, 0.0], size: [W as f32, H as f32], color: [0.2, 0.3, 0.9, 1.0], radius: 0.0 });
    let [x0, y0, x1, y1] = AREA.map(|v| v as f32);
    s.rects.push(Rect { position: [x0, y0], size: [x1 - x0, y1 - y0], color: [0.95, 0.95, 0.9, 1.0], radius: 0.0 });
    // A long column of rows, rounded corners, fractional positions.
    for i in 0..120 {
        let (x, y) = (r.f(0.0, 150.0) + 0.3, i as f32 * 9.25);
        s.rects.push(Rect {
            position: [x0 + x + off[0], y0 + y + off[1]],
            size: [r.f(10.0, 80.0), 7.5],
            color: r.color(),
            radius: r.f(0.0, 3.0),
        });
    }
    // Content clipped to the viewport the way pack clips it: through p3.
    let content = 2..s.rects.len();
    let (mut f_rects, rest) = (s.rects[..2].to_vec(), s.rects[content].to_vec());
    f_rects.extend(rest.into_iter().filter(|q| {
        q.position[1] + q.size[1] > y0 && q.position[1] < y1 && q.position[0] < x1 && q.position[0] + q.size[0] > x0
    }));
    s.rects = f_rects;
    if over {
        // A floating button over the list, drawn after it.
        s.paths.push(Path { model: [200.0, 150.0, 30.0, 14.0], angle: 0.4, color: [0.1, 0.6, 0.2, 0.9] });
    }
    s
}

/// Pack with every content op's box narrowed to the viewport, as the
/// ordered pack does for a clipped group.
fn frame(off: [f32; 2], over: bool) -> Frame {
    let s = scene(off, over);
    let n = s.rects.len();
    let mut order: Vec<Placed> = (0..n).map(|i| Placed {
        item: Node::Rect(i),
        clip: (i >= 2).then_some(AREA.map(|v| v as f32)),
    }).collect();
    if over {
        order.push(Placed { item: Node::Path(0), clip: None });
    }
    Frame::pack_ordered(&s, &order, W, H, W, None).0
}

const OVER: [u32; 4] = [176, 133, 224, 167];

#[test]
fn shifted_frames_equal_full_draws() {
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2");
    let mut px = vec![0u32; (W * H) as usize];
    let mut off = [0.5f32, 0.0];
    let first = frame(off, true);
    t2.render(&first, &mut px).unwrap();
    let mut held = damage::tile_signatures(&first);
    let total = held.len();
    for d in [[0, -9], [0, -37], [0, 12], [3, -1], [0, -150], [-5, 40], [0, -1]] {
        let next = [off[0] + d[0] as f32, off[1] + d[1] as f32];
        let f = frame(next, true);
        let sig = damage::tile_signatures(&f);
        let shift = Shift { area: AREA, d, over: vec![OVER], over_before: vec![OVER] };
        let out = damage::shift(&mut px, W, H, &sig, &mut held, &[shift]);
        let dirty = damage::dirty(&sig, Some(&held));
        let n_dirty = dirty.iter().filter(|&&b| b).count()
            + out.pixels.iter().map(|r| ((r[2] - r[0]) * (r[3] - r[1])) as usize).sum::<usize>() / (TILE * TILE) as usize;
        let moved = out.changed;
        let tiles = damage::rects(&dirty, W.div_ceil(TILE), H.div_ceil(TILE), usize::MAX);
        t2.render_rects(&f, &mut px, &tiles, &out.pixels).unwrap();
        held = sig;
        let mut want = vec![0u32; px.len()];
        t2.render(&f, &mut want).unwrap();
        let bad = px.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(bad, 0, "shift {d:?}: {bad} pixels differ from a full draw");
        if d[1].abs() < 100 {
            assert!(n_dirty * 2 < total, "shift {d:?}: {n_dirty} of {total} tiles rastered");
            assert!(moved.is_some());
        }
        off = next;
    }
}

#[test]
fn what_is_left_to_draw_is_the_strip_the_over_box_and_the_edges() {
    let (w, h) = (64u32, 64u32);
    let mut px: Vec<u32> = (0..w * h).collect();
    let n = (w.div_ceil(TILE) * h.div_ceil(TILE)) as usize;
    let sig = vec![7u64; n];
    let mut held = vec![1u64; n];
    let s = Shift { area: [4, 16, 64, 64], d: [0, -16], over: vec![[0, 50, 10, 52]], over_before: vec![] };
    let out = damage::shift(&mut px, w, h, &sig, &mut held, &[s]);
    assert_eq!(out.changed, Some([0, 16, 64, 64]));
    // Rows 16..48 moved up by 16 inside the area; outside it, untouched.
    assert_eq!(px[0], 0);
    assert_eq!(px[(16 * w + 4) as usize], 32 * w + 4);
    assert_eq!(px[(16 * w + 3) as usize], 16 * w + 3);
    let mut want = vec![
        [4, 48, 64, 64], // the exposed strip
        [4, 50, 10, 52], // the floating box, inside the area
        [0, 16, 4, 64],  // the edge tiles' part outside the area
    ];
    let mut got = out.pixels.clone();
    want.sort();
    got.sort();
    assert_eq!(got, want);
    assert!(held[4..].iter().all(|&v| v == 7), "every tile the area touches holds the new frame");
    assert!(held[..4].iter().all(|&v| v == 1), "outside: left to signatures");
}

/// `cargo test --release --test shift -- --ignored --nocapture`: the scroll
/// demo's window (atrium-scroll-demo), 7 px steps: tiles rastered and time,
/// shifting against drawing the viewport in full.
#[test]
#[ignore]
fn scroll_demo_cost() {
    use std::time::Instant;
    let (w, h) = (900u32, 600u32);
    let view = [40.0f32, 40.0, 860.0, 560.0];
    let area = [40u32, 40, 860, 560];
    let build = |off: f32| {
        let mut s = Scene::default();
        let mut order = Vec::new();
        let mut push = |s: &mut Scene, r: Rect, clip: Option<[f32; 4]>| {
            s.rects.push(r);
            order.push(Placed { item: Node::Rect(s.rects.len() - 1), clip });
        };
        push(&mut s, Rect { position: [0.0, 0.0], size: [900.0, 600.0], color: [0.3, 0.32, 0.36, 1.0], radius: 0.0 }, None);
        push(&mut s, Rect { position: [40.0, 40.0], size: [820.0, 520.0], color: [0.97, 0.97, 0.95, 1.0], radius: 0.0 }, Some(view));
        for i in 0..600 {
            let y = 40.0 + i as f32 * 22.0 + off;
            for (x, rw) in [(8.0, 120.0), (140.0, 260.0), (420.0, 90.0 + (i % 7) as f32 * 30.0)] {
                push(&mut s, Rect { position: [40.0 + x, y + 5.0], size: [rw, 12.0], color: [0.4, 0.45, 0.5, 1.0], radius: 3.0 }, Some(view));
            }
        }
        push(&mut s, Rect { position: [760.0, 480.0], size: [80.0, 50.0], color: [0.1, 0.6, 0.3, 0.9], radius: 10.0 }, None);
        Frame::pack_ordered(&s, &order, w, h, w, None).0
    };
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let mut t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2");
    t2.set_threads(1);
    let mut px = vec![0u32; (w * h) as usize];
    let f0 = build(0.0);
    t2.render(&f0, &mut px).unwrap();
    let mut held = damage::tile_signatures(&f0);
    let over = vec![[760, 480, 840, 530]];
    let (mut t_shift, mut t_full, mut tiles_n) = (0.0, 0.0, 0);
    let frames = 30;
    for k in 1..=frames {
        let f = build(-7.0 * k as f32);
        let t = Instant::now();
        let sig = damage::tile_signatures(&f);
        let sh = Shift { area, d: [0, -7], over: over.clone(), over_before: over.clone() };
        let out = damage::shift(&mut px, w, h, &sig, &mut held, &[sh]);
        let dirty = damage::dirty(&sig, Some(&held));
        tiles_n += dirty.iter().filter(|&&b| b).count()
            + out.pixels.iter().map(|r| ((r[2] - r[0]) * (r[3] - r[1])) as usize).sum::<usize>() / (TILE * TILE) as usize;
        let tiles = damage::rects(&dirty, w.div_ceil(TILE), h.div_ceil(TILE), usize::MAX);
        t2.render_rects(&f, &mut px, &tiles, &out.pixels).unwrap();
        t_shift += t.elapsed().as_secs_f64();
        held = sig;
        let mut full = px.clone();
        let t = Instant::now();
        let all: Vec<bool> = (0..held.len()).map(|i| {
            let (tx, ty) = (i as u32 % w.div_ceil(TILE), i as u32 / w.div_ceil(TILE));
            tx * TILE < 860 && tx * TILE + TILE > 40 && ty * TILE < 560 && ty * TILE + TILE > 40
        }).collect();
        let tl = damage::rects(&all, w.div_ceil(TILE), h.div_ceil(TILE), usize::MAX);
        t2.render_tiles(&f, &mut full, &tl).unwrap();
        t_full += t.elapsed().as_secs_f64();
        assert!(full == px);
    }
    let n = frames as f64;
    println!("tiles (or their pixels) per frame {} of {}; shift {:.2} ms, viewport in full {:.2} ms",
        tiles_n / frames, held.len(), t_shift * 1e3 / n, t_full * 1e3 / n);
}
