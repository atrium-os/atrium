//! The Scenario Manager (docs/spec/officina.md §16.142), driven from the
//! typed bar: `add Best: B1=5, B2=7` (or `add Best: B1:B2` to keep what the
//! cells hold now), `show Best`, `delete Best`, `summary C9, C10` for a
//! summary sheet of the result cells under each scenario. The list is kept
//! with the sheet and changes as one undo step.

use officina_grid::addr::{a1, parse_a1, CellAddr};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::scenario::{Scenario, MAX_CELLS, MAX_NAME, MAX_SCENARIOS, MAX_VALUE};
use officina_grid::Value;
use pergola::widgets::CellRange;

use crate::doc::Doc;

/// The most result cells a summary shows.
pub const MAX_RESULTS: usize = 64;

/// The bar's starting text: a summary of the results when scenarios exist,
/// else adding one over the selection.
pub fn start(d: &Doc, sel: CellRange) -> String {
    let list = d.wb.sheet(d.sheet).scenarios();
    match list.first() {
        Some(sc) => format!("show {}", sc.name),
        None if sel.r0 == sel.r1 && sel.c0 == sel.c1 => format!("add Scenario 1: {}", a1(sel.r0, sel.c0)),
        None => format!("add Scenario 1: {}:{}", a1(sel.r0, sel.c0), a1(sel.r1, sel.c1)),
    }
}

fn cell(t: &str) -> Result<(u32, u32), String> {
    parse_a1(&t.trim().replace('$', "").to_ascii_uppercase()).ok_or_else(|| format!("{:?} isn't a cell on this sheet, such as B2.", t.trim()))
}

/// What a cell holds, as it would be typed back.
fn typed_now(d: &Doc, r: u32, c: u32) -> String {
    match d.wb.get(CellAddr::new(d.sheet, r, c)) {
        Value::Empty => String::new(),
        Value::Num(x) => x.to_string(),
        Value::Bool(b) => if b { "TRUE".into() } else { "FALSE".into() },
        Value::Str(s) => s.to_string(),
        Value::Err(_) => d.wb.formatted(CellAddr::new(d.sheet, r, c)).text,
    }
}

/// The changing cells `spec` names: `B1=5`, `B1`, or `B1:C2` (current values).
fn changing(d: &Doc, spec: &str) -> Result<Vec<(u32, u32, String)>, String> {
    let mut cells: Vec<(u32, u32, String)> = Vec::new();
    let mut put = |r: u32, c: u32, v: String| -> Result<(), String> {
        if v.len() > MAX_VALUE {
            return Err(format!("A scenario value is at most {MAX_VALUE} bytes."));
        }
        if let Some(x) = cells.iter_mut().find(|x| (x.0, x.1) == (r, c)) {
            x.2 = v;
        } else if cells.len() == MAX_CELLS {
            return Err(format!("A scenario changes at most {MAX_CELLS} cells."));
        } else {
            cells.push((r, c, v));
        }
        Ok(())
    };
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((at, v)) = part.split_once('=') {
            let (r, c) = cell(at)?;
            put(r, c, v.trim().to_string())?;
        } else if let Some((a, b)) = part.split_once(':') {
            let ((r0, c0), (r1, c1)) = (cell(a)?, cell(b)?);
            let (r0, r1, c0, c1) = (r0.min(r1), r0.max(r1), c0.min(c1), c0.max(c1));
            if u64::from(r1 - r0 + 1) * u64::from(c1 - c0 + 1) > MAX_CELLS as u64 {
                return Err(format!("A scenario changes at most {MAX_CELLS} cells."));
            }
            for r in r0..=r1 {
                for c in c0..=c1 {
                    put(r, c, typed_now(d, r, c))?;
                }
            }
        } else {
            let (r, c) = cell(part)?;
            put(r, c, typed_now(d, r, c))?;
        }
    }
    for &(r, c, _) in &cells {
        if d.wb.formula_text(CellAddr::new(d.sheet, r, c)).is_some() {
            return Err(format!("{} holds a formula; a scenario changes cells that hold values.", a1(r, c)));
        }
    }
    if cells.is_empty() {
        return Err("Name the cells the scenario changes, such as: add Best: B1=5, B2=7".into());
    }
    Ok(cells)
}

fn find(list: &[Scenario], name: &str) -> Option<usize> {
    list.iter().position(|s| s.name.trim().eq_ignore_ascii_case(name.trim()))
}

fn set_list(d: &mut Doc, label: &str, list: Vec<Scenario>) -> Result<(), String> {
    let sheet = d.sheet;
    d.run(label, vec![GridOp::Scenarios { sheet, list }]);
    if d.status == crate::sheet_protection::REFUSED {
        return Err(d.status.clone());
    }
    Ok(())
}

/// Runs what was typed in the Scenario Manager bar.
pub fn run(d: &mut Doc, typed: &str) -> Result<String, String> {
    let t = typed.trim();
    let (verb, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    let rest = rest.trim();
    let list = d.wb.sheet(d.sheet).scenarios().to_vec();
    match verb.to_lowercase().as_str() {
        "add" | "edit" => {
            let (name, spec) = rest.split_once(':').ok_or("Type add, the name, a colon and the cells: add Best: B1=5, B2=7")?;
            let name = name.trim();
            if name.is_empty() || name.len() > MAX_NAME {
                return Err(format!("A scenario's name is 1 to {MAX_NAME} bytes."));
            }
            let cells = changing(d, spec)?;
            let n = cells.len();
            let mut list = list;
            let sc = Scenario { name: name.to_string(), comment: String::new(), cells };
            let replaced = match find(&list, name) {
                Some(i) => {
                    list[i] = Scenario { comment: std::mem::take(&mut list[i].comment), ..sc };
                    true
                }
                None if list.len() >= MAX_SCENARIOS => return Err(format!("A sheet keeps at most {MAX_SCENARIOS} scenarios.")),
                None => {
                    list.push(sc);
                    false
                }
            };
            set_list(d, if replaced { "Edit Scenario" } else { "Add Scenario" }, list)?;
            Ok(format!("Scenario “{name}” {} with {n} changing {}.", if replaced { "changed" } else { "added" }, if n == 1 { "cell" } else { "cells" }))
        }
        "delete" | "remove" => {
            let i = find(&list, rest).ok_or_else(|| format!("No scenario called {rest:?}."))?;
            let mut list = list;
            let gone = list.remove(i);
            set_list(d, "Delete Scenario", list)?;
            Ok(format!("Scenario “{}” deleted.", gone.name))
        }
        "show" => {
            let i = find(&list, rest).ok_or_else(|| format!("No scenario called {rest:?}."))?;
            let sc = &list[i];
            let s = d.sheet;
            let ops = sc.cells.iter().map(|(r, c, v)| GridOp::set(CellAddr::new(s, *r, *c), Input::typed(v))).collect();
            d.run("Show Scenario", ops);
            if d.status == crate::sheet_protection::REFUSED {
                return Err(d.status.clone());
            }
            d.wb.recalc();
            Ok(format!("Scenario “{}” shown: {} cells set.", sc.name, sc.cells.len()))
        }
        "summary" => summary(d, &list, rest),
        "" => Err("Type add, show, delete or summary.".into()),
        other => Err(format!("{other:?}: type add, show, delete or summary.")),
    }
}

/// A new sheet "Scenario Summary": the changing cells and the result cells
/// down the side, the current values and each scenario across, the results
/// calculated with that scenario's values in place (the sheet is put back).
fn summary(d: &mut Doc, list: &[Scenario], rest: &str) -> Result<String, String> {
    if list.is_empty() {
        return Err("Add a scenario first.".into());
    }
    let s = d.sheet;
    let mut results = Vec::new();
    for p in rest.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if results.len() == MAX_RESULTS {
            return Err(format!("A summary shows at most {MAX_RESULTS} result cells."));
        }
        results.push(cell(p)?);
    }
    if d.wb.sheet_count() >= officina_grid::book::MAX_SHEETS {
        return Err(format!("A workbook holds at most {} sheets.", officina_grid::book::MAX_SHEETS));
    }
    // Every changing cell, in the order first met.
    let mut changing: Vec<(u32, u32)> = Vec::new();
    for sc in list {
        for &(r, c, _) in &sc.cells {
            if !changing.contains(&(r, c)) {
                changing.push((r, c));
            }
        }
    }
    if d.wb.recalc_pending() {
        d.wb.recalc();
    }
    let at = |r: u32, c: u32| CellAddr::new(s, r, c);
    let value = |v: Value| if matches!(v, Value::Empty) { Input::Empty } else { Input::Value(v) };
    let current_changing: Vec<Value> = changing.iter().map(|&(r, c)| d.wb.get(at(r, c))).collect();
    let current_results: Vec<Value> = results.iter().map(|&(r, c)| d.wb.get(at(r, c))).collect();
    let mut columns: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    for sc in list {
        for (r, c, v) in &sc.cells {
            // The typed text as the value it stands for (formulas aren't kept in scenarios).
            let v = match Input::typed(v) {
                Input::Value(v) => v,
                Input::Formula(f) => Value::str(&f),
                Input::Empty => Value::Empty,
            };
            d.wb.set_value(at(*r, *c), v);
        }
        d.wb.recalc();
        let ch = changing.iter().map(|&(r, c)| d.wb.get(at(r, c))).collect();
        let rs = results.iter().map(|&(r, c)| d.wb.get(at(r, c))).collect();
        columns.push((ch, rs));
        for (&(r, c), v) in changing.iter().zip(&current_changing) {
            d.wb.set_value(at(r, c), v.clone());
        }
    }
    d.wb.recalc();
    let text = |t: &str| Input::Value(Value::str(t));
    let mut rows: Vec<Vec<Input>> = Vec::new();
    let mut head = vec![text("Scenario Summary"), text("Current Values")];
    head.extend(list.iter().map(|sc| text(&sc.name)));
    rows.push(head);
    rows.push(vec![text("Changing Cells:")]);
    for (i, &(r, c)) in changing.iter().enumerate() {
        let mut row = vec![text(&a1(r, c)), value(current_changing[i].clone())];
        row.extend(columns.iter().map(|(ch, _)| value(ch[i].clone())));
        rows.push(row);
    }
    if !results.is_empty() {
        rows.push(vec![text("Result Cells:")]);
        for (i, &(r, c)) in results.iter().enumerate() {
            let mut row = vec![text(&a1(r, c)), value(current_results[i].clone())];
            row.extend(columns.iter().map(|(_, rs)| value(rs[i].clone())));
            rows.push(row);
        }
    }
    let name = if d.wb.sheet_id("Scenario Summary").is_none() { "Scenario Summary".to_string() } else { crate::data::copy_name(&d.wb, "Scenario Summary") };
    let n = d.wb.sheet_count() as u16;
    if !d.run_action("Scenario Summary", vec![GridOp::add_sheet(name.clone())]) || d.wb.sheet_count() as u16 == n {
        return Err(if d.status.is_empty() { "The summary sheet couldn't be added.".into() } else { d.status.clone() });
    }
    d.extend(vec![GridOp::block(n, 0, 0, rows)]);
    Ok(format!("“{name}” added: {} scenarios, {} changing and {} result cells.", list.len(), changing.len(), results.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(d: &Doc, s: u16, r: u32, c: u32) -> f64 {
        match d.wb.get(CellAddr::new(s, r, c)) {
            Value::Num(x) => x,
            v => panic!("{} is {v:?}", a1(r, c)),
        }
    }

    #[test]
    fn scenarios_add_show_delete_and_summarise_as_undo_steps() {
        // B1 price, B2 units, B3 = B1*B2.
        let mut d = Doc::from_csv("s.csv", b"Price,10\nUnits,3\nTotal,\n").unwrap().0;
        d.type_into(2, 1, "=B1*B2");
        d.wb.recalc();
        assert_eq!(start(&d, CellRange { r0: 0, c0: 1, r1: 1, c1: 1 }), "add Scenario 1: B1:B2");
        let m = run(&mut d, "add Best: B1=12, B2=5").unwrap();
        assert!(m.contains("added with 2 changing cells"), "{m}");
        run(&mut d, "add Now: B1:B2").unwrap();
        assert_eq!(d.wb.sheet(0).scenarios()[1].cells, vec![(0, 1, "10".to_string()), (1, 1, "3".to_string())]);
        assert!(run(&mut d, "add Bad: B3=1").unwrap_err().contains("holds a formula"));
        assert!(run(&mut d, "add : B1=1").unwrap_err().contains("name is 1 to"));
        assert!(run(&mut d, "show Worst").unwrap_err().contains("No scenario"));
        assert!(run(&mut d, "fly").unwrap_err().contains("type add"));
        // Show: the cells take the values, and undo puts them back.
        run(&mut d, "show best").unwrap();
        assert_eq!(num(&d, 0, 2, 1), 60.0);
        assert!(d.undo());
        d.wb.recalc();
        assert_eq!(num(&d, 0, 2, 1), 30.0);
        // Summary: a new sheet, the sheet itself left as it was.
        let m = run(&mut d, "summary B3").unwrap();
        assert!(m.starts_with("“Scenario Summary” added: 2 scenarios"), "{m}");
        assert_eq!(d.wb.sheet_count(), 2);
        assert_eq!(d.wb.formatted(CellAddr::new(1, 0, 2)).text, "Best");
        assert_eq!(d.wb.formatted(CellAddr::new(1, 2, 0)).text, "B1");
        assert_eq!(num(&d, 1, 2, 2), 12.0);
        assert_eq!(num(&d, 1, 5, 1), 30.0, "current result");
        assert_eq!(num(&d, 1, 5, 2), 60.0, "Best's result");
        assert_eq!(num(&d, 1, 5, 3), 30.0, "Now's result");
        assert_eq!(num(&d, 0, 2, 1), 30.0);
        assert!(d.undo());
        assert_eq!(d.wb.sheet_count(), 1);
        // Edit keeps the place; delete undoes.
        run(&mut d, "add BEST: B1=1").unwrap();
        assert_eq!(d.wb.sheet(0).scenarios()[0].cells.len(), 1);
        run(&mut d, "delete Now").unwrap();
        assert_eq!(d.wb.sheet(0).scenarios().len(), 1);
        assert!(d.undo());
        assert_eq!(d.wb.sheet(0).scenarios().len(), 2);
        // Rows inserted above move the changing cells with them.
        let op = officina_grid::shift::shift_op(&d.wb, 0, officina_grid::shift::Axis::Rows, 0, 2).unwrap();
        d.run("Insert Rows", vec![op]);
        assert_eq!(d.wb.sheet(0).scenarios()[1].cells[0].0, 2);
        // Kept in the native file.
        let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&d.wb)).unwrap();
        assert_eq!(back.sheet(0).scenarios(), d.wb.sheet(0).scenarios());
    }
}
