//! Windows Metafiles (MS-WMF): 16-bit GDI records. The picture's window
//! (the placeable header's box, or the first window it sets) is drawn
//! across the canvas.

use crate::dib;
use crate::gdi::{cp1252, Brush, Gdi, LogFont, Obj, Pen, MAX_POINTS, MAX_RECORDS};
use crate::{i16_at, u16_at, u32_at, PLACEABLE};
use std::rc::Rc;

pub fn play(b: &[u8], width: u32, height: u32) -> Option<tiny_skia::Pixmap> {
    let mut g = Gdi::new(width, height, true)?;
    let start = if u32_at(b, 0) == PLACEABLE {
        let (l, t, r, bo) = (i16_at(b, 6) as f64, i16_at(b, 8) as f64, i16_at(b, 10) as f64, i16_at(b, 12) as f64);
        g.dc.win_org = (l, t);
        g.dc.win_ext = (r - l, bo - t);
        22
    } else {
        let (org, ext, window) = crate::wmf_window(b)?;
        if window {
            g.dc.win_org = org;
            g.dc.win_ext = ext;
        } else {
            g.frame = Some((org, ext));
        }
        0
    };
    if let Some((_, nw, _)) = crate::natural_size(b) {
        g.px = width as f64 / nw.max(1) as f64;
    }
    let mut p = start + 2 * u16_at(b, start + 2) as usize;
    let mut n = 0;
    while p + 6 <= b.len() && n < MAX_RECORDS {
        let size = (u32_at(b, p) as usize).saturating_mul(2);
        let f = u16_at(b, p + 4);
        if size < 6 || f == 0 {
            break;
        }
        let end = p.saturating_add(size).min(b.len());
        record(&mut g, f, &b[p + 6..end], size);
        p = p.saturating_add(size);
        n += 1;
    }
    Some(g.finish())
}

fn record(g: &mut Gdi, f: u16, r: &[u8], size: usize) {
    let s = |i: usize| i16_at(r, i * 2) as f64;
    let color = |i: usize| u32_at(r, i) & 0xFF_FFFF;
    let points = |count: usize, at: usize| -> Vec<(f64, f64)> { (0..count.min(MAX_POINTS)).map_while(|k| r.get(at + k * 4 + 3).map(|_| (i16_at(r, at + k * 4) as f64, i16_at(r, at + k * 4 + 2) as f64))).collect() };
    match f {
        // State.
        0x0201 => g.dc.bk_color = color(0),
        0x0209 => g.dc.text_color = color(0),
        0x0102 => g.dc.bk_mode = u16_at(r, 0) as u32,
        0x012E => g.dc.text_align = u16_at(r, 0) as u32,
        0x0106 => g.dc.winding = u16_at(r, 0) == 2,
        0x0104 => g.dc.rop2 = u16_at(r, 0) as u32,
        0x0103 => g.dc.map_mode = u16_at(r, 0) as u32,
        0x020B => g.dc.win_org = (s(1), s(0)),
        0x020C => {
            // A picture drawn across the box its drawing covers keeps it.
            if s(0) != 0.0 && s(1) != 0.0 && g.frame.is_none() {
                g.dc.win_ext = (s(1), s(0));
            }
        }
        0x020F => g.dc.win_org = (g.dc.win_org.0 + s(1), g.dc.win_org.1 + s(0)),
        0x0410 if g.frame.is_none() => {
            let (yd, yn, xd, xn) = (s(0), s(1), s(2), s(3));
            if yd != 0.0 && xd != 0.0 && yn != 0.0 && xn != 0.0 {
                g.dc.win_ext = (g.dc.win_ext.0 * xn / xd, g.dc.win_ext.1 * yn / yd);
            }
        }
        0x001E => g.save(),
        0x0127 => g.restore(s(0) as i32),
        // Objects.
        0x012D => g.select(u16_at(r, 0) as usize),
        0x01F0 => g.delete(u16_at(r, 0) as usize),
        0x02FA => g.add_obj(Obj::Pen(Pen { style: u16_at(r, 0) as u32, width: s(1).abs(), color: color(6), dashes: Vec::new() })),
        0x02FC => {
            let b = match u16_at(r, 0) {
                1 => Brush::Null,
                2 => Brush::Hatch(u16_at(r, 6) as u32, color(2)),
                _ => Brush::Solid(color(2)),
            };
            g.add_obj(Obj::Brush(b));
        }
        0x02FB => {
            let face: Vec<u8> = r.get(18..r.len().min(18 + 32)).unwrap_or(&[]).iter().copied().take_while(|&c| c != 0).collect();
            g.add_obj(Obj::Font(LogFont {
                height: s(0),
                width: s(1).abs(),
                escapement: s(2),
                weight: s(4) as i32,
                italic: r.get(10).is_some_and(|&v| v != 0),
                underline: r.get(11).is_some_and(|&v| v != 0),
                strike: r.get(12).is_some_and(|&v| v != 0),
                charset: *r.get(13).unwrap_or(&0),
                pitch: *r.get(17).unwrap_or(&0),
                face: cp1252(&face).into_iter().filter_map(char::from_u32).collect(),
            }));
        }
        0x0142 => {
            let d = r.get(4..).unwrap_or(&[]);
            let mono = if u16_at(r, 0) == 3 { dib::mono_packed(d) } else { None };
            let brush = match mono {
                Some(m) => Brush::Mono(Rc::new(m)),
                None => dib::decode_packed(d, false).and_then(|d| pattern(&d)).map(|p| Brush::Pattern(Rc::new(p))).unwrap_or(Brush::Solid(0x80_8080)),
            };
            g.add_obj(Obj::Brush(brush));
        }
        // Everything else that makes an object takes a slot.
        0x00F7 => {
            let n = (u16_at(r, 2) as usize).min(1024);
            let p: Vec<[u8; 4]> = (0..n).map_while(|k| r.get(4 + k * 4..8 + k * 4).map(|c| [c[0], c[1], c[2], 255])).collect();
            g.add_obj(Obj::Palette(Rc::new(p)));
        }
        0x0234 => g.select(u16_at(r, 0) as usize),
        0x01F9 | 0x06FF | 0x06FE | 0x02FD => g.add_obj(Obj::Other),
        // Clipping.
        0x0416 => g.clip_rect(s(3), s(2), s(1), s(0)),
        0x0415 => g.exclude_rect(s(3), s(2), s(1), s(0)),
        // Drawing.
        0x0214 => g.move_to(s(1), s(0)),
        0x0213 => g.to(&[(s(1), s(0))], false),
        0x0325 => g.polyline(&points(u16_at(r, 0) as usize, 2)),
        0x0324 => g.polygon(&[points(u16_at(r, 0) as usize, 2)]),
        0x0538 => {
            let n = u16_at(r, 0) as usize;
            let mut at = 2 + n * 2;
            let mut polys = Vec::new();
            let mut total = 0;
            for k in 0..n.min(r.len() / 2) {
                let c = u16_at(r, 2 + k * 2) as usize;
                total += c;
                if total > MAX_POINTS {
                    break;
                }
                polys.push(points(c, at));
                at += c * 4;
            }
            g.polygon(&polys);
        }
        0x041B => g.rect(s(3), s(2), s(1), s(0)),
        0x061C => g.round_rect(s(5), s(4), s(3), s(2), s(1), s(0)),
        0x0418 => g.ellipse(s(3), s(2), s(1), s(0)),
        0x0817 | 0x0830 | 0x081A => {
            let kind = match f {
                0x0817 => 0,
                0x0830 => 1,
                _ => 2,
            };
            g.arc((s(7), s(6), s(5), s(4)), (s(3), s(2)), (s(1), s(0)), kind);
        }
        0x041F => {
            let (x, y) = (s(3), s(2));
            let saved = g.dc.brush.clone();
            g.dc.brush = Brush::Solid(color(0));
            let pen = std::mem::replace(&mut g.dc.pen.style, 5);
            g.rect(x, y, x + 1.0, y + 1.0);
            g.dc.pen.style = pen;
            g.dc.brush = saved;
        }
        // Text.
        0x0521 => {
            let n = u16_at(r, 0) as usize;
            let at = 2 + (n + 1) / 2 * 2;
            if let Some(t) = r.get(2..2 + n) {
                let (y, x) = (i16_at(r, at) as f64, i16_at(r, at + 2) as f64);
                g.text(x, y, &crate::charset::decode(t, g.dc.font.charset), None, None, None);
            }
        }
        0x0A32 => {
            let (y, x) = (s(0), s(1));
            let n = (u16_at(r, 4) as usize).min(8192);
            let opts = u16_at(r, 6);
            let mut at = 8;
            let mut rect = None;
            if opts & 6 != 0 && r.len() >= at + 8 {
                rect = Some((i16_at(r, at) as f64, i16_at(r, at + 2) as f64, i16_at(r, at + 4) as f64, i16_at(r, at + 6) as f64));
                at += 8;
            }
            let Some(t) = r.get(at..at + n) else { return };
            let dx_at = at + n + (n & 1);
            let dx: Vec<f64> = (0..n).map_while(|k| r.get(dx_at + k * 2 + 1).map(|_| i16_at(r, dx_at + k * 2) as f64)).collect();
            let dx = (dx.len() == n).then_some(dx);
            g.text(x, y, &crate::charset::decode(t, g.dc.font.charset), dx.as_deref(), rect.filter(|_| opts & 2 != 0), rect.filter(|_| opts & 4 != 0));
        }
        // Bitmaps.
        0x0F43 => {
            let rop = u32_at(r, 0);
            let (sh, sw, sy, sx, dh, dw, dy, dx) = (s(3), s(4), s(5), s(6), s(7), s(8), s(9), s(10));
            let pal = g.dc.palette.clone();
            if let Some(d) = r.get(22..).and_then(|d| dib::decode_packed_in(d, false, u16_at(r, 4) as u32, &pal)) {
                let (sy, sh) = flip_src(&d, sy, sh);
                g.blit(d, (sx, sy, sw, sh), (dx, dy, dw, dh), rop, 1.0);
            }
        }
        0x0B41 | 0x0940 => {
            let rop = u32_at(r, 0);
            let bare = size == ((f >> 8) as usize + 3) * 2;
            let stretch = f == 0x0B41;
            if bare {
                let (dh, dw, dy, dx) = if stretch { (s(7), s(8), s(9), s(10)) } else { (s(5), s(6), s(7), s(8)) };
                g.pat_blt(dx, dy, dw, dh, rop);
                return;
            }
            let (sh, sw, sy, sx, dh, dw, dy, dx, at) = if stretch {
                (s(2), s(3), s(4), s(5), s(6), s(7), s(8), s(9), 20)
            } else {
                let (sy, sx, h, w, dy, dx) = (s(2), s(3), s(4), s(5), s(6), s(7));
                (h, w, sy, sx, h, w, dy, dx, 16)
            };
            let pal = g.dc.palette.clone();
            if let Some(d) = r.get(at..).and_then(|d| dib::decode_packed_in(d, false, 0, &pal)) {
                g.blit(d, (sx, sy, sw, sh), (dx, dy, dw, dh), rop, 1.0);
            }
        }
        0x061D => g.pat_blt(s(5), s(4), s(3), s(2), u32_at(r, 0)),
        _ => {}
    }
}

/// Source rectangles count rows from the bottom of a bottom-up DIB; the
/// decoded bitmap is top-down.
pub fn flip_src(d: &dib::Dib, sy: f64, sh: f64) -> (f64, f64) {
    // Most files blit whole bitmaps, where both readings agree.
    if d.top_down || (sy == 0.0 && sh.abs() == d.height as f64) {
        return (sy, sh.abs());
    }
    (d.height as f64 - sy - sh.abs(), sh.abs())
}

pub fn pattern(d: &dib::Dib) -> Option<tiny_skia::Pixmap> {
    if d.width > 256 || d.height > 256 {
        return None;
    }
    let mut p = tiny_skia::Pixmap::new(d.width, d.height)?;
    for (o, i) in p.data_mut().chunks_exact_mut(4).zip(d.rgba.chunks_exact(4)) {
        let a = i[3] as u32;
        o[0] = ((i[0] as u32 * a + 127) / 255) as u8;
        o[1] = ((i[1] as u32 * a + 127) / 255) as u8;
        o[2] = ((i[2] as u32 * a + 127) / 255) as u8;
        o[3] = i[3];
    }
    Some(p)
}
