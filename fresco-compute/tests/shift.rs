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
        let (forced, moved) = damage::shift(&mut px, W, H, &sig, &mut held, &[shift]);
        let dirty: Vec<bool> = damage::dirty(&sig, Some(&held)).iter().zip(&forced).map(|(a, b)| *a || *b).collect();
        let n_dirty = dirty.iter().filter(|&&b| b).count();
        let tiles = damage::rects(&dirty, W.div_ceil(TILE), H.div_ceil(TILE), usize::MAX);
        t2.render_tiles(&f, &mut px, &tiles).unwrap();
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
fn nothing_moves_outside_the_area_and_over_tiles_are_forced() {
    let (w, h) = (64u32, 64u32);
    let mut px: Vec<u32> = (0..w * h).collect();
    let n = (w.div_ceil(TILE) * h.div_ceil(TILE)) as usize;
    let sig = vec![7u64; n];
    let mut held = vec![1u64; n];
    let s = Shift { area: [0, 16, 64, 64], d: [0, -16], over: vec![[0, 50, 10, 52]], over_before: vec![] };
    let (forced, moved) = damage::shift(&mut px, w, h, &sig, &mut held, &[s]);
    assert_eq!(moved, Some([0, 16, 64, 64]));
    // Row 0 of tiles (outside) untouched; row 1 and 2 moved up from 2 and 3.
    assert_eq!(px[0], 0);
    assert_eq!(px[(16 * w) as usize], 32 * w);
    // Tiles 4..8 (row 1) and 8..12 (row 2) clean, but tile 12 (row 3, col 0)
    // is under the floating box, and row 3 is the exposed strip anyway.
    assert!(!forced[4] && held[4] == 7 && !forced[11] && held[11] == 7);
    assert!(forced[12] && forced[15] && held[12] == 1);
    assert!(!forced[0] && held[0] == 1, "outside the area: left to signatures");
}
