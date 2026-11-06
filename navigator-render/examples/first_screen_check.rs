//! first_screen_check <normalized.html>… [--limit PX] [--screen PX]: bands
//! phase 1 on real documents. For each: the full layout and the first-screen
//! layout (`render_html_first`, to `--limit` px, default 1200 = two screens),
//! timed, and their pixels over the first screen `[0, screen)` (default 600)
//! compared at 8 bits per channel — what reaches a display. The margin
//! between screen and limit is the design's: a shadow or a negative margin
//! from just below the cut can reach up into it. One TSV line per document:
//! `stem  full_ms  first_ms  nodes_full  nodes_first  truncated  pixels_equal`.
use navigator_render::{bands, fontset::FontSet, html::{render_html, render_html_first}, raster, Scene};
use navigator_style::cascade::Env;
use std::time::Instant;

const PX: i64 = 64;

fn rows(scene: &Scene, fonts: &FontSet, h: usize) -> Vec<[u8; 4]> {
    let mut s = bands::scene_of(scene, &(0..scene.order.len() as u32).collect::<Vec<_>>());
    s.height = h as i64 * PX;
    raster::paint(&s, fonts, &Default::default()).px.iter().map(|p| p.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let limit: usize = args.iter().position(|a| a == "--limit").and_then(|i| args.get(i + 1)).map(|v| v.parse().unwrap()).unwrap_or(1200);
    let screen: usize = args.iter().position(|a| a == "--screen").and_then(|i| args.get(i + 1)).map(|v| v.parse().unwrap()).unwrap_or(600);
    let fonts = FontSet::load().unwrap();
    let env = Env::default();
    for p in args.iter().filter(|a| a.ends_with(".html")) {
        let html = std::fs::read_to_string(p).unwrap();
        let stem = std::path::Path::new(p).file_stem().unwrap().to_string_lossy().to_string();
        let t = Instant::now();
        let full = render_html(&html, &fonts, &env).scene;
        let full_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let (first, truncated) = render_html_first(&html, &fonts, &env, &Default::default(), limit as i64 * PX);
        let first_ms = t.elapsed().as_secs_f64() * 1e3;
        let h = screen.min((full.height / PX).max(1) as usize);
        let (a, b) = (rows(&first.scene, &fonts, h), rows(&full, &fonts, h));
        let equal = a == b;
        let w = (full.width / PX).max(1) as usize;
        let maxd = a.iter().zip(&b).map(|(x, y)| (0..4).map(|c| x[c].abs_diff(y[c])).max().unwrap()).max().unwrap_or(0);
        let ndiff = a.iter().zip(&b).filter(|(x, y)| x != y).count();
        let first_diff = a.iter().zip(&b).position(|(x, y)| x != y).map(|i| format!("\tfirst_diff_y {}\tpixels {ndiff}\tmax_level_diff {maxd}", i / w)).unwrap_or_default();
        if std::env::var_os("DIFF_NODES").is_some() && !equal {
            let pi = a.iter().zip(&b).position(|(x, y)| x != y).unwrap();
            let y = pi / w;
            let ndiff = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            let xs: Vec<usize> = a.iter().zip(&b).enumerate().filter(|(_, (x, y))| x != y).map(|(i, _)| i % w).collect();
            eprintln!("{stem}: {ndiff} pixels differ; first at x={} y={y}: first {:?} full {:?}; x range {}..{}; y range {}..{}", pi % w, a[pi], b[pi],
                      xs.iter().min().unwrap(), xs.iter().max().unwrap(), y, a.iter().zip(&b).rposition(|(x, y)| x != y).unwrap() / w);
            let yy = y as i64 * PX;
            for (name, sc) in [("first", &first.scene), ("full", &full)] {
                let idx = bands::index(sc, 64 * PX);
                let mut lines = vec![];
                for o in idx.nodes_in(yy, yy + PX) {
                    let (k, i) = sc.order[o as usize];
                    let d = match k {
                        0 => { let r = &sc.rects[i]; let a = sc.rect_attrs[i];
                               format!("rect {} {} {} {} {:08x} r{:?} ring{} clip{:?} g{:?} x{:?}", r.x / PX, r.y / PX, r.w / PX, r.h / PX, r.rgba, r.radii, r.ring,
                                       a.0.map(|c| sc.clips[c as usize]), a.1, a.2.map(|x| sc.xforms[x as usize])) }
                        1 => { let r = &sc.runs[i]; let a = sc.run_attrs[i]; format!("run {} {} {:?} clip{:?}", r.x / PX, r.y / PX, r.text, a.0.map(|c| sc.clips[c as usize])) }
                        5 => { let a = &sc.images[i].area; format!("image {} {} {} {}", a.x / PX, a.y / PX, a.w / PX, a.h / PX) }
                        k => format!("kind {k}"),
                    };
                    lines.push(d);
                }
                eprintln!("{stem} {name} @y={y}: {} nodes", lines.len());
                for l in &lines { eprintln!("   {l}") }
            }
        }
        println!("{stem}\t{full_ms:.1}\t{first_ms:.1}\t{}\t{}\t{}\t{}{first_diff}", full.order.len(), first.scene.order.len(), truncated as u8, equal as u8);
    }
}
