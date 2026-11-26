//! Insert Symbol (docs/spec/officina.md §16.152): a character by its name,
//! its code point (`U+00B0`) or itself, added to the end of the cursor
//! cell's text as one step.

use crate::doc::Doc;
use officina_grid::CellAddr;

/// Symbols by the names typed for them.
pub const NAMED: [(&str, char); 44] = [
    ("degree", '°'), ("euro", '€'), ("pound", '£'), ("yen", '¥'), ("rupee", '₹'), ("cent", '¢'),
    ("copyright", '©'), ("registered", '®'), ("trademark", '™'), ("section", '§'), ("paragraph", '¶'),
    ("plus minus", '±'), ("times", '×'), ("divide", '÷'), ("minus", '−'), ("not equal", '≠'),
    ("less or equal", '≤'), ("greater or equal", '≥'), ("approximately", '≈'), ("infinity", '∞'),
    ("square root", '√'), ("sum", '∑'), ("micro", 'µ'), ("pi", 'π'), ("ohm", 'Ω'), ("delta", 'Δ'),
    ("per mille", '‰'), ("one half", '½'), ("one quarter", '¼'), ("three quarters", '¾'),
    ("squared", '²'), ("cubed", '³'), ("left arrow", '←'), ("right arrow", '→'), ("up arrow", '↑'),
    ("down arrow", '↓'), ("check", '✓'), ("cross", '✗'), ("bullet", '•'), ("ellipsis", '…'),
    ("em dash", '—'), ("en dash", '–'), ("star", '★'), ("heart", '♥'),
];

/// The most characters typed as themselves.
const MAX_CHARS: usize = 8;

/// What `typed` names: a name from [`NAMED`] (ignoring case), `U+XXXX`,
/// or the characters themselves (at most [`MAX_CHARS`], no control ones).
pub fn symbol(typed: &str) -> Result<String, String> {
    let t = typed.trim();
    if t.is_empty() {
        return Err("Type a symbol, its name (degree, euro, check…) or its code, such as U+00B0.".into());
    }
    let lower = t.to_lowercase();
    if let Some(&(_, c)) = NAMED.iter().find(|(n, _)| *n == lower || n.replace(' ', "") == lower.replace([' ', '-'], "")) {
        return Ok(c.to_string());
    }
    if let Some(hex) = lower.strip_prefix("u+").or_else(|| lower.strip_prefix("0x")) {
        let c = u32::from_str_radix(hex, 16).ok().filter(|_| (1..=6).contains(&hex.len())).and_then(char::from_u32).filter(|c| !c.is_control());
        return c.map(|c| c.to_string()).ok_or_else(|| format!("{t} isn't a character's code."));
    }
    if t.chars().count() <= MAX_CHARS && !t.chars().any(char::is_control) && t.chars().any(|c| !c.is_ascii_alphanumeric()) {
        return Ok(t.to_string());
    }
    Err(format!("{t:?} isn't a symbol's name: try {}.", NAMED.iter().take(8).map(|n| n.0).collect::<Vec<_>>().join(", ")))
}

/// Insert Symbol at the cursor: what `typed` names, after the cell's text.
pub fn insert(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let sym = symbol(typed)?;
    let a = CellAddr::new(d.sheet, cursor.0, cursor.1);
    if d.wb.formula_text(a).is_some() {
        return Err("The cell holds a formula: edit it to type the symbol in.".into());
    }
    let text = format!("{}{sym}", d.wb.get(a).display());
    d.status.clear();
    if !d.type_into(cursor.0, cursor.1, &text) {
        return Err(if d.status.is_empty() { "The symbol couldn't go in.".into() } else { d.status.clone() });
    }
    Ok(format!("{sym} inserted."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_by_name_code_or_themselves_go_after_the_text() {
        assert_eq!(symbol("Degree").unwrap(), "°");
        assert_eq!(symbol("plusminus").unwrap(), "±");
        assert_eq!(symbol("U+2122").unwrap(), "™");
        assert!(symbol("U+D800").is_err(), "a surrogate");
        assert!(symbol("U+0007").is_err(), "a control character");
        assert_eq!(symbol("→").unwrap(), "→");
        assert!(symbol("degrees celsius").is_err());
        let mut d = Doc::from_csv("s.csv", b"21,x\n=A1*2,\n").unwrap().0;
        assert_eq!(insert(&mut d, (0, 0), "degree").unwrap(), "° inserted.");
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 0)).display(), "21°");
        insert(&mut d, (0, 1), "check").unwrap();
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)).display(), "x✓");
        assert!(insert(&mut d, (1, 0), "euro").is_err(), "not into a formula");
        assert!(d.undo());
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)).display(), "x");
    }
}
