//! Insert ▸ Sparklines from the typed bar (docs/spec/officina.md §16.143):
//! `line B2:F9` draws one sparkline in each selected cell from the matching
//! row (or column) of the data; `column`, `winloss`, `markers` and
//! `color 00B050` change them; `clear` takes the selection's away. One undo
//! step each.

use officina_grid::addr::{a1, Rect};
use officina_grid::ops::GridOp;
use officina_grid::sparkline::{SparkKind, Sparkline, DEFAULT_COLOR, DEFAULT_NEGATIVE, MAX_POINTS, MAX_SPARKLINES};
use pergola::widgets::CellRange;

use crate::doc::Doc;

fn kind_word(k: SparkKind) -> &'static str {
    match k {
        SparkKind::Line => "line",
        SparkKind::Column => "column",
        SparkKind::WinLoss => "winloss",
    }
}

fn area_text(d: &Doc, r: &Rect) -> String {
    let cells = format!("{}:{}", a1(r.r0, r.c0), a1(r.r1, r.c1));
    if r.sheet == d.sheet { cells } else { format!("'{}'!{cells}", d.wb.sheet_name(r.sheet).replace('\'', "''")) }
}

/// The bar's starting text: the selection's first sparkline as typed, else
/// a line over the numbers left of the selection.
pub fn start(d: &Doc, sel: CellRange) -> String {
    let sh = d.wb.sheet(d.sheet);
    if let Some(sp) = sh.sparklines().iter().find(|s| (sel.r0..=sel.r1).contains(&s.row) && (sel.c0..=sel.c1).contains(&s.col)) {
        let mut t = format!("{} {}", kind_word(sp.kind), area_text(d, &sp.data));
        if sp.markers {
            t.push_str(" markers");
        }
        if sp.color != DEFAULT_COLOR {
            t.push_str(&format!(" color {:06X}", sp.color));
        }
        return t;
    }
    // The block just left of a selected column.
    if sel.c0 > 0 && sel.c0 == sel.c1 {
        let region = crate::data::current_region(&d.wb, d.sheet, sel.r0, sel.c0 - 1);
        if region.c1 == sel.c0 - 1 {
            let r = Rect { r0: sel.r0, r1: sel.r1, ..region };
            return format!("line {}", area_text(d, &r));
        }
    }
    "line ".into()
}

fn hex(t: &str) -> Result<u32, String> {
    let h = t.trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return Ok(v);
        }
    }
    Err(format!("{t:?} isn't a colour: type six hex digits such as 00B050."))
}

/// Runs what was typed in the sparkline bar on the selection.
pub fn run(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    let s = d.sheet;
    let inside = |sp: &Sparkline| (sel.r0..=sel.r1).contains(&sp.row) && (sel.c0..=sel.c1).contains(&sp.col);
    let list = d.wb.sheet(s).sparklines().to_vec();
    let words: Vec<&str> = typed.split_whitespace().collect();
    if words.first().is_some_and(|w| w.eq_ignore_ascii_case("clear")) {
        let kept: Vec<Sparkline> = list.iter().filter(|sp| !inside(sp)).cloned().collect();
        let n = list.len() - kept.len();
        if n == 0 {
            return Err("No sparklines in the selection.".into());
        }
        d.run("Clear Sparklines", vec![GridOp::Sparklines { sheet: s, list: kept }]);
        return protected(d).map(|_| format!("{n} {} cleared.", if n == 1 { "sparkline" } else { "sparklines" }));
    }
    let (mut kind, mut data, mut markers, mut color) = (None, None, false, None);
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        match w.to_lowercase().as_str() {
            "line" | "lines" => kind = Some(SparkKind::Line),
            "column" | "columns" => kind = Some(SparkKind::Column),
            "winloss" | "win/loss" | "win-loss" => kind = Some(SparkKind::WinLoss),
            "markers" | "marked" => markers = true,
            "color" | "colour" => {
                i += 1;
                color = Some(hex(words.get(i).ok_or("Type a colour after color, such as 00B050.")?)?);
            }
            _ => data = Some(crate::whatif::parse_area(&d.wb, s, w)?),
        }
        i += 1;
    }
    let kind = kind.unwrap_or(SparkKind::Line);
    let existing: Vec<&Sparkline> = list.iter().filter(|sp| inside(sp)).collect();
    let mut made: Vec<Sparkline> = Vec::new();
    match data {
        // Restyle the selection's sparklines, their data kept.
        None if !existing.is_empty() => {
            for sp in existing {
                made.push(Sparkline { kind, markers, color: color.unwrap_or(sp.color), ..sp.clone() });
            }
        }
        None => return Err("Type the kind and the data, such as: line B2:F9".into()),
        Some(data) => {
            let (nr, nc) = (sel.r1 - sel.r0 + 1, sel.c1 - sel.c0 + 1);
            if nr > 1 && nc > 1 {
                return Err("Select one row or one column of cells for the sparklines.".into());
            }
            let cells = nr.max(nc);
            let (dr, dc) = (data.r1 - data.r0 + 1, data.c1 - data.c0 + 1);
            // One sparkline per data row when the cells run down, per data column across.
            let rows_each = if cells == 1 { dr == 1 || dc != 1 } else { nr > 1 };
            let count = if cells == 1 { 1 } else if rows_each { dr } else { dc };
            if count != cells && !(cells == 1 && (dr == 1 || dc == 1)) {
                return Err(format!("The data has {count} {}, the selection {cells} cells: they must match.", if rows_each { "rows" } else { "columns" }));
            }
            for k in 0..cells {
                let (row, col) = if nr > 1 { (sel.r0 + k, sel.c0) } else { (sel.r0, sel.c0 + k) };
                let part = if cells == 1 {
                    data
                } else if rows_each {
                    Rect { r0: data.r0 + k, r1: data.r0 + k, ..data }
                } else {
                    Rect { c0: data.c0 + k, c1: data.c0 + k, ..data }
                };
                if part.r0 != part.r1 && part.c0 != part.c1 {
                    return Err("Each sparkline reads one row or one column of numbers.".into());
                }
                if part.cells() > MAX_POINTS {
                    return Err(format!("A sparkline draws at most {MAX_POINTS} points."));
                }
                if part.sheet == s && (part.r0..=part.r1).contains(&row) && (part.c0..=part.c1).contains(&col) {
                    return Err(format!("{} is in its own data: put sparklines beside the data.", a1(row, col)));
                }
                made.push(Sparkline { row, col, data: part, kind, color: color.unwrap_or(DEFAULT_COLOR), negative: DEFAULT_NEGATIVE, markers });
            }
        }
    }
    let n = made.len();
    let mut next: Vec<Sparkline> = list.into_iter().filter(|sp| !made.iter().any(|m| (m.row, m.col) == (sp.row, sp.col))).collect();
    if next.len() + n > MAX_SPARKLINES {
        return Err(format!("A sheet keeps at most {MAX_SPARKLINES} sparklines."));
    }
    next.extend(made);
    d.run("Sparklines", vec![GridOp::Sparklines { sheet: s, list: next }]);
    protected(d).map(|_| format!("{n} {} {}.", kind_word(kind), if n == 1 { "sparkline" } else { "sparklines" }))
}

fn protected(d: &Doc) -> Result<(), String> {
    if crate::sheet_protection::refused(&d.status) { Err(d.status.clone()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparklines_go_in_one_per_row_restyle_clear_and_undo() {
        let mut d = Doc::from_csv("s.csv", b"1,3,2,5\n-1,2,-3,4\n").unwrap().0;
        d.wb.recalc();
        let sel = CellRange { r0: 0, c0: 4, r1: 1, c1: 4 };
        assert_eq!(start(&d, sel), "line A1:D2");
        let m = run(&mut d, sel, "line A1:D2").unwrap();
        assert_eq!(m, "2 line sparklines.");
        let sh = d.wb.sheet(0);
        assert_eq!(sh.sparklines().len(), 2);
        assert_eq!(sh.sparkline_at(1, 4).unwrap().data, Rect { sheet: 0, r0: 1, c0: 0, r1: 1, c1: 3 });
        assert_eq!(sh.sparkline_at(1, 4).unwrap().points(&d.wb), [Some(-1.0), Some(2.0), Some(-3.0), Some(4.0)]);
        // Restyle in place: the data stays.
        run(&mut d, sel, "winloss markers color 00B050").unwrap();
        let sp = d.wb.sheet(0).sparkline_at(0, 4).unwrap().clone();
        assert_eq!((sp.kind, sp.markers, sp.color, sp.data.r0), (SparkKind::WinLoss, true, 0x00B050, 0));
        assert_eq!(start(&d, sel), "winloss A1:D1 markers color 00B050");
        assert!(d.undo());
        assert_eq!(d.wb.sheet(0).sparkline_at(0, 4).unwrap().kind, SparkKind::Line);
        // Refusals: shape mismatch, inside the data, a bad colour.
        assert!(run(&mut d, CellRange { r0: 0, c0: 4, r1: 2, c1: 4 }, "line A1:D2").unwrap_err().contains("must match"));
        assert!(run(&mut d, CellRange { r0: 0, c0: 1, r1: 0, c1: 1 }, "line A1:D1").unwrap_err().contains("its own data"));
        assert!(run(&mut d, sel, "line A1:D2 color green").unwrap_err().contains("isn't a colour"));
        // A column inserted before the data moves the sparklines and their data.
        let op = officina_grid::shift::shift_op(&d.wb, 0, officina_grid::shift::Axis::Cols, 0, 1).unwrap();
        d.run("Insert Columns", vec![op]);
        let sp = d.wb.sheet(0).sparkline_at(0, 5).unwrap();
        assert_eq!((sp.data.c0, sp.data.c1), (1, 4));
        // Kept in the native file; clear takes them away.
        let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&d.wb)).unwrap();
        assert_eq!(back.sheet(0).sparklines(), d.wb.sheet(0).sparklines());
        let sel = CellRange { r0: 0, c0: 5, r1: 1, c1: 5 };
        assert_eq!(run(&mut d, sel, "clear").unwrap(), "2 sparklines cleared.");
        assert!(d.wb.sheet(0).sparklines().is_empty());
    }

    #[test]
    fn a_cell_with_a_sparkline_shows_it() {
        use pergola::widgets::GridSource;
        let mut d = Doc::from_csv("v.csv", b"1,2,3\n").unwrap().0;
        d.wb.recalc();
        run(&mut d, CellRange { r0: 0, c0: 4, r1: 0, c1: 4 }, "column A1:C1").unwrap();
        let doc = std::rc::Rc::new(std::cell::RefCell::new(d));
        let src = crate::source::SheetSource::new(doc);
        let cv = src.cell(0, 4).expect("an empty cell with a sparkline has a view");
        let sp = cv.spark.expect("its sparkline");
        assert_eq!(sp.points, [Some(1.0), Some(2.0), Some(3.0)]);
        assert!(src.cell(0, 5).is_none());
    }
}
