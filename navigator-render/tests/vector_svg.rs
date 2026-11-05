//! Inline SVG as vector shapes (src/path.rs): NSG `path` nodes, one to one
//! with Fresco's ATRIUM_CORE_FILL. Painted here by fresco-compute's own
//! coverage rule, so a test checks what frescod would draw.
use navigator_render::path::{fill_params, from_svg, validate, Path};

const NS: &str = "xmlns=\"http://www.w3.org/2000/svg\"";

/// Coverage of pixel (x, y), window pixels, under every path: src-over of
/// straight-alpha colours, the alpha channel only.
fn alpha_at(paths: &[Path], x: f32, y: f32) -> f32 {
    let mut a = 0.0f32;
    for p in paths {
        let fp = fill_params(p, None);
        let f = fresco_compute::fill::Fill { verbs: fp.verbs, points: fp.points, transform: fp.transform,
            rule: fresco_compute::fill::FillRule::NonZero, color: [0.0; 4], clip: None };
        let lines = fresco_compute::fill::flatten(&f).expect("flattens");
        let acc: f32 = lines.iter().map(|s| fresco_compute::fill::fill_line(*s, x, y)).sum();
        let c = fresco_compute::fill::coverage(acc, p.even_odd) * fp.opacity * match fp.paint { fresco_protocol::FillPaint::Solid { a, .. } => a };
        a = c + a * (1.0 - c);
    }
    a
}

fn tile(px: i64) -> (i64, i64, i64, i64) { (0, 0, px * 64, px * 64) }

#[test]
fn fills_strokes_and_dashes_become_paths_that_paint_where_they_should() {
    let svg = format!("<svg {NS} viewBox=\"0 0 20 20\"><rect x=\"0\" y=\"0\" width=\"10\" height=\"10\" fill=\"#ff0000\"/>\
        <circle cx=\"15\" cy=\"15\" r=\"3\" fill=\"none\" stroke=\"#0000ff\" stroke-width=\"2\"/>\
        <line x1=\"0\" y1=\"18\" x2=\"10\" y2=\"18\" stroke=\"#000\" stroke-width=\"1\" stroke-dasharray=\"2 2\"/></svg>");
    let mut budget = usize::MAX;
    let paths = from_svg(&svg, tile(40), &mut budget).expect("vector");
    // A fill, a stroke outline, a dashed stroke outline — and the line's own
    // default fill, which has no area and covers nothing.
    assert_eq!(paths.len(), 4);
    for p in &paths { validate(p).unwrap() }
    // 40 px for 20 units: 2 px per unit.
    assert!(alpha_at(&paths, 5.0, 5.0) > 0.99, "inside the rect");
    assert!(alpha_at(&paths, 30.0, 30.0) < 0.01, "the circle's centre is not filled (fill: none)");
    assert!(alpha_at(&paths, 30.0, 23.5) > 0.5, "on the stroke ring (r 3 units, 2 px per unit)");
    // The dash: painted on [0,2) and [4,6) units, so x = 1 px painted, x = 5 px not.
    assert!(alpha_at(&paths, 1.0, 35.5) > 0.4, "first dash");
    assert!(alpha_at(&paths, 5.0, 35.5) < 0.01, "first gap");
    assert!(budget < usize::MAX, "the document's line share was charged");
}

#[test]
fn colour_goes_to_fresco_as_linear_rgb() {
    let svg = format!("<svg {NS} viewBox=\"0 0 1 1\"><rect width=\"1\" height=\"1\" fill=\"#808080\"/></svg>");
    let paths = from_svg(&svg, tile(4), &mut usize::MAX.clone()).unwrap();
    assert_eq!(paths[0].rgba, 0x808080ff);
    match fill_params(&paths[0], None).paint {
        fresco_protocol::FillPaint::Solid { r, .. } => assert!((r - 0.2158).abs() < 0.001, "sRGB 0x80 is linear 0.2158: {r}"),
    }
}

#[test]
fn what_a_fill_cannot_say_keeps_the_bitmap_path_with_its_reason() {
    for (body, why) in [
        ("<defs><linearGradient id=\"g\"><stop offset=\"0\" stop-color=\"red\"/><stop offset=\"1\"/></linearGradient></defs><rect width=\"5\" height=\"5\" fill=\"url(#g)\"/>", "gradient paint"),
        ("<defs><clipPath id=\"c\"><rect width=\"2\" height=\"2\"/></clipPath></defs><g clip-path=\"url(#c)\"><rect width=\"5\" height=\"5\"/></g>", "clip path"),
        ("<g opacity=\"0.5\"><rect width=\"3\" height=\"3\"/><rect x=\"2\" width=\"3\" height=\"3\"/></g>", "group opacity over several shapes"),
        ("<text>hi</text>", "text (v1)"),
    ] {
        let svg = format!("<svg {NS} viewBox=\"0 0 10 10\">{body}</svg>");
        assert_eq!(from_svg(&svg, tile(20), &mut usize::MAX.clone()).unwrap_err(), why, "{body}");
    }
    // Group opacity over ONE shape folds into its paint.
    let svg = format!("<svg {NS} viewBox=\"0 0 10 10\"><g opacity=\"0.5\"><rect width=\"3\" height=\"3\"/></g></svg>");
    let p = from_svg(&svg, tile(20), &mut usize::MAX.clone()).unwrap();
    assert_eq!(p[0].opacity, 128);
}

#[test]
fn a_path_past_frescos_bounds_is_refused_before_it_reaches_frescod() {
    // A point past ±32768 path units.
    let svg = format!("<svg {NS} viewBox=\"0 0 100000 10\"><rect x=\"90000\" width=\"5000\" height=\"10\"/></svg>");
    let e = from_svg(&svg, tile(20), &mut usize::MAX.clone()).unwrap_err();
    assert!(e.contains("bound") || e.contains("coordinate"), "{e}");
    // Out of the document's line share: kept as a bitmap.
    let svg = format!("<svg {NS} viewBox=\"0 0 10 10\"><circle cx=\"5\" cy=\"5\" r=\"4\"/></svg>");
    assert_eq!(from_svg(&svg, tile(200), &mut 3usize.clone()).unwrap_err(), "past the document's share of vector lines");
}

#[test]
fn the_nsg_line_is_exact_and_reproducible() {
    let svg = format!("<svg {NS} viewBox=\"0 0 2 2\"><path d=\"M0 0 L2 0 L2 2 Z\" fill=\"#102030\" fill-rule=\"evenodd\"/></svg>");
    let a = from_svg(&svg, tile(2), &mut usize::MAX.clone()).unwrap();
    let b = from_svg(&svg, tile(2), &mut usize::MAX.clone()).unwrap();
    assert_eq!(a, b);
    let p = &a[0];
    assert!(p.even_odd);
    assert_eq!(p.transform, [65536, 0, 0, 65536, 0, 0], "1 px per unit");
    assert_eq!(p.verbs, vec![0, 1, 1, 4]);
    assert_eq!(p.points, vec![(0, 0), (128, 0), (128, 128)]);
}
