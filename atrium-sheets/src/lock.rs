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

/// A fresh salt: a v4 UUID's 122 random bits.
fn salt() -> [u8; 16] {
    uuid::Uuid::new_v4().into_bytes()
}

fn too_long(password: &str) -> Result<(), String> {
    let max = officina_convert::protect::MAX_PASSWORD;
    if password.chars().count() > max { Err(format!("A password is at most {max} characters.")) } else { Ok(()) }
}

/// Whether unprotecting the workbook's structure needs a password.
pub fn workbook_needs_password(d: &Doc) -> bool {
    d.wb.structure_protection().is_some_and(officina_convert::protect::has_password)
}

/// Protect Workbook with `password` (may be empty), or, when the structure
/// is protected already, unprotect it with that password. One step.
pub fn protect_workbook(d: &mut Doc, password: &str) -> Result<String, String> {
    too_long(password)?;
    if let Some(p) = d.wb.structure_protection().cloned() {
        if !officina_convert::protect::check(&p, password)? {
            return Err("That password doesn't unprotect the workbook.".into());
        }
        d.run("Unprotect Workbook", vec![GridOp::Structure(None)]);
        return Ok("Workbook structure unlocked: sheets can be added, deleted, renamed and moved.".into());
    }
    let mut p = SheetProtection::new(ProtectionFormat::ExcelWorkbook);
    if !password.is_empty() {
        for (k, v) in officina_convert::protect::workbook_verifier(password, &salt()) {
            p.keep(k, &v);
        }
    }
    d.run("Protect Workbook", vec![GridOp::Structure(Some(p))]);
    Ok(if password.is_empty() { "Workbook structure protected: sheets can't be added, deleted, renamed, moved or hidden.".into() } else { "Workbook structure protected with a password: sheets can't be added, deleted, renamed, moved or hidden.".into() })
}

/// Range Password: `TITLE: password` gives an Allow Edit Range a password
/// (`TITLE:` alone takes it away). Refused while the sheet is protected,
/// as every change to the ranges is. One step.
pub fn range_password(d: &mut Doc, typed: &str) -> Result<String, String> {
    let s = d.sheet;
    if d.wb.sheet_protected(s) {
        return Err("Unprotect the sheet to change its edit ranges' passwords.".into());
    }
    let (title, password) = typed.split_once(':').ok_or("Type the range's title, a colon and its password, such as: Inputs: secret")?;
    let (title, password) = (title.trim(), password.trim());
    too_long(password)?;
    let mut list = d.wb.sheet(s).edit_ranges().to_vec();
    let e = list.iter_mut().find(|e| e.title.eq_ignore_ascii_case(title)).ok_or_else(|| format!("No edit range is called {title:?}."))?;
    e.password = (!password.is_empty()).then(|| officina_convert::protect::range_verifier(password, &salt()));
    let msg = if password.is_empty() { format!("{} opens without a password.", e.title) } else { format!("{} opens only with its password.", e.title) };
    d.status.clear();
    d.run("Range Password", vec![GridOp::EditRanges { sheet: s, list }]);
    if crate::sheet_protection::refused(&d.status) {
        return Err(d.status.clone());
    }
    Ok(msg)
}

/// Unlock Range: opens, until the workbook is closed, every edit range on
/// the sheet that `password` is the password of. Not a step: nothing in
/// the file changes.
pub fn unlock_range(d: &mut Doc, password: &str) -> Result<String, String> {
    too_long(password)?;
    let guarded: Vec<(String, String)> = d.wb.sheet(d.sheet).edit_ranges().iter().filter_map(|e| Some((e.title.clone(), e.password.clone()?))).collect();
    if guarded.is_empty() {
        return Err("No edit range on this sheet has a password.".into());
    }
    let mut opened = Vec::new();
    for (title, v) in guarded {
        if officina_convert::protect::check_range(&v, password)? {
            d.wb.unlock_ranges(&v);
            opened.push(title);
        }
    }
    if opened.is_empty() {
        return Err("That password doesn't open any edit range on this sheet.".into());
    }
    Ok(format!("Unlocked until the workbook closes: {}.", opened.join(", ")))
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
    fn workbook_password_guards_the_structure() {
        let mut d = doc();
        assert!(protect_workbook(&mut d, "tiles").unwrap().contains("with a password"));
        assert!(d.wb.structure_protected() && workbook_needs_password(&d));
        let p = d.wb.structure_protection().unwrap().clone();
        assert!(p.settings().any(|(k, v)| k == "workbookAlgorithmName" && v == "SHA-512"));
        // A sheet can't be added; the plain toggle won't take it off.
        d.run("Add Sheet", vec![GridOp::add_sheet("Two".into())]);
        assert_eq!(d.wb.sheet_count(), 1);
        assert!(crate::data::protect_workbook(&mut d).is_err());
        assert_eq!(protect_workbook(&mut d, "Tiles").unwrap_err(), "That password doesn't unprotect the workbook.");
        assert!(d.wb.structure_protected());
        protect_workbook(&mut d, "tiles").unwrap();
        assert!(!d.wb.structure_protected());
        // Without a password, none is asked for.
        protect_workbook(&mut d, "").unwrap();
        assert!(!workbook_needs_password(&d));
        protect_workbook(&mut d, "").unwrap();
        assert!(!d.wb.structure_protected());
    }

    #[test]
    fn a_range_password_opens_its_range_only_once_typed() {
        let mut d = doc();
        crate::checks::edit_ranges(&mut d, "add Inputs: B1:B2").unwrap();
        crate::checks::edit_ranges(&mut d, "add Notes: A2").unwrap();
        assert!(range_password(&mut d, "Nope: x").unwrap_err().contains("No edit range"));
        assert!(range_password(&mut d, "inputs: key one").unwrap().contains("only with its password"));
        let v = d.wb.sheet(0).edit_ranges()[0].password.clone().unwrap();
        assert!(v.contains("algorithmName=SHA-512"), "{v}");
        protect(&mut d, "").unwrap();
        assert!(range_password(&mut d, "Inputs:").is_err(), "fixed while protected");
        // Notes has no password: it opens; Inputs waits for its own.
        assert!(d.type_into(1, 0, "x"));
        d.type_into(0, 1, "7");
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)), Value::Num(1.0));
        assert!(unlock_range(&mut d, "key two").unwrap_err().contains("doesn't open"));
        assert_eq!(unlock_range(&mut d, "key one").unwrap(), "Unlocked until the workbook closes: Inputs.");
        d.type_into(0, 1, "7");
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)), Value::Num(7.0));
        // Taken away again: no password left to unlock with.
        unprotect(&mut d, "").unwrap();
        range_password(&mut d, "Inputs:").unwrap();
        assert!(d.wb.sheet(0).edit_ranges()[0].password.is_none());
        assert!(unlock_range(&mut d, "key one").is_err());
    }

    #[test]
    fn tab_steps_through_unlocked_cells_on_a_protected_sheet() {
        let mut d = doc();
        assert_eq!(d.wb.next_unlocked(0, (0, 0), false), None, "not protected");
        let sel = CellRange { r0: 0, c0: 1, r1: 1, c1: 1 };
        crate::special::toggle_protection(&mut d, sel, true).unwrap();
        assert!(d.wb.style(CellAddr::new(0, 0, 1)).unlocked, "B1 unlocked");
        crate::checks::edit_ranges(&mut d, "add Far: D5").unwrap();
        protect(&mut d, "").unwrap();
        let next = |d: &Doc, at, back| d.wb.next_unlocked(0, at, back);
        assert_eq!(next(&d, (0, 0), false), Some((0, 1)));
        assert_eq!(next(&d, (0, 1), false), Some((1, 1)));
        assert_eq!(next(&d, (1, 1), false), Some((4, 3)));
        assert_eq!(next(&d, (4, 3), false), Some((0, 1)), "wraps round");
        assert_eq!(next(&d, (0, 1), true), Some((4, 3)), "and back");
        assert_eq!(next(&d, (4, 3), true), Some((1, 1)));
        assert_eq!(next(&d, (3, 0), true), Some((1, 1)));
        // The grid asks the sheet through its source.
        use pergola::widgets::GridSource;
        let d = std::rc::Rc::new(std::cell::RefCell::new(d));
        let src = crate::source::SheetSource::new(d.clone());
        assert_eq!(src.tab_target((0, 1), false), Some((1, 1)));
        unprotect(&mut d.borrow_mut(), "").unwrap();
        assert_eq!(src.tab_target((0, 1), false), None, "plain Tab once unprotected");
    }

    #[test]
    fn sort_left_to_right_reorders_columns_by_a_row() {
        let mut d = Doc::from_csv("s.csv", b"c,a,b\n3,1,2\nx,y,z\n").unwrap().0;
        let sel = CellRange { r0: 0, c0: 0, r1: 1, c1: 2 };
        assert!(crate::data::sort_left_to_right(&mut d, sel, (0, 0), "9").unwrap_err().contains("isn't a row"));
        assert_eq!(crate::data::sort_left_to_right(&mut d, sel, (0, 0), "1").unwrap(), "Sorted 3 columns by row 1.");
        let row = |d: &Doc, r| (0..3).map(|c| d.wb.formatted(CellAddr::new(0, r, c)).text.to_string()).collect::<Vec<_>>().join(",");
        assert_eq!(row(&d, 0), "a,b,c");
        assert_eq!(row(&d, 1), "1,2,3");
        assert_eq!(row(&d, 2), "x,y,z", "row 3 outside the selection is left alone");
        crate::data::sort_left_to_right(&mut d, sel, (0, 0), "2 desc").unwrap();
        assert_eq!(row(&d, 1), "3,2,1");
        assert!(d.undo());
        assert_eq!(row(&d, 0), "a,b,c");
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
