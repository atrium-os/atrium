//! Defined names (named ranges, constants and formulas): `Sales`, `TaxRate`, a name
//! scoped to one sheet.
//!
//! A name's formula is stored parsed as if written in cell A1, so its relative parts
//! are offsets from the cell that uses it, as in Excel's files. A formula using a name
//! depends on what the name refers to (its footprints include the name's), and when a
//! name changes every formula using names is re-bound and recalculated.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::addr;
use crate::formula::ast::Expr;

/// Names per workbook, at most.
pub const MAX_NAMES: usize = 65_536;
/// Name lengths, as Excel.
pub const MAX_NAME_LEN: usize = 255;
/// Names in names, followed at most this deep (a cycle of names is `#NAME?`).
pub const MAX_NAME_DEPTH: u32 = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct NameDef {
    pub name: Arc<str>,
    /// `None`: the workbook; `Some(sheet)`: that sheet only (it hides a workbook name
    /// of the same spelling there).
    pub scope: Option<u16>,
    pub expr: Arc<Expr>,
}

#[derive(Debug, Clone, Default)]
pub struct Names {
    defs: Vec<NameDef>,
    idx: HashMap<(Option<u16>, String), usize>,
}

fn key(name: &str) -> String {
    name.to_uppercase()
}

impl Names {
    /// The name `name` as seen from `sheet`.
    pub fn get(&self, sheet: u16, name: &str) -> Option<&NameDef> {
        let k = key(name);
        self.idx.get(&(Some(sheet), k.clone())).or_else(|| self.idx.get(&(None, k))).map(|&i| &self.defs[i])
    }

    /// Defines or redefines. `false` when the table is full.
    pub fn set(&mut self, d: NameDef) -> bool {
        let k = (d.scope, key(&d.name));
        if let Some(&i) = self.idx.get(&k) {
            self.defs[i] = d;
            return true;
        }
        if self.defs.len() >= MAX_NAMES {
            return false;
        }
        self.idx.insert(k, self.defs.len());
        self.defs.push(d);
        true
    }

    pub fn remove(&mut self, scope: Option<u16>, name: &str) -> Option<NameDef> {
        let i = self.idx.remove(&(scope, key(name)))?;
        let d = self.defs.remove(i);
        for v in self.idx.values_mut() {
            if *v > i {
                *v -= 1;
            }
        }
        Some(d)
    }

    pub fn iter(&self) -> impl Iterator<Item = &NameDef> {
        self.defs.iter()
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

/// Whether `s` can be a name: a letter, `_` or `\` first, then letters, digits, `_` and
/// `.`; not a cell reference (`A1`, `R1C1`-looking `R2C3`), not `TRUE`/`FALSE`.
pub fn valid_name(s: &str) -> bool {
    let mut it = s.chars();
    let Some(first) = it.next() else { return false };
    if s.len() > MAX_NAME_LEN || !(first.is_alphabetic() || first == '_' || first == '\\') {
        return false;
    }
    if !s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '\\' || c == '?') {
        return false;
    }
    let u = s.to_ascii_uppercase();
    if u == "TRUE" || u == "FALSE" || u == "R" || u == "C" {
        return false;
    }
    if addr::parse_a1(s).is_some() {
        return false;
    }
    // R1C1-style (R2C3, R4, C5) would read as a reference in R1C1 mode.
    let rc = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    if let Some(rest) = u.strip_prefix('R') {
        let (r, c) = rest.split_once('C').unwrap_or((rest, ""));
        if rc(r) && rc(c) {
            return false;
        }
    }
    if let Some(rest) = u.strip_prefix('C') {
        if !rest.is_empty() && rc(rest) {
            return false;
        }
    }
    true
}

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Runs `f` one name deeper; `None` past [`MAX_NAME_DEPTH`].
pub(crate) fn deeper<T>(f: impl FnOnce() -> T) -> Option<T> {
    let d = DEPTH.with(|d| d.get());
    if d >= MAX_NAME_DEPTH {
        return None;
    }
    DEPTH.with(|x| x.set(d + 1));
    let out = f();
    DEPTH.with(|x| x.set(d));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_are_valid() {
        // ("Rate2" is a cell here: columns go to 2^20.)
        for n in ["Sales", "tax_rate", "_x", "\\back", "Q1.total", "ABCD", "Rate_2"] {
            assert!(valid_name(n), "{n}");
        }
        for n in ["", "A1", "xfd1048576", "1abc", "TRUE", "R2C3", "C5", "R", "a b", "a-b", "Rate2"] {
            assert!(!valid_name(n), "{n}");
        }
    }
}

impl crate::book::Workbook {
    /// `defs` as bytes for [`crate::ops::GridOp::Names`]: scope (0xFFFF for
    /// the workbook), name, and formula text as if in A1 of its sheet.
    pub fn encode_names(&self, defs: &[NameDef]) -> Vec<u8> {
        let mut w = officina_core::wire::Writer::new();
        w.var(defs.len() as u64);
        for d in defs {
            w.u16(d.scope.unwrap_or(0xFFFF));
            w.str(&d.name);
            w.str(&self.name_text(d));
        }
        w.into_bytes()
    }

    /// Reads names written by [`encode_names`](Self::encode_names).
    pub fn decode_names(&self, b: &[u8]) -> Result<Vec<NameDef>, officina_core::wire::WireError> {
        use officina_core::wire::{Reader, WireError};
        let mut r = Reader::new(b);
        let n = r.count("names", MAX_NAMES as u64, 3)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let scope = match r.u16()? {
                0xFFFF => None,
                s if (s as usize) < self.sheet_count() => Some(s),
                _ => return Err(WireError::Invalid("name scope")),
            };
            let name = r.str("name", MAX_NAME_LEN as u64)?;
            let src = r.str("name formula", 1 << 16)?;
            if !valid_name(name) {
                return Err(WireError::Invalid("name"));
            }
            let e = self.parse_stored(src, 0, 0).map_err(|_| WireError::Invalid("name formula"))?;
            v.push(NameDef { name: Arc::from(name), scope, expr: Arc::new(e) });
        }
        if !r.is_empty() {
            return Err(WireError::Invalid("trailing bytes"));
        }
        Ok(v)
    }

    /// Puts `defs` in place of the defined names; the old ones back.
    pub fn replace_names(&mut self, defs: Vec<NameDef>) -> Vec<NameDef> {
        let old: Vec<NameDef> = self.names.iter().cloned().collect();
        self.names = Names::default();
        for d in defs {
            self.names.set(d);
        }
        self.rebind_names();
        old
    }
}

/// Renaming defined name `old` of `scope` to `new`: the edits that put the
/// renamed definition in place and rewrite the formulas and other names that
/// use it (where that spelling means this name: not on a sheet whose own
/// name of that spelling hides a workbook one), as one step. A formula that
/// binds the spelling itself with LET or LAMBDA is left as it is; how many
/// were left is returned with the edits.
pub fn rename_ops(wb: &crate::book::Workbook, scope: Option<u16>, old: &str, new: &str) -> Result<(Vec<crate::ops::GridOp>, usize), String> {
    use crate::ops::{Fill, GridOp, Region};
    let new = new.trim();
    if !valid_name(new) {
        return Err(format!("{new:?} can't be a name: start with a letter or _, use letters, digits, _ and ., and don't spell a cell like A1."));
    }
    if wb.names().iter().any(|n| n.scope == scope && key(&n.name) == key(new)) && key(old) != key(new) {
        return Err(format!("There is already a name {new} there."));
    }
    if !wb.names().iter().any(|n| n.scope == scope && *n.name == *old) {
        return Err(format!("There's no name {old}."));
    }
    let means = |sheet: u16| match scope {
        Some(s) => s == sheet,
        None => wb.names().get(sheet, old).is_none_or(|d| d.scope.is_none()),
    };
    let binders = [crate::eval::funcs::lookup("LET"), crate::eval::funcs::lookup("LAMBDA")];
    let binds = |e: &Expr| {
        let mut hit = false;
        e.walk(&mut |x| {
            if let Expr::Call(crate::formula::ast::Func::Known(id), args) = x {
                if binders.contains(&Some(*id)) && args.iter().any(|a| matches!(a, Expr::Name(n) if key(n) == key(old))) {
                    hit = true;
                }
            }
        });
        hit
    };
    let uses = |e: &Expr| {
        let mut hit = false;
        e.walk(&mut |x| hit |= matches!(x, Expr::Name(n) if key(n) == key(old)));
        hit
    };
    let to: Arc<str> = new.into();
    let mut ops = Vec::new();
    // Other names first: their definitions, and the renamed one itself.
    let defs: Vec<NameDef> = wb.names().iter().map(|n| {
        let mut n = n.clone();
        if n.scope == scope && *n.name == *old {
            n.name = to.clone();
        }
        let here = match n.scope { Some(x) => means(x), None => scope.is_none() };
        if here {
            if uses(&n.expr) && !binds(&n.expr) {
                n.expr = Arc::new(swap_name(&n.expr, old, &to));
            }
        }
        n
    }).collect();
    ops.push(GridOp::names(wb, &defs));
    let mut kept = 0;
    for (_, g) in wb.groups() {
        if !means(g.sheet) || !uses(&g.expr) {
            continue;
        }
        if binds(&g.expr) {
            kept += 1;
            continue;
        }
        let e = swap_name(&g.expr, old, &to);
        let src = crate::formula::to_a1(&e, g.r0, g.col, &|id| wb.sheet_name(id));
        ops.push(GridOp::Replace(Region { rect: crate::addr::Rect { sheet: g.sheet, r0: g.r0, c0: g.col, r1: g.r1, c1: g.col }, values: Vec::new(), fills: vec![Fill { col: g.col, r0: g.r0, r1: g.r1, src }] }));
    }
    Ok((ops, kept))
}

/// `e` with every use of name `old` spelled `new`.
fn swap_name(e: &Expr, old: &str, new: &Arc<str>) -> Expr {
    let b = |x: &Expr| Box::new(swap_name(x, old, new));
    let all = |xs: &[Expr]| xs.iter().map(|a| swap_name(a, old, new)).collect::<Vec<_>>();
    match e {
        Expr::Name(n) if key(n) == key(old) => Expr::Name(new.clone()),
        Expr::Neg(x) => Expr::Neg(b(x)),
        Expr::Plus(x) => Expr::Plus(b(x)),
        Expr::Pct(x) => Expr::Pct(b(x)),
        Expr::Paren(x) => Expr::Paren(b(x)),
        Expr::At(x) => Expr::At(b(x)),
        Expr::Bin(op, x, y) => Expr::Bin(*op, b(x), b(y)),
        Expr::Call(f, args) => Expr::Call(f.clone(), all(args)),
        Expr::Invoke(g, args) => Expr::Invoke(b(g), all(args)),
        Expr::Array(rows) => Expr::Array(rows.iter().map(|r| all(r)).collect()),
        other => other.clone(),
    }
}
