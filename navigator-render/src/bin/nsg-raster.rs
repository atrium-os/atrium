//! nsg-raster <file.html|file.md> <out.png> [width_px]
//!
//! REVIEW TOOLING, not the renderer: paints a Scene to pixels so a person can
//! look at what the NSG says before blessing it as a golden. Rects are filled
//! with source-over blending; glyphs are rasterized UNHINTED from the same
//! canonical font bytes the scene names (web-font condition 2a), at the run's
//! sub-pixel position.

use navigator_render::{fontset::FontSet, render, Options, Rect, Scene, PX};
use navigator_style::cascade::Env;

struct Canvas {
    w: usize, h: usize, px: Vec<[f32; 4]>,
    /// The clip rectangle in force, in whole pixels (x0, y0, x1, y1).
    clip: Option<(i64, i64, i64, i64)>,
}

/// Where a device pixel came from before the transform: the inverse map, in
/// 1/64 px. Painting is done by walking the DESTINATION pixels of the
/// transformed bounding box and asking what was there, which needs no
/// polygon rasterizer and gets rotation right.
fn invert(m: &navigator_render::Xform) -> Option<[f64; 6]> {
    let k = navigator_render::XF_ONE as f64;
    let (a, b, c, d) = (m[0] as f64 / k, m[1] as f64 / k, m[2] as f64 / k, m[3] as f64 / k);
    let (e, f) = (m[4] as f64, m[5] as f64);
    let det = a * d - b * c;
    if det.abs() < 1e-9 { return None }
    // Inverse of [a c e; b d f].
    let (ia, ib, ic, id) = (d / det, -b / det, -c / det, a / det);
    Some([ia, ib, ic, id, -(ia * e + ic * f), -(ib * e + id * f)])
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


/// Paint ONE node with no transform of its own. Factored out so the
/// transform path can render a node into a layer and map it into place.
fn paint_one(cv: &mut Canvas, scene: &Scene, fonts: &FontSet, ctx: &mut swash::scale::ScaleContext, kind: u8, i: usize,
             assets: &std::collections::BTreeMap<String, std::path::PathBuf>) {
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
                return;
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
            let Some(font) = swash::FontRef::from_index(&face.bytes, 0) else { return };
            let mut scaler = ctx.builder(font).size(r.size as f32 / PX as f32).hint(false).build();
            for &(gid, dx, dy) in &r.glyphs {
                let (gx, gy) = (r.x + dx, r.y + dy);
                let off = swash::zeno::Vector::new((gx % PX) as f32 / PX as f32, 0.0);
                let Some(img) = swash::scale::Render::new(&[swash::scale::Source::Outline])
                    .format(swash::zeno::Format::Alpha).offset(off)
                    .render(&mut scaler, swash::GlyphId::from(gid)) else { return };
                let (ox, oy) = (gx / PX + img.placement.left as i64, gy / PX - img.placement.top as i64);
                for yy in 0..img.placement.height as i64 {
                    for xx in 0..img.placement.width as i64 {
                        let a = img.data[(yy * img.placement.width as i64 + xx) as usize];
                        if a > 0 { cv.blend(ox + xx, oy + yy, r.rgba, a as f32 / 255.0) }
                    }
                }
            }
        }
        3 => {
            let sh = &scene.shadows[i];
            // The shadow is painted into a layer of its own and blurred
            // there: three box passes are a close enough Gaussian for a
            // review render, with sigma = blur / 2 as CSS specifies.
            let mut layer = Canvas { w: cv.w, h: cv.h, px: vec![[0.0; 4]; cv.w * cv.h], clip: None };
            let mut sub = Scene { width: scene.width, height: scene.height, ..Default::default() };
            sub.order.push((0, 0));
            sub.rects.push(Rect { x: sh.x, y: sh.y, w: sh.w, h: sh.h, rgba: sh.rgba, radii: sh.radii, ring: 0 });
            sub.rect_attrs.push((None, None, None));
            paint_one(&mut layer, &sub, fonts, ctx, 0, 0, assets);
            let sigma = sh.blur as f64 / PX as f64 / 2.0;
            if sigma > 0.0 {
                let bw = ((sigma * 3.0 * (2.0 * std::f64::consts::PI).sqrt() / 4.0 + 0.5) as usize).max(1);
                for _ in 0..3 { box_blur(&mut layer, bw) }
            }
            for y in 0..cv.h as i64 {
                for x in 0..cv.w as i64 {
                    let src = layer.px[y as usize * layer.w + x as usize];
                    if src[3] > 0.0 { cv.blend(x, y, sh.rgba & !0xff | 0xff, src[3]) }
                }
            }
        }
        4 => {
            let g = &scene.grads[i];
            let (a, k) = (&g.area, 1024.0f64);
            let (rad, l) = {
                let deg = g.angle as f64 / 64.0;
                let (sin, cos) = navigator_render::sin_cos_deg(deg);
                // CSS's gradient line: through the tile's centre, long enough
                // that the corners map to 0 and 1.
                ((sin, cos), (a.tw as f64 * sin.abs() + a.th as f64 * cos.abs()).max(1.0))
            };
            for py in (a.y / PX)..((a.y + a.h) / PX) {
                for px in (a.x / PX)..((a.x + a.w) / PX) {
                    // Into tile space, wrapping only on the axes that repeat.
                    let (mut sx, mut sy) = ((px * PX - a.tx) as f64, (py * PX - a.ty) as f64);
                    if a.repeat & 1 != 0 { sx = sx.rem_euclid(a.tw as f64) } else if sx < 0.0 || sx >= a.tw as f64 { continue }
                    if a.repeat & 2 != 0 { sy = sy.rem_euclid(a.th as f64) } else if sy < 0.0 || sy >= a.th as f64 { continue }
                    let (cx, cy) = (a.tw as f64 / 2.0, a.th as f64 / 2.0);
                    let t = (((sx - cx) * rad.0 - (sy - cy) * rad.1) / l + 0.5).clamp(0.0, 1.0) * k;
                    // The pair of stops around t.
                    let mut col = g.stops.first().map(|s| s.0).unwrap_or(0);
                    for w in g.stops.windows(2) {
                        let ((c0, p0), (c1, p1)) = (w[0], w[1]);
                        if t >= p0 as f64 && t <= p1 as f64 {
                            let f = if p1 > p0 { (t - p0 as f64) / (p1 - p0) as f64 } else { 0.0 };
                            let mix = |sh: u32| {
                                let (a0, a1) = (((c0 >> sh) & 0xff) as f64, ((c1 >> sh) & 0xff) as f64);
                                (a0 + (a1 - a0) * f).round() as u32
                            };
                            col = mix(24) << 24 | mix(16) << 16 | mix(8) << 8 | mix(0);
                            break;
                        }
                        if t > p1 as f64 { col = c1 }
                    }
                    cv.blend(px, py, col, 1.0);
                }
            }
        }
        5 => {
            let im = &scene.images[i];
            let a = &im.area;
            let Some(path) = assets.get(&im.address) else { return };
            let Ok(file) = std::fs::File::open(path) else { return };
            let Ok(mut reader) = png::Decoder::new(file).read_info() else { return };
            let mut buf = vec![0; reader.output_buffer_size()];
            let Ok(info) = reader.next_frame(&mut buf) else { return };
            let (iw, ih, ch) = (info.width as i64, info.height as i64, info.color_type.samples());
            for py in (a.y / PX)..((a.y + a.h) / PX) {
                for px in (a.x / PX)..((a.x + a.w) / PX) {
                    let (mut sx, mut sy) = ((px * PX - a.tx) as f64, (py * PX - a.ty) as f64);
                    if a.repeat & 1 != 0 { sx = sx.rem_euclid(a.tw as f64) } else if sx < 0.0 || sx >= a.tw as f64 { continue }
                    if a.repeat & 2 != 0 { sy = sy.rem_euclid(a.th as f64) } else if sy < 0.0 || sy >= a.th as f64 { continue }
                    // Nearest sample of the source inside the tile.
                    let (ux, uy) = ((sx / a.tw as f64 * iw as f64) as i64, (sy / a.th as f64 * ih as f64) as i64);
                    if ux < 0 || uy < 0 || ux >= iw || uy >= ih { continue }
                    let o = ((uy * iw + ux) as usize) * ch;
                    if o + ch > buf.len() { continue }
                    let (r, g, b) = (buf[o] as u32, buf[o + 1.min(ch - 1)] as u32, buf[o + 2.min(ch - 1)] as u32);
                    let al = if ch == 4 { buf[o + 3] as f32 / 255.0 } else { 1.0 };
                    cv.blend(px, py, r << 24 | g << 16 | b << 8 | 0xff, al);
                }
            }
        }
        _ => {}
    }
}

/// One box-blur pass, horizontal then vertical, over the alpha channel.
fn box_blur(cv: &mut Canvas, r: usize) {
    let (w, h) = (cv.w, cv.h);
    let mut tmp = vec![[0.0f32; 4]; w * h];
    for y in 0..h {
        for x in 0..w {
            let (mut a, mut n) = (0.0f32, 0.0f32);
            for k in x.saturating_sub(r)..=(x + r).min(w - 1) { a += cv.px[y * w + k][3]; n += 1.0 }
            tmp[y * w + x] = [0.0, 0.0, 0.0, a / n];
        }
    }
    for x in 0..w {
        for y in 0..h {
            let (mut a, mut n) = (0.0f32, 0.0f32);
            for k in y.saturating_sub(r)..=(y + r).min(h - 1) { a += tmp[k * w + x][3]; n += 1.0 }
            cv.px[y * w + x] = [0.0, 0.0, 0.0, a / n];
        }
    }
}

fn paint(scene: &Scene, fonts: &FontSet, assets: &std::collections::BTreeMap<String, std::path::PathBuf>) -> Canvas {
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
        // A transform is painted by rendering the node into a layer of its
        // own and inverse-mapping it into place: exact for rotation, and it
        // reuses every painter below unchanged.
        let xf = match kind { 0 => scene.rect_attrs.get(i), 1 => scene.run_attrs.get(i), _ => None }
            .and_then(|a| a.2).and_then(|x| scene.xforms.get(x as usize));
        if let Some(m) = xf {
            if let Some(inv) = invert(m) {
                let mut layer = match stack.last() { Some((_, c)) => c.layer(), None => cv.layer() };
                layer.clip = None;
                paint_one(&mut layer, scene, fonts, &mut ctx, kind, i, assets);
                let dst: &mut Canvas = match stack.last_mut() { Some((_, c)) => c, None => &mut cv };
                let clip = clip_of(kind, i).and_then(|c| scene.clips.get(c as usize))
                    .map(|&(x, y, w, h)| (x / PX, y / PX, (x + w) / PX, (y + h) / PX));
                for py in 0..dst.h as i64 {
                    for px in 0..dst.w as i64 {
                        if let Some((x0, y0, x1, y1)) = clip { if px < x0 || py < y0 || px >= x1 || py >= y1 { continue } }
                        let (dx, dy) = ((px * PX) as f64 + 32.0, (py * PX) as f64 + 32.0);
                        let (sx, sy) = (inv[0] * dx + inv[2] * dy + inv[4], inv[1] * dx + inv[3] * dy + inv[5]);
                        let (ix, iy) = ((sx / PX as f64).floor() as i64, (sy / PX as f64).floor() as i64);
                        if ix < 0 || iy < 0 || ix >= layer.w as i64 || iy >= layer.h as i64 { continue }
                        let src = layer.px[iy as usize * layer.w + ix as usize];
                        if src[3] <= 0.0 { continue }
                        let p = &mut dst.px[py as usize * dst.w + px as usize];
                        for k in 0..3 { p[k] = src[k] / src[3].max(1e-6) * src[3] + p[k] * (1.0 - src[3]) }
                        p[3] = src[3] + p[3] * (1.0 - src[3]);
                    }
                }
                continue;
            }
        }
        let cvr: &mut Canvas = match stack.last_mut() { Some((_, c)) => c, None => &mut cv };
        cvr.clip = clip_of(kind, i).and_then(|c| scene.clips.get(c as usize)).map(|&(x, y, w, h)| (x / PX, y / PX, (x + w) / PX, (y + h) / PX));
        paint_one(cvr, scene, fonts, &mut ctx, kind, i, assets);
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
    let mut assets: std::collections::BTreeMap<String, std::path::PathBuf> = Default::default();
    let src = std::fs::read_to_string(&a[1]).expect("readable input");
    let scene = if a[1].ends_with(".md") {
        render(&src, &fonts, &Options { width_px: width }).0
    } else {
        let subs = navigator_render::conformance::subresources(std::path::Path::new(&a[1]));
        // The review tool needs the BYTES the renderer never sees, to show
        // what the scene refers to: address -> file, from the same manifest.
        assets = navigator_render::conformance::subresource_files(std::path::Path::new(&a[1]));
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
    let data: Vec<u8> = cv.px.iter().flat_map(|p| [0, 1, 2].map(|i| (p[i].clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect();
    wr.write_image_data(&data).expect("png data");
    eprintln!("wrote {} ({}x{})", a[2], cv.w, cv.h);
}
