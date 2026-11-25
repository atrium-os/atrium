//! Function argument tips while a formula is typed (Excel's ScreenTip):
//! the arguments of the function the caret is in, the current one marked.

/// Each function's arguments as Excel names them: `[x]` optional, `...`
/// more of the ones before it.
const ARGS: &[(&str, &str)] = &[
    ("SUM", "number1, [number2], ..."),
    ("PRODUCT", "number1, [number2], ..."),
    ("AVERAGE", "number1, [number2], ..."),
    ("MIN", "number1, [number2], ..."),
    ("MAX", "number1, [number2], ..."),
    ("COUNT", "value1, [value2], ..."),
    ("COUNTA", "value1, [value2], ..."),
    ("COUNTBLANK", "range"),
    ("ABS", "number"),
    ("ROUND", "number, num_digits"),
    ("ROUNDUP", "number, num_digits"),
    ("ROUNDDOWN", "number, num_digits"),
    ("INT", "number"),
    ("TRUNC", "number, [num_digits]"),
    ("MOD", "number, divisor"),
    ("POWER", "number, power"),
    ("SQRT", "number"),
    ("EXP", "number"),
    ("LN", "number"),
    ("LOG", "number, [base]"),
    ("LOG10", "number"),
    ("PI", ""),
    ("SIGN", "number"),
    ("SUMPRODUCT", "array1, [array2], ..."),
    ("SUMIF", "range, criteria, [sum_range]"),
    ("SUMIFS", "sum_range, criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("COUNTIF", "range, criteria"),
    ("COUNTIFS", "criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("AVERAGEIF", "range, criteria, [average_range]"),
    ("AVERAGEIFS", "average_range, criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("RAND", ""),
    ("RANDBETWEEN", "bottom, top"),
    ("MEDIAN", "number1, [number2], ..."),
    ("LARGE", "array, k"),
    ("SMALL", "array, k"),
    ("SUBTOTAL", "function_num, ref1, [ref2], ..."),
    ("STDEV", "number1, [number2], ..."),
    ("STDEV.S", "number1, [number2], ..."),
    ("STDEV.P", "number1, [number2], ..."),
    ("STDEVP", "number1, [number2], ..."),
    ("VAR", "number1, [number2], ..."),
    ("VAR.S", "number1, [number2], ..."),
    ("VAR.P", "number1, [number2], ..."),
    ("VARP", "number1, [number2], ..."),
    ("IF", "logical_test, [value_if_true], [value_if_false]"),
    ("IFS", "logical_test1, value_if_true1, [logical_test2], [value_if_true2], ..."),
    ("IFERROR", "value, value_if_error"),
    ("HYPERLINK", "link_location, [friendly_name]"),
    ("IFNA", "value, value_if_na"),
    ("AND", "logical1, [logical2], ..."),
    ("OR", "logical1, [logical2], ..."),
    ("XOR", "logical1, [logical2], ..."),
    ("NOT", "logical"),
    ("TRUE", ""),
    ("FALSE", ""),
    ("FORMULATEXT", "reference"),
    ("ISBLANK", "value"),
    ("ISNUMBER", "value"),
    ("ISTEXT", "value"),
    ("ISLOGICAL", "value"),
    ("ISERROR", "value"),
    ("ISERR", "value"),
    ("ISNA", "value"),
    ("NA", ""),
    ("ROW", "[reference]"),
    ("COLUMN", "[reference]"),
    ("ROWS", "array"),
    ("COLUMNS", "array"),
    ("LEN", "text"),
    ("LEFT", "text, [num_chars]"),
    ("RIGHT", "text, [num_chars]"),
    ("MID", "text, start_num, num_chars"),
    ("UPPER", "text"),
    ("LOWER", "text"),
    ("TRIM", "text"),
    ("CONCATENATE", "text1, [text2], ..."),
    ("CONCAT", "text1, [text2], ..."),
    ("TEXTJOIN", "delimiter, ignore_empty, text1, [text2], ..."),
    ("REPT", "text, number_times"),
    ("FIND", "find_text, within_text, [start_num]"),
    ("SEARCH", "find_text, within_text, [start_num]"),
    ("SUBSTITUTE", "text, old_text, new_text, [instance_num]"),
    ("VALUE", "text"),
    ("TEXT", "value, format_text"),
    ("EXACT", "text1, text2"),
    ("TEXTAFTER", "text, delimiter, [instance_num], [match_mode], [match_end], [if_not_found]"),
    ("CHOOSE", "index_num, value1, [value2], ..."),
    ("INDEX", "array, row_num, [column_num]"),
    ("MATCH", "lookup_value, lookup_array, [match_type]"),
    ("VLOOKUP", "lookup_value, table_array, col_index_num, [range_lookup]"),
    ("HLOOKUP", "lookup_value, table_array, row_index_num, [range_lookup]"),
    ("XLOOKUP", "lookup_value, lookup_array, return_array, [if_not_found], [match_mode], [search_mode]"),
    ("DATE", "year, month, day"),
    ("DATEVALUE", "date_text"),
    ("EOMONTH", "start_date, months"),
    ("YEAR", "serial_number"),
    ("MONTH", "serial_number"),
    ("DAY", "serial_number"),
    ("WEEKDAY", "serial_number, [return_type]"),
    ("TODAY", ""),
    ("NOW", ""),
    ("SEQUENCE", "rows, [columns], [start], [step]"),
    ("RANDARRAY", "[rows], [columns], [min], [max], [whole_number]"),
    ("TRANSPOSE", "array"),
    ("FILTER", "array, include, [if_empty]"),
    ("SORT", "array, [sort_index], [sort_order], [by_col]"),
    ("SORTBY", "array, by_array1, [sort_order1], ..."),
    ("UNIQUE", "array, [by_col], [exactly_once]"),
    ("VSTACK", "array1, [array2], ..."),
    ("HSTACK", "array1, [array2], ..."),
    ("TAKE", "array, rows, [columns]"),
    ("DROP", "array, rows, [columns]"),
    ("CHOOSEROWS", "array, row_num1, [row_num2], ..."),
    ("CHOOSECOLS", "array, col_num1, [col_num2], ..."),
    ("TOCOL", "array, [ignore], [scan_by_column]"),
    ("TOROW", "array, [ignore], [scan_by_column]"),
    ("WRAPROWS", "vector, wrap_count, [pad_with]"),
    ("WRAPCOLS", "vector, wrap_count, [pad_with]"),
    ("EXPAND", "array, rows, [columns], [pad_with]"),
    ("LET", "name1, name_value1, calculation_or_name2, ..."),
    ("LAMBDA", "[parameter1], ..., calculation"),
    ("MAP", "array1, lambda_or_array2, ..."),
    ("REDUCE", "[initial_value], array, lambda"),
    ("SCAN", "[initial_value], array, lambda"),
    ("BYROW", "array, lambda"),
    ("BYCOL", "array, lambda"),
    ("MAKEARRAY", "rows, cols, lambda"),
    ("ISOMITTED", "argument"),
];

/// The most characters of a formula the tip reads.
pub const MAX_TIP_SCAN: usize = 8192;

/// The argument names of `func` (any case), or None for a function not listed.
pub fn args(func: &str) -> Option<Vec<&'static str>> {
    let f = func.strip_prefix("_xlfn.").unwrap_or(func);
    let (_, a) = ARGS.iter().find(|(n, _)| n.eq_ignore_ascii_case(f))?;
    Some(if a.is_empty() { Vec::new() } else { a.split(", ").collect() })
}

/// The function the caret at byte `caret` of `text` is inside, and which of
/// its arguments (from 0) it is in. None outside a function or a formula.
pub fn call_at(text: &str, caret: usize) -> Option<(String, usize)> {
    if !text.starts_with('=') || caret > MAX_TIP_SCAN || !text.is_char_boundary(caret) {
        return None;
    }
    let head = &text[..caret];
    // Open brackets: a function's name and its commas so far, or None for
    // plain parentheses and array constants.
    let mut open: Vec<Option<(usize, usize, usize)>> = Vec::new();
    let mut quote: Option<char> = None;
    for (i, ch) in head.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '(' => {
                let start = head[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_')).map_or(0, |k| k + 1);
                open.push((start < i && head[start..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')).then_some((start, i, 0)));
            }
            '{' => open.push(None),
            ')' | '}' => {
                open.pop();
            }
            ',' => {
                if let Some(Some((_, _, n))) = open.last_mut() {
                    *n += 1;
                }
            }
            _ => {}
        }
    }
    if quote == Some('"') {
        return None;
    }
    // In plain parentheses inside a call, the call's argument is the one.
    let &(s, e, n) = open.iter().rev().find_map(|f| f.as_ref())?;
    Some((head[s..e].to_ascii_uppercase(), n))
}

/// The tip for the caret: `NAME(` then each argument, the one the caret is in
/// marked true. None when there's nothing to show.
pub fn tip(text: &str, caret: usize) -> Option<Vec<(String, bool)>> {
    let (name, at) = call_at(text, caret)?;
    let list = args(&name)?;
    let shown = name.strip_prefix("_XLFN.").unwrap_or(&name).to_string();
    // Past the listed ones (a repeating list): the "..." is current.
    let more = list.iter().position(|a| *a == "...");
    let cur = match more {
        Some(k) if at >= k => k,
        _ => at,
    };
    let mut out = vec![(format!("{shown}("), false)];
    for (i, a) in list.iter().enumerate() {
        let sep = if i + 1 < list.len() { ", " } else { "" };
        out.push((a.to_string(), i == cur));
        if !sep.is_empty() {
            out.push((sep.into(), false));
        }
    }
    out.push((")".into(), false));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_function_has_its_arguments_and_they_match_its_counts() {
        for f in officina_grid::eval::funcs::FUNCS {
            let a = args(f.name).unwrap_or_else(|| panic!("{} has no argument names", f.name));
            let repeats = a.contains(&"...");
            let required = a.iter().filter(|x| !x.starts_with('[') && **x != "...").count();
            assert_eq!(required, f.min as usize, "{}: required arguments", f.name);
            if repeats {
                assert_eq!(f.max, 255, "{}: only a function taking any number repeats", f.name);
            } else {
                assert_eq!(a.len(), f.max as usize, "{}: all arguments", f.name);
            }
        }
        assert_eq!(ARGS.len(), officina_grid::eval::funcs::FUNCS.len(), "no names for functions Sheets lacks");
    }

    #[test]
    fn the_caret_s_function_and_argument_are_found() {
        let at = |t: &str| call_at(t, t.len());
        assert_eq!(at("=VLOOKUP(A1, B:C, "), Some(("VLOOKUP".into(), 2)));
        assert_eq!(at("=vlookup("), Some(("VLOOKUP".into(), 0)));
        // Inner calls, plain parentheses, arrays and strings don't count their commas.
        assert_eq!(at("=IF(SUM(1,2)>3, (1+2"), Some(("IF".into(), 1)));
        assert_eq!(at("=SUM(ROUND(A1, "), Some(("ROUND".into(), 1)));
        assert_eq!(at("=CHOOSE({1,2,3}, \"a,b\", "), Some(("CHOOSE".into(), 2)));
        assert_eq!(at("=SUM('a, b'!A1, "), Some(("SUM".into(), 1)));
        assert_eq!(at("=SUM(1)+"), None);
        assert_eq!(at("=LEN(\"a,("), None, "inside a string");
        assert_eq!(at("SUM("), None, "not a formula");
        assert_eq!(at("=(1+2"), None);
        assert_eq!(call_at("=ROUND(A1, 2)", 7), Some(("ROUND".into(), 0)));
    }

    #[test]
    fn the_tip_marks_the_current_argument() {
        let marked = |t: &str| tip(t, t.len()).map(|v| v.into_iter().filter(|(_, on)| *on).map(|(s, _)| s).collect::<Vec<_>>());
        assert_eq!(marked("=VLOOKUP(A1, B:C, "), Some(vec!["col_index_num".to_string()]));
        assert_eq!(marked("=SUM(1, 2, 3, "), Some(vec!["...".to_string()]));
        assert_eq!(marked("=NOPE("), None);
        let all: String = tip("=MOD(", 5).unwrap().into_iter().map(|(s, _)| s).collect();
        assert_eq!(all, "MOD(number, divisor)");
    }
}
