//! AutoCorrect (docs/spec/officina.md §16.158), as Excel's: typed text is
//! corrected when it is committed. Common misspellings are replaced, `(c)`,
//! `(r)` and `(tm)` become symbols, a word typed with TWo INitial CApitals
//! keeps one, and day names take a capital. Formulas, numbers and dates are
//! left as typed. The correction is its own undo step after the typing, so
//! one undo keeps what was typed, as in Excel.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use officina_grid::book::Input;
use officina_grid::{Rect, Value};

use crate::doc::Doc;

/// Excel's defaults that matter in cells: misspellings and contractions
/// typed without the apostrophe.
const WORDS: &[(&str, &str)] = &[
    ("abscence", "absence"), ("accomodate", "accommodate"), ("acheive", "achieve"), ("acommodate", "accommodate"),
    ("adn", "and"), ("agian", "again"), ("alot", "a lot"), ("arguement", "argument"), ("becuase", "because"),
    ("beggining", "beginning"), ("beleive", "believe"), ("calender", "calendar"), ("cant", "can't"),
    ("collegue", "colleague"), ("comming", "coming"), ("commitee", "committee"), ("definately", "definitely"),
    ("didnt", "didn't"), ("doesnt", "doesn't"), ("dont", "don't"), ("enviroment", "environment"),
    ("existance", "existence"), ("foward", "forward"), ("freind", "friend"), ("goverment", "government"),
    ("hte", "the"), ("independant", "independent"), ("isnt", "isn't"), ("knowlege", "knowledge"),
    ("neccessary", "necessary"), ("occassion", "occasion"), ("occured", "occurred"), ("occurence", "occurrence"),
    ("posession", "possession"), ("publically", "publicly"), ("recieve", "receive"), ("recomend", "recommend"),
    ("refered", "referred"), ("relevent", "relevant"), ("seperate", "separate"), ("succesful", "successful"),
    ("taht", "that"), ("teh", "the"), ("thier", "their"), ("tommorow", "tomorrow"), ("truely", "truly"),
    ("untill", "until"), ("waht", "what"), ("wasnt", "wasn't"), ("wich", "which"), ("wierd", "weird"),
];

/// Symbols replaced wherever they are typed.
const SYMBOLS: &[(&str, &str)] = &[("(c)", "©"), ("(C)", "©"), ("(r)", "®"), ("(R)", "®"), ("(tm)", "™"), ("(TM)", "™"),
    ("(e)", "€"), ("(E)", "€"), ("-->", "→"), ("<--", "←"), ("==>", "⇒"), ("<==", "⇐")];

const DAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

/// Entries of one's own, at most.
pub const MAX_ENTRIES: usize = 1000;
pub const MAX_WORD: usize = 64;
pub const MAX_WITH: usize = 255;
/// The longest text corrected; longer text is left as typed.
const MAX_TEXT: usize = 32_768;

/// The file the person's settings are kept in, once [`init`] has run.
static FILE: OnceLock<PathBuf> = OnceLock::new();
/// The largest settings file read, bytes.
const MAX_FILE: u64 = 1 << 20;

/// Use the settings kept in `dir` (beside the custom lists); each document
/// opened after starts with them, and changes are kept there.
pub fn init(dir: &Path) {
    let _ = FILE.set(dir.join("autocorrect"));
}

#[derive(Debug, Clone)]
pub struct AutoCorrect {
    pub on: bool,
    /// Entries added (word, lowercase, to its replacement) and built-in
    /// entries taken out (to `None`).
    own: BTreeMap<String, Option<String>>,
}

impl Default for AutoCorrect {
    fn default() -> Self {
        Self { on: true, own: BTreeMap::new() }
    }
}

impl AutoCorrect {
    /// The kept settings, or the defaults when there are none (or no file
    /// was set up). A malformed line is skipped.
    pub fn load() -> AutoCorrect {
        let mut ac = AutoCorrect::default();
        let Some(file) = FILE.get() else { return ac };
        let Some(text) = std::fs::metadata(file).ok().filter(|m| m.len() <= MAX_FILE).and_then(|_| std::fs::read_to_string(file).ok()) else { return ac };
        ac.parse(&text);
        ac
    }

    fn parse(&mut self, text: &str) {
        for line in text.lines() {
            let mut f = line.split('\t');
            match (f.next(), f.next(), f.next(), f.next()) {
                (Some("off"), None, ..) => self.on = false,
                (Some("on"), None, ..) => self.on = true,
                (Some("add"), Some(w), Some(with), None) if check(w, MAX_WORD, "word").is_ok() && check(with, MAX_WITH, "replacement").is_ok() && self.own.len() < MAX_ENTRIES => {
                    self.own.insert(w.to_string(), Some(with.to_string()));
                }
                (Some("remove"), Some(w), None, _) if check(w, MAX_WORD, "word").is_ok() && self.own.len() < MAX_ENTRIES => {
                    self.own.insert(w.to_string(), None);
                }
                _ => {}
            }
        }
    }

    fn text(&self) -> String {
        let mut o = String::from(if self.on { "on\n" } else { "off\n" });
        for (w, with) in &self.own {
            match with {
                Some(with) => o.push_str(&format!("add\t{w}\t{with}\n")),
                None => o.push_str(&format!("remove\t{w}\n")),
            }
        }
        o
    }

    /// Keeps the settings for later documents, when a file was set up.
    fn keep(&self) -> Result<(), String> {
        let Some(file) = FILE.get() else { return Ok(()) };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't keep AutoCorrect's settings: {e}"))?;
        }
        let tmp = file.with_extension("new");
        std::fs::write(&tmp, self.text()).and_then(|_| std::fs::rename(&tmp, file)).map_err(|e| format!("Couldn't keep AutoCorrect's settings: {e}"))
    }

    fn replacement(&self, lower: &str) -> Option<&str> {
        match self.own.get(lower) {
            Some(entry) => entry.as_deref(),
            None => WORDS.binary_search_by(|(w, _)| (*w).cmp(lower)).ok().map(|i| WORDS[i].1),
        }
    }

    /// `text` corrected, with each change as `from → to`; `None` when
    /// nothing changes.
    pub fn correct(&self, text: &str) -> Option<(String, Vec<String>)> {
        if !self.on || text.len() > MAX_TEXT {
            return None;
        }
        let mut changes = Vec::new();
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(|c: char| c.is_alphanumeric()) {
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            let end = word_end(rest);
            let word = &rest[..end];
            let chunk = out.rsplit(char::is_whitespace).next().unwrap_or("");
            let in_address = chunk.contains('@') || chunk.contains("://") || chunk.contains("www.") || rest[end..].starts_with('@');
            let fixed = if in_address || word.chars().any(|c| c.is_ascii_digit()) { None } else { self.fix_word(word) };
            match fixed {
                Some(to) => {
                    changes.push(format!("{word} → {to}"));
                    out.push_str(&to);
                }
                None => out.push_str(word),
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        for (from, to) in SYMBOLS {
            if out.contains(from) && self.own.get(*from) != Some(&None) {
                out = out.replace(from, to);
                changes.push(format!("{from} → {to}"));
            }
        }
        (out != text).then_some((out, changes))
    }

    fn fix_word(&self, word: &str) -> Option<String> {
        let lower = word.to_lowercase();
        if let Some(to) = self.replacement(&lower) {
            return Some(match case_of(word) {
                Case::Upper => to.to_uppercase(),
                Case::Title => title(to),
                Case::Lower | Case::Mixed => to.to_string(),
            });
        }
        if DAYS.contains(&lower.as_str()) && word == lower {
            return Some(title(word));
        }
        // TWo INitial CApitals: two capitals, then at least two lowercase letters.
        let chars: Vec<char> = word.chars().collect();
        if chars.len() >= 4 && chars[0].is_uppercase() && chars[1].is_uppercase() && chars[2..].iter().all(|c| c.is_lowercase()) {
            let mut fixed = String::new();
            fixed.push(chars[0]);
            fixed.extend(chars[1].to_lowercase());
            fixed.extend(&chars[2..]);
            return Some(fixed);
        }
        None
    }

    /// The Options bar's text.
    pub fn summary(&self) -> String {
        let added = self.own.values().filter(|v| v.is_some()).count();
        let removed = self.own.values().filter(|v| v.is_none()).count();
        format!("{}{}{}", if self.on { "on" } else { "off" },
            if added > 0 { format!("; {added} added") } else { String::new() },
            if removed > 0 { format!("; {removed} taken out") } else { String::new() })
    }
}

/// Where the word at the start of `s` ends: letters and digits, with an
/// apostrophe inside (`don't`).
fn word_end(s: &str) -> usize {
    let mut end = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_alphanumeric() {
            end = i + c.len_utf8();
        } else if (c == '\'' || c == '’') && chars.peek().is_some_and(|(_, n)| n.is_alphanumeric()) {
            continue;
        } else {
            break;
        }
    }
    end
}

enum Case { Lower, Upper, Title, Mixed }

fn case_of(word: &str) -> Case {
    let mut chars = word.chars();
    let first = chars.next().is_some_and(char::is_uppercase);
    let rest: Vec<char> = chars.collect();
    if !first && word.chars().all(|c| !c.is_uppercase()) { Case::Lower }
    else if first && rest.iter().all(|c| !c.is_lowercase()) && !rest.is_empty() { Case::Upper }
    else if first && rest.iter().all(|c| !c.is_uppercase()) { Case::Title }
    else { Case::Mixed }
}

fn title(word: &str) -> String {
    let mut c = word.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// Commits `text` into `area` as typed, then (as its own step) corrected.
/// The status says what changed.
pub fn type_corrected(d: &mut Doc, area: Rect, text: &str) -> bool {
    if !d.type_all(area, text) {
        return false;
    }
    if !matches!(Input::typed(text), Input::Value(Value::Str(_))) {
        return true;
    }
    let Some((fixed, changes)) = d.autocorrect.correct(text) else { return true };
    // A typed date or number stays as typed (`(c)` alone is text, but never mind).
    if !matches!(Input::typed(&fixed), Input::Value(Value::Str(_))) {
        return true;
    }
    let before = d.status.clone();
    if d.type_all(area, &fixed) {
        if d.status.is_empty() || d.status == before {
            d.status = format!("AutoCorrect: {}. Undo keeps what you typed.", changes.join(", "));
        }
    } else {
        // Refused (validation, protection): what was typed stands.
        d.status = before;
    }
    true
}

/// AutoCorrect Options…: `on`, `off`, `add WORD WITH`, `remove WORD`. The
/// settings belong to the app, as in Excel: kept for later documents.
pub fn options(d: &mut Doc, typed: &str) -> Result<String, String> {
    let said = change(d, typed)?;
    d.autocorrect.keep()?;
    Ok(said)
}

fn change(d: &mut Doc, typed: &str) -> Result<String, String> {
    let t = typed.trim();
    let ac = &mut d.autocorrect;
    let (verb, rest) = t.split_once(char::is_whitespace).map_or((t, ""), |(v, r)| (v, r.trim()));
    match verb.to_ascii_lowercase().as_str() {
        "on" | "off" if rest.is_empty() => {
            ac.on = verb.eq_ignore_ascii_case("on");
        }
        "add" => {
            let (word, with) = rest.split_once(char::is_whitespace).map(|(w, r)| (w, r.trim())).ok_or("Type add, the word, then what replaces it, such as add teh the")?;
            check(word, MAX_WORD, "word")?;
            check(with, MAX_WITH, "replacement")?;
            let key = word.to_lowercase();
            if !ac.own.contains_key(&key) && ac.own.len() >= MAX_ENTRIES {
                return Err(format!("AutoCorrect keeps at most {MAX_ENTRIES} entries of your own."));
            }
            ac.own.insert(key, Some(with.to_string()));
            return Ok(format!("AutoCorrect replaces {word} with {with}."));
        }
        "remove" => {
            check(rest, MAX_WORD, "word")?;
            let key = rest.to_lowercase();
            let built_in = WORDS.iter().any(|(w, _)| *w == key) || SYMBOLS.iter().any(|(s, _)| *s == rest);
            let key = if SYMBOLS.iter().any(|(s, _)| *s == rest) { rest.to_string() } else { key };
            match ac.own.get(&key) {
                Some(Some(_)) if !built_in => { ac.own.remove(&key); }
                _ if built_in => {
                    if !ac.own.contains_key(&key) && ac.own.len() >= MAX_ENTRIES {
                        return Err(format!("AutoCorrect keeps at most {MAX_ENTRIES} entries of your own."));
                    }
                    ac.own.insert(key, None);
                }
                _ => return Err(format!("AutoCorrect has no entry for {rest}.")),
            }
            return Ok(format!("AutoCorrect leaves {rest} as typed."));
        }
        _ => return Err("Type on, off, add WORD REPLACEMENT or remove WORD.".into()),
    }
    Ok(format!("AutoCorrect is {}.", ac.summary()))
}

fn check(s: &str, max: usize, what: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > max || s.chars().any(char::is_control) || (what == "word" && s.chars().any(char::is_whitespace)) {
        return Err(format!("The {what} must be 1 to {max} bytes{}, without control characters.", if what == "word" { " with no spaces" } else { "" }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::CellAddr;

    #[test]
    fn settings_are_kept_as_lines_and_bad_lines_skipped() {
        let mut ac = AutoCorrect::default();
        ac.on = false;
        ac.own.insert("brb".into(), Some("be right back".into()));
        ac.own.insert("teh".into(), None);
        let text = ac.text();
        assert_eq!(text, "off\nadd\tbrb\tbe right back\nremove\tteh\n");
        let mut back = AutoCorrect::default();
        back.parse(&format!("{text}add\tno space\tx\nadd\tw\t\u{7}\nwhat\nremove\n"));
        assert!(!back.on);
        assert_eq!(back.own, ac.own);
        assert_eq!(back.correct("teh brb").map(|c| c.0), None, "off corrects nothing");
        back.on = true;
        assert_eq!(back.correct("teh brb").map(|c| c.0).as_deref(), Some("teh be right back"));
    }

    #[test]
    fn the_list_is_sorted_for_the_search() {
        assert!(WORDS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn corrects_words_symbols_capitals_and_days() {
        let ac = AutoCorrect::default();
        let fix = |t: &str| ac.correct(t).map(|(s, _)| s);
        assert_eq!(fix("teh cat adn dog").as_deref(), Some("the cat and dog"));
        assert_eq!(fix("Teh TEH").as_deref(), Some("The THE"));
        assert_eq!(fix("dont stop").as_deref(), Some("don't stop"));
        assert_eq!(fix("Acme (c) 2027 (tm)").as_deref(), Some("Acme © 2027 ™"));
        assert_eq!(fix("THursday meeting on monday").as_deref(), Some("Thursday meeting on Monday"));
        // Left alone: two capitals before fewer than two lowercase letters,
        // words with digits, addresses, words it doesn't know.
        assert_eq!(fix("IDs and teh2 and ann@teh.com and http://teh.org fine"), None);
        let mut off = AutoCorrect::default();
        off.on = false;
        assert_eq!(off.correct("teh"), None);
    }

    #[test]
    fn typing_corrects_as_a_second_step_and_options_change_it() {
        let mut d = Doc::from_csv("a.csv", b"x\n").unwrap().0;
        let cell = Rect::cell(CellAddr::new(0, 1, 0));
        assert!(type_corrected(&mut d, cell, "teh total"));
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::str("the total"));
        assert!(d.status.starts_with("AutoCorrect: teh → the."), "{}", d.status);
        // One undo keeps what was typed.
        assert!(d.undo());
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::str("teh total"));
        // Formulas and numbers are never touched.
        assert!(type_corrected(&mut d, cell, "=\"teh\""));
        assert_eq!(d.wb.formula_text(CellAddr::new(0, 1, 0)).as_deref(), Some("=\"teh\""));
        // Options: one's own entry, a built-in taken out, off.
        assert!(options(&mut d, "add bdgt budget").is_ok());
        assert!(options(&mut d, "remove teh").is_ok());
        assert!(type_corrected(&mut d, cell, "teh bdgt"));
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::str("teh budget"));
        assert!(options(&mut d, "remove bdgt").is_ok());
        assert!(options(&mut d, "remove nothing").is_err());
        assert!(options(&mut d, "add two words").is_ok());
        assert!(options(&mut d, "add").is_err());
        assert!(options(&mut d, &format!("add {} x", "w".repeat(65))).is_err());
        assert!(options(&mut d, "maybe").is_err());
        assert_eq!(options(&mut d, "off").unwrap(), "AutoCorrect is off; 1 added; 1 taken out.");
        assert!(type_corrected(&mut d, cell, "adn"));
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::str("adn"));
    }
}
