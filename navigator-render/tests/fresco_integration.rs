//! ★ NAVIGATOR'S VECTOR SHAPES THROUGH FRESCOD'S OWN PIPELINE.
//!
//! A page of inline SVGs is rendered by the Navigator into NSG `path` nodes;
//! each becomes the `FillParams` the broker would hand frescod, which this
//! translates exactly as frescod does (`frescod/src/pack.rs`
//! `translate_fill`); `Frame::pack_report` packs them as frescod packs a
//! window; and the frame is rastered by frescod's REAL kernels — Tier-2
//! (the SPIR-V compiled by Cranelift, what frescod runs on the CPU) and the
//! GIR interpreter — and compared with fresco-compute's reference. Then a
//! deliberately over-dense path is dropped with reason 2 (tile too dense),
//! and the scene's bitmap fallback for that SVG is what the UI draws instead.
//!
//! What this does not cover, stated: the socket, the scene server's
//! envelope, the compositor and the `EV_WINDOW_FILLS_DROPPED` delivery —
//! those need frescod running in the VM.

use fresco_compute::*;
use navigator_render::{fontset::FontSet, html::render_html_with, path};

const NS: &str = "xmlns=\"http://www.w3.org/2000/svg\"";
const W: u32 = 480;
const H: u32 = 320;

/// frescod's `translate_fill` (frescod/src/pack.rs), line for line.
fn translate_fill(p: &fresco_protocol::FillParams) -> fill::Fill {
    use fill::{Fill, FillRule, Gradient, GradientShape};
    use fresco_protocol::FillPaint;
    let stops = |s: &[fresco_protocol::GradientStop]| s.iter().map(|s| (s.offset, [s.r, s.g, s.b, s.a])).collect();
    let (color, gradient) = match &p.paint {
        FillPaint::Solid { r, g, b, a } => ([*r, *g, *b, *a * p.opacity], None),
        FillPaint::Linear { x0, y0, x1, y1, stops: s } => (
            [0.0, 0.0, 0.0, p.opacity],
            Some(Gradient { shape: GradientShape::Linear { p0: [*x0, *y0], p1: [*x1, *y1] }, stops: stops(s) }),
        ),
        FillPaint::Radial { cx, cy, radius, stops: s } => (
            [0.0, 0.0, 0.0, p.opacity],
            Some(Gradient { shape: GradientShape::Radial { center: [*cx, *cy], radius: *radius }, stops: stops(s) }),
        ),
    };
    Fill { verbs: p.verbs.clone(), points: p.points.clone(), transform: p.transform,
           rule: if p.rule == 1 { FillRule::EvenOdd } else { FillRule::NonZero }, color, clip: p.clip, gradient }
}

/// A page of inline SVGs, as the normalizer leaves them (replaced `<svg>`
/// elements carrying their standalone text).
fn page() -> (String, navigator_render::Subresources) {
    let svgs = [
        format!("<svg {NS} viewBox=\"0 0 40 40\"><circle cx=\"20\" cy=\"20\" r=\"14\" fill=\"#e03030\" stroke=\"#202060\" stroke-width=\"3\"/></svg>"),
        format!("<svg {NS} viewBox=\"0 0 40 40\"><path d=\"M4 36 Q20 -10 36 36 Z\" fill=\"#30a050\" fill-opacity=\"0.6\" fill-rule=\"evenodd\"/>\
                 <line x1=\"2\" y1=\"20\" x2=\"38\" y2=\"20\" stroke=\"#000\" stroke-width=\"2\" stroke-dasharray=\"4 3\"/></svg>"),
        format!("<svg {NS} viewBox=\"0 0 40 40\"><defs><linearGradient id=\"g\" x2=\"1\" y2=\"1\"><stop offset=\"0\" stop-color=\"#ffcc00\"/>\
                 <stop offset=\"1\" stop-color=\"#0033cc\"/></linearGradient></defs><rect x=\"4\" y=\"4\" width=\"32\" height=\"24\" rx=\"5\" fill=\"url(#g)\"/></svg>"),
        format!("<svg {NS} viewBox=\"0 0 40 40\"><defs><radialGradient id=\"r\"><stop offset=\"0\" stop-color=\"#fff\"/>\
                 <stop offset=\"1\" stop-color=\"#802080\" stop-opacity=\"0.5\"/></radialGradient><clipPath id=\"c\"><rect x=\"6\" y=\"6\" width=\"28\" height=\"28\"/></clipPath></defs>\
                 <g clip-path=\"url(#c)\"><circle cx=\"20\" cy=\"20\" r=\"19\" fill=\"url(#r)\"/></g></svg>"),
    ];
    let mut subs = navigator_render::Subresources::new();
    let mut body = String::new();
    for (i, s) in svgs.iter().enumerate() {
        let a = format!("blake3:{:064x}", i + 1);
        subs.insert(a.clone(), (a.clone(), 96 * 64, 96 * 64));
        body.push_str(&format!("<svg src=\"{a}\" width=\"96\" height=\"96\" atrium-svg=\"{}\"></svg>", s.replace('"', "&quot;")));
    }
    (format!("<html><body><p>Icons</p><div>{body}</div></body></html>"), subs)
}

fn fills_of(scene: &navigator_render::Scene) -> Vec<fill::Fill> {
    scene.paths.iter().map(|p| {
        let fp = path::fill_params(p, None);
        fp.validate().expect("the broker's check passes");
        translate_fill(&fp)
    }).collect()
}

fn diff(a: &[u32], b: &[u32]) -> (usize, u32) {
    let (mut n, mut worst) = (0, 0);
    for (x, y) in a.iter().zip(b) {
        if x != y { n += 1; for s in [0, 8, 16, 24] { worst = worst.max(((x >> s) & 255).abs_diff((y >> s) & 255)) } }
    }
    (n, worst)
}

#[test]
fn navigator_fills_raster_on_frescods_kernels_as_the_reference_does() {
    let (doc, subs) = page();
    let fonts = FontSet::load().unwrap();
    let o = render_html_with(&doc, &fonts, &Default::default(), &subs);
    assert_eq!(o.unimplemented.get("inline svg drawn as vector shapes"), Some(&4), "{:?}", o.unimplemented);
    let fills = fills_of(&o.scene);
    assert!(fills.len() >= 6, "{} fills", fills.len());
    let (frame, dropped) = Frame::pack_report(&Scene { fills, ..Default::default() }, W, H, W);
    assert!(dropped.is_empty(), "nothing dropped: {dropped:?}");
    let want = reference::render(&frame);
    let painted = want.iter().filter(|&&p| p != want[0]).count();
    assert!(painted > 8_000, "the fills paint: {painted} pixels");
    // Tier-2: frescod's CPU renderer, the SPIR-V compiled by Cranelift.
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2");
    let mut got = vec![0u32; frame.target_words()];
    t2.render(&frame, &mut got).unwrap();
    let (n, worst) = diff(&got, &want);
    assert_eq!(n, 0, "Tier-2: {n} pixels differ from the reference, worst channel {worst}");
    // The GIR interpreter: the same kernels, the path RDNA4 takes.
    let run = |spv: &[u8], bufs: &mut atrium_gir::interp::Buffers, groups: [u32; 3]| {
        let m = atrium_spv_frontend::translate(spv).expect("frontend");
        let g = atrium_gir::build::build(&m, 0).expect("build");
        atrium_gir::verify::verify(&g).expect("verify");
        atrium_gir::interp::run(&g, groups, bufs).expect("run");
    };
    let mut bufs: atrium_gir::interp::Buffers = frame.buffers().into_iter().enumerate().map(|(i, v)| ((0, i as u32), v)).collect();
    run(BIN_SPV, &mut bufs, frame.bin_groups());
    run(RASTER_SPV, &mut bufs, frame.raster_groups());
    let gir: Vec<u32> = bufs[&(0, binding::TARGET)].chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let (n, worst) = diff(&gir, &want);
    assert!(n == 0 || worst <= 1, "GIR: {n} pixels differ, worst channel {worst}");
}

#[test]
fn an_over_dense_path_is_dropped_with_reason_2_and_its_fallback_takes_its_place() {
    let (doc, subs) = page();
    let fonts = FontSet::load().unwrap();
    let o = render_html_with(&doc, &fonts, &Default::default(), &subs);
    // A path the worker's own pre-check would have kept as a bitmap, made by
    // hand: 6,000 lines zig-zagging inside ONE 16 px tile (placed on the tile
    // grid: straddling tiles would split them, and frescod would draw it).
    let mut pts = vec![(0i64, 0i64)];
    let mut verbs = vec![0u8];
    for i in 0..6_000 { verbs.push(1); pts.push((if i % 2 == 0 { 15 * 64 } else { 64 }, (i % 15) as i64 * 64 + 32)) }
    verbs.push(4);
    let dense = path::Path { even_odd: false, transform: [65536, 0, 0, 65536, 32 * 64, 32 * 64], paint: path::Paint::Solid(0x000000ff),
                             clip: None, opacity: 255, verbs, points: pts };
    path::validate(&dense).expect("well-formed: only its density is the problem");
    assert!(path::lines_if_drawable(&dense).is_none(), "the worker's pre-check would have refused it");
    let mut fills = fills_of(&o.scene);
    let dense_at = fills.len();
    fills.push(translate_fill(&path::fill_params(&dense, None)));
    let (_, dropped) = Frame::pack_report(&Scene { fills, ..Default::default() }, W, H, W);
    assert_eq!(dropped, vec![(fresco_compute::Node::Fill(dense_at), fresco_protocol::fill_drop::TILE_TOO_DENSE)], "dropped as too dense, and only it");
    // The UI's answer to the drop: the SVG's bitmap fallback.
    let (first, n, image) = path::fallback_for(&o.scene, 0).expect("a fallback for the first SVG");
    assert!(*n > 0 && *first == 0 && image.address.starts_with("blake3:"));
}
