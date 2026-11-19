use super::{Drawing, DrawingContent, FontFiles, Fonts, Page, Pdf, Shaper, LU};
use folium_content::ops::Op;
use officina_draw::chart::{Align, Prim};
use std::io::{self, Write};

pub(super) struct Tile {
    pub origin: [f64; 2],
    pub clip: [f64; 4],
    pub scale: f64,
    pub height: f64,
}

pub(super) fn paint<W: Write>(
    pdf: &mut Pdf<W>, page: &mut Page, drawings: &[Drawing], tile: Tile,
    fonts: &dyn Fonts, files: &dyn FontFiles,
) -> io::Result<()> {
    let [left, top, width, height] = tile.clip;
    let k = tile.scale;
    let ph = tile.height;
    for (id, drawing) in drawings.iter().enumerate() {
        let [x, y, w, h] = drawing.bounds;
        let (x, y, w, h) = (tile.origin[0] + x * k, tile.origin[1] + y * k, w * k, h * k);
        let [bx, by, bw, bh] = drawing.paint_bounds();
        let (bx, by) = (tile.origin[0] + bx * k, tile.origin[1] + by * k);
        let (x0, y0) = (bx.max(left), by.max(top));
        let (x1, y1) = ((bx + bw * k).min(left + width), (by + bh * k).min(top + height));
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        page.c.extend([Op::Save, Op::Rect(x0, ph - y1, x1 - x0, y1 - y0), Op::Clip, Op::EndPath]);
        match &drawing.content {
            DrawingContent::Picture { bytes, crop, .. } => {
                let [l, t, r, b] = crop.map(i64::from);
                let (visible_w, visible_h) = (100_000 - l - r, 100_000 - t - b);
                if visible_w > 0 && visible_h > 0 {
                    let name = pdf.picture(page, id as u64, || Some(bytes.to_vec()))?
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Could not decode a picture in the print area"))?;
                    let (sin, cos) = drawing.rotation();
                    let (sx, sy) = (w / visible_w as f64, h / visible_h as f64);
                    // Local picture coordinates point down. Rotate about the center
                    // while mapping to the PDF coordinate system, which points up.
                    let (cx, cy) = (x + w / 2.0, ph - y - h / 2.0);
                    page.c.extend([
                        Op::Transform([cos, -sin, -sin, -cos,
                            cx - cos * w / 2.0 + sin * h / 2.0,
                            cy + sin * w / 2.0 + cos * h / 2.0]),
                        Op::Rect(0.0, 0.0, w, h), Op::Clip, Op::EndPath,
                        Op::Transform([100_000.0 * sx, 0.0, 0.0, -100_000.0 * sy,
                            -(l as f64) * sx, (100_000 - t) as f64 * sy]),
                        Op::XObject(name),
                    ]);
                }
            }
            DrawingContent::Chart(prims) => {
                let point = |a: f32, b: f32| (x + f64::from(a) * k, ph - y - f64::from(b) * k);
                let rgb = |color: [f32; 4]| [color[0] as f64, color[1] as f64, color[2] as f64];
                let lu = |v: f64| (v * LU as f64).round() as i64;
                for prim in prims {
                    match prim {
                        Prim::Rect { x, y, w, h, color } => {
                            let (a, b) = point(*x, *y + *h);
                            page.c.extend([Op::FillRgb(rgb(*color)), Op::Rect(a, b, f64::from(*w) * k, f64::from(*h) * k), Op::Fill]);
                        }
                        Prim::Line { pts, width, color } => {
                            page.c.extend([Op::StrokeRgb(rgb(*color)), Op::LineWidth(f64::from(*width) * k)]);
                            path(&mut page.c, pts.iter().map(|&(a, b)| point(a, b)), false);
                        }
                        Prim::Area { pts, color } => {
                            page.c.push(Op::FillRgb(rgb(*color)));
                            path(&mut page.c, pts.iter().map(|&(a, b)| point(a, b)), true);
                        }
                        Prim::Dot { x, y, r, color } => {
                            page.c.push(Op::FillRgb(rgb(*color)));
                            path(&mut page.c, arc(*x, *y, *r, 0.0, std::f32::consts::TAU).map(|(a, b)| point(a, b)), true);
                        }
                        Prim::Wedge { cx, cy, r_in, r_out, a0, a1, color } => {
                            let outer = arc(*cx, *cy, *r_out, *a0, *a1);
                            let inner = arc(*cx, *cy, *r_in, *a1, *a0);
                            page.c.push(Op::FillRgb(rgb(*color)));
                            path(&mut page.c, outer.chain(inner).map(|(a, b)| point(a, b)), true);
                        }
                        Prim::Text { x, y, text, size, color, align, bold } => {
                            let (a, b) = point(*x, *y);
                            let color = color[..3].iter().fold(0, |rgb, c| (rgb << 8) | (c * 255.0).round() as u32);
                            let shaper = Shaper::new(fonts, f64::from(*size) * k);
                            let (para, layout, width) = shaper.lay(text, *bold, Some(color));
                            let left = lu(a) - match align {
                                Align::Start => 0,
                                Align::Middle => width / 2,
                                Align::End => width,
                            };
                            let mut items = Vec::new();
                            shaper.paint(&mut items, &para, &layout, left, |_| lu(ph - b));
                            pdf.items(page, items, ph, files, &|_| None)?;
                        }
                    }
                }
            }
        }
        page.c.push(Op::Restore);
    }
    Ok(())
}

fn path(ops: &mut Vec<Op>, mut points: impl Iterator<Item = (f64, f64)>, fill: bool) {
    if let Some((x, y)) = points.next() {
        ops.push(Op::MoveTo(x, y));
        ops.extend(points.map(|(x, y)| Op::LineTo(x, y)));
        if fill {
            ops.extend([Op::ClosePath, Op::Fill]);
        } else {
            ops.push(Op::Stroke);
        }
    }
}

fn arc(x: f32, y: f32, r: f32, start: f32, end: f32) -> impl Iterator<Item = (f32, f32)> {
    let steps = if r == 0.0 { 1 } else { (((end - start).abs() / 0.05).ceil() as u32).clamp(1, 128) };
    (0..=steps).map(move |i| {
        let a = start + (end - start) * i as f32 / steps as f32;
        (x + r * a.sin(), y - r * a.cos())
    })
}
