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
            CellAlign::Center => Align::Center,
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
    let (rows, cols, name, col_px, row_px) = {
        let d = doc.borrow();
        let (r, c) = d.wb.extent(d.sheet);
        let sh = d.wb.sheet(d.sheet);
        let rs = if sh.hidden.is_empty() { sh.row_sizes.clone() } else { sh.view_row_sizes() };
        let col_px: Vec<f64> = (0..c).map(|i| sh.col_sizes.offset(i + 1) - sh.col_sizes.offset(i)).collect();
        let row_px: Vec<f64> = (0..r).map(|i| rs.offset(i + 1) - rs.offset(i)).collect();
        (r, c, sh.name.clone(), col_px, row_px)
    };
    if rows as u64 * cols as u64 > MAX_CELLS {
        return Err(format!("the sheet has more than {MAX_CELLS} cells to print"));
    }
    let src = SheetSource::new(doc.clone());
    let mut cells = Vec::with_capacity((rows * cols) as usize);
    let (mut last_r, mut last_c) = (0, 0);
    for r in 0..rows {
        for c in 0..cols {
            let v = src.cell(r, c).filter(|v| !v.text.is_empty() || v.fill.is_some() || v.borders.iter().any(Option::is_some) || v.bar.is_some());
            if v.is_some() {
                last_r = last_r.max(r + 1);
                last_c = last_c.max(c + 1);
            }
            cells.push(v.map(cell_of));
        }
    }
    // Only up to the last cell that shows anything.
    let mut kept = Vec::with_capacity((last_r * last_c) as usize);
    for r in 0..last_r {
        let at = (r * cols) as usize;
        kept.extend(cells[at..at + last_c as usize].iter().cloned());
    }
    Ok(Snapshot { name, cols: col_px[..last_c as usize].to_vec(), rows: row_px[..last_r as usize].to_vec(), cells: kept })
}

/// The snapshot as PDF on paper `w` × `h` points.
pub fn pdf(s: &Snapshot, w: f64, h: f64, title: &str) -> Result<Vec<u8>, String> {
    let n = s.cols.len();
    let get = |r: u32, c: u32| s.cells.get(r as usize * n + c as usize).cloned().flatten();
    let sheet = Sheet { name: &s.name, cols: s.cols.clone(), rows: s.rows.clone(), cell: &get, gridlines: true };
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
    fn colours_go_back_to_srgb() {
        assert_eq!(srgb(Color::from_srgb8(0x12, 0x80, 0xFE)), 0x1280FE);
    }
}
