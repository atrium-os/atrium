//! nsg-raster <file.html|file.md> <out.png> [width_px]
//!            [--recording <file.json>] [--blobs <dir>]
//!
//! With a recording and a blob directory, an image PAINTS: the recording
//! says which address a `src` has, and the directory holds the bytes under
//! that address — the same two steps the worker takes against Tessera.
//!
//! REVIEW TOOLING, not the renderer: paints a Scene to pixels so a person can
//! look at what the NSG says before blessing it as a golden. Rects are filled
//! with source-over blending; glyphs are rasterized UNHINTED from the same
//! canonical font bytes the scene names (web-font condition 2a), at the run's
//! sub-pixel position.

use navigator_render::{fontset::FontSet, raster::paint, render, Options};
use navigator_style::cascade::Env;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 { eprintln!("usage: nsg-raster <file.html|file.md> <out.png> [width_px]"); std::process::exit(2) }
    let width: i64 = a.get(3).and_then(|w| w.parse().ok()).unwrap_or(800);
    let fonts = FontSet::load().expect("pinned font set");
    let mut assets: std::collections::BTreeMap<String, std::path::PathBuf> = Default::default();
    let src = std::fs::read_to_string(&a[1]).expect("readable input");
    let scene = if a[1].ends_with(".md") {
        render(&src, &fonts, &Options { width_px: width }).0
    } else {
        let flag = |name: &str| a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned();
        let subs = match flag("--recording") {
            Some(rec) => navigator_render::conformance::subresources_from_recording(std::path::Path::new(&rec)),
            None => navigator_render::conformance::subresources(std::path::Path::new(&a[1])),
        };
        // The review tool needs the BYTES the renderer never sees, to show
        // what the scene refers to: address -> file.
        assets = match flag("--blobs") {
            Some(dir) => subs.values().filter_map(|(addr, ..)| {
                let hex = addr.strip_prefix("blake3:")?;
                let p = std::path::Path::new(&dir).join(hex);
                p.exists().then(|| (addr.clone(), p))
            }).collect(),
            None => navigator_render::conformance::subresource_files(std::path::Path::new(&a[1])),
        };
        let o = navigator_render::html::render_html_with(&src, &fonts, &Env { width_px: width as f64, ..Env::default() }, &subs);
        for d in &o.diagnostics { eprintln!("diagnostic {} {}: {}", d.pos, d.code, d.msg) }
        for (k, n) in &o.unimplemented { eprintln!("unimplemented ×{n}: {k}") }
        o.scene
    };
    let cv = paint(&scene, &fonts, &assets);
    let file = std::fs::File::create(&a[2]).expect("writable output");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), cv.w as u32, cv.h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().expect("png header");
    let data: Vec<u8> = cv.rgb8();
    wr.write_image_data(&data).expect("png data");
    eprintln!("wrote {} ({}x{})", a[2], cv.w, cv.h);
}
