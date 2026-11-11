//! Exact group opacity (fresco-scene-groups.md §2.4, `ISOLATE`): a group
//! between `Node::Push` and `Node::Pop` is drawn into a layer and blended
//! at its opacity, in the reference, Tier-2 and the GIR interpreter alike.

use fresco_compute::*;

const W: u32 = 48;
const H: u32 = 32;

fn rect(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> Rect {
    Rect { position: [x, y], size: [w, h], color, radius: 0.0 }
}

fn placed(item: Node) -> Placed {
    Placed { item, clip: None }
}

fn push(o: f32) -> Placed {
    placed(Node::Push(o.to_bits()))
}

fn at(px: &[u32], x: u32, y: u32) -> u32 {
    px[(y * W + x) as usize]
}

fn render(s: &Scene, order: &[Placed]) -> Vec<u32> {
    let (f, d) = Frame::pack_ordered(s, order, W, H, W, None);
    assert!(d.is_empty());
    reference::render(&f)
}

const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// A faded card: two overlapping opaque rects at 50%. Multiplied alpha
/// lets the lower one show through the upper; the isolated group does not.
#[test]
fn a_faded_card_hides_what_it_covers() {
    let s = Scene {
        rects: vec![rect(0.0, 0.0, 48.0, 32.0, WHITE), rect(4.0, 4.0, 20.0, 20.0, RED), rect(14.0, 8.0, 20.0, 20.0, BLUE)],
        ..Default::default()
    };
    let iso = render(&s, &[placed(Node::Rect(0)), push(0.5), placed(Node::Rect(1)), placed(Node::Rect(2)), placed(Node::Pop)]);
    // The same pixels as the upper rect alone at 50% over white.
    let alone = Scene { rects: vec![s.rects[0], rect(14.0, 8.0, 20.0, 20.0, [0.0, 0.0, 1.0, 0.5])], ..Default::default() };
    let want = render(&alone, &[placed(Node::Rect(0)), placed(Node::Rect(1))]);
    assert_eq!(at(&iso, 20, 12), at(&want, 20, 12), "overlap");
    // Where only the lower one is: it at 50%.
    let red = Scene { rects: vec![s.rects[0], rect(4.0, 4.0, 20.0, 20.0, [1.0, 0.0, 0.0, 0.5])], ..Default::default() };
    assert_eq!(at(&iso, 6, 6), at(&render(&red, &[placed(Node::Rect(0)), placed(Node::Rect(1))]), 6, 6));
    // Outside the group: untouched.
    assert_eq!(at(&iso, 40, 2), at(&render(&s, &[placed(Node::Rect(0))]), 40, 2));
    // Multiplied alpha differs in the overlap.
    let mul = Scene {
        rects: vec![s.rects[0], rect(4.0, 4.0, 20.0, 20.0, [1.0, 0.0, 0.0, 0.5]), rect(14.0, 8.0, 20.0, 20.0, [0.0, 0.0, 1.0, 0.5])],
        ..Default::default()
    };
    let m = render(&mul, &[placed(Node::Rect(0)), placed(Node::Rect(1)), placed(Node::Rect(2))]);
    assert_ne!(at(&iso, 20, 12), at(&m, 20, 12));
}

/// Opacity 1, or a group over nothing that overlaps, draws as without it.
#[test]
fn opaque_or_disjoint_groups_change_nothing() {
    let s = Scene {
        rects: vec![rect(0.0, 0.0, 48.0, 32.0, WHITE), rect(4.0, 4.0, 10.0, 10.0, RED), rect(20.0, 4.0, 10.0, 10.0, [0.0, 0.0, 1.0, 0.4])],
        ..Default::default()
    };
    let plain = render(&s, &[placed(Node::Rect(0)), placed(Node::Rect(1)), placed(Node::Rect(2))]);
    let one = render(&s, &[placed(Node::Rect(0)), push(1.0), placed(Node::Rect(1)), placed(Node::Rect(2)), placed(Node::Pop)]);
    assert_eq!(plain, one);
}

/// Groups nest up to `ISOLATE_DEPTH`; past it the kernel ignores a PUSH and its POP;
/// a group left open is closed after the last op; an empty group packs no
/// ops; stray POPs are ignored.
#[test]
fn nesting_depth_and_unbalanced_lists() {
    let s = Scene { rects: vec![rect(0.0, 0.0, 48.0, 32.0, WHITE), rect(4.0, 4.0, 20.0, 20.0, RED)], ..Default::default() };
    // Two nested halves: a quarter.
    let nested = render(&s, &[placed(Node::Rect(0)), push(0.5), push(0.5), placed(Node::Rect(1)), placed(Node::Pop), placed(Node::Pop)]);
    let q = Scene { rects: vec![s.rects[0], rect(4.0, 4.0, 20.0, 20.0, [1.0, 0.0, 0.0, 0.25])], ..Default::default() };
    let qp = render(&q, &[placed(Node::Rect(0)), placed(Node::Rect(1))]);
    let d = |a: u32, b: u32| (0..4).map(|c| ((a >> (8 * c)) & 255).abs_diff((b >> (8 * c)) & 255)).max().unwrap();
    if ISOLATE_DEPTH >= 2 {
        assert!(d(at(&nested, 8, 8), at(&qp, 8, 8)) <= 1);
    }
    // Open at the end: closed then; a stray POP first changes nothing.
    let open = render(&s, &[placed(Node::Pop), placed(Node::Rect(0)), push(0.5), push(0.5), placed(Node::Rect(1))]);
    assert_eq!(open, nested);
    // An empty group: no ops.
    let (f, _) = Frame::pack_ordered(&s, &[placed(Node::Rect(0)), push(0.5), placed(Node::Pop)], W, H, W, None);
    assert_eq!(f.header.op_count, 1);
    // ISOLATE_DEPTH + 1 deep: the innermost is ignored (no opacity of its own).
    let mut deep = vec![placed(Node::Rect(0))];
    deep.extend((0..ISOLATE_DEPTH).map(|_| push(1.0)));
    deep.push(push(0.5));
    deep.push(placed(Node::Rect(1)));
    deep.extend((0..=ISOLATE_DEPTH).map(|_| placed(Node::Pop)));
    assert_eq!(render(&s, &deep), render(&s, &[placed(Node::Rect(0)), placed(Node::Rect(1))]));
}

/// Tier-2 and the GIR interpreter draw isolated groups as the reference does.
#[test]
fn executors_match_reference() {
    let mut r = fuzz::Rng(0x150_1a7e);
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2 load");
    let gir = |spv: &[u8]| {
        let m = atrium_spv_frontend::translate(spv).unwrap();
        atrium_gir::build::build(&m, 0).unwrap()
    };
    let (bin, raster) = (gir(BIN_SPV), gir(RASTER_SPV));
    for _ in 0..12 {
        let s = fuzz::scene(&mut r, W, H);
        // Every element, wrapped in random nested groups.
        let mut order = Vec::new();
        let mut depth = 0;
        let mut items: Vec<Node> = (0..s.rects.len()).map(Node::Rect).collect();
        items.extend((0..s.paths.len()).map(Node::Path));
        items.extend((0..s.fills.len()).map(Node::Fill));
        for it in items {
            if r.chance(30) && depth < 6 {
                order.push(push(r.f(0.0, 1.0)));
                depth += 1;
            }
            order.push(placed(it));
            if depth > 0 && r.chance(25) {
                order.push(placed(Node::Pop));
                depth -= 1;
            }
        }
        let (f, _) = Frame::pack_ordered(&s, &order, W, H, W, None);
        let want = reference::render(&f);
        let mut got = vec![0u32; f.target_words()];
        t2.render(&f, &mut got).unwrap();
        assert_eq!(got, want, "tier2");
        let mut bufs: atrium_gir::interp::Buffers = f.buffers().into_iter().enumerate().map(|(i, v)| ((0, i as u32), v)).collect();
        atrium_gir::interp::run(&bin, f.bin_groups(), &mut bufs).unwrap();
        atrium_gir::interp::run(&raster, f.raster_groups(), &mut bufs).unwrap();
        let g: Vec<u32> = bufs[&(0, binding::TARGET)].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(g, want, "gir");
    }
}
