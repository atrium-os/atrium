//! The code editor's model: text in a rope, several selections, undo, and
//! the edit log a syntax tree follows (dev-tools.md §3.4, plan M11, P4).
//!
//! Positions are **char** indices into the rope; lines are the rope's
//! (`\n`-separated). Display columns count a tab to the next multiple of
//! `tab_width` and wide characters as two, the way [`CodeGrid`] draws
//! them, so vertical motion keeps its column.
//!
//! Every change goes through [`CodeBuffer::apply`]: a set of
//! non-overlapping replacements made at once (one per selection, for
//! multi-cursor typing). It records the inverse for undo and the byte /
//! point form of each replacement for tree-sitter's incremental reparse
//! ([`CodeBuffer::take_edits`]).
//!
//! [`CodeGrid`]: crate::widgets::CodeGrid

use std::ops::Range;

use ropey::Rope;
use unicode_width::UnicodeWidthChar;

use crate::event::{Key, Modifiers};
use crate::text_edit::KeyOutcome;

/// One selection: `anchor` stays, `head` moves (the caret). Empty when they
/// are equal. `goal` is the display column vertical motion aims for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    pub goal: Option<usize>,
}

impl Selection {
    pub fn caret(at: usize) -> Self {
        Selection { anchor: at, head: at, goal: None }
    }
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// A position as tree-sitter wants it: row and **byte** column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub row: usize,
    pub col_byte: usize,
}

/// One replacement, in bytes and points (tree-sitter's `InputEdit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start: Point,
    pub old_end: Point,
    pub new_end: Point,
}

/// A change to undo or redo: replacements in the coordinates of the text
/// *before* it, and the selections on either side.
#[derive(Debug, Clone)]
struct Change {
    /// (range in the old text, text that replaced it)
    parts: Vec<(Range<usize>, String)>,
    /// What each part replaced (for the inverse).
    removed: Vec<String>,
    before: Vec<Selection>,
    after: Vec<Selection>,
    kind: ChangeKind,
    /// Undone and redone together with the step before it (a run of
    /// typing is one step).
    joined: bool,
}

/// Where the selections go after a change.
#[derive(Debug, Clone, Copy)]
enum After {
    /// A caret at the end of each replacement.
    Carets,
    /// Each selection moved with the text; `grow` keeps an insert at the
    /// line start where a selection starts inside it (indenting a block).
    Mapped { grow: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Typing,
    Deleting,
    Other,
}

/// Motions, each applied to every selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    /// First non-blank, then column 0 (smart Home).
    LineStart,
    LineEnd,
    PageUp(usize),
    PageDown(usize),
    DocStart,
    DocEnd,
}

/// The editor's text and state.
#[derive(Debug, Clone)]
pub struct CodeBuffer {
    text: Rope,
    sels: Vec<Selection>,
    /// Index in `sels` of the primary selection (the one scrolled to).
    primary: usize,
    undo: Vec<Change>,
    redo: Vec<Change>,
    /// The next change may join the last one (typing a word).
    coalesce: bool,
    edits: Vec<Edit>,
    /// Bumped on every text change.
    version: u64,
    /// The version that was last saved.
    saved: u64,
    pub tab_width: usize,
    /// Indent with tabs (else `tab_width` spaces).
    pub use_tabs: bool,
    /// Line comment prefix for toggle comment (`//`, `#`).
    pub comment: Option<String>,
}

impl CodeBuffer {
    pub fn new(text: &str) -> Self {
        CodeBuffer {
            text: Rope::from_str(text),
            sels: vec![Selection::caret(0)],
            primary: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            coalesce: false,
            edits: Vec::new(),
            version: 0,
            saved: 0,
            tab_width: 4,
            use_tabs: false,
            comment: None,
        }
    }

    pub fn text(&self) -> &Rope { &self.text }
    pub fn to_string(&self) -> String { self.text.to_string() }
    pub fn len_chars(&self) -> usize { self.text.len_chars() }
    pub fn len_lines(&self) -> usize { self.text.len_lines() }
    pub fn version(&self) -> u64 { self.version }
    /// Changed since [`CodeBuffer::mark_saved`].
    pub fn is_dirty(&self) -> bool { self.version != self.saved }
    pub fn mark_saved(&mut self) { self.saved = self.version; }
    pub fn selections(&self) -> &[Selection] { &self.sels }
    pub fn primary(&self) -> Selection { self.sels[self.primary] }
    pub fn primary_index(&self) -> usize { self.primary }

    /// Replace every selection with these (sorted, merged); the primary
    /// is the one at `primary`.
    pub fn set_selections(&mut self, sels: Vec<Selection>, primary: usize) {
        let keep = sels.get(primary).copied();
        self.sels = if sels.is_empty() { vec![Selection::caret(0)] } else { sels };
        for s in &mut self.sels {
            s.anchor = s.anchor.min(self.text.len_chars());
            s.head = s.head.min(self.text.len_chars());
        }
        self.normalize(keep);
        self.coalesce = false;
    }

    /// One caret at `at` (or a selection from the primary's anchor).
    pub fn set_caret(&mut self, at: usize, extend: bool) {
        let at = at.min(self.text.len_chars());
        let anchor = if extend { self.primary().anchor } else { at };
        self.set_selections(vec![Selection { anchor, head: at, goal: None }], 0);
    }

    /// Add a caret at `at` (Alt+click), as the new primary.
    pub fn add_caret(&mut self, at: usize) {
        let at = at.min(self.text.len_chars());
        let mut v = self.sels.clone();
        v.push(Selection::caret(at));
        let n = v.len() - 1;
        self.set_selections(v, n);
    }

    /// Take the replacements made since the last call, oldest first.
    pub fn take_edits(&mut self) -> Vec<Edit> {
        std::mem::take(&mut self.edits)
    }

    // ---- positions -------------------------------------------------------

    /// (line, char column) of char index `at`.
    pub fn line_col(&self, at: usize) -> (usize, usize) {
        let line = self.text.char_to_line(at);
        (line, at - self.text.line_to_char(line))
    }

    /// The char index of (line, char column), clamped to the line.
    pub fn at_line_col(&self, line: usize, col: usize) -> usize {
        let line = line.min(self.text.len_lines() - 1);
        let start = self.text.line_to_char(line);
        start + col.min(self.line_len(line))
    }

    /// Chars on `line`, without its line end.
    pub fn line_len(&self, line: usize) -> usize {
        let l = self.text.line(line);
        let mut n = l.len_chars();
        if n > 0 && l.char(n - 1) == '\n' {
            n -= 1;
            if n > 0 && l.char(n - 1) == '\r' {
                n -= 1;
            }
        }
        n
    }

    /// `line`'s text, without its line end.
    pub fn line_text(&self, line: usize) -> String {
        let l = self.text.line(line);
        l.slice(..self.line_len(line)).to_string()
    }

    /// The display column of char column `col` on `line`.
    pub fn display_col(&self, line: usize, col: usize) -> usize {
        let mut d = 0;
        for c in self.text.line(line).chars().take(col) {
            d = advance(d, c, self.tab_width);
        }
        d
    }

    /// The char column on `line` nearest display column `d` (not past it).
    pub fn col_at_display(&self, line: usize, d: usize) -> usize {
        let mut x = 0;
        for (i, c) in self.text.line(line).chars().take(self.line_len(line)).enumerate() {
            let next = advance(x, c, self.tab_width);
            if next > d {
                // Past the middle of a wide cell or tab: the next column.
                return if d - x >= (next - x + 1) / 2 && next - x > 1 { i + 1 } else { i };
            }
            x = next;
        }
        self.line_len(line)
    }

    fn point(&self, at: usize) -> Point {
        let row = self.text.char_to_line(at);
        let col_byte = self.text.char_to_byte(at) - self.text.line_to_byte(row);
        Point { row, col_byte }
    }

    fn text_of(&self, r: Range<usize>) -> String {
        self.text.slice(r).to_string()
    }

    // ---- applying changes --------------------------------------------------

    /// Make the replacements `parts` (ranges in the current text, sorted and
    /// not overlapping) at once, record them for undo and the syntax tree,
    /// and place the selections by `after`.
    fn apply(&mut self, parts: Vec<(Range<usize>, String)>, kind: ChangeKind, after: After) -> bool {
        if parts.iter().all(|(r, s)| r.is_empty() && s.is_empty()) {
            return false;
        }
        let before = self.sels.clone();
        let removed: Vec<String> = parts.iter().map(|(r, _)| self.text_of(r.clone())).collect();
        // Whether each selection starts at a line start, in the old text.
        let at_line_start: Vec<bool> = self.sels.iter()
            .map(|sel| { let a = sel.range().start; self.text.line_to_char(self.text.char_to_line(a)) == a })
            .collect();
        self.raw_apply(&parts);
        let sels: Vec<Selection> = match after {
            After::Carets => carets_after(&parts),
            After::Mapped { grow } => before.iter().zip(&at_line_start).map(|(sel, &ls)| {
                let r = sel.range();
                let a = if grow && ls && !sel.is_empty() { map_before(&parts, r.start) } else { map_pos(&parts, r.start) };
                let b = map_pos(&parts, r.end).max(a);
                if sel.head >= sel.anchor { Selection { anchor: a, head: b, goal: None } } else { Selection { anchor: b, head: a, goal: None } }
            }).collect(),
        };
        let keep = sels.get(self.primary).copied();
        self.sels = sels;
        let len = self.text.len_chars();
        for s in &mut self.sels {
            s.anchor = s.anchor.min(len);
            s.head = s.head.min(len);
        }
        self.normalize(keep);
        let joined = self.coalesce && kind != ChangeKind::Other
            && self.undo.last().is_some_and(|l| l.kind == kind);
        let change = Change { parts, removed, before, after: self.sels.clone(), kind, joined };
        self.redo.clear();
        self.undo.push(change);
        self.coalesce = kind != ChangeKind::Other;
        const MAX: usize = 10_000;
        if self.undo.len() > MAX {
            self.undo.drain(0..self.undo.len() - MAX);
            if let Some(f) = self.undo.first_mut() {
                f.joined = false;
            }
        }
        true
    }

    /// Undo the last step (a run of typing is one step).
    pub fn undo(&mut self) -> bool {
        let mut any = false;
        while let Some(c) = self.undo.pop() {
            let joined = c.joined;
            self.revert(&c);
            self.redo.push(c);
            any = true;
            if !joined {
                break;
            }
        }
        self.coalesce = false;
        any
    }

    pub fn redo(&mut self) -> bool {
        let mut any = false;
        while let Some(c) = self.redo.pop() {
            self.replay(&c);
            self.undo.push(c);
            any = true;
            // Keep going through the joined steps that follow.
            if !self.redo.last().is_some_and(|n| n.joined) {
                break;
            }
        }
        self.coalesce = false;
        any
    }

    fn revert(&mut self, c: &Change) {
        // The inverse: in the new text, each part's new span goes back to
        // what it removed.
        let mut inv = Vec::with_capacity(c.parts.len());
        let mut shift: isize = 0;
        for ((r, s), old) in c.parts.iter().zip(&c.removed) {
            let start = (r.start as isize + shift) as usize;
            let n = s.chars().count();
            inv.push((start..start + n, old.clone()));
            shift += n as isize - (r.end - r.start) as isize;
        }
        self.raw_apply(&inv);
        self.sels = c.before.clone();
        self.primary = self.primary.min(self.sels.len() - 1);
    }

    fn replay(&mut self, c: &Change) {
        self.raw_apply(&c.parts);
        self.sels = c.after.clone();
        self.primary = self.primary.min(self.sels.len() - 1);
    }

    /// Replacements without undo bookkeeping (undo/redo themselves).
    fn raw_apply(&mut self, parts: &[(Range<usize>, String)]) {
        let mut shift: isize = 0;
        for (r, s) in parts {
            let start = (r.start as isize + shift) as usize;
            let end = (r.end as isize + shift) as usize;
            let start_byte = self.text.char_to_byte(start);
            let old_end_byte = self.text.char_to_byte(end);
            let (sp, op) = (self.point(start), self.point(end));
            self.text.remove(start..end);
            self.text.insert(start, s);
            let new_end = start + s.chars().count();
            self.edits.push(Edit {
                start_byte, old_end_byte, new_end_byte: self.text.char_to_byte(new_end),
                start: sp, old_end: op, new_end: self.point(new_end),
            });
            shift += s.chars().count() as isize - (r.end - r.start) as isize;
        }
        self.version += 1;
    }

    /// Sort, merge overlapping selections (and carets at one place), keep
    /// `primary` pointing at the one that holds `keep`'s caret.
    fn normalize(&mut self, keep: Option<Selection>) {
        let mut v: Vec<Selection> = std::mem::take(&mut self.sels);
        v.sort_by_key(|s| (s.range().start, s.range().end));
        let mut out: Vec<Selection> = Vec::with_capacity(v.len());
        for s in v {
            if let Some(last) = out.last_mut() {
                let (lr, sr) = (last.range(), s.range());
                let overlap = sr.start < lr.end;
                let same_caret = s.is_empty() && last.is_empty() && s.head == last.head;
                if overlap || same_caret {
                    let r = lr.start..lr.end.max(sr.end);
                    *last = if last.head >= last.anchor {
                        Selection { anchor: r.start, head: r.end, goal: last.goal }
                    } else {
                        Selection { anchor: r.end, head: r.start, goal: last.goal }
                    };
                    continue;
                }
            }
            out.push(s);
        }
        if out.is_empty() {
            out.push(Selection::caret(0));
        }
        self.primary = keep
            .and_then(|k| out.iter().position(|s| s.range().start <= k.head && k.head <= s.range().end))
            .unwrap_or(out.len() - 1);
        self.sels = out;
    }

    // ---- editing -----------------------------------------------------------

    /// Type `s` at every selection, replacing what is selected. When a
    /// multi-line `s` has as many lines as there are selections, each gets
    /// its own line (paste of a column).
    pub fn insert(&mut self, s: &str) -> bool {
        let s = s.replace("\r\n", "\n");
        let lines: Vec<&str> = s.split('\n').collect();
        let per = self.sels.len() > 1 && lines.len() == self.sels.len();
        let parts: Vec<(Range<usize>, String)> = self.sels.iter().enumerate()
            .map(|(i, sel)| (sel.range(), if per { lines[i].to_string() } else { s.clone() }))
            .collect();
        let typing = s.chars().count() == 1 && !s.contains('\n');
        if !typing {
            self.coalesce = false;
        }
        let kind = if typing { ChangeKind::Typing } else { ChangeKind::Other };
        self.apply(parts, kind, After::Carets)
    }

    /// Enter: a new line keeping the indentation, one level more after an
    /// opening bracket.
    pub fn newline(&mut self) -> bool {
        self.coalesce = false;
        let unit = self.indent_unit();
        let parts: Vec<(Range<usize>, String)> = self.sels.iter().map(|sel| {
            let r = sel.range();
            let (line, col) = self.line_col(r.start);
            let text = self.line_text(line);
            let indent: String = text.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let before: String = text.chars().take(col).collect();
            let more = matches!(before.trim_end().chars().last(), Some('{' | '(' | '[' | ':'));
            (r, format!("\n{indent}{}", if more { unit.as_str() } else { "" }))
        }).collect();
        self.apply(parts, ChangeKind::Other, After::Carets)
    }

    fn indent_unit(&self) -> String {
        if self.use_tabs { "\t".into() } else { " ".repeat(self.tab_width) }
    }

    /// Backspace: the selection, else the char before (a whole indent level
    /// in leading spaces); `word` deletes to the word start.
    pub fn backspace(&mut self, word: bool) -> bool {
        let parts: Vec<(Range<usize>, String)> = self.sels.iter().map(|sel| {
            if !sel.is_empty() {
                return (sel.range(), String::new());
            }
            let at = sel.head;
            if at == 0 {
                return (0..0, String::new());
            }
            if word {
                return (self.word_left(at)..at, String::new());
            }
            let (line, col) = self.line_col(at);
            let text = self.line_text(line);
            let lead = text.chars().take(col).all(|c| c == ' ');
            if lead && col > 0 && !self.use_tabs {
                let n = (col - 1) % self.tab_width + 1;
                return (at - n..at, String::new());
            }
            (at - 1..at, String::new())
        }).collect();
        self.delete_parts(parts)
    }

    /// Delete: the selection, else the char after; `word` to the word end.
    pub fn delete(&mut self, word: bool) -> bool {
        let len = self.text.len_chars();
        let parts: Vec<(Range<usize>, String)> = self.sels.iter().map(|sel| {
            if !sel.is_empty() {
                return (sel.range(), String::new());
            }
            let at = sel.head;
            if at >= len {
                return (at..at, String::new());
            }
            if word { (at..self.word_right(at), String::new()) } else { (at..at + 1, String::new()) }
        }).collect();
        self.delete_parts(parts)
    }

    fn delete_parts(&mut self, parts: Vec<(Range<usize>, String)>) -> bool {
        let parts = merge_ranges(parts);
        if self.undo.last().is_none_or(|c| c.kind != ChangeKind::Deleting) {
            self.coalesce = false;
        }
        self.apply(parts, ChangeKind::Deleting, After::Carets)
    }

    /// The lines the selections touch, each once, sorted.
    fn touched_lines(&self) -> Vec<usize> {
        let mut lines: Vec<usize> = Vec::new();
        for s in &self.sels {
            let r = s.range();
            let a = self.text.char_to_line(r.start);
            let mut b = self.text.char_to_line(r.end);
            // A selection ending at a line's start doesn't take that line.
            if b > a && self.text.line_to_char(b) == r.end {
                b -= 1;
            }
            lines.extend(a..=b);
        }
        lines.sort();
        lines.dedup();
        lines
    }

    /// Tab: an indent at each caret, or the selected lines indented.
    pub fn indent(&mut self) -> bool {
        self.coalesce = false;
        let unit = self.indent_unit();
        if self.sels.iter().all(|s| s.is_empty()) {
            let tw = self.tab_width;
            let parts = self.sels.iter().map(|s| {
                let (line, col) = self.line_col(s.head);
                let d = self.display_col(line, col);
                let ins = if self.use_tabs { "\t".to_string() } else { " ".repeat(tw - d % tw) };
                (s.head..s.head, ins)
            }).collect();
            return self.apply(parts, ChangeKind::Other, After::Carets);
        }
        let parts = self.touched_lines().into_iter()
            .filter(|&l| self.line_len(l) > 0)
            .map(|l| { let at = self.text.line_to_char(l); (at..at, unit.clone()) })
            .collect();
        self.apply(parts, ChangeKind::Other, After::Mapped { grow: true })
    }

    /// Shift+Tab: the selected lines one level out.
    pub fn outdent(&mut self) -> bool {
        self.coalesce = false;
        let tw = self.tab_width;
        let parts = self.touched_lines().into_iter().filter_map(|l| {
            let at = self.text.line_to_char(l);
            let text = self.line_text(l);
            let n = if text.starts_with('\t') { 1 } else { text.chars().take(tw).take_while(|c| *c == ' ').count() };
            (n > 0).then(|| (at..at + n, String::new()))
        }).collect();
        self.apply(parts, ChangeKind::Other, After::Mapped { grow: false })
    }

    /// Ctrl+/: comment the touched lines out, or back in when all are.
    pub fn toggle_comment(&mut self) -> bool {
        let Some(prefix) = self.comment.clone() else { return false };
        self.coalesce = false;
        let lines: Vec<usize> = self.touched_lines().into_iter().filter(|&l| !self.line_text(l).trim().is_empty()).collect();
        if lines.is_empty() {
            return false;
        }
        let all = lines.iter().all(|&l| self.line_text(l).trim_start().starts_with(&prefix));
        let indent = lines.iter().map(|&l| {
            let t = self.line_text(l);
            t.chars().take_while(|c| *c == ' ' || *c == '\t').count()
        }).min().unwrap_or(0);
        let parts = lines.iter().map(|&l| {
            let start = self.text.line_to_char(l);
            if all {
                let t = self.line_text(l);
                let lead = t.chars().take_while(|c| *c == ' ' || *c == '\t').count();
                let rest: String = t.chars().skip(lead).collect();
                let n = prefix.chars().count() + rest[prefix.len()..].starts_with(' ') as usize;
                (start + lead..start + lead + n, String::new())
            } else {
                (start + indent..start + indent, format!("{prefix} "))
            }
        }).collect();
        self.apply(parts, ChangeKind::Other, After::Mapped { grow: false })
    }

    // ---- motion ------------------------------------------------------------

    /// Move every selection by `m`; `extend` keeps the anchors.
    pub fn move_by(&mut self, m: Motion, extend: bool) {
        self.coalesce = false;
        let sels: Vec<Selection> = self.sels.iter().map(|s| {
            // Without extend, Left/Right on a selection collapse to its edge.
            if !extend && !s.is_empty() {
                match m {
                    Motion::Left => return Selection::caret(s.range().start),
                    Motion::Right => return Selection::caret(s.range().end),
                    _ => {}
                }
            }
            let (head, goal) = self.motion(s, m);
            Selection { anchor: if extend { s.anchor } else { head }, head, goal }
        }).collect();
        let p = self.primary;
        self.sels = sels;
        let keep = self.sels.get(p).copied();
        self.normalize(keep);
    }

    fn motion(&self, s: &Selection, m: Motion) -> (usize, Option<usize>) {
        let len = self.text.len_chars();
        let at = s.head;
        let (line, col) = self.line_col(at);
        let vertical = |lines: isize| {
            let goal = s.goal.unwrap_or_else(|| self.display_col(line, col));
            let target = line as isize + lines;
            if target < 0 {
                return (0, Some(goal));
            }
            if target as usize >= self.text.len_lines() {
                return (len, Some(goal));
            }
            let t = target as usize;
            (self.text.line_to_char(t) + self.col_at_display(t, goal), Some(goal))
        };
        match m {
            Motion::Left => (at.saturating_sub(1), None),
            Motion::Right => ((at + 1).min(len), None),
            Motion::WordLeft => (self.word_left(at), None),
            Motion::WordRight => (self.word_right(at), None),
            Motion::Up => vertical(-1),
            Motion::Down => vertical(1),
            Motion::PageUp(n) => vertical(-(n.max(1) as isize)),
            Motion::PageDown(n) => vertical(n.max(1) as isize),
            Motion::LineStart => {
                let start = self.text.line_to_char(line);
                let first = self.line_text(line).chars().take_while(|c| c.is_whitespace()).count();
                (if col == first { start } else { start + first }, None)
            }
            Motion::LineEnd => (self.text.line_to_char(line) + self.line_len(line), None),
            Motion::DocStart => (0, None),
            Motion::DocEnd => (len, None),
        }
    }

    fn class(c: char) -> u8 {
        if c.is_alphanumeric() || c == '_' { 2 } else if c.is_whitespace() { 0 } else { 1 }
    }

    /// The start of the word before `at` (skipping blanks first).
    pub fn word_left(&self, at: usize) -> usize {
        let mut i = at;
        while i > 0 && Self::class(self.text.char(i - 1)) == 0 {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        let k = Self::class(self.text.char(i - 1));
        while i > 0 && Self::class(self.text.char(i - 1)) == k {
            i -= 1;
        }
        i
    }

    /// The end of the word after `at`.
    pub fn word_right(&self, at: usize) -> usize {
        let len = self.text.len_chars();
        let mut i = at;
        while i < len && Self::class(self.text.char(i)) == 0 {
            i += 1;
        }
        if i == len {
            return len;
        }
        let k = Self::class(self.text.char(i));
        while i < len && Self::class(self.text.char(i)) == k {
            i += 1;
        }
        i
    }

    /// The word around `at` (double-click).
    pub fn word_at(&self, at: usize) -> Range<usize> {
        let len = self.text.len_chars();
        if len == 0 {
            return 0..0;
        }
        let at = at.min(len - 1);
        let k = Self::class(self.text.char(at));
        let mut a = at;
        while a > 0 && Self::class(self.text.char(a - 1)) == k {
            a -= 1;
        }
        let mut b = at;
        while b < len && Self::class(self.text.char(b)) == k {
            b += 1;
        }
        a..b
    }

    // ---- several cursors ----------------------------------------------------

    /// A caret on the line above (`up`) or below the topmost/bottommost
    /// one, at the primary's column.
    pub fn add_cursor_vertical(&mut self, up: bool) {
        let edge = if up { self.sels.first() } else { self.sels.last() }.copied().unwrap();
        let (line, col) = self.line_col(edge.head);
        let goal = edge.goal.unwrap_or_else(|| self.display_col(line, col));
        let target = if up { line.checked_sub(1) } else { (line + 1 < self.text.len_lines()).then_some(line + 1) };
        if let Some(t) = target {
            let at = self.text.line_to_char(t) + self.col_at_display(t, goal);
            let mut v = self.sels.clone();
            v.push(Selection { anchor: at, head: at, goal: Some(goal) });
            let n = v.len() - 1;
            self.set_selections(v, n);
        }
    }

    /// Ctrl+D: select the word at the caret, then each next occurrence of
    /// the primary selection's text.
    pub fn select_next(&mut self) -> bool {
        let p = self.primary();
        if p.is_empty() {
            let w = self.word_at(p.head);
            if w.is_empty() {
                return false;
            }
            let mut v = self.sels.clone();
            v[self.primary] = Selection { anchor: w.start, head: w.end, goal: None };
            let i = self.primary;
            self.set_selections(v, i);
            return true;
        }
        let needle = self.text_of(p.range());
        let last = self.sels.iter().map(|s| s.range().end).max().unwrap_or(0);
        let found = self.find_from(&needle, last, true).or_else(|| self.find_from(&needle, 0, true));
        match found {
            Some(r) if !self.sels.iter().any(|s| s.range() == r) => {
                let mut v = self.sels.clone();
                v.push(Selection { anchor: r.start, head: r.end, goal: None });
                let n = v.len() - 1;
                self.set_selections(v, n);
                true
            }
            _ => false,
        }
    }

    /// Escape: only the primary selection, collapsed if it was a caret
    /// group.
    pub fn single(&mut self) -> bool {
        if self.sels.len() == 1 {
            return false;
        }
        let p = self.primary();
        self.set_selections(vec![p], 0);
        true
    }

    pub fn select_all(&mut self) {
        let len = self.text.len_chars();
        self.set_selections(vec![Selection { anchor: 0, head: len, goal: None }], 0);
    }

    /// The selected text of every selection, joined by newlines (copy).
    pub fn selected_text(&self) -> String {
        self.sels.iter().filter(|s| !s.is_empty()).map(|s| self.text_of(s.range())).collect::<Vec<_>>().join("\n")
    }

    /// Copy of empty selections takes their whole lines.
    fn copy_text(&self) -> String {
        if self.sels.iter().any(|s| !s.is_empty()) {
            return self.selected_text();
        }
        let mut out = String::new();
        for l in self.touched_lines() {
            out.push_str(&self.line_text(l));
            out.push('\n');
        }
        out
    }

    // ---- find --------------------------------------------------------------

    /// The next match of `needle` at or after `from`.
    pub fn find_from(&self, needle: &str, from: usize, case: bool) -> Option<Range<usize>> {
        if needle.is_empty() {
            return None;
        }
        let hay = self.text.to_string();
        let start_byte = self.text.char_to_byte(from.min(self.text.len_chars()));
        let pos = if case {
            hay[start_byte..].find(needle).map(|p| p + start_byte)
        } else {
            let (h, n) = (hay[start_byte..].to_lowercase(), needle.to_lowercase());
            // Lowercasing can change lengths; fall back to an exact scan.
            if h.len() == hay.len() - start_byte && n.len() == needle.len() {
                h.find(&n).map(|p| p + start_byte)
            } else {
                hay[start_byte..].find(needle).map(|p| p + start_byte)
            }
        }?;
        let a = self.text.byte_to_char(pos);
        Some(a..a + needle.chars().count())
    }

    /// Every match of `needle`, in order.
    pub fn find_all(&self, needle: &str, case: bool) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(r) = self.find_from(needle, from, case) {
            from = r.end.max(r.start + 1);
            out.push(r);
            if from > self.text.len_chars() {
                break;
            }
        }
        out
    }

    /// Select the next match after the primary selection (wrapping).
    pub fn find_next(&mut self, needle: &str, case: bool) -> bool {
        let from = self.primary().range().end;
        match self.find_from(needle, from, case).or_else(|| self.find_from(needle, 0, case)) {
            Some(r) => {
                self.set_selections(vec![Selection { anchor: r.start, head: r.end, goal: None }], 0);
                true
            }
            None => false,
        }
    }

    /// Replace every match; returns how many.
    /// Make the replacements `parts` (char ranges of the current text) as
    /// one undo step; the selections move with the text. Ranges past the
    /// end are clipped, and one that overlaps an earlier one is left out.
    /// Edits from a language server (a rename, formatting) come this way.
    pub fn replace(&mut self, parts: Vec<(Range<usize>, String)>) -> bool {
        let kept = self.kept_parts(parts);
        self.coalesce = false;
        self.apply(kept, ChangeKind::Other, After::Mapped { grow: false })
    }

    /// The edits [`CodeBuffer::replace`] applies of `parts`: clipped to
    /// the text, in order, an overlapping one left out, CRLF made LF.
    pub fn kept_parts(&self, mut parts: Vec<(Range<usize>, String)>) -> Vec<(Range<usize>, String)> {
        let len = self.text.len_chars();
        for (r, _) in &mut parts {
            r.end = r.end.min(len);
            r.start = r.start.min(r.end);
        }
        parts.sort_by_key(|(r, _)| (r.start, r.end));
        let mut kept: Vec<(Range<usize>, String)> = Vec::with_capacity(parts.len());
        for (r, t) in parts {
            if kept.last().is_some_and(|(l, _)| l.end > r.start) {
                continue;
            }
            kept.push((r, t.replace("\r\n", "\n")));
        }
        kept
    }

    pub fn replace_all(&mut self, needle: &str, with: &str, case: bool) -> usize {
        let parts: Vec<(Range<usize>, String)> = self.find_all(needle, case).into_iter().map(|r| (r, with.to_string())).collect();
        let n = parts.len();
        self.coalesce = false;
        self.apply(parts, ChangeKind::Other, After::Mapped { grow: false });
        n
    }

    // ---- keys ----------------------------------------------------------------

    /// A key press, the editor's way. `page` is the lines a page moves.
    pub fn key(&mut self, key: Key, m: Modifiers, chars: &str, page: usize) -> KeyOutcome {
        let cmd = m.ctrl || m.meta;
        let changed = |b: bool| if b { KeyOutcome::Changed } else { KeyOutcome::Ignored };
        let moved = |me: &mut Self, mo: Motion| {
            me.move_by(mo, m.shift);
            KeyOutcome::Changed
        };
        match key {
            Key::Char if cmd => match chars.to_ascii_lowercase().as_str() {
                "a" => { self.select_all(); KeyOutcome::Changed }
                "c" => KeyOutcome::Copy(self.copy_text()),
                "x" => {
                    let t = self.copy_text();
                    if self.sels.iter().all(|s| s.is_empty()) {
                        // Cut of a caret takes its lines.
                        let lines = self.touched_lines();
                        let parts = merge_ranges(lines.iter().map(|&l| {
                            let a = self.text.line_to_char(l);
                            let b = if l + 1 < self.text.len_lines() { self.text.line_to_char(l + 1) } else { self.text.len_chars() };
                            (a..b, String::new())
                        }).collect());
                        self.coalesce = false;
                        self.apply(parts, ChangeKind::Other, After::Carets);
                    } else {
                        self.coalesce = false;
                        let parts = self.sels.iter().map(|s| (s.range(), String::new())).collect();
                        self.apply(parts, ChangeKind::Other, After::Carets);
                    }
                    KeyOutcome::Copy(t)
                }
                "v" => KeyOutcome::Paste,
                "z" if m.shift => changed(self.redo()),
                "z" => changed(self.undo()),
                "y" => changed(self.redo()),
                "d" => changed(self.select_next()),
                "/" => changed(self.toggle_comment()),
                "]" => changed(self.indent()),
                "[" => changed(self.outdent()),
                _ => KeyOutcome::Ignored,
            },
            Key::Char if m.alt => KeyOutcome::Ignored,
            Key::Char => changed(self.insert(chars)),
            Key::Enter => changed(self.newline()),
            Key::Tab if m.shift => changed(self.outdent()),
            Key::Tab => changed(self.indent()),
            Key::Backspace => changed(self.backspace(cmd || m.alt)),
            Key::Delete => changed(self.delete(cmd || m.alt)),
            Key::Escape => changed(self.single()),
            Key::Left => moved(self, if cmd || m.alt { Motion::WordLeft } else { Motion::Left }),
            Key::Right => moved(self, if cmd || m.alt { Motion::WordRight } else { Motion::Right }),
            Key::Up if cmd && m.alt => { self.add_cursor_vertical(true); KeyOutcome::Changed }
            Key::Down if cmd && m.alt => { self.add_cursor_vertical(false); KeyOutcome::Changed }
            Key::Up => moved(self, Motion::Up),
            Key::Down => moved(self, Motion::Down),
            Key::Home => moved(self, if cmd { Motion::DocStart } else { Motion::LineStart }),
            Key::End => moved(self, if cmd { Motion::DocEnd } else { Motion::LineEnd }),
            Key::PageUp => moved(self, Motion::PageUp(page)),
            Key::PageDown => moved(self, Motion::PageDown(page)),
            Key::F(_) => KeyOutcome::Ignored,
        }
    }
}

// ---- helpers ---------------------------------------------------------------

/// The display column after `c` at column `d`.
pub fn advance(d: usize, c: char, tab: usize) -> usize {
    match c {
        '\t' => (d / tab + 1) * tab,
        '\n' | '\r' => d,
        c => d + c.width().unwrap_or(0).max(if (c as u32) < 0x20 { 1 } else { 0 }),
    }
}

/// Where old position `p` lands after `parts` (old-text ranges, sorted,
/// not overlapping). Inside a replaced range, or at an insert, it goes to
/// the end of the replacement; at a replaced range's start it stays there.
fn map_pos(parts: &[(Range<usize>, String)], p: usize) -> usize {
    let mut shift: isize = 0;
    for (r, s) in parts {
        let n = s.chars().count() as isize;
        if p < r.start {
            break;
        }
        if r.is_empty() && p == r.start {
            return (p as isize + shift + n) as usize;
        }
        if p == r.start {
            return (p as isize + shift) as usize;
        }
        if p <= r.end {
            return (r.start as isize + shift + n) as usize;
        }
        shift += n - (r.end - r.start) as isize;
    }
    (p as isize + shift) as usize
}

/// [`map_pos`] for a position that stays *before* an insert made at it.
fn map_before(parts: &[(Range<usize>, String)], p: usize) -> usize {
    let mut shift: isize = 0;
    for (r, s) in parts {
        if p <= r.start {
            break;
        }
        shift += s.chars().count() as isize - (r.end - r.start) as isize;
    }
    (p as isize + shift) as usize
}

/// Carets at the end of each replacement (typing, deleting, pasting).
fn carets_after(parts: &[(Range<usize>, String)]) -> Vec<Selection> {
    let mut shift: isize = 0;
    parts.iter().map(|(r, s)| {
        let n = s.chars().count() as isize;
        let at = (r.start as isize + shift + n) as usize;
        shift += n - (r.end - r.start) as isize;
        Selection::caret(at)
    }).collect()
}

/// Sort and merge overlapping deletion ranges (two carets deleting into
/// each other).
fn merge_ranges(mut parts: Vec<(Range<usize>, String)>) -> Vec<(Range<usize>, String)> {
    parts.sort_by_key(|(r, _)| r.start);
    let mut out: Vec<(Range<usize>, String)> = Vec::with_capacity(parts.len());
    for (r, s) in parts {
        if let Some((last, ls)) = out.last_mut() {
            if r.start <= last.end && s.is_empty() && ls.is_empty() {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        out.push((r, s));
    }
    out
}

#[cfg(test)]
mod tests;
