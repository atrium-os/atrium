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

/// The current sheet from A1 to its last shown cell; Err when it is more
/// than a print can take.
pub fn snapshot(doc: &Rc<RefCell<Doc>>) -> Result<Snapshot, String> {
    let (rows, cols, name, col_px, row_px, from, to) = {
        let d = doc.borrow();
        let (mut r, mut c) = d.wb.extent(d.sheet);
        // A merged area that starts in the used range counts in full.
        let (r0, c0) = (r, c);
        for m in d.wb.merges(d.sheet).iter().filter(|m| m.r0 < r0 && m.c0 < c0) {
            r = r.max(m.r1 + 1);
            c = c.max(m.c1 + 1);
        }
        let sh = d.wb.sheet(d.sheet);
        let rs = if sh.hidden.is_empty() { sh.row_sizes.clone() } else { sh.view_row_sizes() };
        let col_px: Vec<f64> = (0..c).map(|i| sh.col_sizes.offset(i + 1) - sh.col_sizes.offset(i)).collect();
        let row_px: Vec<f64> = (0..r).map(|i| rs.offset(i + 1) - rs.offset(i)).collect();
        (r, c, sh.name.clone(), col_px, row_px, sh.freeze_from, sh.freeze)
    };
    if rows as u64 * cols as u64 > MAX_CELLS {
        return Err(format!("the sheet has more than {MAX_CELLS} cells to print"));
    }
    let src = SheetSource::new(doc.clone());
    let mut cells = Vec::with_capacity((rows * cols) as usize);
    let (mut last_r, mut last_c) = (0, 0);
    let mut across = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let v = src.cell(r, c);
            // A run, as the grid draws it: the empty cells after this one
            // that centre across too.
            if v.as_ref().is_some_and(|v| v.align == CellAlign::Across && !v.text.is_empty() && !v.text.contains('\n')) {
                let mut b = c;
                while b + 1 < cols && b - c < pergola::widgets::grid_view::SPILL_COLS && src.merge_at(r, b + 1).is_none() && src.cell(r, b + 1).is_some_and(|w| w.text.is_empty() && w.align == CellAlign::Across) {
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
    let merges = doc.borrow().wb.merges(doc.borrow().sheet).to_vec();
    for m in &merges {
        if cells[(m.r0 * cols + m.c0) as usize].is_some() {
            last_r = last_r.max((m.r1 + 1).min(rows));
            last_c = last_c.max((m.c1 + 1).min(cols));
        }
    }
    let merges: Vec<[u32; 4]> = merges
        .iter()
        .filter(|m| m.r0 < last_r && m.c0 < last_c)
        .map(|m| [m.r0, m.c0, m.r1.min(last_r.saturating_sub(1)), m.c1.min(last_c.saturating_sub(1))])
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
