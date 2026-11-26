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

    /// As Excel's Find: `*` stands for any run of characters, `?` for any
    /// one, and `~` takes the next of `*`, `?` or `~` as itself.
    pub fn excel(needle: &str, case_sensitive: bool, whole_cell: bool) -> Result<Self, &'static str> {
        if needle.is_empty() { return Err("Enter text to find."); }
        if needle.len() > 16_384 { return Err("The search text is too long."); }
        let mut pattern = String::with_capacity(needle.len() + 8);
        let mut chars = needle.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '~' if matches!(chars.peek(), Some('*' | '?' | '~')) => {
                    let next = chars.next().unwrap_or('~');
                    pattern.push_str(&regex::escape(&next.to_string()));
                }
                '*' => pattern.push_str("(?s:.*)"),
                '?' => pattern.push_str("(?s:.)"),
                c => pattern.push_str(&regex::escape(&c.to_string())),
            }
        }
        let pattern = if whole_cell { format!(r"\A(?:{pattern})\z") } else { pattern };
        regex::RegexBuilder::new(&pattern).case_insensitive(!case_sensitive).size_limit(1 << 22).build()
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

/// Visit each non-empty cell's value as it shows (through its number
/// format; a formula's result), as Excel's Find with Look in: Values.
/// Cells whose value is hidden by protection are passed over.
pub fn values(wb: &Workbook, sheet: u16, visit: &mut dyn FnMut(CellAddr, String) -> bool) {
    let mut more = true;
    let bounds = Rect { sheet, r0: 0, c0: 0, r1: MAX_ROWS - 1, c1: MAX_COLS - 1 };
    wb.each(&bounds, &mut |r, c, value| {
        if !more || value.is_empty() { return; }
        let addr = CellAddr::new(sheet, r, c);
        if wb.hides_value(addr) { return; }
        more = visit(addr, wb.formatted_as(addr, &value).text);
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excel_wildcards_match_as_excel_finds() {
        let s = |n: &str, whole: bool| Search::excel(n, false, whole).unwrap();
        assert!(s("a*c", false).matches("xxABBBCxx") && s("a?c", false).matches("abc") && !s("a?c", false).matches("ac"));
        assert!(s("a*c", true).matches("abc") && !s("a*c", true).matches("abcd"));
        // ~ takes a wildcard (or itself) as written; regex characters are plain.
        assert!(s("5~*", false).matches("5*2") && !s("5~*", false).matches("52"));
        assert!(s("~?", true).matches("?") && s("~~", true).matches("~") && s("a~b", true).matches("a~b"));
        assert!(s("(1+1).", true).matches("(1+1).") && !s("(1+1).", true).matches("(1+1)x"));
        assert!(Search::excel("A*", true, false).unwrap().matches("Ab") && !Search::excel("A*", true, false).unwrap().matches("ab"));
        // Replacing: the greedy run, as Excel's Replace All.
        let r = s("b*d", false).replacement(&Input::Value(Value::str("abcdabcd")), "abcdabcd", "-").unwrap();
        assert_eq!(r, Some(Input::Value(Value::str("a-"))));
        assert!(Search::excel("", false, false).is_err());
        assert!(Search::excel(&"*".repeat(16_385), false, false).is_err());
    }

    #[test]
    fn values_are_visited_as_they_show() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_input(CellAddr::new(0, 0, 0), "=2*3").unwrap();
        wb.set_input(CellAddr::new(0, 1, 0), "0.5").unwrap();
        wb.set_style(&Rect { sheet: 0, r0: 1, c0: 0, r1: 1, c1: 0 }, crate::style::Style { num_fmt: "0%".into(), ..Default::default() });
        wb.recalc();
        let mut seen = Vec::new();
        values(&wb, 0, &mut |at, text| { seen.push((at.row, text)); true });
        seen.sort();
        assert_eq!(seen, [(0, "6".to_string()), (1, "50%".to_string())]);
    }
}
