//! Filled paths: the coverage rule against a supersampled winding count,
//! the limits, and culling.

use fresco_compute::fill::*;
use fresco_compute::*;

/// Winding number of point `p` for closed lines.
fn winding(lines: &[[f32; 4]], p: [f32; 2]) -> i32 {
    let mut w = 0;
    for s in lines {
        let (y0, y1) = (s[1], s[3]);
        if (y0 <= p[1]) != (y1 <= p[1]) {
            let x = s[0] + (p[1] - y0) * (s[2] - s[0]) / (y1 - y0);
            if x > p[0] {
                w += if y1 > y0 { 1 } else { -1 };
            }
        }
    }
    w
}

/// Pack one fill on a transparent target; the reference's coverage per pixel.
fn covered(f: &Fill, w: u32, h: u32) -> Vec<f32> {
    let one = Fill { color: [1.0, 1.0, 1.0, 1.0], ..f.clone() };
    let mut fr = Frame::pack(&Scene { fills: vec![one], ..Default::default() }, w, h, w);
    fr.header.clear = TRANSPARENT;
    reference::render(&fr).iter().map(|p| (p >> 24) as f32 / 255.0).collect()
}

fn check_against_samples(f: &Fill, w: u32, h: u32, even_odd: bool) -> (f32, f32, usize) {
    let lines = flatten(f).unwrap();
    let got = covered(f, w, h);
    let n = 16;
    let (mut worst, mut sum, mut off) = (0.0f32, 0.0f32, 0);
    for y in 0..h {
        for x in 0..w {
            let mut inside = 0;
            for sy in 0..n {
                for sx in 0..n {
                    let p = [x as f32 + (sx as f32 + 0.5) / n as f32, y as f32 + (sy as f32 + 0.5) / n as f32];
                    let wn = winding(&lines, p);
                    if if even_odd { wn % 2 != 0 } else { wn != 0 } {
                        inside += 1;
                    }
                }
            }
            let want = inside as f32 / (n * n) as f32;
            let d = (got[(y * w + x) as usize] - want).abs();
            worst = worst.max(d);
            sum += d;
            off += (d > 0.1) as usize;
        }
    }
    (worst, sum / (w * h) as f32, off)
}

fn circle(cx: f32, cy: f32, r: f32) -> (Vec<u8>, Vec<[f32; 2]>) {
    let k = 0.5523;
    let mut p = vec![[cx + r, cy]];
    let q = [[1.0, 0.0], [0.0, 1.0], [-1.0, 0.0], [0.0, -1.0], [1.0, 0.0]];
    for i in 0..4 {
        let (a, b) = (q[i], q[i + 1]);
        p.push([cx + r * (a[0] + k * b[0]), cy + r * (a[1] + k * b[1])]);
        p.push([cx + r * (b[0] + k * a[0]), cy + r * (b[1] + k * a[1])]);
        p.push([cx + r * b[0], cy + r * b[1]]);
    }
    (vec![VERB_MOVE, VERB_CUBIC, VERB_CUBIC, VERB_CUBIC, VERB_CUBIC, VERB_CLOSE], p)
}

#[test]
fn coverage_is_the_pixel_area() {
    // A ring (circle with a reversed hole), across several tiles.
    let (mut v, mut p) = circle(30.3, 25.7, 22.0);
    let (v2, mut p2) = circle(30.3, 25.7, 9.0);
    p2.reverse();
    v.extend(v2);
    p.extend(p2);
    let ring = Fill { verbs: v, points: p, transform: IDENTITY, ..Default::default() };
    let (worst, mean, _) = check_against_samples(&ring, 64, 52, false);
    assert!(worst < 0.08 && mean < 0.01, "ring: worst {worst} mean {mean}");
    // A thin sliver and a polygon that wraps around tiles to the left.
    let tri = Fill {
        verbs: vec![VERB_MOVE, VERB_LINE, VERB_LINE, VERB_LINE],
        points: vec![[2.0, 3.0], [61.0, 7.5], [40.0, 49.0], [3.0, 45.0]],
        transform: IDENTITY,
        ..Default::default()
    };
    let (worst, mean, _) = check_against_samples(&tri, 64, 52, false);
    assert!(worst < 0.08 && mean < 0.01, "quad: worst {worst} mean {mean}");
    // Even-odd: a pentagram's centre is a hole.
    let star: Vec<[f32; 2]> = (0..5)
        .map(|i| {
            let a = i as f32 * 4.0 * std::f32::consts::PI / 5.0 - 1.5;
            [32.0 + a.cos() * 24.0, 26.0 + a.sin() * 24.0]
        })
        .collect();
    let s = Fill { verbs: vec![VERB_MOVE, VERB_LINE, VERB_LINE, VERB_LINE, VERB_LINE], points: star, transform: IDENTITY, rule: FillRule::EvenOdd, ..Default::default() };
    // Area accumulation is exact where edges do not cross inside a pixel;
    // the five pixels where the star's edges cross are approximate.
    let (worst, mean, off) = check_against_samples(&s, 64, 52, true);
    assert!(off <= 5 && mean < 0.01, "star: {off} pixels off by > 0.1 (worst {worst}), mean {mean}");
    let got = covered(&s, 64, 52);
    assert!(got[(26 * 64 + 32) as usize] < 0.01, "even-odd centre filled");
}

#[test]
fn malformed_and_oversized_paths_are_refused() {
    let ok = [VERB_MOVE, VERB_LINE, VERB_LINE];
    let pts = [[0.0, 0.0], [5.0, 0.0], [0.0, 5.0]];
    assert!(check(&ok, &pts, &IDENTITY).is_ok());
    assert!(check(&[VERB_LINE], &[[0.0, 0.0]], &IDENTITY).is_err(), "no move first");
    assert!(check(&[VERB_MOVE, 9], &[[0.0, 0.0]], &IDENTITY).is_err(), "unknown verb");
    assert!(check(&ok, &pts[..2], &IDENTITY).is_err(), "too few points");
    assert!(check(&ok, &[[0.0, 0.0], [f32::NAN, 0.0], [0.0, 5.0]], &IDENTITY).is_err());
    assert!(check(&ok, &[[0.0, 0.0], [40000.0, 0.0], [0.0, 5.0]], &IDENTITY).is_err());
    // The bound is in path space: a big translation is fine (scroll).
    let far = [1.0, 0.0, 0.0, 1.0, 0.0, -1.4e6];
    assert!(check(&ok, &pts, &far).is_ok());
    // A path flattening past the per-path cap.
    let n = MAX_PATH_SEGMENTS + 10;
    let mut verbs = vec![VERB_MOVE];
    verbs.extend(std::iter::repeat_n(VERB_LINE, n));
    let points: Vec<[f32; 2]> = (0..=n).map(|i| [(i % 100) as f32, (i % 7) as f32]).collect();
    assert!(flatten(&Fill { verbs, points, transform: IDENTITY, ..Default::default() }).is_none());
}

#[test]
fn off_target_paths_are_culled() {
    let tri = |dy: f32| Fill {
        verbs: vec![VERB_MOVE, VERB_LINE, VERB_LINE],
        points: vec![[0.0, 0.0], [20.0, 0.0], [0.0, 20.0]],
        transform: [1.0, 0.0, 0.0, 1.0, 4.0, dy],
        color: [1.0; 4],
        ..Default::default()
    };
    let n = |f: Fill| Frame::pack(&Scene { fills: vec![f], ..Default::default() }, 32, 32, 32).header.op_count;
    assert_eq!(n(tri(4.0)), 1);
    assert_eq!(n(tri(-1.4e6)), 0, "scrolled away: culled");
    let mut clipped = tri(4.0);
    clipped.clip = Some([30.0, 0.0, 32.0, 2.0]);
    assert_eq!(n(clipped), 0, "outside its clip: culled");
}

#[test]
fn gradients_interpolate_in_srgb_and_pad() {
    // Black to white across 0..256 px, linear: byte x at pixel x (t at the
    // pixel centre, quantized), padded outside 64..192 by stops at 0.25/0.75.
    let rect = Fill {
        verbs: vec![VERB_MOVE, VERB_LINE, VERB_LINE, VERB_LINE],
        points: vec![[0.0, 0.0], [256.0, 0.0], [256.0, 4.0], [0.0, 4.0]],
        transform: IDENTITY,
        color: [0.0, 0.0, 0.0, 1.0],
        gradient: Some(Gradient {
            shape: GradientShape::Linear { p0: [0.0, 0.0], p1: [256.0, 0.0] },
            stops: vec![(0.25, [0.0, 0.0, 0.0, 1.0]), (0.75, [1.0, 1.0, 1.0, 1.0])],
        }),
        ..Default::default()
    };
    let f = Frame::pack(&Scene { fills: vec![rect], ..Default::default() }, 256, 4, 256);
    let out = reference::render(&f);
    for x in [0u32, 40, 64, 100, 128, 150, 191, 192, 250] {
        let t = ((x as f32 + 0.5) / 256.0).clamp(0.25, 0.75);
        let want = (((t - 0.25) / 0.5) * 255.0 + 0.5) as u32;
        let got = out[(2 * 256 + x) as usize] & 255;
        assert!(got.abs_diff(want) <= 1, "x {x}: byte {got}, want {want}");
    }
    // Radial with a scaled transform: concentric; the centre is the first stop.
    let disk = Fill {
        verbs: vec![VERB_MOVE, VERB_LINE, VERB_LINE, VERB_LINE],
        points: vec![[-10.0, -10.0], [10.0, -10.0], [10.0, 10.0], [-10.0, 10.0]],
        transform: [2.0, 0.0, 0.0, 2.0, 20.0, 20.0],
        color: [0.0, 0.0, 0.0, 1.0],
        gradient: Some(Gradient {
            shape: GradientShape::Radial { center: [0.0, 0.0], radius: 10.0 },
            stops: vec![(0.0, [1.0, 0.0, 0.0, 1.0]), (1.0, [0.0, 0.0, 1.0, 1.0])],
        }),
        ..Default::default()
    };
    let f = Frame::pack(&Scene { fills: vec![disk], ..Default::default() }, 40, 40, 40);
    let out = reference::render(&f);
    let px = |x: u32, y: u32| out[(y * 40 + x) as usize];
    assert!((px(20, 20) >> 16) & 255 > 240 && px(20, 20) & 255 < 15, "centre is red");
    // Pixel centres at (±9.5, 0.5) and (0.5, 9.5) from the centre.
    assert_eq!(px(29, 20), px(20, 29), "concentric");
    assert_eq!(px(29, 20), px(10, 20), "concentric");
    assert_ne!(px(29, 20), px(20, 20), "it varies");
    // A bad gradient drops the fill.
    let mut bad = Fill { ..f_with_stops(vec![(0.5, [1.0; 4]), (0.2, [0.0; 4])]) };
    assert!(check_gradient(bad.gradient.as_ref().unwrap()).is_err());
    bad.gradient.as_mut().unwrap().stops.clear();
    assert_eq!(Frame::pack(&Scene { fills: vec![bad], ..Default::default() }, 8, 8, 8).header.op_count, 0);
}

fn f_with_stops(stops: Vec<(f32, [f32; 4])>) -> Fill {
    Fill {
        verbs: vec![VERB_MOVE, VERB_LINE, VERB_LINE],
        points: vec![[0.0, 0.0], [8.0, 0.0], [0.0, 8.0]],
        transform: IDENTITY,
        color: [1.0; 4],
        gradient: Some(Gradient { shape: GradientShape::Linear { p0: [0.0, 0.0], p1: [8.0, 0.0] }, stops }),
        ..Default::default()
    }
}

#[test]
fn dropped_fills_are_reported_with_why() {
    let tri = || Fill {
        verbs: vec![VERB_MOVE, VERB_LINE, VERB_LINE],
        points: vec![[0.0, 0.0], [20.0, 0.0], [0.0, 20.0]],
        transform: IDENTITY,
        color: [1.0; 4],
        ..Default::default()
    };
    let malformed = Fill { verbs: vec![VERB_LINE], points: vec![[0.0, 0.0]], ..tri() };
    let bad_paint = f_with_stops(vec![(2.0, [1.0; 4])]);
    let long = {
        let n = MAX_PATH_SEGMENTS + 10;
        let mut verbs = vec![VERB_MOVE];
        verbs.extend(std::iter::repeat_n(VERB_LINE, n));
        let points = (0..=n).map(|i| [(i % 100) as f32, (i % 7) as f32]).collect();
        Fill { verbs, points, ..tri() }
    };
    // 5000 lines inside one 16x16 tile.
    let dense = {
        let n = 5000;
        let mut verbs = vec![VERB_MOVE];
        verbs.extend(std::iter::repeat_n(VERB_LINE, n));
        let points = (0..=n).map(|i| [1.0 + (i % 13) as f32, if i % 2 == 0 { 1.0 } else { 14.0 }]).collect();
        Fill { verbs, points, ..tri() }
    };
    let culled = Fill { transform: [1.0, 0.0, 0.0, 1.0, 0.0, -1.0e5], ..tri() };
    let scene = Scene { fills: vec![tri(), malformed, bad_paint, long, dense, culled], ..Default::default() };
    let (frame, dropped) = Frame::pack_report(&scene, 64, 64, 64);
    assert_eq!(dropped, vec![
        (Node::Fill(1), drop::MALFORMED),
        (Node::Fill(2), drop::BAD_PAINT),
        (Node::Fill(3), drop::TOO_MANY_LINES),
        (Node::Fill(4), drop::TILE_TOO_DENSE),
    ]);
    assert_eq!(frame.header.op_count, 1, "only the good one draws; the culled one is not a drop");
}

/// The words a frame's executors read.
fn words(f: &Frame) -> (Vec<u8>, Vec<u8>, Vec<u32>) {
    (bytes_of(&[f.header]).to_vec(), bytes_of(&f.ops).to_vec(), f.texels.clone())
}

#[test]
fn cached_packs_are_the_same_frame() {
    let fills = fuzz::fills();
    let scene = |skip: usize, dx: f32| Scene {
        fills: fills.iter().skip(skip).map(|f| {
            let mut f = f.clone();
            f.transform[4] += dx;
            f
        }).collect(),
        ..Default::default()
    };
    let mut cache = FillCache::default();
    // Hits at other bases, a moved fill, a dropped first one, back again.
    for (skip, dx) in [(0, 0.0), (0, 0.0), (1, 0.0), (0, 3.5), (0, 0.0), (2, 0.0), (0, 0.0)] {
        let s = scene(skip, dx);
        let (a, da) = Frame::pack_report(&s, 96, 80, 96);
        let (b, db) = Frame::pack_cached(&s, 96, 80, 96, Some(&mut cache));
        assert_eq!(da, db);
        assert!(words(&a) == words(&b), "skip {skip} dx {dx}: cached frame differs");
    }
    assert!(!cache.is_empty());
}

/// `cargo test --release --test fill -- --ignored --nocapture`: a 4,000-
/// point chart and the fixed fills, packed afresh and from the cache.
#[test]
#[ignore]
fn cache_speed() {
    let n = 4000;
    let mut points = vec![[0.0, 120.0]];
    points.extend((0..n).map(|i| {
        let x = i as f32 * 800.0 / n as f32;
        [x, 60.0 + 40.0 * (x * 0.05).sin() + 10.0 * (x * 0.9).sin()]
    }));
    points.push([800.0, 120.0]);
    let mut verbs = vec![VERB_MOVE];
    verbs.extend(std::iter::repeat_n(VERB_LINE, points.len() - 1));
    let mut fills = fuzz::fills();
    fills.push(Fill { verbs, points, transform: [1.0, 0.0, 0.0, 1.0, 50.0, 430.0], color: [1.0; 4], ..Default::default() });
    let s = Scene { fills, ..Default::default() };
    let time = |f: &mut dyn FnMut()| {
        let t = std::time::Instant::now();
        for _ in 0..100 {
            f();
        }
        t.elapsed().as_secs_f64() * 10.0
    };
    let fresh = time(&mut || { Frame::pack(&s, 900, 600, 900); });
    let mut cache = FillCache::default();
    let cached = time(&mut || { Frame::pack_cached(&s, 900, 600, 900, Some(&mut cache)); });
    println!("pack: {fresh:.3} ms fresh, {cached:.3} ms cached");
}

#[test]
fn a_fill_moving_in_texels_redraws_nothing() {
    let fills = fuzz::fills();
    let mut grown = fills.clone();
    // The first fill doubles in size: every later block moves.
    grown[0].transform = [2.0, 0.0, 0.0, 2.0, grown[0].transform[4], grown[0].transform[5]];
    let a = Frame::pack(&Scene { fills: fills.clone(), ..Default::default() }, 160, 120, 160);
    let b = Frame::pack(&Scene { fills: grown.clone(), ..Default::default() }, 160, 120, 160);
    assert_ne!(a.ops[1].meta[1], b.ops[1].meta[1], "the later blocks moved");
    // Only where the first fill was or is now differs.
    let (sa, sb) = (damage::tile_signatures(&a), damage::tile_signatures(&b));
    let first = |f: &Frame| f.ops[0].p3;
    let (ba, bb) = (first(&a), first(&b));
    let tx = a.header.tiles_x;
    for (i, (x, y)) in sa.iter().zip(&sb).enumerate() {
        let (cx, cy) = (((i as u32 % tx) * TILE) as f32, ((i as u32 / tx) * TILE) as f32);
        let touches = |b: [f32; 4]| cx + TILE as f32 > b[0] && cx < b[2] && cy + TILE as f32 > b[1] && cy < b[3];
        if !touches(ba) && !touches(bb) {
            assert_eq!(x, y, "tile {i} away from the changed fill is dirty");
        }
    }
}
