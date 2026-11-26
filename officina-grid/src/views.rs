//! Custom Views (docs/spec/officina.md §16.158), as Excel's: a named view
//! of one sheet — its hidden rows and filter, its column widths and hidden
//! columns, its zoom and the selected cell — kept with the workbook and
//! shown again by name. The list changes only through
//! [`crate::ops::GridOp::CustomViews`], so it undoes.

use officina_core::wire::{Reader, WireError, Writer};

use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::book::{ZOOM_MAX, ZOOM_MIN};
use crate::filter::FilterState;
use crate::ops::{decode_widths, GridOp};
use crate::Workbook;

/// Views in a workbook, at most.
pub const MAX_VIEWS: usize = 64;
/// A view's name, in bytes.
pub const MAX_NAME: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct CustomView {
    pub name: String,
    pub sheet: u16,
    /// The selected cell.
    pub cursor: (u32, u32),
    pub zoom: u16,
    /// Hidden rows and the AutoFilter (its `sheet` is the view's).
    pub filter: FilterState,
    /// Column width overrides (0 hides), rising.
    pub widths: Vec<(u32, f64)>,
    /// The widths hidden columns had, rising.
    pub hidden_widths: Vec<(u32, f64)>,
}

/// Whether `name` can name a view: 1 to [`MAX_NAME`] bytes, trimmed, no
/// control characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.trim() == name && !name.chars().any(char::is_control)
}

fn widths_ok(list: &[(u32, f64)]) -> bool {
    list.windows(2).all(|w| w[0].0 < w[1].0) && list.iter().all(|&(c, v)| c < MAX_COLS && v.is_finite() && (0.0..=8192.0).contains(&v))
}

impl CustomView {
    /// The view of `sheet` as it is now, with `cursor` selected.
    pub fn capture(wb: &Workbook, name: &str, sheet: u16, cursor: (u32, u32)) -> Option<CustomView> {
        if !valid_name(name) || sheet as usize >= wb.sheet_count() {
            return None;
        }
        let sh = wb.sheet(sheet);
        let clamp = |v: &[(u32, f64)]| v.iter().map(|&(c, w)| (c, w.clamp(0.0, 8192.0))).collect::<Vec<_>>();
        Some(CustomView {
            name: name.to_string(),
            sheet,
            cursor: (cursor.0.min(MAX_ROWS - 1), cursor.1.min(MAX_COLS - 1)),
            zoom: sh.zoom.clamp(ZOOM_MIN, ZOOM_MAX),
            filter: wb.filter_state(sheet),
            widths: clamp(sh.col_sizes.overrides()),
            hidden_widths: clamp(&sh.hidden_widths),
        })
    }

    /// The undoable part of showing the view: rows, filter and columns.
    /// The caller sets the zoom, sheet and cursor (view settings, not undo
    /// steps). `None` when the view's sheet is gone.
    pub fn show_ops(&self, wb: &Workbook) -> Option<Vec<GridOp>> {
        if self.sheet as usize >= wb.sheet_count() {
            return None;
        }
        Some(vec![
            GridOp::Filter(self.filter.clone()),
            GridOp::Sizes { sheet: self.sheet, rows: false, over: self.widths.clone() },
            GridOp::HiddenWidths { sheet: self.sheet, list: self.hidden_widths.clone() },
        ])
    }

    fn valid(&self) -> bool {
        valid_name(&self.name)
            && self.filter.sheet == self.sheet
            && self.cursor.0 < MAX_ROWS
            && self.cursor.1 < MAX_COLS
            && (ZOOM_MIN..=ZOOM_MAX).contains(&self.zoom)
            && widths_ok(&self.widths)
            && widths_ok(&self.hidden_widths)
    }
}

/// Whether `list` can be a workbook's views: valid, at most [`MAX_VIEWS`],
/// names unique ignoring case.
pub fn valid(list: &[CustomView]) -> bool {
    let mut names: Vec<String> = list.iter().map(|v| v.name.to_lowercase()).collect();
    names.sort();
    list.len() <= MAX_VIEWS && list.iter().all(CustomView::valid) && names.windows(2).all(|w| w[0] != w[1])
}

pub fn encode(list: &[CustomView], w: &mut Writer) {
    w.var(list.len() as u64);
    for v in list {
        w.str(&v.name);
        w.u16(v.sheet);
        w.u32(v.cursor.0);
        w.u32(v.cursor.1);
        w.u16(v.zoom);
        v.filter.encode(w);
        for list in [&v.widths, &v.hidden_widths] {
            w.var(list.len() as u64);
            for &(c, x) in list.iter() {
                w.var(c as u64);
                w.f64(x);
            }
        }
    }
}

/// Reads [`encode`]; bounded, never panics.
pub fn decode(r: &mut Reader) -> Result<Vec<CustomView>, WireError> {
    let n = r.count("custom views", MAX_VIEWS as u64, 16)?;
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.str("view name", MAX_NAME as u64)?.to_string();
        let sheet = r.u16()?;
        let cursor = (r.u32()?, r.u32()?);
        let zoom = r.u16()?;
        let filter = FilterState::decode(r)?;
        let widths = decode_widths(r)?;
        let hidden_widths = decode_widths(r)?;
        list.push(CustomView { name, sheet, cursor, zoom, filter, widths, hidden_widths });
    }
    if !valid(&list) {
        return Err(WireError::Invalid("custom views"));
    }
    Ok(list)
}

impl Workbook {
    pub fn custom_views(&self) -> &[CustomView] {
        &self.views
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::CellAddr;
    use crate::value::Value;
    use officina_core::history::Op;

    fn book() -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_value(CellAddr::new(0, 0, 0), Value::Num(1.0));
        wb
    }

    #[test]
    fn views_capture_show_undo_and_round_trip() {
        let mut wb = book();
        let mut st = wb.filter_state(0);
        st.hidden = vec![(2, 4)];
        GridOp::Filter(st).apply(&mut wb);
        GridOp::Sizes { sheet: 0, rows: false, over: vec![(1, 0.0), (3, 120.0)] }.apply(&mut wb);
        GridOp::HiddenWidths { sheet: 0, list: vec![(1, 70.0)] }.apply(&mut wb);
        wb.sheet_mut(0).zoom = 150;
        let view = CustomView::capture(&wb, "Summary", 0, (5, 2)).unwrap();
        assert_eq!((view.zoom, view.cursor, view.filter.hidden.clone()), (150, (5, 2), vec![(2, 4)]));
        assert!(CustomView::capture(&wb, " bad", 0, (0, 0)).is_none());
        assert!(CustomView::capture(&wb, "x", 3, (0, 0)).is_none());

        // Stored through the op, which undoes.
        let undo = GridOp::CustomViews(vec![view.clone()]).apply(&mut wb);
        assert_eq!(wb.custom_views(), &[view.clone()]);
        // A change, then showing the view puts it back.
        GridOp::Filter(crate::filter::FilterState { sheet: 0, filter: None, hidden: Vec::new() }).apply(&mut wb);
        GridOp::Sizes { sheet: 0, rows: false, over: Vec::new() }.apply(&mut wb);
        for op in view.show_ops(&wb).unwrap() {
            op.apply(&mut wb);
        }
        assert_eq!(wb.filter_state(0).hidden, vec![(2, 4)]);
        assert_eq!(wb.sheet(0).col_sizes.overrides(), &[(1, 0.0), (3, 120.0)]);

        // The op and the native file carry it.
        let op = GridOp::CustomViews(vec![view.clone()]);
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.custom_views(), &[view.clone()]);
        undo.apply(&mut wb);
        assert!(wb.custom_views().is_empty());

        // Duplicate names (ignoring case) and too many are refused.
        let mut twin = view.clone();
        twin.name = "SUMMARY".into();
        assert!(!valid(&[view.clone(), twin]));
        let mut w = Writer::new();
        encode(&[view.clone(), { let mut v = view.clone(); v.name = "summary".into(); v }], &mut w);
        assert!(decode(&mut Reader::new(&w.into_bytes())).is_err());
        let mut w = Writer::new();
        encode(&[{ let mut v = view; v.zoom = 5; v }], &mut w);
        assert!(decode(&mut Reader::new(&w.into_bytes())).is_err());
    }
}
