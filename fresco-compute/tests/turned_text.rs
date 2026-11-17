//! Turned glyph runs (rotated text, `GlyphRun::turn`, `KIND_GLYPH_XF`).

use fresco_compute::*;

const W: u32 = 64;
const H: u32 = 48;

/// An 8x8 R8 atlas with distinct coverage per texel.
fn atlas() -> Texture {
    Texture { width: 8, height: 8, format: TexFormat::R8, data: (0..64u32).map(|i| (i * 37 % 251 + 4) as u8).collect() }
}

fn run(origin: [f32; 2], turn: Option<[f32; 4]>) -> Scene {
    Scene {
        textures: vec![atlas()],
        glyph_batches: vec![GlyphBatch {
            atlas: 0,
            runs: vec![GlyphRun {
                origin,
                color: [0.9, 0.85, 0.1, 1.0],
                glyphs: vec![
                    Glyph { d_offset: [0.0, 0.0], atlas_uv: [0.0, 0.0, 4.0, 8.0], bearing: [0.0, 7.0] },
                    Glyph { d_offset: [5.0, 0.0], atlas_uv: [4.0, 0.0, 4.0, 8.0], bearing: [1.0, 6.0] },
                ],
                turn,
            }],
        }],
        ..Default::default()
    }
}

fn render(s: &Scene) -> Vec<u32> {
    reference::render(&Frame::pack(s, W, H, W))
}

fn at(img: &[u32], x: i32, y: i32) -> u32 {
    img[(y as u32 * W + x as u32) as usize]
}

#[test]
fn identity_turn_packs_as_upright() {
    let a = Frame::pack(&run([10.0, 20.0], None), W, H, W);
    let b = Frame::pack(&run([10.0, 20.0], Some([1.0, 0.0, 0.0, 1.0])), W, H, W);
    assert_eq!(a.ops, b.ops);
}

/// A quarter turn about a whole-pixel origin lands every texel centre on a
/// pixel centre: the turned run is the upright one, pixel for pixel, turned.
#[test]
fn quarter_turns_are_the_upright_run_turned_exactly() {
    let o = [20, 24];
    let up = render(&run([o[0] as f32, o[1] as f32], None));
    let clear = render(&Scene::default())[0];
    // Upright pixel (x, y) relative to the origin, turned by (a, b, c, d):
    // its centre (x + 0.5, y + 0.5) lands at (a cx + c cy, b cx + d cy).
    for m in [[0.0, 1.0, -1.0, 0.0], [-1.0, 0.0, 0.0, -1.0], [0.0, -1.0, 1.0, 0.0]] {
        let turned = render(&run([o[0] as f32, o[1] as f32], Some(m)));
        let mut drawn = 0;
        for y in 0..H as i32 {
            for x in 0..W as i32 {
                let (cx, cy) = ((x - o[0]) as f32 + 0.5, (y - o[1]) as f32 + 0.5);
                let tx = m[0] * cx + m[2] * cy - 0.5 + o[0] as f32;
                let ty = m[1] * cx + m[3] * cy - 0.5 + o[1] as f32;
                let (tx, ty) = (tx as i32, ty as i32);
                let want = at(&up, x, y);
                if want != clear {
                    drawn += 1;
                }
                if (0..W as i32).contains(&tx) && (0..H as i32).contains(&ty) {
                    assert_eq!(at(&turned, tx, ty), want, "turn {m:?}: upright ({x}, {y}) -> ({tx}, {ty})");
                } else {
                    assert_eq!(want, clear, "turn {m:?}: upright ({x}, {y}) turned off the target");
                }
            }
        }
        assert!(drawn > 30, "only {drawn} pixels drawn");
        // And nothing else was drawn.
        let n = |img: &[u32]| img.iter().filter(|&&p| p != clear).count();
        assert_eq!(n(&turned), n(&up), "turn {m:?}");
    }
}

/// Any angle: what is drawn stays within the turned glyph boxes (grown by
/// the half texel the edges fade over), and a glyph's ink is all there.
#[test]
fn any_angle_draws_within_the_turned_boxes() {
    let clear = render(&Scene::default())[0];
    for deg in [7.0f32, 30.0, 45.0, 90.0, 135.0, 200.0, 333.0] {
        let (s, c) = deg.to_radians().sin_cos();
        let m = [c, s, -s, c];
        let scene = run([30.25, 22.75], Some(m));
        let img = render(&scene);
        let inv = |x: f32, y: f32| (c * x + s * y, -s * x + c * y); // the inverse turn
        let mut ink = 0.0f32;
        for y in 0..H as i32 {
            for x in 0..W as i32 {
                if at(&img, x, y) == clear {
                    continue;
                }
                let (lx, ly) = inv(x as f32 + 0.5 - 30.25, y as f32 + 0.5 - 22.75);
                // Glyph 0's box: (0, -7) 4x8; glyph 1's: (6, -6) 4x8.
                let inside = |bx: f32, by: f32| lx >= bx - 0.5 && lx < bx + 4.5 && ly >= by - 0.5 && ly < by + 8.5;
                assert!(inside(0.0, -7.0) || inside(6.0, -6.0), "{deg} deg: ({x}, {y}) drawn outside the glyphs");
                ink += 1.0;
            }
        }
        assert!(ink > 30.0, "{deg} deg: only {ink} pixels drawn");
    }
}

#[test]
fn a_turn_pack_refuses_draws_nothing() {
    let clear = render(&Scene::default());
    for m in [[0.0, 0.0, 0.0, 0.0], [1.0, 2.0, 2.0, 4.0], [f32::NAN, 0.0, 0.0, 1.0], [100.0, 0.0, 0.0, 1.0]] {
        assert!(!turn_ok(&m));
        assert_eq!(render(&run([20.0, 20.0], Some(m))), clear, "turn {m:?}");
    }
}

#[test]
fn tier2_draws_turned_runs_like_the_reference() {
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2 load");
    for deg in [0.0f32, 13.0, 90.0, 181.0, 290.0] {
        let (s, c) = deg.to_radians().sin_cos();
        let f = Frame::pack(&run([31.5, 20.25], Some([c, s, -s, c])), W, H, W);
        let mut got = vec![0u32; f.target_words()];
        t2.render(&f, &mut got).unwrap();
        assert_eq!(got, reference::render(&f), "{deg} deg");
    }
}
