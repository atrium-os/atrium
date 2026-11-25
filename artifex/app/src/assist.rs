//! What Artifex does with a language server's help as you type
//! (dev-tools.md §3.7): the completion menu and the code-action menu,
//! their filtering, and turning a server's edits into buffer edits. Pure:
//! the engine feeds it and applies what it returns.

use pergola::code::CodeBuffer;
use pergola::widgets::code_grid::Menu;

use crate::lsp::{self, Encoding, TextEdit};

/// Most rows a menu lists after filtering.
pub const MAX_SHOWN: usize = 200;

/// What a menu offers.
#[derive(Debug, Clone, PartialEq)]
pub enum Offer {
    Completions(Vec<lsp::Completion>),
    Actions(Vec<lsp::Action>),
}

/// A menu open in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Popup {
    /// Which open file (its path).
    pub path: std::path::PathBuf,
    pub line: usize,
    /// Where the word being completed starts (a char index of the line).
    pub start: usize,
    pub offer: Offer,
    /// Indexes into the offer, best first.
    pub shown: Vec<usize>,
    pub selected: usize,
    /// The server said its list is incomplete: ask again as the word grows.
    pub incomplete: bool,
}

pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Where the word ending at char `col` of `line` starts.
pub fn word_start(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut i = col.min(chars.len());
    while i > 0 && is_word(chars[i - 1]) {
        i -= 1;
    }
    i
}

/// How well `item` matches what was typed: `None` when it doesn't. A
/// prefix beats a case-insensitive prefix beats letters in order.
pub fn score(item: &str, typed: &str) -> Option<u8> {
    if typed.is_empty() {
        return Some(3);
    }
    if item.starts_with(typed) {
        return Some(0);
    }
    let (il, tl) = (item.to_lowercase(), typed.to_lowercase());
    if il.starts_with(&tl) {
        return Some(1);
    }
    let mut it = il.chars();
    tl.chars().all(|c| it.any(|x| x == c)).then_some(2)
}

impl Popup {
    pub fn completions(path: std::path::PathBuf, line: usize, start: usize, items: Vec<lsp::Completion>, incomplete: bool, typed: &str) -> Option<Popup> {
        let mut p = Popup { path, line, start, offer: Offer::Completions(items), shown: Vec::new(), selected: 0, incomplete };
        p.filter(typed);
        (!p.shown.is_empty()).then_some(p)
    }

    pub fn actions(path: std::path::PathBuf, line: usize, start: usize, actions: Vec<lsp::Action>) -> Option<Popup> {
        if actions.is_empty() {
            return None;
        }
        // The server's preferred fix first.
        let mut shown: Vec<usize> = (0..actions.len()).collect();
        shown.sort_by_key(|&i| !actions[i].preferred);
        shown.truncate(MAX_SHOWN);
        Some(Popup { path, line, start, offer: Offer::Actions(actions), shown, selected: 0, incomplete: false })
    }

    /// Show what matches `typed` (completions only).
    pub fn filter(&mut self, typed: &str) {
        let Offer::Completions(items) = &self.offer else { return };
        let mut m: Vec<(u8, &str, usize)> = items.iter().enumerate()
            .filter_map(|(i, c)| score(&c.filter, typed).map(|s| (s, c.sort.as_str(), i)))
            .collect();
        m.sort();
        self.shown = m.into_iter().take(MAX_SHOWN).map(|x| x.2).collect();
        self.selected = 0;
    }

    pub fn menu(&self) -> Menu {
        let items = self.shown.iter().map(|&i| match &self.offer {
            Offer::Completions(c) => (c[i].label.clone(), c[i].detail.clone()),
            Offer::Actions(a) => (a[i].title.clone(), String::new()),
        }).collect();
        Menu { line: self.line, col: self.start, items, selected: self.selected.min(self.shown.len().saturating_sub(1)) }
    }
}

/// `edits` (server ranges) as char ranges of `b`, in `enc`.
pub fn to_chars(b: &CodeBuffer, edits: &[TextEdit], enc: Encoding) -> Vec<(std::ops::Range<usize>, String)> {
    let at = |(line, col): (usize, usize)| -> usize {
        if line >= b.len_lines() {
            return b.len_chars();
        }
        b.at_line_col(line, enc.to_char(&b.line_text(line), col))
    };
    edits.iter().map(|e| (at(e.range.0)..at(e.range.1).max(at(e.range.0)), e.text.clone())).collect()
}

/// Where char `at` lands once `kept` (as [`CodeBuffer::kept_parts`] gives
/// them) replaced their ranges: moved by what changed before it (an insert
/// at its place too), or to the start of an edit it was inside.
pub fn map_through(at: usize, kept: &[(std::ops::Range<usize>, String)]) -> usize {
    let mut shift = 0isize;
    for (r, t) in kept {
        if r.end <= at && (r.start < at || r.is_empty()) {
            shift += t.chars().count() as isize - r.len() as isize;
        } else if r.start < at {
            return (r.start as isize + shift).max(0) as usize;
        } else {
            break;
        }
    }
    (at as isize + shift).max(0) as usize
}

/// Accept completion `c` in `b`, whose caret is at char `caret` of `line`
/// and whose word starts at `start`: replace from the server's range start
/// (or the word's) to the caret. The caret ends after the text.
pub fn accept(b: &mut CodeBuffer, c: &lsp::Completion, line: usize, start: usize, enc: Encoding) {
    let caret = b.primary().head;
    let from = match c.range {
        Some(((l, col), _)) if l == line => b.at_line_col(line, enc.to_char(&b.line_text(line), col)),
        _ => b.at_line_col(line, start),
    }.min(caret);
    if b.replace(vec![(from..caret, c.insert.clone())]) {
        b.set_caret(from + c.insert.chars().count(), false);
    }
}

/// The one character just typed at the caret, from the buffer's edits: a
/// single insertion of one char that ends where the caret is.
pub fn typed_char(b: &CodeBuffer, edits: &[pergola::code::Edit]) -> Option<char> {
    let [e] = edits else { return None };
    if e.old_end_byte != e.start_byte || e.new_end_byte <= e.start_byte || e.new_end_byte - e.start_byte > 4 || b.selections().len() != 1 {
        return None;
    }
    let caret = b.primary().head;
    if caret == 0 || b.text().char_to_byte(caret) != e.new_end_byte {
        return None;
    }
    let c = b.text().char(caret - 1);
    (b.text().char_to_byte(caret - 1) == e.start_byte).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, sort: &str) -> lsp::Completion {
        lsp::Completion { label: label.into(), detail: String::new(), kind: 1, insert: label.into(), range: None, filter: label.into(), sort: sort.into() }
    }

    #[test]
    fn typing_narrows_the_menu_best_first() {
        let items = vec![item("len", "b"), item("is_empty", "a"), item("Len", "c"), item("filter_len", "d")];
        let mut p = Popup::completions("x".into(), 0, 4, items, false, "").unwrap();
        assert_eq!(p.menu().items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["is_empty", "len", "Len", "filter_len"], "by the server's sort");
        p.filter("le");
        assert_eq!(p.menu().items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["len", "Len", "filter_len"]);
        p.filter("zz");
        assert!(p.shown.is_empty());
        assert!(Popup::completions("x".into(), 0, 0, vec![item("a", "a")], false, "q").is_none(), "nothing matching: no menu");
    }

    #[test]
    fn accepting_replaces_the_typed_word() {
        let mut b = CodeBuffer::new("v.le\n");
        b.set_caret(4, false);
        let mut c = item("len", "a");
        c.insert = "len()".into();
        accept(&mut b, &c, 0, word_start("v.le", 4), Encoding::Utf16);
        assert_eq!((b.to_string().as_str(), b.primary().head), ("v.len()\n", 7));
        // The server's range wins when it gives one.
        let mut b = CodeBuffer::new("x::Ve\n");
        b.set_caret(5, false);
        let mut c = item("Vec", "a");
        c.range = Some(((0, 3), (0, 5)));
        accept(&mut b, &c, 0, 99, Encoding::Utf16);
        assert_eq!(b.to_string(), "x::Vec\n");
    }

    #[test]
    fn a_typed_char_is_one_insert_at_the_caret() {
        let mut b = CodeBuffer::new("ab");
        b.set_caret(2, false);
        b.take_edits();
        b.insert(".");
        let e = b.take_edits();
        assert_eq!(typed_char(&b, &e), Some('.'));
        b.insert("xyz");
        let e = b.take_edits();
        assert_eq!(typed_char(&b, &e), None, "a paste is not typing");
        b.backspace(false);
        let e = b.take_edits();
        assert_eq!(typed_char(&b, &e), None);
        assert_eq!(word_start("let foo_b", 9), 4);
        assert_eq!(score("HashMap", "hm"), Some(2));
    }

    #[test]
    fn places_move_with_edits() {
        // "let count = count + 1;" with both counts renamed to n.
        let kept = vec![(4..9, "n".to_string()), (12..17, "n".to_string())];
        assert_eq!(map_through(4, &kept), 4, "a renamed name keeps its start");
        assert_eq!(map_through(12, &kept), 8, "after one shorter name");
        assert_eq!(map_through(14, &kept), 8, "inside an edit: its start");
        assert_eq!(map_through(20, &kept), 12);
        assert_eq!(map_through(0, &[(0..0, "# a\n".to_string())]), 4, "an insert at a place goes before it");
    }

    #[test]
    fn server_edits_map_to_chars() {
        let b = CodeBuffer::new("é = 1\nx\n");
        let e = vec![TextEdit { range: ((0, 0), (0, 1)), text: "y".into() }, TextEdit { range: ((9, 0), (9, 0)), text: "!".into() }];
        assert_eq!(to_chars(&b, &e, Encoding::Utf16), vec![(0..1, "y".into()), (8..8, "!".into())]);
        let a = Popup::actions("x".into(), 0, 0, vec![
            lsp::Action { title: "b".into(), preferred: false, edits: vec![] },
            lsp::Action { title: "a".into(), preferred: true, edits: vec![] }]).unwrap();
        assert_eq!(a.menu().items[0].0, "a", "the preferred fix first");
    }
}
