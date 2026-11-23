//! Paint worksheet cell geometry in page points, with text shaped as text.
use folium_content::ops::Op;
use officina_slides::draw::Paint;
use officina_text::model::{CharAttrs, Dash, Para, PathCmd};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    Align, Cell, CellPaint, IconPart, Item, Page, ParaLayout, Shaper, VAlign, INDENT_PX, LU,
    PAD_PX, TEXT_PX,
};
use crate::{pt, rgb};

const MAX_LINES: usize = 200;

fn lu(v: f64) -> i64 {
    (v * LU as f64).round() as i64
}

fn top(align: VAlign, available: i64, height: i64, pad: i64) -> i64 {
    let room = (available - height).max(0);
    match align {
        VAlign::Top => pad.min(room),
        VAlign::Center => room / 2,
        VAlign::Bottom => (room - pad).max(0),
    }
}

fn left(align: Align, available: i64, width: i64) -> i64 {
    match align {
        Align::Left => 0,
        Align::Center => (available - width) / 2,
        Align::Right => available - width,
    }
}

fn attrs(c: &Cell) -> CharAttrs {
    CharAttrs {
        bold: Some(c.bold),
        italic: Some(c.italic),
        underline: Some(c.underline),
        strike: Some(c.strike),
        color: c.color,
        ..Default::default()
    }
}

fn layout(shaper: &Shaper, text: &str, attrs: &CharAttrs, width: i64) -> (Para, ParaLayout) {
    let p = shaper.attributed(text, attrs);
    let layout = super::layout_para(&shaper.doc.borrow(), &p, width, shaper.fonts);
    (p, layout)
}

fn width(layout: &ParaLayout) -> i64 {
    layout
        .lines
        .iter()
        .map(|line| line.width)
        .max()
        .unwrap_or(0)
}

fn single(shaper: &Shaper, text: &str, attrs: &CharAttrs) -> (Para, ParaLayout) {
    layout(shaper, text, attrs, i64::MAX / 4)
}

fn fit(
    shaper: &Shaper,
    c: &Cell,
    text: &str,
    attrs: &CharAttrs,
    room: i64,
) -> Option<(Para, ParaLayout)> {
    if room <= 0 {
        return None;
    }
    let whole = single(shaper, text, attrs);
    if width(&whole.1) <= room {
        return Some(whole);
    }
    if c.numeric {
        let one = width(&single(shaper, "#", attrs).1).max(1);
        let count = (room / one).min(256);
        return (count > 0).then(|| single(shaper, &"#".repeat(count as usize), attrs));
    }
    // Fit at grapheme boundaries. Measure the final candidate with its ellipsis.
    let ends: Vec<_> = text.grapheme_indices(true).map(|(at, _)| at).collect();
    let (mut lo, mut hi) = (0, ends.len());
    let mut best = None;
    while lo < hi {
        let at = (lo + hi) / 2;
        let cut = single(shaper, &format!("{}…", &text[..ends[at]]), attrs);
        if width(&cut.1) <= room {
            best = Some(cut);
            lo = at + 1;
        } else {
            hi = at;
        }
    }
    best
}

pub(super) fn words(
    items: &mut Vec<Item>,
    shaper: &Shaper,
    c: &Cell,
    [x, y, w, h]: [f64; 4],
    k: f64,
) {
    let inset = lu(PAD_PX * k);
    let icon_lead = icon_size(c, w, h, k).map_or(0.0, |size| size + 3.0 * k);
    let indent = lu(c.indent as f64 * INDENT_PX * k);
    let inner = lu(w - icon_lead) - 2 * inset;
    let room = inner - indent;
    let height = lu(h);
    if c.text.is_empty() || room <= 0 {
        return;
    }
    let x = lu(x + icon_lead) + inset;
    let y = lu(y);
    let pad = lu(2.0 * k);
    let mut attrs = attrs(c);
    let align = |line_width| match c.align {
        Align::Left => indent,
        other => left(other, inner, line_width),
    };
    let paint = |items: &mut Vec<Item>, p: &Para, l: &ParaLayout, at_x, at_y| {
        super::paint_para(
            items,
            &shaper.doc.borrow(),
            shaper.fonts,
            l,
            &p.text,
            at_x,
            at_y,
            None,
        );
    };

    if (-90..=90).contains(&c.rotation) && c.rotation != 0 {
        let (sin, cos) = (-f64::from(c.rotation)).to_radians().sin_cos();
        let (s, d) = (sin.abs(), cos.abs());
        let text = c.text.replace(['\r', '\n'], " ");
        let sample = single(shaper, "Hg", &attrs).1;
        let line_height = sample
            .lines
            .first()
            .map_or(sample.height, |line| line.ascent + line.descent);
        let cap = line_height as f64;
        let fit_room =
            ((room as f64 - s * cap) / d.max(1e-6)).min((height as f64 - d * cap) / s.max(1e-6));
        if let Some((p, l)) = fit(shaper, c, &text, &attrs, fit_room as i64) {
            let tw = width(&l);
            let box_w = (tw as f64 * d + cap * s).ceil() as i64;
            let box_h = (tw as f64 * s + cap * d).ceil() as i64;
            let cx = x + align(box_w) + box_w / 2;
            let cy = y + top(c.valign, height, box_h, pad) + box_h / 2;
            let start = items.len();
            paint(items, &p, &l, -tw / 2, -line_height / 2);
            for item in &mut items[start..] {
                let turn = |u: i64, v: i64| {
                    (
                        cx + (cos * u as f64 - sin * v as f64).round() as i64,
                        cy + (sin * u as f64 + cos * v as f64).round() as i64,
                    )
                };
                match item {
                    Item::Glyphs(run) => {
                        (run.x, run.y) = turn(run.x, run.y);
                        run.turn = Some([cos, sin, -sin, cos]);
                    }
                    Item::Rect { x, y, w, h, color } => {
                        let points = [(*x, *y), (*x + *w, *y), (*x + *w, *y + *h), (*x, *y + *h)]
                            .map(|(u, v)| turn(u, v));
                        *item = polygon(points, *color);
                    }
                    _ => {}
                }
            }
        }
        return;
    }

    if c.rotation == 255 || (!c.numeric && (c.wrap || c.text.contains(['\r', '\n']))) {
        let line_height = lu(TEXT_PX * k * if c.rotation == 255 { 1.1 } else { 1.3 }).max(1);
        let slots = ((height / line_height) as usize).clamp(1, MAX_LINES);
        let (lines, clipped): (Vec<_>, bool) = if c.rotation == 255 {
            let mut lines = c.text.graphemes(true);
            let visible = lines
                .by_ref()
                .take(slots)
                .map(|g| if g.contains(['\r', '\n']) { "" } else { g })
                .collect();
            (visible, lines.next().is_some())
        } else if c.wrap {
            let (p, l) = layout(shaper, &c.text, &attrs, room);
            // Keep each line with its shaped runs, bidi ordering and exact advances.
            let count = l.lines.len().min(slots);
            let clipped = count < l.lines.len();
            let start = if clipped {
                pad
            } else {
                top(c.valign, height, count as i64 * line_height, pad)
            };
            let mut l = l;
            l.lines.truncate(count);
            for line in &mut l.lines {
                line.x = align(line.width);
                line.height = line_height;
            }
            paint(items, &p, &l, x, y + start);
            return;
        } else {
            let mut lines = c.text.split('\n').map(|s| s.trim_end_matches('\r'));
            let visible = lines.by_ref().take(slots).collect();
            (visible, lines.next().is_some())
        };
        let start = if clipped {
            pad
        } else {
            top(c.valign, height, lines.len() as i64 * line_height, pad)
        };
        for (i, text) in lines.into_iter().enumerate() {
            if let Some((p, l)) = fit(shaper, c, text, &attrs, room) {
                let lead = if c.rotation == 255 {
                    left(Align::Center, inner, width(&l))
                } else {
                    align(width(&l))
                };
                paint(items, &p, &l, x + lead, y + start + i as i64 * line_height);
            }
        }
        return;
    }

    if c.shrink_to_fit {
        let natural = width(&single(shaper, &c.text, &attrs).1);
        if natural > room {
            let minimum = (3.0 * k * 2.0).ceil().max(2.0);
            attrs.size = Some(
                (TEXT_PX * k * 2.0 * room as f64 / natural as f64 * 0.98)
                    .floor()
                    .max(minimum) as u16,
            );
        }
    }
    if let Some((p, l)) = fit(shaper, c, &c.text, &attrs, room) {
        let ink = l
            .lines
            .first()
            .map_or(l.height, |line| line.ascent + line.descent);
        paint(
            items,
            &p,
            &l,
            x + align(width(&l)),
            y + top(c.valign, height, ink, pad),
        );
    }
}

fn polygon<const N: usize>(points: [(i64, i64); N], color: u32) -> Item {
    let mut cmds = Vec::with_capacity(N + 1);
    cmds.push(PathCmd::Move(points[0].0, points[0].1));
    cmds.extend(points[1..].iter().map(|&(x, y)| PathCmd::Line(x, y)));
    cmds.push(PathCmd::Close);
    Item::Path {
        cmds,
        fill: Some(color),
        line: None,
    }
}

pub(super) fn note_marker(items: &mut Vec<Item>, [x, y, w, h]: [f64; 4], k: f64) {
    let side = (6.0 * k).min(w / 2.0).min(h / 2.0);
    items.push(polygon(
        [
            (lu(x + w - side), lu(y)),
            (lu(x + w), lu(y)),
            (lu(x + w), lu(y + side)),
        ],
        0xC93832,
    ));
}

fn background(page: &mut Page, c: &Cell, [x, y, w, h]: [f64; 4], k: f64, ph: f64) {
    let ops = &mut page.c;
    let rect = Op::Rect(x, ph - y - h, w, h);
    if let Some((angle, stops)) = &c.gradient {
        let (dy, dx) = (f64::from(*angle) / 1000.0).to_radians().sin_cos();
        let len = (w * dx.abs() + h * dy.abs()) / 2.0;
        let (cx, cy) = (x + w / 2.0, ph - y - h / 2.0);
        // The shared band painter works in local PDF points.
        ops.extend([
            Op::Save,
            rect,
            Op::Clip,
            Op::EndPath,
            Op::Transform([1.0, 0.0, 0.0, 1.0, cx, cy]),
        ]);
        let paint = Paint::Linear {
            from: (-dx * len, dy * len),
            to: (dx * len, -dy * len),
            stops: stops
                .iter()
                .map(|&(p, c)| (f32::from(p) / 10_000.0, c << 8))
                .collect(),
        };
        crate::slides::gradient(&paint, 2.0 * (w + h), ops);
        ops.push(Op::Restore);
    } else if let Some(p) = &c.pattern {
        ops.extend([
            Op::FillRgb(rgb(p.paper)),
            rect,
            Op::Fill,
            Op::FillRgb(rgb(p.ink)),
        ]);
        let step = (w.max(h) / (48.0 * k)).ceil().max(1.0) * k;
        let count = |n: f64| (n / step).ceil() as usize;
        for row in 0..count(h) {
            let mask = p.rows[row % 8];
            let top = row as f64 * step;
            let height = step.min(h - top);
            let mut col = 0;
            while col < count(w) {
                if mask & (1 << (col % 8)) == 0 {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < count(w) && mask & (1 << (col % 8)) != 0 {
                    col += 1;
                }
                let offset = start as f64 * step;
                ops.push(Op::Rect(
                    x + offset,
                    ph - y - top - height,
                    (col as f64 * step).min(w) - offset,
                    height,
                ));
            }
        }
        ops.push(Op::Fill);
    } else if let Some(color) = c.fill {
        ops.extend([Op::FillRgb(rgb(color)), rect, Op::Fill]);
    }
}

fn icon_size(cell: &Cell, width: f64, height: f64, scale: f64) -> Option<f64> {
    let side = (height - 6.0 * scale).clamp(0.0, 14.0 * scale);
    (cell.icon.is_some() && side >= 6.0 * scale && width > side + 4.0 * scale).then_some(side)
}

fn paint_icon(page: &mut Page, cell: &Cell, [x, y, w, h]: [f64; 4], k: f64, ph: f64) {
    let (Some(parts), Some(side)) = (&cell.icon, icon_size(cell, w, h, k)) else {
        return;
    };
    let x = x + (3.0 + f64::from(cell.indent) * INDENT_PX) * k;
    let y = y + (h - side) / 2.0;
    let scale = side / 14.0;
    let point = |[u, v]: [f32; 2]| (x + f64::from(u) * scale, ph - y - f64::from(v) * scale);
    let ops = &mut page.c;
    for item in parts.iter() {
        match item {
            IconPart::Line {
                from,
                to,
                width,
                color,
            } => {
                let ((x0, y0), (x1, y1)) = (point(*from), point(*to));
                ops.extend([
                    Op::StrokeRgb(rgb(*color)),
                    Op::LineWidth(f64::from(*width) * scale),
                    Op::MoveTo(x0, y0),
                    Op::LineTo(x1, y1),
                    Op::Stroke,
                ]);
            }
            IconPart::Rect {
                rect: [x0, y0, w, h],
                radius,
                color,
            } => {
                let (x0, y0) = point([*x0, *y0]);
                let (w, h) = (f64::from(*w) * scale, f64::from(*h) * scale);
                ops.push(Op::FillRgb(rgb(*color)));
                let r = (f64::from(*radius) * scale).min(w / 2.0).min(h / 2.0);
                if r <= 0.0 {
                    ops.push(Op::Rect(x0, y0 - h, w, h));
                } else {
                    // Quarter-circle corners, with the same radius as the grid.
                    let step = std::f64::consts::FRAC_PI_2;
                    for (corner, (cx, cy)) in [
                        (x0 + w - r, y0 - r),
                        (x0 + r, y0 - r),
                        (x0 + r, y0 - h + r),
                        (x0 + w - r, y0 - h + r),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        for i in 0..=4 {
                            let (sin, cos) = ((corner as f64 + i as f64 / 4.0) * step).sin_cos();
                            let (u, v) = (cx + r * cos, cy + r * sin);
                            ops.push(if corner == 0 && i == 0 {
                                Op::MoveTo(u, v)
                            } else {
                                Op::LineTo(u, v)
                            });
                        }
                    }
                    ops.push(Op::ClosePath);
                }
                ops.push(Op::Fill);
            }
            IconPart::Polygon {
                points,
                clip,
                color,
            } => {
                let Some((&first, rest)) = points.split_first() else {
                    continue;
                };
                if let Some([a, b, c, d]) = *clip {
                    let ((x0, y0), (x1, y1)) = (point([a, b]), point([c, d]));
                    ops.extend([
                        Op::Save,
                        Op::Rect(x0, y1, x1 - x0, y0 - y1),
                        Op::Clip,
                        Op::EndPath,
                    ]);
                }
                let (x0, y0) = point(first);
                ops.extend([Op::FillRgb(rgb(*color)), Op::MoveTo(x0, y0)]);
                ops.extend(rest.iter().map(|&p| {
                    let (u, v) = point(p);
                    Op::LineTo(u, v)
                }));
                ops.extend([Op::ClosePath, Op::Fill]);
                if clip.is_some() {
                    ops.push(Op::Restore);
                }
            }
        }
    }
}

pub(super) fn cell(
    page: &mut Page,
    paint: &mut CellPaint,
    shaper: &Shaper,
    cell: &Cell,
    rect: [f64; 4],
    k: f64,
    ph: f64,
    text: bool,
) {
    background(page, cell, rect, k, ph);
    let [x, y, w, h] = rect;
    if let Some((share, color)) = cell.bar {
        let width = (w - 4.0 * k).max(0.0) * f64::from(share.clamp(0.0, 1.0));
        let height = (h - 5.0 * k).max(0.0);
        page.c.extend([
            Op::FillRgb(rgb(color)),
            Op::Rect(x + 2.0 * k, ph - y - 2.0 * k - height, width, height),
            Op::Fill,
        ]);
    }
    for (down, edge) in cell.diagonals.iter().enumerate() {
        if let Some((width, color)) = edge {
            let width = lu(f64::from(*width) * k).max(1);
            let pad = pt(width).min(w / 2.0).min(h / 2.0);
            let (t, b) = (lu(y + pad), lu(y + h - pad));
            let (a, b) = if down == 0 { (t, b) } else { (b, t) };
            paint.items.push(Item::Path {
                cmds: vec![
                    PathCmd::Move(lu(x + pad), a),
                    PathCmd::Line(lu(x + w - pad), b),
                ],
                line: Some((*color, width, Dash::Solid)),
                fill: None,
            });
        }
    }
    if text {
        paint.words(shaper, cell, rect, k);
    }
    paint_icon(page, cell, rect, k, ph);
    for (side, edge) in cell.borders.iter().enumerate() {
        let Some((bw, color)) = *edge else { continue };
        let bw = (f64::from(bw) * k)
            .max(0.25)
            .min(if side < 2 { w } else { h });
        let (x, y, w, h) = match side {
            0 => (x, y, bw, h),
            1 => (x + w - bw, y, bw, h),
            2 => (x, y, w, bw),
            _ => (x, y + h - bw, w, bw),
        };
        paint.items.push(Item::Rect {
            x: lu(x),
            y: lu(y),
            w: lu(w),
            h: lu(h),
            color,
        });
    }
}
