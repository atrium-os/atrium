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
