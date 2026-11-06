//! The editing core every text input shares: a caret, a selection, motion
//! by grapheme, word and line, undo, and IME composition. It holds no
//! pixels; [`crate::widgets::text_field`] draws it.
//!
//! Positions are byte offsets into the text, always on a grapheme
//! boundary, so an emoji or a letter with combining marks is one step.
//! The selection is `anchor..caret` in either order.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// The longest undo history kept.
const UNDO_MAX: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    /// Start or end of the line (the whole text in a one-line field).
    LineStart,
    LineEnd,
    Start,
    End,
}

/// An in-progress IME composition: shown at the caret, underlined, not yet
/// part of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    /// Byte offset of the IME's own cursor within `text`.
    pub cursor: usize,
}

/// What a key did, for the widget to act on (the clipboard and the form
/// are outside the core).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// The text, caret or selection changed (redraw).
    Changed,
    /// Put this on the clipboard (copy); for cut the text is already gone.
    Copy(String),
    /// Ask the clipboard for text, then call [`TextEdit::insert`].
    Paste,
    /// Enter in a one-line field.
    Submit,
    /// Not an editing key; let it bubble (Tab, Escape, ...).
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Typing,
    Deleting,
    Other,
}

#[derive(Debug, Clone)]
struct Snapshot {
    text: String,
    caret: usize,
    anchor: usize,
}

#[derive(Debug, Clone)]
pub struct TextEdit {
    text: String,
    caret: usize,
    anchor: usize,
    preedit: Option<Preedit>,
    multiline: bool,
    /// A password: no copy or cut, and word motions treat it as one word
    /// (so they don't reveal where its spaces are), and no undo history.
    secret: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last: Kind,
}

impl Default for TextEdit {
    fn default() -> Self { Self::new("") }
}

impl TextEdit {
    pub fn new(text: &str) -> Self {
        let mut e = Self {
            text: String::new(),
            caret: 0,
            anchor: 0,
            preedit: None,
            multiline: false,
            secret: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last: Kind::Other,
        };
        e.text = e.clean(text);
        e.caret = e.text.len();
        e.anchor = e.caret;
        e
    }

    pub fn multiline(mut self, on: bool) -> Self {
        self.multiline = on;
        self
    }

    pub fn secret(mut self, on: bool) -> Self {
        self.secret = on;
        self
    }

    pub fn is_secret(&self) -> bool { self.secret }
    pub fn text(&self) -> &str { &self.text }
    pub fn caret(&self) -> usize { self.caret }
    pub fn anchor(&self) -> usize { self.anchor }
    pub fn preedit(&self) -> Option<&Preedit> { self.preedit.as_ref() }

    pub fn selection(&self) -> Range<usize> {
        self.caret.min(self.anchor)..self.caret.max(self.anchor)
    }

    pub fn has_selection(&self) -> bool { self.caret != self.anchor }

    pub fn selected_text(&self) -> &str { &self.text[self.selection()] }

    /// Replace everything (the app set the value); one undo step.
    pub fn set_text(&mut self, text: &str) {
        let t = self.clean(text);
        if t == self.text {
            return;
        }
        self.checkpoint(Kind::Other);
        self.text = t;
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.preedit = None;
    }

    /// Typed or pasted text, replacing the selection. Control characters
    /// are dropped (a newline too, in a one-line field); a paste is one
    /// undo step, typing coalesces.
    pub fn insert(&mut self, s: &str) -> bool {
        let s = self.clean(s);
        if s.is_empty() && !self.has_selection() {
            return false;
        }
        let typing = s.graphemes(true).count() == 1 && !self.has_selection();
        self.checkpoint(if typing { Kind::Typing } else { Kind::Other });
        let r = self.selection();
        self.text.replace_range(r.clone(), &s);
        self.caret = r.start + s.len();
        self.anchor = self.caret;
        true
    }

    /// Backspace: the selection, else the grapheme (or word) before the caret.
    pub fn backspace(&mut self, word: bool) -> bool {
        let run = !self.has_selection() && !word;
        if !self.has_selection() {
            if self.caret == 0 {
                return false;
            }
            self.anchor = if word { self.word_left(self.caret) } else { self.prev(self.caret) };
        }
        self.checkpoint(if run { Kind::Deleting } else { Kind::Other });
        self.cut_selection();
        true
    }

    /// Delete: the selection, else the grapheme (or word) after the caret.
    pub fn delete(&mut self, word: bool) -> bool {
        let run = !self.has_selection() && !word;
        if !self.has_selection() {
            if self.caret == self.text.len() {
                return false;
            }
            self.anchor = if word { self.word_right(self.caret) } else { self.next(self.caret) };
        }
        self.checkpoint(if run { Kind::Deleting } else { Kind::Other });
        self.cut_selection();
        true
    }

    /// Move the caret; `extend` keeps the anchor (Shift held). Moving left
    /// or right without Shift over a selection collapses it to that side.
    pub fn move_caret(&mut self, m: Motion, extend: bool) {
        self.last = Kind::Other;
        let to = match m {
            Motion::Left if !extend && self.has_selection() => self.selection().start,
            Motion::Right if !extend && self.has_selection() => self.selection().end,
            Motion::Left => self.prev(self.caret),
            Motion::Right => self.next(self.caret),
            Motion::WordLeft => self.word_left(self.caret),
            Motion::WordRight => self.word_right(self.caret),
            Motion::LineStart => self.line_start(self.caret),
            Motion::LineEnd => self.line_end(self.caret),
            Motion::Start => 0,
            Motion::End => self.text.len(),
        };
        self.caret = to;
        if !extend {
            self.anchor = to;
        }
    }

    /// Put the caret at `at` (a click), snapped to a grapheme boundary.
    pub fn set_caret(&mut self, at: usize, extend: bool) {
        self.last = Kind::Other;
        self.caret = self.snap(at);
        if !extend {
            self.anchor = self.caret;
        }
    }

    pub fn select_all(&mut self) {
        self.last = Kind::Other;
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Select the word under `at` (a double click).
    pub fn select_word(&mut self, at: usize) {
        self.last = Kind::Other;
        if self.secret {
            return self.select_all();
        }
        let at = self.snap(at);
        for (i, w) in self.text.split_word_bound_indices() {
            if at >= i && at < i + w.len() || at == self.text.len() && i + w.len() == at {
                self.anchor = i;
                self.caret = i + w.len();
                return;
            }
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else { return false };
        self.redo.push(self.snapshot());
        self.restore(s);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else { return false };
        self.undo.push(self.snapshot());
        self.restore(s);
        true
    }

    /// The IME's composition changed. An empty one ends it.
    pub fn set_preedit(&mut self, text: &str, cursor: usize) {
        self.preedit = (!text.is_empty()).then(|| Preedit { text: text.into(), cursor: cursor.min(text.len()) });
    }

    /// The IME committed `text`: it replaces the composition and the selection.
    pub fn commit_preedit(&mut self, text: &str) {
        self.preedit = None;
        self.insert(text);
    }

    /// One key, as the field receives it. `chars` is the decoded text.
    /// Ctrl or Meta chords are commands (Meta for keyboards that have it).
    pub fn key(&mut self, key: crate::event::Key, m: crate::event::Modifiers, chars: &str) -> KeyOutcome {
        use crate::event::Key;
        let cmd = m.ctrl || m.meta;
        if self.preedit.is_some() {
            // The IME owns the keys while it composes.
            return KeyOutcome::Ignored;
        }
        let changed = |b: bool| if b { KeyOutcome::Changed } else { KeyOutcome::Ignored };
        match key {
            Key::Char if cmd => match chars.to_ascii_lowercase().as_str() {
                "a" => {
                    self.select_all();
                    KeyOutcome::Changed
                }
                "c" if !self.secret && self.has_selection() => KeyOutcome::Copy(self.selected_text().into()),
                "x" if !self.secret && self.has_selection() => {
                    let t = self.selected_text().to_string();
                    self.checkpoint(Kind::Other);
                    self.cut_selection();
                    KeyOutcome::Copy(t)
                }
                "v" => KeyOutcome::Paste,
                "z" if m.shift => changed(self.redo()),
                "z" => changed(self.undo()),
                "y" => changed(self.redo()),
                _ => KeyOutcome::Ignored,
            },
            Key::Char => changed(self.insert(chars)),
            Key::Enter if self.multiline => changed(self.insert("\n")),
            Key::Enter => KeyOutcome::Submit,
            Key::Backspace => changed(self.backspace(cmd || m.alt)),
            Key::Delete => changed(self.delete(cmd || m.alt)),
            Key::Left => {
                self.move_caret(if cmd || m.alt { Motion::WordLeft } else { Motion::Left }, m.shift);
                KeyOutcome::Changed
            }
            Key::Right => {
                self.move_caret(if cmd || m.alt { Motion::WordRight } else { Motion::Right }, m.shift);
                KeyOutcome::Changed
            }
            Key::Home => {
                self.move_caret(if cmd { Motion::Start } else { Motion::LineStart }, m.shift);
                KeyOutcome::Changed
            }
            Key::End => {
                self.move_caret(if cmd { Motion::End } else { Motion::LineEnd }, m.shift);
                KeyOutcome::Changed
            }
            // Up and Down need line layout; a one-line field goes to its ends.
            Key::Up if !self.multiline => {
                self.move_caret(Motion::Start, m.shift);
                KeyOutcome::Changed
            }
            Key::Down if !self.multiline => {
                self.move_caret(Motion::End, m.shift);
                KeyOutcome::Changed
            }
            _ => KeyOutcome::Ignored,
        }
    }

    fn clean(&self, s: &str) -> String {
        s.chars().filter(|&c| !c.is_control() || (self.multiline && c == '\n')).collect()
    }

    fn cut_selection(&mut self) {
        let r = self.selection();
        self.text.replace_range(r.clone(), "");
        self.caret = r.start;
        self.anchor = r.start;
    }

    /// Save the state before an edit, unless it continues a run of the
    /// same kind (typing or deleting), which undoes as one step.
    fn checkpoint(&mut self, k: Kind) {
        // A password keeps no history: no extra copies of it in memory.
        if self.secret {
            self.last = k;
            return;
        }
        if k == Kind::Other || k != self.last {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_MAX {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last = k;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { text: self.text.clone(), caret: self.caret, anchor: self.anchor }
    }

    fn restore(&mut self, s: Snapshot) {
        self.text = s.text;
        self.caret = s.caret;
        self.anchor = s.anchor;
        self.preedit = None;
        self.last = Kind::Other;
    }

    fn snap(&self, at: usize) -> usize {
        let at = at.min(self.text.len());
        let mut last = 0;
        for (i, _) in self.text.grapheme_indices(true) {
            if i > at {
                break;
            }
            last = i;
        }
        if at == self.text.len() { at } else { last }
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at].grapheme_indices(true).next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..].graphemes(true).next().map_or(at, |g| at + g.len())
    }

    fn is_word(s: &str) -> bool { s.chars().any(char::is_alphanumeric) }

    /// The start of the word at or before the caret (skipping spaces first).
    fn word_left(&self, at: usize) -> usize {
        if self.secret {
            return 0;
        }
        let mut to = 0;
        let mut seen_word = false;
        for (i, w) in self.text[..at].split_word_bound_indices().rev() {
            if Self::is_word(w) {
                seen_word = true;
                to = i;
            } else if seen_word || w.contains('\n') {
                break;
            } else {
                to = i;
            }
        }
        to
    }

    /// The end of the word at or after the caret (skipping spaces first).
    fn word_right(&self, at: usize) -> usize {
        if self.secret {
            return self.text.len();
        }
        let mut to = at;
        let mut seen_word = false;
        for (i, w) in self.text[at..].split_word_bound_indices() {
            if Self::is_word(w) {
                seen_word = true;
                to = at + i + w.len();
            } else if seen_word || w.contains('\n') {
                break;
            } else {
                to = at + i + w.len();
            }
        }
        to
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..].find('\n').map_or(self.text.len(), |i| at + i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Key, Modifiers};

    const NONE: Modifiers = Modifiers { shift: false, ctrl: false, alt: false, meta: false };
    const SHIFT: Modifiers = Modifiers { shift: true, ..NONE };
    const CTRL: Modifiers = Modifiers { ctrl: true, ..NONE };
    const CTRL_SHIFT: Modifiers = Modifiers { ctrl: true, shift: true, ..NONE };

    fn typed(s: &str) -> TextEdit {
        let mut e = TextEdit::default();
        for g in s.graphemes(true) {
            e.key(Key::Char, NONE, g);
        }
        e
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut e = typed("helo");
        e.key(Key::Left, NONE, "");
        e.key(Key::Char, NONE, "l");
        assert_eq!(e.text(), "hello");
        assert_eq!(e.caret(), 4);
    }

    #[test]
    fn a_grapheme_is_one_step() {
        // A family emoji (one grapheme, many code points) and an e with a
        // combining acute.
        let mut e = TextEdit::new("a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}e\u{301}");
        e.key(Key::Left, NONE, "");
        assert_eq!(&e.text()[e.caret()..], "e\u{301}");
        e.key(Key::Backspace, NONE, "");
        assert_eq!(e.text(), "ae\u{301}");
        e.key(Key::End, NONE, "");
        e.key(Key::Backspace, NONE, "");
        assert_eq!(e.text(), "a");
    }

    #[test]
    fn shift_extends_and_typing_replaces_the_selection() {
        let mut e = TextEdit::new("hello world");
        e.key(Key::Left, CTRL_SHIFT, "");
        assert_eq!(e.selected_text(), "world");
        e.key(Key::Char, NONE, "there");
        assert_eq!(e.text(), "hello there");
        assert!(!e.has_selection());
    }

    #[test]
    fn left_or_right_collapses_a_selection_to_its_side() {
        let mut e = TextEdit::new("abcdef");
        e.set_caret(2, false);
        e.set_caret(4, true);
        e.key(Key::Left, NONE, "");
        assert_eq!((e.caret(), e.has_selection()), (2, false));
        e.set_caret(4, true);
        e.key(Key::Right, NONE, "");
        assert_eq!(e.caret(), 4);
    }

    #[test]
    fn word_moves_skip_spaces_then_the_word() {
        let mut e = TextEdit::new("one  two, three");
        e.key(Key::Left, CTRL, "");
        assert_eq!(&e.text()[e.caret()..], "three");
        e.key(Key::Left, CTRL, "");
        assert_eq!(&e.text()[e.caret()..], "two, three");
        e.key(Key::Right, CTRL, "");
        assert_eq!(&e.text()[..e.caret()], "one  two");
        e.key(Key::Backspace, CTRL, "");
        assert_eq!(e.text(), "one  , three");
    }

    #[test]
    fn a_password_hides_its_word_boundaries_and_never_copies() {
        let mut e = TextEdit::new("correct horse").secret(true);
        e.key(Key::Left, CTRL, "");
        assert_eq!(e.caret(), 0, "one word");
        e.select_all();
        assert_eq!(e.key(Key::Char, CTRL, "c"), KeyOutcome::Ignored);
        assert_eq!(e.key(Key::Char, CTRL, "x"), KeyOutcome::Ignored);
        assert_eq!(e.text(), "correct horse");
        assert_eq!(e.key(Key::Char, CTRL, "v"), KeyOutcome::Paste);
        e.key(Key::Char, NONE, "!");
        assert_eq!(e.key(Key::Char, CTRL, "z"), KeyOutcome::Ignored, "no undo history kept");
    }

    #[test]
    fn copy_and_cut_give_the_selection() {
        let mut e = TextEdit::new("cut me");
        e.key(Key::Left, CTRL_SHIFT, "");
        assert_eq!(e.key(Key::Char, CTRL, "c"), KeyOutcome::Copy("me".into()));
        assert_eq!(e.key(Key::Char, CTRL, "x"), KeyOutcome::Copy("me".into()));
        assert_eq!(e.text(), "cut ");
        assert_eq!(e.key(Key::Char, CTRL, "c"), KeyOutcome::Ignored, "nothing selected");
    }

    #[test]
    fn typing_undoes_as_one_step_and_redo_brings_it_back() {
        let mut e = typed("hello");
        e.key(Key::Char, NONE, " ");
        e.insert("pasted text");
        assert_eq!(e.text(), "hello pasted text");
        e.key(Key::Char, CTRL, "z");
        assert_eq!(e.text(), "hello ");
        e.key(Key::Char, CTRL, "z");
        assert_eq!(e.text(), "");
        e.key(Key::Char, CTRL_SHIFT, "z");
        assert_eq!(e.text(), "hello ");
        e.key(Key::Char, CTRL, "y");
        assert_eq!(e.text(), "hello pasted text");
        e.key(Key::Char, CTRL, "z");
        e.key(Key::Char, NONE, "!");
        assert!(!e.redo(), "a new edit drops the redo history");
    }

    #[test]
    fn backspaces_coalesce_but_a_move_breaks_the_run() {
        let mut e = TextEdit::new("abcdef");
        e.key(Key::Backspace, NONE, "");
        e.key(Key::Backspace, NONE, "");
        e.key(Key::Left, NONE, "");
        e.key(Key::Backspace, NONE, "");
        assert_eq!(e.text(), "abd");
        e.undo();
        assert_eq!(e.text(), "abcd");
        e.undo();
        assert_eq!(e.text(), "abcdef");
    }

    #[test]
    fn one_line_fields_drop_control_characters_and_submit_on_enter() {
        let mut e = TextEdit::default();
        e.insert("a\nb\tc\u{1b}[31m");
        assert_eq!(e.text(), "abc[31m");
        assert_eq!(e.key(Key::Enter, NONE, ""), KeyOutcome::Submit);
        let mut m = TextEdit::default().multiline(true);
        m.insert("a\nb");
        m.key(Key::Enter, NONE, "");
        assert_eq!(m.text(), "a\nb\n");
    }

    #[test]
    fn home_and_end_stay_on_the_line() {
        let mut m = TextEdit::new("").multiline(true);
        m.insert("first\nsecond line");
        m.key(Key::Left, NONE, "");
        m.key(Key::Home, NONE, "");
        assert_eq!(&m.text()[m.caret()..], "second line");
        m.key(Key::Home, CTRL, "");
        assert_eq!(m.caret(), 0);
        m.key(Key::End, SHIFT, "");
        assert_eq!(m.selected_text(), "first");
    }

    #[test]
    fn the_ime_composes_then_commits_over_the_selection() {
        let mut e = TextEdit::new("I like ");
        e.set_preedit("にほ", 6);
        assert_eq!(e.key(Key::Char, NONE, "x"), KeyOutcome::Ignored, "keys go to the IME");
        assert_eq!(e.text(), "I like ");
        e.commit_preedit("日本");
        assert_eq!(e.text(), "I like 日本");
        assert!(e.preedit().is_none());
    }

    #[test]
    fn a_double_click_selects_the_word() {
        let mut e = TextEdit::new("open the door");
        e.select_word(6);
        assert_eq!(e.selected_text(), "the");
        e.set_caret(100, false);
        assert_eq!(e.caret(), e.text().len(), "a click past the end lands at the end");
    }
}
