//! Bars for a sheet's own settings and the selection's font size: Font
//! Size…, Move or Copy Sheet…, Standard Width… and Standard Height….

use officina_grid::ops::GridOp;
use officina_grid::{CellAddr, Rect};
use pergola::widgets::CellRange;

use crate::doc::Doc;

/// The largest font size, in points, as Excel's.
pub const MAX_POINTS: f64 = 409.0;
/// The widest column or tallest row a sheet's standard size may be, px
/// (as the native format keeps).
pub const MAX_STANDARD_PX: f64 = 8192.0;
/// The longest text the Move or Copy bar reads.
const MAX_TYPED: usize = 512;

/// The selection's font size, as the bar starts: `11`, `10.5`.
pub fn font_size_start(d: &Doc, sel: CellRange) -> String {
    let tw = d.wb.style(CellAddr::new(d.sheet, sel.r0, sel.c0)).font.size.unwrap_or(220);
    points(tw)
}

fn points(twentieths: u16) -> String {
    let p = twentieths as f64 / 20.0;
    if p.fract() == 0.0 { format!("{p:.0}") } else { format!("{p}") }
}

/// A typed size in twentieths of a point: any size from 1 to 409 points
/// in half points (`10.5`, `12 pt`), as Excel takes.
pub fn typed_points(typed: &str) -> Option<u16> {
    let t = typed.trim().to_ascii_lowercase();
    let t = t.strip_suffix("pt").unwrap_or(&t).trim();
    let p: f64 = t.parse().ok().filter(|p: &f64| p.is_finite())?;
    let halves = (p * 2.0).round();
    if halves.fract() != 0.0 || (p * 2.0 - halves).abs() > 1e-9 || !(1.0..=MAX_POINTS).contains(&p) {
        return None;
    }
    // 2..=818 halves, so at most 8180 twentieths.
    Some(halves as u16 * 10)
}

/// Font Size…: the selection's text at a typed size; 11 pt is the
/// default and is kept as no size at all, as Larger and Smaller do.
pub fn font_size(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    let tw = typed_points(typed).ok_or("Type a size from 1 to 409 points, in whole or half points.")?;
    let r = Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    let size = (tw != 220).then_some(tw);
    let op = GridOp::restyle(&d.wb, r, |st| {
        let mut st = st.clone();
        st.font.size = size;
        st
    });
    if !d.run_action("Font Size", vec![op]) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok(format!("Text at {} pt.", points(tw)))
}

/// Where Move or Copy puts the sheet: before or after a sheet, first, or
/// at the end; `copy` first copies it there instead.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub copy: bool,
    /// The sheet's index once it is there.
    pub to: u16,
}

/// The sheet index called `name`, ignoring case, as Excel's names match.
fn sheet_named(d: &Doc, name: &str) -> Option<u16> {
    (0..d.wb.sheet_count()).find(|&i| d.wb.sheet_name(i as u16).eq_ignore_ascii_case(name)).map(|i| i as u16)
}

/// Reads `[copy] before NAME | after NAME | first | end`; a named sheet
/// is its place now.
fn place(d: &Doc, typed: &str) -> Result<(bool, Where), String> {
    if typed.len() > MAX_TYPED {
        return Err("That's too long.".into());
    }
    let t = typed.trim();
    let (copy, rest) = match t.split_once(char::is_whitespace) {
        Some((w, r)) if w.eq_ignore_ascii_case("copy") => (true, r.trim()),
        _ if t.eq_ignore_ascii_case("copy") => (true, "end"),
        _ => (false, t),
    };
    let lower = rest.to_ascii_lowercase();
    let w = if lower == "first" || lower == "start" {
        Where::First
    } else if lower == "end" || lower == "last" || lower == "move to end" {
        Where::End
    } else if let Some(name) = lower.strip_prefix("before ").map(|_| rest.get(7..).unwrap_or_default().trim()) {
        Where::Before(sheet_named(d, unquote(name)).ok_or_else(|| format!("There's no sheet called {name}."))?)
    } else if let Some(name) = lower.strip_prefix("after ").map(|_| rest.get(6..).unwrap_or_default().trim()) {
        Where::After(sheet_named(d, unquote(name)).ok_or_else(|| format!("There's no sheet called {name}."))?)
    } else {
        return Err("Type before NAME, after NAME, first or end, with copy first to copy the sheet.".into());
    };
    Ok((copy, w))
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(s)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Where {
    First,
    End,
    Before(u16),
    After(u16),
}

/// The index a sheet at `from` ends at, among `n`, for `w`: the sheet is
/// taken out and put back in, so a place past it is one less.
fn target(from: u16, n: u16, w: Where) -> u16 {
    let last = n.saturating_sub(1);
    let to = match w {
        Where::First => 0,
        Where::End => last,
        Where::Before(i) if from < i => i - 1,
        Where::Before(i) => i,
        Where::After(i) if from < i => i,
        Where::After(i) if from == i => i,
        Where::After(i) => i + 1,
    };
    to.min(last)
}

/// The bar's starting text: after the sheet's own place.
pub fn move_start(d: &Doc) -> String {
    if d.sheet + 1 >= d.wb.sheet_count() as u16 {
        "end".into()
    } else {
        format!("before {}", d.wb.sheet_name(d.sheet + 1))
    }
}

/// Move or Copy Sheet…: the current sheet (or a copy of it) before or
/// after a sheet, first or at the end. Refused while the workbook's
/// structure is protected, as each step on its own is.
pub fn move_or_copy(d: &mut Doc, typed: &str) -> Result<String, String> {
    let (copy, w) = place(d, typed)?;
    if d.wb.structure_protected() {
        return Err("The workbook's structure is protected.".into());
    }
    let s = d.sheet;
    let mut from = s;
    // A sheet past the original moves up one when the copy goes in just
    // after it.
    let w = if copy {
        from = crate::data::duplicate_sheet(d)?;
        match w {
            Where::Before(i) if i > s => Where::Before(i + 1),
            Where::After(i) if i > s => Where::After(i + 1),
            w => w,
        }
    } else {
        w
    };
    let n = d.wb.sheet_count() as u16;
    if let (Where::Before(i) | Where::After(i), false) = (w, copy) {
        if i == from {
            return Err("Pick another sheet to put it next to.".into());
        }
    }
    let to = target(from, n, w);
    if to != from {
        if copy {
            d.extend(vec![GridOp::MoveSheet { from, to }]);
        } else if !d.run_action("Move Sheet", vec![GridOp::MoveSheet { from, to }]) {
            return Err(std::mem::take(&mut d.status));
        }
    }
    d.sheet = to;
    let name = d.wb.sheet_name(to);
    Ok(format!("{} {name} is sheet {} of {n}.", if copy { "Copied:" } else { "Moved:" }, to + 1))
}

/// The bar's starting text: the sheet's standard size, as Excel gives it.
pub fn standard_start(d: &Doc, rows: bool) -> String {
    let sh = d.wb.sheet(d.sheet);
    crate::app::excel_size(if rows { sh.row_sizes.default } else { sh.col_sizes.default }, rows)
}

/// Standard Width… (`rows` false) or Standard Height…: every column (row)
/// nobody sized takes the typed size; sized ones keep theirs.
pub fn standard(d: &mut Doc, rows: bool, typed: &str) -> Result<String, String> {
    let unit = if rows { "points (12 pt) or pixels (20 px)" } else { "characters (8.43 ch) or pixels (64 px)" };
    let px = crate::app::typed_px(typed, rows).filter(|p| *p > 0.0 && *p <= MAX_STANDARD_PX).ok_or_else(|| format!("Type a size in {unit}."))?;
    let s = d.sheet;
    let sh = d.wb.sheet(s);
    let over = if rows { sh.row_sizes.overrides().to_vec() } else { sh.col_sizes.overrides().to_vec() };
    let label = if rows { "Standard Height" } else { "Standard Width" };
    if !d.run_action(label, vec![GridOp::LayoutSizes { sheet: s, rows, default: px, over }]) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok(format!("{label}: {}.", crate::app::excel_size(px, rows)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(sheets: &[&str]) -> Doc {
        let mut d = Doc::from_csv("a.csv", b"1,2\n3,4\n").unwrap().0;
        d.wb.recalc();
        d.run("Rename", vec![GridOp::RenameSheet { sheet: 0, name: sheets[0].into() }]);
        for n in &sheets[1..] {
            d.run("Add", vec![GridOp::add_sheet((*n).into())]);
        }
        d
    }

    fn names(d: &Doc) -> Vec<String> {
        (0..d.wb.sheet_count()).map(|i| d.wb.sheet_name(i as u16).to_string()).collect()
    }

    #[test]
    fn font_size_takes_any_half_point_and_keeps_eleven_as_none() {
        assert_eq!(typed_points("10.5"), Some(210));
        assert_eq!(typed_points(" 13 pt"), Some(260));
        assert_eq!(typed_points("409"), Some(8180));
        assert_eq!((typed_points("10.25"), typed_points("0.5"), typed_points("410"), typed_points("NaN"), typed_points("x")), (None, None, None, None, None));
        let mut d = doc(&["A"]);
        let sel = CellRange { r0: 0, c0: 0, r1: 1, c1: 1 };
        assert_eq!(font_size(&mut d, sel, "13").unwrap(), "Text at 13 pt.");
        assert_eq!(d.wb.style(CellAddr::new(0, 1, 1)).font.size, Some(260));
        assert_eq!(font_size_start(&d, sel), "13");
        font_size(&mut d, sel, "10.5").unwrap();
        assert_eq!(font_size_start(&d, sel), "10.5");
        font_size(&mut d, sel, "11").unwrap();
        assert_eq!(d.wb.style(CellAddr::new(0, 0, 0)).font.size, None);
        assert!(font_size(&mut d, sel, "500").is_err());
        d.undo();
        assert_eq!(d.wb.style(CellAddr::new(0, 0, 0)).font.size, Some(210));
    }

    #[test]
    fn move_or_copy_puts_the_sheet_where_it_is_told() {
        let mut d = doc(&["A", "B", "C", "D"]);
        d.sheet = 0;
        assert_eq!(move_start(&d), "before B");
        move_or_copy(&mut d, "before D").unwrap();
        assert_eq!(names(&d), ["B", "C", "A", "D"]);
        assert_eq!(d.sheet, 2);
        move_or_copy(&mut d, "after d").unwrap();
        assert_eq!(names(&d), ["B", "C", "D", "A"]);
        move_or_copy(&mut d, "first").unwrap();
        assert_eq!(names(&d), ["A", "B", "C", "D"]);
        d.sheet = 2;
        move_or_copy(&mut d, "after A").unwrap();
        assert_eq!(names(&d), ["A", "C", "B", "D"]);
        assert!(move_or_copy(&mut d, "before Nowhere").unwrap_err().contains("no sheet called"));
        assert!(move_or_copy(&mut d, "sideways").is_err());
        assert!(move_or_copy(&mut d, "before C").is_err(), "next to itself");
        // A copy: made, then placed, with its cells.
        d.sheet = 0;
        move_or_copy(&mut d, "copy end").unwrap();
        assert_eq!(names(&d), ["A", "C", "B", "D", "A (2)"]);
        assert_eq!(d.sheet, 4);
        assert_eq!(d.wb.value(4, 1, 1), officina_grid::Value::num(4.0));
        d.sheet = 3;
        move_or_copy(&mut d, "copy before C").unwrap();
        assert_eq!(names(&d), ["A", "D (2)", "C", "B", "D", "A (2)"]);
        assert_eq!(d.sheet, 1);
        // Structure protection refuses both.
        d.run("Lock", vec![GridOp::Structure(Some(officina_grid::protection::SheetProtection::new(officina_grid::protection::ProtectionFormat::ExcelWorkbook)))]);
        assert!(d.wb.structure_protected());
        assert!(move_or_copy(&mut d, "end").unwrap_err().contains("protected"));
        assert!(move_or_copy(&mut d, "copy first").is_err());
        assert_eq!(d.wb.sheet_count(), 6);
    }

    #[test]
    fn standard_width_and_height_change_unsized_lines_only() {
        let mut d = doc(&["A"]);
        d.run("Size", vec![GridOp::Sizes { sheet: 0, rows: false, over: vec![(1, 120.0)] }]);
        assert_eq!(standard_start(&d, false), crate::app::excel_size(d.wb.sheet(0).col_sizes.default, false));
        assert_eq!(standard(&mut d, false, "10 ch").unwrap(), "Standard Width: 10 ch.");
        let sh = d.wb.sheet(0);
        assert_eq!((sh.col_sizes.default, sh.col_sizes.size(0), sh.col_sizes.size(1)), (75.0, 75.0, 120.0));
        standard(&mut d, true, "30 pt").unwrap();
        assert_eq!(d.wb.sheet(0).row_sizes.default, 40.0);
        assert!(standard(&mut d, true, "0").is_err() && standard(&mut d, false, "99999").is_err() && standard(&mut d, true, "3 ch").is_err());
        d.undo();
        assert_ne!(d.wb.sheet(0).row_sizes.default, 40.0);
        assert_eq!(d.wb.sheet(0).col_sizes.default, 75.0);
    }
}
