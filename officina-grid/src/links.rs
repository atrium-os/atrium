//! Hyperlinks on cells (Excel's `<hyperlink>`, ODF's `text:a`): a web or mail
//! address, or a place in the workbook written `#Sheet1!A1`. Like comments
//! they stay when the cell is cleared or edited and move with inserted,
//! deleted and sorted rows. [`crate::ops::GridOp::Links`] changes them, one
//! undo step. A `HYPERLINK()` formula is a link too ([`Workbook::link_at`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::addr::{CellAddr, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::{Expr, Func};
use crate::eval::funcs;
use officina_core::wire::{Reader, WireError, Writer};

/// The longest link kept, in bytes (Excel's limit is 2,079 characters).
pub const MAX_TARGET: usize = 8 << 10;
/// The most links a sheet keeps (Excel's limit is 65,530).
pub const MAX_LINKS: usize = 1 << 20;

/// Where a link goes: an address (`https:`, `mailto:`, ...) or, starting `#`,
/// a place in the workbook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub target: Arc<str>,
}

impl Link {
    /// `target` trimmed and cut to [`MAX_TARGET`]; None when empty.
    pub fn new(target: &str) -> Option<Link> {
        let t = target.trim();
        if t.is_empty() {
            return None;
        }
        let mut n = t.len().min(MAX_TARGET);
        while !t.is_char_boundary(n) {
            n -= 1;
        }
        Some(Link { target: t[..n].into() })
    }

    /// The place in the workbook it names (`#Sheet2!B4`, `#B4` on its own
    /// sheet), read against `wb` from sheet `own`.
    pub fn place(&self, wb: &Workbook, own: u16) -> Option<CellAddr> {
        let p = self.target.strip_prefix('#')?;
        let (sheet, cell) = match p.rsplit_once('!') {
            Some((s, c)) => {
                let s = s.trim_matches('\'').replace("''", "'");
                (wb.sheet_id(&s)?, c)
            }
            None => (own, p),
        };
        let cell = cell.replace('$', "");
        let cell = cell.split(':').next()?;
        let (row, col) = crate::addr::parse_a1(cell)?;
        Some(CellAddr::new(sheet, row, col))
    }

    /// Whether it is an address that leaves the workbook (not a place in it).
    pub fn is_external(&self) -> bool {
        !self.target.starts_with('#')
    }
}

/// A sheet's links by (row, column).
pub type Links = BTreeMap<(u32, u32), Link>;

/// Writes changes: count, then each cell and its link or none.
pub fn encode(w: &mut Writer, set: &[(u32, u32, Option<Link>)]) {
    w.var(set.len() as u64);
    for (r, c, k) in set {
        w.var(*r as u64);
        w.var(*c as u64);
        match k {
            Some(k) => {
                w.u8(1);
                w.str(&k.target);
            }
            None => w.u8(0),
        }
    }
}

/// Reads what [`encode`] wrote.
pub fn decode(r: &mut Reader) -> Result<Vec<(u32, u32, Option<Link>)>, WireError> {
    let n = r.count("links", MAX_LINKS as u64 * 2, 3)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let (row, col) = (r.var()?, r.var()?);
        if row >= MAX_ROWS as u64 || col >= MAX_COLS as u64 {
            return Err(WireError::Invalid("link cell"));
        }
        let k = match r.u8()? {
            0 => None,
            1 => Some(Link::new(r.str("link", MAX_TARGET as u64)?).ok_or(WireError::Invalid("empty link"))?),
            _ => return Err(WireError::Invalid("link")),
        };
        v.push((row as u32, col as u32, k));
    }
    Ok(v)
}

impl Workbook {
    /// The sheet's links, by (row, column).
    pub fn links(&self, sheet: u16) -> &Links {
        static NONE: Links = BTreeMap::new();
        self.sheets.get(sheet as usize).map_or(&NONE, |s| &s.links)
    }

    /// The link set on a cell (not counting `HYPERLINK()`; see [`Self::link_at`]).
    pub fn link(&self, sheet: u16, row: u32, col: u32) -> Option<&Link> {
        self.links(sheet).get(&(row, col))
    }

    /// Where the cell links to: its link, else the address its `HYPERLINK()`
    /// formula gives.
    pub fn link_at(&self, a: CellAddr) -> Option<Link> {
        if let Some(l) = self.link(a.sheet, a.row, a.col) {
            return Some(l.clone());
        }
        let g = self.group_at(a.sheet, a.row, a.col)?;
        let e = &self.group(g).expr;
        let Expr::Call(Func::Known(id), args) = &**e else { return None };
        if funcs::FUNCS[*id as usize].name != "HYPERLINK" {
            return None;
        }
        let v = crate::eval::Ctx::new(self, a.sheet, a.row, a.col).eval_cell(args.first()?);
        match v {
            crate::Value::Str(s) => Link::new(&s),
            _ => None,
        }
    }

    /// Sets (or, `None`, takes away) links; what was there before, in the
    /// reverse order, so applying it undoes this.
    pub fn set_links(&mut self, sheet: u16, set: Vec<(u32, u32, Option<Link>)>) -> Vec<(u32, u32, Option<Link>)> {
        let Some(sh) = self.sheets.get_mut(sheet as usize) else { return Vec::new() };
        let mut old = Vec::with_capacity(set.len());
        for (r, c, k) in set {
            let before = match k {
                Some(k) if sh.links.len() < MAX_LINKS || sh.links.contains_key(&(r, c)) => sh.links.insert((r, c), k),
                Some(_) => continue,
                None => sh.links.remove(&(r, c)),
            };
            old.push((r, c, before));
        }
        old.reverse();
        old
    }
}

/// The changes moving `on`'s entries by `to` (old cell to new, or None when
/// it goes): every moved or gone cell emptied, then the moved ones set.
pub(crate) fn moved<T: Clone>(on: &BTreeMap<(u32, u32), T>, to: impl Fn(u32, u32) -> Option<(u32, u32)>) -> Vec<(u32, u32, Option<T>)> {
    let mut set = Vec::new();
    let mut add = Vec::new();
    for (&(r, c), k) in on {
        match to(r, c) {
            Some(n) if n == (r, c) => {}
            Some(n) => {
                set.push((r, c, None));
                add.push((n.0, n.1, Some(k.clone())));
            }
            None => set.push((r, c, None)),
        }
    }
    set.extend(add);
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native;
    use crate::ops::GridOp;
    use crate::shift::{shift_op, Axis};
    use officina_core::history::Op;

    #[test]
    fn links_round_trip_follow_rows_and_name_places() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.add_sheet("Totals 2026").unwrap();
        let op = GridOp::Links { sheet: 0, set: vec![(2, 1, Link::new("https://example.org/a?b=1")), (0, 0, Link::new("#'Totals 2026'!$C$5"))] };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let inv = op.apply(&mut wb);
        assert_eq!(wb.link(0, 0, 0).unwrap().place(&wb, 0), Some(CellAddr::new(1, 4, 2)));
        assert!(wb.link(0, 2, 1).unwrap().is_external());
        assert_eq!(Link::new("#B4").unwrap().place(&wb, 1), Some(CellAddr::new(1, 3, 1)));
        let back = native::load(&native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.links(0), wb.links(0));
        let ins = shift_op(&wb, 0, Axis::Rows, 1, 2).unwrap().apply(&mut wb);
        assert!(wb.link(0, 4, 1).is_some() && wb.link(0, 0, 0).is_some() && wb.link(0, 2, 1).is_none());
        ins.apply(&mut wb);
        inv.apply(&mut wb);
        assert!(wb.links(0).is_empty());
        // A HYPERLINK() formula is a link too.
        wb.set_input(CellAddr::new(0, 7, 0), "=HYPERLINK(\"https://example.org/\"&\"x\",\"Site\")").unwrap();
        wb.recalc();
        assert_eq!(wb.value(0, 7, 0), crate::Value::str("Site"));
        assert_eq!(wb.link_at(CellAddr::new(0, 7, 0)).map(|l| l.target.to_string()), Some("https://example.org/x".into()));
    }
}
