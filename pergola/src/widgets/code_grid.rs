//! CodeGrid: the code editor's view of a [`CodeBuffer`] (dev-tools.md
//! §3.4, plan M11, P4). Monospace lines on a cell grid: a gutter with line
//! numbers, text in runs of one syntax tag placed by display column, the
//! selections, a caret per selection, the current line. Only the lines in
//! view are drawn, so a long file costs what a screenful costs.
//!
//! ASCII shares a run (the face is monospaced); any other character is a
//! run of its own on its cell, as the Terminal does, since fallback glyphs
//! need not have the cell's advance. Bidi and shaping for the rare
//! right-to-left string in code are left to a later pass (shared with
//! Officina's lines).
//!
//! Keys go to [`CodeBuffer::key`]; copy and paste go through the clipboard
//! like a [`TextField`](super::TextField); a press places the caret and a
//! drag selects; the wheel scrolls. The primary caret stays in view.

use std::rc::Rc;

use crate::code::{CodeBuffer, Highlight, Span, Tag};
use crate::color::Color;
use crate::event::{Event, Key, KeyEventKind, Modifiers};
use crate::geom::{Point, Rect};
use crate::interaction::{DragPhase, ScrollKind};
use crate::node::{Node, TextStyle};
use crate::reactive::Mutable;
use crate::text_edit::KeyOutcome;
use crate::theme::{font, Semantic, Weight};
use crate::view::{Ctx, View};

/// Where the view is scrolled: the first line shown and the first display
/// column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeView {
    pub top: usize,
    pub left: usize,
}

pub struct CodeGrid {
    buffer: Mutable<CodeBuffer>,
    view: Mutable<CodeView>,
    highlight: Rc<dyn Highlight>,
    rect: Rect,
    size: f32,
    gutter: bool,
    autofocus: bool,
    /// Lines with a diagnostic, and its severity (0 error, 1 warning, 2 note).
    marks: Vec<(usize, u8)>,
    /// Keys the buffer doesn't use (Ctrl+S, Ctrl+P, F12…), for the app.
    other_key: Option<OtherKey>,
    /// Lines with a breakpoint (a dot in the gutter).
    breakpoints: Vec<usize>,
    /// The line a debugger stopped at (shaded).
    stopped: Option<usize>,
    /// A press in the gutter, with its line.
    gutter_click: Option<Rc<dyn Fn(usize)>>,
    /// Breakpoints switched off (a ring in the gutter).
    off_breakpoints: Vec<usize>,
    /// A note under a span of text: line, its first column (a char index
    /// of the line), the text (a debugger's value for a name).
    note: Option<(usize, usize, String)>,
    /// The pointer over the text: its line and char index in the line, or
    /// `None` off the text.
    on_point: Option<Rc<dyn Fn(Option<(usize, usize)>)>>,
}

/// What [`CodeGrid::on_other_key`] gets: the key, its modifiers, its text.
pub type OtherKey = Rc<dyn Fn(Key, Modifiers, &str)>;

/// A run of one tag on a line, at display column `col`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub col: usize,
    pub cells: usize,
    pub text: String,
    pub tag: Tag,
}

impl CodeGrid {
    pub fn new(buffer: Mutable<CodeBuffer>, view: Mutable<CodeView>) -> Self {
        CodeGrid {
            buffer, view,
            highlight: Rc::new(crate::code::highlight::Plain),
            rect: Rect::new(0.0, 0.0, 600.0, 400.0),
            size: 13.0,
            gutter: true,
            autofocus: false,
            marks: Vec::new(),
            other_key: None,
            breakpoints: Vec::new(),
            stopped: None,
            off_breakpoints: Vec::new(),
            note: None,
            on_point: None,
            gutter_click: None,
        }
    }
    pub fn highlight(mut self, h: Rc<dyn Highlight>) -> Self { self.highlight = h; self }
    pub fn rect(mut self, r: Rect) -> Self { self.rect = r; self }
    /// The text size, in points.
    pub fn size(mut self, s: f32) -> Self { self.size = s; self }
    pub fn gutter(mut self, on: bool) -> Self { self.gutter = on; self }
    pub fn autofocus(mut self, on: bool) -> Self { self.autofocus = on; self }
    pub fn marks(mut self, m: Vec<(usize, u8)>) -> Self { self.marks = m; self }
    /// Keys the editor has no use for go to `f` (the app's shortcuts).
    pub fn on_other_key(mut self, f: OtherKey) -> Self { self.other_key = Some(f); self }
    pub fn breakpoints(mut self, lines: Vec<usize>) -> Self { self.breakpoints = lines; self }
    pub fn stopped(mut self, line: Option<usize>) -> Self { self.stopped = line; self }
    pub fn off_breakpoints(mut self, lines: Vec<usize>) -> Self { self.off_breakpoints = lines; self }
    pub fn note(mut self, n: Option<(usize, usize, String)>) -> Self { self.note = n; self }
    pub fn on_point(mut self, f: Rc<dyn Fn(Option<(usize, usize)>)>) -> Self { self.on_point = Some(f); self }
    /// A press in the gutter calls `f` with its line (toggle a breakpoint).
    pub fn on_gutter_click(mut self, f: Rc<dyn Fn(usize)>) -> Self { self.gutter_click = Some(f); self }

    fn style(&self, color: Color) -> TextStyle {
        TextStyle { family: font::MONO.into(), size: self.size, weight: Weight::Regular, color }
    }
}

/// The grid's metrics: cell width, line height, gutter width (points).
#[derive(Debug, Clone, Copy)]
struct Metrics {
    cell: f32,
    line: f32,
    gutter: f32,
    pad: f32,
}

impl Metrics {
    fn rows(&self, r: Rect) -> usize {
        ((r.h() - 2.0 * self.pad) / self.line).floor().max(1.0) as usize
    }
    fn cols(&self, r: Rect) -> usize {
        ((r.w() - self.gutter - 2.0 * self.pad) / self.cell).floor().max(1.0) as usize
    }
}

/// The colour of a tag in `theme`.
pub fn tag_color(theme: &Semantic, tag: Tag) -> Color {
    match tag {
        Tag::Keyword => theme.accent_fg(),
        Tag::Type => theme.info_fg(),
        Tag::Function => theme.warning_fg(),
        Tag::String => theme.success_fg(),
        Tag::Number | Tag::Constant | Tag::Error => theme.danger_fg(),
        Tag::Comment => theme.text_tertiary(),
        Tag::Macro | Tag::Attribute => theme.accent_strong(),
        Tag::Operator | Tag::Punctuation => theme.text_secondary(),
        Tag::Plain | Tag::Variable | Tag::Property => theme.text_primary(),
    }
}

/// Lay out `line` (its chars) with `spans` into runs by display column,
/// tabs expanded to spaces, keeping only what falls in columns
/// `left..left + width`.
pub fn layout_line(chars: &[char], spans: &[Span], tab: usize, left: usize, width: usize) -> Vec<Run> {
    let tag_at = |i: usize| spans.iter().find(|s| s.cols.contains(&i)).map_or(Tag::Plain, |s| s.tag);
    let right = left + width;
    let mut out: Vec<Run> = Vec::new();
    let mut d = 0usize;
    for (i, &c) in chars.iter().enumerate() {
        let next = crate::code::buffer::advance(d, c, tab);
        let cells = next - d;
        if cells == 0 || next <= left {
            d = next;
            continue;
        }
        if d >= right {
            break;
        }
        let tag = tag_at(i);
        let (text, ascii) = if c == '\t' { (" ".repeat(cells), true) } else if (c as u32) < 0x20 { ("?".to_string(), true) } else { (c.to_string(), c.is_ascii()) };
        // A character cut by the left edge shows as blanks.
        let (col, text) = if d < left { (left, " ".repeat(next - left)) } else { (d, text) };
        match out.last_mut() {
            Some(r) if ascii && r.tag == tag && r.text.is_ascii() && r.col + r.cells == col => {
                r.text.push_str(&text);
                r.cells += next - col;
            }
            _ => out.push(Run { col, cells: next - col, text, tag }),
        }
        d = next;
    }
    // Blanks draw as nothing: trim them off each run's ends.
    out.retain(|r| !r.text.trim().is_empty());
    for r in &mut out {
        let lead = r.text.len() - r.text.trim_start_matches(' ').len();
        let trail = r.text.len() - r.text.trim_end_matches(' ').len();
        r.col += lead;
        r.cells -= lead + trail;
        r.text = r.text[lead..r.text.len() - trail].to_string();
    }
    out
}

/// The view moved so the primary caret shows, `rows` x `cols` cells.
pub fn keep_in_view(b: &CodeBuffer, v: CodeView, rows: usize, cols: usize) -> CodeView {
    let (line, col) = b.line_col(b.primary().head);
    let d = b.display_col(line, col);
    let mut v = v;
    if line < v.top {
        v.top = line;
    } else if line >= v.top + rows {
        v.top = line + 1 - rows;
    }
    let margin = 4.min(cols / 4);
    if d < v.left + margin {
        v.left = d.saturating_sub(margin);
    } else if d + margin >= v.left + cols {
        v.left = d + margin + 1 - cols;
    }
    v
}

impl View for CodeGrid {
    #[cfg_attr(debug_assertions, track_caller)]
    fn render(&self, ctx: &mut Ctx) {
        ctx.widget_site();
        let theme = ctx.theme;
        let b = self.buffer.get_cloned();
        let v = self.view.get_cloned();
        let r = self.rect;
        let digits = b.len_lines().to_string().len().max(3);
        let cell = Node::measure_text("M", &self.style(theme.text_primary())).w.max(1.0);
        let line_h = (Node::measure_text("Mg", &self.style(theme.text_primary())).h * 1.15).round().max(1.0);
        let m = Metrics { cell, line: line_h, gutter: if self.gutter { (digits as f32 + 2.0) * cell } else { 0.0 }, pad: 4.0 };
        let (rows, cols) = (m.rows(r), m.cols(r));
        let text_x = r.x() + m.gutter + m.pad;
        let y_of = |row: usize| r.y() + m.pad + row as f32 * line_h;
        let x_of = |dcol: usize| text_x + (dcol as f32 - v.left as f32) * cell;

        let bg = ctx.add(Node::Rect { rect: r, fill: theme.bg_canvas(), radius: 0.0 });
        let focused = ctx.is_focused(bg);
        if self.gutter {
            ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(r.x(), r.y(), m.gutter, r.h()), fill: theme.bg_surface(), radius: 0.0 });
        }
        let primary_line = b.line_col(b.primary().head).0;
        let last = (v.top + rows).min(b.len_lines());
        for line in v.top..last {
            let row = line - v.top;
            let y = y_of(row);
            if self.stopped == Some(line) {
                ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(r.x() + m.gutter, y, r.w() - m.gutter, line_h), fill: theme.accent_bg(), radius: 0.0 });
            } else if line == primary_line && b.selections().len() == 1 && b.primary().is_empty() {
                ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(r.x() + m.gutter, y, r.w() - m.gutter, line_h), fill: theme.state_hover(), radius: 0.0 });
            }
            // Selections on this line.
            let line_start = b.text().line_to_char(line);
            let line_end = line_start + b.line_len(line);
            for s in b.selections().iter().filter(|s| !s.is_empty()) {
                let sr = s.range();
                if sr.end < line_start || sr.start > line_end || (sr.end == line_start && line > 0) {
                    continue;
                }
                let a = b.display_col(line, sr.start.max(line_start) - line_start);
                let past_end = sr.end > line_end;
                let e = b.display_col(line, sr.end.min(line_end) - line_start) + past_end as usize;
                let (xa, xe) = (x_of(a).max(text_x), x_of(e).max(text_x));
                if xe > xa {
                    ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(xa, y, xe - xa, line_h),
                        fill: if focused { theme.selection_bg() } else { theme.selection_idle() }, radius: 0.0 });
                }
            }
            // Gutter: the number, and a diagnostic mark.
            if self.gutter {
                let n = format!("{:>w$}", line + 1, w = digits);
                let color = if line == primary_line { theme.text_secondary() } else { theme.text_tertiary() };
                let st = self.style(color);
                let sz = Node::measure_text(&n, &st);
                ctx.tree.insert(Some(bg), Node::Text { rect: Rect::new(r.x() + cell, y, sz.w, sz.h), content: n, style: st });
                if let Some((_, sev)) = self.marks.iter().find(|(l, _)| *l == line) {
                    let c = match sev { 0 => theme.danger_fg(), 1 => theme.warning_fg(), _ => theme.info_fg() };
                    ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(r.x() + 2.0, y + 2.0, 3.0, line_h - 4.0), fill: c, radius: 1.5 });
                }
                let d = (line_h * 0.55).min(m.gutter * 0.4);
                let dot = Rect::new(r.x() + m.gutter - d - 3.0, y + (line_h - d) / 2.0, d, d);
                if self.off_breakpoints.contains(&line) {
                    // Off: a ring.
                    ctx.tree.insert(Some(bg), Node::Rect { rect: dot, fill: theme.danger_fg(), radius: d / 2.0 });
                    ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(dot.x() + 1.5, dot.y() + 1.5, d - 3.0, d - 3.0), fill: theme.bg_surface(), radius: d / 2.0 - 1.5 });
                } else if self.breakpoints.contains(&line) {
                    ctx.tree.insert(Some(bg), Node::Rect { rect: dot, fill: theme.danger_fg(), radius: d / 2.0 });
                }
                if self.stopped == Some(line) {
                    // Where it stopped: an arrow (a bar and a point) left of the number.
                    let (ax, ah) = (r.x() + 2.0, (line_h * 0.5).max(4.0));
                    let ay = y + (line_h - ah) / 2.0;
                    ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(ax, ay + ah * 0.3, ah * 0.5, ah * 0.4), fill: theme.accent_fg(), radius: 0.0 });
                    for k in 0..4 {
                        let f = k as f32 / 4.0;
                        ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(ax + ah * 0.5 + f * ah * 0.5, ay + f * ah * 0.5, ah * 0.5 / 4.0 + 0.5, ah * (1.0 - f)), fill: theme.accent_fg(), radius: 0.0 });
                    }
                }
            }
            // Text.
            let chars: Vec<char> = b.text().line(line).chars().take(b.line_len(line)).collect();
            let spans = self.highlight.line(line);
            for run in layout_line(&chars, &spans, b.tab_width, v.left, cols) {
                let st = self.style(tag_color(&theme, run.tag));
                let w = run.cells as f32 * cell;
                ctx.tree.insert(Some(bg), Node::Text { rect: Rect::new(x_of(run.col), y, w, line_h), content: run.text, style: st });
            }
        }
        // Carets, over the text.
        if focused {
            for s in b.selections() {
                let (line, col) = b.line_col(s.head);
                if line < v.top || line >= last {
                    continue;
                }
                let x = x_of(b.display_col(line, col));
                if x >= text_x {
                    ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(x, y_of(line - v.top), 2.0, line_h), fill: theme.accent_fg(), radius: 0.0 });
                }
            }
        }

        // The note, under its span: a box of one line.
        if let Some((line, col, text)) = self.note.as_ref().filter(|n| n.0 >= v.top && n.0 < last) {
            let dcol = b.display_col(*line, *col);
            let st = self.style(theme.text_primary());
            let w = Node::measure_text(text, &st).w + 2.0 * m.pad + 4.0;
            let x = x_of(dcol).max(text_x).min(r.x() + r.w() - w).max(r.x());
            let below = y_of(line - v.top) + line_h + 2.0;
            let y = if below + line_h + 6.0 > r.y() + r.h() { y_of(line - v.top) - line_h - 8.0 } else { below };
            let bx = Rect::new(x, y, w, line_h + 6.0);
            ctx.tree.insert(Some(bg), Node::Rect { rect: bx, fill: theme.border_strong(), radius: 4.0 });
            ctx.tree.insert(Some(bg), Node::Rect { rect: Rect::new(x + 1.0, y + 1.0, w - 2.0, line_h + 4.0), fill: theme.bg_elevated(), radius: 3.0 });
            let sz = Node::measure_text(text, &st);
            ctx.tree.insert(Some(bg), Node::Text { rect: Rect::new(x + m.pad + 2.0, y + 3.0, sz.w, sz.h), content: text.clone(), style: st });
        }

        ctx.focusable(bg);
        ctx.takes_tab(bg);
        if self.autofocus {
            ctx.autofocus(bg);
        }
        // The char index under a point (relative to the grid's top-left).
        let hit = move |b: &CodeBuffer, v: CodeView, p: Point| -> usize {
            let row = ((p.y - m.pad) / m.line).floor().max(0.0) as usize;
            let line = (v.top + row).min(b.len_lines() - 1);
            let d = ((p.x - m.gutter - m.pad) / m.cell + 0.5).floor().max(0.0) as usize + v.left;
            b.text().line_to_char(line) + b.col_at_display(line, d)
        };

        let (buffer, view) = (self.buffer.clone(), self.view.clone());
        let other = self.other_key.clone();
        ctx.on_key(bg, move |ev| {
            let Event::Key { kind, key, chars, modifiers } = ev else { return };
            if !kind.pressed() || *kind == KeyEventKind::Up {
                return;
            }
            let mut b = buffer.get_cloned();
            let out = b.key(*key, *modifiers, chars, rows.saturating_sub(1));
            if matches!(out, KeyOutcome::Ignored) {
                if let Some(f) = &other {
                    f(*key, *modifiers, chars);
                }
                return;
            }
            let changed = match out {
                KeyOutcome::Changed => true,
                KeyOutcome::Copy(t) => crate::clipboard::copy_text(&t, false).is_ok(),
                KeyOutcome::Paste => matches!(crate::clipboard::paste_text(), Ok(Some(t)) if b.insert(&t)),
                _ => false,
            };
            if changed {
                view.set(keep_in_view(&b, view.get_cloned(), rows, cols));
                buffer.set(b);
            }
        });
        if let Some(f) = self.on_point.clone() {
            let (buffer, view) = (self.buffer.clone(), self.view.clone());
            ctx.on_pointer_local(bg, move |p| {
                let at = p.filter(|p| p.x >= m.gutter + m.pad).and_then(|p| {
                    let b = buffer.get_cloned();
                    let v = view.get_cloned();
                    let line = v.top + ((p.y - m.pad) / m.line).floor().max(0.0) as usize;
                    if line >= b.len_lines() {
                        return None;
                    }
                    let d = ((p.x - m.gutter - m.pad) / m.cell).floor().max(0.0) as usize + v.left;
                    let c = b.col_at_display(line, d);
                    (c < b.line_len(line)).then_some((line, c))
                });
                f(at);
            });
        }
        let (buffer, view) = (self.buffer.clone(), self.view.clone());
        let gutter_click = self.gutter_click.clone();
        // A press that began in the gutter selects nothing as it moves.
        let in_gutter = Rc::new(std::cell::Cell::new(false));
        ctx.on_drag_local(bg, move |phase, p| {
            let mut b = buffer.get_cloned();
            let v = view.get_cloned();
            if phase != DragPhase::Start && in_gutter.get() {
                return;
            }
            in_gutter.set(false);
            if phase == DragPhase::Start && p.x < m.gutter {
                if let Some(f) = &gutter_click {
                    in_gutter.set(true);
                    let row = ((p.y - m.pad) / m.line).floor().max(0.0) as usize;
                    let line = v.top + row;
                    if line < b.len_lines() {
                        f(line);
                    }
                    return;
                }
            }
            let at = hit(&b, v, p);
            match phase {
                DragPhase::Start => b.set_caret(at, false),
                DragPhase::Move | DragPhase::End => b.set_caret(at, true),
            }
            if phase == DragPhase::Move {
                view.set(keep_in_view(&b, v, rows, cols));
            }
            buffer.set(b);
        });
        let (buffer, view) = (self.buffer.clone(), self.view.clone());
        ctx.on_scroll(bg, move |input| {
            let ScrollKind::Wheel { dx, dy, .. } = input.kind else { return };
            let b = buffer.get_cloned();
            let mut v = view.get_cloned();
            let lines = (dy / line_h).round() as isize;
            let cells = (dx / cell).round() as isize;
            let max_top = b.len_lines().saturating_sub(1);
            v.top = (v.top as isize + lines).clamp(0, max_top as isize) as usize;
            v.left = (v.left as isize + cells).max(0) as usize;
            view.set(v);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::Span;

    fn chars(s: &str) -> Vec<char> { s.chars().collect() }

    #[test]
    fn runs_by_tag_and_column() {
        let spans = vec![Span { cols: 0..2, tag: Tag::Keyword }, Span { cols: 3..7, tag: Tag::Function }];
        let r = layout_line(&chars("fn main()"), &spans, 4, 0, 80);
        let got: Vec<(usize, &str, Tag)> = r.iter().map(|r| (r.col, r.text.as_str(), r.tag)).collect();
        assert_eq!(got, vec![(0, "fn", Tag::Keyword), (3, "main", Tag::Function), (7, "()", Tag::Plain)]);
    }

    #[test]
    fn tabs_expand_and_non_ascii_stands_alone() {
        let r = layout_line(&chars("\tx漢y"), &[], 4, 0, 80);
        let got: Vec<(usize, usize, &str)> = r.iter().map(|r| (r.col, r.cells, r.text.as_str())).collect();
        assert_eq!(got, vec![(4, 1, "x"), (5, 2, "漢"), (7, 1, "y")]);
    }

    #[test]
    fn horizontal_scroll_clips() {
        let r = layout_line(&chars("abcdefghij"), &[], 4, 3, 4);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].col, r[0].text.as_str()), (3, "defg"));
        // A wide char cut by the left edge leaves a blank, not half a glyph.
        let r = layout_line(&chars("漢abc"), &[], 4, 1, 10);
        assert_eq!((r[0].col, r[0].text.as_str()), (2, "abc"));
    }

    #[test]
    fn caret_kept_in_view() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut b = CodeBuffer::new(&text);
        b.set_caret(b.at_line_col(50, 0), false);
        let v = keep_in_view(&b, CodeView::default(), 20, 80);
        assert_eq!(v.top, 31);
        b.set_caret(b.at_line_col(10, 0), false);
        let v = keep_in_view(&b, v, 20, 80);
        assert_eq!(v.top, 10);
        let mut b = CodeBuffer::new(&"x".repeat(200));
        b.set_caret(150, false);
        let v = keep_in_view(&b, CodeView::default(), 20, 80);
        assert!(v.left + 80 > 150 && v.left <= 150);
    }

    #[test]
    fn renders_visible_lines_only() {
        let text: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let grid = CodeGrid::new(Mutable::new(CodeBuffer::new(&text)), Mutable::new(CodeView { top: 500, left: 0 }))
            .rect(Rect::new(0.0, 0.0, 400.0, 200.0));
        let (tree, _) = crate::view::render(&grid, Semantic::LIGHT);
        let texts: Vec<String> = tree.iter().filter_map(|(_, n)| match n { Node::Text { content, .. } => Some(content.clone()), _ => None }).collect();
        assert!(texts.iter().any(|t| t == "line 500"));
        assert!(!texts.iter().any(|t| t == "line 0" || t == "line 999"));
        assert!(texts.len() < 60, "a screenful, not the file: {}", texts.len());
    }
}
