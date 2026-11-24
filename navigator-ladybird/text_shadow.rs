use super::*;
use libcompositing_rust::display_list::commands::{PaintTextShadow, TextShadowLayer};
use pergola::glyphs::{ATLAS_W, Atlas, COLOR_W};

pub(super) fn shadow_image(
    alpha: &[f32],
    w: usize,
    h: usize,
    sigma: f32,
    color: Color,
) -> pergola::node::Image {
    let radius = (3. * sigma).ceil() as i32;
    let mut temp = vec![0.; alpha.len()];
    let weights: Vec<_> = (-radius..=radius)
        .map(|i| {
            if radius == 0 {
                1.
            } else {
                (-0.5 * (i as f32 / sigma).powi(2)).exp()
            }
        })
        .collect();
    let sum: f32 = weights.iter().sum();
    for y in 0..h {
        for x in 0..w {
            for (i, &weight) in (-radius..=radius).zip(&weights) {
                let u = x as i32 + i;
                if (0..w as i32).contains(&u) {
                    temp[y * w + x] += weight / sum * alpha[y * w + u as usize];
                }
            }
        }
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let mut value = 0.;
            for (i, &weight) in (-radius..=radius).zip(&weights) {
                let v = y as i32 + i;
                if (0..h as i32).contains(&v) {
                    value += weight / sum * temp[v as usize * w + x];
                }
            }
            rgba.extend([
                color.red(),
                color.green(),
                color.blue(),
                (value * color.alpha() as f32).round() as u8,
            ]);
        }
    }
    pergola::node::Image::new(w as u32, h as u32, rgba)
}

pub(super) fn nodes(
    payload: &[u8],
    command: PaintTextShadow,
    face: Face,
    size: f32,
    ascent: f32,
) -> Result<Vec<Node>, &'static str> {
    struct Cached {
        face: u32,
        size: f32,
        ascent: f32,
        payload: Vec<u8>,
        nodes: Vec<Node>,
    }
    thread_local! { static LAST: std::cell::RefCell<Option<Cached>> = const {std::cell::RefCell::new(None)}; }
    if payload.len() > 128 * 1024 {
        return Err("glyph shadow record size");
    }
    LAST.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(saved) = slot.as_ref()
            && saved.face == face.id()
            && saved.size == size
            && saved.ascent == ascent
            && saved.payload == payload
        {
            return Ok(saved.nodes.clone());
        }
        let nodes = rasterize(payload, command, &face, size, ascent)?;
        *slot = Some(Cached {
            face: face.id(),
            size,
            ascent,
            payload: payload.to_vec(),
            nodes: nodes.clone(),
        });
        Ok(nodes)
    })
}

fn rasterize(
    payload: &[u8],
    command: PaintTextShadow,
    face: &Face,
    size: f32,
    ascent: f32,
) -> Result<Vec<Node>, &'static str> {
    let glyphs = span_bytes(payload, command.glyphs);
    let layers = span_bytes(payload, command.layers);
    if glyphs.is_empty() || layers.is_empty() {
        return Ok(Vec::new());
    }
    if command.orientation != Orientation::Horizontal
        || layers.len() / size_of::<TextShadowLayer>() > 16
        || glyphs.len() / size_of::<DisplayListGlyph>() > 4096
    {
        return Err("unsupported shadow text orientation or size");
    }
    let size = size * command.scale;
    if !(0.01..=256.).contains(&size) {
        return Err("shadow glyph size");
    }
    let mut out = Vec::new();
    let mut atlas = Atlas::new();
    let mut ink = Vec::new();
    let mut budget = 0usize;
    for raw in glyphs.chunks_exact(size_of::<DisplayListGlyph>()) {
        let g = read_command::<DisplayListGlyph>(raw);
        let gid = u16::try_from(g.glyph_id).map_err(|_| "shadow glyph id")?;
        if let Some(e) = atlas.glyph(face, size, gid) {
            let x = (g.position.x * command.scale).round() + e.left;
            let y = ((g.position.y + ascent) * command.scale).round() - e.top;
            if !x.is_finite() || !y.is_finite() {
                return Err("shadow glyph position");
            }
            budget += (e.w * e.h) as usize;
            ink.push((e, x, y));
        }
    }
    if ink.is_empty() {
        return Ok(out);
    }
    let count = layers.len() / size_of::<TextShadowLayer>();
    if budget.saturating_mul(count) > 2_000_000 {
        return Err("glyph shadow coverage budget");
    }
    for raw in layers.chunks_exact(size_of::<TextShadowLayer>()) {
        let layer = read_command::<TextShadowLayer>(raw);
        if !(0..=16).contains(&layer.blur_radius) {
            return Err("glyph shadow radius");
        }
        let sigma = layer.blur_radius as f32 / 2.;
        let pad = (3. * sigma).ceil();
        let left = ink
            .iter()
            .map(|e| e.1)
            .fold(f32::INFINITY, f32::min)
            .floor()
            - pad;
        let top = ink
            .iter()
            .map(|e| e.2)
            .fold(f32::INFINITY, f32::min)
            .floor()
            - pad;
        let w = (ink
            .iter()
            .map(|(e, x, _)| x + e.w as f32)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil()
            - left
            + pad) as usize;
        let h = (ink
            .iter()
            .map(|(e, _, y)| y + e.h as f32)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil()
            - top
            + pad) as usize;
        if w == 0 || h == 0 || w.saturating_mul(h).saturating_mul(count) > 262144 {
            return Err("glyph shadow texture budget");
        }
        let mut alpha = vec![0.; w * h];
        for &(e, x, y) in &ink {
            let dx = (x - left) as usize;
            let dy = (y - top) as usize;
            if dx + e.w as usize > w || dy + e.h as usize > h {
                return Err("glyph shadow bounds");
            }
            for row in 0..e.h {
                for col in 0..e.w {
                    let u = e.u + col;
                    let v = e.v + row;
                    let coverage = if e.color {
                        atlas.color_pixels().ok_or("missing colour atlas")?
                            [(4 * (v * COLOR_W + u) + 3) as usize]
                    } else {
                        atlas.pixels()[(v * ATLAS_W + u) as usize]
                    } as f32
                        / 255.;
                    let a = &mut alpha[(dy + row as usize) * w + dx + col as usize];
                    *a += coverage * (1. - *a);
                }
            }
        }
        let rect = Rect::new(
            command.translation.x + layer.offset.x + left,
            command.translation.y + layer.offset.y + top,
            w as f32,
            h as f32,
        );
        out.push(Node::Image {
            rect,
            image: shadow_image(&alpha, w, h, sigma, layer.color),
        });
    }
    Ok(out)
}

#[cfg(test)]
#[test]
fn shadow_alpha_is_applied_once_and_blur_is_symmetric() {
    let color = Color::from_rgba(10, 40, 90, 128);
    let mut alpha = vec![0.; 49];
    alpha[24] = 1.;
    let sharp = shadow_image(&alpha, 7, 7, 0., color);
    assert_eq!(sharp.data().rgba[96..100], [10, 40, 90, 128]);
    let blurred = shadow_image(&alpha, 7, 7, 1., color);
    let a = |i: usize| blurred.data().rgba[4 * i + 3];
    assert!(a(24) < 128 && a(24) > a(23) && a(23) > 0);
    assert_eq!(a(23), a(25));
    assert_eq!(a(23), a(17));
}
