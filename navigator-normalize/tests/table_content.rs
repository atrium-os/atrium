//! ★ WHAT A TABLE CELL HOLDS: wrapped text, an image, a nested table — each
//! laid out inside its cell at every zoom (backend §4.9), through the whole
//! producer + renderer path.

use navigator_normalize::{normalize_and_measure, Inputs};
use navigator_render::{fontset::FontSet, html::layout_geometry, html::render_html, Subresources};
use navigator_style::cascade::Env;

const DOC: &str = r#"<html><head><style>body { margin: 0px } td { border: 1px solid #888888; vertical-align: top }
    img { width: 120px; height: 80px }</style></head><body>
    <table k="1"><tr>
      <td k="2">Short</td>
      <td k="3">A long sentence of ordinary words that has to wrap onto several lines once the window or the zoom leaves it little room, as table cells do.</td>
      <td k="4"><img k="5" src="a.png" alt="x"> caption under the picture</td>
      <td k="6"><table k="7"><tr><td k="8">inner one</td><td k="9">inner two, which also wraps when it must</td></tr>
                              <tr><td k="10" colspan="2">a spanning inner row</td></tr></table></td>
    </tr></table></body></html>"#;

#[test]
fn cells_hold_wrapped_text_images_and_nested_tables_at_every_zoom() {
    let fonts = FontSet::load().unwrap();
    // The image's natural size is an input, as the converter records it.
    let mut inputs = Inputs::default();
    inputs.images.insert("a.png".into(), (120, 80).into());
    let (doc, _) = normalize_and_measure(DOC, &inputs, &fonts, &Env::default());
    for zoom in [64u32, 128, 192] {
        for env in [Env::zoomed(800.0, 600.0, zoom), Env { text_scale_64: zoom, ..Env::default() }] {
            let o = render_html(&doc, &fonts, &env);
            assert!(o.diagnostics.is_empty(), "zoom {zoom}: {:?}", o.diagnostics);
            assert!(o.unimplemented.keys().all(|k| !k.contains("nested table")), "zoom {zoom}: {:?}", o.unimplemented);
            let g = layout_geometry(&doc, &fonts, &env, &Subresources::new(), "k");
            let inside = |a: u64, b: u64| {
                let (x, y) = (&g.elements[&a], &g.elements[&b]);
                x.x >= y.x && x.y >= y.y && x.x + x.w <= y.x + y.w && x.y + x.h <= y.y + y.h
            };
            // The long text wraps: its cell is taller than the short one's line.
            // (every cell is as tall as its row; a one-line row is the yardstick)
            assert!(g.elements[&3].h > 2 * g.elements[&10].h, "zoom {zoom}: the long cell wraps");
            assert!(inside(5, 4), "zoom {zoom}: the image is inside its cell");
            for k in [7u64, 8, 9, 10] { assert!(inside(k, 6), "zoom {zoom}: nested part {k} is inside the outer cell") }
            assert!(inside(8, 7) && inside(9, 7) && inside(10, 7), "zoom {zoom}: inner cells inside the inner table");
            assert!(g.elements[&10].w >= g.elements[&8].w + g.elements[&9].w, "zoom {zoom}: the inner colspan spans both columns");
        }
    }
}
