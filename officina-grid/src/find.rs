//! Literal search of editable worksheet inputs, including formula source text.
use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::{Input, Workbook};
use crate::value::Value;

pub struct Search(regex::Regex);

impl Search {
    pub fn new(needle: &str, case_sensitive: bool, whole_cell: bool) -> Result<Self, &'static str> {
        if needle.is_empty() { return Err("Enter text to find."); }
        if needle.len() > 16_384 { return Err("The search text is too long."); }
        let literal = regex::escape(needle);
        let pattern = if whole_cell { format!(r"\A{literal}\z") } else { literal };
        regex::RegexBuilder::new(&pattern).case_insensitive(!case_sensitive).build()
            .map(Self).map_err(|_| "The search text is too long.")
    }

    pub fn matches(&self, text: &str) -> bool { self.0.is_match(text) }

    pub fn replacement(&self, input: &Input, text: &str, replacement: &str) -> Result<Option<Input>, &'static str> {
        const LIMIT: usize = 64 * 1024;
        const TOO_LARGE: &str = "A replacement would make the cell input longer than 64 KiB.";
        let mut matches = self.0.find_iter(text).peekable();
        if matches.peek().is_none() { return Ok(None); }
        let mut changed = String::new();
        let mut end = 0;
        for found in matches {
            if changed.len() + (found.start() - end) + replacement.len() > LIMIT {
                return Err(TOO_LARGE);
            }
            changed.push_str(&text[end..found.start()]);
            changed.push_str(replacement);
            end = found.end();
        }
        if changed.len() + (text.len() - end) > LIMIT { return Err(TOO_LARGE); }
        changed.push_str(&text[end..]);
        if changed == text { return Ok(None); }
        Ok(Some(match input {
            Input::Value(Value::Str(_)) if !changed.is_empty() => Input::Value(Value::str(&changed)),
            _ => Input::typed(&changed),
        }))
    }
}

/// Visit each populated input once without traversing blank rows or columns.
pub fn inputs(wb: &Workbook, sheet: u16, visit: &mut dyn FnMut(CellAddr, Input, String) -> bool) {
    let mut more = true;
    let bounds = Rect { sheet, r0: 0, c0: 0, r1: MAX_ROWS - 1, c1: MAX_COLS - 1 };
    wb.each(&bounds, &mut |r, c, value| {
        if !more || value.is_empty() || wb.group_at(sheet, r, c).is_some() { return; }
        let addr = CellAddr::new(sheet, r, c);
        if wb.hides_value(addr) || wb.spilled_from(addr).is_some() || wb.pivot_at(addr).is_some() { return; }
        let text = value.display();
        more = visit(addr, Input::Value(value), text);
    });
    for (_, g) in wb.groups().filter(|(_, g)| g.sheet == sheet) {
        for row in g.r0..=g.r1 {
            if !more { return; }
            let addr = CellAddr::new(sheet, row, g.col);
            if wb.hides_formula(addr) || wb.hides_value(addr) { continue; }
            if let Input::Formula(text) = wb.input(addr) {
                more = visit(addr, Input::Formula(text.clone()), text);
            }
        }
    }
}
