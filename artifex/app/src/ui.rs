//! Artifex's window (dev-tools.md §3.4): a bar with the workspace and its
//! tasks, the file tree, tabs over the code grid, a panel with the task's
//! output and the problems, and a status line. Clicks and keys become
//! [`Act`]s the loop (`main.rs`) carries out; this file only draws [`State`].
//!
//! Sizes: layout in `em` (grows with the text size).

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use artifex_syntax::LineSpans;
use pergola::code::CodeBuffer;
use pergola::event::{Key, Modifiers};
use pergola::geom::Rect;
use pergola::interaction::ScrollKind;
use pergola::node::Node;
use pergola::theme::{font, radius, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Button, CodeGrid, CodeView};
use pergola::{Color, Mutable, TextStyle};
use portcullis_ipc::WorkspaceInfo;

use crate::git;
use crate::tasks::{Problem, Severity, Task};
use crate::tree::Row;

pub fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

/// What a click or a key asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Show the workspace chooser.
    Choose,
    CloseChooser,
    /// Pick a workspace (a press: portcullisd checks it with frescod).
    Pick(String, [u8; 16]),
    /// Switch to a workspace picked before.
    Use(String),
    Toggle(String),
    Open(String),
    Tab(usize),
    CloseTab(usize),
    Save,
    Task(usize),
    Stop,
    Panel(Panel),
    HidePanel,
    Problem(usize),
    Definition,
    Hover,
    Terminal([u8; 16]),
    Refresh,
    /// Next or previous tab.
    CycleTab(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Panel {
    #[default]
    Output,
    Problems,
}

/// A tab as the bar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub rel: String,
    pub title: String,
    pub dirty: bool,
}

/// The editing state of the active file, shared with the code grid.
#[derive(Clone)]
pub struct DocCells {
    pub path: PathBuf,
    pub buffer: Mutable<CodeBuffer>,
    pub view: Mutable<CodeView>,
    pub spans: Mutable<Rc<LineSpans>>,
    /// Gutter marks: line, 0 error / 1 warning / 2 note.
    pub marks: Mutable<Vec<(usize, u8)>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub workspaces: Vec<WorkspaceInfo>,
    pub listed: bool,
    /// Picked before (mounted at `/work/<name>`).
    pub picked: Vec<String>,
    pub ws: Option<String>,
    pub choosing: bool,
    pub rows: Vec<Row>,
    pub tree_top: usize,
    pub tabs: Vec<Tab>,
    pub active: Option<usize>,
    pub tasks: Vec<Task>,
    pub running: Option<String>,
    pub output: String,
    pub problems: Vec<Problem>,
    pub panel: Panel,
    pub panel_open: bool,
    pub git: Option<git::Status>,
    /// The language server's state, for the status line.
    pub lsp: Option<String>,
    /// Problems the language server has with the active file: line, severity, text.
    pub diags: Vec<(usize, Severity, String)>,
    pub hover: Option<String>,
    pub lang: Option<&'static str>,
    /// Line and column of the caret, 1-based.
    pub caret: (usize, usize),
    pub note: Option<(String, bool)>,
}

pub type Queue = Arc<Mutex<Vec<Act>>>;

pub fn push(q: &Queue, a: Act) {
    q.lock().unwrap_or_else(|p| p.into_inner()).push(a);
}

fn plain(q: &Queue, a: Act) -> impl Fn() + Send + Sync + 'static {
    let q = q.clone();
    move || push(&q, a.clone())
}

fn pressed(q: &Queue, f: impl Fn([u8; 16]) -> Act + Send + Sync + 'static) -> impl Fn() + Send + Sync + 'static {
    let q = q.clone();
    move || {
        let token = pergola::clipboard::current_activation().unwrap_or([0; 16]);
        push(&q, f(token));
    }
}

/// The shortcut a key is, when the editor had no use for it.
pub fn shortcut(key: Key, m: Modifiers, chars: &str) -> Option<Act> {
    let c = chars.chars().next().map(|c| c.to_ascii_lowercase());
    match (key, m.ctrl, m.shift) {
        (Key::Char, true, false) if c == Some('s') => Some(Act::Save),
        (Key::Char, true, false) if c == Some('b') => Some(Act::Task(0)),
        (Key::Char, true, false) if c == Some('w') => Some(Act::CloseTab(usize::MAX)),
        (Key::Char, true, false) if c == Some('j') => Some(Act::Panel(Panel::Output)),
        (Key::Char, true, true) if c == Some('m') => Some(Act::Panel(Panel::Problems)),
        (Key::Char, true, false) if c == Some('k') => Some(Act::Hover),
        (Key::PageDown, true, _) => Some(Act::CycleTab(true)),
        (Key::PageUp, true, _) => Some(Act::CycleTab(false)),
        (Key::F(12), _, _) => Some(Act::Definition),
        (Key::Escape, false, false) => Some(Act::HidePanel),
        _ => None,
    }
}

pub struct ArtifexView {
    pub state: Mutable<State>,
    pub doc: Mutable<Option<DocCells>>,
    pub size: Mutable<(f32, f32)>,
    pub queue: Queue,
}

fn style(family: &str, size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: family.into(), size, weight, color }
}

fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: style(font::SANS, size, weight, color) });
}

fn mono(ctx: &mut Ctx, x: f32, cy: f32, s: &str, color: Color) {
    let h = em(type_size::SM) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: style(font::MONO, type_size::SM, Weight::Regular, color) });
}

/// `s` cut to fit `width` points of text at `size`.
fn fit(s: &str, width: f32, size: f32) -> String {
    let per = em(size) * 0.62;
    let n = (width / per).floor().max(1.0) as usize;
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

pub fn bar_h() -> f32 { em(44.0) }
pub fn side_w() -> f32 { em(240.0) }
pub fn tabs_h() -> f32 { em(32.0) }
pub fn status_h() -> f32 { em(24.0) }
pub fn panel_h() -> f32 { em(200.0) }
pub fn row_h() -> f32 { em(22.0) }

/// Where the code grid goes in a window `w`×`h`.
pub fn editor_rect(w: f32, h: f32, panel: bool) -> Rect {
    let top = bar_h() + tabs_h();
    let bottom = h - status_h() - if panel { panel_h() } else { 0.0 };
    Rect::new(side_w(), top, (w - side_w()).max(em(100.0)), (bottom - top).max(em(60.0)))
}

impl View for ArtifexView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let s = self.state.get_cloned();
        let q = &self.queue;
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });

        // The bar: workspace, tasks, terminal, branch.
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, bar_h()), fill: t.bg_surface(), radius: 0.0 });
        let mid = bar_h() / 2.0;
        let ws_label = s.ws.as_deref().map(crate::title).unwrap_or_else(|| "Choose a workspace".into());
        ctx.key(10);
        Button::ghost(ws_label).at(em(8.0), mid - em(16.0)).width(side_w() - em(16.0)).on_click(plain(q, Act::Choose)).render(ctx);
        let mut x = side_w() + em(8.0);
        for (i, task) in s.tasks.iter().enumerate() {
            ctx.key(20 + i as u64);
            let bw = em(14.0 + 8.0 * task.label.len() as f32);
            if s.running.is_some() {
                Button::secondary(task.label.clone()).at(x, mid - em(16.0)).width(bw).render(ctx);
            } else {
                Button::secondary(task.label.clone()).at(x, mid - em(16.0)).width(bw).on_click(plain(q, Act::Task(i))).render(ctx);
            }
            x += bw + em(8.0);
        }
        if let Some(r) = &s.running {
            ctx.key(40);
            Button::primary("Stop").at(x, mid - em(16.0)).width(em(70.0)).on_click(plain(q, Act::Stop)).render(ctx);
            x += em(78.0);
            text(ctx, x, mid, &format!("{r}…"), type_size::SM, Weight::Regular, t.text_secondary());
        }
        if s.ws.is_some() {
            ctx.key(41);
            Button::ghost("Terminal").at(w - em(110.0), mid - em(16.0)).width(em(100.0)).on_click(pressed(q, Act::Terminal)).render(ctx);
            if let Some(g) = &s.git {
                if let Some(b) = &g.branch {
                    let mut label = format!("⎇ {b}");
                    if g.ahead > 0 {
                        label.push_str(&format!(" ↑{}", g.ahead));
                    }
                    if g.behind > 0 {
                        label.push_str(&format!(" ↓{}", g.behind));
                    }
                    text(ctx, w - em(120.0) - em(8.0 * label.chars().count() as f32), mid, &label, type_size::SM, Weight::Medium, t.text_secondary());
                }
            }
        }

        // The tree.
        let tree_top = bar_h();
        let tree_bottom = h - status_h();
        ctx.add(Node::Rect { rect: Rect::new(0.0, tree_top, side_w(), tree_bottom - tree_top), fill: t.bg_surface(), radius: 0.0 });
        let fit_rows = ((tree_bottom - tree_top - em(8.0)) / row_h()).floor().max(1.0) as usize;
        let top = s.tree_top.min(s.rows.len().saturating_sub(fit_rows));
        ctx.key(50);
        let area = ctx.push(Node::Rect { rect: Rect::new(0.0, tree_top, side_w(), tree_bottom - tree_top), fill: Color::TRANSPARENT, radius: 0.0 });
        let active_rel = s.active.and_then(|i| s.tabs.get(i)).map(|t| t.rel.clone());
        for (i, row) in s.rows.iter().enumerate().skip(top).take(fit_rows) {
            let y = tree_top + em(4.0) + (i - top) as f32 * row_h();
            let r = Rect::new(em(4.0), y, side_w() - em(8.0), row_h());
            ctx.key(1000 + i as u64);
            let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: radius::SM });
            let fill = if active_rel.as_deref() == Some(row.rel.as_str()) { t.selection_idle() } else if ctx.is_hovered(id) { t.state_hover() } else { Color::TRANSPARENT };
            if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(id) {
                *f = fill;
            }
            ctx.on_click(id, plain(q, if row.dir { Act::Toggle(row.rel.clone()) } else { Act::Open(row.rel.clone()) }));
            let indent = em(10.0) + row.depth as f32 * em(14.0);
            let icon = if row.dir { if row.open { "▾" } else { "▸" } } else { " " };
            text(ctx, r.x() + indent - em(8.0), y + row_h() / 2.0, icon, type_size::SM, Weight::Regular, t.text_tertiary());
            let change = s.git.as_ref().and_then(|g| {
                if row.dir {
                    g.dir_changed(&row.rel).then_some('•')
                } else {
                    g.files.get(&row.rel).map(|c| c.letter())
                }
            });
            let color = match change {
                Some('U') | Some('A') => t.success_fg(),
                Some('!') | Some('D') => t.danger_fg(),
                Some(_) => t.warning_fg(),
                None => t.text_primary(),
            };
            text(ctx, r.x() + indent + em(4.0), y + row_h() / 2.0, &fit(&row.name, side_w() - indent - em(36.0), type_size::SM), type_size::SM, Weight::Regular, color);
            if let Some(c) = change {
                text(ctx, side_w() - em(22.0), y + row_h() / 2.0, &c.to_string(), type_size::XS, Weight::Medium, color);
            }
            ctx.pop();
        }
        let st = self.state.clone();
        ctx.on_scroll(area, move |input| {
            let ScrollKind::Wheel { dy, .. } = input.kind else { return };
            let mut v = st.get_cloned();
            let n = (dy / row_h()).round() as isize;
            v.tree_top = (v.tree_top as isize + n).clamp(0, v.rows.len().saturating_sub(1) as isize) as usize;
            st.set(v);
        });
        ctx.pop();
        if s.ws.is_some() && s.rows.is_empty() {
            text(ctx, em(16.0), tree_top + em(20.0), "work is empty", type_size::SM, Weight::Regular, t.text_tertiary());
        }

        // Tabs.
        let ty = bar_h();
        let mut tx = side_w();
        for (i, tab) in s.tabs.iter().enumerate() {
            let label = format!("{}{}", tab.title, if tab.dirty { " •" } else { "" });
            let tw = em(36.0) + em(7.5 * label.chars().count() as f32);
            ctx.key(2000 + i as u64);
            let selected = s.active == Some(i);
            let id = ctx.push(Node::Rect { rect: Rect::new(tx, ty, tw, tabs_h()), fill: if selected { t.bg_canvas() } else { t.bg_surface() }, radius: 0.0 });
            ctx.on_click(id, plain(q, Act::Tab(i)));
            text(ctx, tx + em(10.0), ty + tabs_h() / 2.0, &label, type_size::SM, if selected { Weight::Medium } else { Weight::Regular },
                if selected { t.text_primary() } else { t.text_secondary() });
            ctx.key(1);
            let x_id = ctx.push(Node::Rect { rect: Rect::new(tx + tw - em(22.0), ty + em(7.0), em(18.0), em(18.0)), fill: Color::TRANSPARENT, radius: radius::SM });
            if ctx.is_hovered(x_id) {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(x_id) {
                    *fill = t.state_hover();
                }
            }
            ctx.on_click(x_id, plain(q, Act::CloseTab(i)));
            text(ctx, tx + tw - em(17.0), ty + tabs_h() / 2.0, "×", type_size::SM, Weight::Regular, t.text_tertiary());
            ctx.pop();
            ctx.pop();
            tx += tw + 1.0;
        }

        // The editor.
        let er = editor_rect(w, h, s.panel_open);
        if let Some(doc) = self.doc.get_cloned() {
            let q2 = q.clone();
            ctx.key(3000);
            CodeGrid::new(doc.buffer.clone(), doc.view.clone())
                .rect(er)
                .highlight(doc.spans.get_cloned())
                .marks(doc.marks.get_cloned())
                .autofocus(true)
                .on_other_key(Rc::new(move |k, m, c| {
                    if let Some(a) = shortcut(k, m, c) {
                        push(&q2, a);
                    }
                }))
                .render(ctx);
        } else if s.ws.is_some() {
            text(ctx, er.x() + em(24.0), er.y() + em(32.0), "Open a file from the tree.", type_size::MD, Weight::Regular, t.text_secondary());
            text(ctx, er.x() + em(24.0), er.y() + em(60.0), "Ctrl+S saves · Ctrl+B builds · F12 goes to a definition · Ctrl+J shows output", type_size::SM, Weight::Regular, t.text_tertiary());
        }
        if let Some(hv) = &s.hover {
            let lines: Vec<&str> = hv.lines().take(12).collect();
            let hh = em(12.0) + lines.len() as f32 * em(19.0);
            let hw = (er.w() - em(48.0)).min(em(640.0));
            ctx.key(3100);
            let id = ctx.push(Node::Rect { rect: Rect::new(er.x() + em(24.0), er.y() + er.h() - hh - em(12.0), hw, hh), fill: t.bg_elevated(), radius: radius::SM });
            ctx.on_click(id, plain(q, Act::HidePanel));
            for (i, l) in lines.iter().enumerate() {
                mono(ctx, er.x() + em(32.0), er.y() + er.h() - hh - em(12.0) + em(15.0) + i as f32 * em(19.0), &fit(l, hw - em(16.0), type_size::SM), t.text_primary());
            }
            ctx.pop();
        }

        // The panel: output or problems.
        if s.panel_open {
            let py = er.y() + er.h();
            ctx.add(Node::Rect { rect: Rect::new(side_w(), py, w - side_w(), panel_h()), fill: t.bg_surface(), radius: 0.0 });
            ctx.add(Node::Rect { rect: Rect::new(side_w(), py, w - side_w(), 1.0), fill: t.border_default(), radius: 0.0 });
            let mut px = side_w() + em(8.0);
            for (i, (p, label)) in [(Panel::Output, "Output".to_string()), (Panel::Problems, format!("Problems ({})", s.problems.len()))].into_iter().enumerate() {
                ctx.key(4000 + i as u64);
                let bw = em(24.0 + 7.5 * label.len() as f32);
                if s.panel == p {
                    Button::secondary(label).at(px, py + em(4.0)).width(bw).on_click(plain(q, Act::Panel(p))).render(ctx);
                } else {
                    Button::ghost(label).at(px, py + em(4.0)).width(bw).on_click(plain(q, Act::Panel(p))).render(ctx);
                }
                px += bw + em(6.0);
            }
            ctx.key(4010);
            Button::ghost("Hide").at(w - em(80.0), py + em(4.0)).width(em(70.0)).on_click(plain(q, Act::HidePanel)).render(ctx);
            let body_y = py + em(42.0);
            let lines_fit = ((panel_h() - em(46.0)) / em(18.0)).floor().max(1.0) as usize;
            let width = w - side_w() - em(24.0);
            match s.panel {
                Panel::Output => {
                    let all: Vec<&str> = s.output.lines().collect();
                    for (i, l) in all.iter().skip(all.len().saturating_sub(lines_fit)).enumerate() {
                        mono(ctx, side_w() + em(12.0), body_y + i as f32 * em(18.0), &fit(l, width, type_size::SM), t.text_primary());
                    }
                }
                Panel::Problems => {
                    if s.problems.is_empty() {
                        text(ctx, side_w() + em(12.0), body_y, "No problems from the last task.", type_size::SM, Weight::Regular, t.text_tertiary());
                    }
                    for (i, p) in s.problems.iter().take(lines_fit).enumerate() {
                        let y = body_y + i as f32 * em(18.0);
                        ctx.key(5000 + i as u64);
                        let id = ctx.push(Node::Rect { rect: Rect::new(side_w() + em(4.0), y - em(9.0), width, em(18.0)), fill: Color::TRANSPARENT, radius: 0.0 });
                        if ctx.is_hovered(id) {
                            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                                *fill = t.state_hover();
                            }
                        }
                        ctx.on_click(id, plain(q, Act::Problem(i)));
                        let c = match p.severity { Severity::Error => t.danger_fg(), Severity::Warning => t.warning_fg(), Severity::Note => t.info_fg() };
                        let place = format!("{}:{}:{}", p.file, p.line, p.col);
                        mono(ctx, side_w() + em(12.0), y, &place, c);
                        let off = em(8.0) * place.chars().count() as f32 + em(24.0);
                        text(ctx, side_w() + off, y, &fit(&p.message, width - off, type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
                        ctx.pop();
                    }
                }
            }
        }

        // The status line.
        let sy = h - status_h();
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, status_h()), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, 1.0), fill: t.border_default(), radius: 0.0 });
        let smid = sy + status_h() / 2.0;
        let mut left = String::new();
        if let Some((note, err)) = &s.note {
            text(ctx, em(10.0), smid, &fit(note, w * 0.5, type_size::XS), type_size::XS, Weight::Regular, if *err { t.danger_fg() } else { t.text_secondary() });
        } else if let Some(d) = s.diags.iter().find(|d| d.0 + 1 == s.caret.0) {
            let c = if d.1 == Severity::Error { t.danger_fg() } else { t.warning_fg() };
            text(ctx, em(10.0), smid, &fit(&d.2, w * 0.5, type_size::XS), type_size::XS, Weight::Regular, c);
        }
        if s.active.is_some() {
            let errors = s.diags.iter().filter(|d| d.1 == Severity::Error).count();
            let warnings = s.diags.len() - errors;
            left.push_str(&format!("Ln {}, Col {}", s.caret.0, s.caret.1));
            if let Some(l) = s.lang {
                left.push_str(&format!("  ·  {l}"));
            }
            if errors + warnings > 0 {
                left.push_str(&format!("  ·  {errors} errors, {warnings} warnings"));
            }
        }
        if let Some(l) = &s.lsp {
            left.push_str(&format!("  ·  {l}"));
        }
        let lw = em(6.5) * left.chars().count() as f32;
        text(ctx, w - lw - em(12.0), smid, &left, type_size::XS, Weight::Regular, t.text_secondary());

        // The workspace chooser, over everything.
        if s.choosing || (s.ws.is_none() && s.listed) {
            ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.scrim(), radius: 0.0 });
            let cw = em(420.0).min(w - em(32.0));
            let n = s.workspaces.len().max(1);
            let ch = em(110.0) + n as f32 * em(48.0);
            let (cx, cy) = ((w - cw) / 2.0, ((h - ch) / 2.0).max(em(16.0)));
            ctx.key(6000);
            ctx.push(Node::Rect { rect: Rect::new(cx, cy, cw, ch), fill: t.bg_elevated(), radius: radius::MD });
            text(ctx, cx + em(20.0), cy + em(28.0), "Open a workspace", type_size::LG, Weight::Semibold, t.text_primary());
            text(ctx, cx + em(20.0), cy + em(52.0), "Artifex edits its work and runs its tools there.", type_size::SM, Weight::Regular, t.text_secondary());
            if s.workspaces.is_empty() {
                text(ctx, cx + em(20.0), cy + em(90.0), "No workspaces yet: make one in Workspaces.", type_size::SM, Weight::Regular, t.text_tertiary());
            }
            for (i, ws) in s.workspaces.iter().enumerate() {
                let y = cy + em(76.0) + i as f32 * em(48.0);
                text(ctx, cx + em(20.0), y + em(18.0), &crate::title(&ws.name), type_size::MD, Weight::Medium, t.text_primary());
                ctx.key(6100 + i as u64);
                if s.picked.contains(&ws.name) {
                    Button::secondary("Open").at(cx + cw - em(110.0), y + em(2.0)).width(em(90.0)).on_click(plain(q, Act::Use(ws.name.clone()))).render(ctx);
                } else {
                    let name = ws.name.clone();
                    Button::primary("Allow").at(cx + cw - em(110.0), y + em(2.0)).width(em(90.0))
                        .on_click(pressed(q, move |t| Act::Pick(name.clone(), t))).render(ctx);
                }
            }
            if s.ws.is_some() {
                ctx.key(6900);
                Button::ghost("Cancel").at(cx + cw - em(110.0), cy + ch - em(40.0)).width(em(90.0)).on_click(plain(q, Act::CloseChooser)).render(ctx);
            }
            ctx.pop();
        } else if !s.listed {
            text(ctx, side_w() + em(24.0), bar_h() + em(48.0), "Asking for your workspaces…", type_size::SM, Weight::Regular, t.text_secondary());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, shift: bool) -> Modifiers {
        Modifiers { ctrl, shift, ..Modifiers::default() }
    }

    #[test]
    fn shortcuts() {
        assert_eq!(shortcut(Key::Char, m(true, false), "s"), Some(Act::Save));
        assert_eq!(shortcut(Key::Char, m(true, false), "S"), Some(Act::Save));
        assert_eq!(shortcut(Key::Char, m(true, true), "M"), Some(Act::Panel(Panel::Problems)));
        assert_eq!(shortcut(Key::F(12), m(false, false), ""), Some(Act::Definition));
        assert_eq!(shortcut(Key::Char, m(false, false), "s"), None);
    }

    #[test]
    fn fitting() {
        assert_eq!(fit("short", 1000.0, 13.0), "short");
        assert!(fit(&"x".repeat(500), 100.0, 13.0).ends_with('…'));
    }
}
