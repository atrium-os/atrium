//! Inline SVG as vector shapes (src/path.rs): NSG `path` nodes, one to one
//! with Fresco's ATRIUM_CORE_FILL. Painted here by fresco-compute's own
//! coverage rule, so a test checks what frescod would draw.
use navigator_render::path::{fill_params, from_svg, paint_at, validate, Paint, Path};

const NS: &str = "xmlns=\"http://www.w3.org/2000/svg\"";

/// Coverage of pixel (x, y), window pixels, under every path: src-over of
/// straight-alpha colours, the alpha channel only.
fn alpha_at(paths: &[Path], x: f32, y: f32) -> f32 {
    let mut a = 0.0f32;
    for p in paths {
        let fp = fill_params(p, None);
        let f = fresco_compute::fill::Fill { verbs: fp.verbs, points: fp.points, transform: fp.transform,
            rule: fresco_compute::fill::FillRule::NonZero, color: [0.0; 4], clip: None, gradient: Default::default() };
        let lines = fresco_compute::fill::flatten(&f).expect("flattens");
        let acc: f32 = lines.iter().map(|s| fresco_compute::fill::fill_line(*s, x, y)).sum();
        let paint_a = paint_at(p, x, y).map(|c| c[3]).unwrap_or(0.0);
        let c = fresco_compute::fill::coverage(acc, p.even_odd) * fp.opacity * paint_a;
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
    assert_eq!(paths[0].paint, Paint::Solid(0x808080ff));
    match fill_params(&paths[0], None).paint {
        fresco_protocol::FillPaint::Solid { r, .. } => assert!((r - 0.2158).abs() < 0.001, "sRGB 0x80 is linear 0.2158: {r}"),
        other => panic!("{other:?}"),
    }
    // A gradient's stops, by contrast, go to Fresco in sRGB.
    let svg = format!("<svg {NS} viewBox=\"0 0 4 1\"><defs><linearGradient id=\"g\"><stop offset=\"0\" stop-color=\"#808080\"/><stop offset=\"1\" stop-color=\"#000\"/></linearGradient></defs><rect width=\"4\" height=\"1\" fill=\"url(#g)\"/></svg>");
    let p = from_svg(&svg, tile(4), &mut usize::MAX.clone()).unwrap();
    match fill_params(&p[0], None).paint {
        fresco_protocol::FillPaint::Linear { stops, .. } => assert!((stops[0].r - 128.0 / 255.0).abs() < 1e-6, "sRGB stop: {}", stops[0].r),
        other => panic!("{other:?}"),
    }
}

#[test]
fn what_a_fill_cannot_say_keeps_the_bitmap_path_with_its_reason() {
    for (body, why) in [
        ("<defs><linearGradient id=\"g\" spreadMethod=\"reflect\" x2=\"0.5\"><stop offset=\"0\" stop-color=\"red\"/><stop offset=\"1\"/></linearGradient></defs><rect width=\"5\" height=\"5\" fill=\"url(#g)\"/>", "gradient spread other than pad"),
        (&format!("<defs><linearGradient id=\"g\">{}</linearGradient></defs><rect width=\"5\" height=\"5\" fill=\"url(#g)\"/>",
            (0..17).map(|i| format!("<stop offset=\"{}\" stop-color=\"#{:02x}0000\"/>", i as f32 / 16.0, i * 15)).collect::<String>()), "gradient stop count"),
        ("<defs><clipPath id=\"c\"><circle cx=\"2\" cy=\"2\" r=\"2\"/></clipPath></defs><g clip-path=\"url(#c)\"><rect width=\"5\" height=\"5\"/></g>", "clip path"),
        ("<defs><clipPath id=\"c\"><rect width=\"2\" height=\"2\" transform=\"rotate(30)\"/></clipPath></defs><g clip-path=\"url(#c)\"><rect width=\"5\" height=\"5\"/></g>", "clip path"),
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

#[test]
fn a_rectangle_clip_goes_vector_as_the_fills_own_clip() {
    // The Figma export pattern: the whole drawing under a viewport-sized rectangle clip.
    let svg = format!("<svg {NS} viewBox=\"0 0 10 10\"><g clip-path=\"url(#c)\"><rect x=\"-5\" y=\"-5\" width=\"20\" height=\"20\" fill=\"#00ff00\"/></g>\
        <defs><clipPath id=\"c\"><rect x=\"2\" y=\"2\" width=\"6\" height=\"6\"/></clipPath></defs></svg>");
    let p = from_svg(&svg, tile(10), &mut usize::MAX.clone()).expect("vector");
    assert_eq!(p[0].clip, Some([2 * 64, 2 * 64, 8 * 64, 8 * 64]), "1 px per unit, in 1/64 px");
    assert!(alpha_at(&p, 5.0, 5.0) > 0.99, "inside the clip");
    assert!(alpha_at(&p, 1.0, 1.0) < 0.01, "outside the clip");
    assert_eq!(fill_params(&p[0], Some([0.0, 0.0, 6.0, 100.0])).clip, Some([2.0, 2.0, 6.0, 8.0]), "intersected with the scene's clip");
}

#[test]
fn a_linear_gradient_is_exact_under_a_non_uniform_bounding_box() {
    // objectBoundingBox on a 40x10 rect: the gradient's own space is scaled
    // non-uniformly, and a diagonal gradient's isolines must still be where
    // the SVG puts them.
    let svg = format!("<svg {NS} viewBox=\"0 0 40 10\"><defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\">\
        <stop offset=\"0\" stop-color=\"#000000\"/><stop offset=\"1\" stop-color=\"#ffffff\"/></linearGradient></defs>\
        <rect width=\"40\" height=\"10\" fill=\"url(#g)\"/></svg>");
    // A 40 x 10 px tile: 1 px per user unit.
    let p = from_svg(&svg, (0, 0, 40 * 64, 10 * 64), &mut usize::MAX.clone()).expect("vector");
    // In bounding-box units t(u, v) = (u + v) / 2, with u = x/40 and v = y/10.
    for (x, y) in [(3.0f32, 2.0f32), (20.0, 5.0), (35.0, 8.0), (10.0, 9.0), (39.0, 0.0)] {
        let (cx, cy) = (x + 0.5, y + 0.5);
        let want = ((cx / 40.0 + cy / 10.0) / 2.0).clamp(0.0, 1.0);
        let got = paint_at(&p[0], x, y).unwrap()[0];
        assert!((got - want).abs() < 0.01, "at ({x},{y}): {got} vs {want}");
    }
}

#[test]
fn a_radial_gradient_goes_vector_and_one_with_a_focus_does_not() {
    let svg = format!("<svg {NS} viewBox=\"0 0 10 10\"><defs><radialGradient id=\"g\" gradientUnits=\"userSpaceOnUse\" cx=\"5\" cy=\"5\" r=\"5\">\
        <stop offset=\"0\" stop-color=\"#ffffff\"/><stop offset=\"1\" stop-color=\"#000000\"/></radialGradient></defs>\
        <rect width=\"10\" height=\"10\" fill=\"url(#g)\"/></svg>");
    let p = from_svg(&svg, tile(10), &mut usize::MAX.clone()).expect("vector");
    assert!(paint_at(&p[0], 4.5, 4.5).unwrap()[0] > 0.9, "white at the centre");
    assert!(paint_at(&p[0], 0.0, 0.0).unwrap()[0] < 0.05, "black past the radius (pad)");
    let focal = svg.replace("r=\"5\">", "r=\"5\" fx=\"3\">");
    assert_eq!(from_svg(&focal, tile(10), &mut usize::MAX.clone()).unwrap_err(), "radial gradient with a focus");
}

#[test]
fn a_linear_gradient_is_exact_under_a_skewed_gradient_transform() {
    // gradientTransform skewX(30): t(x) = (G⁻¹x - q0)·d / |d|², G = [[1, tan30], [0, 1]].
    let svg = format!("<svg {NS} viewBox=\"0 0 20 20\"><defs><linearGradient id=\"g\" gradientUnits=\"userSpaceOnUse\" \
        x1=\"0\" y1=\"0\" x2=\"10\" y2=\"5\" gradientTransform=\"skewX(30)\"><stop offset=\"0\" stop-color=\"#000000\"/>\
        <stop offset=\"1\" stop-color=\"#ffffff\"/></linearGradient></defs><rect width=\"20\" height=\"20\" fill=\"url(#g)\"/></svg>");
    let p = from_svg(&svg, tile(20), &mut usize::MAX.clone()).expect("vector");
    let k = 30f32.to_radians().tan();
    for (x, y) in [(2.0f32, 1.0f32), (8.0, 3.0), (5.0, 12.0), (14.0, 6.0)] {
        let (cx, cy) = (x + 0.5, y + 0.5);
        let (gx, gy) = (cx - k * cy, cy); // G⁻¹
        let want = ((gx * 10.0 + gy * 5.0) / 125.0).clamp(0.0, 1.0);
        let got = paint_at(&p[0], x, y).unwrap()[0];
        assert!((got - want).abs() < 0.01, "at ({x},{y}): {got} vs {want}");
    }
}

#[test]
fn every_vector_svg_keeps_its_bitmap_fallback_for_a_dropped_fill() {
    use navigator_render::{fontset::FontSet, html::render_html_with, path::fallback_for};
    let svg = format!("<svg {NS} viewBox=\"0 0 10 10\"><rect width=\"10\" height=\"10\" fill=\"#ff0000\"/><circle cx=\"5\" cy=\"5\" r=\"3\" fill=\"#0000ff\"/></svg>");
    let doc = format!("<html><body><p>x</p><svg src=\"blake3:{}\" width=\"20\" height=\"20\" atrium-svg=\"{}\"></svg></body></html>",
        "a".repeat(64), svg.replace('"', "&quot;"));
    let fonts = FontSet::load().unwrap();
    let mut subs = navigator_render::Subresources::new();
    subs.insert(format!("blake3:{}", "a".repeat(64)), (format!("blake3:{}", "a".repeat(64)), 20 * 64, 20 * 64));
    let o = render_html_with(&doc, &fonts, &Default::default(), &subs);
    assert_eq!(o.scene.paths.len(), 2, "{:?}", o.unimplemented);
    let (first, n, im) = fallback_for(&o.scene, 1).expect("a fallback");
    assert_eq!((*first, *n), (0, 2));
    assert_eq!(im.address, format!("blake3:{}", "a".repeat(64)));
    assert!(o.scene.images.is_empty(), "the bitmap is the fallback, not drawn as well");
    let nsg = navigator_render::nsg::write(&o.scene, &fonts);
    assert!(nsg.contains("\nfallback p0+2 blake3:"), "{nsg}");
}
