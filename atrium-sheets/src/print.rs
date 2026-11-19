//! Printing (printing.md §5.1): the sheet on screen as a PDF for the print
//! sheet. Each cell is taken as the grid draws it ([`SheetSource`]'s
//! cells: number formats, conditional formats, pivots), so the page shows
//! what the screen does; officina-pdf lays out the pages.

use std::cell::RefCell;
use std::rc::Rc;

use officina_pdf::sheet::{Align, Cell, Setup, Sheet, MAX_CELLS};
use pergola::widgets::{CellAlign, CellView, GridSource};
use pergola::Color;

use crate::doc::Doc;
use crate::source::SheetSource;

/// The sheet's printable part, taken on the window's thread and printed on
/// another.
pub struct Snapshot {
    pub name: String,
    pub cols: Vec<f64>,
    pub rows: Vec<f64>,
    /// Row by row.
    pub cells: Vec<Option<Cell>>,
    /// Merged areas, `[r0, c0, r1, c1]` inclusive, cut to the snapshot.
    pub merges: Vec<[u32; 4]>,
    /// Center Across Selection runs, `[row, c0, c1]` inclusive.
    pub across: Vec<[u32; 3]>,
    /// The frozen rows and columns (`a..b`), repeated on every page.
    pub title_rows: (u32, u32),
    pub title_cols: (u32, u32),
}

fn srgb(c: Color) -> u32 {
    let e = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
        (s * 255.0).round() as u32
    };
    (e(c.r) << 16) | (e(c.g) << 8) | e(c.b)
}

/// A cell as the grid draws it, for the page.
pub fn cell_of(v: CellView) -> Cell {
    Cell {
        text: v.text,
        align: match v.align {
            CellAlign::Left => Align::Left,
            CellAlign::Center | CellAlign::Across => Align::Center,
            CellAlign::Right => Align::Right,
        },
        bold: v.bold,
        numeric: v.numeric,
        color: v.color.map(srgb),
        fill: v.fill.map(srgb),
        borders: v.borders.map(|b| b.map(|(w, c)| (w, srgb(c)))),
        bar: v.bar.map(|(s, c)| (s, srgb(c))),
        indent: v.indent,
    }
}

/// The imported Excel print area, when the current sheet has one.
fn print_area(d: &Doc) -> Result<Option<[u32; 4]>, String> {
    use officina_grid::formula::ast::{Coord::Abs, Expr, RangeKind};
    let Some(name) = d.wb.names().get(d.sheet, "_xlnm.Print_Area") else { return Ok(None) };
    let (sheet, bounds) = match name.expr.as_ref() {
        Expr::Ref(r) => (r.sheet, match (r.row, r.col) {
            (Abs(row), Abs(col)) => Some([row, col, row + 1, col + 1]),
            _ => None,
        }),
        Expr::Range(r) => (r.sheet, match (r.kind, r.r0, r.c0, r.r1, r.c1) {
            (RangeKind::Cells, Abs(r0), Abs(c0), Abs(r1), Abs(c1)) =>
                Some([r0.min(r1), c0.min(c1), r0.max(r1) + 1, c0.max(c1) + 1]),
            _ => None,
        }),
        _ => (None, None),
    };
    if name.scope.is_none() && sheet.is_some_and(|sheet| sheet != d.sheet) {
        return Ok(None);
    }
    bounds.filter(|_| sheet.is_none_or(|sheet| sheet == d.sheet)).map(Some)
        .ok_or_else(|| "The print area must be one absolute cell or rectangular range on this sheet".into())
}

/// The current sheet's print area or its used range, captured at print time.
pub fn snapshot(doc: &Rc<RefCell<Doc>>) -> Result<Snapshot, String> {
    let (top, left, rows, cols, name, col_px, row_px, from, to, fixed, merges) = {
        let d = doc.borrow();
        let area = print_area(&d)?;
        let (r, c) = d.wb.extent(d.sheet);
        let (mut bottom, mut right) = (r, c);
        let all_merges = d.wb.merges(d.sheet);
        if area.is_none() {
            for m in all_merges.iter().filter(|m| m.r0 < r && m.c0 < c) {
                bottom = bottom.max(m.r1 + 1);
                right = right.max(m.c1 + 1);
            }
        }
        let [top, left, bottom, right] = area.unwrap_or([0, 0, bottom, right]);
        let (rows, cols) = (bottom - top, right - left);
        if u64::from(rows) * u64::from(cols) > MAX_CELLS {
            return Err(format!("the print area has more than {MAX_CELLS} cells"));
        }
        let sh = d.wb.sheet(d.sheet);
        let rs = if sh.hidden.is_empty() { sh.row_sizes.clone() } else { sh.view_row_sizes() };
        let col_px = (left..right).map(|i| sh.col_sizes.offset(i + 1) - sh.col_sizes.offset(i)).collect::<Vec<_>>();
        let row_px = (top..bottom).map(|i| rs.offset(i + 1) - rs.offset(i)).collect::<Vec<_>>();
        let merges = all_merges.iter()
            .filter(|m| (top..bottom).contains(&m.r0) && (left..right).contains(&m.c0))
            .map(|m| [m.r0 - top, m.c0 - left, m.r1.min(bottom - 1) - top, m.c1.min(right - 1) - left])
            .collect::<Vec<_>>();
        let from = (sh.freeze_from.0.saturating_sub(top), sh.freeze_from.1.saturating_sub(left));
        let to = (sh.freeze.0.saturating_sub(top), sh.freeze.1.saturating_sub(left));
        (top, left, rows, cols, sh.name.clone(), col_px, row_px, from, to, area.is_some(), merges)
    };
    let src = SheetSource::new(doc.clone());
    let mut cells = Vec::with_capacity((rows * cols) as usize);
    let (mut last_r, mut last_c) = (0, 0);
    let mut across = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let v = src.cell(top + r, left + c);
            // A run, as the grid draws it: the empty cells after this one
            // that centre across too.
            if v.as_ref().is_some_and(|v| v.align == CellAlign::Across && !v.text.is_empty() && !v.text.contains('\n')) {
                let mut b = c;
                while b + 1 < cols && b - c < pergola::widgets::grid_view::SPILL_COLS && src.merge_at(top + r, left + b + 1).is_none() && src.cell(top + r, left + b + 1).is_some_and(|w| w.text.is_empty() && w.align == CellAlign::Across) {
                    b += 1;
                }
                if b > c {
                    across.push([r, c, b]);
                    last_c = last_c.max(b + 1);
                }
            }
            let v = v.filter(|v| !v.text.is_empty() || v.fill.is_some() || v.borders.iter().any(Option::is_some) || v.bar.is_some());
            if v.is_some() {
                last_r = last_r.max(r + 1);
                last_c = last_c.max(c + 1);
            }
            cells.push(v.map(cell_of));
        }
    }
    // A merged area whose first cell shows something is printed whole.
    for &[r0, c0, r1, c1] in &merges {
        if cells[(r0 * cols + c0) as usize].is_some() {
            last_r = last_r.max(r1 + 1);
            last_c = last_c.max(c1 + 1);
        }
    }
    if fixed {
        (last_r, last_c) = (rows, cols);
    }
    let merges = merges.into_iter()
        .filter(|m| m[0] < last_r && m[1] < last_c)
        .map(|m| [m[0], m[1], m[2].min(last_r - 1), m[3].min(last_c - 1)])
        .collect();
    let across: Vec<[u32; 3]> = across.into_iter().filter(|a| a[0] < last_r).collect();
    // Only up to the last cell that shows anything.
    let mut kept = Vec::with_capacity((last_r * last_c) as usize);
    for r in 0..last_r {
        let at = (r * cols) as usize;
        kept.extend(cells[at..at + last_c as usize].iter().cloned());
    }
    Ok(Snapshot { name, cols: col_px[..last_c as usize].to_vec(), rows: row_px[..last_r as usize].to_vec(), cells: kept, merges, across, title_rows: (from.0, to.0.min(last_r)), title_cols: (from.1, to.1.min(last_c)) })
}

/// The snapshot as PDF on paper `w` × `h` points.
pub fn pdf(s: &Snapshot, w: f64, h: f64, title: &str) -> Result<Vec<u8>, String> {
    let n = s.cols.len();
    let get = |r: u32, c: u32| s.cells.get(r as usize * n + c as usize).cloned().flatten();
    let sheet = Sheet { name: &s.name, cols: s.cols.clone(), rows: s.rows.clone(), cell: &get, gridlines: true, merges: s.merges.clone(), across: s.across.clone(), title_rows: s.title_rows, title_cols: s.title_cols };
    officina_pdf::sheet::print(&sheet, &Setup::paper(w, h), title).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_is_what_the_grid_shows_up_to_the_last_cell() {
        let doc = Rc::new(RefCell::new(Doc::new("Q3")));
        {
            let mut d = doc.borrow_mut();
            d.type_into(0, 0, "Item");
            d.type_into(2, 1, "0.25");
        }
        let s = snapshot(&doc).unwrap();
        assert_eq!((s.rows.len(), s.cols.len()), (3, 2));
        assert_eq!(s.cells[0].as_ref().unwrap().text, "Item");
        let v = s.cells[2 * 2 + 1].as_ref().unwrap();
        assert_eq!((v.text.as_str(), v.numeric, v.align), ("0.25", true, Align::Right));
        assert_eq!(s.cols[0], 64.0);
        let pdf = pdf(&s, 595.0, 842.0, "Q3").unwrap();
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn imported_print_area_limits_the_snapshot() {
        let doc = Rc::new(RefCell::new(Doc::new("Report")));
        {
            let mut d = doc.borrow_mut();
            d.type_into(0, 0, "Outside");
            d.type_into(2, 3, "Inside");
            d.wb.define_name("_xlnm.Print_Area", Some(0), "=$D$3:$E$5").unwrap();
        }
        let s = snapshot(&doc).unwrap();
        assert_eq!((s.rows.len(), s.cols.len()), (3, 2));
        assert_eq!(s.cells[0].as_ref().unwrap().text, "Inside");
        assert_eq!(s.cells.iter().flatten().count(), 1);
    }

    #[test]
    fn a_merged_title_is_printed_whole() {
        use officina_grid::addr::Rect;
        use officina_grid::ops::GridOp;
        let doc = Rc::new(RefCell::new(Doc::new("Q3")));
        {
            let mut d = doc.borrow_mut();
            d.type_into(0, 0, "Quarter");
            d.type_into(2, 1, "5");
            d.run("Merge Cells", vec![GridOp::Merges { sheet: 0, list: vec![Rect { sheet: 0, r0: 0, c0: 0, r1: 0, c1: 3 }] }]);
        }
        let s = snapshot(&doc).unwrap();
        // The title reaches D, past the last value (B3).
        assert_eq!((s.rows.len(), s.cols.len()), (3, 4));
        assert_eq!(s.merges, vec![[0, 0, 0, 3]]);
        assert!(pdf(&s, 595.0, 842.0, "Q3").unwrap().starts_with(b"%PDF"));
    }

    #[test]
    fn a_title_centred_across_prints_across_its_run() {
        use officina_grid::addr::Rect;
        use officina_grid::ops::GridOp;
        use officina_grid::style::{HAlign, Style};
        let doc = Rc::new(RefCell::new(Doc::new("Q3")));
        {
            let mut d = doc.borrow_mut();
            d.type_into(0, 0, "Quarter");
            d.type_into(1, 3, "5");
            let r = Rect { sheet: 0, r0: 0, c0: 0, r1: 0, c1: 3 };
            let op = GridOp::restyle(&d.wb, r, |st| Style { h_align: HAlign::CenterAcross, ..st.clone() });
            d.run("Center Across Selection", vec![op]);
        }
        let s = snapshot(&doc).unwrap();
        assert_eq!(s.across, vec![[0, 0, 3]]);
        assert!(pdf(&s, 595.0, 842.0, "Q3").unwrap().starts_with(b"%PDF"));
    }

    #[test]
    fn frozen_rows_are_the_print_titles() {
        let doc = Rc::new(RefCell::new(Doc::new("Q3")));
        {
            let mut d = doc.borrow_mut();
            for r in 0..5 {
                d.type_into(r, 0, "x");
            }
            d.type_into(0, 1, "y");
            d.run("Freeze Panes", vec![officina_grid::ops::GridOp::Freeze { sheet: 0, rows: 2, cols: 1, from: (0, 0) }]);
        }
        let s = snapshot(&doc).unwrap();
        assert_eq!((s.title_rows, s.title_cols), ((0, 2), (0, 1)));
    }

    #[test]
    fn colours_go_back_to_srgb() {
        assert_eq!(srgb(Color::from_srgb8(0x12, 0x80, 0xFE)), 0x1280FE);
    }
}
