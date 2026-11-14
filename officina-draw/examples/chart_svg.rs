//! chart_svg: every chart kind laid out and written as one SVG page (a look at the
//! layout without the UI). `cargo run --example chart_svg > charts.svg`

use officina_draw::chart::{layout, Align, Prim, Style};
use officina_grid::addr::{CellAddr, Rect};
use officina_grid::charts::*;
use officina_grid::{Value, Workbook};

fn rgb(c: [f32; 4]) -> String {
    format!("rgb({},{},{})", (c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}

fn main() {
    let mut wb = Workbook::new();
    wb.add_sheet("D").unwrap();
    for (i, h) in ["Month", "Sales", "Costs", "Other"].iter().enumerate() {
        wb.set_value(CellAddr::new(0, 0, i as u32), Value::str(h));
    }
    for r in 1..=12u32 {
        wb.set_value(CellAddr::new(0, r, 0), Value::str(&format!("M{r}")));
        wb.set_value(CellAddr::new(0, r, 1), Value::Num((r * 7 % 11) as f64 + 1.0));
        wb.set_value(CellAddr::new(0, r, 2), Value::Num((r * 5 % 7) as f64 - 2.5));
        wb.set_value(CellAddr::new(0, r, 3), Value::Num((r * 3 % 5) as f64 + 0.5));
    }
    let col = |c: u32| Rect { sheet: 0, r0: 1, c0: c, r1: 12, c1: c };
    let ser = |c: u32, kind| Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, c))), cats: Some(col(0)), vals: col(c), kind };
    let (w, h) = (360.0f32, 230.0f32);
    let mut svg = String::new();
    let kinds = [
        (ChartKind::Column, Grouping::Standard, None),
        (ChartKind::Column, Grouping::Stacked, None),
        (ChartKind::Bar, Grouping::Percent, None),
        (ChartKind::Line, Grouping::Standard, None),
        (ChartKind::Area, Grouping::Stacked, None),
        (ChartKind::Scatter, Grouping::Standard, None),
        (ChartKind::Pie, Grouping::Standard, None),
        (ChartKind::Doughnut, Grouping::Standard, None),
        (ChartKind::Column, Grouping::Standard, Some(ChartKind::Line)),
    ];
    for (i, (kind, g, combo)) in kinds.iter().enumerate() {
        let series = if kind.round() {
            vec![ser(1, None)]
        } else if *kind == ChartKind::Scatter {
            vec![Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, 3))), cats: Some(col(1)), vals: col(3), kind: None }]
        } else {
            vec![ser(1, None), ser(2, *combo), ser(3, None)]
        };
        let c = Chart {
            kind: *kind,
            grouping: *g,
            title: Some(ChartText::Lit(format!("{kind:?} {g:?}").into())),
            series,
            legend: Some([LegendPos::Right, LegendPos::Bottom, LegendPos::Top][i % 3]),
            x_title: (!kind.round()).then(|| "Month".into()),
            y_title: (!kind.round()).then(|| "Amount".into()),
            anchor: Anchor { sheet: 0, from: (0, 0, 0.0, 0.0), to: (1, 1, 0.0, 0.0) },
        };
        let data = c.data(&wb, 100_000);
        let title = c.title.as_ref().map(|t| Chart::text(&wb, t));
        let prims = layout(&c, &data, title.as_deref(), w, h, &Style::default(), &|t, s| t.chars().count() as f32 * s * 0.55);
        let (ox, oy) = ((i % 3) as f32 * (w + 10.0), (i / 3) as f32 * (h + 10.0));
        svg.push_str(&format!(r#"<g transform="translate({ox},{oy})">"#));
        for p in prims {
            match p {
                Prim::Rect { x, y, w, h, color } => svg.push_str(&format!(r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{}"/>"#, rgb(color))),
                Prim::Line { pts, width, color } => svg.push_str(&format!(r#"<polyline fill="none" stroke="{}" stroke-width="{width}" points="{}"/>"#, rgb(color), pts.iter().map(|(x, y)| format!("{x},{y}")).collect::<Vec<_>>().join(" "))),
                Prim::Area { pts, color } => svg.push_str(&format!(r#"<polygon fill="{}" fill-opacity="0.8" points="{}"/>"#, rgb(color), pts.iter().map(|(x, y)| format!("{x},{y}")).collect::<Vec<_>>().join(" "))),
                Prim::Dot { x, y, r, color } => svg.push_str(&format!(r#"<circle cx="{x}" cy="{y}" r="{r}" fill="{}"/>"#, rgb(color))),
                Prim::Wedge { cx, cy, r_in, r_out, a0, a1, color } => {
                    let p = |r: f32, a: f32| (cx + r * a.sin(), cy - r * a.cos());
                    let large = if a1 - a0 > std::f32::consts::PI { 1 } else { 0 };
                    let (s0, s1, i1, i0) = (p(r_out, a0), p(r_out, a1), p(r_in, a1), p(r_in, a0));
                    svg.push_str(&format!(
                        r#"<path fill="{}" stroke="white" d="M{} {} A{r_out} {r_out} 0 {large} 1 {} {} L{} {} A{r_in} {r_in} 0 {large} 0 {} {} Z"/>"#,
                        rgb(color), s0.0, s0.1, s1.0, s1.1, i1.0, i1.1, i0.0, i0.1
                    ));
                }
                Prim::Text { x, y, text, size, color, align, bold } => {
                    let anchor = match align {
                        Align::Start => "start",
                        Align::Middle => "middle",
                        Align::End => "end",
                    };
                    let t = text.replace('&', "&amp;").replace('<', "&lt;");
                    svg.push_str(&format!(r#"<text x="{x}" y="{}" font-family="sans-serif" font-size="{size}" fill="{}" text-anchor="{anchor}" font-weight="{}">{t}</text>"#, y + size, rgb(color), if bold { 600 } else { 400 }));
                }
            }
        }
        svg.push_str("</g>");
    }
    println!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" style="background:#eee">{svg}</svg>"#, 3.0 * (w + 10.0), 3.0 * (h + 10.0));
}
