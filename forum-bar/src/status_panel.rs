//! Status panels (docs/spec/forum-status-items.md §3): an app describes a
//! panel (text, headings, groups, rows and columns, controls) and the bar
//! lays it out and draws it, so the app never draws a pixel in the bar or
//! what hangs from it. What the user does to a control goes back to the
//! app through forum-wm (`StatusInput`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use forum_ctl::{Intent, Reply, StatusNode, StatusPanel, StatusText, StatusValue, STATUS_PANEL_MAX_H};
use pergola::geom::Rect;
use pergola::reactive::Mutable;
use pergola::theme::{font, radius, size, type_size, Weight};
use pergola::text_edit::TextEdit;
use pergola::widgets::{Button, Check, Checkbox, Divider, ProgressBar, RadioGroup, Segmented, Slider, TextField, Toggle};
use pergola::{Ctx, Node, TextStyle, View};

/// Between nodes one under another, and cells of a row.
const GAP: f32 = 8.0;
const ROW_GAP: f32 = 12.0;
/// A group's inset from its frame.
const GROUP_PAD: f32 = 10.0;
const HEADING_H: f32 = 22.0;
const LABEL_H: f32 = 18.0;
const CONTROL_H: f32 = 24.0;
const SEGMENTED_H: f32 = size::BUTTON_HEIGHT_DEFAULT;
const RADIO_ROW_H: f32 = 26.0;
const SEPARATOR_H: f32 = 9.0;

/// One drawn piece of a laid-out panel, in window px.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Heading(String),
    Line(String, StatusText),
    /// A group's frame, and its title if any.
    Frame(String),
    Rule,
    /// A control (or a progress bar) at this rect.
    Control(StatusNode),
}

fn text_style(s: StatusText) -> (TextStyle, bool) {
    let (sz, mono) = match s {
        StatusText::Body | StatusText::Secondary => (type_size::SM, false),
        StatusText::Caption => (type_size::XS, false),
        StatusText::Mono => (type_size::XS, true),
    };
    let family = if mono { font::MONO } else { font::SANS };
    (TextStyle { family: family.into(), size: sz, weight: Weight::Regular, color: pergola::Color::TRANSPARENT }, mono)
}

/// Text at the top left of `r` (a `Label` at a point centres its line
/// on it; a panel lays out by tops).
pub fn text_at(ctx: &mut Ctx, s: &str, x: f32, y: f32, w: f32, size: f32, weight: Weight, mono: bool, color: pergola::Color) {
    let style = TextStyle { family: if mono { font::MONO } else { font::SANS }.into(), size, weight, color };
    let h = pergola::text::measure(s, &style).h;
    ctx.add(Node::Text { rect: Rect::new(x, y, w, h), content: s.to_string(), style });
}

/// Lay `nodes` out one under another from (`x`, `y`), `w` wide, adding
/// the pieces to `out`; returns the height used.
pub fn layout(nodes: &[StatusNode], x: f32, y: f32, w: f32, out: &mut Vec<(Rect, Piece)>) -> f32 {
    let mut cy = y;
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 {
            cy += GAP;
        }
        cy += layout_one(n, x, cy, w, out);
    }
    cy - y
}

fn layout_one(n: &StatusNode, x: f32, y: f32, w: f32, out: &mut Vec<(Rect, Piece)>) -> f32 {
    match n {
        StatusNode::Heading { text } => {
            out.push((Rect::new(x, y, w, HEADING_H), Piece::Heading(text.clone())));
            HEADING_H
        }
        StatusNode::Text { text, style } => {
            let (ts, _) = text_style(*style);
            let lh = pergola::text::line_height(&ts);
            let lines = pergola::text::lines(text, &ts, w);
            for (i, l) in lines.iter().enumerate() {
                out.push((Rect::new(x, y + i as f32 * lh, w, lh), Piece::Line(l.clone(), *style)));
            }
            lines.len() as f32 * lh
        }
        StatusNode::Group { title, children } => {
            let at = out.len();
            out.push((Rect::ZERO_SIZED, Piece::Frame(title.clone())));
            let mut inner = y + GROUP_PAD;
            if !title.is_empty() {
                inner += LABEL_H + GAP / 2.0;
            }
            let h = layout(children, x + GROUP_PAD, inner, w - 2.0 * GROUP_PAD, out);
            let total = inner - y + h + GROUP_PAD;
            out[at].0 = Rect::new(x, y, w, total);
            total
        }
        StatusNode::Row { children } => {
            let n = children.len().max(1) as f32;
            let cw = ((w - ROW_GAP * (n - 1.0)) / n).max(0.0);
            children
                .iter()
                .enumerate()
                .map(|(i, c)| layout_one(c, x + i as f32 * (cw + ROW_GAP), y, cw, out))
                .fold(0.0, f32::max)
        }
        StatusNode::Column { children } => layout(children, x, y, w, out),
        StatusNode::Separator => {
            out.push((Rect::new(x, y + (SEPARATOR_H / 2.0).floor(), w, 1.0), Piece::Rule));
            SEPARATOR_H
        }
        StatusNode::Toggle { .. } | StatusNode::Check { .. } => control(n, x, y, w, CONTROL_H, out),
        StatusNode::Slider { label, .. } => control(n, x, y, w, if label.is_empty() { CONTROL_H } else { LABEL_H + CONTROL_H }, out),
        StatusNode::Choice { options, segmented, .. } => {
            let h = if *segmented { SEGMENTED_H } else { options.len() as f32 * RADIO_ROW_H };
            control(n, x, y, w, h, out)
        }
        StatusNode::Button { .. } => control(n, x, y, w, size::BUTTON_HEIGHT_DEFAULT, out),
        StatusNode::Progress { label, .. } => control(n, x, y, w, if label.is_empty() { 4.0 } else { LABEL_H + 8.0 }, out),
        StatusNode::TextField { label, .. } => {
            control(n, x, y, w, if label.is_empty() { 0.0 } else { LABEL_H } + size::INPUT_HEIGHT_DEFAULT, out)
        }
    }
}

fn control(n: &StatusNode, x: f32, y: f32, w: f32, h: f32, out: &mut Vec<(Rect, Piece)>) -> f32 {
    out.push((Rect::new(x, y, w, h), Piece::Control(n.clone())));
    h
}

/// The panel laid out at (`x`, `y`): its pieces (those that fit in
/// `STATUS_PANEL_MAX_H`) and its height.
pub fn laid_out(p: &StatusPanel, x: f32, y: f32, w: f32) -> (Vec<(Rect, Piece)>, f32) {
    let mut out = Vec::new();
    let h = layout(&p.nodes, x, y, w, &mut out);
    let max = STATUS_PANEL_MAX_H as f32;
    out.retain(|(r, _)| r.y() + r.h() <= y + max + 0.5);
    (out, h.min(max))
}

/// The controls' values as the bar shows them, by id: set from the app's
/// panel each time it changes, and moved at once by the user (so a drag
/// is not undone while the app answers).
#[derive(Clone, Default)]
pub struct Controls {
    map: Rc<RefCell<HashMap<u32, Ctl>>>,
    dirty: Option<Arc<AtomicBool>>,
}

#[derive(Clone)]
enum Ctl {
    On(Mutable<bool>),
    Check(Mutable<Check>),
    Level(Mutable<f64>),
    Index(Mutable<usize>),
    /// The field's edit state, and the value the app last sent (the
    /// field takes a new one only when the app's changes, so what the
    /// user is typing is not overwritten by an unrelated update).
    Text(Mutable<TextEdit>, Rc<RefCell<String>>),
}

impl Controls {
    pub fn new(dirty: Option<Arc<AtomicBool>>) -> Self {
        Self { map: Rc::default(), dirty }
    }

    fn cell<T: 'static>(&self, v: T) -> Mutable<T> {
        match &self.dirty {
            Some(d) => Mutable::with_dirty(v, d.clone()),
            None => Mutable::new(v),
        }
    }

    fn get(&self, n: &StatusNode) -> Option<Ctl> {
        let (id, want) = match n {
            StatusNode::Toggle { id, on, .. } => (*id, Ctl::On(self.cell(*on))),
            StatusNode::Check { id, on, .. } => (*id, Ctl::Check(self.cell(if *on { Check::On } else { Check::Off }))),
            StatusNode::Slider { id, value, .. } => (*id, Ctl::Level(self.cell(*value as f64))),
            StatusNode::Choice { id, selected, .. } => (*id, Ctl::Index(self.cell(*selected as usize))),
            StatusNode::TextField { id, value, .. } => (*id, Ctl::Text(self.cell(TextEdit::new(value)), Rc::new(RefCell::new(value.clone())))),
            _ => return None,
        };
        let mut map = self.map.borrow_mut();
        let e = map.entry(id).or_insert_with(|| want.clone());
        if std::mem::discriminant(e) != std::mem::discriminant(&want) {
            *e = want;
        }
        Some(e.clone())
    }

    /// The app sent a new panel: every control shows its value.
    pub fn sync(&self, p: &StatusPanel) {
        fn walk(c: &Controls, nodes: &[StatusNode]) {
            for n in nodes {
                match n {
                    StatusNode::Group { children, .. } | StatusNode::Row { children } | StatusNode::Column { children } => walk(c, children),
                    _ => match (c.get(n), n) {
                        (Some(Ctl::On(m)), StatusNode::Toggle { on, .. }) if m.get() != *on => m.set(*on),
                        (Some(Ctl::Check(m)), StatusNode::Check { on, .. }) if (m.get() == Check::On) != *on => {
                            m.set(if *on { Check::On } else { Check::Off })
                        }
                        (Some(Ctl::Level(m)), StatusNode::Slider { value, .. }) if (m.get() - *value as f64).abs() > 1e-4 => m.set(*value as f64),
                        (Some(Ctl::Index(m)), StatusNode::Choice { selected, .. }) if m.get() != *selected as usize => m.set(*selected as usize),
                        (Some(Ctl::Text(m, last)), StatusNode::TextField { value, .. }) if *last.borrow() != *value => {
                            *last.borrow_mut() = value.clone();
                            m.set(TextEdit::new(value));
                        }
                        _ => {}
                    },
                }
            }
        }
        walk(self, &p.nodes);
    }
}

/// Where the user's changes go: forum-wm, for app `app_id`.
#[derive(Clone)]
pub struct Sink {
    pub app_id: String,
    /// Set after each change, so the bar asks for the items again.
    pub stale: Arc<AtomicBool>,
}

impl Sink {
    fn send(&self, id: u32, value: StatusValue) {
        let intent = Intent::StatusInput { app_id: self.app_id.clone(), id, value };
        let r = forum_ctl::request(forum_ctl::default_socket_path(), &intent);
        if !matches!(r, Ok(Reply::Ack)) {
            eprintln!("forum-bar: status input {} {id}: {r:?}", self.app_id);
        }
        self.stale.store(true, Ordering::Relaxed);
    }
}

/// Draw the laid-out panel.
pub fn draw(ctx: &mut Ctx, pieces: &[(Rect, Piece)], controls: &Controls, sink: &Sink) {
    let t = ctx.theme;
    for (r, p) in pieces {
        match p {
            Piece::Heading(s) => {
                text_at(ctx, s, r.x(), r.y(), r.w(), type_size::SM, Weight::Semibold, false, t.text_primary());
            }
            Piece::Line(s, style) => {
                let (ts, mono) = text_style(*style);
                let color = match style {
                    StatusText::Body => t.text_primary(),
                    StatusText::Secondary | StatusText::Mono => t.text_secondary(),
                    StatusText::Caption => t.text_tertiary(),
                };
                text_at(ctx, s, r.x(), r.y(), r.w(), ts.size, Weight::Regular, mono, color);
            }
            Piece::Frame(title) => {
                ctx.add(Node::Rect { rect: *r, fill: t.border_default(), radius: radius::MD });
                let inset = Rect::new(r.x() + 1.0, r.y() + 1.0, r.w() - 2.0, r.h() - 2.0);
                ctx.add(Node::Rect { rect: inset, fill: t.bg_elevated(), radius: radius::MD - 1.0 });
                if !title.is_empty() {
                    let w = r.w() - 2.0 * GROUP_PAD;
                    text_at(ctx, title, r.x() + GROUP_PAD, r.y() + GROUP_PAD, w, type_size::XS, Weight::Semibold, false, t.text_secondary());
                }
            }
            Piece::Rule => {
                Divider::horizontal(r.w()).at(r.x(), r.y()).emit(ctx);
            }
            Piece::Control(n) => draw_control(ctx, *r, n, controls, sink),
        }
    }
}

fn draw_control(ctx: &mut Ctx, r: Rect, n: &StatusNode, controls: &Controls, sink: &Sink) {
    let t = ctx.theme;
    let ctl = controls.get(n);
    let label_above = |ctx: &mut Ctx, label: &str| {
        if label.is_empty() {
            0.0
        } else {
            text_at(ctx, label, r.x(), r.y(), r.w(), type_size::XS, Weight::Regular, false, t.text_secondary());
            LABEL_H
        }
    };
    match (n, ctl) {
        (StatusNode::Toggle { id, label, enabled, .. }, Some(Ctl::On(m))) => {
            let (id, s) = (*id, sink.clone());
            Toggle::new(m, label.clone()).at(r.x(), r.y()).enabled(*enabled).on_change(move |b| s.send(id, StatusValue::On(b))).render(ctx);
        }
        (StatusNode::Check { id, label, enabled, .. }, Some(Ctl::Check(m))) => {
            let (id, s) = (*id, sink.clone());
            Checkbox::new(m, label.clone())
                .at(r.x(), r.y())
                .enabled(*enabled)
                .on_change(move |c| s.send(id, StatusValue::On(c == Check::On)))
                .render(ctx);
        }
        (StatusNode::Slider { id, label, enabled, .. }, Some(Ctl::Level(m))) => {
            let dy = label_above(ctx, label);
            let (id, s) = (*id, sink.clone());
            Slider::new(m, 0.0, 1.0)
                .at(r.x(), r.y() + dy)
                .width(r.w())
                .enabled(*enabled)
                .on_change(move |v| s.send(id, StatusValue::Level(v as f32)))
                .render(ctx);
        }
        (StatusNode::Choice { id, options, selected, segmented, enabled }, Some(Ctl::Index(m))) => {
            let (id, s, on, keep, was) = (*id, sink.clone(), *enabled, m.clone(), *selected as usize);
            // A disabled choice does not move.
            let change = move |i: usize| {
                if on {
                    s.send(id, StatusValue::Chosen(i as u32))
                } else {
                    keep.set(was)
                }
            };
            if *segmented {
                Segmented::new(m, options.clone()).at(r.x(), r.y()).width(r.w()).on_change(change).render(ctx);
            } else {
                RadioGroup::new(m, options.clone()).at(r.x(), r.y()).on_change(change).render(ctx);
            }
        }
        (StatusNode::Button { id, label, primary, enabled }, _) => {
            let b = if !*enabled {
                Button::ghost(label.clone())
            } else if *primary {
                Button::primary(label.clone())
            } else {
                Button::secondary(label.clone())
            };
            let b = b.at(r.x(), r.y()).width(r.w());
            let b = if *enabled {
                let (id, s) = (*id, sink.clone());
                // Button callbacks are Send + Sync; the sink is too.
                b.on_click(move || s.send(id, StatusValue::Pressed))
            } else {
                b
            };
            b.render(ctx);
        }
        (StatusNode::TextField { id, label, placeholder, value, enabled }, Some(Ctl::Text(m, _))) => {
            let dy = label_above(ctx, label);
            let field = Rect::new(r.x(), r.y() + dy, r.w(), size::INPUT_HEIGHT_DEFAULT);
            if *enabled {
                let (id, s) = (*id, sink.clone());
                TextField::new(m)
                    .placeholder(placeholder.clone())
                    .at(field.x(), field.y())
                    .width(field.w())
                    .on_submit(move |text| s.send(id, StatusValue::Text(text.to_string())))
                    .render(ctx);
            } else {
                // Read-only: the value in a quiet box, no caret.
                ctx.add(Node::Rect { rect: field, fill: t.border_default(), radius: radius::SM });
                let inner = Rect::new(field.x() + 1.0, field.y() + 1.0, field.w() - 2.0, field.h() - 2.0);
                ctx.add(Node::Rect { rect: inner, fill: t.bg_surface(), radius: radius::SM - 1.0 });
                let shown = if value.is_empty() { placeholder } else { value };
                let style = TextStyle { family: font::SANS.into(), size: type_size::SM, weight: Weight::Regular, color: t.text_tertiary() };
                let h = pergola::text::measure(shown, &style).h;
                ctx.add(Node::Text { rect: Rect::new(field.x() + 8.0, field.y() + (field.h() - h) / 2.0, field.w() - 16.0, h), content: shown.clone(), style });
            }
        }
        (StatusNode::Progress { label, value }, _) => {
            let dy = label_above(ctx, label);
            ProgressBar::new(*value, r.w()).at(r.x(), r.y() + dy + 2.0).emit(ctx);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StatusPanel {
        StatusPanel {
            width: 280,
            nodes: vec![
                StatusNode::Heading { text: "Clipboard".into() },
                StatusNode::Text { text: "Twelve items kept".into(), style: StatusText::Secondary },
                StatusNode::Separator,
                StatusNode::Group {
                    title: "Keep".into(),
                    children: vec![
                        StatusNode::Toggle { id: 1, label: "Paused".into(), on: false, enabled: true },
                        StatusNode::Choice { id: 2, options: vec!["Text".into(), "All".into()], selected: 0, segmented: true, enabled: true },
                    ],
                },
                StatusNode::Row {
                    children: vec![
                        StatusNode::Button { id: 3, label: "Clear".into(), primary: false, enabled: true },
                        StatusNode::Button { id: 4, label: "Open".into(), primary: true, enabled: true },
                    ],
                },
            ],
        }
    }

    #[test]
    fn a_panel_lays_out_top_to_bottom_with_rows_side_by_side() {
        pergola::text::clear_measurer();
        let (pieces, h) = laid_out(&sample(), 10.0, 20.0, 256.0);
        let rect_of = |want: &dyn Fn(&Piece) -> bool| pieces.iter().find(|(_, p)| want(p)).unwrap().0;
        let heading = rect_of(&|p| matches!(p, Piece::Heading(_)));
        let frame = rect_of(&|p| matches!(p, Piece::Frame(_)));
        let toggle = rect_of(&|p| matches!(p, Piece::Control(StatusNode::Toggle { .. })));
        let clear = rect_of(&|p| matches!(p, Piece::Control(StatusNode::Button { id: 3, .. })));
        let open = rect_of(&|p| matches!(p, Piece::Control(StatusNode::Button { id: 4, .. })));
        assert_eq!((heading.x(), heading.y()), (10.0, 20.0));
        // The group's controls sit inside its frame, under its title.
        assert!(toggle.x() == frame.x() + GROUP_PAD && toggle.y() > frame.y() + LABEL_H);
        assert!(frame.y() + frame.h() < clear.y());
        // A row's cells share the width, side by side.
        assert_eq!(clear.y(), open.y());
        assert_eq!(clear.w(), open.w());
        assert!((open.x() + open.w() - (10.0 + 256.0)).abs() < 0.01);
        assert!((h - (open.y() + open.h() - 20.0)).abs() < 0.01);
    }

    #[test]
    fn a_tall_panel_is_cut_at_the_limit() {
        pergola::text::clear_measurer();
        let p = StatusPanel { width: 280, nodes: (0..60).map(|i| StatusNode::Toggle { id: i, label: "x".into(), on: false, enabled: true }).collect() };
        let (pieces, h) = laid_out(&p, 0.0, 0.0, 256.0);
        assert_eq!(h, STATUS_PANEL_MAX_H as f32);
        assert!(pieces.len() < 60 && pieces.iter().all(|(r, _)| r.y() + r.h() <= h));
    }

    #[test]
    fn controls_follow_the_app_and_keep_their_cells() {
        let c = Controls::new(None);
        let mut p = sample();
        c.sync(&p);
        let Some(Ctl::On(a)) = c.get(&p.nodes[3].clone().children()[0]) else { panic!() };
        assert!(!a.get());
        if let StatusNode::Group { children, .. } = &mut p.nodes[3] {
            children[0] = StatusNode::Toggle { id: 1, label: "Paused".into(), on: true, enabled: true };
        }
        c.sync(&p);
        let Some(Ctl::On(b)) = c.get(&p.nodes[3].clone().children()[0]) else { panic!() };
        assert!(a.get() && b.get(), "the same cell, now on");
    }

    #[test]
    fn a_text_field_takes_the_apps_value_only_when_it_changes() {
        let c = Controls::new(None);
        let field = |v: &str| StatusPanel { width: 280, nodes: vec![StatusNode::TextField { id: 7, label: "Server".into(), placeholder: "host".into(), value: v.into(), enabled: true }] };
        let p = field("a.example");
        c.sync(&p);
        let Some(Ctl::Text(m, _)) = c.get(&p.nodes[0]) else { panic!() };
        assert_eq!(m.get_cloned().text(), "a.example");
        // The user types; the app re-sends the same value: kept.
        m.set(TextEdit::new("typed"));
        c.sync(&field("a.example"));
        assert_eq!(m.get_cloned().text(), "typed");
        // The app sends a new value: shown.
        c.sync(&field("b.example"));
        assert_eq!(m.get_cloned().text(), "b.example");
    }

    trait Children {
        fn children(self) -> Vec<StatusNode>;
    }
    impl Children for StatusNode {
        fn children(self) -> Vec<StatusNode> {
            match self {
                StatusNode::Group { children, .. } | StatusNode::Row { children } | StatusNode::Column { children } => children,
                _ => Vec::new(),
            }
        }
    }
}
