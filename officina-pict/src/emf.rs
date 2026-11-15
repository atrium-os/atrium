//! Enhanced Metafiles (MS-EMF): 32-bit GDI records. The picture's frame
//! (hundredths of a millimetre, in the header) is drawn across the canvas.

use std::rc::Rc;

use tiny_skia::{Color, PathBuilder};

use crate::dib;
use crate::gdi::{rgb, Aff, Brush, Gdi, LogFont, Obj, Pen, MAX_POINTS, MAX_RECORDS};
use crate::wmf::{flip_src, pattern};
use crate::{i32_at, u16_at, u32_at};

fn f32_at(b: &[u8], i: usize) -> f64 {
    let v = f32::from_bits(u32_at(b, i)) as f64;
    if v.is_finite() { v } else { 0.0 }
}

pub fn play(b: &[u8], width: u32, height: u32) -> Option<tiny_skia::Pixmap> {
    let mut g = Gdi::new(width, height, false)?;
    let (dw, dh, mw, mh) = (i32_at(b, 72) as f64, i32_at(b, 76) as f64, i32_at(b, 80) as f64, i32_at(b, 84) as f64);
    let (ppx, ppy) = if dw > 0.0 && dh > 0.0 && mw > 0.0 && mh > 0.0 { (dw / mw, dh / mh) } else { (96.0 / 25.4, 96.0 / 25.4) };
    g.dev_per_mm = (ppx, ppy);
    let (fl, ft, fr, fb) = (i32_at(b, 24) as f64, i32_at(b, 28) as f64, i32_at(b, 32) as f64, i32_at(b, 36) as f64);
    // The frame in device pixels; else the bounds.
    let (x0, y0, x1, y1) = if fr > fl && fb > ft {
        (fl * ppx / 100.0, ft * ppy / 100.0, fr * ppx / 100.0, fb * ppy / 100.0)
    } else {
        let (l, t, r, bo) = (i32_at(b, 8) as f64, i32_at(b, 12) as f64, i32_at(b, 16) as f64, i32_at(b, 20) as f64);
        if r < l || bo < t {
            return None;
        }
        (l, t, r + 1.0, bo + 1.0)
    };
    let (sx, sy) = (width as f64 / (x1 - x0).max(1e-6), height as f64 / (y1 - y0).max(1e-6));
    g.dev = Aff { a: sx, b: 0.0, c: 0.0, d: sy, e: -x0 * sx, f: -y0 * sy };
    if !g.dev.finite() {
        return None;
    }
    g.px = sx.abs().max(sy.abs());
    let mut p = u32_at(b, 4) as usize;
    let mut n = 0;
    let mut plus_only = false;
    while p + 8 <= b.len() && n < MAX_RECORDS {
        let ty = u32_at(b, p);
        let size = u32_at(b, p + 4) as usize;
        if size < 8 || size % 4 != 0 || ty == 14 {
            break;
        }
        let r = &b[p..p.saturating_add(size).min(b.len())];
        if ty == 70 && u32_at(r, 12) == 0x2B46_4D45 && u16_at(r, 16) == 0x4001 && u16_at(r, 18) & 1 == 0 {
            plus_only = true;
        }
        record(&mut g, ty, r);
        p = p.saturating_add(size);
        n += 1;
    }
    (!plus_only).then(|| g.finish())
}

fn pts32(r: &[u8], count: usize, at: usize) -> Vec<(f64, f64)> {
    (0..count.min(MAX_POINTS)).map_while(|k| r.get(at + k * 8 + 7).map(|_| (i32_at(r, at + k * 8) as f64, i32_at(r, at + k * 8 + 4) as f64))).collect()
}

fn pts16(r: &[u8], count: usize, at: usize) -> Vec<(f64, f64)> {
    (0..count.min(MAX_POINTS)).map_while(|k| r.get(at + k * 4 + 3).map(|_| (u16_at(r, at + k * 4) as i16 as f64, u16_at(r, at + k * 4 + 2) as i16 as f64))).collect()
}

fn rect(r: &[u8], at: usize) -> (f64, f64, f64, f64) {
    (i32_at(r, at) as f64, i32_at(r, at + 4) as f64, i32_at(r, at + 8) as f64, i32_at(r, at + 12) as f64)
}

/// RGNDATA's rectangles.
fn region(r: &[u8], at: usize, len: usize) -> Vec<(f64, f64, f64, f64)> {
    let Some(d) = r.get(at..at.saturating_add(len).min(r.len())) else { return Vec::new() };
    let n = (u32_at(d, 8) as usize).min(d.len().saturating_sub(32) / 16);
    (0..n).map(|k| rect(d, 32 + k * 16)).collect()
}

/// A bitmap from its two offsets.
fn bitmap(r: &[u8], bmi: usize, cb_bmi: usize, bits: usize, cb_bits: usize, alpha: bool) -> Option<dib::Dib> {
    let h = r.get(bmi..bmi.checked_add(cb_bmi)?)?;
    let px = r.get(bits..bits.checked_add(cb_bits)?)?;
    dib::decode(h, px, alpha)
}

/// A bitmap whose colour table may index the device's palette.
fn bitmap_in(g: &Gdi, r: &[u8], at: [usize; 4], usage: u32, alpha: bool) -> Option<dib::Dib> {
    let h = r.get(at[0]..at[0].checked_add(at[1])?)?;
    let px = r.get(at[2]..at[2].checked_add(at[3])?)?;
    let pal = g.dc.palette.clone();
    dib::decode_in(h, px, alpha, if usage == 1 { dib::Colors::Indices(&pal) } else { dib::Colors::Own })
}

fn record(g: &mut Gdi, ty: u32, r: &[u8]) {
    let i = |at: usize| i32_at(r, at) as f64;
    let u = |at: usize| u32_at(r, at);
    match ty {
        // Polygons and lines.
        2 => g.bezier(&pts32(r, u(24) as usize, 28)),
        85 => g.bezier(&pts16(r, u(24) as usize, 28)),
        3 => g.polygon(&[pts32(r, u(24) as usize, 28)]),
        86 => g.polygon(&[pts16(r, u(24) as usize, 28)]),
        4 => g.polyline(&pts32(r, u(24) as usize, 28)),
        87 => g.polyline(&pts16(r, u(24) as usize, 28)),
        5 => g.to(&pts32(r, u(24) as usize, 28), true),
        88 => g.to(&pts16(r, u(24) as usize, 28), true),
        6 => g.to(&pts32(r, u(24) as usize, 28), false),
        89 => g.to(&pts16(r, u(24) as usize, 28), false),
        7 | 8 | 90 | 91 => {
            let n = (u(24) as usize).min(r.len() / 4);
            let wide = ty == 7 || ty == 8;
            let mut at = 32 + n * 4;
            let mut polys = Vec::new();
            let mut total = 0;
            for k in 0..n {
                let c = u(32 + k * 4) as usize;
                total += c;
                if total > MAX_POINTS {
                    break;
                }
                polys.push(if wide { pts32(r, c, at) } else { pts16(r, c, at) });
                at = at.saturating_add(c * if wide { 8 } else { 4 });
            }
            if ty == 8 || ty == 91 {
                g.polygon(&polys);
            } else {
                for p in &polys {
                    g.polyline(p);
                }
            }
        }
        56 | 92 => {
            let n = u(24) as usize;
            let pts = if ty == 56 { pts32(r, n, 28) } else { pts16(r, n, 28) };
            let types = r.get(28 + n * if ty == 56 { 8 } else { 4 }..).unwrap_or(&[]);
            let mut k = 0;
            while k < pts.len() && k < types.len() {
                let t = types[k];
                match t & !1 {
                    6 => {
                        g.move_to(pts[k].0, pts[k].1);
                        k += 1;
                    }
                    4 if k + 2 < pts.len() => {
                        g.to(&pts[k..k + 3], true);
                        k += 3;
                    }
                    _ => {
                        g.to(&pts[k..k + 1], false);
                        k += 1;
                    }
                }
                if t & 1 != 0 {
                    g.close_figure();
                }
            }
        }
        27 => g.move_to(i(8), i(12)),
        54 => g.to(&[(i(8), i(12))], false),
        42 => {
            let (l, t, rr, b) = rect(r, 8);
            g.ellipse(l, t, rr, b);
        }
        43 => {
            let (l, t, rr, b) = rect(r, 8);
            g.rect(l, t, rr, b);
        }
        44 => {
            let (l, t, rr, b) = rect(r, 8);
            g.round_rect(l, t, rr, b, i(24), i(28));
        }
        45 | 46 | 47 | 55 => {
            let kind = match ty {
                45 => 0,
                46 => 1,
                47 => 2,
                _ => 3,
            };
            g.arc(rect(r, 8), (i(24), i(28)), (i(32), i(36)), kind);
        }
        57 => g.dc.clockwise = u(8) == 2,
        58 => {
            let v = u(8);
            g.dc.miter = if (1..=1000).contains(&v) { v as f32 } else { (f32_at(r, 8) as f32).clamp(1.0, 1000.0) };
        }
        // State.
        9 => {
            if i(8) != 0.0 && i(12) != 0.0 {
                g.dc.win_ext = (i(8), i(12));
            }
        }
        10 => g.dc.win_org = (i(8), i(12)),
        11 => {
            if i(8) != 0.0 && i(12) != 0.0 {
                g.dc.vp_ext = (i(8), i(12));
            }
        }
        12 => g.dc.vp_org = (i(8), i(12)),
        31 | 32 => {
            let (xn, xd, yn, yd) = (i(8), i(12), i(16), i(20));
            if xn != 0.0 && xd != 0.0 && yn != 0.0 && yd != 0.0 {
                let e = if ty == 31 { &mut g.dc.vp_ext } else { &mut g.dc.win_ext };
                *e = (e.0 * xn / xd, e.1 * yn / yd);
            }
        }
        17 => g.dc.map_mode = u(8),
        18 => g.dc.bk_mode = u(8),
        19 => g.dc.winding = u(8) == 2,
        20 => g.dc.rop2 = u(8),
        22 => g.dc.text_align = u(8),
        24 => g.dc.text_color = u(8) & 0xFF_FFFF,
        25 => g.dc.bk_color = u(8) & 0xFF_FFFF,
        33 => g.save(),
        34 => g.restore(i32_at(r, 8)),
        35 | 36 => {
            let x = Aff { a: f32_at(r, 8), b: f32_at(r, 12), c: f32_at(r, 16), d: f32_at(r, 20), e: f32_at(r, 24), f: f32_at(r, 28) };
            let w = g.dc.world;
            let new = match if ty == 35 { 4 } else { u(32) } {
                1 => Aff::ID,
                2 => x.then(&w),
                3 => w.then(&x),
                4 => x,
                _ => w,
            };
            if new.finite() && new.det().abs() > 1e-12 {
                g.dc.world = new;
            }
        }
        // Objects.
        37 => {
            let k = u(8);
            if k & 0x8000_0000 != 0 {
                g.select_stock(k & 0x7FFF_FFFF);
            } else {
                g.select(k as usize);
            }
        }
        40 => g.delete(u(8) as usize),
        38 => g.put_obj(u(8) as usize, Obj::Pen(Pen { style: u(12), width: i(16).abs(), color: u(24) & 0xFF_FFFF, dashes: Vec::new() })),
        95 => {
            let (style, w, bs, color, hatch) = (u(28), u(32) as f64, u(36), u(40) & 0xFF_FFFF, u(44));
            let n = (u(48) as usize).min(16);
            let dashes = (0..n).map(|k| u(52 + k * 4) as f32 * g.m().scale() as f32).collect();
            let _ = (bs, hatch);
            // Cosmetic pens are one pixel whatever the width says.
            let width = if style & 0x0001_0000 != 0 { w } else { 0.0 };
            g.put_obj(u(8) as usize, Obj::Pen(Pen { style, width, color: if bs == 1 { 0 } else { color }, dashes }));
            if bs == 1 {
                if let Some(Some(Obj::Pen(p))) = g.objs.get_mut(u(8) as usize) {
                    p.style = (p.style & !0xF) | 5;
                }
            }
        }
        39 => {
            let b = match u(12) {
                1 => Brush::Null,
                2 => Brush::Hatch(u(20), u(16) & 0xFF_FFFF),
                _ => Brush::Solid(u(16) & 0xFF_FFFF),
            };
            g.put_obj(u(8) as usize, Obj::Brush(b));
        }
        93 | 94 => {
            let (bmi, cb_bmi, bits, cb_bits) = (u(16) as usize, u(20) as usize, u(24) as usize, u(28) as usize);
            let mono = if ty == 93 { r.get(bmi..bmi.saturating_add(cb_bmi)).zip(r.get(bits..bits.saturating_add(cb_bits))).and_then(|(h, b)| dib::mono_bits(h, b)) } else { None };
            let b = match mono {
                Some(m) => Some(Brush::Mono(Rc::new(m))),
                None => bitmap(r, bmi, cb_bmi, bits, cb_bits, false).and_then(|d| pattern(&d)).map(|p| Brush::Pattern(Rc::new(p))),
            };
            g.put_obj(u(8) as usize, Obj::Brush(b.unwrap_or(Brush::Solid(0x80_8080))));
        }
        82 => {
            let face: String = char::decode_utf16((0..32).map(|k| u16_at(r, 40 + k * 2)).take_while(|&c| c != 0)).filter_map(Result::ok).collect();
            g.put_obj(
                u(8) as usize,
                Obj::Font(LogFont {
                    height: i(12),
                    width: i(16).abs(),
                    escapement: i(20),
                    weight: i32_at(r, 28),
                    italic: r.get(32).is_some_and(|&v| v != 0),
                    underline: r.get(33).is_some_and(|&v| v != 0),
                    strike: r.get(34).is_some_and(|&v| v != 0),
                    charset: *r.get(35).unwrap_or(&0),
                    pitch: *r.get(39).unwrap_or(&0),
                    face,
                }),
            );
        }
        49 => {
            let n = (u16_at(r, 14) as usize).min(1024);
            let p: Vec<[u8; 4]> = (0..n).map_while(|k| r.get(16 + k * 4..20 + k * 4).map(|c| [c[0], c[1], c[2], 255])).collect();
            g.put_obj(u(8) as usize, Obj::Palette(Rc::new(p)));
        }
        48 => g.select(u(8) as usize),
        // Clipping.
        29 => {
            let (l, t, rr, b) = rect(r, 8);
            g.exclude_rect(l, t, rr, b);
        }
        30 => {
            let (l, t, rr, b) = rect(r, 8);
            g.clip_rect(l, t, rr, b);
        }
        75 => {
            let rects = region(r, 16, u(8) as usize);
            let dev = g.dev;
            g.clip_region(&rects, u(12), &dev);
        }
        67 => g.clip_path(u(8)),
        // Paths.
        59 => g.begin_path(),
        60 => g.end_path(),
        61 => g.close_figure(),
        62 => g.draw_path(true, false),
        63 => g.draw_path(true, true),
        64 => g.draw_path(false, true),
        68 => g.abort_path(),
        // Regions filled.
        71 | 74 => {
            let (at, len) = if ty == 71 { (32, u(24) as usize) } else { (28, u(24) as usize) };
            let rects = region(r, at, len);
            let saved = g.dc.brush.clone();
            if ty == 71 {
                g.select(u(28) as usize);
            }
            let pen = std::mem::replace(&mut g.dc.pen.style, 5);
            for (l, t, rr, b) in rects.into_iter().take(10_000) {
                g.rect(l, t, rr, b);
            }
            g.dc.pen.style = pen;
            g.dc.brush = saved;
        }
        // Text.
        83 | 84 => {
            let (x, y) = (i(36), i(40));
            let n = (u(44) as usize).min(8192);
            let (off, opts, off_dx) = (u(48) as usize, u(52), u(72) as usize);
            if opts & 0x10 != 0 {
                // Glyph indices of the file's own font: nothing to map them by.
                return;
            }
            let text: Vec<u32> = if ty == 84 {
                char::decode_utf16((0..n).map_while(|k| r.get(off + k * 2 + 1).map(|_| u16_at(r, off + k * 2)))).map(|c| c.map_or(0xFFFD, |c| c as u32)).collect()
            } else {
                r.get(off..off + n).map(|t| crate::charset::decode(t, g.dc.font.charset)).unwrap_or_default()
            };
            let step = if opts & 0x2000 != 0 { 8 } else { 4 };
            let dx: Vec<f64> = if off_dx == 0 { Vec::new() } else { (0..text.len()).map_while(|k| r.get(off_dx + k * step + 3).map(|_| i32_at(r, off_dx + k * step) as f64)).collect() };
            let dx = (dx.len() == text.len() && !dx.is_empty()).then_some(dx);
            let rc = rect(r, 56);
            let rc = (rc.2 > rc.0 && rc.3 > rc.1).then_some(rc);
            g.text(x, y, &text, dx.as_deref(), rc.filter(|_| opts & 2 != 0), rc.filter(|_| opts & 4 != 0));
        }
        108 => {
            let (x, y, n, opts) = (i(8), i(12), (u(16) as usize).min(8192), u(20));
            let (mut at, mut rc) = (36, None);
            if opts & 0x100 == 0 {
                rc = Some(rect(r, 36));
                at += 16;
            }
            if opts & 0x10 != 0 {
                return;
            }
            let text: Vec<u32> = if opts & 0x200 != 0 {
                r.get(at..at + n).map(|t| crate::charset::decode(t, g.dc.font.charset)).unwrap_or_default()
            } else {
                char::decode_utf16((0..n).map_while(|k| r.get(at + k * 2 + 1).map(|_| u16_at(r, at + k * 2)))).map(|c| c.map_or(0xFFFD, |c| c as u32)).collect()
            };
            g.text(x, y, &text, None, rc.filter(|_| opts & 2 != 0), rc.filter(|_| opts & 4 != 0));
        }
        // Bitmaps.
        76 | 77 | 78 | 114 | 116 => {
            let (dx, dy, dw, dh, rop) = (i(24), i(28), i(32), i(36), u(40));
            let (sx, sy) = (i(44), i(48));
            let xf = (f32_at(r, 52), f32_at(r, 64));
            let (sw, sh) = if ty == 76 {
                let k = |v: f64| if v.abs() > 1e-6 { v.abs() } else { 1.0 };
                (dw * k(xf.0), dh * k(xf.1))
            } else {
                (i(100), i(104))
            };
            let (off_bmi, cb_bmi, off_bits, cb_bits) = (u(84) as usize, u(88) as usize, u(92) as usize, u(96) as usize);
            if cb_bmi == 0 {
                g.pat_blt(dx, dy, dw, dh, rop);
                return;
            }
            let alpha = ty == 114 && r.get(43) == Some(&1);
            let Some(mut d) = bitmap_in(g, r, [off_bmi, cb_bmi, off_bits, cb_bits], u(80), alpha) else { return };
            let (rop, opacity) = match ty {
                114 => (0x00CC_0020, *r.get(42).unwrap_or(&255) as f32 / 255.0),
                116 => {
                    let key = u(40) & 0xFF_FFFF;
                    let k = [key as u8, (key >> 8) as u8, (key >> 16) as u8];
                    for p in d.rgba.chunks_exact_mut(4) {
                        if p[..3] == k {
                            p.fill(0);
                        }
                    }
                    (0x00CC_0020, 1.0)
                }
                78 => (0x00CC_0020, 1.0),
                _ => (rop, 1.0),
            };
            g.blit(d, (sx, sy, sw, sh), (dx, dy, dw, dh), rop, opacity);
        }
        80 => {
            let (dx, dy, sx, sy, sw, sh) = (i(24), i(28), i(32), i(36), i(40), i(44));
            if let Some(d) = bitmap_in(g, r, [u(48) as usize, u(52) as usize, u(56) as usize, u(60) as usize], u(64), false) {
                let (sy, sh) = flip_src(&d, sy, sh);
                g.blit(d, (sx, sy, sw, sh), (dx, dy, sw, sh), 0x00CC_0020, 1.0);
            }
        }
        81 => {
            let (dx, dy, sx, sy, sw, sh) = (i(24), i(28), i(32), i(36), i(40), i(44));
            let (rop, dw, dh) = (u(68), i(72), i(76));
            if u(52) == 0 {
                g.pat_blt(dx, dy, dw, dh, rop);
                return;
            }
            if let Some(d) = bitmap_in(g, r, [u(48) as usize, u(52) as usize, u(56) as usize, u(60) as usize], u(64), false) {
                let (sy, sh) = flip_src(&d, sy, sh);
                g.blit(d, (sx, sy, sw, sh), (dx, dy, dw, dh), rop, 1.0);
            }
        }
        118 => {
            let (nv, nt, mode) = (u(24) as usize, u(28) as usize, u(32));
            let nv = nv.min(r.len() / 16);
            let v: Vec<(f64, f64, Color)> = (0..nv)
                .map(|k| {
                    let o = 36 + k * 16;
                    let c = |j: usize| (u16_at(r, o + 8 + j * 2) >> 8) as u8;
                    (i(o), i(o + 4), Color::from_rgba8(c(0), c(1), c(2), 255))
                })
                .collect();
            let mut at = 36 + nv * 16;
            for _ in 0..nt.min(10_000) {
                if mode == 2 {
                    let (a, b, c) = (u(at) as usize, u(at + 4) as usize, u(at + 8) as usize);
                    at += 12;
                    let (Some(a), Some(b), Some(c)) = (v.get(a), v.get(b), v.get(c)) else { break };
                    // A triangle: its average colour.
                    let avg = |f: fn(&Color) -> f32| (f(&a.2) + f(&b.2) + f(&c.2)) / 3.0;
                    let col = Color::from_rgba(avg(Color::red), avg(Color::green), avg(Color::blue), 1.0).unwrap_or(Color::BLACK);
                    let m = g.m();
                    let mut pb = PathBuilder::new();
                    for (k, p) in [a, b, c].iter().enumerate() {
                        let (x, y) = m.apply(p.0, p.1);
                        if k == 0 { pb.move_to(x as f32, y as f32) } else { pb.line_to(x as f32, y as f32) }
                    }
                    pb.close();
                    if let Some(p) = pb.finish() {
                        g.fill_with(&p, col);
                    }
                } else {
                    let (a, b) = (u(at) as usize, u(at + 4) as usize);
                    at += 8;
                    let (Some(a), Some(b)) = (v.get(a), v.get(b)) else { break };
                    g.gradient_rect((a.0, a.1, b.0, b.1), a.2, b.2, mode == 1);
                }
            }
        }
        // A pixel.
        15 => {
            let (x, y) = (i(8), i(12));
            let m = g.m();
            let (px, py) = m.apply(x, y);
            let s = (g.px as f32).max(1.0);
            if let Some(r) = tiny_skia::Rect::from_xywh(px as f32, py as f32, s, s) {
                g.fill_with(&PathBuilder::from_rect(r), rgb(u(16) & 0xFF_FFFF));
            }
        }
        _ => {}
    }
}
