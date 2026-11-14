//! Formulas between OpenFormula (ODF 1.3 Part 4, as `table:formula="of:=..."`) and the
//! engine's A1 text.
//!
//! OpenFormula writes references in brackets (`[.A1:.B2]`, `[Sheet2.A1]`,
//! `['My sheet'.$A$1]`), separates arguments with `;` and array rows with `|`. The rest
//! (operators, strings, numbers, function names) is the same text, with a few
//! namespaced names (`COM.MICROSOFT.XLOOKUP`) for functions Excel added.

/// `of:=SUM([.A1:.A3])` → `=SUM(A1:A3)`. `None` if it is not OpenFormula (another
/// namespace, an unclosed bracket or string).
pub fn from_of(f: &str) -> Option<String> {
    let body = if let Some(b) = f.strip_prefix("of:") {
        b
    } else if let Some(b) = f.strip_prefix("oooc:") {
        b
    } else if f.starts_with('=') {
        f
    } else {
        return None;
    };
    let body = body.strip_prefix('=').unwrap_or(body);
    let ch: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len() + 1);
    out.push('=');
    let mut i = 0;
    let mut brace = 0;
    while i < ch.len() {
        let c = ch[i];
        match c {
            '"' => {
                out.push('"');
                i += 1;
                loop {
                    let d = *ch.get(i)?;
                    out.push(d);
                    i += 1;
                    if d == '"' {
                        if ch.get(i) == Some(&'"') {
                            out.push('"');
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            '[' => {
                let mut j = i + 1;
                let mut q = false;
                while j < ch.len() && (q || ch[j] != ']') {
                    if ch[j] == '\'' {
                        q = !q;
                    }
                    j += 1;
                }
                if j >= ch.len() {
                    return None;
                }
                let inner: String = ch[i + 1..j].iter().collect();
                out.push_str(&ref_from_of(&inner)?);
                i = j + 1;
            }
            '{' => {
                brace += 1;
                out.push(c);
                i += 1;
            }
            '}' => {
                brace -= 1;
                out.push(c);
                i += 1;
            }
            ';' => {
                out.push(',');
                i += 1;
            }
            '|' if brace > 0 => {
                out.push(';');
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut j = i;
                while j < ch.len() && (ch[j].is_ascii_alphanumeric() || ch[j] == '_' || ch[j] == '.') {
                    j += 1;
                }
                let word: String = ch[i..j].iter().collect();
                let w = word.strip_prefix("COM.MICROSOFT.").or_else(|| word.strip_prefix("_xlfn.")).unwrap_or(&word);
                out.push_str(w);
                i = j;
                // OpenFormula's TRUE() and FALSE() are Excel's constants.
                if (w.eq_ignore_ascii_case("TRUE") || w.eq_ignore_ascii_case("FALSE")) && ch.get(i) == Some(&'(') && ch.get(i + 1) == Some(&')') {
                    i += 2;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// One bracketed reference's inside: `.A1`, `.A1:.B2`, `Sheet2.A1:.B2`, `'x y'.A1`,
/// `$Sheet1.$A$1`, `.A:.A`, `.1:.3`.
fn ref_from_of(s: &str) -> Option<String> {
    let parts = split_outside_quotes(s, ':');
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let mut out = String::new();
    for (k, p) in parts.iter().enumerate() {
        let (sheet, cell) = split_sheet(p)?;
        if k == 0 {
            if let Some(sh) = sheet {
                out.push_str(&sheet_a1(&sh));
                out.push('!');
            }
        } else {
            out.push(':');
        }
        if cell.contains('#') {
            return Some("#REF!".into());
        }
        out.push_str(&cell);
    }
    Some(out)
}

fn split_outside_quotes(s: &str, sep: char) -> Vec<String> {
    let mut v = vec![String::new()];
    let mut q = false;
    for c in s.chars() {
        if c == '\'' {
            q = !q;
        }
        if c == sep && !q {
            v.push(String::new());
        } else {
            v.last_mut().unwrap().push(c);
        }
    }
    v
}

/// `Sheet1.$A$1` → (Some("Sheet1"), "$A$1"); `.A1` → (None, "A1").
fn split_sheet(p: &str) -> Option<(Option<String>, String)> {
    let p = p.trim();
    // The cell is after the last '.' outside quotes.
    let mut q = false;
    let mut dot = None;
    for (i, c) in p.char_indices() {
        if c == '\'' {
            q = !q;
        }
        if c == '.' && !q {
            dot = Some(i);
        }
    }
    let d = dot?;
    let sheet = p[..d].trim_start_matches('$');
    let cell = p[d + 1..].to_string();
    if sheet.is_empty() {
        return Some((None, cell));
    }
    let name = if let Some(q) = sheet.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) { q.replace("''", "'") } else { sheet.to_string() };
    Some((Some(name), cell))
}

/// A sheet name as A1 text needs it: quoted unless it is a plain word.
pub fn sheet_a1(name: &str) -> String {
    let plain = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.chars().next().unwrap().is_ascii_digit() && !looks_like_cell(name);
    if plain {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "''"))
    }
}

fn looks_like_cell(s: &str) -> bool {
    let s = s.trim_start_matches('$');
    let letters = s.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let rest = &s[letters..];
    let rest = rest.strip_prefix('$').unwrap_or(rest);
    (1..=3).contains(&letters) && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// `=SUM(A1:A3)` → `of:=SUM([.A1:.A3])`.
pub fn to_of(a1: &str) -> String {
    let body = a1.strip_prefix('=').unwrap_or(a1);
    let ch: Vec<char> = body.chars().collect();
    let mut out = String::from("of:=");
    let mut i = 0;
    let mut brace = 0;
    while i < ch.len() {
        let c = ch[i];
        match c {
            '#' => {
                // An error literal (#REF!, #N/A, #DIV/0!) as it is.
                let rest: String = ch[i..].iter().collect();
                let n = ["#NULL!", "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A"].iter().find(|e| rest.to_ascii_uppercase().starts_with(*e)).map_or(1, |e| e.len());
                out.extend(&ch[i..i + n]);
                i += n;
            }
            '"' => {
                out.push('"');
                i += 1;
                while i < ch.len() {
                    let d = ch[i];
                    out.push(d);
                    i += 1;
                    if d == '"' {
                        if ch.get(i) == Some(&'"') {
                            out.push('"');
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            '{' => {
                brace += 1;
                out.push(c);
                i += 1;
            }
            '}' => {
                brace -= 1;
                out.push(c);
                i += 1;
            }
            ',' if brace > 0 => {
                out.push(';');
                i += 1;
            }
            ';' if brace > 0 => {
                out.push('|');
                i += 1;
            }
            ',' => {
                out.push(';');
                i += 1;
            }
            '\'' => {
                // 'Sheet name'!ref
                let mut j = i + 1;
                let mut name = String::new();
                while j < ch.len() {
                    if ch[j] == '\'' {
                        if ch.get(j + 1) == Some(&'\'') {
                            name.push('\'');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    name.push(ch[j]);
                    j += 1;
                }
                if ch.get(j + 1) == Some(&'!') {
                    let (r, n) = read_ref(&ch, j + 2);
                    out.push_str(&of_ref(Some(&name), &r));
                    i = n;
                } else {
                    out.extend(&ch[i..(j + 1).min(ch.len())]);
                    i = j + 1;
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                let mut j = i;
                while j < ch.len() && (ch[j].is_ascii_alphanumeric() || ch[j] == '_' || ch[j] == '.' || ch[j] == '$') {
                    j += 1;
                }
                let word: String = ch[i..j].iter().collect();
                if ch.get(j) == Some(&'!') {
                    let (r, n) = read_ref(&ch, j + 1);
                    out.push_str(&of_ref(Some(&word), &r));
                    i = n;
                } else if ch.get(j) == Some(&'(') {
                    out.push_str(&word);
                    i = j;
                } else if is_ref_start(&word) || is_col(&word) && ch.get(j) == Some(&':') {
                    let (r, n) = read_ref(&ch, i);
                    out.push_str(&of_ref(None, &r));
                    i = n;
                } else if word.eq_ignore_ascii_case("TRUE") || word.eq_ignore_ascii_case("FALSE") {
                    // OpenFormula has no logical constants: TRUE() and FALSE().
                    out.push_str(&word);
                    out.push_str("()");
                    i = j;
                } else {
                    out.push_str(&word);
                    i = j;
                }
            }
            c if c.is_ascii_digit() => {
                // A number (with its exponent), or a row range `1:3`.
                let mut j = i;
                while j < ch.len() && (ch[j].is_ascii_digit() || ch[j] == '.') {
                    j += 1;
                }
                if j < ch.len() && (ch[j] == 'E' || ch[j] == 'e') && ch.get(j + 1).is_some_and(|d| d.is_ascii_digit() || *d == '+' || *d == '-') {
                    j += 2;
                    while j < ch.len() && ch[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                out.extend(&ch[i..j]);
                i = j;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn is_col(w: &str) -> bool {
    let w = w.trim_start_matches('$');
    !w.is_empty() && w.len() <= 3 && w.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_ref_start(w: &str) -> bool {
    looks_like_cell(w)
}

/// Reads `A1`, `$A$1:B2`, `A:C` from `ch[i..]`; returns it and the index after.
fn read_ref(ch: &[char], i: usize) -> (String, usize) {
    let part = |mut j: usize| {
        let s = j;
        while j < ch.len() && (ch[j].is_ascii_alphanumeric() || ch[j] == '$') {
            j += 1;
        }
        (ch[s..j].iter().collect::<String>(), j)
    };
    let (a, mut j) = part(i);
    let mut r = a;
    if ch.get(j) == Some(&':') {
        let (b, k) = part(j + 1);
        if !b.is_empty() {
            r.push(':');
            r.push_str(&b);
            j = k;
        }
    }
    (r, j)
}

fn of_ref(sheet: Option<&str>, r: &str) -> String {
    let sh = match sheet {
        Some(s) => {
            let plain = s.chars().all(|c| c.is_alphanumeric() || c == '_');
            if plain {
                format!("${s}")
            } else {
                format!("$'{}'", s.replace('\'', "''"))
            }
        }
        None => String::new(),
    };
    match r.split_once(':') {
        Some((a, b)) => format!("[{sh}.{a}:.{b}]"),
        None => format!("[{sh}.{r}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_and_out() {
        for (of, a1) in [
            ("of:=SUM([.A1:.A3])", "=SUM(A1:A3)"),
            ("of:=IF([.B2]>0;\"a;b\";[Other.$C$3])", "=IF(B2>0,\"a;b\",Other!$C$3)"),
            ("of:=['My sheet'.A1]+[$'It''s'.B2:.C4]", "='My sheet'!A1+'It''s'!B2:C4"),
            ("of:=SUM([.A:.A])", "=SUM(A:A)"),
            ("of:=COM.MICROSOFT.XLOOKUP(1;[.A1:.A3];[.B1:.B3])", "=XLOOKUP(1,A1:A3,B1:B3)"),
            ("of:=SUM({1;2|3;4})", "=SUM({1,2;3,4})"),
            ("of:=[.#REF!]+1", "=#REF!+1"),
            ("of:=1.5E+3*2", "=1.5E+3*2"),
            ("of:=IF(FALSE();1)", "=IF(FALSE,1)"),
        ] {
            assert_eq!(from_of(of).as_deref(), Some(a1), "{of}");
        }
        for (a1, of) in [
            ("=SUM(A1:A3)", "of:=SUM([.A1:.A3])"),
            ("=IF(B2>0,\"a,b\",Other!$C$3)", "of:=IF([.B2]>0;\"a,b\";[$Other.$C$3])"),
            ("='My sheet'!A1+'It''s'!B2:C4", "of:=[$'My sheet'.A1]+[$'It''s'.B2:.C4]"),
            ("=SUM(A:A)", "of:=SUM([.A:.A])"),
            ("=SUM({1,2;3,4})", "of:=SUM({1;2|3;4})"),
            ("=LOG10(100)+ATAN2(1,1)", "of:=LOG10(100)+ATAN2(1;1)"),
            ("=TRUE", "of:=TRUE()"),
            ("=IF(FALSE,1)", "of:=IF(FALSE();1)"),
            ("=1E+30", "of:=1E+30"),
            ("=ISERROR(#REF!)+ISNA(#N/A)", "of:=ISERROR(#REF!)+ISNA(#N/A)"),
        ] {
            assert_eq!(to_of(a1), of, "{a1}");
            assert_eq!(from_of(of).as_deref(), Some(a1), "back {of}");
        }
        assert_eq!(from_of("msoxl:=A1"), None);
        assert_eq!(from_of("of:=[.A1"), None);
    }
}
