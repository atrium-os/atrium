//! nsg-raster <file.html|file.md> <out.png> [width_px]
//!
//! REVIEW TOOLING, not the renderer: paints a Scene to pixels so a person can
//! look at what the NSG says before blessing it as a golden. Rects are filled
//! with source-over blending; glyphs are rasterized UNHINTED from the same
//! canonical font bytes the scene names (web-font condition 2a), at the run's
//! sub-pixel position.

use navigator_render::{fontset::FontSet, html::render_html, render, Options, Scene, PX};
use navigator_style::cascade::Env;

struct Canvas {
    w: usize, h: usize, px: Vec<[f32; 4]>,
    /// The clip rectangle in force, in whole pixels (x0, y0, x1, y1).
    clip: Option<(i64, i64, i64, i64)>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self { Canvas { w, h, px: vec![[1.0, 1.0, 1.0, 1.0]; w * h], clip: None } }
    /// A transparent canvas the same size: an opacity group is painted here
    /// and composited once, which is what makes group opacity different from
    /// multiplying each node's alpha.
    fn layer(&self) -> Self { Canvas { w: self.w, h: self.h, px: vec![[0.0; 4]; self.w * self.h], clip: self.clip } }
    fn over(&mut self, layer: &Canvas, alpha: f32) {
        for (dst, src) in self.px.iter_mut().zip(layer.px.iter()) {
            let a = src[3] * alpha;
            if a <= 0.0 { continue }
            for i in 0..3 { dst[i] = src[i] / src[3].max(1e-6) * a + dst[i] * (1.0 - a) }
            dst[3] = a + dst[3] * (1.0 - a);
        }
    }
    fn blend(&mut self, x: i64, y: i64, rgba: u32, cover: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h { return }
        if let Some((x0, y0, x1, y1)) = self.clip { if x < x0 || y < y0 || x >= x1 || y >= y1 { return } }
        let a = (rgba & 0xff) as f32 / 255.0 * cover;
        let c = [(rgba >> 24) as f32 / 255.0, ((rgba >> 16) & 0xff) as f32 / 255.0, ((rgba >> 8) & 0xff) as f32 / 255.0];
        let p = &mut self.px[y as usize * self.w + x as usize];
        for i in 0..3 { p[i] = c[i] * a + p[i] * (1.0 - a) }
        p[3] = a + p[3] * (1.0 - a);
    }
}

fn paint(scene: &Scene, fonts: &FontSet) -> Canvas {
    let (w, h) = ((scene.width / PX) as usize, (scene.height / PX) as usize);
    let mut cv = Canvas::new(w.max(1), h.max(1));
    let mut ctx = swash::scale::ScaleContext::new();
    // Opacity groups, innermost last: each is a layer of its own, composited
    // into its parent when the last node belonging to it has been painted.
    let mut stack: Vec<(u32, Canvas)> = vec![];
    let group_of = |kind: u8, i: usize| -> Option<u32> {
        match kind { 0 => scene.rect_attrs.get(i), 1 => scene.run_attrs.get(i), _ => None }.and_then(|a| a.1)
    };
    let clip_of = |kind: u8, i: usize| -> Option<u32> {
        match kind { 0 => scene.rect_attrs.get(i), 1 => scene.run_attrs.get(i), _ => None }.and_then(|a| a.0)
    };
    // The chain of groups a node is in, outermost first.
    let chain = |g: Option<u32>| -> Vec<u32> {
        let (mut out, mut cur) = (vec![], g);
        while let Some(i) = cur { out.push(i); cur = scene.groups.get(i as usize).and_then(|g| g.1) }
        out.reverse();
        out
    };
    for &(kind, i) in &scene.order {
        // Close the groups this node is not in, then open the ones it is.
        let want = chain(group_of(kind, i));
        while stack.len() > want.len() || stack.last().is_some_and(|(id, _)| !want.contains(id)) {
            let (id, layer) = stack.pop().expect("group");
            let alpha = scene.groups.get(id as usize).map(|g| g.0).unwrap_or(255) as f32 / 255.0;
            match stack.last_mut() { Some((_, parent)) => parent.over(&layer, alpha), None => cv.over(&layer, alpha) }
        }
        for id in want.into_iter().skip(stack.len()) {
            let l = stack.last().map(|(_, c)| c.layer()).unwrap_or_else(|| cv.layer());
            stack.push((id, l));
        }
        let cvr: &mut Canvas = match stack.last_mut() { Some((_, c)) => c, None => &mut cv };
        cvr.clip = clip_of(kind, i).and_then(|c| scene.clips.get(c as usize)).map(|&(x, y, w, h)| (x / PX, y / PX, (x + w) / PX, (y + h) / PX));
        let cv = cvr;
        match kind {
            0 => {
                let r = &scene.rects[i];
                if r.radii != [0; 4] || r.ring != 0 {
                    // Rounded and/or ringed: 4×4 supersampling of the test.
                    let inside_at = |sx: f32, sy: f32, inset: f32, radii: [f32; 4]| {
                        let (x0, y0) = (r.x as f32 + inset, r.y as f32 + inset);
                        let (x1, y1) = ((r.x + r.w) as f32 - inset, (r.y + r.h) as f32 - inset);
                        if sx < x0 || sx >= x1 || sy < y0 || sy >= y1 { return false }
                        // Corner: top-left, top-right, bottom-right, bottom-left.
                        let (i, cx, cy) = if sx < x0 + radii[0] && sy < y0 + radii[0] { (0, x0 + radii[0], y0 + radii[0]) }
                            else if sx > x1 - radii[1] && sy < y0 + radii[1] { (1, x1 - radii[1], y0 + radii[1]) }
                            else if sx > x1 - radii[2] && sy > y1 - radii[2] { (2, x1 - radii[2], y1 - radii[2]) }
                            else if sx < x0 + radii[3] && sy > y1 - radii[3] { (3, x0 + radii[3], y1 - radii[3]) }
                            else { return true };
                        let rad = radii[i];
                        rad <= 0.0 || (sx - cx).powi(2) + (sy - cy).powi(2) <= rad * rad
                    };
                    let outer = r.radii.map(|v| v.min(r.w / 2).min(r.h / 2) as f32);
                    let ring = r.ring as f32;
                    let inner = r.radii.map(|v| (v - r.ring).max(0).min((r.w - 2 * r.ring).max(0) / 2).min((r.h - 2 * r.ring).max(0) / 2) as f32);
                    let inside = |sx: f32, sy: f32| inside_at(sx, sy, 0.0, outer)
                        && !(ring > 0.0 && inside_at(sx, sy, ring, inner));
                    for py in (r.y / PX)..=((r.y + r.h) / PX) {
                        for px in (r.x / PX)..=((r.x + r.w) / PX) {
                            let mut n = 0;
                            for j in 0..4 { for k in 0..4 {
                                if inside((px * PX) as f32 + (k as f32 + 0.5) * 16.0, (py * PX) as f32 + (j as f32 + 0.5) * 16.0) { n += 1 }
                            } }
                            if n > 0 { cv.blend(px, py, r.rgba, n as f32 / 16.0) }
                        }
                    }
                    continue;
                }
                // Coverage-exact on 1/64 px edges.
                let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
                for py in (y0 / PX)..=((y1 - 1).max(y0) / PX) {
                    let cy = ((y1.min((py + 1) * PX) - y0.max(py * PX)).max(0)) as f32 / PX as f32;
                    for px in (x0 / PX)..=((x1 - 1).max(x0) / PX) {
                        let cx = ((x1.min((px + 1) * PX) - x0.max(px * PX)).max(0)) as f32 / PX as f32;
                        cv.blend(px, py, r.rgba, cx * cy);
                    }
                }
            }
            1 => {
                let r = &scene.runs[i];
                let face = &fonts.faces[r.face];
                let Some(font) = swash::FontRef::from_index(&face.bytes, 0) else { continue };
                let mut scaler = ctx.builder(font).size(r.size as f32 / PX as f32).hint(false).build();
                for &(gid, dx, dy) in &r.glyphs {
                    let (gx, gy) = (r.x + dx, r.y + dy);
                    let off = swash::zeno::Vector::new((gx % PX) as f32 / PX as f32, 0.0);
                    let Some(img) = swash::scale::Render::new(&[swash::scale::Source::Outline])
                        .format(swash::zeno::Format::Alpha).offset(off)
                        .render(&mut scaler, swash::GlyphId::from(gid)) else { continue };
                    let (ox, oy) = (gx / PX + img.placement.left as i64, gy / PX - img.placement.top as i64);
                    for yy in 0..img.placement.height as i64 {
                        for xx in 0..img.placement.width as i64 {
                            let a = img.data[(yy * img.placement.width as i64 + xx) as usize];
                            if a > 0 { cv.blend(ox + xx, oy + yy, r.rgba, a as f32 / 255.0) }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // Any group still open at the end composites now.
    while let Some((id, layer)) = stack.pop() {
        let alpha = scene.groups.get(id as usize).map(|g| g.0).unwrap_or(255) as f32 / 255.0;
        match stack.last_mut() { Some((_, parent)) => parent.over(&layer, alpha), None => cv.over(&layer, alpha) }
    }
    cv
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 { eprintln!("usage: nsg-raster <file.html|file.md> <out.png> [width_px]"); std::process::exit(2) }
    let width: i64 = a.get(3).and_then(|w| w.parse().ok()).unwrap_or(800);
    let fonts = FontSet::load().expect("pinned font set");
    let src = std::fs::read_to_string(&a[1]).expect("readable input");
    let scene = if a[1].ends_with(".md") {
        render(&src, &fonts, &Options { width_px: width }).0
    } else {
        let o = render_html(&src, &fonts, &Env { width_px: width as f64, ..Env::default() });
        for d in &o.diagnostics { eprintln!("diagnostic {} {}: {}", d.pos, d.code, d.msg) }
        for (k, n) in &o.unimplemented { eprintln!("unimplemented ×{n}: {k}") }
        o.scene
    };
    let cv = paint(&scene, &fonts);
    let file = std::fs::File::create(&a[2]).expect("writable output");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), cv.w as u32, cv.h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().expect("png header");
    let data: Vec<u8> = cv.px.iter().flat_map(|p| [0, 1, 2].map(|i| (p[i].clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect();
    wr.write_image_data(&data).expect("png data");
    eprintln!("wrote {} ({}x{})", a[2], cv.w, cv.h);
}
