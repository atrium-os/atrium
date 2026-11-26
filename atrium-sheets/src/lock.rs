//! Protect Sheet, Unprotect Sheet and their options (docs/spec/officina.md
//! §16.149). The password is never kept: an Excel verifier (SHA-512, a
//! fresh salt, 100,000 spins) is, so the sheet saves to `.xlsx` as Excel
//! protects one, and a sheet protected by Excel or LibreOffice unprotects
//! here with its password.

use officina_grid::ops::GridOp;
use officina_grid::protection::{ProtectionFormat, SheetProtection};

use crate::doc::Doc;

/// What a protected sheet may still have done to it: the words typed, the
/// Excel setting, and whether Sheets enforces it (the rest are kept and
/// saved for Excel).
pub const ALLOWANCES: [(&str, &str, bool); 11] = [
    ("format cells", "formatCells", true),
    ("format columns", "formatColumns", true),
    ("format rows", "formatRows", true),
    ("insert hyperlinks", "insertHyperlinks", true),
    ("autofilter", "autoFilter", true),
    ("objects", "objects", true),
    ("scenarios", "scenarios", true),
    ("insert columns", "insertColumns", false),
    ("insert rows", "insertRows", false),
    ("delete columns", "deleteColumns", false),
    ("delete rows", "deleteRows", false),
];

/// Excel's own allowances it keeps without Sheets doing them.
const KEPT: [(&str, &str); 2] = [("sort", "sort"), ("pivot tables", "pivotTables")];

fn all() -> impl Iterator<Item = (&'static str, &'static str)> {
    ALLOWANCES.iter().map(|(w, k, _)| (*w, *k)).chain(KEPT)
}

/// The options bar's text: what the next Protect Sheet allows.
pub fn options_start(d: &Doc) -> String {
    let words: Vec<&str> = all().filter(|(_, k)| d.allowances.contains(k)).map(|(w, _)| w).collect();
    if words.is_empty() { "none".into() } else { words.join(", ") }
}

/// Protection Options: `format cells, autofilter` (or `none`), for the
/// next Protect Sheet. Refused while the sheet is protected, so they can't
/// open a protected sheet.
pub fn options(d: &mut Doc, typed: &str) -> Result<String, String> {
    if d.wb.sheet_protected(d.sheet) {
        return Err("Unprotect the sheet to change what protecting it allows.".into());
    }
    let mut keys = Vec::new();
    for part in typed.split(',').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty() && p != "none") {
        let (_, k) = all().find(|(w, k)| *w == part || k.eq_ignore_ascii_case(&part)).ok_or_else(|| {
            format!("{part:?}: type none or any of {}.", all().map(|(w, _)| w).collect::<Vec<_>>().join(", "))
        })?;
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    d.allowances = keys;
    Ok(if d.allowances.is_empty() { "Protecting the sheet will allow only editing unlocked cells.".into() } else { format!("Protecting the sheet will also allow: {}.", options_start(d)) })
}

/// Protect Sheet with `password` (may be empty: no password). One step.
pub fn protect(d: &mut Doc, password: &str) -> Result<String, String> {
    let s = d.sheet;
    if d.wb.sheet_protected(s) {
        return Err("The sheet is protected already.".into());
    }
    if password.chars().count() > officina_convert::protect::MAX_PASSWORD {
        return Err(format!("A password is at most {} characters.", officina_convert::protect::MAX_PASSWORD));
    }
    let mut p = SheetProtection::new(ProtectionFormat::Excel);
    if !password.is_empty() {
        // A v4 UUID's 122 random bits, as the salt.
        let salt = uuid::Uuid::new_v4().into_bytes();
        for (k, v) in officina_convert::protect::verifier(password, &salt) {
            p.keep(k, &v);
        }
    }
    for k in &d.allowances {
        p.keep(k, "0");
    }
    d.status.clear();
    if !d.run_validated("Protect Sheet", vec![GridOp::Protection { sheet: s, protection: Some(p) }]) {
        return Err(if d.status.is_empty() { "The sheet couldn't be protected.".into() } else { d.status.clone() });
    }
    Ok(if password.is_empty() { "Sheet protected: only unlocked cells can be edited.".into() } else { "Sheet protected with a password: only unlocked cells can be edited.".into() })
}

/// Whether unprotecting the sheet needs a password.
pub fn needs_password(d: &Doc) -> bool {
    d.wb.sheet(d.sheet).protection.as_ref().is_some_and(officina_convert::protect::has_password)
}

/// Unprotect Sheet, checking `password` against the sheet's verifier.
/// Its allowances become the next Protect Sheet's. One step.
pub fn unprotect(d: &mut Doc, password: &str) -> Result<String, String> {
    let s = d.sheet;
    let Some(p) = d.wb.sheet(s).protection.clone() else {
        return Err("The sheet isn't protected.".into());
    };
    if !officina_convert::protect::check(&p, password)? {
        return Err("That password doesn't unprotect this sheet.".into());
    }
    d.status.clear();
    if !d.run_validated("Unprotect Sheet", vec![GridOp::Protection { sheet: s, protection: None }]) {
        return Err(if d.status.is_empty() { "The sheet couldn't be unprotected.".into() } else { d.status.clone() });
    }
    d.allowances = all().map(|(_, k)| k).filter(|k| p.allows(k)).collect();
    Ok("Sheet unprotected.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::{CellAddr, Value};
    use pergola::widgets::CellRange;

    fn doc() -> Doc {
        Doc::from_csv("p.csv", b"a,1\nb,2\n").unwrap().0
    }

    #[test]
    fn protect_with_a_password_and_unprotect_with_it_only() {
        let mut d = doc();
        assert!(protect(&mut d, "open sesame").unwrap().contains("with a password"));
        assert!(d.wb.sheet_protected(0));
        d.type_into(0, 1, "9");
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)), Value::Num(1.0), "locked cells refuse edits");
        assert!(protect(&mut d, "x").is_err());
        assert!(needs_password(&d));
        assert_eq!(unprotect(&mut d, "Open sesame").unwrap_err(), "That password doesn't unprotect this sheet.");
        assert!(d.wb.sheet_protected(0));
        assert_eq!(unprotect(&mut d, "open sesame").unwrap(), "Sheet unprotected.");
        d.type_into(0, 1, "9");
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)), Value::Num(9.0));
        // Undo puts the protection back; no password, none needed.
        assert!(d.undo());
        assert!(d.undo());
        assert!(d.wb.sheet_protected(0));
        assert!(d.undo());
        assert!(!d.wb.sheet_protected(0));
        protect(&mut d, "").unwrap();
        assert!(!needs_password(&d));
        unprotect(&mut d, "").unwrap();
    }

    #[test]
    fn allowances_open_only_what_they_name() {
        let mut d = doc();
        assert!(options(&mut d, "format cells, autofilter, sort").unwrap().contains("format cells, autofilter, sort"));
        assert!(options(&mut d, "fly").unwrap_err().contains("type none or any of"));
        protect(&mut d, "").unwrap();
        assert!(options(&mut d, "none").is_err(), "fixed while protected");
        let sel = CellRange { r0: 0, c0: 0, r1: 1, c1: 1 };
        // Formatting is allowed, wrapping included; hiding a row isn't.
        crate::data::run(&mut d, sel, (0, 0), crate::data::DataCmd::WrapText);
        assert!(d.wb.style(CellAddr::new(0, 0, 0)).wrap, "{}", d.status);
        crate::data::run(&mut d, CellRange { r0: 1, c0: 0, r1: 1, c1: 0 }, (1, 0), crate::data::DataCmd::HideRows);
        assert!(!d.wb.sheet(0).row_hidden(1));
        assert!(crate::sheet_protection::refused(&d.status), "{}", d.status);
        // Saved as Excel writes them, and handed back on unprotecting.
        let p = d.wb.sheet(0).protection.clone().unwrap();
        assert!(p.allows("formatCells") && p.allows("autoFilter") && p.allows("sort") && !p.allows("formatRows"));
        unprotect(&mut d, "").unwrap();
        assert_eq!(options_start(&d), "format cells, autofilter, sort");
        // Without the allowance, formatting is refused.
        options(&mut d, "none").unwrap();
        protect(&mut d, "").unwrap();
        crate::data::run(&mut d, sel, (0, 0), crate::data::DataCmd::WrapText);
        assert!(d.wb.style(CellAddr::new(0, 0, 0)).wrap, "unchanged by the refusal");
    }

    #[test]
    fn zero_values_hide_and_shrink_to_fit_toggles() {
        use pergola::widgets::GridSource;
        let d = std::rc::Rc::new(std::cell::RefCell::new(Doc::from_csv("z.csv", b"0,5\n").unwrap().0));
        let src = crate::source::SheetSource::new(d.clone());
        assert_eq!(src.cell(0, 0).unwrap().text, "0");
        let sel = CellRange { r0: 0, c0: 0, r1: 0, c1: 1 };
        crate::data::run(&mut d.borrow_mut(), sel, (0, 0), crate::data::DataCmd::ZeroValues);
        assert!(!d.borrow().wb.sheet(0).show_zeros && d.borrow().dirty);
        assert_eq!(src.cell(0, 0).unwrap().text, "");
        assert_eq!(src.cell(0, 1).unwrap().text, "5");
        crate::data::run(&mut d.borrow_mut(), sel, (0, 0), crate::data::DataCmd::ShrinkToFit);
        assert!(d.borrow().wb.style(CellAddr::new(0, 0, 1)).shrink_to_fit);
        assert!(src.cell(0, 1).unwrap().shrink_to_fit);
        assert!(d.borrow_mut().undo());
        assert!(!d.borrow().wb.style(CellAddr::new(0, 0, 1)).shrink_to_fit);
    }
}
