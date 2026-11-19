//! Page geometry in screen pixels, as in Officina Sheets. Paint at 144 dpi.
use super::{check, Error};
use crate::paint::glyphs;
use mail_office_copy::{OfficeCopy, PagePixels};
use officina_draw::chart::{self, Prim};
use officina_grid::{
    addr::CellAddr,
    charts::{Chart, ChartKind},
    style::{HAlign, Line, VAlign},
    Value, Workbook,
};
use officina_pdf::sheet::Shaper;
use officina_text::{layout::PinnedFonts, paint::Item, LU};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

const SCALE: f32 = 1.5;
const LU_PX: f32 = LU as f32 * 0.75;
const WIDTH: u32 = 1224;
const HEIGHT: u32 = 1584;
const LEFT: f32 = 40.;
const TOP: f32 = 56.;

fn ink(rgb: u32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255);
    p
}
fn rect(p: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, color: u32) -> Result<(), Error> {
    let r =
        Rect::from_xywh(x * SCALE, y * SCALE, w * SCALE, h * SCALE).ok_or("invalid rectangle")?;
    p.fill_rect(r, &ink(color), Transform::identity(), None);
    Ok(())
}
struct Page<'a> {
    pixels: Pixmap,
    fonts: &'a PinnedFonts,
}
impl Page<'_> {
    fn label(
        &mut self,
        text: &str,
        size: f32,
        bold: bool,
        color: u32,
        x: f32,
        y: f32,
        maxw: f32,
        maxh: f32,
        align: f32,
        valign: f32,
    ) -> Result<(), Error> {
        if text.is_empty() {
            return Ok(());
        }
        check(
            text.len() <= 4096 && !text.chars().any(char::is_control),
            "unsupported multiline or oversized text",
        )?;
        let shaper = Shaper::new(self.fonts, f64::from(size) * 0.75);
        let (mut items, w, h) = shaper.label(text, bold, color, 0, 0);
        let (w, h) = (w as f32 / LU_PX, h as f32 / LU_PX);
        check(w <= maxw && h <= maxh, "a label does not fit completely")?;
        let ox = ((x + (maxw - w) * align) * LU_PX).round() as i64;
        let oy = ((y + (maxh - h) * valign) * LU_PX).round() as i64;
        let mut painted = String::new();
        for item in &mut items {
            let Item::Glyphs(run) = item else {
                return Err("unsupported text paint".into());
            };
            run.x += ox;
            run.y += oy;
            glyphs(&mut self.pixels, self.fonts, run)?;
            painted.push_str(&run.text);
        }
        check(painted == text, "incomplete spreadsheet label")
    }
    fn chart(
        &mut self,
        book: &Workbook,
        ch: &Chart,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> Result<(), Error> {
        check(
            w >= 150. && h >= 100. && (1..=8).contains(&ch.series.len()),
            "chart layout too small or too many series",
        )?;
        let limit = 64;
        let data = ch.data(book, limit + 1);
        let mut labels = Vec::new();
        let title = ch.title.as_ref().map(|t| Chart::text(book, t));
        labels.extend(title.iter().cloned());
        labels.extend(
            ch.x_title
                .iter()
                .chain(ch.y_title.iter())
                .map(|v| v.to_string()),
        );
        for d in &data {
            check(
                !d.vals.is_empty()
                    && d.vals.len() <= limit
                    && d.vals
                        .iter()
                        .all(|v| v.is_some_and(|v| v.is_finite() && v.abs() <= 1e15)),
                "incomplete chart series",
            )?;
            check(
                d.cats.is_empty() || d.cats.len() == d.vals.len(),
                "incomplete chart categories",
            )?;
            if ch.kind == ChartKind::Scatter {
                check(
                    d.xs.len() == d.vals.len()
                        && d.xs.iter().all(|v| v.is_some_and(f64::is_finite)),
                    "incomplete scatter coordinates",
                )?;
            } else if !ch.kind.round() || ch.legend.is_some() {
                labels.extend(d.cats.iter().cloned());
            }
            if ch.legend.is_some() && !ch.kind.round() {
                labels.push(d.name.clone());
            }
        }
        if ch.kind.round() {
            check(
                data.len() == 1 && data[0].vals.iter().all(|v| v.is_some_and(|v| v > 0.)),
                "unsupported pie series",
            )?;
        }
        let measure = |t: &str, s: f32| {
            Shaper::new(self.fonts, f64::from(s) * 0.75).width(t, false) as f32 / LU_PX
        };
        let palette = chart::Style::default();
        let prims = chart::layout(ch, &data, title.as_deref(), w, h, &palette, &measure);
        let shown = prims
            .iter()
            .filter_map(|p| {
                if let Prim::Text { text, .. } = p {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<std::collections::BTreeSet<_>>();
        check(
            labels
                .iter()
                .filter(|v| !v.is_empty())
                .all(|v| shown.contains(v.as_str()))
                && !shown.iter().any(|v| v.ends_with('…')),
            "chart labels incomplete",
        )?;
        if ch.kind.round() {
            check(
                prims
                    .iter()
                    .filter(|p| matches!(p, Prim::Wedge { .. }))
                    .count()
                    == data[0].vals.len(),
                "incomplete chart slices",
            )?;
        }
        check(
            data.iter().all(|d| d.kind == ch.kind),
            "unsupported combination chart",
        )?;
        if !ch.kind.round() && ch.kind != ChartKind::Scatter {
            let align = if ch.kind == ChartKind::Bar {
                chart::Align::End
            } else {
                chart::Align::Middle
            };
            let axis_labels=prims.iter().filter(|p| matches!(p,Prim::Text{align:a,color,bold:false,..} if *a==align && *color==palette.muted)).count();
            check(
                axis_labels == data.iter().map(|s| s.vals.len()).max().unwrap_or(0),
                "chart skipped category labels",
            )?;
        }
        if ch.legend.is_some() {
            let count = prims
                .iter()
                .filter(|p| matches!(p,Prim::Text{color,bold:false,..} if *color==palette.text))
                .count();
            check(
                count
                    == if ch.kind.round() {
                        data[0].vals.len()
                    } else {
                        data.len()
                    },
                "chart legend incomplete",
            )?;
        }
        // The normal renderer may decimate a dense series. Account for
        // every data point before accepting its visual snapshot.
        for (index, series) in data.iter().enumerate().filter(|_| !ch.kind.round()) {
            let col = palette.series[index];
            let n = series.vals.len();
            let count = prims
                .iter()
                .map(|p| match (series.kind, p) {
                    (ChartKind::Bar | ChartKind::Column, Prim::Rect { color, .. })
                        if *color == col =>
                    {
                        1
                    }
                    (ChartKind::Scatter | ChartKind::Line, Prim::Dot { color, .. })
                        if *color == col =>
                    {
                        1
                    }
                    (ChartKind::Line, Prim::Line { color, pts, .. }) if *color == col => pts.len(),
                    (ChartKind::Area, Prim::Area { color, pts, .. }) if *color == col => {
                        if ch.grouping == officina_grid::charts::Grouping::Standard {
                            pts.len().saturating_sub(2)
                        } else {
                            pts.len() / 2
                        }
                    }
                    _ => 0,
                })
                .sum::<usize>();
            let legend_swatch = usize::from(
                ch.legend.is_some() && matches!(series.kind, ChartKind::Bar | ChartKind::Column),
            );
            check(count == n + legend_swatch, "chart series was shortened")?;
        }
        let rgb = |c: [f32; 4]| {
            c[..3]
                .iter()
                .fold(0u32, |a, s| (a << 8) | ((s * 255.).round() as u32))
        };
        let mut marks = 0;
        for p in prims {
            match p {
                Prim::Text {
                    x: a,
                    y: b,
                    text,
                    size,
                    color,
                    align,
                    bold,
                } => {
                    let width = Shaper::new(self.fonts, size as f64 * 0.75).width(&text, bold)
                        as f32
                        / LU_PX;
                    let a = a - match align {
                        chart::Align::Start => 0.,
                        chart::Align::End => width,
                        chart::Align::Middle => width / 2.,
                    };
                    check(
                        a >= -0.1 && a + width <= w + 0.1 && b >= 0.,
                        "chart label outside drawing",
                    )?;
                    self.label(
                        &text,
                        size,
                        bold,
                        rgb(color),
                        x + a,
                        y + b,
                        width + 0.01,
                        h - b,
                        0.,
                        0.,
                    )?;
                }
                Prim::Rect {
                    x: a,
                    y: b,
                    w: ww,
                    h: hh,
                    color,
                } => {
                    if ww > 0. && hh > 0. {
                        rect(&mut self.pixels, x + a, y + b, ww, hh, rgb(color))?;
                    }
                    marks += 1;
                }
                Prim::Line { pts, width, color } => {
                    self.path(&pts, x, y, Some(width), rgb(color))?;
                    marks += 1;
                }
                Prim::Area { pts, color } => {
                    self.path(&pts, x, y, None, rgb(color))?;
                    marks += 1;
                }
                Prim::Dot {
                    x: a,
                    y: b,
                    r,
                    color,
                } => {
                    let path =
                        PathBuilder::from_circle((x + a) * SCALE, (y + b) * SCALE, r * SCALE)
                            .ok_or("invalid scatter point")?;
                    self.pixels.fill_path(
                        &path,
                        &ink(rgb(color)),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                    marks += 1;
                }
                Prim::Wedge {
                    cx,
                    cy,
                    r_in,
                    r_out,
                    a0,
                    a1,
                    color,
                } => {
                    let n = (((a1 - a0).abs() * r_out * SCALE).ceil() as usize).clamp(3, 8192);
                    let point = |i: usize, r: f32| {
                        let a = a0 + (a1 - a0) * i as f32 / n as f32;
                        (cx + a.sin() * r, cy - a.cos() * r)
                    };
                    let pts = (0..=n)
                        .map(|i| point(i, r_out))
                        .chain((0..=n).rev().map(|i| point(i, r_in)))
                        .collect::<Vec<_>>();
                    self.path(&pts, x, y, None, rgb(color))?;
                    marks += 1;
                }
            }
        }
        check(marks > 1, "empty chart drawing")
    }
    fn path(
        &mut self,
        pts: &[(f32, f32)],
        x: f32,
        y: f32,
        stroke: Option<f32>,
        color: u32,
    ) -> Result<(), Error> {
        check(pts.len() >= 2, "incomplete chart path")?;
        let mut b = PathBuilder::new();
        for (i, (a, c)) in pts.iter().enumerate() {
            let (xx, yy) = ((x + a) * SCALE, (y + c) * SCALE);
            if i == 0 {
                b.move_to(xx, yy)
            } else {
                b.line_to(xx, yy)
            }
        }
        if stroke.is_none() {
            b.close()
        }
        let path = b.finish().ok_or("invalid chart path")?;
        if let Some(width) = stroke {
            self.pixels.stroke_path(
                &path,
                &ink(color),
                &Stroke {
                    width: width * SCALE,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        } else {
            self.pixels.fill_path(
                &path,
                &ink(color),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        Ok(())
    }
}

fn bands(sizes: &[f32], room: f32) -> Result<Vec<std::ops::Range<usize>>, Error> {
    let mut list = vec![];
    let mut start = 0;
    let mut used = 0.;
    for (i, &size) in sizes.iter().enumerate() {
        check(
            size.is_finite() && size > 0. && size <= room,
            "hidden or oversized cell dimension",
        )?;
        if used + size > room {
            list.push(start..i);
            start = i;
            used = 0.;
        }
        used += size;
    }
    list.push(start..sizes.len());
    Ok(list)
}
pub fn workbook(w: &Workbook) -> Result<OfficeCopy, Error> {
    let fonts = PinnedFonts::new()?;
    let mut pages = vec![];
    let mut encoded = 0;
    for i in 0..w.sheet_count() as u16 {
        let sheet = w.sheet(i);
        let (mut rows, mut cols) = w.extent(i);
        for m in w.merges(i) {
            rows = rows.max(m.r1 + 1);
            cols = cols.max(m.c1 + 1);
        }
        let mut drawings = vec![];
        for ch in w.charts().iter().filter(|ch| ch.anchor.sheet == i) {
            let point = |(r, c, x, y)| {
                (
                    sheet.col_sizes.offset(c) as f32 + x / 0.75,
                    sheet.row_sizes.offset(r) as f32 + y / 0.75,
                )
            };
            let (x, y) = point(ch.anchor.from);
            let (right, bottom) = point(ch.anchor.to);
            check(
                x >= 0.
                    && y >= 0.
                    && right > x
                    && bottom > y
                    && right.is_finite()
                    && bottom.is_finite(),
                "invalid chart bounds",
            )?;
            let end = |s: &officina_grid::column::Sizes, v: f32| {
                let n = s.index_at(v as f64);
                n.saturating_add(u32::from(s.offset(n) < v as f64))
            };
            rows = rows.max(end(&sheet.row_sizes, bottom));
            cols = cols.max(end(&sheet.col_sizes, right));
            drawings.push((ch, x, y, right, bottom));
        }
        check(
            rows <= 4096 && cols <= 256 && u64::from(rows) * u64::from(cols) <= 100_000,
            "drawing extent too large",
        )?;
        let rb = bands(
            &(0..rows)
                .map(|r| sheet.row_sizes.size(r) as f32)
                .collect::<Vec<_>>(),
            HEIGHT as f32 / SCALE - TOP - 40.,
        )?;
        let cb = bands(
            &(0..cols)
                .map(|c| sheet.col_sizes.size(c) as f32)
                .collect::<Vec<_>>(),
            WIDTH as f32 / SCALE - 2. * LEFT,
        )?;
        check(
            (pages.len() + rb.len() * cb.len()) as u64 * u64::from(WIDTH) * u64::from(HEIGHT)
                <= 48_000_000,
            "workbook page budget exceeded",
        )?;
        let mut drawn = 0;
        for cs in &cb {
            for rs in &rb {
                let mut page = Page {
                    pixels: Pixmap::new(WIDTH, HEIGHT).ok_or("bitmap allocation failed")?,
                    fonts: &fonts,
                };
                page.pixels.fill(Color::WHITE);
                page.label(&w.sheet_name(i), 15., true, 0, LEFT, 20., 730., 32., 0., 0.)?;
                let left = sheet.col_sizes.offset(cs.start as u32) as f32;
                let top = sheet.row_sizes.offset(rs.start as u32) as f32;
                for r in rs.clone() {
                    for c in cs.clone() {
                        let (r, c) = (r as u32, c as u32);
                        let a = CellAddr::new(i, r, c);
                        let (r1, c1) = if let Some(m) = w.merge_at(a) {
                            if (r, c) != (m.r0, m.c0) {
                                continue;
                            }
                            check(
                                m.r1 < rs.end as u32 && m.c1 < cs.end as u32,
                                "merge crosses page boundary",
                            )?;
                            (m.r1 + 1, m.c1 + 1)
                        } else {
                            (r + 1, c + 1)
                        };
                        let sx = sheet.col_sizes.offset(c);
                        let sy = sheet.row_sizes.offset(r);
                        let (x, y) = (LEFT + sx as f32 - left, TOP + sy as f32 - top);
                        let (cw, rh) = (
                            (sheet.col_sizes.offset(c1) - sx) as f32,
                            (sheet.row_sizes.offset(r1) - sy) as f32,
                        );
                        let st = w.style(a);
                        check(
                            !st.wrap
                                && st.rotation == 0
                                && !st.font.italic
                                && !st.font.underline
                                && !st.font.strike
                                && st.font.name.is_none()
                                && st.font.size.is_none(),
                            "unsupported cell typography",
                        )?;
                        rect(&mut page.pixels, x, y, cw, rh, st.fill.unwrap_or(0xffffff))?;
                        for (side, b) in st.borders.iter().enumerate() {
                            check(
                                matches!(
                                    b.line,
                                    Line::None | Line::Thin | Line::Medium | Line::Thick
                                ),
                                "unsupported cell border",
                            )?;
                            let (sw, col) = if b.line == Line::None {
                                (0.4, 0xd9dadc)
                            } else {
                                (b.line.width(), b.color.unwrap_or(0))
                            };
                            let (xx, yy, ww, hh) = match side {
                                0 => (x, y, sw, rh),
                                1 => (x + cw - sw, y, sw, rh),
                                2 => (x, y, cw, sw),
                                _ => (x, y + rh - sw, cw, sw),
                            };
                            rect(&mut page.pixels, xx, yy, ww, hh, col)?;
                        }
                        let val = w.get(a);
                        let formatted = w.formatted(a);
                        check(
                            !formatted.hashes
                                && w.styles.format(w.style_id(a)).is_exact()
                                && !st.num_fmt.contains(['*', '_']),
                            "unsupported number format",
                        )?;
                        let align = match st.h_align {
                            HAlign::Left => 0.,
                            HAlign::Right => 1.,
                            HAlign::Center => 0.5,
                            HAlign::General => match val {
                                Value::Num(_) => 1.,
                                Value::Bool(_) => 0.5,
                                _ => 0.,
                            },
                            _ => return Err("unsupported cell alignment".into()),
                        };
                        let valign = match st.v_align {
                            VAlign::Bottom => 1.,
                            VAlign::Center => 0.5,
                            VAlign::Top => 0.,
                            _ => return Err("unsupported vertical alignment".into()),
                        };
                        let inset = 4. + st.indent as f32 * 12.;
                        page.label(
                            &formatted.text,
                            12.,
                            st.font.bold,
                            formatted
                                .color
                                .map(|c| c.rgb())
                                .or(st.font.color)
                                .unwrap_or(0),
                            x + inset,
                            y,
                            cw - inset - 4.,
                            rh,
                            align,
                            valign,
                        )?;
                    }
                }
                for &(ch, x, y, right, bottom) in &drawings {
                    let (x1, y1) = (
                        sheet.col_sizes.offset(cs.end as u32) as f32,
                        sheet.row_sizes.offset(rs.end as u32) as f32,
                    );
                    if right <= left || bottom <= top || x >= x1 || y >= y1 {
                        continue;
                    }
                    check(
                        x >= left && y >= top && right <= x1 && bottom <= y1,
                        "chart crosses page boundary",
                    )?;
                    page.chart(w, ch, LEFT + x - left, TOP + y - top, right - x, bottom - y)?;
                    drawn += 1;
                }
                let rgb = page
                    .pixels
                    .data()
                    .chunks_exact(4)
                    .flat_map(|p| p[..3].iter().copied())
                    .collect::<Vec<_>>();
                let samples = miniz_oxide::deflate::compress_to_vec_zlib(&rgb, 1);
                encoded += samples.len();
                check(encoded <= 32 << 20, "workbook output too large")?;
                pages.push(PagePixels {
                    width_twips: WIDTH * 10,
                    height_twips: HEIGHT * 10,
                    samples,
                });
            }
        }
        check(drawn == drawings.len(), "incomplete sheet drawings")?;
    }
    Ok(OfficeCopy { pages })
}
